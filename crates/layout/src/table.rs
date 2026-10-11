//! Table layout: column widths, cell boxes, row heights, merged cells, borders and shading,
//! table-style conditional formatting. Rows are laid out as free-standing boxes; pagination
//! moves whole rows or splits them between lines (`split_row`).
//!
//! —— 中文标注（业务流程概述）——
//! 本文件实现表格布局，整体流程：
//!   1. 定列宽：`layout_table` 先取表格声明的网格列宽（tblGrid），按表格目标宽度（百分比/绝对值）
//!      整体缩放；非固定布局（tblLayout 非 fixed）时再用内容测量法（`measure_table_columns` 的
//!      同族逻辑 `column_content` + `autofit`）按内容调整列宽。
//!   2. 第一遍排版：逐行逐格用 `layout_box` 排单元格内容，算出各格高度与行高；`cell.span()`
//!      对应 OOXML 的 gridSpan（一格占多个网格列），vMerge 分 Restart/Continue 处理纵向合并。
//!   3. 第二遍放置：按最终行高放置底纹（Fill）、边框线（Rule）与单元格区域（Cell，仅供命中
//!      测试/样式查询），竖排文本按行高重排后经 `turn` 仿射放置。
//!   4. 跨页：分页器（lib.rs 的 place_table）把整行搬页，或在行内按行拆分（`split_row`）；
//!      “标题行跨页重复”由分页器按行属性 `row.props.header` 完成，本文件的
//!      `t.props.look.header_row` 只是表格样式的“标题行”条件格式选项，二者不是一回事。
//! 表格样式条件格式：`region_of` 算出单元格的六区位（标题行/末行/首列/末列/隔行条纹/隔列条纹），
//! 决定底纹（fill 叠加）与字符格式（`region_text` 生成 CellText）；边框按 `region_borders`
//! 叠加（ECMA-376 §17.7.6，后叠加者胜）。
//! 嵌套表格最多 8 层：测量侧 `blocks_content` 里 `depth < 8`，排版侧 lib.rs 的 `layout_box`
//! 在 `depth > 8` 时直接丢弃该表（防病态文档递归爆炸）。

use wordcraft_doc::numbering::Counters;
use wordcraft_doc::props::{Align, Border, Borders, HeightRule, Rgb, TextDirection, VAlign, VMerge};
use wordcraft_doc::styles::{TableStyleParts, TableStyleProps};
use wordcraft_doc::{Block, Blocks, Paragraph, StoryRef, Table};
use wordcraft_geom::Rect;

use crate::para::{CellText, ClKind, ParaLayout};
use crate::{BoxLayout, Ctx, Placed, layout_box};

/// 一行排版完成后的结果：行高加行内全部已放置元素。
pub struct RowLayout {
    /// 行高（磅）。被 [`split_row`] 拆分时，上半部分为切点位置，下半部分为剩余高度。
    pub height: f32,
    /// Items relative to the table's top-left (x) and the row's top (y).
    /// 行内元素（文字行/底纹/边框线/单元格区域等）：x 相对表格左上角，y 相对本行行顶。
    pub items: Vec<Placed>,
}

/// 整张表格排版完成后的结果。
pub struct TableLayout {
    /// Table x offset within the column.
    /// 表格在栏内的横向偏移：由对齐方式（居中/右对齐）、缩进与 Word 兼容模式的边框半宽修正共同决定。
    pub x: f32,
    /// 表格总宽度（各网格列宽之和，经目标宽度与可用宽度调整后的最终值）。
    pub width: f32,
    /// 逐行结果，顺序与文档中的表格行一致。
    pub rows: Vec<RowLayout>,
}

/// 默认单元格边距 [上, 左, 下, 右]（磅）：左右各 5.4pt，即 Word 内置样式的缺省值。
const DEFAULT_MARGINS: [f32; 4] = [0.0, 5.4, 0.0, 5.4];

/// The longest line turned text gets when its row height isn't exact (a Letter page's body
/// height): longer text wraps.
/// 竖排文本探针排版用的行长：行高非“精确值”时先按此长度试排（取 Letter 页正文高度），
/// 超过它的文本会换行。
const TURNED_MAX: f32 = 648.0;
/// The longest line turned text ever gets (Word's largest page, 22 inches).
/// 竖排文本一行的绝对上限（Word 最大页面尺寸，22 英寸 = 1584 磅）。
const TURNED_LIMIT: f32 = 1584.0;

/// A cell whose text is turned, between the two passes of [`layout_table`].
/// 竖排（文字方向旋转）的单元格：在 [`layout_table`] 两遍排版之间携带的中间状态。
struct Turned {
    /// 文字方向：Down 为从上往下读（行框原点在右上），Up 为从下往上读（原点在左下）。
    turn: TextDirection,
    /// Line length it was laid out with, and how far its lines reach across the cell.
    /// 第一遍采用的行长（行沿单元格高度方向伸展的长度），以及各行累计横跨单元格的宽度。
    len: f32,
    across: f32,
    /// List counters and equation count before the cell, to lay it out again the same way.
    /// 排版该格之前的列表编号计数器与公式计数快照：第二遍按同样起点重排，编号才不串号。
    snap: (Counters, u32),
    /// Its formatting region (header row, total row, first column, last column, row band, column
    /// band) and cell index in the row.
    /// 该格的样式条件格式区位（标题行/末行/首列/末列/隔行/隔列）与它在行内的单元格序号。
    region: Region,
    /// 行内格序号：第二遍按新行高重排竖排内容时，用它找回文档模型中的单元格。
    ci: usize,
    /// Top margin plus top border band.
    /// 上边距加顶边框带宽：竖排内容的纵向起点偏移。
    top: f32,
}

/// 求 `items` 中已排版文本“不换行所需的最小行长”：逐行取可见字形（无字形时取首尾坐标）
/// 的实际宽度范围，加上行缩进与行尾连字符宽度，取各行最大值；末尾再加 0.5 磅余量，
/// 保证按此长度重排同一文本时不会发生换行。
/// How long the lines laid out in `items` are without the room they didn't use: the length
/// turned text needs so that nothing wraps.
fn natural_length(items: &[Placed]) -> f32 {
    let mut len = 0.0f32;
    for it in items {
        let Placed::Lines { para, l0, l1, x, .. } = it else { continue };
        let indents = para.rp.indent_left.max(0.0) + para.rp.indent_right.max(0.0) + para.rp.indent_first.max(0.0);
        for l in para.lines.get(*l0..*l1).unwrap_or(&[]) {
            let (mut lo, mut hi) = (f32::MAX, f32::MIN);
            for v in &l.vis {
                lo = lo.min(v.x);
                hi = hi.max(v.x + v.w);
            }
            if l.vis.is_empty()
                && let (Some(a), Some(b)) = (l.xs.first(), l.xs.last())
            {
                (lo, hi) = (*a, *b);
            }
            let hy = l.hyphen.map_or(0.0, |h| h.2);
            if hi >= lo {
                len = len.max(x.max(0.0) + hi - lo + hy + indents);
            }
        }
    }
    // A little slack, so laying the text out again at this length doesn't wrap it.
    len + 0.5
}

/// The table's style with its based-on chain merged.
/// 取表格声明引用的表格样式（沿 basedOn 继承链合并后的最终属性）；表格未引用样式时为 None。
fn table_style(ctx: &Ctx, t: &Table) -> Option<TableStyleProps> {
    ctx.doc.styles.table_style(t.props.style.as_deref()?)
}

/// The table's default cell margins: its own, else its style's, else Word's.
/// 表格的默认单元格边距：表格自带 > 样式提供 > Word 缺省，三级回退。
fn default_margins(t: &Table, style: Option<&TableStyleProps>) -> [f32; 4] {
    t.props.cell_margins.or(style.and_then(|s| s.parts.cell_margins)).unwrap_or(DEFAULT_MARGINS)
}

/// The most paragraphs [`measure_table_columns`] lays out: a huge table is measured by its first
/// rows only, so AutoFit stays quick.
/// [`measure_table_columns`] 最多实际排版的段落数：超大表格只按前几行测量即可，保证 AutoFit 快速。
const MEASURE_BUDGET: usize = 4000;
/// The line length paragraphs are measured at: wide enough that nothing real wraps.
/// 测量段落时给的行宽（15840 磅）：足够宽，任何真实内容都不会被迫换行。
const MEASURE_WIDE: f32 = 15_840.0;
/// What an empty cell's text needs (about a paragraph mark), points.
/// 空单元格内容的最小宽度需求（约一个段落标记的宽度），磅。
const EMPTY_TEXT: f32 = 12.0;

