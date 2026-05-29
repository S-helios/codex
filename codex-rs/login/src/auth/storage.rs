//! 【文件职责】定义认证凭据的持久化抽象与四种存储后端，统一 `auth.json` 的读 / 写 / 删。
//!
//! 【核心抽象】`AuthStorageBackend` trait（`load` / `save` / `delete`）+ 四个实现：
//!   - `FileAuthStorage`：明文落盘到 `$CODEX_HOME/auth.json`（Unix 下 0600 权限）。
//!   - `KeyringAuthStorage`：写系统钥匙串（macOS Keychain / Windows Credential 等），更安全。
//!   - `AutoAuthStorage`：优先钥匙串、失败回退文件，兼顾安全与可用性（默认推荐）。
//!   - `EphemeralAuthStorage`：进程内全局内存表，仅本次运行有效，用于外部注入的临时令牌。
//!
//! 【架构位置】
//!   层级：认证子系统 · 持久化层
//!   上游：`auth/manager.rs`（`load_auth` / `save_auth` / `persist_tokens` 经 `create_auth_storage` 取后端）
//!   下游：`codex_keyring_store`（钥匙串）、`std::fs`（文件）、进程内 `EPHEMERAL_AUTH_STORE`
//!
//! 【数据载体】`AuthDotJson` 即 `auth.json` 的结构映射；本文件只管「存在哪、怎么存」，
//!   令牌字段的语义（access/refresh/id_token、计划类型等）见 `token_data` 与 `manager.rs`。
//!
//! 【阅读建议】先看 `AuthDotJson` 字段、再看 `AuthStorageBackend` 契约，
//!   最后按需对照各后端实现；`create_auth_storage` 是统一工厂入口。

use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashMap;
use std::fmt::Debug;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use tracing::warn;

use crate::token_data::TokenData;
use codex_agent_identity::AgentIdentityJwtClaims;
use codex_agent_identity::decode_agent_identity_jwt;
use codex_app_server_protocol::AuthMode;
use codex_config::types::AuthCredentialsStoreMode;
use codex_keyring_store::DefaultKeyringStore;
use codex_keyring_store::KeyringStore;
use codex_protocol::account::PlanType as AccountPlanType;
use once_cell::sync::Lazy;

/// Expected structure for $CODEX_HOME/auth.json.
/// `auth.json` 文件的结构映射，是认证子系统所有读写的统一数据载体。
/// 字段大多 `Option` 且 `skip_serializing_if`：不同认证模式只填其相关字段，
/// 序列化时省略空值以保持文件简洁、向后兼容。
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
pub struct AuthDotJson {
    // 认证模式标识（apikey/chatgpt/...）。缺省时由 `manager.rs::resolved_mode()`
    // 依据其它字段推断，以兼容早期未写该字段的旧文件。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_mode: Option<AuthMode>,

    // 注意：磁盘上的字段名是大写 `OPENAI_API_KEY`（历史约定），与 Rust 字段名映射。
    #[serde(rename = "OPENAI_API_KEY")]
    pub openai_api_key: Option<String>,

    // ChatGPT OAuth 令牌组（access/refresh/id_token 等），仅 ChatGPT 系模式存在。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<TokenData>,

    // 上次刷新时间，用于 `manager.rs` 判断是否到了主动刷新窗口。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_refresh: Option<DateTime<Utc>>,

    // Agent Identity 模式专用：直接保存原始 JWT 字符串，校验/解码在加载时进行。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_identity: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct AgentIdentityAuthRecord {
    pub agent_runtime_id: String,
    pub agent_private_key: String,
    pub account_id: String,
    pub chatgpt_user_id: String,
    pub email: String,
    pub plan_type: AccountPlanType,
    pub chatgpt_account_is_fedramp: bool,
}

impl AgentIdentityAuthRecord {
    pub(crate) fn from_agent_identity_jwt(jwt: &str) -> std::io::Result<Self> {
        let claims =
            decode_agent_identity_jwt(jwt, /*jwks*/ None).map_err(std::io::Error::other)?;

        Ok(claims.into())
    }
}

impl From<AgentIdentityJwtClaims> for AgentIdentityAuthRecord {
    fn from(claims: AgentIdentityJwtClaims) -> Self {
        Self {
            agent_runtime_id: claims.agent_runtime_id,
            agent_private_key: claims.agent_private_key,
            account_id: claims.account_id,
            chatgpt_user_id: claims.chatgpt_user_id,
            email: claims.email,
            plan_type: claims.plan_type.into(),
            chatgpt_account_is_fedramp: claims.chatgpt_account_is_fedramp,
        }
    }
}

