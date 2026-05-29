# Codex 源码学习 · AI 中文注释规范 v2

> 规定六种注释类型的格式、触发条件、取舍原则，以及影响可读性的排版约定。
> AI 生成注释时必须严格遵守，不得随意增减注释密度。

---

## Rust 适配速查（codex-rs 项目专用）

本规范的示例以 TypeScript 撰写，应用到 codex-rs (Rust) 时按下表映射：

| TypeScript 概念 | Rust 等价物 |
|----------------|------------|
| `// 单行注释` | `// 单行注释`（相同） |
| `/** JSDoc 块 */` | `/// 行文档注释`（每行一个 `///`，推荐）或 `/** 块文档 */` |
| 文件顶部 `/** */` 文件注释 | `//! 模块级文档注释`（仅文件首部可用） |
| `function name() {}` | `fn name() {}` |
| `async function` | `async fn` |
| `@param` / `@returns` | Rust 文档惯例用 `# Arguments` / `# Returns` 段落，但**中文注释可沿用 `@param` / `@returns` 风格**以便和原文对齐 |

**关键约定**：
- Rust 公开 API 的英文文档通常已是 `///` 形式；中文注释**紧跟其后**，同样使用 `///`。
- Rust 模块文件顶部的英文 `//!` 注释保留原样，中文模块说明同样用 `//!`，紧跟原文之后。
- 行内逻辑注释统一用 `//`。

---

## 零、两条铁律

**1. 不删除任何原有英文注释。**
原文注释是作者的第一手意图，不可替代。中文注释只做补充，紧跟在原文之后。

**2. 注释的敌人有两个：太少和太多。**
判断标准只有一条：「去掉这条注释，读者是否会误解或浪费时间？」
答案是否，就不写。

---

## 一、中英文对照原则

### 1.1 对照结构

所有中文注释**紧跟**原有英文注释之后，不插入其间，不替换原文。

```typescript
// Flatten the tool call results into a single message array.
// 将工具调用结果展平为单个消息数组，供下一轮 prompt 使用。
```

多行英文块注释：英文块完整保留，中文块紧跟其后，用同样的注释符号。

```typescript
/**
 * Executes a single agent turn: sends messages to the model,
 * streams the response, and handles any tool calls returned.
 *
 * @param messages - Full conversation history including system prompt
 * @param tools    - Available tools for this turn
 * @returns        - Updated message history after model response
 */
/**
 * 执行单轮 Agent 循环：向模型发送消息、流式接收响应、处理工具调用。
 *
 * @param messages - 完整消息历史，含 system prompt
 * @param tools    - 本轮可用的工具列表
 * @returns        - 模型响应后的完整消息历史（含新增消息）
 *
 * 副作用：就地修改 messages 数组（追加 assistant/tool 消息）
 * 异常：工具执行失败时返回 error tool_result，不向上抛出
 */
```

### 1.2 原文有注释 vs 原文无注释

| 情况 | 处理方式 |
|------|---------|
| 原文有英文注释 | 保留原文 → 中文对照紧跟其后 |
| 原文无注释，但该位置需要补充说明 | 直接写中文注释，无需英文对照 |
| 原文注释是临时 TODO/FIXME/HACK | 保留原文，可在后面加中文说明背景 |

### 1.3 对照翻译质量要求

不是逐字直译，而是**意译 + 补充**：在忠实原意的基础上，补充原文
未说明的「为什么」，尤其是 Codex 特定的设计背景。

```typescript
// Bad：逐字直译，无增量信息
// Retry up to 3 times on failure.
// 失败时最多重试 3 次。

// Good：意译 + 补充设计背景
// Retry up to 3 times on failure.
// 最多重试 3 次：实验值，超过 3 次通常意味着模型陷入固定错误模式，
// 继续重试无意义。具体策略见 RetryPolicy 配置。
```

---

## 二、文件注释

### 作用

建立心智地图：读者打开文件的第一秒就知道这里干什么、
在系统里处于什么位置、值不值得深读。

### 格式

原有英文文件头完整保留，中文文件注释紧跟其后：

