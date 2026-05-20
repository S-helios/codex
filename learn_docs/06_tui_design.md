# 06 - TUI 界面设计
> 文档编号：06 | 类型：UI 设计 | 适合读者：想参与 TUI 开发的开发者

---

## 目录

1. [TUI 整体布局](#1-tui-整体布局)
2. [App struct 架构](#2-app-struct-架构)
3. [事件模型](#3-事件模型重点)
4. [ChatWidget 设计](#4-chatwidget-设计)
5. [BottomPane 设计](#5-bottompane-设计)
6. [键位映射系统（Keymap）](#6-键位映射系统keymap)
7. [渲染架构](#7-渲染架构)
8. [Markdown 渲染](#8-markdown-渲染)
9. [ANSI 转 Ratatui](#9-ansi-转-ratatui)
10. [样式规范](#10-样式规范)

---

## 1. TUI 整体布局

Codex TUI 基于 **Ratatui 0.29** + **Crossterm 0.28** 构建，位于 `codex-rs/tui/` crate。整个界面分为三个垂直排列的主要区域：

```
┌──────────────────────────────────────────────────────────────────┐
│                                                                  │
│  SessionHeader（会话顶部信息条）                                   │
│  model: gpt-4o  ·  dir: ~/my-project  ·  [yolo]                 │
│                                                                  │
├──────────────────────────────────────────────────────────────────┤
│                                                                  │
│  ╭──────────────────────────────────────────────────────────╮   │
│  │  UserHistoryCell（用户消息气泡）                           │   │
│  │  ▶ 请帮我分析一下这个文件                                   │   │
│  ╰──────────────────────────────────────────────────────────╯   │
│                                                                  │
│  AgentMarkdownCell（AI 文字输出，Markdown 渲染）                  │
│  我来分析一下这个文件：                                            │
│  1. 函数 `foo()` 做了...                                         │
│  2. 类 `Bar` 实现了...                                           │
│                                                                  │
│                                                                  │  ← ChatWidget（主对话区）
│  ┌──────────────────────────────────────────────────────────┐   │    committed_cells + active_cell
│  │  ExecCell（命令执行）                                      │   │
│  │  $ cargo test -p codex-tui                               │   │
│  │  running 42 tests ...                                    │   │
│  │  ✓ all tests passed  (2.3s)                              │   │
│  └──────────────────────────────────────────────────────────┘   │
│                                                                  │
│  PatchHistoryCell（文件修改摘要）                                  │
│  M  src/lib.rs                                                   │
│  +  src/new_module.rs                                            │
│                                                                  │
│  FinalMessageSeparator（耗时分隔线）                               │
│  ─────────── worked for 12s · 1,234 tokens ───────────          │
│                                                                  │
│  ActiveCell（流式输出中的气泡 ← 正在生成）  ◐                      │
│  接下来我会...                                                    │
│                                                                  │
├──────────────────────────────────────────────────────────────────┤
│                                                                  │
│  StatusLine（底部状态行）                                         │
│  gpt-4o  ·  main  ·  ctx 42%  ·  ↑ Enter 发送                  │
│                                                                  │
│  BottomPane（底部交互区）                          ← 多视图切换    │
│  ┌──────────────────────────────────────────────────────────┐   │
│  │  ChatComposer（默认：文本输入框）                           │   │
│  │  > 在这里输入消息...              [Ctrl+K 命令 | ? 帮助]   │   │
│  └──────────────────────────────────────────────────────────┘   │
│  [当前可切换为：ApprovalOverlay / SelectionPicker /              │
│   RequestUserInputView / McpElicitationForm /                    │
│   FileSearchPopup / CommandPopup / FeedbackView ...]             │
│                                                                  │
└──────────────────────────────────────────────────────────────────┘
```

### 各区域职责说明

| 区域 | 所在代码 | 职责 |
|------|---------|------|
| **SessionHeader** | `chatwidget/session_header.rs` | 顶部会话信息条，展示当前模型名、工作目录、权限模式（如 yolo）、快速设置入口 |
| **ChatWidget** | `tui/src/chatwidget.rs` | 主对话区，管理消息气泡列表、滚动、流式输出、spinner 动画、Markdown 渲染 |
| **HistoryCell 列表** | `tui/src/history_cell.rs` | 对话历史中的已完成消息气泡（committed_cells） |
| **ActiveCell** | `chatwidget.rs` 中 `active_cell` | 当前正在流式输出的气泡，渲染时追加到列表末尾 |
| **BottomPane** | `tui/src/bottom_pane/` | 底部交互区，根据当前状态切换不同视图（输入框、审批弹窗、文件搜索等） |
| **StatusLine** | `bottom_pane/footer.rs` | 底部状态行，展示模型、Git 分支、上下文使用量、键位提示等可配置内容 |

---

## 2. App struct 架构

`App`（`tui/src/app.rs`）是整个 TUI 的**顶层协调者**，负责将所有子组件粘合在一起。

```
                        ┌─────────────────────────────────────────┐
                        │              App (pub(crate))           │
                        │                                         │
                        │  ┌───────────────┐  ┌───────────────┐  │
                        │  │  ChatWidget   │  │  BottomPane   │  │
                        │  │  (对话逻辑)    │  │  (通过内部持有) │  │
                        │  └───────────────┘  └───────────────┘  │
                        │           ↑ ChatWidget 内部持有          │
                        │  ┌─────────────────────────────────────┐│
                        │  │        AppServerSession             ││
                        │  │   (连接 app-server 的 HTTP 客户端)   ││
                        │  └─────────────────────────────────────┘│
                        │  ┌─────────────────────────────────────┐│
                        │  │       FileSearchManager             ││
                        │  │      (模糊文件搜索功能)               ││
                        │  └─────────────────────────────────────┘│
                        │  ┌─────────────────────────────────────┐│
                        │  │       Arc<ModelCatalog>             ││
                        │  │        (可用模型列表)                 ││
                        │  └─────────────────────────────────────┘│
                        │  ┌─────────────────────────────────────┐│
                        │  │         RuntimeKeymap               ││
                        │  │     (运行时可配置键位映射)             ││
                        │  └─────────────────────────────────────┘│
                        │  ┌─────────────────────────────────────┐│
                        │  │    transcript_cells / overlay       ││
                        │  │   (Ctrl+T 全屏 Transcript 覆盖层)    ││
                        │  └─────────────────────────────────────┘│
                        │                                         │
                        │  全局状态：                              │
                        │  · active_thread_id / primary_thread_id │
                        │  · thread_event_channels (多 agent)     │
                        │  · backtrack（Esc 回退状态）             │
                        │  · pending_update_action（更新提示）     │
                        │  · windows_sandbox（沙箱状态）           │
                        └─────────────────────────────────────────┘
```

### App 的子模块（`tui/src/app/`）

App struct 的实现被拆分为多个子模块，每个模块负责一类关注点：

```
tui/src/app/
├── agent_navigation.rs       — 多 agent 导航状态（主/子 agent 切换）
├── app_server_adapter.rs     — 与 app-server 协议层的适配转换
├── app_server_requests.rs    — 待处理的 app-server 请求（审批、输入等）
├── background_requests.rs    — 后台异步请求（速率限制拉取等）
├── config_persistence.rs     — 配置写入持久化（模型、键位等）
├── event_dispatch.rs         — AppEvent 分发中心（最大的子模块）
├── history_ui.rs             — 历史记录 UI 操作（滚动、展开等）
├── input.rs                  — 键盘输入事件处理
├── loaded_threads.rs         — 已加载 thread 管理（子 agent threads）
├── pending_interactive_replay.rs — 恢复会话时的交互回放缓冲
├── platform_actions.rs       — 平台相关操作（打开外部编辑器等）
├── replay_filter.rs          — 回放事件过滤（跳过已完成工具调用等）
├── resize_reflow.rs          — 终端 resize 时的文本回流计算
├── session_lifecycle.rs      — 会话启动/停止/恢复生命周期管理
├── side.rs                   — Side Conversation（旁路对话）状态
├── startup_prompts.rs        — 启动时的引导提示（信任 NUX 等）
├── thread_events.rs          — 处理来自 app-server 的 thread 事件
├── thread_goal_actions.rs    — Thread Goal（线程目标）操作
├── thread_routing.rs         — 多 thread 事件路由（主/子 agent）
└── thread_session_state.rs   — Thread 级别的会话状态快照
```

### App::run() 的主事件循环结构

```rust
// 伪代码，展示 tokio::select! 结构
loop {
    tokio::select! {
        // 1. 来自终端的 TuiEvent（键盘、粘贴、Resize、Draw）
        Some(tui_event) = tui_event_stream.next() => {
            app.handle_tui_event(&mut tui, &mut app_server, tui_event).await?;
        }

        // 2. 来自 app_event_rx 的内部 AppEvent
        Some(app_event) = app_event_rx.recv() => {
            app.handle_event(&mut tui, &mut app_server, app_event).await?;
        }

        // 3. 来自当前活跃 thread 的 ThreadBufferedEvent
        Some(thread_event) = active_thread_rx.recv() => {
            app.handle_thread_event(thread_event).await?;
        }
    }
}
```

---

## 3. 事件模型（重点）

TUI 中有两类事件总线并行运行，通过 `tokio::select!` 统一调度：

### 完整事件流图

```
┌─────────────────────────────────────────────────────────────────────────┐
│                        用户操作 / 外部通知                                │
└─────────────────────────────────────────────────────────────────────────┘
          │                                    │
          ▼                                    ▼
   crossterm 键盘/鼠标事件              app-server 推送事件
   粘贴事件 / 终端 Resize             (ServerNotification / ServerRequest)
          │                                    │
          ▼                                    │
  EventBroker（tui/tui/event_stream.rs）      │
  · 过滤 focus/paste 协议事件                  │
  · 处理括号粘贴序列（bracketed paste）         │
  · 检测增强键盘协议支持                        │
          │                                    │
          ▼                                    │
   TuiEvent（tui/src/tui.rs）                  │
   ├── Key(KeyEvent)                           │
   ├── Paste(String)                           │
   ├── Resize                                  │
   └── Draw（帧调度器触发重绘）                 │
          │                                    │
          ▼                                    ▼
   ┌─────────────────────────────────────────────────────────┐
   │          tokio::select!（App::run 主循环）               │
   │                                                         │
   │  · TuiEvent → App::handle_tui_event()                  │
   │  · AppEvent → App::handle_event()  ←──────────┐        │
   │  · ThreadEvent → App::handle_thread_event()   │        │
   └─────────────────────────────────────────────────────────┘
          │                        ↑
          │         通过 AppEventSender（tokio::sync::mpsc）
          │         任何组件都可以异步发送 AppEvent：
          │           · ChatWidget 内部逻辑
          │           · BottomPane 视图完成
          │           · 后台任务（文件搜索、速率限制拉取等）
          │           · app-server 响应回调
          ▼
   App::handle_event()（event_dispatch.rs）
          │
          ├──→ 会话生命周期事件   → session_lifecycle.rs
          ├──→ Thread 事件       → thread_events.rs / ChatWidget
          ├──→ UI 交互事件       → ChatWidget / BottomPane
          ├──→ 功能事件          → FileSearch / Markdown / Connectors
          └──→ App 直接处理      → Exit / Overlay / Config 等
                   │
                   ▼
          UI 状态更新完毕
                   │
                   ▼
   FrameRequester::schedule_frame()  ← 触发重绘请求
   （任何状态变更后都应调用）
                   │
                   ▼
   App::handle_tui_event() 收到 TuiEvent::Draw
                   │
                   ▼
   App::draw() → tui.draw() / tui.draw_with_resize_reflow()
                   │
                   ▼
   ChatWidget::render(frame.area, frame.buffer)
   BottomPane::render(...)
                   │
                   ▼
   Ratatui 差量渲染（diff_buffers）→ Crossterm 写入终端
```

### AppEvent 主要变体（按功能分组）

```
AppEvent（tui/src/app_event.rs）
│
├── 会话生命周期事件
│   ├── NewSession                  — 新建会话
│   ├── ResumeSessionByIdOrName     — 按 ID/名称恢复会话
│   ├── ForkCurrentSession          — 复刻当前会话
│   ├── ClearUi                     — 清空界面
│   ├── ClearUiAndSubmitUserMessage — 清空并提交新消息
│   └── Exit / FatalExitRequest    — 正常/异常退出
│
├── Thread & Turn 事件
│   ├── SubmitThreadOp              — 向指定 Thread 发送操作
│   ├── ThreadHistoryEntryResponse  — Thread 历史记录回应
│   ├── StartSide                   — 启动 Side Conversation
│   ├── SelectAgentThread           — 切换到指定 Agent Thread
│   └── ApplyThreadRollback         — 回滚 Thread 历史（含 N 轮）
│
├── 消息渲染事件
│   ├── InsertHistoryCell           — 插入一条历史气泡
│   ├── BeginInitialHistoryReplayBuffer — 开始历史回放缓冲
│   ├── EndInitialHistoryReplayBuffer   — 结束历史回放缓冲
│   ├── ConsolidateAgentMessage     — 合并连续 Agent 消息（流结束）
│   ├── ConsolidateProposedPlan     — 合并计划流
│   ├── StartCommitAnimation        — 开始 spinner 动画
│   ├── StopCommitAnimation         — 停止 spinner 动画
│   └── CommitTick                  — 动画时钟滴答
│
├── UI 交互事件
│   ├── OpenAgentPicker             — 打开 Agent 选择器
│   ├── OpenResumePicker            — 打开会话恢复列表
│   ├── OpenRealtimeAudioDeviceSelection — 打开语音设备选择
│   ├── FullScreenApprovalRequest   — 全屏显示审批请求
│   ├── LaunchExternalEditor        — 启动外部编辑器
│   ├── OpenFeedbackNote            — 打开反馈输入
│   └── OpenKeymapActionMenu        — 打开键位映射菜单
│
├── 功能事件
│   ├── StartFileSearch             — 开始文件搜索
│   ├── FileSearchResult            — 文件搜索结果返回
│   ├── SkillsListLoaded            — Skill 列表加载完成
│   ├── DiffResult                  — /diff 命令结果
│   ├── ConnectorsLoaded            — Connector 列表加载
│   ├── McpInventoryLoaded          — MCP 工具清单加载完成
│   └── StatusLineBranchUpdated    — Git 分支名更新
│
├── 模型/配置事件
│   ├── UpdateModel / UpdateReasoningEffort
│   ├── PersistModelSelection      — 持久化模型选择
│   ├── SyntaxThemeSelected        — 切换代码高亮主题
│   └── KeymapCaptured / KeymapCleared — 键位捕获/清除
│
└── 错误/告警事件
    ├── RateLimitsLoaded            — 速率限制信息加载
    ├── RefreshRateLimits           — 刷新速率限制
    └── Logout                      — 退出登录
```

---

## 4. ChatWidget 设计

`ChatWidget`（`tui/src/chatwidget.rs`）是 TUI 中最复杂的组件，承担**对话逻辑的全部职责**。

### ChatWidget 内部结构

```
┌──────────────────────────────────────────────────────────────────────┐
│                         ChatWidget                                   │
│                                                                      │
│  ┌─────────────────────────────────────────────────────────────┐    │
│  │                   BottomPane（持有者）                       │    │
│  └─────────────────────────────────────────────────────────────┘    │
│                                                                      │
│  ┌───────────────────────────────────────┐                          │
│  │         committed_cells               │  ← Vec<Arc<dyn           │
│  │  ┌──────────────────────────────┐    │     HistoryCell>>         │
│  │  │  UserHistoryCell             │    │    已完成气泡列表           │
│  │  ├──────────────────────────────┤    │                           │
│  │  │  AgentMarkdownCell           │    │                           │
│  │  ├──────────────────────────────┤    │                           │
│  │  │  ExecCell / UnifiedExecCell  │    │                           │
│  │  ├──────────────────────────────┤    │                           │
│  │  │  PatchHistoryCell            │    │                           │
│  │  ├──────────────────────────────┤    │                           │
│  │  │  FinalMessageSeparator       │    │                           │
│  │  └──────────────────────────────┘    │                           │
│  └───────────────────────────────────────┘                          │
│                                                                      │
│  ┌───────────────────────────────────────┐                          │
│  │         active_cell                   │  ← Option<Box<dyn        │
│  │  (正在流式输出的气泡，渲染在列表末尾)    │     HistoryCell>>         │
│  │  active_cell_revision: u64  (缓存键)  │                          │
│  └───────────────────────────────────────┘                          │
│                                                                      │
│  agent_turn_running: bool     ← 控制 spinner 显示                   │
│  mcp_startup_status: Option<HashMap> ← MCP 启动进度                 │
│  stream_controller            ← 流式输出生命周期控制                  │
│  plan_stream_controller       ← Plan 流式输出控制                    │
│  token_info / rate_limit_*    ← Token 使用量与速率限制               │
│  running_commands: HashMap    ← 正在执行的命令                       │
│  skills_all / connectors_cache ← 技能与连接器缓存                    │
│  last_agent_markdown          ← 最近 AI 回复（供复制用）             │
└──────────────────────────────────────────────────────────────────────┘
```

### HistoryCell 类型详解

`HistoryCell`（`tui/src/history_cell.rs`）是所有气泡的公共 trait，每种气泡实现不同的 `display_lines()` 方法：

```
HistoryCell trait
│
├── UserHistoryCell          — 用户消息气泡
│   · 带 "▶" 前缀的包裹文本
│   · 支持 @mention 标记展示
│   · 支持内嵌/远程图片引用
│
├── AgentMessageCell         — AI 文字输出（原始行，流式阶段）
├── AgentMarkdownCell        — AI 文字输出（Markdown 渲染，流结束后合并）
│   · 使用 pulldown-cmark 解析
│   · 代码块语法高亮（syntect）
│   · 流式渲染时使用 MarkdownStreamCollector
│
├── ReasoningSummaryCell     — AI 推理摘要块（扩展思维模式）
│
├── ExecCell 系列            — shell 命令执行
│   ├── UnifiedExecInteractionCell  — 交互式命令（含 stdin）
│   ├── UnifiedExecProcessesCell    — 并行进程组展示
│   └── 命令状态：运行中 | 成功 | 失败 | 超时
│       显示：命令文本 + 状态标识 + stdout/stderr 摘要 + 耗时
│
├── PatchHistoryCell         — apply_patch 文件修改
│   · 展示 diff 摘要（M/A/D + 文件路径）
│   · 颜色：新增绿色、删除红色
│
├── McpToolCallCell          — MCP 工具调用
│   · 工具名 + 参数 + 结果/错误
│   · 支持图片输出（Base64 解码）
│   · 正在执行时显示 spinner 动画
│
├── WebSearchCell            — 网络搜索
│   · 查询词 + 执行状态 + 结果摘要
│
├── ProposedPlanCell         — AI 提出的执行计划（完整 Markdown）
├── ProposedPlanStreamCell   — 计划流式输出中间态
├── PlanUpdateCell           — 计划项更新（说明 + 步骤）
│
├── SessionHeaderHistoryCell — 会话开始 header
│   · 版本号 + 模型名 + 工作目录
│   · 权限模式（yolo/normal）
│   · 推理努力级别（low/medium/high）
│
├── FinalMessageSeparator    — Turn 结束分隔线
│   · 工作耗时 + Token 用量 + 运行时指标
│
├── RequestUserInputResultCell — request_user_input 的问答记录
│
├── McpInventoryLoadingCell  — MCP 工具清单加载中动画
│
├── PlainHistoryCell         — 纯文本行
├── PrefixedWrappedHistoryCell — 带前缀的自动换行文本
├── CompositeHistoryCell     — 组合多个子 Cell
│
└── DeprecationNoticeCell    — 模型/功能弃用提示
    CyberPolicyNoticeCell    — 安全策略提示
    UpdateAvailableHistoryCell — 版本更新提示
```

### ChatWidget 子模块（`tui/src/chatwidget/`）

```
chatwidget/
├── goal_menu.rs         — Thread Goal 目标设置弹窗
├── goal_status.rs       — Thread Goal 状态指示器渲染
├── interrupts.rs        — Ctrl+C 中断逻辑（区分「中断任务」vs「退出」）
├── keymap_picker.rs     — /keymap 命令的键位映射配置 UI
├── mcp_startup.rs       — MCP 服务启动进度展示
├── plan_implementation.rs — AI 计划实现确认视图
├── plugins.rs           — 插件（Plugin）管理与展示
├── realtime.rs          — 语音实时通话模式（WebRTC）
├── reasoning_shortcuts.rs — 推理努力快捷键（Ctrl+↑/↓）
├── session_header.rs    — 会话顶部信息条（模型名）
├── side.rs              — Side Conversation（旁路对话）逻辑
├── skills.rs            — Skill 调用展示
├── slash_dispatch.rs    — 斜杠命令分发（/status、/model 等）
└── status_surfaces.rs   — StatusLine 与 TerminalTitle 状态面的管理
```

---

## 5. BottomPane 设计

`BottomPane`（`tui/src/bottom_pane/`）通过一个**视图栈（view_stack）**实现多视图互斥切换。同一时刻，栈顶视图负责渲染和处理键盘事件；栈底保留 `ChatComposer` 作为默认态。

### BottomPane 视图切换状态机

```
                   ┌─────────────────────────────────────────────┐
                   │              BottomPane                      │
                   │                                             │
                   │    view_stack: Vec<Box<dyn BottomPaneView>> │
                   │    ┌────────────────────────────────────┐   │
                   │    │  [栈顶] 当前显示的视图              │   │
                   │    ├────────────────────────────────────┤   │
                   │    │  ...（中间层，被压栈的视图）         │   │
                   │    ├────────────────────────────────────┤   │
                   │    │  [栈底] ChatComposer（永远保留）    │   │
                   │    └────────────────────────────────────┘   │
                   └─────────────────────────────────────────────┘

触发条件                          切换到的视图
──────────────────────────────────────────────────────────────────
默认 / 视图完成弹出          ───→  ChatComposer（文本输入框）
                                   · 多行编辑（Textarea widget）
                                   · @ mention 语法
                                   · 历史导航（↑/↓）

Ctrl+P                       ───→  FileSearchPopup（文件模糊搜索）
                                   · 实时过滤 · ↑/↓ 选择 · Enter 插入

Ctrl+K / 输入 "/"            ───→  CommandPopup（斜杠命令面板）
                                   · /model /status /clear /diff 等

AI 发起 ToolCallApprovalRequest ─→  ApprovalOverlay（审批弹窗）
                                   · 展示命令/补丁 · Allow/Deny/Always

AI 调用 request_user_input   ───→  RequestUserInputView
                                   · 展示 AI 提问 · 等待用户回答

MCP 服务发起 elicitation     ───→  McpServerElicitationForm
                                   · MCP 表单字段 · 用户填写并提交

/model 命令 / OpenAgentPicker ──→  ListSelectionView（列表选择）
                                   · 模型选择 · 沙箱策略 · 审批策略

触发反馈                     ───→  FeedbackView（反馈提交）
打开 App Link                ───→  AppLinkView（应用链接详情）
/experimental                ───→  ExperimentalFeaturesView
/keymap                      ───→  KeymapActionMenu / KeymapCapture
```

```
         Esc / 视图 is_complete() ──→ pop_active_view_with_completion()
              │
              ├── 如果弹出的是 ApprovalOverlay → 发送 Deny 决策
              ├── 如果弹出的是 RequestUserInput → 发送中断回应
              └── 恢复下层视图（通常是 ChatComposer）
```

### BottomPaneView Trait

所有底部视图均实现 `BottomPaneView` trait（`bottom_pane/bottom_pane_view.rs`）：

```
trait BottomPaneView: Renderable {
    fn handle_key_event(&mut self, key: KeyEvent) {}     // 处理键盘事件
    fn is_complete(&self) -> bool { false }              // 是否已完成（弹出）
    fn completion(&self) -> Option<ViewCompletion>       // 完成原因（Accept/Cancel）
    fn on_ctrl_c(&mut self) -> CancellationEvent         // 处理 Ctrl+C
    fn prefer_esc_to_handle_key_event(&self) -> bool     // Esc 路由到 handle_key_event
    fn handle_paste(&mut self, pasted: String) -> bool   // 处理粘贴
    fn terminal_title_requires_action(&self) -> bool     // 是否需要"Action Required"标题
    // 审批/输入请求消费方法
    fn try_consume_approval_request(...)
    fn try_consume_user_input_request(...)
    fn try_consume_mcp_server_elicitation_request(...)
    fn dismiss_app_server_request(...)
}
```

### ChatComposer 关键特性

`ChatComposer`（`bottom_pane/chat_composer.rs`）是默认视图，提供功能最丰富的文本输入体验：

```
ChatComposer 功能列表
│
├── 多行文本编辑
│   · 基于自研 Textarea widget（`bottom_pane/textarea.rs`）
│   · Emacs 风格移动快捷键（Ctrl+A/E/K/Y 等）
│   · Shift+Enter 插入换行符
│
├── @ mention 语法
│   · @filename → 插入文件路径引用（触发 FileSearchPopup）
│   · @skill → 引用已安装的 Skill
│   · @plugin → 引用插件
│   · MentionBinding 记录 mention 文本 ↔ 实际路径的映射
│
├── 命令历史
│   · ↑/↓ 翻历史消息（chat_composer_history.rs）
│   · Ctrl+R 搜索历史（history_search_previous/next）
│
├── 斜杠命令（slash_commands.rs）
│   · /model   — 切换模型
│   · /status  — 查看使用量
│   · /clear   — 清空对话
│   · /diff    — 查看文件差异
│   · /keymap  — 配置键位映射
│   · /memory  — 内存设置
│   · 输入 "/" 自动弹出 CommandPopup 补全列表
│
├── 粘贴处理（paste_burst.rs）
│   · 检测 burst paste（短时间内多次粘贴）
│   · 批量合并粘贴内容，避免误操作
│   · flush_paste_burst_if_due()：基于时间的刷新
│
├── StatusLine 渲染（footer.rs）
│   · 可配置的底部状态行（模型、Git 分支、上下文用量等）
│   · FooterMode 决定当前显示内容（contextual / instructional）
│   · 宽度响应：终端过窄时自动折叠部分内容
│
└── 特殊输入模式
    · 任务运行中：禁用 Enter 提交，显示"Ctrl+C 中断"提示
    · Pending steers：AI 响应途中的"插话"队列机制
    · 外部编辑器集成（Ctrl+E / EDITOR 环境变量）
```

---

## 6. 键位映射系统（Keymap）

TUI 键位映射系统允许用户在 `config.toml` 中自定义快捷键，运行时动态生效。

### RuntimeKeymap 结构

```
RuntimeKeymap（tui/src/keymap.rs）
│
├── app: AppKeymap
│   · open_transcript         默认 Ctrl+T    — 全屏查看对话历史
│   · open_external_editor    默认 Ctrl+E    — 启动外部编辑器
│   · copy                    默认 Ctrl+O    — 复制最近 AI 回复
│   · clear_terminal          默认 Ctrl+L    — 清空终端滚动缓冲
│
├── chat: ChatKeymap
│   · decrease_reasoning_effort  Ctrl+↓   — 降低推理努力级别
│   · increase_reasoning_effort  Ctrl+↑   — 提升推理努力级别
│   · edit_queued_message        —        — 编辑待发消息
│
├── composer: ComposerKeymap
│   · submit                  默认 Enter     — 提交消息
│   · queue                   默认 Alt+Enter — 排队消息（不立即发送）
│   · toggle_shortcuts        默认 Shift+?   — 切换快捷键提示
│   · history_search_previous 默认 Ctrl+R /↑ — 翻上条历史
│   · history_search_next     默认 ↓         — 翻下条历史
│
├── editor: EditorKeymap（Textarea 内编辑快捷键）
│   · insert_newline          Shift+Enter / Ctrl+J
│   · move_left/right/up/down ← → ↑ ↓
│   · move_word_left/right    Alt+B / Alt+F（Emacs 风格）
│   · move_line_start/end     Ctrl+A / Ctrl+E
│   · delete_backward         Backspace
│   · delete_forward          Delete
│   · delete_backward_word    Ctrl+W / Alt+Backspace
│   · delete_forward_word     Alt+D
│   · kill_line_start         Ctrl+U
│   · kill_line_end           Ctrl+K
│   · yank                    Ctrl+Y
│
├── pager: PagerKeymap（Transcript 全屏覆盖层）
│   · scroll_up/down          ↑ / ↓ / j / k
│   · page_up/down            PageUp / PageDown / Ctrl+B / Ctrl+F
│   · half_page_up/down       Ctrl+U / Ctrl+D
│   · jump_top/bottom         g / G
│   · close                   Esc / q
│   · close_transcript        Ctrl+T（切换回来）
│
├── list: ListKeymap（选择列表通用）
│   · move_up/down            ↑ / ↓
│   · accept                  Enter
│   · cancel                  Esc
│
└── approval: ApprovalKeymap（审批视图）
    · open_fullscreen          Ctrl+Shift+A  — 全屏查看命令
    · open_thread              Ctrl+Shift+T  — 查看线程历史
    · approve                  a             — 批准（本次）
    · approve_for_session      s             — 本 Session 内全部批准
    · approve_for_prefix       p             — 相同前缀全部批准
    · deny                     d             — 拒绝
    · decline                  r             — 拒绝（保留任务）
    · cancel                   Esc           — 取消（中断）
```

### 键位映射的保留与冲突验证

```
config.toml 中配置示例：
  [tui.keymap.app]
  open_transcript = ["ctrl+t", "ctrl+shift+t"]

  [tui.keymap.editor]
  kill_line_end = "ctrl+k"     # 覆盖默认绑定

验证规则（validate_conflicts）：
  · Composer 快捷键不能与 App 快捷键冲突
  · Editor 快捷键不能与主界面固定快捷键冲突
  · Approval 快捷键不能与 App/List 快捷键冲突
  · 固定快捷键（Ctrl+C / Ctrl+Z 等）不可覆盖

MAIN_RESERVED_BINDINGS（不可重新绑定）：
  · Ctrl+C  — 中断/退出
  · Ctrl+Z  — 挂起进程（Unix）
  · Ctrl+\  — SIGQUIT（Unix）

TRANSCRIPT_BACKTRACK_RESERVED_BINDINGS：
  · 在 Transcript 叠加层中保留的 Pager 专用快捷键
```

---

## 7. 渲染架构

### 渲染流程总览

```
每帧渲染流程：

FrameRequester::schedule_frame()  ← 各组件调用触发重绘
        │
        ▼
FrameScheduler（tokio task）
  · 合并短时间内的多次请求（coalescing）
  · FrameRateLimiter：限制最高 120 FPS（每帧 ≥ 8.33ms）
  · 发送广播信号到 draw_tx channel
        │
        ▼
TUI 主循环收到 TuiEvent::Draw
        │
        ▼
App::handle_tui_event(Draw)
  · pre_draw_tick()：widget 处理定时任务（hook 超时、paste flush 等）
  · 计算 desired_height（ChatWidget 总高度）
        │
        ├── 有 resize reflow → tui.draw_with_resize_reflow()
        └── 普通绘制      → tui.draw()
              │
              ▼
        Tui::draw() / draw_with_resize_reflow()
          · update_inline_viewport()：计算当前 inline viewport 区域
          · terminal.try_draw()：
              1. 创建新 Frame（Buffer）
              2. 调用渲染闭包：
                 ChatWidget::render(area, buffer)
                 cursor_pos → frame.set_cursor_position()
              3. diff_buffers()：计算新旧 Buffer 差异
              4. draw()：将差异转为 DrawCommand
              5. backend.flush()：写入终端（Crossterm）
          · SynchronizedUpdate：先发 DCS=2s，后发 DCS=2e
            （防止渲染过程中终端撕裂）
```

### Renderable Trait

所有可渲染组件实现 `Renderable` trait（`render/renderable.rs`）：

```rust
trait Renderable {
    fn render(&self, area: Rect, buf: &mut Buffer);
    fn desired_height(&self, width: u16) -> u16;
    fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> { None }
}
```

### ChatWidget 的布局计算

```
ChatWidget::render(area, buffer)
│
├── 计算 BottomPane.desired_height(width)
├── ChatArea = area - bottom_height（顶部对话区）
│
├── 渲染 committed_cells（从底部向上，按需截断）
│   · 每个 cell: cell.render(cell_area, buffer)
│   · 超出 ChatArea 顶部的 cell 直接跳过
│
├── 渲染 active_cell（流式输出中的气泡）
│   · 追加在 committed_cells 之后
│   · 使用 shimmer 效果（亮度波动）
│   · SpinnerFrame（commit_anim_running）：ASCII 动画帧轮播
│
└── 渲染 BottomPane
    · 状态行（StatusLine）
    · 当前活跃视图（ChatComposer / 弹窗等）
    · 可选：pending_input_preview（队列消息预览）
    · 可选：unified_exec_footer（并行进程摘要）
```

### FrameRequester 内部结构

```
FrameRequester（轻量可 Clone 的句柄）
    │ mpsc::UnboundedSender<Instant>
    ▼
FrameScheduler（专用 tokio task，actor 模式）
    │
    ├── 持有 FrameRateLimiter
    │   · 记录上次发送时刻
    │   · clamp_deadline()：若请求过于频繁，延迟到最小帧间隔后
    │
    ├── tokio::select!:
    │   · 收到新的 draw_at → 更新 next_deadline（取最小值）
    │   · 睡眠到达 next_deadline → 发送 draw_tx.send(())
    │
    └── 保证：多次并发请求合并为单次广播通知
```

---

## 8. Markdown 渲染

TUI 的 Markdown 渲染分为**批量渲染**和**流式渲染**两个场景。

### 批量渲染（turn 完成后）

```
AgentMarkdownCell::display_lines()
        │
        ▼
crate::markdown::append_markdown(source, width, cwd, lines)
        │
        ▼
markdown_render::render_markdown_text_with_width_and_cwd()
        │
        ├── pulldown_cmark::Parser::new_ext(source, Options::all())
        │   解析 Markdown 事件流：
        │   · Text / Code / Html
        │   · Heading / Paragraph / BlockQuote
        │   · List（有序/无序）/ ListItem
        │   · CodeBlock（fenced / indented）
        │   · Link / Image / Rule / Table
        │
        ├── MarkdownStyles（样式映射）：
        │   · h1 → bold + underlined
        │   · h2 → bold
        │   · h3 → bold + italic
        │   · h4~h6 → italic
        │   · code → cyan
        │   · emphasis → italic
        │   · strong → bold
        │   · strikethrough → crossed_out
        │   · link → cyan + underlined
        │   · blockquote → green（引用块整体绿色）
        │   · list markers → light_blue（列表标记浅蓝）
        │
        ├── 代码块高亮（render/highlight.rs）：
        │   · highlight_code_to_lines(code, lang)
        │   · 使用 syntect crate 解析语言语法
        │   · 用户可通过 /theme 命令切换高亮主题
        │   · 不支持的语言回退到纯 cyan 样式
        │
        ├── 文件链接特殊处理：
        │   · 本地文件路径显示相对于 session cwd 的相对路径
        │   · 规范化 #L123 行号格式
        │
        └── 文本自动换行（wrapping.rs）：
            · textwrap::wrap / adaptive_wrap_line
            · 不切割 URL 类 token
            · 支持 initial_indent / subsequent_indent
```

### 流式渲染（AI 输出过程中）

```
MarkdownStreamCollector（tui/src/markdown_stream.rs）
│
├── push_delta(chunk: &str)
│   · 追加到内部 buffer
│
├── commit_complete_lines() → Vec<Line>
│   · 只提交已完成行（以换行符结尾）
│   · 避免在行中间触发重新解析
│   · 特殊处理：
│     - 标题（## Heading）：确保前面有空行
│     - 列表/引用块：检测块结构完整性
│     - 代码围栏（```...```）：整块一次提交
│     - 表格（| ... |）：检测到头行后整行提交
│
├── finalize_and_drain() → Vec<Line>
│   · turn 结束时：提交所有剩余内容（含不完整行）
│   · 完整重新解析确保样式正确
│
└── set_width(width: u16)
    · 当终端宽度变化时通知收集器重新计算换行
```

---

## 9. ANSI 转 Ratatui

命令执行的输出通常包含 **ANSI 颜色/格式转义码**，TUI 需要将其转换为 Ratatui 的 `Line<'static>`。

```
命令原始输出（含 ANSI 转义码）
  例如："\x1b[32mOK\x1b[0m  src/main.rs"

        │
        ▼
codex-ansi-escape crate（codex-rs/ansi-escape/）
  · 解析 ANSI CSI 序列（颜色、粗体、下划线等）
  · 提取 SGR 参数（Select Graphic Rendition）
  · 将字节流转换为带样式的文本片段

        │
        ▼（底层使用 ansi-to-tui 库）
ansi_to_tui::ansi_to_text()
  · 输出：Vec<ratatui::text::Line<'static>>
  · ANSI 颜色码 → Ratatui Color
  · ANSI 属性码 → Ratatui Modifier（Bold / Italic / Dim 等）

        │
        ▼
ExecCell / UnifiedExecProcessesCell 渲染
  · 使用转换后的 Line 列表渲染到 Buffer
  · 保留原始终端颜色语义（成功绿/错误红/警告黄等）
  · 长输出截断：只显示最后 N 行（stderr_tail_max_lines）

  特殊场景：
  · 交互式命令（Terminal Interaction）：
    · 暂停 TUI 渲染，移交终端控制权给子进程
    · 子进程退出后恢复 TUI 状态
  · 后台命令（Background Terminal）：
    · 不中断 TUI，只在 ChatWidget 中渲染输出摘要
```

---

## 10. 样式规范

Codex TUI 的样式基于 `tui/styles.md` 中的设计规范。

### 颜色系统

```
颜色语义映射（tui/styles.md）：

  default（默认前景）  — 主要文本内容（不指定颜色）
  cyan               — 用户输入提示、选中项、状态指示器、代码块、链接
  green              — 成功、文件新增（diff +）
  red                — 错误、失败、文件删除（diff -）
  magenta            — Codex 品牌色（spinner、Session 标识）
  dim                — 次要信息（时间戳、路径、说明文字）
  bold               — 标题、重要信息
  italic             — 强调、引用

  禁用颜色（避免使用）：
  · black / white    — 使用 default/reset 代替，避免主题不兼容
  · blue / yellow    — 当前样式指南未使用
  · 硬编码 RGB 颜色  — 可能在不同终端主题下对比度差
```

### Stylize 使用规范

```rust
// ✅ 推荐写法（Ratatui Stylize trait）
"text".into()              // 普通 Span，无样式
"text".dim()               // 次要信息
"text".bold()              // 标题/重要内容
"text".cyan()              // 输入提示/选中
"text".green()             // 成功
"text".red()               // 错误/删除
"text".magenta()           // Codex 品牌
"text".cyan().underlined() // 链接

// ✅ 构建 Line
vec!["  └ ".into(), "M".red(), " ".dim(), "path.rs".dim()]
// → 转为 Line 时：vec![...].into()

// ❌ 避免写法
Span::styled("text", Style::new().fg(Color::Green)) // 冗长，优先用 Stylize
"text".white()  // 禁用
"text".blue()   // 禁用
```

### 图标与符号规范

```
功能图标对照：

  ▶       — 用户消息前缀
  ◐ ◑ ◒ ◓ — Spinner 动画帧（默认主题）
  ✓       — 成功/完成
  ✗       — 失败/错误
  ─       — 分隔线
  M / A / D — 文件修改/新增/删除状态（diff 摘要）
  └       — 目录树最后一项连接符
  ·       — 列表中间分隔点

ASCII 动画帧（tui/frames/）：
  default  — 旋转圆弧（◐◑◒◓）
  codex    — Codex 品牌动画
  openai   — OpenAI 品牌动画
  blocks   — 方块动画
  dots     — 点动画
  hash / hbars / vbars — 线条/格子动画
  shapes / slug — 形状/蛞蝓动画
  共 36 帧/组，帧率默认 80ms/帧（约 12.5 FPS）
```

### 文字处理

```
文本换行规则：
  · 纯字符串：使用 textwrap::wrap() 
  · ratatui Line：使用 tui/src/wrapping.rs 中的工具函数
    - word_wrap_lines()     — 换行一组 Line
    - word_wrap_line()      — 换行单条 Line
    - prefix_lines()        — 为所有行添加前缀（首行/后续行可不同）
    - adaptive_wrap_line()  — 保留缩进的自适应换行
  · URL 类 token 不切割（保留可点击性）
  · 极窄宽度（< 10 列）时退化为只显示前缀

行截断规则：
  · line_truncation.rs：单行超出宽度时加省略号（…）
  · 优先在词边界截断，不截断 Unicode 字素簇
```

---

## 附录：核心文件速查表

| 文件路径 | 职责简述 |
|---------|---------|
| `tui/src/app.rs` | App struct 定义与主事件循环 |
| `tui/src/app/event_dispatch.rs` | AppEvent 分发总调度 |
| `tui/src/app/session_lifecycle.rs` | 会话启动/停止/恢复 |
| `tui/src/app_event.rs` | AppEvent 枚举（全部变体）|
| `tui/src/app_event_sender.rs` | AppEventSender（线程安全发送句柄）|
| `tui/src/chatwidget.rs` | ChatWidget（最大单文件，>12000行）|
| `tui/src/chatwidget/` | ChatWidget 子模块目录 |
| `tui/src/history_cell.rs` | 所有 HistoryCell 类型实现 |
| `tui/src/bottom_pane/mod.rs` | BottomPane 主文件 |
| `tui/src/bottom_pane/bottom_pane_view.rs` | BottomPaneView trait |
| `tui/src/bottom_pane/chat_composer.rs` | 文本输入框主视图 |
| `tui/src/bottom_pane/approval_overlay.rs` | 命令/补丁审批弹窗 |
| `tui/src/bottom_pane/textarea.rs` | 自研多行文本编辑 widget |
| `tui/src/bottom_pane/footer.rs` | 底部状态行渲染逻辑 |
| `tui/src/keymap.rs` | RuntimeKeymap 与各子 keymap 定义 |
| `tui/src/keymap_setup.rs` | 键位映射配置 UI（/keymap 命令）|
| `tui/src/tui.rs` | Tui struct，TuiEvent，终端初始化 |
| `tui/src/tui/frame_requester.rs` | FrameRequester / FrameScheduler |
| `tui/src/tui/frame_rate_limiter.rs` | 120 FPS 帧率限制器 |
| `tui/src/custom_terminal.rs` | 自定义 Terminal（SynchronizedUpdate）|
| `tui/src/markdown.rs` | append_markdown() 入口 |
| `tui/src/markdown_render.rs` | pulldown-cmark 完整解析器 |
| `tui/src/markdown_stream.rs` | 流式 Markdown 状态机 |
| `tui/src/render/highlight.rs` | syntect 代码语法高亮 |
| `tui/src/render/renderable.rs` | Renderable / ColumnRenderable trait |
| `tui/src/pager_overlay.rs` | Transcript/Static 全屏覆盖层 |
| `tui/src/app_server_session.rs` | AppServerSession HTTP 客户端封装 |
| `tui/src/wrapping.rs` | Ratatui Line 换行工具函数 |
| `tui/src/frames.rs` | ASCII 动画帧数据（36帧/组）|
| `tui/styles.md` | 样式规范参考文档 |
