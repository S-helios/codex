//! 【文件职责】把「已解析完成的权限 profile」连同它的身份信息（legacy / 内置 /
//! 命名）打包成一组数据结构，作为 config 与 session 状态之间的可信桥梁。
//!
//! 【架构位置】
//!   层级：core 配置层（权限子系统）
//!   上游：config/mod.rs（装配 `Config` 时构造）、session 状态恢复路径
//!   下游：`Permissions`（mod.rs 中持有它并对外暴露权限投影）
//!
//! 【设计背景】一个权限 profile 解析后需要同时携带三类信息：具体的
//! `PermissionProfile`（实际生效的沙箱权限）、可选的「活动 profile id」（用于
//! 让用户重新选中/round-trip）、以及 profile 自带的 workspace roots。本文件用
//! 一个三态枚举 `ResolvedPermissionProfile` 把它们统一编码，再由
//! `PermissionProfileSnapshot`（对外可信快照）和 `PermissionProfileState`
//! （带约束校验的内部状态）两层包装提供给不同调用方。
//!
//! 【阅读建议】先看 `ResolvedPermissionProfile` 的三个变体含义，再看
//! `from_active_profile()`（id → 变体的分派逻辑），最后看
//! `PermissionProfileSnapshot` / `PermissionProfileState` 两个外壳的职责差异。

use codex_config::Constrained;
use codex_config::ConstraintResult;
use codex_protocol::models::ActivePermissionProfile;
use codex_protocol::models::BUILT_IN_PERMISSION_PROFILE_DANGER_FULL_ACCESS;
use codex_protocol::models::BUILT_IN_PERMISSION_PROFILE_READ_ONLY;
use codex_protocol::models::BUILT_IN_PERMISSION_PROFILE_WORKSPACE;
use codex_protocol::models::PermissionProfile;
use codex_utils_absolute_path::AbsolutePathBuf;

/// 内置权限 profile 的强类型 id。三个内置档分别对应只读 / 工作区可写 /
/// 危险全开（无沙箱）。用枚举而非裸字符串，避免在 `match` 中漏分支。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BuiltInPermissionProfileId {
    ReadOnly,
    Workspace,
    DangerFullAccess,
}

impl BuiltInPermissionProfileId {
    fn from_str(id: &str) -> Option<Self> {
        match id {
            BUILT_IN_PERMISSION_PROFILE_READ_ONLY => Some(Self::ReadOnly),
            BUILT_IN_PERMISSION_PROFILE_WORKSPACE => Some(Self::Workspace),
            BUILT_IN_PERMISSION_PROFILE_DANGER_FULL_ACCESS => Some(Self::DangerFullAccess),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => BUILT_IN_PERMISSION_PROFILE_READ_ONLY,
            Self::Workspace => BUILT_IN_PERMISSION_PROFILE_WORKSPACE,
            Self::DangerFullAccess => BUILT_IN_PERMISSION_PROFILE_DANGER_FULL_ACCESS,
        }
    }
}

/// 解析后的权限 profile 三态。区分这三态的根本目的是：是否能向客户端「回报」
/// 一个可重新选中的活动 profile id。
/// - `Legacy`：来自旧式 `sandbox_mode` 语法或本地覆盖，没有 profile 身份，
///   对外 `active_permission_profile()` 返回 `None`。
/// - `BuiltIn`：选中了某个内置档（如 `:workspace`），id 用强类型表示。
/// - `Named`：选中了用户在 `[permissions]` 里自定义的命名档，id 是任意字符串。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResolvedPermissionProfile {
    Legacy(LegacyPermissionProfile),
    BuiltIn(BuiltInPermissionProfile),
    Named(NamedPermissionProfile),
}

