//! 【文件职责】macOS Seatbelt 沙箱的「策略生成器」。把 Codex 的文件/网络沙箱策略翻译成
//! Seatbelt Policy Language（SBPL，.sb 脚本）文本，并拼出最终交给 `/usr/bin/sandbox-exec`
//! 的参数：`-p <策略文本> -D<参数名>=<路径> … -- <真实命令>`。
//! 【为何用 -D 参数而非把路径直接写进策略】路径以 `(param "KEY")` 占位、用 -D 注入，既避免
//!   把含特殊字符的路径塞进 SBPL 文本引发注入/转义问题，也让策略文本本身可缓存/可读。
//! 【三大块策略】文件读（file-read*）、文件写（file-write*，含受保护元数据如 .git 的排除）、
//!   网络（按代理/受管网络情况收紧或放行，无可用端点时「fail closed」彻底禁网）。最终再拼上
//!   基础策略 MACOS_SEATBELT_BASE_POLICY 等。glob 不被 Seatbelt 原生支持，故转成锚定正则。
use codex_network_proxy::NetworkProxy;
use codex_network_proxy::PROXY_URL_ENV_KEYS;
use codex_network_proxy::has_proxy_url_env_vars;
use codex_network_proxy::proxy_url_env_value;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_protocol::permissions::PROTECTED_METADATA_PATH_NAMES;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::WritableRoot;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;
use tracing::warn;
use url::Url;

const MACOS_SEATBELT_BASE_POLICY: &str = include_str!("seatbelt_base_policy.sbpl");
const MACOS_SEATBELT_NETWORK_POLICY: &str = include_str!("seatbelt_network_policy.sbpl");
const MACOS_RESTRICTED_READ_ONLY_PLATFORM_DEFAULTS: &str =
    include_str!("restricted_read_only_platform_defaults.sbpl");

/// When working with `sandbox-exec`, only consider `sandbox-exec` in `/usr/bin`
/// to defend against an attacker trying to inject a malicious version on the
/// PATH. If /usr/bin/sandbox-exec has been tampered with, then the attacker
/// already has root access.
pub const MACOS_PATH_TO_SEATBELT_EXECUTABLE: &str = "/usr/bin/sandbox-exec";

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1"
}

fn proxy_scheme_default_port(scheme: &str) -> u16 {
    match scheme {
        "https" => 443,
        "socks5" | "socks5h" | "socks4" | "socks4a" => 1080,
        _ => 80,
    }
}

fn proxy_loopback_ports_from_env(env: &HashMap<String, String>) -> Vec<u16> {
    let mut ports = BTreeSet::new();
    for key in PROXY_URL_ENV_KEYS {
        let Some(proxy_url) = proxy_url_env_value(env, key) else {
            continue;
        };
        let trimmed = proxy_url.trim();
        if trimmed.is_empty() {
            continue;
        }

        let candidate = if trimmed.contains("://") {
            trimmed.to_string()
        } else {
            format!("http://{trimmed}")
        };
        let Ok(parsed) = Url::parse(&candidate) else {
            continue;
        };
        let Some(host) = parsed.host_str() else {
            continue;
        };
        if !is_loopback_host(host) {
            continue;
        }

        let scheme = parsed.scheme().to_ascii_lowercase();
        let port = parsed
            .port()
            .unwrap_or_else(|| proxy_scheme_default_port(scheme.as_str()));
        ports.insert(port);
    }
    ports.into_iter().collect()
}

#[derive(Debug, Default)]
struct ProxyPolicyInputs {
    ports: Vec<u16>,
    has_proxy_config: bool,
    allow_local_binding: bool,
    unix_domain_socket_policy: UnixDomainSocketPolicy,
}

#[derive(Debug, Clone)]
// Keep allow-all and allowlist modes disjoint so we don't carry ignored state.
enum UnixDomainSocketPolicy {
    AllowAll,
    Restricted { allowed: Vec<AbsolutePathBuf> },
}

impl Default for UnixDomainSocketPolicy {
    fn default() -> Self {
        Self::Restricted { allowed: vec![] }
    }
}

#[derive(Debug, Clone)]
struct UnixSocketPathParam {
    index: usize,
    path: AbsolutePathBuf,
}

