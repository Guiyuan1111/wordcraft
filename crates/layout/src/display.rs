//! Page → draw items, shared by the raster renderer, the PDF exporter and thumbnails.
//!
//! —— 中文标注（业务流程概述）——
//! 本文件把排版产物 `Page`（一组页面坐标下的 Placed 元素）转成绘制指令列表 `Vec<Draw>`，
//! 供光栅渲染器、PDF 导出器与缩略图三方共用。`page_display` 主流程：
//!   1. 先画页眉、页脚（dim_header 时整体降透明度到 0.45），再画正文（dim_body 同理）——
//!      “页装饰”指页眉页脚，本文件不含水印与页面边框的绘制；
//!   2. 正文元素逐个经 `item` 转换，其中文字行进入 `lines` 逐行绘制：批注底色 →
//!      高亮/字符底纹与字符边框 → 列表编号 → 行尾连字符 → 字形串（按样式分组，应用
//!      修订颜色与插入/删除标记）→ 下划线/删除线 → 拼写校对波浪线 → 制表符前导符 →
//!      行内对象（图片/形状/组合/图表/公式）→ 格式标记（¶ · → ↵、分页分栏、分节符）；
//!   3. 若开启修订标记并显示更改行，最后由 `change_bars` 在页边画修订竖线。
//! 修订颜色按作者序号从 `revision_color` 调色板取色；插入/删除的标记方式由 MarkupOptions
//! （对应 Word 的“修订选项”）控制。`Draw::Turned` 包裹竖排文本、`Draw::Rotated` 包裹旋转/
//! 翻转的对象、`Draw::Figure` 把行内图表连同替代文本打包（供 tagged PDF）。
//! 注意：文本框不在本文件递归展开——其内容由 lib.rs 的 layout_box 预先排版成普通页面元素，
//! 这里只逐项转换（`Placed::Object`/`Placed::Cell` 在 `item` 中被跳过，它们仅用于命中测试）。

use wordcraft_doc::graphic::{Graphic, GraphicItem, GraphicKind, PathSeg, TextAlign};
use wordcraft_doc::para::{InlineObject, ShapeKind};
use wordcraft_doc::props::{Border, BorderStyle, Rgb, TextColor, TextDirection, Underline};
use wordcraft_doc::section::SectionStart;
use wordcraft_doc::{Block, Document, Path, StoryRef};
use wordcraft_fonts::{BezPath, FaceRef};
use wordcraft_geom::{Rect, Spin};

use crate::math::MItem;
use crate::para::{ClKind, LineEnd, ParaLayout};
use crate::{Page, Placed};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// 线型：实线 / 点线 / 虚线 / 双线 / 波浪线（边框、下划线、校对波浪线共用）。
pub enum Stroke {
    Solid,
    Dotted,
    Dashed,
    Double,
    Wave,
}

#[derive(Clone, Debug)]
/// 与后端无关的绘制指令列表：渲染器、PDF 导出器、缩略图都消费它。
pub enum Draw {
    /// 一段同样式字形串：文字绘制的核心指令。
    Glyphs {
        /// 字体引用（含变体选择）。
        face: FaceRef,
        /// 字号（磅）。
        size: f32,
        /// Glyph id, x, baseline y (page coordinates).
        /// 字形序列：(字形 ID, x, 基线 y)，坐标为页面坐标。
        glyphs: Vec<(u32, f32, f32)>,
        /// 文字颜色。
        color: Rgb,
        /// 不透明度（页眉页脚淡显、选区外淡显等用）。
        alpha: f32,
        /// 合成粗体（字体本身无粗体时描边模拟）。
        synth_bold: bool,
        /// 合成斜体（字体本身无斜体时倾斜模拟）。
        synth_italic: bool,
        /// The text these glyphs show (for PDF text extraction), and the hyperlink.
        /// 这串字形显示的文本（供 PDF 提取文本用）与超链接目标。
        text: String,
        link: Option<String>,
        /// Byte range of `text` each glyph shows (one per glyph; a ligature's range covers all
        /// its characters). Empty when unknown: the PDF writer then guesses from the font.
        /// 每个字形对应 `text` 的字节区间（与 glyphs 一一对应；连字的区间覆盖其全部字符）。
        /// 有了它 PDF 里的文字才可选中/可复制；为空时 PDF 写出端只能按字体猜测映射。
        ranges: Vec<std::ops::Range<usize>>,
    },
    /// 矩形纯色填充：底纹、高亮、批注锚点底色等。
    Fill {
        rect: Rect,
        color: Rgb,
        alpha: f32,
    },
    /// 线段：段落边框、下划线/删除线、修订竖线、校对波浪线等。
    Line {
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        width: f32,
        color: Rgb,
        stroke: Stroke,
        alpha: f32,
    },
    /// 位图：rect 为页面矩形，media 为媒体资源标识，crop 为裁剪参数。
    Image {
        rect: Rect,
        media: String,
        crop: [f32; 4],
        alpha: f32,
    },
    /// 预设形状（矩形/椭圆/三角等），可描边填充，effects 为阴影/发光/柔化边缘效果。
    Shape {
        rect: Rect,
        kind: ShapeKind,
        fill: Option<Rgb>,
        stroke: Option<Rgb>,
        stroke_width: f32,
        /// Shadow, glow and soft edges (drawn with [`wordcraft_doc::effects::bands`]).
        /// 阴影、发光与柔化边缘（用 [`wordcraft_doc::effects::bands`] 绘制）。
        effects: wordcraft_doc::effects::ShapeEffects,
    },
    /// An ink stroke: a line through `pts` (page points) with round ends and joins, `width`
    /// wide, in `color` at opacity `alpha`.
    /// 墨迹笔画：穿过页面点列 `pts` 的折线，圆头圆角连接，线宽 `width`，颜色 `color`，不透明度
    /// `alpha`（手写/批注笔迹，受 DisplayOptions::hide_ink 控制是否显示）。
    Ink {
        pts: Vec<(f32, f32)>,
        color: Rgb,
        width: f32,
        alpha: f32,
    },
    /// A vector path (a chart's or diagram's polygons, slices, lines): filled, then stroked.
    /// 矢量路径（图表/示意图的多边形、扇区、线段）：先填充后描边。
    Path {
        segs: Vec<PathSeg>,
        fill: Option<Rgb>,
        stroke: Option<Rgb>,
        stroke_width: f32,
    },
    /// An inline chart or diagram's draws, as one figure with its alt text (for tagged PDF).
    /// 行内图表/示意图：其全部绘制指令打包成一个图元并携带替代文本（供 tagged PDF 无障碍）。
    Figure {
        alt: String,
        kind: GraphicKind,
        draws: Vec<Draw>,
    },
    /// A formatting mark (¶ · → ↵) in the UI's mark colour, or in `color` (a tracked
    /// paragraph mark in its reviser's colour).
    /// 格式标记（¶ · → ↵ 等），通常用界面标记色绘制；`color` 非空时用指定色（如被修订的
    /// 段落标记用修订者颜色）。仅屏幕显示，不进打印/PDF。
    Mark {
        x: f32,
        baseline: f32,
        size: f32,
        ch: char,
        color: Option<Rgb>,
    },
    /// Turned text (a table cell's text direction): `items` are drawn in a frame turned `turn`
    /// whose origin is page point (`x`, `y`) (see [`crate::turn_point`]).
    /// 竖排文本容器（表格单元格文字方向）：`items` 在一个按 `turn` 旋转、原点为页面点
    /// (`x`, `y`) 的坐标系里绘制（仿射见 [`crate::turn_point`]）。
    Turned {
        x: f32,
        y: f32,
        turn: TextDirection,
        items: Vec<Draw>,
    },
    /// A rotated or flipped picture, shape or chart: `items` drawn turned by `spin` about page
    /// point (`cx`, `cy`) (see [`Spin::matrix`]).
    /// 旋转/翻转容器（图片、形状、图表）：`items` 绕页面点 (`cx`, `cy`) 按 `spin` 变换绘制
    /// （矩阵见 [`Spin::matrix`]）。
    Rotated {
        cx: f32,
        cy: f32,
        spin: Spin,
        items: Vec<Draw>,
    },
    /// A formatting-mark label (a section break's name) in the UI's mark colour, left edge at
    /// `x`, drawn with the same face as [`Draw::Mark`]. Screen only, like every mark.
    /// 格式标记文本（分节符名称等），界面标记色，左缘在 `x`，字体与 [`Draw::Mark`] 相同。
    /// 与所有格式标记一样仅屏幕显示。
    MarkText {
        x: f32,
        baseline: f32,
        size: f32,
        text: String,
    },
}

impl Draw {
    /// The affine map (a, b, c, d, e, f: x' = a·x + c·y + e, y' = b·x + d·y + f) from a frame
    /// turned `turn` with its origin at page point (`x`, `y`) to the page.
    /// 竖排坐标到页面坐标的仿射矩阵 (a,b,c,d,e,f)：x' = a·x + c·y + e，y' = b·x + d·y + f。
    /// 原点在页面点 (`x`, `y`) 的竖排行框按 `turn` 映射到页面（Horizontal 为恒等平移）。
    pub fn turn_matrix(turn: TextDirection, x: f32, y: f32) -> [f32; 6] {
        match turn {
            TextDirection::Horizontal => [1.0, 0.0, 0.0, 1.0, x, y],
            TextDirection::Down => [0.0, 1.0, -1.0, 0.0, x, y],
            TextDirection::Up => [0.0, -1.0, 1.0, 0.0, x, y],
        }
    }
}