```typescript
// =============================================================================
// [原有英文文件注释，原样保留]
// =============================================================================

/**
 * 【文件职责】一句话。动词开头，说清楚这个文件「做什么」。
 *
 * 【架构位置】
 *   层级：CLI 入口层 / Agent 核心层 / 工具执行层 / 上下文管理层
 *   上游：谁调用它（文件名）
 *   下游：它调用谁（文件名）
 *
 * 【数据流】输入 → [本文件做的事] → 输出
 *
 * 【阅读建议】先看 xxx()，再看 yyy()；zzz() 是工具函数可跳过。
 */
```

### 示例

```typescript
// This module implements the core agent loop for Codex.
// It manages the turn-based interaction between the user, the model,
// and any tools the model chooses to invoke.

/**
 * 【文件职责】实现 Codex 的 Agent 主循环，驱动用户、模型、工具三方的轮次交互。
 *
 * 【架构位置】
 *   层级：Agent 核心层
 *   上游：cli/index.ts（传入初始消息和 AgentConfig）
 *   下游：tools/executor.ts（工具执行）、openai/stream.ts（流式 API）
 *
 * 【数据流】
 *   初始 Message[] → runLoop() → handleToolCalls() → 最终 AssistantMessage
 *
 * 【阅读建议】从 runLoop() 入口开始，重点看 handleToolCalls() 的递归逻辑，
 *              跳过底部的格式化工具函数。
 */
```

### 取舍规则

| 情况 | 要不要写中文文件注释 |
|------|-------------------|
| 架构关键文件（主循环、工具分发、沙箱通信） | ✅ 必须写 |
| 职责不明显的文件（utils.ts、helpers.ts） | ✅ 必须写 |
| 文件名完全自解释（constants.ts、types.ts） | 只写【文件职责】一行 |
| 纯类型定义、纯常量文件 | ❌ 不写，类型名已自解释 |

---

## 三、函数注释

### 作用

描述函数的「契约」：调用方需要知道什么。
不描述实现细节——那是函数体内步骤注释的工作。

### 格式（按复杂度分两档）

**档位 A：简单函数（≤15 行、无副作用、命名自解释）**

原文注释（如有）原样保留，中文单行紧跟：

```typescript
// Returns the total token count across all messages in the history.
// 返回消息历史中所有消息的 token 总数，用于判断是否需要压缩上下文。
function countTokens(messages: Message[]): number
```

无原文注释时，直接写中文单行：

```typescript
// 将多段 delta token 拼接为完整字符串，过滤掉 null/undefined 片段。
function joinDeltas(deltas: (string | null)[]): string
```

**档位 B：复杂函数（有副作用 / 并发 / 多分支 / 递归）**

英文 JSDoc 完整保留，中文 JSDoc 块紧跟其后：

```typescript
/**
 * Handle all tool calls returned by the model in a single turn.
 * Executes each tool serially and appends results to the message history.
 * Loops until the model stops requesting tool calls.
 *
 * @param messages  - Conversation history (mutated in place)
 * @param toolCalls - Tool calls from the current model response
 * @returns         - Updated message history
 */
/**
 * 处理模型在单轮中发出的所有 tool_call。
 * 串行执行每个工具并将结果追加到消息历史，循环直到模型不再发出 tool_call。
 *
 * @param messages  - 完整消息历史（就地修改，调用方可见变更）
 * @param toolCalls - 当前模型响应中的 tool_call 列表
 * @returns         - 追加了所有 tool_result 的消息历史
 *
 * 副作用：修改 messages（追加 tool/assistant 消息）
 * 异常：工具执行失败时返回 error tool_result，不向上抛出
 * 性能：每次工具调用都会触发新一轮 API 请求，慎在热路径中调用
 */
async function handleToolCalls(
  messages: Message[],
  toolCalls: ToolCall[]
): Promise<Message[]>
```

### 取舍规则

必须写中文函数注释（档位 B）的条件，满足任一即触发：
- 就地修改参数（副作用）
- 会发网络请求 / 读写磁盘
- 返回值语义不显然
- 有意忽略的异常 case
- 递归或并发逻辑

