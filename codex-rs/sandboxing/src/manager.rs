//! 【文件职责】SandboxManager —— 跨平台沙箱的「选择 + 改写」中枢。它回答两个问题：
//!   1. select_initial：这条命令到底要不要进沙箱、进哪种？返回 SandboxType（None / macOS
//!      Seatbelt / Linux Seccomp+Landlock / Windows 受限令牌），依据是调用方偏好
//!      （Auto/Require/Forbid）+ 文件/网络策略是否要求平台沙箱。
//!   2. transform：把一条「可移植命令」按选定沙箱类型**包裹**成真正可执行的 argv ——
//!      macOS 前面拼 sandbox-exec + 策略 profile、Linux 前面拼 codex-linux-sandbox 包装器，
//!      并把 PermissionProfile 叠加额外权限后落成 SandboxExecRequest（含最终 argv、env、
//!      生效的文件/网络策略、arg0 覆盖等）交给 exec 层 spawn。
//! 【设计取舍】「选择」与「改写」分两步：select_initial 可先定类型，失败时上层（orchestrator）
//!   还能据偏好降级重试；transform 才真正绑定平台细节，平台差异都收敛在这一个 match 里。
#[cfg(target_os = "linux")]
use crate::bwrap::WSL1_BWRAP_WARNING;
#[cfg(target_os = "linux")]
use crate::bwrap::is_wsl1;
use crate::landlock::CODEX_LINUX_SANDBOX_ARG0;
use crate::landlock::allow_network_for_proxy;
use crate::landlock::create_linux_sandbox_command_args_for_permission_profile;
use crate::policy_transforms::effective_permission_profile;
use crate::policy_transforms::should_require_platform_sandbox;
use codex_network_proxy::NetworkProxy;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_protocol::protocol::SandboxPolicy;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;

/// 四种沙箱后端：None（不隔离，直接跑）、macOS 的 Seatbelt（sandbox-exec）、Linux 的
/// Seccomp + Landlock（经 codex-linux-sandbox 包装器）、Windows 受限令牌。as_metric_tag
/// 给埋点用。具体平台只会用到其中之一，跨平台代码靠这个枚举统一表达「用哪种隔离」。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SandboxType {
    None,
    MacosSeatbelt,
    LinuxSeccomp,
    WindowsRestrictedToken,
}

impl SandboxType {
    pub fn as_metric_tag(self) -> &'static str {
        match self {
            SandboxType::None => "none",
            SandboxType::MacosSeatbelt => "seatbelt",
            SandboxType::LinuxSeccomp => "seccomp",
            SandboxType::WindowsRestrictedToken => "windows_sandbox",
        }
    }
}

/// 调用方对「是否进沙箱」的偏好（由具体工具的 Sandboxable trait 给出）：
/// - Auto：交给策略判断（should_require_platform_sandbox），默认行为。
/// - Require：强制进平台沙箱（拿不到平台沙箱则退化为 None）。
/// - Forbid：明确不进沙箱（如某些必须裸跑的命令）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SandboxablePreference {
    Auto,
    Require,
    Forbid,
}

pub fn get_platform_sandbox(windows_sandbox_enabled: bool) -> Option<SandboxType> {
    if cfg!(target_os = "macos") {
        Some(SandboxType::MacosSeatbelt)
    } else if cfg!(target_os = "linux") {
        Some(SandboxType::LinuxSeccomp)
    } else if cfg!(target_os = "windows") {
        if windows_sandbox_enabled {
            Some(SandboxType::WindowsRestrictedToken)
        } else {
            None
        }
    } else {
        None
    }
}

#[derive(Debug)]
pub struct SandboxCommand {
    pub program: OsString,
    pub args: Vec<String>,
    pub cwd: AbsolutePathBuf,
    pub env: HashMap<String, String>,
    pub additional_permissions: Option<AdditionalPermissionProfile>,
}