#[derive(Clone, Debug)]
/// 显示选项：控制本次转 Draw 时画什么、怎么画（对应 Word 的视图/审阅开关）。
pub struct DisplayOptions {
    /// Show formatting marks (¶).
    /// 是否显示格式标记（¶ · → ↵ 等）。
    pub marks: bool,
    /// Dim headers/footers (editing the body) or the body (editing a header/footer).
    /// 是否淡显页眉页脚（编辑正文时）/ 正文（编辑页眉页脚时）。
    pub dim_header: bool,
    pub dim_body: bool,
    /// Show tracked changes as markup (coloured, underlined/struck).
    /// 是否以修订标记形式显示修订（着色、下划线/删除线）。
    pub markup: bool,
    /// Show on-screen-only marks: equation placeholders and prompts (never in print or PDF).
    /// 是否显示仅屏幕可见的占位与提示（如公式占位符）；打印与 PDF 永不显示。
    pub placeholders: bool,
    /// Review › Hide Ink: leave ink strokes out (they stay in the document).
    /// “审阅 › 隐藏墨迹”：不绘制墨迹笔画（文档中仍保留）。
    pub hide_ink: bool,
    /// How tracked changes and comments are marked when `markup` is on (Track Changes Options).
    /// 修订标记的具体方式（对应 Word 的“修订选项”对话框）。
    pub revisions: MarkupOptions,
}

impl Default for DisplayOptions {
    fn default() -> Self {
        DisplayOptions {
            marks: false,
            dim_header: true,
            dim_body: false,
            markup: true,
            placeholders: false,
            hide_ink: false,
            revisions: MarkupOptions::default(),
        }
    }
}

/// An option list with stable names for commands and saved preferences.
/// 声明式宏：生成一个“带稳定名称的枚举”——用于命令与偏好设置存取（serde camelCase、
/// ALL 表、name()/parse()）。
macro_rules! named {
    ($(#[$m:meta])* $name:ident { $($(#[$vm:meta])* $v:ident = $s:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        pub enum $name { $($(#[$vm])* $v),+ }
        impl $name {
            /// Every choice with its name, in menu order.
            // 全部选项及其稳定名称（按菜单顺序）：命令与设置界面用它枚举展示。
            pub const ALL: &'static [($name, &'static str)] = &[$(($name::$v, $s)),+];
            // name()：取本选项的稳定名称（未匹配返回空串）。
            pub fn name(self) -> &'static str {
                Self::ALL.iter().find(|(v, _)| *v == self).map(|(_, s)| *s).unwrap_or("")
            }
            // parse()：按稳定名称反查选项（未匹配返回 None）。
            pub fn parse(s: &str) -> Option<Self> {
                Self::ALL.iter().find(|(_, n)| *n == s).map(|(v, _)| *v)
            }
        }
    };
}

named! {
    /// How inserted text is marked.
    /// 插入文本的标记方式（下划线/双下划线/加粗/斜体/删除线/仅变色/不标记）。
    InsertMark {
        #[default]
        Underline = "underline",
        DoubleUnderline = "doubleUnderline",
        Bold = "bold",
        Italic = "italic",
        Strikethrough = "strikethrough",
        ColorOnly = "colorOnly",
        None = "none",
    }
}

named! {
    /// How deleted text is marked.
    /// 删除文本的标记方式（删除线/双删除线/隐藏/脱字符^/#号/下划线/仅变色）。
    DeleteMark {
        #[default]
        Strikethrough = "strikethrough",
        DoubleStrikethrough = "doubleStrikethrough",
        Hidden = "hidden",
        Caret = "caret",
        Hash = "hash",
        Underline = "underline",
        ColorOnly = "colorOnly",
    }
}

named! {
    /// Where the bar beside changed lines goes.
    /// 更改行旁修订竖线的位置：外侧（镜像页边距时取外侧）/左/右/不显示。
    ChangeBar {
        #[default]
        Outside = "outside",
        Left = "left",
        Right = "right",
        None = "none",
    }
}

named! {
    /// What the markup area beside the page shows.
    /// 页边批注区显示什么：修订（当前画法与 CommentsAndFormatting 相同，修订仍在行内——疑为
    /// 未完成的分支）/ 全部行内不占边区 / 批注与格式（默认）。
    BalloonMode {
        /// Deletions, formatting and comments in balloons (drawn like `CommentsAndFormatting`
        /// for now: revisions stay inline).
        Revisions = "revisions",
        /// Everything inline, no markup area.
        Inline = "inline",
        #[default]
        CommentsAndFormatting = "commentsAndFormatting",
    }
}

/// Track Changes Options: what markup shows and how revisions are drawn. A per-user preference,
/// as in Word (saved with the interface settings, not the document).
/// 修订选项（Track Changes Options）：显示哪些标记、修订怎么画。是按用户的界面偏好，
/// 与 Word 一致存于界面设置而非文档。
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct MarkupOptions {
    /// 是否显示批注。
    pub comments: bool,
    /// 是否显示墨迹修订。
    pub ink: bool,
    /// 是否显示插入/删除标记。
    pub insertions_deletions: bool,
    /// 是否显示格式更改标记。
    pub formatting: bool,
    /// 页边批注区模式。
    pub balloons: BalloonMode,
    /// 插入标记方式。
    pub insert_mark: InsertMark,
    /// `None` = by author.
    /// 插入文字颜色；None = 按作者自动配色。
    pub insert_color: Option<Rgb>,
    /// 删除标记方式。
    pub delete_mark: DeleteMark,
    /// `None` = by author.
    /// 删除文字颜色；None = 按作者自动配色。
    pub delete_color: Option<Rgb>,
    /// 更改行竖线位置。
    pub changed_lines: ChangeBar,
    /// `None` = automatic (dark grey).
    /// 更改行竖线颜色；None = 自动（深灰 CHANGE_BAR）。
    pub changed_lines_color: Option<Rgb>,
    /// Record formatting changes while tracking.
    /// 修订时是否同时记录格式更改。
    pub track_formatting: bool,
}

impl Default for MarkupOptions {
    fn default() -> Self {
        MarkupOptions {
            comments: true,
            ink: true,
            insertions_deletions: true,
            formatting: true,
            balloons: BalloonMode::default(),
            insert_mark: InsertMark::default(),
            insert_color: None,
            delete_mark: DeleteMark::default(),
            delete_color: None,
            changed_lines: ChangeBar::default(),
            changed_lines_color: None,
            track_formatting: true,
        }
    }
}

impl MarkupOptions {
    /// Tracked deletions take no room: they are hidden, or insertions and deletions aren't shown.
    /// 删除内容是否不占版面：删除标记为“隐藏”，或插入/删除标记整体关闭时为真。
    pub fn hides_deletions(&self) -> bool {
        !self.insertions_deletions || self.delete_mark == DeleteMark::Hidden
    }
}

/// Colour for revisions by author index.
/// 修订作者配色：按作者序号从 6 色调色板循环取色（紫/蓝/绿/橙/紫罗兰/青）。
pub fn revision_color(i: u32) -> Rgb {
    const C: [Rgb; 6] =
        [Rgb(0xB0, 0x1E, 0x8F), Rgb(0x1E, 0x6E, 0xB0), Rgb(0x2E, 0x8B, 0x3E), Rgb(0xC0, 0x5A, 0x10), Rgb(0x70, 0x3C, 0xB0), Rgb(0x0E, 0x7C, 0x86)];
    C.get(i as usize % C.len()).copied().unwrap_or(C[0])
}

/// 下划线样式 → 线型映射：点/虚/点划归 Dotted/Dashed，双线/双波浪归 Double/Wave，
/// 其余（含波浪）按 Word 直觉映射。
fn stroke_of(u: Underline) -> Stroke {
    match u {
        Underline::Dotted => Stroke::Dotted,
        Underline::Dash | Underline::DotDash | Underline::DotDotDash => Stroke::Dashed,
        Underline::Double => Stroke::Double,
        Underline::Wave | Underline::DoubleWave => Stroke::Wave,
        _ => Stroke::Solid,
    }
}

/// Draw items for a page.
/// 单页 → 绘制指令列表的主入口。流程：页眉 → 页脚（淡显系数 ha）→ 正文（系数 ba）→
/// 更改行修订竖线（change_bars）。修订颜色、校对波浪线、格式标记都在 `lines` 内部按行处理。
pub fn page_display(doc: &Document, page: &Page, opts: &DisplayOptions) -> Vec<Draw> {
    let mut out = Vec::new();
    // 淡显系数：编辑正文时页眉页脚压到 0.45，编辑页眉页脚时正文压到 0.45。
    let ha = if opts.dim_header { 0.45 } else { 1.0 };
    let ba = if opts.dim_body { 0.45 } else { 1.0 };
    // 页眉在前、页脚其次、正文最后：正文绘制在最上层。
    for it in &page.header {
        item(doc, it, opts, ha, &mut out);
    }
    for it in &page.footer {
        item(doc, it, opts, ha, &mut out);
    }
    for it in &page.items {
        item(doc, it, opts, ba, &mut out);
    }
    // 更改行竖线最后画：需要显示插入/删除或格式标记，且竖线模式非 None。
    if opts.markup && (opts.revisions.insertions_deletions || opts.revisions.formatting) && opts.revisions.changed_lines != ChangeBar::None {
        change_bars(doc, page, &opts.revisions, ba, &mut out);
    }
    out
}