以下情况不写注释（函数名 + 参数名已经完整描述契约）：
```typescript
// 这些函数无需注释，名字已经是文档
function clampValue(val: number, min: number, max: number): number
function isAbsolutePath(path: string): boolean
function formatBytes(bytes: number): string
```

---

## 四、步骤注释

### 作用

为函数内部的逻辑段落编号，让读者快速定位，
并说明每一步「为什么在这里做」而不是「在做什么」。

### 格式

原有英文步骤注释原样保留，中文步骤注释紧跟其后：

```typescript
async function applyPatch(patch: string, cwd: string) {
  // Step 1: Validate the patch doesn't escape the working directory.
  // Step 1：安全检查——拒绝路径穿越攻击。
  // Codex 运行在沙箱模式，任何写入 cwd 之外的操作都应被拦截。
  // 模型有可能生成 ../../etc/passwd 这类恶意路径，必须在此处防御。
  if (isPathTraversal(patch, cwd)) {
    throw new SecurityError('patch path escapes working directory');
  }

  // Step 2: Apply the unified diff using the system `patch` command.
  // Step 2：调用系统 `patch` 命令而非手动解析 diff。
  // unified diff 的边缘情况极多（CRLF、no-newline-at-EOF 等），
  // 复用系统工具比自己解析更可靠，这是有意为之的设计取舍。
  await execa('patch', ['-p1'], { input: patch, cwd });

  // Step 3: Verify the result is syntactically valid.
  // Step 3：验证 patch 后的文件语法正确。
  // 防止模型生成语法错误的补丁被静默接受，导致后续编译失败。
  await validateSyntax(cwd);
}
```

### 无原文步骤注释时的格式

```typescript
// ── Step 1：验证路径安全性 ──────────────────────────────────
// Codex 沙箱模式下，写入 cwd 之外的操作一律拦截。
// 此步骤必须在执行前完成，不能依赖后置校验。
if (isPathTraversal(patch, cwd)) { ... }

// ── Step 2：执行 patch 命令 ──────────────────────────────────
// 选用系统命令而非手动解析：unified diff 边缘情况过多，见上方函数注释。
await execa('patch', ['-p1'], { input: patch, cwd });
```

### 取舍规则

**写步骤注释的条件：**
- 函数体超过 20 行
- 逻辑上有明显的阶段划分（校验 → 执行 → 验证）
- 某一步的时机选择不显然（「为什么在这里做而不是那里」）

**不写步骤注释的情况：**
- 函数体 ≤ 10 行，逻辑一眼看穿
- 步骤只有一个，编号无意义
- 步骤描述等同于代码的逐行翻译（噪音）

**禁止：**
```typescript
// ❌ 步骤等同于代码翻译，零信息量
// Step 1：调用 isPathTraversal 函数
if (isPathTraversal(patch, cwd)) { ... }

// Step 2：调用 execa 执行 patch 命令
await execa('patch', ['-p1'], { input: patch, cwd });
```

---

## 五、变量注释

### 作用

解释变量的「语义」和「约束」，而非类型（类型系统已经表达了类型）。

### 格式

原有英文变量注释保留，中文注释紧跟：

```typescript
// Maximum number of retry attempts before giving up.
// 最大重试次数：实验值。超过 3 次通常意味着模型陷入固定错误模式，
// 继续重试无意义。调整此值前先看 RetryPolicy 的退避策略。
const MAX_RETRIES = 3;

// Timeout for individual tool execution in milliseconds.
// 单次工具执行超时（毫秒）：30s 是兼顾 git clone 等慢操作的安全上限，
// 低于此值会误杀正常的网络操作。
const TOOL_TIMEOUT_MS = 30_000;
```

无原文注释时，行尾注释（≤25 字）或行上注释（需要解释原因）：

```typescript
// 行尾：适合「是什么」已经清楚，只需补充约束
const MAX_CONTEXT_TOKENS = 100_000;  // 留 4K 给响应，不能用满 128K

// 行上：适合需要解释「为什么」
// 使用 Map 而非对象字面量：工具名可能包含特殊字符，Map 的键类型更安全。
const toolRegistry = new Map<string, ToolHandler>();
```

