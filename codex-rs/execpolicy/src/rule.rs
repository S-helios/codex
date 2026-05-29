//! 【文件职责】定义 execpolicy 规则系统的数据模型与匹配逻辑：命令前缀规则
//! （`PrefixRule` / `PrefixPattern` / `PatternToken`）、网络规则（`NetworkRule`）、
//! 匹配结果（`RuleMatch`），以及把规则抽象成对象的 `Rule` trait。
//!
//! 【架构位置】
//!   层级：执行策略（execpolicy）DSL · 规则模型核心
//!   上游：`parser.rs`（把 `.rules` 里的 `prefix_rule(...)` /
//!         `network_rule(...)` 调用构造成这里的类型）、`policy.rs`
//!         （持有规则集合并驱动匹配）
//!   下游：`decision.rs`（裁决三值枚举 `Decision`）
//!
//! 【核心概念：前缀匹配】execpolicy 不做正则、不做通配，而是「前缀匹配」——
//! 按命令的**第一个 token** 建索引，再逐位比对后续 token。例如规则
//! `prefix_rule(["git", "status"])` 命中 `git status`、`git status -s`，
//! 但不命中 `git commit`。第一个 token 必须是固定字符串（因为它是索引键），
//! 后续 token 才允许 `Alts` 多选一。
//!
//! 【阅读建议】先看 `PatternToken` / `PrefixPattern::matches_prefix`
//! （匹配算法本体），再看 `RuleMatch`（区分「显式规则命中」与「启发式兜底」，
//! 这个区分决定了上层能否自动追加规则），其余是网络规则的 host 规范化与
//! 示例校验工具。

use crate::decision::Decision;
use crate::error::Error;
use crate::error::Result;
use crate::policy::MatchOptions;
use crate::policy::Policy;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde::Deserialize;
use serde::Serialize;
use shlex::try_join;
use std::any::Any;
use std::fmt::Debug;
use std::sync::Arc;

/// Matches a single command token, either a fixed string or one of several allowed alternatives.
/// 匹配命令中的单个 token：要么是固定字符串（`Single`，必须精确相等），
/// 要么是一组候选（`Alts`，命中其中之一即可）。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PatternToken {
    Single(String),
    Alts(Vec<String>),
}

impl PatternToken {
    fn matches(&self, token: &str) -> bool {
        match self {
            Self::Single(expected) => expected == token,
            Self::Alts(alternatives) => alternatives.iter().any(|alt| alt == token),
        }
    }

    /// 把 token 统一看成「候选列表」返回：`Single` 视为单元素切片，
    /// `Alts` 直接返回其候选。便于 `parser.rs` 对首 token 的每个候选各生成
    /// 一条规则（首 token 是索引键，不能用一条规则承载多个候选）。
    pub fn alternatives(&self) -> &[String] {
        match self {
            Self::Single(expected) => std::slice::from_ref(expected),
            Self::Alts(alternatives) => alternatives,
        }
    }
}

/// Prefix matcher for commands with support for alternative match tokens.
/// First token is fixed since we key by the first token in policy.
/// 命令前缀匹配器，后续 token 支持「多选一」。
/// 首 token（`first`）固定为单字符串：因为 `Policy` 用首 token 作为
/// `MultiMap` 的索引键，索引键必须唯一确定，所以不能是 `Alts`。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrefixPattern {
    pub first: Arc<str>,
    pub rest: Arc<[PatternToken]>,
}

