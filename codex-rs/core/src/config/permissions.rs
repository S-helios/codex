//! 【文件职责】把 `config.toml` 中声明式的权限 profile（`[permissions]` 表 +
//! 内置档）「编译」成运行时真正生效的沙箱策略：文件系统策略
//! `FileSystemSandboxPolicy` 与网络策略 `NetworkSandboxPolicy`。
//!
//! 【架构位置】
//!   层级：core 配置层（权限子系统 · 编译器一侧）
//!   上游：config/mod.rs（`load_config_with_layer_stack` 在装配 `Config` 时调用本文件的 `compile_*` / `network_proxy_*` 系列函数）
//!   下游：`codex_protocol::permissions`（沙箱策略数据结构）、`codex_network_proxy`（网络代理配置）
//!
//! 【数据流】
//!   TOML profile（含 `extends` 继承链）→ `resolve_permission_profile()` 展平继承
//!     → `compile_permission_profile()` 逐条编译 filesystem/network 条目
//!     → (`FileSystemSandboxPolicy`, `NetworkSandboxPolicy`)
//!
//! 【关键概念】
//!   - 内置档（built-in）：`:read-only` / `:workspace` / `:danger-full-access`，
//!     id 以 `:` 前缀保留，用户档不得占用该前缀。
//!   - 特殊路径（`:special_path`）：`:root` / `:workspace_roots` / `:tmpdir` 等
//!     符号化路径，解析时刻意对未知值「向前兼容」（降级为警告而非报错）。
//!   - glob 平台差异：非 macOS 沙箱对 `**` 与 read/write glob 支持有限，编译时
//!     会产出启动告警提示用户。
//!
//! 【阅读建议】先看顶部三个 `BUILT_IN_*` 常量与 `builtin_permission_profile()`
//! （内置档 → 运行时权限的映射），再看核心入口 `compile_permission_profile()`，
//! 然后顺着它调用的 `compile_filesystem_permission()` / `compile_network_sandbox_policy()`
//! 往下读。底部 `parse_*` / `normalize_windows_*` 一类是路径解析小工具，可按需查阅。

use std::borrow::Cow;
use std::io;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use codex_config::permissions_toml::FilesystemPermissionToml;
use codex_config::permissions_toml::FilesystemPermissionsToml;
use codex_config::permissions_toml::NetworkDomainPermissionToml;
use codex_config::permissions_toml::NetworkDomainPermissionsToml;
use codex_config::permissions_toml::NetworkToml;
use codex_config::permissions_toml::NetworkUnixSocketPermissionToml;
use codex_config::permissions_toml::NetworkUnixSocketPermissionsToml;
use codex_config::permissions_toml::PermissionProfileToml;
use codex_config::permissions_toml::PermissionsToml;
use codex_config::permissions_toml::ResolvedPermissionProfileToml;
use codex_config::permissions_toml::WorkspaceRootsToml;
use codex_config::types::SandboxWorkspaceWrite;
use codex_features::NetworkProxyConfigToml;
use codex_features::NetworkProxyDomainPermissionToml;
use codex_features::NetworkProxyModeToml;
use codex_features::NetworkProxyUnixSocketPermissionToml;
use codex_network_proxy::NetworkMode;
use codex_network_proxy::NetworkProxyConfig;
#[cfg(test)]
use codex_network_proxy::NetworkUnixSocketPermission as ProxyNetworkUnixSocketPermission;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::BUILT_IN_PERMISSION_PROFILE_DANGER_FULL_ACCESS;
use codex_protocol::models::BUILT_IN_PERMISSION_PROFILE_READ_ONLY;
use codex_protocol::models::BUILT_IN_PERMISSION_PROFILE_WORKSPACE;
use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::FileSystemAccessMode;
use codex_protocol::permissions::FileSystemPath;
use codex_protocol::permissions::FileSystemSandboxEntry;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::FileSystemSpecialPath;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_protocol::permissions::project_roots_glob_pattern;
use codex_utils_absolute_path::AbsolutePathBuf;

use super::ProjectConfig;

// 三个内置档的 id 字符串别名（均带 `:` 前缀，定义在 protocol crate）。本文件内
// 大量 `match profile_name` 直接用这些常量做分支判定。
pub(crate) const BUILT_IN_READ_ONLY_PROFILE: &str = BUILT_IN_PERMISSION_PROFILE_READ_ONLY;
pub(crate) const BUILT_IN_WORKSPACE_PROFILE: &str = BUILT_IN_PERMISSION_PROFILE_WORKSPACE;
pub(crate) const BUILT_IN_DANGER_FULL_ACCESS_PROFILE: &str =
    BUILT_IN_PERMISSION_PROFILE_DANGER_FULL_ACCESS;

