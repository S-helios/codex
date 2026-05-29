//! 【文件职责】把 `.rules` 文件（一段 Starlark 脚本）解析成内存中的 `Policy`。
//! 提供对外的 `PolicyParser`（可多次 `parse` 累积、最后 `build`），以及注册给
//! Starlark 解释器的内建函数 `prefix_rule` / `network_rule` / `host_executable`。
//!
//! 【架构位置】
//!   层级：执行策略（execpolicy）DSL · 解析前端
//!   上游：`core/src/exec_policy.rs::load_exec_policy`（按层逐个文件 `parse`，
//!         再 `build` 出最终 `Policy`）
//!   下游：`policy.rs`（产物 `Policy`）、`rule.rs`（构造各类规则）、
//!         `starlark` crate（底层脚本解析与求值）
//!
//! 【为什么用 Starlark】`.rules` 不是死板的配置，而是可写函数调用、列表、
//! f-string 的小型脚本（Starlark 是 Python 的安全受限子集，无文件/网络副作用）。
//! 规则作者通过调用内建函数声明规则；本文件把这些函数注册进解释器，
//! 求值脚本时函数被回调，从而把声明「收集」进 `PolicyBuilder`。
//!
//! 【数据流】`.rules` 文本 → `AstModule::parse` → `eval_module`
//!   →（内建函数被回调，往 `PolicyBuilder` 累积规则）
//!   → 校验 pending 示例 → `build()` → `Policy`
//!
//! 【阅读建议】先看 `PolicyParser::parse`（解析主流程），再看底部
//! `#[starlark_module] policy_builtins` 里三个内建函数（DSL 的实际词汇表）；
//! 中段是把 Starlark `Value` 转成 Rust 类型的解析辅助函数。

use codex_utils_absolute_path::AbsolutePathBuf;
use multimap::MultiMap;
use starlark::any::ProvidesStaticType;
use starlark::codemap::FileSpan;
use starlark::environment::GlobalsBuilder;
use starlark::environment::Module;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::syntax::AstModule;
use starlark::syntax::Dialect;
use starlark::values::Value;
use starlark::values::list::ListRef;
use starlark::values::list::UnpackList;
use starlark::values::none::NoneType;
use std::cell::RefCell;
use std::cell::RefMut;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use crate::decision::Decision;
use crate::error::Error;
use crate::error::ErrorLocation;
use crate::error::Result;
use crate::error::TextPosition;
use crate::error::TextRange;
use crate::executable_name::executable_lookup_key;
use crate::executable_name::executable_path_lookup_key;
use crate::rule::NetworkRule;
use crate::rule::NetworkRuleProtocol;
use crate::rule::PatternToken;
use crate::rule::PrefixPattern;
use crate::rule::PrefixRule;
use crate::rule::RuleRef;
use crate::rule::validate_match_examples;
use crate::rule::validate_not_match_examples;

/// 策略解析器：对外门面。内部持有一个 `PolicyBuilder` 累积器，可对多个
/// `.rules` 文件依次 `parse`（规则累加），最后 `build` 出合并后的 `Policy`。
///
/// 用 `RefCell` 包裹 builder：因为 Starlark 内建函数通过 `Evaluator.extra`
/// 拿到的是 `&` 共享引用（见 `policy_builder()`），却需要可变地往里加规则，
/// 故借内部可变性绕过借用检查。
pub struct PolicyParser {
    builder: RefCell<PolicyBuilder>,
}

impl Default for PolicyParser {
    fn default() -> Self {
        Self::new()
    }
}

impl PolicyParser {
    pub fn new() -> Self {
        Self {
            builder: RefCell::new(PolicyBuilder::new()),
        }
    }