impl PrefixPattern {
    /// 判断命令 `cmd` 是否以本模式为前缀。
    ///
    /// @param cmd - 待匹配的完整命令 argv（如 `["git", "status", "-s"]`）
    /// @returns   - 命中则返回「被匹配走的那段前缀切片」（含首 token，
    ///              长度 = 模式长度），未命中返回 `None`
    ///
    /// 关键点：是**前缀**匹配而非整段相等——命令可以比模式长（多出来的尾部
    /// 参数不参与匹配），但不能比模式短。返回的前缀长度供上层比较「哪条规则
    /// 更具体」（前缀越长越具体，见 core 侧 `derive_*_reason`）。
    pub fn matches_prefix(&self, cmd: &[String]) -> Option<Vec<String>> {
        // 模式长度 = 首 token(1) + 后续模式 token 数。命令短于它直接不匹配。
        let pattern_length = self.rest.len() + 1;
        if cmd.len() < pattern_length || cmd[0] != self.first.as_ref() {
            return None;
        }

        // 逐位比对后续 token；任一位不匹配即整体失败。
        for (pattern_token, cmd_token) in self.rest.iter().zip(&cmd[1..pattern_length]) {
            if !pattern_token.matches(cmd_token) {
                return None;
            }
        }

        Some(cmd[..pattern_length].to_vec())
    }
}

/// 一次命令匹配的结果，区分两类来源——这个区分是整套审批逻辑的关键：
/// - `PrefixRuleMatch`：命中了用户/默认配置里**显式写下的**前缀规则。
/// - `HeuristicsRuleMatch`：没有任何显式规则命中，由上层启发式兜底产生的裁决。
///
/// 为什么要分？因为「能否自动给策略追加规则（amendment）」取决于此：显式规则
/// 优先级最高、不能被自动覆盖；只有启发式裁决才允许据此推导「下次自动放行」
/// 的新规则。core 侧用 `is_policy_match()` 判定是否为 `PrefixRuleMatch`，
/// 详见 `core/src/exec_policy.rs` 的 `try_derive_execpolicy_amendment_*`。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RuleMatch {
    PrefixRuleMatch {
        #[serde(rename = "matchedPrefix")]
        matched_prefix: Vec<String>,
        decision: Decision,
        // 命令首 token 是绝对路径、且经 host_executable 解析过时，记录解析到的
        // 真实可执行文件路径（如 `/usr/bin/git`）；否则为 None。
        #[serde(rename = "resolvedProgram", skip_serializing_if = "Option::is_none")]
        resolved_program: Option<AbsolutePathBuf>,
        /// Optional rationale for why this rule exists.
        ///
        /// This can be supplied for any decision and may be surfaced in different contexts
        /// (e.g., prompt reasons or rejection messages).
        #[serde(skip_serializing_if = "Option::is_none")]
        justification: Option<String>,
    },
    HeuristicsRuleMatch {
        command: Vec<String>,
        decision: Decision,
    },
}

impl RuleMatch {
    /// 取出本次匹配对应的裁决，屏蔽两种来源的差异。
    pub fn decision(&self) -> Decision {
        match self {
            Self::PrefixRuleMatch { decision, .. } => *decision,
            Self::HeuristicsRuleMatch { decision, .. } => *decision,
        }
    }

    /// 为「前缀规则命中」补上解析后的真实可执行文件路径，返回新值。
    /// 仅 `PrefixRuleMatch` 有意义；`HeuristicsRuleMatch` 原样返回。
    /// 在 `Policy::match_host_executable_rules` 里用：命令用绝对路径调用时，
    /// 先按 basename 匹配规则，再把解析到的路径塞回结果供展示/审计。
    pub fn with_resolved_program(self, resolved_program: &AbsolutePathBuf) -> Self {
        match self {
            Self::PrefixRuleMatch {
                matched_prefix,
                decision,
                justification,
                ..
            } => Self::PrefixRuleMatch {
                matched_prefix,
                decision,
                resolved_program: Some(resolved_program.clone()),
                justification,
            },
            other => other,
        }
    }
}

/// 一条前缀规则：模式 + 裁决 + 可选理由。
/// `justification` 会在审批/拒绝提示里回显给用户（如「该命令被禁止：请改用 X」），
/// 由 `.rules` 文件作者填写。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrefixRule {
    pub pattern: PrefixPattern,
    pub decision: Decision,
    pub justification: Option<String>,
}

/// 网络规则支持的协议类型。注意 `parse` 与 `as_policy_string` 不是严格互逆：
/// 解析时把若干别名（`https_connect` / `http-connect`）都归一到 `Https`，
/// 但回写时只输出规范名 `https`。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetworkRuleProtocol {
    Http,
    Https,
    Socks5Tcp,
    Socks5Udp,
}