/// Trusted snapshot of a resolved permission profile.
///
/// This is a bridge for already-resolved session/config state. It keeps the
/// concrete `PermissionProfile`, optional active profile id, and
/// profile-defined workspace roots together so `Permissions` can validate and
/// install them atomically. It is not a resolver: callers that are handling
/// user-selected profile ids should resolve those ids through config instead
/// of constructing this type directly.
///
/// 「可信快照」：把已解析好的 profile 三元组（具体权限 + 活动 id + workspace
/// roots）原子地搬运给 `Permissions` 安装。它本身**不做解析**——调用方若拿到的是
/// 用户选择的 profile id，应当先走 config 解析，而不是直接构造本类型（否则 id
/// 与权限可能对不上）。这是 `pub` 类型，是跨 crate 的对外契约。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionProfileSnapshot {
    resolved_permission_profile: ResolvedPermissionProfile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LegacyPermissionProfile {
    permission_profile: PermissionProfile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BuiltInPermissionProfile {
    id: BuiltInPermissionProfileId,
    extends: Option<String>,
    permission_profile: PermissionProfile,
    profile_workspace_roots: Vec<AbsolutePathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NamedPermissionProfile {
    id: String,
    extends: Option<String>,
    permission_profile: PermissionProfile,
    profile_workspace_roots: Vec<AbsolutePathBuf>,
}

impl ResolvedPermissionProfile {
    /// 根据「活动 profile 元数据是否存在 + id 是否为内置档」三路分派到对应变体：
    /// - 无活动元数据 → `Legacy`（无身份）。
    /// - id 命中内置档 → `BuiltIn`。
    /// - 其余 → `Named`（用户自定义命名档）。
    pub(crate) fn from_active_profile(
        permission_profile: PermissionProfile,
        active_permission_profile: Option<ActivePermissionProfile>,
        profile_workspace_roots: Vec<AbsolutePathBuf>,
    ) -> Self {
        let Some(active_permission_profile) = active_permission_profile else {
            return Self::legacy(permission_profile);
        };

        let ActivePermissionProfile { id, extends } = active_permission_profile;
        if let Some(built_in_id) = BuiltInPermissionProfileId::from_str(&id) {
            Self::BuiltIn(BuiltInPermissionProfile {
                id: built_in_id,
                extends,
                permission_profile,
                profile_workspace_roots,
            })
        } else {
            Self::Named(NamedPermissionProfile {
                id,
                extends,
                permission_profile,
                profile_workspace_roots,
            })
        }
    }

    pub(crate) fn legacy(permission_profile: PermissionProfile) -> Self {
        Self::Legacy(LegacyPermissionProfile { permission_profile })
    }

    pub(crate) fn permission_profile(&self) -> &PermissionProfile {
        match self {
            Self::Legacy(profile) => &profile.permission_profile,
            Self::BuiltIn(profile) => &profile.permission_profile,
            Self::Named(profile) => &profile.permission_profile,
        }
    }

    pub(crate) fn active_permission_profile(&self) -> Option<ActivePermissionProfile> {
        match self {
            Self::Legacy(_) => None,
            Self::BuiltIn(profile) => Some(ActivePermissionProfile {
                id: profile.id.as_str().to_string(),
                extends: profile.extends.clone(),
            }),
            Self::Named(profile) => Some(ActivePermissionProfile {
                id: profile.id.clone(),
                extends: profile.extends.clone(),
            }),
        }
    }

    pub(crate) fn profile_workspace_roots(&self) -> &[AbsolutePathBuf] {
        match self {
            Self::Legacy(_) => &[],
            Self::BuiltIn(profile) => &profile.profile_workspace_roots,
            Self::Named(profile) => &profile.profile_workspace_roots,
        }
    }
}

impl PermissionProfileSnapshot {
    /// Create a snapshot with no active profile id.
    ///
    /// Prefer this only for legacy data or local overrides that genuinely do
    /// not have a named/built-in profile identity. Using this for a built-in or
    /// named profile will intentionally clear the active profile metadata.
    pub fn legacy(permission_profile: PermissionProfile) -> Self {
        Self {
            resolved_permission_profile: ResolvedPermissionProfile::legacy(permission_profile),
        }
    }

    /// Create a snapshot for a known active profile id.
    ///
    /// Use this only after a trusted caller has already resolved the active id
    /// to the supplied concrete `PermissionProfile`. This constructor does not
    /// verify that the id and profile match; `Permissions` will still enforce
    /// configured permission constraints when the snapshot is installed.
    pub fn active(
        permission_profile: PermissionProfile,
        active_permission_profile: ActivePermissionProfile,
    ) -> Self {
        Self::active_with_profile_workspace_roots(
            permission_profile,
            active_permission_profile,
            Vec::new(),
        )
    }

    /// Create a snapshot for a known active profile id with profile roots.
    ///
    /// As with `active`, the caller is responsible for passing the concrete
    /// profile and active id that were resolved together. Use this variant when
    /// the selected profile declared workspace roots that should remain
    /// distinct from turn-scoped runtime workspace roots.
    pub fn active_with_profile_workspace_roots(
        permission_profile: PermissionProfile,
        active_permission_profile: ActivePermissionProfile,
        profile_workspace_roots: Vec<AbsolutePathBuf>,
    ) -> Self {
        Self {
            resolved_permission_profile: ResolvedPermissionProfile::from_active_profile(
                permission_profile,
                Some(active_permission_profile),
                profile_workspace_roots,
            ),
        }
    }

    /// Reconstruct a trusted snapshot from session state.
    ///
    /// This is intended for session responses emitted by core, where the
    /// concrete profile and active profile id were captured together. Avoid
    /// using this as a shortcut for arbitrary user input because mismatched
    /// arguments can still misrepresent the active profile identity.
    pub fn from_session_snapshot(
        permission_profile: PermissionProfile,
        active_permission_profile: Option<ActivePermissionProfile>,
    ) -> Self {
        match active_permission_profile {
            Some(active_permission_profile) => {
                Self::active(permission_profile, active_permission_profile)
            }
            None => Self::legacy(permission_profile),
        }
    }

    /// Borrow the concrete permission profile captured in this snapshot.
    pub fn permission_profile(&self) -> &PermissionProfile {
        self.resolved_permission_profile.permission_profile()
    }

    /// Return the active profile id captured in this snapshot, if any.
    pub fn active_permission_profile(&self) -> Option<ActivePermissionProfile> {
        self.resolved_permission_profile.active_permission_profile()
    }

    /// Borrow profile-declared workspace roots captured in this snapshot.
    pub fn profile_workspace_roots(&self) -> &[AbsolutePathBuf] {
        self.resolved_permission_profile.profile_workspace_roots()
    }

    pub(crate) fn into_resolved_permission_profile(self) -> ResolvedPermissionProfile {
        self.resolved_permission_profile
    }
}

/// `Permissions` 内部持有的权限 profile 状态：在 `ResolvedPermissionProfile`
/// 外再套一层 `Constrained`，使「设置新 profile」时会经过 requirements 约束校验
/// （例如 requirements.toml 禁止某些沙箱模式）。与 `PermissionProfileSnapshot`
/// 的区别：Snapshot 是可信、无校验的搬运容器；State 是带约束、可拒绝写入的活状态。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PermissionProfileState {
    resolved_permission_profile: Constrained<ResolvedPermissionProfile>,
}

impl PermissionProfileState {
    pub(crate) fn from_constrained_legacy(
        constrained_permission_profile: Constrained<PermissionProfile>,
    ) -> ConstraintResult<Self> {
        let resolved =
            ResolvedPermissionProfile::legacy(constrained_permission_profile.get().clone());
        Self::from_constrained_resolved(constrained_permission_profile, resolved)
    }

    pub(crate) fn from_constrained_active_profile(
        constrained_permission_profile: Constrained<PermissionProfile>,
        active_permission_profile: Option<ActivePermissionProfile>,
        profile_workspace_roots: Vec<AbsolutePathBuf>,
    ) -> ConstraintResult<Self> {
        let resolved = ResolvedPermissionProfile::from_active_profile(
            constrained_permission_profile.get().clone(),
            active_permission_profile,
            profile_workspace_roots,
        );
        Self::from_constrained_resolved(constrained_permission_profile, resolved)
    }

    /// 把「对 `PermissionProfile` 的约束」转译成「对 `ResolvedPermissionProfile`
    /// 的约束」：新约束的校验逻辑就是取出候选 resolved 内部的具体
    /// `PermissionProfile`，再交给原始约束判定。这样 resolved 这层包装在切换时
    /// 仍受底层 requirements 约束保护。
    pub(crate) fn from_constrained_resolved(
        constrained_permission_profile: Constrained<PermissionProfile>,
        resolved_permission_profile: ResolvedPermissionProfile,
    ) -> ConstraintResult<Self> {
        let permission_profile_constraint = constrained_permission_profile;
        let resolved_permission_profile = Constrained::new(
            resolved_permission_profile,
            move |candidate: &ResolvedPermissionProfile| {
                permission_profile_constraint.can_set(candidate.permission_profile())
            },
        )?;
        Ok(Self {
            resolved_permission_profile,
        })
    }

    pub(crate) fn permission_profile(&self) -> &PermissionProfile {
        self.resolved_permission_profile.get().permission_profile()
    }

    pub(crate) fn active_permission_profile(&self) -> Option<ActivePermissionProfile> {
        self.resolved_permission_profile
            .get()
            .active_permission_profile()
    }

    pub(crate) fn profile_workspace_roots(&self) -> &[AbsolutePathBuf] {
        self.resolved_permission_profile
            .get()
            .profile_workspace_roots()
    }

    pub(crate) fn can_set_legacy_permission_profile(
        &self,
        permission_profile: &PermissionProfile,
    ) -> ConstraintResult<()> {
        let candidate = ResolvedPermissionProfile::legacy(permission_profile.clone());
        self.resolved_permission_profile.can_set(&candidate)
    }

    pub(crate) fn set_legacy_permission_profile(
        &mut self,
        permission_profile: PermissionProfile,
    ) -> ConstraintResult<()> {
        self.resolved_permission_profile
            .set(ResolvedPermissionProfile::legacy(permission_profile))
    }

    pub(crate) fn set_permission_profile_snapshot(
        &mut self,
        snapshot: PermissionProfileSnapshot,
    ) -> ConstraintResult<()> {
        self.resolved_permission_profile
            .set(snapshot.into_resolved_permission_profile())
    }
}
