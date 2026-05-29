//! 【文件职责】把新规则**追加写入** `.rules` 文件（DSL 源文件），实现「持久化
//! 用户在审批弹窗里做出的放行决定」。提供两个公开入口：追加允许前缀规则
//! `blocking_append_allow_prefix_rule`、追加网络规则 `blocking_append_network_rule`。
//!
//! 【架构位置】
//!   层级：执行策略（execpolicy）DSL · 持久化写回
//!   上游：`core/src/exec_policy.rs`（`append_amendment_and_update` /
//!         `append_network_rule_and_update` 在 `spawn_blocking` 里调用）
//!   下游：`rule.rs`（host 规范化、协议序列化）、`std::fs`（带锁的文件 I/O）
//!
//! 【与 parser 的对称性】parser 把 `.rules` 文本读成规则；本文件反过来把规则
//! 写成 `.rules` 文本（生成 `prefix_rule(...)` / `network_rule(...)` 这样的
//! 函数调用行）。写出的字符串字段统一用 `serde_json::to_string` 转义，保证
//! 特殊字符不会破坏 Starlark 语法。
//!
//! 【并发与阻塞】函数名前缀 `blocking_` 是契约信号：内部用 advisory 文件锁
//! （`file.lock()`）做跨进程互斥、且是同步阻塞 I/O，因此从 async 上下文调用
//! **必须**包在 `tokio::task::spawn_blocking` 里（见各函数文档）。

use std::fs::OpenOptions;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use crate::decision::Decision;
use crate::rule::NetworkRuleProtocol;
use crate::rule::normalize_network_rule_host;
use thiserror::Error;

