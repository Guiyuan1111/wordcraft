//! Paragraph layout: resolve runs, shape them into clusters, break lines (first-fit, as word
//! processors do), place tabs, list labels and alignment.
//!
//! # 中文标注：模块概览
//!
//! 本文件是 WordCraft 排版引擎的“段落布局”模块，负责把一个 `Paragraph`（文档模型层的段落）
//! 排成可绘制的 `ParaLayout`。整体业务流程如下：
//!
//! 1. **解析 run**：按 `run_ranges` 遍历段落的字符属性，用文档样式表（`doc.styles.resolve_char_in`）
//!    解析成 `ResolvedChar`，并打包为可共享的 `StyleRun`（字体 + 字号 + 合成粗斜体 + 基线度量）。
//! 2. **整形（shaping）**：对每段同格式文本调用 HarfBuzz（`wordcraft_fonts::shape_run`），把字符
//!    变成带位置的 `Glyph`，并按“字形簇（grapheme cluster）”聚合为最小的排版单元 `Cluster`
//!    （文本、空格、制表符、换行/分页/分栏符、内联对象、零宽标记等）。
//! 3. **断行（first-fit）**：`break_lines` 逐行贪心装填 cluster：放不下就回退到行内最后一个
//!    可断点（`last_break`），必要时在连字点断开并画连字符（`hyphen_glyph`）；同时处理制表位、
//!    列表标签、首字下沉、环绕浮动对象的可用行宽（`row_spans`）。
//! 4. **制表位求解**：左对齐制表直接推进 `x`；右/居中/小数制表先“惰性”占位（`pending_tab`），
//!    随后文每增长一个 cluster 就把制表间隙整体左移，行尾由 `resolve_tab` 收尾定稿。
//! 5. **垂直度量**：逐行统计最高上行（ascent）/最深下行（descent），图片坐在基线上、公式按
//!    文本计，再按 `LineSpacing` 三模式（倍数 / 至少 / 固定值）算行高与基线位置。
//! 6. **对齐**：居中/右对齐整体平移；两端对齐（Justify）只拉开词间空格（行尾悬挂空格除外）；
//!    分散对齐（Distribute）把空隙均摊到每个字符之间；Word 2013+ 兼容模式还允许压缩空格。
//! 7. **产出**：所有 `Line` 装进 `ParaLayout`，连同字形表、样式表、脚注引用、公式布局、
//!    校对问题、被排除文本（隐藏文字/已删修订）等元数据，供上层绘制（display list）与编辑
//!    （光标/选区命中测试）使用。
//!
//! 与其他模块的关系：`lib.rs` 是布局 crate 的总入口（页面流、表格、分页），表格模块 `table.rs`
//! 排单元格时通过 `ParaEnv::table`（`CellText`）把表格样式的条件格式（标题行、首列…）注入到
//! 本文件的段落/字符解析链中，每个单元格文本最终仍由 `layout_para` 排版；数学公式走 `crate::math`，
//! 连字与校对走 `wordcraft_proof`，字段文本走 `crate::fields`。

use std::collections::VecDeque;
use std::sync::Arc;

use wordcraft_doc::math::MathJc;
use wordcraft_doc::numbering::{Level, LevelSuffix};
use wordcraft_doc::para::{COLUMN_BREAK, InlineObject, LINE_BREAK, NoteKind, OBJ, PAGE_BREAK, SOFT_HYPHEN};
use wordcraft_doc::props::{Align, CharProps, LineSpacing, ParaProps, TabAlign, TabLeader, TabStop};
use wordcraft_doc::resolve::{ResolvedChar, ResolvedPara};
use wordcraft_doc::{Document, Paragraph};
use wordcraft_fonts::FaceRef;

use crate::fields::{FieldCtx, field_text};

/// A face at a size with the formatting needed to draw it.
/// 【中文标注】一次“可绘制格式”的完整描述：字体面 + 字号 + 合成粗/斜 + 行度量 + 基线偏移。
/// 同一段落里格式完全相同的字符共享一个 `StyleRun`（见 `Builder::style` 的去重逻辑），
/// 绘制层从 `rc`（解析后的字符属性）里读取链接、删除线、下划线颜色等展示信息。
#[derive(Clone, Debug)]
pub struct StyleRun {
    /// 【中文标注】要使用的字体面（可能经字体回退或替换，不一定是用户请求的那个）。
    pub face: FaceRef,
    /// 【中文标注】字号（pt）；小型大写字母会按 0.8 倍缩放（见 `Builder::style`）。
    pub size: f32,
    /// 【中文标注】需要合成加粗（该字体没有真正的 Bold 字重时）。
    pub synth_bold: bool,
    /// 【中文标注】需要合成斜体（该字体没有真正的 Italic 变体时）。
    pub synth_italic: bool,
    /// 【中文标注】该 run 对应的解析后字符属性（共享所有权，格式去重的比对依据）。
    pub rc: Arc<ResolvedChar>,
    /// Line ascent/descent at this size, points.
    /// 【中文标注】该字号下的行上行/下行（pt），决定行的最小自然高度。
    pub ascent: f32,
    pub descent: f32,
    /// Baseline shift (superscript etc.), points, positive up.
    /// 【中文标注】基线偏移（上标/下标等），pt，正值向上；参与行高计算（见 break_lines 的垂直度量）。
    pub shift: f32,
}

/// A positioned glyph inside its cluster.
/// 【中文标注】簇内的单个字形：整形器输出的 glyph id 加上相对簇原点的偏移（pt）。
/// dx 还累计了同簇中前序字形的步进（见 `Builder::shape_with` 里 `adv` 的累加）。
#[derive(Clone, Copy, Debug)]
pub struct Glyph {
    /// 【中文标注】字体中的字形 id（交给绘制层取轮廓）。
    pub gid: u32,
    /// Offset from the cluster's x, points.
    /// 【中文标注】相对簇左缘的 x 偏移（pt），已含字距（rc.spacing）与缩放。
    pub dx: f32,
    pub dy: f32,
}

/// 【中文标注】簇的种类：决定断行、对齐与绘制时的特殊处理。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClKind {
    /// 【中文标注】普通文本簇。
    Text,
    /// 【中文标注】空格（含全角 U+3000）：两端对齐时被拉伸/压缩的“弹性单位”。
    Space,
    /// 【中文标注】制表符：其 adv 在断行时按制表位求解回填。
    Tab,
    /// 【中文标注】软换行（Shift+Enter，或段落内的 \r）。
    LineBreak,
    /// 【中文标注】分页符。
    PageBreak,
    /// 【中文标注】分栏符。
    ColumnBreak,
    /// Inline object with its index in `Paragraph::objects`.
    /// 【中文标注】内联对象（图片、公式、字段…），携带在 `Paragraph::objects` 里的下标。
    Object(usize),
    /// Zero-width marker (bookmark, comment anchor) or hidden text.
    /// 【中文标注】零宽标记（书签、批注锚点、软连字符、隐藏文字）：不占宽度、不可见，
    /// 但仍占据一个 cluster 以保持字节偏移的连续性。
    Marker,
}

/// The smallest unit of layout: a grapheme cluster (or object, tab, break).
/// 【中文标注】排版的最小单元：一个字位簇（grapheme cluster），或一个对象/制表符/换行符。
/// 一个簇可能对应多个字形（如连字、组合字符），通过 [g0, g1) 区间索引 `ParaLayout::glyphs`；
/// `start/end` 是它在段落文本中的字节区间，用于把几何坐标映射回编辑偏移。
#[derive(Clone, Debug)]
pub struct Cluster {
    /// 【中文标注】在段落文本中的起始字节偏移（含）。
    pub start: usize,
    /// 【中文标注】在段落文本中的结束字节偏移（不含）；断行机会按 `end` 匹配。
    pub end: usize,
    /// 【中文标注】步进宽度（pt）：簇占用的水平空间。制表簇的 adv 在断行时动态求解。
    pub adv: f32,
    /// 【中文标注】簇种类，见 [`ClKind`]。
    pub kind: ClKind,
    /// Index into `ParaLayout::styles`.
    /// 【中文标注】指向 `ParaLayout::styles` 的样式下标。
    pub style: u16,
    /// Glyph range in `ParaLayout::glyphs`.
    /// 【中文标注】字形区间 [g0, g1)（半开区间），Marker 簇两者相等（无字形）。
    pub g0: u32,
    pub g1: u32,
    /// A line may break after this cluster.
    /// 【中文标注】允许在本簇之后断行（来自 Unicode 断行算法 + 禁则处理 + 对象/制表符）。
    pub break_after: bool,
    /// Height above the baseline for objects (images), points.
    /// 【中文标注】内联对象（图片）在基线以上的高度（pt）；公式则为其上行。
    pub obj_h: f32,
    /// Depth below the baseline for objects (equations), points.
    /// 【中文标注】内联对象（公式）在基线以下的深度（pt）。
    pub obj_d: f32,
    /// The cluster is a decimal separator (decimal tabs align on it).
    /// 【中文标注】本簇是小数点（"." 或 ","）：小数制表位按它对齐（见 pending_tab 求解）。
    pub dot: bool,
}

/// 【中文标注】一行的结束方式：决定分页/分栏处理、空行补排与两端对齐是否生效。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineEnd {
    /// Wrapped (or last line).
    /// 【中文标注】自动换行（或段落最后一行）：两端对齐只对 Wrap 行生效。
    Wrap,
    /// 【中文标注】软换行符结束。
    LineBreak,
    /// 【中文标注】分页符结束。
    PageBreak,
    /// 【中文标注】分栏符结束。
    ColumnBreak,
    /// End of paragraph.
    /// 【中文标注】段落末行（含段落标记所在行）。
    Para,
}

/// A laid-out line. Coordinates are relative to the paragraph's content box: x from the
/// column's left edge, y from the top of the first line.
/// 【中文标注】排好的一行。坐标系是“段落内容盒”：x 从栏左缘起算，y 从首行顶起算（与
/// `ParaEnv::exclusions`、`ParaLayout::drop_cap` 等同一坐标系）。注意：RTL（从右向左）段落
/// 在断行阶段被“镜像”处理——xs 先从右缘起算，最后由 `reorder_line` 镜像回真实坐标。
/// 双向文本行同时维护两套表示：`xs`（视觉序的簇左缘 x）与 `vis`（每个簇的视觉盒子+双向
/// 嵌入层级），读取几何信息应统一走 `cl_left`/`cl_right` 等封装方法。
#[derive(Clone, Debug)]
pub struct Line {
    /// 【中文标注】行顶的 y（段落坐标系，pt）。
    pub top: f32,
    /// 【中文标注】行高（pt）：垂直度量与 LineSpacing 三模式的产物。
    pub height: f32,
    /// 【中文标注】基线的 y（段落坐标系，pt）：字形绘制与光标定位的基准。
    pub baseline: f32,
    /// Cluster index range.
    /// 【中文标注】本行覆盖的 cluster 区间 [c0, c1)。
    pub c0: usize,
    pub c1: usize,
    /// x of each cluster in the line, plus the end x (len = c1 - c0 + 1).
    /// 【中文标注】每个簇的左缘 x（按视觉序），末尾再附一个“行尾 x”，长度 = c1 - c0 + 1。
    pub xs: Vec<f32>,
    /// For tab clusters: the leader to draw across them.
    /// 【中文标注】制表簇到其制表位的前导符（点线/下划线等），记录 (簇下标, 前导样式)。
    pub leaders: Vec<(usize, TabLeader)>,
    /// 【中文标注】行的结束方式，见 [`LineEnd`]。
    pub end: LineEnd,
    /// Byte range of the paragraph covered by the line.
    /// 【中文标注】本行覆盖的段落文本字节区间 [start, stop)，编辑器用它做偏移↔行号映射。
    pub start: usize,
    pub stop: usize,
    /// Left and right edges available to the line.
    /// 【中文标注】本行可用的左右边界（扣除缩进与环绕对象后的净宽），pt。
    pub left: f32,
    pub right: f32,
    /// The line ends at a hyphenation point: (style, glyph, advance) of the hyphen drawn there.
    /// 【中文标注】行尾连字符：(样式下标, 字形 id, 步进)；Some 表示本行因连字点而换行并要画“-”。
    pub hyphen: Option<(u16, u32, f32)>,
    /// The text continues the previous line's row on the far side of a floating object (same top
    /// and height): one row, two lines. Row counts (line numbers, keeping rows on one page) skip it.
    /// 【中文标注】“并行行”：文本绕着浮动对象排，同一行高里对象两侧各占一条 Line。
    /// 行号统计、整行保持在同一页等逻辑要跳过它（beside == true 的行不另计一行）。
    pub beside: bool,
    /// Bidirectional lines (right-to-left text, or any line of a right-to-left paragraph): each
    /// cluster's visual box, in cluster order (len = c1 - c0), after the Unicode Bidirectional
    /// Algorithm reordered the line. Empty for plain left-to-right lines, whose clusters sit at
    /// `xs`. Read cluster geometry through [`Line::cl_left`] and friends, which handle both.
    /// 【中文标注】双向文本行的“视觉盒子表”：重排后每个簇（按逻辑序存放）的视觉 x/宽/层级。
    /// 纯 LTR 行为空（几何直接用 xs）。xs 在重排后也改存视觉序左缘。两者由下方方法统一封装。
    pub vis: Vec<VisCl>,
    /// The paragraph reads right to left (its start edge is on the right).
    /// 【中文标注】段落方向为从右向左（起始边在右）：决定段落标记画在行尾的哪一侧等。
    pub rtl: bool,
}

/// Where a cluster of a bidirectional line is drawn.
/// 【中文标注】双向文本行中一个簇的视觉盒子：绘制与命中测试统一从这里取几何。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VisCl {
    /// Left edge and width, points (paragraph coordinates, like `Line::xs`).
    /// 【中文标注】盒子左缘 x 与宽度（pt，段落坐标系，与 `Line::xs` 同基准）。
    pub x: f32,
    pub w: f32,
    /// Embedding level after rule L1 of UAX #9 (odd = right to left).
    /// 【中文标注】UAX #9 规则 L1 处理后的双向嵌入层级（奇数 = 从右向左）。
    pub level: u8,
}

impl VisCl {
    /// 【中文标注】是否为从右向左文本（层级为奇数）。
    pub fn is_rtl(&self) -> bool {
        self.level % 2 == 1
    }
    /// The edge a caret before this cluster sits at (right edge of right-to-left text).
    /// 【中文标注】“前缘”：光标停在本簇之前时的 x——RTL 文本取盒子右缘，LTR 文本取左缘。
    pub fn leading(&self) -> f32 {
        if self.is_rtl() { self.x + self.w } else { self.x }
    }
    /// The edge a caret after this cluster sits at.
    /// 【中文标注】“后缘”：光标停在本簇之后时的 x——RTL 文本取左缘，LTR 文本取右缘。
    pub fn trailing(&self) -> f32 {
        if self.is_rtl() { self.x } else { self.x + self.w }
    }
}

