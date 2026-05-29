# Apply-Patch 与 V4A 文件编辑机制：补丁解析、应用与容错

> Codex 让模型修改文件不是靠「整文件重写」，而是靠一种自定义补丁格式（俗称 V4A）：模型产出 `*** Begin Patch … *** End Patch` 文本，Codex 把它解析成结构化变更、用「模糊匹配」在真实文件里定位旧行、倒序套用替换、再落盘。本文从语法、解析、验证、定位、落盘、工具集成到跨回合追踪逐层拆解，并解释每一处「为什么这么设计」。

涉及代码主要在两处：独立的 `codex-rs/apply-patch/` crate（纯解析/验证/应用，不依赖 core），以及 `codex-rs/core/` 里把它接到工具系统、权限、沙箱、UI 事件的胶水层。

## 目录

1. [补丁格式与语法规范](#1-补丁格式与语法规范)
2. [解析层：文本到结构化-hunks](#2-解析层文本到结构化-hunks)
3. [验证层：补丁校验与权限计算](#3-验证层补丁校验与权限计算)
4. [应用层：模糊匹配与行定位](#4-应用层模糊匹配与行定位)
5. [落盘层：文件系统操作与容错](#5-落盘层文件系统操作与容错)
6. [工具集成：与-llm-的反馈环](#6-工具集成与-llm-的反馈环)
7. [跨回合追踪：TurnDiffTracker-机制](#7-跨回合追踪turndifftracker-机制)
8. [常见问题](#8-常见问题)

---

## 1. 补丁格式与语法规范

### 1.1 为什么不用标准 unified diff？

标准 `diff -u` 依赖精确的行号（`@@ -10,7 +10,6 @@`）。LLM 数行号极易出错，一旦行号偏一行整个补丁就废了。Codex 的 V4A 格式刻意**去行号化**：上下文用「内容」而非「坐标」描述，定位交给模糊匹配算法（见 §4）。这是整套机制的设计原点——把「模型不擅长的精确计数」换成「模型擅长的内容复述」。

### 1.2 Lark 语法

权威语法以 Lark 文法写在解析器文件头部，`apply-patch/src/parser.rs:4-22`：

```
start: begin_patch environment_id? hunk+ end_patch
begin_patch: "*** Begin Patch" LF
environment_id: "*** Environment ID: " filename LF
end_patch: "*** End Patch" LF?

hunk: add_hunk | delete_hunk | update_hunk
add_hunk: "*** Add File: " filename LF add_line+
delete_hunk: "*** Delete File: " filename LF
update_hunk: "*** Update File: " filename LF change_move? change?
add_line: "+" /(.+)/ LF -> line

change_move: "*** Move to: " filename LF
change: (change_context | change_line)+ eof_line?
change_context: ("@@" | "@@ " /(.+)/) LF
change_line: ("+" | "-" | " ") /(.+)/ LF
eof_line: "*** End of File" LF
```

各标记常量集中定义在 `apply-patch/src/parser.rs:35-44`：

| 常量 | 字面量 | 作用 |
|------|--------|------|
| `BEGIN_PATCH_MARKER` | `*** Begin Patch` | 补丁起始 |
| `END_PATCH_MARKER` | `*** End Patch` | 补丁结束 |
| `ENVIRONMENT_ID_MARKER` | `*** Environment ID: ` | 可选：多环境时指定目标环境 |
| `ADD_FILE_MARKER` | `*** Add File: ` | 新增文件 hunk |
| `DELETE_FILE_MARKER` | `*** Delete File: ` | 删除文件 hunk |
| `UPDATE_FILE_MARKER` | `*** Update File: ` | 更新文件 hunk |
| `MOVE_TO_MARKER` | `*** Move to: ` | 更新 + 重命名 |
| `EOF_MARKER` | `*** End of File` | 标记 chunk 贴着文件末尾 |
| `CHANGE_CONTEXT_MARKER` | `@@ ` | 带注释的上下文锚点 |
| `EMPTY_CHANGE_CONTEXT_MARKER` | `@@` | 空上下文锚点 |

### 1.3 三类 Hunk 一例

```
*** Begin Patch
*** Add File: src/new.rs              ← 新增：每行必须 '+' 开头
+fn hello() {}
*** Update File: src/main.rs          ← 更新：'-' 删、'+' 增、' ' 上下文
*** Move to: src/renamed.rs           ← 可选：顺带改名
@@ fn main()                          ← 锚点：定位到含 "fn main()" 的位置之后
 let x = 1;
-let y = 2;
+let y = 3;
*** Delete File: src/old.rs           ← 删除：只需路径，无正文
*** End Patch
```

> 三件「严格」的事，模型错了会被拒：① 路径要相对（最终对 `cwd` 解析为绝对，见 `Hunk::resolve_path`，`apply-patch/src/parser.rs:84-90`）；② Add File 的每行必须 `+` 开头；③ Update File 的每个 chunk 默认要以 `@@` 开头（除非宽松放行，见 §2.4）。

---

## 2. 解析层：文本到结构化 Hunks

### 2.1 架构定位

`apply-patch` 是**独立 crate**，文件级注释（`apply-patch/src/lib.rs:1-19`）明确它「不依赖 codex-core」，文件读写经 `codex-exec-server` 的 `ExecutorFileSystem` 抽象——这样补丁应用逻辑既能直接跑、也能塞进沙箱里跑。模块划分见 `apply-patch/src/lib.rs:21-25`：`invocation`（识别/校验调用）、`parser`（批量解析）、`seek_sequence`（模糊匹配）、`standalone_executable`（自调子命令）、`streaming_parser`（增量解析）。

### 2.2 Hunk 数据结构

解析产物是 `enum Hunk`，`apply-patch/src/parser.rs:65-81`：

```rust
pub enum Hunk {
    AddFile { path: PathBuf, contents: String },
    DeleteFile { path: PathBuf },
    UpdateFile {
        path: PathBuf,
        move_path: Option<PathBuf>,
        chunks: Vec<UpdateFileChunk>,  // chunk 之间须按文件中出现顺序排列
    },
}
```

`UpdateFileChunk`（同文件 `:118-126`）是更新的最小单元：

| 字段 | 含义 |
|------|------|
| `change_context: Option<String>` | `@@` 后的锚点行（可空） |
| `old_lines: Vec<String>` | 被替换/删除的旧行（含 `' '` 上下文与 `'-'` 行） |
| `new_lines: Vec<String>` | 替换后的新行（含 `' '` 上下文与 `'+'` 行） |
| `is_end_of_file: bool` | 该 chunk 是否贴着文件末尾（影响匹配起点，见 §4.2） |

注意 `' '` 上下文行同时进 `old_lines` 和 `new_lines`，`'+'` 只进 new，`'-'` 只进 old —— 见分类逻辑 `apply-patch/src/parser.rs:429-461`。

### 2.3 解析入口与三步式主循环

`parse_patch()` 是公开入口，`apply-patch/src/parser.rs:128-135`，它固定走「宽松模式」（`PARSE_IN_STRICT_MODE = false`，见 `:52`）。真正干活的是 `parse_patch_text()`，`apply-patch/src/parser.rs:176-199`，三步：

```
parse_patch_text(patch, mode):
  ① check_patch_boundaries_{strict|lenient}  → 校验首尾 *** Begin/End Patch，剥出中间 hunk_lines
  ② parse_environment_id_preamble            → 提取可选 *** Environment ID
  ③ while remaining_lines 非空:               → 逐个 parse_one_hunk，累计行号
       hunks.push(hunk)
```

### 2.4 为什么需要「宽松模式」

`ParseMode::Lenient`（`apply-patch/src/parser.rs:137-173`）有大段注释解释动机：gpt-4.1 在 `local_shell` 工具里会把补丁写成 heredoc 形式 `apply_patch <<'EOF' … EOF`，但 `local_shell` 不经 shell、走 `execvpe(3)`，于是 heredoc 包装被当成字面字符串原样传进来。`check_patch_boundaries_lenient`（`apply-patch/src/parser.rs:240-262`）的对策：先按严格模式试，失败时检查首行是否是 `<<EOF` / `<<'EOF'` / `<<"EOF"` 且尾行以 `EOF` 结尾、总行数 ≥ 4，是则剥掉 heredoc 外壳再按严格模式解析中间内容。

> 取舍：本可让每个调用点传一个 strictness 参数，但注释直言「穿过所有调用点太麻烦」，索性对所有模型一律宽松。

### 2.5 三类 hunk 的逐行解析

`parse_one_hunk()`（`apply-patch/src/parser.rs:286-374`）按首行前缀分派：

- **Add File**（`:288-306`）：逐行扫描 `'+'` 前缀，`strip_prefix('+')` 后拼进 `contents` 并补 `'\n'`；遇到非 `'+'` 行即停止该 hunk。
- **Delete File**（`:307-313`）：只取路径，固定消费 1 行。
- **Update File**（`:314-366`）：先取 `path`，再看下一行是否 `*** Move to:` 提取 `move_path`；然后循环——空行跳过、遇 `'*'` 开头（下一个 hunk 标记）则停、否则递归 `parse_update_file_chunk` 收一段 chunk。

`parse_update_file_chunk()`（`apply-patch/src/parser.rs:376-465`）是 chunk 解析核心：

1. 识别 `@@`（`EMPTY_CHANGE_CONTEXT_MARKER`，无锚点）或 `@@ xxx`（`CHANGE_CONTEXT_MARKER`，带锚点）；若都不匹配且不允许缺锚点则报错（`:392-401`）。
2. 逐行按首字符分类：空行 → old/new 都加空串；`' '` → old/new 都加；`'+'` → 仅 new；`'-'` → 仅 old（`:429-461`）。
3. 遇 `*** End of File` 置 `is_end_of_file=true` 并停（`:418-428`）；遇无法识别的首字符且已解析过行，则视为「下一个 hunk 的开头」而停（`:446-457`）。

---

## 3. 验证层：补丁校验与权限计算

解析只保证「补丁格式合法」，不保证「能套到真实文件上」（`parser.rs:2` 注释明示）。验证在 `invocation.rs` 完成。

### 3.1 隐式调用拦截 + 校验

`maybe_parse_apply_patch_verified()`（`apply-patch/src/invocation.rs:134-159`）先拦「隐式调用」——模型只把补丁正文当成命令或 shell 脚本正文传进来、却没显式 `apply_patch`：

```rust
if let [body] = argv && parse_patch(body).is_ok() {
    return CorrectnessError(ApplyPatchError::ImplicitInvocation);   // invocation.rs:142-146
}
```

`ApplyPatchError::ImplicitInvocation`（`apply-patch/src/lib.rs:75-79`）的错误消息直接教模型怎么改：`Rerun as ["apply_patch", "<patch>"]`。这是「错误即反馈」设计的缩影。

随后调 `verify_apply_patch_args()`（`apply-patch/src/invocation.rs:161-`），把 hunks 转成 `ApplyPatchFileChange` map。关键点：Delete 和 Update 需要**读原文件**来计算 `content` / `unified_diff` / `new_content`，读不到就归为 `CorrectnessError`；成功则返回 `MaybeApplyPatchVerified::Body(ApplyPatchAction)`。

四态结果枚举 `MaybeApplyPatchVerified`（`apply-patch/src/lib.rs:146-159`）：

| 变体 | 含义 | core 侧处理 |
|------|------|-------------|
| `Body(ApplyPatchAction)` | 是 apply_patch，校验通过 | 走落盘 |
| `ShellParseError` | argv 无法判断是否 apply_patch | 当普通 shell 解析错 |
| `CorrectnessError(ApplyPatchError)` | 是 apply_patch 但补丁有错 | 报错给模型 |
| `NotApplyPatch` | 明确不是 apply_patch | 放行给普通 shell |

### 3.2 变更计划数据结构

`ApplyPatchFileChange`（`apply-patch/src/lib.rs:124-141`）是「拟施加」计划（尚未落盘）：

```rust
pub enum ApplyPatchFileChange {
    Add    { content: String },
    Delete { content: String },
    Update { unified_diff: String, move_path: Option<PathBuf>, new_content: String },
}
```

`ApplyPatchAction`（`apply-patch/src/lib.rs:163-174`）打包整个补丁的施用计划：`changes: HashMap<PathBuf, ApplyPatchFileChange>` + `patch`（已剥 heredoc 的原文）+ `cwd: AbsolutePathBuf`。注释保证「所有路径都是绝对路径」。

### 3.3 安全评估与委派

core 侧 `apply_patch()`（`core/src/apply_patch.rs:33-74`）调 `assess_patch_safety()` 做权限/沙箱决策，按结果返回 `InternalApplyPatchInvocation`（`core/src/apply_patch.rs:13-24`）：

```
assess_patch_safety(action, approval_policy, permission_profile,
                    fs_sandbox_policy, cwd, windows_sandbox_level)
  match SafetyCheck:
    AutoApprove{user_explicitly_approved} → DelegateToRuntime{ exec_approval: Skip }
    AskUser                               → DelegateToRuntime{ exec_approval: NeedsApproval }
    Reject{reason}                        → Output(Err RespondToModel("patch rejected: …"))
```

注意 `AutoApprove` 时 `auto_approved = !user_explicitly_approved`（`core/src/apply_patch.rs:51`）——自动批准和用户显式批准都走 `DelegateToRuntime`，区别只在是否还要弹审批。沙箱策略的三种模式与决策细节见 [doc13 沙箱机制](./13_sandbox_mechanism.md)。

> **易误解点**：`DelegateToRuntime` 之后，权限审批与沙箱限制才**真正生效**；前面的解析/校验只是「形式」阶段，落盘时才「实质」检查文件系统权限。把校验当成「已确保能写」是错觉。

`convert_apply_patch_to_protocol()`（`core/src/apply_patch.rs:76-100`）把 `ApplyPatchFileChange` 转成协议层 `FileChange`（`protocol/src/protocol.rs:3670-3681`，`#[serde(tag = "type", rename_all = "snake_case")]`，含 Add/Delete/Update 三态）供 UI 展示。注意转换时 `Update` 丢掉了 `new_content`（`core/src/apply_patch.rs:91`），协议只关心 diff 和 move 目标。协议事件与 `Op` 体系见 [doc11 §10](../2_运行时核心/11_thread_session_turn_lifecycle.md)。

---

## 4. 应用层：模糊匹配与行定位

这是整套机制最精巧的部分：在没有行号的前提下，把 chunk 准确套到文件里。

### 4.1 Update 应用三段式

`derive_new_contents_from_chunks()`（`apply-patch/src/lib.rs:689-721`）：

```
① 读原文件 → split('\n') 成行向量；末尾因终止换行产生的空串 pop 掉（对齐 diff 行计数）
② compute_replacements()  → 算出 (start_idx, old_len, new_lines) 三元组列表
③ apply_replacements()    → 倒序套用三元组
④ 若结果末行非空则补一个空串 → join('\n') 还原终止换行
```

### 4.2 compute_replacements：容错定位核心

`compute_replacements()`（`apply-patch/src/lib.rs:731-819`）逐 chunk 计算替换，分三种情况：

**(a) 有 change_context**（`:742-757`）：用 `seek_sequence` 单独定位锚点行，命中则把 `line_index` 推进到锚点之后再继续；找不到锚点直接报 `Failed to find context`。

**(b) 纯新增块 `old_lines.is_empty()`**（`:759-768`）：插到文件末尾；若末行是空串（终止换行），插到它之前。

**(c) 普通块**（`:771-813`）：用 `seek_sequence` 逐字定位 `old_lines`；定位失败且 `old_lines` 末尾是空串时，去掉这个尾随空串再重试（`:788-802`）——这正是 §1.1 提到的「EOF 换行哨兵」处理：

```rust
if found.is_none() && pattern.last().is_some_and(String::is_empty) {
    pattern = &pattern[..pattern.len() - 1];       // 去掉末尾空串
    if new_slice.last().is_some_and(String::is_empty) {
        new_slice = &new_slice[..new_slice.len() - 1];
    }
    found = seek_sequence::seek_sequence(original_lines, pattern, line_index, chunk.is_end_of_file);
}
```

最后 `replacements.sort_by_key(|(index,_,_)| *index)` 升序排（`:816`），为倒序套用做准备。

> **为什么有这个哨兵坑**：补丁协议里 `old_lines` 末尾常带一个代表「终止换行」的空串，但 `split('\n')` 切出的 `original_lines` 在 ① 步已 pop 掉末尾空串，两者不对齐导致直接匹配失败。去尾重试是救济措施，不是根本解（文件头 `lib.rs:17-19` 把这点列为「关键设计」）。

### 4.3 seek_sequence：四级降级匹配

`seek_sequence()`（`apply-patch/src/seek_sequence.rs:12-110`）从严到松四级尝试，镜像 `git apply` 的模糊行为：

```
① exact   : lines[i..i+n] == pattern               (seek_sequence.rs:35-39)
② rstrip  : 逐行 trim_end() 后比                    (seek_sequence.rs:41-52)
③ trim    : 逐行 trim()（两端）后比                  (seek_sequence.rs:54-65)
④ normalise: Unicode 标点转 ASCII 后比              (seek_sequence.rs:96-107)
```

第四级 `normalise()`（`apply-patch/src/seek_sequence.rs:76-94`）把各式排版字符折叠成 ASCII：EN/EM DASH（`U+2010`~`U+2015`、`U+2212`）→ `'-'`；花引号（`U+2018`~`U+201B`）→ `'\''`、（`U+201C`~`U+201F`）→ `'"'`；各种不间断/全角空格 → 普通空格。这让模型生成的纯 ASCII 补丁能匹配含排版引号的源码。

**eof 模式**（`apply-patch/src/seek_sequence.rs:29-33`）：当 `eof=true`（chunk 的 `is_end_of_file`），搜索起点设为 `lines.len() - pattern.len()`（直奔文件末尾），否则从 `start` 开始——专门服务贴边 EOF 的修改。另有两处防御：空 pattern 返回 `Some(start)`、`pattern.len() > lines.len()` 直接返回 `None` 避免越界 panic（`:18-28`）。

> **二阶效应**：四级容错灵活，但过度容错可能在相似代码块误匹配。设计上「先 exact 再降级」+「从 `line_index` 之后顺序推进」就是为压低误伤面。

### 4.4 apply_replacements：为什么必须倒序

`apply_replacements()`（`apply-patch/src/lib.rs:825-849`）对每个三元组先 `remove` 旧行再 `insert` 新行。`.rev()` 倒序遍历是硬性要求：

```rust
for (start_idx, old_len, new_segment) in replacements.iter().rev() {  // lib.rs:831
    for _ in 0..old_len { lines.remove(start_idx); }                  // 先删
    for (offset, l) in new_segment.iter().enumerate() {
        lines.insert(start_idx + offset, l.clone());                  // 再插
    }
}
```

若顺序套用，靠前区域的增删会挪动靠后三元组记录的下标，导致系统性错位。compute 阶段已升序排好，这里倒着走，靠后的先改、不影响靠前下标。

```
原始:  [A][B][C][D][E]          替换计划(升序):  (1, 1, [b'])  (3, 1, [d'])
顺序套(错): 改 idx=1 → [A][b'][C][D][E]，长度未变巧合没错；
            但若 (1,2,[x]) 删 2 行后 idx=3 已指向别处 → 错位
倒序套(对): 先改 idx=3 → [A][B][C][d'][E]，再改 idx=1，互不干扰
```

---

## 5. 落盘层：文件系统操作与容错

### 5.1 总入口

`apply_patch()`（`apply-patch/src/lib.rs:311-347`）：`parse_patch` 失败 → 把可读错误写 `stderr` → 返回带空 delta 的 `ApplyPatchFailure`；成功 → 转 `apply_hunks()` → `apply_hunks_to_files()` 真正写盘。函数签名带 `fs: &dyn ExecutorFileSystem` 和 `sandbox: Option<&FileSystemSandboxContext>`，即文件操作全经抽象层、可被沙箱包裹。

### 5.2 逐 hunk dispatch 与 exact 标志

`apply_hunks_to_files()`（`apply-patch/src/lib.rs:396-585`）逐 hunk 分派，并维护 `delta`。核心容错思想：**任何可能让 delta 不再「精确」的事都把 `delta.exact` 置 false**。`try_write!` 宏（`:413-423`）在写失败时设 `exact=false` 再返回错误，因为「截断后 ENOSPC」之类失败可能已改了目标文件。

| Hunk | 落盘动作 | exact 受影响处 |
|------|----------|----------------|
| `AddFile`（`:429-450`） | 读原内容存 `overwritten_content` → `write_file_with_missing_parent_retry`（自动建父目录） | 读不到原内容、写失败 |
| `DeleteFile`（`:451-488`） | 读待删内容 → `ensure_not_directory` → `remove` | 读不到内容、删失败 |
| `UpdateFile + move`（`:497-556`） | **先写目标**、记 Add 占位、`ensure_not_directory` 源 → 删源 → 把占位**改写**成 Update 记录 | 删源失败 |
| `UpdateFile 无 move`（`:557-576`） | 直接 `write_file` 覆写源 | 写失败 |

> **Move 不是原子的**：先写目标、入 delta、再删源；中间失败会留下目标文件。`ApplyPatchFailure.delta()`（`apply-patch/src/lib.rs:298-300`）返回的「已落盘变更」是回滚的关键依据。

### 5.3 已落盘 delta 数据结构

`AppliedPatchDelta`（`apply-patch/src/lib.rs:215-247`）记录「实际已落盘」的变更序列：

```rust
pub struct AppliedPatchDelta {
    changes: Vec<AppliedPatchChange>,
    exact: bool,                       // 是否全部精确套用（无模糊/IO 异常）
}
```

`append()`（`:243-246`）用按位与聚合 exact：`self.exact &= other.exact`，一处不精确则整体不精确。`AppliedPatchChange`（`:256-260`）= `path` + `AppliedPatchFileChange`。后者（`:262-277`）比「计划」版多带回滚信息：Add 记 `overwritten_content`、Update 记 `old_content` 和 `overwritten_move_content`。

> 注意：大纲里的「`inexact` 字段」在真实源码中是 `exact: bool`（语义相反），文中以 `delta.is_exact()`（`:238-240`）为准。

---

## 6. 工具集成：与 LLM 的反馈环

### 6.1 处理器定位

`ApplyPatchHandler`（`core/src/tools/handlers/apply_patch.rs:73-82`）是 tools 层的写文件工具，文件头注释（`:1-13`）概括其职责：**解析补丁 → 选定目标环境 → 在该环境文件系统上先校验 → 据 diff 计算最小所需权限 → 按需审批 → 落盘并发 `PatchApplyUpdated` 事件**。设计要点是「先校验再应用」+「按 diff 算最小权限」，避免过度授权。工具系统总体见 [doc11](../2_运行时核心/11_thread_session_turn_lifecycle.md)。

### 6.2 freeform 调用主流程

`ApplyPatchHandler::handle()`（`core/src/tools/handlers/apply_patch.rs:329-`）：

```
取 Custom payload 补丁文本
  → parse_patch()               失败 → RespondToModel(让模型重写)
  → require_environment_id()    多环境时必须显式指定 environment_id (:356-357)
  → resolve_tool_environment()  解析目标环境 (:360-366)
  → verify_apply_patch_args()   在该环境 fs 上校验 (:370)
    └ Body(changes):
        effective_patch_permissions() 算最小权限 (:374-376)
        apply_patch::apply_patch()     安全评估 (:377)
          → Output:           直接返回
          → DelegateToRuntime: 发 begin 事件 → 构造 ApplyPatchRequest → ApplyPatchRuntime 落盘
```

> **关键设计**：所有失败都以 `FunctionCallError::RespondToModel` 返回（`:344-346`、`:351-353`），让模型看到原因自行纠偏，而非中断整个回合。这是「补丁校验两层把关 → 错误反馈给模型」反馈环的落点。

### 6.3 shell 路径拦截

`intercept_apply_patch()`（`core/src/tools/handlers/apply_patch.rs:516-`）处理另一条路径：模型可能把补丁塞进 shell 命令里。它调 `maybe_parse_apply_patch_verified()` 识别「这其实是 apply_patch」，命中就改走同一套落盘流程（含 `effective_patch_permissions`、`apply_patch::apply_patch`、`ToolEmitter::apply_patch` 发 begin 事件）。这样无论模型走 freeform 工具还是 shell，最终都汇流到相同的权限/落盘逻辑。

### 6.4 运行时

`ApplyPatchRequest`（`core/src/tools/runtimes/apply_patch.rs:46-55`）打包一次落盘所需的一切：`turn_environment`、`action`、`file_paths`、协议 `changes`、`exec_approval_requirement`、`additional_permissions`、`permissions_preapproved`。`ApplyPatchRuntime`（`:57-60`）持有 `committed_delta: AppliedPatchDelta`，`ApplyPatchRuntimeOutput`（`:62-66`）含 `exec_output` 和最终 `delta`。审批键 `ApplyPatchApprovalKey`（`:40-44`）= `environment_id` + `path`，用于缓存按路径的批准决策。

### 6.5 流式预览（Streaming）

为让用户边接收补丁边看预览，有一套增量解析 + 节流推流。`StreamingPatchParser`（`apply-patch/src/streaming_parser.rs:21-26`）是行缓冲状态机，模式枚举 `StreamingParserMode`（`:34-45`）：

```
NotStarted → StartedPatch → {AddFile | DeleteFile | UpdateFile{hunk_line_number}} → EndedPatch
```

`ApplyPatchArgumentDiffConsumer.push_delta()`（`core/src/tools/handlers/apply_patch.rs:113-134`）的节流逻辑：

```
parser.push_delta(delta) → hunks
convert_apply_patch_hunks_to_protocol(hunks) → changes (:118, 151-173)
event = PatchApplyUpdatedEvent{ call_id, changes }
  距上次发送 < 500ms (APPLY_PATCH_ARGUMENT_DIFF_BUFFER_INTERVAL, :70):
      → 存入 pending，不发
  否则:
      → 立即发，更新 last_sent_at
finish() (:136-148) → 刷出 pending event
```

`convert_apply_patch_hunks_to_protocol()`（`core/src/tools/handlers/apply_patch.rs:151-173`）和 §3.3 的 `convert_apply_patch_to_protocol` 类似，但输入是流式解析的 `Hunk`、且 DeleteFile 的 `content` 给空串（预览阶段还没读文件）、Update 用 `format_update_chunks_for_progress` 临时拼 diff。

### 6.6 给模型的使用说明

`APPLY_PATCH_TOOL_INSTRUCTIONS`（`apply-patch/src/lib.rs:54-55`）用 `include_str!` 嵌入 `apply_patch_tool_instructions.md`，含详细用法示例、语法图解和常见错误提示——这是把补丁格式「教给模型」的载体，与 §3.1 的错误消息一起构成模型侧的引导。

---

## 7. 跨回合追踪：TurnDiffTracker 机制

### 7.1 目的

模型一个回合内可能多次 apply_patch。UI 想展示「本回合相对回合初态的净 diff」，但每次都回文件系统重读太慢。`TurnDiffTracker`（`core/src/turn_diff_tracker.rs:18-24`）就是「不重读文件系统、纯从已落盘 delta 累积净 diff」的内存追踪器：

```rust
pub struct TurnDiffTracker {
    valid: bool,                                       // 是否还能精确追踪
    display_root: Option<PathBuf>,
    baseline_by_path: HashMap<PathBuf, String>,        // 回合初态
    current_by_path: HashMap<PathBuf, String>,         // 当前态
    origin_by_current_path: HashMap<PathBuf, PathBuf>, // 改名溯源
}
```

> 注意字段名与大纲略有出入：真实字段是 `baseline_by_path` / `current_by_path` / `origin_by_current_path` + `valid`，而非泛指的 baseline/current/rename_pairs map。

### 7.2 track_delta：精确性闸门

`track_delta()`（`core/src/turn_diff_tracker.rs:49-58`）：

```rust
pub fn track_delta(&mut self, delta: &AppliedPatchDelta) {
    if !delta.is_exact() {
        self.invalidate();              // 一旦不精确，整个回合追踪作废
        return;
    }
    for change in delta.changes() {
        self.apply_change(change);
    }
}
```

`invalidate()`（`:60-62`）置 `valid=false`；之后 `get_unified_diff()`（`:64-`）首句 `if !self.valid { return None; }`。即 **delta 不精确 → 回合 diff 追踪失效 → 上层只能回文件系统重读**。

`apply_change()`（`core/src/turn_diff_tracker.rs:109-130`）按 `AppliedPatchFileChange` 三态分派到 `apply_add`（`:132`）、`apply_delete`（`:145`）、`apply_update`（`:154`）。逻辑要点：首次见到某路径时把「被覆盖/删除/更新前的内容」存进 `baseline_by_path`（仅当 baseline 和 current 都还没有该路径），后续更新只动 `current_by_path`。`rename_pairs()`（`:200`）从 `origin_by_current_path` 还原改名配对，供 `get_unified_diff` 产出 git 风格的 rename diff。

### 7.3 inexact 的级联影响

任何 I/O 失败、读不到原文件、目标是 symlink/目录等非普通文件，都会在 §5.2 把 `delta.exact` 置 false。这一个 bool 顺着 `AppliedPatchDelta.append` 的按位与传到 `track_delta`，最终让本回合 `get_unified_diff()` 返回 `None`。涉及多回合连续编辑时，这是「为什么有时 diff 视图会回退到重读文件系统」的根因。

---

## 8. 常见问题

**Q1：为什么不直接让模型给标准 unified diff？**
标准 diff 依赖精确行号，LLM 数行号极易出错。V4A 去行号、用内容定位，把定位难题交给 `seek_sequence` 四级模糊匹配（§4.3），容错性远高于行号匹配。

**Q2：`old_lines` 末尾的空串是什么？为什么定位会失败？**
它是补丁协议里代表「文件终止换行」的哨兵。但 `derive_new_contents_from_chunks` 用 `split('\n')` 切原文件后会 pop 掉末尾空串（`apply-patch/src/lib.rs:706-708`），两者不对齐。`compute_replacements` 的「去尾重试」（`:788-802`）是救济，不是根治。

**Q3：替换为什么必须倒序套用？**
顺序套用时靠前区域的增删会挪动靠后三元组的下标，导致系统性错位。compute 阶段升序排、apply 阶段 `.rev()` 倒序走，靠后的先改、不影响靠前（`apply-patch/src/lib.rs:829-831`）。

**Q4：Unicode 标点容错会不会把不该匹配的匹配上？**
四级匹配严格→宽松递进，前三级（exact/rstrip/trim）优先，只有都失败才走第四级 `normalise`。加上「从 `line_index` 之后顺序推进」，误伤面被压到较小。但理论上相似代码块仍可能误匹配——这是容错的固有二阶代价。

**Q5：补丁校验通过就代表一定能写盘吗？**
不。校验只在「形式」层（路径合法、上下文能定位、能读原文件）。权限/沙箱的「实质」检查发生在 `DelegateToRuntime` 之后的落盘阶段（§3.3）。校验通过的补丁仍可能因沙箱拒写、ENOSPC 等失败。

**Q6：Move（改名）操作失败会留下什么？**
非原子：先写目标、再删源。删源失败时目标文件已存在（`apply-patch/src/lib.rs:497-555`）。`ApplyPatchFailure.delta()` 返回的已落盘变更列表是回滚/汇报的依据。

**Q7：为什么一次小改动后整个回合的 diff 视图失效了？**
本回合某次 apply_patch 的 delta `exact=false`（可能因读不到原文件、写中途失败、非普通文件等）。该标志让 `TurnDiffTracker.track_delta` 调 `invalidate()`，之后 `get_unified_diff()` 返回 `None`，上层只能回文件系统重读（§7.2-7.3）。

---

### 相关文档

- [doc11 线程/会话/回合生命周期](../2_运行时核心/11_thread_session_turn_lifecycle.md)：工具系统、协议 `Op`（§10）、回合流程。
- [doc13 沙箱机制](./13_sandbox_mechanism.md)：`SandboxPolicy` 三种模式、`assess_patch_safety` 背后的沙箱决策。
- [doc16 Agent 优化](../2_运行时核心/16_agent_optimization.md)：模型侧引导与工具说明的关系。
