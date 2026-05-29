//! 【文件职责】core 侧装配「网络代理状态」：从分层配置（system/user/project/
//! session）+ execpolicy 中解析出网络策略，校验「受信任层」施加的不可逾越约束，
//! 编译 MITM 钩子，最终产出 `ConfigState`/`NetworkProxyState` 供代理运行；并提供
//! 基于文件 mtime 的热重载器，配置文件改动时自动重建状态。
//!
//! 【架构位置】
//!   层级：Agent 核心层 → 网络代理装配
//!   上游：core 启动网络代理时调用 `build_network_proxy_state*`。
//!   下游：`codex_config` 的分层加载/合并、`exec_policy` 的网络规则、
//!         `codex_network_proxy`（消费产出的 `NetworkProxyConfig`/约束/状态）。
//!
//! 【数据流】
//!   分层配置 + execpolicy
//!     → `config_from_layers`（合并 TOML、按 `default_permissions` 选 profile、
//!        编译 MITM 钩子、叠加 execpolicy 的 allow/deny 域名）→ `NetworkProxyConfig`
//!     → `enforce_trusted_constraints`（仅取「非用户可控层」算约束并校验配置不越界）
//!     → `build_config_state` → `ConfigState`(+ 各层 mtime 快照)
//!
//! 【信任边界·安全要点】`is_user_controlled_layer` 把 User/Project/SessionFlags
//!   视为「用户可控」。约束（constraints）只由 System 等「受信任层」推导，再用它
//!   校验「合并后的最终配置」——确保用户层不能放宽受信任层设下的网络限制
//!   （如把 mode 从 limited 改成 full、或加白名单域名）。这是防提权的核心机制。
//!
//! 【阅读建议】先看 `build_config_state_with_mtimes`（总装），再看
//!   `config_from_layers` + `NetworkConfigAccumulator`（配置如何累积/收尾）与
//!   `enforce_trusted_constraints`（约束如何施加）；底部 `MtimeConfigReloader`
//!   是热重载实现。

use crate::config::find_codex_home;
use crate::config::is_builtin_permission_profile_name;
use crate::config::reject_unknown_builtin_permission_profile;
use crate::config::resolve_permission_profile;
use crate::exec_policy::ExecPolicyError;
use crate::exec_policy::format_exec_policy_error_with_source;
use crate::exec_policy::load_exec_policy;
use anyhow::Context;
use anyhow::Result;
use async_trait::async_trait;
use codex_app_server_protocol::ConfigLayerSource;
use codex_config::CONFIG_TOML_FILE;
use codex_config::CloudRequirementsLoader;
use codex_config::ConfigLayerStack;
use codex_config::ConfigLayerStackOrdering;
use codex_config::LoaderOverrides;
use codex_config::loader::load_config_layers_state;
use codex_config::merge_toml_values;
use codex_config::permissions_toml::NetworkMitmActionToml;
use codex_config::permissions_toml::NetworkMitmHookToml;
use codex_config::permissions_toml::NetworkMitmToml;
use codex_config::permissions_toml::NetworkToml;
use codex_config::permissions_toml::PermissionsToml;
use codex_config::permissions_toml::overlay_network_domain_permissions;
use codex_exec_server::LOCAL_FS;
use codex_network_proxy::ConfigReloader;
use codex_network_proxy::ConfigState;
use codex_network_proxy::NetworkMode;
use codex_network_proxy::NetworkProxyConfig;
use codex_network_proxy::NetworkProxyConstraintError;
use codex_network_proxy::NetworkProxyConstraints;
use codex_network_proxy::NetworkProxyState;
use codex_network_proxy::build_config_state;
use codex_network_proxy::normalize_host;
use codex_network_proxy::validate_policy_against_constraints;
use codex_utils_absolute_path::AbsolutePathBuf;
use indexmap::IndexMap;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::RwLock;

/// 装配带热重载能力的网络代理状态：构建初始 `ConfigState` 并绑定 mtime 重载器，
/// 配置文件变更时可自动重建。是 core 启动代理的常用入口。
pub async fn build_network_proxy_state() -> Result<NetworkProxyState> {
    let (state, reloader) = build_network_proxy_state_and_reloader().await?;
    Ok(NetworkProxyState::with_reloader(state, Arc::new(reloader)))
}

