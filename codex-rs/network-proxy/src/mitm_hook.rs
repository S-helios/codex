#![cfg_attr(not(test), allow(dead_code))]
//! 【文件职责】定义 MITM「钩子」的配置模型、编译流程与匹配/求值逻辑。钩子
//! 让代理在解密某些 host 的 HTTPS 内层请求后，按规则匹配并改写请求头——
//! 典型用途是向特定 API（如 `api.github.com`）注入鉴权令牌，而令牌不必经过
//! 模型/沙箱明文持有。
//!
//! 【架构位置】
//!   层级：网络代理层（codex-rs/network-proxy）
//!   上游：`runtime.rs`/装配阶段调用 `compile_mitm_hooks` 把配置编译为运行态；
//!         `mitm.rs` 在解密内层 HTTPS 请求后调用 `evaluate_mitm_hooks` 求值。
//!   下游：依赖 `policy::normalize_host` 归一化 host，`globset` 编译通配匹配器。
//!
//! 【两套类型：Config vs 运行态】
//!   - `*Config`（`MitmHookConfig` 等）：可序列化的 TOML 配置原样，字符串形态。
//!   - 运行态（`MitmHook`/`MitmHookMatcher`/`ResolvedInjectedHeader` 等）：
//!     `compile_*` 把字符串编译成已解析的 `HeaderName`/`GlobMatcher`、并把
//!     注入头的密钥从环境变量/文件解析为最终 `HeaderValue`。编译期一次性
//!     完成校验与密钥读取，匹配期（每请求）只做纯比较，避免热路径开销。
//!
//! 【匹配模型】一条钩子需 host 精确命中（不允许通配），再逐项满足 method /
//!   path 前缀 / query / header 约束（全部为 AND）。路径与值支持 `literal:` /
//!   `pattern:` 前缀显式区分「字面量」与「glob 通配」，默认按字面量处理以防
//!   配置里的元字符被意外当作通配。
//!
//! 【阅读建议】先看顶部 Config/运行态类型与 `HookEvaluation`，再看
//!   `compile_mitm_hooks_with_resolvers`（编译主流程）与 `evaluate_mitm_hooks`
//!   /`hook_matches`（求值主流程）；底部 `parse_*`/`validate_*` 是校验工具。

use crate::config::NetworkProxyConfig;
use crate::policy::normalize_host;
use anyhow::Context as _;
use anyhow::Result;
use anyhow::anyhow;
use codex_utils_absolute_path::AbsolutePathBuf;
use globset::GlobBuilder;
use globset::GlobMatcher;
use rama_http::HeaderValue;
use rama_http::Request;
use rama_http::header::HeaderName;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::Path;
use url::form_urlencoded;

// 匹配字符串的两个保留前缀：`pattern:` 显式声明为 glob 通配，`literal:`
// 显式声明为字面量。无前缀时默认按字面量处理（见 `parse_matcher_pattern`），
// 这样配置里出现 `[` `*` 等元字符不会被意外当成通配。
const PATTERN_PREFIX: &str = "pattern:";
const LITERAL_PREFIX: &str = "literal:";

