//! 【文件职责】探测宿主机可用的 shell（zsh / bash / sh / PowerShell / cmd），
//!   并把「一条命令字符串」翻译成各 shell 对应的 `exec()` 参数列表。
//!
//! 【架构位置】
//!   层级：工具执行层 / shell 抽象
//!   上游：执行命令的运行时（如 shell 工具、命令执行路径）需要知道「用哪个
//!         shell、传什么参数」时调用本模块
//!   下游：`shell_detect`（按路径判定 shell 类型）、`shell_snapshot`
//!         （环境快照）、`which` crate（在 PATH 中定位可执行文件）、`libc`
//!         （Unix 下读取用户默认 shell）
//!
//! 【数据流】
//!   shell 类型 + 可选路径
//!     → `get_shell()` 逐级探测出可执行路径 → 构造 `Shell`
//!     → `Shell::derive_exec_args(command)` → 可直接交给 `exec()` 的 argv
//!
//! 【阅读建议】
//!   1. 先看 `Shell::derive_exec_args`：理解各 shell 的命令调用约定差异。
//!   2. 再看 `get_shell` → `get_shell_path` 的「四级回退」探测逻辑。
//!   3. `default_user_shell` 是无模型指定路径时的默认选择策略。
//!   4. `get_user_shell_path`（Unix）涉及 libc unsafe，独立成段可后看。

use crate::shell_detect::detect_shell_type;
use crate::shell_snapshot::ShellSnapshot;
use serde::Deserialize;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::watch;

/// 受支持的 shell 种类。不同种类的命令调用约定差异很大（见
/// `derive_exec_args`），因此先归类再分派。
#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub enum ShellType {
    Zsh,
    Bash,
    PowerShell,
    Sh,
    Cmd,
}

/// 一个已探测就绪、可用于执行命令的 shell 实例。
///
/// 同时携带 shell 类型、可执行路径，以及一份「环境快照」的订阅端，后者用于
/// 在执行命令时复现用户的 shell 环境（如 PATH、自定义变量）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Shell {
    pub(crate) shell_type: ShellType,
    pub(crate) shell_path: PathBuf,
    // shell 环境快照的 watch 订阅端：快照异步生成，故用 `watch::Receiver`
    // 随时取最新值（可能尚未就绪，故为 `Option`）。
    // 序列化时跳过该字段（运行期状态、不应持久化），反序列化时用
    // `empty_shell_snapshot_receiver` 补一个空通道占位。
    #[serde(
        skip_serializing,
        skip_deserializing,
        default = "empty_shell_snapshot_receiver"
    )]
    pub(crate) shell_snapshot: watch::Receiver<Option<Arc<ShellSnapshot>>>,
}

impl Shell {
    pub fn name(&self) -> &'static str {
        match self.shell_type {
            ShellType::Zsh => "zsh",
            ShellType::Bash => "bash",
            ShellType::PowerShell => "powershell",
            ShellType::Sh => "sh",
            ShellType::Cmd => "cmd",
        }
    }

    /// Takes a string of shell and returns the full list of command args to
    /// use with `exec()` to run the shell command.
    ///
    /// 把一条命令字符串翻译成「可直接交给 `exec()` 的完整 argv」。
    ///
    /// @param command         - 要执行的命令文本（整段交给 shell 解析）
    /// @param use_login_shell - 是否以登录 shell 运行：true 时加载用户的
    ///                          登录配置（如 `~/.zprofile`），命令能看到用户
    ///                          的自定义 PATH 等；false 则跳过 profile 以求更快
    /// @returns               - 形如 `[shell路径, 模式参数, command]` 的 argv
    ///
    /// 各 shell 的调用约定不同，故按类型分派；argv[0] 统一为 shell 自身路径。
    pub fn derive_exec_args(&self, command: &str, use_login_shell: bool) -> Vec<String> {
        match self.shell_type {
            ShellType::Zsh | ShellType::Bash | ShellType::Sh => {
                // POSIX 系：`-lc` = 登录 shell + 执行命令，`-c` = 仅执行命令。
                let arg = if use_login_shell { "-lc" } else { "-c" };
                vec![
                    self.shell_path.to_string_lossy().to_string(),
                    arg.to_string(),
                    command.to_string(),
                ]
            }
            ShellType::PowerShell => {
                let mut args = vec![self.shell_path.to_string_lossy().to_string()];
                // PowerShell 默认就会加载 profile；要「非登录」语义需显式
                // 用 `-NoProfile` 关掉，逻辑与 POSIX 系正好相反。
                if !use_login_shell {
                    args.push("-NoProfile".to_string());
                }

                args.push("-Command".to_string());
                args.push(command.to_string());
                args
            }
            ShellType::Cmd => {
                // cmd.exe 无登录态概念，故忽略 `use_login_shell`，恒用 `/c`。
                let mut args = vec![self.shell_path.to_string_lossy().to_string()];
                args.push("/c".to_string());
                args.push(command.to_string());
                args
            }
        }
    }

    /// Return the shell snapshot if existing.
    pub fn shell_snapshot(&self) -> Option<Arc<ShellSnapshot>> {
        self.shell_snapshot.borrow().clone()
    }
}

