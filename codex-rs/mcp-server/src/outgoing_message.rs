//! 【文件职责】定义 MCP server 向客户端发送的「出站消息」抽象，并封装
//! server 主动发起 request 时的请求-响应回调配对机制。
//!
//! 【架构位置】
//!   层级：MCP server 出站方向的传输适配层
//!   上游：MessageProcessor、codex_tool_runner、各 approval 处理器（调用
//!         send_response / send_notification / send_request 等）
//!   下游：lib.rs 的 stdout 写出任务（通过 mpsc 通道收 OutgoingMessage 并序列化）
//!
//! 【数据流】
//!   业务代码调用 OutgoingMessageSender 的方法 → 构造内部 `OutgoingMessage`
//!   → 经 mpsc 通道送往写出任务 → 由 `From<OutgoingMessage>` 转成符合 JSON-RPC
//!   线格式的 `OutgoingJsonRpcMessage` → 序列化到 stdout。
//!
//! 【阅读建议】先看 `OutgoingMessageSender`：`send_request` 与
//!   `notify_client_response` 是一对（用 oneshot + RequestId→回调表实现
//!   "发出请求后异步等待客户端应答"）；其余 send_* 是单向发送。
//!   底部一组 `Outgoing*` struct 是线格式的中间表示，`From` 实现负责转换。

use std::collections::HashMap;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;

use codex_protocol::ThreadId;
use codex_protocol::protocol::Event;
use rmcp::model::CustomNotification;
use rmcp::model::CustomRequest;
use rmcp::model::ErrorData;
use rmcp::model::JsonRpcError;
use rmcp::model::JsonRpcMessage;
use rmcp::model::JsonRpcNotification;
use rmcp::model::JsonRpcRequest;
use rmcp::model::JsonRpcResponse;
use rmcp::model::JsonRpcVersion2_0;
use rmcp::model::RequestId;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tracing::warn;

// 最终写到 stdout 的 JSON-RPC 消息线格式类型；与 lib.rs 的 IncomingMessage 对应，
// 但请求/通知用 rmcp 的 Custom* 变体（透传任意 method + params，不限于 MCP 标准方法）。
pub(crate) type OutgoingJsonRpcMessage = JsonRpcMessage<CustomRequest, Value, CustomNotification>;

/// Sends messages to the client and manages request callbacks.
/// 向客户端发消息，并管理 server 主动发起请求时的回调配对。
///
/// [引用范围] 由 MessageProcessor 持有为 `Arc`，克隆后分发给各个异步会话任务
/// 与 approval 处理器共享，是出站方向的唯一收口。
pub(crate) struct OutgoingMessageSender {
    // server 自增的请求 ID 计数器。用原子量是因为多个会话任务可能并发发起请求，
    // 需保证 ID 全局唯一且无锁递增。
    next_request_id: AtomicI64,
    // 出站消息的发送端；接收端在 lib.rs 的 stdout 写出任务里。
    sender: mpsc::UnboundedSender<OutgoingMessage>,
    // 请求 ID → 等待客户端应答的 oneshot 发送端。发出请求时登记，收到对应
    // 响应时取出并唤醒等待方，实现"异步 RPC 调用"语义。
    request_id_to_callback: Mutex<HashMap<RequestId, oneshot::Sender<Value>>>,
}

impl OutgoingMessageSender {
    pub(crate) fn new(sender: mpsc::UnboundedSender<OutgoingMessage>) -> Self {
        Self {
            next_request_id: AtomicI64::new(0),
            sender,
            request_id_to_callback: Mutex::new(HashMap::new()),
        }
    }