impl Line {
    /// Left edge of cluster `k` (a paragraph cluster index on this line).
    /// 【中文标注】簇 k 的左缘 x：纯 LTR 行直接取 xs[k-c0]；双向行取 vis 盒子的 x。
    pub fn cl_left(&self, k: usize) -> Option<f32> {
        let i = k.checked_sub(self.c0)?;
        if self.vis.is_empty() { self.xs.get(i).copied() } else { self.vis.get(i).map(|v| v.x) }
    }
    /// Right edge of cluster `k`.
    /// 【中文标注】簇 k 的右缘 x：纯 LTR 行取 xs[k-c0+1]（下一簇左缘）；双向行取盒子 x+w。
    pub fn cl_right(&self, k: usize) -> Option<f32> {
        let i = k.checked_sub(self.c0)?;
        if self.vis.is_empty() { self.xs.get(i + 1).copied() } else { self.vis.get(i).map(|v| v.x + v.w) }
    }
    /// Is cluster `k` right-to-left text?
    /// 【中文标注】簇 k 是否为 RTL 文本：查 vis 表层级奇偶；纯 LTR 行（vis 为空）恒为 false。
    pub fn cl_rtl(&self, k: usize) -> bool {
        k.checked_sub(self.c0).and_then(|i| self.vis.get(i)).is_some_and(VisCl::is_rtl)
    }
    /// Caret x after the line's last cluster (its logical end), hyphen included.
    /// 【中文标注】行末光标 x（逻辑序末簇之后），含行尾连字符宽。双向行：末簇为 RTL 时
    /// 从其盒子左缘向左延伸连字符；纯 LTR 行直接取 xs 末元素（排连字符后已写入）。
    pub fn end_x(&self) -> f32 {
        let h = self.hyphen.map_or(0.0, |h| h.2);
        match self.vis.last() {
            Some(v) if v.is_rtl() => v.x - h,
            Some(v) => v.x + v.w + h,
            None => self.xs.last().copied().unwrap_or(self.left),
        }
    }
    /// x of the line's visual end for the paragraph's direction, where its paragraph mark goes:
    /// the right edge of the rightmost cluster in a left-to-right paragraph, the left edge of the
    /// leftmost in a right-to-left one. Differs from [`Line::end_x`] when the logically last text
    /// runs the other way (Hebrew ending a left-to-right paragraph, Latin ending a right-to-left one).
    /// 【中文标注】段落方向上的“视觉行尾”：段落标记的落点。LTR 段落取所有视觉盒子右缘的
    /// 最大值，RTL 段落取左缘的最小值（与 end_x 折算 max/min）。当逻辑序末簇的走向与段落
    /// 方向相反（如 LTR 段落以希伯来文结尾）时，本值与 end_x 不同。
    pub fn visual_end_x(&self) -> f32 {
        let end = self.end_x();
        if self.rtl { self.vis.iter().map(|v| v.x).fold(end, f32::min) } else { self.vis.iter().map(|v| v.x + v.w).fold(end, f32::max) }
    }
    /// x where the hyphen of a hyphenated line (advance `adv`) is drawn.
    /// 【中文标注】行尾连字符的绘制 x。双向行：LTR 末簇画在其盒子右缘起向右；RTL 末簇从其
    /// 盒子左缘起向左（v.x - adv）。纯行：xs 末元素（行尾 x）已含连字符宽，减去 adv 即其左缘。
    pub fn hyphen_x(&self, adv: f32) -> f32 {
        match self.vis.last() {
            Some(v) if v.is_rtl() => v.x - adv,
            Some(v) => v.x + v.w,
            None => self.xs.last().copied().unwrap_or(0.0) - adv,
        }
    }
    /// Visual extents, left to right, of clusters `k0..k1`: one span on a left-to-right line,
    /// possibly several where bidirectional text splits a logical range.
    /// 【中文标注】求簇区间 [k0,k1) 的“视觉矩形”列表（选区高亮用）。纯 LTR 行是一个连续
    /// 区间；双向行把逻辑连续的盒子按 x 排序、再合并相邻（浮点容差内）——同一段逻辑文本
    /// 被 RTL 片段隔开时会得到多段视觉矩形。
    pub fn spans(&self, k0: usize, k1: usize) -> Vec<(f32, f32)> {
        let (k0, k1) = (k0.max(self.c0), k1.min(self.c1));
        if k0 >= k1 {
            return Vec::new();
        }
        if self.vis.is_empty() {
            return match (self.cl_left(k0), self.cl_left(k1).or_else(|| self.xs.last().copied())) {
                (Some(a), Some(b)) => vec![(a, b)],
                _ => Vec::new(),
            };
        }
        let mut boxes: Vec<(f32, f32)> =
            self.vis.get(k0 - self.c0..k1 - self.c0).unwrap_or(&[]).iter().filter(|v| v.w > 0.0).map(|v| (v.x, v.x + v.w)).collect();
        boxes.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut out: Vec<(f32, f32)> = Vec::new();
        for (a, b) in boxes {
            // 【中文标注】按左缘合并相邻/重叠的盒子：0.01 是浮点容差。
            match out.last_mut() {
                Some(last) if a <= last.1 + 0.01 => last.1 = last.1.max(b),
                _ => out.push((a, b)),
            }
        }
        out
    }
}

/// The list label drawn on the first line.
/// 【中文标注】首行的列表编号/项目符号标签：文本（"1." / "•"…）、样式、预排字形与几何。
/// 字形在排版标签时一次性算好（dx 从 0 累计），断行时只需按对齐方式摆放 `x`。
#[derive(Clone, Debug)]
pub struct Label {
    /// 【中文标注】标签文本（编号格式化 + Symbol 字体替换后的结果）。
    pub text: String,
    /// 【中文标注】标签的样式下标（取自段落标记的字符属性叠加编号级别属性）。
    pub style: u16,
    /// 【中文标注】标签的已整形字形（dx 相对标签原点累计，RTL 段落已在镜像阶段翻转）。
    pub glyphs: Vec<Glyph>,
    /// 【中文标注】标签原点的 x（段落坐标系）；编号按级别设置居中/右对齐时从 first-line indent 反推。
    pub x: f32,
    /// 【中文标注】标签总宽度（pt）：后随制表/空格从 x + width 处开始。
    pub width: f32,
}

/// A laid-out paragraph.
/// 【中文标注】排好的段落：几何数据（lines/clusters/glyphs）+ 语义数据（notes/issues/left_out…）
/// 的总集合，是绘制层（display list）与编辑层（光标、选区、点击命中）共同的只读视图。
#[derive(Clone, Debug)]
pub struct ParaLayout {
    /// 【中文标注】解析后的段落属性（缩进、对齐、行距、制表位、首字下沉行数等）。
    pub rp: ResolvedPara,
    /// 【中文标注】段落内去重后的样式表；cluster 与 label 用下标引用。
    pub styles: Vec<StyleRun>,
    /// 【中文标注】全部字形，按 cluster 的 [g0, g1) 区间切片取用。
    pub glyphs: Vec<Glyph>,
    /// 【中文标注】段落级 cluster 序列（逻辑序，与文本顺序一致）。
    pub clusters: Vec<Cluster>,
    /// 【中文标注】断行结果：每行的几何与字节区间。
    pub lines: Vec<Line>,
    /// 【中文标注】首行列表标签（非列表段落为 None）。
    pub label: Option<Label>,
    /// Sum of line heights (without space before/after).
    /// 【中文标注】行高总和（不含段前/段后距），分页计算用。
    pub height: f32,
    /// 【中文标注】段落文本总字节数（空行等场景兜底用）。
    pub text_len: usize,
    /// Paragraph contains fields whose text depends on the page.
    /// 【中文标注】段落含“依赖页码的字段”（PAGE 等）：页面变化后需重排本段。
    pub has_page_fields: bool,
    /// Note references (part id) in this paragraph, by cluster index.
    /// 【中文标注】脚注/尾注引用：(cluster 下标, 注释 id)，用于点击跳转与注释区排布。
    pub notes: Vec<(usize, u32)>,
    /// Proofing issues: (byte start, byte end, grammar?).
    /// 【中文标注】校对问题：拼写/语法错误的字节区间及类型（true = 语法）。
    pub issues: Vec<(usize, usize, bool)>,
    /// Drop cap: (clusters at the start that form it, lines it drops, width with its gap).
    /// 【中文标注】首字下沉：(构成下沉字的簇数, 下沉行数, 含间隙的总宽度)。
    /// 下沉字的 adv 被清零、字形在 break_lines 末尾被移到左侧悬挂位置。
    pub drop_cap: Option<(usize, u8, f32)>,
    /// Clusters after which the line may break with a hyphen (soft hyphens, auto hyphenation), sorted.
    /// 【中文标注】可加连字断行的簇下标（软连字符 + 自动连字点），升序；二分查找用。
    pub hyph_after: Vec<u32>,
    /// Bidi embedding level of each byte of the paragraph text (UAX #9, before the per-line
    /// rule L1). Empty when the paragraph is plain left-to-right text.
    /// 【中文标注】段落文本逐字节的双向嵌入层级（UAX #9，未做行级 L1）。纯 LTR 段落为空表。
    pub bidi_levels: Vec<u8>,
    /// Width of the drop cap letter itself (without the gap to the text).
    /// 【中文标注】首字下沉字母本身的宽度（不含与正文的间隙）。
    pub drop_cap_w: f32,
    /// Laid-out equations by object index.
    /// 【中文标注】公式布局结果，按对象下标存放。
    pub maths: Vec<(usize, Arc<crate::math::MathLayout>)>,
    /// Display equations (on lines of their own): cluster index and justification.
    /// 【中文标注】独立成行的公式：(cluster 下标, 对齐方式)。
    pub displays: Vec<(usize, wordcraft_doc::math::MathJc)>,
    /// Byte ranges of the text laid out as nothing ([`left_out`]): hidden text that is not shown,
    /// tracked deletions in the final text. Resolved as laid out, table formatting included.
    /// 【中文标注】被排除在排版外的文本字节区间（隐藏文字、最终稿中被删的修订），
    /// 相邻区间已合并；断行机会与连字点都会跳过它们。
    pub left_out: Vec<std::ops::Range<usize>>,
    /// The text drawn by clusters that stand for an object (field results, note numbers,
    /// equations) rather than for the paragraph's own text, by cluster index, sorted, with each of
    /// the cluster's glyphs' byte range in that text (a ligature's glyph spans its letters).
    /// 【中文标注】“代显文本”：字段结果、脚注编号等对象簇实际绘制的文字，
    /// 以及其中每个字形对应到该文字的字节区间（连字的字形横跨多个字母）——
    /// 编辑器据此把点击/选区映射回对象的字节位置。
    pub shown: Vec<(usize, String, Vec<std::ops::Range<usize>>)>,
}

/// What a table style gives the text of a cell: its paragraph and run formatting, with the
/// cell's conditional formatting (header row, first column…) applied. It sits between document
/// defaults and the paragraph's style (`StyleSheet::resolve_para_in`, `resolve_char_in`).
/// 【中文标注】表格样式为单元格文本提供的段落/字符格式（已套用条件格式：标题行、首列等）。
/// 解析优先级：文档默认 < 本结构（表格样式）< 段落自身样式。由 table.rs 构造并经
/// `ParaEnv::table` 传入 `layout_para`。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CellText {
    /// 【中文标注】表格样式给出的段落格式（缩进、对齐、行距…）。
    pub para: ParaProps,
    /// 【中文标注】表格样式给出的字符格式（字体、字号、颜色…）。
    pub chr: CharProps,
}

/// Inputs that change a paragraph's layout beyond its own content.
/// 【中文标注】段落自身内容之外的排版输入（环境）：文档上下文、栏宽、列表编号、字段缓存、
/// 隐藏文字开关、表格样式、校对开关、环绕区域与公式编号计数器。
pub struct ParaEnv<'a> {
    /// 【中文标注】文档引用：样式表、脚注格式、全局设置（默认制表距、自动连字…）都从这取。
    pub doc: &'a Document,
    /// Column width, points.
    /// 【中文标注】栏宽（pt）：段落内容盒的最大宽度。
    pub width: f32,
    /// 【中文标注】列表编号：(编号文本, 编号级别定义)；有它才排 Label 并套用级别缩进。
    pub label: Option<(String, Level)>,
    /// 【中文标注】字段求值缓存（PAGE/NUMPAGES 等在本次页面排版中已算出的值）。
    pub fields: &'a FieldCtx,
    /// 【中文标注】是否显示隐藏文字（“显示编辑标记”模式）。
    pub show_hidden: bool,
    /// Leave tracked deletions out, like hidden text (the final, "No Markup" text).
    /// 【中文标注】是否按“最终稿”隐藏已删除的修订文本。
    pub hide_deleted: bool,
    /// The table style's formatting for text in this paragraph's cell (`None` outside tables).
    /// 【中文标注】表格样式注入（单元格内段落才有；普通段落为 None）。
    pub table: Option<&'a CellText>,
    /// 【中文标注】是否做拼写/语法校对（校对结果随布局一起产出）。
    pub proofing: bool,
    /// Areas text must flow around (floating objects), relative to the paragraph: x from the
    /// column's left edge, y from the top of the first line.
    /// 【中文标注】文本必须绕开的区域（浮动对象），段落坐标系：x 从栏左缘、y 从首行顶起算。
    pub exclusions: &'a [Exclusion],
    /// Automatic equation numbers used before this paragraph.
    /// 【中文标注】本段之前已用掉的自动公式编号（保证全文公式编号连续）。
    pub eq_number: u32,
}

/// An area text wraps around.
/// 【中文标注】一个文本环绕区（浮动对象的包围盒）。坐标与 ParaEnv::exclusions 的说明一致
/// （段落内容盒坐标系）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Exclusion {
    /// 【中文标注】区域顶部 y（pt）。
    pub top: f32,
    /// 【中文标注】区域底部 y（pt）。
    pub bottom: f32,
    /// 【中文标注】区域左缘 x（pt）。
    pub left: f32,
    /// 【中文标注】区域右缘 x（pt）。
    pub right: f32,
    /// Text only above and below (no text beside it).
    /// 【中文标注】上下型环绕（Top and bottom）：左右两侧不放文字，整行让路。
    pub top_bottom: bool,
}

/// 【中文标注】排版中间状态的累加器：在 `layout_para` 里逐步填充样式/字形/簇，
/// 最后整体搬进 `ParaLayout`。与 ParaLayout 分离是为了让 &mut 借用边界清晰。
struct Builder<'a> {
    env: &'a ParaEnv<'a>,
    /// 【中文标注】正在积累的样式表。
    styles: Vec<StyleRun>,
    /// Hash of a style's format → (index in `styles`, small caps), see `style`.
    /// 【中文标注】样式去重索引：格式哈希 → (styles 下标, 是否小型大写) 桶，
    /// 见 `Builder::style`——同格式字符共享一个 StyleRun，避免样式表爆炸。
    style_index: std::collections::HashMap<u64, Vec<(u16, bool)>>,
    /// 【中文标注】正在积累的字形表。
    glyphs: Vec<Glyph>,
    /// 【中文标注】正在积累的簇序列。
    clusters: Vec<Cluster>,
    /// Bidi level per byte of the paragraph text (empty: all left to right).
    /// 【中文标注】段落文本逐字节的双向层级（空 = 全部从左向右）。
    levels: Vec<u8>,
    /// Text drawn by clusters that stand for an object, by cluster index (see `ParaLayout::shown`).
    /// 【中文标注】对象簇的“代显文本”（见 `ParaLayout::shown`）。
    shown: Vec<(usize, String, Vec<std::ops::Range<usize>>)>,
}

/// What a piece of text is shaped as: one face, case, script formatting and direction.
/// 【中文标注】整形分段键：字体面/小型大写/复杂文种格式/方向任一变化都要切开重新整形。
/// 同一 SegKey 的连续字节构成一个 shaping run，保证连字与阿拉伯连接不断裂。
#[derive(Clone, Copy)]
struct SegKey {
    /// 【中文标注】回退字体面（None = 主字体能覆盖当前字符）。
    face: Option<FaceRef>,
    /// 【中文标注】小型大写字母（小写字母缩放为小型大写）。
    small: bool,
    /// Formatted with the complex-script properties (Persian, Arabic, Hebrew…).
    /// 【中文标注】使用复杂文种（波斯/阿拉伯/希伯来…）专用的字体、字号、粗斜体设置。
    complex: bool,
    /// Odd bidi level: shaped right to left.
    /// 【中文标注】双向层级为奇数：按从右向左整形（决定 HarfBuzz 的 direction）。
    rtl: bool,
}

impl SegKey {
    /// 【中文标注】两个分段键是否等价（face 按 id 比较）：相等则并入当前整形段，否则切段。
    fn same(&self, o: &SegKey) -> bool {
        self.face.map(|f| f.id()) == o.face.map(|f| f.id()) && self.small == o.small && self.complex == o.complex && self.rtl == o.rtl
    }
}

/// Combining marks and joiners (bidi classes NSM, BN: harakat, ZWNJ, ZWJ) belong to the
/// character before them: same font, same shaping run, so Arabic joining isn't broken.
/// 【中文标注】判断是否为“附着字符”：组合符号与连接符（双向类 NSM/BN：开拉克音标、ZWNJ、ZWJ）。
/// 它们归属前一个字符——整形分段时强制沿用前一分段的字体与方向，否则阿拉伯字母的连写形式会断。
fn is_mark(c: char) -> bool {
    use unicode_bidi::BidiClass::{BN, NSM};
    matches!(unicode_bidi::bidi_class(c), NSM | BN) && !c.is_control()
}