/// 在用户未显式选档时，挑选默认内置档。
///
/// 规则：项目被信任或被明确标记不信任（即用户对该项目做过信任决策），且不处于
/// 「Windows 但沙箱被禁用」的组合时，默认给 `:workspace`（工作区可写）；否则
/// 回退到更保守的 `:read-only`。Windows+沙箱禁用排除掉的原因是：那种环境下放开
/// 工作区写权限却没有沙箱兜底会偏危险，故收紧为只读。
pub(crate) fn default_builtin_permission_profile_name(
    active_project: &ProjectConfig,
    windows_sandbox_level: WindowsSandboxLevel,
) -> &'static str {
    if (active_project.is_trusted() || active_project.is_untrusted())
        && !(cfg!(target_os = "windows") && windows_sandbox_level == WindowsSandboxLevel::Disabled)
    {
        BUILT_IN_WORKSPACE_PROFILE
    } else {
        BUILT_IN_READ_ONLY_PROFILE
    }
}

pub(crate) fn is_builtin_permission_profile_name(profile_name: &str) -> bool {
    matches!(
        profile_name,
        BUILT_IN_READ_ONLY_PROFILE
            | BUILT_IN_WORKSPACE_PROFILE
            | BUILT_IN_DANGER_FULL_ACCESS_PROFILE
    )
}

/// 把内置档 id 映射为具体的 `PermissionProfile`。
///
/// `:workspace` 档可叠加旧式 `[sandbox_workspace_write]` 的定制（可写根、是否放开
/// 网络、是否排除 `$TMPDIR` / `/tmp`），故这里在有 `workspace_write` 时用
/// `workspace_write_with(...)` 把这些细节带入。`:danger-full-access` 映射为
/// `Disabled`（不启用沙箱）。非内置 id 返回 `None`，交由调用方按命名档处理。
pub(crate) fn builtin_permission_profile(
    profile_name: &str,
    workspace_write: Option<&SandboxWorkspaceWrite>,
) -> Option<PermissionProfile> {
    match profile_name {
        BUILT_IN_READ_ONLY_PROFILE => Some(PermissionProfile::read_only()),
        BUILT_IN_WORKSPACE_PROFILE => Some(match workspace_write {
            Some(SandboxWorkspaceWrite {
                writable_roots: _,
                network_access,
                exclude_tmpdir_env_var,
                exclude_slash_tmp,
            }) => PermissionProfile::workspace_write_with(
                &[],
                if *network_access {
                    NetworkSandboxPolicy::Enabled
                } else {
                    NetworkSandboxPolicy::Restricted
                },
                *exclude_tmpdir_env_var,
                *exclude_slash_tmp,
            ),
            None => PermissionProfile::workspace_write(),
        }),
        BUILT_IN_DANGER_FULL_ACCESS_PROFILE => Some(PermissionProfile::Disabled),
        _ => None,
    }
}

/// 校验用户自定义档名未占用保留前缀。所有内置档 id 都以 `:` 开头，因此用户档名
/// 若以 `:` 开头即视为非法（会与内置档冲突），直接报 `InvalidInput`。
pub(crate) fn validate_user_permission_profile_names(
    permissions: Option<&PermissionsToml>,
) -> io::Result<()> {
    let Some(permissions) = permissions else {
        return Ok(());
    };

    for profile_name in permissions.entries.keys() {
        if profile_name.starts_with(':') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "permissions profile `{profile_name}` uses a reserved built-in profile prefix"
                ),
            ));
        }
    }

    Ok(())
}

/// 从 profile 的 `[network]` 配置抽取代理设置，但强制 `network.enabled = false`。
///
/// 关键取舍：profile 里写的代理/白名单只是「素材」，供 NetworkProxy feature gate
/// 在真正放开网络时取用；profile 自己**不会**启动托管代理。所以这里清掉 enabled
/// 位，避免仅因 profile 携带代理配置就误启代理。
pub(crate) fn network_proxy_config_from_profile_network(
    network: Option<&NetworkToml>,
) -> NetworkProxyConfig {
    let mut config = network.map_or_else(
        NetworkProxyConfig::default,
        NetworkToml::to_network_proxy_config,
    );
    // Profile `network.enabled` controls sandbox network access. Profiles may
    // provide proxy settings for the feature gate to consume when that network
    // access is enabled, but they do not start the managed proxy on their own.
    config.network.enabled = false;
    config
}