/// transform 的产物：一条「已包裹好、可直接 spawn」的命令。command 是最终 argv（可能已被
/// 套上 sandbox-exec / codex-linux-sandbox 前缀），并附带生效后的文件/网络策略与 arg0 覆盖，
/// 供 exec 层落地执行。它与下面的 SandboxCommand（改写前的可移植输入）相对。
#[derive(Debug)]
pub struct SandboxExecRequest {
    pub command: Vec<String>,
    pub cwd: AbsolutePathBuf,
    pub env: HashMap<String, String>,
    pub network: Option<NetworkProxy>,
    pub sandbox: SandboxType,
    pub windows_sandbox_level: WindowsSandboxLevel,
    pub windows_sandbox_private_desktop: bool,
    pub permission_profile: PermissionProfile,
    pub file_system_sandbox_policy: FileSystemSandboxPolicy,
    pub network_sandbox_policy: NetworkSandboxPolicy,
    pub arg0: Option<String>,
}

/// Bundled arguments for sandbox transformation.
///
/// This keeps call sites self-documenting when several fields are optional.
pub struct SandboxTransformRequest<'a> {
    pub command: SandboxCommand,
    pub permissions: &'a PermissionProfile,
    pub sandbox: SandboxType,
    pub enforce_managed_network: bool,
    // TODO(viyatb): Evaluate switching this to Option<Arc<NetworkProxy>>
    // to make shared ownership explicit across runtime/sandbox plumbing.
    pub network: Option<&'a NetworkProxy>,
    pub sandbox_policy_cwd: &'a Path,
    pub codex_linux_sandbox_exe: Option<&'a Path>,
    pub use_legacy_landlock: bool,
    pub windows_sandbox_level: WindowsSandboxLevel,
    pub windows_sandbox_private_desktop: bool,
}

#[derive(Debug)]
pub enum SandboxTransformError {
    MissingLinuxSandboxExecutable,
    #[cfg(target_os = "linux")]
    Wsl1UnsupportedForBubblewrap,
    #[cfg(not(target_os = "macos"))]
    SeatbeltUnavailable,
}

impl std::fmt::Display for SandboxTransformError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingLinuxSandboxExecutable => {
                write!(f, "missing codex-linux-sandbox executable path")
            }
            #[cfg(target_os = "linux")]
            Self::Wsl1UnsupportedForBubblewrap => write!(f, "{WSL1_BWRAP_WARNING}"),
            #[cfg(not(target_os = "macos"))]
            Self::SeatbeltUnavailable => write!(f, "seatbelt sandbox is only available on macOS"),
        }
    }
}

impl std::error::Error for SandboxTransformError {}

#[derive(Default)]
pub struct SandboxManager;

impl SandboxManager {
    pub fn new() -> Self {
        Self
    }

    /// 第一步「选型」：按调用方偏好定沙箱类型。Forbid→None；Require→平台沙箱（无则 None）；
    /// Auto→看文件/网络策略与是否有受管网络需求是否「要求」平台沙箱，要则取平台沙箱否则 None。
    /// 注意：选到 None 不代表无限制——上层可能因此先尝试无沙箱、失败再升级（escalate）。
    pub fn select_initial(
        &self,
        file_system_policy: &FileSystemSandboxPolicy,
        network_policy: NetworkSandboxPolicy,
        pref: SandboxablePreference,
        windows_sandbox_level: WindowsSandboxLevel,
        has_managed_network_requirements: bool,
    ) -> SandboxType {
        match pref {
            SandboxablePreference::Forbid => SandboxType::None,
            SandboxablePreference::Require => {
                get_platform_sandbox(windows_sandbox_level != WindowsSandboxLevel::Disabled)
                    .unwrap_or(SandboxType::None)
            }
            SandboxablePreference::Auto => {
                if should_require_platform_sandbox(
                    file_system_policy,
                    network_policy,
                    has_managed_network_requirements,
                ) {
                    get_platform_sandbox(windows_sandbox_level != WindowsSandboxLevel::Disabled)
                        .unwrap_or(SandboxType::None)
                } else {
                    SandboxType::None
                }
            }
        }
    }

