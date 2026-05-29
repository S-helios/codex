//! 【文件职责】跨回合 diff 追踪器：在内存里累积「本回合相对回合初态」的净 diff，
//! 全程不重读工作区文件系统，最终产出 git 风格的统一 diff 供 UI 展示。
//!
//! 【架构位置】
//!   层级：工具执行层（apply_patch 的伴生组件）
//!   上游：运行时每次落盘成功后调用 `track_delta()` 喂入已提交的变更
//!   下游：UI/事件层调用 `get_unified_diff()` 拿到聚合 diff 字符串
//!
//! 【为什么需要它】
//!   一个回合内模型可能多次 apply_patch，UI 想展示「本回合净改动」。
//!   每次都回磁盘重读太慢，于是这里纯靠已落盘的 `AppliedPatchDelta`
//!   增量维护「回合初态」与「当前态」两份内容快照，自行算 diff。
//!
//! 【核心数据结构】见 `TurnDiffTracker` 各字段注释。关键不变式：
//!   某路径首次被改时，把「改动前内容」记入 `baseline_by_path`（回合初态），
//!   之后所有改动只更新 `current_by_path`（当前态）；diff = baseline vs current。
//!
//! 【精确性闸门】只要任一 delta 不是「精确套用」（含模糊匹配/容错），
//!   立即 `invalidate()`，此后 `get_unified_diff()` 一律返回 `None`，
//!   迫使上层回退到「重读文件系统」算 diff。详见 `track_delta`。
//!
//! 【阅读建议】先看 `track_delta` → `apply_change` 的三态分派
//!   （add/delete/update），再看 `get_unified_diff` 如何排序、配对改名、
//!   逐路径渲染；`render_diff` 是真正吐 git diff 文本的地方。
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;

use sha1::digest::Output;

use codex_apply_patch::AppliedPatchChange;
use codex_apply_patch::AppliedPatchDelta;
use codex_apply_patch::AppliedPatchFileChange;

// 下面三个常量复刻 git diff 文本里的固定字段，使输出能被标准 diff 渲染器识别。
// 全零 OID：git 用它表示「对端不存在该 blob」（新增文件的旧侧 / 删除文件的新侧）。
const ZERO_OID: &str = "0000000000000000000000000000000000000000";
// `/dev/null`：unified diff 头里表示「文件不存在」的一侧（新增/删除时用）。
const DEV_NULL: &str = "/dev/null";
// `100644`：git 普通文件的默认模式（非可执行、非软链）。此追踪器只处理普通文件。
const REGULAR_FILE_MODE: &str = "100644";

/// Tracks the net text diff for the current turn from committed apply_patch
/// mutations, without rereading the workspace filesystem.
/// 从「已提交的 apply_patch 变更」累积本回合净文本 diff，全程不重读工作区文件系统。
pub struct TurnDiffTracker {
    /// 追踪是否仍然有效。任一不精确 delta 会置 false，
    /// 之后 `get_unified_diff()` 直接返回 `None`（见文件级注释「精确性闸门」）。
    valid: bool,
    /// 显示用根目录：渲染 diff 时把绝对路径裁成相对此根的展示路径（见 `display_path`）。
    /// `None` 表示按原始路径展示。
    display_root: Option<PathBuf>,
    /// 「回合初态」内容快照：路径 → 该路径在本回合第一次被改之前的内容。
    /// 仅在某路径首次被触碰时写入，是 diff 的左侧（旧侧）来源。
    baseline_by_path: HashMap<PathBuf, String>,
    /// 「当前态」内容快照：路径 → 最近一次落盘后的内容，是 diff 的右侧（新侧）来源。
    /// 删除会从这里移除条目。
    current_by_path: HashMap<PathBuf, String>,
    /// 改名溯源：当前路径 → 它在回合初态时的原始路径。
    /// 仅当发生 Update + Move（重命名）时记录，供 `rename_pairs` 还原 git rename diff。
    origin_by_current_path: HashMap<PathBuf, PathBuf>,
}

impl Default for TurnDiffTracker {
    fn default() -> Self {
        Self {
            valid: true,
            display_root: None,
            baseline_by_path: HashMap::new(),
            current_by_path: HashMap::new(),
            origin_by_current_path: HashMap::new(),
        }
    }
}