pub(super) fn get_auth_file(codex_home: &Path) -> PathBuf {
    codex_home.join("auth.json")
}

// 删除 auth.json：文件不存在视为「未删任何东西」返回 `Ok(false)`，而非报错。
// 返回值约定：`Ok(true)`=确实删了一个文件，`Ok(false)`=本就不存在。
pub(super) fn delete_file_if_exists(codex_home: &Path) -> std::io::Result<bool> {
    let auth_file = get_auth_file(codex_home);
    match std::fs::remove_file(&auth_file) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

/// 认证存储后端统一契约：屏蔽「文件 / 钥匙串 / 内存」的差异，对上只暴露读写删三操作。
/// 约定：`load` 在「无凭据」时返回 `Ok(None)`（非错误）；`delete` 返回是否真的删除了内容。
/// 需 `Send + Sync`：会被 `Arc<dyn AuthStorageBackend>` 跨线程共享（见 `manager.rs`）。
pub(super) trait AuthStorageBackend: Debug + Send + Sync {
    fn load(&self) -> std::io::Result<Option<AuthDotJson>>;
    fn save(&self, auth: &AuthDotJson) -> std::io::Result<()>;
    fn delete(&self) -> std::io::Result<bool>;
}

/// 后端一：明文文件存储，凭据直接落到 `$CODEX_HOME/auth.json`。
/// 简单可移植，但凭据以明文存在磁盘上——靠文件权限（Unix 0600）做基本保护，
/// 安全性弱于钥匙串，故默认推荐 `Auto`（钥匙串优先）。
#[derive(Clone, Debug)]
pub(super) struct FileAuthStorage {
    codex_home: PathBuf,
}

impl FileAuthStorage {
    pub(super) fn new(codex_home: PathBuf) -> Self {
        Self { codex_home }
    }

    /// Attempt to read and parse the `auth.json` file in the given `CODEX_HOME` directory.
    /// Returns the full AuthDotJson structure.
    pub(super) fn try_read_auth_json(&self, auth_file: &Path) -> std::io::Result<AuthDotJson> {
        let mut file = File::open(auth_file)?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        let auth_dot_json: AuthDotJson = serde_json::from_str(&contents)?;

        Ok(auth_dot_json)
    }
}

impl AuthStorageBackend for FileAuthStorage {
    fn load(&self) -> std::io::Result<Option<AuthDotJson>> {
        let auth_file = get_auth_file(&self.codex_home);
        let auth_dot_json = match self.try_read_auth_json(&auth_file) {
            Ok(auth) => auth,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        Ok(Some(auth_dot_json))
    }

    fn save(&self, auth_dot_json: &AuthDotJson) -> std::io::Result<()> {
        let auth_file = get_auth_file(&self.codex_home);

        if let Some(parent) = auth_file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json_data = serde_json::to_string_pretty(auth_dot_json)?;
        let mut options = OpenOptions::new();
        options.truncate(true).write(true).create(true);
        #[cfg(unix)]
        {
            // 关键安全点：Unix 下以 0600（仅属主可读写）创建文件，
            // 防止同机其它用户读取明文令牌。Windows 无此机制故不设置。
            options.mode(0o600);
        }
        let mut file = options.open(auth_file)?;
        file.write_all(json_data.as_bytes())?;
        file.flush()?;
        Ok(())
    }

    fn delete(&self) -> std::io::Result<bool> {
        delete_file_if_exists(&self.codex_home)
    }
}

const KEYRING_SERVICE: &str = "Codex Auth";

// turns codex_home path into a stable, short key string
// 把 codex_home 路径转为稳定且短的键：先规范化路径（失败则用原值），
// 再取其 SHA-256 十六进制前 16 位，前缀 `cli|`。用作钥匙串条目名与内存表的 key，
// 保证同一 home 始终映射到同一条目，且不把完整路径明文写进键名。
fn compute_store_key(codex_home: &Path) -> std::io::Result<String> {
    let canonical = codex_home
        .canonicalize()
        .unwrap_or_else(|_| codex_home.to_path_buf());
    let path_str = canonical.to_string_lossy();
    let mut hasher = Sha256::new();
    hasher.update(path_str.as_bytes());
    let digest = hasher.finalize();
    let hex = format!("{digest:x}");
    let truncated = hex.get(..16).unwrap_or(&hex);
    Ok(format!("cli|{truncated}"))
}

/// 后端二：系统钥匙串存储，凭据交给 OS 安全存储（macOS Keychain / Linux Secret Service 等）。
/// 比明文文件安全；`save`/`delete` 会顺带清理可能残留的回退文件，避免明文副本泄露。
#[derive(Clone, Debug)]
struct KeyringAuthStorage {
    codex_home: PathBuf,
    keyring_store: Arc<dyn KeyringStore>,
}

impl KeyringAuthStorage {
    fn new(codex_home: PathBuf, keyring_store: Arc<dyn KeyringStore>) -> Self {
        Self {
            codex_home,
            keyring_store,
        }
    }

    fn load_from_keyring(&self, key: &str) -> std::io::Result<Option<AuthDotJson>> {
        match self.keyring_store.load(KEYRING_SERVICE, key) {
            Ok(Some(serialized)) => serde_json::from_str(&serialized).map(Some).map_err(|err| {
                std::io::Error::other(format!(
                    "failed to deserialize CLI auth from keyring: {err}"
                ))
            }),
            Ok(None) => Ok(None),
            Err(error) => Err(std::io::Error::other(format!(
                "failed to load CLI auth from keyring: {}",
                error.message()
            ))),
        }
    }

    fn save_to_keyring(&self, key: &str, value: &str) -> std::io::Result<()> {
        match self.keyring_store.save(KEYRING_SERVICE, key, value) {
            Ok(()) => Ok(()),
            Err(error) => {
                let message = format!(
                    "failed to write OAuth tokens to keyring: {}",
                    error.message()
                );
                warn!("{message}");
                Err(std::io::Error::other(message))
            }
        }
    }
}

impl AuthStorageBackend for KeyringAuthStorage {
    fn load(&self) -> std::io::Result<Option<AuthDotJson>> {
        let key = compute_store_key(&self.codex_home)?;
        self.load_from_keyring(&key)
    }

    fn save(&self, auth: &AuthDotJson) -> std::io::Result<()> {
        let key = compute_store_key(&self.codex_home)?;
        // Simpler error mapping per style: prefer method reference over closure
        let serialized = serde_json::to_string(auth).map_err(std::io::Error::other)?;
        self.save_to_keyring(&key, &serialized)?;
        // 写钥匙串成功后清掉旧的明文回退文件：防止磁盘上残留过期的明文凭据副本。
        // 删除失败仅告警不报错——钥匙串才是权威来源，残留文件不影响正确性。
        if let Err(err) = delete_file_if_exists(&self.codex_home) {
            warn!("failed to remove CLI auth fallback file: {err}");
        }
        Ok(())
    }

    fn delete(&self) -> std::io::Result<bool> {
        let key = compute_store_key(&self.codex_home)?;
        let keyring_removed = self
            .keyring_store
            .delete(KEYRING_SERVICE, &key)
            .map_err(|err| {
                std::io::Error::other(format!("failed to delete auth from keyring: {err}"))
            })?;
        let file_removed = delete_file_if_exists(&self.codex_home)?;
        Ok(keyring_removed || file_removed)
    }
}

/// 后端三：自动模式（默认推荐），组合钥匙串与文件两种后端。
/// 策略：读 / 写 一律先试钥匙串，钥匙串不可用（无凭据或出错）时回退到文件。
/// 这样在支持钥匙串的环境里更安全，在不支持的环境里仍能工作。
#[derive(Clone, Debug)]
struct AutoAuthStorage {
    keyring_storage: Arc<KeyringAuthStorage>,
    file_storage: Arc<FileAuthStorage>,
}

impl AutoAuthStorage {
    fn new(codex_home: PathBuf, keyring_store: Arc<dyn KeyringStore>) -> Self {
        Self {
            keyring_storage: Arc::new(KeyringAuthStorage::new(codex_home.clone(), keyring_store)),
            file_storage: Arc::new(FileAuthStorage::new(codex_home)),
        }
    }
}

impl AuthStorageBackend for AutoAuthStorage {
    fn load(&self) -> std::io::Result<Option<AuthDotJson>> {
        // 钥匙串命中即用；命中空（`Ok(None)`，如尚未迁移到钥匙串）或读取出错（`Err`）
        // 都回退到文件存储读取。区别仅在于出错时多打一条告警。
        match self.keyring_storage.load() {
            Ok(Some(auth)) => Ok(Some(auth)),
            Ok(None) => self.file_storage.load(),
            Err(err) => {
                warn!("failed to load CLI auth from keyring, falling back to file storage: {err}");
                self.file_storage.load()
            }
        }
    }

    fn save(&self, auth: &AuthDotJson) -> std::io::Result<()> {
        match self.keyring_storage.save(auth) {
            Ok(()) => Ok(()),
            Err(err) => {
                warn!("failed to save auth to keyring, falling back to file storage: {err}");
                self.file_storage.save(auth)
            }
        }
    }

    fn delete(&self) -> std::io::Result<bool> {
        // Keyring storage will delete from disk as well
        self.keyring_storage.delete()
    }
}

// A global in-memory store for mapping codex_home -> AuthDotJson.
// [引用范围 · 进程级全局单例] 由所有 `EphemeralAuthStorage` 实例共享的内存表，
// 键为 `compute_store_key(codex_home)`。进程退出即丢失，不落盘——这正是它存在的意义：
// 承载外部宿主 App 注入的临时 ChatGPT 令牌（不应写入磁盘）。
static EPHEMERAL_AUTH_STORE: Lazy<Mutex<HashMap<String, AuthDotJson>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// 后端四：内存（临时）存储，凭据只活在本进程内存里，从不落盘。
/// 用途：外部注入的 ChatGPT 令牌、以及测试场景。多实例共享同一张全局表 `EPHEMERAL_AUTH_STORE`。
#[derive(Clone, Debug)]
struct EphemeralAuthStorage {
    codex_home: PathBuf,
}

impl EphemeralAuthStorage {
    fn new(codex_home: PathBuf) -> Self {
        Self { codex_home }
    }

    fn with_store<F, T>(&self, action: F) -> std::io::Result<T>
    where
        F: FnOnce(&mut HashMap<String, AuthDotJson>, String) -> std::io::Result<T>,
    {
        let key = compute_store_key(&self.codex_home)?;
        let mut store = EPHEMERAL_AUTH_STORE
            .lock()
            .map_err(|_| std::io::Error::other("failed to lock ephemeral auth storage"))?;
        action(&mut store, key)
    }
}

impl AuthStorageBackend for EphemeralAuthStorage {
    fn load(&self) -> std::io::Result<Option<AuthDotJson>> {
        self.with_store(|store, key| Ok(store.get(&key).cloned()))
    }

    fn save(&self, auth: &AuthDotJson) -> std::io::Result<()> {
        self.with_store(|store, key| {
            store.insert(key, auth.clone());
            Ok(())
        })
    }

    fn delete(&self) -> std::io::Result<bool> {
        self.with_store(|store, key| Ok(store.remove(&key).is_some()))
    }
}

/// 统一工厂：按存储模式选出对应的后端实现，是 `manager.rs` 获取存储后端的唯一入口。
/// 默认注入系统钥匙串实现 `DefaultKeyringStore`；测试可走下面的 `_with_keyring_store` 注入桩。
pub(super) fn create_auth_storage(
    codex_home: PathBuf,
    mode: AuthCredentialsStoreMode,
) -> Arc<dyn AuthStorageBackend> {
    let keyring_store: Arc<dyn KeyringStore> = Arc::new(DefaultKeyringStore);
    create_auth_storage_with_keyring_store(codex_home, mode, keyring_store)
}

fn create_auth_storage_with_keyring_store(
    codex_home: PathBuf,
    mode: AuthCredentialsStoreMode,
    keyring_store: Arc<dyn KeyringStore>,
) -> Arc<dyn AuthStorageBackend> {
    match mode {
        AuthCredentialsStoreMode::File => Arc::new(FileAuthStorage::new(codex_home)),
        AuthCredentialsStoreMode::Keyring => {
            Arc::new(KeyringAuthStorage::new(codex_home, keyring_store))
        }
        AuthCredentialsStoreMode::Auto => Arc::new(AutoAuthStorage::new(codex_home, keyring_store)),
        AuthCredentialsStoreMode::Ephemeral => Arc::new(EphemeralAuthStorage::new(codex_home)),
    }
}

#[cfg(test)]
#[path = "storage_tests.rs"]
mod tests;