    /// Parses a policy, tagging parser errors with `policy_identifier` so failures include the
    /// identifier alongside line numbers.
    /// 解析一个 `.rules` 文件，把其中的规则累加进内部 builder。
    ///
    /// @param policy_identifier    - 文件标识（通常是路径），错误信息里会带上
    ///                               它配合行号，便于定位
    /// @param policy_file_contents - 文件文本内容
    /// @returns                    - 解析或示例校验失败时返回 `Error`
    ///
    /// 副作用：向 `self.builder` 追加规则。可对多个文件多次调用，规则累积。
    pub fn parse(&mut self, policy_identifier: &str, policy_file_contents: &str) -> Result<()> {
        // Step 1：记下解析前已有的 pending 示例数。本文件可能新增若干「待校验
        // 示例」，结束时只校验本次新增的那批（`..from(start)`），避免重复校验
        // 之前文件已校验过的示例。
        let pending_validation_count = self.builder.borrow().pending_example_validations.len();

        // Step 2：配置 Starlark 方言。用 Extended 并开启 f-string，让规则作者
        // 能用 f"..." 拼接路径等动态内容。
        let mut dialect = Dialect::Extended.clone();
        dialect.enable_f_strings = true;
        let ast = AstModule::parse(
            policy_identifier,
            policy_file_contents.to_string(),
            &dialect,
        )
        .map_err(Error::Starlark)?;

        // Step 3：注册内建函数后求值脚本。求值过程中 `prefix_rule` 等被回调，
        // 通过 `eval.extra`（指向 `self.builder`）把规则写入累积器。
        let globals = GlobalsBuilder::standard().with(policy_builtins).build();
        let module = Module::new();
        {
            let mut eval = Evaluator::new(&module);
            eval.extra = Some(&self.builder);
            eval.eval_module(ast, &globals).map_err(Error::Starlark)?;
        }

        // Step 4：校验本次新增规则的 r#match / not_match 示例（见 Step 1 说明）。
        self.builder
            .borrow()
            .validate_pending_examples_from(pending_validation_count)?;
        Ok(())
    }

    /// 消费解析器，把累积的规则固化为不可变的 `Policy`。
    pub fn build(self) -> crate::policy::Policy {
        self.builder.into_inner().build()
    }
}

/// 规则累积器：解析期的可变中间态，最终 `build` 成 `Policy`。
/// 派生 `ProvidesStaticType` 是为了能通过 `Evaluator.extra` 在内建函数里被
/// 安全地向下转型取回（见 `policy_builder()`）。
///
/// `pending_example_validations` 单独存放而非边解析边校验：因为示例校验需要
/// 拿「同一文件内定义的全部规则」组成临时 `Policy` 来跑，必须等该文件求值
/// 完成后再做（见 `validate_pending_examples_from`）。
#[derive(Debug, ProvidesStaticType)]
struct PolicyBuilder {
    rules_by_program: MultiMap<String, RuleRef>,
    network_rules: Vec<NetworkRule>,
    host_executables_by_name: HashMap<String, Arc<[AbsolutePathBuf]>>,
    pending_example_validations: Vec<PendingExampleValidation>,
}

impl PolicyBuilder {
    fn new() -> Self {
        Self {
            rules_by_program: MultiMap::new(),
            network_rules: Vec::new(),
            host_executables_by_name: HashMap::new(),
            pending_example_validations: Vec::new(),
        }
    }

    fn add_rule(&mut self, rule: RuleRef) {
        self.rules_by_program
            .insert(rule.program().to_string(), rule);
    }

    fn add_network_rule(&mut self, rule: NetworkRule) {
        self.network_rules.push(rule);
    }

    fn add_host_executable(&mut self, name: String, paths: Vec<AbsolutePathBuf>) {
        self.host_executables_by_name.insert(name, paths.into());
    }

    fn add_pending_example_validation(
        &mut self,
        rules: Vec<RuleRef>,
        matches: Vec<Vec<String>>,
        not_matches: Vec<Vec<String>>,
        location: Option<ErrorLocation>,
    ) {
        self.pending_example_validations
            .push(PendingExampleValidation {
                rules,
                matches,
                not_matches,
                location,
            });
    }

    /// 校验从下标 `start` 起新增的那批 pending 示例（`start` 由调用方在解析前
    /// 记下，见 `PolicyParser::parse` Step 1）。
    ///
    /// 关键设计：每条 `prefix_rule` 的示例只针对**它自己生成的规则**校验，
    /// 因此为每条 validation 单独拼一个仅含其规则的临时 `Policy` 来跑——而不是
    /// 拿全局策略，否则别处的规则可能误命中本规则的反例，造成误判。
    /// 校验失败时用 `attach_validation_location` 补上规则在源码里的位置。
    fn validate_pending_examples_from(&self, start: usize) -> Result<()> {
        for validation in &self.pending_example_validations[start..] {
            let mut rules_by_program = MultiMap::new();
            for rule in &validation.rules {
                rules_by_program.insert(rule.program().to_string(), rule.clone());
            }

            let policy = crate::policy::Policy::from_parts(
                rules_by_program,
                Vec::new(),
                self.host_executables_by_name.clone(),
            );
            // 先验反例（不该匹配）再验正例（必须匹配）；任一失败立即返回。
            validate_not_match_examples(&policy, &validation.rules, &validation.not_matches)
                .map_err(|error| attach_validation_location(error, validation.location.clone()))?;
            validate_match_examples(&policy, &validation.rules, &validation.matches)
                .map_err(|error| attach_validation_location(error, validation.location.clone()))?;
        }

        Ok(())
    }