/// 把 NetworkProxy feature gate 的配置（`NetworkProxyConfigToml`）就地合并进已有的
/// `NetworkProxyConfig`。
///
/// 实现上先把 feature 配置逐字段翻译成等价的 `NetworkToml`（包括 mode、domains 白
/// 名单、unix socket 权限等枚举的一一对应转换），再调用其
/// `apply_to_network_proxy_config()` 完成合并。属于两套 TOML 配置形态之间的字段
/// 搬运，无业务分支逻辑。
///
/// 副作用：就地修改入参 `config`。
pub(crate) fn apply_network_proxy_feature_config(
    config: &mut NetworkProxyConfig,
    feature_config: &NetworkProxyConfigToml,
) {
    NetworkToml {
        enabled: feature_config.enabled,
        proxy_url: feature_config.proxy_url.clone(),
        enable_socks5: feature_config.enable_socks5,
        socks_url: feature_config.socks_url.clone(),
        enable_socks5_udp: feature_config.enable_socks5_udp,
        allow_upstream_proxy: feature_config.allow_upstream_proxy,
        dangerously_allow_non_loopback_proxy: feature_config.dangerously_allow_non_loopback_proxy,
        dangerously_allow_all_unix_sockets: feature_config.dangerously_allow_all_unix_sockets,
        mode: feature_config.mode.map(|mode| match mode {
            NetworkProxyModeToml::Limited => NetworkMode::Limited,
            NetworkProxyModeToml::Full => NetworkMode::Full,
        }),
        domains: feature_config
            .domains
            .as_ref()
            .map(|domains| NetworkDomainPermissionsToml {
                entries: domains
                    .iter()
                    .map(|(pattern, permission)| {
                        let permission = match permission {
                            NetworkProxyDomainPermissionToml::Allow => {
                                NetworkDomainPermissionToml::Allow
                            }
                            NetworkProxyDomainPermissionToml::Deny => {
                                NetworkDomainPermissionToml::Deny
                            }
                        };
                        (pattern.clone(), permission)
                    })
                    .collect(),
            }),
        unix_sockets: feature_config.unix_sockets.as_ref().map(|unix_sockets| {
            NetworkUnixSocketPermissionsToml {
                entries: unix_sockets
                    .iter()
                    .map(|(path, permission)| {
                        let permission = match permission {
                            NetworkProxyUnixSocketPermissionToml::Allow => {
                                NetworkUnixSocketPermissionToml::Allow
                            }
                            NetworkProxyUnixSocketPermissionToml::None => {
                                NetworkUnixSocketPermissionToml::None
                            }
                        };
                        (path.clone(), permission)
                    })
                    .collect(),
            }
        }),
        allow_local_binding: feature_config.allow_local_binding,
        mitm: None,
    }
    .apply_to_network_proxy_config(config);
}

/// 展平命名档的 `extends` 继承链，得到 `ResolvedPermissionProfileToml`（含合并后的
/// profile 以及继承到的父档 id 列表）。继承链终点可能是内置档，由
/// `extensible_builtin_parent_profile_marker` 提供终止标记。
pub(crate) fn resolve_permission_profile(
    permissions: &PermissionsToml,
    profile_name: &str,
) -> io::Result<ResolvedPermissionProfileToml> {
    permissions
        .resolve_profile(profile_name, extensible_builtin_parent_profile_marker)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err.to_string()))
}

/// Built-in parents provide their runtime permissions below. Resolution only
/// needs an empty profile marker so inheritance can terminate while preserving
/// the built-in parent id in `inherited_profile_names`.
///
/// 内置父档（`:read-only` / `:workspace`）的实际权限由后续编译步骤注入，所以这里
/// 继承解析只需返回一个「空 profile 标记」让继承链能在内置父档处终止——同时父档 id
/// 仍被记录到 `inherited_profile_names`，供编译时据此选取内置基线权限。返回 `None`
/// 表示该 id 不是可继承的内置父档。
fn extensible_builtin_parent_profile_marker(profile_name: &str) -> Option<PermissionProfileToml> {
    matches!(
        profile_name,
        BUILT_IN_READ_ONLY_PROFILE | BUILT_IN_WORKSPACE_PROFILE
    )
    .then_some(PermissionProfileToml {
        description: None,
        extends: None,
        workspace_roots: None,
        filesystem: None,
        network: None,
    })
}

/// 取得「选中某 profile 时」对应的网络代理配置。
///
/// 内置档不携带自定义代理配置，直接返回默认值；非内置档则解析其继承链后从
/// `[network]` 抽取代理设置（仍会清掉 enabled 位，见
/// `network_proxy_config_from_profile_network`）。`profile_name` 以 `:` 开头但又
/// 非已知内置档时报错（未知内置档）。
pub(crate) fn network_proxy_config_for_profile_selection(
    permissions: Option<&PermissionsToml>,
    profile_name: &str,
) -> io::Result<NetworkProxyConfig> {
    if is_builtin_permission_profile_name(profile_name) {
        return Ok(NetworkProxyConfig::default());
    }
    reject_unknown_builtin_permission_profile(profile_name)?;

    let permissions = permissions.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "default_permissions requires a `[permissions]` table",
        )
    })?;
    let profile = resolve_permission_profile(permissions, profile_name)?.profile;
    Ok(network_proxy_config_from_profile_network(
        profile.network.as_ref(),
    ))
}