// 构造一个「永远为空」的快照订阅端占位：发送端 `_tx` 立即丢弃，故通道
// 永不更新，`borrow()` 始终读到初始的 `None`。用于反序列化或尚无真实快照
// 时给 `Shell::shell_snapshot` 字段填默认值。
pub(crate) fn empty_shell_snapshot_receiver() -> watch::Receiver<Option<Arc<ShellSnapshot>>> {
    let (_tx, rx) = watch::channel(None);
    rx
}

impl PartialEq for Shell {
    fn eq(&self, other: &Self) -> bool {
        self.shell_type == other.shell_type && self.shell_path == other.shell_path
    }
}

impl Eq for Shell {}

// 读取当前用户在 `/etc/passwd` 中登记的默认登录 shell（Unix）。
// 失败（无该用户、字段缺失、libc 出错）一律返回 `None`，由调用方回退。
#[cfg(unix)]
fn get_user_shell_path() -> Option<PathBuf> {
    let uid = unsafe { libc::getuid() };
    use std::ffi::CStr;
    use std::mem::MaybeUninit;
    use std::ptr;

    let mut passwd = MaybeUninit::<libc::passwd>::uninit();

    // We cannot use getpwuid here: it returns pointers into libc-managed
    // storage, which is not safe to read concurrently on all targets (the musl
    // static build used by the CLI can segfault when parallel callers race on
    // that buffer). getpwuid_r keeps the passwd data in caller-owned memory.
    // 为何不用 `getpwuid`：它返回指向 libc 内部静态缓冲区的指针，多线程并发
    // 读取并不安全——CLI 用的 musl 静态构建在并发争用该缓冲区时会 segfault。
    // `getpwuid_r`（可重入版）把结果写进调用方自备的 `buffer`，从而线程安全。
    let suggested_buffer_len = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let buffer_len = usize::try_from(suggested_buffer_len)
        .ok()
        .filter(|len| *len > 0)
        .unwrap_or(1024);
    let mut buffer = vec![0; buffer_len];

    loop {
        let mut result = ptr::null_mut();
        let status = unsafe {
            libc::getpwuid_r(
                uid,
                passwd.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };

        // status == 0 表示调用成功，但「成功」不等于「找到了该用户」：
        // 找不到条目时 `result` 会被置空指针，需单独判 `None`。
        if status == 0 {
            if result.is_null() {
                return None;
            }

            let passwd = unsafe { passwd.assume_init_ref() };
            if passwd.pw_shell.is_null() {
                return None;
            }

            let shell_path = unsafe { CStr::from_ptr(passwd.pw_shell) }
                .to_string_lossy()
                .into_owned();
            return Some(PathBuf::from(shell_path));
        }

        // ERANGE 之外的错误码无法靠扩容解决，直接放弃。
        if status != libc::ERANGE {
            return None;
        }

        // Retry with a larger buffer until libc can materialize the passwd entry.
        // ERANGE 意味着 buffer 太小：翻倍重试，直到 libc 能容下该 passwd 条目。
        // 设 1MiB 上限作为防御，避免异常情况下无限扩容耗尽内存。
        let new_len = buffer.len().checked_mul(2)?;
        if new_len > 1024 * 1024 {
            return None;
        }
        buffer.resize(new_len, 0);
    }
}

#[cfg(not(unix))]
fn get_user_shell_path() -> Option<PathBuf> {
    None
}

fn file_exists(path: &PathBuf) -> Option<PathBuf> {
    if std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file()) {
        Some(PathBuf::from(path))
    } else {
        None
    }
}