/// The automatic colour of the bars beside changed lines.
/// 更改行修订竖线的自动颜色（深灰）；用户可在修订选项里覆盖。
pub const CHANGE_BAR: Rgb = Rgb(0x50, 0x50, 0x50);

/// Bars in the margin beside body lines with tracked insertions or deletions, or (when shown)
/// tracked formatting changes of their text, paragraph or paragraph mark.
/// 在页边为含修订的正文行画竖线：该行有插入/删除，或（开启时）其文本/段落/段落标记有
/// 格式修订。逐行判断后画 0.75 磅竖线段。
fn change_bars(doc: &Document, page: &Page, m: &MarkupOptions, alpha: f32, out: &mut Vec<Draw>) {
    // Outside: the left margin, or the outer one of a right-hand page with mirrored margins.
    // Outside（外侧）：镜像页边距时奇数页（1、3、…，右页）外侧在右，否则一律画在左侧。
    let right = match m.changed_lines {
        ChangeBar::Right => true,
        ChangeBar::Outside => doc.settings.mirror_margins && page.number % 2 == 1,
        _ => false,
    };
    // 竖线 x：右侧在正文右缘外 9 磅，左侧在正文左缘外 9 磅。
    let bx = if right { page.body.right() + 9.0 } else { page.body.x - 9.0 };
    let color = m.changed_lines_color.unwrap_or(CHANGE_BAR);
    for it in &page.items {
        // 只看正文文字行；竖排文字行不画竖线。
        let Placed::Lines { story, path, para: pl, l0, l1, y, turn, .. } = it else { continue };
        if turn.is_turned() {
            continue;
        }
        let Some(first) = pl.lines.get(*l0) else { continue };
        let (text, fmt) = (m.insertions_deletions, m.formatting);
        let para = doc.para(*story, path);
        // 段落标记本身被修订（插入/删除/格式）：只在该段的段落标记所在行体现。
        let mark_rev = para.is_some_and(|p| (text && (p.mark.ins.is_some() || p.mark.del.is_some())) || (fmt && p.mark.fmt_change.is_some()));
        // A paragraph whose own formatting changed is marked on every line.
        // 段落自身格式被修订：该段每一行都画竖线。
        let para_fmt = fmt && para.is_some_and(|p| p.props.fmt_change.is_some());
        for li in *l0..*l1 {
            let Some(line) = pl.lines.get(li) else { continue };
            // 行级判定：段落格式修订、行内任一样式含插入/删除/格式修订、或本行是
            // 段落标记行且段落标记被修订。
            let changed = para_fmt
                || (line.c0..line.c1)
                    .filter_map(|k| pl.clusters.get(k).and_then(|c| pl.styles.get(c.style as usize)))
                    .any(|st| (text && (st.rc.ins.is_some() || st.rc.del.is_some())) || (fmt && st.rc.fmt.is_some()))
                || (mark_rev && line.end == LineEnd::Para);
            if changed {
                // 该行画一段 0.75 磅竖线（y 相对首行行顶换算）。
                let top = y + (line.top - first.top);
                out.push(Draw::Line { x0: bx, y0: top, x1: bx, y1: top + line.height, width: 0.75, color, stroke: Stroke::Solid, alpha });
            }
        }
    }
}

/// 边框样式 → 线型映射（Draw::Line 用）。
fn border_stroke(s: BorderStyle) -> Stroke {
    match s {
        BorderStyle::Dotted => Stroke::Dotted,
        BorderStyle::Dashed | BorderStyle::DotDash => Stroke::Dashed,
        BorderStyle::Double | BorderStyle::Triple => Stroke::Double,
        BorderStyle::Wave => Stroke::Wave,
        _ => Stroke::Solid,
    }
}

/// A border line from `(x0, y0)` to `(x1, y1)` (paragraph rules and character borders).
/// 一条边框线：把 Border（样式/宽度/颜色）转成 Draw::Line（段落边框、字符边框共用）。
/// Thick 样式最细 1.5 磅，其余最细 0.25 磅；无色时取黑。
fn rule(x0: f32, y0: f32, x1: f32, y1: f32, b: &Border, alpha: f32) -> Draw {
    Draw::Line {
        x0,
        y0,
        x1,
        y1,
        width: if b.style == BorderStyle::Thick { b.width.max(1.5) } else { b.width.max(0.25) },
        color: b.color.unwrap_or(Rgb::BLACK),
        stroke: border_stroke(b.style),
        alpha,
    }
}

/// Closed box around one line segment of a character-border group, widened by `space`.
// Note: border drawn outside text advance, reserve width in line breaking if overlap shows.
/// 字符边框：给一行内的一段“同边框字符组”画一个闭合矩形框，四周外扩 b.space。
// Note: border drawn outside text advance, reserve width in line breaking if overlap shows.
fn char_box(b: &Border, x0: f32, x1: f32, top: f32, bottom: f32, alpha: f32, out: &mut Vec<Draw>) {
    let (l, r) = (x0 - b.space, x1 + b.space);
    out.push(rule(l, top, r, top, b, alpha));
    out.push(rule(l, bottom, r, bottom, b, alpha));
    out.push(rule(l, top, l, bottom, b, alpha));
    out.push(rule(r, top, r, bottom, b, alpha));
}

/// 单个已放置元素 → 绘制指令的分发：填充/线段直接转换；图片/形状/图表带旋转时包一层
/// Rotated（spun）；文字行交给 `lines`（竖排行包 Turned）。
fn item(doc: &Document, it: &Placed, opts: &DisplayOptions, alpha: f32, out: &mut Vec<Draw>) {
    match it {
        Placed::Fill { rect, color } => out.push(Draw::Fill { rect: *rect, color: *color, alpha }),
        Placed::Rule { x0, y0, x1, y1, border } => out.push(rule(*x0, *y0, *x1, *y1, border, alpha)),
        Placed::Image { rect, media, crop, spin, .. } => {
            spun(*spin, *rect, vec![Draw::Image { rect: *rect, media: media.clone(), crop: *crop, alpha }], out)
        }
        Placed::Shape { rect, kind, fill, stroke, stroke_width, effects, freeform, spin } => {
            let mut items = Vec::new();
            shape_draws(*rect, *kind, *fill, *stroke, *stroke_width, effects_in(*effects, *spin), freeform.as_deref(), opts, &mut items);
            spun(*spin, *rect, items, out)
        }
        Placed::Graphic { rect, graphic, spin, .. } => spun(*spin, *rect, graphic_draws(doc, graphic, *rect, alpha), out),
        // 单元格区域与行内对象占位不产生绘制指令：表格单元格的可见内容已拆成 Lines/Fill/Rule
        // 逐项放置，这两类元素只用于命中测试与环绕计算（见 hit.rs / lib.rs）。
        Placed::Cell { .. } | Placed::Object { .. } => {}
        // 竖排文字行：先按行框局部坐标转出指令，再整体包进 Turned；
        // 链接热区是轴对齐的页面矩形，竖排文字无法给出，故清空其超链接。
        Placed::Lines { story, path, para, l0, l1, x, y, turn } if turn.is_turned() => {
            let mut items = Vec::new();
            lines(doc, *story, path, para, *l0, *l1, 0.0, 0.0, opts, alpha, &mut items);
            // Link areas are axis-aligned page rectangles: turned text gets none.
            for d in &mut items {
                if let Draw::Glyphs { link, .. } = d {
                    *link = None;
                }
            }
            out.push(Draw::Turned { x: *x, y: *y, turn: *turn, items });
        }
        // 普通水平文字行：直接转换。
        Placed::Lines { story, path, para, l0, l1, x, y, .. } => lines(doc, *story, path, para, *l0, *l1, *x, *y, opts, alpha, out),
    }
}

