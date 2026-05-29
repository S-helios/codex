//! 【文件职责】定义 execpolicy 的核心容器 `Policy`（已编译的规则集合）及其
//! 匹配/裁决入口，外加裁决结果聚合类型 `Evaluation` 与匹配选项 `MatchOptions`。
//!
//! 【架构位置】
//!   层级：执行策略（execpolicy）DSL · 运行时判定核心
//!   上游：`parser.rs`（`PolicyParser::build` → `Policy::from_parts`）、
//!         `core/src/exec_policy.rs`（持有 `Policy`、对每条命令调
//!         `check_multiple_with_options`）
//!   下游：`rule.rs`（`Rule`/`RuleMatch`）、`decision.rs`（`Decision`）
//!
//! 【数据流】命令 argv → `matches_for_command_with_options`
//!   →（先查显式规则；未命中且允许时按宿主可执行文件解析重试；
//!     仍未命中则调启发式 `heuristics_fallback` 兜底）→ `Vec<RuleMatch>`
//!   → `Evaluation::from_matches`（取最严裁决）→ `Evaluation`
//!
//! 【两类规则各走各路】
//!   - 前缀规则（`rules_by_program`）：参与命令匹配，产出 `Decision`。
//!   - 网络规则（`network_rules`）：不参与命令匹配，由
//!     `compiled_network_domains` 编译成允许/拒绝域名清单交给网络代理。
//!
//! 【阅读建议】先看 `matches_for_command_with_options`（匹配主算法），
//! 再看 `Evaluation::from_matches`（多规则命中如何合成单一裁决——取最严），
//! `merge_overlay` / `compiled_network_domains` 是附加能力可后看。

use crate::decision::Decision;
use crate::error::Error;
use crate::error::Result;
use crate::executable_name::executable_path_lookup_key;
use crate::rule::NetworkRule;
use crate::rule::NetworkRuleProtocol;
use crate::rule::PatternToken;
use crate::rule::PrefixPattern;
use crate::rule::PrefixRule;
use crate::rule::RuleMatch;
use crate::rule::RuleRef;
use crate::rule::normalize_network_rule_host;
use codex_utils_absolute_path::AbsolutePathBuf;
use multimap::MultiMap;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;

/// 启发式兜底回调：显式规则全部未命中时，由调用方提供「这条命令该怎么裁决」
/// 的逻辑（实现在 `core/src/exec_policy.rs::render_decision_for_unmatched_command`）。
/// `Option` 为 `None` 时表示「不兜底」，未命中就返回空匹配列表。
type HeuristicsFallback<'a> = Option<&'a dyn Fn(&[String]) -> Decision>;

/// 匹配选项。`resolve_host_executables` 为 `true` 时：命令首 token 是绝对
/// 路径却没匹配到规则时，会按其 basename 再查一次规则，并校验该路径在
/// 对应 `host_executable` 声明的白名单内（详见 `match_host_executable_rules`）。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MatchOptions {
    pub resolve_host_executables: bool,
}

/// 已编译的执行策略：规则的内存表示，可直接对命令做匹配判定。
/// - `rules_by_program`：前缀规则，按首 token（程序名）建 `MultiMap` 索引，
///   同一程序可挂多条规则。
/// - `network_rules`：网络规则，按定义顺序保存（编译时后定义者覆盖先定义者）。
/// - `host_executables_by_name`：basename → 允许的绝对路径白名单，
///   把「裸命令名规则」安全地扩展到「按绝对路径调用」的场景。
#[derive(Clone, Debug)]
pub struct Policy {
    rules_by_program: MultiMap<String, RuleRef>,
    network_rules: Vec<NetworkRule>,
    host_executables_by_name: HashMap<String, Arc<[AbsolutePathBuf]>>,
}

impl Policy {
    pub fn new(rules_by_program: MultiMap<String, RuleRef>) -> Self {
        Self::from_parts(rules_by_program, Vec::new(), HashMap::new())
    }

    pub fn from_parts(
        rules_by_program: MultiMap<String, RuleRef>,
        network_rules: Vec<NetworkRule>,
        host_executables_by_name: HashMap<String, Arc<[AbsolutePathBuf]>>,
    ) -> Self {
        Self {
            rules_by_program,
            network_rules,
            host_executables_by_name,
        }
    }

    pub fn empty() -> Self {
        Self::new(MultiMap::new())
    }

    pub fn rules(&self) -> &MultiMap<String, RuleRef> {
        &self.rules_by_program
    }

    pub fn network_rules(&self) -> &[NetworkRule] {
        &self.network_rules
    }

    pub fn host_executables(&self) -> &HashMap<String, Arc<[AbsolutePathBuf]>> {
        &self.host_executables_by_name
    }

