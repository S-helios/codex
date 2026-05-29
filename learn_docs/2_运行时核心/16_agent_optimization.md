# 16 - Agent 优化：上下文管理与压缩

> 文档编号：16 | 类型：深度解析 | 适合读者：Agent 优化、长任务/自治 Agent、core 贡献者
>
> 本文档回答：模型的上下文窗口是有限的，可一个 agent 任务动辄几十上百轮、塞满工具
> 输出，凭什么能一直跑下去？token 是怎么估的？什么时候触发"压缩"、压缩到底压掉了
> 什么、又保住了什么？切换到更小的模型时为什么不会"一上来就爆"？

---

## 目录

1. [核心矛盾：无限对话 vs 有限窗口](#1-核心矛盾无限对话-vs-有限窗口)
2. [ContextManager：历史的内存模型](#2-contextmanager历史的内存模型)
3. [Token 估算：字节启发式](#3-token-估算字节启发式)
4. [有效上下文窗口：故意不用满](#4-有效上下文窗口故意不用满)
5. [自动压缩的触发判断](#5-自动压缩的触发判断)
6. [prefix cache 与两种计量 scope](#6-prefix-cache-与两种计量-scope)
7. [压缩流程：三个入口到一处实现](#7-压缩流程三个入口到一处实现)
8. [压缩算法：保新弃旧 + 摘要](#8-压缩算法保新弃旧--摘要)
9. [ContextWindowExceeded：丢最旧、保前缀](#9-contextwindowexceeded丢最旧保前缀)
10. [模型降档预压缩](#10-模型降档预压缩)
11. [回滚：按轮丢弃](#11-回滚按轮丢弃)
12. [远程压缩](#12-远程压缩)
13. [核心代码路径索引](#13-核心代码路径索引)

---

## 1. 核心矛盾：无限对话 vs 有限窗口

每个大模型都有**上下文窗口**上限（如 128K / 200K token）。而一个 agent 任务的历史会
不断膨胀：

- 每轮用户输入、助手回复、推理（reasoning）都进历史；
- 工具调用的输出尤其凶猛——一条 `cat 大文件` 或 `grep` 可能几千 token；
- 任务越久，历史越长，迟早撑爆窗口。

如果什么都不做，agent 跑到一定轮数就会因"上下文超限"报错而中断。**上下文优化的目标
就是：在有限窗口内，让 agent 能持续、稳定地跑下去，同时尽量不丢关键信息。** codex 用
三套手段配合：

1. **记录时截断**——单条超长工具输出进历史前就先截断（`record_items` + `TruncationPolicy`）。
2. **压缩（compaction）**——历史整体逼近上限时，用模型把旧历史总结成一段摘要，腾出空间。
3. **超限兜底**——真的还是超了，逐条丢最旧的，保住前缀缓存。

---

## 2. ContextManager：历史的内存模型

一切的载体是 `ContextManager`
（[`core/src/context_manager/history.rs`](../codex-rs/core/src/context_manager/history.rs)）：

```rust
pub(crate) struct ContextManager {
    items: Vec<ResponseItem>,          // 最旧在前、最新在后——顺序即时间序
    history_version: u64,              // 每次"整体改写"(压缩/回滚)就+1
    token_info: Option<TokenUsageInfo>,
    reference_context_item: Option<TurnContextItem>,  // 设置变更 diff 的基准
}
```

它围绕 `items` 提供四类能力：

| 能力 | 方法 | 说明 |
|------|------|------|
| **记录** | `record_items` | 新条目按截断策略写入，过滤掉不该进历史的项 |
| **取用** | `for_prompt` | 产出"发给模型"的规范化视图 |
| **改写** | `remove_first_item` / `drop_last_n_user_turns` / `replace` | 压缩丢最旧 / 回滚 / 压缩后整体替换 |
| **估算** | `estimate_token_count` / `get_total_token_usage` | 反推 token 数，给压缩判断提供依据 |

**设计取舍**：历史只在"发给模型那一刻"才规范化（`for_prompt` 按值消费 `self`，拿走
快照所有权，避免污染长期存的历史）；平时增删尽量做局部修补，避免每次全量扫描。

`record_items` 入口两步把关：
1. `is_api_message` 过滤——system 消息、纯本地触发项（如 `CompactionTrigger`）不进历史；
2. `process_item` 按 `TruncationPolicy` 压缩超长工具输出（带 1.2× 序列化预算余量）。

---

## 3. Token 估算：字节启发式

要判断"何时该压缩"，先得知道"现在用了多少 token"。但精确 tokenize 既慢又依赖具体
分词器，codex 采用**粗略的字节启发式**作为下界估计（lower bound）：

```
token 数 ≈ model-visible 字节数 ÷ 4   （带 ceiling 除法）
```

核心函数 `estimate_response_item_model_visible_bytes`：

- 普通条目：`serde_json` 序列化后取字节长度；
- **图片**：base64 原始 payload 太大且不代表真实 token，故替换为固定估算
  （`RESIZED_IMAGE_BYTES_ESTIMATE = 7373` 字节 ≈ 1844 token），`detail:"original"` 的
  图片则按 32px patch 数算（带 LRU 缓存避免重复解码）；
- **加密推理 / 压缩内容**：按编码长度反推明文长度（`estimate_reasoning_length`）。

`get_total_token_usage` 的精妙之处在于服务端计量的"时间差"：

```
服务端返回的 total_tokens 只覆盖到"最后一次 API 响应"那一刻
              │
              ▼
本地之后又追加了工具输出等条目 ── 服务端没算
              │
total = 服务端报的值
      + 本地新增条目的估算
      + （若 server_reasoning_included == false）非末轮的 reasoning 估算
```

即"服务端权威值 + 本地增量补估"，避免重复计 / 漏计推理 token。

---

## 4. 有效上下文窗口：故意不用满

`TurnContext::model_context_window`
（[`turn_context.rs`](../codex-rs/core/src/session/turn_context.rs)）不直接用模型标称窗口：

```rust
有效窗口 = 标称窗口 × effective_context_window_percent / 100
```

为什么打折？因为要给**输出和推理留空间**，也降低踩到服务端硬上限的风险。压缩判断、
token 预算都以这个"打折后"的值为准，而不是名义上限。这是个朴素但重要的工程余量。

---

## 5. 自动压缩的触发判断

回合执行时，`auto_compact_token_status`
（[`core/src/session/turn.rs`](../codex-rs/core/src/session/turn.rs)）算出当前是否"该压了"：

```rust
token_limit_reached =
       auto_compact_scope_tokens >= auto_compact_scope_limit   // 配置的压缩预算耗尽
    || full_context_window_limit_reached                       // 或可用窗口已满
```

触发点有两处：

- **采样前**（pre-turn）：`run_pre_sampling_compact` 在每个回合开始、调用模型前检查，
  超限就先压缩腾空间，再开始本回合。注入策略用 `DoNotInject`。
- **回合中**（mid-turn）：`turn.rs:311` 处，当 `token_limit_reached && needs_follow_up`
  （还要继续追问模型）时，以 `BeforeLastUserMessage` 注入策略先压缩再继续循环。

> 注释里有一句关键判断："只要压缩能把 token 压到远低于上限，就不必担心死循环。"
> 即压缩必须"显著"腾空间，否则压完又立刻超限会陷入反复压缩。

---

## 6. prefix cache 与两种计量 scope

模型 API 通常对**未变的前缀**提供缓存折扣（prefix cache）——只要历史开头那段不变，
重复发送就便宜。codex 的压缩策略刻意配合这一点，提供两种计量 scope
（`AutoCompactTokenLimitScope`）：

| Scope | 计量口径 | 适用 |
|-------|---------|------|
| `Total` | 整个上下文都计入限额 | 简单直接 |
| `BodyAfterPrefix` | **只计前缀缓存基线之后**的"增量主体" | 配合 prefix cache，更省 |

`BodyAfterPrefix` 用一个"自动压缩窗口"（`AutoCompactWindow`，
[`core/src/state/auto_compact_window.rs`](../codex-rs/core/src/state/auto_compact_window.rs)）
跟踪 prefill（被缓存的前缀）token 数：

```
active_context_tokens（当前总量）
        − prefill_input_tokens（已被服务端缓存的前缀，便宜）
        ───────────────────────────────────
        = auto_compact_scope_tokens（真正"贵"的增量主体）
```

这样限额只卡在"增量主体"上，让被缓存的前缀不挤占预算——既省钱又能多跑几轮才压缩。

---

## 7. 压缩流程：三个入口到一处实现

压缩逻辑集中在 [`core/src/compact.rs`](../codex-rs/core/src/compact.rs)，三个触发口最终
汇流到同一实现：

```
run_inline_auto_compact_task   （回合中自动触发，token 超限）
run_compact_task               （用户手动 /compact，或 Op::Compact）
                    │
                    ▼
        run_compact_task_inner         （挂 hook + telemetry 的中间层）
                    │
                    ▼
        run_compact_task_inner_impl    （真正干活，5 步）
```

`run_compact_task_inner_impl` 的步骤（compact.rs `200`~`328`）：
1. 取当前历史的副本，把"触发压缩的输入"（`compact_prompt`）`record_items` 进去；
2. 循环向模型发请求要摘要（`drain_to_completed`）；错误三分支——`Interrupted` 直接上抛、
   `ContextWindowExceeded` 且历史多于 1 条时丢最旧一条并重置重试计数再来（见 §9）、
   其它错误指数退避重试至上限；
3. 用 `compact_prompt`（默认 `SUMMARIZATION_PROMPT`，来自模板文件 `templates/compact/prompt.md`）作为
   摘要指令；拿到摘要后拼成 `SUMMARY_PREFIX + 摘要正文`；
4. `build_compacted_history`（薄封装 → `build_compacted_history_with_limit`）拼出新历史；若注入策略是
   `BeforeLastUserMessage`，再用 `insert_initial_context_before_last_real_user_or_summary` 回插初始上下文
   （`DoNotInject` 则不注入、`reference_context_item` 置 `None`）；
5. `replace_compacted_history` 整体替换历史并重算 token 用量。

> 注意：初始上下文注入发生在**摘要生成之后**的组装阶段，而非开头；`BeforeLastUserMessage` 用于回合中
> 压缩、`DoNotInject` 用于回合前自动压缩与手动 `/compact`。

---

## 8. 压缩算法：保新弃旧 + 摘要

核心是 `build_compacted_history_with_limit`：它**不是简单"全删换摘要"**，而是要在
保留摘要的同时，尽量留住最近的用户消息（这些往往是当前任务最相关的）。

```
预算 COMPACT_USER_MESSAGE_MAX_TOKENS = 20_000 token
        │
        ▼
从最新往最旧反向遍历用户消息（保新弃旧）：
    累计 token，未超剩余预算就整条保留；
    遇到第一条会超预算的，把它按剩余预算截断后纳入，再停
        │
        ▼
最终历史 = [初始上下文(可选)] + [保留的最近用户消息们] + [摘要消息]
```

"反向遍历 + 预算"这套（注释里叫**保新弃旧**）保证：

- 最近的用户意图被原样保留（不被摘要稀释）；
- 更早的细节被压成一段摘要；
- 总量受控在预算内。

**摘要怎么存进历史**（本地压缩路径）：`build_compacted_history_with_limit` 把保留的用户消息
和摘要都作为**普通 `ResponseItem::Message`（role = `"user"`、`InputText` 纯文本）**写回（compact.rs
`542`~`564`）。摘要文本是 `SUMMARY_PREFIX + 模型生成的摘要`。注意：**它不是加密的 `Compaction` /
`ContextCompaction` 条目**——那两种带 `encrypted_content` 的 `ResponseItem` 属于**远程压缩**路径
（compact_remote_v2.rs 构造），其 token 估算才走 `estimate_reasoning_length` 按编码长度反推（见 §3）。
本地摘要是明文 user 消息，按普通条目序列化字节估算。

> [说明] 当前实现里**本地路径直接用 user 文本消息**承载摘要，带 `encrypted_content` 的压缩条目
> （`Compaction` / `ContextCompaction`）仅见于远程压缩路径。

---

## 9. ContextWindowExceeded：丢最旧、保前缀

压缩本身要调一次模型（生成摘要），万一**连这次调用都超窗口**怎么办？
`run_compact_task_inner_impl` 捕获 `CodexErr::ContextWindowExceeded`，策略是
**丢弃最旧的一条历史再重试**：

```rust
Err(e @ CodexErr::ContextWindowExceeded) => {
    if turn_input_len > 1 {
        // 从头部删以保前缀缓存、留住近期消息；重置重试计数再来一轮
        history.remove_first_item();
        retries = 0;
        continue;
    }
    // 只剩一条还超 → 标记 token 满、报错放弃
    sess.set_total_tokens_full(turn_context.as_ref()).await;
    // ...
    return Err(e);
}
```

为什么丢**最旧**而不是最新？两个原因：

1. 最旧的内容通常最不相关；
2. **保护 prefix cache**——从头部删能让"剩下的前缀"尽量稳定，配合 §6 的缓存策略。

`remove_first_item` 还会顺带删掉被删条目的 call/output 配对另一半，维持"调用必有输出"
的不变量，避免给模型发半截配对被拒。

---

## 10. 模型降档预压缩

一个容易被忽视的优化：用户**中途切换到上下文窗口更小的模型**时，旧历史可能装不进新
窗口，新模型一上来就会失败。`maybe_run_previous_model_inline_compact`
（[`turn.rs`](../codex-rs/core/src/session/turn.rs)）专治此症：

> 三个条件**全满足**才执行：① 历史超出新模型阈值 && ② 模型确实换了 && ③ 旧窗口比新窗口大。
> 此时**先用上一个（更大窗口的）模型**对历史做一次压缩，免得新模型一上来就因装不下旧历史而失败。

这是个很贴心的"换挡保护"——把压缩这件需要大窗口的事，留给还没切走的大模型来做。

---

## 11. 回滚：按轮丢弃

`Op::ThreadRollback { num_turns }` → `drop_last_n_user_turns`
（[`history.rs`](../codex-rs/core/src/context_manager/history.rs)）：从尾部砍掉最近 N 个
**指令轮**（真正的用户消息，或助手发出的 inter-agent 指令——这俩才算"轮边界"）。

语义对齐"replay-to-rebuild"：

- `num_turns == 0` → 空操作；
- 没有用户轮 → 空操作；
- 要砍的超过现有轮数 → 砍光所有用户轮，但**第一个用户消息之前的会话前缀一律保留**。

回滚还有个隐藏副作用：若砍掉的开发者消息是混合了"上下文片段 + 持久开发者文本"的
`build_initial_context` 包，会顺带清空 `reference_context_item`——因为后续设置变更 diff
失去了基准，下一轮必须**全量重新注入**上下文而非增量 diff。

**注意**：回滚只动内存历史，**不撤销磁盘上的文件改动**（这点 `Op::ThreadRollback` 文档
明确说了，由客户端负责）。

---

## 12. 远程压缩

除本地压缩外，codex 还有"远程压缩"路径
（[`compact_remote.rs`](../codex-rs/core/src/compact_remote.rs) /
`compact_remote_v2.rs`），由 `should_use_remote_compact_task` 决定是否启用。其思路是把
压缩这件"调模型总结历史"的活交给服务端做，减少客户端往返与本地处理。触发口与本地版
对称：`run_inline_remote_auto_compact_task`。具体协议细节随后端能力演进，这里只需知道
"压缩既可本地也可远程，由特性/配置切换"。

---

## 13. 核心代码路径索引

| 主题 | 文件 | 关键符号 |
|------|------|---------|
| 历史内存模型 | [`context_manager/history.rs`](../codex-rs/core/src/context_manager/history.rs) | `ContextManager`、`record_items`、`for_prompt` |
| Token 估算 | 同上 | `estimate_response_item_model_visible_bytes`、`get_total_token_usage` |
| 有效窗口 | [`session/turn_context.rs`](../codex-rs/core/src/session/turn_context.rs) | `model_context_window` |
| 压缩触发判断 | [`session/turn.rs`](../codex-rs/core/src/session/turn.rs) | `auto_compact_token_status`、`run_pre_sampling_compact` |
| 压缩窗口 / prefix | [`state/auto_compact_window.rs`](../codex-rs/core/src/state/auto_compact_window.rs) | `AutoCompactWindow` |
| 压缩实现 | [`compact.rs`](../codex-rs/core/src/compact.rs) | `run_compact_task_inner_impl`、`build_compacted_history_with_limit` |
| 压缩预算常量 | 同上 | `COMPACT_USER_MESSAGE_MAX_TOKENS = 20_000` |
| 远程压缩 | [`compact_remote.rs`](../codex-rs/core/src/compact_remote.rs) | `run_inline_remote_auto_compact_task` |
| 模型降档预压缩 | [`session/turn.rs`](../codex-rs/core/src/session/turn.rs) | `maybe_run_previous_model_inline_compact` |
| 回滚 | [`context_manager/history.rs`](../codex-rs/core/src/context_manager/history.rs) | `drop_last_n_user_turns` |

---

> **上一篇**：[15 - API 与协议层详解](../5_前端_集成_协议/15_api_protocol_layer.md)
> **下一篇**：（本系列暂止于此；后续可基于新 upstream 快照另起 `feature-learn-vNEXT`）
