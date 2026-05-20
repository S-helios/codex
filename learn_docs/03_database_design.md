# Codex 数据库设计文档

**文档编号：03**
**版本：1.0**
**适用代码库：`codex-rs`（Rust 实现）**
**主要 Crate：`codex-state`（`codex-rs/state/`）**

---

## 目录

1. [总体架构](#1-总体架构)
2. [主状态库（state.db）](#2-主状态库statedb)
   - [threads 表](#21-threads-表)
   - [thread_dynamic_tools 表](#22-thread_dynamic_tools-表)
   - [stage1_outputs 表（memories）](#23-stage1_outputs-表memories)
   - [jobs 表](#24-jobs-表)
   - [agent_jobs 表](#25-agent_jobs-表)
   - [agent_job_items 表](#26-agent_job_items-表)
   - [thread_spawn_edges 表](#27-thread_spawn_edges-表)
   - [remote_control_enrollments 表](#28-remote_control_enrollments-表)
   - [device_key_bindings 表](#29-device_key_bindings-表)
   - [thread_goals 表](#210-thread_goals-表)
3. [日志库（logs.db）](#3-日志库logsdb)
   - [logs 表](#31-logs-表)
4. [表关系 ER 图](#4-表关系-er-图)
5. [索引设计说明](#5-索引设计说明)
6. [JSONL Rollout 文件格式](#6-jsonl-rollout-文件格式)
7. [Migration 演化历史](#7-migration-演化历史)
8. [关键查询场景](#8-关键查询场景)
9. [数据库文件位置说明](#9-数据库文件位置说明)

---

## 1. 总体架构

Codex 的持久化层由三个部分组成，各自承担不同职责：

```
~/.codex/
├── state.db              ← 主状态库：存储会话元数据、任务状态等结构化数据
├── logs.db               ← 日志库：存储运行时 tracing 日志（独立 DB，可单独清理）
└── sessions/
    ├── rollout-{ts}-{uuid}.jsonl   ← 活跃会话的完整对话历史（JSONL 格式）
    └── ...
└── archived_sessions/
    └── rollout-{ts}-{uuid}.jsonl   ← 已归档会话（同格式）
```

### 设计原则

| 存储层 | 格式 | 职责 | 优先级 |
|--------|------|------|--------|
| `state.db` | SQLite（通过 SQLx） | 会话元数据、任务调度、记忆摘要等快速查询场景 | 主索引，支持列表和搜索 |
| `logs.db` | SQLite（通过 SQLx） | 运行时日志，独立存储便于按时间窗口清理 | 诊断、审计 |
| `sessions/*.jsonl` | JSONL（换行分隔 JSON） | 完整对话历史的权威来源（source of truth） | 归档、重建、记忆提取 |

**重要设计决策**：`state.db` 是 JSONL 文件的**索引**，而非权威来源。当 JSONL 文件存在但 `state.db` 中缺少对应记录时，系统会通过 **backfill** 机制（migration 0008）重建 `state.db` 中的元数据。

### Crate 对应关系

- `codex-state`（`codex-rs/state/`）：数据库访问层，定义 `StateRuntime`，管理 SQLite 连接池
- `codex-rollout`（`codex-rs/rollout/`）：JSONL 文件的读写，以及从文件提取元数据到 `state.db` 的 backfill 逻辑
- `codex-protocol`（`codex-rs/protocol/`）：定义 `RolloutLine`、`RolloutItem`、`SessionMeta` 等序列化类型

---

## 2. 主状态库（state.db）

- **文件路径**：`{sqlite_home}/state.db`（默认 `~/.codex/state.db`，可通过 `CODEX_SQLITE_HOME` 环境变量覆盖）
- **Migration 目录**：`codex-rs/state/migrations/`
- **Migration 工具**：SQLx 内置 `sqlx::migrate!()`，对应常量 `STATE_DB_VERSION = 5`

### 2.1 `threads` 表

**创建 Migration**：`0001_threads.sql`
**功能**：核心会话（Thread）元数据索引表，每行对应一个 JSONL rollout 文件。

```sql
CREATE TABLE threads (
    -- 主键：UUID 格式的字符串，与 JSONL 文件名中的 UUID 段对应
    id                  TEXT    PRIMARY KEY,
    
    -- JSONL 文件的绝对路径
    rollout_path        TEXT    NOT NULL,
    
    -- 创建时间（Unix 秒，legacy；新代码读 created_at_ms）
    created_at          INTEGER NOT NULL,
    
    -- 最后更新时间（Unix 秒，legacy；新代码读 updated_at_ms）
    updated_at          INTEGER NOT NULL,
    
    -- 会话来源，枚举字符串，如 "cli"、"vscode"、"chatgpt"、"atlas"
    source              TEXT    NOT NULL,
    
    -- 模型供应商 ID，如 "openai"、"azure"
    model_provider      TEXT    NOT NULL,
    
    -- 会话启动时的工作目录（CWD），已规范化
    cwd                 TEXT    NOT NULL,
    
    -- 会话标题（从首条用户消息自动提取或截断）
    title               TEXT    NOT NULL,
    
    -- 沙箱策略，JSON 序列化字符串
    sandbox_policy      TEXT    NOT NULL,
    
    -- 审批模式，如 "on_request"、"never"、"always"
    approval_mode       TEXT    NOT NULL,
    
    -- 该会话消耗的 token 总数
    tokens_used         INTEGER NOT NULL DEFAULT 0,
    
    -- 是否有过用户事件（0 = 无，1 = 有）
    has_user_event      INTEGER NOT NULL DEFAULT 0,
    
    -- 是否已归档（0 = 活跃，1 = 已归档）
    archived            INTEGER NOT NULL DEFAULT 0,
    
    -- 归档时间（Unix 毫秒），archived=1 时设置
    archived_at         INTEGER,
    
    -- Git 信息（可选，会话启动时从 git 命令获取）
    git_sha             TEXT,
    git_branch          TEXT,
    git_origin_url      TEXT,
    
    -- [0005] Codex CLI 版本号，如 "0.12.0"
    cli_version         TEXT,
    
    -- [0007] 首条用户消息文本（用于列表预览）
    first_user_message  TEXT    NOT NULL DEFAULT '',
    
    -- [0013] AgentControl 生成的子 Agent 昵称（可选）
    agent_nickname      TEXT,
    
    -- [0013] 子 Agent 的角色标识，如 "planner"、"executor"
    agent_role          TEXT,
    
    -- [0018] 内存模式，如 "enabled"、"disabled"
    memory_mode         TEXT    NOT NULL DEFAULT 'enabled',
    
    -- [0020] 具体模型名称，如 "gpt-4o"、"o3"
    model               TEXT,
    
    -- [0020] 推理努力程度，如 "low"、"medium"、"high"
    reasoning_effort    TEXT,
    
    -- [0022] 子 Agent 的规范路径，如 "/planner/executor"
    agent_path          TEXT,
    
    -- [0025] 创建时间（Unix 毫秒，精度更高）- 当前主要使用字段
    created_at_ms       INTEGER,
    
    -- [0025] 最后更新时间（Unix 毫秒，精度更高）- 当前主要使用字段
    updated_at_ms       INTEGER
);
```

**触发器**（migration 0025 新增）：
- `threads_created_at_ms_after_insert`：INSERT 后若 `created_at_ms` 为空，自动填充 `created_at * 1000`
- `threads_updated_at_ms_after_insert`：同上，针对 `updated_at_ms`
- `threads_created_at_ms_after_update`：`created_at` 更新时同步更新 `created_at_ms`
- `threads_updated_at_ms_after_update`：同上，针对 `updated_at_ms`

**字段说明补充**：
- `source` 的有效值由 `SessionSource` 枚举决定（`Cli`、`VSCode`、`Custom("atlas")`、`Custom("chatgpt")` 等）
- `rollout_path` 存储 JSONL 文件的绝对路径，是从 `state.db` 导航到完整历史的关键链接
- 时间戳迁移背景：系统初期用秒精度（`created_at`/`updated_at`），0025 migration 升级为毫秒精度并保证唯一性（通过 CTE 递增避免重复毫秒）

---

### 2.2 `thread_dynamic_tools` 表

**创建 Migration**：`0004_thread_dynamic_tools.sql`
**功能**：存储每个 Thread 的动态工具定义（由 AgentControl 注入的自定义 MCP 工具）。

```sql
CREATE TABLE thread_dynamic_tools (
    -- 关联的 Thread ID（外键，级联删除）
    thread_id       TEXT    NOT NULL,
    
    -- 工具在该 Thread 中的排列顺序（0-based）
    position        INTEGER NOT NULL,
    
    -- 工具名称（在 namespace 内唯一）
    name            TEXT    NOT NULL,
    
    -- 工具描述（传递给 LLM 的工具说明）
    description     TEXT    NOT NULL,
    
    -- 工具输入 Schema（JSON 格式，符合 JSON Schema 规范）
    input_schema    TEXT    NOT NULL,
    
    -- [0019] 是否延迟加载（0=立即加载，1=首次使用时加载）
    defer_loading   INTEGER NOT NULL DEFAULT 0,
    
    -- [0026] 工具所属的命名空间（可选，用于多 MCP 服务器场景隔离）
    namespace       TEXT,
    
    PRIMARY KEY (thread_id, position),
    FOREIGN KEY (thread_id) REFERENCES threads(id) ON DELETE CASCADE
);
```

**业务逻辑说明**：
- 动态工具在会话创建时从 `SessionMeta` 中提取并持久化到此表
- `defer_loading = 1` 表示该工具的实际实现需要在运行时动态解析（例如远程 MCP 服务器上的工具）
- `namespace` 字段区分来自不同 MCP 服务器的同名工具，避免冲突

---

### 2.3 `stage1_outputs` 表（memories）

**创建 Migration**：`0006_memories.sql`
**功能**：存储记忆系统 Phase 1 的输出结果——对单个会话的对话内容提取的记忆摘要。

```sql
CREATE TABLE stage1_outputs (
    -- 关联的 Thread ID（1:1，也是主键）
    thread_id                           TEXT    PRIMARY KEY,
    
    -- 对应的 JSONL 文件最后修改时间（Unix 秒）
    -- 用于检测文件是否有新内容，需要重新生成摘要
    source_updated_at                   INTEGER NOT NULL,
    
    -- LLM 生成的原始记忆文本
    raw_memory                          TEXT    NOT NULL,
    
    -- 会话摘要（用于向 LLM 汇报该会话的历史概要）
    rollout_summary                     TEXT    NOT NULL,
    
    -- 记忆生成时间（Unix 秒）
    generated_at                        INTEGER NOT NULL,
    
    -- [0009] 会话的短标识（slug），如文件名的时间戳+uuid 部分
    rollout_slug                        TEXT,
    
    -- [0016] 该记忆被引用的次数（用于统计记忆使用频率）
    usage_count                         INTEGER,
    
    -- [0016] 最后一次被引用的时间（Unix 秒）
    last_usage                          INTEGER,
    
    -- [0017] 是否被选中参与 Phase 2 全局记忆汇总（0=未选，1=已选）
    selected_for_phase2                 INTEGER NOT NULL DEFAULT 0,
    
    -- [0018] Phase 2 处理时记录的快照时间戳，用于增量处理
    selected_for_phase2_source_updated_at INTEGER,
    
    FOREIGN KEY (thread_id) REFERENCES threads(id) ON DELETE CASCADE
);
```

**记忆系统架构说明**：
- **Stage 1**（per-thread）：对单个会话的 JSONL 对话历史运行 LLM 分析，生成 `raw_memory`（结构化事实）和 `rollout_summary`（自然语言摘要），存入此表
- **Stage 2（Phase 2）**（global）：从多个 Stage 1 输出中选取，汇总为全局记忆，供后续会话参考
- `jobs` 表协调这两个阶段的任务分发和重试（见 2.4 节）

---

### 2.4 `jobs` 表

**创建 Migration**：`0006_memories.sql`（与 `stage1_outputs` 同一 migration）
**功能**：通用的后台任务调度表，当前用于记忆生成的 Stage 1 和 Phase 2 两类作业。

```sql
CREATE TABLE jobs (
    -- 任务类型，当前有效值：
    --   "memory_stage1"               - 对单个会话生成记忆摘要
    --   "memory_consolidate_global"   - 汇总全局记忆
    kind                    TEXT    NOT NULL,
    
    -- 任务的唯一键（对于 Stage 1 是 thread_id，对于 Phase 2 是固定字符串 "global"）
    job_key                 TEXT    NOT NULL,
    
    -- 任务状态："pending"、"running"、"done"、"failed"
    status                  TEXT    NOT NULL,
    
    -- 当前持有任务的 worker 标识符（UUID）
    worker_id               TEXT,
    
    -- 所有权令牌（用于分布式抢占式锁定，防止多 worker 并发执行同一任务）
    ownership_token         TEXT,
    
    -- 任务开始时间（Unix 秒）
    started_at              INTEGER,
    
    -- 任务完成时间（Unix 秒）
    finished_at             INTEGER,
    
    -- 租约到期时间（Unix 秒，超时后其他 worker 可以抢占）
    lease_until             INTEGER,
    
    -- 下次重试时间（Unix 秒）
    retry_at                INTEGER,
    
    -- 剩余重试次数（默认值：3）
    retry_remaining         INTEGER NOT NULL,
    
    -- 最后一次失败的错误信息
    last_error              TEXT,
    
    -- 输入水位线（用于增量处理，记录上次处理到的时间点）
    input_watermark         INTEGER,
    
    -- 上次成功完成时的水位线
    last_success_watermark  INTEGER,
    
    PRIMARY KEY (kind, job_key)
);
```

**任务调度说明**：
- 通过 `ownership_token` 实现乐观锁，防止多个 Codex 进程并发执行同一记忆生成任务
- `lease_until` 提供超时保护：若 worker 崩溃，租约到期后任务可被重新认领
- `retry_remaining` 控制失败重试（默认 3 次），彻底失败后 `status = "failed"` 且 `last_error` 记录原因

---

### 2.5 `agent_jobs` 表

**创建 Migration**：`0014_agent_jobs.sql`
**功能**：存储批处理 Agent 任务（用于 CSV 批量处理场景）。

```sql
CREATE TABLE agent_jobs (
    -- 主键：UUID 字符串
    id                  TEXT    PRIMARY KEY,
    
    -- 任务名称（用户定义）
    name                TEXT    NOT NULL,
    
    -- 任务状态："pending"、"running"、"completed"、"failed"、"cancelled"
    status              TEXT    NOT NULL,
    
    -- 对 Agent 的指令（system prompt 或 task description）
    instruction         TEXT    NOT NULL,
    
    -- 输出字段的 JSON Schema（可选，用于结构化输出验证）
    output_schema_json  TEXT,
    
    -- 输入 CSV 的列头 JSON 数组
    input_headers_json  TEXT    NOT NULL,
    
    -- 输入 CSV 文件的绝对路径
    input_csv_path      TEXT    NOT NULL,
    
    -- 输出 CSV 文件的绝对路径
    output_csv_path     TEXT    NOT NULL,
    
    -- 是否自动导出完成的 item 到 CSV（1=是，0=否）
    auto_export         INTEGER NOT NULL DEFAULT 1,
    
    -- 创建时间（Unix 秒）
    created_at          INTEGER NOT NULL,
    
    -- 最后更新时间（Unix 秒）
    updated_at          INTEGER NOT NULL,
    
    -- 任务开始时间（Unix 秒，任务从 pending→running 时设置）
    started_at          INTEGER,
    
    -- 任务完成时间（Unix 秒，所有 item 完成时设置）
    completed_at        INTEGER,
    
    -- 最后一次错误信息
    last_error          TEXT,
    
    -- [0015] 单个 item 的最大运行时间（秒），超时后 item 被标记为失败
    max_runtime_seconds INTEGER
);
```

---

### 2.6 `agent_job_items` 表

**创建 Migration**：`0014_agent_jobs.sql`（与 `agent_jobs` 同一 migration）
**功能**：CSV 批处理任务的每行数据项，每项对应一个独立的 Thread 执行。

```sql
CREATE TABLE agent_job_items (
    -- 关联的 agent_jobs.id（外键，级联删除）
    job_id              TEXT    NOT NULL,
    
    -- item 的唯一标识（UUID 字符串）
    item_id             TEXT    NOT NULL,
    
    -- 在原始 CSV 中的行号（0-based）
    row_index           INTEGER NOT NULL,
    
    -- 可选的来源标识符（如外部系统的 record ID）
    source_id           TEXT,
    
    -- 该行的完整数据（JSON 对象，字段名对应 input_headers_json）
    row_json            TEXT    NOT NULL,
    
    -- item 状态："pending"、"running"、"completed"、"failed"、"skipped"
    status              TEXT    NOT NULL,
    
    -- 分配给该 item 执行的 Thread ID（运行中时设置）
    assigned_thread_id  TEXT,
    
    -- 已尝试执行的次数
    attempt_count       INTEGER NOT NULL DEFAULT 0,
    
    -- 执行结果（JSON 对象，结构由 output_schema_json 定义）
    result_json         TEXT,
    
    -- 最后一次失败的错误信息
    last_error          TEXT,
    
    -- 创建时间（Unix 秒）
    created_at          INTEGER NOT NULL,
    
    -- 最后更新时间（Unix 秒）
    updated_at          INTEGER NOT NULL,
    
    -- 完成时间（Unix 秒，status 变为 completed/failed/skipped 时设置）
    completed_at        INTEGER,
    
    -- 结果上报到外部系统的时间（Unix 秒）
    reported_at         INTEGER,
    
    PRIMARY KEY (job_id, item_id),
    FOREIGN KEY (job_id) REFERENCES agent_jobs(id) ON DELETE CASCADE
);
```

**业务逻辑**：
- 每个 `agent_job_item` 由调度器分配一个 Thread（`assigned_thread_id`），Thread 完成后将结果写回 `result_json`
- `attempt_count` 支持失败重试，与 `agent_jobs.max_runtime_seconds` 共同控制重试策略

---

### 2.7 `thread_spawn_edges` 表

**创建 Migration**：`0021_thread_spawn_edges.sql`
**功能**：记录 AgentControl 生成的父子 Thread 关系图（有向边）。

```sql
CREATE TABLE thread_spawn_edges (
    -- 父 Thread ID（生成子 Agent 的 Thread）
    parent_thread_id    TEXT    NOT NULL,
    
    -- 子 Thread ID（被生成的子 Agent Thread），也是主键，保证每个子 Thread 只有一个父
    child_thread_id     TEXT    NOT NULL PRIMARY KEY,
    
    -- 边的状态，枚举字符串：
    --   "running"    - 子 Agent 正在执行
    --   "completed"  - 子 Agent 已完成
    --   "failed"     - 子 Agent 执行失败
    --   "cancelled"  - 子 Agent 被取消
    status              TEXT    NOT NULL
);
```

**业务逻辑**：
- 支持多层嵌套 Agent 树（AgentControl 树状调用结构），通过递归查询可获取所有后代节点
- `StateRuntime` 提供 `list_thread_spawn_descendants_with_status` 通过 CTE 递归查询整棵子树
- 注意：表中没有到 `threads` 表的显式外键约束（支持在 Thread 创建前记录边关系）

---

### 2.8 `remote_control_enrollments` 表

**创建 Migration**：`0024_remote_control_enrollments.sql`
**功能**：存储 App Server 远程控制的注册信息，记录客户端已配对的服务器端点。

```sql
CREATE TABLE remote_control_enrollments (
    -- WebSocket 服务器的 URL
    websocket_url               TEXT    NOT NULL,
    
    -- 账号 ID（与远程服务器关联的用户账号）
    account_id                  TEXT    NOT NULL,
    
    -- App Server 客户端名称（标识连接的客户端类型）
    app_server_client_name      TEXT    NOT NULL,
    
    -- 远程服务器 ID
    server_id                   TEXT    NOT NULL,
    
    -- 远程环境 ID
    environment_id              TEXT    NOT NULL,
    
    -- 远程服务器可读名称（用于 UI 展示）
    server_name                 TEXT    NOT NULL,
    
    -- 最后更新时间（Unix 秒）
    updated_at                  INTEGER NOT NULL,
    
    -- 复合主键：同一 URL + 账号 + 客户端名称组合唯一
    PRIMARY KEY (websocket_url, account_id, app_server_client_name)
);
```

---

### 2.9 `device_key_bindings` 表

**创建 Migration**：`0028_device_key_bindings.sql`
**功能**：存储设备密钥绑定记录，关联本地设备的密钥与远程账号用户。

```sql
CREATE TABLE device_key_bindings (
    -- 密钥 ID（对应本地生成的设备密钥，UUID 格式）
    key_id              TEXT    PRIMARY KEY NOT NULL,
    
    -- 绑定的账号用户 ID
    account_user_id     TEXT    NOT NULL,
    
    -- 客户端 ID（标识具体的客户端实例）
    client_id           TEXT    NOT NULL,
    
    -- 绑定创建时间（Unix 秒）
    created_at          INTEGER NOT NULL,
    
    -- 最后更新时间（Unix 秒）
    updated_at          INTEGER NOT NULL
);
```

---

### 2.10 `thread_goals` 表

**创建 Migration**：`0029_thread_goals.sql`
**功能**：存储会话的目标（Goal）追踪信息，支持 token/time budget 控制和进度监控。

```sql
CREATE TABLE thread_goals (
    -- 关联的 Thread ID（1:1，也是主键；外键，级联删除）
    thread_id           TEXT    PRIMARY KEY NOT NULL
                                REFERENCES threads(id) ON DELETE CASCADE,
    
    -- 目标 ID（UUID，外部系统或用户创建时指定）
    goal_id             TEXT    NOT NULL,
    
    -- 目标的自然语言描述
    objective           TEXT    NOT NULL,
    
    -- 目标状态（CHECK 约束保证只有这四种值）：
    --   "active"         - 正在执行
    --   "paused"         - 已暂停（外部请求）
    --   "budget_limited" - 已达到预算上限（token 或时间）
    --   "complete"       - 已完成
    status              TEXT    NOT NULL
                                CHECK(status IN ('active', 'paused', 'budget_limited', 'complete')),
    
    -- token 预算上限（可选，为 NULL 表示无限制）
    token_budget        INTEGER,
    
    -- 已使用的 token 数量
    tokens_used         INTEGER NOT NULL DEFAULT 0,
    
    -- 已运行时间（秒）
    time_used_seconds   INTEGER NOT NULL DEFAULT 0,
    
    -- 目标创建时间（Unix 毫秒，注意：_ms 后缀）
    created_at_ms       INTEGER NOT NULL,
    
    -- 目标最后更新时间（Unix 毫秒）
    updated_at_ms       INTEGER NOT NULL
);
```

**业务逻辑**：
- `status = 'budget_limited'` 由 `StateRuntime` 自动检测：每次更新 `tokens_used` 或 `time_used_seconds` 时，若超过 `token_budget`，自动转为此状态
- 注意此表的时间戳使用毫秒（`_ms` 后缀），与 `threads` 表的 `created_at_ms`/`updated_at_ms` 保持一致

---

## 3. 日志库（logs.db）

- **文件路径**：`{codex_home}/logs.db`（固定在 `~/.codex/logs.db`）
- **Migration 目录**：`codex-rs/state/logs_migrations/`
- **版本常量**：`LOGS_DB_VERSION = 2`
- **历史**：日志功能最初在主库（migration 0002～0012），migration 0023（`drop_logs.sql`）将日志迁移到独立库，同时对主库执行 `DROP TABLE logs` 和 `PRAGMA auto_vacuum = INCREMENTAL`

### 3.1 `logs` 表

**创建 Migration**：`logs_migrations/0001_logs.sql`

```sql
CREATE TABLE logs (
    -- 自增主键（整数，便于分页）
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    
    -- 日志时间戳（Unix 秒）
    ts              INTEGER NOT NULL,
    
    -- 时间戳的纳秒部分（与 ts 组合可获得纳秒精度）
    ts_nanos        INTEGER NOT NULL,
    
    -- 日志级别："TRACE"、"DEBUG"、"INFO"、"WARN"、"ERROR"
    level           TEXT    NOT NULL,
    
    -- tracing target（通常是 Rust 模块路径，如 "codex_core::agent"）
    target          TEXT    NOT NULL,
    
    -- [0002 → renamed in 0002_logs_feedback_log_body.sql]
    -- feedback_log_body：实际日志内容（JSON 或文本），用于反馈和查询
    -- 注：原字段名为 message，migration 0002 重建表并改名
    feedback_log_body   TEXT,
    
    -- Rust 模块路径（可选，如 "codex_state::runtime::logs"）
    module_path     TEXT,
    
    -- 源文件路径（可选，如 "src/runtime/logs.rs"）
    file            TEXT,
    
    -- 源文件行号（可选）
    line            INTEGER,
    
    -- 关联的 Thread ID（可选，NULL 表示非会话上下文的日志）
    thread_id       TEXT,
    
    -- 进程 UUID（标识产生此日志的 Codex 进程实例）
    process_uuid    TEXT,
    
    -- 预估的字节数（用于分区容量控制，避免单个 thread/process 日志过多）
    estimated_bytes INTEGER NOT NULL DEFAULT 0
);
```

**日志保留策略**：
- 每次批量 INSERT 后，系统调用 `prune_logs_after_insert` 修剪超出容量的旧日志
- 按 `thread_id` 分区：每个 Thread 保留固定字节数的日志（滑动窗口）
- 按 `process_uuid` 分区（`thread_id IS NULL` 的日志）：按进程实例保留最近日志
- 默认保留天数：`LOG_RETENTION_DAYS = 10`

---

## 4. 表关系 ER 图

```
主状态库（state.db）
═══════════════════════════════════════════════════════════════════════

  ┌────────────────────────────────────────────────────────────────┐
  │                          threads                               │
  │──────────────────────────────────────────────────────────────  │
  │ PK  id TEXT                                                    │
  │     rollout_path TEXT ──────────────────────────► sessions/*.jsonl │
  │     source TEXT                                                │
  │     model_provider TEXT                                        │
  │     cwd TEXT                                                   │
  │     model TEXT                                                 │
  │     reasoning_effort TEXT                                      │
  │     agent_nickname TEXT                                        │
  │     agent_role TEXT                                            │
  │     agent_path TEXT                                            │
  │     tokens_used INTEGER                                        │
  │     archived INTEGER                                           │
  │     created_at_ms INTEGER                                      │
  │     updated_at_ms INTEGER                                      │
  │     memory_mode TEXT                                           │
  │     ... (git_sha, git_branch, git_origin_url, etc.)           │
  └────────────────────────────────────────────────────────────────┘
       │                      │                   │                │
       │ 1:N (CASCADE DEL)    │ 1:1 (CASCADE DEL) │ 1:1 (CASCADE) │
       ▼                      ▼                   ▼               │
  ┌──────────────┐   ┌─────────────────┐  ┌─────────────┐        │
  │thread_dynamic│   │  stage1_outputs │  │thread_goals │        │
  │    _tools    │   │   (memories)    │  │─────────────│        │
  │──────────────│   │─────────────────│  │PK thread_id │        │
  │PK(thread_id, │   │PK thread_id     │  │   goal_id   │        │
  │   position)  │   │  raw_memory     │  │   objective │        │
  │  name        │   │  rollout_summary│  │   status    │        │
  │  description │   │  generated_at   │  │  CHECK(IN(  │        │
  │  input_schema│   │  usage_count    │  │  'active',  │        │
  │  defer_loading│  │  last_usage     │  │  'paused',  │        │
  │  namespace   │   │  selected_for   │  │  'budget_   │        │
  └──────────────┘   │    _phase2      │  │  limited',  │        │
                     └─────────────────┘  │  'complete')│        │
                              │           │  token_budget│        │
                              │ (via jobs)│  tokens_used│        │
                              ▼           └─────────────┘        │
                     ┌────────────────┐                          │
                     │      jobs      │   ← 任务调度             │
                     │────────────────│                          │
                     │PK(kind,job_key)│                          │
                     │  status        │                          │
                     │  worker_id     │                          │
                     │  lease_until   │                          │
                     │  retry_remaining│                         │
                     └────────────────┘                          │
                                                                 │ 1:N (CASCADE DEL)
       ┌──────────────────────────────────────────────┐          ▼
       │              agent_jobs                      │  ┌──────────────────────┐
       │──────────────────────────────────────────────│  │  thread_spawn_edges  │
       │ PK id TEXT                                   │  │──────────────────────│
       │    name TEXT                                 │  │PK child_thread_id    │
       │    status TEXT                               │  │   parent_thread_id ──┼─► threads.id
       │    instruction TEXT                          │  │   status             │
       │    input_csv_path TEXT                       │  └──────────────────────┘
       │    output_csv_path TEXT                      │
       │    max_runtime_seconds INTEGER               │
       └──────────────────────────────────────────────┘
                      │ 1:N (CASCADE DEL)
                      ▼
       ┌──────────────────────────────────────────────┐
       │            agent_job_items                   │
       │──────────────────────────────────────────────│
       │ PK(job_id, item_id)                          │
       │    row_index INTEGER                         │
       │    row_json TEXT                             │
       │    status TEXT                               │
       │    assigned_thread_id TEXT ──────────────────┼──► threads.id (软引用)
       │    attempt_count INTEGER                     │
       │    result_json TEXT                          │
       └──────────────────────────────────────────────┘

  ┌──────────────────────────────────┐   ┌──────────────────────────┐
  │   remote_control_enrollments     │   │    device_key_bindings   │
  │──────────────────────────────────│   │──────────────────────────│
  │ PK(websocket_url,account_id,     │   │ PK key_id TEXT           │
  │    app_server_client_name)       │   │    account_user_id TEXT  │
  │    server_id TEXT                │   │    client_id TEXT        │
  │    environment_id TEXT           │   │    created_at INTEGER    │
  │    server_name TEXT              │   │    updated_at INTEGER    │
  │    updated_at INTEGER            │   └──────────────────────────┘
  └──────────────────────────────────┘

日志库（logs.db）[独立文件]
═══════════════════════════════════════════════════════════════════════

  ┌────────────────────────────────────────────────────────────────┐
  │                            logs                                │
  │──────────────────────────────────────────────────────────────  │
  │ PK id INTEGER AUTOINCREMENT                                    │
  │    ts INTEGER, ts_nanos INTEGER  ← 纳秒精度时间戳             │
  │    level TEXT                                                  │
  │    target TEXT                                                 │
  │    feedback_log_body TEXT                                      │
  │    thread_id TEXT ─────────────────────────────► threads.id   │
  │    process_uuid TEXT             ← 软引用，无外键约束          │
  │    estimated_bytes INTEGER                                     │
  └────────────────────────────────────────────────────────────────┘
```

---

## 5. 索引设计说明

### 5.1 `threads` 表索引

| 索引名 | 字段 | 创建 Migration | 用途 |
|--------|------|---------------|------|
| `idx_threads_created_at` | `(created_at DESC, id DESC)` | 0001 | 按创建时间倒序分页列表（legacy，秒精度） |
| `idx_threads_updated_at` | `(updated_at DESC, id DESC)` | 0001 | 按更新时间倒序分页列表（legacy，秒精度） |
| `idx_threads_archived` | `(archived)` | 0001 | 快速过滤已归档/活跃会话 |
| `idx_threads_source` | `(source)` | 0001 | 按来源过滤（CLI vs VSCode vs 其他） |
| `idx_threads_provider` | `(model_provider)` | 0001 | 按模型供应商过滤 |
| `idx_threads_created_at_ms` | `(created_at_ms DESC, id DESC)` | 0025 | 按创建时间倒序分页（毫秒精度，当前使用） |
| `idx_threads_updated_at_ms` | `(updated_at_ms DESC, id DESC)` | 0025 | 按更新时间倒序分页（毫秒精度，当前使用） |
| `idx_threads_archived_cwd_created_at_ms` | `(archived, cwd, created_at_ms DESC, id DESC)` | 0027 | 按 CWD 过滤后按时间排序（当前目录视图） |
| `idx_threads_archived_cwd_updated_at_ms` | `(archived, cwd, updated_at_ms DESC, id DESC)` | 0027 | 同上，按更新时间排序 |

**设计说明**：
- 所有时间排序索引均带 `id DESC` 作为 tiebreaker，保证同毫秒内的排序确定性
- migration 0025 同时保留旧索引（`created_at`/`updated_at`），保证向后兼容
- `(archived, cwd, ...)` 复合索引支持"当前目录"视图（只显示该 CWD 下的会话），这是 TUI 界面的常用过滤场景

### 5.2 `thread_dynamic_tools` 表索引

| 索引名 | 字段 | 用途 |
|--------|------|------|
| `idx_thread_dynamic_tools_thread` | `(thread_id)` | 按 thread_id 查询该会话的所有工具 |

### 5.3 `stage1_outputs` 表索引

| 索引名 | 字段 | 用途 |
|--------|------|------|
| `idx_stage1_outputs_source_updated_at` | `(source_updated_at DESC, thread_id DESC)` | 按文件修改时间排序，用于记忆系统增量处理（找出最近更新的会话优先处理） |

### 5.4 `jobs` 表索引

| 索引名 | 字段 | 用途 |
|--------|------|------|
| `idx_jobs_kind_status_retry_lease` | `(kind, status, retry_at, lease_until)` | 任务调度器核心查询：按类型和状态找出可执行的任务，同时检查重试时间和租约状态 |

### 5.5 `agent_jobs` / `agent_job_items` 表索引

| 索引名 | 字段 | 用途 |
|--------|------|------|
| `idx_agent_jobs_status` | `(status, updated_at DESC)` | 按状态列出任务，按更新时间排序 |
| `idx_agent_job_items_status` | `(job_id, status, row_index ASC)` | 按 job 内状态过滤 item，按行号顺序处理 |

### 5.6 `thread_spawn_edges` 表索引

| 索引名 | 字段 | 用途 |
|--------|------|------|
| `idx_thread_spawn_edges_parent_status` | `(parent_thread_id, status)` | 按父节点和状态查询子 Agent，支持 AgentControl 状态监控 |

### 5.7 `logs` 表索引（logs.db）

| 索引名 | 字段 | 条件 | 用途 |
|--------|------|------|------|
| `idx_logs_ts` | `(ts DESC, ts_nanos DESC, id DESC)` | 无 | 全局时间顺序查询 |
| `idx_logs_thread_id` | `(thread_id)` | 无 | 按会话 ID 查询日志 |
| `idx_logs_thread_id_ts` | `(thread_id, ts DESC, ts_nanos DESC, id DESC)` | 无 | 会话日志分页（主要使用） |
| `idx_logs_process_uuid_threadless_ts` | `(process_uuid, ts DESC, ts_nanos DESC, id DESC)` | `WHERE thread_id IS NULL` | 非会话上下文的进程级日志查询（Partial Index） |

**Partial Index 说明**：`idx_logs_process_uuid_threadless_ts` 是 SQLite Partial Index，只索引 `thread_id IS NULL` 的行，有效减小索引体积，专门加速"threadless"日志查询。

---

## 6. JSONL Rollout 文件格式

### 6.1 文件位置与命名

```
~/.codex/sessions/rollout-{YYYY-MM-DDTHH-MM-SS}-{UUID}.jsonl
~/.codex/archived_sessions/rollout-{YYYY-MM-DDTHH-MM-SS}-{UUID}.jsonl
```

示例：
```
~/.codex/sessions/rollout-2024-11-15T09-30-45-550e8400-e29b-41d4-a716-446655440000.jsonl
```

- 时间戳格式：`YYYY-MM-DDTHH-MM-SS`（注意分隔符是 `-` 而非 `:`，规避文件系统限制）
- UUID：对应 `threads.id`，是会话的全局唯一标识符
- `sessions/`：活跃会话（默认目录）
- `archived_sessions/`：已归档会话（用户或系统手动归档后移至此处）

### 6.2 文件格式

每行是一个独立的 JSON 对象，结构为 `RolloutLine`：

```json
{
  "timestamp": "2024-11-15T09:30:45.123456Z",
  "type": "session_meta",
  "payload": { ... }
}
```

`timestamp` 字段为 RFC 3339 格式的 UTC 时间字符串，`type` 和 `payload` 通过 serde 的 `tag = "type"` 枚举映射。

### 6.3 `RolloutItem` 类型枚举

`type` 字段决定 `payload` 的结构：

#### `"session_meta"` — 会话元数据（首行）

每个 JSONL 文件的第一行（通常）是 `session_meta`，记录会话的初始配置。

```json
{
  "timestamp": "2024-11-15T09:30:45Z",
  "type": "session_meta",
  "payload": {
    "id": "550e8400-e29b-41d4-a716-446655440000",
    "timestamp": "2024-11-15T09-30-45",
    "cwd": "/home/user/project",
    "originator": "codex-cli",
    "cli_version": "0.12.0",
    "source": "cli",
    "model_provider": "openai",
    "agent_nickname": null,
    "agent_role": null,
    "agent_path": null,
    "base_instructions": { ... },
    "dynamic_tools": [ ... ],
    "memory_mode": "enabled",
    "git": {
      "commit_hash": { "0": "abc123def456..." },
      "branch": "main",
      "repository_url": "https://github.com/org/repo"
    }
  }
}
```

关键字段说明：
- `id`：会话 UUID，与文件名中的 UUID 段一致，同时是 `threads.id`
- `base_instructions`：系统提示词（`BaseInstructions` 结构），可能包含模型参数
- `dynamic_tools`：该会话注入的动态工具列表（同步存储到 `thread_dynamic_tools` 表）
- `forked_from_id`：若本会话是从另一会话 fork 出来的，记录父会话 ID

#### `"response_item"` — AI 响应与工具调用

对话历史的核心内容，`payload` 是 `ResponseItem`（来自 OpenAI Responses API 格式）。

常见子类型：
```json
// 用户消息
{"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "帮我修复这个 bug"}]}}

// AI 回复
{"type": "response_item", "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "我来看看这个问题..."}]}}

// 工具调用（如 shell 命令）
{"type": "response_item", "payload": {"type": "function_call", "id": "fc_xxx", "call_id": "call_xxx", "name": "shell", "arguments": "{\"cmd\": [\"ls\", \"-la\"]}"}}

// 工具结果
{"type": "response_item", "payload": {"type": "function_call_output", "call_id": "call_xxx", "output": "total 48\\ndrwxr-xr-x ..."}}
```

#### `"turn_context"` — 上下文快照

每个用户 turn 开始时写入，记录当时的沙箱策略、模型配置、权限配置等，支持会话恢复时正确还原运行时状态。

```json
{
  "timestamp": "2024-11-15T09:31:00Z",
  "type": "turn_context",
  "payload": {
    "turn_id": "turn_001",
    "cwd": "/home/user/project",
    "model": "gpt-4o",
    "effort": "medium",
    "approval_policy": "on_request",
    "sandbox_policy": { ... },
    "current_date": "2024-11-15",
    "timezone": "UTC"
  }
}
```

#### `"compacted"` — 上下文压缩快照

当对话历史过长，系统对历史进行压缩后写入此行。包含压缩后的摘要消息和可选的替代历史记录（`replacement_history`），用于 resume 时还原完整可用的上下文。

```json
{
  "timestamp": "2024-11-15T10:00:00Z",
  "type": "compacted",
  "payload": {
    "message": "对话历史已压缩。前 X 轮对话摘要：...",
    "replacement_history": [ ... ]
  }
}
```

#### `"event_msg"` — 自定义事件消息

用于记录非对话类的系统事件（如工具状态更新、Agent 生命周期事件等）。

### 6.4 文件读写机制

- **写入**：`RolloutRecorder`（`codex-rollout` crate）维护异步写入 channel，通过 `JsonlWriter` 将每个 `RolloutLine` 序列化为 JSON 并追加（append-only）到文件末尾
- **读取**：`RolloutRecorder::load_rollout_items` 逐行解析，遇到无法解析的行记录为 `parse_errors` 计数但不中断
- **恢复（Resume）**：读取 JSONL 文件重建对话历史，`TurnContextItem` 和 `CompactedItem` 帮助跳过不必要的历史重放

---

## 7. Migration 演化历史

以下按功能阶段梳理 migration 0001～0029 的演化历程：

### 阶段一：核心基础（0001～0005）

| Migration | 功能 |
|-----------|------|
| `0001_threads.sql` | 建立核心 `threads` 表，包含基础字段和五个初始索引（按创建时间、更新时间、归档状态、来源、供应商） |
| `0002_logs.sql` | 在主库创建 `logs` 表（后被废弃，仅作历史记录） |
| `0003_logs_thread_id.sql` | 为 `logs` 增加 `thread_id` 字段，将日志与会话关联 |
| `0004_thread_dynamic_tools.sql` | 引入动态工具支持，存储 MCP 工具定义 |
| `0005_threads_cli_version.sql` | 为 `threads` 增加 `cli_version`，追踪产生会话的客户端版本 |

### 阶段二：记忆系统（0006～0012）

| Migration | 功能 |
|-----------|------|
| `0006_memories.sql` | 引入记忆系统核心表：`stage1_outputs`（单会话记忆）和 `jobs`（任务调度） |
| `0007_threads_first_user_message.sql` | 增加 `first_user_message` 字段，用于 TUI 列表预览，避免每次读取 JSONL |
| `0008_backfill_state.sql` | 引入 backfill 机制：从现有 JSONL 文件重建 `threads` 表 |
| `0009_stage1_outputs_rollout_slug.sql` | 为记忆摘要增加 `rollout_slug` 字段，便于快速关联到 JSONL 文件 |
| `0010_logs_process_id.sql` | 为 `logs` 增加 `process_uuid`，区分多进程并发场景 |
| `0011_logs_partition_prune_indexes.sql` | 为 `logs` 增加分区剪枝索引，支持按 thread/process 清理日志 |
| `0012_logs_estimated_bytes.sql` | 为 `logs` 增加 `estimated_bytes`，用于容量控制 |

### 阶段三：Agent 功能扩展（0013～0018）

| Migration | 功能 |
|-----------|------|
| `0013_threads_agent_nickname.sql` | 引入 `agent_nickname` 和 `agent_role` 字段，支持多 Agent 场景中区分子 Agent |
| `0014_agent_jobs.sql` | 引入批处理任务：`agent_jobs`（批次任务）和 `agent_job_items`（CSV 行项目） |
| `0015_agent_jobs_max_runtime_seconds.sql` | 增加 `max_runtime_seconds`，支持对单个任务项设置超时 |
| `0016_memory_usage.sql` | 增加 `usage_count` 和 `last_usage`，追踪记忆被引用频率 |
| `0017_phase2_selection_flag.sql` | 增加 `selected_for_phase2`，标记参与全局记忆汇总的会话 |
| `0018_phase2_selection_snapshot.sql` | 增加 Phase 2 快照时间戳和 `memory_mode` 字段，支持记忆系统的增量处理 |

### 阶段四：精细化控制（0019～0023）

| Migration | 功能 |
|-----------|------|
| `0019_thread_dynamic_tools_defer_loading.sql` | 增加 `defer_loading` 标志，支持工具懒加载 |
| `0020_threads_model_reasoning_effort.sql` | 增加 `model` 和 `reasoning_effort` 字段，记录具体模型和推理强度 |
| `0021_thread_spawn_edges.sql` | 引入 `thread_spawn_edges`，记录 AgentControl 的父子 Agent 关系图 |
| `0022_threads_agent_path.sql` | 增加 `agent_path`，支持多层 Agent 路径寻址 |
| `0023_drop_logs.sql` | **关键重构**：从主库删除 `logs` 表（迁移到独立 `logs.db`），并开启主库的 `PRAGMA auto_vacuum = INCREMENTAL` |

### 阶段五：远程控制与精度升级（0024～0029）

| Migration | 功能 |
|-----------|------|
| `0024_remote_control_enrollments.sql` | 引入 `remote_control_enrollments`，支持 App Server 远程控制注册 |
| `0025_thread_timestamps_millis.sql` | **精度升级**：新增 `created_at_ms`/`updated_at_ms` 字段（毫秒精度）；迁移存量数据；创建 4 个触发器保持同步；新增毫秒级索引 |
| `0026_thread_dynamic_tools_namespace.sql` | 增加 `namespace`，支持多 MCP 服务器的工具命名空间隔离 |
| `0027_threads_cwd_sort_indexes.sql` | 增加 `(archived, cwd, *_ms)` 复合索引，优化"当前目录"筛选场景 |
| `0028_device_key_bindings.sql` | 引入 `device_key_bindings`，支持设备密钥绑定到账号 |
| `0029_thread_goals.sql` | 引入 `thread_goals`，支持 token/时间预算追踪和目标状态管理 |

### 阶段六：Thread 来源追踪（0030）

| Migration | 功能 |
|-----------|------|
| `0030_threads_thread_source.sql` | 在 `threads` 表新增 `thread_source TEXT` 列（可为 NULL），用于记录 Thread 的具体来源（如 app-server 创建、MCP spawn、用户手动创建等），方便后续按来源分析 |

### 向后兼容策略

`StateRuntime` 使用带 `ignore_missing = true` 的 `runtime_migrator`，允许旧版本 Codex 打开已被新版本迁移的数据库，而不会因"发现了未知 migration"而报错。已知 migration 的校验和仍然验证，防止数据损坏。

---

## 8. 关键查询场景

### 8.1 列出最近的活跃 Threads（TUI 主界面）

```sql
-- 按更新时间倒序，支持游标分页
SELECT
    id, rollout_path, created_at_ms, updated_at_ms,
    source, model_provider, model, cwd, title,
    first_user_message, tokens_used, archived_at,
    agent_nickname, agent_role, agent_path
FROM threads
WHERE archived = 0
ORDER BY updated_at_ms DESC, id DESC
LIMIT 50;

-- 当前目录过滤（配合 0027 的复合索引）
SELECT * FROM threads
WHERE archived = 0 AND cwd = '/home/user/my-project'
ORDER BY updated_at_ms DESC, id DESC
LIMIT 50;
```

### 8.2 查询某个 Thread 的详细信息

```sql
-- 获取 Thread 元数据（包括动态工具和记忆）
SELECT t.*, s.raw_memory, s.rollout_summary, s.usage_count
FROM threads t
LEFT JOIN stage1_outputs s ON s.thread_id = t.id
WHERE t.id = '550e8400-e29b-41d4-a716-446655440000';

-- 获取该 Thread 的动态工具
SELECT namespace, name, description, input_schema, defer_loading
FROM thread_dynamic_tools
WHERE thread_id = '550e8400-e29b-41d4-a716-446655440000'
ORDER BY position ASC;
```

### 8.3 查询某个 Thread 的 Agent 子树

```sql
-- 直接子 Agent（BFS 第一层）
SELECT child_thread_id, status
FROM thread_spawn_edges
WHERE parent_thread_id = '550e8400-e29b-41d4-a716-446655440000'
ORDER BY child_thread_id;

-- 递归查询所有后代 Agent（CTE 递归）
WITH RECURSIVE descendants(child_thread_id, depth) AS (
    SELECT child_thread_id, 1
    FROM thread_spawn_edges
    WHERE parent_thread_id = '550e8400-e29b-41d4-a716-446655440000'
    UNION ALL
    SELECT e.child_thread_id, d.depth + 1
    FROM thread_spawn_edges e
    JOIN descendants d ON e.parent_thread_id = d.child_thread_id
)
SELECT d.child_thread_id, d.depth, t.agent_path, e.status
FROM descendants d
JOIN threads t ON t.id = d.child_thread_id
JOIN thread_spawn_edges e ON e.child_thread_id = d.child_thread_id
ORDER BY d.depth ASC, d.child_thread_id ASC;
```

### 8.4 查询某个 agent_job 的进度

```sql
-- 任务整体状态
SELECT
    j.id, j.name, j.status, j.instruction,
    j.created_at, j.started_at, j.completed_at,
    COUNT(*) as total_items,
    SUM(CASE WHEN ji.status = 'completed' THEN 1 ELSE 0 END) as completed_items,
    SUM(CASE WHEN ji.status = 'failed' THEN 1 ELSE 0 END) as failed_items,
    SUM(CASE WHEN ji.status = 'running' THEN 1 ELSE 0 END) as running_items
FROM agent_jobs j
LEFT JOIN agent_job_items ji ON ji.job_id = j.id
WHERE j.id = 'job-uuid-here'
GROUP BY j.id;

-- 失败的 items
SELECT item_id, row_index, row_json, last_error, attempt_count
FROM agent_job_items
WHERE job_id = 'job-uuid-here' AND status = 'failed'
ORDER BY row_index ASC;
```

### 8.5 记忆系统：查找需要处理的 Stage 1 任务

```sql
-- 找出可以被认领的 Stage 1 记忆任务
SELECT kind, job_key, status, retry_remaining, last_error
FROM jobs
WHERE kind = 'memory_stage1'
  AND status IN ('pending', 'failed')
  AND (retry_at IS NULL OR retry_at <= strftime('%s', 'now'))
  AND (lease_until IS NULL OR lease_until <= strftime('%s', 'now'))
  AND retry_remaining > 0
ORDER BY retry_at ASC NULLS FIRST;
```

### 8.6 查询某个 Thread 的运行日志（logs.db）

```sql
-- 按时间倒序获取最近 100 条
SELECT id, ts, ts_nanos, level, target, feedback_log_body, file, line
FROM logs
WHERE thread_id = '550e8400-e29b-41d4-a716-446655440000'
ORDER BY ts DESC, ts_nanos DESC, id DESC
LIMIT 100;

-- 只看 ERROR 级别的日志
SELECT id, ts, level, target, feedback_log_body
FROM logs
WHERE thread_id = '550e8400-e29b-41d4-a716-446655440000'
  AND level = 'ERROR'
ORDER BY ts DESC, ts_nanos DESC, id DESC;
```

### 8.7 查询 Thread 目标进度

```sql
-- 查询活跃目标及其预算使用情况
SELECT
    t.id, t.title,
    g.goal_id, g.objective, g.status,
    g.token_budget, g.tokens_used,
    CASE
        WHEN g.token_budget IS NOT NULL
        THEN ROUND(100.0 * g.tokens_used / g.token_budget, 1)
        ELSE NULL
    END AS token_usage_pct,
    g.time_used_seconds
FROM threads t
JOIN thread_goals g ON g.thread_id = t.id
WHERE g.status = 'active'
ORDER BY t.updated_at_ms DESC;
```

---

## 9. 数据库文件位置说明

### 9.1 默认路径

| 文件 | 默认路径 | 常量 |
|------|----------|------|
| 主状态库 | `~/.codex/state.db` | `STATE_DB_FILENAME = "state"` |
| 日志库 | `~/.codex/logs.db` | `LOGS_DB_FILENAME = "logs"` |
| 活跃会话 JSONL | `~/.codex/sessions/rollout-*.jsonl` | `SESSIONS_SUBDIR = "sessions"` |
| 归档会话 JSONL | `~/.codex/archived_sessions/rollout-*.jsonl` | `ARCHIVED_SESSIONS_SUBDIR = "archived_sessions"` |

### 9.2 路径自定义

**`sqlite_home`**（仅影响 `state.db` 和 `logs.db`）：
- 环境变量：`CODEX_SQLITE_HOME`
- 若未设置，默认等同于 `codex_home`（即 `~/.codex/`）
- Rust 函数：`state_db_path(codex_home)` 和 `logs_db_path(codex_home)`

**`codex_home`**（影响 JSONL 文件目录）：
- 默认值：`~/.codex/`
- 通过 `Config.codex_home` 配置

### 9.3 路径解析代码

```rust
// codex-rs/state/src/runtime.rs
pub fn state_db_path(codex_home: &Path) -> PathBuf {
    codex_home.join(state_db_filename())   // → ~/.codex/state.db
}

pub fn logs_db_path(codex_home: &Path) -> PathBuf {
    codex_home.join(logs_db_filename())    // → ~/.codex/logs.db
}
```

### 9.4 多进程并发安全

- SQLx 使用 SQLite 的 WAL（Write-Ahead Logging）模式，允许多个读者并发，一个写者独占
- `jobs` 表通过 `ownership_token` + `lease_until` 实现应用层分布式锁，防止多个 Codex 进程并发执行同一记忆生成任务
- migration 使用 SQLx 的 `locking = true` 模式，migration 期间加表级锁防止并发迁移
- `ignore_missing = true`（`runtime_migrator`）允许旧版本二进制在新版已迁移的数据库上正常运行

---

*文档基于 `codex-rs/state/migrations/` 目录下 0001～0029 共 29 个 migration 文件及相关 Rust 源码分析编写。*