impl TurnDiffTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_display_root(display_root: PathBuf) -> Self {
        let mut tracker = Self::new();
        tracker.display_root = Some(display_root);
        tracker
    }

    /// 喂入一批已落盘的变更，更新内存中的初态/当前态快照。
    ///
    /// 精确性闸门：若该 delta 不是「精确套用」（即过程中用了模糊匹配/容错，
    /// 见 `AppliedPatchDelta::is_exact`），说明我们无法可靠重建文本内容，
    /// 直接整体作废本回合追踪并提前返回——宁可让上层回磁盘重读，也不给出错误 diff。
    ///
    /// 副作用：就地修改 self 的三张 map / `valid` 标志。
    pub fn track_delta(&mut self, delta: &AppliedPatchDelta) {
        if !delta.is_exact() {
            self.invalidate();
            return;
        }

        for change in delta.changes() {
            self.apply_change(change);
        }
    }

    /// 标记追踪失效。一旦调用，后续 `get_unified_diff()` 恒返回 `None`。
    pub fn invalidate(&mut self) {
        self.valid = false;
    }

    /// 产出本回合相对回合初态的聚合 git diff；无改动或追踪已失效时返回 `None`。
    ///
    /// 流程：① 失效则直接返回 None；② 收集所有涉及路径并按展示路径排序去重，
    /// 保证输出稳定有序；③ 重命名的源/目标合并为一条 rename diff、其余逐路径渲染。
    pub fn get_unified_diff(&self) -> Option<String> {
        if !self.valid {
            return None;
        }

        // Step 1：算出改名配对（源 → 目标），并记下所有「目标路径」集合。
        // 目标路径会在源路径那条 rename diff 里一并处理，故遍历时要跳过它们，
        // 避免同一改名被渲染两次。
        let rename_pairs = self.rename_pairs();
        let paired_destinations = rename_pairs.values().cloned().collect::<HashSet<_>>();
        let mut handled = HashSet::new();
        // Step 2：汇总「初态 ∪ 当前态」涉及的全部路径，按展示路径排序后去重。
        // 排序是为了 diff 输出顺序稳定（便于人读、便于测试快照比对）。
        let mut paths = self
            .baseline_by_path
            .keys()
            .chain(self.current_by_path.keys())
            .cloned()
            .collect::<Vec<_>>();
        paths.sort_by_key(|path| self.display_path(path));
        paths.dedup();

        // Step 3：逐路径渲染并拼接。`handled` 防止重复处理；
        // 命中改名源则连带标记其目标为已处理，输出一条 rename diff。
        let mut aggregated = String::new();
        for path in paths {
            if !handled.insert(path.clone()) {
                continue;
            }

            // 该路径是某次改名的目标：留给它的源路径统一处理，这里跳过。
            if paired_destinations.contains(&path) {
                continue;
            }

            let diff = if let Some(dest) = rename_pairs.get(&path) {
                handled.insert(dest.clone());
                self.render_rename_diff(&path, dest)
            } else {
                self.render_path_diff(&path)
            };

            if let Some(diff) = diff {
                aggregated.push_str(&diff);
                if !aggregated.ends_with('\n') {
                    aggregated.push('\n');
                }
            }
        }

        (!aggregated.is_empty()).then_some(aggregated)
    }

    /// 按 `AppliedPatchFileChange` 的三态（增/删/改）分派到对应的 `apply_*`。
    fn apply_change(&mut self, change: &AppliedPatchChange) {
        let source_path = change.path.as_path();
        match &change.change {
            AppliedPatchFileChange::Add {
                content,
                overwritten_content,
            } => self.apply_add(source_path, content, overwritten_content.as_deref()),
            AppliedPatchFileChange::Delete { content } => self.apply_delete(source_path, content),
            AppliedPatchFileChange::Update {
                move_path,
                old_content,
                overwritten_move_content,
                new_content,
            } => self.apply_update(
                source_path,
                move_path.as_deref(),
                old_content,
                overwritten_move_content.as_deref(),
                new_content,
            ),
        }
    }

    /// 处理「新增文件」。
    /// `overwritten_content` 非空表示该新增其实覆盖了一个已存在文件——此时
    /// 若该路径在本回合还从未记过初态，则把「被覆盖的旧内容」存为初态，
    /// 这样最终 diff 才能反映「从旧内容变成新内容」而非凭空新建。
    fn apply_add(&mut self, path: &Path, content: &str, overwritten_content: Option<&str>) {
        self.origin_by_current_path.remove(path);
        if !self.current_by_path.contains_key(path)
            && !self.baseline_by_path.contains_key(path)
            && let Some(overwritten_content) = overwritten_content
        {
            self.baseline_by_path
                .insert(path.to_path_buf(), overwritten_content.to_string());
        }
        self.current_by_path
            .insert(path.to_path_buf(), content.to_string());
    }

    /// 处理「删除文件」。
    /// 若当前态里没有该路径（说明它是回合初就存在、本回合首次触碰即删），
    /// 且初态也未记过，则把被删内容补记为初态——否则 diff 无从知道删了什么。
    /// 从当前态移除该路径即代表「现在不存在」。
    fn apply_delete(&mut self, path: &Path, content: &str) {
        if self.current_by_path.remove(path).is_none() && !self.baseline_by_path.contains_key(path)
        {
            self.baseline_by_path
                .insert(path.to_path_buf(), content.to_string());
        }
        self.origin_by_current_path.remove(path);
    }

    /// 处理「更新文件」，可选带改名（`move_path`）。
    ///
    /// @param source_path              - 被更新文件的原路径
    /// @param move_path                - 若为改名则是新路径，否则 `None`（原地更新）
    /// @param old_content              - 更新前内容（首次触碰时用作初态）
    /// @param overwritten_move_content - 改名目标若已存在文件，则是被覆盖的旧内容
    /// @param new_content              - 更新后内容（写入当前态）
    ///
    /// 不变式：仅当 `source_path` 在本回合首次出现时才把 `old_content` 记为初态，
    /// 后续更新只改当前态，确保初态始终是「回合最开始」的样子。
    fn apply_update(
        &mut self,
        source_path: &Path,
        move_path: Option<&Path>,
        old_content: &str,
        overwritten_move_content: Option<&str>,
        new_content: &str,
    ) {
        if !self.current_by_path.contains_key(source_path)
            && !self.baseline_by_path.contains_key(source_path)
        {
            self.baseline_by_path
                .insert(source_path.to_path_buf(), old_content.to_string());
        }

        match move_path {
            // 带改名：内容从 source_path 搬到 dest_path，需维护初态/当前态/改名溯源三处。
            Some(dest_path) => {
                // 改名目标若覆盖了已存在文件，首次触碰时把被覆盖内容记为该目标的初态。
                if !self.current_by_path.contains_key(dest_path)
                    && !self.baseline_by_path.contains_key(dest_path)
                    && let Some(overwritten_move_content) = overwritten_move_content
                {
                    self.baseline_by_path.insert(
                        dest_path.to_path_buf(),
                        overwritten_move_content.to_string(),
                    );
                }
                // 溯源到「最初的原始路径」：若 source 本身就是上一次改名的结果，
                // 取其更早的 origin，从而支持 a→b→c 这样的链式改名也能正确归并。
                let origin = self
                    .origin_by_current_path
                    .remove(source_path)
                    .unwrap_or_else(|| source_path.to_path_buf());
                self.current_by_path.remove(source_path);
                self.current_by_path
                    .insert(dest_path.to_path_buf(), new_content.to_string());
                self.origin_by_current_path.remove(dest_path);
                // 仅当目标确实不同于最初原始路径时才记改名（绕回原名等价于没改名）。
                if dest_path != origin.as_path() {
                    self.origin_by_current_path
                        .insert(dest_path.to_path_buf(), origin);
                }
            }
            // 原地更新：只刷新当前态内容即可。
            None => {
                self.current_by_path
                    .insert(source_path.to_path_buf(), new_content.to_string());
            }
        }
    }

    /// 从 `origin_by_current_path` 还原出「源路径 → 目标路径」的有效改名配对。
    /// 过滤掉无法构成 git rename 的情形（见各判据），只保留：
    /// 源在初态有、目标在初态无、且源已不在当前态——即真正「移走」的改名。
    fn rename_pairs(&self) -> HashMap<PathBuf, PathBuf> {
        self.origin_by_current_path
            .iter()
            .filter_map(|(dest_path, origin_path)| {
                // 任一判据成立即「不是有效改名」，剔除：
                //   ① 源==目标：绕回原名，等于没改名；
                //   ② 源仍在当前态：说明源没被移走（可能又被新建回来）；
                //   ③ 目标不在当前态：目标已不存在，构不成 rename 的「新位置」；
                //   ④ 源不在初态：回合初就没有该源文件，无从「移动」；
                //   ⑤ 目标在初态已存在：那是覆盖而非纯改名，按普通增删处理更准确。
                if dest_path == origin_path
                    || self.current_by_path.contains_key(origin_path)
                    || !self.current_by_path.contains_key(dest_path)
                    || !self.baseline_by_path.contains_key(origin_path)
                    || self.baseline_by_path.contains_key(dest_path)
                {
                    return None;
                }

                Some((origin_path.clone(), dest_path.clone()))
            })
            .collect()
    }

    fn render_path_diff(&self, path: &Path) -> Option<String> {
        self.render_diff(
            path,
            self.baseline_by_path.get(path).map(String::as_str),
            path,
            self.current_by_path.get(path).map(String::as_str),
        )
    }

    fn render_rename_diff(&self, source_path: &Path, dest_path: &Path) -> Option<String> {
        self.render_diff(
            source_path,
            self.baseline_by_path.get(source_path).map(String::as_str),
            dest_path,
            self.current_by_path.get(dest_path).map(String::as_str),
        )
    }

    /// 渲染单条 git 风格 diff（被 `render_path_diff` / `render_rename_diff` 复用）。
    /// `None` 内容表示该侧文件不存在（新增/删除）。两侧内容相同则返回 `None`（无 diff）。
    ///
    /// 输出严格对齐 `git diff` 文本格式：`diff --git` 头 + 文件模式行（新增/删除时）
    /// + `index <oldoid>..<newoid>` + 由 `similar` 生成的 unified hunk。
    fn render_diff(
        &self,
        left_path: &Path,
        left_content: Option<&str>,
        right_path: &Path,
        right_content: Option<&str>,
    ) -> Option<String> {
        if left_content == right_content {
            return None;
        }

        let left_display = self.display_path(left_path);
        let right_display = self.display_path(right_path);
        // 内容不存在的一侧用全零 OID；存在的一侧算真实 git blob OID，
        // 使输出的 `index` 行与 git 行为一致。
        let left_oid = left_content.map_or_else(
            || ZERO_OID.to_string(),
            |content| git_blob_oid(content.as_bytes()),
        );
        let right_oid = right_content.map_or_else(
            || ZERO_OID.to_string(),
            |content| git_blob_oid(content.as_bytes()),
        );

        let mut diff = format!("diff --git a/{left_display} b/{right_display}\n");
        // 仅在「新增/删除」时追加文件模式行；两侧都存在（普通修改）无需此行。
        // 两侧都不存在不应到这（上方已 early-return），保守再挡一次。
        match (left_content, right_content) {
            (None, Some(_)) => diff.push_str(&format!("new file mode {REGULAR_FILE_MODE}\n")),
            (Some(_), None) => diff.push_str(&format!("deleted file mode {REGULAR_FILE_MODE}\n")),
            (Some(_), Some(_)) => {}
            (None, None) => return None,
        }

        diff.push_str(&format!("index {left_oid}..{right_oid}\n"));

        // `---`/`+++` 头：存在的一侧写 `a/`/`b/` 路径，不存在的一侧写 `/dev/null`。
        let old_header = if left_content.is_some() {
            format!("a/{left_display}")
        } else {
            DEV_NULL.to_string()
        };
        let new_header = if right_content.is_some() {
            format!("b/{right_display}")
        } else {
            DEV_NULL.to_string()
        };

        // 用 `similar` 按行算 unified hunk，上下文半径 3 行（与 git 默认一致）。
        // 不存在的一侧以空串参与对比，从而把整文件渲染为全增/全删。
        let unified =
            similar::TextDiff::from_lines(left_content.unwrap_or(""), right_content.unwrap_or(""))
                .unified_diff()
                .context_radius(3)
                .header(&old_header, &new_header)
                .to_string();
        diff.push_str(&unified);
        Some(diff)
    }

    /// 把绝对路径转成展示路径：能去掉 `display_root` 前缀就去掉，
    /// 并把 Windows 反斜杠统一成 `/`，保证 diff 路径在各平台一致可读。
    fn display_path(&self, path: &Path) -> String {
        let display = self
            .display_root
            .as_deref()
            .and_then(|root| path.strip_prefix(root).ok())
            .unwrap_or(path);
        display.display().to_string().replace('\\', "/")
    }
}

/// 把 git blob SHA-1 摘要格式化为 40 位十六进制字符串（即 git 的 OID）。
fn git_blob_oid(data: &[u8]) -> String {
    format!("{:x}", git_blob_sha1_hex_bytes(data))
}

/// Compute the Git SHA-1 blob object ID for the given content (bytes).
/// 计算给定内容的 git blob 对象 ID（SHA-1）。
/// git 的 blob 哈希并非对内容直接求 SHA-1，而是先拼上头部
/// `blob <字节数>\0` 再哈希——这里精确复刻该规则，使 OID 与 git 完全一致。
fn git_blob_sha1_hex_bytes(data: &[u8]) -> Output<sha1::Sha1> {
    let header = format!("blob {}\0", data.len());
    use sha1::Digest;
    let mut hasher = sha1::Sha1::new();
    hasher.update(header.as_bytes());
    hasher.update(data);
    hasher.finalize()
}

#[cfg(test)]
#[path = "turn_diff_tracker_tests.rs"]
mod tests;