fn proxy_policy_inputs(
    network: Option<&NetworkProxy>,
    extra_allow_unix_sockets: &[AbsolutePathBuf],
) -> ProxyPolicyInputs {
    let extra_allowed = extra_allow_unix_sockets
        .iter()
        .filter_map(|socket_path| normalize_path_for_sandbox(socket_path.as_path()))
        .collect::<Vec<_>>();

    match network {
        Some(network) => {
            let mut env = HashMap::new();
            network.apply_to_env(&mut env);
            let unix_domain_socket_policy = if network.dangerously_allow_all_unix_sockets() {
                UnixDomainSocketPolicy::AllowAll
            } else {
                let mut allowed = network
                    .allow_unix_sockets()
                    .iter()
                    .filter_map(|socket_path| {
                        match normalize_path_for_sandbox(Path::new(socket_path)) {
                            Some(path) => Some(path),
                            None => {
                                warn!(
                                    "ignoring network.allow_unix_sockets entry because it could not be normalized: {socket_path}"
                                );
                                None
                            }
                        }
                    })
                    .collect::<Vec<_>>();
                allowed.extend(extra_allowed);
                UnixDomainSocketPolicy::Restricted { allowed }
            };
            ProxyPolicyInputs {
                ports: proxy_loopback_ports_from_env(&env),
                has_proxy_config: has_proxy_url_env_vars(&env),
                allow_local_binding: network.allow_local_binding(),
                unix_domain_socket_policy,
            }
        }
        None => ProxyPolicyInputs {
            unix_domain_socket_policy: UnixDomainSocketPolicy::Restricted {
                allowed: extra_allowed,
            },
            ..Default::default()
        },
    }
}

fn normalize_path_for_sandbox(path: &Path) -> Option<AbsolutePathBuf> {
    // `AbsolutePathBuf::from_absolute_path()` normalizes relative paths against the current
    // working directory, so keep the explicit check to avoid silently accepting relative entries.
    if !path.is_absolute() {
        return None;
    }

    let absolute_path = AbsolutePathBuf::from_absolute_path(path).ok()?;
    let normalized_path = absolute_path
        .as_path()
        .canonicalize()
        .ok()
        .and_then(|canonical_path| AbsolutePathBuf::from_absolute_path(canonical_path).ok());
    normalized_path.or(Some(absolute_path))
}

fn unix_socket_path_params(proxy: &ProxyPolicyInputs) -> Vec<UnixSocketPathParam> {
    let mut deduped_paths: BTreeMap<String, AbsolutePathBuf> = BTreeMap::new();
    let UnixDomainSocketPolicy::Restricted { allowed } = &proxy.unix_domain_socket_policy else {
        return vec![];
    };
    for path in allowed {
        deduped_paths
            .entry(path.to_string_lossy().to_string())
            .or_insert_with(|| path.clone());
    }

    deduped_paths
        .into_values()
        .enumerate()
        .map(|(index, path)| UnixSocketPathParam { index, path })
        .collect()
}

fn unix_socket_path_param_key(index: usize) -> String {
    format!("UNIX_SOCKET_PATH_{index}")
}

fn unix_socket_dir_params(proxy: &ProxyPolicyInputs) -> Vec<(String, PathBuf)> {
    unix_socket_path_params(proxy)
        .into_iter()
        .map(|param| {
            (
                unix_socket_path_param_key(param.index),
                param.path.into_path_buf(),
            )
        })
        .collect()
}

