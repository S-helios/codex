# Codex 沙箱机制深度解析

## 1. 概述：为什么需要沙箱？

Codex 的核心能力是让 AI 模型能够在用户的计算机上**自主执行 shell 命令、修改文件、运行代码**。这种能力带来了显著的安全风险：

- AI 可能生成并执行**破坏性命令**（误删文件、格式化磁盘）
- 执行的代码可能**窃取敏感数据**（API Keys、SSH 密钥、数据库凭据）
- 恶意或错误的代码可能**向外网传输数据**
- 代码可能**提升进程权限**，突破用户账户限制

沙箱机制是 Codex 的"安全护城河"——它在操作系统层面限制 AI 所运行的子进程能做什么，即使 AI 生成了危险命令，沙箱也会阻止其实际产生破坏。

---

## 2. 沙箱的权限策略（SandboxPolicy）

沙箱行为由**沙箱策略（`SandboxPolicy`）**决定。`SandboxPolicy` 定义在
`codex-rs/protocol/src/protocol.rs:928`，用 `#[serde(tag = "type", rename_all = "kebab-case")]`
序列化，共有 **4 个变体**（CLI 的 `--sandbox` 标志只暴露前 3 个，对应
`SandboxMode` 枚举，见 `codex-rs/protocol/src/config_types.rs:86`）：

| 变体 | serde / CLI 标识符 | 文件系统权限 | 网络权限 | 典型场景 |
|------|--------|------------|---------|---------|
| `ReadOnly` | `read-only` | 全盘只读，禁止写入 | 默认禁止（`network_access` 可放行） | 浏览代码、分析文件 |
| `WorkspaceWrite` | `workspace-write` | 工作区可写，其余只读（可配 `writable_roots` / 是否含 TMPDIR、`/tmp`） | 默认禁止（`network_access` 可放行） | 默认开发模式 |
| `DangerFullAccess` | `danger-full-access` | 不限制文件系统 | 不限制 | 明确授权的高权限任务 |
| `ExternalSandbox` | `external-sandbox` | 放开磁盘（假定已在外部沙箱内） | 按外部沙箱的 `network_access` 设置 | 进程已被外层沙箱包裹时 |

> 注意：`SandboxPolicy`（「能碰什么」）与运行时的 `FileSystemSandboxPolicy` /
> `NetworkSandboxPolicy`（`codex-rs/protocol/src/permissions.rs`）是两套表示：前者是
> 配置/协议层的高层策略，后者是决策函数实际消费的细粒度运行时权限。

CLI 配置方式（`--sandbox` 取值即上表 serde 标识符的前 3 个）：
```
codex --sandbox read-only
codex --sandbox workspace-write
codex --sandbox danger-full-access
```

---

## 3. 跨平台沙箱实现

Codex 根据操作系统选择不同的沙箱技术。核心类型定义在 `codex-rs/sandboxing/src/manager.rs:41`：

```rust
pub enum SandboxType {
    None,
    MacosSeatbelt,           // macOS 专用：sandbox-exec
    LinuxSeccomp,            // Linux 专用（实际是 bubblewrap 文件系统隔离 + seccomp 系统调用过滤）
    WindowsRestrictedToken,  // Windows 专用：受限令牌
}
```

`SandboxType` 只是「后端选型」枚举（用 `as_metric_tag()` 映射成埋点短标签：
`none` / `seatbelt` / `seccomp` / `windows_sandbox`）。真正描述「能碰什么」的策略
枚举是 `SandboxPolicy`（见 `codex-rs/protocol/src/protocol.rs:928`），二者正交：
`SandboxPolicy` 决定权限，`SandboxType` 决定用哪种 OS 机制去落地这份权限。

### 3.1 macOS：Seatbelt（沙箱执行）

**技术原理：** 调用 macOS 内置的 `/usr/bin/sandbox-exec`，传入一段用 Scheme 语言写成的安全策略文件（`.sbpl` 格式）。

**关键文件：**
- `codex-rs/sandboxing/src/seatbelt.rs` — 策略生成逻辑
- `codex-rs/sandboxing/src/seatbelt_base_policy.sbpl` — 基础拒绝策略
- `codex-rs/sandboxing/src/seatbelt_network_policy.sbpl` — 网络策略

**工作方式：**

1. **默认全部拒绝**（`(deny default)`）——沙箱从完全封闭开始，只允许显式声明的操作：

```scheme
(version 1)
; inspired by Chrome's sandbox policy
(deny default)

(allow process-exec)
(allow process-fork)
(allow signal (target same-sandbox))
```