    /// 第二步「改写」：把可移植命令按选定沙箱类型包裹成最终 argv。先合并 PermissionProfile +
    /// 额外权限算出生效的文件/网络策略；再按 sandbox 分派：None 原样；MacosSeatbelt 在前面拼
    /// sandbox-exec + 由策略生成的 .sb profile 参数；LinuxSeccomp 在前面拼 codex-linux-sandbox
    /// 包装器（并校验 WSL1/bubblewrap 兼容性）+ 设定 arg0；Windows 暂原样（隔离在 spawn 时另做）。
    /// 平台不匹配的分支返回 SandboxTransformError（如非 macOS 选了 Seatbelt）。
    pub fn transform(
        &self,
        request: SandboxTransformRequest<'_>,
    ) -> Result<SandboxExecRequest, SandboxTransformError> {
        let SandboxTransformRequest {
            mut command,
            permissions,
            sandbox,
            enforce_managed_network,
            network,
            sandbox_policy_cwd,
            codex_linux_sandbox_exe,
            use_legacy_landlock,
            windows_sandbox_level,
            windows_sandbox_private_desktop,
        } = request;
        let additional_permissions = command.additional_permissions.take();
        let effective_permission_profile =
            effective_permission_profile(permissions, additional_permissions.as_ref());
        let (effective_file_system_policy, effective_network_policy) =
            effective_permission_profile.to_runtime_permissions();
        let mut argv = Vec::with_capacity(1 + command.args.len());
        argv.push(command.program);
        argv.extend(command.args.into_iter().map(OsString::from));

        let (argv, arg0_override) = match sandbox {
            SandboxType::None => (os_argv_to_strings(argv), None),
            #[cfg(target_os = "macos")]
            SandboxType::MacosSeatbelt => {
                use crate::seatbelt::CreateSeatbeltCommandArgsParams;
                use crate::seatbelt::MACOS_PATH_TO_SEATBELT_EXECUTABLE;
                use crate::seatbelt::create_seatbelt_command_args;

                let mut args = create_seatbelt_command_args(CreateSeatbeltCommandArgsParams {
                    command: os_argv_to_strings(argv),
                    file_system_sandbox_policy: &effective_file_system_policy,
                    network_sandbox_policy: effective_network_policy,
                    sandbox_policy_cwd,
                    enforce_managed_network,
                    network,
                    extra_allow_unix_sockets: &[],
                });
                let mut full_command = Vec::with_capacity(1 + args.len());
                full_command.push(MACOS_PATH_TO_SEATBELT_EXECUTABLE.to_string());
                full_command.append(&mut args);
                (full_command, None)
            }
            #[cfg(not(target_os = "macos"))]
            SandboxType::MacosSeatbelt => return Err(SandboxTransformError::SeatbeltUnavailable),
            SandboxType::LinuxSeccomp => {
                let exe = codex_linux_sandbox_exe
                    .ok_or(SandboxTransformError::MissingLinuxSandboxExecutable)?;
                let allow_proxy_network = allow_network_for_proxy(enforce_managed_network);
                #[cfg(target_os = "linux")]
                ensure_linux_bubblewrap_is_supported(
                    &effective_file_system_policy,
                    use_legacy_landlock,
                    allow_proxy_network,
                    is_wsl1(),
                )?;
                let mut args = create_linux_sandbox_command_args_for_permission_profile(
                    os_argv_to_strings(argv),
                    command.cwd.as_path(),
                    &effective_permission_profile,
                    sandbox_policy_cwd,
                    use_legacy_landlock,
                    allow_proxy_network,
                );
                let mut full_command = Vec::with_capacity(1 + args.len());
                full_command.push(os_string_to_command_component(exe.as_os_str().to_owned()));
                full_command.append(&mut args);
                (full_command, Some(linux_sandbox_arg0_override(exe)))
            }
            #[cfg(target_os = "windows")]
            SandboxType::WindowsRestrictedToken => (os_argv_to_strings(argv), None),
            #[cfg(not(target_os = "windows"))]
            SandboxType::WindowsRestrictedToken => (os_argv_to_strings(argv), None),
        };

        Ok(SandboxExecRequest {
            command: argv,
            cwd: command.cwd,
            env: command.env,
            network: network.cloned(),
            sandbox,
            windows_sandbox_level,
            windows_sandbox_private_desktop,
            permission_profile: effective_permission_profile,
            file_system_sandbox_policy: effective_file_system_policy,
            network_sandbox_policy: effective_network_policy,
            arg0: arg0_override,
        })
    }
}