/// 把一个**命名档**编译为运行时的 (文件系统策略, 网络策略)。这是本文件的核心编译入口。
///
/// @param permissions       - 全部命名档目录（用于继承解析）
/// @param profile_name      - 要编译的命名档 id
/// @param policy_cwd        - 解析相对/特殊路径时的基准目录
/// @param startup_warnings  - 收集启动告警（就地追加，副作用）
/// @returns                 - 编译后的 (`FileSystemSandboxPolicy`, `NetworkSandboxPolicy`)
///
/// 流程：① 展平继承链 → ② 从继承到的内置父档取「基线权限」（无则回退到
/// restricted/Restricted）→ ③ 若 profile 声明了 `[filesystem]`，逐条编译追加到
/// 文件系统策略（非 macOS 平台还会对不受支持的 glob 产出告警）→ ④ 处理
/// `glob_scan_max_depth` 上限 → ⑤ 编译网络策略。空 `[filesystem]` 或完全无条目时，
/// 推送「该版本无可识别 filesystem 条目」告警并保持 restricted。
pub(crate) fn compile_permission_profile(
    permissions: &PermissionsToml,
    profile_name: &str,
    policy_cwd: &Path,
    startup_warnings: &mut Vec<String>,
) -> io::Result<(FileSystemSandboxPolicy, NetworkSandboxPolicy)> {
    let ResolvedPermissionProfileToml {
        profile,
        inherited_profile_names,
    } = resolve_permission_profile(permissions, profile_name)?;
    // 从继承到的内置父档（read-only / workspace）取运行时基线权限；继承链未触及内置
    // 父档时，下面 `unwrap_or_else` 会回退到最严格的 restricted + Restricted。
    let base_permissions = inherited_profile_names.iter().find_map(|name| {
        match name.as_str() {
            BUILT_IN_READ_ONLY_PROFILE => Some(PermissionProfile::read_only()),
            BUILT_IN_WORKSPACE_PROFILE => Some(PermissionProfile::workspace_write()),
            _ => None,
        }
        .map(|profile| profile.to_runtime_permissions())
    });
    let (mut file_system_sandbox_policy, base_network_sandbox_policy) = base_permissions
        .unwrap_or_else(|| {
            (
                FileSystemSandboxPolicy::restricted(Vec::new()),
                NetworkSandboxPolicy::Restricted,
            )
        });
    if let Some(filesystem) = profile.filesystem.as_ref() {
        if filesystem.is_empty() && file_system_sandbox_policy.entries.is_empty() {
            push_warning(
                startup_warnings,
                missing_filesystem_entries_warning(profile_name),
            );
        } else {
            // 仅非 macOS 平台：Linux/Windows 沙箱对 glob 支持弱于 macOS Seatbelt。
            // 这里只产出「提示用户改写规则」的启动告警（不阻断加载）：read/write
            // glob 与无界 `**` 在这些平台无法原生表达，建议改用精确路径、尾部
            // `/**` 子树规则，或设置 `glob_scan_max_depth` 限定展开深度。
            if cfg!(not(target_os = "macos")) {
                for pattern in unsupported_read_write_glob_paths(filesystem) {
                    push_warning(
                        startup_warnings,
                        format!(
                            "Filesystem glob `{pattern}` uses `read` or `write` access, which is not fully supported by this platform's sandboxing. Use an exact path or trailing `/**` subtree rule instead. `deny` globs are supported."
                        ),
                    );
                }
                for pattern in unbounded_unreadable_globstar_paths(filesystem) {
                    push_warning(
                        startup_warnings,
                        format!(
                            "Filesystem deny-read glob `{pattern}` uses `**`. Non-macOS sandboxing does not support unbounded `**` natively; set `glob_scan_max_depth` in this filesystem profile to cap Linux glob expansion and silence this warning, or enumerate explicit depths such as `*.env`, `*/*.env`, and `*/*/*.env`."
                        ),
                    );
                }
            }
            for (path, permission) in &filesystem.entries {
                file_system_sandbox_policy
                    .entries
                    .extend(compile_filesystem_permission(
                        path,
                        permission,
                        policy_cwd,
                        startup_warnings,
                    )?);
            }
        }
    } else if file_system_sandbox_policy.entries.is_empty() {
        push_warning(
            startup_warnings,
            missing_filesystem_entries_warning(profile_name),
        );
    }
    let glob_scan_max_depth = validate_glob_scan_max_depth(
        profile
            .filesystem
            .as_ref()
            .and_then(|filesystem| filesystem.glob_scan_max_depth),
    )?;
    if let Some(glob_scan_max_depth) = glob_scan_max_depth {
        file_system_sandbox_policy.glob_scan_max_depth = Some(glob_scan_max_depth);
    }
    let network_sandbox_policy =
        compile_network_sandbox_policy(profile.network.as_ref(), base_network_sandbox_policy);
    Ok((file_system_sandbox_policy, network_sandbox_policy))
}

