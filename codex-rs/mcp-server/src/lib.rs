//! Prototype MCP server.
//!
//! 【文件职责】codex 作为 MCP (Model Context Protocol) server 的进程入口，
//! 把 Codex 的会话能力包装成 MCP 工具暴露给外部客户端。
//!
//! 【架构位置】
//!   层级：MCP server 入口层
//!   上游：codex CLI 的 `mcp` 子命令（传入 arg0 路径、CLI -c 覆盖项）
//!   下游：`message_processor::MessageProcessor`（处理每条 JSON-RPC 消息）、
//!         `outgoing_message`（向客户端回写消息）
//!
//! 【数据流】
//!   stdin (JSON-RPC 一行一条) → 反序列化 → incoming 通道 → MessageProcessor
//!   → outgoing 通道 → 序列化 → stdout
//!   即标准的 stdio JSON-RPC transport：三个 Tokio 任务用 mpsc 通道串联成流水线。
//!
//! 【阅读建议】只有一个公开入口 `run_main()`，从它读起。重点看其中 spawn 的
//!   三个任务（stdin 读取 / 消息处理 / stdout 写出）如何靠通道首尾相接，
//!   以及关闭如何从 EOF 逐级传播。
// 禁止在本 crate 直接 print 到 stdout/stderr：stdout 是 JSON-RPC 传输通道，
// 任何裸 print 都会破坏协议帧；日志一律走 tracing 写到 stderr。
#![deny(clippy::print_stdout, clippy::print_stderr)]

use std::io::ErrorKind;
use std::io::Result as IoResult;
use std::sync::Arc;

use codex_arg0::Arg0DispatchPaths;
use codex_core::config::ConfigBuilder;
use codex_core::resolve_installation_id;
use codex_exec_server::EnvironmentManager;
use codex_exec_server::ExecServerRuntimePaths;
use codex_login::default_client::set_default_client_residency_requirement;
use codex_utils_cli::CliConfigOverrides;

use rmcp::model::ClientNotification;
use rmcp::model::ClientRequest;
use rmcp::model::JsonRpcMessage;
use serde_json::Value;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::io::{self};
use tokio::sync::mpsc;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;

mod codex_tool_config;
mod codex_tool_runner;
mod exec_approval;
pub(crate) mod message_processor;
mod outgoing_message;
mod patch_approval;

use crate::message_processor::MessageProcessor;
use crate::outgoing_message::OutgoingJsonRpcMessage;
use crate::outgoing_message::OutgoingMessage;
use crate::outgoing_message::OutgoingMessageSender;

pub use crate::codex_tool_config::CodexToolCallParam;
pub use crate::codex_tool_config::CodexToolCallReplyParam;
pub use crate::exec_approval::ExecApprovalElicitRequestParams;
pub use crate::exec_approval::ExecApprovalResponse;
pub use crate::patch_approval::PatchApprovalElicitRequestParams;
pub use crate::patch_approval::PatchApprovalResponse;

/// Size of the bounded channels used to communicate between tasks. The value
/// is a balance between throughput and memory usage – 128 messages should be
/// plenty for an interactive CLI.
/// 任务间有界通道（incoming 方向）的容量：在吞吐和内存间折中。
/// 128 条对交互式 CLI 绰绰有余；满了会对 stdin 读取任务形成背压（背压只发生在
/// incoming 方向，outgoing 是 unbounded 通道）。
const CHANNEL_CAPACITY: usize = 128;
// MCP server 默认开启遥测/分析上报；当 config 未显式配置时回退到此默认值。
const DEFAULT_ANALYTICS_ENABLED: bool = true;
// OTEL 上报时标识本服务的名字，便于在观测后端区分来源进程。
const OTEL_SERVICE_NAME: &str = "codex_mcp_server";

// 从客户端进来的一条 JSON-RPC 消息：可能是 Request / Response / Notification /
// Error 四种之一（由 `JsonRpcMessage` 的变体表达）。
type IncomingMessage = JsonRpcMessage<ClientRequest, Value, ClientNotification>;