/// 同时返回初始状态与重载器（分开持有的版本），调用方可自行决定如何接线重载。
pub async fn build_network_proxy_state_and_reloader() -> Result<(ConfigState, MtimeConfigReloader)>
{
    let (state, layer_mtimes) = build_config_state_with_mtimes().await?;
    Ok((state, MtimeConfigReloader::new(layer_mtimes)))
}

/// 装配核心：加载分层配置 + execpolicy，解析出网络配置，校验受信任约束，
/// 产出 `ConfigState` 及各配置层的 mtime 快照（供重载器比对）。
///
/// execpolicy 解析容错：仅当错误是「策略文本解析失败」时降级为空策略并告警，
/// 让网络代理仍能基于其余配置启动；其它错误（如 IO）直接上抛。
async fn build_config_state_with_mtimes() -> Result<(ConfigState, Vec<LayerMtime>)> {
    // Step 1：定位 CODEX_HOME 并加载全部配置层（不含 cwd/CLI override）。
    let codex_home = find_codex_home().context("failed to resolve CODEX_HOME")?;
    let cli_overrides = Vec::new();
    let overrides = LoaderOverrides::default();
    let config_layer_stack = load_config_layers_state(
        LOCAL_FS.as_ref(),
        &codex_home,
        /*cwd*/ None,
        &cli_overrides,
        overrides,
        CloudRequirementsLoader::default(),
        &codex_config::NoopThreadConfigLoader,
    )
    .await
    .context("failed to load Codex config")?;

    // Step 2：加载 execpolicy。解析失败时降级为空策略 + 告警，避免一处策略
    // 语法错误就让整个网络代理起不来；非解析类错误仍直接失败。
    let (exec_policy, warning) = match load_exec_policy(&config_layer_stack).await {
        Ok(policy) => (policy, None),
        Err(err @ ExecPolicyError::ParsePolicy { .. }) => {
            (codex_execpolicy::Policy::empty(), Some(err))
        }
        Err(err) => return Err(err.into()),
    };
    if let Some(err) = warning.as_ref() {
        tracing::warn!(
            "failed to parse execpolicy while building network proxy state: {}",
            format_exec_policy_error_with_source(err)
        );
    }

    // Step 3：把分层配置 + execpolicy 解析为最终网络配置。
    let config = config_from_layers(&config_layer_stack, &exec_policy)?;

    // Step 4：用「受信任层」推导的约束校验最终配置不越界（防用户层提权），
    // 收集各层 mtime 快照，最后组装运行态 `ConfigState`。
    let constraints = enforce_trusted_constraints(&config_layer_stack, &config)?;
    let layer_mtimes = collect_layer_mtimes(&config_layer_stack);
    let state = build_config_state(config, constraints)?;
    Ok((state, layer_mtimes))
}

fn collect_layer_mtimes(stack: &ConfigLayerStack) -> Vec<LayerMtime> {
    stack
        .get_layers(
            ConfigLayerStackOrdering::LowestPrecedenceFirst,
            /*include_disabled*/ false,
        )
        .iter()
        .filter_map(|layer| {
            let path = match &layer.name {
                ConfigLayerSource::System { file } => Some(file.clone()),
                ConfigLayerSource::User { file, .. } => Some(file.clone()),
                ConfigLayerSource::Project { dot_codex_folder } => {
                    Some(dot_codex_folder.join(CONFIG_TOML_FILE))
                }
                ConfigLayerSource::LegacyManagedConfigTomlFromFile { file } => Some(file.clone()),
                _ => None,
            };
            path.map(LayerMtime::new)
        })
        .collect()
}

/// 推导并施加受信任约束：从受信任层算出 `NetworkProxyConstraints`，再校验
/// 最终（含用户层的）配置不违反这些约束。违反即报错，阻止启动。
fn enforce_trusted_constraints(
    layers: &ConfigLayerStack,
    config: &NetworkProxyConfig,
) -> Result<NetworkProxyConstraints> {
    let constraints = network_constraints_from_trusted_layers(layers)?;
    validate_policy_against_constraints(config, &constraints)
        .map_err(NetworkProxyConstraintError::into_anyhow)
        .context("network proxy constraints")?;
    Ok(constraints)
}

