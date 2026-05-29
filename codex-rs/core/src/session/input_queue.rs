//! 【文件职责】统一管理「回合待处理输入」与「子 Agent 邮箱」两类异步输入源，
//!   把它们汇流成下一次模型请求的输入。这是「追问 / 打断」机制的输入侧枢纽。
//!
//! 【架构位置】
//!   层级：会话 / 回合状态层
//!   上游：session/session.rs（[`Session`] 持有唯一的 [`InputQueue`] 字段）；
//!         session/mod.rs（用户中途 steer 新提问时写入 pending_input）；
//!         session/handlers.rs（子 Agent 投递邮件时调 `enqueue_mailbox_communication`）；
//!         session/inject.rs（注入 ResponseItem 形式的 pending_input）。
//!   下游：session/turn.rs 的 agent 主循环——每轮开头 `get_pending_input` 取走输入并入
//!         队，循环末尾 `has_pending_input` 决定是否还要再追问一轮。
//!
//! 【两类输入源的分工】
//!   · 「回合待处理输入」（pending_input）：存放在 [`TurnState::pending_input`] 里，
//!     是回合内的局部状态。来源是用户在模型运行时通过 UI 追加的提问，或注入的
//!     ResponseItem。生命周期跟随单个回合，由 [`TurnInputQueue`] 承载。
//!   · 「子 Agent 邮箱」（mailbox）：是会话级共享状态，存放在 [`InputQueue`] 自身。
//!     多 Agent v2 场景下，子 Agent 之间互发的 [`InterAgentCommunication`] 先在此排队，
//!     再按 [`MailboxDeliveryPhase`] 决定并入本回合还是留给下回合。
//!
//! 【演进背景】本模块取代了旧的 `Session::get_pending_input` 与挂在 `TurnState` 上的
//!   push/take_pending_input 散落逻辑，把「两类输入源如何汇流」收敛到一处。可与
//!   learn_docs/10（追问 / 打断 / 进度）、learn_docs/11 互相印证。

use crate::state::ActiveTurn;
use crate::state::MailboxDeliveryPhase;
use crate::state::TurnState;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::user_input::UserInput;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::watch;

/// 回合待处理输入的单元：要么是一批用户输入（[`UserInput`]，如文本 / 图片），要么是
/// 一条已经成形的 [`ResponseItem`]（如注入的上下文、子 Agent 邮件转换而来的消息）。
/// 两者最终都会被拼进发往模型的 prompt，但前者还需要经过用户消息成型这一步。
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TurnInput {
    UserInput(Vec<UserInput>),
    ResponseItem(ResponseItem),
}

/// Turn-local pending input storage owned by the input queue flow.
/// 回合级的待处理输入缓冲。它内嵌在 [`TurnState::pending_input`] 中，作用域随回合而生灭。
/// 之所以单列为类型而非裸 `Vec`，是为了让「回合输入」的所有读写都经由本模块的方法，
/// 把汇流逻辑收口在一处。
#[derive(Default)]
pub(crate) struct TurnInputQueue {
    items: Vec<TurnInput>,
}

/// Session-scoped pending input storage and active-turn mailbox delivery coordination.
/// 会话级状态：既保管子 Agent 邮箱，又协调「邮件何时并入当前回合」。
/// [`Session`] 全程持有唯一一份实例（见 session/session.rs）。
/// 注意命名里的 "pending input storage" 指的是它对 [`TurnState::pending_input`] 的存取
/// 入口，回合输入本身并不存在这里，而是存在各回合的 [`TurnState`] 中。
pub(crate) struct InputQueue {
    /// 邮箱「有新邮件」的广播通道。空载荷 `()`：仅作唤醒信号，订阅方收到后自行去
    /// drain 邮箱拿真实内容。用 `watch` 而非 `mpsc`：多个等待方只需知道「状态变了」，
    /// 不关心错过了几次变更，watch 天然合并通知、且新订阅者能立即对齐当前状态。
    mailbox_tx: watch::Sender<()>,
    /// 待投递的子 Agent 邮件队列（FIFO，保持投递顺序）。`Mutex<VecDeque<>>` 因为多个
    /// 子 Agent 可能并发投递，而 drain 时要按到达顺序一次取空。
    mailbox_pending_mails: Mutex<VecDeque<InterAgentCommunication>>,
}