/// 为指定 `shell_type` 定位一个真实存在的可执行文件路径。
///
/// 按「四级回退」从最可信到最兜底逐步尝试，命中即返回，全部落空返回 `None`：
///   1. 调用方显式给的 `provided_path`（且文件确实存在）；
///   2. 用户默认登录 shell（若其类型恰好匹配且文件存在）；
///   3. 在 PATH 中 `which` 查找 `binary_name`；
///   4. 逐个尝试硬编码的 `fallback_paths`。
fn get_shell_path(
    shell_type: ShellType,
    provided_path: Option<&PathBuf>,
    binary_name: &str,
    fallback_paths: &[&str],
) -> Option<PathBuf> {
    // If exact provided path exists, use it
    // 第 1 级：调用方明确指定的路径优先（仅当文件真实存在时采纳）。
    if provided_path.and_then(file_exists).is_some() {
        return provided_path.cloned();
    }

    // Check if the shell we are trying to load is user's default shell
    // if just use it
    // 第 2 级：若用户的默认登录 shell 恰好就是要找的这种类型，直接复用，
    // 这样能尊重用户的真实环境而非随便挑一个同名二进制。
    let default_shell_path = get_user_shell_path();
    if let Some(default_shell_path) = default_shell_path
        && detect_shell_type(&default_shell_path) == Some(shell_type)
        && file_exists(&default_shell_path).is_some()
    {
        return Some(default_shell_path);
    }

    // 第 3 级：按可执行名在 PATH 中查找（最常见的命中路径）。
    if let Ok(path) = which::which(binary_name) {
        return Some(path);
    }

    // 第 4 级：PATH 也找不到时，逐个试硬编码的常见安装位置（见各 *_FALLBACK_PATHS）。
    for path in fallback_paths {
        //check exists
        if let Some(path) = file_exists(&PathBuf::from(path)) {
            return Some(path);
        }
    }

    None
}

const ZSH_FALLBACK_PATHS: &[&str] = &["/bin/zsh"];

fn get_zsh_shell(path: Option<&PathBuf>) -> Option<Shell> {
    let shell_path = get_shell_path(ShellType::Zsh, path, "zsh", ZSH_FALLBACK_PATHS);

    shell_path.map(|shell_path| Shell {
        shell_type: ShellType::Zsh,
        shell_path,
        shell_snapshot: empty_shell_snapshot_receiver(),
    })
}

const BASH_FALLBACK_PATHS: &[&str] = &["/bin/bash"];

fn get_bash_shell(path: Option<&PathBuf>) -> Option<Shell> {
    let shell_path = get_shell_path(ShellType::Bash, path, "bash", BASH_FALLBACK_PATHS);

    shell_path.map(|shell_path| Shell {
        shell_type: ShellType::Bash,
        shell_path,
        shell_snapshot: empty_shell_snapshot_receiver(),
    })
}

const SH_FALLBACK_PATHS: &[&str] = &["/bin/sh"];

fn get_sh_shell(path: Option<&PathBuf>) -> Option<Shell> {
    let shell_path = get_shell_path(ShellType::Sh, path, "sh", SH_FALLBACK_PATHS);

    shell_path.map(|shell_path| Shell {
        shell_type: ShellType::Sh,
        shell_path,
        shell_snapshot: empty_shell_snapshot_receiver(),
    })
}