/// 编译「选中的 profile」为运行时权限：先尝试当作内置档（命中则直接转运行时权限
/// 返回），否则按命名档走 `compile_permission_profile`。这是 mod.rs 装配 `Config`
/// 时实际调用的统一入口，把内置档与命名档两条路径合并。
pub(crate) fn compile_permission_profile_selection(
    permissions: Option<&PermissionsToml>,
    profile_name: &str,
    workspace_write: Option<&SandboxWorkspaceWrite>,
    policy_cwd: &Path,
    startup_warnings: &mut Vec<String>,
) -> io::Result<(FileSystemSandboxPolicy, NetworkSandboxPolicy)> {
    if let Some(permission_profile) = builtin_permission_profile(profile_name, workspace_write) {
        return Ok(permission_profile.to_runtime_permissions());
    }
    reject_unknown_builtin_permission_profile(profile_name)?;

    let permissions = permissions.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "default_permissions requires a `[permissions]` table",
        )
    })?;
    compile_permission_profile(permissions, profile_name, policy_cwd, startup_warnings)
}

pub(crate) fn compile_permission_profile_workspace_roots(
    permissions: Option<&PermissionsToml>,
    profile_name: &str,
    policy_cwd: &Path,
) -> io::Result<Vec<AbsolutePathBuf>> {
    if is_builtin_permission_profile_name(profile_name) {
        return Ok(Vec::new());
    }
    reject_unknown_builtin_permission_profile(profile_name)?;

    let permissions = permissions.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "default_permissions requires a `[permissions]` table",
        )
    })?;
    let profile = resolve_permission_profile(permissions, profile_name)?;
    Ok(compile_workspace_roots(
        profile.profile.workspace_roots.as_ref(),
        policy_cwd,
    ))
}

fn compile_workspace_roots(
    workspace_roots: Option<&WorkspaceRootsToml>,
    policy_cwd: &Path,
) -> Vec<AbsolutePathBuf> {
    workspace_roots.map_or_else(Vec::new, |workspace_roots| {
        workspace_roots
            .enabled_roots()
            .map(|path| AbsolutePathBuf::resolve_path_against_base(path, policy_cwd))
            .collect()
    })
}

pub(crate) fn reject_unknown_builtin_permission_profile(profile_name: &str) -> io::Result<()> {
    if profile_name.starts_with(':') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("default_permissions refers to unknown built-in profile `{profile_name}`"),
        ));
    }

    Ok(())
}

/// Returns a list of paths that must be readable by shell tools in order
/// for Codex to function. These should always be added to the
/// `FileSystemSandboxPolicy` for a thread.
///
/// 返回 Codex 运行时必须可读的路径：shell 工具依赖的补丁版 zsh、以及 shell 提权用
/// 的 execve wrapper 可执行文件所在目录。无论用户配了多严格的沙箱，这些 root 都应
/// 追加进 `FileSystemSandboxPolicy`，否则 Codex 自身无法工作。
///
/// 其中对 execve wrapper 有一处特殊处理：若它位于 `$CODEX_HOME/tmp/arg0` 之下
/// （Codex 临时落地的 arg0 包装），则只放开其父目录而非该文件本身——[推测] 这样可
/// 覆盖该目录下成对出现的包装文件，避免逐个登记。
pub(crate) fn get_readable_roots_required_for_codex_runtime(
    codex_home: &Path,
    zsh_path: Option<&PathBuf>,
    main_execve_wrapper_exe: Option<&PathBuf>,
) -> Vec<AbsolutePathBuf> {
    let arg0_root = AbsolutePathBuf::from_absolute_path(codex_home.join("tmp").join("arg0")).ok();
    let zsh_path = zsh_path.and_then(|path| AbsolutePathBuf::from_absolute_path(path).ok());
    let execve_wrapper_root = main_execve_wrapper_exe.and_then(|path| {
        let path = AbsolutePathBuf::from_absolute_path(path).ok()?;
        if let Some(arg0_root) = arg0_root.as_ref()
            && path.as_path().starts_with(arg0_root.as_path())
        {
            path.parent()
        } else {
            Some(path)
        }
    });

    let mut readable_roots = Vec::new();
    if let Some(zsh_path) = zsh_path {
        readable_roots.push(zsh_path);
    }
    if let Some(execve_wrapper_root) = execve_wrapper_root {
        readable_roots.push(execve_wrapper_root);
    }
    readable_roots
}

/// 编译网络沙箱策略：profile 的 `network.enabled` 显式 true/false 时分别映射为
/// `Enabled`/`Restricted`；未设置（`None`）则沿用继承到的基线策略。无 `[network]`
/// 块时同样回退基线。
fn compile_network_sandbox_policy(
    network: Option<&NetworkToml>,
    base_network_sandbox_policy: NetworkSandboxPolicy,
) -> NetworkSandboxPolicy {
    let Some(network) = network else {
        return base_network_sandbox_policy;
    };

    match network.enabled {
        Some(true) => NetworkSandboxPolicy::Enabled,
        Some(false) => NetworkSandboxPolicy::Restricted,
        None => base_network_sandbox_policy,
    }
}