/// Bidi embedding levels per byte of `text` (UAX #9 rules up to I2), or empty when the
/// paragraph is left to right and has no right-to-left characters.
/// 【中文标注】bidi 重排入口之一：计算文本逐字节的双向嵌入层级（UAX #9 到 I2 规则）。
/// 纯 LTR 文本返回空表（后续所有 bidi 处理都被跳过，零开销）；段落方向 rtl 时整体以
/// 层级 1 为段落级。结果存进 `Builder::levels` / `ParaLayout::bidi_levels`。
pub fn bidi_levels(text: &str, rtl_para: bool) -> Vec<u8> {
    if !rtl_para && !text.chars().any(wordcraft_fonts::is_rtl) {
        return Vec::new();
    }
    let level = if rtl_para { unicode_bidi::Level::rtl() } else { unicode_bidi::Level::ltr() };
    let info = unicode_bidi::ParagraphBidiInfo::new(text, Some(level));
    info.levels.iter().map(|l| l.number()).collect()
}

/// Rule L2 of UAX #9: the visual order (left to right) of items with these levels: from the
/// highest level down to the lowest odd one, reverse every run at that level or above.
/// 【中文标注】UAX #9 规则 L2 的实现：由逻辑序求“从左到右”的视觉序。从最高层级降到最低奇数
/// 层级，逐级反转该层级及以上的连续段——这正是“RTL 区间整段翻、其内 LTR 片段再翻回来”
/// 的经典双向重排。返回值是下标排列：order[i] = 视觉第 i 位对应逻辑序中的哪个元素。
pub fn visual_order(levels: &[u8]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..levels.len()).collect();
    let Some(min_odd) = levels.iter().copied().filter(|l| l % 2 == 1).min() else { return order };
    let max = levels.iter().copied().max().unwrap_or(0);
    let level_at = |order: &[usize], i: usize| order.get(i).and_then(|&o| levels.get(o)).copied().unwrap_or(0);
    let mut lvl = max;
    while lvl >= min_odd {
        let mut i = 0;
        while i < order.len() {
            // 从当前层级向下逐级扫描：找到一段“层级 >= lvl”的连续区间并整体反转（L2）。
            if level_at(&order, i) >= lvl {
                let mut j = i;
                while j < order.len() && level_at(&order, j) >= lvl {
                    j += 1;
                }
                if let Some(run) = order.get_mut(i..j) {
                    run.reverse();
                }
                i = j;
            } else {
                i += 1;
            }
        }
        if lvl == 0 {
            break;
        }
        lvl -= 1;
    }
    order
}

impl<'a> Builder<'a> {
    /// Style index for a resolved char in `face` (caps-scaled `size` override for small caps).
    /// 【中文标注】取（或新建）字符样式在样式表中的下标：先按格式哈希定位桶，桶内整体比对
    /// 确认，未命中才新建 StyleRun 入表。face_override 为回退字体；small 为小型大写（0.8 倍字号）。
    fn style(&mut self, rc: &Arc<ResolvedChar>, face_override: Option<FaceRef>, small: bool) -> u16 {
        let r = wordcraft_fonts::word::resolve(&rc.font, rc.bold, rc.italic);
        let face = face_override.unwrap_or(r.face);
        // The display list reads every run's format (link target, strike kind, underline colour,
        // baseline shift…) from its StyleRun, so runs share one only when their formats are equal.
        // Keyed by a hash of the commonly varying fields (no allocation: this runs for every shaped
        // piece), then confirmed by comparing whole formats within the bucket.
        // 【中文标注】绘制层要从每个 run 的样式读展示属性（链接目标、删除线、下划线颜色、
        // 基线偏移…），所以只有格式完全相等才共享一个 StyleRun：用常用变化字段的哈希分桶
        // （零分配——每个整形分段都要跑一次），桶内再整体比对确认。
        let key = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            (face.id(), small, &rc.font, rc.size.to_bits(), rc.bold, rc.italic, rc.color, rc.highlight, rc.shading).hash(&mut h);
            (rc.underline_color, rc.strike, rc.double_strike, &rc.link, rc.ins, rc.del, rc.hidden).hash(&mut h);
            h.finish()
        };
        let bucket = self.style_index.entry(key).or_default();
        // Formats that never compare equal (a NaN from a hostile file) get their own styles; a capped
        // scan keeps such a bucket from turning quadratic.
        // 【中文标注】含 NaN 的格式永不相等（对抗恶意文件）：它们各自独占样式；桶内扫描上限
        // 64 个，防止这种哈希碰撞桶退化成平方级开销。
        for &(i, s) in bucket.iter().rev().take(64) {
            if s == small
                && let Some(st) = self.styles.get(i as usize)
                && st.face.id() == face.id()
                && (Arc::ptr_eq(&st.rc, rc) || *st.rc == **rc)
            {
                return i;
            }
        }
        let size = rc.draw_size() * if small { 0.8 } else { 1.0 };
        // 小型大写字母按 0.8 倍字号排版。
        let (a, d) = wordcraft_fonts::word::line_metrics(&face);
        let k = size as f64 / face.upem.max(1.0);
        // 字号 → 字体单位的缩放系数（upem 兜底为 1，防除零）。
        // A fallback face (letters the requested font lacks) is emboldened or slanted when it has
        // no bold or italic of its own.
        // 回退字体的合成粗/斜判定：只有当回退字体本身没有对应字重/斜体时才合成。
        let (synth_bold, synth_italic) = match face_override {
            None => (r.synth_bold, r.synth_italic),
            Some(f) => (rc.bold && f.get().weight < 600.0, rc.italic && !f.get().italic),
        };
        let st = StyleRun {
            face,
            size,
            synth_bold,
            synth_italic,
            rc: rc.clone(),
            ascent: (a * k) as f32,
            descent: (d * k) as f32,
            shift: rc.baseline_shift(),
        };
        let i = self.styles.len().min(u16::MAX as usize) as u16;
        // 样式表容量钳制到 u16::MAX（cluster.style 是 u16 下标），极端文件下安全降级。
        self.styles.push(st);
        self.style_index.entry(key).or_default().push((i, small));
        i
    }

    /// Shape `text` (a piece of the paragraph starting at byte `base`) in one style and append clusters.
    /// 【中文标注】把段落中从字节 base 开始的一段同格式文本整形为簇并追加到表尾。
    fn shape(&mut self, text: &str, base: usize, rc: &Arc<ResolvedChar>, kind_override: Option<ClKind>) {
        self.shape_with(text, base, rc, kind_override, true);
    }

    /// `para_text`: `text` is paragraph text at `base`, so the paragraph's bidi levels apply
    /// (false for field results and list labels, which are shaped on their own).
    /// 【中文标注】整形主实现。para_text 指明 text 是否为段落正文：是则套用整段的 bidi 层级
    /// 决定各分段的整形方向；字段结果、列表标签是独立文本，传 false（无段落级 bidi）。
    fn shape_with(&mut self, text: &str, base: usize, rc: &Arc<ResolvedChar>, kind_override: Option<ClKind>, para_text: bool) {
        if text.is_empty() {
            return;
        }
        // Complex-script characters (Persian, Arabic…) use the run's complex-script font, size,
        // bold and italic.
        // 【中文标注】若文本含复杂文种字符（波斯/阿拉伯…），克隆一份字符属性并切换为复杂文种
        // 专用的字体/字号/粗斜体（rc.complex()），供下面的分段与样式解析使用。
        let rc_cs: Option<Arc<ResolvedChar>> = text.chars().any(|c| rc.uses_complex(c)).then(|| Arc::new(rc.complex()));
        let primary = wordcraft_fonts::word::resolve(&rc.font, rc.bold, rc.italic).face;
        let primary_cs = rc_cs.as_ref().map(|r| wordcraft_fonts::word::resolve(&r.font, r.bold, r.italic).face);
        let use_levels = para_text && !self.levels.is_empty();
        // 有双向层级且是段落正文时才按层级方向分段整形（字段结果、列表标签单独整形，无段落级 bidi）。
        // Split by font coverage (fallback faces), small caps case, script formatting and direction.
        // 以下循环把文本切成若干整形段（SegKey）：字体回退、小型大写、复杂文种、方向任一变化即切段。
        let mut seg_start = 0;
        let mut cur: Option<SegKey> = None;
        let mut segs: Vec<(usize, usize, SegKey)> = Vec::new();
        for (i, c) in text.char_indices() {
            let mark = is_mark(c);
            let complex = match cur {
                Some(k) if mark => k.complex,
                _ => rc_cs.is_some() && rc.uses_complex(c),
            };
            let prim = if complex { primary_cs.unwrap_or(primary) } else { primary };
            let face = match cur {
                Some(k) if mark => k.face,
                _ if prim.covers(c) || c.is_whitespace() || c.is_control() || c == SOFT_HYPHEN => None,
                // Stay in the fallback face the word started in while it covers the letters.
                // 主字体不覆盖时进入回退字体；只要回退字体还覆盖后续字母就继续沿用（避免一词多字体）。
                Some(SegKey { face: Some(f), .. }) if f.covers(c) => Some(f),
                _ => {
                    let (bold, italic) = if complex { (rc.bold_cs, rc.italic_cs) } else { (rc.bold, rc.italic) };
                    wordcraft_fonts::FontDb::global().fallback_styled(c, prim.id(), bold, italic).map(|f| FaceRef::of(&f))
                }
            };
            let small = rc.small_caps && !rc.caps && c.is_lowercase();
            // 小型大写：仅当启用 small_caps 且未全大写、字符本身是小写字母时缩小排版。
            let rtl = use_levels && self.levels.get(base + i).is_some_and(|l| l % 2 == 1);
            // 该字节的双向层级为奇数 → 这一段按 RTL 整形。
            let k = SegKey { face, small, complex, rtl };
            match cur {
                Some(p) if p.same(&k) => {}
                Some(p) => {
                    segs.push((seg_start, i, p));
                    seg_start = i;
                    cur = Some(k);
                }
                None => cur = Some(k),
            }
        }
        if let Some(p) = cur {
            segs.push((seg_start, text.len(), p));
        }
        for (a, b, key) in segs {
            let Some(sub) = text.get(a..b) else { continue };
            let rc = match (&rc_cs, key.complex) {
                (Some(cs), true) => cs.clone(),
                _ => rc.clone(),
            };
            let rc = &rc;
            let small = key.small;
            let si = self.style(rc, key.face, small);
            let Some(st) = self.styles.get(si as usize).cloned() else { continue };
            let upper = rc.caps || small;
            // 全大写（caps）或小型大写时，字符统一映射为大写再整形。
            let map = |c: char| if upper { c.to_uppercase().next().unwrap_or(c) } else { c };
            let shaped = if use_levels {
                // 有方向分段时用 shape_run（携带 rtl 方向），否则普通 shape（默认 LTR）。
                wordcraft_fonts::shape_run(&st.face, sub, &[], map, key.rtl)
            } else {
                wordcraft_fonts::shape(&st.face, sub, &[], map)
            };
            let k = st.size / st.face.upem.max(1.0) as f32;
            let hscale = rc.scale / 100.0;
            // 字符缩放百分比（字符间距缩放，Word 的“缩放”效果）。
            // Group glyphs by cluster byte offset; graphemes may span several shaper clusters.
            // 聚合为 Cluster：HarfBuzz 的 cluster 边界比字位簇更细，这里按 grapheme 边界
            // 归并字形——多个 shaper cluster（如连字拆出的字形）可属于同一个字位簇。
            let bounds: Vec<usize> = unicode_segmentation::UnicodeSegmentation::grapheme_indices(sub, true).map(|(i, _)| i).collect();
            let mut gi = 0usize;
            for (bi, &gs) in bounds.iter().enumerate() {
                let ge = bounds.get(bi + 1).copied().unwrap_or(sub.len());
                let g0 = self.glyphs.len() as u32;
                let mut adv = 0.0f32;
                // 收集落在该字位簇内的所有字形，dx 依次累加前序字形的步进。
                while let Some(g) = shaped.get(gi) {
                    if g.cluster >= ge {
                        break;
                    }
                    self.glyphs.push(Glyph { gid: g.gid, dx: adv + g.x_offset as f32 * k * hscale, dy: g.y_offset as f32 * k });
                    adv += g.x_advance as f32 * k * hscale;
                    gi += 1;
                }
                let g1 = self.glyphs.len() as u32;
                let s = sub.get(gs..ge).unwrap_or("");
                let ch = s.chars().next().unwrap_or(' ');
                // 按簇首字符分类：空格/全角空格 → Space；软连字符 → 零宽 Marker（可选断点）。
                let kind = kind_override.unwrap_or(if ch == ' ' || ch == '\u{3000}' {
                    ClKind::Space
                } else if ch == SOFT_HYPHEN {
                    ClKind::Marker
                } else {
                    ClKind::Text
                });
                let adv = if kind == ClKind::Marker { 0.0 } else { adv + rc.spacing };
                // Marker（软连字符）步进为 0：不占宽，仅作为断行机会存在；普通簇附加字符间距。
                self.clusters.push(Cluster {
                    start: base + a + gs,
                    end: base + a + ge,
                    adv,
                    kind,
                    style: si,
                    g0,
                    g1: if kind == ClKind::Marker { g0 } else { g1 },
                    break_after: false,
                    obj_h: 0.0,
                    obj_d: 0.0,
                    dot: s == "." || s == ",",
                });
            }
        }
    }

    /// One cluster for a whole object with its display text (fields, note references).
    /// 【中文标注】把对象（字段结果、脚注编号）的代显文本整形成“单个不可分簇”：先按普通
    /// 方式整形，再把产生的所有簇合并为一簇，并登记 shown（代显文本 + 字形↔字节区间映射）。
    fn shape_atomic(&mut self, text: &str, start: usize, end: usize, rc: &Arc<ResolvedChar>) {
        let before = self.clusters.len();
        self.shape_with(text, start, rc, None, false);
        let added: Vec<Cluster> = self.clusters.drain(before..).collect();
        let Some(first) = added.first() else {
            let si = self.style(rc, None, false);
            let g = self.glyphs.len() as u32;
            self.clusters.push(Cluster {
                start,
                end,
                adv: 0.0,
                kind: ClKind::Marker,
                style: si,
                g0: g,
                g1: g,
                break_after: false,
                obj_h: 0.0,
                obj_d: 0.0,
                dot: false,
            });
            return;
        };
        // Merge into one cluster: rebase glyph dx onto the first cluster, and keep each glyph's
        // text (a ligature's glyph takes the letters of the clusters after it that have none).
        // 合并为单簇：把后续簇的字形 dx 平移到首簇坐标系上（x 累计步进），并记录每个字形
        // 对应的文字区间——连字的字形会“继承”其后没有自有字形的簇的字母。
        let mut x = 0.0;
        let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
        for c in &added {
            for g in self.glyphs.get_mut(c.g0 as usize..c.g1 as usize).into_iter().flatten() {
                g.dx += x;
            }
            x += c.adv;
            let (a, b) = (c.start.saturating_sub(start), c.end.saturating_sub(start));
            if c.g1 <= c.g0 {
                // 没有自有字形的簇（被连字吞掉的字母）：把它并入最后一个字形的文字区间。
                let from = ranges.last().map(|r| r.start);
                for r in ranges.iter_mut().rev().take_while(|r| Some(r.start) == from) {
                    r.end = b;
                }
            }
            ranges.extend((c.g0..c.g1).map(|_| a..b));
        }
        let g0 = first.g0;
        let style = first.style;
        let g1 = added.last().map(|c| c.g1).unwrap_or(g0);
        // 记录“代显文本”：编辑器据此知道这个对象簇画了哪些字、字形对应哪些字节。
        self.shown.push((self.clusters.len(), text.to_string(), ranges));
        // 对象整体成为一个 Text 簇：字段结果等文本不可在中间断行、编辑时当作一个整体。
        self.clusters.push(Cluster { start, end, adv: x, kind: ClKind::Text, style, g0, g1, break_after: false, obj_h: 0.0, obj_d: 0.0, dot: false });
    }
}

/// Whether text with this formatting is left out of the layout: hidden text unless it is shown, and
/// tracked deletions in the final text. It takes no room, draws nothing and is no place to break.
/// 【中文标注】该格式化的文本是否被排除出排版：隐藏文字（未开显示时）或“最终稿”模式下的
/// 已删修订。被排除者不占空间、不绘制、也不构成断行点（见 layout_para 里 left 列表的用法）。
pub(crate) fn left_out(rc: &ResolvedChar, show_hidden: bool, hide_deleted: bool) -> bool {
    (rc.hidden && !show_hidden) || (rc.del.is_some() && hide_deleted)
}