/// What one paragraph needs: the longest run of text without a break opportunity (`min`) and
/// its longest line when nothing wraps (`max`), indents included.
/// 内容测量法中对单个段落的度量，返回 (最窄, 最宽)（均含段落缩进）：
/// 最窄 = 段内不含断行机会的最长文本串（即最长“单词”，窄于此必溢出）；
/// 最宽 = 完全不换行时最长一行的宽度。
/// 做法：把段落以 [`MEASURE_WIDE`] 的极宽行宽真实排版一遍，再统计簇（cluster）。
fn measure_para(p: &wordcraft_doc::Paragraph, env: &crate::para::ParaEnv) -> (f32, f32) {
    use crate::para::ClKind;
    let pl = crate::para::layout_para(p, env);
    let rp = &pl.rp;
    let indents = rp.indent_left.max(0.0) + rp.indent_right.max(0.0) + rp.indent_first.max(0.0);
    let (mut word, mut min) = (0.0f32, 0.0f32);
    // 最窄：顺序扫簇，文本/内联对象累加宽度，遇到断行机会（空格、软/硬换行等）就截断记一次峰值。
    for c in &pl.clusters {
        match c.kind {
            ClKind::Text | ClKind::Object(_) => word += c.adv.max(0.0),
            ClKind::Marker => {}
            _ => {
                min = min.max(word);
                word = 0.0;
            }
        }
        // 簇自带的后置断行机会（如连字符点、kinsoku 允许断点）同样截断。
        if c.break_after {
            min = min.max(word);
            word = 0.0;
        }
    }
    min = min.max(word);
    // 最宽：逐行取“非空白”簇的左右边界极差（行尾连字符宽度计入）；空格/换行/标记不算宽度。
    let mut max = 0.0f32;
    for l in &pl.lines {
        let (mut lo, mut hi) = (f32::MAX, f32::MIN);
        for k in l.c0..l.c1 {
            let (Some(c), Some(a), Some(b)) = (pl.clusters.get(k), l.cl_left(k), l.cl_right(k)) else { continue };
            lo = lo.min(a.min(b));
            if !matches!(c.kind, ClKind::Space | ClKind::Marker | ClKind::LineBreak | ClKind::PageBreak | ClKind::ColumnBreak) {
                hi = hi.max(a.max(b));
            }
        }
        if hi > lo {
            max = max.max(hi - lo + l.hyphen.map_or(0.0, |h| h.2));
        }
    }
    let min = if min.is_finite() { min } else { 0.0 };
    let max = if max.is_finite() { max.max(min) } else { min };
    (min + indents, max + indents)
}

/// What each grid column of `t` needs for AutoFit to Contents, as (narrowest, widest) in points
/// with cell margins included: the narrowest fits each cell's longest word (text that can't
/// wrap), the widest fits every paragraph on one line. A cell spanning several columns spreads
/// what its columns lack over them. Paragraphs are laid out with the table style's formatting;
/// after [`MEASURE_BUDGET`] paragraphs the remaining rows are skipped.
/// 内容测量法（“根据内容调整表格”/AutoFit to Contents 的列需求测量）：
/// 返回每个网格列的 (最窄, 最宽)（磅，已含单元格左右边距）。最窄 = 放下该列各格最长“单词”；
/// 最宽 = 该列各格每段单行排放。跨列单元格（gridSpan）的需求缺口事后摊给所跨各列。
/// 段落按表格样式的条件字符格式排版度量；超过 [`MEASURE_BUDGET`] 个段落后剩余行跳过。
pub fn measure_table_columns(doc: &wordcraft_doc::Document, t: &Table) -> Vec<(f32, f32)> {
    use wordcraft_doc::Block;
    let style = t.props.style.as_deref().and_then(|id| doc.styles.table_style(id));
    let margins_def = default_margins(t, style.as_ref());
    let ncols = t.cols().clamp(1, wordcraft_doc::table::MAX_COLS);
    let fields = crate::FieldCtx::default();
    let mut cols = vec![(0.0f32, 0.0f32); ncols];
    // Cells spanning several columns: (first column, span, min, max), applied after the others.
    // 跨列单元格（gridSpan）：(起始网格列, 跨数, 最窄, 最宽)，等单列格处理完再统一分摊。
    let mut spanned: Vec<(usize, usize, f32, f32)> = Vec::new();
    let mut budget = MEASURE_BUDGET;
    'rows: for (ri, row) in t.rows.iter().enumerate() {
        let mut g = 0usize;
        for cell in &row.cells {
            let span = cell.span();
            let margins = cell.props.margins.unwrap_or(margins_def);
            let side = margins[1].max(0.0) + margins[3].max(0.0);
            // 该格所在条件区位的字符格式（如标题行加粗），测量时也要按它排版才准。
            let text = style.as_ref().map(|st| region_text(st, region_of(t, style.as_ref(), ri, g, span, ncols)));
            let env = crate::para::ParaEnv {
                doc,
                width: MEASURE_WIDE,
                label: None,
                fields: &fields,
                show_hidden: false,
                hide_deleted: false,
                table: text.as_ref(),
                proofing: false,
                exclusions: &[],
                eq_number: 0,
            };
            let (mut min, mut max) = (EMPTY_TEXT, EMPTY_TEXT);
            // 只测真实承载内容的格：vMerge 续格没有内容；竖排文字沿行高方向走，不占列宽。
            if cell.props.vmerge != VMerge::Continue && !cell.props.text_direction.is_turned() {
                for b in &cell.blocks {
                    match &**b {
                        Block::Para(p) => {
                            if budget == 0 {
                                break 'rows;
                            }
                            budget -= 1;
                            let (a, b) = measure_para(p, &env);
                            min = min.max(a);
                            max = max.max(b);
                        }
                        // A nested table keeps its own column widths.
                        // 嵌套表格：按它自己声明的列宽合计计入（有限值，封顶 1584 磅）。
                        Block::Table(inner) => {
                            let w: f32 = inner.grid.iter().filter(|w| w.is_finite()).map(|w| w.clamp(0.0, 1584.0)).sum();
                            min = min.max(w);
                            max = max.max(w);
                        }
                    }
                }
            }
            // 需求加上本格左右边距，并封顶 1584 磅；单列格直接并入该列，跨列格暂存待分摊。
            let (min, max) = ((min + side).min(1584.0), (max + side).min(1584.0));
            if span == 1 {
                if let Some(c) = cols.get_mut(g) {
                    c.0 = c.0.max(min);
                    c.1 = c.1.max(max);
                }
            } else {
                spanned.push((g, span, min, max));
            }
            g += span;
        }
    }
    // 跨列格分摊：需求超出所跨各列现有总和的缺口，平均摊到每一列（最宽值不低于最窄值）。
    for (g, span, min, max) in spanned {
        let Some(cs) = cols.get_mut(g..(g + span).min(ncols)) else { continue };
        if cs.is_empty() {
            continue;
        }
        let n = cs.len() as f32;
        let (have_min, have_max) = cs.iter().fold((0.0, 0.0), |(a, b), c| (a + c.0, b + c.1));
        for c in cs.iter_mut() {
            c.0 += ((min - have_min) / n).max(0.0);
            c.1 += ((max - have_max) / n).max(0.0);
            c.1 = c.1.max(c.0);
        }
    }
    // A column no cell starts in still needs room for its margins.
    // 没有任何单元格起始的网格列也要有最小宽度：左右边距加一个空段标记的宽度。
    let floor = margins_def[1].max(0.0) + margins_def[3].max(0.0) + EMPTY_TEXT;
    cols.iter_mut().for_each(|c| {
        c.0 = c.0.max(floor);
        c.1 = c.1.max(c.0);
    });
    cols
}

/// Column widths for AutoFit to Contents, the way Word shares the room: every column gets its
/// widest content when that all fits in `avail`; otherwise each gets its narrowest plus a share
/// of the remaining room in proportion to how much wider its content would like to be. When
/// even the narrowest widths don't fit, the columns keep them (the table runs past the margin,
/// as in Word).
/// AutoFit 到内容的列宽分配算法（与 Word 的空间分配方式一致）：
///   1. 各列最宽值之和 ≤ 可用宽度 avail：每列直接取最宽值，皆大欢喜；
///   2. 否则各列先保底最窄值，剩余空间按比例系数 k = (avail−Σmin)/(Σmax−Σmin) 分配，
///      每列得 min + (max−min)·k——内容“越想宽”的列分得越多；
///   3. 连最窄值之和都放不下：各列保持最窄值，表格溢出页边距（与 Word 行为一致）。
pub fn autofit_widths(cols: &[(f32, f32)], avail: f32) -> Vec<f32> {
    // 清洗输入：非有限值按 0，所有值夹到 [0, 1584] 磅；最宽值不低于最窄值。
    let clean = |v: f32| if v.is_finite() { v.clamp(0.0, 1584.0) } else { 0.0 };
    let cols: Vec<(f32, f32)> = cols.iter().map(|&(a, b)| (clean(a), clean(b).max(clean(a)))).collect();
    let avail = clean(avail);
    // 三分支判定：全放得下→全取最宽；只放得下最窄（或退化）→全取最窄；否则按比例折中。
    let (sum_min, sum_max) = cols.iter().fold((0.0f32, 0.0f32), |(a, b), c| (a + c.0, b + c.1));
    if sum_max <= avail {
        return cols.iter().map(|c| c.1).collect();
    }
    if sum_min >= avail || sum_max <= sum_min {
        return cols.iter().map(|c| c.0).collect();
    }
    // 比例系数 k ∈ (0,1)：剩余空间占“最窄到最宽”总缺口的份额。
    let k = (avail - sum_min) / (sum_max - sum_min);
    cols.iter().map(|c| c.0 + (c.1 - c.0) * k).collect()
}