    /// 收集所有「允许（`Allow`）」前缀规则的可读前缀，去重排序后返回。
    /// 仅取 `PrefixRule` 且裁决为 `Allow` 者；`Alts` 多选 token 会被渲染成
    /// `[a|b]` 形式。供上层展示「当前已放行了哪些命令前缀」。
    pub fn get_allowed_prefixes(&self) -> Vec<Vec<String>> {
        let mut prefixes = Vec::new();

        for (_program, rules) in self.rules_by_program.iter_all() {
            for rule in rules {
                let Some(prefix_rule) = rule.as_any().downcast_ref::<PrefixRule>() else {
                    continue;
                };
                if prefix_rule.decision != Decision::Allow {
                    continue;
                }

                let mut prefix = Vec::with_capacity(prefix_rule.pattern.rest.len() + 1);
                prefix.push(prefix_rule.pattern.first.as_ref().to_string());
                prefix.extend(prefix_rule.pattern.rest.iter().map(render_pattern_token));
                prefixes.push(prefix);
            }
        }

        prefixes.sort();
        prefixes.dedup();
        prefixes
    }

    /// 就地往策略里追加一条前缀规则（运行时动态加规则的入口）。
    ///
    /// @param prefix   - 规则前缀 token 序列，不能为空（首 token 作索引键）
    /// @param decision - 该前缀的裁决
    /// @returns        - 前缀为空时返回 `InvalidPattern` 错误
    ///
    /// 副作用：修改 `self.rules_by_program`。
    /// 用途：用户在审批弹窗里点「以后总是允许」后，core 侧据此调用本方法
    /// 把新规则注入内存策略（见 `exec_policy.rs::append_amendment_and_update`）。
    /// 注意：这里构造的所有后续 token 都是 `Single`（精确匹配），不产生 `Alts`。
    pub fn add_prefix_rule(&mut self, prefix: &[String], decision: Decision) -> Result<()> {
        let (first_token, rest) = prefix
            .split_first()
            .ok_or_else(|| Error::InvalidPattern("prefix cannot be empty".to_string()))?;

        let rule: RuleRef = Arc::new(PrefixRule {
            pattern: PrefixPattern {
                first: Arc::from(first_token.as_str()),
                rest: rest
                    .iter()
                    .map(|token| PatternToken::Single(token.clone()))
                    .collect::<Vec<_>>()
                    .into(),
            },
            decision,
            justification: None,
        });

        self.rules_by_program.insert(first_token.clone(), rule);
        Ok(())
    }

    pub fn add_network_rule(
        &mut self,
        host: &str,
        protocol: NetworkRuleProtocol,
        decision: Decision,
        justification: Option<String>,
    ) -> Result<()> {
        let host = normalize_network_rule_host(host)?;
        if let Some(raw) = justification.as_deref()
            && raw.trim().is_empty()
        {
            return Err(Error::InvalidRule(
                "justification cannot be empty".to_string(),
            ));
        }
        self.network_rules.push(NetworkRule {
            host,
            protocol,
            decision,
            justification,
        });
        Ok(())
    }

    pub fn set_host_executable_paths(&mut self, name: String, paths: Vec<AbsolutePathBuf>) {
        self.host_executables_by_name.insert(name, paths.into());
    }

    /// 把 `overlay` 策略叠加到当前策略之上，返回合并后的新策略（不改原值）。
    /// 规则与网络规则都是「追加」（`overlay` 的规则补在后面），host 可执行白名单
    /// 则是「同名覆盖」。
    ///
    /// 设计背景：core 侧用它把「requirements 强制策略」叠加到从 `.rules` 文件
    /// 加载的用户策略上（见 `exec_policy.rs::load_exec_policy` 末尾）。因为
    /// `Evaluation` 取最严裁决，追加 overlay 只会让策略更严或持平，不会放松。
    pub fn merge_overlay(&self, overlay: &Policy) -> Policy {
        let mut combined_rules = self.rules_by_program.clone();
        for (program, rules) in overlay.rules_by_program.iter_all() {
            for rule in rules {
                combined_rules.insert(program.clone(), rule.clone());
            }
        }

        let mut combined_network_rules = self.network_rules.clone();
        combined_network_rules.extend(overlay.network_rules.iter().cloned());

        let mut host_executables_by_name = self.host_executables_by_name.clone();
        host_executables_by_name.extend(
            overlay
                .host_executables_by_name
                .iter()
                .map(|(name, paths)| (name.clone(), paths.clone())),
        );

        Policy::from_parts(
            combined_rules,
            combined_network_rules,
            host_executables_by_name,
        )
    }