// Note the `pwsh` and `powershell` fallback paths are where the respective
// shells are commonly installed on GitHub Actions Windows runners, but may not
// be present on all Windows machines:
// https://docs.github.com/en/actions/tutorials/build-and-test-code/powershell
// 下面这些 `pwsh` / `powershell` 兜底路径取自 GitHub Actions Windows runner 的
// 常见安装位置，方便 CI 场景命中；普通 Windows 机器不一定有这些路径。

#[cfg(windows)]
const PWSH_FALLBACK_PATHS: &[&str] = &[r#"C:\Program Files\PowerShell\7\pwsh.exe"#];
#[cfg(not(windows))]
const PWSH_FALLBACK_PATHS: &[&str] = &["/usr/local/bin/pwsh"];

#[cfg(windows)]
const POWERSHELL_FALLBACK_PATHS: &[&str] =
    &[r#"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"#];
#[cfg(not(windows))]
const POWERSHELL_FALLBACK_PATHS: &[&str] = &[];

fn get_powershell_shell(path: Option<&PathBuf>) -> Option<Shell> {
    // 优先找跨平台的 PowerShell 7（`pwsh`），找不到再退回 Windows 自带的
    // 旧版 `powershell`（Windows PowerShell 5.x）。
    let shell_path = get_shell_path(ShellType::PowerShell, path, "pwsh", PWSH_FALLBACK_PATHS)
        .or_else(|| {
            get_shell_path(
                ShellType::PowerShell,
                path,
                "powershell",
                POWERSHELL_FALLBACK_PATHS,
            )
        });

    shell_path.map(|shell_path| Shell {
        shell_type: ShellType::PowerShell,
        shell_path,
        shell_snapshot: empty_shell_snapshot_receiver(),
    })
}

fn get_cmd_shell(path: Option<&PathBuf>) -> Option<Shell> {
    let shell_path = get_shell_path(ShellType::Cmd, path, "cmd", &[]);

    shell_path.map(|shell_path| Shell {
        shell_type: ShellType::Cmd,
        shell_path,
        shell_snapshot: empty_shell_snapshot_receiver(),
    })
}

// 所有探测手段都失败时的最终兜底 shell：Windows 用 `cmd.exe`、其余平台用
// `/bin/sh`。这两者在各自平台几乎必然存在，保证调用方总能拿到一个可用 shell。
fn ultimate_fallback_shell() -> Shell {
    if cfg!(windows) {
        Shell {
            shell_type: ShellType::Cmd,
            shell_path: PathBuf::from("cmd.exe"),
            shell_snapshot: empty_shell_snapshot_receiver(),
        }
    } else {
        Shell {
            shell_type: ShellType::Sh,
            shell_path: PathBuf::from("/bin/sh"),
            shell_snapshot: empty_shell_snapshot_receiver(),
        }
    }
}

/// 根据「模型给定的 shell 路径」解析出一个可用 `Shell`。
///
/// 先按路径推断 shell 类型，再据此探测可执行文件；类型识别不出或探测失败时
/// 回退到 `ultimate_fallback_shell`，因此本函数恒返回可用实例（非 `Option`）。
pub fn get_shell_by_model_provided_path(shell_path: &PathBuf) -> Shell {
    detect_shell_type(shell_path)
        .and_then(|shell_type| get_shell(shell_type, Some(shell_path)))
        .unwrap_or(ultimate_fallback_shell())
}

/// 按 shell 类型分派到对应的探测函数；`path` 为调用方可选指定的优先路径。
/// 返回 `None` 表示该类型在本机不可用（无回退，回退由上层决定）。
pub fn get_shell(shell_type: ShellType, path: Option<&PathBuf>) -> Option<Shell> {
    match shell_type {
        ShellType::Zsh => get_zsh_shell(path),
        ShellType::Bash => get_bash_shell(path),
        ShellType::PowerShell => get_powershell_shell(path),
        ShellType::Sh => get_sh_shell(path),
        ShellType::Cmd => get_cmd_shell(path),
    }
}

/// 在「模型未指定 shell」时，挑选一个合理的默认 shell。
/// 入口薄封装：读出用户默认 shell 路径后交给 `default_user_shell_from_path`
/// （拆分是为了让选择逻辑可独立测试，避开真实的 libc 调用）。
pub fn default_user_shell() -> Shell {
    default_user_shell_from_path(get_user_shell_path())
}

// 默认 shell 选择的纯逻辑部分（不直接读系统，便于测试）。
// 选择优先级：尊重「用户自己的默认 shell」> 按平台习惯的候选 > 最终兜底。
fn default_user_shell_from_path(user_shell_path: Option<PathBuf>) -> Shell {
    if cfg!(windows) {
        // Windows 直接选 PowerShell，不参考用户登录 shell（passwd 概念不适用）。
        get_shell(ShellType::PowerShell, /*path*/ None).unwrap_or(ultimate_fallback_shell())
    } else {
        // 先尝试用户在 passwd 中登记的默认 shell（最贴近用户真实环境）。
        let user_default_shell = user_shell_path
            .and_then(|shell| detect_shell_type(&shell))
            .and_then(|shell_type| get_shell(shell_type, /*path*/ None));

        // 用户默认 shell 不可用时按「平台习惯」回退：
        // macOS 优先 zsh（自 Catalina 起的系统默认），其余类 Unix 优先 bash。
        let shell_with_fallback = if cfg!(target_os = "macos") {
            user_default_shell
                .or_else(|| get_shell(ShellType::Zsh, /*path*/ None))
                .or_else(|| get_shell(ShellType::Bash, /*path*/ None))
        } else {
            user_default_shell
                .or_else(|| get_shell(ShellType::Bash, /*path*/ None))
                .or_else(|| get_shell(ShellType::Zsh, /*path*/ None))
        };

        shell_with_fallback.unwrap_or(ultimate_fallback_shell())
    }
}

#[cfg(test)]
mod detect_shell_type_tests {
    use super::*;

    #[test]
    fn test_detect_shell_type() {
        assert_eq!(
            detect_shell_type(&PathBuf::from("zsh")),
            Some(ShellType::Zsh)
        );
        assert_eq!(
            detect_shell_type(&PathBuf::from("bash")),
            Some(ShellType::Bash)
        );
        assert_eq!(
            detect_shell_type(&PathBuf::from("pwsh")),
            Some(ShellType::PowerShell)
        );
        assert_eq!(
            detect_shell_type(&PathBuf::from("powershell")),
            Some(ShellType::PowerShell)
        );
        assert_eq!(detect_shell_type(&PathBuf::from("fish")), None);
        assert_eq!(detect_shell_type(&PathBuf::from("other")), None);
        assert_eq!(
            detect_shell_type(&PathBuf::from("/bin/zsh")),
            Some(ShellType::Zsh)
        );
        assert_eq!(
            detect_shell_type(&PathBuf::from("/bin/bash")),
            Some(ShellType::Bash)
        );
        assert_eq!(
            detect_shell_type(&PathBuf::from("powershell.exe")),
            Some(ShellType::PowerShell)
        );
        assert_eq!(
            detect_shell_type(&PathBuf::from(if cfg!(windows) {
                "C:\\windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe"
            } else {
                "/usr/local/bin/pwsh"
            })),
            Some(ShellType::PowerShell)
        );
        assert_eq!(
            detect_shell_type(&PathBuf::from("pwsh.exe")),
            Some(ShellType::PowerShell)
        );
        assert_eq!(
            detect_shell_type(&PathBuf::from("/usr/local/bin/pwsh")),
            Some(ShellType::PowerShell)
        );
        assert_eq!(
            detect_shell_type(&PathBuf::from("/bin/sh")),
            Some(ShellType::Sh)
        );
        assert_eq!(detect_shell_type(&PathBuf::from("sh")), Some(ShellType::Sh));
        assert_eq!(
            detect_shell_type(&PathBuf::from("cmd")),
            Some(ShellType::Cmd)
        );
        assert_eq!(
            detect_shell_type(&PathBuf::from("cmd.exe")),
            Some(ShellType::Cmd)
        );
    }
}

#[cfg(test)]
#[cfg(unix)]
#[path = "shell_tests.rs"]
mod tests;
