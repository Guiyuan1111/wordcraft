# 关键模块精读笔记：`crates/layout`（排版引擎）

> 精读对象：12,628 行（非测试约 9,200 行），本文为标注工作与报告"关键模块详细设计"的底稿。

## 0. 模块职责与文件构成

把 `Document` 变成"页"。输入文档模型，输出页面项（`Placed`）与显示列表（`Draw`）。

| 文件 | 行数 | 职责 |
|---|---|---|
| `lib.rs` | 2,085 | 分页主循环：节→页/栏、表格落位、keep/widow 规则、页眉页脚、缓存 |
| `para.rs` | 1,833 | 段落→行：run 解析、整形、cluster、first-fit 断行、制表位、对齐/缩排 |
| `math.rs` | 2,240 | 公式排版：OMML 树→二维槽位、 stretchy 符号伸展、自动编号 |
| `display.rs` | 1,233 | 页→`Vec<Draw>` 显示列表：修订颜色、格式标记、裁剪 |
| `table.rs` | 1,010 | 表格布局：列宽测量/autofit、行跨页拆分 `split_row`、边框区域 |
| `hit.rs` | 593 | 点↔位置：光标几何、选区矩形、行导航（含双向文本） |
| `kinsoku.rs` | 103 | 东亚禁则（行首行尾禁排字符表） |
| `fields.rs` | 93 | 布局期字段上下文（PAGE/NUMPAGES、脚注编号） |
| `tests.rs`+`tests_bidi.rs` | 3,438 | 单元/属性测试（proptest） |

## 1. 业务流程（一次完整排版）

```
layout(doc, cache, opts)                      [lib.rs:1107]
 ├─ env_hash()：样式表/编号/用户词典/选项 变化则清空段级缓存
 ├─ note_numbers()：文档序扫描，给脚注/尾注分配显示编号
 ├─ Ctx{counters, fields, notes_hash, …}：分页游走状态
 ├─ for 每节 SectionProps：
 │    ├─ PageBuilder::new_page()：新页/新栏框架（body 矩形、装饰边框、栏盒）
 │    ├─ while block ≤ 节尾：
 │    │    ├─ place_para()：段落落页（keep-with-next、widow/orphan、浮动环绕重排）
 │    │    └─ place_table()：表格落页（行拆分跨页、浮动表）
 │    └─ Continuous 节直接续排当前栏
 ├─ 尾注区、垂直对齐 apply_valign()、flush_notes()（脚注延续页）
 ├─ headers_footers()：把页眉/页脚故事排版并附加到每页
 ├─ 缓存清理：保留本轮 used 的键
 └─ 建 index：(story,path)→(page,item) 供命中测试/编辑定位
```

段落落页的迭代（`place_para` → `anchor_floats` → `para_labelled`）：
浮动对象先定位→算环绕排除区 `Exclusion`→段落**带排除区重排一次**→按行高逐行推进，
行尾触发 `advance()`（下一栏/下一页）。keep 规则不满足则整段搬下一页。

## 2. 关键算法

### 2.1 first-fit 断行（`para.rs`，与 Word 一致的贪心）
- 用 `unicode-linebreak`（UAX #14）取允许断点，叠加 proof 连字点与 kinsoku 禁则，
  逐 cluster 累加宽度 `x += c.adv`；超右缘（`fits=false`）时回退到本行最后一个
  `last_break`（para.rs:1110-1131）；无断点则硬断（长词）。
- **连字区**（HYPHENATION_ZONE）：只有普通断点会留下超过连字区的空白时，才改在
  连字点断行并画连字符（`hyph_after` 二分查找 + `hyphen_glyph`）。
- **制表位四类对齐**：左/右/居中/小数点。右/居中制表用 `pending_tab` 惰性求解——
  后续文本增长时向左"吸收"制表空间并回移 `xs`（para.rs:1139-1177）。
- **两端对齐**：仅作用于 Wrap 行（末行不撑满）；空格间均分 slack；兼容模式 15 下
  允许压缩词间距（`shrink_spaces`）；Distribute 模式对字间也均分。
- **行高**：`LineSpacing::{Multiple,AtLeast,Exactly}` × 文本 ascent/descent；
  行内最高图片坐在基线上、只把行顶撑高不放大 descent（para.rs:1269-1281）。

### 2.2 段级记忆化（`LayoutCache`）
- 缓存键 `Key{rev, width, label, page, table, notes, excl, eq}`：rev 是段落修订计数器；
  其余是排版环境指纹（栏宽/列表标签/页字段值/表格样式/脚注哈希/排除区哈希/公式编号）。
- 环境哈希 `env_hash` 含样式表、编号表、默认制表、用户词典长度——用户加词触发重排。
- 淘汰：缓存超过本轮使用量 4 倍时 retain 清理（lib.rs:1294）。

### 2.3 表格布局（`table.rs`）
- 列宽三模式：固定 grid、`measure_table_columns` 内容测量、`autofit_widths`
  （按内容比例分配剩余宽度）；测量用"段落装进极窄盒二分"求最小/自然宽度。
- 行跨页：`split_row(row, cut)` 把一行在页底切成上下两半（含合并单元格矩形切分
  `split_rect`、边框区域重算）；`is_automatic` 表允许行内拆分。
- 嵌套表格深度限 8（lib.rs:588）。

### 2.4 公式排版（`math.rs`）
- OMML 树 → `MItem` 线性化 → 递归 `Slot` 布局（分数/根号/上下标/矩阵）；
  stretchy 符号（括号/根号）按 OpenType MATH 表 `GlyphPart` 伸展拼接；
  自动编号 `(eq_number)` 走 `Ctx::eq_count` 文档序计数并计入缓存键。

### 2.5 双向文本与竖排（para.rs/hit.rs/lib.rs）
- 整形后 cluster 按逻辑序；RTL 行经 UAX #9 重排，`Line.vis: Vec<VisCl>` 存每个
  cluster 的视觉盒与嵌入 level；几何读取统一走 `Line::cl_left/cl_right/spans`。
- 竖排单元格（`TextDirection::Down/Up`）：所有页面项先在"框架坐标"排好，
  最后经 `turn_point/turn_rect` 仿射映射到页面坐标；`Draw::Turned` 递归嵌套。

### 2.6 命中测试（`hit.rs`）
- 点→cluster：先按行 y 区间找行，再在 `xs/vis` 上二分找最近 cluster 边缘；
  `VisualStep` 统一上下左右键的视觉步进（双向文本/制表/对象都视作 cluster）。

## 3. 开发经验与技巧（从代码中提炼）

1. **输入即敌情**：所有 f32 经 `.clamp(0.0, 100_000.0)`/`is_finite` 消毒；深度/轮数
   有硬上限（表格深度 8、浮动表避让 64 轮、文本框 `BoxBudget`）；索引一律 `get()`。
2. **`f32::to_bits()` 进哈希键**：避免 NaN 参与键比较的未定义行为（lib.rs:412）。
3. **一次遍历双信息**：`left_out_objects` 用单个 run 游走同时解析格式与对象可见性，
   相同 run 的解析结果记忆在 `last`，避免重复样式解析（lib.rs:448-478）。
4. **浮点比较带 epsilon**：`x + adv <= right_edge + 0.01`、`y <= top + 0.01`——
   排版里 0.01pt 是约定的几何噪声容差。
5. **Arc 化布局结果**：`Arc<ParaLayout>` 在页面项/缓存/index 间共享，页重绘零拷贝。
6. **测试即规格**：`tests.rs` 用真实 Word 产出文件对拍（注释常引 docxide-pdf fixtures）；
   proptest 保证"任意文档不 panic"。