    /// 把网络规则编译成 `(允许域名, 拒绝域名)` 两张清单，交给网络代理执行。
    ///
    /// @returns - `(allowed, denied)` 两个有序去重的 host 列表
    ///
    /// 关键语义：**按定义顺序后者覆盖前者**——同一 host 先 deny 再 allow，
    /// 最终落在 allowed（先从 denied 移除再加入 allowed），反之亦然。
    /// `Decision::Prompt` 对网络规则无意义（无法对一次连接「弹窗」），直接忽略。
    pub fn compiled_network_domains(&self) -> (Vec<String>, Vec<String>) {
        let mut allowed = Vec::new();
        let mut denied = Vec::new();

        for rule in &self.network_rules {
            match rule.decision {
                Decision::Allow => {
                    // 改判为允许：从拒绝清单剔除，再加入允许清单（保证互斥）。
                    denied.retain(|entry| entry != &rule.host);
                    upsert_domain(&mut allowed, &rule.host);
                }
                Decision::Forbidden => {
                    allowed.retain(|entry| entry != &rule.host);
                    upsert_domain(&mut denied, &rule.host);
                }
                Decision::Prompt => {}
            }
        }

        (allowed, denied)
    }

    pub fn check<F>(&self, cmd: &[String], heuristics_fallback: &F) -> Evaluation
    where
        F: Fn(&[String]) -> Decision,
    {
        let matched_rules = self.matches_for_command_with_options(
            cmd,
            Some(heuristics_fallback),
            &MatchOptions::default(),
        );
        Evaluation::from_matches(matched_rules)
    }

    pub fn check_with_options<F>(
        &self,
        cmd: &[String],
        heuristics_fallback: &F,
        options: &MatchOptions,
    ) -> Evaluation
    where
        F: Fn(&[String]) -> Decision,
    {
        let matched_rules =
            self.matches_for_command_with_options(cmd, Some(heuristics_fallback), options);
        Evaluation::from_matches(matched_rules)
    }

    /// Checks multiple commands and aggregates the results.
    /// 对多条命令分别匹配，再把所有 `RuleMatch` 汇总成一个 `Evaluation`。
    /// 用于 `bash -lc "a && b && c"` 这类被拆成多段的复合命令：任一段触发更严
    /// 裁决，整体就按更严的来（`Evaluation` 取最严）。
    pub fn check_multiple<Commands, F>(
        &self,
        commands: Commands,
        heuristics_fallback: &F,
    ) -> Evaluation
    where
        Commands: IntoIterator,
        Commands::Item: AsRef<[String]>,
        F: Fn(&[String]) -> Decision,
    {
        self.check_multiple_with_options(commands, heuristics_fallback, &MatchOptions::default())
    }

    pub fn check_multiple_with_options<Commands, F>(
        &self,
        commands: Commands,
        heuristics_fallback: &F,
        options: &MatchOptions,
    ) -> Evaluation
    where
        Commands: IntoIterator,
        Commands::Item: AsRef<[String]>,
        F: Fn(&[String]) -> Decision,
    {
        let matched_rules: Vec<RuleMatch> = commands
            .into_iter()
            .flat_map(|command| {
                self.matches_for_command_with_options(
                    command.as_ref(),
                    Some(heuristics_fallback),
                    options,
                )
            })
            .collect();

        Evaluation::from_matches(matched_rules)
    }

    /// Returns matching rules for the given command. If no rules match and
    /// `heuristics_fallback` is provided, returns a single
    /// `HeuristicsRuleMatch` with the decision rendered by
    /// `heuristics_fallback`.
    ///
    /// If `heuristics_fallback.is_some()`, then the returned vector is
    /// guaranteed to be non-empty.
    pub fn matches_for_command(
        &self,
        cmd: &[String],
        heuristics_fallback: HeuristicsFallback<'_>,
    ) -> Vec<RuleMatch> {
        self.matches_for_command_with_options(cmd, heuristics_fallback, &MatchOptions::default())
    }

    /// `matches_for_command` 的带选项版本，也是整个匹配流程的主算法。
    ///
    /// 匹配按「三级回退」依次尝试，命中即止：
    ///   1. 按首 token 精确匹配显式规则（`match_exact_rules`）；
    ///   2. 若开启 `resolve_host_executables` 且首 token 是绝对路径，按其
    ///      basename 再查规则（`match_host_executable_rules`）；
    ///   3. 仍无命中且提供了 `heuristics_fallback`，则产出单条
    ///      `HeuristicsRuleMatch`（启发式兜底裁决）。
    ///
    /// 不变式：`heuristics_fallback.is_some()` 时返回值保证非空——这是
    /// `Evaluation::from_matches` 「列表非空」前提的来源。
    pub fn matches_for_command_with_options(
        &self,
        cmd: &[String],
        heuristics_fallback: HeuristicsFallback<'_>,
        options: &MatchOptions,
    ) -> Vec<RuleMatch> {
        let matched_rules = self
            .match_exact_rules(cmd)
            .filter(|matched_rules| !matched_rules.is_empty())
            .or_else(|| {
                options
                    .resolve_host_executables
                    .then(|| self.match_host_executable_rules(cmd))
                    .filter(|matched_rules| !matched_rules.is_empty())
            })
            .unwrap_or_default();

        // 显式规则一个都没命中时，才落到启发式兜底（若调用方提供了的话）。
        if matched_rules.is_empty()
            && let Some(heuristics_fallback) = heuristics_fallback
        {
            vec![RuleMatch::HeuristicsRuleMatch {
                command: cmd.to_vec(),
                decision: heuristics_fallback(cmd),
            }]
        } else {
            matched_rules
        }
    }

