//! 【文件职责】定义 execpolicy crate 的错误类型 `Error`、本地 `Result` 别名，
//! 以及用于在错误里携带「出错位置」的 `TextPosition` / `TextRange` /
//! `ErrorLocation`。
//!
//! 【架构位置】
//!   层级：执行策略（execpolicy）DSL · 基础设施
//!   上游：`parser.rs`（解析 `.rules` 文件时产生 `Starlark` / `InvalidRule`
//!         等错误）、`policy.rs`、`rule.rs`（规则校验产生 `ExampleDidMatch`
//!         等）、`core/src/exec_policy.rs`（把错误转成展示文案）
//!   下游：`starlark`（底层 Starlark 解析错误经 `Error::Starlark` 包装）
//!
//! 【设计背景】`.rules` 文件本质是一段 Starlark 脚本（见 `parser.rs`），
//! 解析或示例校验失败时，光报「哪里错了」不够，还要能定位到文件第几行第几列，
//! 才能在 TUI / 启动告警里给出可点击、可定位的提示。因此本文件除了普通的
//! 错误枚举，还专门保留了行列坐标结构，并提供 `with_location` / `location`
//! 把坐标在错误之间转移、提取出来。

use starlark::Error as StarlarkError;
use thiserror::Error;

/// crate 内部统一的 `Result`：错误类型固定为本模块的 `Error`，
/// 调用处只需写 `Result<T>` 即可省去重复的错误类型标注。
pub type Result<T> = std::result::Result<T, Error>;

/// 文本位置：源文件中的「第几行第几列」。
/// 注意是 1-based（从 1 开始计数），与编辑器/终端展示惯例一致；
/// 而 Starlark 内部用的是 0-based，所以转换时都会 `+ 1`（见 `location()`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextPosition {
    pub line: usize,
    pub column: usize,
}

/// 文本区间：由起止两个 `TextPosition` 围成，标记一段出错的源码范围。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextRange {
    pub start: TextPosition,
    pub end: TextPosition,
}

/// 错误定位：哪个文件（`path`）的哪一段（`range`）出了问题。
/// 供上层把解析/校验错误渲染成「file:line:col」形式的可定位提示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorLocation {
    pub path: String,
    pub range: TextRange,
}

/// execpolicy 的统一错误枚举。
/// 前四个变体是「DSL 写错了」（裁决词非法、模式/示例/规则不合法），
/// 中间两个是「示例与规则不自洽」（`r#match` / `not_match` 校验失败），
/// 最后一个 `Starlark` 包装底层脚本解析错误（含其自带的位置信息）。
#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid decision: {0}")]
    InvalidDecision(String),
    #[error("invalid pattern element: {0}")]
    InvalidPattern(String),
    #[error("invalid example: {0}")]
    InvalidExample(String),
    #[error("invalid rule: {0}")]
    InvalidRule(String),
    // 规则自带的 `r#match` 正例没有命中任何规则——通常意味着规则写得太严，
    // 或示例与模式不匹配；附带 rules / 未命中示例列表便于排查。
    #[error(
        "expected every example to match at least one rule. rules: {rules:?}; unmatched examples: \
         {examples:?}"
    )]
    ExampleDidNotMatch {
        rules: Vec<String>,
        examples: Vec<String>,
        location: Option<ErrorLocation>,
    },
    // 规则的 `not_match` 反例反而命中了规则——意味着规则写得太宽，
    // 把本不该匹配的命令也覆盖了。
    #[error("expected example to not match rule `{rule}`: {example}")]
    ExampleDidMatch {
        rule: String,
        example: String,
        location: Option<ErrorLocation>,
    },
    #[error("starlark error: {0}")]
    Starlark(StarlarkError),
}

impl Error {
    /// 给「尚无位置」的示例校验错误补上位置信息。
    /// 仅对 `ExampleDidNotMatch` / `ExampleDidMatch` 且当前 `location` 为
    /// `None` 时生效（避免覆盖已有位置）；其它错误原样返回。
    ///
    /// 设计背景：示例校验发生在规则收集完成之后（见 `parser.rs` 的
    /// `validate_pending_examples_from`），那时才知道规则定义在源文件的哪一行，
    /// 因此位置是「事后附加」而非构造时就带上。
    pub fn with_location(self, location: ErrorLocation) -> Self {
        match self {
            Error::ExampleDidNotMatch {
                rules,
                examples,
                location: None,
            } => Error::ExampleDidNotMatch {
                rules,
                examples,
                location: Some(location),
            },
            Error::ExampleDidMatch {
                rule,
                example,
                location: None,
            } => Error::ExampleDidMatch {
                rule,
                example,
                location: Some(location),
            },
            other => other,
        }
    }

    /// 提取错误的位置信息（若有）。
    /// 示例校验错误直接返回其携带的 `location`；`Starlark` 错误则从底层
    /// `span` 现场解析出来。其余错误没有位置，返回 `None`。
    pub fn location(&self) -> Option<ErrorLocation> {
        match self {
            Error::ExampleDidNotMatch { location, .. }
            | Error::ExampleDidMatch { location, .. } => location.clone(),
            Error::Starlark(err) => err.span().map(|span| {
                let resolved = span.resolve_span();
                // Starlark 行列从 0 开始，这里 +1 转成 1-based 展示坐标。
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
            }),
            _ => None,
        }
    }
}