/// Lay out one paragraph.
/// 【中文标注】排版一个段落的总入口。流程：解析段落/字符属性 → 逐 run 整形为 Cluster
/// （含对象、字段、公式、脚注引用、首字下沉）→ 计算断行机会与连字点 → 排列表标签 →
/// 调 `break_lines` 断行并对齐。段落自身不负责分页——那是 lib.rs 页面流的事。
pub fn layout_para(p: &Paragraph, env: &ParaEnv) -> ParaLayout {
    let doc = env.doc;
    // 段落属性解析：文档默认 < 表格样式（CellText::para）< 段落自身样式。
    let mut rp = doc.styles.resolve_para_in(&p.props, env.table.map(|t| &t.para));
    // List level indents apply unless the paragraph sets its own.
    // 列表级别的缩进只在段落没有自定义缩进时套用（左缩进 + 悬挂缩进）。
    if let Some((_, lvl)) = &env.label {
        if p.props.indent_left.is_none() {
            rp.indent_left = lvl.indent;
        }
        if p.props.indent_first.is_none() {
            rp.indent_first = -lvl.hanging;
        }
    }
    let para_style = p.props.style.as_deref();
    let table_chr = env.table.map(|t| &t.chr);
    // 字符属性解析闭包：字符默认 < 表格样式字符格式 < 段落样式 < 直接格式（resolve_char_in 内部处理）。
    let resolve = |c: &CharProps| -> Arc<ResolvedChar> { Arc::new(doc.styles.resolve_char_in(para_style, table_chr, c)) };
    // bidi 重排入口：先算整段的逐字节双向层级（纯 LTR 文本为空表）。
    let levels = bidi_levels(&p.text, rp.bidi);
    let mut b =
        Builder { env, styles: Vec::new(), style_index: Default::default(), glyphs: Vec::new(), clusters: Vec::new(), levels, shown: Vec::new() };
    let mark_rc = resolve(&p.mark);
    let mark_style = b.style(&mark_rc, None, false);
    let mut has_page_fields = false;
    let mut notes = Vec::new();
    let mut obj_index = 0usize;
    // Drop cap: the first character, sized so its capital spans `lines` lines.
    // 首字下沉前置判断：取段落第一个字符，且不能是制表/换行/分页/对象/空格等特殊字符，
    // 并要求段落后文还有内容（下沉字不能独占整段）。
    let drop_first = (rp.drop_cap > 0)
        .then(|| p.text.chars().next())
        .flatten()
        .filter(|c| !matches!(*c, '\t' | LINE_BREAK | PAGE_BREAK | COLUMN_BREAK | OBJ | '\r' | ' '))
        .filter(|c| p.text.len() > c.len_utf8());
    let mut drop_cap = None;
    let mut drop_cap_w = 0.0;
    let mut maths = Vec::new();
    let mut displays = Vec::new();
    let mut eq_counter = env.eq_number;
    let math_props = doc.settings.math.clone().unwrap_or_default();
    // The byte ranges of runs left out of the layout, in order, adjacent runs merged.
    // 主体循环：按 run 遍历段落文本。left 记录被排除（隐藏/已删修订）文本的字节区间。
    let mut left: Vec<std::ops::Range<usize>> = Vec::new();
    for (range, props) in p.run_ranges() {
        let rc = resolve(props);
        let Some(text) = p.text.get(range.clone()) else { continue };
        // 隐藏文字 / 最终稿中已删的修订：不整形、不占位，只留零宽 Marker 簇维持偏移连续。
        if left_out(&rc, b.env.show_hidden, b.env.hide_deleted) {
            match left.last_mut() {
                // 相邻的被排除区间合并成一个。
                Some(last) if last.end == range.start => last.end = range.end,
                _ => left.push(range.clone()),
            }
            let si = b.style(&rc, None, false);
            let g = b.glyphs.len() as u32;
            // Word still prints the note of a hidden reference mark (and numbers it); a deleted
            // reference is gone from the final text, note and all.
            // 【中文标注】Word 行为细节：隐藏的脚注/尾注引用仍打印其注释并编号；而“已删修订”
            // 的引用连注释一起从最终稿消失——keeps_notes 区分这两种情况。
            let keeps_notes = !(rc.del.is_some() && b.env.hide_deleted);
            for (i, c) in text.char_indices() {
                let mut note = None;
                if c == OBJ {
                    if let Some(InlineObject::NoteRef { id, .. }) = p.objects.get(obj_index)
                        && keeps_notes
                    {
                        note = Some(*id);
                    }
                    obj_index += 1;
                }
                if let Some(id) = note {
                    notes.push((b.clusters.len(), id));
                }
                b.clusters.push(Cluster {
                    start: range.start + i,
                    end: range.start + i + c.len_utf8(),
                    adv: 0.0,
                    kind: ClKind::Marker,
                    style: si,
                    g0: g,
                    g1: g,
                    break_after: false,
                    obj_h: 0.0,
                    obj_d: 0.0,
                    dot: false,
                });
            }
            continue;
        }
        let mut seg = 0;
        // 首字下沉：把首字符放大后排进开头，占 `lines` 行的高度。
        if range.start == 0
            && let Some(c) = drop_first
            && let Some(first) = text.get(..c.len_utf8())
        {
            let line_h = (rc.size * 1.2).max(1.0);
            // 单行自然行高的估计：字号 × 1.2（Word 默认行距系数）。
            let lines = rp.drop_cap as f32;
            // Capital height is ~0.7 em: the letter's cap spans from line 1's caps to line N's baseline.
            // 下沉字字号反推：大写字母高约 0.7em，要让它从第 1 行的大写顶落到第 N 行的基线，
            // 需要的字号 = ((N-1)×行高 + 本行字号×0.7) / 0.7。
            let mut big = (*rc).clone();
            // `clamp` would panic here when the text itself is larger than 1000 pt.
            // 不用 clamp：clamp 在 min>max（原文比 1000pt 还大）时会 panic；这里显式
            // “上限 1000pt、下限原字号”，保证 min <= max。
            big.size = (((lines - 1.0) * line_h + rc.size * 0.7) / 0.7).min(1000.0).max(rc.size);
            let before = b.clusters.len();
            b.shape(first, 0, &Arc::new(big), None);
            let w: f32 = b.clusters.get(before..).map(|c| c.iter().map(|c| c.adv).sum()).unwrap_or(0.0);
            // 记录下沉信息：簇数、下沉行数、宽度（字母宽 + 与正文的间隙，间隙取 0.3em）。
            drop_cap = Some((b.clusters.len() - before, rp.drop_cap, w + rc.size * 0.3));
            drop_cap_w = w;
            // The letter hangs beside the text; lines flow at the indent `break_lines` adds.
            // 下沉字的 adv 清零：它不随文本流推进，而是悬挂在文本左侧；后续行通过
            // break_lines 里临时加大的左缩进为它让位。
            for c in b.clusters.iter_mut().skip(before) {
                c.adv = 0.0;
            }
            seg = c.len_utf8();
        }
        // 逐字符扫描：特殊字符（制表/换行/分页/分栏/对象/回车）前的普通文本整段整形，
        // 特殊字符本身生成对应 kind 的簇。
        for (i, c) in text.char_indices() {
            if i < seg {
                continue;
            }
            let special = matches!(c, '\t' | LINE_BREAK | PAGE_BREAK | COLUMN_BREAK | OBJ | '\r');
            if !special {
                continue;
            }
            // 特殊字符之前的普通文本先整形入库。
            if let Some(s) = text.get(seg..i) {
                b.shape(s, range.start + seg, &rc, None);
            }
            seg = i + c.len_utf8();
            let (start, end) = (range.start + i, range.start + i + c.len_utf8());
            let si = b.style(&rc, None, false);
            let g = b.glyphs.len() as u32;
            let push = |b: &mut Builder, kind: ClKind, adv: f32, h: f32| {
                b.clusters.push(Cluster { start, end, adv, kind, style: si, g0: g, g1: g, break_after: false, obj_h: h, obj_d: 0.0, dot: false })
            };
            match c {
                '\t' => push(&mut b, ClKind::Tab, 0.0, 0.0),
                LINE_BREAK | '\r' => push(&mut b, ClKind::LineBreak, 0.0, 0.0),
                PAGE_BREAK => push(&mut b, ClKind::PageBreak, 0.0, 0.0),
                COLUMN_BREAK => push(&mut b, ClKind::ColumnBreak, 0.0, 0.0),
                _ => {
                    // OBJ（对象占位符 U+FFFC 或私有占位）：按对象类型分派排版。
                    let k = obj_index;
                    obj_index += 1;
                    match p.objects.get(k) {
                        Some(o) if o.is_drawing() => {
                            // 内联图片：占用行内宽度与高度；浮动图片只留零宽占位（环绕逻辑在页面层）。
                            if let Some((w, h, float)) = o.frame()
                                && float.wrap == wordcraft_doc::para::Wrap::Inline
                            {
                                // 可用宽 = 栏宽 - 左右缩进；过宽的图片按比例缩小塞进行内。
                                let maxw = (env.width - rp.indent_left.max(0.0) - rp.indent_right.max(0.0)).max(18.0);
                                let (w, h) = (w.clamp(1.0, 4000.0), h.clamp(1.0, 4000.0));
                                // 图片尺寸防御性钳制到 1..4000pt（对抗损坏文件里的离谱数值）。
                                let s = if w > maxw { maxw / w } else { 1.0 };
                                // The line makes room for effects (shadows) around the picture too, and
                                // for a rotated one's bounds, as Word does.
                                // 行高要容纳图片的阴影等效果外扩、以及旋转后的包围盒外扩（spin_pad）。
                                let [el, et, er, eb] = float.effect_extent();
                                let (px, py) = float.spin_pad(w * s, h * s);
                                push(&mut b, ClKind::Object(k), w * s + el + er + 2.0 * px, h * s + et + eb + 2.0 * py);
                            } else {
                                push(&mut b, ClKind::Object(k), 0.0, 0.0);
                            }
                        }
                        Some(InlineObject::Field { instr, result, .. }) => {
                            // 字段：求值出显示文本（PAGE 等依赖页码的字段会标记 has_page_fields，
                            // 页面变化时本段要重排），再作为一个不可分割的原子簇排入。
                            let (t, page_dep) = field_text(instr, result, env.fields);
                            has_page_fields |= page_dep;
                            b.shape_atomic(&t, start, end, &rc);
                        }
                        Some(InlineObject::NoteRef { kind, id, custom }) => {
                            // 脚注/尾注引用：编号来自字段缓存，自定义标记优先；固定为上标格式。
                            let num = if custom.is_empty() {
                                let n = env.fields.note_number(*id);
                                match kind {
                                    NoteKind::Footnote => doc.settings.footnote_format.format(n),
                                    NoteKind::Endnote => doc.settings.endnote_format.format(n),
                                }
                            } else {
                                custom.clone()
                            };
                            let mut sup = (*rc).clone();
                            // 脚注编号强制上标（Word 行为）。
                            sup.vert_align = wordcraft_doc::props::VertAlign::Superscript;
                            b.shape_atomic(&num, start, end, &Arc::new(sup));
                            // 记录脚注引用位置（cluster 下标 → 注释 id），供点击跳转与注释区排布。
                            notes.push((b.clusters.len().saturating_sub(1), *id));
                        }
                        Some(InlineObject::Equation { linear, display, math }) => {
                            // 公式：调用数学排版模块，得到宽度/上行/下行，作为一个 Object 簇排入。
                            let avail = (env.width - rp.indent_left.max(0.0) - rp.indent_right.max(0.0)).max(12.0);
                            let ml = crate::math::layout_equation(math, linear, &rc, *display, avail, &math_props, &mut eq_counter);
                            let st = b.styles.get(si as usize);
                            // The line is at least as tall as the text around it.
                            // 公式行至少与周围文本一样高：obj_h/obj_d 与所在 run 的上/下行取 max。
                            // obj_h 用 0.01 兜底，避免“高度为 0 的行”参与空行判定。
                            let (a, d) = st.map(|s| (s.ascent, s.descent)).unwrap_or((0.0, 0.0));
                            b.clusters.push(Cluster {
                                start,
                                end,
                                adv: ml.width,
                                kind: ClKind::Object(k),
                                style: si,
                                g0: g,
                                g1: g,
                                break_after: false,
                                obj_h: ml.ascent.max(a).max(0.01),
                                obj_d: ml.descent.max(d),
                                dot: false,
                            });
                            if *display {
                                // 独立公式（display）：独占一行，断行循环会为它单独起行并按 MathJc 对齐。
                                displays.push((b.clusters.len() - 1, math.jc));
                            }
                            maths.push((k, Arc::new(ml)));
                        }
                        Some(InlineObject::Opaque { text, .. }) => b.shape_atomic(text, start, end, &rc),
                        _ => push(&mut b, ClKind::Marker, 0.0, 0.0),
                    }
                }
            }
        }
        if let Some(s) = text.get(seg..) {
            b.shape(s, range.start + seg, &rc, None);
        }
    }
    // Break opportunities and hyphenation points come from the text as laid out: hidden text that is
    // not shown and, without markup, tracked deletions are left out, so a space or soft hyphen there
    // is no place to break, and words are hyphenated as they read.
    // The text and, for each char of it, its end there and in `p.text`.
    // 【中文标注】断行机会与连字点都要在“排版可见文本”上计算：隐藏文字、最终稿中的已删
    // 修订被排除后，其中的空格/软连字符不再是断行点，自动连字也按“读出来的词”切分。
    // text 为可见文本；ends 记录每个字符的（text 内字节尾，段落内字节尾），用于坐标映射。
    let (text, ends): (std::borrow::Cow<str>, Vec<(usize, usize)>) = if left.is_empty() {
        (p.text.as_str().into(), Vec::new())
    } else {
        let (mut text, mut ends) = (String::new(), Vec::new());
        let mut next = left.iter().peekable();
        for (i, c) in p.text.char_indices() {
            // `left` is in order: skip the ranges that end here, then is `i` in the next one?
            // 【中文标注】left 按顺序存放：先弹出在此处之前结束的排除区间，再看 i 是否落在
            // 下一个排除区间内——是则该字符不进入可见文本。
            while next.next_if(|r| r.end <= i).is_some() {}
            if next.peek().is_some_and(|r| r.start <= i) {
                continue;
            }
            text.push(c);
            ends.push((text.len(), i + c.len_utf8()));
        }
        (text.into(), ends)
    };
    // The end of a char in `text` → the same place in `p.text`.
    // 偏移映射：`text` 是“排版可见文本”（剔除了隐藏/已删部分），to_para 把它在 `text` 中
    // 的字节位置映射回原段落文本 `p.text` 的位置（无排除文本时为恒等映射）。
    let to_para = |i: usize| -> Option<usize> {
        if left.is_empty() { Some(i) } else { ends.binary_search_by_key(&i, |e| e.0).ok().and_then(|k| ends.get(k)).map(|e| e.1) }
    };
    let mut found = Vec::new();
    // 断行机会：Unicode 断行算法（UAX #14）在“可见文本”上找候选点。
    for (i, o) in unicode_linebreak::linebreaks(&text) {
        // Word keeps "and/or" and web addresses whole: no break right after a slash (a word
        // too long for the line still breaks anywhere).
        // 仿 Word：斜杠后不断行（保住 "and/or"、网址整体）；但超长单词仍可在任意处断。
        let after_slash =
            text.get(..i).is_some_and(|t| t.ends_with('/')) && text.get(i..).and_then(|t| t.chars().next()).is_some_and(char::is_alphanumeric);
        if (o == unicode_linebreak::BreakOpportunity::Allowed || i < text.len()) && !after_slash {
            found.push(i);
        }
    }
    // Asian typography: kinsoku and breaking Latin words anywhere.
    // 东亚排版：禁则处理（kinsoku，如行首禁标点）与“西文单词内任意断行”（word_wrap 模式）。
    crate::kinsoku::apply(&text, &mut found, rp.kinsoku, rp.word_wrap);
    // 把候选点从可见文本坐标映射回段落坐标，落到对应簇的 break_after 上；
    // 对象与制表符之后总允许断行。
    let opps: std::collections::HashSet<usize> = found.into_iter().filter_map(to_para).collect();
    for c in &mut b.clusters {
        c.break_after = opps.contains(&c.end) || matches!(c.kind, ClKind::Object(_) | ClKind::Tab);
    }

    // List label.
    // 列表标签排版：编号文本按“段落标记字符属性 + 编号级别字符属性”叠加解析，
    // 独立整形（不参与段落 bidi），预排好字形供首行摆放。
    let label = env.label.as_ref().map(|(text, lvl)| {
        let lc = p.mark.clone().overlaid(&lvl.chr);
        let rc = resolve(&lc);
        // A Symbol/Wingdings bullet (U+F0B7…) without that font installed, or in a face that has
        // no glyph at the private-use code: draw the standard character instead, since a
        // substitute has another glyph, or none, there.
        // 项目符号的私有区字符（U+F0B7 等）在字体被替换/缺失时，改画标准字符（如 •）——
        // 替换字体在私有区通常没有（或不是）对应字形。
        let r = wordcraft_fonts::word::resolve(&rc.font, rc.bold, rc.italic);
        let text: String = text
            .chars()
            .map(|c| match wordcraft_fonts::word::symbol_font_char(&rc.font, c) {
                Some(u) if r.substituted || !r.face.covers(c) => u,
                _ => c,
            })
            .collect();
        let before_c = b.clusters.len();
        let before_g = b.glyphs.len();
        b.shape_with(&text, 0, &rc, None, false);
        let mut cl: Vec<Cluster> = b.clusters.drain(before_c..).collect();
        // A right-to-left paragraph's label reads right to left too ("1." shows as ".1").
        // RTL 段落的标签也要镜像："1." 视觉上显示为 ".1"——对标签单独做 bidi 重排。
        if rp.bidi {
            let lv = bidi_levels(&text, true);
            let cl_lv: Vec<u8> = cl.iter().map(|c| lv.get(c.start).copied().unwrap_or(1)).collect();
            let order = visual_order(&cl_lv);
            cl = order.iter().filter_map(|&i| cl.get(i).cloned()).collect();
        }
        let mut glyphs = Vec::new();
        let mut x = 0.0;
        let mut style = mark_style;
        // 把标签的字形平移到标签自身坐标系（dx 从 0 累计），暂存进 Label；
        // 原 glyphs 里的临时数据截断回收（标签不进正文 glyph 流）。
        for c in &cl {
            style = c.style;
            for g in b.glyphs.get(c.g0 as usize..c.g1 as usize).into_iter().flatten() {
                glyphs.push(Glyph { dx: g.dx + x, ..*g });
            }
            x += c.adv;
        }
        b.glyphs.truncate(before_g);
        Label { text, style, glyphs, x: 0.0, width: x }
    });

    let mut pl = ParaLayout {
        rp,
        styles: b.styles,
        glyphs: b.glyphs,
        clusters: b.clusters,
        shown: b.shown,
        lines: Vec::new(),
        label,
        height: 0.0,
        text_len: p.text.len(),
        has_page_fields,
        notes,
        issues: if env.proofing { proof_issues(p) } else { Vec::new() },
        drop_cap,
        hyph_after: Vec::new(),
        bidi_levels: b.levels,
        drop_cap_w,
        maths,
        displays,
        left_out: Vec::new(),
    };
    // 连字点：软连字符 + （开启自动连字时的）词典/模式连字点；随后把对应簇标记为可断行。
    pl.hyph_after = hyphenation_points(&text, to_para, &pl, env.doc.settings.auto_hyphenation && !pl.rp.suppress_hyphens);
    pl.left_out = left;
    for k in pl.hyph_after.clone() {
        if let Some(c) = pl.clusters.get_mut(k as usize) {
            c.break_after = true;
        }
    }
    // 断行主阶段：first-fit 装行、制表位、垂直度量、对齐、bidi 重排。
    break_lines(&mut pl, env, mark_style, env.label.as_ref().map(|(_, l)| l));
    pl
}