/// The first cell's left margin: how far its text sits inside the table's edge.
/// 首个单元格的左边距：表格边缘到其文字的缩进量（旧版兼容模式下表格定位要用它回退）。
pub(crate) fn first_cell_left_margin(ctx: &Ctx, t: &Table) -> f32 {
    first_cell_left_margin_in(t, default_margins(t, table_style(ctx, t).as_ref()))
}

fn first_cell_left_margin_in(t: &Table, def: [f32; 4]) -> f32 {
    // 首行首格自带的边距优先，否则用表格默认边距；取 [1] 即左边距。
    t.rows.first().and_then(|r| r.cells.first()).and_then(|c| c.props.margins).unwrap_or(def)[1]
}

/// A cell's borders from the table style's conditional regions it is in (`regions`: header row,
/// total row, first column, last column, row band, column band), later regions winning. Row
/// regions give their left/right edges at the table's sides and `inside_v` between cells; column
/// regions their top/bottom edges at the table's top and bottom and `between` between rows.
/// 六区位边框模型：按单元格所在的表格样式条件区位（`regions` 依次为标题行/末行/首列/末列/
/// 隔行条纹/隔列条纹）合成它应得的边框，后叠加者获胜。
/// 行类区位（标题/末行/隔行）：左右边在表格两侧取 left/right，格间取 inside_v；
/// 列类区位（首列/末列/隔列）：上下边在表格顶底取 top/bottom，行间取 between。
fn region_borders(p: &TableStyleParts, (ri, nrows): (usize, usize), (first_g, last_g): (bool, bool), regions: [bool; 6]) -> Borders {
    let [header, total, first_col, last_col, band, col_band] = regions;
    // 行类区位 → 每格边框：上下照搬；左右按是否位于表格边缘选择 outer 或 inside_v。
    let row = |b: Option<Borders>| {
        b.map(|b| Borders {
            top: b.top,
            bottom: b.bottom,
            left: if first_g { b.left } else { b.inside_v },
            right: if last_g { b.right } else { b.inside_v },
            between: None,
            inside_v: None,
        })
    };
    // 列类区位 → 每格边框：左右照搬；上下在表格首行取 top、末行取 bottom、中间行取 between。
    let col = |b: Option<Borders>| {
        b.map(|b| Borders {
            top: if ri == 0 { b.top } else { b.between },
            bottom: if ri + 1 >= nrows { b.bottom } else { b.between },
            left: b.left,
            right: b.right,
            between: None,
            inside_v: None,
        })
    };
    let mut out = Borders::default();
    // 按从弱到强的顺序叠加（ECMA-376 §17.7.6）：隔列 < 隔行 < 首列 < 末列 < 标题行 < 末行。
    for (on, b) in [
        (col_band, col(p.col_band_borders)),
        (band, row(p.band_borders)),
        (first_col, col(p.first_col_borders)),
        (last_col, col(p.last_col_borders)),
        (header, row(p.header_borders)),
        (total, row(p.total_borders)),
    ] {
        if on && let Some(b) = b {
            out.overlay(&b);
        }
    }
    out
}