/// 仅从「非用户可控」的受信任层推导网络约束：跳过 User/Project/SessionFlags，
/// 只合并 System 等层，再解析出其中的网络设置作为不可逾越的上界。
/// 这样用户层即便写了更宽松的网络配置，也会在 `enforce_trusted_constraints`
/// 的校验中被挡下。
fn network_constraints_from_trusted_layers(
    layers: &ConfigLayerStack,
) -> Result<NetworkProxyConstraints> {
    let mut constraints = NetworkProxyConstraints::default();
    let mut merged = toml::Value::Table(toml::map::Map::new());
    for layer in layers.get_layers(
        ConfigLayerStackOrdering::LowestPrecedenceFirst,
        /*include_disabled*/ false,
    ) {
        // 跳过用户可控层：约束只能来自受信任来源，否则形同虚设。
        if is_user_controlled_layer(&layer.name) {
            continue;
        }

        merge_toml_values(&mut merged, &layer.config);
    }

    let parsed = network_tables_from_toml(&merged)?;
    if let Some(network) = selected_network_from_tables(parsed)? {
        apply_network_constraints(network, &mut constraints);
    }
    Ok(constraints)
}

fn apply_network_constraints(network: NetworkToml, constraints: &mut NetworkProxyConstraints) {
    if let Some(enabled) = network.enabled {
        constraints.enabled = Some(enabled);
    }
    if let Some(mode) = network.mode {
        constraints.mode = Some(mode);
    }
    if let Some(allow_upstream_proxy) = network.allow_upstream_proxy {
        constraints.allow_upstream_proxy = Some(allow_upstream_proxy);
    }
    if let Some(dangerously_allow_non_loopback_proxy) = network.dangerously_allow_non_loopback_proxy
    {
        constraints.dangerously_allow_non_loopback_proxy =
            Some(dangerously_allow_non_loopback_proxy);
    }
    if let Some(dangerously_allow_all_unix_sockets) = network.dangerously_allow_all_unix_sockets {
        constraints.dangerously_allow_all_unix_sockets = Some(dangerously_allow_all_unix_sockets);
    }
    if let Some(domains) = network.domains.as_ref() {
        let mut config = NetworkProxyConfig::default();
        if let Some(allowed_domains) = constraints.allowed_domains.take() {
            config.network.set_allowed_domains(allowed_domains);
        }
        if let Some(denied_domains) = constraints.denied_domains.take() {
            config.network.set_denied_domains(denied_domains);
        }
        overlay_network_domain_permissions(&mut config, domains);
        constraints.allowed_domains = config.network.allowed_domains();
        constraints.denied_domains = config.network.denied_domains();
    }
    if let Some(unix_sockets) = network.unix_sockets.as_ref() {
        let allow_unix_sockets = unix_sockets.allow_unix_sockets();
        constraints.allow_unix_sockets = Some(allow_unix_sockets);
    }
    if let Some(allow_local_binding) = network.allow_local_binding {
        constraints.allow_local_binding = Some(allow_local_binding);
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct NetworkTablesToml {
    default_permissions: Option<String>,
    permissions: Option<PermissionsToml>,
}

fn network_tables_from_toml(value: &toml::Value) -> Result<NetworkTablesToml> {
    value
        .clone()
        .try_into()
        .context("failed to deserialize network tables from config")
}

/// 从合并后的配置表中解析出「当前生效的网络设置」。逻辑：
///   - 未设 `default_permissions` → 无自定义网络配置，返回 None；
///   - 是内建 profile 名（如内建的标准权限档）→ 网络由内建逻辑处理，返回 None；
///   - 否则按名解析自定义 `[permissions]` profile，取其 `network` 段。
/// `reject_unknown_builtin_permission_profile` 拦截误用内建命名空间的未知名。
fn selected_network_from_tables(parsed: NetworkTablesToml) -> Result<Option<NetworkToml>> {
    let Some(default_permissions) = parsed.default_permissions else {
        return Ok(None);
    };
    if is_builtin_permission_profile_name(&default_permissions) {
        return Ok(None);
    }
    reject_unknown_builtin_permission_profile(&default_permissions)?;

    let permissions = parsed
        .permissions
        .context("default_permissions requires a `[permissions]` table for network settings")?;
    let profile = resolve_permission_profile(&permissions, &default_permissions)
        .map_err(anyhow::Error::from)?;
    Ok(profile.profile.network)
}

#[cfg(test)]
fn apply_network_tables(config: &mut NetworkProxyConfig, parsed: NetworkTablesToml) -> Result<()> {
    if let Some(network) = selected_network_from_tables(parsed)? {
        network.apply_to_network_proxy_config(config);
    }
    Ok(())
}

/// 网络配置累加器：把（合并后的）网络设置吸收进 `config`，并把 MITM 的
/// hooks/actions 单独累积到有序表（`IndexMap` 保序，命名引用按定义顺序解析），
/// 最后 `finish` 时再编译成运行态钩子并校正 `mitm` 开关。
#[derive(Default)]
struct NetworkConfigAccumulator {
    config: NetworkProxyConfig,
    mitm_hooks: IndexMap<String, NetworkMitmHookToml>,
    mitm_actions: IndexMap<String, NetworkMitmActionToml>,
}

impl NetworkConfigAccumulator {
    fn apply_network_tables(&mut self, parsed: NetworkTablesToml) -> Result<()> {
        if let Some(network) = selected_network_from_tables(parsed)? {
            self.apply_network(network);
        }
        Ok(())
    }

    /// 吸收一份网络设置：先把 MITM 段摘出（hooks/actions 单独累积），其余字段
    /// 直接应用到 `config`。MITM 单独处理是因为它需要跨「钩子 ↔ 命名 action」
    /// 做引用解析，不能简单字段覆盖。
    fn apply_network(&mut self, mut network: NetworkToml) {
        let mitm = network.mitm.take();
        network.apply_to_network_proxy_config(&mut self.config);

        if let Some(mitm) = mitm {
            if let Some(actions) = mitm.actions {
                self.mitm_actions.extend(actions);
            }
            if let Some(hooks) = mitm.hooks {
                self.mitm_hooks.extend(hooks);
            }
        }
    }

    /// 收尾：把累积的 MITM 钩子按命名 action 引用解析、校验引用合法后编成运行态
    /// 钩子写回 `config`，并自动校正 `mitm` 开关——limited 模式或存在钩子时必须
    /// 开启 MITM（前者要解密以强制方法白名单，后者要解密以改写请求）。
    fn finish(mut self) -> Result<NetworkProxyConfig> {
        if !self.mitm_hooks.is_empty() {
            let actions = self.mitm_actions;
            let mitm = NetworkMitmToml {
                hooks: Some(self.mitm_hooks),
                actions: Some(actions.clone()),
            };
            mitm.validate_action_references(&actions)
                .map_err(anyhow::Error::msg)?;
            self.config.network.mitm_hooks = mitm.to_runtime_hooks(Some(&actions));
        }

        self.config.network.mitm = self.config.network.mode == NetworkMode::Limited
            || !self.config.network.mitm_hooks.is_empty();
        Ok(self.config)
    }
}

/// 把全部配置层（含用户层）合并并解析为最终 `NetworkProxyConfig`，再叠加
/// execpolicy 推导出的域名 allow/deny 规则。与 `network_constraints_from_trusted_layers`
/// 的区别：这里产出「实际生效配置」，那里只取受信任层产出「约束上界」。
fn config_from_layers(
    layers: &ConfigLayerStack,
    exec_policy: &codex_execpolicy::Policy,
) -> Result<NetworkProxyConfig> {
    // 按优先级从低到高合并，使高优先级层覆盖低优先级层（标准分层语义）。
    let mut merged = toml::Value::Table(toml::map::Map::new());
    for layer in layers.get_layers(
        ConfigLayerStackOrdering::LowestPrecedenceFirst,
        /*include_disabled*/ false,
    ) {
        merge_toml_values(&mut merged, &layer.config);
    }
    let parsed = network_tables_from_toml(&merged)?;
    let mut accumulator = NetworkConfigAccumulator::default();
    accumulator.apply_network_tables(parsed)?;
    let mut config = accumulator.finish()?;
    apply_exec_policy_network_rules(&mut config, exec_policy);
    Ok(config)
}

/// 把 execpolicy 编译出的网络域名规则叠加到配置：allow 域名 upsert 为 Allow，
/// deny 域名 upsert 为 Deny。让 execpolicy 与 `[network]` 配置共同决定域名策略。
fn apply_exec_policy_network_rules(
    config: &mut NetworkProxyConfig,
    exec_policy: &codex_execpolicy::Policy,
) {
    let (allowed_domains, denied_domains) = exec_policy.compiled_network_domains();
    for host in allowed_domains {
        upsert_network_domain(
            config,
            host,
            codex_network_proxy::NetworkDomainPermission::Allow,
        );
    }
    for host in denied_domains {
        upsert_network_domain(
            config,
            host,
            codex_network_proxy::NetworkDomainPermission::Deny,
        );
    }
}

fn upsert_network_domain(
    config: &mut NetworkProxyConfig,
    host: String,
    permission: codex_network_proxy::NetworkDomainPermission,
) {
    config
        .network
        .upsert_domain_permission(host, permission, normalize_host);
}

/// 判定某配置层是否「用户可控」——即终端用户能任意编辑的层。这是信任边界的
/// 定义：User（用户 config）、Project（仓库内 `.codex`）、SessionFlags（本次会话
/// 标志）都算用户可控；System 等其余层视为受信任，用于推导网络约束上界。
fn is_user_controlled_layer(layer: &ConfigLayerSource) -> bool {
    matches!(
        layer,
        ConfigLayerSource::User { .. }
            | ConfigLayerSource::Project { .. }
            | ConfigLayerSource::SessionFlags
    )
}

/// 单个配置层文件的 mtime 快照：记录路径与上次读到的修改时间，重载器据此
/// 判断文件是否被改动。`mtime` 为 `None` 表示当时文件不存在或读不到时间。
#[derive(Clone)]
struct LayerMtime {
    path: AbsolutePathBuf,
    mtime: Option<std::time::SystemTime>,
}

impl LayerMtime {
    fn new(path: AbsolutePathBuf) -> Self {
        let mtime = path.metadata().and_then(|m| m.modified()).ok();
        Self { path, mtime }
    }
}

/// 基于文件 mtime 的配置热重载器：持有各配置层的 mtime 快照，`maybe_reload`
/// 时比对磁盘上是否有层被修改，有则重建 `ConfigState` 并刷新快照。
/// 用 mtime 而非 inotify 之类，是为跨平台简单可靠（轮询触发，无需文件监听）。
pub struct MtimeConfigReloader {
    layer_mtimes: RwLock<Vec<LayerMtime>>,
}

impl MtimeConfigReloader {
    fn new(layer_mtimes: Vec<LayerMtime>) -> Self {
        Self {
            layer_mtimes: RwLock::new(layer_mtimes),
        }
    }

    /// 是否需要重载：任一层文件的当前 mtime 比快照更新，或出现/消失，即判定
    /// 需要重载。四种组合：变新→是；原本不存在现在出现→是；原本存在现在消失
    /// →是；始终不存在→否。
    async fn needs_reload(&self) -> bool {
        let guard = self.layer_mtimes.read().await;
        guard.iter().any(|layer| {
            let metadata = std::fs::metadata(&layer.path).ok();
            match (metadata.and_then(|m| m.modified().ok()), layer.mtime) {
                (Some(new_mtime), Some(old_mtime)) => new_mtime > old_mtime,
                (Some(_), None) => true,
                (None, Some(_)) => true,
                (None, None) => false,
            }
        })
    }
}

#[async_trait]
impl ConfigReloader for MtimeConfigReloader {
    fn source_label(&self) -> String {
        "config layers".to_string()
    }

    /// 按需重载：先 `needs_reload` 判定，无变化返回 `None`（调用方据此跳过）；
    /// 有变化则重建状态并刷新 mtime 快照后返回新状态。
    async fn maybe_reload(&self) -> Result<Option<ConfigState>> {
        if !self.needs_reload().await {
            return Ok(None);
        }

        let (state, layer_mtimes) = build_config_state_with_mtimes().await?;
        let mut guard = self.layer_mtimes.write().await;
        *guard = layer_mtimes;
        Ok(Some(state))
    }

    /// 强制重载：不看 mtime 直接重建并刷新快照，用于显式触发的场景。
    async fn reload_now(&self) -> Result<ConfigState> {
        let (state, layer_mtimes) = build_config_state_with_mtimes().await?;
        let mut guard = self.layer_mtimes.write().await;
        *guard = layer_mtimes;
        Ok(state)
    }
}

#[cfg(test)]
#[path = "network_proxy_loader_tests.rs"]
mod tests;
