# Codex 数据库设计文档

**文档编号：03**
**版本：2.0**
**适用代码库：`codex-rs`（Rust 实现）**
**主要 Crate：`codex-state`（`codex-rs/state/`）**

> 2.0 修订说明：Codex 已将原先的单一 `state.db` 拆分为**四个独立的 SQLite
> 文件**（state / logs / goals / memories），每个文件有自己的 migration 目录和
> 版本号。本版据 `codex-rs/state/migrations/`（0001~0035）以及
> `logs_migrations/`、`goals_migrations/`、`memory_migrations/` 三个独立目录的
> 实际 SQL 重写，修正了旧版中已被迁移/删除的表（`thread_goals`、
> `stage1_outputs`、`jobs`、`device_key_bindings`）的归属。

---

## 目录

1. [总体架构（四库分离）](#1-总体架构四库分离)
2. [主状态库（state_5.sqlite）](#2-主状态库state_5sqlite)
   - [threads 表](#21-threads-表)
   - [thread_dynamic_tools 表](#22-thread_dynamic_tools-表)
   - [backfill_state 表](#23-backfill_state-表)
   - [agent_jobs 表](#24-agent_jobs-表)
   - [agent_job_items 表](#25-agent_job_items-表)
   - [thread_spawn_edges 表](#26-thread_spawn_edges-表)
   - [remote_control_enrollments 表](#27-remote_control_enrollments-表)
3. [目标库（goals_1.sqlite）](#3-目标库goals_1sqlite)
4. [记忆库（memories_1.sqlite）](#4-记忆库memories_1sqlite)
5. [日志库（logs_2.sqlite）](#5-日志库logs_2sqlite)
6. [已废弃 / 迁移的表（历史）](#6-已废弃--迁移的表历史)
7. [表关系 ER 图](#7-表关系-er-图)
8. [索引设计说明](#8-索引设计说明)
9. [JSONL Rollout 文件格式](#9-jsonl-rollout-文件格式)
10. [Migration 演化历史](#10-migration-演化历史)
11. [关键查询场景](#11-关键查询场景)
12. [数据库文件位置说明](#12-数据库文件位置说明)

---

## 1. 总体架构（四库分离）

Codex 的持久化层由**四个 SQLite 文件**加上 **JSONL rollout 文件**组成，分别
承担不同职责：

```
~/.codex/                                （= $CODEX_HOME，亦可由 $CODEX_SQLITE_HOME 覆盖 DB 位置）
├── state_5.sqlite        ← 主状态库：会话元数据、动态工具、批处理任务、Agent 关系图
├── logs_2.sqlite         ← 日志库：运行时 tracing 日志（独立 DB，按窗口清理）
├── goals_1.sqlite        ← 目标库：thread_goals（目标 / 预算追踪）
├── memories_1.sqlite     ← 记忆库：stage1_outputs（单会话记忆）+ jobs（记忆任务调度）
└── sessions/
    ├── rollout-{ts}-{uuid}.jsonl   ← 活跃会话的完整对话历史（JSONL，权威来源）
    └── ...
└── archived_sessions/
    └── rollout-{ts}-{uuid}.jsonl   ← 已归档会话（同格式）
```

### 1.1 为什么拆成四个库

四个库由 `StateRuntime`（`codex-rs/state/src/runtime.rs`）在启动时分别打开并
迁移。拆分的核心动机是**降低锁竞争、隔离故障域、便于独立清理**：

| 库文件 | migration 目录 | 内容 | 拆分理由 |
|--------|---------------|------|---------|
| `state_5.sqlite` | `migrations/` | 会话索引、动态工具、批处理、Agent 图、远控注册、backfill 游标 | 主索引，前台读写最频繁 |
| `logs_2.sqlite` | `logs_migrations/` | tracing 日志 | 写入量极大且可丢弃，与前台状态分开避免 WAL 膨胀拖慢主库 |
| `goals_1.sqlite` | `goals_migrations/` | 目标 / token / 时间预算 | 由 core 目标运行时（`core/src/goals.rs`）高频更新（每次工具/turn 累加用量） |
| `memories_1.sqlite` | `memory_migrations/` | 记忆摘要 + 记忆任务队列 | 后台记忆流水线独占，可整库清空（`codex /memory clear`） |

> **版本号编码在文件名里**：`state_5`、`logs_2`、`goals_1`、`memories_1` 的数字
> 后缀就是该库的「世代版本」。当 schema 发生**破坏性重构**（无法用 ALTER 平滑
> 迁移）时，会直接 bump 文件名数字、丢弃旧库重建——因此代码里**没有**
> `STATE_DB_VERSION` 这类常量，文件名即版本。常量定义见
> `codex-rs/state/src/lib.rs`：
>
> ```rust
> pub const STATE_DB_FILENAME: &str    = "state_5.sqlite";
> pub const LOGS_DB_FILENAME: &str     = "logs_2.sqlite";
> pub const GOALS_DB_FILENAME: &str    = "goals_1.sqlite";
> pub const MEMORIES_DB_FILENAME: &str = "memories_1.sqlite";
> ```

### 1.2 四个 Migrator

`codex-rs/state/src/migrations.rs` 用 `sqlx::migrate!()` 在编译期把四个目录下的
`.sql` 嵌入二进制，得到四个独立的 `Migrator`：

```rust
pub(crate) static STATE_MIGRATOR:    Migrator = sqlx::migrate!("./migrations");
pub(crate) static LOGS_MIGRATOR:     Migrator = sqlx::migrate!("./logs_migrations");
pub(crate) static GOALS_MIGRATOR:    Migrator = sqlx::migrate!("./goals_migrations");
pub(crate) static MEMORIES_MIGRATOR: Migrator = sqlx::migrate!("./memory_migrations");
```

每个库各自维护一张 `_sqlx_migrations` 元数据表记录已应用的版本。运行时通过
`runtime_migrator(base)` 包一层，把 `ignore_missing` 置为 `true`：

```rust
// 允许「旧二进制打开已被新二进制迁移过的库」而不报错——
// 比自己 embedded 集更新的已应用版本会被忽略；已知版本仍按 checksum 校验防损坏。
fn runtime_migrator(base: &'static Migrator) -> Migrator {
    Migrator { migrations: Cow::Borrowed(base.migrations.as_ref()),
               ignore_missing: true, locking: base.locking, .. }
}
```

这是多版本共存的关键：用户可能同时运行新旧两个 Codex 进程，旧进程不能因为
「看到一个我不认识的 migration」就崩溃。

### 1.3 SQLite 连接参数

四个库共用 `base_sqlite_options()`（`runtime.rs`）的连接配置：

| 参数 | 取值 | 说明 |
|------|------|------|
| `journal_mode` | `WAL` | 写前日志，允许多读单写并发 |
| `synchronous` | `Normal` | WAL 下兼顾安全与吞吐的常用档位 |
| `busy_timeout` | `5s` | 写锁竞争时的等待上限 |
| `create_if_missing` | `true` | 首次启动自动建库 |
| `max_connections` | `5` | 连接池上限 |
| `auto_vacuum`（仅 state） | `INCREMENTAL` | 删除大表（如 0023 drop logs）后回收空间 |

### 1.4 JSONL 是权威来源，DB 是索引

**重要设计决策**：四个 SQLite 库都是 JSONL 文件的**派生索引**，而非权威来源。
当 JSONL 文件存在但 `state_5.sqlite` 中缺少对应记录时，系统通过 **backfill**
机制（见 [§2.3 backfill_state](#23-backfill_state-表)）扫描 `sessions/` 目录重建
`threads` 元数据。删库重建（bump 文件名）之所以可行，正是因为 JSONL 始终保留
完整历史。

### 1.5 Crate 对应关系

- `codex-state`（`codex-rs/state/`）：四库的访问层，定义 `StateRuntime`，管理
  连接池、migration、backfill、记忆/目标子存储。
- `codex-rollout`（`codex-rs/rollout/`）：JSONL 文件的读写，以及从文件提取元数据
  到 `state_5.sqlite` 的 backfill 逻辑。
- `codex-protocol`（`codex-rs/protocol/`）：定义 `RolloutLine`、`RolloutItem`、
  `SessionMeta` 等序列化类型。
- 目标读写：现行实现在 `codex-core` 的 `core/src/goals.rs`，经
  `StateRuntime::thread_goals()`（`GoalStore`）落到 `goals_1.sqlite`。（`codex-rs/ext/goal/`
  是一个尚未接线的实验性扩展 sketch，当前不参与运行，详见 `07_goal_mode.md` §2。）

---

## 2. 主状态库（state_5.sqlite）

- **文件路径**：`{sqlite_home}/state_5.sqlite`（默认 `~/.codex/state_5.sqlite`，
  可通过 `CODEX_SQLITE_HOME` 环境变量覆盖目录）
- **Migration 目录**：`codex-rs/state/migrations/`（0001~0035，共 35 个）
- **Migration 工具**：`STATE_MIGRATOR`（`sqlx::migrate!("./migrations")`）

经过全部 35 个 migration 后，**仍存活于 state 库**的表为：`threads`、
`thread_dynamic_tools`、`backfill_state`、`agent_jobs`、`agent_job_items`、
`thread_spawn_edges`、`remote_control_enrollments`。曾经存在但已迁出 / 删除的表
（`logs`、`thread_goals`、`stage1_outputs`、`jobs`、`device_key_bindings`）见
[§6](#6-已废弃--迁移的表历史)。

### 2.1 `threads` 表

**创建 Migration**：`0001_threads.sql`（后续多个 migration 增列）
**功能**：核心会话（Thread）元数据索引表，每行对应一个 JSONL rollout 文件。

```sql
CREATE TABLE threads (
    -- 主键：UUID 格式字符串，与 JSONL 文件名中的 UUID 段对应
    id                  TEXT    PRIMARY KEY,

    -- JSONL 文件的绝对路径（从 DB 导航到完整历史的关键链接）
    rollout_path        TEXT    NOT NULL,

    -- 创建 / 更新时间（Unix 秒，legacy；新代码读 *_ms 字段）
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL,

    -- 会话来源，由 SessionSource 枚举决定（如 "cli"、"vscode"、"chatgpt"）
    source              TEXT    NOT NULL,

    -- 模型供应商 ID，如 "openai"、"azure"
    model_provider      TEXT    NOT NULL,

    -- 会话启动时的工作目录（已规范化）
    cwd                 TEXT    NOT NULL,

    -- 会话标题（从首条用户消息自动提取或截断）
    title               TEXT    NOT NULL,

    -- 沙箱策略（JSON 序列化字符串）
    sandbox_policy      TEXT    NOT NULL,

    -- 审批模式，如 "on_request"、"never"、"unless_trusted"
    approval_mode       TEXT    NOT NULL,

    tokens_used         INTEGER NOT NULL DEFAULT 0,   -- 该会话消耗的 token 总数
    has_user_event      INTEGER NOT NULL DEFAULT 0,   -- 是否有过用户事件
    archived            INTEGER NOT NULL DEFAULT 0,   -- 是否已归档
    archived_at         INTEGER,                      -- 归档时间

    -- Git 信息（可选，会话启动时从 git 命令获取）
    git_sha             TEXT,
    git_branch          TEXT,
    git_origin_url      TEXT,

    -- [0005] Codex CLI 版本号，如 "0.12.0"（NOT NULL，默认空串）
    cli_version         TEXT    NOT NULL DEFAULT '',

    -- [0007] 首条用户消息文本（列表预览用，回填存量时取 title）
    first_user_message  TEXT    NOT NULL DEFAULT '',

    -- [0013] 子 Agent 昵称 / 角色（多 Agent 场景下区分）
    agent_nickname      TEXT,
    agent_role          TEXT,

    -- [0018] 记忆模式，如 "enabled"（默认）；记忆流水线据此过滤是否提取本会话
    memory_mode         TEXT    NOT NULL DEFAULT 'enabled',

    -- [0020] 具体模型名 / 推理强度，如 "gpt-5-codex"、"high"
    model               TEXT,
    reasoning_effort    TEXT,

    -- [0022] 子 Agent 的规范路径，如 "/planner/executor"
    agent_path          TEXT,

    -- [0025] 毫秒精度时间戳（当前主要使用字段；触发器自动从秒值回填）
    created_at_ms       INTEGER,
    updated_at_ms       INTEGER,

    -- [0030] Thread 的具体来源标识（app-server 创建 / MCP spawn / 手动等）
    thread_source       TEXT,

    -- [0032] 列表预览文本（优先 first_user_message，回填存量时还会回退到目标 objective）
    preview             TEXT    NOT NULL DEFAULT ''
);
```

**触发器**（migration 0025 新增，共 4 个）：
- `threads_created_at_ms_after_insert` / `_updated_at_ms_after_insert`：INSERT 后
  若 `*_ms` 为空，自动填充 `*_at * 1000`。
- `threads_created_at_ms_after_update` / `_updated_at_ms_after_update`：秒值更新时
  同步刷新对应毫秒列。

**字段说明补充**：
- 时间戳迁移背景：系统初期用秒精度（`created_at`/`updated_at`），0025 升级为毫秒
  精度，并用递归 CTE 为存量数据分配**严格递增且唯一**的毫秒值（避免同毫秒排序
  抖动）。新代码一律读 `*_ms`，旧列仅为兼容保留。
- `preview`（0032）与 `first_user_message`（0007）的区别：前者是面向 UI 的「展示
  用一句话」，回填逻辑会在 `first_user_message` 为空时回退到当时 `thread_goals`
  表里的 `objective`（注意：0032 运行时 `thread_goals` 尚在 state 库，0034 才迁出，
  迁移按序执行因此历史上自洽）。

---

### 2.2 `thread_dynamic_tools` 表

**创建 Migration**：`0004_thread_dynamic_tools.sql`
**功能**：存储每个 Thread 的动态工具定义（运行期注入的自定义 / MCP 工具）。

```sql
CREATE TABLE thread_dynamic_tools (
    thread_id       TEXT    NOT NULL,   -- 关联 Thread（外键，级联删除）
    position        INTEGER NOT NULL,   -- 在该 Thread 中的排列顺序（0-based）
    name            TEXT    NOT NULL,   -- 工具名（namespace 内唯一）
    description     TEXT    NOT NULL,   -- 传给 LLM 的工具说明
    input_schema    TEXT    NOT NULL,   -- 输入 JSON Schema
    defer_loading   INTEGER NOT NULL DEFAULT 0,  -- [0019] 是否懒加载（首次使用时再解析）
    namespace       TEXT,                         -- [0026] 工具命名空间（多 MCP 服务器隔离）
    PRIMARY KEY (thread_id, position),
    FOREIGN KEY (thread_id) REFERENCES threads(id) ON DELETE CASCADE
);
```

业务逻辑：动态工具在会话创建时从 `SessionMeta` 提取并持久化；`defer_loading = 1`
表示工具实现需运行时解析（如远程 MCP 上的工具）；`namespace` 区分不同 MCP
服务器的同名工具。

---

### 2.3 `backfill_state` 表

**创建 Migration**：`0008_backfill_state.sql`
**功能**：单行游标表，记录「从 `sessions/` 目录回填 `threads` 元数据」的进度。

```sql
CREATE TABLE backfill_state (
    id              INTEGER PRIMARY KEY CHECK (id = 1),  -- 强制单行（singleton）
    status          TEXT    NOT NULL,                    -- 'pending' / 'running' / 'done' 等
    last_watermark  TEXT,                                -- 上次扫描到的位置（文件名水位线）
    last_success_at INTEGER,                             -- 上次成功完成时间
    updated_at      INTEGER NOT NULL
);
-- 建表时即插入 id=1 的初始 'pending' 行（ON CONFLICT DO NOTHING）
```

`CHECK (id = 1)` 保证全表至多一行。`StateRuntime::init` 启动时会确保这行存在
（`ensure_backfill_state_row`）。backfill 是 DB 索引可被安全重建的兜底机制——
即使删库，重新扫描 JSONL 即可恢复 `threads`。

---

### 2.4 `agent_jobs` 表

**创建 Migration**：`0014_agent_jobs.sql`
**功能**：批处理 Agent 任务（CSV 批量处理场景，每个 job 拆成多行 item）。

```sql
CREATE TABLE agent_jobs (
    id                  TEXT    PRIMARY KEY,         -- UUID
    name                TEXT    NOT NULL,            -- 用户定义的任务名
    status              TEXT    NOT NULL,            -- pending/running/completed/failed/cancelled
    instruction         TEXT    NOT NULL,            -- 对 Agent 的指令
    output_schema_json  TEXT,                        -- 结构化输出的 JSON Schema（可选）
    input_headers_json  TEXT    NOT NULL,            -- 输入 CSV 列头 JSON 数组
    input_csv_path      TEXT    NOT NULL,            -- 输入 CSV 绝对路径
    output_csv_path     TEXT    NOT NULL,            -- 输出 CSV 绝对路径
    auto_export         INTEGER NOT NULL DEFAULT 1,  -- 是否自动导出完成项到 CSV
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL,
    started_at          INTEGER,
    completed_at        INTEGER,
    last_error          TEXT,
    max_runtime_seconds INTEGER                      -- [0015] 单 item 运行超时（秒）
);
```

---

### 2.5 `agent_job_items` 表

**创建 Migration**：`0014_agent_jobs.sql`（与 `agent_jobs` 同一 migration）
**功能**：批处理任务的每行数据项，每项对应一次独立的 Thread 执行。

```sql
CREATE TABLE agent_job_items (
    job_id              TEXT    NOT NULL,            -- 关联 agent_jobs.id（外键，级联删除）
    item_id             TEXT    NOT NULL,            -- item UUID
    row_index           INTEGER NOT NULL,            -- 原始 CSV 行号（0-based）
    source_id           TEXT,                        -- 可选来源标识（外部系统 record ID）
    row_json            TEXT    NOT NULL,            -- 该行完整数据（JSON，键对应 input_headers_json）
    status              TEXT    NOT NULL,            -- pending/running/completed/failed/skipped
    assigned_thread_id  TEXT,                        -- 执行该项的 Thread ID（软引用）
    attempt_count       INTEGER NOT NULL DEFAULT 0,  -- 已尝试次数
    result_json         TEXT,                        -- 执行结果（结构由 output_schema_json 定义）
    last_error          TEXT,
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL,
    completed_at        INTEGER,
    reported_at         INTEGER,                     -- 结果上报外部系统的时间
    PRIMARY KEY (job_id, item_id),
    FOREIGN KEY (job_id) REFERENCES agent_jobs(id) ON DELETE CASCADE
);
```

---

### 2.6 `thread_spawn_edges` 表

**创建 Migration**：`0021_thread_spawn_edges.sql`
**功能**：记录父子 Thread 关系图（有向边），支撑多层 Agent 树。

```sql
CREATE TABLE thread_spawn_edges (
    parent_thread_id    TEXT    NOT NULL,            -- 父 Thread
    child_thread_id     TEXT    NOT NULL PRIMARY KEY,-- 子 Thread（PK 保证每个子至多一个父）
    status              TEXT    NOT NULL             -- running/completed/failed/cancelled
);
```

业务逻辑：通过 CTE 递归查询可获取整棵子树；表中**无**到 `threads` 的外键约束
（允许在 Thread 行落库前先记录边关系）。

---

### 2.7 `remote_control_enrollments` 表

**创建 Migration**：`0024_remote_control_enrollments.sql`
**功能**：App Server 远程控制的注册信息，记录客户端已配对的服务器端点。

```sql
CREATE TABLE remote_control_enrollments (
    websocket_url           TEXT    NOT NULL,        -- WebSocket 服务器 URL
    account_id              TEXT    NOT NULL,        -- 关联账号 ID
    app_server_client_name  TEXT    NOT NULL,        -- App Server 客户端名
    server_id               TEXT    NOT NULL,        -- 远程服务器 ID
    environment_id          TEXT    NOT NULL,        -- 远程环境 ID
    server_name             TEXT    NOT NULL,        -- 服务器可读名（UI 展示）
    updated_at              INTEGER NOT NULL,
    PRIMARY KEY (websocket_url, account_id, app_server_client_name)
);
```

---

## 3. 目标库（goals_1.sqlite）

- **文件路径**：`{sqlite_home}/goals_1.sqlite`
- **Migration 目录**：`codex-rs/state/goals_migrations/`（当前仅 `0001`）
- **Migrator**：`GOALS_MIGRATOR`
- **访问层**：`StateRuntime::thread_goals()` → `GoalStore`（`runtime/goals.rs`）；
  上层逻辑在 `core/src/goals.rs`（计费、续期、状态机）。

> 历史：`thread_goals` 最初建于 state 库（0029），0033 重建以扩充状态枚举，
> 0034 从 state 库 `DROP`。它现在独占 `goals_1.sqlite`，**不再有**指向
> `threads` 的外键（跨库无法施加 FK）。

### 3.1 `thread_goals` 表

**创建 Migration**：`goals_migrations/0001_thread_goals.sql`
**功能**：会话目标（Goal）追踪，支持 token / 时间预算控制与进度监控。

```sql
CREATE TABLE thread_goals (
    thread_id           TEXT    PRIMARY KEY NOT NULL,  -- 1:1 对应 Thread（无 FK，跨库）
    goal_id             TEXT    NOT NULL,              -- 目标 ID（UUID）
    objective           TEXT    NOT NULL,              -- 目标的自然语言描述
    -- 状态枚举（CHECK 约束，6 种值）：
    --   active         正在执行
    --   paused         已暂停（外部请求）
    --   blocked        被阻塞（等待外部条件）            ← 0033 新增
    --   usage_limited  达到使用量上限（速率 / 配额）      ← 0033 新增
    --   budget_limited 达到 token / 时间预算上限
    --   complete       已完成
    status              TEXT    NOT NULL CHECK(status IN (
                            'active','paused','blocked',
                            'usage_limited','budget_limited','complete')),
    token_budget        INTEGER,                        -- token 预算（NULL = 无限制）
    tokens_used         INTEGER NOT NULL DEFAULT 0,
    time_used_seconds   INTEGER NOT NULL DEFAULT 0,
    created_at_ms       INTEGER NOT NULL,               -- 毫秒精度时间戳
    updated_at_ms       INTEGER NOT NULL
);
```

业务逻辑：每个 turn 结束时由目标扩展累加 `tokens_used` / `time_used_seconds`；
超过 `token_budget` 时转 `budget_limited`；遇限流 / 配额转 `usage_limited`。状态
机的完整语义见文档 [`07_goal_mode.md`](./07_goal_mode.md)。

---

## 4. 记忆库（memories_1.sqlite）

- **文件路径**：`{sqlite_home}/memories_1.sqlite`
- **Migration 目录**：`codex-rs/state/memory_migrations/`（当前仅 `0001`）
- **Migrator**：`MEMORIES_MIGRATOR`
- **访问层**：`StateRuntime::memories()` → `MemoryStore`（`runtime/memories.rs`）。
  注意 `MemoryStore` 同时持有 state 库连接的句柄，因为记忆筛选要按
  `threads.memory_mode = 'enabled'` 过滤——这是少数需要**跨库读**的场景，靠应用层
  两步查询而非 SQL JOIN 实现。
- **整库清空**：`StateRuntime::clear_memory_data_in_sqlite_home` 支撑
  `codex /memory clear`，可单独删本库而不动其它三库。

> 历史：记忆相关表最初建于 state 库（0006/0009/0016/0017/0018），0035 从 state
> 库 `DROP TABLE jobs; DROP TABLE stage1_outputs;`，迁入本库。本库版本**不再有**
> 指向 `threads` 的外键。

### 4.1 `stage1_outputs` 表

**创建 Migration**：`memory_migrations/0001_memories.sql`
**功能**：记忆系统 Phase 1 输出——对单个会话提取的记忆摘要（per-thread）。

```sql
CREATE TABLE stage1_outputs (
    thread_id                             TEXT    PRIMARY KEY, -- 1:1 对应 Thread（无 FK）
    source_updated_at                     INTEGER NOT NULL,    -- 源 JSONL 最后修改时间（增量检测）
    raw_memory                            TEXT    NOT NULL,    -- LLM 生成的原始记忆文本
    rollout_summary                       TEXT    NOT NULL,    -- 会话摘要（向 LLM 汇报历史概要）
    rollout_slug                          TEXT,                -- 会话短标识（关联 JSONL）
    generated_at                          INTEGER NOT NULL,    -- 记忆生成时间
    usage_count                           INTEGER,             -- 被引用次数
    last_usage                            INTEGER,             -- 最后被引用时间
    selected_for_phase2                   INTEGER NOT NULL DEFAULT 0, -- 是否入选 Phase 2 全局汇总
    selected_for_phase2_source_updated_at INTEGER             -- Phase 2 处理时的快照水位线
);
```

记忆系统两阶段：**Stage 1（per-thread）** 对单会话生成 `raw_memory` 与
`rollout_summary`；**Phase 2（global）** 从入选的 Stage 1 输出汇总全局记忆。详见
文档 [`12_memory_system.md`](./12_memory_system.md)。

### 4.2 `jobs` 表

**创建 Migration**：`memory_migrations/0001_memories.sql`（与 `stage1_outputs` 同一）
**功能**：记忆流水线的后台任务队列（Stage 1 与 Phase 2 两类作业）。

```sql
CREATE TABLE jobs (
    kind                    TEXT    NOT NULL,   -- "memory_stage1" / "memory_consolidate_global"
    job_key                 TEXT    NOT NULL,   -- Stage 1 用 thread_id；Phase 2 用固定串 "global"
    status                  TEXT    NOT NULL,   -- pending/running/done/failed
    worker_id               TEXT,               -- 当前持有任务的 worker
    ownership_token         TEXT,               -- 抢占式锁令牌（防多进程并发执行同一任务）
    started_at              INTEGER,
    finished_at             INTEGER,
    lease_until             INTEGER,            -- 租约到期（超时后可被其它 worker 抢占）
    retry_at                INTEGER,            -- 下次重试时间
    retry_remaining         INTEGER NOT NULL,   -- 剩余重试次数
    last_error              TEXT,
    input_watermark         INTEGER,            -- 输入水位线（增量处理游标）
    last_success_watermark  INTEGER,
    PRIMARY KEY (kind, job_key)
);
```

任务调度：`ownership_token` + `lease_until` 实现应用层分布式锁；worker 崩溃后租约
到期任务可被重认领；`retry_remaining` 控制失败重试。

---

## 5. 日志库（logs_2.sqlite）

- **文件路径**：`{sqlite_home}/logs_2.sqlite`
- **Migration 目录**：`codex-rs/state/logs_migrations/`（`0001`、`0002`）
- **Migrator**：`LOGS_MIGRATOR`
- **历史**：日志表最初建于 state 库（0002~0012），0023（`drop_logs.sql`）从主库
  `DROP TABLE logs` 并开启 `PRAGMA auto_vacuum = INCREMENTAL`；日志迁入本独立库。
  `logs_migrations/0002` 将原 `message` 列重命名为 `feedback_log_body`（整表重建）。

### 5.1 `logs` 表

**创建 Migration**：`logs_migrations/0001_logs.sql`（0002 重建改列名）

```sql
CREATE TABLE logs (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,  -- 自增主键（便于分页）
    ts                INTEGER NOT NULL,                   -- Unix 秒
    ts_nanos          INTEGER NOT NULL,                   -- 纳秒部分（与 ts 组合得纳秒精度）
    level             TEXT    NOT NULL,                   -- TRACE/DEBUG/INFO/WARN/ERROR
    target            TEXT    NOT NULL,                   -- tracing target（Rust 模块路径）
    feedback_log_body TEXT,                               -- [0002] 实际日志内容（原名 message）
    module_path       TEXT,
    file              TEXT,
    line              INTEGER,
    thread_id         TEXT,                               -- 关联 Thread（NULL = 非会话日志）
    process_uuid      TEXT,                               -- 产生日志的进程实例
    estimated_bytes   INTEGER NOT NULL DEFAULT 0          -- 预估字节数（分区容量控制）
);
```

**日志保留策略**（`StateRuntime` 启动维护 + 每批 INSERT 后修剪）：
- 按 `thread_id` 分区：每个 Thread 保留固定字节 / 行数的滑动窗口
  （`LOG_PARTITION_SIZE_LIMIT_BYTES = 10 MiB`、`LOG_PARTITION_ROW_LIMIT = 1000`）。
- 按 `process_uuid` 分区（`thread_id IS NULL` 的 threadless 日志）：按进程实例保留。

---

## 6. 已废弃 / 迁移的表（历史）

下列表曾出现在 state 库，现已删除或迁出。**新代码不应再引用它们在 state 库中的
位置**；本节仅供阅读旧 migration / 旧文档时对照。

| 表 | 曾建于 | 现状 | 现位置 |
|----|--------|------|--------|
| `logs` | state 0002 | state 库 0023 `DROP` | 迁至 `logs_2.sqlite` |
| `device_key_bindings` | state 0028 | state 库 0031 `DROP`（彻底删除） | 无（功能已撤） |
| `thread_goals` | state 0029（0033 重建） | state 库 0034 `DROP` | 迁至 `goals_1.sqlite` |
| `stage1_outputs` | state 0006 | state 库 0035 `DROP` | 迁至 `memories_1.sqlite` |
| `jobs` | state 0006 | state 库 0035 `DROP` | 迁至 `memories_1.sqlite` |

> `device_key_bindings` 是唯一被**彻底删除而未迁移**的表：0028 引入、0031 删除，
> 对应的设备密钥绑定功能已从产品中移除。

---

## 7. 表关系 ER 图

```
state_5.sqlite（主状态库）
═══════════════════════════════════════════════════════════════════════════

  ┌────────────────────────────────────────────────────────────────────┐
  │                              threads                                 │
  │────────────────────────────────────────────────────────────────────│
  │ PK id TEXT                                                           │
  │    rollout_path TEXT ────────────────────────────► sessions/*.jsonl  │
  │    source / model_provider / model / reasoning_effort TEXT           │
  │    cwd / title / sandbox_policy / approval_mode TEXT                 │
  │    agent_nickname / agent_role / agent_path / thread_source TEXT     │
  │    memory_mode TEXT   ← 记忆流水线据此过滤                            │
  │    tokens_used / archived / created_at_ms / updated_at_ms INTEGER    │
  │    preview / first_user_message TEXT  ← 列表预览                     │
  └────────────────────────────────────────────────────────────────────┘
       │ 1:N (CASCADE)                         ▲ 软引用（无 FK）
       ▼                                       │
  ┌──────────────────────┐          ┌──────────────────────┐
  │ thread_dynamic_tools │          │  thread_spawn_edges   │
  │──────────────────────│          │──────────────────────│
  │PK(thread_id,position)│          │PK child_thread_id     │
  │  name / description  │          │   parent_thread_id ───┼─► threads.id（软）
  │  input_schema        │          │   status              │
  │  defer_loading       │          └──────────────────────┘
  │  namespace           │
  └──────────────────────┘

  ┌──────────────────────────┐  1:N (CASCADE)  ┌──────────────────────────┐
  │        agent_jobs        │────────────────►│      agent_job_items     │
  │──────────────────────────│                 │──────────────────────────│
  │ PK id TEXT               │                 │ PK(job_id, item_id)      │
  │   status / instruction   │                 │   row_index / row_json   │
  │   input_csv_path         │                 │   status                 │
  │   output_csv_path        │                 │   assigned_thread_id ────┼─► threads.id（软）
  │   max_runtime_seconds    │                 │   result_json            │
  └──────────────────────────┘                 └──────────────────────────┘

  ┌──────────────────────────────────┐   ┌──────────────────────────────┐
  │   remote_control_enrollments     │   │       backfill_state         │
  │──────────────────────────────────│   │──────────────────────────────│
  │ PK(websocket_url, account_id,    │   │ PK id=1（singleton）         │
  │    app_server_client_name)       │   │   status / last_watermark    │
  │   server_id / environment_id     │   │   last_success_at            │
  │   server_name / updated_at       │   └──────────────────────────────┘
  └──────────────────────────────────┘

goals_1.sqlite                         memories_1.sqlite
══════════════════════════             ═══════════════════════════════════════
  ┌──────────────────────┐               ┌──────────────────┐  ┌──────────────┐
  │     thread_goals     │               │  stage1_outputs  │  │     jobs     │
  │──────────────────────│               │──────────────────│  │──────────────│
  │PK thread_id（软引用）│               │PK thread_id（软）│  │PK(kind,      │
  │  goal_id / objective │               │  raw_memory      │  │   job_key)   │
  │  status CHECK(6 值)  │               │  rollout_summary │  │  status      │
  │  token_budget        │               │  usage_count     │  │  worker_id   │
  │  tokens_used         │               │  selected_for    │  │  lease_until │
  │  time_used_seconds   │               │    _phase2       │  │  retry_*     │
  └──────────────────────┘               └──────────────────┘  └──────────────┘
   ↑ thread_id 均为软引用：跨库无法施加外键，由应用层保证一致性

logs_2.sqlite（日志库）
═══════════════════════════════════════════════════════════════════════════
  ┌────────────────────────────────────────────────────────────────────┐
  │                                logs                                  │
  │────────────────────────────────────────────────────────────────────│
  │ PK id INTEGER AUTOINCREMENT                                          │
  │    ts / ts_nanos INTEGER     ← 纳秒精度时间戳                        │
  │    level / target TEXT                                               │
  │    feedback_log_body TEXT                                            │
  │    thread_id TEXT ───────────────────────────────► threads.id（软）  │
  │    process_uuid TEXT          ← threadless 日志按进程分区            │
  │    estimated_bytes INTEGER                                           │
  └────────────────────────────────────────────────────────────────────┘
```

> **跨库无外键**：只有 state 库内部的 `thread_dynamic_tools`、`agent_job_items`
> 有真正的 `FOREIGN KEY ... CASCADE`。goals / memories / logs 三库与 `threads`
> 的关系全是「软引用」——靠 `thread_id` 字符串匹配，一致性由应用层维护。

---

## 8. 索引设计说明

### 8.1 `threads` 表索引（state 库）

| 索引名 | 字段 | Migration | 用途 |
|--------|------|-----------|------|
| `idx_threads_created_at` | `(created_at DESC, id DESC)` | 0001 | 按创建时间分页（legacy 秒精度） |
| `idx_threads_updated_at` | `(updated_at DESC, id DESC)` | 0001 | 按更新时间分页（legacy 秒精度） |
| `idx_threads_archived` | `(archived)` | 0001 | 过滤归档 / 活跃 |
| `idx_threads_source` | `(source)` | 0001 | 按来源过滤 |
| `idx_threads_provider` | `(model_provider)` | 0001 | 按供应商过滤 |
| `idx_threads_created_at_ms` | `(created_at_ms DESC, id DESC)` | 0025 | 按创建时间分页（毫秒，当前用） |
| `idx_threads_updated_at_ms` | `(updated_at_ms DESC, id DESC)` | 0025 | 按更新时间分页（毫秒，当前用） |
| `idx_threads_archived_cwd_created_at_ms` | `(archived, cwd, created_at_ms DESC, id DESC)` | 0027 | 「当前目录」视图按创建时间排序 |
| `idx_threads_archived_cwd_updated_at_ms` | `(archived, cwd, updated_at_ms DESC, id DESC)` | 0027 | 同上，按更新时间排序 |

设计要点：所有时间排序索引都带 `id DESC` 作 tiebreaker，保证同毫秒内排序确定；
0025 保留旧秒级索引以向后兼容；`(archived, cwd, *_ms)` 复合索引服务 TUI 的
「只看当前目录会话」高频场景。

### 8.2 state 库其它索引

| 表 | 索引名 | 字段 | 用途 |
|----|--------|------|------|
| `thread_dynamic_tools` | `idx_thread_dynamic_tools_thread` | `(thread_id)` | 取某会话全部工具 |
| `agent_jobs` | `idx_agent_jobs_status` | `(status, updated_at DESC)` | 按状态列任务 |
| `agent_job_items` | `idx_agent_job_items_status` | `(job_id, status, row_index ASC)` | job 内按状态过滤、按行号处理 |
| `thread_spawn_edges` | `idx_thread_spawn_edges_parent_status` | `(parent_thread_id, status)` | 按父节点 + 状态查子 Agent |

### 8.3 memories 库索引

| 表 | 索引名 | 字段 | 用途 |
|----|--------|------|------|
| `stage1_outputs` | `idx_stage1_outputs_source_updated_at` | `(source_updated_at DESC, thread_id DESC)` | 记忆增量处理：优先最近更新的会话 |
| `jobs` | `idx_jobs_kind_status_retry_lease` | `(kind, status, retry_at, lease_until)` | 调度器核心查询：按类型 / 状态找可执行任务 |

### 8.4 logs 库索引

| 索引名 | 字段 | 条件 | 用途 |
|--------|------|------|------|
| `idx_logs_ts` | `(ts DESC, ts_nanos DESC, id DESC)` | — | 全局时间顺序查询 |
| `idx_logs_thread_id` | `(thread_id)` | — | 按会话查日志 |
| `idx_logs_thread_id_ts` | `(thread_id, ts DESC, ts_nanos DESC, id DESC)` | — | 会话日志分页（主要使用） |
| `idx_logs_process_uuid_threadless_ts` | `(process_uuid, ts DESC, ts_nanos DESC, id DESC)` | `WHERE thread_id IS NULL` | threadless 日志查询（Partial Index） |

> goals 库的 `thread_goals` 仅有主键索引（`thread_id`），无额外二级索引。

---

## 9. JSONL Rollout 文件格式

### 9.1 文件位置与命名

```
~/.codex/sessions/rollout-{YYYY-MM-DDTHH-MM-SS}-{UUID}.jsonl
~/.codex/archived_sessions/rollout-{YYYY-MM-DDTHH-MM-SS}-{UUID}.jsonl
```

- 时间戳分隔符用 `-` 而非 `:`（规避文件系统限制）；UUID 对应 `threads.id`。
- `sessions/` 为活跃会话；归档后移至 `archived_sessions/`。

### 9.2 文件格式

每行是一个独立 JSON 对象（`RolloutLine`）：

```json
{ "timestamp": "2024-11-15T09:30:45.123456Z", "type": "session_meta", "payload": { ... } }
```

`timestamp` 为 RFC 3339 UTC；`type` + `payload` 通过 serde `tag = "type"` 映射到
`RolloutItem` 枚举。

### 9.3 `RolloutItem` 类型枚举

#### `"session_meta"` — 会话元数据（首行）

记录会话初始配置：`id`（= `threads.id`）、`cwd`、`originator`、`cli_version`、
`source`、`model_provider`、`agent_*`、`base_instructions`、`dynamic_tools`
（同步入 `thread_dynamic_tools`）、`memory_mode`、`git`、`forked_from_id`（若由
其它会话 fork 而来）等。

#### `"response_item"` — AI 响应与工具调用

对话历史核心，`payload` 是 `ResponseItem`（OpenAI Responses API 格式）。常见子类型：
`message`（user/assistant）、`function_call`（工具调用）、
`function_call_output`（工具结果）、`reasoning`、`custom_tool_call` 等。

#### `"turn_context"` — 上下文快照

每个用户 turn 开始时写入，记录当时的 `cwd`、`model`、`effort`、
`approval_policy`、`sandbox_policy` 等，支持 resume 时正确还原运行时状态。

#### `"compacted"` — 上下文压缩快照

历史过长时压缩后写入，含压缩摘要与可选 `replacement_history`，resume 时还原可用
上下文。压缩机制详见文档 16（Agent 优化）的上下文压缩章节。

#### `"event_msg"` — 自定义事件消息

记录非对话类系统事件（工具状态更新、Agent 生命周期事件等）。

### 9.4 文件读写机制

- **写入**：`RolloutRecorder`（`codex-rollout`）维护异步写 channel，将每个
  `RolloutLine` 序列化后 append 到文件末尾（append-only）。
- **读取**：逐行解析，遇无法解析的行计入 `parse_errors` 但不中断。
- **恢复**：读取 JSONL 重建对话历史，`turn_context` 与 `compacted` 帮助跳过不必要
  的历史重放。

---

## 10. Migration 演化历史

四个库各有独立的 migration 序列。下表先梳理 **state 库 0001~0035**，再列出三个
独立库。

### 10.1 state 库（`migrations/`，0001~0035）

| 阶段 | Migration | 功能 |
|------|-----------|------|
| 核心基础 | `0001_threads` | 建 `threads` 表 + 5 个初始索引 |
| | `0002_logs` / `0003_logs_thread_id` | 在主库建 `logs`（后于 0023 迁出） |
| | `0004_thread_dynamic_tools` | 动态工具支持 |
| | `0005_threads_cli_version` | `threads.cli_version` |
| 记忆系统 | `0006_memories` | 主库建 `stage1_outputs` + `jobs`（后于 0035 迁出） |
| | `0007_threads_first_user_message` | 列表预览字段 |
| | `0008_backfill_state` | backfill 游标表（从 JSONL 重建 threads） |
| | `0009_stage1_outputs_rollout_slug` | 记忆关联 JSONL 的 slug |
| | `0010`~`0012` logs_* | `logs` 增 `process_uuid` / 分区剪枝索引 / `estimated_bytes` |
| Agent 扩展 | `0013_threads_agent_nickname` | `agent_nickname` / `agent_role` |
| | `0014_agent_jobs` | `agent_jobs` + `agent_job_items` |
| | `0015_agent_jobs_max_runtime_seconds` | 单 item 超时 |
| | `0016_memory_usage` | 记忆 `usage_count` / `last_usage` |
| | `0017_phase2_selection_flag` | `selected_for_phase2` |
| | `0018_phase2_selection_snapshot` | Phase 2 快照水位线 **+ `threads.memory_mode`** |
| 精细化控制 | `0019_thread_dynamic_tools_defer_loading` | 工具懒加载 |
| | `0020_threads_model_reasoning_effort` | `model` / `reasoning_effort` |
| | `0021_thread_spawn_edges` | 父子 Agent 关系图 |
| | `0022_threads_agent_path` | 多层 Agent 路径寻址 |
| | `0023_drop_logs` | **从主库删 `logs`**，开启 `auto_vacuum = INCREMENTAL` |
| 远控 / 精度 | `0024_remote_control_enrollments` | 远程控制注册 |
| | `0025_thread_timestamps_millis` | **毫秒精度升级**：新增 `*_ms` 列 + 迁移存量 + 4 触发器 + 毫秒索引 |
| | `0026_thread_dynamic_tools_namespace` | 工具命名空间 |
| | `0027_threads_cwd_sort_indexes` | `(archived, cwd, *_ms)` 复合索引 |
| 目标 / 来源 | `0028_device_key_bindings` | 设备密钥绑定（后于 0031 删除） |
| | `0029_thread_goals` | 主库建 `thread_goals`（4 状态枚举） |
| | `0030_threads_thread_source` | `threads.thread_source` |
| 拆库重构 | `0031_drop_device_key_bindings` | **删除** `device_key_bindings`（功能撤销） |
| | `0032_threads_preview` | `threads.preview`（列表预览，回填回退到目标 objective） |
| | `0033_thread_goal_stopped_statuses` | 重建 `thread_goals`，状态枚举扩至 6 值（+blocked/+usage_limited） |
| | `0034_drop_thread_goals` | **从主库删 `thread_goals`**（迁至 `goals_1.sqlite`） |
| | `0035_drop_memory_tables` | **从主库删 `jobs` / `stage1_outputs`**（迁至 `memories_1.sqlite`） |

### 10.2 独立库

| 库 | 目录 | Migration | 功能 |
|----|------|-----------|------|
| goals | `goals_migrations/` | `0001_thread_goals` | 在独立库重建 `thread_goals`（6 状态，无 FK） |
| memories | `memory_migrations/` | `0001_memories` | 在独立库重建 `stage1_outputs` + `jobs`（无 FK） |
| logs | `logs_migrations/` | `0001_logs` | 独立库建 `logs` 表 + 索引 |
| logs | `logs_migrations/` | `0002_logs_feedback_log_body` | 整表重建，`message` → `feedback_log_body` |

### 10.3 向后兼容策略

四库均通过 `runtime_migrator(...)`（`ignore_missing = true`）打开：允许旧版本
Codex 打开已被新版本迁移的库，而不因「发现未知 migration」报错；已知 migration
仍按 checksum 校验防数据损坏。migration 期间 SQLx 加表级锁（`locking`）防并发迁移。

---

## 11. 关键查询场景

> ⚠️ **跨库 JOIN 已不可行**：`thread_goals`（goals 库）、`stage1_outputs` / `jobs`
> （memories 库）与 `threads`（state 库）位于**不同 SQLite 文件**，普通连接无法直接
> JOIN。如确需在一条 SQL 里关联，须先 `ATTACH DATABASE`；Codex 实际采用「分库两步
> 查询，应用层拼装」的方式。下列示例已据此调整。

### 11.1 列出最近活跃 Threads（TUI 主界面，state 库）

```sql
SELECT id, rollout_path, created_at_ms, updated_at_ms,
       source, model_provider, model, cwd, title,
       first_user_message, preview, tokens_used, archived_at,
       agent_nickname, agent_role, agent_path, thread_source
FROM threads
WHERE archived = 0
ORDER BY updated_at_ms DESC, id DESC
LIMIT 50;

-- 当前目录过滤（命中 0027 复合索引）
SELECT * FROM threads
WHERE archived = 0 AND cwd = '/home/user/my-project'
ORDER BY updated_at_ms DESC, id DESC
LIMIT 50;
```

### 11.2 查询某 Thread 详情（分库两步）

```sql
-- 步骤 1：state 库取元数据 + 动态工具
SELECT * FROM threads WHERE id = '550e8400-...';
SELECT namespace, name, description, input_schema, defer_loading
FROM thread_dynamic_tools WHERE thread_id = '550e8400-...' ORDER BY position;

-- 步骤 2：memories 库（memories_1.sqlite）单独查记忆
SELECT raw_memory, rollout_summary, usage_count
FROM stage1_outputs WHERE thread_id = '550e8400-...';

-- 步骤 3：goals 库（goals_1.sqlite）单独查目标
SELECT goal_id, objective, status, token_budget, tokens_used, time_used_seconds
FROM thread_goals WHERE thread_id = '550e8400-...';
```

### 11.3 查询某 Thread 的 Agent 子树（state 库，CTE 递归）

```sql
WITH RECURSIVE descendants(child_thread_id, depth) AS (
    SELECT child_thread_id, 1 FROM thread_spawn_edges
    WHERE parent_thread_id = '550e8400-...'
    UNION ALL
    SELECT e.child_thread_id, d.depth + 1
    FROM thread_spawn_edges e JOIN descendants d ON e.parent_thread_id = d.child_thread_id
)
SELECT d.child_thread_id, d.depth, t.agent_path, e.status
FROM descendants d
JOIN threads t ON t.id = d.child_thread_id
JOIN thread_spawn_edges e ON e.child_thread_id = d.child_thread_id
ORDER BY d.depth ASC, d.child_thread_id ASC;
```

### 11.4 查询 agent_job 进度（state 库）

```sql
SELECT j.id, j.name, j.status,
       COUNT(*) AS total_items,
       SUM(ji.status = 'completed') AS completed_items,
       SUM(ji.status = 'failed')    AS failed_items,
       SUM(ji.status = 'running')   AS running_items
FROM agent_jobs j
LEFT JOIN agent_job_items ji ON ji.job_id = j.id
WHERE j.id = 'job-uuid' GROUP BY j.id;
```

### 11.5 记忆系统：找可认领的 Stage 1 任务（memories 库）

```sql
-- 注意：本查询运行在 memories_1.sqlite，而非 state 库
SELECT kind, job_key, status, retry_remaining, last_error
FROM jobs
WHERE kind = 'memory_stage1'
  AND status IN ('pending', 'failed')
  AND (retry_at    IS NULL OR retry_at    <= strftime('%s','now'))
  AND (lease_until IS NULL OR lease_until <= strftime('%s','now'))
  AND retry_remaining > 0
ORDER BY retry_at ASC NULLS FIRST;
```

### 11.6 查询某 Thread 的运行日志（logs 库）

```sql
-- 运行在 logs_2.sqlite
SELECT id, ts, ts_nanos, level, target, feedback_log_body, file, line
FROM logs
WHERE thread_id = '550e8400-...'
ORDER BY ts DESC, ts_nanos DESC, id DESC
LIMIT 100;
```

### 11.7 查询活跃目标及预算使用（goals 库）

```sql
-- 运行在 goals_1.sqlite；如需 thread 标题，另在 state 库按 thread_id 查
SELECT thread_id, goal_id, objective, status,
       token_budget, tokens_used,
       CASE WHEN token_budget IS NOT NULL
            THEN ROUND(100.0 * tokens_used / token_budget, 1) END AS token_usage_pct,
       time_used_seconds
FROM thread_goals
WHERE status = 'active'
ORDER BY updated_at_ms DESC;
```

---

## 12. 数据库文件位置说明

### 12.1 默认路径与常量

| 文件 | 默认路径 | 常量（`codex-rs/state/src/lib.rs`） |
|------|----------|------|
| 主状态库 | `~/.codex/state_5.sqlite` | `STATE_DB_FILENAME = "state_5.sqlite"` |
| 日志库 | `~/.codex/logs_2.sqlite` | `LOGS_DB_FILENAME = "logs_2.sqlite"` |
| 目标库 | `~/.codex/goals_1.sqlite` | `GOALS_DB_FILENAME = "goals_1.sqlite"` |
| 记忆库 | `~/.codex/memories_1.sqlite` | `MEMORIES_DB_FILENAME = "memories_1.sqlite"` |
| 活跃会话 JSONL | `~/.codex/sessions/rollout-*.jsonl` | — |
| 归档会话 JSONL | `~/.codex/archived_sessions/rollout-*.jsonl` | — |

### 12.2 路径自定义

- **`sqlite_home`**（四个 DB 文件的所在目录）：环境变量
  `CODEX_SQLITE_HOME`（常量 `SQLITE_HOME_ENV`）。未设置时默认等同 `$CODEX_HOME`
  （即 `~/.codex/`）。配置项见 `config_toml.rs` 的 `sqlite_home`。
- **`codex_home`**（JSONL 目录所在）：默认 `~/.codex/`，由 `Config.codex_home`
  决定。

### 12.3 路径解析与工具函数（`runtime.rs`）

```rust
pub fn state_db_path(codex_home: &Path)    -> PathBuf  // → {home}/state_5.sqlite
pub fn logs_db_path(codex_home: &Path)     -> PathBuf  // → {home}/logs_2.sqlite
pub fn goals_db_path(codex_home: &Path)    -> PathBuf  // → {home}/goals_1.sqlite
pub fn memories_db_path(codex_home: &Path) -> PathBuf  // → {home}/memories_1.sqlite

// 一次性列出全部四个库（codex doctor 用来做体检）
pub fn runtime_db_paths(codex_home: &Path) -> Vec<RuntimeDbPath>

// 对任一库做 SQLite 自带完整性校验
pub async fn sqlite_integrity_check(path: &Path) -> anyhow::Result<Vec<String>>
```

四库由 `RUNTIME_DBS: [RuntimeDbSpec; 4]` 统一描述（label / filename / kind /
open & migrate 阶段名），`StateRuntime::init` 遍历它们逐个打开 + 迁移。

### 12.4 多进程并发安全

- 四库均用 WAL 模式：多读并发、单写独占。
- 拆库本身降低锁竞争——日志的高频写不再阻塞主库前台读写。
- `jobs` 表（memories 库）用 `ownership_token` + `lease_until` 实现应用层分布式锁，
  防止多个 Codex 进程并发执行同一记忆任务。
- `ignore_missing = true` 允许新旧二进制在同一组库上并存运行。

---

*文档基于 `codex-rs/state/migrations/`（0001~0035，共 35 个）以及
`logs_migrations/`、`goals_migrations/`、`memory_migrations/` 三个独立目录的
实际 SQL，并对照 `codex-rs/state/src/{lib,runtime,migrations}.rs` 源码编写。*