2. **动态生成文件系统策略**——根据权限配置构建允许读写的路径：

```
// 只允许工作区可写
(allow file-write*
  (subpath (param "WRITABLE_ROOT_0")))

// 其余路径只读
(allow file-read*
  (regex #"^/"))
```

3. **网络策略动态注入**：
   - 网络禁用：不添加任何 `network-outbound` 规则（隐式拒绝）
   - 仅代理模式：只允许本地代理端口（如 `localhost:8080`）
   - 完全网络：`(allow network-outbound)(allow network-inbound)`

4. **硬编码可执行文件路径**（防攻击者替换）：
```rust
pub const MACOS_PATH_TO_SEATBELT_EXECUTABLE: &str = "/usr/bin/sandbox-exec";
```

**调用示意：**
```bash
/usr/bin/sandbox-exec -p "(deny default)(allow file-read*)(allow process-exec)..." \
  -DWRITABLE_ROOT_0=/Users/user/myproject \
  -- bash -c "npm install"
```

---

### 3.2 Linux：双层沙箱（Bubblewrap + Seccomp）

Linux 采用两层叠加的沙箱，防御深度更强。

**关键文件：**
- `codex-rs/linux-sandbox/src/linux_run_main.rs` — 沙箱 helper 入口（`run_main()` 在 `:169`）
- `codex-rs/linux-sandbox/src/bwrap.rs` — Bubblewrap 文件系统隔离的实现（文件头自述：
  「整体 Linux 沙箱 = 进程内 seccomp + `PR_SET_NO_NEW_PRIVS` + bubblewrap 文件系统隔离」）
- `codex-rs/linux-sandbox/src/landlock.rs` — **进程内 seccomp 过滤 + `no_new_privs` 的真正实现**
  （文件头：「In-process Linux sandbox primitives」；Landlock 文件系统规则在此仅作 legacy/备用，
  实际文件系统隔离交给 bubblewrap）
- `codex-rs/sandboxing/src/landlock.rs` — 注意：此文件**只是 helper 的 CLI 参数构造器**
  （`create_linux_sandbox_command_args_for_permission_profile` 在 `:23`），不含 seccomp/landlock 落地逻辑

> 重构说明：seccomp/landlock 的实际系统调用过滤代码位于 `linux-sandbox/src/landlock.rs`，
> 而非同名的 `sandboxing/src/landlock.rs`。后者属于跨平台 `sandboxing` crate，只负责把权限
> profile 序列化成 `codex-linux-sandbox` 的命令行参数；前者属于 `linux-sandbox` crate，在 helper
> 进程内真正调用 `seccompiler` / `prctl(PR_SET_NO_NEW_PRIVS)`。两者同名易混淆，务必分清。

#### 第一层：Bubblewrap 文件系统命名空间隔离

Bubblewrap（`bwrap`）是一个无需 root 权限的沙箱工具，通过 Linux 用户命名空间创建隔离的文件系统视图：

- 以只读绑定挂载（`--ro-bind`）暴露系统目录
- 以可写绑定挂载（`--bind`）暴露工作区目录
- 隔离 PID 命名空间（`--unshare-pid`）
- 隔离网络命名空间（`--unshare-net`，网络禁用时）
- 挂载全新的 `/proc`（`--proc /proc`）
- 在容器外看不到宿主机的其他目录

#### 第二层：Seccomp 系统调用过滤

在 Bubblewrap 建立文件系统视图后，再应用 seccomp 规则过滤危险系统调用。
规则定义见 `codex-rs/linux-sandbox/src/landlock.rs` 的
`install_network_seccomp_filter_on_current_thread`（`:169`）。seccomp 默认动作是
**放行（`SeccompAction::Allow`）**，命中规则的系统调用返回 `EPERM`（黑名单式）。

**永远拒绝的系统调用（只要装了网络 seccomp 过滤器，所有模式都拒）：**
- `ptrace` — 进程监控/注入
- `process_vm_readv`, `process_vm_writev` — 跨进程内存读写
- `io_uring_setup`, `io_uring_enter`, `io_uring_register` — 异步 I/O 环（可绕过沙箱）

**`Restricted`（断网）模式额外拒绝的网络相关调用（以下为部分，完整名单见源码）：**
- `connect`, `accept`, `accept4` — 网络连接
- `bind`, `listen` — 服务器套接字
- `sendto`, `sendmmsg`, `recvmmsg` — 套接字收发（注意 `recvfrom` 被有意放行，
  否则 `cargo clippy` 等工具的 socketpair 子进程管理会失败）
