# Codex 记忆系统详解：短期记忆与长期记忆

> 用户在 ChatGPT 里看到的"记忆/简介"功能，Codex 是用类似机制实现的。
> 本文档全面解释 Codex 的短期记忆和长期记忆，包括工程实现、文件布局、触发时机、注入路径。

---

## 目录

1. [两种记忆的本质区别](#1-两种记忆的本质区别)
2. [短期记忆：Session 内的上下文](#2-短期记忆session-内的上下文)
3. [长期记忆：跨 Session 的知识持久化](#3-长期记忆跨-session-的知识持久化)
4. [长期记忆文件布局](#4-长期记忆文件布局)
5. [记忆生成流水线：Phase 1 + Phase 2](#5-记忆生成流水线phase-1--phase-2)
6. [记忆注入路径：如何进入 LLM 提示词](#6-记忆注入路径如何进入-llm-提示词)
7. [AGENTS.md：项目级指令（类比"简介"）](#7-agentsmd项目级指令类比简介)
8. [配置控制](#8-配置控制)
9. [ThreadMemoryMode：线程级记忆开关](#9-threadmemorymode线程级记忆开关)
10. [核心代码路径索引](#10-核心代码路径索引)

---

## 1. 两种记忆的本质区别

| 维度 | 短期记忆 | 长期记忆 |
|------|---------|---------|
| **存储位置** | 内存（`ContextManager`，`Vec<ResponseItem>`） | 磁盘（`~/.codex/memories/`） |
| **生存周期** | 一个 Session 内（进程退出即消失） | 跨 Session、跨进程、跨天 |
| **来源** | 用户输入 + 模型输出（对话流） | 对话历史（rollout 文件）经 LLM 提炼 |
| **注入时机** | 每次 LLM 调用自动包含（via `clone_history().for_prompt()`） | 每次 Turn 开始时注入 system prompt（developer instructions） |
| **用途** | 保持对话上下文连贯 | 保持用户偏好、项目惯例、跨项目知识 |
| **类比** | 人的工作记忆（当下谈话内容） | 人的长期记忆（经验、习惯、知识） |

---

## 2. 短期记忆：Session 内的上下文

### 核心数据结构

```rust
// codex-rs/core/src/context_manager/history.rs:34
pub(crate) struct ContextManager {
    items: Vec<ResponseItem>,            // 完整对话历史（模型输入/输出/工具调用/工具结果）
    history_version: u64,                // compaction/rollback 时自增，用于检测历史变更
    token_info: Option<TokenUsageInfo>,  // 累计 token 使用量
    // 还有一个 baseline 上下文快照字段（用于 diff 出"设置更新"项），此处省略
}
```

`ContextManager` 是短期记忆的容器。它随着对话进行不断增长，直到 Session 结束（进程退出）或被 `Op::Compact` 截断。

### 短期记忆的写入

每次有新内容产生，`record_conversation_items()` 追加到 ContextManager：

```
模型输出 token     → ResponseItem::AssistantMessage
工具调用发起       → ResponseItem::ToolCall
工具执行结果       → ResponseItem::ToolResult  
用户追问（steer）  → ResponseItem::UserMessage（从 pending_input 取出）
```

### 短期记忆的读取（→ LLM）

每次 sampling loop 顶部：

```rust
// clone_history(): session/mod.rs:2852（取 ContextManager 快照）
let history = sess.clone_history().await;             // ContextManager 快照
// for_prompt(): context_manager/history.rs:119（消费 self，按模型 modalities 产出 Vec<ResponseItem>）
let prompt_input = history.for_prompt(&modalities);   // 完整历史 → LLM 输入
```

LLM 每次 API call 都看到**完整的短期记忆**，不截断（直到 context window 上限）。

### 短期记忆的压缩：Op::Compact

当对话历史太长接近 context window 时，触发 `Op::Compact`：

1. 当前 ContextManager 历史被截断
2. 用一个摘要（CompactedItem）替代所有历史
3. 写入 rollout 文件一条 `RolloutItem::Compacted`
4. 新的 LLM call 看到的是摘要，而非完整历史

**注意**：Compact 只影响短期记忆，rollout 文件中的完整历史不删除。

---

## 3. 长期记忆：跨 Session 的知识持久化

Codex 的长期记忆系统与 ChatGPT 的 "Memory" 功能类似：它从历史对话中提炼用户偏好和知识，并在未来对话中自动使用。这就是你在 ChatGPT 里看到的"简介/记忆"功能在 Codex 里的等价物。

### 长期记忆的工作原理（概览）

```
历史对话 rollout 文件（磁盘）
    ↓
Phase 1（提炼）: 一个 LLM（gpt-5.4-mini）读取 rollout，提炼 raw_memory + rollout_summary
    ↓ 存入 state DB（SQLite）
Phase 2（整合）: 另一个 LLM（gpt-5.4）读取所有 raw_memory，整合更新 memory 文件
    ↓ 写入 ~/.codex/memories/ 目录
每次新对话开始: memory_summary.md 自动注入 system prompt
    ↓ LLM 拥有用户历史知识，无需重复说明
```

### 触发条件

长期记忆流水线由 app-server 在**每次带输入的 Turn 启动**时触发（`app-server/src/request_processors/turn_processor.rs:462`，仅当 `turn_has_input` 为真），调用 `start_memories_startup_task`（定义在 crate `codex-memories-write` 的 `memories/write/src/start.rs:22`）。函数内部再用以下条件提前返回（`start.rs:31`）：

```rust
pub fn start_memories_startup_task(..., source: &SessionSource) {
    if config.ephemeral                          // 跳过：短暂 session
        || !config.features.enabled(Feature::MemoryTool) // 跳过：功能未启用
        || source.is_non_root_agent()            // 跳过：子 Agent session
    {
        return;
    }
    // 还会检查 state DB 是否可用（context.state_db().is_none() → 跳过）
    tokio::spawn(async move { /* Phase 1 → Phase 2 */ });
}
```

条件：
- **非 ephemeral**（非临时 session）
- **`Feature::MemoryTool` 已启用**（feature flag）
- **根 session**（非子 Agent）
- **state DB 可用**（本地 SQLite）

---

## 4. 长期记忆文件布局

所有长期记忆文件存放在 `~/.codex/memories/`（`codex_home/memories/`）：

```
~/.codex/memories/
├── memory_summary.md          ← ★ 每次对话自动注入 system prompt（2500 token 上限）
├── MEMORY.md                  ← 可搜索的知识手册（条目 + 索引）
├── raw_memories.md            ← Phase 1 输出的合并原始记忆（Phase 2 的输入，临时文件）
├── skills/                    ← 可复用的技能/流程
│   └── <skill-name>/
│       ├── SKILL.md           ← 技能入口（操作步骤、注意事项）
│       ├── scripts/           ← 可选：辅助脚本
│       ├── templates/         ← 可选：模板文件
│       └── examples/          ← 可选：示例输出
├── rollout_summaries/         ← 每个 rollout 对话的摘要
│   └── <rollout_slug>.md      ← 按对话索引，包含经验教训和关键证据
├── extensions/                ← 记忆扩展（额外的记忆来源，如 GitHub PR 记录等）
│   └── <extension-name>/
│       └── instructions.md    ← 如何使用该扩展来源
└── .git/                      ← memories 目录的 git baseline（用于计算 diff）
```

### 各文件的作用

| 文件/目录 | 写入者 | 读取方式 | 用途 |
|---------|--------|---------|------|
| `memory_summary.md` | Phase 2 整合 Agent | 自动注入 system prompt | 高密度摘要，引导 LLM 快速定位记忆 |
| `MEMORY.md` | Phase 2 整合 Agent | LLM 用 grep 工具搜索 | 完整知识条目，可检索 |
| `raw_memories.md` | Phase 2 流水线 | Phase 2 整合时读取 | Phase 1 各 rollout raw_memory 的合并体，临时输入 |
| `rollout_summaries/` | Phase 2 整合 Agent | LLM 按需查阅 | 具体对话的摘要 + 关键证据（可引用） |
| `skills/` | Phase 2 整合 Agent | LLM 按任务查阅 | 可复用的操作流程（如"如何部署该项目"） |
| `extensions/` | 用户/外部工具 | Phase 1/2 提炼时引用 | 额外记忆来源（GitHub PR, Slack 等） |

---

## 5. 记忆生成流水线：Phase 1 + Phase 2

### Phase 1：per-rollout 提炼（提取）

**何时运行**：Session 启动时，后台 tokio::spawn 异步执行。

**做什么**：
1. 从 state DB 认领一批符合条件的 rollout（最多 `THREAD_SCAN_LIMIT = 5000` 个）
2. 筛选条件：
   - 来自交互式 session 来源（不是 sub-agent）
   - 在配置的 age window 内（不太新、不太旧）
   - 未被其他 worker 认领
3. 对每个 rollout，发送给 `gpt-5.4-mini`（轻量模型），要求输出：
   ```json
   {
     "raw_memory": "详细的 markdown 记忆内容...",
     "rollout_summary": "紧凑的摘要行，用于路由和索引",
     "rollout_slug": "可选的 slug（用于命名摘要文件）"
   }
   ```
4. 对生成内容进行 **secret redaction**（隐去 API key、密码等敏感信息）
5. 将结果写入 state DB（stage-1 outputs 表）

**并发控制**：`CONCURRENCY_LIMIT = 8`，最多 8 个 rollout 并行提炼。

**输入裁剪**：rollout 内容过长时，截断到模型 context window 的 70%（`CONTEXT_WINDOW_PERCENT = 70`）。

**无有价值内容时**：模型返回全空字段（no-op），不写任何记忆。

### Phase 2：全局整合（Consolidation）

**何时运行**：Phase 1 完成后紧接着运行。

**做什么**：
1. 认领全局 Phase 2 锁（state DB），确保同时只有一个 consolidation
2. 从 state DB 加载最新的 stage-1 outputs（按 usage 排名，最多 N 条）
3. 同步文件系统 artifacts：
   - `raw_memories.md`：合并所有 raw_memory（按 thread_id 升序）
   - `rollout_summaries/`：同步为当前选中的 rollout 摘要集合
4. 计算 **git diff**（memories 目录的 git baseline → 当前工作树）
5. 若有变更：启动整合子 Agent（gpt-5.4，中等推理力度）
   - Agent 看到 `phase2_workspace_diff.md`（diff 内容）
   - Agent 更新 `MEMORY.md`、`memory_summary.md`、`skills/` 等
   - Agent 完成后，reset git baseline
6. 将 Phase 2 结果写回 state DB

**整合 Agent 的限制**：
- 无网络访问
- 仅允许写本地 memories 目录
- 禁用 collaboration/sub-agent 模式（防止递归）

### 两个 Phase 为何分开

| Phase 1 | Phase 2 |
|---------|---------|
| 并行，每个 rollout 独立处理 | 串行，全局 mutex 锁 |
| 轻量模型（gpt-5.4-mini） | 较强模型（gpt-5.4） |
| 产出 per-rollout 原始记忆 | 产出整合后的全局知识库 |
| 可并发多 worker | 同一时刻最多一个 |

---

## 6. 记忆注入路径：如何进入 LLM 提示词

记忆注入**不是** session/mod.rs 里的内联代码，而是走**扩展（extension）机制**。记忆读取侧由独立 crate `codex-memories-extension`（`ext/memories/`）实现，它注册一个 `MemoriesExtension`，实现 `ContextContributor` trait（`ext/memories/src/extension.rs:49`）：

```rust
// ext/memories/src/extension.rs:50  —  contribute() 在每次组装提示词时被宿主调用
impl ContextContributor for MemoriesExtension {
    fn contribute<'a>(&'a self, _session: &'a ExtensionData, thread: &'a ExtensionData) -> ... {
        Box::pin(async move {
            let Some(config) = thread.get::<MemoriesExtensionConfig>() else { return Vec::new() };
            if !config.enabled { return Vec::new() }       // gate：见下方 from_config
            build_memory_tool_developer_instructions(&config.codex_home)
                .await
                .map(PromptFragment::developer_policy)      // 包装成 developer 段
                .into_iter().collect()
        })
    }
}
```

**启用开关**（`MemoriesExtensionConfig::from_config`，`ext/memories/src/extension.rs:42`）：

```rust
enabled: config.features.enabled(Feature::MemoryTool) && config.memories.use_memories
```

**安装位置**：`codex_memories_extension::install(&mut builder, ...)`（`app-server/src/extensions.rs:33`，crate 入口 `ext/memories/src/lib.rs:9` 的 `pub use extension::install`）。宿主在组装提示词时遍历所有 contributor 产出的 `PromptFragment`，把 `developer_policy` 段汇入 developer instructions（汇编循环在 `session/mod.rs` 约 2750–2785 行，与 AGENTS.md / 环境上下文同处一段）。

`build_memory_tool_developer_instructions` 的实现（`ext/memories/src/prompts.rs:27`）：

```rust
pub(crate) async fn build_memory_tool_developer_instructions(codex_home) -> Option<String> {
    let base_path = codex_home.join("memories");
    let memory_summary = fs::read_to_string(base_path.join("memory_summary.md"))
        .await.ok()?.trim().to_string();
    // 截断到 2500 tokens（常量 MEMORY_TOOL_DEVELOPER_INSTRUCTIONS_SUMMARY_TOKEN_LIMIT）
    let memory_summary = truncate_text(&memory_summary, TruncationPolicy::Tokens(2_500));
    if memory_summary.is_empty() { return None }   // 摘要为空 → 不注入
    // 渲染 ext/memories/templates/memories/read_path.md 模板
    MEMORY_TOOL_DEVELOPER_INSTRUCTIONS_TEMPLATE.render([
        ("base_path", base_path),
        ("memory_summary", memory_summary),
    ]).ok()
}
```

注入到 LLM 的内容包含（来自 `read_path.md` 模板）：

1. **记忆使用决策边界**：什么时候该查记忆，什么时候跳过
2. **记忆文件布局说明**：memory_summary.md / MEMORY.md / rollout_summaries / skills
3. **快速记忆检索流程**（4-6 步以内）
4. **memory_summary.md 的内容**（实际的记忆摘要，2500 token 上限）
5. **记忆引用格式要求**（`<oai-mem-citation>` 标签，用于追踪记忆使用）
6. **如何更新记忆**（仅在用户明确请求时，写入 `extensions/ad_hoc/notes/`）

### 记忆检索流程（LLM 视角）

LLM 被指示执行以下步骤：

```
1. 读 memory_summary.md（已在 prompt 中，无需 read 工具）
   → 提取与当前任务相关的关键词

2. grep MEMORY.md（用关键词搜索）
   → 找到相关条目/指针

3. 若 MEMORY.md 指向 rollout_summaries/ 或 skills/：
   → 读取 1-2 个最相关的文件

4. 若需要精确命令、错误文本、具体证据：
   → 搜索对应的 rollout_path

5. 若无相关命中 → 停止记忆检索，正常回答
```

**预算限制**：最多 4-6 步记忆检索步骤，避免过度消耗 token。

---

## 7. AGENTS.md：项目级指令（类比"简介"）

除了跨项目的长期记忆，Codex 还有一个**项目级短期指令**机制：`AGENTS.md`。

### 什么是 AGENTS.md

`AGENTS.md` 是放在项目目录中的 Markdown 文件，包含针对当前项目的 AI 操作指令。类似于 ChatGPT 的"自定义指令"或"系统 prompt 补充"。

文件名优先级（高 → 低）：
1. `AGENTS.override.md`（本地覆盖，不应提交到版本控制）
2. `AGENTS.md`（标准文件）
3. `config.project_doc_fallback_filenames`（自定义 fallback 文件名列表）

还有一个**全局 AGENTS.md**：`~/.codex/AGENTS.md`（用户级全局指令）。

### AGENTS.md 的发现机制

`AgentsMdManager`（`agents_md.rs`）负责发现和加载：

```
当前工作目录 cwd
    ↓ 向上遍历找项目根（.git 等标记文件）
    ↓ 从项目根 → cwd 的每一层收集 AGENTS.md
    ↓ 按从上到下的顺序拼接所有 AGENTS.md 内容
→ 最终 user_instructions 字符串
```

例如，`cwd = /work/myproject/src/auth`：

```
发现：
  /work/.git/         → 项目根（不收集）
  /work/AGENTS.md     ← 收集
  /work/myproject/AGENTS.md ← 收集
  /work/myproject/src/AGENTS.md ← 收集（若存在）
  /work/myproject/src/auth/AGENTS.md ← 收集（若存在）

按顺序拼接，形成 user_instructions
```

### AGENTS.md 与长期记忆的区别

| | AGENTS.md | 长期记忆（~/.codex/memories） |
|-|-----------|---------------------------|
| **内容** | 开发者/项目维护者手写的指令 | LLM 自动从历史对话中提炼 |
| **范围** | 项目专属 | 用户全局（跨项目） |
| **更新** | 人工编辑 | 自动（每次 Session 启动后台更新） |
| **注入位置** | user message（不同于 developer message） | developer message |
| **类比** | README 里的 AI 操作规范 | 用户个人偏好档案 |

### AGENTS.md 的注入路径

在 `session/mod.rs` 中，每次 Turn 开始时：

```rust
// session/mod.rs:2776
if let Some(user_instructions) = turn_context.user_instructions.as_deref() {
    contextual_user_sections.push(
        UserInstructions {
            text: user_instructions.to_string(),
            directory: turn_context.cwd.to_string_lossy().into_owned(),
        }.render(),
    );
}
```

user_instructions = `config.user_instructions`（固定配置）+ AGENTS.md 内容（按目录层级拼接）。

---

## 8. 配置控制

### 配置文件位置

`~/.codex/config.toml`

### 记忆相关配置

配置结构：`MemoriesToml`（on-disk TOML 形态，全 Option）→ `MemoriesConfig`（应用默认值后的有效配置），均定义在 `config/src/types.rs`（`MemoriesToml:261` / `MemoriesConfig:294`）。

```toml
[memories]
# 是否为新线程生成记忆（false → 新线程在 state DB 中以 memory_mode="disabled" 存储）
generate_memories = true              # 默认 true

# 是否将 memory_summary.md 注入每次对话的 developer prompt
use_memories = true                   # 默认 true

# 是否通过扩展工具面暴露专用记忆工具（list / read / search / add_ad_hoc_note）
dedicated_tools = false               # 默认 false

# 外部上下文（MCP 工具 / Web Search）是否把线程 memory_mode 标记为 "polluted"
# （旧键名 no_memories_if_mcp_or_web_search 仍作为 serde alias 兼容）
disable_on_external_context = false   # 默认 false（注意：默认不污染）

# Phase 2 整合时最多保留多少条 raw_memory（取值范围 1..=4096）
max_raw_memories_for_consolidation = 200   # 有内置默认常量

# 记忆超过多少天未被使用就不再参与 Phase 2 选择
max_unused_days = 90                  # 有内置默认常量

# 参与提炼的 rollout 最大年龄（天）；每次启动最多处理的 rollout 候选数（1..=128）
max_rollout_age_days = 30
max_rollouts_per_startup = 16

# 线程最后活动到生成记忆之间的最小空闲时长（小时，建议 >12h）
min_rollout_idle_hours = 12

# 启动记忆流水线前要求的 Codex 限流窗口剩余百分比下限（0..=100）
min_rate_limit_remaining_percent = 20

# 覆盖 Phase 1 / Phase 2 默认模型（默认 None → 用内置 gpt-5.4-mini / gpt-5.4）
extract_model = "gpt-5.4-mini"
consolidation_model = "gpt-5.4"
```

> 注：以上 `max_rollout_age_days`、`max_rollouts_per_startup` 等数值仅为示意；真实默认值是 `config/src/types.rs` 中的 `DEFAULT_MEMORIES_*` 常量，可能随版本变化，使用前请以源码为准。

### AGENTS.md 相关配置

```toml
# 项目文档（AGENTS.md）的最大字节数（0 = 禁用）
project_doc_max_bytes = 1048576    # 默认 1MB

# 额外的 fallback 文件名（除 AGENTS.md 外）
project_doc_fallback_filenames = ["README.md"]

# 项目根标记文件（用于 AGENTS.md 向上搜索停止点）
project_root_markers = [".git", "pyproject.toml"]
```

### 用户指令配置

```toml
# 全局固定的用户指令（每次 Turn 注入）
instructions = "请用中文回答"
```

---

## 9. ThreadMemoryMode：线程级记忆开关

每个线程（Thread）可以独立设置记忆生成开关：

```rust
// protocol/src/protocol.rs:665
#[serde(rename_all = "lowercase")]   // 线上序列化为 "enabled" / "disabled"
pub enum ThreadMemoryMode {
    Enabled,
    Disabled,
}
```

通过 `Op::SetThreadMemoryMode { mode }` 控制：

```rust
// Op 变体定义：protocol/src/protocol.rs:635（字符串映射 "set_thread_memory_mode" 在 :756）
Op::SetThreadMemoryMode { mode } => {
    // 仅更新 state DB 中该线程的 memory_mode 元数据
    // 不触发任何 LLM 调用
}
```

这个状态存储在 state DB（SQLite）的线程元数据中。Phase 1 在选择 rollout 时会跳过 `memory_mode = "disabled"` 的线程。

**污染标记**（`memory_mode = "polluted"`）：注意 `"polluted"` **不是** `ThreadMemoryMode` 枚举的变体（该枚举只有 `Enabled` / `Disabled`），而是 state DB 中 `memory_mode` 字段额外的内部取值。若线程使用了 MCP 工具或 Web Search（且开启了 `disable_on_external_context`，见 §8），系统自动把该线程标记为 polluted，不生成长期记忆。这是为了防止外部动态内容污染记忆（如今天的天气、临时 API 输出）。

---

## 10. 核心代码路径索引

> 记忆系统横跨三个 crate，先记住分工：
> - **`codex-memories-write`**（`memories/write/`）：写入流水线（start / Phase 1 / Phase 2 / storage / prompts）。
> - **`codex-memories-read`**（`memories/read/`）：引用解析（citations）、usage 指标、`memory_root` 路径助手；被 core 直接依赖。
> - **`codex-memories-extension`**（`ext/memories/`）：读取注入侧 + 专用记忆工具；通过扩展机制装进宿主（app-server）。

| 功能 | 文件 | 行号 | 说明 |
|------|------|------|------|
| **短期记忆** | | | |
| ContextManager 定义 | `core/src/context_manager/history.rs` | 34 | `pub(crate) struct`，`items: Vec<ResponseItem>` |
| 历史 → LLM 输入 | `core/src/context_manager/history.rs` | 119 | `for_prompt(self, &[InputModality]) -> Vec<ResponseItem>` |
| 追加历史 | `core/src/session/mod.rs` | 2482 | `record_conversation_items()` |
| 读取历史快照 | `core/src/session/mod.rs` | 2852 | `clone_history()` |
| Compact 操作 | `core/src/session/handlers.rs` | ~ | `compact()` handler |
| **长期记忆（写入：crate codex-memories-write）** | | | |
| 启动记忆流水线 | `memories/write/src/start.rs` | 22 | `start_memories_startup_task()`；调用点 `app-server/.../turn_processor.rs:462` |
| Phase 1 运行 | `memories/write/src/phase1.rs` | 70 | `run()` 提炼 rollout |
| Phase 2 运行 | `memories/write/src/phase2.rs` | 45 | `run()` 整合记忆 |
| Phase 1/2 模型常量 | `memories/write/src/lib.rs` | 79 / 104 | `gpt-5.4-mini` / `gpt-5.4`（可被 config 覆盖） |
| 并发/扫描/裁剪常量 | `memories/write/src/lib.rs` | 82 / 85 / 100 | `CONCURRENCY_LIMIT=8` / `THREAD_SCAN_LIMIT=5000` / `CONTEXT_WINDOW_PERCENT=70` |
| Phase 1 系统提示词 | `memories/write/templates/memories/stage_one_system.md` | - | Phase 1 LLM 的 system prompt |
| Phase 2 整合提示词 | `memories/write/templates/memories/consolidation.md` | - | Phase 2 整合 Agent 的提示词 |
| **长期记忆（读取注入：crate codex-memories-extension）** | | | |
| 扩展安装入口 | `ext/memories/src/lib.rs` | 9 | `pub use extension::install`；token 上限常量在 `:16`（=2500） |
| 注入实现（ContextContributor）| `ext/memories/src/extension.rs` | 49–66 | `contribute()` → `PromptFragment::developer_policy` |
| 启用 gate | `ext/memories/src/extension.rs` | 42 | `MemoryTool` 已启用 && `use_memories` |
| 安装点（宿主侧）| `app-server/src/extensions.rs` | 33 | `codex_memories_extension::install(...)` |
| 读取 memory_summary | `ext/memories/src/prompts.rs` | 27 | 读文件 + trim + 截断到 2500 token，空则返回 None |
| 注入模板 | `ext/memories/templates/memories/read_path.md` | - | 完整的记忆使用指令模板 |
| feature flag | `features/src/lib.rs` | 116 / 825 | `Feature::MemoryTool`（key=`"memories"`） |
| **AGENTS.md** | | | |
| AGENTS.md 管理器 | `core/src/agents_md.rs` | 48 | `struct AgentsMdManager<'a>` |
| 拼接内容 | `core/src/agents_md.rs` | 97 | `user_instructions()` |
| 发现文件路径 | `core/src/agents_md.rs` | 243 | `agents_md_paths()` 向上遍历 |
| 全局 AGENTS.md | `core/src/agents_md.rs` | 62 | `load_global_instructions()` |
| 本地覆盖文件名 | `core/src/agents_md.rs` | 40 | `LOCAL_AGENTS_MD_FILENAME = "AGENTS.override.md"` |
| 注入到 prompt | `core/src/session/mod.rs` | 2776 | `user_instructions` → `UserInstructions.render()` |
| **配置** | | | |
| MemoriesToml / MemoriesConfig | `config/src/types.rs` | 261 / 294 | TOML 形态 / 应用默认后的有效配置 |
| ThreadMemoryMode | `protocol/src/protocol.rs` | 665 | `Enabled` / `Disabled`（serde lowercase） |
| Op::SetThreadMemoryMode | `protocol/src/protocol.rs` | 635 | 设置线程记忆开关（字符串映射 `:756`） |

---

## 整体架构图

```
┌─────────────────────────────────────────────────────────────────┐
│                        每次 Turn 开始                            │
│                                                                 │
│  system/developer message:                                      │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │ 1. base_instructions（模型默认指令）                     │    │
│  │ 2. developer_instructions（来自 config）                 │    │
│  │ 3. 长期记忆（memory_summary.md）       ← ~/.codex/memories │   │
│  │    + 记忆使用指南（read_path.md 模板）                   │    │
│  │ 4. collaboration_mode 指令                               │    │
│  │ 5. 环境上下文（shell、cwd、平台）                        │    │
│  └─────────────────────────────────────────────────────────┘    │
│                                                                 │
│  user message:                                                  │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │ 6. AGENTS.md 内容（项目根 → cwd 层级）  ← 项目文件系统   │    │
│  │    + config.user_instructions                            │    │
│  └─────────────────────────────────────────────────────────┘    │
│                                                                 │
│  conversation history（短期记忆）:                              │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │ 7. ContextManager 全量历史                               │    │
│  │    （本 Session 内所有对话轮次）                          │    │
│  └─────────────────────────────────────────────────────────┘    │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────┐
│                    后台：长期记忆流水线                           │
│                   （Session 启动时 tokio::spawn）                │
│                                                                 │
│  rollout 文件（历史对话） → Phase 1（gpt-5.4-mini）              │
│                             ↓ raw_memory + rollout_summary       │
│                          state DB（SQLite）                      │
│                             ↓                                   │
│                          Phase 2（gpt-5.4 整合 Agent）           │
│                             ↓ 更新 memory 文件                  │
│                          ~/.codex/memories/                     │
│                          ├── memory_summary.md  ← 下次启动注入  │
│                          ├── MEMORY.md          ← LLM 按需读取  │
│                          └── rollout_summaries/ ← LLM 按需引用  │
└─────────────────────────────────────────────────────────────────┘
```

---

## 常见问题

**Q: 长期记忆对应 ChatGPT 的哪个功能？**

对应 ChatGPT 的 "Memory" 功能。ChatGPT 会记住你说过的事情（"我是素食主义者"、"我在做一个 Python 项目"），下次对话自动应用。Codex 的长期记忆也做同样的事，但实现是通过异步后台流水线提炼 rollout 文件，而非实时记录。"简介"在 Codex 里对应的是 `memory_summary.md`，它始终被注入到每次对话的系统提示词中。

**Q: 长期记忆是实时更新的吗？**

不是。长期记忆在 Session **启动时**（后台异步）更新，用的是**上一次 Session 的历史**。当前 Session 的对话内容在本次 Session 结束后才会被 Phase 1 提炼。

**Q: LLM 能直接写 MEMORY.md 吗？**

不能直接写。LLM 只能在用户明确要求时，在 `extensions/ad_hoc/notes/` 目录写一个小文件（update note）。Phase 2 会在下次启动时将该 note 整合进 MEMORY.md。这是为了防止 LLM 在没有用户意图的情况下随意修改记忆。

**Q: AGENTS.md 和长期记忆同时存在时，优先级如何？**

两者不冲突，都会注入到提示词中：
- AGENTS.md → user message（项目级指令，开发者设定）
- 长期记忆 → developer message（用户级偏好，LLM 自动学习）

LLM 会同时考虑两者，AGENTS.md 的项目特定指令可以覆盖长期记忆中的通用偏好。