    fn match_exact_rules(&self, cmd: &[String]) -> Option<Vec<RuleMatch>> {
        let first = cmd.first()?;
        Some(
            self.rules_by_program
                .get_vec(first)
                .map(|rules| rules.iter().filter_map(|rule| rule.matches(cmd)).collect())
                .unwrap_or_default(),
        )
    }

    /// 当命令以**绝对路径**调用时，把它当作「基于 basename 的命令」再匹配规则。
    /// 例如规则写的是 `git`，命令实际是 `/usr/bin/git status`，本方法负责让二者
    /// 对上。任一前置条件不满足（非绝对路径、无 basename、无同名规则）即返回空。
    ///
    /// 安全约束：若该 basename 在 `.rules` 里通过 `host_executable` 声明了
    /// 路径白名单，则命令的绝对路径必须在白名单内才放行匹配——否则拒绝，防止
    /// 用伪造的同名可执行文件（如 `/tmp/git`）冒用规则。命中后用
    /// `with_resolved_program` 把真实路径记进结果。
    fn match_host_executable_rules(&self, cmd: &[String]) -> Vec<RuleMatch> {
        let Some(first) = cmd.first() else {
            return Vec::new();
        };
        let Ok(program) = AbsolutePathBuf::try_from(first.clone()) else {
            return Vec::new();
        };
        let Some(basename) = executable_path_lookup_key(program.as_path()) else {
            return Vec::new();
        };
        let Some(rules) = self.rules_by_program.get_vec(&basename) else {
            return Vec::new();
        };
        // 声明了白名单却不含当前路径 → 视为不匹配（防伪造同名可执行文件）。
        if let Some(paths) = self.host_executables_by_name.get(&basename)
            && !paths.iter().any(|path| path == &program)
        {
            return Vec::new();
        }

        // 把首 token 换成 basename 再喂给规则匹配（规则是按 basename 写的）。
        let basename_command = std::iter::once(basename)
            .chain(cmd.iter().skip(1).cloned())
            .collect::<Vec<_>>();
        rules
            .iter()
            .filter_map(|rule| rule.matches(&basename_command))
            .map(|rule_match| rule_match.with_resolved_program(&program))
            .collect()
    }
}

fn upsert_domain(entries: &mut Vec<String>, host: &str) {
    entries.retain(|entry| entry != host);
    entries.push(host.to_string());
}

fn render_pattern_token(token: &PatternToken) -> String {
    match token {
        PatternToken::Single(value) => value.clone(),
        PatternToken::Alts(alternatives) => format!("[{}]", alternatives.join("|")),
    }
}

/// 一次策略评估的最终结果：合成后的单一 `decision` + 全部命中明细
/// `matched_rules`。明细保留下来供上层推导拒绝/审批理由、判断能否自动加规则。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Evaluation {
    pub decision: Decision,
    #[serde(rename = "matchedRules")]
    pub matched_rules: Vec<RuleMatch>,
}

impl Evaluation {
    /// 是否命中了**显式规则**（而非仅启发式兜底）。
    /// 只要有一条 `PrefixRuleMatch` 就算 `true`；全是 `HeuristicsRuleMatch`
    /// 则为 `false`。
    pub fn is_match(&self) -> bool {
        self.matched_rules
            .iter()
            .any(|rule_match| !matches!(rule_match, RuleMatch::HeuristicsRuleMatch { .. }))
    }

    /// Caller is responsible for ensuring that `matched_rules` is non-empty.
    /// 从命中明细合成单一裁决：**取所有命中里最严的一个**。
    /// 这依赖 `Decision` 的排序 `Allow < Prompt < Forbidden`（见
    /// `decision.rs` 的 `Ord` 派生），所以 `.max()` 即「最严」。
    /// 调用方必须保证 `matched_rules` 非空（否则 `expect` 触发 panic）——
    /// 该不变式由 `matches_for_command_with_options` 在带兜底时保证。
    fn from_matches(matched_rules: Vec<RuleMatch>) -> Self {
        let decision = matched_rules.iter().map(RuleMatch::decision).max();
        #[expect(clippy::expect_used)]
        let decision = decision.expect("invariant failed: matched_rules must be non-empty");

        Self {
            decision,
            matched_rules,
        }
    }
}
