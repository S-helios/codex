# 网络代理：受沙箱约束的出网管控

> 一句话主旨：Codex 在子进程与外网之间塞进一个**本地 HTTP/SOCKS5 代理**，用统一的「域名白名单 + 协议方法 + 运行时审批」三道闸门管控所有出网流量，并把代理地址通过环境变量注入被沙箱包裹的命令，让网络管控与文件系统沙箱（见 doc13）形成互补的双层防线。

本篇聚焦 `codex-rs/network-proxy` 这个 crate 以及它在 `core` 里的接线方式。它回答四个问题：代理凭什么能拦住流量、一次请求如何被判定放行/拒绝/询问、会话怎样随权限切换动态重建代理、以及为什么 HTTPS 需要「中间人」（MITM）。

---

## 目录

1. [架构总览：代理为什么是出网管控的咽喉](#1-架构总览代理为什么是出网管控的咽喉)
2. [核心组件清单](#2-核心组件清单)
3. [网络策略判定流程](#3-网络策略判定流程)
4. [会话级代理管理](#4-会话级代理管理)
5. [流量拦截机制（HTTP / CONNECT / SOCKS5 / MITM）](#5-流量拦截机制http--connect--socks5--mitm)
6. [与沙箱的整合](#6-与沙箱的整合)
7. [关键设计模式](#7-关键设计模式)
8. [责任范围之外：responses-api-proxy](#8-责任范围之外responses-api-proxy)
9. [常见问题](#9-常见问题)

---

## 1. 架构总览：代理为什么是出网管控的咽喉

文件系统沙箱（doc13）能拦住「写哪里」，但对「连哪个域名」无能为力——`sandbox-exec` 的 seatbelt 规则只能粗粒度地放行/禁止整段 `network-outbound`，没法说「只允许访问 `github.com`、禁止 `evil.com`」。Codex 的解法是：**把网络出口收敛到一个本地代理进程**，再用环境变量（`HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` 等）逼迫子进程里的 `curl`/`npm`/`git` 全部走这个代理，于是代理就成了唯一的咽喉，可以在应用层做精细判定。

```
                      ┌─────────────────────────────────────────────┐
                      │           Codex Session 进程                 │
                      │                                              │
                      │   ┌──────────────────────────────────┐      │
                      │   │  NetworkProxy（本地监听）          │      │
                      │   │   HTTP listener  :随机端口          │      │
                      │   │   SOCKS5 listener:随机端口          │      │
                      │   │   策略引擎 evaluate_host_policy()   │      │
                      │   └──────────────▲───────────────────┘      │
                      │                  │ HTTP_PROXY/ALL_PROXY 注入  │
                      │   ┌──────────────┴───────────────────┐      │
                      │   │  被沙箱包裹的子进程（curl/npm…）   │      │
                      │   └────────────────────────────────────┘      │
                      └──────────────────┼───────────────────────────┘
                                         │ 放行后转发
                                         ▼
                                    外部网络（github.com …）
```

整套机制的边界划分：

| 层次 | 关心什么 | 实现位置 |
|------|---------|---------|
| 文件系统沙箱 | 子进程能读写哪些路径 | `codex-rs/sandboxing/`（doc13） |
| **网络代理** | **子进程能连哪些域名 / 用什么方法 / 是否需要审批** | **`codex-rs/network-proxy/`（本篇）** |
| 环境变量注入 | 怎么把子进程「骗」进代理 | `network-proxy/src/proxy.rs` 的 `apply_proxy_env_overrides()` |
| 会话接线 | 权限切换时如何重建代理 | `core/src/session/mod.rs`、`turn_context.rs` |

> 关键取舍：代理是**进程内的本地服务**，不是独立守护进程。它随 `Session` 生命周期启停，监听本机随机端口（loopback），所以即使被泄露也无法被外部网络利用。

---

## 2. 核心组件清单

`network-proxy` crate 的对外门面在 `network-proxy/src/lib.rs:19-69`。下面只列与出网管控直接相关的导出项（实际导出更多，含 MITM hook 配置等）：

| 导出项 | 定义位置 | 职责 |
|--------|---------|------|
| `NetworkPolicyDecider`（trait） | `network_policy.rs:266-269` | 运行时审批回调，`async fn decide()` 决定一次请求 Allow/Deny/Ask |
| `NetworkDecision`（enum） | `network_policy.rs:121-129` | 判定结果：`Allow` 或 `Deny { reason, source, decision }` |
| `NetworkPolicyDecision`（enum） | `network_policy.rs:41-46` | 拒绝的「子类型」：`Deny`（硬拒）/ `Ask`（待审批） |
| `NetworkDecisionSource`（enum） | `network_policy.rs:57-64` | 判定来源：`BaselinePolicy` / `ModeGuard` / `ProxyState` / `Decider` |
| `NetworkProxyConfig` | `config.rs` | 代理配置：白/黑名单域名、`mode`、`mitm`、`allow_upstream_proxy` 等 |
| `NetworkProxyState` | `runtime.rs`（导出于 `lib.rs:62`） | 代理运行时状态：编译后的域名 globset、MITM 状态、审计元数据、阻断记录 |
| `NetworkProxy` / `NetworkProxyBuilder` | `proxy.rs`（`lib.rs:47-48`） | 代理实例与构建器，`build().run()` 启动监听 |
| `BlockedRequest` / `BlockedRequestObserver` | `runtime.rs:90-104` / 导出于 `lib.rs:57-59` | 阻断事件的数据结构与观察者回调 |
| `NetworkMode`（enum） | `config.rs:276-285` | `Limited`（仅 GET/HEAD/OPTIONS）/ `Full`（全方法） |
| `NetworkProxyAuditMetadata` | `runtime.rs:48-58` | 审计事件携带的会话上下文（conversation_id、user_email…） |

> 注意：大纲提到的导出名与真实源码基本一致，但 `lib.rs:32-38` 实际同时导出了 `NetworkDecision`、`NetworkDecisionSource`、`NetworkPolicyRequest` 等更多类型——`NetworkPolicyDecision` 只是其中一员。

core 侧的接线组件：

| 组件 | 定义位置 | 职责 |
|------|---------|------|
| `build_network_proxy_state()` | `core/src/network_proxy_loader.rs:42-45` | 从配置分层（system/user/project）加载并构建 `NetworkProxyState` |
| `NetworkProxySpec` | `core/src/config/network_proxy_spec.rs:23-30` | 「可重算的代理蓝图」，封装 base_config / requirements / constraints |
| `network_approval_context_from_payload()` | `core/src/network_policy_decision.rs:26-44` | 从待审批 payload 抽取审批上下文 |
| `execpolicy_network_rule_amendment()` | `core/src/network_policy_decision.rs:74-102` | 把用户审批映射为 exec policy 网络规则修订 |

---

## 3. 网络策略判定流程

### 3.1 三类来源、两种结果

每次请求最终落到 `evaluate_host_policy()`（`network-proxy/src/network_policy.rs:289-359`）。它的核心逻辑只有三步：

```rust
// network_policy.rs:294-319（节选）
let host_decision = state.host_blocked(&request.host, request.port).await?;
let (decision, policy_override) = match host_decision {
    HostBlockDecision::Allowed => (NetworkDecision::Allow, false),       // ① 配置直接放行
    HostBlockDecision::Blocked(HostBlockReason::NotAllowed) => {
        if let Some(decider) = decider {                                  // ② 不在白名单 → 问审批
            let decider_decision = map_decider_decision(decider.decide(request.clone()).await);
            let policy_override = matches!(decider_decision, NetworkDecision::Allow);
            (decider_decision, policy_override)
        } else {
            (NetworkDecision::deny_with_source(/* not_allowed */ …, BaselinePolicy), false)
        }
    }
    HostBlockDecision::Blocked(reason) => (                               // ③ 显式拒绝 / 本地地址
        NetworkDecision::deny_with_source(reason.as_str(), BaselinePolicy), false),
};
```

判定来源（`NetworkDecisionSource`，`network_policy.rs:57-64`）是这套流程最值得理解的设计：

| 来源 | 含义 | 谁能用它放行？ |
|------|------|--------------|
| `BaselinePolicy` | 配置层判定（域名白/黑名单、本地地址限制） | 白名单命中时放行 |
| `ModeGuard` | `NetworkMode` 方法限制 / MITM 缺失 | 不放行，只拒绝 |
| `ProxyState` | 代理整体被禁用等状态层拒绝 | 不放行，只拒绝 |
| `Decider` | 运行时审批回调 | **只有它能 override `not_allowed` 域名** |

关键点：**只有 `Decider` 能把一个「不在白名单」的域名翻盘成放行**（`policy_override = true`，见 `network_policy.rs:300`）。`HostBlockReason::Denied`（显式黑名单）和 `NotAllowedLocal`（本地/私网地址）走的是第 ③ 分支，**直接 `BaselinePolicy` 拒绝，连 decider 都不问**——这是「显式拒绝不可被审批翻盘」的设计。

`HostBlockReason` 的三个变体见 `runtime.rs:60-65`：`Denied` / `NotAllowed` / `NotAllowedLocal`，分别对应原因字符串 `denied` / `not_allowed` / `not_allowed_local`（core 侧 `network_policy_decision.rs:60-67` 把它们翻译成给用户看的中文/英文说明）。

### 3.2 Decider trait：审批的抽象

```rust
// network_policy.rs:266-269
#[async_trait]
pub trait NetworkPolicyDecider: Send + Sync + 'static {
    async fn decide(&self, req: NetworkPolicyRequest) -> NetworkDecision;
}
```

巧妙之处在于 `network_policy.rs:278-287` 给**任意闭包** `Fn(NetworkPolicyRequest) -> Future<NetworkDecision>` 实现了这个 trait，所以测试和默认实现都能用闭包直接当 decider（例如 `network_proxy_spec.rs:138` 的默认 decider 就是 `|_request| async { NetworkDecision::ask("not_allowed") }`）。

### 3.3 审计事件

无论放行还是拒绝，`evaluate_host_policy()` 都会调用 `emit_policy_audit_event()`（`network_policy.rs:342-356`）发出结构化 tracing 事件，target 为 `codex_otel.network_proxy`、事件名 `codex.network_proxy.policy_decision`。两个便捷入口在 `network_policy.rs:179-191`：`emit_block_decision_audit_event()` / `emit_allow_decision_audit_event()`（它们走 `non_domain` scope，用于非域名维度的拦截，如方法限制）。

事件字段（`network_policy.rs:228-255`）包含：

- `network.policy.scope`：`domain`（域名判定）/ `non_domain`（方法/MITM 等非域名判定）
- `network.policy.decision`：`allow` / `deny`
- `network.policy.source`：上面四种来源
- `network.policy.reason`、`network.transport.protocol`、`server.address`、`server.port`、`http.request.method`、`client.address`、`network.policy.override`
- 一批会话上下文：`conversation.id`、`user.email`、`model`、`auth_mode` 等，来自 `state.audit_metadata()`

---

## 4. 会话级代理管理

### 4.1 NetworkProxySpec：可重算的代理蓝图

直接持有一个跑起来的代理还不够——用户可能在一次对话中切换权限档（permission profile），此时白名单约束会变。Codex 用 `NetworkProxySpec`（`core/src/config/network_proxy_spec.rs:23-30`）把「配置 + 约束」打包成一个**可重算**的蓝图：

```rust
// network_proxy_spec.rs:23-30
pub struct NetworkProxySpec {
    base_config: NetworkProxyConfig,              // 原始配置（重算的输入）
    requirements: Option<NetworkConstraints>,     // 受管约束（来自可信配置层）
    config: NetworkProxyConfig,                    // 应用约束后的有效配置
    constraints: NetworkProxyConstraints,          // 编译后的约束
    hard_deny_allowlist_misses: bool,              // 白名单外是否硬拒
}
```

它提供两条核心方法：

- `recompute_for_permission_profile(profile)`（`:154-163`）：用新权限档从 `base_config` + `requirements` 重新算出 `config`/`constraints`。
- `start_proxy(...)`（`:123-152`）：真正启动代理。

`start_proxy()` 里有段关键逻辑（`:133-140`）——只有当 `enable_network_approval_flow && !self.hard_deny_allowlist_misses` 时才注入 decider：用户提供的优先，否则在「受管沙箱激活」时挂一个默认的 `ask("not_allowed")` decider。换句话说：**只要开启了硬拒模式（`hard_deny_allowlist_misses == true`），就根本不挂 decider，白名单外的域名一律硬拒、不走审批**。

`hard_deny_allowlist_misses` 由 `requirements.managed_allowed_domains_only` 决定（`:95-97`、`:328-330`）。

> 注意一个命名陷阱：`managed_sandbox_active()`（`:336-338`）判定的是 `PermissionProfile::Managed { .. }`，**白名单/黑名单的「扩展」（用户追加域名）只在 `Managed` 档下生效**；而后面会讲的「代理是否激活」用的是另一个判据（`!= Disabled`）。二者不是同一回事。

### 4.2 PermissionProfile 与「代理是否激活」

`PermissionProfile`（`protocol/src/models.rs:313-327`）只有三个变体：

```rust
pub enum PermissionProfile {
    Managed { file_system, network },   // Codex 自建沙箱
    Disabled,                           // 不套外层沙箱 = full access
    External { network },               // 文件隔离由外部调用方负责
}
```

`Session::managed_network_proxy_active_for_permission_profile()`（`core/src/session/mod.rs:885-889`）：

```rust
fn managed_network_proxy_active_for_permission_profile(profile: &PermissionProfile) -> bool {
    !matches!(profile, PermissionProfile::Disabled)
}
```

即：**`Disabled`（full access）时代理不激活**——既然用户已经放开了一切，代理也没必要拦。`Managed` 和 `External` 都激活代理。

### 4.3 启动：start_managed_network_proxy()

`core/src/session/mod.rs:919-955`。它做三件事：

1. 调 `spec.with_exec_policy_network_rules(exec_policy)` 把 exec policy 里的网络规则（如某条 `curl github.com` 的批准）合并进代理的允许域名（失败则 warn 并退回原 spec）。
2. 调 `spec.start_proxy(...)` 启动代理，传入 decider、blocked observer、审批开关、审计元数据。
3. 读出代理实际监听的 `http_addr` / `socks_addr`，封装成 `SessionNetworkProxyRuntime { http_addr, socks_addr }` 返回。

`core/src/session/tests.rs:702-734`（`start_managed_network_proxy_applies_execpolicy_network_rules`）正是验证第 1 步：往 exec policy 加一条 `example.com / Https / Allow`，启动后断言 `current_cfg.network.allowed_domains() == Some(vec!["example.com"])`。

### 4.4 刷新：随权限切换重建

当一次 turn 的权限档变了，`turn_context.rs:649-651` 会触发刷新：

```rust
// turn_context.rs:649-652
if permission_profile_changed {
    self.refresh_managed_network_proxy_for_current_permission_profile().await;
}
```

`refresh_managed_network_proxy_for_current_permission_profile()`（`session/mod.rs:957-1027`）的要点：

- **串行化**：开头 `self.managed_network_proxy_refresh_lock.acquire().await`（`:958`）拿信号量，防止同一 turn 内并发重建互相覆盖（`apply()` 互踩）。
- **重算**：`spec.recompute_for_permission_profile(...)`（`:977-978`）按新权限档重算，再叠加当前 exec policy 网络规则（`:987`）。
- **就地更新 vs 新建**：若代理已在跑（`:996`），调 `spec.apply_to_started_proxy()` 热替换配置状态；否则走 `start_managed_network_proxy()` 新建一个（`:1003`）。

`apply_to_started_proxy()`（`network_proxy_spec.rs:180-192`）内部调用 `proxy().replace_config_state(state)`——**热替换不重启监听端口**，所以已经建立的连接和已注入的环境变量不受影响。

### 4.5 turn_context 何时把代理交给执行环境

`turn_context.rs:730-739` 决定一个 turn 的命令执行环境是否拿到代理：

```rust
self.services.network_proxy.load_full().as_ref()
    .and_then(|started_proxy| {
        Self::managed_network_proxy_active_for_permission_profile(
            &session_configuration.permission_profile(),
        ).then(|| started_proxy.proxy())
    }),
```

两个条件**同时满足**才传：① 代理已经在跑（`load_full()` 非空）；② 当前权限档允许代理激活（非 `Disabled`）。这解释了一个微妙现象：代理可能在跑（之前在 `Managed` 档启动过），但当前 turn 切到 `Disabled`，于是这一 turn 的命令拿不到代理——直连外网。

### 4.6 decider 在权限重算中存活

`session/tests.rs:782-847`（`managed_network_proxy_decider_survives_full_access_start`）测了一个反直觉的场景：以 `PermissionProfile::Disabled`（注释直呼 "full access"）启动、挂上 decider，然后 `recompute_for_permission_profile(workspace_write)` 重算并 `apply_to_started_proxy()`。最终一个不在白名单的请求被 **403 Forbidden** 拦下、响应头带 `x-proxy-error: blocked-by-allowlist`，且 decider 恰好被调用 1 次（`:841-845`）。这验证了：**decider 是挂在 proxy 实例上的，重算配置状态（`replace_config_state`）不会把它弄丢**。

---

## 5. 流量拦截机制（HTTP / CONNECT / SOCKS5 / MITM）

### 5.1 HTTP 代理监听与分流

`run_http_proxy()`（`network-proxy/src/http_proxy.rs:86-103`）绑定监听后，把请求按类型分流（`:130-146`）：

```rust
// http_proxy.rs:130-146（节选，用 rama 的 Layer 组装）
let http_service = HttpServer::http1().service((
    UpgradeLayer::new(
        MethodMatcher::CONNECT,
        service_fn(move |req| http_connect_accept(policy_decider.clone(), req)),  // CONNECT 接受阶段
        service_fn(http_connect_proxy),                                            // CONNECT 升级后
    ),
    RemoveResponseHeaderLayer::hop_by_hop(),
).into_layer(service_fn(move |req| http_plain_proxy(policy_decider.clone(), req))));  // 普通 HTTP
```

- **普通 HTTP**（GET/POST/…）→ `http_plain_proxy()`（`:479`）。
- **CONNECT**（HTTPS 隧道握手）→ 先 `http_connect_accept()`（`:156`）判定，再 `http_connect_proxy()`（`:331`）处理升级后的流。

### 5.2 普通 HTTP：http_plain_proxy()

`http_plain_proxy()`（`http_proxy.rs:479` 起）的判定顺序：

1. 取 `NetworkProxyState`，读 `method_allowed()`（`:491-498`）——`NetworkMode::Limited` 下只放行 GET/HEAD/OPTIONS。
2. `x-unix-socket` 头是访问本地守护进程的逃生通道（`:503` 起），默认 macOS-only + 显式白名单，避免代理沦为本地权限提升手段。
3. 走 `evaluate_host_policy()` 判定域名（§3）。
4. 放行 → 建上游连接转发；拒绝/待审批 → 返回 **403** + `x-proxy-error` 头（如 §4.6 测试看到的 `blocked-by-allowlist`）。

### 5.3 CONNECT 与 MITM：为什么 HTTPS 特殊

HTTPS 的内容被 TLS 加密，代理默认看不到 URL 路径、方法、Host 之外的细节。于是 Codex 的策略是：

- **Full 模式 + 无 hook**：HTTPS CONNECT **直接隧道转发**（`NetworkMode::Full` 的 doc 注释见 `config.rs:281-282`），代理只在 CONNECT 阶段按域名判定一次，之后不拆包。
- **Limited 模式 或 命中 MITM hook**：必须拆包（MITM），否则无法在加密流内部强制「只允许 GET/HEAD/OPTIONS」或执行 hook。

`http_connect_accept()`（`:269`）算出 `connect_needs_mitm`：

```rust
// http_proxy.rs:269
let connect_needs_mitm = mode == NetworkMode::Limited || host_has_mitm_hooks;
```

如果 `connect_needs_mitm` 但 `mitm_state.is_none()`（`:271`），CONNECT 被**拒绝**，来源 `ModeGuard`、原因 `REASON_MITM_REQUIRED`（`:274-311`）——即「我需要拆包才能执行策略，但拆包能力没开，那就不放行」。

MITM 能力本身在 `MitmState::new()`（`network-proxy/src/mitm.rs:99-121`）里准备：加载/生成本地 CA（`ManagedMitmCa::load_or_create()`，`:107`），并按 `allow_upstream_proxy` 选择上游连接器（`:109-113`）——开启则继承环境里的上游代理，否则直连。真正的拆包隧道是 `mitm_tunnel()`（`mitm.rs:137`），用 `tls_acceptor_data_for_host(host)`（`:123-125`）给每个目标主机现签一张叶子证书来终止 TLS。

**MITM 状态何时被构建**：`build_config_state()`（`network-proxy/src/state.rs:76-83`）里——只有 `config.network.mitm == true` 才创建 `MitmState`（含 CA），否则为 `None`。所以「是否具备 MITM 能力」是配置决定的；「这次 CONNECT 是否真用 MITM」是运行时 `connect_needs_mitm` 决定的。两者缺一不可。

```
CONNECT 到达
  │
  ├─ enabled? 否 → 拒绝（proxy_disabled）
  ├─ evaluate_host_policy() → Deny? → 403 + 审计
  │
  ├─ connect_needs_mitm = (mode==Limited) || host_has_mitm_hooks
  │     ├─ 需要 MITM 但 mitm_state == None → 拒绝（ModeGuard / mitm_required）
  │     └─ 需要 MITM 且有能力 → mitm_tunnel()：现签证书、终止 TLS、对内层请求再判定
  └─ 不需要 MITM（Full + 无 hook）→ 直接隧道转发
```

### 5.4 SOCKS5

SOCKS5 监听在 `network-proxy/src/socks5.rs`，同样复用 `evaluate_host_policy()`（`socks5.rs:12` 引入）。`NetworkMode::Limited` 下 SOCKS5 整体被阻断（`config.rs:279` 注释明确「SOCKS5 remains blocked in limited mode」，对应 `socks5.rs:200` 的 `NetworkMode::Limited` 分支）。

### 5.5 本地/私网地址兜底

`connect_policy.rs` 的 `TargetCheckedTcpConnector`（`:19-37`）在建立 TCP 连接前再查一道：若 `!allow_local_binding && is_non_public_ip(addr.ip())`（`:70`），直接 `PermissionDenied`。这是防止「域名解析到 127.0.0.1 / 内网 IP」绕过白名单的兜底。

---

## 6. 与沙箱的整合

### 6.1 环境变量注入：把子进程「骗」进代理

代理能拦流量的前提是子进程愿意走代理。`apply_proxy_env_overrides()`（`network-proxy/src/proxy.rs:474` 起）往子进程环境里塞一大批代理变量：

| 变量 | 值 | 作用 |
|------|----|----|
| `CODEX_NETWORK_PROXY_ACTIVE` | `"1"` | 标记代理激活（`PROXY_ACTIVE_ENV_KEY`，`proxy.rs:366` / 设值 `:483`） |
| `CODEX_NETWORK_ALLOW_LOCAL_BINDING` | `"0"`/`"1"` | 是否允许本地绑定（`:367` / `:484-491`） |
| `HTTP_PROXY` / `HTTPS_PROXY`（含小写、YARN/NPM/BUNDLE/PIP/DOCKER 变体） | `http://{http_addr}` | 逼 HTTP 客户端走代理（`:494-516`） |
| `WS_PROXY` / `WSS_PROXY` 等 | `http://{http_addr}` | WebSocket 客户端（`:519`） |
| `NO_PROXY`（含 6 个变体，`NO_PROXY_ENV_KEYS` `:416-423`） | `DEFAULT_NO_PROXY_VALUE` | loopback / IP 字面量直连，绕开代理（`:524`） |
| `ELECTRON_GET_USE_PROXY` / `NODE_USE_ENV_PROXY` | `"true"` / `"1"` | Electron / Node 内置 HTTP 客户端才认环境代理（`:526-531`） |
| `ALL_PROXY` / `FTP_PROXY` | `socks5h://{socks_addr}` | **仅当 socks_enabled 时**才设为 SOCKS（`:536-538`） |

> 设计细节（`proxy.rs:533-535` 注释）：`HTTP_PROXY`/`HTTPS_PROXY` 坚持给 HTTP 端点，**不能**塞 SOCKS URL（很多客户端会因此崩），只有 `ALL_PROXY` 切到 SOCKS。`NO_PROXY` 只放 loopback/IP 字面量，**不放主机名后缀**——否则客户端会本地解析内部域名而不让代理来解析。

macOS 上还有一招（`proxy.rs:463-472`）：通过 `GIT_SSH_COMMAND` 注入一个走 SOCKS 的 ssh 命令（带 `CODEX_PROXY_GIT_SSH_COMMAND_MARKER` 标记），让 `git clone git@…` 这类 SSH 流量也能被代理覆盖，且重复注入时能识别并刷新自己之前注入的那条。

### 6.2 与文件系统沙箱的协作

文件系统沙箱（doc13）放行了代理监听的 loopback 端口（doc13 §3.1「仅代理模式」），子进程才能连上本地代理。两层的分工：seatbelt/landlock 决定「能不能发起到 localhost:代理端口 的连接」，代理决定「这个连接最终能不能到达 github.com」。

### 6.3 受信配置层约束

`build_network_proxy_state()`（`network_proxy_loader.rs:42-89`）从配置分层加载，并通过 `enforce_trusted_constraints()`（`:113-122`）+ `network_constraints_from_trusted_layers()`（`:124-145`）施加**只来自受信层**（system / 非用户控制层，`:133` 跳过 `is_user_controlled_layer`）的约束，再用 `validate_policy_against_constraints()` 校验配置不违反约束。这保证企业/管理员下发的硬约束不会被用户配置覆盖。

exec policy 网络规则的合并顺序（`network_proxy_spec.rs:165-178` 的 `with_exec_policy_network_rules`）：**先有约束校验，exec policy 规则在 `apply_requirements` 之后再叠加并重新校验**——所以 exec policy 规则不能突破受管硬约束（管理员的 `managed_allowed_domains_only` 仍然封顶）。

---

## 7. 关键设计模式

1. **判定与执行分离 + 来源标记**。`evaluate_host_policy()` 只产出 `NetworkDecision { reason, source, decision }`，把「谁做的判定」编码进 `source`。这让审计可观测、也让「只有 `Decider` 能 override」成为类型层面的不变量（`network_policy.rs:321-340`）。

2. **闭包即 trait**。`NetworkPolicyDecider` 给闭包做了 blanket impl（`network_policy.rs:278-287`），默认 decider 和测试 decider 都用一行闭包搞定，省去样板 struct。

3. **可重算蓝图（Spec）**。`NetworkProxySpec` 把「输入（base_config + requirements）」和「派生结果（config + constraints）」分开存（`network_proxy_spec.rs:23-30`），权限切换时从输入重算，而非在派生结果上打补丁——避免状态漂移。对比 doc11 的 `TurnContext`「每 turn 重建」思路一脉相承。

4. **热替换而非重启**。`apply_to_started_proxy()` → `replace_config_state()` 在不重启监听的前提下换策略（`network_proxy_spec.rs:180-192`），保住端口和已注入的环境变量。

5. **信号量串行化重建**。`managed_network_proxy_refresh_lock`（`session/mod.rs:958`）确保权限连续切换时重建不会并发互踩。

6. **多客户端环境变量矩阵**。`apply_proxy_env_overrides()` 不只设标准的 `HTTP_PROXY`，而是覆盖 npm/yarn/bundle/pip/docker/electron/node/git-ssh 一整套各家私有变量（`proxy.rs:494-538`），因为现实中各工具读的变量五花八门。

---

## 8. 责任范围之外：responses-api-proxy

`codex-rs/responses-api-proxy` 是**另一个独立的小代理**，容易和 `network-proxy` 混淆，这里澄清：它**不是**出网管控，而是给 OpenAI Responses API 做的轻量转发器。

`responses-api-proxy/src/lib.rs:73-136` 的 `run_main()`：

- 从 stdin 读取 auth header（`:74`，保密考虑，不走命令行参数）。
- 绑定 loopback 临时端口（`bind_listener()` `:138-143`，`port.unwrap_or(0)`）。
- 只放行 **`POST /v1/responses`**（精确匹配、无 query string，`forward_request()` `:163`、判定 `:170-173`），转发到 `--upstream-url`（默认 `https://api.openai.com/v1/responses`），并**替换 Authorization 头**（`:215-219`，且 `set_sensitive(true)` 避免泄露到日志）。
- 可选把请求/响应导出到 JSON 文件（`--dump-dir`，`:89-94`）。

它的典型用途是在受沙箱的环境里给 API 交互做一个固定出口，与本篇主角 `network-proxy` 的「通用出网管控」是两套东西。

---

## 9. 常见问题

**Q1：为什么 full access（`Disabled`）下代理直接不激活？**
A：`managed_network_proxy_active_for_permission_profile()` 对 `Disabled` 返回 false（`session/mod.rs:885-889`）。既然用户已放开一切文件/网络权限，再拦网络既无意义又徒增延迟。`Managed` / `External` 档才激活。注意「激活与否」（`!= Disabled`）和「白/黑名单是否可被用户扩展」（`Managed`，`network_proxy_spec.rs:336-338`）是两个不同判据。

**Q2：MITM 是不是默认开？会拆我所有 HTTPS 吗？**
A：不会。MITM 能力只在 `config.network.mitm == true` 时构建（`state.rs:76-83`）；即便构建了，也只在 `NetworkMode::Limited` 或命中 host MITM hook 时才真正拆包（`http_proxy.rs:269`）。`Full` 模式 + 无 hook 的 HTTPS 是直接隧道转发，代理看不到明文。

**Q3：一个不在白名单的域名，能被审批放行吗？**
A：取决于来源。`not_allowed`（不在白名单）→ 若挂了 decider 且未开硬拒模式，会走审批（Ask），用户批准则 `Decider` override 放行（`network_policy.rs:297-300`）。但 `denied`（显式黑名单）和 `not_allowed_local`（本地/私网地址）走 `BaselinePolicy` 直接拒绝，**审批也救不回来**（`network_policy.rs:312-318`）。

**Q4：`hard_deny_allowlist_misses` 是什么开关？**
A：当受管约束的 `managed_allowed_domains_only == true` 时为真（`network_proxy_spec.rs:95-97`）。此时 `start_proxy()` 根本不挂 decider（`:133`），白名单外的域名一律硬拒、不弹审批。用于管理员要求「只准这几个域名、不许临时放行」的强约束场景。

**Q5：权限在一次对话中途切换，会不会出现两份代理打架？**
A：不会。刷新逻辑被 `managed_network_proxy_refresh_lock` 信号量串行化（`session/mod.rs:958`）；且优先「热替换」已跑的代理（`replace_config_state`）而非新建，避免端口和环境变量错乱。

**Q6：审计事件能看到 conversation_id、user_email 吗？**
A：能，但有前提。这些来自 `NetworkProxyAuditMetadata`（`runtime.rs:48-58`），在代理**创建时**写入并冻结。如果代理在会话上下文尚未填齐前就启动，这些字段可能为空——元数据不会实时回填。[推测] 这也是为什么 `start_managed_network_proxy()` 显式把 `audit_metadata` 作为参数透传（`session/mod.rs:926`、`:1014`）。

**Q7：`ModeGuard` 和 `BaselinePolicy` 在审计里怎么区分？**
A：`BaselinePolicy` 表示域名/约束维度的判定（白黑名单、本地地址）；`ModeGuard` 表示 `NetworkMode` 维度的拦截（Limited 模式方法限制、MITM 缺失，见 `http_proxy.rs:277` / `:540`）；`ProxyState` 表示代理整体被禁用；`Decider` 仅在运行时审批 override 时出现。四者枚举见 `network_policy.rs:57-64`。

---

## 交叉引用

- 文件系统沙箱与 `--sandbox` 策略：**doc13**（`13_sandbox_mechanism.md`）。
- 会话 / Turn 生命周期、`new_turn()` 与权限档切换：**doc11**（`11_thread_session_turn_lifecycle.md`）。
- exec 命令执行与安全闸门、审批协议：**doc18**（`18_exec_and_safety.md`）。
- 配置分层（system/user/project）加载机制：**doc21**（`21_config_system.md`）。