/// Returns zero or more complete Seatbelt policy lines for unix socket rules.
/// When non-empty, the returned string is newline-terminated so callers can
/// append it directly to larger policy blocks.
fn unix_socket_policy(proxy: &ProxyPolicyInputs) -> String {
    let socket_params = unix_socket_path_params(proxy);
    let has_unix_socket_access = matches!(
        proxy.unix_domain_socket_policy,
        UnixDomainSocketPolicy::AllowAll
    ) || !socket_params.is_empty();
    if !has_unix_socket_access {
        return String::new();
    }

    let mut policy = String::new();
    policy.push_str("(allow system-socket (socket-domain AF_UNIX))\n");
    if matches!(
        proxy.unix_domain_socket_policy,
        UnixDomainSocketPolicy::AllowAll
    ) {
        // Keep AllowAll genuinely broad here; path qualifiers look narrower
        // without a clear macOS behavioral benefit.
        policy.push_str("(allow network-bind (local unix-socket))\n");
        policy.push_str("(allow network-outbound (remote unix-socket))\n");
        return policy;
    }

    for param in socket_params {
        let key = unix_socket_path_param_key(param.index);
        // Use subpath so allowlists cover sockets created beneath approved directories.
        policy.push_str(&format!(
            "(allow network-bind (local unix-socket (subpath (param \"{key}\"))))\n"
        ));
        policy.push_str(&format!(
            "(allow network-outbound (remote unix-socket (subpath (param \"{key}\"))))\n"
        ));
    }
    policy
}

#[cfg_attr(not(test), allow(dead_code))]
fn dynamic_network_policy(
    sandbox_policy: &SandboxPolicy,
    enforce_managed_network: bool,
    proxy: &ProxyPolicyInputs,
) -> String {
    dynamic_network_policy_for_network(
        NetworkSandboxPolicy::from(sandbox_policy),
        enforce_managed_network,
        proxy,
    )
}

/// 生成网络相关的 SBPL 段，是安全上最敏感的一块。三种走向：
/// ① 受限网络（有代理端口 / 配了代理 / 强制受管网络 / 禁网但要放行 unix socket）→ 只放行
///    loopback、必要的 DNS、指定代理端口与 unix socket，再拼基础网络策略。
/// ② 「fail closed」：声明了代理配置或强制受管网络，却推断不出可用 loopback 端点 → 返回空串
///    （= 完全禁网），宁可禁死也不静默放宽——这是刻意的安全默认。
/// ③ 普通放行：无代理且策略允许联网 → 放行全部出入站。否则（禁网）返回空串。
fn dynamic_network_policy_for_network(
    network_policy: NetworkSandboxPolicy,
    enforce_managed_network: bool,
    proxy: &ProxyPolicyInputs,
) -> String {
    let has_some_unix_socket_access = match &proxy.unix_domain_socket_policy {
        UnixDomainSocketPolicy::AllowAll => true,
        UnixDomainSocketPolicy::Restricted { allowed } => !allowed.is_empty(),
    };
    let should_use_restricted_network_policy = !proxy.ports.is_empty()
        || proxy.has_proxy_config
        || enforce_managed_network
        || (!network_policy.is_enabled() && has_some_unix_socket_access);
    if should_use_restricted_network_policy {
        let mut policy = String::new();
        if proxy.allow_local_binding {
            policy.push_str("; allow local binding and loopback traffic\n");
            policy.push_str("(allow network-bind (local ip \"*:*\"))\n");
            policy.push_str("(allow network-inbound (local ip \"localhost:*\"))\n");
            policy.push_str("(allow network-outbound (remote ip \"localhost:*\"))\n");
        }
        if proxy.allow_local_binding && !proxy.ports.is_empty() {
            policy.push_str("; allow DNS lookups while application traffic remains proxy-routed\n");
            policy.push_str("(allow network-outbound (remote ip \"*:53\"))\n");
        }
        for port in &proxy.ports {
            policy.push_str(&format!(
                "(allow network-outbound (remote ip \"localhost:{port}\"))\n"
            ));
        }
        let unix_socket_policy = unix_socket_policy(proxy);
        if !unix_socket_policy.is_empty() {
            policy.push_str("; allow unix domain sockets for local IPC\n");
            policy.push_str(&unix_socket_policy);
        }
        return format!("{policy}{MACOS_SEATBELT_NETWORK_POLICY}");
    }

    if proxy.has_proxy_config {
        // Proxy configuration is present but we could not infer any valid loopback endpoints.
        // Fail closed to avoid silently widening network access in proxy-enforced sessions.
        return String::new();
    }

    if enforce_managed_network {
        // Managed network requirements are active but no usable proxy endpoints
        // are available. Fail closed for network access.
        return String::new();
    }

    if network_policy.is_enabled() {
        // No proxy env is configured: retain the existing full-network behavior.
        let mut policy = String::from("(allow network-outbound)\n(allow network-inbound)\n");
        let unix_socket_policy = unix_socket_policy(proxy);
        if !unix_socket_policy.is_empty() {
            policy.push_str("; allow unix domain sockets for local IPC\n");
            policy.push_str(&unix_socket_policy);
        }
        format!("{policy}{MACOS_SEATBELT_NETWORK_POLICY}")
    } else {
        String::new()
    }
}