/// 编译单条 `[filesystem]` 条目为零或多个 `FileSystemSandboxEntry`。
///
/// `[filesystem]` 条目有两种形态：
/// - `Access`：`path = "read"/"write"/"deny"`，一条路径对应一种访问模式。
/// - `Scoped`：`path = { subpath = mode, ... }`，在某个基准路径下再嵌套若干子路径
///   规则（一条键展开成多条 entry）。
///
/// 其中 glob + `deny` 的 scoped 条目是「一等公民」的 glob 模式 entry（沙箱原生支持
/// deny glob）；其余 scoped 条目仍走精确路径解析，以保持既有路径语义不变。
fn compile_filesystem_permission(
    path: &str,
    permission: &FilesystemPermissionToml,
    policy_cwd: &Path,
    startup_warnings: &mut Vec<String>,
) -> io::Result<Vec<FileSystemSandboxEntry>> {
    let mut entries = Vec::new();
    match permission {
        FilesystemPermissionToml::Access(access) => {
            entries.push(FileSystemSandboxEntry {
                path: compile_filesystem_access_path(path, *access, startup_warnings)?,
                access: *access,
            });
        }
        FilesystemPermissionToml::Scoped(scoped_entries) => {
            for (subpath, access) in scoped_entries {
                let has_glob = contains_glob_chars(subpath);
                let can_compile_as_pattern = match parse_special_path(path) {
                    Some(FileSystemSpecialPath::ProjectRoots { .. }) | None => true,
                    Some(_) => false,
                };
                if has_glob && *access == FileSystemAccessMode::Deny && can_compile_as_pattern {
                    // Scoped glob syntax is a first-class filesystem policy
                    // pattern entry. Literal scoped paths continue through the
                    // exact-path parser so existing path semantics stay intact.
                    let entry = FileSystemSandboxEntry {
                        path: FileSystemPath::GlobPattern {
                            pattern: compile_scoped_filesystem_pattern(
                                path, subpath, *access, policy_cwd,
                            )?,
                        },
                        access: *access,
                    };
                    entries.push(entry);
                } else {
                    let subpath = compile_read_write_glob_path(subpath, *access)?;
                    entries.push(FileSystemSandboxEntry {
                        path: compile_scoped_filesystem_path(path, subpath, startup_warnings)?,
                        access: *access,
                    });
                }
            }
        }
    }
    Ok(entries)
}

fn compile_filesystem_access_path(
    path: &str,
    access: FileSystemAccessMode,
    startup_warnings: &mut Vec<String>,
) -> io::Result<FileSystemPath> {
    if !contains_glob_chars(path) {
        return compile_filesystem_path(path, startup_warnings);
    }

    if access == FileSystemAccessMode::Deny {
        // At this point `path` is an unscoped filesystem table key. Top-level
        // glob deny entries still go through the absolute-path parser before
        // becoming policy patterns; relative project-root glob syntax is
        // handled by `compile_scoped_filesystem_pattern`.
        return Ok(FileSystemPath::GlobPattern {
            pattern: parse_absolute_path(path)?.to_string_lossy().into_owned(),
        });
    }

    let path = compile_read_write_glob_path(path, access)?;
    compile_filesystem_path(path, startup_warnings)
}

fn compile_filesystem_path(
    path: &str,
    startup_warnings: &mut Vec<String>,
) -> io::Result<FileSystemPath> {
    if let Some(special) = parse_special_path(path) {
        maybe_push_unknown_special_path_warning(&special, startup_warnings);
        return Ok(FileSystemPath::Special { value: special });
    }

    let path = parse_absolute_path(path)?;
    Ok(FileSystemPath::Path { path })
}

fn compile_scoped_filesystem_path(
    path: &str,
    subpath: &str,
    startup_warnings: &mut Vec<String>,
) -> io::Result<FileSystemPath> {
    if subpath == "." {
        return compile_filesystem_path(path, startup_warnings);
    }

    if let Some(special) = parse_special_path(path) {
        let subpath = parse_relative_subpath(subpath)?;
        let special = match special {
            FileSystemSpecialPath::ProjectRoots { .. } => Ok(FileSystemPath::Special {
                value: FileSystemSpecialPath::project_roots(Some(subpath)),
            }),
            FileSystemSpecialPath::Unknown { path, .. } => Ok(FileSystemPath::Special {
                value: FileSystemSpecialPath::unknown(path, Some(subpath)),
            }),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("filesystem path `{path}` does not support nested entries"),
            )),
        }?;
        if let FileSystemPath::Special { value } = &special {
            maybe_push_unknown_special_path_warning(value, startup_warnings);
        }
        return Ok(special);
    }

    let subpath = parse_relative_subpath(subpath)?;
    let base = parse_absolute_path(path)?;
    let path = AbsolutePathBuf::resolve_path_against_base(&subpath, base.as_path());
    Ok(FileSystemPath::Path { path })
}

