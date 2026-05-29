# 命令执行与安全：系统架构

> 本文档回答：模型说要执行一条 shell 命令后，Codex 内部到底经历了哪些「关卡」才让它真正跑起来？
> 命令解析、执行策略（execpolicy）、安全评估、审批、沙箱包裹、进程执行、Guardian 自动审查、
> 提权、网络策略——这一整条「执行与安全」流水线的边界、判定依据与调用链。

本文聚焦 `codex-rs/core/` 的执行与安全层（exec / exec_policy / safety / guardian /
unified_exec / 提权 / 网络策略），以及横切的 `codex-rs/execpolicy/` crate。
沙箱的 OS 落地细节（Seatbelt / bubblewrap+seccomp / Windows 受限令牌）见
[doc13 沙箱机制](./13_sandbox_mechanism.md)；Thread/Session/Turn 生命周期与 `Op` 协议见
[doc11](../2_运行时核心/11_thread_session_turn_lifecycle.md)。

---

## 目录

1. [概述与设计哲学](#1-概述与设计哲学)
2. [命令解析与规范化](#2-命令解析与规范化)
3. [执行策略（execpolicy DSL）](#3-执行策略execpolicy-dsl)
4. [安全评估与审批流](#4-安全评估与审批流)
5. [沙箱变换与选型](#5-沙箱变换与选型)
6. [进程执行层（exec.rs）](#6-进程执行层execrs)
7. [Guardian 自动审查系统](#7-guardian-自动审查系统)
8. [提权（Shell-Escalation）](#8-提权shell-escalation)
9. [网络策略落地](#9-网络策略落地)
10. [集成点与调用链](#10-集成点与调用链)
11. [常见问题](#11-常见问题)

---

## 1. 概述与设计哲学

Codex 让模型自主跑命令，但每一条命令在「真正变成子进程」之前，要穿过几道**正交**的关卡。
理解这套系统的关键是分清两类机制——它们独立判定、任何一个都能让命令被拒：

| 关卡 | 问的问题 | 落地机制 | 关键文件 |
|------|---------|---------|---------|
| **execpolicy（执行策略）** | 这条命令该 `允许` / `提示` / `禁止`？ | 前缀规则 + 启发式兜底 | `core/src/exec_policy.rs`、`execpolicy/` crate |
| **安全评估（safety）** | apply_patch 是否限定在可写路径内？ | 路径约束 + 沙箱可用性 | `core/src/safety.rs` |
| **审批（approval）** | 该不该弹给用户/转给 Guardian？ | `AskForApproval` + 审查路由 | `exec_policy.rs`、`guardian/` |
| **沙箱（sandbox）** | 进程能碰哪些文件/网络？ | OS 原生隔离（见 doc13） | `sandboxing/` crate |

**两条核心设计取舍**（贯穿全文）：

1. **execpolicy 决定「该不该跑」，沙箱决定「跑起来能碰什么」——二者并行、互不替代。**
   即便 execpolicy 放行（`Allow`），沙箱仍会在文件系统/网络层兜底；反过来即便沙箱很宽松，
   execpolicy 也可能 `Forbidden`。

2. **`exec.rs` 这一层本身不做任何沙箱。** 沙箱包裹是在更上游的 `build_exec_request` 阶段、
   由 `SandboxManager::transform` 注入到 argv/env 里的。这让最底层的执行层保持**可移植**——
   它只管「拿一个完整 argv 去 spawn 并收输出」，不关心是 Seatbelt 还是 bubblewrap。
   见 `core/src/exec.rs:16-17` 的文件头自述。

整条流水线的「形状」（`core/src/exec.rs:1-21` 文件头）：

```
ShellRuntime 拼好 ExecParams
        │
        ▼
process_exec_tool_call  ← 对外主入口（exec.rs:336）
        │
        ▼
build_exec_request      ← SandboxManager::transform 注入沙箱包裹（exec.rs:363）
        │   产出 ExecRequest
        ▼
crate::sandboxing::execute_env   ← 统一执行路径
        │
        ▼
execute_exec_request → get_raw_output_result → exec()  ← 真正 spawn
        │
        ▼
consume_output          ← 并发读 stdout/stderr、按超时/取消杀进程组（exec.rs:1371）
```

---

## 2. 命令解析与规范化

模型给的命令往往不是裸命令，而是 `bash -lc "cd foo && npm install"` 这类**外壳包裹**。
要对它做策略判定和审批缓存，先得把里面真正要跑的东西「掏出来」。这件事有两套解析器，
服务于两个不同目的：

### 2.1 审批缓存规范化：`canonicalize_command_for_approval`

定义在 `core/src/command_canonicalization.rs:14-38`。它的目的是让**审批决定跨包装路径稳定**——
`/bin/bash -lc foo` 和 `bash -lc foo` 应当命中同一条缓存，否则用户批准一次后换个 shell 路径
又得重批。策略是分三档：

```rust
// command_canonicalization.rs:14
pub(crate) fn canonicalize_command_for_approval(command: &[String]) -> Vec<String> {
    // ① 简单命令（bash -lc 里就一条裸命令）→ 直接返回内层 tokenized 命令
    if let Some(commands) = parse_shell_lc_plain_commands(command)
        && let [single_command] = commands.as_slice() { return single_command.clone(); }
    // ② 复杂 bash 脚本 → 保留脚本原文（前缀 __codex_shell_script__ + 模式 + 脚本文本）
    if let Some((_shell, script)) = extract_bash_command(command) { ... }
    // ③ PowerShell 脚本 → 同理（前缀 __codex_powershell_script__）
    if let Some((_shell, script)) = extract_powershell_command(command) { ... }
    command.to_vec()  // ④ 兜底：原样返回
}
```

**关键取舍**：简单命令被 tokenize（便于规则匹配），但**复杂脚本逐字保留原文**。因为复杂脚本
无法安全地还原成一串确定的命令序列——若强行拆解再据此做 amendment（自动放宽规则），可能
放行出比用户想批准的更宽的范围。审批 amendment 只会针对这个 canonical 形式，防止绕过。

### 2.2 策略判定解析：`commands_for_exec_policy`

定义在 `core/src/exec_policy.rs:772-810`。它供 execpolicy 判定使用，按优先级尝试三种解析：

1. `parse_shell_lc_plain_commands`：解析 `bash -lc` 里的多条裸命令（`A && B && C`），
   `used_complex_parsing = false`，`command_origin = Generic`。
2. （仅 Windows）`parse_powershell_command_into_plain_commands`：PowerShell 解析，
   `command_origin = PowerShell`。
3. `parse_shell_lc_single_command_prefix`：heredoc/复杂脚本的**单前缀兜底**——只取一个命令前缀，
   并把 `used_complex_parsing = true`。

`used_complex_parsing` 这个 flag 很关键：**只有非复杂解析时才允许自动 amendment**
（`exec_policy.rs:294` 的 `auto_amendment_allowed = !used_complex_parsing`）。理由同 §2.1——
复杂解析下的命令拆得不可靠，自动放宽规则会过度放权。

返回结构 `ExecPolicyCommands { commands, used_complex_parsing, command_origin }`。

---

## 3. 执行策略（execpolicy DSL）

execpolicy 是一个独立 crate（`codex-rs/execpolicy/`），负责回答「这条命令的裁决是什么」。
它的产物是一个三值枚举 `Decision`。

### 3.1 核心裁决：`Decision`

`execpolicy/src/decision.rs:7-16`，serde `camelCase` 序列化：

```rust
pub enum Decision {
    Allow,      // 无需进一步审批即可运行
    Prompt,     // 需用户显式批准；approval_policy="never" 时直接拒
    Forbidden,  // 直接拦截，不再考虑
}
```

### 3.2 前缀规则：`PrefixPattern` / `PatternToken`

execpolicy 的规则核心是**前缀匹配**——按命令的第一个 token 建索引，再逐位匹配后续 token。

`PatternToken`（`execpolicy/src/rule.rs:16-19`）描述单个 token 怎么匹配：

```rust
pub enum PatternToken {
    Single(String),       // 固定字符串，必须精确相等
    Alts(Vec<String>),    // 多个候选之一即可
}
```

`PrefixPattern`（`rule.rs:39-43`）：`first: Arc<str>`（首 token 固定，因为按它建索引）+
`rest: Arc<[PatternToken]>`（后续模式 token）。`matches_prefix`（`rule.rs:45-60`）要求命令长度
≥ 模式长度，且逐位命中，返回被匹配走的前缀切片。

### 3.3 匹配结果：`RuleMatch`

`rule.rs:62-82` 区分两类来源——这个区分对「是否允许自动 amendment」至关重要：

```rust
pub enum RuleMatch {
    PrefixRuleMatch {        // 显式策略规则命中（用户/默认配置里写的规则）
        matched_prefix: Vec<String>,
        decision: Decision,
        resolved_program: Option<AbsolutePathBuf>,
        justification: Option<String>,
    },
    HeuristicsRuleMatch {    // 没有显式规则、由启发式兜底产生的裁决
        command: Vec<String>,
        decision: Decision,
    },
}
```

`core/src/exec_policy.rs` 里用 `is_policy_match(rule_match)`（判定是否 `PrefixRuleMatch`）来区分二者：
显式策略规则优先级最高，启发式只在没有显式规则时兜底；amendment 也只针对启发式裁决推导
（见 `try_derive_execpolicy_amendment_for_prompt_rules`，`exec_policy.rs:822`）。

### 3.4 判定入口：`check_multiple_with_options`

`core/src/exec_policy.rs` 里通过 `exec_policy.check_multiple_with_options(commands, &fallback, &options)`
（`exec_policy.rs:312`）对解析出的多条命令逐条判定，未命中显式规则时调用 `fallback`
（即下一节的启发式兜底）。`MatchOptions { resolve_host_executables: true }`（`exec_policy.rs:309`）
表示会解析命令到宿主机实际可执行文件路径再匹配。

---

## 4. 安全评估与审批流

execpolicy 给出 `Decision` 之后，要把它转成「对用户/系统意味着什么」。这里有两条线：
shell 命令走 `ExecPolicyManager`，apply_patch 走 `safety.rs`。

### 4.1 Shell 命令的审批判定：`create_exec_approval_requirement_for_command`

`core/src/exec_policy.rs:272-379`。入参是 `ExecApprovalRequest`（`exec_policy.rs:241-249`）：

```rust
pub(crate) struct ExecApprovalRequest<'a> {
    pub(crate) command: &'a [String],
    pub(crate) approval_policy: AskForApproval,
    pub(crate) permission_profile: PermissionProfile,
    pub(crate) file_system_sandbox_policy: &'a FileSystemSandboxPolicy,
    pub(crate) sandbox_cwd: &'a Path,
    pub(crate) sandbox_permissions: SandboxPermissions,
    pub(crate) prefix_rule: Option<Vec<String>>,
}
```

> [纠偏] 大纲把字段简记为 `cwd`，实际字段名是 `sandbox_cwd`；并无单独的 `cwd` 字段。

判定逻辑（`exec_policy.rs:272-379`）：
1. `commands_for_exec_policy(command)` 解析（§2.2）。
2. `check_multiple_with_options` 拿到 `evaluation.decision`（§3.4）。
3. 按 `Decision` 映射成**返回类型 `ExecApprovalRequirement`**（注意不是直接返回 `Decision`）：

| execpolicy `Decision` | → `ExecApprovalRequirement` | 含义 |
|----|----|----|
| `Forbidden` | `Forbidden { reason }` | 拒绝，附理由 |
| `Prompt` | `NeedsApproval { reason, proposed_execpolicy_amendment }` | 需审批；但若 `prompt_is_rejected_by_policy`（如 `approval_policy=Never`）则转 `Forbidden` |
| `Allow` | `Skip { bypass_sandbox, proposed_execpolicy_amendment }` | 跳过审批；仅当每段命令都被显式 `Allow` 时 `bypass_sandbox=true` |

> [纠偏] 大纲称该函数「返回 Allow/Prompt/Forbidden」，准确说是：它把 execpolicy 的
> `Decision` 三值**翻译**成审批语义三态 `Skip / NeedsApproval / Forbidden`（`ExecApprovalRequirement`）。

注意 `Allow` 分支里 `bypass_sandbox` 的谨慎：只有**所有**命令段都被显式策略规则 `Allow`
才绕过沙箱（`exec_policy.rs:360-371`），否则照样上沙箱——又一处「execpolicy 与沙箱正交」的体现。

### 4.2 未匹配命令的启发式兜底：`render_decision_for_unmatched_command`

`core/src/exec_policy.rs:632-750`。当没有显式规则命中时，这个函数**不是简单拒绝**，而是叠加
多层判断后给出 `Decision`。判定顺序：

```
1. is_known_safe = 命令在「已知安全」名单里？（Generic 走 is_known_safe_command；
                    Windows PowerShell 走 is_safe_powershell_words）
2. 若 is_known_safe && 非复杂解析 && (UnlessTrusted 或 环境无沙箱保护) → Allow
3. command_is_dangerous = 命令疑似危险？（command_might_be_dangerous / is_dangerous_powershell_words）
4. 若 危险 || 环境无沙箱保护：
     - Never:   沙箱显式禁用 → Allow；否则 → Forbidden
     - 其余策略 → Prompt
5. 既不危险、环境有沙箱保护：按 approval_policy + 沙箱种类 + sandbox_permissions 细分：
     - Never / OnFailure → Allow（靠沙箱兜底）
     - UnlessTrusted     → Prompt（安全名单已查过返回 false）
     - OnRequest:  Unrestricted/External → Allow；Restricted 下若请求提权 → Prompt 否则 Allow
     - Granular:   同 OnRequest
```

**设计要点**：这是分层启发式，不是一刀切。核心理念是「在受限沙箱里，非危险、非提权命令
**不打扰用户**——让沙箱默默兜底就好」（`exec_policy.rs:723-731` 注释）；只有危险命令或无沙箱
保护的环境才升级到 `Prompt`/`Forbidden`。Windows 的 ReadOnly 沙箱被特判为「无真正沙箱保护」
（`environment_lacks_sandbox_protections`，`exec_policy.rs:655-660`），因为它不是真沙箱。

### 4.3 apply_patch 的安全评估：`assess_patch_safety`

`core/src/safety.rs:33-116`，返回 `SafetyCheck`（`safety.rs:21-31`）：

```rust
pub enum SafetyCheck {
    AutoApprove { sandbox_type: SandboxType, user_explicitly_approved: bool },
    AskUser,
    Reject { reason: String },
}
```

> [纠偏] 大纲把变体记为「AutoApprove / AskUser / Reject」（无字段），实际 `AutoApprove` 带
> `sandbox_type` + `user_explicitly_approved`，`Reject` 带 `reason`。

判定要点（`safety.rs:33-116`）：
- 空 patch → `Reject`。
- `UnlessTrusted` → 直接 `AskUser`。
- 若 patch **限定在可写路径内**（`is_write_patch_constrained_to_writable_paths`）或策略是 `OnFailure`：
  - `Disabled`/`External` profile：不上 Codex 沙箱，`AutoApprove { sandbox_type: None }`。
  - 否则查平台沙箱可用性（`get_platform_sandbox`）：能上沙箱 → `AutoApprove`；不能上沙箱时，
    若策略拒绝沙箱审批（`Never` 或 `Granular` 关了 `sandbox_approval`）→ `Reject`，否则 `AskUser`。
- patch 越界写 + 拒绝沙箱审批 → `Reject`；否则 `AskUser`。

注释里特别提到：即便 patch 看似限定在可写路径，**仍可能通过硬链接指向可写根之外**
（`safety.rs:67-69`），所以这种情况照样要在沙箱里跑 `apply_patch`——又一处「不信任表象、靠沙箱兜底」。

---

## 5. 沙箱变换与选型

这一节是「执行层」和「沙箱层」的接缝。沙箱的 OS 落地见 [doc13](./13_sandbox_mechanism.md)，
这里只讲**它如何被注入到一条 exec 请求里**。

`build_exec_request`（`core/src/exec.rs:363-465`）把可移植的 `ExecParams` 变换成带沙箱包裹的
`ExecRequest`，步骤：

1. `permission_profile.to_runtime_permissions()` → 推出 `FileSystemSandboxPolicy` +
   `NetworkSandboxPolicy`（`exec.rs:388`）。
2. `select_process_exec_tool_sandbox_type` → `SandboxManager::new().select_initial(...)` 选出
   `SandboxType`（`exec.rs:168-181` + `390`）。
3. 若启用网络代理，`network.apply_to_env(&mut env)` 注入代理变量（`exec.rs:398-400`）。
4. `manager.transform(SandboxTransformRequest { ... })` → 注入沙箱包裹参数，得到 `ExecRequest`
   （`exec.rs:420-438`）。
5. 据 Windows 后端（提权 / 受限令牌）解析对应文件系统覆写（`exec.rs:439-463`）。

**刻意忽略的字段**：`build_exec_request` 解构 `ExecParams` 时，把 `arg0`、`justification`、
`sandbox_permissions` 标了 `_` 忽略（`exec.rs:380-384`）——因为它们是**审批阶段**的字段，到了
「构造要 spawn 的请求」这步已无关。这也佐证了执行层与审批层的解耦。

---

## 6. 进程执行层（exec.rs）

### 6.1 可移植描述：`ExecParams`

`core/src/exec.rs:105-125`。这是「要跑什么」的跨平台、与沙箱后端无关的描述：

```rust
pub struct ExecParams {
    pub command: Vec<String>,        // 完整 argv；若需沙箱，包裹参数须已拼在这里
    pub cwd: AbsolutePathBuf,
    pub expiration: ExecExpiration,  // 何时强制结束
    pub capture_policy: ExecCapturePolicy,  // 输出是否封顶 + 是否超时
    pub env: HashMap<String, String>,
    pub network: Option<NetworkProxy>,
    pub sandbox_permissions: SandboxPermissions,
    pub windows_sandbox_level: WindowsSandboxLevel,
    pub windows_sandbox_private_desktop: bool,
    pub justification: Option<String>,  // 提权审批给用户看的理由；执行阶段忽略
    pub arg0: Option<String>,           // 覆写 argv[0]
}
```

### 6.2 捕获策略：`ExecCapturePolicy`

`core/src/exec.rs:158-166`，控制「输出封顶」与「是否超时」两个开关：

| 变体 | 输出上限 | 是否超时 | 用途 |
|------|---------|---------|------|
| `ShellTool`（默认） | `EXEC_OUTPUT_MAX_BYTES`（= `DEFAULT_OUTPUT_BYTES_CAP`，`exec.rs:90`） | 是 | 模型驱动的 shell 命令 |
| `FullBuffer` | 无上限（`exec.rs:306` 返回 `None`） | 否（`exec.rs:317` 返回 `false`） | 受信内部 helper 跑确定性命令 |

设计动机（`exec.rs:155-157`）：模型生成的命令既怕**狂吐 stdout 撑爆内存**、又怕**卡死**，
所以两道闸都开；受信 helper 要拿全量输出、且不该被超时打断，故都关。两道闸的实现分别是
`retained_bytes_cap`（`exec.rs:303-308`）和 `uses_expiration`（`exec.rs:314-319`）。

### 6.3 终止机制：`ExecExpiration`

`core/src/exec.rs:188-196`，描述命令在自然结束前被强制终止的几种方式：

```rust
pub enum ExecExpiration {
    Timeout(Duration),         // 纯超时
    DefaultTimeout,            // 默认超时（DEFAULT_EXEC_COMMAND_TIMEOUT_MS）
    Cancellation(CancellationToken),  // 仅等取消令牌
    TimeoutOrCancellation { timeout, cancellation },  // 二者取先到（biased 优先判取消）
}
```

### 6.4 主入口与执行链

- `process_exec_tool_call`（`exec.rs:336-354`）：对外主入口，两步——`build_exec_request` 变换
  + `crate::sandboxing::execute_env` 执行。把变换与执行拆开，是为了让**所有 exec（不论来自哪个
  工具）都汇聚到同一条沙箱执行链**（`exec.rs:332-334` 注释）。
- `execute_exec_request`（`exec.rs:471-523`）：把 `ExecRequest` 拆回参数，计时跑
  `get_raw_output_result`（`exec.rs:526`；Windows 受限令牌沙箱走专门分支，其余 Unix 走 `exec()`），
  再 `finalize_exec_result`（`exec.rs:764`）归一成 `ExecToolCallOutput`。

### 6.5 输出收集与超时杀进程组：`consume_output`

`core/src/exec.rs:1371-1508`。流程：spawn 两个 `read_output` 任务并发读 stdout/stderr，然后
`tokio::select!` 在「子进程退出 / 到期 / Ctrl-C」之间竞速（`exec.rs:1414-1419`）。

**关键设计：超时/取消时杀的是「进程组」而非单进程**（`exec.rs:1422` 的 `kill_child_process_group`，
该函数从 `codex_utils_pty::process_group` 导入，`exec.rs:70`）。原因（`exec.rs:18-20` + `100-103` 注释）：
子进程可能 fork 出**孙进程**并继承 stdout/stderr 管道 fd；只杀直接子进程的话，孙进程仍持有管道
写端，`read()` 永远等不到 EOF，读管道任务会**永久阻塞，整个 agent 卡死**。所以必须连同孙进程
一起杀掉。取消（`Cancelled`）分支更礼貌：先 `terminate_process_group` 发 TERM 给一段宽限期做
cleanup，超时未退再 KILL（`exec.rs:1429-1455`）。

### 6.6 沙箱拒绝识别：`is_likely_sandbox_denied`

`core/src/exec.rs:829-884`。命令失败后，需要判断「是不是被沙箱挡了」（以便决定是否申请提权重试）。
启发式：

1. `SandboxType::None` 或 `exit_code == 0` → 直接 `false`。
2. 输出（stderr/stdout/aggregated）含关键词 → `true`。关键词 7 个（`exec.rs:841`）：
   `operation not permitted` / `permission denied` / `read-only file system` / `seccomp` /
   `sandbox` / `landlock` / `failed to write file`。
3. 命中「确定非沙箱」的退出码（2/126/127，分别是 shell 误用/权限拒绝/命令未找到）→ `false`。
4. （Unix）`LinuxSeccomp` 且退出码 == `SIGSYS` 信号码 → `true`（seccomp 拦截系统调用的典型信号）。

---

## 7. Guardian 自动审查系统

当审批判定为「需要审批」时，Codex 可以不弹给用户，而是**派出一个 Guardian 审查员会话**自动评估
风险与授权——这是 `auto-review` 模式。Guardian 在 `core/src/guardian/`。

### 7.1 路由：何时转给 Guardian？

`routes_approval_to_guardian`（`guardian/review.rs:147-152`）：

```rust
pub(crate) fn routes_approval_to_guardian(turn: &TurnContext) -> bool {
    matches!(turn.approval_policy.value(), AskForApproval::OnRequest | AskForApproval::Granular(_))
        && turn.config.approvals_reviewer == ApprovalsReviewer::AutoReview
}
```

即：审批策略是 `OnRequest`/`Granular`，**且**配置的审查员是 `AutoReview` 时，才转 Guardian。

### 7.2 审查执行：`run_guardian_review`

`guardian/review.rs:246-326`（及后续）。它 spawn 一个 Guardian 审查员 session（reviewer 名为
`"guardian"`，`guardian/mod.rs:47`），让其评估并产出结构化的 `GuardianAssessment`
（`guardian/mod.rs:62-68`，含 `risk_level` / `user_authorization` / `outcome` / `rationale`）。
过程中向上层发 `GuardianAssessment` 事件（InProgress → 终态）。

**核心安全约束：永远 fail closed**（`guardian/review.rs:243-245` 文档注释）——超时、审查会话失败、
解析失败**全部阻断执行**，但超时会被区分出来单独上报（与显式拒绝不同）。超时阈值
`GUARDIAN_REVIEW_TIMEOUT = 90 秒`（`guardian/mod.rs:46`）。

### 7.3 熔断器：`GuardianRejectionCircuitBreaker`

防止模型陷入「反复被拒 → 反复重试」的死循环。`guardian/mod.rs:48-50` 定义阈值：

| 常量 | 值 | 含义 |
|------|----|----|
| `MAX_CONSECUTIVE_GUARDIAN_DENIALS_PER_TURN` | 3 | 单 turn 内连续拒绝上限 |
| `MAX_RECENT_AUTO_REVIEW_DENIALS_PER_TURN` | 10 | 单 turn 内近期拒绝（滑动窗口）上限 |
| `AUTO_REVIEW_DENIAL_WINDOW_SIZE` | 50 | 滑动窗口大小 |

熔断器按 turn 维护 `consecutive_denials`（连续拒）+ `recent_denials`（滑动窗口 `VecDeque<bool>`），
动作枚举 `GuardianRejectionCircuitBreakerAction`（`guardian/mod.rs:88-95`）为 `Continue` 或
`InterruptTurn { consecutive_denials, recent_denials }`。当**连续拒 ≥3 或近期拒 ≥10** 时触发
`InterruptTurn`，中断整个 turn。

---

## 8. 提权（Shell-Escalation）

普通沙箱执行被拒后，Codex 在 Unix 上有一条**内核级 execve 拦截**的提权路径（zsh fork），
让命令在用户授权后以更高权限重试，且能在子进程每次 `execve` 时逐条判定。

### 8.1 入口与可用性门槛：`try_run_zsh_fork`

`core/src/tools/runtimes/shell/unix_escalation.rs:100-231`。这是**可选 feature**，三道门槛缺一即
回退到普通 exec（返回 `Ok(None)`）：

1. `ctx.session.services.shell_zsh_path` 已配置（`unix_escalation.rs:106`）。
2. `Feature::ShellZshFork` 已启用（`unix_escalation.rs:110`）。
3. 用户 shell 是 Zsh（`unix_escalation.rs:114`）。
4. 此外还需 `main_execve_wrapper_exe` 二进制（`unix_escalation.rs:174-183`，缺失则 `Rejected`）。

### 8.2 工作机制

`try_run_zsh_fork` 构造一个 `CoreShellActionProvider`（`unix_escalation.rs:203-217`）作为
**提权策略提供者**，封装了 execpolicy、审批策略、permission_profile、沙箱策略等；它实现了
`EscalationPolicy` trait（`unix_escalation.rs:582`）。然后交给 `EscalateServer`
（`unix_escalation.rs:219-223`，来自 `codex_shell_escalation` crate）执行：

```
ShellRequest
   │
   ▼
try_run_zsh_fork
   │  构造 CoreShellActionProvider（提权策略）+ CoreShellCommandExecutor
   ▼
EscalateServer::new(zsh 路径, execve wrapper, 策略).exec(...)
   │  zsh 子进程每次 execve 被内核级 wrapper 拦截
   ▼
策略逐条判定该 execve 该不该放行 → 决策落地
```

相比「整条命令跑前判一次」，zsh fork 能在脚本执行过程中**对每个 exec 系统调用单独判定**，
粒度更细。取消令牌处也做了级联：若 `attempt.network_denial_cancellation_token` 存在，会用
`cancel_when_either` 把它叠进 stopwatch 的取消令牌（`unix_escalation.rs:196-198`）——网络被拒
能直接掐断正在跑的命令（见 §9）。

---

## 9. 网络策略落地

网络访问由独立的网络代理（`codex_network_proxy`）拦截，被挡的请求可以走审批流，
审批结果再回写成 execpolicy 网络规则 amendment。核心在 `core/src/network_policy_decision.rs`。

### 9.1 从被挡请求提取审批上下文：`network_approval_context_from_payload`

`network_policy_decision.rs:26-44`：

```rust
pub(crate) fn network_approval_context_from_payload(
    payload: &NetworkPolicyDecisionPayload,
) -> Option<NetworkApprovalContext> {
    if !payload.is_ask_from_decider() { return None; }   // 只处理 "ask" 决策
    let protocol = payload.protocol?;
    let host = payload.host.as_deref()?.trim();
    if host.is_empty() { return None; }
    Some(NetworkApprovalContext { host: host.to_string(), protocol })
}
```

即从被代理拦截的请求里提取 `host` + `protocol`，构造审批上下文（仅当决策是 `ask` 时）。

### 9.2 amendment 类型与流程

审批通过后，网络规则会以 `ExecPolicyNetworkRuleAmendment`（`network_policy_decision.rs:11-16`，
含 `protocol` / `decision` / `justification`）形式回写，供后续请求直接放行/拒绝。整体流程：

```
被代理拦截的请求（blocked）
   │
   ▼
network_approval_context_from_payload  ← 提取 host+protocol
   │
   ▼
弹审批提示
   │  用户决定
   ▼
回写 execpolicy 网络规则 amendment  → 后续同类请求据此放行/拒绝
```

### 9.3 网络拒绝的级联取消

当请求被代理判 `Deny`（`denied_network_policy_message`，`network_policy_decision.rs:46`），它可以
设置一个取消令牌，**级联到正在执行的命令把它掐掉**。这就是 §8.2 提到的
`network_denial_cancellation_token`——网络被拒不仅返回错误信息，还能主动终止那条正在跑的命令，
避免命令在网络被封后继续空转。

---

## 10. 集成点与调用链

把前九节串起来，从「模型说要跑命令」到「子进程真正跑完」的全景：

### 10.1 主执行链（shell 命令）

```
模型工具调用
   │
   ▼
ShellRuntime（tools/runtimes/shell.rs）拼 ExecParams（含 shell 包裹）
   │
   ▼
process_exec_tool_call（exec.rs:336）
   │
   ▼
build_exec_request（exec.rs:363）→ SandboxManager::transform 注入沙箱 → ExecRequest
   │
   ▼
crate::sandboxing::execute_env
   │
   ▼
execute_exec_request（exec.rs:471）→ get_raw_output_result（exec.rs:526）
   │
   ▼
exec() spawn → consume_output（exec.rs:1371，超时杀进程组）
   │
   ▼
ExecToolCallOutput（含 is_likely_sandbox_denied 判定，exec.rs:829）
```

### 10.2 审批判定链（shell 命令）

```
ExecApprovalRequest（exec_policy.rs:241）
   │
   ▼
create_exec_approval_requirement_for_command（exec_policy.rs:272）
   │
   ▼
commands_for_exec_policy 解析（exec_policy.rs:772） →
check_multiple_with_options（exec_policy.rs:312）
   │
   ├── 命中显式 PrefixRule → 用其 Decision
   └── 未命中 → render_decision_for_unmatched_command（exec_policy.rs:632，启发式兜底）
   │
   ▼
Decision（Allow/Prompt/Forbidden）
   │
   ▼
翻译成 ExecApprovalRequirement（Skip / NeedsApproval / Forbidden）
```

### 10.3 Guardian 审查链（当 Prompt 且 auto-review）

```
NeedsApproval
   │
   ▼
routes_approval_to_guardian（review.rs:147）== true？
   │ 是
   ▼
run_guardian_review（review.rs:246）→ spawn Guardian 审查 session
   │  评估 risk_level / user_authorization（fail closed，90s 超时）
   ▼
GuardianRejectionCircuitBreaker 计数（mod.rs:48-50）
   │  连续 ≥3 或近期 ≥10 → InterruptTurn
   ▼
ReviewDecision → 决定命令是否继续执行
```

### 10.4 提权与网络级联

```
普通沙箱被拒（is_likely_sandbox_denied 判定）
   │
   ▼
[Unix zsh fork 可用] try_run_zsh_fork（unix_escalation.rs:100）
   │  CoreShellActionProvider（提权策略）→ EscalateServer → execve 内核级拦截逐条判定
   │
   └── network_denial_cancellation_token 级联：网络被 Deny → 掐断命令执行
```

### 10.5 关键代码位置速查

| 关注点 | 文件 | 关键行 |
|--------|------|--------|
| exec 流水线文件头 | `core/src/exec.rs` | 1-21 |
| `ExecParams` | `core/src/exec.rs` | 105-125 |
| `ExecCapturePolicy` | `core/src/exec.rs` | 158-166 |
| `ExecExpiration` | `core/src/exec.rs` | 188-196 |
| `process_exec_tool_call` | `core/src/exec.rs` | 336-354 |
| `build_exec_request` | `core/src/exec.rs` | 363-465 |
| `execute_exec_request` | `core/src/exec.rs` | 471-523 |
| `is_likely_sandbox_denied` | `core/src/exec.rs` | 829-884 |
| `consume_output` | `core/src/exec.rs` | 1371-1508 |
| `canonicalize_command_for_approval` | `core/src/command_canonicalization.rs` | 14-38 |
| `ExecApprovalRequest` | `core/src/exec_policy.rs` | 241-249 |
| `create_exec_approval_requirement_for_command` | `core/src/exec_policy.rs` | 272-379 |
| `render_decision_for_unmatched_command` | `core/src/exec_policy.rs` | 632-750 |
| `commands_for_exec_policy` | `core/src/exec_policy.rs` | 772-810 |
| `SafetyCheck` / `assess_patch_safety` | `core/src/safety.rs` | 21-31 / 33-116 |
| `Decision` | `execpolicy/src/decision.rs` | 7-16 |
| `PatternToken` / `PrefixPattern` / `RuleMatch` | `execpolicy/src/rule.rs` | 16-19 / 39-43 / 62-82 |
| Guardian 常量 | `core/src/guardian/mod.rs` | 46-50 |
| `run_guardian_review` | `core/src/guardian/review.rs` | 246+ |
| `routes_approval_to_guardian` | `core/src/guardian/review.rs` | 147-152 |
| Unified Exec 文件头 | `core/src/unified_exec/mod.rs` | 1-23 |
| `try_run_zsh_fork` | `core/src/tools/runtimes/shell/unix_escalation.rs` | 100-231 |
| `network_approval_context_from_payload` | `core/src/network_policy_decision.rs` | 26-44 |

---

## 11. 常见问题

**Q1：execpolicy 放行了，为什么命令还是受沙箱限制？**
因为二者正交。execpolicy 决定「该不该跑」（`Allow/Prompt/Forbidden`），沙箱决定「跑起来能碰什么」。
只有当**每段命令都被显式策略规则 `Allow`** 时，`bypass_sandbox` 才为 `true`（`exec_policy.rs:360-371`）；
启发式 `Allow` 或部分命令被允许时，照样上沙箱兜底。任何一层都能导致拒绝。

**Q2：为什么 `exec.rs` 不直接做沙箱？**
为了**可移植**。`exec.rs` 只管「拿一个完整 argv 去 spawn 并收输出」，沙箱包裹（Seatbelt 前缀 /
bubblewrap 参数 / Windows 后端）是在更上游的 `build_exec_request` 由 `SandboxManager::transform`
注入到 argv/env 里的（`exec.rs:16-17`、`363-465`）。这样底层执行层不必知道任何 OS 沙箱细节。

**Q3：超时为什么要杀整个进程组，不能只杀那条命令吗？**
不能。子进程可能 fork 出孙进程并继承 stdout/stderr 管道 fd；只杀直接子进程，孙进程仍持有管道
写端，读管道任务 `read()` 永远等不到 EOF，**整个 agent 会卡死**（`exec.rs:18-20`、`100-103`）。所以
`consume_output` 在超时时调 `kill_child_process_group`（`exec.rs:1422`）连孙进程一起杀。

**Q4：未匹配的命令是不是直接拒绝？**
不是。`render_decision_for_unmatched_command`（`exec_policy.rs:632-750`）是**分层启发式**：先查
「已知安全名单」，再查「是否危险」，再结合 `approval_policy` + 沙箱种类 + `sandbox_permissions`
细分。核心理念是「受限沙箱里非危险、非提权命令不打扰用户，让沙箱默默兜底」。

**Q5：Guardian 审查超时了会放行命令吗？**
不会。Guardian **永远 fail closed**（`review.rs:243-245`）——超时（90s）、审查会话失败、解析失败
全部阻断执行；只是超时会被区分出来单独上报，区别于显式拒绝。

**Q6：为什么复杂脚本不做自动 amendment？**
因为复杂脚本无法安全地还原成确定的命令序列。`commands_for_exec_policy` 用单前缀兜底解析时会置
`used_complex_parsing = true`，进而 `auto_amendment_allowed = !used_complex_parsing` 为 `false`
（`exec_policy.rs:294`）——避免据不可靠的拆解推导出过度放权的规则。审批缓存同理：
`canonicalize_command_for_approval` 对复杂脚本**逐字保留原文**（`command_canonicalization.rs:21-35`）。

**Q7：zsh fork 提权随时可用吗？**
不是，它是 Unix opt-in feature，需同时满足：配置了 `shell_zsh_path`、启用 `Feature::ShellZshFork`、
用户 shell 是 Zsh、且有 `main_execve_wrapper_exe` 二进制（`unix_escalation.rs:106-114`、`174-183`）。
任一缺失即回退到普通 exec。它的价值是能在 `execve` 系统调用级别**逐条**判定，而非整条命令判一次。

**Q8：网络被封会怎样影响正在跑的命令？**
网络代理判 `Deny` 时可设置取消令牌，通过 `network_denial_cancellation_token` 级联到命令执行的取消
令牌（`unix_escalation.rs:196-198`），**主动掐断**正在跑的命令，避免其在网络被封后空转。