impl NetworkRuleProtocol {
    /// 把 `.rules` 里的协议字符串解析为枚举；接受 https 的几个历史别名。
    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "http" => Ok(Self::Http),
            "https" | "https_connect" | "http-connect" => Ok(Self::Https),
            "socks5_tcp" => Ok(Self::Socks5Tcp),
            "socks5_udp" => Ok(Self::Socks5Udp),
            other => Err(Error::InvalidRule(format!(
                "network_rule protocol must be one of http, https, socks5_tcp, socks5_udp (got {other})"
            ))),
        }
    }

    /// 反向：把枚举写回 `.rules` 文件时用的规范字符串。
    /// 用于 `amend.rs` 追加 `network_rule(...)` 时序列化协议字段。
    pub fn as_policy_string(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
            Self::Socks5Tcp => "socks5_tcp",
            Self::Socks5Udp => "socks5_udp",
        }
    }
}

/// 一条网络规则：对某个 `host` 上的某种 `protocol` 流量做裁决。
/// 与前缀规则不同，它不参与命令匹配，而是被编译成「允许/拒绝域名清单」
/// 交给网络代理执行（见 `policy.rs::compiled_network_domains`）。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NetworkRule {
    pub host: String,
    pub protocol: NetworkRuleProtocol,
    pub decision: Decision,
    pub justification: Option<String>,
}

/// 规范化网络规则中的 host：去空白、剥掉端口、统一小写、校验非法形态。
///
/// @param raw - 用户写的 host 字符串（可能含端口、IPv6 方括号、尾点等）
/// @returns   - 规范化后的纯主机名/IP；非法时返回 `InvalidRule` 错误
///
/// 设计意图：网络规则只针对「具体某台主机」，所以这里**故意**拒绝带 scheme、
/// 带路径、带通配 `*` 的输入——避免作者误以为支持 URL 或泛域名匹配而留下
/// 安全漏洞。端口会被剥离（规则按主机粒度生效，不区分端口）。
pub(crate) fn normalize_network_rule_host(raw: &str) -> Result<String> {
    let mut host = raw.trim();
    if host.is_empty() {
        return Err(Error::InvalidRule(
            "network_rule host cannot be empty".to_string(),
        ));
    }
    if host.contains("://") || host.contains('/') || host.contains('?') || host.contains('#') {
        return Err(Error::InvalidRule(
            "network_rule host must be a hostname or IP literal (without scheme or path)"
                .to_string(),
        ));
    }

    // 分两种带端口形态分别剥离端口：
    // (1) 方括号包裹的 IPv6 字面量，如 `[::1]:8080` —— 取方括号内的部分。
    if let Some(stripped) = host.strip_prefix('[') {
        let Some((inside, rest)) = stripped.split_once(']') else {
            return Err(Error::InvalidRule(
                "network_rule host has an invalid bracketed IPv6 literal".to_string(),
            ));
        };
        let port_ok = rest
            .strip_prefix(':')
            .is_some_and(|port| !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()));
        if !rest.is_empty() && !port_ok {
            return Err(Error::InvalidRule(format!(
                "network_rule host contains an unsupported suffix: {raw}"
            )));
        }
        host = inside;
    // (2) 恰好一个冒号且冒号后全是数字，视为 `host:port`，剥掉端口。
    // 限定「恰好一个冒号」是为了不误伤未加方括号的裸 IPv6（含多个冒号）。
    } else if host.matches(':').count() == 1
        && let Some((candidate, port)) = host.rsplit_once(':')
        && !candidate.is_empty()
        && !port.is_empty()
        && port.chars().all(|c| c.is_ascii_digit())
    {
        host = candidate;
    }

    let normalized = host.trim_end_matches('.').trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return Err(Error::InvalidRule(
            "network_rule host cannot be empty".to_string(),
        ));
    }
    if normalized.contains('*') {
        return Err(Error::InvalidRule(
            "network_rule host must be a specific host; wildcards are not allowed".to_string(),
        ));
    }
    if normalized.chars().any(char::is_whitespace) {
        return Err(Error::InvalidRule(
            "network_rule host cannot contain whitespace".to_string(),
        ));
    }

    Ok(normalized)
}