/// 排版一张表格（两遍算法），返回 [`TableLayout`]。`path` 是表格在故事中的块路径（含此前的
/// 行/格序号前缀），`avail` 是栏可用宽度，`depth` 是表格嵌套深度（供防递归与浮动定位）。
/// 第一遍逐格排版内容并算行高；第二遍放置底纹、边框与单元格区域，并重排竖排文本。
pub fn layout_table(ctx: &mut Ctx, story: StoryRef, t: &Table, path: &[u32], avail: f32, depth: usize) -> TableLayout {
    let style = table_style(ctx, t);
    let parts = style.as_ref().map(|s| &s.parts);
    // Cell text formatting per region, built once per table.
    // 各条件区位的单元格字符格式（CellText），每张表只构建一次、按区位缓存复用。
    let mut cell_text: Vec<(Region, CellText)> = Vec::new();
    // 网格列数（至少 1）：列宽向量、colx 前缀和与越界兜底都以它为界。
    let ncols = t.cols().max(1);
    // Column widths.
    // 网格列宽：优先用表格声明的 tblGrid（非有限值按 72pt、过小按 4pt 兜底）；列数对不上时均分。
    let mut grid: Vec<f32> = if t.grid.len() == ncols {
        t.grid.iter().map(|w| if w.is_finite() { w.max(4.0) } else { 72.0 }).collect()
    } else {
        vec![avail / ncols as f32; ncols]
    };
    // 网格总宽 = 各列宽之和；后续按目标宽缩放或压回可用宽度时同步更新。
    let mut total: f32 = grid.iter().sum();
    // 目标总宽：优先百分比（占可用宽度的份额，封顶 100%），其次绝对宽度；都没有则维持网格值。
    let target = match (t.props.width_pct, t.props.width) {
        (Some(p), _) if p > 0.0 => Some(avail * (p / 100.0).min(1.0)),
        (_, Some(w)) if w > 0.0 => Some(w),
        _ => None,
    };
    // 有目标总宽：各列等比例缩放到目标值。
    if let Some(tw) = target {
        let k = tw / total.max(1.0);
        grid.iter_mut().for_each(|w| *w *= k);
        total = tw;
    }
    // 非固定布局且总宽超出可用宽度的 1.5 倍：整体压回可用宽度（允许超出到 1.5 倍是 Word 的余量）。
    if total > avail * 1.5 && !t.props.fixed {
        let k = avail / total.max(1.0);
        grid.iter_mut().for_each(|w| *w *= k);
        total = avail;
    }
    if !t.props.fixed {
        // Measuring lays paragraphs out once more; it mustn't count their equations twice.
        // AutoFit：先按内容测量列需求再调整列宽。测量会把段落再排一遍，公式计数要先存后还原，
        // 否则公式编号会被统计两次。
        let eq_count = ctx.eq_count;
        let content = column_content(ctx, t, style.as_ref(), depth);
        ctx.eq_count = eq_count;
        if let Some(content) = content {
            autofit(&mut grid, &content, t, avail);
            // Never wider than the room it had, however long an unbreakable word.
            // 无论不可断的长词多长，结果都不得宽于原有空间（网格总值或可用宽度取大者）。
            let fitted: f32 = grid.iter().sum();
            let cap = avail.max(total);
            if fitted > cap {
                grid.iter_mut().for_each(|w| *w *= cap / fitted);
            }
            total = grid.iter().sum();
        }
    }
    // 列起点前缀和：colx[g] 为第 g 列左缘，共 ncols+1 项（末项为表格右缘）。
    let mut colx = Vec::with_capacity(ncols + 1);
    let mut acc = 0.0;
    for w in &grid {
        colx.push(acc);
        acc += w;
    }
    colx.push(acc);
    // Word 2013 and later (compatibility mode 15) put the table's border at the margin (plus its
    // indent); earlier modes line the first cell's text up with it instead.
    // Word 2013+（兼容模式 15）：表格边框落在页边距（加缩进）处；更早的兼容模式则让首格
    // 文字与页边距对齐，表格边缘要向外回退一个首格左边距。
    let margins_def = default_margins(t, style.as_ref());
    let indent = t.props.indent.unwrap_or(0.0);
    // The style's borders, overlaid by the table's own side by side.
    // 边框合成第一步：样式边框打底，表格自带的 tblBorders 逐边叠加覆盖。
    let mut tborders = parts.and_then(|p| p.borders).unwrap_or_default();
    if let Some(own) = t.props.borders {
        tborders.overlay(&own);
    }
    let first_cell = t.rows.first().and_then(|r| r.cells.first());
    // 表格横向位置 x：居中/右对齐按剩余空间折算；兼容模式 15 起从边框外缘起排（边框居中于
    // 定位边，要向内挪半个线宽），旧版则从首格文字起排。
    let x = match t.props.align {
        Some(Align::Center) => (avail - total) / 2.0,
        Some(Align::Right) => avail - total,
        _ if ctx.doc.settings.compat_mode >= 15 => {
            // The border is centred on the edge, so half of it sits outside: Word moves the
            // table in by that half.
            let border = first_cell.and_then(|c| c.props.borders.and_then(|b| b.left)).or(tborders.left);
            indent + border.filter(Border::is_visible).map_or(0.0, |b| b.width.clamp(0.0, 12.0) / 2.0)
        }
        _ => indent - first_cell_left_margin_in(t, margins_def),
    };
    // 总行数：合计行（末行）判定与 region_borders 的行位判定都要用。
    let nrows = t.rows.len();
    // 表格样式“标题行”条件格式选项（tblLook 的 firstRow 标志），仅影响第一行的样式格式；
    // “标题行跨页重复”用的是行属性 row.props.header，由分页器（lib.rs）在新页顶部重复表头，
    // 与这里是两回事。
    let header_rows = t.props.look.header_row;
    // First pass: lay out every cell's content.
    // 第一遍：排版每个单元格的内容，暂存为 CellBox，行高确定后第二遍再统一放置。
    struct CellBox {
        /// 已排版的单元格内容（相对行顶的坐标）。
        items: Vec<Placed>,
        /// 内容高度（含上下边距与边框带的 insets）。
        h: f32,
        /// 单元格左缘的网格 x 坐标。
        x: f32,
        /// 单元格总宽（所跨网格列宽之和）。
        w: f32,
        /// 垂直对齐方式（顶/居中/底）。
        valign: VAlign,
        /// 底纹色（单元格自带或样式条件区位提供）。
        fill: Option<Rgb>,
        /// 合成后的四边边框。
        borders: Borders,
        /// 纵向合并状态：None / Restart（起始格）/ Continue（延续格）。
        vmerge: VMerge,
        /// 该格生效的四边边距 [上, 左, 下, 右]。
        margins: [f32; 4],
        /// 起始网格列号。
        g: usize,
        /// 跨越的网格列数（gridSpan）。
        span: usize,
        /// Turned text: `items` are still in the turned frame (placed in the second pass).
        /// 竖排文本状态：非空时 `items` 仍在竖排坐标系里，第二遍才旋转放置。
        turned: Option<Turned>,
    }
    // 第一遍的暂存容器：每行一个 CellBox 列表，以及各行行高。
    let mut rows: Vec<Vec<CellBox>> = Vec::with_capacity(nrows);
    let mut heights: Vec<f32> = Vec::with_capacity(nrows);
    for (ri, row) in t.rows.iter().enumerate() {
        let mut g = 0usize;
        let mut cells = Vec::with_capacity(row.cells.len());
        let mut rh = 0.0f32;
        // The tallest cell margins plus border bands in the row.
        // 本行各格“上下边距 + 边框带”的最大值：行高取“至少”值时 Word 要在行高外再加这么多。
        let mut row_insets = 0.0f32;
        // 三个行级条件区位标志：标题行（第一行）、末行（合计行）、本行是否落在隔行条纹上。
        let is_header = header_rows && ri == 0;
        let is_total = t.props.look.total_row && ri + 1 == nrows && nrows > 1;
        let band = banded(t, style.as_ref(), ri);
        for (ci, cell) in row.cells.iter().enumerate() {
            // gridSpan：本格占据 span 个网格列，行内网格游标 g 随之前进。
            let span = cell.span();
            // 本格横向范围：所跨各列的左缘到右缘。
            let x0 = colx.get(g).copied().unwrap_or(acc);
            let x1 = colx.get((g + span).min(ncols)).copied().unwrap_or(acc);
            let margins = cell.props.margins.unwrap_or(margins_def);
            // 内容可用宽度 = 格宽 − 左右边距，最小 4 磅兜底。
            let cw = (x1 - x0 - margins[1] - margins[3]).max(4.0);
            let mut fill = cell.props.shading;
            let region = region_of(t, style.as_ref(), ri, g, span, ncols);
            let (_, _, first_col, last_col, _, col_band) = region;
            // Conditional formatting, later regions winning (ECMA-376 §17.7.6): whole table,
            // column bands, row bands, first/last column, header/total row.
            // 底纹的条件格式叠加，后者覆盖前者（ECMA-376 §17.7.6）：整表 < 隔列条纹 < 隔行条纹
            // < 首列/末列 < 标题行/末行；单元格自带底纹最优先。
            if let Some(p) = parts {
                let row_fill = if is_header {
                    p.header_fill
                } else if is_total {
                    p.total_fill
                } else {
                    None
                };
                let col_fill = if first_col {
                    p.first_col_fill
                } else if last_col {
                    p.last_col_fill
                } else {
                    None
                };
                fill = fill
                    .or(row_fill)
                    .or(col_fill)
                    .or(if band { p.band_fill } else { None })
                    .or(if col_band { p.col_band_fill } else { None })
                    .or(p.fill);
            }
            // 该区位的字符格式只在首次遇到时构建一次，之后查缓存；排版单元格内容时传入。
            if let Some(st) = &style
                && !cell_text.iter().any(|(r, _)| *r == region)
            {
                cell_text.push((region, region_text(st, region)));
            }
            let text = cell_text.iter().find(|(r, _)| *r == region).map(|(_, c)| c);
            // 单元格内块的路径 = 表格路径 + 行号 + 格号（用于域解析、修订归属等）。
            let mut cpath = path.to_vec();
            cpath.push(ri as u32);
            cpath.push(ci as u32);
            // Effective borders: cell > table (outer vs inside).
            // 边框合成第二步：单元格自带 > 条件区位 > 表格级；表格级按位置取外边框（outer）
            // 或内部横竖线（between/inside_v）。edge 闭包封装“自有优先，否则按位置二选一”。
            let tb = tborders;
            let edge =
                |own: Option<Border>, outer: bool, outer_b: Option<Border>, inner_b: Option<Border>| own.or(if outer { outer_b } else { inner_b });
            let cb = cell.props.borders.unwrap_or_default();
            let (first_g, last_g) = (g == 0, g + span >= ncols);
            let rb = parts
                .map(|p| region_borders(p, (ri, nrows), (first_g, last_g), [is_header, is_total, first_col, last_col, band, col_band]))
                .unwrap_or_default();
            // 四边逐一定边：格自带（含区位叠加 rb）优先；顶边只在首行取表格外框 top，
            // 其余行取行间横线 between；左右边同理在首列/末列取外框，中间取竖线 inside_v。
            let mut borders = Borders {
                top: edge(cb.top.or(rb.top), ri == 0, tb.top, tb.between),
                bottom: edge(cb.bottom.or(rb.bottom), ri + 1 == nrows, tb.bottom, tb.between),
                left: edge(cb.left.or(if first_g { rb.left } else { rb.inside_v }), first_g, tb.left, tb.inside_v),
                right: edge(cb.right.or(if last_g { rb.right } else { rb.inside_v }), last_g, tb.right, tb.inside_v),
                between: None,
                inside_v: None,
            };
            // 末行特殊规则：样式未给“合计行上边框”时，退用样式的 total_border_top（Word 的
            // 兼容性回退），保证合计行上方有一条线。
            if is_total
                && parts.is_none_or(|p| p.total_borders.is_none_or(|b| b.top.is_none()))
                && let Some(b) = parts.and_then(|p| p.total_border_top)
            {
                borders.top = Some(b);
            }
            // Word keeps the cell's text clear of its top border (and the last row's of its bottom
            // border): the border's width sits on top of the cell margin.
            // Word 让文字与顶边框保持距离（末行对底边框同理）：边框带宽直接压在单元格边距之上。
            let band = |b: Option<Border>| b.filter(Border::is_visible).map_or(0.0, |b| b.width.clamp(0.0, 12.0));
            let (band_t, band_b) = (band(borders.top), if ri + 1 == nrows { band(borders.bottom) } else { 0.0 });
            // 内容纵向预留 = 上下边距 + 顶边框带 + （仅末行）底边框带。
            let insets = margins[0] + margins[2] + band_t + band_b;
            let turn = cell.props.text_direction;
            let mut turned = None;
            // 三分支：vMerge 延续格不排内容（高度由起始格跨行累计）；
            // 竖排格走专用流程；普通格直接排版。
            let (items, h) = if cell.props.vmerge == VMerge::Continue {
                (Vec::new(), 0.0)
            } else if turn.is_turned() {
                // Turned text: its lines run along the cell's height. An exact row height sets
                // their length; otherwise they get the length the text needs unwrapped and the row
                // grows to fit (the second pass lays them out again if the row ends up taller).
                // 竖排文本：行沿单元格高度方向伸展。行高为“精确值”时行长就取它；否则先探针排版
                // 求“不换行所需长度”，行高随后增高（若第二遍时行更高，还会按新行长重排一次）。
                // 探针排版前快照计数器，失败路径上要还原。
                let snap = (ctx.counters.clone(), ctx.eq_count);
                let len = match (row.props.height, row.props.height_rule) {
                    (Some(h), HeightRule::Exact) if h > 0.0 => (h - insets).clamp(4.0, TURNED_LIMIT),
                    _ => {
                        let probe = layout_box(ctx, story, &cell.blocks, &cpath, TURNED_MAX, text, depth, None).items;
                        (ctx.counters, ctx.eq_count) = (snap.0.clone(), snap.1);
                        natural_length(&probe).clamp(4.0, TURNED_MAX)
                    }
                };
                let BoxLayout { items, height, floats_bottom } = layout_box(ctx, story, &cell.blocks, &cpath, len, text, depth, None);
                // across = 竖排各行横跨的总宽度（即内容“高度”在旋转后的体现）。
                let across = height.max(floats_bottom);
                turned = Some(Turned { turn, len, across, snap, region, ci, top: margins[0] + band_t });
                (items, len)
            } else {
                // 普通格：按内容宽排版，再平移到 (格左缘 + 左边距, 上边距 + 顶边框带)。
                let BoxLayout { mut items, height, floats_bottom } = layout_box(ctx, story, &cell.blocks, &cpath, cw, text, depth, None);
                for it in &mut items {
                    it.translate(x0 + margins[1], margins[0] + band_t);
                }
                // The cell grows to hold its floating tables.
                // 有浮动表格时单元格要长高把它们装下。
                (items, height.max(floats_bottom))
            };
            // 内容高加上 insets 才是单元格高；合并起始格（Restart）暂不计入行高（后面按跨行总和处理），
            // 其余格与行内 insets 一起撑高本行。
            let h = h + insets;
            if cell.props.vmerge != VMerge::Restart {
                rh = rh.max(h);
            }
            row_insets = row_insets.max(insets);
            cells.push(CellBox {
                items,
                h,
                x: x0,
                w: x1 - x0,
                valign: cell.props.valign,
                fill,
                borders,
                vmerge: cell.props.vmerge,
                margins,
                g,
                span,
                turned,
            });
            g += span;
        }
        // Word adds the cells' top and bottom margins and border bands to an at-least row height:
        // a 30pt row with 5pt margins and 0.5pt borders is 40.5pt tall even when its text needs less.
        // 行高的三种规则：Exact 精确值（行高恒为 h）；atLeast/atMost 等“至少”值 = max(内容高,
        // h + 行内 insets)——边距和边框带要额外加进去；未声明行高时用内容自然高度。
        let rh = match (row.props.height, row.props.height_rule) {
            (Some(h), HeightRule::Exact) if h > 0.0 => h,
            (Some(h), _) if h > 0.0 => rh.max(h + row_insets),
            _ => rh,
        };
        // 行高最低 4 磅兜底；本行各格入册。
        heights.push(rh.max(4.0));
        rows.push(cells);
    }
    // Vertically merged cells: grow the last row of the merge if the content is taller.
    // 纵向合并（vMerge）：起始格内容比合并区现有总高还高时，把缺口加到合并区最后一行，
    // 保证起始格内容放得下。
    for ri in 0..rows.len() {
        let restarts: Vec<(usize, f32, usize)> =
            rows.get(ri).map(|r| r.iter().filter(|c| c.vmerge == VMerge::Restart).map(|c| (c.g, c.h, c.span)).collect()).unwrap_or_default();
        for (g, h, _) in restarts {
            let mut end = ri;
            while rows.get(end + 1).is_some_and(|r| r.iter().any(|c| c.g == g && c.vmerge == VMerge::Continue)) {
                end += 1;
            }
            let have: f32 = heights.get(ri..=end).map(|s| s.iter().sum()).unwrap_or(0.0);
            if h > have
                && let Some(last) = heights.get_mut(end)
            {
                *last += h - have;
            }
        }
    }
    // Grid columns holding a vertical-merge continuation, per row.
    // 每行中处于 vMerge 延续状态的网格列号表：判断某格的合并区是否延续到下一行时使用。
    let cont: Vec<Vec<usize>> = t
        .rows
        .iter()
        .map(|r| {
            let mut gx = 0;
            r.cells
                .iter()
                .filter_map(|x| {
                    let g = gx;
                    gx += x.span();
                    (x.props.vmerge == VMerge::Continue).then_some(g)
                })
                .collect()
        })
        .collect();
    // Second pass: place cells with their row height, fills and borders.
    // 第二遍：行高已定，逐行放置底纹、内容、边框线与单元格区域。
    let mut out = Vec::with_capacity(rows.len());
    for (ri, cells) in rows.into_iter().enumerate() {
        let rh = heights.get(ri).copied().unwrap_or(0.0);
        let mut items = Vec::new();
        for (ci, c) in cells.into_iter().enumerate() {
            // A restart cell spans several rows: its height is their sum.
            // vMerge 起始格的可用高度 = 合并区各行行高之和（沿 cont 表向下累计延续行）。
            let mut span_h = rh;
            if c.vmerge == VMerge::Restart {
                let mut k = ri + 1;
                while cont.get(k).is_some_and(|v| v.contains(&c.g)) {
                    span_h += heights.get(k).copied().unwrap_or(0.0);
                    k += 1;
                }
            }
            let rect = Rect::new(c.x, 0.0, c.w, span_h);
            // 底纹：只有合并起始格（及普通格）画，延续格不重复画。
            if c.vmerge != VMerge::Continue
                && let Some(f) = c.fill
            {
                items.push(Placed::Fill { rect, color: f });
            }
            // 垂直对齐：内容在本格可用高度内的纵向偏移 dy。
            let mut dy = match c.valign {
                VAlign::Top => 0.0,
                VAlign::Center => ((span_h - c.h) / 2.0).max(0.0),
                VAlign::Bottom => (span_h - c.h).max(0.0),
            };
            let mut cell_items = c.items;
            if let Some(tn) = c.turned {
                // 竖排格：内容随行高整体处理，不用 dy 垂直对齐（对齐改在横向做）。
                dy = 0.0;
                let m = c.margins;
                // The lines run the cell's whole height (laid out again if the row grew).
                // 新行长 = 合并区总高扣掉非内容部分（原 insets）：行长变了就按新行长重排一次。
                let len = (span_h - (c.h - tn.len)).clamp(4.0, TURNED_LIMIT);
                let mut across = tn.across;
                if (len - tn.len).abs() > 0.01
                    && let Some(cell) = t.rows.get(ri).and_then(|r| r.cells.get(tn.ci))
                {
                    let mut cpath = path.to_vec();
                    cpath.extend([ri as u32, tn.ci as u32]);
                    let text = cell_text.iter().find(|(r, _)| *r == tn.region).map(|(_, c)| c);
                    // Same list numbers as the first layout: lay it out from the same counters.
                    // 换回第一遍排版前的计数器快照再重排，列表编号与公式编号保持一致，结束后还原。
                    let now = (std::mem::replace(&mut ctx.counters, tn.snap.0), std::mem::replace(&mut ctx.eq_count, tn.snap.1));
                    let b = layout_box(ctx, story, &cell.blocks, &cpath, len, text, depth, None);
                    (cell_items, across) = (b.items, b.height.max(b.floats_bottom));
                    (ctx.counters, ctx.eq_count) = now;
                }
                // Lines stack across the cell from its start edge (right for top-to-bottom text,
                // left for bottom-to-top); the vertical alignment moves them across it.
                // 竖排行从起始边起沿格宽方向堆叠（从上往下读的文本起始边在右、从下往上在左）；
                // 原本的“垂直对齐”在这里变成横向偏移 off。
                let cw = (c.w - m[1] - m[3]).max(0.0);
                let off = match c.valign {
                    VAlign::Top => 0.0,
                    VAlign::Center => ((cw - across) / 2.0).max(0.0),
                    VAlign::Bottom => (cw - across).max(0.0),
                };
                // 竖排仿射原点：Up（从下往上读）在左下角，其余（Down 等）在右上角。
                let (ox, oy) = match tn.turn {
                    TextDirection::Up => (c.x + m[1] + off, tn.top + len),
                    _ => (c.x + m[1] + cw - off, tn.top),
                };
                // 把竖排坐标系里的元素旋到页面坐标。
                for it in &mut cell_items {
                    it.turn(tn.turn, ox, oy);
                }
            }
            // 普通格：内容再按垂直对齐下移 dy 后放入行。
            for mut it in cell_items {
                it.translate(0.0, dy);
                items.push(it);
            }
            // 画单元格四边边框线（Placed::Rule）：合并区的中间横线不画（由起始格的底边或
            // 延续结束处的边线代替）。
            let b = c.borders;
            let (x0, x1, y0, y1) = (c.x, c.x + c.w, 0.0, rh);
            // 顶边只在合并起始格（或普通格）画，延续格顶部是合并区内部。
            let top_vis = c.vmerge != VMerge::Continue;
            if top_vis && let Some(e) = b.top.filter(Border::is_visible) {
                items.push(Placed::Rule { x0, y0, x1, y1: y0, border: e });
            }
            // 底边只在“合并区最后一格”画：普通格必是，延续中的格不是。
            let last_of_merge = c.vmerge == VMerge::None || !cont.get(ri + 1).is_some_and(|v| v.contains(&c.g));
            if last_of_merge && let Some(e) = b.bottom.filter(Border::is_visible) {
                items.push(Placed::Rule { x0, y0: y1, x1, y1, border: e });
            }
            if let Some(e) = b.left.filter(Border::is_visible) {
                items.push(Placed::Rule { x0, y0, x1: x0, y1, border: e });
            }
            if let Some(e) = b.right.filter(Border::is_visible) {
                items.push(Placed::Rule { x0: x1, y0, x1, y1, border: e });
            }
            // 单元格区域（Cell）：本行行高为准的矩形，携带表格路径与行列号，供命中测试与
            // 样式查询（不在 display.rs 绘制任何可见内容）。
            items.push(Placed::Cell { rect: Rect::new(c.x, 0.0, c.w, rh), table: wordcraft_doc::Path(path.to_vec()), row: ri, cell: ci, story });
        }
        out.push(RowLayout { height: rh, items });
    }
    // 组装整表结果：逐行布局 + 表格横向位置与总宽。
    TableLayout { x, width: total, rows: out }
}