- `getpeername`, `getsockname`, `shutdown`, `getsockopt`, `setsockopt`
- `socket` / `socketpair`：仅放行 `AF_UNIX`（domain == AF_UNIX），其余协议族一律拒绝

> `ProxyRouted`（受管代理）模式与 `Restricted` 不同：它**放行 `AF_INET` / `AF_INET6`**
> （让流量能到本地 TCP 桥），反而**拒绝 `AF_UNIX`** 的 `socket`/`socketpair`，防止
> 绕过受管代理桥。两种网络模式由 `NetworkSeccompMode` 枚举区分。

**权限提升防护：**
```
PR_SET_NO_NEW_PRIVS  // 防止子进程通过 setuid 提升权限；也是装载 seccomp 的前置条件
```
注意：`PR_SET_NO_NEW_PRIVS` 仅在「需要装 seccomp」或「显式走 legacy landlock 文件系统管线」
时才设置——因为很多 `bwrap` 部署依赖 setuid，过早设置会破坏 bubblewrap（见 `landlock.rs:57-65`）。

#### 执行流程

Linux sandbox helper 以独立进程（`codex-linux-sandbox`）形式存在：

```
主进程 → 构建参数 → 启动 codex-linux-sandbox →
  codex-linux-sandbox 内部：
    1. 解析权限 profile JSON
    2. 调用 bubblewrap 构建文件系统视图
    3. 在 bwrap 内应用 seccomp 规则
    4. execvp 目标命令
```

Landlock（旧版备用）：显式指定 `--use-legacy-landlock` 时，改用 Linux 的 Landlock LSM
直接在 helper 进程内应用文件系统访问控制（`install_filesystem_landlock_rules_on_current_thread`，
当前实现用 `ABI::V5`，`BestEffort` 兼容级别）。默认路径走 bubblewrap，此函数标了
`#[cfg_attr(not(test), allow(dead_code))]` / 注释「currently unused」，仅作 fallback 保留。
注意 legacy landlock 后端**不支持「受限只读」**（无全盘读权限时直接报
`UnsupportedOperation`，见 `landlock.rs:71-77`）。

---

### 3.3 Windows：受限令牌沙箱

Windows 使用安全令牌机制（Restricted Token）降低子进程权限。

**运行时级别枚举 `WindowsSandboxLevel`（`codex-rs/protocol/src/config_types.rs:269`，
kebab-case 序列化），共 3 个变体：**
- `Disabled`（默认）：不启用 Windows 沙箱
- `RestrictedToken`：仅降低令牌权限，不需要提升权限
- `Elevated`：完整沙箱设置，需要管理员权限提升

> 注意：TOML 配置项 `windows_sandbox_mode` 用的是另一个枚举 `WindowsSandboxModeToml`
> （变体为 `Elevated` / `Unelevated`）。在 `config/mod.rs:2629-2631` 处映射到运行时枚举：
> `Elevated → WindowsSandboxLevel::Elevated`、`Unelevated → WindowsSandboxLevel::RestrictedToken`。
> 即「配置里的 Unelevated」对应「运行时的 RestrictedToken」。

**关键文件：** `codex-rs/core/src/windows_sandbox.rs`（`resolve_windows_sandbox_mode` 等）；
CLI 侧实际执行入口为 `run_command_under_windows_sandbox`（`codex-rs/cli/src/debug_sandbox.rs`）。

---

## 4. 沙箱的触发决策逻辑

### 4.1 何时启用沙箱？

核心决策函数在 `codex-rs/sandboxing/src/policy_transforms.rs:509`：

```rust
pub fn should_require_platform_sandbox(
    file_system_policy: &FileSystemSandboxPolicy,
    network_policy: NetworkSandboxPolicy,
    has_managed_network_requirements: bool,
) -> bool {
    // 1. 有受管网络要求时，强制沙箱（确保流量经过代理）
    if has_managed_network_requirements {
        return true;
    }

    // 2. 网络被禁用时，除非是外部沙箱，否则启用平台沙箱
    if !network_policy.is_enabled() {
        return !matches!(
            file_system_policy.kind,
            FileSystemSandboxKind::ExternalSandbox
        );
    }

    // 3. 文件系统受限时，启用沙箱
    match file_system_policy.kind {
        FileSystemSandboxKind::Restricted => !file_system_policy.has_full_disk_write_access(),
        FileSystemSandboxKind::Unrestricted | FileSystemSandboxKind::ExternalSandbox => false,
    }
}
```

**决策矩阵：**