/// 追加规则到 `.rules` 文件过程中可能出现的错误。
/// 覆盖三类失败：DSL 层（空前缀、非法网络规则）、序列化（前缀/网络字段转
/// JSON 失败）、文件 I/O（建目录/开/读/写/锁/seek 文件失败，各自带路径与
/// 底层 `io::Error` 便于定位）。
#[derive(Debug, Error)]
pub enum AmendError {
    #[error("prefix rule requires at least one token")]
    EmptyPrefix,
    #[error("invalid network rule: {0}")]
    InvalidNetworkRule(String),
    #[error("policy path has no parent: {path}")]
    MissingParent { path: PathBuf },
    #[error("failed to create policy directory {dir}: {source}")]
    CreatePolicyDir {
        dir: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to format prefix tokens: {source}")]
    SerializePrefix { source: serde_json::Error },
    #[error("failed to serialize network rule field: {source}")]
    SerializeNetworkRule { source: serde_json::Error },
    #[error("failed to open policy file {path}: {source}")]
    OpenPolicyFile {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to write to policy file {path}: {source}")]
    WritePolicyFile {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to lock policy file {path}: {source}")]
    LockPolicyFile {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to seek policy file {path}: {source}")]
    SeekPolicyFile {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to read policy file {path}: {source}")]
    ReadPolicyFile {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to read metadata for policy file {path}: {source}")]
    PolicyMetadata {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// Note this thread uses advisory file locking and performs blocking I/O, so it should be used with
/// [`tokio::task::spawn_blocking`] when called from an async context.
/// 把一条 `decision="allow"` 的前缀规则追加到 `policy_path` 指向的 `.rules`。
///
/// @param policy_path - 目标规则文件（不存在会连同父目录一起创建）
/// @param prefix      - 前缀 token 序列，不能为空
/// @returns           - 空前缀/序列化/IO 失败时返回对应 `AmendError`
///
/// 用户在审批弹窗点「以后总是允许该命令」后，core 侧据此把规则落盘，下次
/// 启动即生效。每个 token 用 JSON 转义后拼成 `pattern=[...]`，再渲染成完整
/// 的 `prefix_rule(...)` 行，最终走带锁追加（去重见 `append_locked_line`）。
pub fn blocking_append_allow_prefix_rule(
    policy_path: &Path,
    prefix: &[String],
) -> Result<(), AmendError> {
    if prefix.is_empty() {
        return Err(AmendError::EmptyPrefix);
    }

    let tokens = prefix
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| AmendError::SerializePrefix { source })?;
    let pattern = format!("[{}]", tokens.join(", "));
    let rule = format!(r#"prefix_rule(pattern={pattern}, decision="allow")"#);
    append_rule_line(policy_path, &rule)
}

/// Note this function uses advisory file locking and performs blocking I/O, so it should be used
/// with [`tokio::task::spawn_blocking`] when called from an async context.
/// 把一条网络规则追加到 `.rules`，生成 `network_rule(host=, protocol=, decision=, ...)`。
///
/// @param justification - 可选理由；给了但为空白则报错（理由要么不写、
///                        要么有实质内容）
///
/// 注意裁决词的映射：写回时 `Forbidden` 渲染成 `"deny"`（与 parser 接受的
/// 别名对齐），而非 `Decision::parse` 用的 `"forbidden"`。host 同样先经
/// `normalize_network_rule_host` 规范化，所有字符串字段都 JSON 转义。
pub fn blocking_append_network_rule(
    policy_path: &Path,
    host: &str,
    protocol: NetworkRuleProtocol,
    decision: Decision,
    justification: Option<&str>,
) -> Result<(), AmendError> {
    let host = normalize_network_rule_host(host)
        .map_err(|err| AmendError::InvalidNetworkRule(err.to_string()))?;
    if let Some(raw) = justification
        && raw.trim().is_empty()
    {
        return Err(AmendError::InvalidNetworkRule(
            "justification cannot be empty".to_string(),
        ));
    }

    let host = serde_json::to_string(&host)
        .map_err(|source| AmendError::SerializeNetworkRule { source })?;
    let protocol = serde_json::to_string(protocol.as_policy_string())
        .map_err(|source| AmendError::SerializeNetworkRule { source })?;
    let decision = serde_json::to_string(match decision {
        Decision::Allow => "allow",
        Decision::Prompt => "prompt",
        Decision::Forbidden => "deny",
    })
    .map_err(|source| AmendError::SerializeNetworkRule { source })?;

    let mut args = vec![
        format!("host={host}"),
        format!("protocol={protocol}"),
        format!("decision={decision}"),
    ];
    if let Some(justification) = justification {
        let justification = serde_json::to_string(justification)
            .map_err(|source| AmendError::SerializeNetworkRule { source })?;
        args.push(format!("justification={justification}"));
    }
    let rule = format!("network_rule({})", args.join(", "));
    append_rule_line(policy_path, &rule)
}

/// 确保 `.rules` 文件的父目录存在，然后把 `rule` 行带锁追加进去。
/// 目录已存在（`AlreadyExists`）视为成功，不当作错误。
fn append_rule_line(policy_path: &Path, rule: &str) -> Result<(), AmendError> {
    let dir = policy_path
        .parent()
        .ok_or_else(|| AmendError::MissingParent {
            path: policy_path.to_path_buf(),
        })?;
    match std::fs::create_dir(dir) {
        Ok(()) => {}
        Err(ref source) if source.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(source) => {
            return Err(AmendError::CreatePolicyDir {
                dir: dir.to_path_buf(),
                source,
            });
        }
    }

    append_locked_line(policy_path, rule)
}

/// 把一行内容幂等地、加锁追加到文件末尾——本文件并发安全的核心。
///
/// 副作用：可能创建文件、写入磁盘；持有 advisory 文件锁直至函数返回（`file`
/// drop 时释放）。
///
/// 流程要点：
///   1. 以 create+read+append 打开，再 `lock()` 取独占锁（跨进程互斥，防两个
///      Codex 进程同时改同一文件造成交错写入）。
///   2. seek 回 0 读全文，做**去重**：该行已存在就直接返回，不重复追加。
///   3. 若文件非空且末尾缺换行，先补一个 `\n`，避免新行粘在旧行尾。
///   4. 追加 `line + "\n"`。
/// 注意：以 append 模式打开时，写入位置始终在末尾，第 2 步的 seek 只为读取，
/// 不影响第 3、4 步的写入位置。
fn append_locked_line(policy_path: &Path, line: &str) -> Result<(), AmendError> {
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(policy_path)
        .map_err(|source| AmendError::OpenPolicyFile {
            path: policy_path.to_path_buf(),
            source,
        })?;
    file.lock().map_err(|source| AmendError::LockPolicyFile {
        path: policy_path.to_path_buf(),
        source,
    })?;

    file.seek(SeekFrom::Start(0))
        .map_err(|source| AmendError::SeekPolicyFile {
            path: policy_path.to_path_buf(),
            source,
        })?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .map_err(|source| AmendError::ReadPolicyFile {
            path: policy_path.to_path_buf(),
            source,
        })?;

    // 幂等：完全相同的规则行已存在则跳过，避免重复审批积累冗余规则。
    if contents.lines().any(|existing| existing == line) {
        return Ok(());
    }

    // 旧内容末尾没有换行时先补一个，防止新规则与上一行粘连成一行。
    if !contents.is_empty() && !contents.ends_with('\n') {
        file.write_all(b"\n")
            .map_err(|source| AmendError::WritePolicyFile {
                path: policy_path.to_path_buf(),
                source,
            })?;
    }

    file.write_all(format!("{line}\n").as_bytes())
        .map_err(|source| AmendError::WritePolicyFile {
            path: policy_path.to_path_buf(),
            source,
        })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    #[test]
    fn appends_rule_and_creates_directories() {
        let tmp = tempdir().expect("create temp dir");
        let policy_path = tmp.path().join("rules").join("default.rules");

        blocking_append_allow_prefix_rule(
            &policy_path,
            &[String::from("echo"), String::from("Hello, world!")],
        )
        .expect("append rule");

        let contents = std::fs::read_to_string(&policy_path).expect("default.rules should exist");
        assert_eq!(
            contents,
            r#"prefix_rule(pattern=["echo", "Hello, world!"], decision="allow")
"#
        );
    }

    #[test]
    fn appends_rule_without_duplicate_newline() {
        let tmp = tempdir().expect("create temp dir");
        let policy_path = tmp.path().join("rules").join("default.rules");
        std::fs::create_dir_all(policy_path.parent().unwrap()).expect("create policy dir");
        std::fs::write(
            &policy_path,
            r#"prefix_rule(pattern=["ls"], decision="allow")
"#,
        )
        .expect("write seed rule");

        blocking_append_allow_prefix_rule(
            &policy_path,
            &[String::from("echo"), String::from("Hello, world!")],
        )
        .expect("append rule");

        let contents = std::fs::read_to_string(&policy_path).expect("read policy");
        assert_eq!(
            contents,
            r#"prefix_rule(pattern=["ls"], decision="allow")
prefix_rule(pattern=["echo", "Hello, world!"], decision="allow")
"#
        );
    }

    #[test]
    fn inserts_newline_when_missing_before_append() {
        let tmp = tempdir().expect("create temp dir");
        let policy_path = tmp.path().join("rules").join("default.rules");
        std::fs::create_dir_all(policy_path.parent().unwrap()).expect("create policy dir");
        std::fs::write(
            &policy_path,
            r#"prefix_rule(pattern=["ls"], decision="allow")"#,
        )
        .expect("write seed rule without newline");

        blocking_append_allow_prefix_rule(
            &policy_path,
            &[String::from("echo"), String::from("Hello, world!")],
        )
        .expect("append rule");

        let contents = std::fs::read_to_string(&policy_path).expect("read policy");
        assert_eq!(
            contents,
            r#"prefix_rule(pattern=["ls"], decision="allow")
prefix_rule(pattern=["echo", "Hello, world!"], decision="allow")
"#
        );
    }

    #[test]
    fn appends_network_rule() {
        let tmp = tempdir().expect("create temp dir");
        let policy_path = tmp.path().join("rules").join("default.rules");

        blocking_append_network_rule(
            &policy_path,
            "Api.GitHub.com",
            NetworkRuleProtocol::Https,
            Decision::Allow,
            Some("Allow https_connect access to api.github.com"),
        )
        .expect("append network rule");

        let contents = std::fs::read_to_string(&policy_path).expect("read policy");
        assert_eq!(
            contents,
            r#"network_rule(host="api.github.com", protocol="https", decision="allow", justification="Allow https_connect access to api.github.com")
"#
        );
    }

    #[test]
    fn appends_prefix_and_network_rules() {
        let tmp = tempdir().expect("create temp dir");
        let policy_path = tmp.path().join("rules").join("default.rules");

        blocking_append_allow_prefix_rule(&policy_path, &[String::from("curl")])
            .expect("append prefix rule");
        blocking_append_network_rule(
            &policy_path,
            "api.github.com",
            NetworkRuleProtocol::Https,
            Decision::Allow,
            Some("Allow https_connect access to api.github.com"),
        )
        .expect("append network rule");

        let contents = std::fs::read_to_string(&policy_path).expect("read policy");
        assert_eq!(
            contents,
            r#"prefix_rule(pattern=["curl"], decision="allow")
network_rule(host="api.github.com", protocol="https", decision="allow", justification="Allow https_connect access to api.github.com")
"#
        );
    }

    #[test]
    fn rejects_wildcard_network_rule_host() {
        let tmp = tempdir().expect("create temp dir");
        let policy_path = tmp.path().join("rules").join("default.rules");
        let err = blocking_append_network_rule(
            &policy_path,
            "*.example.com",
            NetworkRuleProtocol::Https,
            Decision::Allow,
            /*justification*/ None,
        )
        .expect_err("wildcards should be rejected");
        assert_eq!(
            err.to_string(),
            "invalid network rule: invalid rule: network_rule host must be a specific host; wildcards are not allowed"
        );
    }
}