/// Split a row at `cut` (points below the row's top) so the part above fits on this page.
/// Paragraph lines break between lines; fills, vertical rules and cell areas are cut; the
/// rest moves up to the top of the continuation. `None` when no line fits above the cut.
/// 行跨页拆分：在距行顶 `cut` 磅处把一行拆成上下两半，上半留在本页。段落按行界拆；
/// 底纹、竖直边框线与单元格区域按矩形切割；其余元素整体上移到续页部分顶部。
/// 切点之上放不进任何一行时返回 None（整行搬到下一页），由分页器（lib.rs）调用。
pub fn split_row(row: &RowLayout, cut: f32) -> Option<(RowLayout, RowLayout)> {
    // 切点必须在行内（既不在行顶之前，也不在行底之后）。
    if cut <= 0.0 || cut >= row.height {
        return None;
    }
    // Where each paragraph splits, and how far up the continuation moves.
    // 第一趟只探测不修改：找出每个段落拆在第几行，以及续页部分需要上移的距离
    // （first_moved = 留给下半部分的首个元素顶端）。
    let mut first_moved = f32::MAX;
    let mut kept_any = false;
    for it in &row.items {
        // Turned text moves as a whole, like a picture.
        // 竖排文本像图片一样整体搬，不逐行拆：整体在上半页则记 kept_any，否则记其顶端。
        if let Some(r) = it.turned_bounds() {
            if r.bottom() <= cut + 0.01 {
                kept_any = true;
            } else {
                first_moved = first_moved.min(r.y);
            }
            continue;
        }
        match it {
            // 文字行：逐行比较行底与切点，底部在切点之上的行留在上半页。
            Placed::Lines { para, l0, l1, y, .. } => {
                let base = para.lines.get(*l0).map(|l| l.top).unwrap_or(0.0);
                for k in *l0..*l1 {
                    let Some(l) = para.lines.get(k) else { continue };
                    let (top, bottom) = (y + l.top - base, y + l.top - base + l.height);
                    if bottom <= cut + 0.01 {
                        kept_any = true;
                    } else {
                        first_moved = first_moved.min(top);
                        break;
                    }
                }
            }
            // 整体元素（图片/形状/图表）：底边在切点之上留下，否则整体算入下半页。
            Placed::Image { rect, .. } | Placed::Shape { rect, .. } | Placed::Graphic { rect, .. } => {
                if rect.bottom() <= cut + 0.01 {
                    kept_any = true;
                } else {
                    first_moved = first_moved.min(rect.y);
                }
            }
            _ => {}
        }
    }
    // 上半页连一行都放不下：返回 None，让分页器把整行搬到下一页。
    if !kept_any {
        return None;
    }
    // Every line fits and only a nested table's cell runs past the cut (a floating table's
    // fixed-height row, say): split there rather than move the whole row on. A row that is just
    // taller than its text moves on whole, as in Word.
    // 所有文字行都放得下、只有嵌套表格的单元格区域越过了切点（如浮动表的固定高行）：
    // 就在切点处拆，而不是整行搬走。行只比内容略高一点时整行搬走（与 Word 一致）。
    // own_depth = 本行最浅的单元格区域深度，比它更深的 Cell 属于嵌套表格。
    let own_depth = row.items.iter().filter_map(|it| if let Placed::Cell { table, .. } = it { Some(table.0.len()) } else { None }).min();
    let nested_past_cut =
        row.items.iter().any(|it| matches!(it, Placed::Cell { rect, table, .. } if Some(table.0.len()) > own_depth && rect.bottom() > cut + 0.01));
    if first_moved == f32::MAX && nested_past_cut {
        first_moved = cut;
    }
    if first_moved == f32::MAX {
        return None;
    }
    // Keep a little of the cell's top margin on the continuation.
    // 续页部分整体上移 shift：比 first_moved 少留 2 磅，给下半页保留一点单元格上边距。
    let shift = (first_moved - 2.0).max(0.0);
    let (mut a, mut b) = (Vec::new(), Vec::new());
    // 第二趟真正拆分：每个元素按类型分给上半（a）或下半（b，坐标上移 shift）。
    for it in &row.items {
        // 竖排文本：整体判归属，去下半页时上移 shift。
        if let Some(r) = it.turned_bounds() {
            let mut it = it.clone();
            if r.bottom() <= cut + 0.01 {
                a.push(it);
            } else {
                it.translate(0.0, -shift);
                b.push(it);
            }
            continue;
        }
        match it {
            // 文字行：找到拆分行号 split，[l0, split) 留上半，[split, l1) 去下半（y 重置到拆分行顶）。
            Placed::Lines { story, path, para, l0, l1, x, y, turn } => {
                let base = para.lines.get(*l0).map(|l| l.top).unwrap_or(0.0);
                let mut split = *l1;
                for k in *l0..*l1 {
                    let Some(l) = para.lines.get(k) else { continue };
                    if y + l.top - base + l.height > cut + 0.01 {
                        split = k;
                        break;
                    }
                }
                if split > *l0 {
                    a.push(Placed::Lines { story: *story, path: path.clone(), para: para.clone(), l0: *l0, l1: split, x: *x, y: *y, turn: *turn });
                }
                if split < *l1 {
                    let top = para.lines.get(split).map(|l| y + l.top - base).unwrap_or(*y);
                    b.push(Placed::Lines {
                        story: *story,
                        path: path.clone(),
                        para: para.clone(),
                        l0: split,
                        l1: *l1,
                        x: *x,
                        y: top - shift,
                        turn: *turn,
                    });
                }
            }
            // 整体元素（含行内对象占位）：底边在切点之上留下，否则整体去下半页。
            Placed::Image { rect, .. } | Placed::Shape { rect, .. } | Placed::Graphic { rect, .. } | Placed::Object { rect, .. } => {
                if rect.bottom() <= cut + 0.01 {
                    a.push(it.clone());
                } else {
                    let mut it = it.clone();
                    it.translate(0.0, -shift);
                    b.push(it);
                }
            }
            // Shading and cell areas: the part above the cut stays, the rest moves up with the
            // continuation (a nested table's cell below the cut moves whole).
            // 底纹与单元格区域按矩形切割：切点以上归上半页，以下部分上移后归下半页
            // （split_rect 内部处理上移量 shift）。
            Placed::Fill { rect, color } => {
                let (above, below) = split_rect(*rect, cut, shift);
                a.extend(above.map(|rect| Placed::Fill { rect, color: *color }));
                b.extend(below.map(|rect| Placed::Fill { rect, color: *color }));
            }
            // 单元格区域：同样按矩形切割为上下两块（下半块上移），文字行已单独拆好。
            Placed::Cell { rect, table, row: r, cell, story } => {
                let (above, below) = split_rect(*rect, cut, shift);
                let cell_at = |rect| Placed::Cell { rect, table: table.clone(), row: *r, cell: *cell, story: *story };
                a.extend(above.map(cell_at));
                b.extend(below.map(cell_at));
            }
            // 边框线：分水平线与竖直线两类处理。
            Placed::Rule { x0, y0, x1, y1, border } => {
                if (y0 - y1).abs() < 0.01 {
                    // Horizontal: the top edge stays, the bottom edge moves; both fragments close.
                    // 水平线（单元格顶/底边框）要“封口”：表格顶边框留在上半页后，上半页在切点处
                    // 补一条闭合线；底边框去下半页后，上半页在切点处补一条。中间的照常分配。
                    if *y0 <= 0.01 {
                        a.push(it.clone());
                        b.push(Placed::Rule { x0: *x0, y0: 0.0, x1: *x1, y1: 0.0, border: *border });
                    } else if *y0 >= row.height - 0.01 {
                        a.push(Placed::Rule { x0: *x0, y0: cut, x1: *x1, y1: cut, border: *border });
                        b.push(Placed::Rule { x0: *x0, y0: y0 - shift, x1: *x1, y1: y1 - shift, border: *border });
                    } else if *y0 <= cut {
                        a.push(it.clone());
                    } else {
                        b.push(Placed::Rule { x0: *x0, y0: y0 - shift, x1: *x1, y1: y1 - shift, border: *border });
                    }
                } else {
                    // 竖直线：在切点处切成两段，上段归上半页、下段上移后归下半页。
                    let (t, bt) = (y0.min(*y1), y0.max(*y1));
                    if t < cut {
                        a.push(Placed::Rule { x0: *x0, y0: t, x1: *x1, y1: bt.min(cut), border: *border });
                    }
                    if bt > cut {
                        b.push(Placed::Rule { x0: *x0, y0: (t - shift).max(0.0), x1: *x1, y1: bt - shift, border: *border });
                    }
                }
            }
        }
    }
    // 上半页 = 高度为切点 cut 的行；下半页 = 剩余内容整体上移 shift 后的行（最低 4 磅兜底）。
    Some((RowLayout { height: cut, items: a }, RowLayout { height: (row.height - shift).max(4.0), items: b }))
}