impl InputQueue {
    pub(crate) fn new() -> Self {
        let (mailbox_tx, _) = watch::channel(());
        Self {
            mailbox_tx,
            mailbox_pending_mails: Mutex::new(VecDeque::new()),
        }
    }

    /// 订阅邮箱唤醒信号。返回的 receiver 会在每次有新邮件入队时被唤醒。
    /// 关键细节：订阅时若邮箱里已有积压邮件，立即 `mark_changed` 把 receiver 标记为
    /// 「已变更」——否则订阅者会漏掉订阅之前到达的邮件，陷入永久等待。这是用 `watch`
    /// 通道做唤醒时必须手动补的一课（watch 只通知订阅之后的变更）。
    pub(crate) async fn subscribe_mailbox(&self) -> watch::Receiver<()> {
        let mut mailbox_rx = self.mailbox_tx.subscribe();
        if self.has_pending_mailbox_items().await {
            mailbox_rx.mark_changed();
        }
        mailbox_rx
    }

    /// 子 Agent 投递一封邮件：先入队，再 `send_replace(())` 广播唤醒信号。
    /// 调用方见 session/handlers.rs 的 `inter_agent_communication`——若该邮件
    /// `trigger_turn` 为真，投递后还会尝试为空闲会话启动一个回合。
    ///
    /// 副作用：修改 `mailbox_pending_mails` 队列；唤醒所有 `subscribe_mailbox` 订阅者。
    pub(crate) async fn enqueue_mailbox_communication(
        &self,
        communication: InterAgentCommunication,
    ) {
        self.mailbox_pending_mails
            .lock()
            .await
            .push_back(communication);
        self.mailbox_tx.send_replace(());
    }

    /// 邮箱里是否还有未投递邮件。
    pub(crate) async fn has_pending_mailbox_items(&self) -> bool {
        !self.mailbox_pending_mails.lock().await.is_empty()
    }

    /// 邮箱里是否存在「应主动唤醒一个回合」的邮件（`trigger_turn == true`）。
    /// 普通邮件只是排队等下次请求顺带捎上；带 `trigger_turn` 的邮件则意味着发件方
    /// 期望收件 Agent 立刻醒来处理，调度侧据此决定要不要把空闲会话拉起来跑一轮。
    pub(crate) async fn has_trigger_turn_mailbox_items(&self) -> bool {
        self.mailbox_pending_mails
            .lock()
            .await
            .iter()
            .any(|mail| mail.trigger_turn)
    }

    /// 取空邮箱，并把每封邮件转换成可直接投喂模型的 [`ResponseItem`]。
    /// 转换走 [`InterAgentCommunication::to_response_input_item`]：邮件被序列化成 JSON，
    /// 包装为 `assistant` 角色的 commentary 消息（即以「旁白」形式呈现给模型，而非用户
    /// 提问）。drain 后邮箱清空，故本方法每封邮件只会被消费一次。
    pub(crate) async fn drain_mailbox_input_items(&self) -> Vec<ResponseItem> {
        self.mailbox_pending_mails
            .lock()
            .await
            .drain(..)
            .map(|mail| ResponseItem::from(mail.to_response_input_item()))
            .collect()
    }

    /// 按 `sub_id`（回合标识）找出当前活跃回合的 [`TurnState`] 句柄。
    /// 仅当存在活跃回合、且其运行任务的 `sub_id` 与传入值匹配时才返回 `Some`；
    /// 这道匹配是为了防止把状态误操作到一个已被换掉的旧回合上（请求与回合错位时
    /// 直接返回 `None`，让调用方安全地跳过）。
    pub(crate) async fn turn_state_for_sub_id(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
        sub_id: &str,
    ) -> Option<Arc<Mutex<TurnState>>> {
        let active = active_turn.lock().await;
        active.as_ref().and_then(|active_turn| {
            active_turn
                .task
                .as_ref()
                .is_some_and(|task| task.turn_context.sub_id == sub_id)
                .then(|| Arc::clone(&active_turn.turn_state))
        })
    }