    fn build(self) -> crate::policy::Policy {
        crate::policy::Policy::from_parts(
            self.rules_by_program,
            self.network_rules,
            self.host_executables_by_name,
        )
    }
}

#[derive(Debug)]
struct PendingExampleValidation {
    rules: Vec<RuleRef>,
    matches: Vec<Vec<String>>,
    not_matches: Vec<Vec<String>>,
    location: Option<ErrorLocation>,
}

fn parse_pattern<'v>(pattern: UnpackList<Value<'v>>) -> Result<Vec<PatternToken>> {
    let tokens: Vec<PatternToken> = pattern
        .items
        .into_iter()
        .map(parse_pattern_token)
        .collect::<Result<_>>()?;
    if tokens.is_empty() {
        Err(Error::InvalidPattern("pattern cannot be empty".to_string()))
    } else {
        Ok(tokens)
    }
}

/// 把单个模式元素（Starlark 值）解析为 `PatternToken`。
/// 规则：字符串 → `Single`；字符串列表 → `Alts`（多选一）；其它类型报错。
/// 边界处理：空列表报错；单元素列表退化为 `Single`（无需多选语义）。
fn parse_pattern_token<'v>(value: Value<'v>) -> Result<PatternToken> {
    if let Some(s) = value.unpack_str() {
        Ok(PatternToken::Single(s.to_string()))
    } else if let Some(list) = ListRef::from_value(value) {
        let tokens: Vec<String> = list
            .content()
            .iter()
            .map(|value| {
                value
                    .unpack_str()
                    .ok_or_else(|| {
                        Error::InvalidPattern(format!(
                            "pattern alternative must be a string (got {})",
                            value.get_type()
                        ))
                    })
                    .map(str::to_string)
            })
            .collect::<Result<_>>()?;

        match tokens.as_slice() {
            [] => Err(Error::InvalidPattern(
                "pattern alternatives cannot be empty".to_string(),
            )),
            [single] => Ok(PatternToken::Single(single.clone())),
            _ => Ok(PatternToken::Alts(tokens)),
        }
    } else {
        Err(Error::InvalidPattern(format!(
            "pattern element must be a string or list of strings (got {})",
            value.get_type()
        )))
    }
}

fn parse_examples<'v>(examples: UnpackList<Value<'v>>) -> Result<Vec<Vec<String>>> {
    examples.items.into_iter().map(parse_example).collect()
}

/// 解析 `host_executable` 声明里的路径，要求必须是绝对路径。
/// 相对路径会被拒绝——白名单要锁定到磁盘上确切的某个文件，相对路径无法保证。
fn parse_literal_absolute_path(raw: &str) -> Result<AbsolutePathBuf> {
    if !Path::new(raw).is_absolute() {
        return Err(Error::InvalidRule(format!(
            "host_executable paths must be absolute (got {raw})"
        )));
    }

    AbsolutePathBuf::try_from(raw.to_string())
        .map_err(|error| Error::InvalidRule(format!("invalid absolute path `{raw}`: {error}")))
}

/// 校验 `host_executable` 的 name 必须是「裸可执行文件名」（不含路径分隔符）。
/// 例如允许 `git`、拒绝 `bin/git` 或 `/usr/bin/git`——name 是索引键，必须与
/// 命令 basename 对齐，带路径会破坏匹配语义。
fn validate_host_executable_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::InvalidRule(
            "host_executable name cannot be empty".to_string(),
        ));
    }

    // 「单一路径组件且该组件等于 name 本身」即认定为裸文件名。
    let path = Path::new(name);
    if path.components().count() != 1
        || path.file_name().and_then(|value| value.to_str()) != Some(name)
    {
        return Err(Error::InvalidRule(format!(
            "host_executable name must be a bare executable name (got {name})"
        )));
    }

    Ok(())
}

/// 解析网络规则的裁决词。比通用 `Decision::parse` 多接受一个别名 `deny`
/// （等价于 `Forbidden`）——网络场景下 `deny` 比 `forbidden` 更符合直觉。
fn parse_network_rule_decision(raw: &str) -> Result<Decision> {
    match raw {
        "deny" => Ok(Decision::Forbidden),
        other => Decision::parse(other),
    }
}