/// 把段落的行区间 [l0, l1) 转成绘制指令（`lines` 是文字绘制的核心）。
/// 逐行依次绘制：批注底色 → 高亮/字符底纹 + 字符边框 → 列表编号 → 连字符 → 按样式分组的
/// 字形串（含修订颜色与插入/删除标记）→ 下划线/删除线 → 校对波浪线 → 制表符前导符 →
/// 行内对象 → 格式标记。(x, y) 为放置原点（竖排行则为行框原点，调用方已剥去旋转）。
fn lines(
    doc: &Document,
    story: StoryRef,
    path: &Path,
    pl: &ParaLayout,
    l0: usize,
    l1: usize,
    x: f32,
    y: f32,
    opts: &DisplayOptions,
    alpha: f32,
    out: &mut Vec<Draw>,
) {
    let Some(first) = pl.lines.get(l0) else { return };
    let para = doc.para(story, path);
    for li in l0..l1 {
        let Some(line) = pl.lines.get(li) else { continue };
        // 本行的行顶 top、基线 base、行底 bottom（y 平移到本行）。
        let top = y + (line.top - first.top);
        let base = top + (line.baseline - line.top);
        let bottom = top + line.height;
        // Commented text gets a soft shade (comment anchors are object markers).
        // 批注锚定的文本铺一层淡紫底色（批注锚点是对象标记，扫描成对取锚区）；
        // 未闭合的批注一直延伸到段落末尾。
        if opts.markup
            && opts.revisions.comments
            && let Some(p) = para
        {
            let mut open: Option<usize> = None;
            let mut ranges = Vec::new();
            for off in p.object_offsets() {
                match p.object_at(off) {
                    Some(InlineObject::CommentStart { .. }) => open = open.or(Some(off)),
                    Some(InlineObject::CommentEnd { .. }) => {
                        ranges.push((open.take().unwrap_or(0), off));
                    }
                    _ => {}
                }
            }
            if let Some(o) = open {
                ranges.push((o, p.len()));
            }
            for (a, b) in ranges {
                if b <= line.start || a >= line.stop {
                    continue;
                }
                for (x0, x1) in pl.x_spans(li, a.max(line.start), b.min(line.stop)) {
                    if x1 > x0 {
                        out.push(Draw::Fill { rect: Rect::new(x + x0, top, x1 - x0, line.height), color: Rgb(0xEF, 0xE3, 0xF7), alpha });
                    }
                }
            }
        }
        // Backgrounds first: highlight and character shading; character borders group equal adjacent borders.
        // 先画背景：字符高亮/底纹逐簇铺色；字符边框把相邻同款边框的簇合并成组再画框
        //（open 记录当前未闭合组的边框与左右界）。
        let mut open: Option<(Border, f32, f32)> = None;
        for k in line.c0..line.c1 {
            let (Some(c), Some(cx), Some(nx)) = (pl.clusters.get(k), line.cl_left(k), line.cl_right(k)) else { continue };
            let Some(st) = pl.styles.get(c.style as usize) else { continue };
            if c.kind == ClKind::Marker {
                continue;
            }
            if let Some(h) = st.rc.highlight.or(st.rc.shading) {
                out.push(Draw::Fill { rect: Rect::new(x + cx, top, (nx - cx).max(0.0), line.height), color: h, alpha });
            }
            let b = st.rc.border;
            if let Some((ob, x0, x1)) = open.as_mut()
                && Some(*ob) == b
            {
                // Right-to-left text grows leftwards.
                // 从右向左的文字横向坐标向左增长：取 min/max 而非简单延伸。
                *x0 = x0.min(x + cx);
                *x1 = x1.max(x + nx);
                continue;
            }
            if let Some((ob, x0, x1)) = open.take() {
                char_box(&ob, x0, x1, top, bottom, alpha, out);
            }
            open = b.map(|b| (b, x + cx, x + nx));
        }
        if let Some((ob, x0, x1)) = open {
            char_box(&ob, x0, x1, top, bottom, alpha, out);
        }
        // Label.
        // 列表编号标签：只在首行画一次（编号挂在段落首行基线上）。
        if li == 0
            && let Some(lab) = &pl.label
            && let Some(st) = pl.styles.get(lab.style as usize)
        {
            let gx = x + lab.x;
            out.push(Draw::Glyphs {
                face: st.face,
                size: st.size,
                glyphs: lab.glyphs.iter().map(|g| (g.gid, gx + g.dx, base - st.shift - g.dy)).collect(),
                color: text_color(&st.rc.color, None),
                alpha,
                synth_bold: st.synth_bold,
                synth_italic: st.synth_italic,
                text: lab.text.clone(),
                link: None,
                ranges: Vec::new(),
            });
        }
        // Hyphen at a hyphenated line end.
        // 断词连字符：行尾因断词产生的“-”按其样式画出。
        if let Some((si, gid, adv)) = line.hyphen
            && let Some(st) = pl.styles.get(si as usize)
        {
            let hx = x + line.hyphen_x(adv);
            out.push(Draw::Glyphs {
                face: st.face,
                size: st.size,
                glyphs: vec![(gid, hx, base - st.shift)],
                color: text_color(&st.rc.color, None),
                alpha,
                synth_bold: st.synth_bold,
                synth_italic: st.synth_italic,
                text: "-".into(),
                link: None,
                ranges: Vec::new(),
            });
        }
        // Glyph runs grouped by style.
        // 字形串：按样式分组逐段生成 Glyphs（样式相同的相邻簇合为一段）。
        // 内层循环收簇：记录文本 text、每字形对应 text 的字节区间 ranges（保 PDF 可选中）。
        let mut k = line.c0;
        while k < line.c1 {
            let Some(c) = pl.clusters.get(k) else { break };
            let si = c.style;
            let Some(st) = pl.styles.get(si as usize) else {
                k += 1;
                continue;
            };
            let mut glyphs = Vec::new();
            let mut text = String::new();
            let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
            let run_start = k;
            while k < line.c1 {
                let Some(c) = pl.clusters.get(k) else { break };
                if c.style != si {
                    break;
                }
                let cx = x + line.cl_left(k).unwrap_or(0.0);
                if matches!(c.kind, ClKind::Text | ClKind::Space) {
                    let a = text.len();
                    // An object's cluster (a field, a note number) stands for the text it shows.
                    // 对象簇（域结果、脚注编号等）本身无源文本：从 pl.shown 查它代表显示的文本。
                    let mut shown = None;
                    if let Ok(i) = pl.shown.binary_search_by_key(&k, |(i, _, _)| *i)
                        && let Some((_, s, r)) = pl.shown.get(i)
                    {
                        text.push_str(s);
                        shown = Some(r);
                    } else if let Some(p) = para {
                        text.push_str(p.text.get(c.start..c.end).unwrap_or(""));
                    }
                    let b = text.len();
                    let cg = pl.glyphs.get(c.g0 as usize..c.g1 as usize).unwrap_or(&[]);
                    if let Some(shown) = shown
                        && cg.len() > 1
                    {
                        // A whole field result is one cluster of many glyphs: each glyph gets the
                        // text it was shaped from (a ligature its letters), not the whole text as
                        // one ligature. Without that, a character each (the last takes any remainder).
                        // 一个域结果是“一簇多字形”：按字形逐个切分字节区间——已知每字形对应文本
                        // （shown 长度吻合）就照抄；否则按字符边界均摊（最后一个字形吃掉剩余）。
                        // 若不细分会把整段域文本当成一个“连字”，PDF 里无法部分选中。
                        let bounds: Vec<usize> = text.get(a..b).unwrap_or("").char_indices().map(|(i, _)| a + i).collect();
                        let known = shown.len() == cg.len();
                        for (n, g) in cg.iter().enumerate() {
                            let r = match shown.get(n).filter(|_| known) {
                                Some(r) => (a + r.start).min(b)..(a + r.end).min(b),
                                None => {
                                    let from = bounds.get(n).or(bounds.last()).copied().unwrap_or(a);
                                    let to = if n + 1 == cg.len() { b } else { bounds.get(n + 1).copied().unwrap_or(b) };
                                    from..to
                                }
                            };
                            glyphs.push((g.gid, cx + g.dx, base - st.shift - g.dy));
                            ranges.push(r.start..r.end.max(r.start));
                        }
                        k += 1;
                        continue;
                    }
                    if cg.is_empty() {
                        // No glyph of its own (the second letter of a ligature such as لا): its
                        // text belongs to the glyphs before it.
                        // 该簇没有自己的字形（如连字 لا 的第二个字母）：把它的文本区间并入
                        // 前面连续同起点的字形区间。
                        let from = ranges.last().map(|r| r.start);
                        for r in ranges.iter_mut().rev().take_while(|r| Some(r.start) == from) {
                            r.end = b;
                        }
                    }
                    for g in cg {
                        glyphs.push((g.gid, cx + g.dx, base - st.shift - g.dy));
                        ranges.push(a..b);
                    }
                }
                k += 1;
            }
            let run_end = k;
            let rc = &st.rc;
            let m = &opts.revisions;
            // —— 修订处理 ——
            // Without markup a deletion is never drawn as ordinary text, even in a layout that
            // kept it; nor when deletions are hidden.
            // 关闭修订标记、或删除标记设为“隐藏”时：被删文本绝不按普通文字画出
            //（即便布局阶段为了测量保留了它）。
            if rc.del.is_some() && (!opts.markup || m.hides_deletions()) {
                continue;
            }
            // 修订开着才显示插入/删除着色；同一处既有删除又有插入时，删除优先（画删除色）。
            let shown = opts.markup && m.insertions_deletions;
            let del = rc.del.filter(|_| shown);
            let ins = rc.ins.filter(|_| shown && del.is_none());
            // 作者色：按修订记录里作者首次出现的顺序取 revision_color 调色板。
            let author = |r: u32| revision_color(doc.revisions.get(r as usize).map(|v| author_index(doc, &v.author)).unwrap_or(0));
            // 颜色优先级：插入色 > 删除色 > 普通文字色（Auto 时按底纹亮度反色）。
            let color = match (ins, del) {
                (Some(r), _) => m.insert_color.unwrap_or_else(|| author(r)),
                (None, Some(r)) => m.delete_color.unwrap_or_else(|| author(r)),
                _ => text_color(&rc.color, rc.shading.or(rc.highlight)),
            };
            let ins_mark = ins.map(|_| m.insert_mark);
            let del_mark = del.map(|_| m.delete_mark);
            if del_mark == Some(DeleteMark::Caret) {
                // A caret where the text was, instead of the text.
                // 删除标记 = 脱字符 ^：在原文字位置画一个小 ^ 号代替整段文本。
                if let Some(cx) = line.cl_left(run_start).map(|c| x + c) {
                    let h = st.size * 0.3;
                    let w = (st.size / 18.0).max(0.6);
                    out.push(Draw::Line {
                        x0: cx - h * 0.6,
                        y0: base + 1.0,
                        x1: cx,
                        y1: base + 1.0 - h,
                        width: w,
                        color,
                        stroke: Stroke::Solid,
                        alpha,
                    });
                    out.push(Draw::Line {
                        x0: cx,
                        y0: base + 1.0 - h,
                        x1: cx + h * 0.6,
                        y1: base + 1.0,
                        width: w,
                        color,
                        stroke: Stroke::Solid,
                        alpha,
                    });
                }
                continue;
            }
            if del_mark == Some(DeleteMark::Hash) {
                // Each deleted character shows as #.
                // 删除标记 = #号：把该段所有字形统一替换成 # 字形。
                let gid = st.face.glyph_for('#');
                for g in &mut glyphs {
                    g.0 = gid;
                }
            }
            // 正式生成这段字形（颜色/合成粗斜体已含修订标记的加粗/斜体）。
            if !glyphs.is_empty() {
                out.push(Draw::Glyphs {
                    face: st.face,
                    size: st.size,
                    glyphs,
                    color,
                    alpha,
                    synth_bold: st.synth_bold || ins_mark == Some(InsertMark::Bold),
                    synth_italic: st.synth_italic || ins_mark == Some(InsertMark::Italic),
                    text,
                    link: rc.link.clone(),
                    ranges,
                });
            }
            // Decorations across the run (underline skips trailing spaces of the line). Right-to-left
            // text inside a run can split it into several visual spans.
            // 行内修饰（下划线/删除线）跨整段画：下划线跳过行尾空白；从右向左的文字可能把
            // 一段拆成多个视觉区间（line.spans 返回多个 (起,止)）。
            let mut end_k = run_end;
            // 收缩区间末尾：行末的空白/标记/换行簇不参与下划线（仅当这段就是行尾时）。
            while end_k > run_start
                && pl.clusters.get(end_k - 1).is_some_and(|c| matches!(c.kind, ClKind::Space | ClKind::Marker | ClKind::LineBreak))
                && end_k == line.c1
            {
                end_k -= 1;
            }
            // 线宽随字号缩放（最小 0.5 磅）。
            let thick = (st.size / 18.0).max(0.5);
            // 下划线样式：修订插入/删除标记可覆盖字符自身的下划线设置。
            let underline = match (ins_mark, del_mark) {
                (Some(InsertMark::Underline), _) | (_, Some(DeleteMark::Underline)) => Underline::Single,
                (Some(InsertMark::DoubleUnderline), _) => Underline::Double,
                _ => rc.underline,
            };
            // 双删除线/删除线：字符删除线、双删除效果、或修订插入/删除标记触发。
            let double = rc.double_strike || del_mark == Some(DeleteMark::DoubleStrikethrough);
            let struck = rc.strike || double || ins_mark == Some(InsertMark::Strikethrough) || del_mark == Some(DeleteMark::Strikethrough);
            let spans = if underline != Underline::None || struck { line.spans(run_start, end_k) } else { Vec::new() };
            for (x0, x1) in spans.into_iter().map(|(a, b)| (x + a, x + b)).filter(|(a, b)| b > a) {
                if underline != Underline::None {
                    // 下划线位置：基线下方 0.12 字号处；颜色可单独指定。
                    let uy = base - st.shift + st.size * 0.12;
                    let ucolor = rc.underline_color.unwrap_or(color);
                    if underline == Underline::Words {
                        // “仅字下划线”：逐词画（跳过空格）；属于其它视觉区间的词跳过。
                        for kk in run_start..end_k {
                            let Some(c) = pl.clusters.get(kk) else { continue };
                            if c.kind != ClKind::Text {
                                continue;
                            }
                            let a = x + line.cl_left(kk).unwrap_or(0.0);
                            let b = x + line.cl_right(kk).unwrap_or(a - x);
                            if a < x0 - 0.01 || b > x1 + 0.01 {
                                continue; // drawn with its own span
                            }
                            out.push(Draw::Line { x0: a, y0: uy, x1: b, y1: uy, width: thick, color: ucolor, stroke: Stroke::Solid, alpha });
                        }
                    } else {
                        // 整段一条下划线（Thick 加粗一倍；样式映射到对应线型）。
                        let w = if underline == Underline::Thick { thick * 2.0 } else { thick };
                        out.push(Draw::Line { x0, y0: uy, x1, y1: uy, width: w, color: ucolor, stroke: stroke_of(underline), alpha });
                    }
                }
                if struck {
                    // 删除线：基线上方 0.28 字号处，双删除用 Double 线型。
                    let sy = base - st.shift - st.size * 0.28;
                    let stroke = if double { Stroke::Double } else { Stroke::Solid };
                    out.push(Draw::Line { x0, y0: sy, x1, y1: sy, width: thick, color, stroke, alpha });
                }
            }
        }
        // Proofing squiggles.
        // 拼写/语法校对波浪线：拼写为红色波浪线，语法为蓝色双线；与行文字区间求交后逐段画。
        for (a, b, grammar) in &pl.issues {
            if *b <= line.start || *a >= line.stop {
                continue;
            }
            for (x0, x1) in pl.x_spans(li, (*a).max(line.start), (*b).min(line.stop)) {
                if x1 - x0 < 1.0 {
                    continue;
                }
                let y = base + 2.5;
                // 语法用蓝色 #2B57C0 双线，拼写用红色 #E02424 波浪线（与 Word 的默认色一致）。
                let color = if *grammar { Rgb(0x2B, 0x57, 0xC0) } else { Rgb(0xE0, 0x24, 0x24) };
                out.push(Draw::Line {
                    x0: x + x0,
                    y0: y,
                    x1: x + x1,
                    y1: y,
                    width: 0.8,
                    color,
                    stroke: if *grammar { Stroke::Double } else { Stroke::Wave },
                    alpha,
                });
            }
        }
        // Tab leaders.
        // 制表符前导符（点线/横线等）：从制表位起点向后重复铺前导字符，直到制表位终点。
        for (k, leader) in &line.leaders {
            let (Some(c), Some(a), Some(b)) = (pl.clusters.get(*k), line.cl_left(*k), line.cl_right(*k)) else { continue };
            let Some(st) = pl.styles.get(c.style as usize) else { continue };
            let Some(ch) = leader.char() else { continue };
            let gid = st.face.glyph_for(ch);
            let adv = (st.face.advance(gid) as f32 * st.size / st.face.upem.max(1.0) as f32).max(1.0);
            let (mut gx, end) = (x + a + adv * 0.5, x + b - adv * 1.2);
            let mut glyphs = Vec::new();
            // Align leader dots to a grid so consecutive lines line up.
            // 前导字符按步长网格对齐：相邻行的前导点才能上下对齐；上限 2000 个防失控。
            gx = (gx / adv).ceil() * adv;
            while gx < end && glyphs.len() < 2000 {
                glyphs.push((gid, gx, base));
                gx += adv;
            }
            out.push(Draw::Glyphs {
                face: st.face,
                size: st.size,
                glyphs,
                color: text_color(&st.rc.color, None),
                alpha,
                synth_bold: false,
                synth_italic: false,
                text: String::new(),
                link: None,
                ranges: Vec::new(),
            });
        }
        // Inline objects.
        // 行内对象：图片/形状/组合 → object_draws（带自身旋转）；图表 → 打包成 Figure；
        // 公式 → 按 MItem 树转指令（占位符仅屏幕显示）。
        for k in line.c0..line.c1 {
            let Some(c) = pl.clusters.get(k) else { continue };
            let ClKind::Object(oi) = c.kind else { continue };
            if c.obj_h <= 0.0 {
                continue;
            }
            let cx = x + line.cl_left(k).unwrap_or(0.0);
            let obj = para.and_then(|p| p.objects.get(oi));
            let rect = inline_rect(obj, cx, base, c.adv, c.obj_h);
            let spin = obj.and_then(InlineObject::frame).map(|(_, _, f)| f.spin()).unwrap_or_default();
            match obj {
                Some(o @ (InlineObject::Image { .. } | InlineObject::Shape { .. } | InlineObject::Group { .. })) => {
                    object_draws(o, rect, Spin::default(), alpha, opts, out)
                }
                Some(InlineObject::Graphic { graphic, alt, .. }) => {
                    let mut draws = Vec::new();
                    spun(spin, rect, graphic_draws(doc, graphic, rect, alpha), &mut draws);
                    out.push(Draw::Figure { alt: alt.clone(), kind: graphic.kind, draws })
                }
                Some(InlineObject::Equation { .. }) => {
                    if let Some((_, ml)) = pl.maths.iter().find(|(k, _)| *k == oi) {
                        equation(&ml.items, cx, base, alpha, opts.placeholders, out);
                    }
                }
                _ => {}
            }
        }
        // Formatting marks.
        // 格式标记（仅屏幕）：空格 · 、制表 →/←、软换行 ↵、分页 ⤓/分栏 ⇥（带虚线延伸到行尾）、
        // 段落标记 ¶（或分节符标签）。
        if opts.marks {
            // 标记字号：默认取段落第一个样式的字号。
            let msize = pl.styles.first().map(|s| s.size).unwrap_or(11.0);
            for k in line.c0..line.c1 {
                let Some(c) = pl.clusters.get(k) else { continue };
                let cx = x + line.cl_left(k).unwrap_or(0.0);
                let nx = x + line.cl_right(k).unwrap_or(cx - x);
                let size = pl.styles.get(c.style as usize).map(|s| s.size).unwrap_or(msize);
                // 制表符箭头方向跟随行走向（RTL 行向左）。
                let arrow = if line.rtl { '←' } else { '→' };
                match c.kind {
                    ClKind::Space => {
                        out.push(Draw::Mark { x: (cx + nx) / 2.0 - size * 0.12, baseline: base - size * 0.08, size, ch: '·', color: None })
                    }
                    ClKind::Tab => {
                        out.push(Draw::Mark { x: cx + ((nx - cx) / 2.0 - size * 0.3).max(0.0), baseline: base, size, ch: arrow, color: None })
                    }
                    ClKind::LineBreak => out.push(Draw::Mark { x: cx + 1.0, baseline: base, size, ch: '↵', color: None }),
                    ClKind::PageBreak | ClKind::ColumnBreak => {
                        // 分页/分栏标记 ⤓/⇥：附一条点虚线延伸到行尾，表示“此处之后换页/栏”。
                        let label = if c.kind == ClKind::PageBreak { '⤓' } else { '⇥' };
                        out.push(Draw::Line {
                            x0: cx + 2.0,
                            y0: base - size * 0.3,
                            x1: x + line.right,
                            y1: base - size * 0.3,
                            width: 0.5,
                            color: Rgb(0x60, 0x60, 0x60),
                            stroke: Stroke::Dotted,
                            alpha,
                        });
                        out.push(Draw::Mark { x: cx + 1.0, baseline: base, size, ch: label, color: None });
                    }
                    _ => {}
                }
            }
            if line.end == LineEnd::Para {
                // The mark follows the line's visual end, past every glyph, even where the
                // logically last text runs against the paragraph's direction.
                // 段落标记 ¶ 画在行的“视觉末端”：紧随最后一个字形之后，即使逻辑上最后的文字
                // 与段落方向相逆（由 visual_end_x 换算）。
                let ex = x + line.visual_end_x();
                let size = pl.lines.first().map(|_| msize).unwrap_or(msize);
                // A right-to-left paragraph's mark sits at its end, on the left.
                // 从右向左的段落其“末端”在左侧：标记要向左偏出。
                let mx = if line.rtl { ex - 1.0 - size * 0.6 } else { ex + 1.0 };
                // A tracked (inserted or deleted) paragraph mark is drawn in its author's colour.
                // 段落标记本身被修订（插入/删除）时按修订者颜色绘制（删除优先，同文字规则）。
                let m = &opts.revisions;
                let color = para.filter(|_| opts.markup && m.insertions_deletions).and_then(|p| match (p.mark.ins, p.mark.del) {
                    (_, Some(r)) => Some(
                        m.delete_color
                            .unwrap_or_else(|| revision_color(doc.revisions.get(r as usize).map(|v| author_index(doc, &v.author)).unwrap_or(0))),
                    ),
                    (Some(r), None) => Some(
                        m.insert_color
                            .unwrap_or_else(|| revision_color(doc.revisions.get(r as usize).map(|v| author_index(doc, &v.author)).unwrap_or(0))),
                    ),
                    _ => None,
                });
                // The last paragraph of a section ends in a section break instead of a plain ¶.
                // 分节符：节的最后一个段落以分节符标签（虚线双线夹节名）代替普通 ¶。
                if let Some(start) = section_break_after(doc, story, path) {
                    let (x0, x1) = if line.rtl { (x + line.left, ex - 2.0) } else { (ex + 2.0, x + line.right) };
                    section_break_mark(start, x0, x1, base, size, alpha, out);
                } else {
                    out.push(Draw::Mark { x: mx, baseline: base, size, ch: '¶', color });
                }
            }
        }
        // bottom 已在上文字符边框绘制处使用；此处显式取用疑为消除未使用告警的冗余写法（保持原样）。
        let _ = bottom;
    }
}