### 取舍规则

**必须写变量注释：**
- 魔法数字（直接写数字的常量，如超时值、重试次数、阈值）
- 命名不足以表达约束（如 `buffer`、`data`、`result`）
- 有反直觉的初始值或取值范围

**不写变量注释：**
```typescript
// ❌ 类型和名字已经说清楚了
const userName: string = '';
const isLoading: boolean = false;
const messages: Message[] = [];

// ❌ 行尾注释复述变量名
const retryCount = 0;  // 重试计数
```

---

## 六、引用范围注释

### 作用

标注一个变量、配置或常量的**作用域边界**：在哪里生效、
哪些地方依赖它、修改时需要同步更新哪些位置。

### 格式

```typescript
// [引用范围] 被 runTurn() / handleToolCalls() / retryOnFailure() 共同依赖。
// 修改此值会同时影响三处重试行为，需一并回归测试。
const MAX_RETRIES = 3;

// [引用范围] 仅在 sandbox/ipc.ts 内部使用，外部模块不直接访问。
// 如需在外部读取沙箱状态，请通过 getSandboxStatus() 接口。
const _sandboxSocket: net.Socket | null = null;
```

### 引用范围的四种典型模式

```typescript
// [引用范围 · 全局配置] 影响整个 Agent 会话，运行时不可修改。
const agentConfig: AgentConfig = loadConfig();

// [引用范围 · 模块私有] 仅 tools/executor.ts 内部可见。
const _executionQueue: ToolCall[] = [];

// [引用范围 · 跨文件共享] 由 context/store.ts 导出，被以下模块读写：
//   - agent/loop.ts（追加消息）
//   - cli/display.ts（读取展示）
//   - tools/executor.ts（追加 tool_result）
export const messageHistory: Message[] = [];

// [引用范围 · 环境变量] 从 process.env 读取，部署时配置，代码中不可修改。
const OPENAI_API_KEY = process.env.OPENAI_API_KEY;
```

### 取舍规则

**写引用范围注释的条件：**
- 导出的常量或状态（外部有多个引用方）
- 私有变量但名字容易让人误解它是可公开的
- 环境变量或外部配置
- 全局单例（singleton）

**不写引用范围：**
- 函数内的局部变量（作用域由语法结构决定，一眼可见）
- 类型定义（范围由 export 关键字表达）

---

## 七、区域划分注释

### 作用

在较长的文件中，将相关代码归组，提供视觉锚点，
让读者可以快速跳转到目标区域，无需通读全文。

### 格式

```typescript
// ═══════════════════════════════════════════════════════════════
// Types & Constants  ·  类型定义与常量
// ═══════════════════════════════════════════════════════════════

// ───────────────────────────────────────────────────────────────
// Public API  ·  对外接口
// ───────────────────────────────────────────────────────────────

// ── Internal Helpers  ·  内部工具函数 ───────────────────────────
```

三级分隔线，对应三种层级：

| 级别 | 符号 | 用途 |
|------|------|------|
| 一级（主分区） | `═══` | 文件级别的大区块，如「类型区 / 核心逻辑区 / 工具函数区」|
| 二级（小节） | `───` | 区块内的功能分组，如「工具注册 / 工具执行 / 结果处理」|
| 三级（标注） | `──` | 函数内部的逻辑段落（与步骤注释配合使用） |

### 完整示例