/// The parts of `r` above `cut` and below it, the latter moved up by `shift` (onto the
/// continuation of a row split at `cut`).
/// 矩形切割：返回 (切点以上的部分, 切点以下的部分)，后者整体上移 shift（搬到续页行顶部）。
/// 某一侧为空时返回 None。
fn split_rect(r: Rect, cut: f32, shift: f32) -> (Option<Rect>, Option<Rect>) {
    let above = (r.y < cut).then(|| Rect::new(r.x, r.y, r.w, (r.bottom().min(cut) - r.y).max(0.0)));
    let top = (r.y - shift).max(0.0);
    let below = (r.bottom() > cut).then(|| Rect::new(r.x, top, r.w, (r.bottom() - shift - top).max(0.0)));
    (above, below)
}

/// A cell's region for table-style conditional formatting: (header row, total row, first column,
/// last column, banded row, banded column).
/// 单元格的“条件区位”六元组（表格样式条件格式用）：(标题行, 末行, 首列, 末列, 隔行条纹, 隔列条纹)。
type Region = (bool, bool, bool, bool, bool, bool);

/// The region of the cell at grid column `g` spanning `span` of `ncols` columns in row `ri`.
/// 求第 ri 行、起始网格列 g、跨 span 列的单元格的六区位：
/// 首列 = 开了首列选项且 g==0；末列 = 开了末列选项且格右缘达最右（单列表除外，避免首末列重合）；
/// 隔列条纹 = 开了隔列选项、非首列，且相对首列偏移为偶数；行类区位由 banded 与行号判定。
fn region_of(t: &Table, style: Option<&TableStyleProps>, ri: usize, g: usize, span: usize, ncols: usize) -> Region {
    let (look, nrows) = (&t.props.look, t.rows.len());
    let first_col = look.first_column && g == 0;
    let last_col = look.last_column && g.saturating_add(span) >= ncols && ncols > 1;
    let col_band = look.banded_columns && !first_col && g.saturating_sub(usize::from(look.first_column)).is_multiple_of(2);
    (look.header_row && ri == 0, look.total_row && ri + 1 == nrows && nrows > 1, first_col, last_col, banded(t, style, ri), col_band)
}