/// The kind of section break that ends at body paragraph `path`: the start type of the section
/// that follows it. `None` for any other paragraph, and for the document's last section.
/// 求正文段落 `path` 末尾的分节符类型 = 下一节的起始类型（NextPage/Continuous/…）。
/// 仅对带分节属性的正文顶层段落有效；文档最后一节之后没有分节符，返回 None。
fn section_break_after(doc: &Document, story: StoryRef, path: &Path) -> Option<SectionStart> {
    if story != StoryRef::Body || path.0.len() != 1 {
        return None;
    }
    let i = *path.0.first()? as usize;
    match doc.body.get(i).map(|b| &**b) {
        Some(Block::Para(p)) if p.section.is_some() => {}
        _ => return None,
    }
    let secs = doc.sections();
    let k = secs.iter().position(|(end, _)| *end == i)?;
    secs.get(k + 1).map(|(_, s)| s.start)
}

/// On-screen name of a section break.
/// 分节符的屏幕显示名称：按起始类型返回英文界面文案（仅供格式标记视图显示）。
pub fn section_break_label(start: SectionStart) -> &'static str {
    match start {
        SectionStart::NextPage => "Section Break (Next Page)",
        SectionStart::Continuous => "Section Break (Continuous)",
        SectionStart::EvenPage => "Section Break (Even Page)",
        SectionStart::OddPage => "Section Break (Odd Page)",
        SectionStart::NextColumn => "Section Break (Next Column)",
    }
}