pub fn compatibility_sandbox_policy_for_permission_profile(
    permissions: &PermissionProfile,
    file_system_policy: &FileSystemSandboxPolicy,
    network_policy: NetworkSandboxPolicy,
    cwd: &Path,
) -> SandboxPolicy {
    permissions
        .to_legacy_sandbox_policy(cwd)
        .unwrap_or_else(|_| {
            compatibility_workspace_write_policy(file_system_policy, network_policy, cwd)
        })
}

fn compatibility_workspace_write_policy(
    file_system_policy: &FileSystemSandboxPolicy,
    network_policy: NetworkSandboxPolicy,
    cwd: &Path,
) -> SandboxPolicy {
    let cwd_abs = AbsolutePathBuf::from_absolute_path(cwd).ok();
    let writable_roots = file_system_policy
        .get_writable_roots_with_cwd(cwd)
        .into_iter()
        .map(|root| root.root)
        .filter(|root| cwd_abs.as_ref() != Some(root))
        .collect();
    let tmpdir_writable = std::env::var_os("TMPDIR")
        .filter(|tmpdir| !tmpdir.is_empty())
        .and_then(|tmpdir| {
            AbsolutePathBuf::from_absolute_path(std::path::PathBuf::from(tmpdir)).ok()
        })
        .is_some_and(|tmpdir| file_system_policy.can_write_path_with_cwd(tmpdir.as_path(), cwd));
    let slash_tmp = Path::new("/tmp");
    let slash_tmp_writable = slash_tmp.is_absolute()
        && slash_tmp.is_dir()
        && file_system_policy.can_write_path_with_cwd(slash_tmp, cwd);

    SandboxPolicy::WorkspaceWrite {
        writable_roots,
        network_access: network_policy.is_enabled(),
        exclude_tmpdir_env_var: !tmpdir_writable,
        exclude_slash_tmp: !slash_tmp_writable,
    }
}

#[cfg(target_os = "linux")]
fn ensure_linux_bubblewrap_is_supported(
    file_system_sandbox_policy: &FileSystemSandboxPolicy,
    use_legacy_landlock: bool,
    allow_network_for_proxy: bool,
    is_wsl1: bool,
) -> Result<(), SandboxTransformError> {
    let requires_bubblewrap = !use_legacy_landlock
        && (!file_system_sandbox_policy.has_full_disk_write_access() || allow_network_for_proxy);
    if is_wsl1 && requires_bubblewrap {
        return Err(SandboxTransformError::Wsl1UnsupportedForBubblewrap);
    }

    Ok(())
}

fn os_argv_to_strings(argv: Vec<OsString>) -> Vec<String> {
    argv.into_iter()
        .map(os_string_to_command_component)
        .collect()
}

fn os_string_to_command_component(value: OsString) -> String {
    value
        .into_string()
        .unwrap_or_else(|value| value.to_string_lossy().into_owned())
}

fn linux_sandbox_arg0_override(exe: &Path) -> String {
    if exe.file_name().and_then(|name| name.to_str()) == Some(CODEX_LINUX_SANDBOX_ARG0) {
        os_string_to_command_component(exe.as_os_str().to_owned())
    } else {
        CODEX_LINUX_SANDBOX_ARG0.to_string()
    }
}

#[cfg(test)]
#[path = "manager_tests.rs"]
mod tests;