/// 把「scoped glob 子路径」编译为最终的 glob 模式字符串。当前仅支持 `deny`（见
/// 下方英文说明）。对 `:workspace_roots` 基准的 glob 会保持**符号化**
/// （`project_roots_glob_pattern`），延后到运行时已知实际 workspace roots 后再
/// 物化；普通绝对路径基准则直接拼接 base + subpath。
fn compile_scoped_filesystem_pattern(
    path: &str,
    subpath: &str,
    access: FileSystemAccessMode,
    _policy_cwd: &Path,
) -> io::Result<String> {
    // Pattern entries currently mean deny-read only. Supporting broader access
    // modes here would imply glob-based read/write allow semantics that the
    // sandbox policy does not express yet.
    if access != FileSystemAccessMode::Deny {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("filesystem glob subpath `{subpath}` only supports `deny` access"),
        ));
    }
    let subpath = parse_relative_subpath(subpath)?;

    match parse_special_path(path) {
        Some(FileSystemSpecialPath::ProjectRoots { .. }) => {
            // Keep `:workspace_roots` glob patterns symbolic until the active
            // workspace roots are known, then materialize them for cwd and any
            // runtime/profile-added workspace roots together.
            Ok(project_roots_glob_pattern(&subpath))
        }
        Some(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("filesystem path `{path}` does not support nested entries"),
        )),
        None => {
            let base = parse_absolute_path(path)?;
            Ok(base.join(&subpath).to_string_lossy().to_string())
        }
    }
}

fn compile_read_write_glob_path(path: &str, access: FileSystemAccessMode) -> io::Result<&str> {
    if !contains_glob_chars(path) {
        return Ok(path);
    }

    let path_without_trailing_glob = remove_trailing_glob_suffix(path);
    if !contains_glob_chars(path_without_trailing_glob) {
        return Ok(path_without_trailing_glob);
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "filesystem glob path `{path}` only supports `deny` access; use an exact path or trailing `/**` for `{access}` subtree access"
        ),
    ))
}

fn unsupported_read_write_glob_paths(filesystem: &FilesystemPermissionsToml) -> Vec<String> {
    let mut patterns = Vec::new();
    for (path, permission) in &filesystem.entries {
        match permission {
            FilesystemPermissionToml::Access(access) => {
                if *access != FileSystemAccessMode::Deny
                    && contains_glob_chars(remove_trailing_glob_suffix(path))
                {
                    patterns.push(path.clone());
                }
            }
            FilesystemPermissionToml::Scoped(scoped_entries) => {
                for (subpath, access) in scoped_entries {
                    if *access != FileSystemAccessMode::Deny
                        && contains_glob_chars(remove_trailing_glob_suffix(subpath))
                    {
                        patterns.push(format!("{path}/{subpath}"));
                    }
                }
            }
        }
    }
    patterns
}

fn unbounded_unreadable_globstar_paths(filesystem: &FilesystemPermissionsToml) -> Vec<String> {
    if filesystem.glob_scan_max_depth.is_some() {
        return Vec::new();
    }

    let mut patterns = Vec::new();
    for (path, permission) in &filesystem.entries {
        match permission {
            FilesystemPermissionToml::Access(FileSystemAccessMode::Deny) => {
                if path.contains("**") {
                    patterns.push(path.clone());
                }
            }
            FilesystemPermissionToml::Access(_) => {}
            FilesystemPermissionToml::Scoped(scoped_entries) => {
                for (subpath, access) in scoped_entries {
                    if *access == FileSystemAccessMode::Deny && subpath.contains("**") {
                        patterns.push(format!("{path}/{subpath}"));
                    }
                }
            }
        }
    }
    patterns
}

fn validate_glob_scan_max_depth(max_depth: Option<usize>) -> io::Result<Option<usize>> {
    match max_depth {
        Some(0) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "glob_scan_max_depth must be at least 1",
        )),
        _ => Ok(max_depth),
    }
}

fn contains_glob_chars(path: &str) -> bool {
    contains_glob_chars_for_platform(path, cfg!(windows))
}

fn contains_glob_chars_for_platform(path: &str, is_windows: bool) -> bool {
    let normalized_windows_path = if is_windows {
        normalize_windows_device_path(path)
    } else {
        None
    };
    let path = normalized_windows_path.as_deref().unwrap_or(path);
    path.chars().any(|ch| matches!(ch, '*' | '?' | '[' | ']'))
}

fn remove_trailing_glob_suffix(path: &str) -> &str {
    path.strip_suffix("/**").unwrap_or(path)
}