/// 把 Starlark 的 `FileSpan` 转成本 crate 的 `ErrorLocation`。
/// 行列均 `+1`：Starlark 用 0-based，本 crate 对外用 1-based（见 `error.rs`）。
fn error_location_from_file_span(span: FileSpan) -> ErrorLocation {
    let resolved = span.resolve_span();
    ErrorLocation {
        path: span.filename().to_string(),
        range: TextRange {
            start: TextPosition {
                line: resolved.begin.line + 1,
                column: resolved.begin.column + 1,
            },
            end: TextPosition {
                line: resolved.end.line + 1,
                column: resolved.end.column + 1,
            },
        },
    }
}

fn attach_validation_location(error: Error, location: Option<ErrorLocation>) -> Error {
    match location {
        Some(location) => error.with_location(location),
        None => error,
    }
}

fn parse_example<'v>(value: Value<'v>) -> Result<Vec<String>> {
    if let Some(raw) = value.unpack_str() {
        parse_string_example(raw)
    } else if let Some(list) = ListRef::from_value(value) {
        parse_list_example(list)
    } else {
        Err(Error::InvalidExample(format!(
            "example must be a string or list of strings (got {})",
            value.get_type()
        )))
    }
}

fn parse_string_example(raw: &str) -> Result<Vec<String>> {
    let tokens = shlex::split(raw).ok_or_else(|| {
        Error::InvalidExample("example string has invalid shell syntax".to_string())
    })?;

    if tokens.is_empty() {
        Err(Error::InvalidExample(
            "example cannot be an empty string".to_string(),
        ))
    } else {
        Ok(tokens)
    }
}

fn parse_list_example(list: &ListRef) -> Result<Vec<String>> {
    let tokens: Vec<String> = list
        .content()
        .iter()
        .map(|value| {
            value
                .unpack_str()
                .ok_or_else(|| {
                    Error::InvalidExample(format!(
                        "example tokens must be strings (got {})",
                        value.get_type()
                    ))
                })
                .map(str::to_string)
        })
        .collect::<Result<_>>()?;

    if tokens.is_empty() {
        Err(Error::InvalidExample(
            "example cannot be an empty list".to_string(),
        ))
    } else {
        Ok(tokens)
    }
}