/// Next tab stop after `x` (column coordinates): explicit stops, the hanging-indent implicit stop,
/// then default stops.
/// 【中文标注】求 x 之后的下一个制表位（栏坐标系）。优先级：段落显式制表位（取 x 右侧最近的）
/// > 悬挂缩进产生的隐式制表位 > 默认制表距（default_tab，2 字符兜底 36pt，Word 默认 0.5"）。
/// +0.01 是浮点容差：恰好停在制表位上的 x 不会再命中同一个制表位。
fn next_tab(x: f32, tabs: &[TabStop], default_tab: f32, hanging_at: Option<f32>) -> TabStop {
    let mut best: Option<TabStop> = tabs.iter().copied().filter(|t| t.pos > x + 0.01).min_by(|a, b| a.pos.total_cmp(&b.pos));
    // 悬挂缩进位置是隐式的左对齐制表位（比显式位更靠左时优先）。
    if let Some(h) = hanging_at
        && h > x + 0.01
        && best.is_none_or(|t| h < t.pos)
    {
        best = Some(TabStop { pos: h, align: TabAlign::Left, leader: TabLeader::None });
    }
    best.unwrap_or_else(|| {
        // 没有任何制表位时按默认制表距取整：跳到下一个 d 的整数倍。
        let d = if default_tab > 1.0 { default_tab } else { 36.0 };
        let n = ((x + 0.01) / d).floor() + 1.0;
        // 上限 1e6：防御 default_tab 异常大或 x 非常规输入导致的位置爆炸。
        TabStop { pos: (n * d).min(1e6), align: TabAlign::Left, leader: TabLeader::None }
    })
}