/// Whether row `ri` is in an odd band of the table style's banded rows (bands of the style's
/// band size, counted below the header row).
/// 隔行条纹判定：行 ri 是否落在表格样式“隔行条纹”的条纹组上。
/// 条纹组大小取样式的 band_size（默认 1，夹到 [1,1000]）；标题行（若启用）不计入条纹，
/// 其余行按“扣除标题行后的行号 / band_size”分组，组序号（0 起）为偶数的组套用条纹。
fn banded(t: &Table, style: Option<&TableStyleProps>, ri: usize) -> bool {
    let look = &t.props.look;
    let size = style.and_then(|s| s.parts.band_size).unwrap_or(1).clamp(1, 1000) as usize;
    look.banded_rows && !(look.header_row && ri == 0) && (ri.saturating_sub(usize::from(look.header_row)) / size).is_multiple_of(2)
}

/// The table style's text formatting in `region`: the whole table's, then the column band's, the
/// row band's, the first/last column's and the header/total row's (later regions win,
/// ECMA-376 §17.7.6).
/// 表格样式在某区位的字符格式（CellText）：整表格式打底，再依次叠加隔列条纹、隔行条纹、
/// 首列、末列、标题行/末行（后者覆盖前者，ECMA-376 §17.7.6）；段落级格式直接取样式值。
fn region_text(st: &TableStyleProps, (header, total, first_col, last_col, band, col_band): Region) -> CellText {
    // 整表字符格式打底，再按从弱到强叠加所中区位的字符格式（顺序与函数文档一致）。
    let mut chr = st.chr.clone();
    // 隔列条纹。
    if col_band {
        chr.overlay(&st.parts.col_band_chr);
    }
    // 隔行条纹。
    if band {
        chr.overlay(&st.parts.band_chr);
    }
    // 首列。
    if first_col {
        chr.overlay(&st.parts.first_col_chr);
    }
    // 末列。
    if last_col {
        chr.overlay(&st.parts.last_col_chr);
    }
    // 标题行与末行互斥（nrows>1 时首行不可能同时是末行），elif 二选一。
    if header {
        chr.overlay(&st.parts.header_chr);
    } else if total {
        chr.overlay(&st.parts.total_chr);
    }
    // 段落级格式不随区位变化，直接取样式整表值。
    CellText { para: st.para.clone(), chr }
}

/// Each column's narrowest and widest content, cell margins included: its longest word and its
/// longest line. A cell spanning columns shares what they lack between them; nested tables count
/// with their own. `None` when every cell is empty.
/// layout_table 专用的内容测量（与 [`measure_table_columns`] 同思路，但用真实排版环境）：
/// 求每列 (最窄, 最宽)（含边距）。跨列格的缺口摊给所跨各列；嵌套表格按其自身宽度计入；
/// 全表内容皆空时返回 None（维持网格原宽）。
fn column_content(ctx: &mut Ctx, t: &Table, style: Option<&TableStyleProps>, depth: usize) -> Option<Vec<(f32, f32)>> {
    let margins_def = default_margins(t, style);
    let mut cols = vec![(0.0f32, 0.0f32); t.cols().max(1)];
    // 跨列格暂存：(起始列, 跨数, (最窄, 最宽))，主循环结束后统一分摊。
    let mut spanning = Vec::new();
    // 是否出现过任何非空内容：全表皆空时最终返回 None（调用方维持网格原宽）。
    let mut any = false;
    for (ri, row) in t.rows.iter().enumerate() {
        let mut g = 0usize;
        for cell in &row.cells {
            let span = cell.span();
            // 延续格无内容不测；测完把需求（加左右边距）归入起始列，跨列格暂存。
            if cell.props.vmerge != VMerge::Continue {
                let text = style.map(|st| region_text(st, region_of(t, style, ri, g, cell.span(), cols.len())));
                let (lo, hi) = blocks_content(ctx, &cell.blocks, text.as_ref(), depth);
                any |= hi > 0.0;
                let m = cell.props.margins.unwrap_or(margins_def);
                let need = (lo + m[1] + m[3], hi + m[1] + m[3]);
                match cols.get_mut(g) {
                    Some(col) if span == 1 => *col = (col.0.max(need.0), col.1.max(need.1)),
                    _ => spanning.push((g, span, need)),
                }
            }
            g += span;
        }
    }
    // 跨列格分摊：缺口（需求减所跨列现有总和）均摊到每列，只加不减。
    for (g, span, need) in spanning {
        let Some(cs) = cols.get_mut(g..(g + span).min(t.cols())) else { continue };
        let n = cs.len().max(1) as f32;
        let (have_lo, have_hi) = cs.iter().fold((0.0, 0.0), |(l, h), c| (l + c.0, h + c.1));
        for c in cs {
            c.0 += (need.0 - have_lo).max(0.0) / n;
            c.1 += (need.1 - have_hi).max(0.0) / n;
        }
    }
    any.then_some(cols)
}