fn root_absolute_path() -> AbsolutePathBuf {
    match AbsolutePathBuf::from_absolute_path(Path::new("/")) {
        Ok(path) => path,
        Err(err) => panic!("root path must be absolute: {err}"),
    }
}

#[derive(Debug, Clone)]
struct SeatbeltAccessRoot {
    root: AbsolutePathBuf,
    excluded_subpaths: Vec<AbsolutePathBuf>,
    protected_metadata_names: Vec<String>,
}

/// 为某类文件操作（action 如 "file-read*"/"file-write*"）生成 `(allow …)` 策略段。每个根都
/// 落成一个 -D 参数（param_prefix_N）。若根带「排除子路径」或「受保护元数据名」，则用
/// `(require-all (subpath 根) (require-not …))` 收紧：既禁掉精确路径又禁其子树（光 subpath
/// 会给「首次创建该受保护目录本身」留口子，如 mkdir .codex，故同时排 literal + subpath）。
/// 返回 (策略文本, 需注入的 -D 参数列表)。
fn build_seatbelt_access_policy(
    action: &str,
    param_prefix: &str,
    roots: Vec<SeatbeltAccessRoot>,
) -> (String, Vec<(String, PathBuf)>) {
    let mut policy_components = Vec::new();
    let mut params = Vec::new();

    for (index, access_root) in roots.into_iter().enumerate() {
        let root =
            normalize_path_for_sandbox(access_root.root.as_path()).unwrap_or(access_root.root);
        let root_param = format!("{param_prefix}_{index}");
        params.push((root_param.clone(), root.clone().into_path_buf()));

        if access_root.excluded_subpaths.is_empty()
            && access_root.protected_metadata_names.is_empty()
        {
            policy_components.push(format!("(subpath (param \"{root_param}\"))"));
            continue;
        }

        let mut require_parts = vec![format!("(subpath (param \"{root_param}\"))")];
        for (excluded_index, excluded_subpath) in
            access_root.excluded_subpaths.into_iter().enumerate()
        {
            let excluded_subpath =
                normalize_path_for_sandbox(excluded_subpath.as_path()).unwrap_or(excluded_subpath);
            let excluded_param = format!("{param_prefix}_{index}_EXCLUDED_{excluded_index}");
            params.push((excluded_param.clone(), excluded_subpath.into_path_buf()));
            // Exclude both the exact protected path and anything beneath it.
            // `subpath` alone leaves a gap for first-time creation of the
            // protected directory itself, such as `mkdir .codex`.
            require_parts.push(format!(
                "(require-not (literal (param \"{excluded_param}\")))"
            ));
            require_parts.push(format!(
                "(require-not (subpath (param \"{excluded_param}\")))"
            ));
        }
        for metadata_name in access_root.protected_metadata_names {
            let regex =
                seatbelt_protected_metadata_name_regex(&root, &metadata_name).replace('"', "\\\"");
            require_parts.push(format!(r#"(require-not (regex #"{regex}"))"#));
        }
        policy_components.push(format!("(require-all {} )", require_parts.join(" ")));
    }

    if policy_components.is_empty() {
        (String::new(), Vec::new())
    } else {
        (
            format!("(allow {action}\n{}\n)", policy_components.join(" ")),
            params,
        )
    }
}

fn seatbelt_protected_metadata_name_regex(root: &AbsolutePathBuf, name: &str) -> String {
    let mut root = root.to_string_lossy().to_string();
    while root.len() > 1 && root.ends_with('/') {
        root.pop();
    }
    let root = regex_lite::escape(&root);
    let name = regex_lite::escape(name);
    if root == "/" {
        format!(r#"^/{name}(/.*)?$"#)
    } else {
        format!(r#"^{root}/{name}(/.*)?$"#)
    }
}

fn protected_metadata_names_for_writable_root(
    file_system_sandbox_policy: &FileSystemSandboxPolicy,
    writable_root: &WritableRoot,
    cwd: &Path,
) -> Vec<String> {
    let mut names = writable_root.protected_metadata_names.clone();
    for name in PROTECTED_METADATA_PATH_NAMES {
        if names.iter().any(|existing| existing == name) {
            continue;
        }
        let path = writable_root.root.join(*name);
        if !file_system_sandbox_policy.can_write_path_with_cwd(path.as_path(), cwd) {
            names.push((*name).to_string());
        }
    }
    names
}

fn build_seatbelt_unreadable_glob_policy(
    file_system_sandbox_policy: &FileSystemSandboxPolicy,
    cwd: &Path,
) -> String {
    // Seatbelt does not understand the filesystem policy's glob syntax directly.
    // Convert each unreadable pattern into an anchored regex deny rule and apply
    // it to both reads and unlink-style writes so a denied path cannot be probed
    // through destructive filesystem operations.
    let unreadable_globs = file_system_sandbox_policy.get_unreadable_globs_with_cwd(cwd);
    if unreadable_globs.is_empty() {
        return String::new();
    }

    let mut policy_components = Vec::new();
    for pattern in unreadable_globs {
        let mut regexes = BTreeSet::new();
        if let Some(regex) = seatbelt_regex_for_unreadable_glob(&pattern) {
            regexes.insert(regex);
        }
        if let Some(pattern) = canonicalize_glob_static_prefix_for_sandbox(&pattern)
            && let Some(regex) = seatbelt_regex_for_unreadable_glob(&pattern)
        {
            regexes.insert(regex);
        }
        for regex in regexes {
            let regex = regex.replace('"', "\\\"");
            policy_components.push(format!(r#"(deny file-read* (regex #"{regex}"))"#));
            policy_components.push(format!(r#"(deny file-write-unlink (regex #"{regex}"))"#));
        }
    }

    policy_components.join("\n")
}

fn canonicalize_glob_static_prefix_for_sandbox(pattern: &str) -> Option<String> {
    let first_glob_index = pattern
        .char_indices()
        .find_map(|(index, ch)| matches!(ch, '*' | '?' | '[' | ']').then_some(index));
    let Some(first_glob_index) = first_glob_index else {
        return normalize_path_for_sandbox(Path::new(pattern))
            .map(|path| path.to_string_lossy().to_string());
    };

    let static_prefix = &pattern[..first_glob_index];
    let prefix_end = if static_prefix.ends_with('/') {
        static_prefix.len() - 1
    } else {
        static_prefix.rfind('/').unwrap_or(0)
    };
    if prefix_end == 0 {
        return None;
    }

    let root = normalize_path_for_sandbox(Path::new(&pattern[..prefix_end]))?;
    let root = root.to_string_lossy();
    let suffix = &pattern[prefix_end..];
    let normalized_pattern = format!("{root}{suffix}");
    (normalized_pattern != pattern).then_some(normalized_pattern)
}

/// 把 git 风格的 glob 子集翻译成 Seatbelt 能用的锚定正则（`^…$`）：`*`/`?` 只在单层路径段内
/// 匹配（`[^/]*`/`[^/]`），`**/` 可跨零或多层（`(.*/)?`），方括号字符类保留，未闭合的 `[`
/// 当字面量。完全没有 glob 元字符时按「精确路径 + 其子树」处理（补 `(/.*)?`）。
/// 用途：file-system 策略的「不可读 glob」需转成 deny 规则，而 Seatbelt 不认 glob 语法。
fn seatbelt_regex_for_unreadable_glob(pattern: &str) -> Option<String> {
    if pattern.is_empty() {
        return None;
    }

    // Translate the supported git-style glob subset into a Seatbelt regex:
    // `*` and `?` stay within one path component, `**/` can consume zero or
    // more components, and closed character classes remain character classes.
    // A pattern with no glob metacharacters is treated as exact path plus subtree.
    let mut regex = String::from("^");
    let mut chars = pattern.chars().collect::<VecDeque<_>>();
    let mut saw_glob = false;

    while let Some(ch) = chars.pop_front() {
        match ch {
            '*' => {
                saw_glob = true;
                if chars.front() == Some(&'*') {
                    chars.pop_front();
                    if chars.front() == Some(&'/') {
                        chars.pop_front();
                        regex.push_str("(.*/)?");
                    } else {
                        regex.push_str(".*");
                    }
                } else {
                    regex.push_str("[^/]*");
                }
            }
            '?' => {
                saw_glob = true;
                regex.push_str("[^/]");
            }
            '[' => {
                saw_glob = true;
                let mut class = Vec::new();
                let mut closed = false;
                while let Some(class_ch) = chars.pop_front() {
                    if class_ch == ']' {
                        closed = true;
                        break;
                    }
                    class.push(class_ch);
                }
                if !closed {
                    regex.push_str("\\[");
                    for class_ch in class.into_iter().rev() {
                        chars.push_front(class_ch);
                    }
                    continue;
                }

                regex.push('[');
                let mut class_chars = class.into_iter();
                if let Some(first) = class_chars.next() {
                    match first {
                        '!' => regex.push('^'),
                        '^' => regex.push_str("\\^"),
                        _ => regex.push(first),
                    }
                }
                for class_ch in class_chars {
                    match class_ch {
                        '\\' => regex.push_str("\\\\"),
                        _ => regex.push(class_ch),
                    }
                }
                regex.push(']');
            }
            ']' => {
                saw_glob = true;
                regex.push_str("\\]");
            }
            _ => regex.push_str(&regex_lite::escape(&ch.to_string())),
        }
    }

    if !saw_glob {
        regex.push_str("(/.*)?");
    }
    regex.push('$');
    Some(regex)
}

#[cfg_attr(not(test), allow(dead_code))]
fn create_seatbelt_command_args_for_legacy_policy(
    command: Vec<String>,
    sandbox_policy: &SandboxPolicy,
    sandbox_policy_cwd: &Path,
    enforce_managed_network: bool,
    network: Option<&NetworkProxy>,
) -> Vec<String> {
    let file_system_sandbox_policy = FileSystemSandboxPolicy::from_legacy_sandbox_policy_for_cwd(
        sandbox_policy,
        sandbox_policy_cwd,
    );
    create_seatbelt_command_args(CreateSeatbeltCommandArgsParams {
        command,
        file_system_sandbox_policy: &file_system_sandbox_policy,
        network_sandbox_policy: NetworkSandboxPolicy::from(sandbox_policy),
        sandbox_policy_cwd,
        enforce_managed_network,
        network,
        extra_allow_unix_sockets: &[],
    })
}

#[derive(Debug)]
pub struct CreateSeatbeltCommandArgsParams<'a> {
    pub command: Vec<String>,
    pub file_system_sandbox_policy: &'a FileSystemSandboxPolicy,
    pub network_sandbox_policy: NetworkSandboxPolicy,
    pub sandbox_policy_cwd: &'a Path,
    pub enforce_managed_network: bool,
    pub network: Option<&'a NetworkProxy>,
    pub extra_allow_unix_sockets: &'a [AbsolutePathBuf],
}

/// 本文件的主入口：根据文件/网络策略生成完整 SBPL 文本并组装 sandbox-exec 参数。
/// 流程：①按「是否全盘读/写」分别生成 file-read* / file-write* 段（受限时列出可读/可写根 +
/// 排除只读子路径 + 保护 .git 等元数据）；②生成 deny 段把不可读 glob 转正则禁掉；③生成网络段；
/// ④把各段 + 基础策略拼成 full_policy；⑤所有路径作为 -D 参数注入，最后 `-- command` 收尾。
/// 返回值即 sandbox-exec 之后的全部 argv（不含 sandbox-exec 本身，由 manager.transform 补上）。
pub fn create_seatbelt_command_args(args: CreateSeatbeltCommandArgsParams<'_>) -> Vec<String> {
    let CreateSeatbeltCommandArgsParams {
        command,
        file_system_sandbox_policy,
        network_sandbox_policy,
        sandbox_policy_cwd,
        enforce_managed_network,
        network,
        extra_allow_unix_sockets,
    } = args;

    let unreadable_roots =
        file_system_sandbox_policy.get_unreadable_roots_with_cwd(sandbox_policy_cwd);
    let (file_write_policy, file_write_dir_params) =
        if file_system_sandbox_policy.has_full_disk_write_access() {
            if unreadable_roots.is_empty() {
                // Allegedly, this is more permissive than `(allow file-write*)`.
                (
                    r#"(allow file-write* (regex #"^/"))"#.to_string(),
                    Vec::new(),
                )
            } else {
                build_seatbelt_access_policy(
                    "file-write*",
                    "WRITABLE_ROOT",
                    vec![SeatbeltAccessRoot {
                        root: root_absolute_path(),
                        excluded_subpaths: unreadable_roots.clone(),
                        protected_metadata_names: Vec::new(),
                    }],
                )
            }
        } else {
            build_seatbelt_access_policy(
                "file-write*",
                "WRITABLE_ROOT",
                file_system_sandbox_policy
                    .get_writable_roots_with_cwd(sandbox_policy_cwd)
                    .into_iter()
                    .map(|root| SeatbeltAccessRoot {
                        protected_metadata_names: protected_metadata_names_for_writable_root(
                            file_system_sandbox_policy,
                            &root,
                            sandbox_policy_cwd,
                        ),
                        root: root.root,
                        excluded_subpaths: root.read_only_subpaths,
                    })
                    .collect(),
            )
        };

    let (file_read_policy, file_read_dir_params) =
        if file_system_sandbox_policy.has_full_disk_read_access() {
            if unreadable_roots.is_empty() {
                (
                    "; allow read-only file operations\n(allow file-read*)".to_string(),
                    Vec::new(),
                )
            } else {
                let (policy, params) = build_seatbelt_access_policy(
                    "file-read*",
                    "READABLE_ROOT",
                    vec![SeatbeltAccessRoot {
                        root: root_absolute_path(),
                        excluded_subpaths: unreadable_roots,
                        protected_metadata_names: Vec::new(),
                    }],
                );
                (
                    format!("; allow read-only file operations\n{policy}"),
                    params,
                )
            }
        } else {
            let (policy, params) = build_seatbelt_access_policy(
                "file-read*",
                "READABLE_ROOT",
                file_system_sandbox_policy
                    .get_readable_roots_with_cwd(sandbox_policy_cwd)
                    .into_iter()
                    .map(|root| SeatbeltAccessRoot {
                        excluded_subpaths: unreadable_roots
                            .iter()
                            .filter(|path| path.as_path().starts_with(root.as_path()))
                            .cloned()
                            .collect(),
                        protected_metadata_names: Vec::new(),
                        root,
                    })
                    .collect(),
            );
            if policy.is_empty() {
                (String::new(), params)
            } else {
                (
                    format!("; allow read-only file operations\n{policy}"),
                    params,
                )
            }
        };

    let proxy = proxy_policy_inputs(network, extra_allow_unix_sockets);
    let network_policy =
        dynamic_network_policy_for_network(network_sandbox_policy, enforce_managed_network, &proxy);

    let include_platform_defaults = file_system_sandbox_policy.include_platform_defaults();
    let deny_read_policy =
        build_seatbelt_unreadable_glob_policy(file_system_sandbox_policy, sandbox_policy_cwd);
    let mut policy_sections = vec![
        MACOS_SEATBELT_BASE_POLICY.to_string(),
        file_read_policy,
        file_write_policy,
        deny_read_policy,
        network_policy,
    ];
    if include_platform_defaults {
        policy_sections.push(MACOS_RESTRICTED_READ_ONLY_PLATFORM_DEFAULTS.to_string());
    }

    let full_policy = policy_sections.join("\n");

    let dir_params = [
        file_read_dir_params,
        file_write_dir_params,
        unix_socket_dir_params(&proxy),
    ]
    .concat();

    let mut seatbelt_args: Vec<String> = vec!["-p".to_string(), full_policy];
    let definition_args = dir_params
        .into_iter()
        .map(|(key, value): (String, PathBuf)| {
            format!("-D{key}={value}", value = value.to_string_lossy())
        });
    seatbelt_args.extend(definition_args);
    seatbelt_args.push("--".to_string());
    seatbelt_args.extend(command);
    seatbelt_args
}

#[cfg(test)]
#[path = "seatbelt_tests.rs"]
mod tests;