// WARNING: keep this parser forward-compatible.
// Adding a new `:special_path` must not make older Codex versions reject the
// config. Unknown values intentionally round-trip through
// `FileSystemSpecialPath::Unknown` so they can be surfaced as warnings and
// ignored, rather than aborting config load.
//
// 解析 `:special_path` 符号化路径（`:root` / `:minimal` / `:workspace_roots` /
// `:tmpdir`）。铁律：保持向前兼容——未来版本新增的 `:xxx` 不能让旧版 Codex 拒绝整份
// config。故所有未识别的 `:` 前缀值都刻意降级为 `FileSystemSpecialPath::Unknown`
// （后续以警告提示并忽略），而非报错中断加载。非 `:` 开头返回 `None`（按普通路径处理）。
fn parse_special_path(path: &str) -> Option<FileSystemSpecialPath> {
    match path {
        ":root" => Some(FileSystemSpecialPath::Root),
        ":minimal" => Some(FileSystemSpecialPath::Minimal),
        ":workspace_roots" => Some(FileSystemSpecialPath::project_roots(/*subpath*/ None)),
        ":tmpdir" => Some(FileSystemSpecialPath::Tmpdir),
        _ if path.starts_with(':') => {
            Some(FileSystemSpecialPath::unknown(path, /*subpath*/ None))
        }
        _ => None,
    }
}

fn parse_absolute_path(path: &str) -> io::Result<AbsolutePathBuf> {
    parse_absolute_path_for_platform(path, cfg!(windows))
}

fn parse_absolute_path_for_platform(path: &str, is_windows: bool) -> io::Result<AbsolutePathBuf> {
    let path_ref = normalize_absolute_path_for_platform(path, is_windows);
    if !is_absolute_path_for_platform(path, path_ref.as_ref(), is_windows)
        && path != "~"
        && !path.starts_with("~/")
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("filesystem path `{path}` must be absolute, use `~/...`, or start with `:`"),
        ));
    }
    AbsolutePathBuf::from_absolute_path(path_ref.as_ref())
}

fn is_absolute_path_for_platform(path: &str, normalized_path: &Path, is_windows: bool) -> bool {
    if is_windows {
        is_windows_absolute_path(path)
            || is_windows_absolute_path(&normalized_path.to_string_lossy())
    } else {
        normalized_path.is_absolute()
    }
}

fn normalize_absolute_path_for_platform(path: &str, is_windows: bool) -> Cow<'_, Path> {
    if !is_windows {
        return Cow::Borrowed(Path::new(path));
    }

    match normalize_windows_device_path(path) {
        Some(normalized) => Cow::Owned(PathBuf::from(normalized)),
        None => Cow::Borrowed(Path::new(path)),
    }
}

fn normalize_windows_device_path(path: &str) -> Option<String> {
    if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
        return Some(format!(r"\\{unc}"));
    }
    if let Some(unc) = path.strip_prefix(r"\\.\UNC\") {
        return Some(format!(r"\\{unc}"));
    }
    if let Some(path) = path.strip_prefix(r"\\?\")
        && is_windows_drive_absolute_path(path)
    {
        return Some(path.to_string());
    }
    if let Some(path) = path.strip_prefix(r"\\.\")
        && is_windows_drive_absolute_path(path)
    {
        return Some(path.to_string());
    }
    None
}

fn is_windows_absolute_path(path: &str) -> bool {
    is_windows_drive_absolute_path(path) || path.starts_with(r"\\")
}

fn is_windows_drive_absolute_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/')
}

/// 校验并解析 scoped 条目里的相对子路径。安全约束：子路径必须是「后代路径」——
/// 仅允许普通路径分量（`Component::Normal`），任何 `.` / `..` / 绝对路径前缀都会被
/// 拒绝。这是防止配置借子路径穿越到基准目录之外的关键防线。
fn parse_relative_subpath(subpath: &str) -> io::Result<PathBuf> {
    let path = Path::new(subpath);
    if !subpath.is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return Ok(path.to_path_buf());
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "filesystem subpath `{}` must be a descendant path without `.` or `..` components",
            path.display()
        ),
    ))
}

fn push_warning(startup_warnings: &mut Vec<String>, message: String) {
    tracing::warn!("{message}");
    startup_warnings.push(message);
}

fn missing_filesystem_entries_warning(profile_name: &str) -> String {
    format!(
        "Permissions profile `{profile_name}` does not define any recognized filesystem entries for this version of Codex. Filesystem access will remain restricted. Upgrade Codex if this profile expects filesystem permissions."
    )
}

fn maybe_push_unknown_special_path_warning(
    special: &FileSystemSpecialPath,
    startup_warnings: &mut Vec<String>,
) {
    let FileSystemSpecialPath::Unknown { path, subpath } = special else {
        return;
    };
    push_warning(
        startup_warnings,
        match subpath.as_deref() {
            Some(subpath) => format!(
                "Configured filesystem path `{path}` with nested entry `{}` is not recognized by this version of Codex and will be ignored. Upgrade Codex if this path is required.",
                subpath.display()
            ),
            None => format!(
                "Configured filesystem path `{path}` is not recognized by this version of Codex and will be ignored. Upgrade Codex if this path is required."
            ),
        },
    );
}

#[cfg(test)]
#[path = "permissions_tests.rs"]
mod tests;