/// 从 Starlark `Evaluator` 的 `extra` 字段取回 `PolicyBuilder` 的可变借用。
/// 内建函数没有别的途径访问累积器，只能经由 `eval.extra`（在
/// `PolicyParser::parse` 里被设为指向 `RefCell<PolicyBuilder>`）。
/// 两个 `expect` 对应「调用方必须正确设置 extra」的内部不变式——配置正确时
/// 不会触发。
fn policy_builder<'v, 'a>(eval: &Evaluator<'v, 'a, '_>) -> RefMut<'a, PolicyBuilder> {
    #[expect(clippy::expect_used)]
    eval.extra
        .as_ref()
        .expect("policy_builder requires Evaluator.extra to be populated")
        .downcast_ref::<RefCell<PolicyBuilder>>()
        .expect("Evaluator.extra must contain a PolicyBuilder")
        .borrow_mut()
}

/// `.rules` DSL 的内建函数词汇表。`#[starlark_module]` 宏把下面每个 `fn`
/// 暴露成 Starlark 全局函数，规则作者在 `.rules` 里直接调用它们来声明规则。
/// 三个函数即 DSL 的全部「关键字」：`prefix_rule` / `network_rule` /
/// `host_executable`。
#[starlark_module]
fn policy_builtins(builder: &mut GlobalsBuilder) {
    /// DSL 函数 `prefix_rule(pattern, decision=, match=, not_match=, justification=)`：
    /// 声明一条命令前缀规则。
    /// - `pattern`：token 列表，元素为字符串(精确)或字符串列表(多选一)。
    /// - `decision`：缺省为 `allow`。
    /// - `match` / `not_match`：正/反例，解析期立即校验（见 builder）。
    /// 首 token 的每个候选会各生成一条规则（因首 token 是索引键，不能多选）。
    fn prefix_rule<'v>(
        pattern: UnpackList<Value<'v>>,
        decision: Option<&'v str>,
        r#match: Option<UnpackList<Value<'v>>>,
        not_match: Option<UnpackList<Value<'v>>>,
        justification: Option<&'v str>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let decision = match decision {
            Some(raw) => Decision::parse(raw)?,
            None => Decision::Allow,
        };

        let justification = match justification {
            Some(raw) if raw.trim().is_empty() => {
                return Err(Error::InvalidRule("justification cannot be empty".to_string()).into());
            }
            Some(raw) => Some(raw.to_string()),
            None => None,
        };

        let pattern_tokens = parse_pattern(pattern)?;

        let matches: Vec<Vec<String>> =
            r#match.map(parse_examples).transpose()?.unwrap_or_default();
        let not_matches: Vec<Vec<String>> = not_match
            .map(parse_examples)
            .transpose()?
            .unwrap_or_default();
        let location = eval
            .call_stack_top_location()
            .map(error_location_from_file_span);

        let mut builder = policy_builder(eval);

        let (first_token, remaining_tokens) = pattern_tokens
            .split_first()
            .ok_or_else(|| Error::InvalidPattern("pattern cannot be empty".to_string()))?;

        let rest: Arc<[PatternToken]> = remaining_tokens.to_vec().into();

        // 首 token 若是多选（如 `["python", "python3"]`），为每个候选各建一条
        // 规则——因为 `Policy` 按首 token 建索引，索引键必须是确定的单字符串。
        // 后续 token 共享同一个 `rest`（`Arc` 克隆，零拷贝）。
        let rules: Vec<RuleRef> = first_token
            .alternatives()
            .iter()
            .map(|head| {
                Arc::new(PrefixRule {
                    pattern: PrefixPattern {
                        first: Arc::from(head.as_str()),
                        rest: rest.clone(),
                    },
                    decision,
                    justification: justification.clone(),
                }) as RuleRef
            })
            .collect();

        // 先登记示例校验（针对这批规则），再把规则真正加入累积器。
        builder.add_pending_example_validation(rules.clone(), matches, not_matches, location);
        rules.into_iter().for_each(|rule| builder.add_rule(rule));
        Ok(NoneType)
    }

    /// DSL 函数 `network_rule(host, protocol, decision, justification=)`：
    /// 声明一条网络规则。`host` 会被规范化（剥端口/小写/校验），`decision`
    /// 接受 `deny` 别名。不参与命令匹配，最终编译为代理的允许/拒绝域名清单。
    fn network_rule<'v>(
        host: &'v str,
        protocol: &'v str,
        decision: &'v str,
        justification: Option<&'v str>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let protocol = NetworkRuleProtocol::parse(protocol)?;
        let decision = parse_network_rule_decision(decision)?;
        let justification = match justification {
            Some(raw) if raw.trim().is_empty() => {
                return Err(Error::InvalidRule("justification cannot be empty".to_string()).into());
            }
            Some(raw) => Some(raw.to_string()),
            None => None,
        };

        let mut builder = policy_builder(eval);
        builder.add_network_rule(NetworkRule {
            host: crate::rule::normalize_network_rule_host(host)?,
            protocol,
            decision,
            justification,
        });
        Ok(NoneType)
    }

    /// DSL 函数 `host_executable(name, paths)`：把裸命令名 `name` 绑定到一组
    /// 允许的绝对路径白名单。配合 `MatchOptions.resolve_host_executables`：
    /// 命令以绝对路径调用时，只有路径在白名单内，针对 `name` 的规则才生效，
    /// 防止伪造的同名可执行文件冒用规则（见 `policy.rs`）。
    /// 每个 path 的 basename 必须等于 `name`，重复路径自动去重。
    fn host_executable<'v>(
        name: &'v str,
        paths: UnpackList<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        validate_host_executable_name(name)?;

        let mut parsed_paths = Vec::new();
        for value in paths.items {
            let raw = value.unpack_str().ok_or_else(|| {
                Error::InvalidRule(format!(
                    "host_executable paths must be strings (got {})",
                    value.get_type()
                ))
            })?;
            let path = parse_literal_absolute_path(raw)?;
            let Some(path_name) = executable_path_lookup_key(path.as_path()) else {
                return Err(Error::InvalidRule(format!(
                    "host_executable path `{raw}` must have basename `{name}`"
                ))
                .into());
            };
            if path_name != executable_lookup_key(name) {
                return Err(Error::InvalidRule(format!(
                    "host_executable path `{raw}` must have basename `{name}`"
                ))
                .into());
            }
            if !parsed_paths.iter().any(|existing| existing == &path) {
                parsed_paths.push(path);
            }
        }

        policy_builder(eval).add_host_executable(executable_lookup_key(name), parsed_paths);
        Ok(NoneType)
    }
}