| 条件 | 结果 |
|------|------|
| 有受管网络（managed network）| 启用沙箱 |
| 网络禁用 + 非外部沙箱 | 启用沙箱 |
| 文件系统受限（非全盘写入）| 启用沙箱 |
| 完全访问（unrestricted）| 不启用 |
| 外部沙箱已处理 | 不启用 |

### 4.2 完整调用链

```
工具请求（shell/apply_patch/exec）
    ↓
SandboxManager::select_initial()         ← 据 Auto/Require/Forbid + 策略决定 SandboxType
    ↓
SandboxManager::transform()              ← 将命令包装进沙箱（拼前缀 / 注入参数）
    ↓
[macOS]  /usr/bin/sandbox-exec -p <SBPL策略> [-D<KEY>=<dir>...] -- <cmd>
[Linux]  codex-linux-sandbox --sandbox-policy-cwd <dir> --command-cwd <dir>
                              --permission-profile <json>
                              [--use-legacy-landlock] [--allow-network-for-proxy]
                              -- <cmd>
[Win]    受限令牌执行（透传 argv，隔离在 windows_sandbox 后端处理）
    ↓
子进程在沙箱内执行
```

> 说明：Linux 分支会在 argv 前插入 `codex-linux-sandbox` 可执行文件，并通过 `arg0`
> 覆写让该可执行文件「自调」识别身份（`CODEX_LINUX_SANDBOX_ARG0 = "codex-linux-sandbox"`），
> 随后在 helper 进程内 `run_main()` 解析上述参数、建 bubblewrap 视图、装 seccomp，再 exec 目标命令。

### 4.3 权限升级重试机制

当命令因沙箱限制被拒绝时，系统会请求用户批准，并可能以更高权限重试：

```
初始沙箱执行失败
    ↓
申请更多权限（AskForApproval）
    ↓ 用户批准
以升级后的权限重新执行
```

---

## 5. 沙箱的环境变量标记

沙箱激活时会在子进程环境中注入标记变量，让应用代码感知自身运行在沙箱中。
实际代码用的是命名常量（值如下注释），定义在 `core/src/spawn.rs`：
`CODEX_SANDBOX_ENV_VAR = "CODEX_SANDBOX"`、
`CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR = "CODEX_SANDBOX_NETWORK_DISABLED"`。

```rust
// 网络被禁用时（任意平台，最先判断）
if !network_sandbox_policy.is_enabled() {
    env.insert(CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR.to_string(), "1".to_string());
}

// 仅 macOS Seatbelt 沙箱激活时（#[cfg(target_os = "macos")]）
if sandbox == SandboxType::MacosSeatbelt {
    env.insert(CODEX_SANDBOX_ENV_VAR.to_string(), "seatbelt".to_string());
}
```

注入位置：`codex-rs/core/src/sandboxing/mod.rs:133-142`（在
`ExecRequest::from_sandbox_exec_request` 中）。注意 `CODEX_SANDBOX=seatbelt` **仅 macOS** 注入；
Linux/Windows 不设此变量，但断网时同样会设 `CODEX_SANDBOX_NETWORK_DISABLED=1`。

---

## 6. 关键设计决策

### 6.1 为什么 macOS 使用 sandbox-exec 而不是 Docker/虚拟机？

- macOS 沙箱（Seatbelt）是内核内置的，**零额外依赖**
- 比 Docker 轻量，**启动延迟极低**（毫秒级）
- 细粒度控制到系统调用和文件路径级别
- 不需要 root 权限

### 6.2 为什么 Linux 用两层而不是一层？

- Bubblewrap 提供**文件系统视图隔离**（哪些路径可见）
- Seccomp 提供**系统调用级别过滤**（哪些操作被允许）
- 两层互补：bubblewrap 不过滤系统调用；seccomp 不限制文件路径可见性

### 6.3 为什么硬编码 sandbox-exec 路径？

```rust
pub const MACOS_PATH_TO_SEATBELT_EXECUTABLE: &str = "/usr/bin/sandbox-exec";
```

防止攻击者在 PATH 中注入伪造的 `sandbox-exec`，绕过沙箱。

### 6.4 为什么 io_uring 被永久禁用？

`io_uring` 的异步 I/O 机制历史上多次被用于绕过 seccomp 过滤规则，因此无论其他权限配置如何，都永久禁用。

### 6.5 受保护的元数据路径

即使子进程获得了工作区写权限，沙箱仍会把若干「元数据目录」保持只读。
受保护名单 `PROTECTED_METADATA_PATH_NAMES`（`codex-rs/protocol/src/permissions.rs:27`）含三项：

- `.git`（尤其 `.git/hooks`——可被用来在后续 git 操作时执行任意代码）
- `.agents`
- `.codex`（Codex 自身的配置目录）