    /// 由 server 主动向客户端发起一个 JSON-RPC 请求，并返回一个 oneshot 接收端，
    /// 调用方 `await` 它即可拿到客户端的应答（典型用途：审批 elicitation）。
    ///
    /// @returns - oneshot 接收端，被对应响应到达时唤醒；若 server 在收到响应前关闭，
    ///            发送端被 drop，接收端会得到 `RecvError`。
    ///
    /// 副作用：分配一个新请求 ID，在回调表中登记，并把请求推入出站通道。
    pub(crate) async fn send_request(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> oneshot::Receiver<Value> {
        // 原子自增取一个唯一请求 ID。`Relaxed` 足够：这里只要求计数器单调唯一，
        // 不需要与其他内存操作建立同步顺序。
        let id = RequestId::Number(self.next_request_id.fetch_add(1, Ordering::Relaxed));
        let outgoing_message_id = id.clone();
        let (tx_approve, rx_approve) = oneshot::channel();
        {
            // 先登记回调再发请求：保证响应即便瞬间返回，也总能在表里找到 oneshot 发送端，
            // 不会出现"响应先到、回调还没登记"的竞态。锁作用域用花括号收紧，尽早释放。
            let mut request_id_to_callback = self.request_id_to_callback.lock().await;
            request_id_to_callback.insert(id, tx_approve);
        }

        let outgoing_message = OutgoingMessage::Request(OutgoingRequest {
            id: outgoing_message_id,
            method: method.to_string(),
            params,
        });
        let _ = self.sender.send(outgoing_message);
        rx_approve
    }

    /// 当客户端对 server 此前发出的请求作出响应时调用：按 ID 找回 `send_request`
    /// 登记的 oneshot 发送端，把结果送回去唤醒等待方。
    /// 由 MessageProcessor 处理 incoming Response 时驱动（见 process_response）。
    pub(crate) async fn notify_client_response(&self, id: RequestId, result: Value) {
        // 取出并从表中移除该 ID 的回调（一个请求只应被响应一次）。
        let entry = {
            let mut request_id_to_callback = self.request_id_to_callback.lock().await;
            request_id_to_callback.remove_entry(&id)
        };

        match entry {
            Some((id, sender)) => {
                if let Err(err) = sender.send(result) {
                    warn!("could not notify callback for {id:?} due to: {err:?}");
                }
            }
            None => {
                warn!("could not find callback for {id:?}");
            }
        }
    }

    /// 对客户端的某个请求回送成功响应。泛型 `T: Serialize` 由调用方传入，
    /// 序列化失败时自动转而回送一个 JSON-RPC error（而非静默丢弃），
    /// 保证客户端的每个请求都有一条回复。
    pub(crate) async fn send_response<T: Serialize>(&self, id: RequestId, response: T) {
        let result = match serde_json::to_value(response) {
            Ok(result) => result,
            Err(err) => {
                // 序列化失败：降级为给同一请求 ID 回送 internal_error，避免请求悬空。
                self.send_error(
                    id,
                    ErrorData::internal_error(format!("failed to serialize response: {err}"), None),
                )
                .await;
                return;
            }
        };

        let outgoing_message = OutgoingMessage::Response(OutgoingResponse { id, result });
        let _ = self.sender.send(outgoing_message);
    }

    /// This is used with the MCP server, but not the more general JSON-RPC app
    /// server. Prefer [`OutgoingMessageSender::send_server_notification`] where
    /// possible.
    /// 把一个 Codex `Event` 包装成 `codex/event` 通知推给客户端。
    /// 仅用于 MCP server 这条路径；通用 JSON-RPC app server 另有
    /// `send_server_notification`，能用那个就别用这个。
    ///
    /// @param event - 要转发的 Codex 事件（会话配置、agent 消息、审批请求等）
    /// @param meta  - 可选的 MCP 专属元数据（requestId / threadId），用于多路复用时
    ///                把通知关联回原始请求与所属线程
    pub(crate) async fn send_event_as_notification(
        &self,
        event: &Event,
        meta: Option<OutgoingNotificationMeta>,
    ) {
        // Event 是 Codex 内部类型，结构稳定，序列化失败属于不可恢复的程序错误，
        // 故用 expect 直接 panic（已用 #[expect] 显式豁免 clippy 的禁用规则）。
        #[expect(clippy::expect_used)]
        let event_json = serde_json::to_value(event).expect("Event must serialize");

        // 优先把 event 连同 meta 一起按 OutgoingNotificationParams 结构序列化；
        // 万一外层包装失败（meta 异常等），降级为只发裸 event_json，保证通知仍能送出
        // 而不是整条丢弃——宁可少 meta 也不丢事件。
        let params = if let Ok(params) = serde_json::to_value(OutgoingNotificationParams {
            meta,
            event: event_json.clone(),
        }) {
            params
        } else {
            warn!("Failed to serialize event as OutgoingNotificationParams");
            event_json
        };

        self.send_notification(OutgoingNotification {
            method: "codex/event".to_string(),
            params: Some(params.clone()),
        })
        .await;
    }

    pub(crate) async fn send_notification(&self, notification: OutgoingNotification) {
        let outgoing_message = OutgoingMessage::Notification(notification);
        let _ = self.sender.send(outgoing_message);
    }

    pub(crate) async fn send_error(&self, id: RequestId, error: ErrorData) {
        let outgoing_message = OutgoingMessage::Error(OutgoingError { id, error });
        let _ = self.sender.send(outgoing_message);
    }
}

/// Outgoing message from the server to the client.
/// server 发往客户端的出站消息（内部表示）。四个变体对应 JSON-RPC 的四种消息。
/// 之所以不直接用 rmcp 的线格式类型，是为了让业务侧用更简洁的字段构造，
/// 再由下面的 `From` 统一翻译成线格式。
pub(crate) enum OutgoingMessage {
    Request(OutgoingRequest),
    Notification(OutgoingNotification),
    Response(OutgoingResponse),
    Error(OutgoingError),
}

// 把内部 `OutgoingMessage` 翻译成 JSON-RPC 线格式 `OutgoingJsonRpcMessage`。
// 关键点：Request/Notification 被包成 rmcp 的 Custom* 类型，serde 会把
// method/params 展平（flatten）到顶层，而不是嵌在 "request"/"notification" 字段下
// （对应测试里断言 `obj.get("request").is_none()`）。
impl From<OutgoingMessage> for OutgoingJsonRpcMessage {
    fn from(val: OutgoingMessage) -> Self {
        use OutgoingMessage::*;
        match val {
            Request(OutgoingRequest { id, method, params }) => {
                JsonRpcMessage::Request(JsonRpcRequest {
                    jsonrpc: JsonRpcVersion2_0,
                    id,
                    request: CustomRequest::new(method, params),
                })
            }
            Notification(OutgoingNotification { method, params }) => {
                JsonRpcMessage::Notification(JsonRpcNotification {
                    jsonrpc: JsonRpcVersion2_0,
                    notification: CustomNotification::new(method, params),
                })
            }
            Response(OutgoingResponse { id, result }) => {
                JsonRpcMessage::Response(JsonRpcResponse {
                    jsonrpc: JsonRpcVersion2_0,
                    id,
                    result,
                })
            }
            Error(OutgoingError { id, error }) => JsonRpcMessage::Error(JsonRpcError {
                jsonrpc: JsonRpcVersion2_0,
                id: Some(id),
                error,
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct OutgoingRequest {
    pub id: RequestId,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct OutgoingNotification {
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct OutgoingNotificationParams {
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<OutgoingNotificationMeta>,

    #[serde(flatten)]
    pub event: serde_json::Value,
}

// Additional mcp-specific data to be added to a [`codex_protocol::protocol::Event`] as notification.params._meta
// MCP Spec: https://modelcontextprotocol.io/specification/2025-06-18/basic#meta
// Typescript Schema: https://github.com/modelcontextprotocol/modelcontextprotocol/blob/0695a497eb50a804fc0e88c18a93a21a675d6b3e/schema/2025-06-18/schema.ts
// 附加在通知 `params._meta` 上的 MCP 专属元数据：按规范放在 `_meta` 字段（见上方
// 链接），用于把无状态的事件通知关联回它的发起请求和所属线程。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OutgoingNotificationMeta {
    // 触发此事件的原始 tools/call 请求 ID，便于客户端做请求-事件关联。
    pub request_id: Option<RequestId>,

    /// Because multiple threads may be multiplexed over a single MCP connection,
    /// include the `threadId` in the notification meta.
    /// 因为多个 Codex 线程可能复用同一条 MCP 连接，必须在通知元数据里带上
    /// `threadId`，客户端才能区分某条事件属于哪个会话线程。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<ThreadId>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct OutgoingResponse {
    pub id: RequestId,
    pub result: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct OutgoingError {
    pub error: ErrorData,
    pub id: RequestId,
}

#[cfg(test)]
mod tests {

    use anyhow::Result;
    use codex_protocol::ThreadId;
    use codex_protocol::models::PermissionProfile;
    use codex_protocol::openai_models::ReasoningEffort;
    use codex_protocol::protocol::AskForApproval;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::SessionConfiguredEvent;
    use codex_utils_absolute_path::test_support::PathBufExt;
    use codex_utils_absolute_path::test_support::test_path_buf;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tempfile::NamedTempFile;

    use super::*;

    #[test]
    fn outgoing_request_serializes_as_jsonrpc_request() {
        let msg: OutgoingJsonRpcMessage = OutgoingMessage::Request(OutgoingRequest {
            id: RequestId::Number(1),
            method: "elicitation/create".to_string(),
            params: Some(json!({ "k": "v" })),
        })
        .into();

        let value = serde_json::to_value(msg).expect("message should serialize");
        let obj = value.as_object().expect("json object");

        assert_eq!(obj.get("jsonrpc"), Some(&json!("2.0")));
        assert_eq!(obj.get("id"), Some(&json!(1)));
        assert_eq!(obj.get("method"), Some(&json!("elicitation/create")));
        assert_eq!(obj.get("params"), Some(&json!({ "k": "v" })));
        assert!(
            obj.get("request").is_none(),
            "rmcp request must flatten to JSON-RPC method/params"
        );
    }

    #[test]
    fn outgoing_notification_serializes_as_jsonrpc_notification() {
        let msg: OutgoingJsonRpcMessage = OutgoingMessage::Notification(OutgoingNotification {
            method: "notifications/initialized".to_string(),
            params: None,
        })
        .into();

        let value = serde_json::to_value(msg).expect("message should serialize");
        let obj = value.as_object().expect("json object");

        assert_eq!(obj.get("jsonrpc"), Some(&json!("2.0")));
        assert_eq!(obj.get("method"), Some(&json!("notifications/initialized")));
        assert_eq!(obj.get("params"), Some(&serde_json::Value::Null));
        assert!(
            obj.get("notification").is_none(),
            "rmcp notification must flatten to JSON-RPC method/params"
        );
    }

    #[tokio::test]
    async fn test_send_event_as_notification() -> Result<()> {
        let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<OutgoingMessage>();
        let outgoing_message_sender = OutgoingMessageSender::new(outgoing_tx);

        let thread_id = ThreadId::new();
        let rollout_file = NamedTempFile::new()?;
        let event = Event {
            id: "1".to_string(),
            msg: EventMsg::SessionConfigured(SessionConfiguredEvent {
                session_id: codex_protocol::SessionId::new(),
                thread_id,
                forked_from_id: None,
                thread_source: None,
                thread_name: None,
                model: "gpt-4o".to_string(),
                model_provider_id: "test-provider".to_string(),
                service_tier: None,
                approval_policy: AskForApproval::Never,
                approvals_reviewer: codex_protocol::config_types::ApprovalsReviewer::User,
                permission_profile: PermissionProfile::read_only(),
                active_permission_profile: None,
                cwd: test_path_buf("/home/user/project").abs(),
                reasoning_effort: Some(ReasoningEffort::default()),
                initial_messages: None,
                network_proxy: None,
                rollout_path: Some(rollout_file.path().to_path_buf()),
            }),
        };

        outgoing_message_sender
            .send_event_as_notification(&event, /*meta*/ None)
            .await;

        let result = outgoing_rx.recv().await.unwrap();
        let OutgoingMessage::Notification(OutgoingNotification { method, params }) = result else {
            panic!("expected Notification for first message");
        };
        assert_eq!(method, "codex/event");

        let Ok(expected_params) = serde_json::to_value(&event) else {
            panic!("Event must serialize");
        };
        assert_eq!(params, Some(expected_params));
        Ok(())
    }

    #[tokio::test]
    async fn test_send_event_as_notification_with_meta() -> Result<()> {
        let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<OutgoingMessage>();
        let outgoing_message_sender = OutgoingMessageSender::new(outgoing_tx);

        let thread_id = ThreadId::new();
        let rollout_file = NamedTempFile::new()?;
        let session_configured_event = SessionConfiguredEvent {
            session_id: codex_protocol::SessionId::new(),
            thread_id,
            forked_from_id: None,
            thread_source: None,
            thread_name: None,
            model: "gpt-4o".to_string(),
            model_provider_id: "test-provider".to_string(),
            service_tier: None,
            approval_policy: AskForApproval::Never,
            approvals_reviewer: codex_protocol::config_types::ApprovalsReviewer::User,
            permission_profile: PermissionProfile::read_only(),
            active_permission_profile: None,
            cwd: test_path_buf("/home/user/project").abs(),
            reasoning_effort: Some(ReasoningEffort::default()),
            initial_messages: None,
            network_proxy: None,
            rollout_path: Some(rollout_file.path().to_path_buf()),
        };
        let event = Event {
            id: "1".to_string(),
            msg: EventMsg::SessionConfigured(session_configured_event.clone()),
        };
        let meta = OutgoingNotificationMeta {
            request_id: Some(RequestId::String("123".into())),
            thread_id: None,
        };

        outgoing_message_sender
            .send_event_as_notification(&event, Some(meta))
            .await;

        let result = outgoing_rx.recv().await.unwrap();
        let OutgoingMessage::Notification(OutgoingNotification { method, params }) = result else {
            panic!("expected Notification for first message");
        };
        assert_eq!(method, "codex/event");
        let expected_params = json!({
            "_meta": {
                "requestId": "123",
            },
            "id": "1",
            "msg": {
                "type": "session_configured",
                "session_id": session_configured_event.session_id,
                "thread_id": session_configured_event.thread_id,
                "model": "gpt-4o",
                "model_provider_id": "test-provider",
                "approval_policy": "never",
                "approvals_reviewer": "user",
                "permission_profile": session_configured_event.permission_profile,
                "cwd": test_path_buf("/home/user/project"),
                "reasoning_effort": session_configured_event.reasoning_effort,
                "rollout_path": rollout_file.path().to_path_buf(),
            }
        });
        assert_eq!(params.unwrap(), expected_params);
        Ok(())
    }

    #[tokio::test]
    async fn test_send_event_as_notification_with_meta_and_thread_id() -> Result<()> {
        let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<OutgoingMessage>();
        let outgoing_message_sender = OutgoingMessageSender::new(outgoing_tx);

        let thread_id = ThreadId::new();
        let rollout_file = NamedTempFile::new()?;
        let session_configured_event = SessionConfiguredEvent {
            session_id: codex_protocol::SessionId::new(),
            thread_id,
            forked_from_id: None,
            thread_source: None,
            thread_name: None,
            model: "gpt-4o".to_string(),
            model_provider_id: "test-provider".to_string(),
            service_tier: None,
            approval_policy: AskForApproval::Never,
            approvals_reviewer: codex_protocol::config_types::ApprovalsReviewer::User,
            permission_profile: PermissionProfile::read_only(),
            active_permission_profile: None,
            cwd: test_path_buf("/home/user/project").abs(),
            reasoning_effort: Some(ReasoningEffort::default()),
            initial_messages: None,
            network_proxy: None,
            rollout_path: Some(rollout_file.path().to_path_buf()),
        };
        let event = Event {
            id: "1".to_string(),
            msg: EventMsg::SessionConfigured(session_configured_event.clone()),
        };
        let meta = OutgoingNotificationMeta {
            request_id: Some(RequestId::String("123".into())),
            thread_id: Some(thread_id),
        };

        outgoing_message_sender
            .send_event_as_notification(&event, Some(meta))
            .await;

        let result = outgoing_rx.recv().await.unwrap();
        let OutgoingMessage::Notification(OutgoingNotification { method, params }) = result else {
            panic!("expected Notification for first message");
        };
        assert_eq!(method, "codex/event");
        let expected_params = json!({
            "_meta": {
                "requestId": "123",
                "threadId": thread_id.to_string(),
            },
            "id": "1",
            "msg": {
                "type": "session_configured",
                "session_id": session_configured_event.session_id,
                "thread_id": session_configured_event.thread_id,
                "model": "gpt-4o",
                "model_provider_id": "test-provider",
                "approval_policy": "never",
                "approvals_reviewer": "user",
                "permission_profile": session_configured_event.permission_profile,
                "cwd": test_path_buf("/home/user/project"),
                "reasoning_effort": session_configured_event.reasoning_effort,
                "rollout_path": rollout_file.path().to_path_buf(),
            }
        });
        assert_eq!(params.unwrap(), expected_params);
        Ok(())
    }
}