/// 单条 MITM 钩子的配置（TOML 形态）：作用于哪个 `host`、匹配条件 `matcher`、
/// 命中后的动作 `actions`。`#[serde(rename = "match")]` 让 TOML 里写作 `match`。
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct MitmHookConfig {
    pub host: String,
    #[serde(rename = "match", default)]
    pub matcher: MitmHookMatchConfig,
    #[serde(default)]
    pub actions: MitmHookActionsConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct MitmHookMatchConfig {
    pub methods: Vec<String>,
    pub path_prefixes: Vec<String>,
    pub query: BTreeMap<String, Vec<String>>,
    pub headers: BTreeMap<String, Vec<String>>,
    pub body: Option<MitmHookBodyConfig>,
}

/// 命中钩子后对请求执行的动作（配置形态）：先删除 `strip_request_headers`
/// 列出的头，再注入 `inject_request_headers`。两者配合可实现「替换鉴权头」。
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct MitmHookActionsConfig {
    pub strip_request_headers: Vec<String>,
    pub inject_request_headers: Vec<InjectedHeaderConfig>,
}

/// 待注入头的配置：头名 `name`，值来自密钥源——`secret_env_var`（环境变量）
/// 或 `secret_file`（绝对路径文件），二者必须恰好提供其一；可选 `prefix`
/// 拼在密钥前（如 `Bearer `）。密钥在编译期解析，避免运行时反复读取。
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct InjectedHeaderConfig {
    pub name: String,
    pub secret_env_var: Option<String>,
    pub secret_file: Option<String>,
    pub prefix: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct MitmHookBodyConfig(pub serde_json::Value);

/// 编译后的运行态钩子（对应 `MitmHookConfig`）：字符串均已解析为可直接比较
/// 的匹配器/已解析头/已读取密钥，匹配期零额外解析开销。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MitmHook {
    pub host: String,
    pub matcher: MitmHookMatcher,
    pub actions: MitmHookActions,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MitmHookMatcher {
    pub methods: Vec<String>,
    pub path_prefixes: Vec<PathMatcher>,
    pub query: Vec<QueryConstraint>,
    pub headers: Vec<HeaderConstraint>,
    pub body: Option<MitmHookBodyMatcher>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryConstraint {
    pub name: String,
    pub allowed_values: Vec<ValueMatcher>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderConstraint {
    pub name: HeaderName,
    pub allowed_values: Vec<ValueMatcher>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MitmHookActions {
    pub strip_request_headers: Vec<HeaderName>,
    pub inject_request_headers: Vec<ResolvedInjectedHeader>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedInjectedHeader {
    pub name: HeaderName,
    pub value: HeaderValue,
    pub source: SecretSource,
}

/// 已注入头的密钥来源标记（仅记录来源，值已解析进 `HeaderValue`）。
/// 用于审计/诊断时说明该头的密钥取自环境变量还是哪个文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretSource {
    EnvVar(String),
    File(AbsolutePathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MitmHookBodyMatcher {
    pub raw: serde_json::Value,
}

/// 路径匹配器：字面量按「前缀」匹配（`starts_with`），glob 则整段通配。
/// 路径 glob 编译时启用 `literal_separator`，使 `*` 不跨越 `/` 段边界。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathMatcher {
    Prefix(String),
    Glob(CompiledGlobMatcher),
}

/// 值匹配器（用于 query 值与 header 值）：字面量按「全等」匹配，glob 则通配。
/// 与路径不同，值 glob 不启用 `literal_separator`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueMatcher {
    Exact(String),
    Glob(CompiledGlobMatcher),
}

// `parse_matcher_pattern` 的中间产物：把带前缀的配置串归类为字面量或 glob，
// 供 `compile_path_matchers`/`compile_value_matchers` 决定编成哪种匹配器。
enum MatcherPattern<'a> {
    Literal(&'a str),
    Glob(&'a str),
}

/// 已编译的 glob 匹配器：保留原始 `pattern` 字符串仅为 Debug/相等比较，
/// 实际匹配走预编译的 `matcher`。`GlobMatcher` 本身不实现 Debug/PartialEq/Eq，
/// 故下方手动实现——相等性按 `pattern` 判定（同一字符串编出的匹配器等价）。
#[derive(Clone)]
pub struct CompiledGlobMatcher {
    pattern: String,
    matcher: GlobMatcher,
}

impl std::fmt::Debug for CompiledGlobMatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompiledGlobMatcher")
            .field("pattern", &self.pattern)
            .finish()
    }
}

impl PartialEq for CompiledGlobMatcher {
    fn eq(&self, other: &Self) -> bool {
        self.pattern == other.pattern
    }
}

impl Eq for CompiledGlobMatcher {}

impl CompiledGlobMatcher {
    fn is_match(&self, candidate: &str) -> bool {
        self.matcher.is_match(candidate)
    }
}

/// 编译后的钩子索引：按归一化 host 分组，便于求值时 O(log n) 定位该 host
/// 的钩子列表，再在列表内按配置顺序逐条尝试匹配。
pub type MitmHooksByHost = BTreeMap<String, Vec<MitmHook>>;

/// 对单个内层请求求值的三态结果，调用方据此决定后续行为：
///   - `NoHooksForHost`：该 host 没配钩子（不影响放行，按普通策略走）；
///   - `Matched`：命中某条钩子，返回要执行的动作（删/注入头）；
///   - `HookedHostNoMatch`：该 host 配了钩子但本次请求没匹配上。
/// 区分后两者很关键：「配了但没匹配」可能意味着请求落在受控 host 的非预期
/// 路径上，调用方可据此做更严格处置（见 `mitm.rs`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookEvaluation {
    NoHooksForHost,
    Matched { actions: MitmHookActions },
    HookedHostNoMatch,
}

/// 校验 MITM 钩子配置的合法性（不产出运行态，仅做语义检查并给出带路径的
/// 错误）。在编译前调用，确保后续 `compile_*` 不会遇到非法输入。
///
/// 关键约束：钩子存在则必须开启 `network.mitm`（否则无法解密何谈改写）；
/// host 不可为空且不可含通配；methods/path_prefixes 不可为空；`body` 匹配器
/// 暂为保留特性直接报错。错误均用 `with_context` 标注是第几条钩子的哪个字段。
pub(crate) fn validate_mitm_hook_config(config: &NetworkProxyConfig) -> Result<()> {
    let hooks = &config.network.mitm_hooks;
    if hooks.is_empty() {
        return Ok(());
    }

    // 前置约束：要改写内层请求必先能解密，故钩子依赖 `network.mitm` 开启。
    if !config.network.mitm {
        return Err(anyhow!("network.mitm_hooks requires network.mitm = true"));
    }

    for (hook_index, hook) in hooks.iter().enumerate() {
        let host = normalize_hook_host(&hook.host)
            .with_context(|| format!("invalid network.mitm_hooks[{hook_index}].host"))?;

        let methods = normalize_methods(&hook.matcher.methods)
            .with_context(|| format!("invalid network.mitm_hooks[{hook_index}].match.methods"))?;
        if methods.is_empty() {
            return Err(anyhow!(
                "network.mitm_hooks[{hook_index}].match.methods must not be empty"
            ));
        }

        let path_prefixes =
            compile_path_matchers(&hook.matcher.path_prefixes).with_context(|| {
                format!("invalid network.mitm_hooks[{hook_index}].match.path_prefixes")
            })?;
        if path_prefixes.is_empty() {
            return Err(anyhow!(
                "network.mitm_hooks[{hook_index}].match.path_prefixes must not be empty"
            ));
        }

        if let Some(body) = hook.matcher.body.as_ref() {
            let _ = body;
            return Err(anyhow!(
                "network.mitm_hooks[{hook_index}].match.body is reserved for a future release and is not yet supported"
            ));
        }

        validate_query_constraints(&hook.matcher.query)
            .with_context(|| format!("invalid network.mitm_hooks[{hook_index}].match.query"))?;
        validate_header_constraints(&hook.matcher.headers)
            .with_context(|| format!("invalid network.mitm_hooks[{hook_index}].match.headers"))?;
        validate_strip_request_headers(&hook.actions.strip_request_headers).with_context(|| {
            format!("invalid network.mitm_hooks[{hook_index}].actions.strip_request_headers")
        })?;
        validate_injected_headers(&hook.actions.inject_request_headers).with_context(|| {
            format!("invalid network.mitm_hooks[{hook_index}].actions.inject_request_headers")
        })?;

        if host.is_empty() {
            return Err(anyhow!(
                "network.mitm_hooks[{hook_index}].host must not be empty"
            ));
        }
    }

    Ok(())
}

/// 把配置编译为运行态钩子索引，使用「真实」密钥解析器（进程环境变量 +
/// 读文件）。测试走 `compile_mitm_hooks_with_resolvers` 注入桩解析器。
pub(crate) fn compile_mitm_hooks(config: &NetworkProxyConfig) -> Result<MitmHooksByHost> {
    compile_mitm_hooks_with_resolvers(
        config,
        |name| env::var(name).ok(),
        |path| {
            let value = fs::read_to_string(path.as_path()).with_context(|| {
                format!("failed to read secret file {}", path.as_path().display())
            })?;
            Ok(value.trim().to_string())
        },
    )
}

/// 对已解密的内层请求求值：先按归一化 host 定位钩子列表，再按配置顺序返回
/// 「首个」匹配钩子的动作（短路）。host 无钩子 → `NoHooksForHost`；有钩子但
/// 全不匹配 → `HookedHostNoMatch`。求值是只读纯比较，无副作用。
pub(crate) fn evaluate_mitm_hooks(
    hooks_by_host: &MitmHooksByHost,
    host: &str,
    req: &Request,
) -> HookEvaluation {
    let normalized_host = normalize_host(host);
    let Some(hooks) = hooks_by_host.get(&normalized_host) else {
        return HookEvaluation::NoHooksForHost;
    };

    // 按配置顺序取首个命中者：配置中靠前的钩子优先级更高（见 evaluate 测试）。
    for hook in hooks {
        if hook_matches(hook, req) {
            return HookEvaluation::Matched {
                actions: hook.actions.clone(),
            };
        }
    }

    HookEvaluation::HookedHostNoMatch
}

/// 编译主流程（参数化密钥解析器，便于测试注入）：先 `validate` 兜底校验，
/// 再把每条钩子的字符串字段逐一编译——host 归一化、methods 大写、path/query/
/// header 匹配器编译、strip/inject 头解析（注入头在此读取密钥并拼成 HeaderValue）
/// ——按 host 聚合进 `MitmHooksByHost`。任一字段非法即整体失败。
///
/// `resolve_env_var` / `read_secret_file` 是密钥解析钩子：生产用真实环境/文件，
/// 测试传桩函数以避免触碰真实环境。
fn compile_mitm_hooks_with_resolvers<EnvFn, FileFn>(
    config: &NetworkProxyConfig,
    resolve_env_var: EnvFn,
    read_secret_file: FileFn,
) -> Result<MitmHooksByHost>
where
    EnvFn: Fn(&str) -> Option<String>,
    FileFn: Fn(&AbsolutePathBuf) -> Result<String>,
{
    validate_mitm_hook_config(config)?;

    let mut hooks_by_host = MitmHooksByHost::new();
    for hook in &config.network.mitm_hooks {
        let host = normalize_hook_host(&hook.host)?;
        let methods = normalize_methods(&hook.matcher.methods)?;
        let path_prefixes = compile_path_matchers(&hook.matcher.path_prefixes)?;
        let query = hook
            .matcher
            .query
            .iter()
            .map(|(name, values)| {
                Ok(QueryConstraint {
                    name: normalize_query_name(name)?,
                    allowed_values: compile_value_matchers(values)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let headers = hook
            .matcher
            .headers
            .iter()
            .map(|(name, values)| {
                Ok(HeaderConstraint {
                    name: parse_header_name(name)?,
                    allowed_values: compile_value_matchers(values)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let strip_request_headers = hook
            .actions
            .strip_request_headers
            .iter()
            .map(|name| parse_header_name(name))
            .collect::<Result<Vec<_>>>()?;
        let inject_request_headers = hook
            .actions
            .inject_request_headers
            .iter()
            .map(|header| {
                compile_injected_header(header, &resolve_env_var, &read_secret_file)
                    .with_context(|| format!("failed to compile injected header {}", header.name))
            })
            .collect::<Result<Vec<_>>>()?;

        hooks_by_host
            .entry(host.clone())
            .or_default()
            .push(MitmHook {
                host,
                matcher: MitmHookMatcher {
                    methods,
                    path_prefixes,
                    query,
                    headers,
                    body: None,
                },
                actions: MitmHookActions {
                    strip_request_headers,
                    inject_request_headers,
                },
            });
    }

    Ok(hooks_by_host)
}

/// 把一个待注入头配置编译为最终的 `ResolvedInjectedHeader`：解析头名、按
/// 「环境变量 XOR 文件」二选一读出密钥、拼上可选前缀生成 `HeaderValue`。
/// 缺失环境变量或两个/零个密钥源都视为配置错误。密钥在此一次性读出，运行期
/// 不再触碰环境/磁盘。
fn compile_injected_header<EnvFn, FileFn>(
    header: &InjectedHeaderConfig,
    resolve_env_var: &EnvFn,
    read_secret_file: &FileFn,
) -> Result<ResolvedInjectedHeader>
where
    EnvFn: Fn(&str) -> Option<String>,
    FileFn: Fn(&AbsolutePathBuf) -> Result<String>,
{
    let name = parse_header_name(&header.name)?;
    // 恰好提供 env 或 file 其一才合法；(Some,Some)/(None,None) 都落入 `_` 报错。
    let (secret, source) = match (
        header.secret_env_var.as_deref(),
        header.secret_file.as_deref(),
    ) {
        (Some(env_var), None) => {
            let value = resolve_env_var(env_var)
                .ok_or_else(|| anyhow!("missing required environment variable {env_var}"))?;
            (value, SecretSource::EnvVar(env_var.to_string()))
        }
        (None, Some(secret_file)) => {
            let path = parse_secret_file(secret_file)?;
            let value = read_secret_file(&path)?;
            (value, SecretSource::File(path))
        }
        _ => {
            return Err(anyhow!(
                "expected exactly one of secret_env_var or secret_file"
            ));
        }
    };

    let prefix = header.prefix.clone().unwrap_or_default();
    let value = HeaderValue::from_str(&format!("{prefix}{secret}"))
        .with_context(|| format!("invalid value for injected header {}", header.name))?;

    Ok(ResolvedInjectedHeader {
        name,
        value,
        source,
    })
}

/// 判断单条钩子是否匹配该请求：method / path / query / header 四项约束按 AND
/// 逐项短路求值，任一不满足即返回 false。各项内部语义见对应 `*_matches`。
fn hook_matches(hook: &MitmHook, req: &Request) -> bool {
    // method 大小写不敏感：请求方法统一大写后与（编译期已大写的）允许集比较。
    let method = req.method().as_str().to_ascii_uppercase();
    if !hook
        .matcher
        .methods
        .iter()
        .any(|allowed| allowed == &method)
    {
        return false;
    }

    let path = req.uri().path();
    if !path_matches(&hook.matcher.path_prefixes, path) {
        return false;
    }

    if !query_matches(&hook.matcher.query, req) {
        return false;
    }

    headers_match(&hook.matcher.headers, req)
}

/// query 约束匹配：每个约束要求请求 query 中存在同名参数，且其任一实际值命中
/// 该约束的任一允许值（约束间 AND、值间 OR）。无约束视为通过。
fn query_matches(query_constraints: &[QueryConstraint], req: &Request) -> bool {
    // 无约束 = 不限制 query，直接通过。
    if query_constraints.is_empty() {
        return true;
    }

    let actual_query = req.uri().query().unwrap_or_default();
    let mut actual_values: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, value) in form_urlencoded::parse(actual_query.as_bytes()) {
        actual_values
            .entry(name.into_owned())
            .or_default()
            .push(value.into_owned());
    }

    query_constraints.iter().all(|constraint| {
        actual_values.get(&constraint.name).is_some_and(|actual| {
            actual.iter().any(|candidate| {
                constraint
                    .allowed_values
                    .iter()
                    .any(|allowed| allowed.matches(candidate))
            })
        })
    })
}

/// header 约束匹配：每个约束要求请求带有该头；若约束未列允许值，则只校验
/// 「存在」即可；否则要求某个实际值命中任一允许值。约束间 AND。
fn headers_match(header_constraints: &[HeaderConstraint], req: &Request) -> bool {
    header_constraints.iter().all(|constraint| {
        let actual = req.headers().get_all(&constraint.name);
        // 该头缺失即不匹配（约束隐含「必须存在」）。
        if actual.iter().next().is_none() {
            return false;
        }
        // 未指定允许值 = 只要求头存在，不校验具体值。
        if constraint.allowed_values.is_empty() {
            return true;
        }

        actual.iter().any(|value| {
            value.to_str().ok().is_some_and(|candidate| {
                constraint
                    .allowed_values
                    .iter()
                    .any(|allowed| allowed.matches(candidate))
            })
        })
    })
}

fn path_matches(path_prefixes: &[PathMatcher], path: &str) -> bool {
    path_prefixes.iter().any(|matcher| matcher.matches(path))
}

impl PathMatcher {
    fn matches(&self, candidate: &str) -> bool {
        match self {
            Self::Prefix(prefix) => candidate.starts_with(prefix),
            Self::Glob(glob) => glob.is_match(candidate),
        }
    }
}

impl ValueMatcher {
    fn matches(&self, candidate: &str) -> bool {
        match self {
            Self::Exact(value) => value == candidate,
            Self::Glob(glob) => glob.is_match(candidate),
        }
    }
}

fn compile_path_matchers(path_prefixes: &[String]) -> Result<Vec<PathMatcher>> {
    path_prefixes
        .iter()
        .map(|prefix| {
            match parse_matcher_pattern(prefix)? {
                MatcherPattern::Literal(prefix) => {
                    if prefix.is_empty() {
                        return Err(anyhow!("path_prefixes must not contain empty entries"));
                    }
                    Ok(PathMatcher::Prefix(prefix.to_string()))
                }
                MatcherPattern::Glob(glob_pattern) => Ok(PathMatcher::Glob(compile_glob_matcher(
                    glob_pattern,
                    /*literal_separator*/ true,
                )?)),
            }
        })
        .collect()
}

fn compile_value_matchers(values: &[String]) -> Result<Vec<ValueMatcher>> {
    values
        .iter()
        .map(|value| match parse_matcher_pattern(value)? {
            MatcherPattern::Literal(value) => Ok(ValueMatcher::Exact(value.to_string())),
            MatcherPattern::Glob(glob_pattern) => Ok(ValueMatcher::Glob(compile_glob_matcher(
                glob_pattern,
                /*literal_separator*/ false,
            )?)),
        })
        .collect()
}

/// 解析匹配串的前缀语义，决定按字面量还是 glob 处理：
///   - `literal:xxx` → 字面量 `xxx`（即使含 `*`/`[` 也当普通字符，用于值里
///     本就含保留前缀的场景）；
///   - `pattern:xxx` → glob `xxx`（不可为空）；
///   - 无前缀 → 默认字面量（安全默认，避免配置里的元字符被误当通配）。
fn parse_matcher_pattern(pattern: &str) -> Result<MatcherPattern<'_>> {
    if let Some(literal) = pattern.strip_prefix(LITERAL_PREFIX) {
        return Ok(MatcherPattern::Literal(literal));
    }
    let Some(glob_pattern) = pattern.strip_prefix(PATTERN_PREFIX) else {
        return Ok(MatcherPattern::Literal(pattern));
    };
    if glob_pattern.is_empty() {
        return Err(anyhow!("glob pattern must not be empty"));
    }
    Ok(MatcherPattern::Glob(glob_pattern))
}

/// 编译一个 glob 为 `CompiledGlobMatcher`。`literal_separator=true` 时 `*` 不跨
/// `/` 段（路径用，防 `/repos/*/codex` 越段匹配）；`false` 时 `*` 可跨任意字符
/// （query/header 值用）。开启 `backslash_escape` 以支持 `\` 转义元字符。
fn compile_glob_matcher(pattern: &str, literal_separator: bool) -> Result<CompiledGlobMatcher> {
    let mut builder = GlobBuilder::new(pattern);
    builder
        .backslash_escape(true)
        .literal_separator(literal_separator);
    builder
        .build()
        .map(|glob| CompiledGlobMatcher {
            pattern: pattern.to_string(),
            matcher: glob.compile_matcher(),
        })
        .map_err(|err| anyhow!("invalid glob pattern {pattern:?}: {err}"))
}

/// 归一化并校验钩子 host：必须非空且不含通配。钩子会注入密钥到匹配请求，
/// 故刻意要求 host 精确匹配——通配 host 可能把密钥误注入到非预期目标，是
/// 安全考量而非功能限制。
fn normalize_hook_host(host: &str) -> Result<String> {
    let normalized = normalize_host(host);
    if normalized.is_empty() {
        return Err(anyhow!("host must not be empty"));
    }
    if normalized.contains('*') {
        return Err(anyhow!(
            "MITM hook hosts must be exact hosts and cannot contain wildcards"
        ));
    }
    Ok(normalized)
}

fn normalize_methods(methods: &[String]) -> Result<Vec<String>> {
    methods
        .iter()
        .map(|method| {
            let normalized = method.trim().to_ascii_uppercase();
            if normalized.is_empty() {
                return Err(anyhow!("methods must not contain empty entries"));
            }
            Ok(normalized)
        })
        .collect()
}

fn validate_query_constraints(query: &BTreeMap<String, Vec<String>>) -> Result<()> {
    for (name, values) in query {
        let normalized = normalize_query_name(name)?;
        if normalized.is_empty() {
            return Err(anyhow!("query keys must not be empty"));
        }
        if values.is_empty() {
            return Err(anyhow!(
                "query key {name:?} must list at least one allowed value"
            ));
        }
        let _ = compile_value_matchers(values)
            .with_context(|| format!("invalid matcher for query key {name:?}"))?;
    }
    Ok(())
}

fn normalize_query_name(name: &str) -> Result<String> {
    if name.is_empty() {
        return Err(anyhow!("query keys must not be empty"));
    }
    Ok(name.to_string())
}

fn validate_header_constraints(headers: &BTreeMap<String, Vec<String>>) -> Result<()> {
    for (name, values) in headers {
        let _ = parse_header_name(name)?;
        let _ = compile_value_matchers(values)
            .with_context(|| format!("invalid matcher for header {name:?}"))?;
    }
    Ok(())
}

fn validate_strip_request_headers(header_names: &[String]) -> Result<()> {
    for name in header_names {
        let _ = parse_header_name(name)?;
    }
    Ok(())
}

fn validate_injected_headers(headers: &[InjectedHeaderConfig]) -> Result<()> {
    for header in headers {
        let _ = parse_header_name(&header.name)?;
        match (
            header.secret_env_var.as_deref(),
            header.secret_file.as_deref(),
        ) {
            (Some(secret_env_var), None) => {
                if secret_env_var.trim().is_empty() {
                    return Err(anyhow!("secret_env_var must not be empty"));
                }
            }
            (None, Some(secret_file)) => {
                let _ = parse_secret_file(secret_file)?;
            }
            _ => {
                return Err(anyhow!(
                    "expected exactly one of secret_env_var or secret_file"
                ));
            }
        }
    }
    Ok(())
}

fn parse_header_name(name: &str) -> Result<HeaderName> {
    HeaderName::from_bytes(name.as_bytes())
        .map_err(|err| anyhow!("invalid header name {name:?}: {err}"))
}

/// 校验并包装密钥文件路径。强制绝对路径：密钥文件解析时机/工作目录不确定，
/// 相对路径含义模糊且易被误解析到错误位置，故一律拒绝。
fn parse_secret_file(path: &str) -> Result<AbsolutePathBuf> {
    if path.trim().is_empty() {
        return Err(anyhow!("secret_file must not be empty"));
    }
    let path = Path::new(path);
    if !path.is_absolute() {
        return Err(anyhow!("secret_file must be an absolute path: {path:?}"));
    }
    AbsolutePathBuf::from_absolute_path(path)
        .with_context(|| format!("secret_file must be an absolute path: {path:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NetworkMode;
    use crate::config::NetworkProxySettings;
    use pretty_assertions::assert_eq;
    use rama_http::Body;
    use rama_http::Method;
    use tempfile::NamedTempFile;

    fn base_config() -> NetworkProxyConfig {
        NetworkProxyConfig {
            network: NetworkProxySettings {
                mitm: true,
                mode: NetworkMode::Limited,
                ..NetworkProxySettings::default()
            },
        }
    }

    fn github_hook() -> MitmHookConfig {
        MitmHookConfig {
            host: "api.github.com".to_string(),
            matcher: MitmHookMatchConfig {
                methods: vec!["POST".to_string(), "PUT".to_string()],
                path_prefixes: vec!["/repos/openai/".to_string()],
                ..MitmHookMatchConfig::default()
            },
            actions: MitmHookActionsConfig {
                strip_request_headers: vec!["authorization".to_string()],
                inject_request_headers: vec![InjectedHeaderConfig {
                    name: "authorization".to_string(),
                    secret_env_var: Some("CODEX_GITHUB_TOKEN".to_string()),
                    secret_file: None,
                    prefix: Some("Bearer ".to_string()),
                }],
            },
        }
    }

    #[test]
    fn validate_requires_mitm_for_hooks() {
        let mut config = base_config();
        config.network.mitm = false;
        config.network.mitm_hooks = vec![github_hook()];

        let err = validate_mitm_hook_config(&config).expect_err("hooks require mitm");
        assert!(
            err.to_string()
                .contains("network.mitm_hooks requires network.mitm = true")
        );
    }

    #[test]
    fn validate_allows_hooks_in_full_mode() {
        let mut config = base_config();
        config.network.mode = NetworkMode::Full;
        config.network.mitm_hooks = vec![github_hook()];

        validate_mitm_hook_config(&config).expect("hooks should be allowed in full mode");
    }

    #[test]
    fn validate_rejects_body_matchers_for_now() {
        let mut config = base_config();
        let mut hook = github_hook();
        hook.matcher.body = Some(MitmHookBodyConfig(serde_json::json!({
            "repository": "openai/codex"
        })));
        config.network.mitm_hooks = vec![hook];

        let err = validate_mitm_hook_config(&config).expect_err("body matchers are reserved");
        assert!(err.to_string().contains("match.body is reserved"));
    }

    #[test]
    fn validate_rejects_relative_secret_file() {
        let mut config = base_config();
        let mut hook = github_hook();
        hook.actions.inject_request_headers[0].secret_env_var = None;
        hook.actions.inject_request_headers[0].secret_file = Some("token.txt".to_string());
        config.network.mitm_hooks = vec![hook];

        let err = validate_mitm_hook_config(&config).expect_err("secret file must be absolute");
        assert!(format!("{err:#}").contains("secret_file must be an absolute path"));
    }

    #[test]
    fn validate_rejects_dual_secret_sources() {
        let mut config = base_config();
        let mut hook = github_hook();
        hook.actions.inject_request_headers[0].secret_file = Some("/tmp/github-token".to_string());
        config.network.mitm_hooks = vec![hook];

        let err = validate_mitm_hook_config(&config).expect_err("dual secret sources invalid");
        assert!(format!("{err:#}").contains("exactly one of secret_env_var or secret_file"));
    }

    #[test]
    fn compile_resolves_env_backed_injected_headers() {
        let mut config = base_config();
        config.network.mitm_hooks = vec![github_hook()];

        let hooks = compile_mitm_hooks_with_resolvers(
            &config,
            |name| (name == "CODEX_GITHUB_TOKEN").then(|| "ghp-secret".to_string()),
            |_| Err(anyhow!("unexpected file lookup")),
        )
        .unwrap();

        let compiled = hooks.get("api.github.com").unwrap();
        assert_eq!(compiled.len(), 1);
        assert_eq!(
            compiled[0].actions.inject_request_headers[0].source,
            SecretSource::EnvVar("CODEX_GITHUB_TOKEN".to_string())
        );
        assert_eq!(
            compiled[0].actions.inject_request_headers[0].value,
            HeaderValue::from_static("Bearer ghp-secret")
        );
    }

    #[test]
    fn compile_resolves_file_backed_injected_headers() {
        let secret_file = NamedTempFile::new().unwrap();
        std::fs::write(secret_file.path(), "ghp-file-secret\n").unwrap();

        let mut config = base_config();
        let mut hook = github_hook();
        hook.actions.inject_request_headers[0].secret_env_var = None;
        hook.actions.inject_request_headers[0].secret_file =
            Some(secret_file.path().display().to_string());
        config.network.mitm_hooks = vec![hook];

        let hooks = compile_mitm_hooks(&config).unwrap();
        let compiled = hooks.get("api.github.com").unwrap();
        assert_eq!(
            compiled[0].actions.inject_request_headers[0].value,
            HeaderValue::from_static("Bearer ghp-file-secret")
        );
    }

    #[test]
    fn evaluate_returns_first_matching_hook() {
        let mut config = base_config();
        let mut first = github_hook();
        first.matcher.path_prefixes = vec!["/repos/openai/".to_string()];
        let mut second = github_hook();
        second.actions.inject_request_headers[0].prefix = Some("Token ".to_string());
        config.network.mitm_hooks = vec![first, second];

        let hooks = compile_mitm_hooks_with_resolvers(
            &config,
            |_| Some("abc".to_string()),
            |_| Err(anyhow!("unexpected file lookup")),
        )
        .unwrap();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/repos/openai/codex/issues")
            .header("x-trace", "1")
            .body(Body::empty())
            .unwrap();

        let evaluation = evaluate_mitm_hooks(&hooks, "api.github.com", &req);
        let HookEvaluation::Matched { actions } = evaluation else {
            panic!("expected a matching hook");
        };

        assert_eq!(
            actions.inject_request_headers[0].value,
            HeaderValue::from_static("Bearer abc")
        );
    }

    #[test]
    fn evaluate_matches_query_and_header_constraints() {
        let mut config = base_config();
        let mut hook = github_hook();
        hook.matcher.query = BTreeMap::from([(
            "state".to_string(),
            vec!["open".to_string(), "triage".to_string()],
        )]);
        hook.matcher.headers = BTreeMap::from([(
            "x-github-api-version".to_string(),
            vec!["2022-11-28".to_string()],
        )]);
        config.network.mitm_hooks = vec![hook];

        let hooks = compile_mitm_hooks_with_resolvers(
            &config,
            |_| Some("abc".to_string()),
            |_| Err(anyhow!("unexpected file lookup")),
        )
        .unwrap();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/repos/openai/codex/issues?state=open&per_page=10")
            .header("x-github-api-version", "2022-11-28")
            .body(Body::empty())
            .unwrap();

        assert_eq!(
            evaluate_mitm_hooks(&hooks, "api.github.com", &req),
            HookEvaluation::Matched {
                actions: hooks.get("api.github.com").unwrap()[0].actions.clone(),
            }
        );
    }

    #[test]
    fn evaluate_matches_wildcard_path_query_and_header_constraints() {
        let mut config = base_config();
        let mut hook = github_hook();
        hook.matcher.path_prefixes = vec!["pattern:/repos/*/codex/issues*".to_string()];
        hook.matcher.query =
            BTreeMap::from([("state".to_string(), vec!["pattern:op*".to_string()])]);
        hook.matcher.headers = BTreeMap::from([(
            "x-github-api-version".to_string(),
            vec!["pattern:2022*preview".to_string()],
        )]);
        config.network.mitm_hooks = vec![hook];

        let hooks = compile_mitm_hooks_with_resolvers(
            &config,
            |_| Some("abc".to_string()),
            |_| Err(anyhow!("unexpected file lookup")),
        )
        .unwrap();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/repos/openai/codex/issues?state=open")
            .header("x-github-api-version", "2022-11-28-preview")
            .body(Body::empty())
            .unwrap();

        assert_eq!(
            evaluate_mitm_hooks(&hooks, "api.github.com", &req),
            HookEvaluation::Matched {
                actions: hooks.get("api.github.com").unwrap()[0].actions.clone(),
            }
        );
    }

    #[test]
    fn validate_rejects_invalid_wildcard_path_pattern() {
        let mut config = base_config();
        let mut hook = github_hook();
        hook.matcher.path_prefixes = vec!["pattern:/repos/[".to_string()];
        config.network.mitm_hooks = vec![hook];

        let err = validate_mitm_hook_config(&config).expect_err("invalid glob should fail");
        assert!(format!("{err:#}").contains("invalid glob pattern"));
    }

    #[test]
    fn evaluate_path_wildcard_does_not_cross_segment_boundaries() {
        let mut config = base_config();
        let mut hook = github_hook();
        hook.matcher.path_prefixes = vec!["pattern:/repos/*/codex/issues*".to_string()];
        config.network.mitm_hooks = vec![hook];

        let hooks = compile_mitm_hooks_with_resolvers(
            &config,
            |_| Some("abc".to_string()),
            |_| Err(anyhow!("unexpected file lookup")),
        )
        .unwrap();
        let nested_req = Request::builder()
            .method(Method::POST)
            .uri("/repos/openai/private/codex/issues")
            .body(Body::empty())
            .unwrap();

        assert_eq!(
            evaluate_mitm_hooks(&hooks, "api.github.com", &nested_req),
            HookEvaluation::HookedHostNoMatch
        );
    }

    #[test]
    fn evaluate_treats_glob_metacharacters_as_literal_without_glob_prefix() {
        let mut config = base_config();
        let mut hook = github_hook();
        hook.matcher.path_prefixes = vec!["/repos/[draft]/".to_string()];
        hook.matcher.query = BTreeMap::from([("state".to_string(), vec!["op*".to_string()])]);
        hook.matcher.headers = BTreeMap::from([(
            "x-github-api-version".to_string(),
            vec!["2022-11-28[preview]".to_string()],
        )]);
        config.network.mitm_hooks = vec![hook];

        let hooks = compile_mitm_hooks_with_resolvers(
            &config,
            |_| Some("abc".to_string()),
            |_| Err(anyhow!("unexpected file lookup")),
        )
        .unwrap();
        let exact_req = Request::builder()
            .method(Method::POST)
            .uri("/repos/[draft]/codex/issues?state=op*")
            .header("x-github-api-version", "2022-11-28[preview]")
            .body(Body::empty())
            .unwrap();
        let non_literal_req = Request::builder()
            .method(Method::POST)
            .uri("/repos/draft/codex/issues?state=open")
            .header("x-github-api-version", "2022-11-28-preview")
            .body(Body::empty())
            .unwrap();

        assert_eq!(
            evaluate_mitm_hooks(&hooks, "api.github.com", &exact_req),
            HookEvaluation::Matched {
                actions: hooks.get("api.github.com").unwrap()[0].actions.clone(),
            }
        );
        assert_eq!(
            evaluate_mitm_hooks(&hooks, "api.github.com", &non_literal_req),
            HookEvaluation::HookedHostNoMatch
        );
    }

    #[test]
    fn evaluate_allows_literal_values_with_reserved_prefixes() {
        let mut config = base_config();
        let mut hook = github_hook();
        hook.matcher.query =
            BTreeMap::from([("state".to_string(), vec!["literal:pattern:*".to_string()])]);
        hook.matcher.headers = BTreeMap::from([(
            "x-github-api-version".to_string(),
            vec!["literal:pattern:*".to_string()],
        )]);
        config.network.mitm_hooks = vec![hook];

        let hooks = compile_mitm_hooks_with_resolvers(
            &config,
            |_| Some("abc".to_string()),
            |_| Err(anyhow!("unexpected file lookup")),
        )
        .unwrap();
        let exact_req = Request::builder()
            .method(Method::POST)
            .uri("/repos/openai/codex/issues?state=pattern%3A%2A")
            .header("x-github-api-version", "pattern:*")
            .body(Body::empty())
            .unwrap();
        let non_literal_req = Request::builder()
            .method(Method::POST)
            .uri("/repos/openai/codex/issues?state=pattern%3Aopen")
            .header("x-github-api-version", "pattern:preview")
            .body(Body::empty())
            .unwrap();

        assert_eq!(
            evaluate_mitm_hooks(&hooks, "api.github.com", &exact_req),
            HookEvaluation::Matched {
                actions: hooks.get("api.github.com").unwrap()[0].actions.clone(),
            }
        );
        assert_eq!(
            evaluate_mitm_hooks(&hooks, "api.github.com", &non_literal_req),
            HookEvaluation::HookedHostNoMatch
        );
    }

    #[test]
    fn evaluate_returns_hooked_host_no_match_when_query_constraint_fails() {
        let mut config = base_config();
        let mut hook = github_hook();
        hook.matcher.query = BTreeMap::from([("state".to_string(), vec!["open".to_string()])]);
        config.network.mitm_hooks = vec![hook];

        let hooks = compile_mitm_hooks_with_resolvers(
            &config,
            |_| Some("abc".to_string()),
            |_| Err(anyhow!("unexpected file lookup")),
        )
        .unwrap();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/repos/openai/codex/issues?state=closed")
            .body(Body::empty())
            .unwrap();

        assert_eq!(
            evaluate_mitm_hooks(&hooks, "api.github.com", &req),
            HookEvaluation::HookedHostNoMatch
        );
    }

    #[test]
    fn evaluate_returns_no_hooks_for_unconfigured_host() {
        let req = Request::builder()
            .method(Method::POST)
            .uri("/repos/openai/codex/issues")
            .body(Body::empty())
            .unwrap();

        assert_eq!(
            evaluate_mitm_hooks(&MitmHooksByHost::new(), "api.github.com", &req),
            HookEvaluation::NoHooksForHost
        );
    }
}