这通过 `WritableRoot.protected_metadata_names` 字段落地（`protocol.rs:983` 的结构体注释明确
点名 `.codex` / `.git` / `.git/hooks`）。目的：防止恶意代码通过改写这些目录里的文件
（git hook、Codex 配置等）实现持久化或权限提升攻击。

---

## 7. 代码位置速查

| 功能 | 文件 | 关键行 |
|------|------|--------|
| `SandboxType` 枚举（后端选型） | `codex-rs/sandboxing/src/manager.rs` | 41-46 |
| `SandboxPolicy` 枚举（权限策略） | `codex-rs/protocol/src/protocol.rs` | 928-976 |
| 沙箱类型选择 `select_initial` | `codex-rs/sandboxing/src/manager.rs` | 165-192 |
| 命令包装转换 `transform` | `codex-rs/sandboxing/src/manager.rs` | 199-292 |
| 是否启用沙箱决策 `should_require_platform_sandbox` | `codex-rs/sandboxing/src/policy_transforms.rs` | 509-529 |
| macOS 策略生成 `create_seatbelt_command_args` | `codex-rs/sandboxing/src/seatbelt.rs` | 631-770 |
| macOS 路径常量 `MACOS_PATH_TO_SEATBELT_EXECUTABLE` | `codex-rs/sandboxing/src/seatbelt.rs` | 46 |
| macOS 基础策略 | `codex-rs/sandboxing/src/seatbelt_base_policy.sbpl` | 全文 |
| Linux sandbox helper 入口 `run_main` | `codex-rs/linux-sandbox/src/linux_run_main.rs` | 169 |
| Linux seccomp + no_new_privs 落地 | `codex-rs/linux-sandbox/src/landlock.rs` | 全文（filter 在 169） |
| Linux bubblewrap 文件系统隔离 | `codex-rs/linux-sandbox/src/bwrap.rs` | 全文 |
| Linux helper 命令行参数构造 | `codex-rs/sandboxing/src/landlock.rs` | 23-59 |
| 核心适配层 | `codex-rs/core/src/sandboxing/mod.rs` | 全文 |
| 环境变量注入 | `codex-rs/core/src/sandboxing/mod.rs` | 133-142 |
| Windows 级别枚举 `WindowsSandboxLevel` | `codex-rs/protocol/src/config_types.rs` | 269-274 |

---

## 8. 沙箱调试工具

Codex 提供 `codex sandbox` 命令，在「Codex 自带沙箱」里直接跑任意命令，用于测试沙箱行为。
**它是单一子命令，按当前操作系统自动选择后端**（macOS→seatbelt、Linux→landlock/bwrap、
Windows→受限令牌），而非三个独立命令：

```bash
# 在沙箱内执行命令（-- 之后是被沙箱包裹的真实命令）
codex sandbox -- ls /

# 指定命名权限 profile（--permissions-profile / -C 指定 cwd）
codex sandbox --permissions-profile my-profile -C /path/to/project -- cat /etc/passwd
```

> 重构说明：早期版本的 `codex debug-seatbelt` / `codex debug-landlock` /
> `codex debug-windows-sandbox` 三个独立子命令已被合并为统一的 `codex sandbox`。
> 在 `codex-rs/cli/src/main.rs` 中，`Subcommand::Sandbox(HostSandboxArgs)`（`:160`）按
> `#[cfg(target_os = ...)]` 把参数结构体别名为 `SeatbeltCommand` / `LandlockCommand` /
> `WindowsCommand`（`cli/src/lib.rs`），分别分派到 `run_command_under_seatbelt` /
> `run_command_under_landlock` / `run_command_under_windows_sandbox`（`main.rs:1259-1278`）。

调试实现：`codex-rs/cli/src/debug_sandbox.rs`（注意此文件内另有一个 CLI 本地的
`SandboxType { Seatbelt, Landlock, Windows }` 枚举，与 `sandboxing/manager.rs` 的
`SandboxType { None, MacosSeatbelt, LinuxSeccomp, WindowsRestrictedToken }` 同名但不同物）。

---

## 9. 总结

Codex 沙箱机制的核心设计哲学是：

> **最小权限原则**（Principle of Least Privilege）——AI 执行的每个命令，只获得完成任务所需的最小权限，而非用户账户的完整权限。

通过平台原生沙箱技术（macOS Seatbelt、Linux Bubblewrap+Seccomp、Windows Restricted Token），Codex 在不损失性能的前提下，为用户提供了一道防止 AI 意外或恶意操作造成系统损害的安全屏障。