/// 【中文标注】断行主循环（first-fit 逐行贪心装填）。一行一行地排：先解出本行可用的左右边界
/// （缩进、首字下沉、环绕浮动对象），再从 i 开始向右装 cluster，直到装不下（回退到最后一个
/// 可断点 last_break）或遇到换行/分页/段落尾；然后做垂直度量（行距三模式）、水平对齐
/// （居中/右/两端/分散），双向文本再走 `reorder_line`。循环出口时把首字下沉的字形挂到
/// 左侧悬挂位置、RTL 标签镜像回真实坐标，最后写入 `pl.lines` / `pl.height`。
fn break_lines(pl: &mut ParaLayout, env: &ParaEnv, mark_style: u16, level: Option<&Level>) {
    let suffix = level.map(|l| l.suffix);
    let rp = pl.rp.clone();
    // 栏宽下限 12pt 防御性钳制；右边界 = 栏宽 - 右缩进（至少留 1pt）。
    let width = env.width.max(12.0);
    let base_right = (width - rp.indent_right).max(1.0);
    let default_tab = env.doc.settings.default_tab;
    // 首行左缩进 = 左缩进 + 首行缩进（悬挂缩进为负值）。
    let first_left = rp.indent_left + rp.indent_first;
    // Word 2013+ (compatibility mode 15) fits more text on a justified line by shrinking its
    // spaces, up to MAX_SPACE_SHRINK of their width; never on lines with tabs.
    // 兼容模式 15（Word 2013+）的两端对齐：允许把空格压缩至多 20% 来多塞文本；含制表符的行除外。
    let shrink_spaces = rp.align == Align::Justify && env.doc.settings.compat_mode >= wordcraft_doc::COMPAT_MODE_CURRENT;
    // 负的首行缩进 = 悬挂缩进：其位置同时是一个隐式制表位（见 next_tab）。
    let hanging_at = if rp.indent_first < 0.0 { Some(rp.indent_left) } else { None };
    let n = pl.clusters.len();
    // A right-to-left paragraph is laid out from its start edge, as if mirrored: indents, tabs,
    // the list label and alignment are measured from the right. Lines are mirrored back (and
    // bidirectional text reordered) once broken; floating objects are mirrored in.
    // RTL 段落按“镜像世界”排版：一切度量都从右缘起算，断行完成后再镜像回真实坐标
    // （reorder_line 里做），浮动对象区域也要左右翻转。
    let rtl_para = rp.bidi;
    // 【中文标注】bidi：本段是否需要逐行重排——RTL 段落，或 LTR 段落里混有 RTL 文字。
    let bidi = rtl_para || !pl.bidi_levels.is_empty();
    let mirrored: Vec<Exclusion>;
    let exclusions: &[Exclusion] = if rtl_para {
        mirrored = env.exclusions.iter().map(|e| Exclusion { left: width - e.right, right: width - e.left, ..*e }).collect();
        &mirrored
    } else {
        env.exclusions
    };
    let mut first_x0: Option<f32> = None;
    let mut hcache: Vec<(u16, u32, f32)> = Vec::new();
    // 连字符整形的按样式缓存：同一行/段多次试探不必反复整形 "-"。
    let mut lines: Vec<Line> = Vec::new();
    let mut i = 0usize;
    let mut top = 0.0f32;
    let mut first = true;
    // 首行高度估计：用段落标记的行度量乘倍数行距，用于浮动对象环绕判断；异常时兜底 14pt。
    let est_h = pl
        .styles
        .get(mark_style as usize)
        .map(|st| (st.ascent + st.descent) * if let LineSpacing::Multiple(m) = rp.line_spacing { m } else { 1.0 })
        .unwrap_or(14.0)
        .max(1.0);
    // The rest of the current row: spans on the far side of floating objects, left to right.
    // 当前行剩余的可用水平区间（绕开浮动对象后可能有多段，从左到右排队）：
    // 一“行”（row）可由多条 Line 组成（对象两侧各一条），队列空了才算进入下一行。
    let mut row_rest: VecDeque<(f32, f32)> = VecDeque::new();
    // 【中文标注】row_first：当前“行”（row）的第一条 Line 在 lines 里的下标，行高统一时用。
    let mut row_first = 0usize;
    loop {
        let beside = !row_rest.is_empty();
        let (left, right_edge) = if let Some(span) = row_rest.pop_front() {
            // 同一行的下一段可用区间：浮动对象另一侧。
            span
        } else {
            // Flow around floating objects: the spans beside them, or below them.
            // 新行：按当前 top 解析环绕区间；被“上下型”对象挡住或都太窄时下移重试。
            let mut guard = 0;
            loop {
                let mut lo = if first { first_left } else { rp.indent_left };
                // 首字下沉的行：左起点右移一个下沉宽度（字母挂在文本左侧）。
                if let Some((_, dl, dw)) = pl.drop_cap
                    && lines.len() < dl as usize
                {
                    lo = first_left.max(rp.indent_left) + dw;
                }
                match row_spans(exclusions, top, est_h, lo, base_right).map(|mut spans| (spans.pop_front(), spans)) {
                    // 拿到第一段可用区间，其余排进队列（同行的后续 Line 用）。
                    Ok((Some((a, b)), rest)) => {
                        row_rest = rest;
                        row_first = lines.len();
                        break (a, b.max(a + 12.0));
                    }
                    // Err(y)：此处放不下（上下型环绕/区间太窄），跳到 y 再试；guard 防死循环。
                    Err(below) if below > top && guard < 50 => {
                        top = below;
                        guard += 1;
                    }
                    // 没有环绕对象（或无法跳过）：整行宽可用。
                    Ok((None, _)) | Err(_) => {
                        row_first = lines.len();
                        break (lo, base_right.max(lo + 12.0));
                    }
                }
            }
        };
        let mut x = left;
        // Label on the first line.
        // 首行列表标签：按级别设置把编号对齐到首行缩进处（左/中/右），正文从标签尾开始。
        if first && let Some(lab) = pl.label.as_mut() {
            // The number is aligned at the first-line indent: its left edge, centre or right edge.
            // 编号的对齐基准：左对齐贴左缘；居中/右对齐以 first-line indent 为中心/右缘反推。
            lab.x = match level.map(|l| l.align) {
                Some(Align::Center) => left - lab.width / 2.0,
                Some(Align::Right) => left - lab.width,
                _ => left,
            };
            let end = lab.x + lab.width;
            // 编号后的后缀：制表（跳到下一个制表位）/ 一个空格 / 无。
            x = match suffix.unwrap_or(LevelSuffix::Tab) {
                LevelSuffix::Tab => {
                    let t = next_tab(end, &rp.tabs, default_tab, hanging_at).pos;
                    // The level's own tab stop after the number, when it comes first.
                    // 编号级别自带制表位位置（有限且在编号之后）时优先于通用制表位。
                    match level.and_then(|l| l.tab).filter(|p| p.is_finite() && *p > end + 0.01) {
                        Some(p) if p < t => p,
                        _ => t,
                    }
                }
                // “后随一个空格”按字号的 1/4 估宽（近似空格宽）。
                LevelSuffix::Space => end + pl.styles.get(lab.style as usize).map(|s| s.size * 0.25).unwrap_or(3.0),
                LevelSuffix::Nothing => end,
            };
        }
        let line_start_x = x;
        let c0 = i;
        let mut xs: Vec<f32> = Vec::new();
        let mut leaders = Vec::new();
        // ===== 断行内循环：从 c0 开始逐簇装行 =====
        let mut last_break: Option<usize> = None; // cluster index after which we may break
        // last_break：行内最后一个可断点（普通断点或制表符后）；放不下时回退到这里。
        let mut last_plain: Option<(usize, f32)> = None; // last non-hyphen break and the x after it
        // last_plain：最后一个“非连字”断点及其后的 x——用于连字区判断（见下）。
        let mut end = LineEnd::Para;
        let mut j = i;
        // pending_tab：右/居中/小数制表的“惰性求解”状态：(制表簇下标, 制表位, 制表起点 x)。
        let mut pending_tab: Option<(usize, TabStop, f32)> = None; // tab cluster, stop, x where tab started
        let mut space_w = 0.0f32; // width of the spaces so far on this line
        // space_w：本行已装空格总宽（压缩空格用）；has_tab：本行是否含制表符。
        let mut has_tab = false;
        while j < n {
            let Some(c) = pl.clusters.get(j).cloned() else { break };
            match c.kind {
                // 换行/分页/分栏：先收尾未决的右/居中制表，然后整行立即结束。
                ClKind::LineBreak | ClKind::PageBreak | ClKind::ColumnBreak => {
                    resolve_tab(pl, &mut pending_tab, &mut xs, c0, x, &mut x);
                    xs.push(x);
                    end = match c.kind {
                        ClKind::LineBreak => LineEnd::LineBreak,
                        ClKind::PageBreak => LineEnd::PageBreak,
                        _ => LineEnd::ColumnBreak,
                    };
                    j += 1;
                    break;
                }
                ClKind::Tab => {
                    // 制表符：先收尾上一个惰性制表，再解出下一个制表位。
                    has_tab = true;
                    resolve_tab(pl, &mut pending_tab, &mut xs, c0, x, &mut x);
                    let stop = next_tab(x, &rp.tabs, default_tab, hanging_at);
                    xs.push(x);
                    if stop.pos > right_edge + 0.01 && j > c0 && stop.align == TabAlign::Left {
                        // Tab past the right indent wraps (Word moves it to the next line).
                        // 左对齐制表越过右缩进：本行在此断开，制表符归零宽（Word 同款行为）。
                        if let Some(c) = pl.clusters.get_mut(j) {
                            c.adv = 0.0;
                        }
                        x = right_edge.max(x);
                        last_break = Some(j);
                        j += 1;
                        continue;
                    }
                    match stop.align {
                        // 左/竖线/清除制表：间隙宽度立即可知，直接写进制表簇的 adv。
                        TabAlign::Left | TabAlign::Bar | TabAlign::Clear => {
                            let w = (stop.pos - x).max(0.0);
                            if let Some(c) = pl.clusters.get_mut(j) {
                                c.adv = w;
                            }
                            if stop.leader != TabLeader::None {
                                leaders.push((j, stop.leader));
                            }
                            x += w;
                        }
                        // 右/居中/小数制表：间隙宽度取决于后续文本，先记入 pending_tab 惰性求解。
                        _ => {
                            pending_tab = Some((j, stop, x));
                            if stop.leader != TabLeader::None {
                                leaders.push((j, stop.leader));
                            }
                        }
                    }
                    // 制表符后总是可以断行（Word 行为）。
                    last_break = Some(j);
                    j += 1;
                    continue;
                }
                _ => {}
            }
            // A display equation sits on a line of its own.
            // 独立公式独占一行：行内已有可见内容时，在此断开，公式另起一行。
            let is_display = pl.displays.iter().any(|(ci, _)| *ci == j);
            if is_display && (c0..j).any(|k| pl.clusters.get(k).is_some_and(|c| c.kind != ClKind::Marker)) {
                end = LineEnd::Wrap;
                break;
            }
            // Text after a right/centre tab grows leftwards into the tab's space first.
            // “吸收”判定：右/居中/小数制表后的文本先长进制表预留的间隙里（文本向左顶）。
            // 右/小数制表：间隙 ≥ 整个簇宽即可吸收；居中制表：间隙 ≥ 半个簇宽且不出右边界。
            let absorbs = pending_tab.is_some_and(|(tj, stop, _)| {
                let room = pl.clusters.get(tj).map(|t| t.adv).unwrap_or(0.0);
                match stop.align {
                    TabAlign::Right | TabAlign::Decimal => room >= c.adv,
                    TabAlign::Center => room >= c.adv / 2.0 && x + c.adv / 2.0 <= right_edge + 0.01,
                    _ => false,
                }
            });
            let shrink = if shrink_spaces && !has_tab { space_w * MAX_SPACE_SHRINK } else { 0.0 };
            // fits 判定：能吸收进制表间隙 / 加宽后不超右界（含压缩空格的余量与 0.01 容差）/
            // 或本簇是空格、Marker（空格允许悬挂在行尾，Marker 零宽）。
            let fits = absorbs || x + c.adv <= right_edge + shrink + 0.01 || c.kind == ClKind::Space || c.kind == ClKind::Marker;
            if !fits && j > c0 {
                // Wrap: back up to the last break opportunity on this line.
                // 放不下：回退（first-fit 的“回退”步骤）。
                end = LineEnd::Wrap;
                // Hyphenate only when the plain break leaves more than the hyphenation zone empty.
                // 连字区规则（Word HYPHENATION_ZONE = 0.25" = 18pt）：只有当“按普通断点断开”
                // 会在行尾留下超过一个连字区的空白、且该处确有连字点时，才改用更早的连字断点——
                // 即 last_plain（普通断点）在 last_break（连字点）之前、且剩余空间 ≤ 连字区。
                if let (Some(bk), Some((pj, px))) = (last_break, last_plain)
                    && pj >= c0
                    && pj < bk
                    && pl.hyph_after.binary_search(&(bk as u32)).is_ok()
                    && right_edge - px <= HYPHENATION_ZONE
                {
                    last_break = Some(pj);
                }
                if let Some(bk) = last_break.filter(|bk| *bk >= c0) {
                    // 截断 xs 到断点，j 回退到断点之后；x 重算为断点处的行尾位置。
                    xs.truncate(bk + 1 - c0);
                    j = bk + 1;
                    // Recompute x for the kept clusters.
                    // 【中文标注】按保留的前缀重算行笔位 x：截断后 xs 的末元素加上断点簇的步进。
                    x = xs.last().copied().unwrap_or(line_start_x) + pl.clusters.get(bk).map(|c| c.adv).unwrap_or(0.0);
                } else {
                    // No opportunity: break here (a long word).
                    // 行内没有任何断点（超长单词）：硬断在当前位置。
                }
                break;
            }
            xs.push(x);
            x += c.adv;
            if c.kind == ClKind::Space {
                space_w += c.adv;
            }
            // Decimal/center/right tab: shift pending text as it grows.
            // 惰性求解的核心：右/居中/小数制表后的文本每增长一簇，就把制表间隙（制表簇的 adv）
            // 与其后所有簇的 x 整体左移，使对齐锚点始终钉在制表位上——
            // 右对齐：文本尾钉在 stop.pos；居中：文本中点钉在 stop.pos；
            // 小数：小数点（首个 dot 簇）钉在 stop.pos，没有小数点则按右对齐处理。
            if let Some((tj, stop, tx)) = pending_tab {
                let seg_w = x - tx;
                let shift = match stop.align {
                    TabAlign::Right => stop.pos - tx - seg_w,
                    TabAlign::Center => stop.pos - tx - seg_w / 2.0,
                    TabAlign::Decimal => {
                        // Width up to the first '.' in the segment.
                        // 从制表簇向后找第一个小数点：锚定“点到制表位”的宽度。
                        let mut w = 0.0;
                        let mut found = false;
                        for k in tj + 1..=j {
                            let Some(cc) = pl.clusters.get(k) else { break };
                            let s = pl_text_at(cc);
                            if found {
                                break;
                            }
                            if s {
                                found = true;
                                break;
                            }
                            w += cc.adv;
                        }
                        stop.pos - tx - if found { w } else { seg_w }
                    }
                    _ => 0.0,
                };
                // shift 不允许为负：文本已越过制表位时不再左拉（锚点卡在 0 处）。
                let shift = shift.max(0.0);
                let old = pl.clusters.get(tj).map(|c| c.adv).unwrap_or(0.0);
                // 增量超过 0.001 才写回，避免浮点抖动反复改写。
                if (shift - old).abs() > 0.001 {
                    let delta = shift - old;
                    if let Some(c) = pl.clusters.get_mut(tj) {
                        c.adv = shift;
                    }
                    let from = tj + 1 - c0;
                    for v in xs.iter_mut().skip(from) {
                        *v += delta;
                    }
                    x += delta;
                }
            }
            if is_display {
                // Markers right after it stay on its line; anything else starts a new one.
                // 独立公式行：紧随其后的零宽 Marker 留在本行，其余内容另起一行。
                j += 1;
                while j < n && pl.clusters.get(j).is_some_and(|c| c.kind == ClKind::Marker) {
                    xs.push(x);
                    j += 1;
                }
                end = if j >= n { LineEnd::Para } else { LineEnd::Wrap };
                break;
            }
            if c.break_after {
                // 记录本簇后的断点：普通断点同时记入 last_break 与 last_plain；
                // 连字断点只在“加上连字符后仍不超行宽”时才记入 last_break。
                let hyph = pl.hyph_after.binary_search(&(j as u32)).is_ok();
                if !hyph {
                    last_break = Some(j);
                    last_plain = Some((j, x));
                } else if x + hyphen_glyph(pl, c.style, &mut hcache).2 <= right_edge + shrink + 0.01 {
                    last_break = Some(j);
                }
            }
            j += 1;
        }
        // 行被换行/段落尾结束时：收尾未决的右/居中/小数制表（文本最终定型，锚点求解一次到位）。
        if end == LineEnd::Para || end == LineEnd::Wrap {
            let xc = x;
            resolve_tab(pl, &mut pending_tab, &mut xs, c0, xc, &mut x);
        }
        // 装到段落尾且没有显式换行：本行就是段落末行。
        if j >= n && end != LineEnd::Wrap && !matches!(end, LineEnd::LineBreak | LineEnd::PageBreak | LineEnd::ColumnBreak) {
            end = LineEnd::Para;
        }
        let c1 = j.max(c0);
        // Ensure progress.
        // 保证前进性：一行至少吞一个簇，防止零宽内容（全 Marker 行等）造成死循环。
        let (c1, j) = if c1 == c0 && c0 < n { (c0 + 1, c0 + 1) } else { (c1, j) };
        // 补齐/截断 xs：长度必须等于 c1 - c0（每簇一个 x）。
        while xs.len() < c1 - c0 {
            let last = xs.last().copied().unwrap_or(line_start_x);
            let adv = pl.clusters.get(c0 + xs.len().saturating_sub(1)).map(|c| c.adv).unwrap_or(0.0);
            xs.push(if xs.is_empty() { line_start_x } else { last + adv });
        }
        xs.truncate(c1 - c0);
        // A line wrapped at a hyphenation point shows a hyphen after its last cluster.
        // 若本行断在连字点：行尾补一个连字符（样式、字形、步进），end_x 也要算上它的宽。
        let hyphen = (end == LineEnd::Wrap && c1 > c0 && pl.hyph_after.binary_search(&((c1 - 1) as u32)).is_ok())
            .then(|| pl.clusters.get(c1 - 1).map(|c| hyphen_glyph(pl, c.style, &mut hcache)))
            .flatten();
        let end_x = match (xs.last(), c1.checked_sub(1).and_then(|k| pl.clusters.get(k))) {
            (Some(lx), Some(c)) => lx + c.adv + hyphen.map_or(0.0, |h| h.2),
            _ => line_start_x,
        };
        // xs 末尾附加“行尾 x”（长度 = 簇数 + 1，见 Line::xs 的注释）。
        xs.push(end_x);
        pending_tab = None;
        let _ = pending_tab;

        // Vertical metrics.
        // ===== 垂直度量：逐簇取最大上行/下行，求行高与基线 =====
        let (mut asc, mut desc) = (0.0f32, 0.0f32);
        // The tallest picture on the line (it sits on the baseline).
        // obj_asc：行内最高图片的高度（图片坐在基线上）。
        let mut obj_asc = 0.0f32;
        let mut any = false;
        let dropped = pl.drop_cap.map_or(0, |d| d.0);
        for (k, c) in pl.clusters.get(c0..c1).into_iter().flatten().enumerate() {
            // Marker 与首字下沉的簇不参与行高（下沉字已在 shaping 时单独放大）。
            if c.kind == ClKind::Marker || c0 + k < dropped {
                continue;
            }
            if let Some(st) = pl.styles.get(c.style as usize) {
                // A picture sits on the baseline of its run: the line keeps that font's ascent
                // and descent, and grows above the baseline to fit a taller picture.
                // 图片的基线规则：图片坐在其所在 run 的基线上——行的上下行仍取该字体度量，
                // 图片更高时只向上（基线以上）撑高。
                let (a, d) = if matches!(c.kind, ClKind::Object(_)) && c.obj_h > 0.0 {
                    if pl.maths.iter().any(|(k, _)| c.kind == ClKind::Object(*k)) {
                        // An equation is text: its ascent and descent count like the text's.
                        // 公式按“文本”计：上/下行直接用公式的 obj_h/obj_d（参与行距缩放）。
                        (c.obj_h, c.obj_d)
                    } else {
                        obj_asc = obj_asc.max(c.obj_h);
                        (st.ascent, st.descent)
                    }
                } else {
                    // 普通文本：上/下行要计入基线偏移（上标抬升上行、下标加深下行）。
                    (st.ascent + st.shift.max(0.0), st.descent + (-st.shift).max(0.0))
                };
                asc = asc.max(a);
                desc = desc.max(d);
                any = true;
            }
        }
        // 整行没有任何有效度量（空行/全 Marker）：退回段落标记的行度量。
        if (!any || (asc == 0.0 && desc == 0.0))
            && let Some(st) = pl.styles.get(mark_style as usize)
        {
            asc = st.ascent;
            desc = st.descent;
        }
        // 列表标签也要撑首行行高（大号编号不挤压）。
        if first
            && let Some(lab) = &pl.label
            && let Some(st) = pl.styles.get(lab.style as usize)
        {
            asc = asc.max(st.ascent);
            desc = desc.max(st.descent);
        }
        // Line spacing scales the text's height, never a picture's: a line is at least as tall
        // as its tallest picture plus the descent below the baseline.
        // 行距三模式：Multiple（倍数）只缩放“文本高度”；AtLeast（至少）取 max(文本高, 至少值)；
        // Exactly（固定值）无视内容直接用给定行高（过高图片会溢出，Word 同款行为）。
        // 但无论哪种模式，行高都不得小于“最高图片 + 下行”——图片不能被行距裁掉。
        // shortcut: Word leaves less than a full descent below a line of only pictures (0.5pt to
        // 1.9pt in docxide-pdf's header fixtures case121/case123, where the descent is 3.3pt), and
        // no rule tried so far (none, only the text's descent) matched both; it needs Word probes.
        // 已知偏差：纯图片行 Word 在基线下留的距离小于完整下行（0.5~1.9pt，实测样例中
        // 下行为 3.3pt），现有规则未复现，需对 Word 探针才能定论（此处保守取下行全值）。
        let natural = asc + desc;
        let pictures = if obj_asc > asc { obj_asc + desc } else { 0.0 };
        let height = match rp.line_spacing {
            LineSpacing::Multiple(m) => (natural * m).max(pictures),
            LineSpacing::AtLeast(v) => natural.max(pictures).max(v),
            LineSpacing::Exactly(v) => v,
        };
        // 基线 = 行顶 + 行高 - 下行：从底部往上留出下行空间。
        let baseline = top + height - desc;

        // Alignment / justification (trailing spaces hang).
        // ===== 水平对齐 =====
        // 先求“内容尾”content_end：从行尾向左跳过悬挂的空格/Marker/换行符——
        // 行尾空格不占对齐宽度（Word 的“悬挂空格”行为）。
        let mut content_end = xs.last().copied().unwrap_or(line_start_x);
        for k in (c0..c1).rev() {
            match pl.clusters.get(k).map(|c| c.kind) {
                Some(ClKind::Space) | Some(ClKind::Marker) | Some(ClKind::LineBreak) | Some(ClKind::PageBreak) | Some(ClKind::ColumnBreak) => {
                    content_end = xs.get(k - c0).copied().unwrap_or(content_end);
                }
                _ => break,
            }
        }
        // slack：右边界与内容尾之间的剩余空间（正值 = 有富余，负值 = 溢出）。
        let slack = right_edge - content_end;
        // 对齐从“最后一个制表符之后”开始：制表符引导的内容对齐不受段落对齐影响。
        let last_tab = (c0..c1).rev().find(|k| pl.clusters.get(*k).is_some_and(|c| c.kind == ClKind::Tab));
        let align_from = last_tab.map(|k| k + 1).unwrap_or(c0);
        // 独立公式行按 MathJc（公式自带的对齐声明）优先于段落对齐。
        let display = pl.displays.iter().find(|(ci, _)| *ci >= c0 && *ci < c1).map(|d| d.1);
        let shift = match (display, rp.align) {
            (Some(MathJc::Left), _) => 0.0,
            // 整体平移：居中取剩余空间的一半，右对齐取全部剩余。
            (Some(MathJc::Right), _) => slack,
            (Some(_), _) => slack / 2.0,
            (None, Align::Center) => slack / 2.0,
            (None, Align::Right) => slack,
            _ => 0.0,
        };
        if shift > 0.0 && slack > 0.0 {
            for v in xs.iter_mut().skip(align_from - c0) {
                *v += shift;
            }
        }
        // 两端对齐 vs 分散对齐的区别：Justify 只拉伸“因自动换行结束的行”里的词间空格
        // （末行、软换行行都不拉伸）；Distribute 无条件把空隙均摊到每个字符之间。
        let justify = display.is_none() && ((rp.align == Align::Justify && end == LineEnd::Wrap) || rp.align == Align::Distribute);
        // A line that only fits with shrunk spaces is shrunk, the last line of the paragraph too.
        // 空格压缩（Word 2013+）：行溢出（slack < 0）且本行无制表符时压缩行内空格。
        let shrink = display.is_none() && shrink_spaces && slack < 0.0 && last_tab.is_none();
        if shrink {
            shrink_line_spaces(pl, &mut xs, c0, c1, content_end, slack);
        } else if justify && slack > 0.0 {
            // Spaces inside the content (after the last tab).
            // 参与拉伸的词间空格：内容区内、x 在 content_end 之前（排除行尾悬挂空格）。
            let spaces: Vec<usize> = (align_from..c1)
                .filter(|k| {
                    pl.clusters.get(*k).is_some_and(|c| c.kind == ClKind::Space) && xs.get(k - c0).copied().unwrap_or(f32::MAX) < content_end - 0.01
                })
                .collect();
            if rp.align == Align::Distribute || spaces.is_empty() {
                // 分散对齐（或没有空格可拉）：把空隙均摊到每两个相邻簇之间——
                // 每个簇的 x 按其前面的空隙份数整体右移。
                let cnt = (c1 - align_from).saturating_sub(1).max(1) as f32;
                if rp.align == Align::Distribute {
                    let per = slack / cnt;
                    for (q, v) in xs.iter_mut().enumerate().skip(align_from - c0) {
                        let k = (q + c0 - align_from) as f32;
                        *v += per * k.min(cnt);
                    }
                }
            } else {
                // 两端对齐：把剩余空间平均加到每个词间空格上，空格之后的簇依次右移。
                let per = slack / spaces.len() as f32;
                let mut add = 0.0;
                let mut si = 0;
                for k in align_from..c1 {
                    if let Some(v) = xs.get_mut(k - c0) {
                        *v += add;
                    }
                    if spaces.get(si) == Some(&k) {
                        add += per;
                        si += 1;
                    }
                }
                if let Some(v) = xs.last_mut() {
                    *v += add;
                }
            }
        }
        // 行的字节区间：start/stop 取首末簇在段落文本中的位置（供编辑器做行↔偏移映射）。
        let start = pl.clusters.get(c0).map(|c| c.start).unwrap_or(pl.text_len);
        let stop = if c1 > c0 { pl.clusters.get(c1 - 1).map(|c| c.end).unwrap_or(pl.text_len) } else { start };
        // 记录首行首簇的 x（首字下沉字形悬挂位置的计算要用）。
        if first_x0.is_none() {
            first_x0 = xs.first().copied();
        }
        let mut line = Line {
            top,
            height,
            baseline,
            c0,
            c1,
            xs,
            leaders,
            end,
            start,
            stop,
            left,
            right: right_edge,
            hyphen,
            beside,
            vis: Vec::new(),
            rtl: rtl_para,
        };
        // 双向段落（或含 RTL 片段的段落）：对本行做 L1/L2 重排并生成 vis 视觉盒子。
        if bidi {
            reorder_line(pl, &mut line, width);
        }
        lines.push(line);
        first = false;
        i = j;
        // Text that wrapped continues on the row's far side; anything else ends the row.
        // 因自动换行结束的行可以续排到同一行（row）的浮动对象另一侧；显式换行/段落尾则结束整行。
        if end != LineEnd::Wrap || i >= n {
            row_rest.clear();
        }
        if row_rest.is_empty() {
            // The row is as tall as its tallest line.
            // 一“行”（row）排完：行高统一取行内最高的那条（beside 的两条线等高），
            // 各行基线按统一行高下移，top 推进到下一行。
            let row = lines.get_mut(row_first..).unwrap_or_default();
            let h = row.iter().map(|l| l.height).fold(0.0f32, f32::max);
            for l in row {
                l.baseline += h - l.height;
                l.height = h;
            }
            top += h;
        }
        if i >= n {
            // A paragraph ending with a line break gets an empty last line.
            // 【中文标注】段落以显式换行/分页/分栏符结束时补一个空行承载段落标记——
            // Word 同款行为（按 Enter 之后光标停驻的那个空行）。
            if matches!(end, LineEnd::LineBreak | LineEnd::PageBreak | LineEnd::ColumnBreak) {
                let (asc, desc) = pl.styles.get(mark_style as usize).map(|s| (s.ascent, s.descent)).unwrap_or((10.0, 3.0));
                let natural = asc + desc;
                let h = match rp.line_spacing {
                    LineSpacing::Multiple(m) => natural * m,
                    LineSpacing::AtLeast(v) => natural.max(v),
                    LineSpacing::Exactly(v) => v,
                };
                let x0 = rp.indent_left;
                let (x0, left, right) = if rtl_para { (width - x0, width - base_right, width - x0) } else { (x0, x0, base_right) };
                lines.push(Line {
                    top,
                    height: h,
                    baseline: top + h - desc,
                    c0: n,
                    c1: n,
                    xs: vec![x0],
                    leaders: Vec::new(),
                    end: LineEnd::Para,
                    start: pl.text_len,
                    stop: pl.text_len,
                    left,
                    right,
                    hyphen: None,
                    beside: false,
                    vis: Vec::new(),
                    rtl: rtl_para,
                });
                top += h;
            }
            break;
        }
        if lines.len() > 200_000 {
            // 【中文标注】防失控保险丝：病态文件/极端内容导致行数爆炸时强制终止断行循环。
            break;
        }
    }
    // Hang the drop cap from the first line's caps down to line N's baseline, left of the text.
    // 【中文标注】首字下沉悬挂：把下沉字的字形整体平移到正文左侧——垂直下移 dy（首行基线
    // 到第 N 行基线的距离，行数不足时按 (N-1)×首行高估算）；水平退让 dx 取“下沉宽度”与
    // “实际可用缩进空间”的较小值，避免把字母推出栏外。
    if let Some((nc, dl, dw)) = pl.drop_cap
        && let (Some(l0), Some(ln)) = (lines.first(), lines.get((dl as usize).min(lines.len()).saturating_sub(1)))
    {
        let short = lines.len() < dl as usize;
        let dy = if short { (dl as f32 - 1.0) * l0.height } else { ln.baseline - l0.baseline };
        let dx = dw.min(first_x0.unwrap_or(0.0) - first_left.max(rp.indent_left));
        // Toward the start edge: left, or right (and from the letter's right edge) when mirrored.
        // 【中文标注】朝段落起始边悬挂：LTR 段向左移 dx；RTL 段从字母右缘起算（镜像减法）。
        let dx = if rtl_para { pl.drop_cap_w - dx } else { dx };
        if short {
            top = top.max(l0.top + l0.height * dl as f32);
        }
        for c in pl.clusters.get(..nc).unwrap_or(&[]).to_vec() {
            for g in pl.glyphs.get_mut(c.g0 as usize..c.g1 as usize).into_iter().flatten() {
                g.dy -= dy;
                g.dx -= dx;
            }
        }
    }
    if rtl_para && let Some(lab) = pl.label.as_mut() {
        // 【中文标注】RTL 段落：列表标签的 x 从“镜像世界”换算回真实坐标（右缘反推）。
        lab.x = width - lab.x - lab.width;
    }
    pl.height = top;
    pl.lines = lines;
}