/// Width of a [`Draw::MarkText`] label.
/// [`Draw::MarkText`] 标签文本的排版宽度：标记文本固定用 Source Sans 3 常规体
/// （与 [`Draw::Mark`] 同字体），逐字符取步进宽并换算到目标字号后求和。
pub fn mark_text_width(text: &str, size: f32) -> f32 {
    // 标记文本专用字体：Source Sans 3 常规体。
    let face = wordcraft_fonts::word::resolve("Source Sans 3", false, false).face;
    // 字号 → 字体单位（upem）的缩放系数。
    let k = size / face.upem.max(1.0) as f32;
    text.chars().map(|c| face.advance(face.glyph_for(c)) as f32 * k).sum()
}

/// A section break mark between `x0` and `x1`: a dotted double rule with the break's name centred
/// in it (just the name when there is no room for the rule).
/// 分节符标记：在 [x0, x1] 之间画上下两条点线（双横线），节名居中夹在线间；
/// 空间放不下“节名 + 四段留白”时只画节名。
fn section_break_mark(start: SectionStart, x0: f32, x1: f32, base: f32, size: f32, alpha: f32, out: &mut Vec<Draw>) {
    let text = section_break_label(start);
    // 标签字号取标记字号的 0.8 倍，夹到 [6, 11] 磅；标签宽与线端留白按标签字号推算。
    let lsize = (size * 0.8).clamp(6.0, 11.0);
    let w = mark_text_width(text, lsize);
    let gap = lsize * 0.4;
    // 双横线的中线位置：略高于基线。
    let mid = base - size * 0.3;
    let color = Rgb(0x60, 0x60, 0x60);
    // 剩余空间不足：只画节名，不画双横线。
    if x1 - x0 <= w + 4.0 * gap {
        out.push(Draw::MarkText { x: x0, baseline: base, size: lsize, text: text.into() });
        return;
    }
    // 节名水平居中；其左右两段各画上下两条点线（mid ± 1.2 磅）构成双横线。
    let tx = (x0 + x1 - w) / 2.0;
    for (a, b) in [(x0, tx - gap), (tx + w + gap, x1)] {
        for y in [mid - 1.2, mid + 1.2] {
            out.push(Draw::Line { x0: a, y0: y, x1: b, y1: y, width: 0.5, color, stroke: Stroke::Dotted, alpha });
        }
    }
    // 节名画在双线之间（基线略低于中线，使文字视觉居中）。
    out.push(Draw::MarkText { x: tx, baseline: mid + lsize * 0.33, size: lsize, text: text.into() });
}

/// An equation's glyphs and rules with its origin at (`x`, `base`).
/// 公式项树 → 绘制指令，原点在 (`x`, `base`)（基线）。数学排版坐标 y 向上，转页面坐标时
/// 字形/矩形/线段的 y 一律按 base 翻转；`screen` 为 false（打印/PDF）时不画仅屏幕项。
fn equation(items: &[MItem], x: f32, base: f32, alpha: f32, screen: bool, out: &mut Vec<Draw>) {
    for it in items {
        match it {
            // 字形串：局部坐标 (gx, gy) 平移并翻转后输出；公式文本没有逐字形字节区间，
            // ranges 置空，由 PDF 写出端按字体自行做文本映射。
            MItem::Glyphs { face, size, color, synth_bold, synth_italic, glyphs, text } => out.push(Draw::Glyphs {
                face: *face,
                size: *size,
                glyphs: glyphs.iter().map(|(g, gx, gy)| (*g, x + gx, base - gy)).collect(),
                color: *color,
                alpha,
                synth_bold: *synth_bold,
                synth_italic: *synth_italic,
                text: text.clone(),
                link: None,
                // Equation text maps back through the font (no per-glyph ranges from the math layout).
                ranges: Vec::new(),
            }),
            // 矩形（分数线、根号杆等规则块）：y 翻转后取 base − y − h 为顶、h 为高。
            MItem::Rect { x: rx, y, w, h, color } => out.push(Draw::Fill { rect: Rect::new(x + rx, base - y - h, *w, *h), color: *color, alpha }),
            // 线段：dotted 时用点线（如可延伸的根号顶线）。
            MItem::Line { x0, y0, x1, y1, width, color, dotted } => out.push(Draw::Line {
                x0: x + x0,
                y0: base - y0,
                x1: x + x1,
                y1: base - y1,
                width: *width,
                color: *color,
                stroke: if *dotted { Stroke::Dotted } else { Stroke::Solid },
                alpha,
            }),
            // 仅屏幕可见项（如占位符提示）：screen 开启时递归展开绘制；打印/PDF 永不绘制。
            MItem::ScreenOnly(inner) => {
                if screen {
                    equation(std::slice::from_ref(inner), x, base, alpha, screen, out);
                }
            }
        }
    }
}