    /// Clear any pending waiters and input buffered for the current turn.
    /// 打断 / 回合收尾时清空当前回合的所有待处理项：
    ///   · `clear_pending_waiters` 丢弃全部未决审批 / 权限等 oneshot sender（fail-closed）；
    ///   · 清空 `pending_input`，丢弃还没来得及投喂模型的追加输入。
    /// 注意此处只清回合级状态，不动会话级邮箱——被打断的回合不应吞掉发给整个会话的邮件。
    pub(crate) async fn clear_pending(&self, active_turn: &ActiveTurn) {
        let mut turn_state = active_turn.turn_state.lock().await;
        turn_state.clear_pending_waiters();
        turn_state.pending_input.items.clear();
    }

    /// 把当前回合的邮件投递阶段切到 [`MailboxDeliveryPhase::NextTurn`]：本回合已经
    /// 给出可见的最终答案，晚到的子邮件不应再去续接这条已展示的答案，留到下回合。
    ///
    /// 防御性短路：若回合此刻还有未消费的 `pending_input`，则**不**切换——因为这些
    /// 待处理输入注定会触发一次追问回合，那一轮里邮件本就该被并入，没必要推迟。
    /// 找不到匹配回合（请求与回合错位）时静默返回。
    pub(crate) async fn defer_mailbox_delivery_to_next_turn(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
        sub_id: &str,
    ) {
        let turn_state = self.turn_state_for_sub_id(active_turn, sub_id).await;
        let Some(turn_state) = turn_state else {
            return;
        };
        let mut turn_state = turn_state.lock().await;
        if !turn_state.pending_input.items.is_empty() {
            return;
        }
        turn_state.set_mailbox_delivery_phase(MailboxDeliveryPhase::NextTurn);
    }

    /// 按 `sub_id` 定位回合后，重开其邮件投递阶段为 `CurrentTurn`（见下方按 turn_state
    /// 的同名实现）。找不到匹配回合时静默返回。
    pub(crate) async fn accept_mailbox_delivery_for_current_turn(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
        sub_id: &str,
    ) {
        let turn_state = self.turn_state_for_sub_id(active_turn, sub_id).await;
        let Some(turn_state) = turn_state else {
            return;
        };
        self.accept_mailbox_delivery_for_turn_state(turn_state.as_ref())
            .await;
    }

    /// 把给定回合的邮件投递阶段重开为 [`MailboxDeliveryPhase::CurrentTurn`]，让此前排队
    /// 的子邮件能并入这一回合接下来的模型请求。当回合又获得明确的同回合工作时调用。
    pub(super) async fn accept_mailbox_delivery_for_turn_state(
        &self,
        turn_state: &Mutex<TurnState>,
    ) {
        turn_state
            .lock()
            .await
            .accept_mailbox_delivery_for_current_turn();
    }

