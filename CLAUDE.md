# Claude Code 项目指引（feature-learn 学习分支专属）

> 此文件仅存在于 `feature-learn` 学习分支，不存在于 upstream/main。
> 切到 main 不会看到这个文件，符合"sidecar 学习内容、不污染上游"的设计。

## 项目性质

这是 OpenAI Codex 的个人学习 fork。本分支以 upstream 某次快照为基准，**只增不改、不再 rebase**：

- `main` 分支：保持与 `upstream/main` 同步（fast-forward only），不在 main 上提交任何东西。
- `feature-learn` 分支：在 upstream 快照基础上添加学习产出，**不再 merge/rebase upstream**。
  想看新版本就基于新 tag 另起 `feature-learn-vNEXT`，本分支归档。
- 学习产出沉淀在两处：`learn_docs/`（独立文档）+ 对核心源码文件内联中文注释。

## 中文注释任务约定

当用户要求"加注释"、"翻译注释"、"补充中文说明"等任务时，**必须严格遵守**：

📖 **注释规范权威文件：[`learn_docs/ANNOTATION_SPEC.md`](./learn_docs/ANNOTATION_SPEC.md)**

执行注释任务前先完整阅读该规范，重点关注：

1. **两条铁律**：不删除任何原有英文注释；不滥加噪音注释。
2. **中英对照原则**：中文紧跟英文之后，意译 + 补充设计背景，不逐字直译。
3. **六种注释类型**：文件 / 函数 / 步骤 / 变量 / 引用范围 / 区域划分，各有触发条件。
4. **Rust 语法适配**：JSDoc `/** */` → Rust `///`；文件级注释 → `//!`。
5. **取舍标准**：去掉这条注释，读者是否会误解或浪费时间？答案是否就不写。

## 学习产出目录

- `learn_docs/00_overview.md` ~ `learn_docs/14_*.md`：模块学习笔记
- `learn_docs/ANNOTATION_SPEC.md`：本规范
- 源码内联注释：codex-rs/core/ 下若干关键文件（见 git log）

## 禁止行为

- ❌ 不要建议或执行 `git rebase upstream/main` / `git merge upstream/main` 到 feature-learn 上。
- ❌ 不要修改根目录 `AGENTS.md`（那是上游约定，不属于学习内容）。
- ❌ 不要在 codex 源码文件中删除或重写英文注释 —— 中文只做补充。