/// Finish a broken line of a bidirectional paragraph: resolve each cluster's level for the line
/// (UAX #9 rule L1), order the clusters visually (rule L2) and give each its box. `line.xs`
/// arrive in logical order measured from the paragraph's start edge (mirrored for a
/// right-to-left paragraph); afterwards `line.vis` holds the boxes and `xs` their left edges.
/// 【中文标注】双向段落断行后的“行收尾”（bidi 重排主入口之一）：先按 UAX #9 规则 L1 修正
/// 行内各簇的层级，再按规则 L2（visual_order）求视觉序，为每个簇生成 VisCl 视觉盒子。
/// 进入时 line.xs 是按逻辑序、从段落起始边起算的 x（RTL 段落已镜像度量）；返回后
/// line.vis 存视觉盒子、xs 改存视觉序左缘（末元素仍是行尾光标 x）。
fn reorder_line(pl: &ParaLayout, line: &mut Line, width: f32) {
    // 【中文标注】RTL 段落先交换行的左右边界（断行阶段是镜像度量），最后统一换算回真实坐标。
    let rtl_para = line.rtl;
    if rtl_para {
        let (l, r) = (line.left, line.right);
        line.left = width - r;
        line.right = width - l;
    }
    let (c0, c1) = (line.c0, line.c1);
    let n = c1.saturating_sub(c0);
    // 【中文标注】空行（只含段落标记的行）：RTL 段落仅镜像 xs，不做重排。
    if n == 0 {
        if rtl_para {
            line.xs = line.xs.iter().map(|x| width - x).collect();
        }
        return;
    }
    let para_level = u8::from(rtl_para);
    let mut levels: Vec<u8> = (c0..c1).map(|k| pl.clusters.get(k).and_then(|c| pl.bidi_levels.get(c.start)).copied().unwrap_or(para_level)).collect();
    // L1: tabs and breaks, and the whitespace before them or at the end of the line, take the
    // paragraph level (so trailing spaces stay at the line's end edge).
    // 【中文标注】规则 L1：制表符与换行符本身，以及行末或它们之前连续的空白/零宽标记，
    // 层级一律重置为段落层级——保证行尾空格留在行的“结束边”、空白贴着制表位。
    // trailing 标记从行尾向前扫描时是否正处于“尾随空白段”中。
    let mut trailing = true;
    for (i, k) in (c0..c1).enumerate().rev() {
        let kind = pl.clusters.get(k).map(|c| c.kind);
        let reset = match kind {
            // 【中文标注】制表/换行：自身与其前空白都重置为段落级，并重新进入尾随段。
            Some(ClKind::Tab | ClKind::LineBreak) => {
                trailing = true;
                true
            }
            // 【中文标注】空白/分页/分栏/零宽标记：仅在尾随段中才重置（即 L1 的“行末与制表前”）。
            Some(ClKind::Space | ClKind::PageBreak | ClKind::ColumnBreak | ClKind::Marker) => trailing,
            // 【中文标注】普通文本/对象：结束尾随段。
            _ => {
                trailing = false;
                false
            }
        };
        if reset && let Some(v) = levels.get_mut(i) {
            *v = para_level;
        }
    }
    // 【中文标注】LTR 段落里整行都是 0 级（RTL 文字在其他行）：无需重排，几何保持 xs 原样。
    if !rtl_para && levels.iter().all(|l| *l == 0) {
        return; // A left-to-right line of a paragraph with right-to-left text elsewhere.
    }
    // 【中文标注】规则 L2：由修正后的层级求“逻辑序 → 视觉序”的置换（逐级反转）。
    let order = visual_order(&levels);
    // 【中文标注】各簇宽 = xs 相邻差；末簇宽扣除行尾连字符（连字符不属于任何簇）。
    let hyph = line.hyphen.map_or(0.0, |h| h.2);
    let mut w: Vec<f32> = (0..n).map(|i| line.xs.get(i + 1).zip(line.xs.get(i)).map_or(0.0, |(b, a)| b - a)).collect();
    if let Some(last) = w.last_mut() {
        *last -= hyph;
    }
    let total: f32 = w.iter().sum();
    let x0 = line.xs.first().copied().unwrap_or(0.0);
    // 【中文标注】视觉摆放起点：LTR 行直接用 xs[0]；RTL 行的 xs 从右缘起算，换算为真实左缘
    // = 栏宽 - 起始 x - 视觉总宽。
    let mut x = if rtl_para { width - x0 - total } else { x0 };
    // 【中文标注】按视觉序从左到右摆放盒子：order[i] 是“视觉第 i 位”的逻辑簇下标。
    let mut vis = vec![VisCl { x: 0.0, w: 0.0, level: para_level }; n];
    for &i in &order {
        if let (Some(v), Some(wi), Some(l)) = (vis.get_mut(i), w.get(i), levels.get(i)) {
            *v = VisCl { x, w: *wi, level: *l };
            x += *wi;
        }
    }
    // 【中文标注】用视觉盒子重建 xs：每簇的视觉左缘 + 行尾光标 x（end_x 含连字符宽）。
    line.vis = vis;
    let end = line.end_x();
    line.xs = line.vis.iter().map(|v| v.x).chain(std::iter::once(end)).collect();
}

/// Narrowest span text flows into beside a floating object, points. Word puts an empty
/// paragraph beside a floating table in a 21.3pt gap but not in an 18.6pt one.
/// 【中文标注】文本可挤进浮动对象旁的最窄区间（pt）。实测 Word：21.3pt 的缝隙放得下一个
/// 空段落，18.6pt 则不放——故取 20pt 作经验分界，更窄的缝隙不放文字。
const MIN_SPAN: f32 = 20.0;

/// The spans of a row at `top` (about `h` tall) between `lo` and `hi` that text can use around
/// the exclusions, left to right (Square wrapping uses both sides of an object). `Err(y)`: none
/// here, try again at `y` (below what's in the way).
/// 【中文标注】求一行（row）在纵向 [top, top+h)、横向 [lo, hi) 内可用的水平区间列表
/// （绕开所有环绕对象；方形环绕 Square 时对象两侧都可放文字），从左到右返回。
/// Err(y)：此处完全放不下，调用方应把行移到 y（通常是被挡对象的底部）重试。
fn row_spans(exclusions: &[Exclusion], top: f32, h: f32, lo: f32, hi: f32) -> Result<VecDeque<(f32, f32)>, f32> {
    // 【中文标注】先筛出与本行纵向、横向都相交的环绕对象。
    let here: Vec<&Exclusion> = exclusions.iter().filter(|e| e.bottom > top && e.top < top + h && e.right > lo && e.left < hi).collect();
    if here.is_empty() {
        return Ok(VecDeque::from([(lo, hi)]));
    }
    // 【中文标注】上下型环绕（Top and bottom）：两侧不放文字，整行下移到该类对象的最低底部。
    if let Some(below) = here.iter().filter(|e| e.top_bottom).map(|e| e.bottom).reduce(f32::max) {
        return Err(below);
    }
    // 【中文标注】逐个对象切割当前区间集合：与对象重叠的区间被切成“对象左侧 + 对象右侧”两段。
    let mut spans = vec![(lo, hi)];
    for e in &here {
        spans = spans
            .into_iter()
            .flat_map(|(a, b)| if e.right <= a || e.left >= b { vec![(a, b)] } else { vec![(a, e.left.min(b)), (e.right.max(a), b)] })
            .collect();
    }
    // 【中文标注】太窄（< MIN_SPAN）的缝隙不放文字。
    spans.retain(|(a, b)| b - a >= MIN_SPAN);
    if spans.is_empty() {
        // Nothing wide enough: the line goes below the first object that ends.
        // 【中文标注】没有一段够宽：移到最先结束的那个对象的底部重试。
        return Err(here.iter().map(|e| e.bottom).reduce(f32::min).unwrap_or(top));
    }
    Ok(spans.into())
}

/// Word's default hyphenation zone (0.25"), points.
/// 【中文标注】Word 默认连字区宽（0.25 英寸 = 18pt）：若按普通断点换行会在行尾留下超过
/// 一个连字区的空白，才考虑更早的连字点断行——避免“行尾留白却加连字符”的难看排版。
const HYPHENATION_ZONE: f32 = 18.0;

/// How much of their width the spaces of a justified line may give up to fit more text (Word
/// 2013+, compatibility mode 15).
/// 【中文标注】Word 2013+（兼容模式 15）两端对齐允许的最大空格压缩比例（20%）：
/// 行放不下时压缩行内空格最多此比例，以容纳更多文本。
const MAX_SPACE_SHRINK: f32 = 0.2;

/// Take `-slack` (an overflow) out of the line's inner spaces, in proportion to their widths and
/// never more than [`MAX_SPACE_SHRINK`] of them. Trailing spaces hang and are left alone.
/// 【中文标注】空格压缩（Word 2013+ 两端对齐）：把行溢出量 -slack 按各空格宽度成比例地从
/// 行内空格里扣掉，每个空格最多压缩 MAX_SPACE_SHRINK（20%）；行尾悬挂空格（x 达到
/// content_end 者）不参与压缩。压缩后所有后续簇与行尾 x 相应左移。
fn shrink_line_spaces(pl: &ParaLayout, xs: &mut [f32], c0: usize, c1: usize, content_end: f32, slack: f32) {
    // 【中文标注】收集行内“内容区”的空格（簇下标， 宽度）；x 未达 content_end 的才算内容空格。
    let spaces: Vec<(usize, f32)> = (c0..c1)
        .filter_map(|k| {
            let c = pl.clusters.get(k).filter(|c| c.kind == ClKind::Space)?;
            (xs.get(k - c0).copied().unwrap_or(f32::MAX) < content_end - 0.01).then_some((k, c.adv))
        })
        .collect();
    let total: f32 = spaces.iter().map(|s| s.1).sum();
    // 【中文标注】没有可压缩的空格、或溢出量异常（非有限值，如病态输入产生的 NaN/inf）：放弃压缩。
    if total <= 0.0 || !slack.is_finite() {
        return;
    }
    // 【中文标注】压缩比例 = 需求收缩量 / 空格总宽，上限 MAX_SPACE_SHRINK（clamp 防御异常比例）。
    let ratio = (-slack / total).clamp(0.0, MAX_SPACE_SHRINK);
    // 【中文标注】从左到右累计收缩量 take：每个簇（及行尾 x）左移其之前空格已收缩的总量。
    let mut take = 0.0;
    let mut next = spaces.iter().peekable();
    for k in c0..c1 {
        if let Some(v) = xs.get_mut(k - c0) {
            *v -= take;
        }
        if let Some((_, adv)) = next.next_if(|s| s.0 == k) {
            take += adv * ratio;
        }
    }
    if let Some(v) = xs.last_mut() {
        *v -= take;
    }
}

/// The hyphen glyph and advance in a style (cached per paragraph).
/// 【中文标注】取某样式下连字符 “-” 的（样式， 字形 id， 步进），按样式缓存于 hcache——
/// 同一段内多次试探连字断点不必反复整形；样式不存在或整形失败时返回 gid=0、步进 0
/// （画不出来也不占宽）。
fn hyphen_glyph(pl: &ParaLayout, style: u16, cache: &mut Vec<(u16, u32, f32)>) -> (u16, u32, f32) {
    // 【中文标注】命中缓存直接返回（一行内可能对同一连字点试探多次）。
    if let Some(h) = cache.iter().find(|h| h.0 == style) {
        return *h;
    }
    // 【中文标注】用该样式的字体整形 “-”；步进按 字号/单位em 缩放，并计入字符缩放百分比。
    let h = pl
        .styles
        .get(style as usize)
        .and_then(|st| {
            let g = wordcraft_fonts::shape(&st.face, "-", &[], |c| c).into_iter().next()?;
            let k = st.size / st.face.upem.max(1.0) as f32 * st.rc.scale / 100.0;
            Some((style, g.gid, g.x_advance as f32 * k))
        })
        .unwrap_or((style, 0, 0.0));
    cache.push(h);
    h
}

