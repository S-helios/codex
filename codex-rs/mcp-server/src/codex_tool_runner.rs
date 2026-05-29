//! Asynchronous worker that executes a **Codex** tool-call inside a spawned
//! Tokio task. Separated from `message_processor.rs` to keep that file small
//! and to make future feature-growth easier to manage.
//!
//! 【文件职责】在 message_processor spawn 出来的后台 task 里，驱动一个完整的
//! Codex 会话：投递用户输入、循环消费会话事件并转发为 MCP 通知，
//! 在会话完成/出错/需审批时回送恰当的 `tools/call` 响应。
//!
//! 【架构位置】
//!   层级：MCP server 会话执行层
//!   上游：message_processor.rs 的 handle_tool_call_codex / *_reply（spawn 调用）
//!   下游：codex-core 的 CodexThread（next_event/submit）、outgoing_message（回写）、
//!         exec_approval / patch_approval（审批 round-trip）
//!
//! 【数据流】
//!   开/取会话 → 投递初始或续聊 prompt → run_codex_tool_session_inner 的事件循环：
//!   每条 Event 先无条件转成 `codex/event` 通知发给客户端，再按事件类型决定是否
//!   要回送终态响应（TurnComplete/Error）或发起审批（Exec/ApplyPatch approval）。
//!
//! 【阅读建议】两个入口 `run_codex_tool_session`（开新会话）与
//!   `run_codex_tool_session_reply`（续聊）最终都汇入
//!   `run_codex_tool_session_inner` 的事件循环——那里是本文件的核心，重点看
//!   循环里对各 EventMsg 变体的分流（哪些终结循环、哪些触发审批、哪些只透传）。

use std::collections::HashMap;
use std::sync::Arc;

use crate::exec_approval::handle_exec_approval_request;
use crate::outgoing_message::OutgoingMessageSender;
use crate::outgoing_message::OutgoingNotificationMeta;
use crate::patch_approval::handle_patch_approval_request;
use codex_core::CodexThread;
use codex_core::NewThread;
use codex_core::ThreadManager;
use codex_core::config::Config as CodexConfig;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AgentMessageEvent;
use codex_protocol::protocol::ApplyPatchApprovalRequestEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecApprovalRequestEvent;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::Submission;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::user_input::UserInput;
use rmcp::model::CallToolResult;
use rmcp::model::Content;
use rmcp::model::RequestId;
use serde_json::json;
use tokio::sync::Mutex;

/// To adhere to MCP `tools/call` response format, include the Codex
/// `threadId` in the `structured_content` field of the response.
/// Some MCP clients ignore `content` when `structuredContent` is present, so
/// mirror the text there as well.
/// 构造一条携带 `threadId` 的 `tools/call` 结果：把 threadId 放进
/// `structured_content`（与 codex_tool_config 的输出 schema 对齐），同时把文本
/// 同步镜像到 `content`——因为部分 MCP 客户端在有 structuredContent 时会忽略
/// content，两边都写才能保证文本一定被客户端读到。
pub(crate) fn create_call_tool_result_with_thread_id(
    thread_id: ThreadId,
    text: String,
    is_error: Option<bool>,
) -> CallToolResult {
    let content_text = text;
    let content = vec![Content::text(content_text.clone())];
    let structured_content = json!({
        "threadId": thread_id,
        "content": content_text,
    });
    let mut result = CallToolResult::success(content);
    result.is_error = is_error;
    result.structured_content = Some(structured_content);
    result
}

