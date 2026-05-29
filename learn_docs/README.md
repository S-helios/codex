# Codex 源码学习文档（feature-learn-v2）

> 本目录是 OpenAI Codex（`codex-rs/`）的个人学习产出：一套**对照真实源码逐条核对**的中文学习文档，
> 配套对核心源码文件的内联中文注释。本文件为总索引；规范见 [`ANNOTATION_SPEC.md`](./ANNOTATION_SPEC.md)。

文档共 24 篇（00–23），按主题归入 7 个子目录。**推荐首次阅读顺序**：按编号 00 → 23 线性推进
（详见 [`0_导览/00_overview.md`](./0_导览/00_overview.md) 里的"学习路径"图与按角色推荐）。
下面是**按目录的索引**，便于按主题查找。

---

## 0_导览/ — 入门与导航

| 文档 | 主题 |
|------|------|
| [00 - Codex 项目总览](./0_导览/00_overview.md) | Codex 是什么、技术栈、四种模式、整体架构、学习路径 |
| [01 - 仓库结构与 Crate 说明](./0_导览/01_repository_structure.md) | 目录结构、全部 crate 分组与职责 |
| [08 - 架构图集](./0_导览/08_architecture_diagrams.md) | Crate 依赖图、数据流图、沙箱图、MCP 集成等全景图 |
| [09 - core 源码精读指南](./0_导览/09_codex_core_reading_guide.md) | 15.4 万行 core 源码阅读路线图、各模块速查、避坑指南 |

## 1_架构/ — 核心架构

| 文档 | 主题 |
|------|------|
| [02 - 核心架构](./1_架构/02_core_architecture.md) | codex-core 内部：ThreadManager / Session / Agent / ToolRouter |
| [04 - Agent 主流程](./1_架构/04_agent_main_flow.md) | Agent Turn 执行时序 |
| [05 - 组件设计](./1_架构/05_component_design.md) | 核心组件拆解 |

## 2_运行时核心/ — 运行时主循环

| 文档 | 主题 |
|------|------|
| [10 - 追问 / 打断 / 进度](./2_运行时核心/10_followup_interrupt_progress.md) | 运行中追问、打断、进度汇报（InputQueue 机制） |
| [11 - 三层生命周期](./2_运行时核心/11_thread_session_turn_lifecycle.md) | Thread / Session / Turn 的创建与流转、Op 全枚举 |
| [16 - Agent 优化](./2_运行时核心/16_agent_optimization.md) | 上下文管理与自动压缩（本地 / 远程） |

## 3_执行与安全/ — 对世界动手，安全地

| 文档 | 主题 |
|------|------|
| [13 - 沙箱机制](./3_执行与安全/13_sandbox_mechanism.md) | 三平台沙箱：Seatbelt / Landlock+seccomp / Windows |
| [17 - apply-patch 与 V4A 编辑](./3_执行与安全/17_apply_patch_editing.md) | 补丁格式、解析、模糊定位、落盘与跨回合追踪 |
| [18 - 命令执行与安全](./3_执行与安全/18_exec_and_safety.md) | execpolicy / safety / 审批 / Guardian / 升权 |
| [23 - 网络代理](./3_执行与安全/23_network_proxy.md) | 本地 HTTP/SOCKS5 代理 + 域名白名单出网管控 |

## 4_工具与多Agent/ — 工具生态与协作

| 文档 | 主题 |
|------|------|
| [07 - Goal 模式](./4_工具与多Agent/07_goal_mode.md) | /goal 长时任务自动续期机制 |
| [14 - 多 Agent 系统](./4_工具与多Agent/14_multi_agent_system.md) | 子 Agent 树形协作、邮箱、权限继承 |
| [19 - MCP 双向集成](./4_工具与多Agent/19_mcp_integration.md) | 既当 MCP 客户端又当 MCP 服务器，多层审批链 |

## 5_前端_集成_协议/ — 对外接口

| 文档 | 主题 |
|------|------|
| [06 - TUI 设计](./5_前端_集成_协议/06_tui_design.md) | 终端 UI 渲染与事件处理 |
| [15 - API 与协议层](./5_前端_集成_协议/15_api_protocol_layer.md) | SQ/EQ 双队列、跨语言协议契约 |
| [20 - app-server 集成层](./5_前端_集成_协议/20_app_server_layer.md) | IDE / 远控经 JSON-RPC 落到内核 `Op` |
| [22 - 认证与登录](./5_前端_集成_协议/22_auth_and_login.md) | 四种认证模式：API Key / ChatGPT OAuth / Agent Identity / Keyring |

## 6_数据与配置/ — 状态与配置

| 文档 | 主题 |
|------|------|
| [03 - 数据库设计](./6_数据与配置/03_database_design.md) | SQLite schema、Rollout JSONL 格式、状态管理 |
| [12 - 长期记忆系统](./6_数据与配置/12_memory_system.md) | 记忆注入、维护、用量统计（独立 memories DB） |
| [21 - 配置系统](./6_数据与配置/21_config_system.md) | 分层配置、Profile、优先级与不可覆盖约束 |

---

> 说明：本系列基于某次 upstream 快照（基线提交 `3cf4cc8065`），**只增不改、不再 rebase**。
> 文档中的 `源码路径:行号` 引用均以该基线为准；若日后跟进新版本，建议另起 `feature-learn-vNEXT` 分支重做。
