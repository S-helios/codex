# Codex 分层配置系统：优先级、Profile 与覆盖机制

> 本文档回答：Codex 的一份"有效配置"到底是从哪些文件、按什么顺序拼出来的？
> 为什么项目目录里的 `.codex/config.toml` 不能改你的 API 地址？admin 强制的约束怎么做到"不可被覆盖"？
> CLI 的 `-c` 覆盖、Profile 配置集、运行时 `ThreadSettingsOverrides` 三者各管什么、谁压谁？

---

## 目录

1. [一句话总览：两条独立的优先级链](#1-一句话总览两条独立的优先级链)
2. [配置来源与加载顺序（优先级层次）](#2-配置来源与加载顺序优先级层次)
3. [`ConfigLayerStack`：分层配置栈数据结构](#3-configlayerstack分层配置栈数据结构)
4. [完整加载流程：`load_config_layers_state`](#4-完整加载流程load_config_layers_state)
5. [Profile（配置集）机制](#5-profile配置集机制)
6. [安全边界：项目层禁用清单与 admin 约束](#6-安全边界项目层禁用清单与-admin-约束)
7. [关键配置结构体与字段](#7-关键配置结构体与字段)
8. [`ThreadSettingsOverrides`：运行时逐回合覆盖](#8-threadsettingsoverrides运行时逐回合覆盖)
9. [`ConfigLock`：冻结与回放有效配置](#9-configlock冻结与回放有效配置)
10. [配置应用生命周期：从磁盘到一次 Turn](#10-配置应用生命周期从磁盘到一次-turn)
11. [常见问题](#11-常见问题)
12. [核心代码路径索引](#12-核心代码路径索引)

---

## 1. 一句话总览：两条独立的优先级链

Codex 的配置不是"一份文件"，而是把**多个来源**（MDM、系统、用户、Profile、项目、CLI、旧版 managed）按优先级**叠加合并**出来的。理解整个系统最关键的一点是：**它分成两条互不混淆的链**。

| 链 | 是什么 | 谁定义 | 能否被后续层覆盖 |
|------|--------|--------|------------------|
| **Requirements（约束）** | admin/系统强制的红线，例如"只允许这些 sandbox 模式" | cloud / macOS MDM / `requirements.toml` | **不能**——早层约束晚层无法推翻 |
| **Configuration（配置）** | 普通可调项，例如 model、approval_policy | system / user / profile / project / CLI / 旧 managed | 能——高优先级层覆盖低优先级层 |

这两条链在 `codex-rs/config/src/loader/mod.rs:78-109` 的文档注释里被明确分开描述。Requirements 是"先建立约束清单，早层约束晚层不可推翻"；Configuration 是"后层覆盖前层"。二者最终都被装进同一个 `ConfigLayerStack`，但语义完全不同：Configuration 解决"取哪个值"，Requirements 解决"允不允许取这个值"。

> 与许多工具"一个 config 文件 + 命令行参数"的扁平模型相比，Codex 选择分层是因为它要同时服务三类主体：**企业管理员**（强制安全策略）、**用户**（个人偏好）、**仓库**（项目约定）。三者诉求会冲突，必须用优先级 + 不可覆盖约束来仲裁。

---

## 2. 配置来源与加载顺序（优先级层次）

### 2.1 数值化的优先级：`ConfigLayerSource::precedence()`

每一个配置层都带一个来源标签 `ConfigLayerSource`（枚举，定义在 `codex-rs/app-server-protocol/src/protocol/v2/config.rs:27-83`）。它的 `precedence()` 方法（同文件 `:88-104`）给每种来源分配一个 `i16` 数值，**数值大者覆盖数值小者**：

```rust
// app-server-protocol/src/protocol/v2/config.rs:88-104
pub fn precedence(&self) -> i16 {
    match self {
        ConfigLayerSource::Mdm { .. } => 0,
        ConfigLayerSource::System { .. } => 10,
        ConfigLayerSource::User { profile, .. } => {
            if profile.is_some() { 21 } else { 20 }   // base=20, profile 覆盖层=21
        }
        ConfigLayerSource::Project { .. } => 25,
        ConfigLayerSource::SessionFlags => 30,                       // CLI -c / UI 选择器
        ConfigLayerSource::LegacyManagedConfigTomlFromFile { .. } => 40,
        ConfigLayerSource::LegacyManagedConfigTomlFromMdm => 50,
    }
}
```

完整优先级链（**后者覆盖前者**）：

```
低 ─────────────────────────────────────────────────────────────────────→ 高
 MDM(0)  System(10)  User-base(20)  User-profile(21)  Project(25)  SessionFlags(30)  LegacyFile(40)  LegacyMdm(50)
  │         │           │              │                 │             │                │               │
 macOS    /etc/codex  $CODEX_HOME    $CODEX_HOME/      cwd 及向上的    CLI -c /        managed_       managed_config
 MDM      /config     /config.toml   <name>.config    .codex/config   UI 模型选择器    config.toml     来自 MDM
          .toml                      .toml（选中时）   .toml（树形）                   （旧方案）       （旧方案）
```

### 2.2 七种来源逐一说明

| 来源（`precedence`） | 路径 / 出处 | 角色 |
|----------------------|-------------|------|
| `Mdm`（0） | macOS 托管设备配置（domain/key） | 企业 MDM 下发的偏好，优先级最低（仅作默认建议） |
| `System`（10） | Unix: `/etc/codex/config.toml`（`loader/mod.rs:52`） | 系统级配置 |
| `User`（20，无 profile） | `$CODEX_HOME/config.toml` | 用户基础配置，**可写、通常在工作区之外** |
| `User`（21，有 profile） | `$CODEX_HOME/<name>.config.toml` | Profile-v2 覆盖层（见 §5） |
| `Project`（25） | `cwd/.codex/config.toml` 及沿目录树向上至 git root | 项目级；**untrusted 时被加载但标记禁用** |
| `SessionFlags`（30） | CLI `-c key=value` / UI 模型选择器 / 服务端线程配置 | 运行时覆盖 |
| `LegacyManagedConfigTomlFromFile`（40） | `managed_config.toml` 文件 | 旧版 managed 方案，**正在被 `requirements.toml` 取代** |
| `LegacyManagedConfigTomlFromMdm`（50） | 来自 MDM 的 managed_config | 旧版 managed 方案，优先级最高（兜底兼容） |

> **设计要点 1（向后兼容的尴尬）**：`SessionFlags(30)` 比 `Project(25)` 高，所以 CLI `-c` 能覆盖项目配置；但它又**低于** `LegacyManaged(40/50)`。这意味着用户在命令行临时改的值，仍可能被旧的 `managed_config.toml` 压制。源码注释（`config.rs:72-75`、`loader/mod.rs:338-342`）直言这套 managed 方案"没完全达到预期效果"，保留它纯粹是"尽力而为"地兼容老部署，新部署应迁移到 `requirements.toml`。
>
> **设计要点 2（命名陷阱）**：`ConfigLayerSource::System` 这个变体上方的 doc 注释写的是 "Managed config layer from a file"，但实际它承载的是 `/etc/codex/config.toml`（`loader/mod.rs:195-212`）。注释与变体名略有出入，以代码用法为准——它就是"系统级 config.toml 层"。

---

## 3. `ConfigLayerStack`：分层配置栈数据结构

所有层最终装进 `ConfigLayerStack`（`codex-rs/config/src/state.rs:211-242`）：

```rust
// config/src/state.rs:211-242（节选）
pub struct ConfigLayerStack {
    /// 层按"最低优先级在前、最高在后"排列；Vec 中靠后的条目覆盖靠前的。
    layers: Vec<ConfigLayerEntry>,

    /// 指向"活跃用户层"的下标。当 profile 激活时，可能有两个用户层
    /// （base + profile 覆盖），此下标指向最高优先级的那个，因为那是
    /// "profile 感知编辑"时的可写目标。
    user_layer_index: Option<usize>,

    /// 从 layers 推导 Config 时必须强制执行的约束（Requirements 链）。
    requirements: ConfigRequirements,

    /// 原始 requirements 数据（保留 allow-list 以便通过 API 暴露）。
    requirements_toml: ConfigRequirementsToml,

    /// execpolicy 是否跳过 user/project 文件夹下的 .rules 文件。
    ignore_user_and_project_exec_policy_rules: bool,

    /// 构建栈时发现的启动警告。None=没检查；Some(vec![])=检查了但无警告。
    startup_warnings: Option<Vec<String>>,
}
```

几个关键不变量与方法：

- **排序自检**：`ConfigLayerStack::new()`（`state.rs:244-259`）调用 `verify_layer_ordering()`（`state.rs:516`）确认 `layers` 确实按优先级单调排列，并顺便算出 `user_layer_index`。
- **取值合并方向**：`layers` Vec 从低到高，合并时靠后覆盖靠前（注释 `state.rs:213-215`）。
- **`get_active_user_layer()`**（`state.rs:288-291`）：返回**最高优先级**的用户层——profile 激活时返回 profile 层而非 base，因为编辑要写进 profile 文件。
- **`effective_user_config()`**（`state.rs:321-335`）：只合并"启用的用户层"，profile 激活时 = base 合并 profile 覆盖。
- **`with_user_config_profile()`**（`state.rs:361-407`）：返回一个新栈，替换/插入指定 profile 的用户层（用于运行时切 profile、改用户配置）。

```
ConfigLayerStack
├── layers: [System(10), User-base(20), User-profile(21), Project(25), SessionFlags(30), ...]  ← 低→高
│                                         ▲
│                                  user_layer_index 指这里（profile 激活时）
├── requirements: ConfigRequirements        ← 不可覆盖的约束链（独立来源）
├── requirements_toml: ...                   ← 原始 allow-list（供 API 暴露）
└── startup_warnings: Some([...])            ← untrusted 项目、被忽略的项目 key 等
```

---

## 4. 完整加载流程：`load_config_layers_state`

整套栈由 `load_config_layers_state()` 构建（`codex-rs/config/src/loader/mod.rs:111-119` 是签名，函数体延伸到 `:389`）：

```rust
// config/src/loader/mod.rs:111-119
pub async fn load_config_layers_state(
    fs: &dyn ExecutorFileSystem,
    codex_home: &Path,
    cwd: Option<AbsolutePathBuf>,                  // 线程绑定 cwd；/config 端点为 None
    cli_overrides: &[(String, TomlValue)],         // CLI -c key=value
    options: impl Into<ConfigLoadOptions>,         // loader_overrides + strict_config
    cloud_requirements: CloudRequirementsLoader,   // 云端约束加载器
    thread_config_loader: &dyn ThreadConfigLoader, // 服务端线程级配置注入点
) -> io::Result<ConfigLayerStack>
```

执行步骤（对照源码行号）：

```
①  加载 Requirements 链（除非 ignore_managed_requirements）             loader/mod.rs:131-152
    ├─ CloudRequirements（远端）                                        :132-138
    ├─ macOS MDM admin requirements                                    :140-147
    └─ system requirements.toml（/etc/codex/requirements.toml）         :149-151
②  load_config_layers_internal：读 managed_config.toml（旧方案）        :156-157
    └─ 把 legacy managed 映射成 requirements（向后兼容）                :159-165
③  通过 ThreadConfigLoader 拿线程级 config 层（cwd 注入 context）       :167-174
④  构造 cli_overrides_layer（若 -c 非空，strict 时做严格校验）          :178-193
⑤  push System 层（/etc/codex/config.toml）                            :195-212
⑥  push User-base 层（$CODEX_HOME/config.toml）                        :217-249
    └─ 若选了 profile 但 base 里还有 legacy [profiles.x] → 报错拒绝     :227-248
⑦  若 active_user_file ≠ base（即选了 profile）→ push User-profile 层  :251-262
⑧  若 cwd 存在：合并已有层算 project_root_markers → 加载 project 层     :264-324
    └─ cwd 及向上至 root 的每个 .codex/config.toml；untrusted 标禁用    load_project_layers :1128-
⑨  push SessionFlags 层（cli_overrides_layer）                         :326-332
⑩  按 precedence 插入 thread_config_layers                             :334-336
⑪  push LegacyManaged 层（file=40 / mdm=50），放在最顶                  :338-377
⑫  ConfigLayerStack::new(layers, requirements, requirements_toml)      :379-388
```

注意 `cwd` 是 `Option`：只有"线程绑定的配置加载"才有 cwd；app-server 的 `/config` 端点这类"与线程无关"的加载会传 `None`，此时跳过整个项目层逻辑（注释 `loader/mod.rs:106-109`）。

### 4.1 项目层的树形搜索与信任门控

`load_project_layers()`（`loader/mod.rs:1128`）从 `cwd` 沿 `ancestors()` 向上扫描，直到 `project_root`（通常是 git 顶层），对每个含 `.codex/` 的目录加载 `config.toml`。关键：**untrusted 目录的配置仍会被加载，但带 `disabled_reason` 标记为禁用**（`:1167-1168`、`:1247`），并把"被忽略的项目 key"记入 `startup_warnings`。这样 UI 能告诉用户"这个项目还没被信任，它的配置没生效"，而不是静默丢弃。信任级别 `TrustLevel`、`.codex/` 树形搜索的更多细节与沙箱 cwd 解析相关，见 [doc13 沙箱机制](../3_执行与安全/13_sandbox_mechanism.md)。

---

## 5. Profile（配置集）机制

### 5.1 Profile 是什么

`ConfigProfile`（`codex-rs/config/src/profile_toml.rs:22-71`）是"一组可成套切换的常用配置项"：

```rust
// config/src/profile_toml.rs:22-71（节选）
pub struct ConfigProfile {
    pub model: Option<String>,
    pub service_tier: Option<String>,        // default / priority / flex
    pub model_provider: Option<String>,
    pub approval_policy: Option<AskForApproval>,
    pub approvals_reviewer: Option<ApprovalsReviewer>,
    pub sandbox_mode: Option<SandboxMode>,
    pub model_reasoning_effort: Option<ReasoningEffort>,
    pub model_reasoning_summary: Option<ReasoningSummary>,
    pub model_verbosity: Option<Verbosity>,
    pub personality: Option<Personality>,
    pub chatgpt_base_url: Option<String>,
    // ... include_*_instructions / tools / web_search / analytics / tui / features 等
}
```

每个字段都是 `Option`：只有显式设置的项才参与覆盖，未设置的保持下层值不变。`#[schemars(deny_unknown_fields)]` 保证 profile 里写错的键会被 schema 拒绝。

### 5.2 Profile-v2 是"覆盖层"而非"替代品"（重要）

这是最容易误解的一点。Profile-v2 **不是**"换一份完整配置"，而是**在 base user config 之上叠一层只含差异项的覆盖**：

```
$CODEX_HOME/config.toml           ← User-base 层（precedence=20），必须存在
$CODEX_HOME/myprofile.config.toml ← User-profile 层（precedence=21），只写要改的项
                                     两层都生效；profile 覆盖 base
```

源码体现在 `loader/mod.rs:214-262`：先无条件 push base user 层（`:219-249`），再"若 `active_user_file != base_user_file`"才额外 push profile 层（`:251-262`）。`load_user_config_layer()`（`:391-419`）把 profile 名塞进 `ConfigLayerSource::User { file, profile }`，从而拿到 precedence=21。

合并时 `effective_user_config()`（`state.rs:321-335`）按 `LowestPrecedenceFirst` 先 base 后 profile 依次 merge，所以 profile 缺省的字段自动回落到 base。

### 5.3 Profile-v1（legacy）与 v2 不兼容并存

旧版 profile 是写在 `config.toml` 内的 `[profiles.<name>]` 表 + `profile = "<name>"` 选择器。新版 v2 把 profile 拆成独立文件。两者**不能混用**：若你用 `--profile foo` 选了 v2，但 base `config.toml` 里又有 legacy `profile = "foo"` 或 `[profiles.foo]`，加载会直接报错（`loader/mod.rs:227-248`），提示你把设置迁到 `foo.config.toml` 并删除 legacy 选择器。

### 5.4 `user_layer_index` 指向哪个用户层

`ConfigLayerStack::user_layer_index`（`state.rs:217-223`）在 profile 激活时指向**profile 层**（precedence=21）而非 base，因为那是"profile 感知编辑"的可写目标——用户在 UI 里改设置，应写进 profile 文件而非污染 base。

---

## 6. 安全边界：项目层禁用清单与 admin 约束

### 6.1 `PROJECT_LOCAL_CONFIG_DENYLIST`

项目配置来自仓库内容，而仓库可能是不可信的第三方代码。因此**项目层禁止设置一批敏感项**（`loader/mod.rs:57-72`）：

```rust
// config/src/loader/mod.rs:57-72
const PROJECT_LOCAL_CONFIG_DENYLIST: &[&str] = &[
    "openai_base_url",       // 不能改 API 地址（防凭据外泄）
    "chatgpt_base_url",
    "apps_mcp_product_sku",
    "model_provider",        // 不能换模型提供商
    "model_providers",
    "notify",                // 不能注入本地命令
    "profile",               // 不能改 profile 选择
    "profiles",
    "experimental_realtime_ws_base_url",
    "otel",
];
```

注释（`:57-60`）说得很直白："项目本地配置来自仓库内容，所以它不该决定用户的凭据发往哪里、或运行什么本地命令。"这些项仍可在 user/system/managed/runtime 层设置——只是 project 层无权。一个克隆下来的恶意仓库无法借 `.codex/config.toml` 把你的请求重定向到攻击者服务器。

### 6.2 Requirements 不可被覆盖

admin/cloud/system 通过 `requirements.toml`（或旧 `managed_config.toml`）下发的约束装进 `ConfigRequirements`（`config/src/config_requirements.rs:86`），与 Configuration 链分离。注释 `loader/mod.rs:78-89` 明确："早层定义的约束不能被后层推翻。" 例如 admin 限定 `allowed_sandbox_modes` 后，无论用户怎么在 config.toml 写 `sandbox_mode`，超出 allow-list 的值都会在从栈派生 `Config` 时被拒绝。这是一条独立于"取值优先级"的硬门控。

---

## 7. 关键配置结构体与字段

配置在内存里有两种形态，必须区分：

| 形态 | 类型 | 角色 |
|------|------|------|
| **反序列化层** | `ConfigToml`（`config/src/config_toml.rs`） | 直接对应 TOML 文件的字段，全是 `Option`，未解析 |
| **已解决层** | `Config`（`core/src/config/mod.rs:544`） | 合并所有层 + 应用约束 + 特性特化后，字段已具体化 |

`Config` 核心字段（`core/src/config/mod.rs:544-650`，节选）：

```rust
// core/src/config/mod.rs:544-650（节选）
pub struct Config {
    /// 来源溯源：这份 Config 是由哪些层合并 + 哪些约束派生出来的。
    pub config_layer_stack: ConfigLayerStack,
    pub startup_warnings: Vec<String>,
    pub model: Option<String>,
    pub service_tier: Option<String>,           // 新回合的有效 service tier
    pub model_provider_id: String,              // model_providers 里的 key（已解析）
    pub model_provider: ModelProviderInfo,      // 发请求所需的完整 provider 信息
    pub personality: Option<Personality>,
    pub permissions: Permissions,               // shell 工具执行的有效权限配置
    /// config 是否显式选了命名权限 profile，而非 legacy sandbox_mode 语法。
    pub explicit_permission_profile_mode: bool,
    pub custom_permission_profiles: Vec<CustomPermissionProfileSummary>,
    pub approvals_reviewer: ApprovalsReviewer,
    // ... include_*_instructions / hide_agent_reasoning / user_instructions 等
}
```

注意 `Config` 自带 `config_layer_stack`——它把"溯源信息"一起带着，这样运行时改配置（切 profile、改权限）能基于原始栈重新派生，而不是丢失来源。`ConfigToml → Config` 的转换由 `load_config_with_layer_stack()`（`core/src/config/mod.rs:2436`）完成，它应用所有约束和特性特化，产出 model/permissions/service_tier/model_provider_info 等已具体化的值。

> `load_from_base_config_with_overrides()`（`config/mod.rs:2419-2434`）是**测试辅助**入口，用 `ConfigLayerStack::default()` 且**忽略 requirements 强制**（见其注释 `:2424`）。生产路径走 `load_config_with_layer_stack()`，别把测试入口当成主流程。

---

## 8. `ThreadSettingsOverrides`：运行时逐回合覆盖

前面 §2-§7 都是"启动时从磁盘加载"的静态配置。但用户在会话中途想换模型、调 effort、改 sandbox 怎么办？这就是 `ThreadSettingsOverrides`（`codex-rs/protocol/src/protocol.rs:423-496`）——**持久化的线程级运行时覆盖**：

```rust
// protocol/src/protocol.rs:423-496（节选）
pub struct ThreadSettingsOverrides {
    pub cwd: Option<PathBuf>,                                 // sandbox/工具调用的 cwd
    pub workspace_roots: Option<Vec<AbsolutePathBuf>>,       // 用于符号化 :workspace_roots 权限
    pub profile_workspace_roots: Option<Vec<AbsolutePathBuf>>, // 用于状态摘要/逐回合配置重建
    pub approval_policy: Option<AskForApproval>,
    pub approvals_reviewer: Option<ApprovalsReviewer>,
    pub sandbox_policy: Option<SandboxPolicy>,
    pub permission_profile: Option<PermissionProfile>,
    pub active_permission_profile: Option<ActivePermissionProfile>,
    pub windows_sandbox_level: Option<WindowsSandboxLevel>,
    pub model: Option<String>,
    pub effort: Option<Option<ReasoningEffortConfig>>,       // 注意双层 Option！
    pub summary: Option<ReasoningSummaryConfig>,
    pub service_tier: Option<Option<String>>,                // 双层 Option
    pub collaboration_mode: Option<CollaborationMode>,       // 设了就盖过 model/effort/指令
    pub personality: Option<Personality>,
}
```

### 8.1 `Option<Option<T>>` 的三态语义（重要陷阱）

`effort` 和 `service_tier` 用了**双层 Option**，源码注释（`protocol.rs:470-475`、`:481-486`）解释了三种语义：

| 写法 | 含义 |
|------|------|
| `Some(Some(v))` | 设为具体值 `v` |
| `Some(None)` | **显式清除**（恢复默认 / 让模型自己定） |
| `None` | 保持现有值不变 |

外层 Option 区分"这次更新有没有提到这个字段"，内层 Option 区分"设值还是清空"。单层 Option 做不到"显式清空"与"不动"的区分——这正是为什么只有需要"清空"语义的字段（effort、service_tier）才用双层。

### 8.2 `workspace_roots` vs `profile_workspace_roots`

两个 roots 字段分工不同（注释 `protocol.rs:431-439`）：
- `workspace_roots`：运行时用来**物化** `:workspace_roots` 这种符号化文件系统权限（实际授权哪些目录可写）。
- `profile_workspace_roots`：profile 定义的 roots，用于**状态摘要展示**和**逐回合 config 重建快照**。

### 8.3 `collaboration_mode` 的优先级

`collaboration_mode` 一旦设置，**优先于 model、effort 和 developer 指令**（注释 `protocol.rs:488-489`）。这是个实验性的"预设协作模式"，整套覆盖里它是最强的。

### 8.4 应用路径：`thread_settings_update()`

`Op::UserInput`（`protocol.rs:559-578`）把输入项和 `thread_settings`（`#[serde(flatten)]`）**捆绑在同一条 SQ 提交里**。注释 `:555-558` 道出原因：设置与回合启动共用同一条提交队列，**保证调用方顺序、杜绝"设置还没生效就开始回合"的竞态**。也有独立的 `Op::ThreadSettings`（`:580-587`）用于"只改设置不启动回合"，走同一队列以保序。

实际转换在 `core/src/session/handlers.rs:129-178` 的 `thread_settings_update()`：

```rust
// core/src/session/handlers.rs:129-161（节选）
async fn thread_settings_update(sess: &Session, thread_settings: ThreadSettingsOverrides)
    -> SessionSettingsUpdate
{
    let ThreadSettingsOverrides { cwd, workspace_roots, /* ...解构全部字段... */
        model, effort, summary, service_tier, collaboration_mode, personality } = thread_settings;
    let collaboration_mode = match collaboration_mode {
        Some(collaboration_mode) => collaboration_mode,
        None => {
            // collaboration_mode 没给 → 从 session state 读当前 mode，
            // 并把 model/effort 的更新合进去（model 和 effort 当前住在 CollaborationMode 里）
            let state = sess.state.lock().await;
            state.session_configuration.collaboration_mode
                .with_updates(model, effort, /*developer_instructions*/ None)
        }
    };
    SessionSettingsUpdate { cwd, /* ... */, collaboration_mode: Some(collaboration_mode), .. }
}
```

关键细节：`model` 和 `effort` 当前**寄居在 `CollaborationMode` 里**，所以当 `collaboration_mode` 字段为 `None` 时，handler 会读出会话当前的 mode，再用 `with_updates(model, effort, ...)` 把这次的 model/effort 合进去（`handlers.rs:150-160`）。

最终 `SessionSettingsUpdate` 在 `new_turn_with_sub_id(sub_id, updates)`（`handlers.rs:229`）里**原子应用**——在 Turn 真正开始前生效。`Op::UserInput` 的处理路径见 `handlers.rs:209-241`，会话 / Turn 生命周期见 [doc11 §10 Op 全枚举](../2_运行时核心/11_thread_session_turn_lifecycle.md#10-op-全枚举所有操作及其对生命周期的影响)。

### 8.5 服务端线程级配置注入：`ThreadConfigLoader`

除了用户的逐回合覆盖，**服务端**（如 app-server / cloud）也能注入线程级配置，通道是 `ThreadConfigLoader` trait（`config/src/thread_config.rs:89-113`）。它的载荷是 `SessionThreadConfig`（`thread_config.rs:24-30`）：

```rust
// config/src/thread_config.rs:24-30
pub struct SessionThreadConfig {
    pub model_provider: Option<String>,
    pub model_providers: HashMap<String, ModelProviderInfo>,
    pub features: BTreeMap<String, bool>,
}
```

`thread_config_source_to_layer()`（`thread_config.rs:151-171`）把它转成 `ConfigLayerEntry`，**标记为 `ConfigLayerSource::SessionFlags`（precedence=30）**，再由 `load_config_layers_state` 的 `insert_layer_by_precedence`（`loader/mod.rs:421-429`）按优先级插入栈中。所以服务端线程配置与 CLI `-c` 同级——高于 project、低于 legacy managed。空表会返回 `None` 不入栈（`thread_config.rs:157-158`）。`UserThreadConfig` 目前是空结构，注释（`:166-169`）说将来若长出字段应折叠进现有 user 层而非新增 source 变体。

---

## 9. `ConfigLock`：冻结与回放有效配置

`core/src/config_lock.rs:1-177` 实现了一套"把有效配置冻结成快照、之后回放验证确定性"的机制，主要服务于调试/可复现性。

锁文件结构 `ConfigLockfileToml`（定义在 `config/src/config_toml.rs:495-503`）：

```rust
// config/src/config_toml.rs:495-503
pub struct ConfigLockfileToml {
    pub version: u32,                 // CONFIG_LOCK_VERSION = 1
    pub codex_version: String,        // 生成锁文件时的 Codex 版本
    pub config: ConfigToml,           // 可回放的有效配置快照
}
```

三个核心函数：

| 函数 | 作用 | 行号 |
|------|------|------|
| `config_lockfile(config)` | 把一份 `ConfigToml` 包成锁文件（带当前版本号） | `config_lock.rs:38-44` |
| `lock_layer_from_config(path, lockfile)` | 把锁文件配置变成一个 `User` 层（剥掉 lock 自身的 debug 控制项） | `:76-91` |
| `validate_config_lock_replay(expected, actual, opts)` | 比对"锁文件里的 config"与"本次重新解析的 config"，不一致就报带 diff 的错误 | `:46-74` |

回放逻辑（`config_lock.rs:46-74`）：先校验版本形状，再（除非允许版本不匹配）检查 `codex_version` 是否一致，最后逐字段比对 config，不同则用 `similar` 库生成 unified diff 报告。比对前会 `clear_config_lock_debug_controls()`（`:99-110`）剥掉 `debug.config_lockfile` 控制项本身，避免"控制锁的配置"干扰"被锁的配置"。

触发入口在 `core/src/config/mod.rs:1186-1221`：当 `config.debug.config_lockfile.load_path` 被设置时，读取锁文件、把它当作唯一 User 层重建一个 `ConfigLayerStack`、重新派生 `Config`，并把期望锁存进 `config.config_lock_toml` 供后续回放校验。这让"同一份锁文件 → 永远得到同一份有效配置"可被验证，对排查"为什么这台机器上配置不一样"很有用。

> [推测] 这套机制的典型用途是：导出当前会话的有效配置锁，在 CI 或另一台机器上 replay，确认配置解析是确定性的、没被环境差异（不同的 `requirements.toml`、不同的项目信任状态）悄悄改变。`allow_codex_version_mismatch` 开关允许跨版本回放（会清空版本字段再比对，见 `:122-132`）。

---

## 10. 配置应用生命周期：从磁盘到一次 Turn

把前面所有环节串成一条时间线：

```
启动期（一次性）
══════════════════════════════════════════════════════════════════════════
 磁盘文件                          load_config_layers_state()           ConfigLayerStack
 ┌──────────────────┐             ┌─────────────────────────┐          ┌──────────────┐
 │ requirements.toml├──约束链────→│ ① Requirements 链        │          │              │
 │ /etc/codex/...   │             │ ② legacy managed→req     ├─────────→│ layers[低→高]│
 │ $CODEX_HOME/...  ├──配置链────→│ ③ thread config layers   │          │ requirements │
 │ <profile>.config │             │ ④-⑪ 各 Configuration 层  │          │ user_layer_  │
 │ cwd/.codex/...   │             │      按 precedence 叠加  │          │   index      │
 │ managed_config   │             │ ⑫ ConfigLayerStack::new  │          └──────┬───────┘
 └──────────────────┘             └─────────────────────────┘                 │
                                                                                │
                                   load_config_with_layer_stack()              │ + ConfigOverrides
                                   （合并层 + 强制 requirements + 特性特化）    ▼
                                                                         ┌──────────────┐
                                                                         │  Config      │ ← model/permissions/
                                                                         │ (已解决)     │   service_tier 已具体化
                                                                         └──────┬───────┘
                                                                                │
运行期（每条提交）                                                              ▼
══════════════════════════════════════════════════════════════════════════════════════
 Op::UserInput { items, thread_settings }   ── 同一条 SQ，保序 ──→  Session
        │                                                              │
        ├─ thread_settings_update()  ───────────────────────────────→ SessionSettingsUpdate
        │   （读 collaboration_mode，合入 model/effort）                │
        │                                                              ▼
        └─ items ────────────────────→ new_turn_with_sub_id(updates) ─→ 原子应用 → 开始 Turn
```

要点回顾：
1. **启动期**把多来源合成 `ConfigLayerStack` → 派生 `Config`，约束在此强制执行。
2. **运行期**用户可通过 `ThreadSettingsOverrides` 逐回合覆盖，与输入捆绑同一条 SQ 保证"设置先于回合生效"。
3. **权限/沙箱**相关字段（`sandbox_policy`、`permission_profile`、`workspace_roots`）的覆盖最终汇入会话状态，由沙箱层物化为实际授权，详见 [doc13 沙箱机制](../3_执行与安全/13_sandbox_mechanism.md)。

---

## 11. 常见问题

**Q1：为什么我在项目 `.codex/config.toml` 里写 `model_provider` 没生效？**
A：`model_provider` 在 `PROJECT_LOCAL_CONFIG_DENYLIST`（`loader/mod.rs:57-72`）里，项目层无权设置它（防恶意仓库改你的模型提供商）。请写到 `$CODEX_HOME/config.toml` 或用 CLI `-c`。被忽略的项目 key 会进 `startup_warnings`，UI 会提示。

**Q2：CLI `-c model=gpt-5` 一定能覆盖一切吗？**
A：不一定。`SessionFlags`(30) 高于 `Project`(25)、`User`(20) 但**低于** `LegacyManaged`(40/50)。若部署了 `managed_config.toml` 强制了 model，CLI 会被它压制。约束链（`requirements.toml`）则更硬——超出 allow-list 直接报错，无关优先级。

**Q3：Profile 选中后，base `config.toml` 还生效吗？**
A：生效。Profile-v2 是**覆盖层不是替代品**（`loader/mod.rs:214-262`）：base(20) + profile(21) 两层都在栈里，profile 只盖它显式写的字段，其余回落到 base。

**Q4：`effort: None` 和 `effort: Some(None)` 有什么区别？**
A：`None` = 这次更新不碰 effort（保持现状）；`Some(None)` = 显式清除 effort（恢复默认）。这是 `Option<Option<T>>` 的三态设计（`protocol.rs:470-475`）。`service_tier` 同理。

**Q5：admin 怎么做到"用户绝对改不了某个设置"？**
A：用 `requirements.toml`（Requirements 链）而非 `config.toml`（Configuration 链）。前者是独立的约束链，"早层约束晚层不可推翻"（`loader/mod.rs:78-89`），从栈派生 `Config` 时强制校验，任何越界值都被拒绝。注意旧 `managed_config.toml` 走的是 Configuration 链（仅高优先级），不如 requirements 硬，官方正推动迁移。

**Q6：`config_lock` 是用来锁定用户不能改配置的吗？**
A：不是。它是**调试/可复现性**工具（`config_lock.rs`）：冻结一份有效配置快照，之后 replay 验证"同样的输入是否得到同样的有效配置"，检测确定性。锁定/强制用户不能改是 Requirements 的职责。

---

## 12. 核心代码路径索引

| 主题 | 文件:行 |
|------|---------|
| 优先级链文档注释（两链分离） | `config/src/loader/mod.rs:78-109` |
| `ConfigLayerSource` 枚举变体 | `app-server-protocol/src/protocol/v2/config.rs:27-83` |
| `precedence()` 数值映射 | `app-server-protocol/src/protocol/v2/config.rs:88-104` |
| `load_config_layers_state()` 签名 | `config/src/loader/mod.rs:111-119` |
| 加载流程函数体 | `config/src/loader/mod.rs:120-389` |
| 项目层树形搜索 + 信任门控 | `config/src/loader/mod.rs:1128`（`load_project_layers`） |
| `PROJECT_LOCAL_CONFIG_DENYLIST` | `config/src/loader/mod.rs:57-72` |
| `insert_layer_by_precedence()` | `config/src/loader/mod.rs:421-429` |
| `ConfigLayerStack` 结构体 | `config/src/state.rs:211-242` |
| `verify_layer_ordering()` | `config/src/state.rs:516` |
| `get_active_user_layer` / `effective_user_config` | `config/src/state.rs:288-335` |
| `with_user_config_profile()` | `config/src/state.rs:361-407` |
| `ConfigProfile` 结构体 | `config/src/profile_toml.rs:22-71` |
| `ThreadConfigLoader` trait | `config/src/thread_config.rs:89-113` |
| `SessionThreadConfig` | `config/src/thread_config.rs:24-30` |
| `thread_config_source_to_layer()` | `config/src/thread_config.rs:151-171` |
| `Config` 结构体核心字段 | `core/src/config/mod.rs:544-650` |
| `load_config_with_layer_stack()` | `core/src/config/mod.rs:2436` |
| `config_lock` 模块 | `core/src/config_lock.rs:1-177` |
| `ConfigLockfileToml` 结构体 | `config/src/config_toml.rs:495-503` |
| config_lock replay 触发入口 | `core/src/config/mod.rs:1186-1221` |
| `ThreadSettingsOverrides` 结构体 | `protocol/src/protocol.rs:423-496` |
| `Op::UserInput` / `Op::ThreadSettings` | `protocol/src/protocol.rs:559-587` |
| `thread_settings_update()` | `core/src/session/handlers.rs:129-178` |
| `Op::UserInput` 处理（原子应用） | `core/src/session/handlers.rs:209-241` |

---

### 交叉引用

- Thread / Session / Turn 生命周期与 `Op` 全枚举：[doc11](../2_运行时核心/11_thread_session_turn_lifecycle.md)
- 沙箱机制、`TrustLevel`、workspace 权限物化：[doc13](../3_执行与安全/13_sandbox_mechanism.md)