/// The draws of a chart or diagram's items inside `rect` (page coordinates). The items were built
/// for the size `g.w` × `g.h` (0: `rect`'s own size), so they are scaled to `rect`; strokes and text
/// by the mean of the two scales.
/// 图表/示意图各项 → 绘制指令：项按原始尺寸 `g.w`×`g.h` 构建（0 表示与 rect 同尺寸，
/// 不缩放），绘制时整体缩放到 `rect`；线宽与文字字号按 x、y 两个缩放系数的均值缩放。
fn graphic_draws(doc: &Document, g: &Graphic, rect: Rect, alpha: f32) -> Vec<Draw> {
    // x、y 方向各自的缩放系数；k 为综合系数（线宽/文字用均值）。
    let (sx, sy) = (scale_of(rect.w, g.w), scale_of(rect.h, g.h));
    let k = (sx + sy) / 2.0;
    // 把项内矩形 [x, y, w, h] 从图表坐标系映射到页面矩形。
    let inside = |r: &[f32; 4]| Rect::new(rect.x + r[0] * sx, rect.y + r[1] * sy, r[2] * sx, r[3] * sy);
    let mut out = Vec::with_capacity(g.items.len());
    for it in &g.items {
        match it {
            // 预设形状：矩形映射后直接输出（图表内部不挂阴影/发光效果）。
            GraphicItem::Shape { rect: r, kind, fill, stroke, stroke_width } => out.push(Draw::Shape {
                rect: inside(r),
                kind: *kind,
                fill: *fill,
                stroke: *stroke,
                stroke_width: stroke_width * k,
                effects: Default::default(),
            }),
            // 矢量路径：各段坐标逐点映射到页面坐标。
            GraphicItem::Path { segs, fill, stroke, stroke_width } => {
                let segs = page_segs(segs, |x, y| (rect.x + x * sx, rect.y + y * sy));
                out.push(Draw::Path { segs, fill: *fill, stroke: *stroke, stroke_width: stroke_width * k })
            }
            // 文本行：经 text_glyphs 在映射后的矩形内排版成一条字形串指令。
            GraphicItem::Text { rect: r, .. } => out.extend(text_glyphs(doc, inside(r), it, alpha, k)),
            // 位图：图表内图片不裁剪（crop 全 0）。
            GraphicItem::Image { rect: r, media } => out.push(Draw::Image { rect: inside(r), media: media.clone(), crop: [0.0; 4], alpha }),
        }
    }
    out
}

/// Scale from the size `built` points a graphic's items were made for to `len` points; 1 when
/// the built size is unknown (0).
/// 构建尺寸 `built` → 目标尺寸 `len` 的缩放系数；构建尺寸未知（0）或任一值非法时返回 1（不缩放）。
fn scale_of(len: f32, built: f32) -> f32 {
    if built.is_finite() && built > 0.0 && len.is_finite() { (len / built).max(0.0) } else { 1.0 }
}

/// Path segments mapped to page coordinates by `map`, cleaned for the rasterizer and PDF: a segment
/// with a non-finite number is dropped, and so is anything before a move (or after a broken one).
/// 路径段经 `map` 映射到页面坐标并清洗（供光栅渲染器与 PDF）：含非有限坐标的段丢弃；
/// Move 之前（或某段被弃导致子路径断裂之后）的 Line/Cubic/Close 一并丢弃，保证输出总是合法路径。
fn page_segs(segs: &[PathSeg], map: impl Fn(f32, f32) -> (f32, f32)) -> Vec<PathSeg> {
    // fin：坐标对是否全部有限。
    let fin = |p: (f32, f32)| p.0.is_finite() && p.1.is_finite();
    let mut out = Vec::with_capacity(segs.len());
    // open：当前是否处于一条已由 Move 开启、未断裂的子路径中。
    let mut open = false;
    for s in segs {
        match *s {
            // Move：开启新子路径；坐标非法则不开启（后续段随之丢弃）。
            PathSeg::Move(x, y) => {
                let p = map(x, y);
                open = fin(p);
                if open {
                    out.push(PathSeg::Move(p.0, p.1));
                }
            }
            // Line：仅在开放子路径中且坐标有限时保留。
            PathSeg::Line(x, y) => {
                let p = map(x, y);
                if open && fin(p) {
                    out.push(PathSeg::Line(p.0, p.1));
                }
            }
            // Cubic（三次贝塞尔）：三点坐标都有限才保留。
            PathSeg::Cubic(a, b, c, d, e, f) => {
                let (p1, p2, p3) = (map(a, b), map(c, d), map(e, f));
                if open && fin(p1) && fin(p2) && fin(p3) {
                    out.push(PathSeg::Cubic(p1.0, p1.1, p2.0, p2.1, p3.0, p3.1));
                }
            }
            // Close：闭合当前子路径；此后回到未开启状态，直到下一个 Move。
            PathSeg::Close => {
                if open {
                    out.push(PathSeg::Close);
                }
                open = false;
            }
        }
    }
    out
}

/// A path of clean segments (see [`page_segs`]) as a kurbo path, for the rasteriser and PDF.
/// The draws of a shape at `rect`: a preset outline, or a freeform's paths (an ink stroke left out
/// under Review › Hide Ink).
/// 形状 → 绘制指令：预设形状输出一条 Shape 指令；自由形状（freeform）按其路径逐条输出——
/// 墨迹笔画输出为 Ink（“审阅 › 隐藏墨迹”开启时整体省略），普通路径输出为 Path。
/// （注：上方第一行英文文档疑为 [`seg_path`] 的说明误置于此，照原样保留。）
#[allow(clippy::too_many_arguments)]
fn shape_draws(
    rect: Rect,
    kind: ShapeKind,
    fill: Option<Rgb>,
    stroke: Option<Rgb>,
    stroke_width: f32,
    effects: wordcraft_doc::effects::ShapeEffects,
    freeform: Option<&wordcraft_doc::freeform::Freeform>,
    opts: &DisplayOptions,
    out: &mut Vec<Draw>,
) {
    // 非自由形状（或没有自由路径数据）：直接一条预设形状指令。
    let Some(f) = freeform.filter(|_| kind == ShapeKind::Freeform) else {
        out.push(Draw::Shape { rect, kind, fill, stroke, stroke_width, effects });
        return;
    };
    // 墨迹笔画且“审阅 › 隐藏墨迹”开启：整体不画（文档中仍保留该墨迹）。
    if f.is_ink() && opts.hide_ink {
        return;
    }
    // 线宽清洗：非有限值按 0.75 磅，合法值夹到 [0, 200] 磅。
    let width = if stroke_width.is_finite() { stroke_width.clamp(0.0, 200.0) } else { 0.75 };
    // 遍历自由形状按 rect 缩放、平移后的各条路径（pts 点列；closed 是否闭合）。
    for (pts, closed) in f.placed(rect.x, rect.y, rect.w, rect.h) {
        // 墨迹笔画，或无填充的开放路径：按折线描边输出为 Ink。
        if f.is_ink() || (!closed && fill.is_none()) {
            // 有描边色才画；闭合路径补上首点让折线首尾相接。不透明度取墨迹自带 alpha。
            if let Some(color) = stroke {
                let mut pts: Vec<(f32, f32)> = pts.iter().map(|[x, y]| (*x, *y)).collect();
                if closed && let Some(first) = pts.first().copied() {
                    pts.push(first);
                }
                out.push(Draw::Ink { pts, color, width: width.max(0.25), alpha: f.alpha });
            }
            continue;
        }
        // 普通路径：转成路径段（首点 Move、其余 Line），闭合路径再加 Close。
        let mut segs: Vec<PathSeg> = Vec::with_capacity(pts.len() + 1);
        for (i, [x, y]) in pts.iter().enumerate() {
            segs.push(if i == 0 { PathSeg::Move(*x, *y) } else { PathSeg::Line(*x, *y) });
        }
        if closed {
            segs.push(PathSeg::Close);
        }
        // 闭合路径才填充；开放路径只描边。
        out.push(Draw::Path { segs, fill: if closed { fill } else { None }, stroke, stroke_width: width });
    }
}

/// 路径段 → kurbo 贝塞尔路径（供光栅渲染器与 PDF 写出端把 Draw::Path 落到画布）。
/// 一比一转换，不做有限性清洗（清洗已在 [`page_segs`] 完成）。
pub fn seg_path(segs: &[PathSeg]) -> BezPath {
    let mut p = BezPath::new();
    for s in segs {
        match *s {
            PathSeg::Move(x, y) => p.move_to((x as f64, y as f64)),
            PathSeg::Line(x, y) => p.line_to((x as f64, y as f64)),
            PathSeg::Cubic(a, b, c, d, e, f) => p.curve_to((a as f64, b as f64), (c as f64, d as f64), (e as f64, f as f64)),
            PathSeg::Close => p.close_path(),
        }
    }
    p
}