    /// 「追加输入」+「重开邮件投递」二合一，且在同一把锁内原子完成。
    /// 典型场景：用户在回合运行中途 steer 一条新提问（见 session/mod.rs 的回合续接路径）。
    /// 之所以两步必须捆在一起：新输入会触发一次同回合的追问，而 steer 表达的是「我现在
    /// 想继续这一轮」，所以排队的子邮件也应一并并入——不能让阶段停留在 `NextTurn` 把邮件
    /// 推迟掉。拆成两次加锁则可能在中间被并发观察到「输入已加、阶段未开」的半成品状态。
    ///
    /// 副作用：扩充 `pending_input`；把投递阶段重置为 `CurrentTurn`。
    pub(super) async fn extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
        &self,
        turn_state: &Mutex<TurnState>,
        input: Vec<TurnInput>,
    ) {
        let mut turn_state = turn_state.lock().await;
        turn_state.pending_input.items.extend(input);
        turn_state.accept_mailbox_delivery_for_current_turn();
    }

    /// 向指定回合追加待处理输入（不触碰邮件投递阶段，与上面的二合一版本相区别）。
    /// 用于注入 ResponseItem 等不改变「同回合 / 下回合」语义的场景（见 session/inject.rs）。
    pub(crate) async fn extend_pending_input_for_turn_state(
        &self,
        turn_state: &Mutex<TurnState>,
        input: Vec<TurnInput>,
    ) {
        turn_state.lock().await.pending_input.items.extend(input);
    }

    /// 取走指定回合的全部待处理输入，原地清空缓冲。
    /// `split_off(0)` 把整个 `items` 搬出并留下空 `Vec`，等价于「take」语义：返回值归
    /// 调用方所有，回合内的缓冲被清零，故同一批输入不会被消费两次。
    pub(crate) async fn take_pending_input_for_turn_state(
        &self,
        turn_state: &Mutex<TurnState>,
    ) -> Vec<TurnInput> {
        turn_state.lock().await.pending_input.items.split_off(0)
    }

    /// 取走「下一次模型请求」应当携带的全部输入：把回合级 `pending_input` 与会话级
    /// 邮箱内容按规则汇流成一个有序列表。agent 主循环每轮开头调用它（见 session/turn.rs）。
    ///
    /// 汇流规则：
    ///   · 回合输入永远先取（FIFO，保持用户追加顺序）；
    ///   · 邮件仅当本回合投递阶段为 `CurrentTurn` 时才一并 drain，并追加在回合输入之后；
    ///     若阶段为 `NextTurn`，邮件留在邮箱，本次只返回回合输入。
    ///
    /// 副作用：清空回合的 `pending_input`；（阶段允许时）清空会话邮箱。两者都是「取走」语义。
    ///
    /// 关于 `#[expect(await_holding_invalid_type)]`：内层先锁 `active_turn` 再锁其
    /// `turn_state`，且要在持锁期间 `.await`。这是有意为之——「确认仍是同一活跃回合」与
    /// 「读改其状态」必须原子完成，否则可能在判定与改写之间回合被换掉，造成状态错位。
    /// 故在此显式放行该 lint，原因见属性内的 `reason`。
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub(crate) async fn get_pending_input(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
    ) -> Vec<TurnInput> {
        // Step 1：在一把锁内取走回合输入并读出当前投递阶段。
        // 无活跃回合时返回 (空, 接受投递)——「接受」是为了让随后的逻辑照常 drain 邮箱，
        // 这样即便没有回合在跑，积压的邮件也能在这一轮被取出。
        let (pending_input, accepts_mailbox_delivery) = {
            let mut active = active_turn.lock().await;
            match active.as_mut() {
                Some(active_turn) => {
                    let mut turn_state = active_turn.turn_state.lock().await;
                    (
                        turn_state.pending_input.items.split_off(0),
                        turn_state.accepts_mailbox_delivery_for_current_turn(),
                    )
                }
                None => (Vec::new(), true),
            }
        };
        // Step 2：阶段为 NextTurn 则提前返回，把邮件留在邮箱，绝不去 drain。
        // 这正是「已展示最终答案后，晚到邮件不续接本轮」语义的落地点。
        if !accepts_mailbox_delivery {
            return pending_input;
        }
        // Step 3：drain 邮箱并把邮件追加在回合输入之后，保证回合输入优先于邮件。
        // pending_input 为空时直接收集邮件，省一次无谓的扩容拷贝。
        let mailbox_items = self
            .drain_mailbox_input_items()
            .await
            .into_iter()
            .map(TurnInput::ResponseItem);
        if pending_input.is_empty() {
            mailbox_items.collect()
        } else {
            let mut pending_input = pending_input;
            pending_input.extend(mailbox_items);
            pending_input
        }
    }

    /// 「是否还有输入待处理」的只读探测，不消费任何东西。
    /// agent 主循环用它决定本轮跑完后是否还需再追问一轮（与 `model_needs_follow_up`
    /// 取或，见 session/turn.rs：`needs_follow_up = model_needs_follow_up || has_pending_input`）。
    ///
    /// 判定顺序与 `get_pending_input` 的取数规则严格对齐，确保「探测为真」与「取到非空」一致：
    ///   1. 回合有待处理输入 → 直接真；
    ///   2. 否则若阶段为 `NextTurn`（不接受本轮投递）→ 假，邮件不算进本轮；
    ///   3. 否则看邮箱是否非空。
    /// 同样需 `#[expect(await_holding_invalid_type)]`：理由同 `get_pending_input`，两把锁
    /// 嵌套下的判定必须原子，避免与并发的回合切换错位。
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state reads must remain atomic"
    )]
    pub(crate) async fn has_pending_input(&self, active_turn: &Mutex<Option<ActiveTurn>>) -> bool {
        let (has_turn_pending_input, accepts_mailbox_delivery) = {
            let active = active_turn.lock().await;
            match active.as_ref() {
                Some(active_turn) => {
                    let turn_state = active_turn.turn_state.lock().await;
                    (
                        !turn_state.pending_input.items.is_empty(),
                        turn_state.accepts_mailbox_delivery_for_current_turn(),
                    )
                }
                None => (false, true),
            }
        };
        if has_turn_pending_input {
            return true;
        }
        if !accepts_mailbox_delivery {
            return false;
        }
        self.has_pending_mailbox_items().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::AgentPath;
    use pretty_assertions::assert_eq;

    fn make_mail(
        author: AgentPath,
        recipient: AgentPath,
        content: &str,
        trigger_turn: bool,
    ) -> InterAgentCommunication {
        InterAgentCommunication::new(
            author,
            recipient,
            Vec::new(),
            content.to_string(),
            trigger_turn,
        )
    }

    #[tokio::test]
    async fn input_queue_notifies_mailbox_subscribers() {
        let input_queue = InputQueue::new();
        let mut mailbox_rx = input_queue.subscribe_mailbox().await;

        input_queue
            .enqueue_mailbox_communication(make_mail(
                AgentPath::root(),
                AgentPath::try_from("/root/worker").expect("agent path"),
                "one",
                /*trigger_turn*/ false,
            ))
            .await;
        input_queue
            .enqueue_mailbox_communication(make_mail(
                AgentPath::root(),
                AgentPath::try_from("/root/worker").expect("agent path"),
                "two",
                /*trigger_turn*/ false,
            ))
            .await;

        mailbox_rx.changed().await.expect("mailbox update");
    }

    #[tokio::test]
    async fn input_queue_drains_mailbox_in_delivery_order() {
        let input_queue = InputQueue::new();
        let mail_one = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "one",
            /*trigger_turn*/ false,
        );
        let mail_two = make_mail(
            AgentPath::try_from("/root/worker").expect("agent path"),
            AgentPath::root(),
            "two",
            /*trigger_turn*/ false,
        );

        input_queue
            .enqueue_mailbox_communication(mail_one.clone())
            .await;
        input_queue
            .enqueue_mailbox_communication(mail_two.clone())
            .await;

        assert_eq!(
            input_queue.drain_mailbox_input_items().await,
            vec![
                ResponseItem::from(mail_one.to_response_input_item()),
                ResponseItem::from(mail_two.to_response_input_item())
            ]
        );
        assert!(!input_queue.has_pending_mailbox_items().await);
    }

    #[tokio::test]
    async fn input_queue_tracks_pending_trigger_turn_mail() {
        let input_queue = InputQueue::new();

        input_queue
            .enqueue_mailbox_communication(make_mail(
                AgentPath::root(),
                AgentPath::try_from("/root/worker").expect("agent path"),
                "queued",
                /*trigger_turn*/ false,
            ))
            .await;
        assert!(!input_queue.has_trigger_turn_mailbox_items().await);

        input_queue
            .enqueue_mailbox_communication(make_mail(
                AgentPath::root(),
                AgentPath::try_from("/root/worker").expect("agent path"),
                "wake",
                /*trigger_turn*/ true,
            ))
            .await;
        assert!(input_queue.has_trigger_turn_mailbox_items().await);
    }
}