```typescript
// ═══════════════════════════════════════════════════════════════
// Types & Constants  ·  类型定义与常量
// ═══════════════════════════════════════════════════════════════

// [引用范围 · 全局] 所有工具执行相关的超时配置集中在此处管理。
const TOOL_TIMEOUT_MS = 30_000;
const MAX_RETRIES = 3;

type ToolResult = { output: string } | { error: string };


// ═══════════════════════════════════════════════════════════════
// Public API  ·  对外接口
// ═══════════════════════════════════════════════════════════════

// ───────────────────────────────────────────────────────────────
// Agent Loop  ·  Agent 主循环
// ───────────────────────────────────────────────────────────────

export async function runLoop(...) { ... }
export async function runTurn(...) { ... }


// ───────────────────────────────────────────────────────────────
// Tool Handling  ·  工具调用处理
// ───────────────────────────────────────────────────────────────

export async function handleToolCalls(...) { ... }
export async function executeOneTool(...) { ... }


// ═══════════════════════════════════════════════════════════════
// Internal Helpers  ·  内部工具函数
// ═══════════════════════════════════════════════════════════════

function buildSystemPrompt(...) { ... }
function countTokens(...) { ... }
function formatToolResult(...) { ... }
```

### 取舍规则

**写区域划分的条件：**
- 文件超过 150 行
- 文件内有多种性质的代码混合（类型、核心逻辑、工具函数）
- 团队约定的代码分层（public / internal / types）

**不写区域划分：**
- 文件 ≤ 80 行，分区比内容还多
- 单一职责文件（全是工具函数，或全是类型定义）
- 分区只有 1 个函数，标题比内容更显眼

---

## 八、可读性排版约定

### 8.1 中英文之间加空格

```typescript
// ❌ 混排无空格，读起来挤
// 调用OpenAI的stream API获取响应

// ✅ 中英文之间空格，提升可读性
// 调用 OpenAI 的 stream API 获取响应
```

### 8.2 注释行宽控制在 80 字符以内

超过 80 字符换行，换行后与上一行文字对齐（不是与 `//` 对齐）：

```typescript
// ✅ 正确换行对齐
// 当 context window 剩余空间不足 4K token 时，触发上下文压缩流程，
// 保留 system prompt + 最近 N 轮 + 所有 tool_result，压缩中间轮次。

// ❌ 错误：第二行顶格
// 当 context window 剩余空间不足 4K token 时，触发上下文压缩流程，
// 保留 system prompt + 最近 N 轮 + 所有 tool_result，压缩中间轮次。
```

### 8.3 中文注释使用「直角引号」引用名词

```typescript
// ✅ 引用具体名词时用直角引号
// 将「消息历史」压缩为「摘要消息」，释放 context window 空间。

// 引用代码符号时用反引号（不用引号）
// 调用 `execa()` 而非 `child_process.exec()`，原因是前者支持 Promise。
```

### 8.4 推测与不确定性必须标注

```typescript
// [推测] 此处的 50ms 延迟可能是为了避免终端输出竞争，待核实。
await sleep(50);

// [待确认] 不清楚此处为何重置 retryCount 而非在 catch 块中重置。
retryCount = 0;

// [已知限制] Windows CRLF 行尾时行为未定义，非当前优先修复项。
```

---

## 九、完整文件示例

展示六种注释类型协同工作的效果：