/// Clusters after which a line may end with a hyphen: soft hyphens always, and dictionary or
/// pattern hyphenation points of each word when automatic hyphenation is on. Both are found in
/// `text`, the paragraph as laid out (without what the layout leaves out, [`left_out`]), and
/// `to_para` maps the end of a char there to the paragraph's text.
/// 【中文标注】求“可在其后加连字符断行”的簇下标列表（升序、去重）。两个来源：
/// ① 软连字符（U+00AD）之后；② 开启自动连字时，连字模式表给出的单词内连字点。
/// 候选字节位置先在“可见文本”里找（剔除隐藏/已删文本），再经 to_para 映射回段落坐标，
/// 最后挑出 end 恰好落在这些位置上的 Text/Marker 簇。
fn hyphenation_points(text: &str, to_para: impl Fn(usize) -> Option<usize>, pl: &ParaLayout, auto: bool) -> Vec<u32> {
    // 【中文标注】软连字符：记录其后的字节位置（即断行点所在）。
    let mut bytes: Vec<usize> = text.char_indices().filter(|(_, c)| *c == SOFT_HYPHEN).map(|(i, c)| i + c.len_utf8()).collect();
    // 【中文标注】自动连字：把可见文本切成单词（字母 + 撇号），逐词查连字模式表取连字点。
    if auto {
        let lim = wordcraft_proof::hyphen::Limits::default();
        let mut start: Option<usize> = None;
        for (i, c) in text.char_indices().chain(std::iter::once((text.len(), ' '))) {
            // 撇号（直/弯）视作词内字符：“don't”整体是一个词。
            let word_char = c.is_alphabetic() || c == '\'' || c == '\u{2019}';
            match (start, word_char) {
                (None, true) => start = Some(i),
                (Some(a), false) => {
                    // The patterns are for Latin-script languages: never hyphenate Persian/Arabic words.
                    // 【中文标注】连字模式只适用于拉丁文种：波斯/阿拉伯等 RTL 词、以及短于
                    // 最小词长（lim.min_word）的词一律不自动连字。
                    if let Some(w) = text.get(a..i)
                        && w.chars().count() >= lim.min_word
                        && !w.chars().any(wordcraft_fonts::is_rtl)
                    {
                        let offs: Vec<usize> = w.char_indices().map(|(o, _)| o).collect();
                        for pt in wordcraft_proof::hyphen::hyphen_points(w, &lim) {
                            if let Some(o) = offs.get(pt) {
                                bytes.push(a + o);
                            }
                        }
                    }
                    start = None;
                }
                _ => {}
            }
        }
    }
    // 【中文标注】字节位置映射回段落坐标并排序；再筛出 end 命中的 Text/Marker 簇下标并去重。
    let mut bytes: Vec<usize> = bytes.into_iter().filter_map(to_para).collect();
    if bytes.is_empty() {
        return Vec::new();
    }
    bytes.sort_unstable();
    let mut out: Vec<u32> = pl
        .clusters
        .iter()
        .enumerate()
        .filter(|(_, c)| matches!(c.kind, ClKind::Text | ClKind::Marker) && bytes.binary_search(&c.end).is_ok())
        .map(|(i, _)| i as u32)
        .collect();
    out.dedup();
    out
}

/// 【中文标注】判断簇是否为小数点（"." 或 ","，即 Cluster::dot 的谓词包装）：小数制表位
/// 对齐时用它找锚点。函数名与实际功能不符（疑为早期实现遗留），仅返回 c.dot。
fn pl_text_at(c: &Cluster) -> bool {
    c.dot
}

/// 【中文标注】收尾一个“惰性”的右/居中/小数制表（在行结束、遇到下一个制表符或显式换行时
/// 调用）。此时制表段文本已定型（终点 x），反向解出制表簇应有的间隙 adv：Right/Decimal 让
/// 文本尾钉在制表位，Center 让文本中点钉在制表位；间隙钳为非负（文本已越过制表位时不再
/// 左拉），差异超过 0.001 才回写并平移制表段之后的所有 x 与行尾 x。
/// 注意：Decimal 在此处按“文本尾”锚定，与 break_lines 增长阶段按“首个小数点”锚定的规则
/// 不一致（后者会被此处覆盖）——疑为缺陷或有意简化，标注存疑。
fn resolve_tab(pl: &mut ParaLayout, pending: &mut Option<(usize, TabStop, f32)>, xs: &mut [f32], c0: usize, x: f32, xo: &mut f32) {
    // Finish a centre/right/decimal tab: its text (possibly empty) ends at `x`.
    // 【中文标注】收尾居中/右/小数制表：其文本（可能为空）最终停在 x。
    let Some((tj, stop, tx)) = pending.take() else { return };
    // 【中文标注】制表段文本的净宽 = 当前笔位 x - 制表起点 - 制表簇已记录的间隙。
    let seg = x - tx - pl.clusters.get(tj).map(|c| c.adv).unwrap_or(0.0);
    // 【中文标注】期望间隙：Right/Decimal 以文本尾对齐制表位，Center 以文本中点对齐；
    // .max(0.0) 保证文本已越过制表位时不再向左拉（间隙最小为 0）。
    let want = match stop.align {
        TabAlign::Right | TabAlign::Decimal => stop.pos - tx - seg,
        TabAlign::Center => stop.pos - tx - seg / 2.0,
        _ => return,
    }
    .max(0.0);
    // 【中文标注】与增长阶段已写入的间隙比较，变化超过 0.001 才回写（防浮点抖动）；
    // delta 同时平移制表段之后所有簇的 x（xs）与调用方的行尾笔位（xo）。
    let old = pl.clusters.get(tj).map(|c| c.adv).unwrap_or(0.0);
    let delta = want - old;
    if delta.abs() > 0.001 {
        if let Some(c) = pl.clusters.get_mut(tj) {
            c.adv = want;
        }
        for v in xs.iter_mut().skip(tj + 1 - c0) {
            *v += delta;
        }
        *xo += delta;
    }
}

impl ParaLayout {
    /// x of byte offset `off` within line `li` (clamped to the line).
    /// 【中文标注】字节偏移 → 行内 x（光标定位）：双向行走 x_of_bidi；纯 LTR 行逐簇扫描，
    /// 偏移落在簇边界或簇内部时都取该簇左缘（簇是不可再分的命中单元）。
    pub fn x_of(&self, li: usize, off: usize) -> Option<f32> {
        let l = self.lines.get(li)?;
        if !l.vis.is_empty() {
            return self.x_of_bidi(l, off);
        }
        for k in l.c0..l.c1 {
            let c = self.clusters.get(k)?;
            if off <= c.start {
                return l.xs.get(k - l.c0).copied();
            }
            if off < c.end {
                return l.xs.get(k - l.c0).copied();
            }
        }
        l.xs.last().copied()
    }
    /// The line holding offset `off` (a position at a wrap point belongs to the next line).
    /// 【中文标注】字节偏移 → 行号：换行点上的偏移归下一行。
    pub fn line_of(&self, off: usize) -> usize {
        // 【中文标注】主判据：off 小于下一行的起始偏移即属本行；对以显式换行符结束的行，
        // off 落在其换行符之前（off < l.stop）仍算本行。循环走完则取最后一行兜底。
        for (i, l) in self.lines.iter().enumerate() {
            let next_start = self.lines.get(i + 1).map(|n| n.start);
            match next_start {
                Some(ns) if off < ns => return i,
                Some(_) => {}
                None => return i,
            }
            if matches!(l.end, LineEnd::LineBreak | LineEnd::PageBreak | LineEnd::ColumnBreak) && off < l.stop {
                return i;
            }
        }
        self.lines.len().saturating_sub(1)
    }
    /// Caret x on a bidirectional line: the leading edge of the cluster after `off` (the right
    /// edge of right-to-left text), except where `off` leaves an embedded run (the cluster before
    /// it has the higher level), which keeps the caret at that run's trailing edge, so each
    /// visual boundary of the common mixed cases has its own offset.
    /// 【中文标注】双向行的光标 x：一般取 off 所在（或其后）簇的“前缘”（RTL 文本即右缘）；
    /// 但当 off 恰好离开一个嵌入片段（前一簇层级更高）时，光标贴在前一片段的“后缘”上——
    /// 保证混合文本（如 LTR 段落中的希伯来词）的每个视觉边界都有唯一偏移与之对应。
    fn x_of_bidi(&self, l: &Line, off: usize) -> Option<f32> {
        // 【中文标注】找到 off 所在（或紧随其后）的簇；找不到则光标在行尾。
        let k = (l.c0..l.c1).find(|&k| self.clusters.get(k).is_some_and(|c| off < c.end || off <= c.start));
        let Some(k) = k else { return Some(l.end_x()) };
        let c = self.clusters.get(k)?;
        let here = l.vis.get(k - l.c0)?;
        // 【中文标注】off 在簇内部（或本行首簇之前）：取该簇前缘。
        if off > c.start || k == l.c0 {
            return Some(here.leading());
        }
        // 【中文标注】off 恰在簇边界：前簇层级更高则贴前簇后缘，否则取本簇前缘。
        let prev = l.vis.get(k - 1 - l.c0)?;
        Some(if prev.level > here.level { prev.trailing() } else { here.leading() })
    }

    /// Visual extents, left to right, of the text between byte offsets `from..to` on line `li`
    /// (one span unless bidirectional text splits the range).
    /// 【中文标注】字节区间 → 行内视觉矩形列表（选区高亮）：纯 LTR 行返回一个连续区间；
    /// 双向行先找覆盖该字节区间的簇范围，再经 Line::spans 得到可能多段的视觉矩形。
    pub fn x_spans(&self, li: usize, from: usize, to: usize) -> Vec<(f32, f32)> {
        let Some(l) = self.lines.get(li) else { return Vec::new() };
        if l.vis.is_empty() {
            return match (self.x_of(li, from), self.x_of(li, to)) {
                (Some(a), Some(b)) => vec![(a, b)],
                _ => Vec::new(),
            };
        }
        let ks: Vec<usize> = (l.c0..l.c1).filter(|&k| self.clusters.get(k).is_some_and(|c| c.start < to && c.end > from)).collect();
        match (ks.first(), ks.last()) {
            (Some(&a), Some(&b)) => l.spans(a, b + 1),
            _ => Vec::new(),
        }
    }

    /// Caret offsets a click or arrow key can reach on line `li`: each cluster start and the line
    /// end, except after a line-ending break or past a wrapped line's trailing space.
    /// 【中文标注】行上可达的光标偏移集合：各簇起点 + 行尾。两类位置被排除——显式换行符
    /// 之后的偏移（它同时是下一行的开头，会出现两个偏移落在同一视觉点）；换行行尾的悬挂
    /// 空格之后（光标不应停在看不见的空格里）。
    pub fn line_offsets(&self, li: usize) -> Vec<usize> {
        let Some(l) = self.lines.get(li) else { return Vec::new() };
        let mut v: Vec<usize> = (l.c0..l.c1).filter_map(|k| self.clusters.get(k).map(|c| c.start)).collect();
        let last_break =
            l.c1 > l.c0 && self.clusters.get(l.c1 - 1).is_some_and(|c| matches!(c.kind, ClKind::LineBreak | ClKind::PageBreak | ClKind::ColumnBreak));
        let wrapped = l.c1 > l.c0 && l.end == LineEnd::Wrap && self.lines.get(li + 1).is_some();
        if !last_break && !wrapped || l.c1 == l.c0 {
            v.push(l.stop);
        }
        v
    }

    /// Byte offset closest to x on line `li`.
    /// 【中文标注】x 坐标 → 字节偏移（点击命中）：取与 x 最近的合法光标位置。
    pub fn off_at_x(&self, li: usize, x: f32) -> usize {
        let Some(l) = self.lines.get(li) else { return self.text_len };
        // 【中文标注】双向行：视觉序与逻辑序不一致，不能直接扫 xs——在合法偏移集合
        // （line_offsets）里逐个算 x 距离取最近者。
        if !l.vis.is_empty() {
            let mut best = (l.start, f32::MAX);
            for off in self.line_offsets(li) {
                let d = self.x_of(li, off).map_or(f32::MAX, |cx| (cx - x).abs());
                if d < best.1 {
                    best = (off, d);
                }
            }
            return best.0;
        }
        let mut best = l.start;
        let mut bestd = f32::MAX;
        // 【中文标注】纯 LTR 行：在各簇边界 x（含行尾）里找距离最近者；行尾位置可能不可达。
        for k in l.c0..=l.c1 {
            let Some(cx) = l.xs.get(k - l.c0) else { break };
            let off = if k < l.c1 { self.clusters.get(k).map(|c| c.start).unwrap_or(l.stop) } else { l.stop };
            // Don't place the caret after a line-ending break or wrap-trailing space on a wrapped line.
            // 【中文标注】同 line_offsets 的排除规则：换行符之后、换行行尾悬挂空格之后不放光标。
            if k == l.c1 && k > l.c0 {
                let lastc = self.clusters.get(k - 1);
                if lastc.is_some_and(|c| matches!(c.kind, ClKind::LineBreak | ClKind::PageBreak | ClKind::ColumnBreak)) {
                    continue;
                }
                if l.end == LineEnd::Wrap && self.lines.get(li + 1).is_some() {
                    continue;
                }
            }
            let d = (cx - x).abs();
            if d < bestd {
                bestd = d;
                best = off;
            }
        }
        best
    }
    /// Line ascent/descent of the paragraph mark (for empty paragraphs and caret height).
    /// 【中文标注】某行基线上下的实际高度（光标高度用）；行不存在时退回 (10, 3) 兜底值。
    pub fn caret_metrics(&self, li: usize) -> (f32, f32) {
        let Some(l) = self.lines.get(li) else { return (10.0, 3.0) };
        (l.baseline - l.top, l.top + l.height - l.baseline)
    }
}

/// Spelling and grammar issues in a paragraph (skipping "do not check" and hidden runs).
/// 【中文标注】段落内的拼写/语法问题列表：(起字节, 止字节, 是否语法)。跳过标记了
/// “不检查拼写”、隐藏文字或超链接的 run；空段落与超长段落（>100KB）直接不校对。
fn proof_issues(p: &Paragraph) -> Vec<(usize, usize, bool)> {
    // 【中文标注】快速跳过：空白段落与超长段落（防校对卡顿）。
    if p.text.trim().is_empty() || p.text.len() > 100_000 {
        return Vec::new();
    }
    let text = proof_text(p);
    // 【中文标注】与问题区间重叠的 run 若声明“不检查”、隐藏或是超链接，则丢弃该问题。
    let skip = |a: usize, b: usize| {
        p.run_ranges().any(|(r, c)| r.start < b && a < r.end && (c.no_proof == Some(true) || c.hidden == Some(true) || c.link.is_some()))
    };
    let mut v: Vec<(usize, usize, bool)> =
        wordcraft_proof::check_spelling(&text).into_iter().filter(|i| !skip(i.start, i.end)).map(|i| (i.start, i.end, false)).collect();
    v.extend(wordcraft_proof::check_grammar(&text).into_iter().filter(|i| !skip(i.start, i.end)).map(|i| (i.start, i.end, true)));
    v
}

/// Text for proofing with the same byte offsets: inline objects and tracked deletions become
/// U+0001 bytes (neither words nor spaces).
/// 【中文标注】构造校对用文本：对象占位符（OBJ）与已删修订字符逐字节替换为 U+0001——
/// 它既不是单词字符也不是空白，校对器自然跳过；替换逐字节进行，偏移与原文一一对应，
/// 校对出的问题区间可直接映射回段落文本。
pub fn proof_text(p: &wordcraft_doc::Paragraph) -> String {
    let mut out = String::with_capacity(p.text.len());
    for (i, c) in p.text.char_indices() {
        let deleted = c == wordcraft_doc::para::OBJ || p.props_of_char(i).del.is_some();
        if deleted {
            for _ in 0..c.len_utf8() {
                out.push('\u{1}');
            }
        } else {
            out.push(c);
        }
    }
    out
}
