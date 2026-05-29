# 认证与登录（Authentication & Login）

> 一句话主旨：`codex-login` crate 把"用什么身份访问 OpenAI/ChatGPT 后端"这件事收敛成一个 `CodexAuth` 枚举与一个进程内唯一的 `AuthManager`，统一处理四种认证模式（API Key、托管 ChatGPT OAuth、外部 ChatGPT 令牌、Agent Identity）的加载、缓存、主动刷新、401 恢复与注销。

本文聚焦 `codex-rs/login/` 这个 crate（认证逻辑的本体），以及 `codex-rs/core/src/installation_id.rs`（安装 ID）。HTTP 客户端如何携带认证头进入请求循环，见 `doc11`（生命周期）与 core 的 `client.rs`；协议层 `Op` 见 `doc11 §10`。

---

## 目录

1. [总览：认证在架构中的位置](#1-总览认证在架构中的位置)
2. [`CodexAuth`：四种认证模式](#2-codexauth四种认证模式)
3. [OAuth 登录流程（浏览器回环 + 设备代码）](#3-oauth-登录流程浏览器回环--设备代码)
4. [API Key 方式](#4-api-key-方式)
5. [Agent Identity 认证](#5-agent-identity-认证)
6. [令牌存储与管理（`auth.json` 与四种后端）](#6-令牌存储与管理authjson-与四种后端)
7. [`AuthManager` 核心架构](#7-authmanager-核心架构)
8. [刷新机制（主动刷新 + 失败分类）](#8-刷新机制主动刷新--失败分类)
9. [401 Unauthorized 恢复状态机](#9-401-unauthorized-恢复状态机)
10. [外部认证集成（`ExternalAuth` trait）](#10-外部认证集成externalauth-trait)
11. [安装 ID](#11-安装-id)
12. [注销与令牌撤销](#12-注销与令牌撤销)
13. [登录约束强制检查](#13-登录约束强制检查)
14. [常见问题](#14-常见问题)

---

## 1. 总览：认证在架构中的位置

Codex 需要访问 OpenAI 的两类后端：

- **API 后端**（`api.openai.com`）：传统 API Key 路径。
- **ChatGPT 后端**（`https://chatgpt.com/backend-api`，见 `login/src/auth/manager.rs:95`）：通过 ChatGPT 账户的 OAuth 令牌访问，是订阅用户（Plus/Pro/Business/Enterprise）的主路径。

认证子系统要解决的核心矛盾是：**令牌会过期、会被撤销、会被多进程并发改写，而程序的各个部分又需要看到一致的认证快照**。Codex 的取舍是——不让各处自己读 `auth.json`，而是统一经过 `AuthManager`：它持有一份内存缓存，只有显式 `reload()` 才会重新读盘（`login/src/auth/manager.rs:1251-1253` 注释明确说明了这个设计目标）。

```
                       ┌─────────────────────────────────────────┐
                       │              AuthManager                  │
   各业务模块  ──auth()─►│  RwLock<CachedAuth>  (内存唯一真相)        │
   (core/client 等)     │  watch::Sender<u64>  (认证变更广播)        │
                       │  Semaphore           (刷新并发闸)          │
                       │  RwLock<ExternalAuth> (外部认证插件)        │
                       └───────────────┬───────────────────────────┘
                                       │ reload() / refresh
                                       ▼
                       ┌─────────────────────────────────────────┐
                       │   AuthStorageBackend (trait)              │
                       │   File / Keyring / Auto / Ephemeral       │
                       └───────────────┬───────────────────────────┘
                                       ▼
                            $CODEX_HOME/auth.json  或  系统钥匙串
```

---

## 2. `CodexAuth`：四种认证模式

认证机制的"顶层和"是 `CodexAuth` 枚举，定义在 `login/src/auth/manager.rs:49-56`：

```rust
/// Authentication mechanism used by the current user.
#[derive(Debug, Clone)]
pub enum CodexAuth {
    ApiKey(ApiKeyAuth),
    Chatgpt(ChatgptAuth),
    ChatgptAuthTokens(ChatgptAuthTokens),
    AgentIdentity(AgentIdentityAuth),
}
```

与之对应的、序列化进 `auth.json` 的标识符是 `AuthMode` 枚举（注意它定义在另一个 crate：`app-server-protocol/src/protocol/common.rs:21`，用 `#[serde(rename_all = "lowercase")]`）：

| `CodexAuth` 变体 | `AuthMode` / serde 标识 | 令牌来源 | 是否落盘 | 刷新责任方 |
|---|---|---|---|---|
| `ApiKey` | `apikey` | 用户提供的 OpenAI API Key | 是（`OPENAI_API_KEY` 字段） | 无需刷新 |
| `Chatgpt` | `chatgpt` | Codex 托管的 ChatGPT OAuth 令牌 | 是（`tokens` 字段） | **Codex 自己刷新** |
| `ChatgptAuthTokens` | `chatgptAuthTokens` | 外部宿主 App 注入的 ChatGPT 令牌 | **否（仅内存）** | 外部宿主 App |
| `AgentIdentity` | `agentIdentity` | 注册过的 Agent Identity JWT | 是（`agent_identity` 字段存原始 JWT） | 由 Agent Identity 体系处理 |

> `ChatgptAuthTokens` 与 `AgentIdentity` 这两个变体在 `AuthMode` 上用了显式的 `#[serde(rename = "chatgptAuthTokens")]` / `#[serde(rename = "agentIdentity")]`（`common.rs:30-38`），因为 `lowercase` 规则会把它们压成全小写、丢掉驼峰边界。`ChatgptAuthTokens` 的文档注释还标了 **"FOR OPENAI INTERNAL USE ONLY"**（`common.rs:26-29`）——这是给宿主 App（如桌面端）内嵌 Codex 时用的内存令牌通道。

`CodexAuth` 的 `PartialEq` 只比较 `api_auth_mode()`（`manager.rs:58-62`），即"两个认证是否属于同一种模式"，并不比较具体令牌内容——这是个刻意的简化，避免把 token 字符串卷进相等性判断。

---

## 3. OAuth 登录流程（浏览器回环 + 设备代码）

ChatGPT OAuth 有**两条并行的登录流程**，都用 PKCE（Proof Key for Code Exchange，`S256`）防止授权码被中途截获。

### 3.1 浏览器回环流程（默认）

入口 `run_login_server()`（`login/src/server.rs:140`）。它在本地起一个 HTTP 服务器接收 OAuth 回调：

1. **生成 PKCE**：`generate_pkce()`（`login/src/pkce.rs:12`）随机生成 `code_verifier`，再用 `BASE64URL(SHA256(verifier))` 算出 `code_challenge`（`pkce.rs:19-21`，即标准 `S256` 方法）。
2. **绑定本地端口 + 构造回调地址**：`redirect_uri = http://localhost:{actual_port}/auth/callback`（`server.rs:156`）。
3. **构造授权 URL** 并让用户在浏览器打开（`build_authorize_url`，`server.rs:157`），URL 里带上 `client_id`、`redirect_uri`、`code_challenge`、`state`（CSRF 防护）。
4. **等回调**：用户在浏览器授权后，OpenAI 重定向回 `http://localhost:{port}/auth/callback?code=...&state=...`；`process_request`（`server.rs:283` 处理 `/auth/callback`）校验 `state` 是否匹配（`server.rs:304` 处会记录 state mismatch）。
5. **换令牌**：用 `code` + `code_verifier` 向令牌端点交换，得到 `id_token` / `access_token` / `refresh_token`，最终 `save_auth` 落盘。

### 3.2 设备代码流程（无浏览器/远程环境）

入口 `request_device_code()`（`login/src/device_code_auth.rs:159`），适合没有本地浏览器的场景（SSH、容器）。三步：

```
request_user_code        → 拿到 user_code（一次性码）+ 轮询 interval
   (POST {base}/api/accounts/deviceauth/usercode)
        │
        ▼  打印提示：让用户去 {base}/codex/device 输入 user_code
poll_for_token           → 循环轮询，直到用户在网页授权
   (POST .../deviceauth/token，15 分钟超时)
        │
        ▼  返回 authorization_code + code_verifier
exchange_code_for_tokens → 用 PKCE 换 id/access/refresh 令牌
```

关键细节：

- **15 分钟硬超时**：`poll_for_token` 里 `max_wait = Duration::from_secs(15 * 60)`（`device_code_auth.rs:107`），超时返回 `"device auth timed out after 15 minutes"`（`:132-134`）。
- **轮询节流**：每轮等待 `min(interval, 剩余时间)`（`:136`），状态码 `403/404` 表示"还没授权，继续等"（`:130`）。
- **服务端未开启该流程**：`usercode` 返回 `404` 时给出明确提示"device code login is not enabled"（`device_code_auth.rs:82-87`），引导用户改用浏览器登录。
- 设备码提示文案专门写了反钓鱼警告"Never share this code"（`:155`）——设备码是常见钓鱼目标。

> 两条流程最终都汇聚到同一处：把拿到的令牌写成 `AuthDotJson { auth_mode: Chatgpt, tokens: ... }` 并 `save_auth`。区别只在"如何拿到授权码"。

---

## 4. API Key 方式

最简单的路径，入口 `login_with_api_key()`（`login/src/auth/manager.rs:531-544`）：

```rust
pub fn login_with_api_key(
    codex_home: &Path,
    api_key: &str,
    auth_credentials_store_mode: AuthCredentialsStoreMode,
) -> std::io::Result<()> {
    let auth_dot_json = AuthDotJson {
        auth_mode: Some(ApiAuthMode::ApiKey),
        openai_api_key: Some(api_key.to_string()),
        tokens: None,
        last_refresh: None,
        agent_identity: None,
    };
    save_auth(codex_home, &auth_dot_json, auth_credentials_store_mode)
}
```

注意它是**同步函数**（不像 OAuth 流程是 `async`），因为不涉及任何网络往返——API Key 不需要验证、不需要刷新，直接写盘即可。

除了显式登录，加载阶段还支持**环境变量注入**：`load_auth` 在最前面会检查 `enable_codex_api_key_env` 并尝试 `read_codex_api_key_from_env()`（`manager.rs:740-742`），命中则直接返回 `CodexAuth::from_api_key`，完全不读 `auth.json`。这让 CI/脚本场景可以零落盘地用 API Key 跑。

---

## 5. Agent Identity 认证

Agent Identity 是面向"程序化/无人值守"场景的认证：不是 API Key、也不是交互式 ChatGPT，而是一段注册过的 JWT 代表某个 agent runtime。

### 5.1 登录路径

入口 `login_with_access_token()`（`manager.rs:547-566`）：

```rust
pub async fn login_with_access_token(
    codex_home: &Path,
    access_token: &str,           // 这里其实是 agent identity JWT
    auth_credentials_store_mode: AuthCredentialsStoreMode,
    chatgpt_base_url: Option<&str>,
) -> std::io::Result<()> {
    let base_url = chatgpt_base_url
        .unwrap_or(DEFAULT_CHATGPT_BACKEND_BASE_URL)...;
    verified_agent_identity_record(access_token, &base_url).await?;   // 先在线验证
    let auth_dot_json = AuthDotJson {
        auth_mode: Some(ApiAuthMode::AgentIdentity),
        openai_api_key: None,
        tokens: None,
        last_refresh: None,
        agent_identity: Some(access_token.to_string()),   // 原始 JWT 存这里
    };
    save_auth(codex_home, &auth_dot_json, auth_credentials_store_mode)
}
```

它会先 `verified_agent_identity_record`（拉取 JWKS、解析 JWT claims 做在线验证），通过后把**原始 JWT 字符串**存进 `auth.json` 的 `agent_identity` 字段（不是放在 `tokens` 里）。

### 5.2 认证记录与 task 注册

`AgentIdentityAuthRecord`（`login/src/auth/storage.rs:50-59`）是从 JWT 解析出的结构化身份：

```rust
pub struct AgentIdentityAuthRecord {
    pub agent_runtime_id: String,
    pub agent_private_key: String,
    pub account_id: String,
    pub chatgpt_user_id: String,
    pub email: String,
    pub plan_type: AccountPlanType,
    pub chatgpt_account_is_fedramp: bool,
}
```

加载时 `AgentIdentityAuth::load(record)`（`login/src/auth/agent_identity.rs:20-33`）会**调用 `register_agent_task` 注册一个 agent task**，拿回 `process_task_id` 存进 `AgentIdentityAuth`：

```rust
pub struct AgentIdentityAuth {
    record: AgentIdentityAuthRecord,
    process_task_id: String,        // load() 时由 register_agent_task 返回
}
```

> **注意**：每次 `load` 都会产生一次网络注册（`register_agent_task`）。`process_task_id` 当前用于把本进程与某个 agent task 关联，[推测] 未来可用于 session/审计追踪。`agent_identity_authapi_base_url()` 默认指向 `https://auth.openai.com/api/accounts`，可由 `CODEX_AGENT_IDENTITY_AUTHAPI_BASE_URL` 覆盖（`agent_identity.rs:10-11, 64-70`）。

加载阶段也支持环境变量注入：`read_codex_access_token_from_env()` 命中时直接走 `from_agent_identity_jwt`（`manager.rs:766-770`）。

---

## 6. 令牌存储与管理（`auth.json` 与四种后端）

### 6.1 `auth.json` 结构

`AuthDotJson`（`login/src/auth/storage.rs:32-48`）是落盘格式，所有可选字段都 `skip_serializing_if = "Option::is_none"`，所以不同模式写出的 JSON 各不相同：

```rust
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
pub struct AuthDotJson {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_mode: Option<AuthMode>,
    #[serde(rename = "OPENAI_API_KEY")]
    pub openai_api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<TokenData>,          // ChatGPT OAuth 三件套
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_refresh: Option<DateTime<Utc>>, // 上次刷新时间，用于 8 天定期刷新判断
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_identity: Option<String>,      // Agent Identity 原始 JWT
}
```

`OPENAI_API_KEY` 字段用了显式 `rename`，是为了让 `auth.json` 里这个键名与环境变量名一致、可读性更好。

`TokenData`（`login/src/token_data.rs:18-25`）里含 `access_token` / `refresh_token` / `account_id`，以及从 `id_token` JWT payload 解析出的 `IdTokenInfo`（`token_data.rs:28-42`）：

| `IdTokenInfo` 字段 | 含义 | 用途 |
|---|---|---|
| `chatgpt_account_id` | 组织/workspace 标识 | 工作区权限限制（见 §13） |
| `chatgpt_plan_type` | 订阅计划（free/plus/pro/business/enterprise/edu） | 能力门控、`is_workspace_account()` 判断 |
| `chatgpt_user_id` | ChatGPT 用户标识 | 账户识别 |
| `chatgpt_account_is_fedramp` | 是否需走 FedRAMP 边缘 | 合规路由 |

### 6.2 四种存储后端

存储抽象是 `AuthStorageBackend` trait，由 `AuthCredentialsStoreMode`（`login/src/auth/storage.rs:336-357` 的 `create_auth_storage` 分发）选择：

| 模式 | 实现 | 行为 |
|---|---|---|
| `File` | `FileAuthStorage` | 读写 `$CODEX_HOME/auth.json` |
| `Keyring` | `KeyringAuthStorage` | 存进系统钥匙串（服务名 `"Codex Auth"`，`storage.rs:160`） |
| `Auto` | `AutoAuthStorage` | **先 keyring 后 file**：读优先 keyring、失败降级 file；写优先 keyring、失败降级 file |
| `Ephemeral` | `EphemeralAuthStorage` | **纯内存**，用于外部 ChatGPT 令牌（`ChatgptAuthTokens`），不落盘 |

**钥匙串的 key 怎么算？** `compute_store_key`（`storage.rs:163-174`）：对 `canonicalize` 后的 `codex_home` 路径算 `SHA256`，取 hex 摘要的**前 16 个字符**，拼成 `cli|<16位hex>`。这样不同 `CODEX_HOME` 互不串台，又不把绝对路径明文塞进钥匙串。

**钥匙串保存的迁移副作用**：`KeyringAuthStorage::save`（`storage.rs:226-235`）写完钥匙串后，会**删除同目录的旧 `auth.json` 文件**：

```rust
self.save_to_keyring(&key, &serialized)?;
if let Err(err) = delete_file_if_exists(&self.codex_home) {
    warn!("failed to remove CLI auth fallback file: {err}");   // 仅 warn，不阻断
}
```

这是从 File 迁移到 Keyring 的清理机制——避免凭据同时存在文件和钥匙串两处。

---

## 7. `AuthManager` 核心架构

`AuthManager`（`login/src/auth/manager.rs:1254-1264`）是整个认证系统对外的唯一门面：

```rust
pub struct AuthManager {
    codex_home: PathBuf,
    inner: RwLock<CachedAuth>,                       // 内存唯一真相（认证快照）
    auth_change_tx: watch::Sender<u64>,              // 认证变更广播（递增计数）
    enable_codex_api_key_env: bool,
    auth_credentials_store_mode: AuthCredentialsStoreMode,
    forced_chatgpt_workspace_id: RwLock<Option<Vec<String>>>,
    chatgpt_base_url: Option<String>,
    refresh_lock: Semaphore,                          // 刷新并发闸（避免 N 个请求同时刷新）
    external_auth: RwLock<Option<Arc<dyn ExternalAuth>>>,  // 外部认证插件
}
```

三个并发原语各司其职：

- **`RwLock<CachedAuth>`**：业务侧 `auth()` 读的是这份缓存的克隆，不直接碰磁盘。外部对 `auth.json` 的改动**不会被自动感知**，必须显式 `reload()`（`manager.rs:1251-1253` 的注释把这点列为设计目标——为了让程序各部分看到一致的认证快照）。
- **`watch::Sender<u64>`**：每次认证变更递增计数并广播，订阅方（如 TUI、core）可据此刷新 UI 或重建客户端。
- **`refresh_lock: Semaphore`**：刷新令牌时的并发闸。多个请求同时撞到"令牌过期"时，靠这个信号量保证只有一个真正发起刷新网络请求，其余等待复用结果。

### 7.1 加载优先级（`load_auth`）

`load_auth`（`manager.rs:740-787`）的加载顺序很关键，体现了"外部 > 环境 > 持久化"的优先级：

```
1. 若 enable_codex_api_key_env 且环境里有 CODEX API KEY → 直接用 ApiKey，不读盘
2. 检查 Ephemeral（内存）存储  ← 外部 ChatGPT 令牌总是优先！
       命中则返回（External ChatGPT 优先于任何持久化凭据）
3. 若调用方明确要 Ephemeral 模式 → 没有持久化回退，返回 None
4. 若环境里有 agent identity JWT → 走 AgentIdentity
5. 最后才读配置的持久化后端（File/Keyring/Auto）
```

第 2 步的注释写得很直白（`manager.rs:744-745`）：

> External ChatGPT auth tokens live in the in-memory (ephemeral) store. Always check this first so external auth takes precedence over any persisted credentials.

**为什么外部优先？** 当宿主 App（如桌面客户端）注入了内存令牌，说明它要接管认证，此时即使磁盘上还残留着旧的 `auth.json`，也应当被内存令牌覆盖——否则会用错身份。

---

## 8. 刷新机制（主动刷新 + 失败分类）

### 8.1 两个时间阈值

```rust
const TOKEN_REFRESH_INTERVAL: i64 = 8;                       // 天
const CHATGPT_ACCESS_TOKEN_REFRESH_WINDOW_MINUTES: i64 = 5;  // 分钟
```

（`manager.rs:86-87`）

### 8.2 主动刷新判定

`should_refresh_proactively`（`manager.rs:1812-1834`）决定"现在要不要提前刷新"，**只对 `Chatgpt` 托管模式生效**（其他模式直接 `return false`）：

```rust
fn should_refresh_proactively(auth: &CodexAuth) -> bool {
    let chatgpt_auth = match auth {
        CodexAuth::Chatgpt(chatgpt_auth) => chatgpt_auth,
        _ => return false,
    };
    ...
    // 优先：access_token 的 JWT 过期时间是否在 5 分钟内
    if let Some(tokens) = ... && let Ok(Some(expires_at)) = parse_jwt_expiration(...) {
        return expires_at <= Utc::now()
            + chrono::Duration::minutes(CHATGPT_ACCESS_TOKEN_REFRESH_WINDOW_MINUTES);
    }
    // 回退：解析不出过期时间时，看 last_refresh 是否超过 8 天
    last_refresh < Utc::now() - chrono::Duration::days(TOKEN_REFRESH_INTERVAL)
}
```

逻辑是**短路优先**：

1. 能从 `access_token` 解析出 JWT 过期时间 → 若它在 **5 分钟**内到期，就刷新（提前于真正过期，避免请求中途 401）。
2. 解析不出过期时间 → 退而求其次，若上次刷新已超过 **8 天**，强制刷新（兜底，防止令牌长期不动腐化）。

### 8.3 刷新执行与持久化

实际刷新走 `request_chatgpt_token_refresh`（`manager.rs:817`，向 `REFRESH_TOKEN_URL = https://auth.openai.com/oauth/token` 发 `grant_type=refresh_token`，带 `CLIENT_ID`），成功后 `persist_tokens`（`manager.rs:790-813`）更新 `tokens` 并**把 `last_refresh` 刷为当前时间**，再 `save`。`refresh_and_persist_chatgpt_token`（`manager.rs:1891`）串起这两步并 `reload()` 同步缓存。

### 8.4 刷新失败分类

`classify_refresh_token_failure`（`manager.rs:860-887`）根据后端返回的错误码区分**永久失败**与**临时失败**：

| 后端错误码 | `RefreshTokenFailedReason` | 性质 |
|---|---|---|
| `refresh_token_expired` | `Expired` | 永久（refresh token 过期） |
| `refresh_token_reused` | `Exhausted` | 永久（refresh token 已被用过） |
| `refresh_token_invalidated` | `Revoked` | 永久（被撤销） |
| 其他/未知 | `Other` | 视情况，会 `warn!` 记录原始响应 |

这层分类很重要：**永久失败**意味着"再重试也没用，请用户重新登录"（错误文案如 `REFRESH_TOKEN_EXPIRED_MESSAGE`，`manager.rs:89-93`），上层会缓存这个永久失败状态、避免无意义的重试网络请求；**临时失败**（如网络抖动，归为 `RefreshTokenError::Transient`）则可以再试。

> `RefreshTokenError` 只有两个变体：`Permanent(RefreshTokenFailedError)` 和 `Transient(std::io::Error)`（`manager.rs:102-108`）。这个二分法贯穿整个刷新与恢复链路。

---

## 9. 401 Unauthorized 恢复状态机

当一个请求拿到 `401`，`AuthManager` 不会直接放弃，而是走 `UnauthorizedRecovery` 状态机（`manager.rs:1077-1244`）尝试自救。

### 9.1 两种模式

构造时（`UnauthorizedRecovery::new`，`manager.rs:1095-1118`）根据当前认证决定模式：

```rust
let mode = if manager.has_external_api_key_auth()
    || cached_auth.is_some_and(CodexAuth::is_external_chatgpt_tokens) {
    UnauthorizedRecoveryMode::External    // 外部认证：refresh 一次就完事
} else {
    UnauthorizedRecoveryMode::Managed      // 托管：可走 Reload + RefreshToken 两步
};
```

### 9.2 状态流转

```
Managed 模式：
   Reload ──reload_if_account_id_matches──► RefreshToken ──refresh_token_from_authority──► Done
     │ (账户 ID 不匹配)
     └──► Done (返回账户切换错误 ACCOUNT_MISMATCH)

External 模式：
   ExternalRefresh ──external_auth.refresh()──► Done
```

`next()`（`manager.rs:1186-1243`）逐步推进：

- **`Reload`**（仅 Managed）：先 `reload_if_account_id_matches`（见下），把磁盘最新状态读进缓存。若 reload 成功（无论令牌是否变化）→ 进入 `RefreshToken`；若账户 ID 不匹配被跳过 → 直接 `Done` 并返回永久失败（账户切换文案）。
- **`RefreshToken`**（仅 Managed）：`refresh_token_from_authority()` 真正发起刷新 → `Done`。
- **`ExternalRefresh`**（仅 External）：调外部插件 `refresh_external_auth(Unauthorized)` → `Done`。

> **两种模式的重试预算不同**：External 模式本质只重试一次（`ExternalRefresh → Done`），因为外部令牌的刷新责任在宿主 App，Codex 只能请它给一次新令牌；Managed 模式可走两步（`Reload + RefreshToken`）。两者都会在永久失败时停下、避免频繁打网络。

### 9.3 账户 ID 匹配校验（并发安全的关键）

`reload_if_account_id_matches`（`manager.rs:1450-1483`）是防"多进程并发改写 `auth.json` 导致账户串台"的护栏：

```rust
let new_account_id = new_auth.as_ref().and_then(CodexAuth::get_account_id);
if new_account_id.as_deref() != Some(expected_account_id) {
    tracing::info!("Skipping auth reload due to account id mismatch ...");
    return ReloadOutcome::Skipped;   // 账户变了 → 不继续刷新
}
```

设想：进程 A 正用账户 X，期间用户在另一个进程里登录了账户 Y（改写了 `auth.json`）。A 收到 401 触发 reload 时，发现磁盘上的账户 ID 已变成 Y，与自己期望的 X 不符——这时**绝不能**用 Y 的令牌继续 A 的请求，否则就是身份混淆。于是返回 `Skipped`，上层据此抛出"账户切换"错误（`REFRESH_TOKEN_ACCOUNT_MISMATCH_MESSAGE`）。

---

## 10. 外部认证集成（`ExternalAuth` trait）

`ExternalAuth` trait（`login/src/auth/manager.rs:160-180`）是给宿主 App 接管认证的插件点：

```rust
#[async_trait]
pub trait ExternalAuth: Send + Sync {
    fn auth_mode(&self) -> AuthMode;                       // ApiKey 或 Chatgpt

    /// 同步可用就返回，否则 None（默认实现返回 None）
    async fn resolve(&self) -> std::io::Result<Option<ExternalAuthTokens>> {
        Ok(None)
    }

    /// 按需刷新——必须返回值（不能 None）
    async fn refresh(&self, context: ExternalAuthRefreshContext)
        -> std::io::Result<ExternalAuthTokens>;
}
```

设计上的不对称很值得注意：

- **`resolve()` 可以返回 `None`**（默认实现就是 `None`），表示"我现在没有立即可用的令牌"。
- **`refresh()` 必须返回 `ExternalAuthTokens`**（要么给令牌、要么报错），因为它是在 401 恢复时被驱动的"最后一搏"，不允许含糊。

`ExternalAuthTokens`（`manager.rs:110-114`）携带 `access_token` 和可选的 `chatgpt_metadata`（`account_id` + `plan_type`）。**API Key 外部认证无需元数据**（`chatgpt_metadata: None`，见 `access_token_only`，`manager.rs:123-128`），只有 ChatGPT 模式才需要 workspace/plan 信息。

外部 ChatGPT 刷新后（`refresh_external_auth`，`manager.rs:1836` 起），新令牌会经 `AuthDotJson::from_external_tokens`（`manager.rs:938`）转换，并以 **`Ephemeral` 模式 `save_auth`**（`manager.rs:1879-1884`，不落盘），再 `reload()`。刷新前还会校验返回的 workspace 是否在 `forced_chatgpt_workspace_id` 允许列表内（`manager.rs:1867-1876`），不符则报临时错误。

---

## 11. 安装 ID

安装 ID 是标识"这台机器上的这份 Codex 安装"的稳定 UUID，定义在 `core/src/installation_id.rs:19-64`（注意它在 `core` 而非 `login` crate）。

`resolve_installation_id`（`installation_id.rs:19`）：

```rust
pub async fn resolve_installation_id(codex_home: &AbsolutePathBuf) -> Result<String> {
    let path = codex_home.join("installation_id");
    fs::create_dir_all(codex_home).await?;
    tokio::task::spawn_blocking(move || {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)] { options.mode(0o644); }       // Unix 权限 0o644
        let mut file = options.open(&path)?;
        file.lock()?;                                // 文件锁，保证并发安全
        #[cfg(unix)] {
            // 验证现有权限是否为 0o644，不是则纠正
            let current_mode = metadata.permissions().mode() & 0o777;
            if current_mode != 0o644 { ... set_mode(0o644) ... }
        }
        // 读出已有 UUID，能解析就复用；否则生成 v4 UUID 写回
        ...
        let installation_id = Uuid::new_v4().to_string();
        file.set_len(0)?; file.write_all(...)?; file.sync_all()?;
        Ok(installation_id)
    }).await?
}
```

三个设计要点：

1. **文件锁（`file.lock()`，`:32`）**：多个 Codex 进程可能同时首次启动，靠文件锁保证只有一个真正生成 UUID，避免竞态写出两个不同 ID。
2. **复用优先**：先读现有内容，能 `Uuid::parse_str` 成功就直接复用（`:48-52`）——安装 ID 一旦生成就应稳定不变。
3. **Unix 权限 `0o644`（`:28`）**：创建时指定，且每次打开都**校验并纠正**为 `0o644`（`:38-42`）。这是个普通可读文件（不像凭据那样敏感），但仍要确保权限可控。

> 安装 ID 与认证身份正交：它标识"安装实例"，常用于遥测/分析时区分设备，与"用谁的账户登录"无关。

---

## 12. 注销与令牌撤销

### 12.1 普通注销

`AuthManager::logout`（`manager.rs:1771`）→ `logout_all_stores`（`manager.rs:721`）**删除所有存储后端的凭据**（File + Keyring + Ephemeral 都清），再 `reload()` 清空内存缓存。"删全部"是因为用户可能历史上用过不同后端、各处都有残留，注销必须彻底。

### 12.2 带撤销的注销

`logout_with_revoke`（`manager.rs:1778`）在删除前**先向服务端撤销令牌**，让令牌即使被泄露也立即失效。

撤销逻辑在 `login/src/auth/revoke.rs`：

```rust
fn client_id(self) -> Option<&'static str> {
    match self {
        Self::Access => None,            // access_token 撤销不带 client_id
        Self::Refresh => Some(CLIENT_ID), // refresh_token 撤销带 client_id
    }
}
```

`revoke_auth_tokens`（`revoke.rs:55-65`）的策略（对应 `RevokeTokenKind::Refresh` 优先于 `Access`）：

- **优先撤销 `refresh_token`**（撤销它会连带使派生的 access token 失效），**回退到 `access_token`**（无 refresh token 时）。这个优先级体现在 `revocable_token` 的选取上。
- POST 到 `REVOKE_TOKEN_URL = https://auth.openai.com/oauth/revoke`（`manager.rs:97`），**10 秒超时**（`REVOKE_HTTP_TIMEOUT = Duration::from_secs(10)`，`revoke.rs:23`）。
- **尽力而为**：模块文档注释（`revoke.rs:1-6`）明确"callers still complete their primary work if the revoke request fails"——撤销失败不阻断注销主流程。

---

## 13. 登录约束强制检查

`enforce_login_restrictions`（`manager.rs:619-704`）在启动时（被 `tui/src/lib.rs:1182` 和 `exec/src/lib.rs:454` 调用）强制企业/受限场景的登录策略：

### 13.1 强制登录方式（`forced_login_method`）

`ForcedLoginMethod::Api` vs `ForcedLoginMethod::Chatgpt`，与当前 `auth.auth_mode()` 做矩阵匹配（`manager.rs:632-647`）：

| 要求 | 当前实际 | 结果 |
|---|---|---|
| `Api` | `ApiKey` | ✅ 通过 |
| `Chatgpt` | `Chatgpt` / `ChatgptAuthTokens` / `AgentIdentity` | ✅ 通过 |
| `Api` | ChatGPT 系 | ❌ 注销（"API key login is required..."） |
| `Chatgpt` | `ApiKey` | ❌ 注销（"ChatGPT login is required..."） |

### 13.2 强制工作区（`forced_chatgpt_workspace_id`）

若配置了允许的 workspace 列表，会取当前认证的 `chatgpt_account_id`（即 workspace，`manager.rs:658-677`），不在允许列表内就注销并提示。`ApiKey` 认证直接跳过此检查（`manager.rs:660` 的 `return Ok(())`）。

违反任一约束时，统一走 `logout_with_message`（`manager.rs:706`），它同样会清空 File + Ephemeral 双存储（注释 `manager.rs:711-712`），确保被禁的凭据彻底清除。

> 这层强制检查是企业部署的合规闸门：管理员可强制只用 API Key（便于审计计费）或只用某个企业 workspace，违反即自动登出，杜绝绕过。

---

## 14. 常见问题

**Q1：为什么业务代码改了 `auth.json` 后程序没反应？**
因为 `AuthManager` 持有内存缓存，外部对 `auth.json` 的改动**不会被自动感知**（`manager.rs:1251-1253` 注释把这点列为设计目标）。必须显式调用 `reload()`。这是为了让程序各部分在一次运行内看到一致的认证快照，避免中途身份漂移。

**Q2：`Chatgpt` 和 `ChatgptAuthTokens` 有什么区别？**
都是 ChatGPT 后端认证，但：`Chatgpt` 是 **Codex 托管**——令牌落盘、Codex 自己负责刷新；`ChatgptAuthTokens` 是**外部宿主注入**——只在内存（Ephemeral 存储）、不落盘、刷新责任在宿主 App，且标了"OPENAI INTERNAL USE ONLY"（`common.rs:26-29`）。加载时外部令牌**总是优先**于持久化凭据（`manager.rs:744-759`）。

**Q3：令牌什么时候会被主动刷新？**
仅对 `Chatgpt` 托管模式。两个条件之一满足即刷新（`manager.rs:1812-1834`）：① `access_token` JWT 将在 **5 分钟**内过期；② 解析不出过期时间时，`last_refresh` 已超过 **8 天**。

**Q4：刷新失败了会怎样？会一直重试吗？**
看失败类型（`manager.rs:860-887`）。`refresh_token_expired/reused/invalidated` 是**永久失败**——会缓存该状态、不再重试，提示用户重新登录；其他（如网络问题）是**临时失败**，可再试。401 恢复状态机里 External 模式只试一次、Managed 模式最多 Reload+RefreshToken 两步（`manager.rs:1095-1244`）。

**Q5：从文件迁移到钥匙串会怎样？旧文件还在吗？**
不在。`KeyringAuthStorage::save` 写完钥匙串后会删除同目录的旧 `auth.json`（`storage.rs:231-233`），作为迁移清理，避免凭据双份存在。

**Q6：多个 Codex 进程同时跑会不会把认证搞乱？**
有两道护栏：① 安装 ID 用**文件锁**生成（`installation_id.rs:32`）；② 401 恢复时做**账户 ID 匹配校验**（`manager.rs:1450-1483`）——若 reload 后发现磁盘账户已被别的进程改成另一个账户，则跳过刷新并返回账户切换错误，绝不混用身份。

**Q7：钥匙串里的 key 是怎么算的？会不会和别的 `CODEX_HOME` 冲突？**
不会。`compute_store_key`（`storage.rs:163-174`）对规范化后的 `codex_home` 路径算 SHA256、取前 16 位 hex，拼成 `cli|<hex>`。不同 `CODEX_HOME` 路径产生不同 key，互不干扰；同时也不把绝对路径明文写进钥匙串。

---

## 相关章节

- 生命周期与 `Op` 协议：见 `doc11`（`11_thread_session_turn_lifecycle.md`，§10 为协议 Op）。
- 沙箱机制（与认证正交的另一道安全边界）：见 `doc13`（`13_sandbox_mechanism.md`）。
- HTTP 客户端如何携带认证头进入请求循环：见 core 的 `client.rs`（认证由 `AuthManager::auth()` 提供快照）。