/// Run a complete Codex session and stream events back to the client.
///
/// On completion (success or error) the function sends the appropriate
/// `tools/call` response so the LLM can continue the conversation.
/// 开启一个全新 Codex 会话并把事件流式回送客户端（对应 `codex` 工具）。
/// 无论成功还是失败，最终都会回送一条 `tools/call` 响应，使调用方 LLM 能继续对话。
///
/// 副作用：在 ThreadManager 里新建会话；向 `running_requests_id_to_codex_uuid`
/// 登记 (请求 ID → thread_id)；通过 outgoing 通道发送大量事件通知。
pub async fn run_codex_tool_session(
    id: RequestId,
    initial_prompt: String,
    config: CodexConfig,
    outgoing: Arc<OutgoingMessageSender>,
    thread_manager: Arc<ThreadManager>,
    running_requests_id_to_codex_uuid: Arc<Mutex<HashMap<RequestId, ThreadId>>>,
) {
    let NewThread {
        thread_id,
        thread,
        session_configured,
    } = match thread_manager.start_thread(config.clone()).await {
        Ok(res) => res,
        Err(e) => {
            // 会话都没起来，此时还没 thread_id，只能回送普通 error 结果（不带 threadId）。
            let result = CallToolResult::error(vec![Content::text(format!(
                "Failed to start Codex session: {e}"
            ))]);
            outgoing.send_response(id.clone(), result).await;
            return;
        }
    };

    let session_configured_event = Event {
        // Use a fake id value for now.
        id: "".to_string(),
        msg: EventMsg::SessionConfigured(session_configured.clone()),
    };
    outgoing
        .send_event_as_notification(
            &session_configured_event,
            Some(OutgoingNotificationMeta {
                request_id: Some(id.clone()),
                thread_id: Some(thread_id),
            }),
        )
        .await;

    // Use the original MCP request ID as the `sub_id` for the Codex submission so that
    // any events emitted for this tool-call can be correlated with the
    // originating `tools/call` request.
    // 把原始 MCP 请求 ID 复用为 Codex 提交的 `sub_id`：这样会话为本次工具调用产出的
    // 任何事件，都能反向关联到发起它的那条 `tools/call` 请求。
    let sub_id = id.to_string();
    // 登记 (请求 ID → thread_id)，使后续 cancel 通知能据请求 ID 找到要中断的会话。
    running_requests_id_to_codex_uuid
        .lock()
        .await
        .insert(id.clone(), thread_id);
    let submission = Submission {
        id: sub_id.clone(),
        op: Op::UserInput {
            environments: None,
            items: vec![UserInput::Text {
                text: initial_prompt.clone(),
                // MCP tool prompts are plain text with no UI element ranges.
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        },
        trace: None,
    };

    if let Err(e) = thread.submit_with_id(submission).await {
        tracing::error!("Failed to submit initial prompt: {e}");
        let result = create_call_tool_result_with_thread_id(
            thread_id,
            format!("Failed to submit initial prompt: {e}"),
            Some(true),
        );
        outgoing.send_response(id.clone(), result).await;
        // unregister the id so we don't keep it in the map
        running_requests_id_to_codex_uuid.lock().await.remove(&id);
        return;
    }

    run_codex_tool_session_inner(
        thread_id,
        thread,
        outgoing,
        id,
        running_requests_id_to_codex_uuid,
    )
    .await;
}

/// 向一个已存在的 Codex 会话投递下一条用户输入并继续流式回送事件
/// （对应 `codex-reply` 工具）。与 `run_codex_tool_session` 的区别在于不新建会话，
/// 直接复用传入的 `thread`，其余事件循环逻辑共用 `run_codex_tool_session_inner`。
pub async fn run_codex_tool_session_reply(
    thread_id: ThreadId,
    thread: Arc<CodexThread>,
    outgoing: Arc<OutgoingMessageSender>,
    request_id: RequestId,
    prompt: String,
    running_requests_id_to_codex_uuid: Arc<Mutex<HashMap<RequestId, ThreadId>>>,
) {
    running_requests_id_to_codex_uuid
        .lock()
        .await
        .insert(request_id.clone(), thread_id);
    if let Err(e) = thread
        .submit(Op::UserInput {
            environments: None,
            items: vec![UserInput::Text {
                text: prompt,
                // MCP tool prompts are plain text with no UI element ranges.
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await
    {
        tracing::error!("Failed to submit user input: {e}");
        let result = create_call_tool_result_with_thread_id(
            thread_id,
            format!("Failed to submit user input: {e}"),
            Some(true),
        );
        outgoing.send_response(request_id.clone(), result).await;
        // unregister the id so we don't keep it in the map
        running_requests_id_to_codex_uuid
            .lock()
            .await
            .remove(&request_id);
        return;
    }

    run_codex_tool_session_inner(
        thread_id,
        thread,
        outgoing,
        request_id,
        running_requests_id_to_codex_uuid,
    )
    .await;
}

/// 会话事件循环（两个入口的共用内核）：持续拉取会话事件，先无条件转发为通知，
/// 再按事件类型决定流程走向。在收到终态事件（TurnComplete/Error）或拉取出错时
/// 回送 `tools/call` 终响应并跳出循环；遇到审批请求则发起审批 round-trip 后继续。
async fn run_codex_tool_session_inner(
    thread_id: ThreadId,
    thread: Arc<CodexThread>,
    outgoing: Arc<OutgoingMessageSender>,
    request_id: RequestId,
    running_requests_id_to_codex_uuid: Arc<Mutex<HashMap<RequestId, ThreadId>>>,
) {
    let request_id_str = request_id.to_string();

    // Stream events until the task needs to pause for user interaction or
    // completes.
    // 流式消费事件，直到会话需要暂停等用户交互（审批）或彻底完成。
    loop {
        match thread.next_event().await {
            Ok(event) => {
                // 不论后续如何分流，每条事件都先原样转成 `codex/event` 通知发给客户端，
                // 让客户端能实时看到会话进展（下面的 match 只决定是否还要额外动作）。
                outgoing
                    .send_event_as_notification(
                        &event,
                        Some(OutgoingNotificationMeta {
                            request_id: Some(request_id.clone()),
                            thread_id: Some(thread_id),
                        }),
                    )
                    .await;

                match event.msg {
                    EventMsg::ExecApprovalRequest(ev) => {
                        let approval_id = ev.effective_approval_id();
                        let ExecApprovalRequestEvent {
                            turn_id: _,
                            started_at_ms: _,
                            command,
                            cwd,
                            call_id,
                            approval_id: _,
                            reason: _,
                            proposed_execpolicy_amendment: _,
                            proposed_network_policy_amendments: _,
                            parsed_cmd,
                            network_approval_context: _,
                            additional_permissions: _,
                            available_decisions: _,
                        } = ev;
                        // 命令执行审批：向客户端发起审批请求并等待决定，决定回传给会话后
                        // `continue` 继续循环——审批不终结会话，会话据决定决定是否执行命令。
                        handle_exec_approval_request(
                            command,
                            cwd.to_path_buf(),
                            outgoing.clone(),
                            thread.clone(),
                            request_id.clone(),
                            request_id_str.clone(),
                            event.id.clone(),
                            call_id,
                            approval_id,
                            parsed_cmd,
                            thread_id,
                        )
                        .await;
                        continue;
                    }
                    EventMsg::PlanDelta(_) => {
                        continue;
                    }
                    EventMsg::Error(err_event) => {
                        // Always respond in tools/call's expected shape, and include conversationId so the client can resume.
                        // 错误是终态：回送带 threadId 的 error 结果（is_error=true）并跳出循环。
                        // 带上 threadId 是为了让客户端仍能基于该线程发起 codex-reply 续聊/重试。
                        let result = create_call_tool_result_with_thread_id(
                            thread_id,
                            err_event.message,
                            Some(true),
                        );
                        outgoing.send_response(request_id.clone(), result).await;
                        break;
                    }
                    EventMsg::Warning(_)
                    | EventMsg::GuardianWarning(_)
                    | EventMsg::ModelVerification(_) => {
                        continue;
                    }
                    EventMsg::GuardianAssessment(_) => {
                        continue;
                    }
                    EventMsg::ElicitationRequest(_) => {
                        // TODO: forward elicitation requests to the client?
                        continue;
                    }
                    EventMsg::ApplyPatchApprovalRequest(ApplyPatchApprovalRequestEvent {
                        call_id,
                        turn_id: _,
                        started_at_ms: _,
                        reason,
                        grant_root,
                        changes,
                    }) => {
                        handle_patch_approval_request(
                            call_id,
                            reason,
                            grant_root,
                            changes,
                            outgoing.clone(),
                            thread.clone(),
                            request_id.clone(),
                            request_id_str.clone(),
                            event.id.clone(),
                            thread_id,
                        )
                        .await;
                        continue;
                    }
                    EventMsg::TurnComplete(TurnCompleteEvent {
                        last_agent_message, ..
                    }) => {
                        // 一轮正常完成（成功终态）：把最后一条 agent 消息作为结果文本回送
                        // （没有则用空串），随后注销请求 ID 并跳出循环。
                        let text = match last_agent_message {
                            Some(msg) => msg,
                            None => "".to_string(),
                        };
                        let result = create_call_tool_result_with_thread_id(
                            thread_id, text, /*is_error*/ None,
                        );
                        outgoing.send_response(request_id.clone(), result).await;
                        // unregister the id so we don't keep it in the map
                        // 会话本轮收尾，从在途映射表移除该 ID，避免后续误中断或表无限增长。
                        running_requests_id_to_codex_uuid
                            .lock()
                            .await
                            .remove(&request_id);
                        break;
                    }
                    EventMsg::SessionConfigured(_) => {
                        tracing::error!("unexpected SessionConfigured event");
                    }
                    EventMsg::ThreadGoalUpdated(_) => {
                        // Ignore thread goal metadata updates in MCP tool runner.
                    }
                    EventMsg::McpStartupUpdate(_) | EventMsg::McpStartupComplete(_) => {
                        // Ignored in MCP tool runner.
                    }
                    EventMsg::AgentMessage(AgentMessageEvent { .. }) => {
                        // TODO: think how we want to support this in the MCP
                    }
                    EventMsg::AgentReasoningRawContent(_)
                    | EventMsg::TurnStarted(_)
                    | EventMsg::ThreadSettingsApplied(_)
                    | EventMsg::TokenCount(_)
                    | EventMsg::AgentReasoning(_)
                    | EventMsg::AgentReasoningSectionBreak(_)
                    | EventMsg::McpToolCallBegin(_)
                    | EventMsg::McpToolCallEnd(_)
                    | EventMsg::RealtimeConversationListVoicesResponse(_)
                    | EventMsg::ExecCommandBegin(_)
                    | EventMsg::TerminalInteraction(_)
                    | EventMsg::ExecCommandOutputDelta(_)
                    | EventMsg::ExecCommandEnd(_)
                    | EventMsg::StreamError(_)
                    | EventMsg::PatchApplyBegin(_)
                    | EventMsg::PatchApplyUpdated(_)
                    | EventMsg::PatchApplyEnd(_)
                    | EventMsg::TurnDiff(_)
                    | EventMsg::WebSearchBegin(_)
                    | EventMsg::WebSearchEnd(_)
                    | EventMsg::PlanUpdate(_)
                    | EventMsg::TurnAborted(_)
                    | EventMsg::UserMessage(_)
                    | EventMsg::ShutdownComplete
                    | EventMsg::ImageGenerationBegin(_)
                    | EventMsg::ImageGenerationEnd(_)
                    | EventMsg::ViewImageToolCall(_)
                    | EventMsg::RawResponseItem(_)
                    | EventMsg::EnteredReviewMode(_)
                    | EventMsg::ItemStarted(_)
                    | EventMsg::ItemCompleted(_)
                    | EventMsg::HookStarted(_)
                    | EventMsg::HookCompleted(_)
                    | EventMsg::AgentMessageContentDelta(_)
                    | EventMsg::ReasoningContentDelta(_)
                    | EventMsg::ReasoningRawContentDelta(_)
                    | EventMsg::ExitedReviewMode(_)
                    | EventMsg::RequestUserInput(_)
                    | EventMsg::RequestPermissions(_)
                    | EventMsg::DynamicToolCallRequest(_)
                    | EventMsg::DynamicToolCallResponse(_)
                    | EventMsg::ContextCompacted(_)
                    | EventMsg::ModelReroute(_)
                    | EventMsg::ThreadRolledBack(_)
                    | EventMsg::CollabAgentSpawnBegin(_)
                    | EventMsg::CollabAgentSpawnEnd(_)
                    | EventMsg::CollabAgentInteractionBegin(_)
                    | EventMsg::CollabAgentInteractionEnd(_)
                    | EventMsg::CollabWaitingBegin(_)
                    | EventMsg::CollabWaitingEnd(_)
                    | EventMsg::CollabCloseBegin(_)
                    | EventMsg::CollabCloseEnd(_)
                    | EventMsg::CollabResumeBegin(_)
                    | EventMsg::CollabResumeEnd(_)
                    | EventMsg::RealtimeConversationStarted(_)
                    | EventMsg::RealtimeConversationSdp(_)
                    | EventMsg::RealtimeConversationRealtime(_)
                    | EventMsg::RealtimeConversationClosed(_)
                    | EventMsg::DeprecationNotice(_) => {
                        // For now, we do not do anything extra for these
                        // events. Note that
                        // send(codex_event_to_notification(&event)) above has
                        // already dispatched these events as notifications,
                        // though we may want to do give different treatment to
                        // individual events in the future.
                        // 这一大组事件目前无需额外处理：它们已在循环开头被无条件转成
                        // 通知发给客户端了，这里只是显式穷举（match 不允许遗漏变体），
                        // 以便将来按需为个别事件加特殊处理时一目了然。
                    }
                }
            }
            Err(e) => {
                // 从会话拉取事件本身失败（运行时错误）：作为终态回送 error 结果并退出循环。
                let result = create_call_tool_result_with_thread_id(
                    thread_id,
                    format!("Codex runtime error: {e}"),
                    Some(true),
                );
                outgoing.send_response(request_id.clone(), result).await;
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn call_tool_result_includes_thread_id_in_structured_content() {
        let thread_id = ThreadId::new();
        let result = create_call_tool_result_with_thread_id(
            thread_id,
            "done".to_string(),
            /*is_error*/ None,
        );
        assert_eq!(
            result.structured_content,
            Some(json!({
                "threadId": thread_id,
                "content": "done",
            }))
        );
    }
}