/// MCP server 的进程主入口：构建 Config、初始化遥测、搭好三条流水线任务，
/// 然后阻塞等待它们全部结束。
///
/// @param arg0_paths          - 当前可执行文件及 linux-sandbox 等辅助程序的路径，
///                              传递给会话以便按需重新 exec 自己（见 arg0 调度机制）
/// @param cli_config_overrides - 命令行 `-c key=value` 形式的配置覆盖项（尚未解析）
/// @param strict_config        - 严格配置模式：未知配置键是否报错而非忽略
/// @returns                   - 三个任务全部退出后返回 `Ok(())`；构建期错误转成 io::Error
///
/// 副作用：注册全局 tracing subscriber、读取磁盘上的 config、初始化状态库与遥测，
/// 长期占用 stdin/stdout 直到 EOF。
pub async fn run_main(
    arg0_paths: Arg0DispatchPaths,
    cli_config_overrides: CliConfigOverrides,
    strict_config: bool,
) -> IoResult<()> {
    // Parse CLI overrides once and derive the base Config eagerly so later
    // components do not need to work with raw TOML values.
    // 一次性解析 CLI 覆盖项并立即构建出基线 Config：后续组件直接拿到结构化的
    // Config，无需再和原始 TOML 值打交道。各类构建错误统一映射成 io::Error 上抛。
    let cli_kv_overrides = cli_config_overrides.parse_overrides().map_err(|e| {
        std::io::Error::new(
            ErrorKind::InvalidInput,
            format!("error parsing -c overrides: {e}"),
        )
    })?;
    let config = ConfigBuilder::default()
        .cli_overrides(cli_kv_overrides)
        .strict_config(strict_config)
        .build()
        .await
        .map_err(|e| {
            std::io::Error::new(ErrorKind::InvalidData, format!("error loading config: {e}"))
        })?;
    set_default_client_residency_requirement(config.enforce_residency.value());
    let otel = codex_core::otel_init::build_provider(
        &config,
        env!("CARGO_PKG_VERSION"),
        Some(OTEL_SERVICE_NAME),
        DEFAULT_ANALYTICS_ENABLED,
    )
    .map_err(|e| {
        std::io::Error::new(
            ErrorKind::InvalidData,
            format!("error loading otel config: {e}"),
        )
    })?;
    codex_core::otel_init::record_process_start(otel.as_ref(), OTEL_SERVICE_NAME);
    codex_core::otel_init::install_sqlite_telemetry(otel.as_ref(), OTEL_SERVICE_NAME);
    let state_db = codex_core::init_state_db(&config).await;
    let environment_manager = Arc::new(
        EnvironmentManager::from_codex_home(
            config.codex_home.clone(),
            Some(ExecServerRuntimePaths::from_optional_paths(
                arg0_paths.codex_self_exe.clone(),
                arg0_paths.codex_linux_sandbox_exe.clone(),
            )?),
        )
        .await
        .map_err(std::io::Error::other)?,
    );

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(EnvFilter::from_default_env());
    let otel_logger_layer = otel.as_ref().and_then(|provider| provider.logger_layer());
    let otel_tracing_layer = otel.as_ref().and_then(|provider| provider.tracing_layer());

    let _ = tracing_subscriber::registry()
        .with(fmt_layer)
        .with(otel_logger_layer)
        .with(otel_tracing_layer)
        .try_init();

    // Set up channels.
    // 搭建两条通道：incoming 用有界通道（带背压，防止突发流量打爆内存），
    // outgoing 用无界通道（写出端绝不能阻塞处理逻辑，否则会造成死锁）。
    let (incoming_tx, mut incoming_rx) = mpsc::channel::<IncomingMessage>(CHANNEL_CAPACITY);
    let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<OutgoingMessage>();
    let installation_id = resolve_installation_id(&config.codex_home).await?;

    // Task: read from stdin, push to `incoming_tx`.
    // 任务一（生产者）：逐行读 stdin，按行反序列化为 JSON-RPC 消息推入 incoming 通道。
    let stdin_reader_handle = tokio::spawn({
        async move {
            let stdin = io::stdin();
            let reader = BufReader::new(stdin);
            let mut lines = reader.lines();

            // 逐行读取直到 EOF；读取出错时 `unwrap_or_default()` 把 Err 当成 None，
            // 等价于提前结束循环——读 stdin 失败和 EOF 都按"没有更多输入"处理。
            while let Some(line) = lines.next_line().await.unwrap_or_default() {
                match serde_json::from_str::<IncomingMessage>(&line) {
                    Ok(msg) => {
                        if incoming_tx.send(msg).await.is_err() {
                            // Receiver gone – nothing left to do.
                            // 接收端（处理任务）已退出，再发也没人收，直接收工。
                            break;
                        }
                    }
                    // 单条消息反序列化失败只记日志、不中断循环：一条坏消息不应拖垮
                    // 整个 server，继续读下一行。
                    Err(e) => error!("Failed to deserialize JSON-RPC message: {e}"),
                }
            }

            // 循环退出意味着 stdin 已 EOF；此处 `incoming_tx` 随闭包结束被 drop，
            // 从而触发关闭信号向下游传播（见 run_main 末尾说明）。
            debug!("stdin reader finished (EOF)");
        }
    });

    // Task: process incoming messages.
    // 任务二（核心消费者）：从 incoming 通道取消息，按 JSON-RPC 四类分派给
    // MessageProcessor 处理；处理过程中产生的回写消息经 outgoing 通道流向任务三。
    let processor_handle = tokio::spawn({
        let outgoing_message_sender = OutgoingMessageSender::new(outgoing_tx);
        let mut processor = MessageProcessor::new(
            outgoing_message_sender,
            arg0_paths,
            Arc::new(config),
            environment_manager,
            state_db,
            installation_id,
        )
        .await;
        async move {
            // `recv()` 返回 None 即 incoming 通道关闭（任务一已退出且无在途消息），
            // 循环自然结束——这是关闭链的第二环。
            while let Some(msg) = incoming_rx.recv().await {
                match msg {
                    JsonRpcMessage::Request(r) => processor.process_request(r).await,
                    JsonRpcMessage::Response(r) => processor.process_response(r).await,
                    JsonRpcMessage::Notification(n) => processor.process_notification(n).await,
                    JsonRpcMessage::Error(e) => processor.process_error(e),
                }
            }

            info!("processor task exited (channel closed)");
        }
    });

    // Task: write outgoing messages to stdout.
    // 任务三（最终消费者）：从 outgoing 通道取回写消息，序列化成 JSON 后逐行写 stdout。
    let stdout_writer_handle = tokio::spawn(async move {
        let mut stdout = io::stdout();
        while let Some(outgoing_message) = outgoing_rx.recv().await {
            // 内部 OutgoingMessage 先转成符合 JSON-RPC 线格式的 OutgoingJsonRpcMessage，
            // 再序列化（转换逻辑见 outgoing_message.rs 的 From 实现）。
            let msg: OutgoingJsonRpcMessage = outgoing_message.into();
            match serde_json::to_string(&msg) {
                Ok(json) => {
                    // 写入失败（如 stdout 管道被对端关闭）直接 break 退出任务，
                    // 没有重试——传输通道已断，重试无意义。
                    if let Err(e) = stdout.write_all(json.as_bytes()).await {
                        error!("Failed to write to stdout: {e}");
                        break;
                    }
                    // 每条消息以换行结尾：line-delimited JSON-RPC，对端按行切分。
                    if let Err(e) = stdout.write_all(b"\n").await {
                        error!("Failed to write newline to stdout: {e}");
                        break;
                    }
                }
                // 序列化失败只丢弃这条消息、不中断任务：单条坏消息不应拖垮写出流。
                Err(e) => error!("Failed to serialize JSON-RPC message: {e}"),
            }
        }

        info!("stdout writer exited (channel closed)");
    });

    // Wait for all tasks to finish.  The typical exit path is the stdin reader
    // hitting EOF which, once it drops `incoming_tx`, propagates shutdown to
    // the processor and then to the stdout task.
    // 阻塞等三个任务全部结束。典型关闭链是一条多米诺：stdin 读到 EOF → 任务一
    // 退出并 drop `incoming_tx` → 任务二的 `recv()` 返回 None 而退出并 drop
    // `outgoing_tx` → 任务三的 `recv()` 返回 None 而退出。`join!` 忽略各任务的
    // JoinError（用 `_` 丢弃），因为此处无可恢复的处理动作。
    let _ = tokio::join!(stdin_reader_handle, processor_handle, stdout_writer_handle);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_config::types::OtelExporterKind;
    use codex_core::config::ConfigBuilder;
    use pretty_assertions::assert_eq;
    use std::collections::HashMap;
    use tempfile::TempDir;

    #[test]
    fn mcp_server_defaults_analytics_to_enabled() {
        assert_eq!(DEFAULT_ANALYTICS_ENABLED, true);
    }

    #[tokio::test]
    async fn mcp_server_builds_otel_provider_with_logs_traces_and_metrics() -> anyhow::Result<()> {
        let codex_home = TempDir::new()?;
        let mut config = ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .build()
            .await?;
        let exporter = OtelExporterKind::OtlpGrpc {
            endpoint: "http://localhost:4317".to_string(),
            headers: HashMap::new(),
            tls: None,
        };
        config.otel.exporter = exporter.clone();
        config.otel.trace_exporter = exporter.clone();
        config.otel.metrics_exporter = exporter;
        config.analytics_enabled = None;

        let provider = codex_core::otel_init::build_provider(
            &config,
            "0.0.0-test",
            Some(OTEL_SERVICE_NAME),
            DEFAULT_ANALYTICS_ENABLED,
        )
        .map_err(|err| anyhow::anyhow!(err.to_string()))?
        .expect("otel provider");

        assert!(provider.logger.is_some(), "expected log exporter");
        assert!(
            provider.tracer_provider.is_some(),
            "expected trace exporter"
        );
        assert!(provider.metrics().is_some(), "expected metrics exporter");
        provider.shutdown();

        Ok(())
    }
}