/// The narrowest and widest a block list can be laid out.
/// 一组块（段落/嵌套表格）的 (最窄, 最宽) 可排版宽度：逐块取两者最大值。
fn blocks_content(ctx: &mut Ctx, blocks: &Blocks, text: Option<&CellText>, depth: usize) -> (f32, f32) {
    let (mut lo, mut hi) = (0.0f32, 0.0f32);
    for b in blocks.iter() {
        let (l, h) = match &**b {
            // 段落：带列表缩进环境（label 只取层级、文字留空——测量不算编号文本宽度）。
            Block::Para(p) => {
                let label = list_level(ctx, p).map(|l| (String::new(), l));
                para_content(&ctx.para_labelled(p, UNBOUNDED, text, &[], label))
            }
            // A nested table: its fixed width, else its columns' but at least its preferred width
            // (its own, else its grid's when it has preferred widths at all).
            // 嵌套表格：固定布局取声明宽度；否则取各列内容需求（下限为声明宽度）。
            // 深度 ≥ 8 时不再递归测量（防病态嵌套，见文件头说明）。
            Block::Table(nt) if depth < 8 => {
                let grid = if is_automatic(nt) { 0.0 } else { nt.grid.iter().filter(|w| w.is_finite()).map(|w| w.max(0.0)).sum() };
                let width = nt.props.width.filter(|w| w.is_finite()).unwrap_or(grid).clamp(0.0, UNBOUNDED);
                if nt.props.fixed && width > 0.0 {
                    (width, width)
                } else {
                    let style = table_style(ctx, nt);
                    let cols = column_content(ctx, nt, style.as_ref(), depth + 1).unwrap_or_default();
                    let (l, h) = cols.iter().fold((0.0, 0.0), |(l, h), c| (l + c.0, h + c.1));
                    (width.max(l), width.max(h))
                }
            }
            Block::Table(_) => (0.0, 0.0),
        };
        lo = lo.max(l);
        hi = hi.max(h);
    }
    (lo, hi)
}

/// The list level `p` is in, for its indents (without counting it: measuring isn't laying out).
/// 取段落所属的编号定义层级（供测量时取其缩进）；只取定义不推进计数——测量不是真实排版。
fn list_level(ctx: &Ctx, p: &Paragraph) -> Option<wordcraft_doc::numbering::Level> {
    // 段落自带编号优先，否则用段落样式解析出的；num==0 表示“无编号”。
    let n = p.props.numbering.or_else(|| ctx.doc.styles.resolve_para(&p.props).numbering).filter(|n| n.num != 0)?;
    ctx.doc.numbering.level(n.num, n.level).cloned()
}

/// A width to lay a paragraph out in so that only its own line breaks end lines.
/// 测量段落时给的“无限宽”行宽：宽到只有文本自身的换行符能结束一行。
const UNBOUNDED: f32 = 100_000.0;

/// A paragraph's longest word and longest line (between line breaks), indents included.
/// 段落内容的 (最长单词, 最长行)（行界为软/硬换行），均含缩进。与 [`measure_para`] 相比
/// 不实际排版，直接按簇几何统计：Space 截断“单词”累计；换行类簇同时结束当前行。
fn para_content(pl: &ParaLayout) -> (f32, f32) {
    let indents = pl.rp.indent_left.max(0.0) + pl.rp.indent_right.max(0.0) + pl.rp.indent_first.max(0.0);
    let (mut word, mut line, mut spaces) = (0.0f32, 0.0f32, 0.0f32);
    let (mut lo, mut hi) = (0.0f32, 0.0f32);
    // 逐簇统计：word 记当前“单词”累计宽，line 记当前行宽，spaces 记待并入行宽的空白。
    for c in &pl.clusters {
        // 步进宽清洗：非有限值按 0，其余夹到 [0, UNBOUNDED]。
        let adv = if c.adv.is_finite() { c.adv.clamp(0.0, UNBOUNDED) } else { 0.0 };
        match c.kind {
            // 空格：截断当前“单词”（宽度并入 lo），其自身宽度暂记，随后并入行宽。
            ClKind::Space => {
                lo = lo.max(word);
                word = 0.0;
                spaces += adv;
            }
            // 硬换行（换行/分页/分栏）：同时截断当前“单词”并结算当前行。
            ClKind::LineBreak | ClKind::PageBreak | ClKind::ColumnBreak => {
                lo = lo.max(word);
                hi = hi.max(line);
                (word, line, spaces) = (0.0, 0.0, 0.0);
            }
            // 普通文字/对象：累计“单词”与行宽（暂记空白并入行宽）；簇自带断行机会再截词。
            _ => {
                word += adv;
                line += spaces + adv;
                spaces = 0.0;
                if c.break_after {
                    lo = lo.max(word);
                    word = 0.0;
                }
            }
        }
    }
    (lo.max(word) + indents, hi.max(line) + indents)
}

/// Whether nothing in `t` asks for a width (its own or any cell's): its columns then follow
/// their content alone.
/// 判断表格是否“完全自动宽”：表格自身与每个单元格都没声明宽度（含百分比）。
/// 此时列宽完全由内容决定（Word 的 auto 行为）。
fn is_automatic(t: &Table) -> bool {
    t.props.width.is_none()
        && t.props.width_pct.is_none()
        && t.rows.iter().all(|r| r.cells.iter().all(|c| c.props.width.is_none() && c.props.width_pct.is_none()))
}

/// Word's autofit for a table whose widths may follow its content (`tblLayout` not fixed).
/// - With no preferred width anywhere (the table's and every cell's automatic) the columns come
///   from their content alone: each its widest line if they all fit in `avail`, else shared out
///   between narrowest and widest in proportion to how much each can give.
/// - Otherwise the grid stands, but no column is narrower than its longest word: the columns with
///   room to spare give up the difference.
/// Word 式 autofit（tblLayout 非 fixed 时按内容调整列宽），就地修改 grid：
///   1) 完全自动宽（表格与所有单元格都未声明宽度）：与 [`autofit_widths`] 同算法——
///      放得下全取最宽；否则按 (avail−Σmin)/(Σmax−Σmin) 比例在最窄与最宽间折中；
///   2) 有声明宽度：网格列宽保持不变，但任何列不得窄于其最长单词——缺口由有余量的列按比例让出。
fn autofit(grid: &mut [f32], content: &[(f32, f32)], t: &Table, avail: f32) {
    // 列数与测量结果对不上（或宽度非法）时放弃调整。
    if grid.len() != content.len() || !avail.is_finite() {
        return;
    }
    if is_automatic(t) {
        // 完全自动：k=1 全取最宽；放不下且最窄也超支（或退化）时 k=0 全取最窄；否则折中。
        let (lo, hi) = content.iter().fold((0.0f32, 0.0f32), |(l, h), c| (l + c.0, h + c.1.max(c.0)));
        let k = if hi <= avail {
            1.0
        } else if lo >= avail || hi - lo < 0.01 {
            0.0
        } else {
            (avail - lo) / (hi - lo)
        };
        // 每列 = 最窄 + 缺口 × k，最低 4 磅兜底。
        for (w, c) in grid.iter_mut().zip(content) {
            *w = (c.0 + (c.1.max(c.0) - c.0) * k).max(4.0);
        }
        return;
    }
    // 有声明宽度：short = 各列“最长单词放不下”的总缺口；无缺口则网格照旧。
    let short: f32 = grid.iter().zip(content).map(|(w, c)| (c.0 - *w).max(0.0)).sum();
    if short < 0.01 {
        return;
    }
    // spare = 各列高出其最长单词的总余量；give = 让出比例（缺口/余量，封顶 1）。
    let spare: f32 = grid.iter().zip(content).map(|(w, c)| (*w - c.0).max(0.0)).sum();
    let give = (short / spare.max(0.01)).min(1.0);
    // 过窄的列补到最长单词宽度，有余量的列按比例让出。
    for (w, c) in grid.iter_mut().zip(content) {
        *w = if *w < c.0 { c.0 } else { *w - (*w - c.0) * give };
    }
}
