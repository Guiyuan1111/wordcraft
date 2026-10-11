# WordCraft 总体架构泛读笔记（R 阶段成果）

> 依据：实际代码阅读（2026-10），与 `docs/architecture.md`、`README.md` 交叉核对。
> 本文是《开源软件阅读维护报告》"架构设计"章节的底稿。

## 1. 项目定位

WordCraft 是 ArtCraft 团队用纯 Rust 从零实现的、对标 Microsoft Word 工作流的开源文字处理器：
ribbon 界面、样式、表格、修订、引用、邮件合并；读写 .docx（OOXML），导入 .doc/.odt/.rtf/.html/
.md/.tex/.txt，导出 PDF（真实可选中文本）；原生运行于 macOS/Windows/Linux/BSD 与浏览器
（WebAssembly）。当前版本 0.5.0（alpha），共 **460 条命令**，对 Word ribbon 功能覆盖率 ≥90.4%。

## 2. 分层架构（L0–L6 + apps）

| 层 | crate | 职责 | 行数（src，含测试） |
|---|---|---|---|
| L0 | `geom` | 点/缇/EMU/英寸单位、Rect/Point/Spin 旋转、测量解析 | 335 |
| L1 | `doc` | 文档中心模型 `Document`（多故事）+ 编辑原语 + 样式/编号/字段/公式树 | 11,192 |
| L1 | `fonts` | 字体库（内置 OFL + 系统扫描）、HarfRust 整形、Word 字体替换、MATH 表 | 2,659 |
| L1 | `proof` | Moby 词表拼写检查、规则语法检查、Liang 连字 | 1,427 |
| L2 | `layout` | 断行、分页、分栏、表格、页眉页脚、脚注、浮动环绕、公式排版、命中测试 | 12,628 |
| L2 | `docx` | ECMA-376 OOXML 读写（含加密、OMML、图表、SmartArt、VBA 保留） | 14,550 |
| L2 | `docbin` | Word 97-2003 .doc 只读（FIB/piece table/FKP/sprm） | 4,397 |
| L2 | `formats` | ODT/RTF/HTML/Markdown/LaTeX/TXT，经中间模型 `Flow` 桥接 | 10,707 |
| L3 | `render` | vello_cpu 光栅化：页面位图、缩略图、暗色反转 | 1,204 |
| L3 | `pdf` | krilla PDF 导出：子集化字体、可选中文本、链接、大纲、Tagged PDF | 2,125 |
| — | `metafile` | WMF/EMF → 中立矢量模型（render/pdf 共用） | 2,787 |
| L4 | `engine` | `Session`、undo/redo、**命令注册表**、Word 功能目录、I/O 分发 | 21,868 |
| L5 | `mcp` | MCP 服务器（16 工具；无头或桥接运行中的桌面应用） | 972 |
| — | `zotero` | Zotero 插件线协议（端口 23116），插引文/书目 | 1,671 |
| — | `control-key` | 控制通道密钥文件约定（零依赖） | 83 |
| L6 | `ui-egui` | Word 风格前端：ribbon/画布/对话框/侧栏/i18n | 21,821 |
| apps | `wordcraft` / `wordcraft-cli` / `wordcraft-web` | 桌面壳 / 命令行 / WASM | 2,000 / 383 / 334 |

分层规则由 `cargo xtask layers` 在 CI 强制：低层不得依赖高层，L6 以下禁止依赖 egui/winit/rfd。

## 3. 核心数据流

```
.docx/.doc/.odt/… ──(docx|docbin|formats 适配器)──►  Document（唯一权威模型）
                                                        │
                    Session.run(命令 id, JSON 参数) ◄── 用户/CLI/MCP/控制通道
                                                        │
                     layout(doc, cache, opts) → DocLayout{ pages: Vec<Page{ items: Vec<Placed> }> }
                                                        │
                                    display::page_display() → Vec<Draw>（显示列表）
                                            │                       │
                                render::render_page         pdf::export
                                             （vello_cpu 位图）  （krilla PDF）
```

保存反向走同一组适配器。undo 依赖 `Document` 的 `Arc<Block>` 写时复制——整文档快照 O(1)。

## 4. 模块间接口（跨 crate 边界的关键类型）

- `Document`/`Block`/`Paragraph`/`Run`/`InlineObject`/`SectionProps`/`StyleSheet`（doc）：
  所有读写器的产出、layout 的输入、engine 的操作对象。
- `InlineObject`（锚在段落文本 U+FFFC 占位符上：Image/Shape/Field/NoteRef/Equation/Group…）：
  docx 读写与 layout 摆放浮动对象/脚注/公式的统一协议；未知 OOXML 元素用 `Opaque` 保真往返。
- `FaceRef`/`shape()`/`ShapedGlyph`（fonts）：整形结果（glyph id + cluster + advance，字体单位）
  被 layout 排列；`FaceRef` 是轻量 Copy 句柄，直通 render 与 pdf 做子集化。
- `ParaLayout`/`Line`/`Cluster`/`Placed`（layout）→ `Draw`（display.rs）：
  render 与 pdf 消费同一显示列表；`Draw::Glyphs.ranges` 保留"字形→原文字节"映射保证 PDF 可选中。
- `Session`/`CommandSpec`/`Registry`（engine）：UI、CLI、MCP、控制通道四条前端的唯一执行入口。
- `Flow`（formats）：六种文本格式与 `Document` 之间降维中间模型（标题/列表/内联格式子集）。
- `Picture`/`Item`（metafile）：WMF/EMF 统一矢量模型，render/pdf 直接绘制。

## 5. 横切设计模式（开发技巧沉淀）

1. **一切皆命令**：`CommandSpec{id,label,location,shortcut,params,mutates,enabled,run}` 声明式注册；
   ribbon 内容、快捷键表、CLI 脚本接口、MCP 工具面全部由同一注册表驱动。
2. **COW + 修订计数器**：`Arc<Block>` 廉价克隆 + `Paragraph.rev` 全局原子计数器 →
   `LayoutCache` 段级记忆化（188 页文档编辑后重排约 1.4ms）。
3. **宽松且有界的解析器公约**：docx/docbin/formats/metafile 统一"未知跳过、坏值回落默认、
   计数与尺寸设上限、绝不 panic"；workspace 级 `#![deny(clippy::unwrap_used, …)]`。
4. **显示列表抽象**：排版事实（`Placed`）与绘制指令（`Draw`）分离，屏幕与 PDF 同源。
5. **架构即测试**：`cargo xtask layers`（分层）、`cargo xtask parity`（Word 功能覆盖下限只升不降）
   把架构约束写进 CI。
6. **失败不留痕**：`Session::run` 对 mutates 命令整体快照，失败回滚；`catch_unwind` 兜底 panic。

## 6. 与上游文档的差异（阅读发现）

- `docs/architecture.md` 称 15 crates，实际 16 个（`metafile` 未列入文档层表）。
- `docs/architecture.md` 的行数统计不含测试文件（如 layout 表写 9.6k，实测全目录 12,628）。
