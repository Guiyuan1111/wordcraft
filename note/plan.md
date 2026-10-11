# WordCraft 阅读维护实践计划

- 上游项目：https://github.com/storytold/wordcraft （fork：https://github.com/Guiyuan1111/wordcraft ）
- 上游基线 commit：`1d4f741`（master 分支第一版 = 初始代码版本）
- 本地基线版本号：0.5.0（`Cargo.toml` workspace.package.version）

## 任务分解与提交编号约定

提交信息以任务编号开头（沿用上游 `M2.1:` 风格），编号体系如下：

| 系列 | 内容 |
|---|---|
| `R*` | 泛读：编译运行、总体架构、模块划分、模块间接口 |
| `A*` | 标注：`crates/layout`（关键模块）逐文件中文代码标注 |
| `F*` | 维护：新增「写作洞察（Writing Insights）」功能 |
| `D*` | 报告与文档 |

## 里程碑

1. **R 阶段（泛读）**：编译安装运行验证；总体架构；16 crate 模块划分与接口。
2. **A 阶段（标注）**：关键模块选定为 `crates/layout`（排版引擎，12,628 行 ≥ 3000 行要求），
   业务流程、关键算法、开发技巧分析 + 全部源文件中文标注。
3. **F 阶段（维护）**：新增创意需求「写作洞察」——文档可读性与写作质量分析（多指标 +
   中英文支持 + engine 命令 + CLI 子命令 + UI 面板），新增代码 ≥ 2000 行，功能可运行。
4. **D 阶段**：撰写《开源软件阅读维护报告》。

## 分支与版本

- `master`：3 个版本 —— ① 初始代码（`1d4f741`）② 标注完成后 ③ 维护完成后。
- `personal`：按阅读维护过程持续细粒度提交（≥ 30 个），每个阶段完成点以 squash merge 汇入 master。
- 版本号：维护完成后 workspace 版本 0.5.0 → 0.6.0（`note/release/0.6.0.md`）。

## 临时文件约定

编译产物在 `target/`（已 gitignore）；验证用文件统一放 `TEMP/`，用后清理。

## 执行状态（收尾）

- R/A/F 三阶段全部完成；全工作区测试 1108 通过 / 0 失败（`note/report/验证记录-0.6.0.md`）。
- `master` 三版本：① `1d4f741` 初始 → ② `7b602f2` 标注（layout +2,038 行中文注释）→ ③ v0.6.0 维护版（squash 合入）。
- `personal` 全过程细粒度提交 30+。
- 版本号已升 0.6.0，发布说明在 `note/release/0.6.0.md`；报告底稿与 docx 在 `note/report/`。