```typescript
// This module handles context window management for the Codex agent.
// When the conversation history grows too large, it compresses older
// messages to stay within the model's token limit.

/**
 * 【文件职责】管理 Agent 对话的 context window，在超限前自动压缩消息历史。
 *
 * 【架构位置】
 *   层级：上下文管理层
 *   上游：agent/loop.ts（每轮发送前调用 ensureWithinLimit）
 *   下游：openai/tokenizer.ts（token 计数）
 *
 * 【数据流】Message[] → countTokens() → [超限时] compress() → 压缩后的 Message[]
 *
 * 【阅读建议】先看 ensureWithinLimit()，再看 compressHistory() 的压缩策略。
 */


// ═══════════════════════════════════════════════════════════════
// Constants  ·  常量
// ═══════════════════════════════════════════════════════════════

// Safety margin: reserve this many tokens for the model's response.
// 安全余量：为模型响应预留的 token 数。
// 设为 4K 是经验值：足够容纳大多数代码生成响应，又不过多压缩上下文。
// [引用范围] 被 ensureWithinLimit() 和 compressHistory() 共同依赖。
const RESPONSE_BUFFER_TOKENS = 4_096;

// Maximum context tokens for the current model (gpt-4o).
// 当前模型（gpt-4o）的最大 context window 大小。
// [引用范围 · 全局配置] 切换模型时此值必须同步更新，否则会静默截断消息。
const MAX_CONTEXT_TOKENS = 128_000;


// ═══════════════════════════════════════════════════════════════
// Public API  ·  对外接口
// ═══════════════════════════════════════════════════════════════

/**
 * Ensures the message history fits within the model's context window.
 * Compresses older messages if necessary, preserving the system prompt
 * and all tool results.
 *
 * @param messages - Full conversation history (mutated in place)
 * @returns        - Token count after any compression
 */
/**
 * 确保消息历史不超出 context window，必要时压缩旧消息。
 * 保留：system prompt + 最近 N 轮 + 所有 tool_result
 * 压缩：中间的 user/assistant 轮次替换为摘要
 *
 * @param messages - 完整消息历史（就地修改）
 * @returns        - 压缩后的 token 总数
 *
 * 副作用：修改 messages（替换中间轮次为摘要消息）
 */
export async function ensureWithinLimit(messages: Message[]): Promise<number> {
  // ── Step 1：统计当前 token 用量 ─────────────────────────────
  // 必须在发送前统计，而不是依赖上次缓存的计数：
  // 工具执行结果可能体积巨大（如 grep 的大量输出），导致计数失效。
  const currentTokens = await countTokens(messages);
  const limit = MAX_CONTEXT_TOKENS - RESPONSE_BUFFER_TOKENS;

  if (currentTokens <= limit) {
    return currentTokens;
  }

  // ── Step 2：触发压缩 ────────────────────────────────────────
  // 只有确认超限后才压缩，避免不必要的摘要 API 调用（有额外费用）。
  return compressHistory(messages, limit);
}


// ═══════════════════════════════════════════════════════════════
// Internal Helpers  ·  内部工具函数
// ═══════════════════════════════════════════════════════════════

// ───────────────────────────────────────────────────────────────
// Compression Logic  ·  压缩逻辑
// ───────────────────────────────────────────────────────────────

/**
 * Compresses the message history to fit within the token limit.
 * Always preserves: system prompt, last 3 turns, all tool results.
 */
/**
 * 将消息历史压缩到 token 限制以内。
 * 压缩策略（按优先级保留）：
 *   1. system prompt（必须保留，模型行为的根基）
 *   2. 最近 3 轮对话（保证模型有足够的近期上下文）
 *   3. 所有 tool_result（丢失工具结果会导致模型产生幻觉）
 *   4. 剩余空间：按时间顺序保留尽可能多的历史轮次
 *
 * [已知限制] 压缩摘要由模型生成，质量取决于当时的模型输出，不稳定。
 */
async function compressHistory(
  messages: Message[],
  tokenLimit: number
): Promise<number> { ... }

// Returns the token count for a message array using the tiktoken library.
// 使用 tiktoken 计算消息数组的 token 总数。
// [推测] 此处使用 cl100k_base 编码是因为 gpt-4o 系列均使用该分词器，
//         切换模型时需确认编码器是否匹配。
async function countTokens(messages: Message[]): Promise<number> { ... }
```

---

## 十、速查卡

```
铁律       → 原有英文注释一律保留，中文紧跟其后
对照翻译   → 意译 + 补充「为什么」，不逐字直译

文件注释   → 职责 / 架构位置 / 数据流 / 阅读建议（架构关键文件必写）
函数注释   → 契约描述：参数 / 返回 / 副作用 / 异常（复杂函数必写）
步骤注释   → 「为什么在这里」而非「做了什么」（函数 >20 行时写）
变量注释   → 语义 + 约束（魔法数字、非直觉初始值必写）
引用范围   → 作用域边界、引用方、修改影响（导出变量 / 全局状态必写）
区域划分   → 三级分隔线，文件 >150 行时写

取舍原则   → 去掉这条注释，读者会误解或浪费时间吗？答案是否就不写
排版       → 中英文之间加空格 / 行宽 ≤80 / 引号用「」/ 不确定标 [推测]
```