/// 规则的对象抽象：`Policy` 以 `Arc<dyn Rule>` 持有规则，无需关心具体类型。
/// 目前唯一实现是 `PrefixRule`，留 trait 是为了未来可扩展其它匹配方式。
/// - `program()` 返回索引键（首 token），`Policy` 用它建 `MultiMap`。
/// - `matches()` 是匹配入口，命中返回 `RuleMatch`。
/// - `as_any()` 用于向下转型（如 `policy.rs` 里 downcast 回 `PrefixRule`
///   以读取其 `decision` / `pattern`）。
pub trait Rule: Any + Debug + Send + Sync {
    fn program(&self) -> &str;

    fn matches(&self, cmd: &[String]) -> Option<RuleMatch>;

    fn as_any(&self) -> &dyn Any;
}

/// 规则的共享引用别名。用 `Arc` 是因为同一规则可能被多处（不同程序索引、
/// 示例校验时的临时 `Policy`）共享持有。
pub type RuleRef = Arc<dyn Rule>;

impl Rule for PrefixRule {
    fn program(&self) -> &str {
        self.pattern.first.as_ref()
    }

    fn matches(&self, cmd: &[String]) -> Option<RuleMatch> {
        self.pattern
            .matches_prefix(cmd)
            .map(|matched_prefix| RuleMatch::PrefixRuleMatch {
                matched_prefix,
                decision: self.decision,
                resolved_program: None,
                justification: self.justification.clone(),
            })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Count how many rules match each provided example and error if any example is unmatched.
/// 校验每个「正例」（`r#match`）都至少命中一条规则，否则报错。
/// 设计意图：规则作者用正例自证「我这条规则确实能覆盖这些命令」，相当于给
/// DSL 写了内联单测；解析阶段就跑校验，避免写错的规则被静默加载。
pub(crate) fn validate_match_examples(
    policy: &Policy,
    rules: &[RuleRef],
    matches: &[Vec<String>],
) -> Result<()> {
    let mut unmatched_examples = Vec::new();
    let options = MatchOptions {
        resolve_host_executables: true,
    };

    for example in matches {
        if !policy
            .matches_for_command_with_options(example, /*heuristics_fallback*/ None, &options)
            .is_empty()
        {
            continue;
        }

        unmatched_examples.push(
            try_join(example.iter().map(String::as_str))
                .unwrap_or_else(|_| "unable to render example".to_string()),
        );
    }

    if unmatched_examples.is_empty() {
        Ok(())
    } else {
        Err(Error::ExampleDidNotMatch {
            rules: rules.iter().map(|rule| format!("{rule:?}")).collect(),
            examples: unmatched_examples,
            location: None,
        })
    }
}

/// Ensure that no rule matches any provided negative example.
/// 校验每个「反例」（`not_match`）都**不被任何规则命中**，否则报错。
/// 与正例相反，用于防止规则写得太宽：作者声明「这些命令不该被这条规则覆盖」，
/// 一旦误覆盖就在解析期暴露问题，而非等到线上误放/误拦。
pub(crate) fn validate_not_match_examples(
    policy: &Policy,
    _rules: &[RuleRef],
    not_matches: &[Vec<String>],
) -> Result<()> {
    let options = MatchOptions {
        resolve_host_executables: true,
    };

    for example in not_matches {
        if let Some(rule) = policy
            .matches_for_command_with_options(example, /*heuristics_fallback*/ None, &options)
            .first()
        {
            return Err(Error::ExampleDidMatch {
                rule: format!("{rule:?}"),
                example: try_join(example.iter().map(String::as_str))
                    .unwrap_or_else(|_| "unable to render example".to_string()),
                location: None,
            });
        }
    }

    Ok(())
}