/// One line of a `GraphicItem::Text` in `r`, shaped in its font (the document's body font when
/// unset) and aligned horizontally, centred vertically, at its size times `k`. `None` when there is
/// nothing to draw, or the item's rectangle or size isn't a finite number (a degenerate scale:
/// `r` itself is always finite, as [`Rect::new`] makes it).
/// 图表文本项 → 单条字形串指令：在矩形 `r` 内水平按 `align` 对齐、垂直居中，字号乘整体
/// 缩放系数 `k`；矩形或字号非有限（退化缩放）、整形结果为空时返回 None。
fn text_glyphs(doc: &Document, r: Rect, it: &GraphicItem, alpha: f32, k: f32) -> Option<Draw> {
    // 只处理 Text 项；字号先乘整体缩放系数。
    let GraphicItem::Text { rect, text, size, color, bold, align, font } = it else { return None };
    let size = *size * k;
    // 矩形四值与字号任一非有限：放弃绘制。
    if !rect.iter().chain([&size]).all(|v| v.is_finite()) {
        return None;
    }
    // 字体族：图表项自带优先，否则回退文档正文字体；字号夹到 [1, 400] 磅。
    let family = font.as_deref().filter(|f| !f.is_empty()).unwrap_or(&doc.settings.minor_font);
    let size = size.clamp(1.0, 400.0);
    // 解析字体（含合成粗体判定）后整段文字整形；整形为空则无可画。
    let resolved = wordcraft_fonts::word::resolve(family, *bold, false);
    let face = resolved.face.get();
    let shaped = wordcraft_fonts::shape(face, text, &[], |c| c);
    if shaped.is_empty() {
        return None;
    }
    // 字号 → 字体单位（upem）缩放系数；总宽按各字形步进求和。
    let k = size / face.upem.max(1.0) as f32;
    let width: f32 = shaped.iter().map(|g| g.x_advance as f32 * k).sum();
    // 垂直居中：矩形中线加半行高（上伸减下伸的一半）得基线。
    let (ascent, descent) = wordcraft_fonts::word::line_metrics(face);
    let baseline = r.y + r.h / 2.0 + (ascent - descent) as f32 * k / 2.0;
    // 水平对齐起点：左对齐取左缘、居中取剩余空间一半、右对齐从右缘退回总宽。
    let mut pen = match align {
        TextAlign::Left => r.x,
        TextAlign::Center => r.x + (r.w - width) / 2.0,
        TextAlign::Right => r.x + r.w - width,
    };
    // 逐字形落位（含偏移量），画笔按步进前进。
    let mut glyphs = Vec::with_capacity(shaped.len());
    for g in &shaped {
        glyphs.push((g.gid, pen + g.x_offset as f32 * k, baseline - g.y_offset as f32 * k));
        pen += g.x_advance as f32 * k;
    }
    Some(Draw::Glyphs {
        face: resolved.face,
        size,
        glyphs,
        color: *color,
        alpha,
        synth_bold: resolved.synth_bold,
        synth_italic: false,
        text: text.clone(),
        link: None,
        ranges: Vec::new(),
    })
}

/// 修订作者的调色板序号：按作者在修订记录中首次出现的顺序编号（0 起），
/// 供 [`revision_color`] 取该作者的修订颜色；查无此人按 0（取第一种颜色）。
fn author_index(doc: &Document, author: &str) -> u32 {
    // 一遍扫描按出现顺序收集去重后的作者名单。
    let mut seen: Vec<&str> = Vec::new();
    for r in &doc.revisions {
        if !seen.contains(&r.author.as_str()) {
            seen.push(&r.author);
        }
    }
    // 目标作者在名单中的位次即其序号；不在名单按 0。
    seen.iter().position(|a| *a == author).unwrap_or(0) as u32
}

/// Automatic colour is black, or white on a dark background.
/// 文字颜色：指定色原样返回；Auto 时按背景亮度自适应——背景亮度 < 100（深色，通常是
/// 字符底纹或高亮色）用白字，否则黑字。
pub fn text_color(c: &TextColor, background: Option<Rgb>) -> Rgb {
    match c {
        TextColor::Rgb(r) => *r,
        TextColor::Auto => {
            if background.is_some_and(|b| b.luma() < 100.0) {
                Rgb::WHITE
            } else {
                Rgb::BLACK
            }
        }
    }
}

/// `items` turned by `spin` about the centre of `rect` (as they are, unturned).
/// 子指令的旋转打包：`spin` 为恒等时直接并入输出；否则包一层 [`Draw::Rotated`]，
/// 绕 `rect` 中心旋转。
fn spun(spin: Spin, rect: Rect, items: Vec<Draw>, out: &mut Vec<Draw>) {
    if spin.is_identity() {
        out.extend(items);
    } else {
        out.push(Draw::Rotated { cx: rect.x + rect.w / 2.0, cy: rect.y + rect.h / 2.0, spin, items });
    }
}

/// A picture, shape or group drawn in `rect` (its unturned frame), turned by its own spin about
/// the rect's centre; a group's members are turned inside it. `outer` is the spin it is already
/// drawn inside (its group's). Nothing for anything else.
/// 行内对象递归绘制：对象在自身（未旋转）框架 `rect` 内绘制，再按自带旋转绕中心旋转；
/// 组合（Group）对其成员逐个递归（成员可再嵌组合，旋转逐层嵌套合成）。
/// `outer` 是它已处的外层旋转（所属组合的）。其余对象类型不产生绘制指令。
fn object_draws(o: &InlineObject, rect: Rect, outer: Spin, alpha: f32, opts: &DisplayOptions, out: &mut Vec<Draw>) {
    // 对象自带旋转（框架里的 spin），未声明时为恒等。
    let own = o.frame().map(|(_, _, f)| f.spin()).unwrap_or_default();
    match o {
        // 图片：单条 Image 指令按自带旋转打包。
        InlineObject::Image { media, crop, .. } => spun(own, rect, vec![Draw::Image { rect, media: media.clone(), crop: *crop, alpha }], out),
        // 形状：效果先按“自带旋转叠加外层旋转”修正，再转形状指令打包。
        InlineObject::Shape { kind, fill, stroke, stroke_width, effects, freeform, .. } => {
            let effects = effects_in(*effects, own.within(outer));
            let mut items = Vec::new();
            shape_draws(rect, *kind, *fill, *stroke, *stroke_width, effects, freeform.as_deref(), opts, &mut items);
            spun(own, rect, items, out)
        }
        // 组合：按组合布局把成员摆到各自子矩形，逐成员递归展开（传入合成后的外层旋转），
        // 成员指令整体打包进本组合的旋转。
        InlineObject::Group { .. } => {
            let mut members = Vec::new();
            for ([x, y, w, h], c) in o.group_rects(rect.x, rect.y, rect.w, rect.h) {
                object_draws(c, Rect::new(x, y, w, h), own.within(outer), alpha, opts, &mut members);
            }
            spun(own, rect, members, out)
        }
        _ => {}
    }
}

/// A shape's `effects` as drawn turned by `spin` (all the turns it is drawn inside): glows and
/// soft edges turn with the shape, and so does a shadow's offset when it rotates with the shape
/// (`rotWithShape`); one that doesn't keeps its direction on the page, so its angle is turned back.
/// 旋转坐标系下的形状效果修正：发光/柔边随形状旋转，无需处理；随形状旋转（rotWithShape）
/// 的阴影偏移同样无需处理；不随形状旋转的阴影要保持页面方向，角度须经旋转逆变换转回页面系。
pub(crate) fn effects_in(effects: wordcraft_doc::effects::ShapeEffects, spin: Spin) -> wordcraft_doc::effects::ShapeEffects {
    let mut e = effects;
    // 仅当阴影不随形状旋转、且外层确有旋转时才需要修正角度。
    if let Some(s) = e.shadow.as_mut().filter(|s| !s.rot_with_shape && !spin.is_identity()) {
        // 把阴影方向单位向量旋回页面坐标，再反推页面系角度并规范化到 [0, 360)。
        let a = s.angle.to_radians();
        let (dx, dy) = spin.unapply(0.0, 0.0, a.cos(), a.sin());
        s.angle = wordcraft_geom::normalize_degrees(dy.atan2(dx).to_degrees());
    }
    e
}

/// Where inline object `obj` is drawn, given its cluster's box (`adv` × `obj_h` standing on the
/// baseline at `cx`): inside the room kept for its effects.
/// A rotated one's unrotated frame, centred in that room.
/// 求行内对象的绘制矩形：以簇盒（立于 `cx` 处基线上的 `adv`×`obj_h`）扣除四周效果外扩
/// （effect_extent，阴影/发光预留）得到可用空间 room；带旋转的对象返回其“未旋转”框——
/// 按旋转后外接界的比例缩回并居中于 room，使旋转后的外接界恰好落在 room 内。
pub(crate) fn inline_rect(obj: Option<&InlineObject>, cx: f32, base: f32, adv: f32, obj_h: f32) -> Rect {
    // 对象框架（尺寸与效果）；无框架时效果外扩全 0。
    let frame = obj.and_then(InlineObject::frame);
    // room = 簇盒减去四周效果外扩后的可用空间。
    let [l, t, r, b] = frame.map_or([0.0; 4], |(_, _, float)| float.effect_extent());
    let room = Rect::new(cx + l, base - obj_h + t, (adv - l - r).max(0.0), (obj_h - t - b).max(0.0));
    // 无旋转：直接使用 room。
    let Some((w, h, float)) = frame.filter(|(_, _, f)| f.rot != 0.0) else { return room };
    // The room is the rotated bounds of the (possibly scaled-down) frame: undo the rotation.
    // 框架尺寸清洗后求旋转外接宽，按 room.w/外接宽 的比例把原始框架缩回，再在 room 内居中。
    let (w, h) = (wordcraft_geom::finite(w).clamp(1.0, 4000.0), wordcraft_geom::finite(h).clamp(1.0, 4000.0));
    let (bw, _) = float.spin().extent(w, h);
    let k = if bw > 0.0 { room.w / bw } else { 1.0 };
    let (fw, fh) = (w * k, h * k);
    Rect::new(room.x + (room.w - fw) / 2.0, room.y + (room.h - fh) / 2.0, fw, fh)
}
