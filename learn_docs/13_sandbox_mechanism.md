# Codex 沙箱机制深度解析

## 1. 概述：为什么需要沙箱？

Codex 的核心能力是让 AI 模型能够在用户的计算机上**自主执行 shell 命令、修改文件、运行代码**。这种能力带来了显著的安全风险：

- AI 可能生成并执行**破坏性命令**（误删文件、格式化磁盘）
- 执行的代码可能**窃取敏感数据**（API Keys、SSH 密钥、数据库凭据）
- 恶意或错误的代码可能**向外网传输数据**
- 代码可能**提升进程权限**，突破用户账户限制

沙箱机制是 Codex 的"安全护城河"——它在操作系统层面限制 AI 所运行的子进程能做什么，即使 AI 生成了危险命令，沙箱也会阻止其实际产生破坏。

---

## 2. 沙箱的三种权限模式

沙箱行为由**权限配置（PermissionProfile）**决定，有三种内置级别：

| 模式 | 标识符 | 文件系统权限 | 网络权限 | 典型场景 |
|------|--------|------------|---------|---------|
| 只读模式 | `:minimal` / `ReadOnly` | 全盘只读，禁止写入 | 禁止网络 | 浏览代码、分析文件 |
| 工作区写入 | `:workspace` / `WorkspaceWrite` | 工作区目录可写，其余只读 | 可配置 | 默认开发模式 |
| 完全访问 | `:unrestricted` / `DangerFullAccess` | 不限制文件系统 | 不限制 | 明确授权的高权限任务 |

CLI 配置方式：
```
codex --sandbox read-only
codex --sandbox workspace-write
codex --sandbox danger-full-access
```

---

## 3. 跨平台沙箱实现

Codex 根据操作系统选择不同的沙箱技术。核心类型定义在 `codex-rs/sandboxing/src/manager.rs:23`：

```rust
pub enum SandboxType {
    None,
    MacosSeatbelt,       // macOS 专用
    LinuxSeccomp,        // Linux 专用（实际同时含 bubblewrap）
    WindowsRestrictedToken, // Windows 专用
}
```

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
- `codex-rs/linux-sandbox/src/linux_run_main.rs` — 沙箱 helper 入口
- `codex-rs/linux-sandbox/src/bwrap.rs` — Bubblewrap 集成
- `codex-rs/sandboxing/src/landlock.rs` — Landlock/Seccomp 集成

#### 第一层：Bubblewrap 文件系统命名空间隔离

Bubblewrap（`bwrap`）是一个无需 root 权限的沙箱工具，通过 Linux 用户命名空间创建隔离的文件系统视图：

- 以只读绑定挂载（`--ro-bind`）暴露系统目录
- 以可写绑定挂载（`--bind`）暴露工作区目录
- 隔离 PID 命名空间（`--unshare-pid`）
- 隔离网络命名空间（`--unshare-net`，网络禁用时）
- 挂载全新的 `/proc`（`--proc /proc`）
- 在容器外看不到宿主机的其他目录

#### 第二层：Seccomp 系统调用过滤

在 Bubblewrap 建立文件系统视图后，再应用 seccomp 规则过滤危险系统调用：

**永远拒绝的系统调用（所有模式）：**
- `ptrace` — 进程监控/注入
- `process_vm_readv`, `process_vm_writev` — 跨进程内存读写
- `io_uring_setup`, `io_uring_enter`, `io_uring_register` — 异步 I/O 环（可绕过沙箱）

**网络限制模式下额外拒绝：**
- `connect`, `accept`, `accept4` — 网络连接
- `bind`, `listen` — 服务器套接字
- `sendto`, `sendmmsg` — 套接字发送

**权限提升防护：**
```
PR_SET_NO_NEW_PRIVS  // 防止子进程通过 setuid 提升权限
```

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

Landlock（旧版备用）：当系统不支持 bubblewrap 或显式指定 `--use-legacy-landlock` 时，使用 Linux 5.13+ 的 Landlock LSM 直接在进程内应用文件系统访问控制。

---

### 3.3 Windows：受限令牌沙箱

Windows 使用安全令牌机制（Restricted Token）降低子进程权限：

**两种级别（`windows_sandbox_mode`）：**
- `Elevated`：完整沙箱设置，需要管理员权限提升
- `RestrictedToken`：仅降低令牌权限，不需要提升权限

**关键文件：** `codex-rs/core/src/windows_sandbox.rs`

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
SandboxManager.select_initial()          ← 决定沙箱类型
    ↓
SandboxManager.transform()               ← 将命令包装进沙箱
    ↓
[macOS]  sandbox-exec -p <policy> -- <cmd>
[Linux]  codex-linux-sandbox --permission-profile <json> -- <cmd>
[Win]    CreateProcess with restricted token
    ↓
子进程在沙箱内执行
```

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

沙箱激活时会在子进程环境中注入标记变量，让应用代码感知自身运行在沙箱中：

```rust
// macOS 沙箱激活时
env.insert("CODEX_SANDBOX", "seatbelt");

// 网络被禁用时
env.insert("CODEX_SANDBOX_NETWORK_DISABLED", "1");
```

定义位置：`codex-rs/core/src/sandboxing/mod.rs:133-141`

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

沙箱特别保护 Codex 自身的配置目录（`.codex`）——即使子进程获得了工作区写权限，也无法修改 Codex 的配置文件。这防止了恶意代码通过修改 Codex 配置来持久化攻击。

---

## 7. 代码位置速查

| 功能 | 文件 | 关键行 |
|------|------|--------|
| 沙箱类型定义 | `codex-rs/sandboxing/src/manager.rs` | 23-28 |
| 沙箱类型选择 | `codex-rs/sandboxing/src/manager.rs` | 139-166 |
| 命令包装转换 | `codex-rs/sandboxing/src/manager.rs` | 168-261 |
| 是否启用沙箱决策 | `codex-rs/sandboxing/src/policy_transforms.rs` | 509-529 |
| macOS 策略生成 | `codex-rs/sandboxing/src/seatbelt.rs` | 603-743 |
| macOS 基础策略 | `codex-rs/sandboxing/src/seatbelt_base_policy.sbpl` | 全文 |
| Linux sandbox 入口 | `codex-rs/linux-sandbox/src/linux_run_main.rs` | 147+ |
| Seccomp/Landlock 配置 | `codex-rs/sandboxing/src/landlock.rs` | 全文 |
| 核心适配层 | `codex-rs/core/src/sandboxing/mod.rs` | 全文 |
| 环境变量注入 | `codex-rs/core/src/sandboxing/mod.rs` | 133-141 |

---

## 8. 沙箱调试工具

Codex 提供内置调试命令用于测试沙箱行为：

```bash
# 调试 macOS seatbelt 策略
codex debug-seatbelt --sandbox workspace-write -- ls /

# 调试 Linux landlock 沙箱
codex debug-landlock -- cat /etc/passwd

# 调试 Windows 沙箱
codex debug-windows-sandbox -- cmd /c dir
```

调试实现：`codex-rs/cli/src/debug_sandbox.rs`

---

## 9. 总结

Codex 沙箱机制的核心设计哲学是：

> **最小权限原则**（Principle of Least Privilege）——AI 执行的每个命令，只获得完成任务所需的最小权限，而非用户账户的完整权限。

通过平台原生沙箱技术（macOS Seatbelt、Linux Bubblewrap+Seccomp、Windows Restricted Token），Codex 在不损失性能的前提下，为用户提供了一道防止 AI 意外或恶意操作造成系统损害的安全屏障。
