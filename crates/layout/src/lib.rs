//! WordCraft layout: turns a [`Document`] into pages.
//!
//! - [`para`]: one paragraph → lines (shaping, first-fit line breaking, tabs, list labels,
//!   alignment). Results are memoised per paragraph revision in a [`LayoutCache`].
//! - [`layout`]: sections → pages and columns; tables; keep/widow rules; page breaks; headers
//!   and footers; page-number fields.
//! - [`display`]: a page → draw items (glyph runs, rules, fills, images) for the renderers.
//! - [`hit`]: point ↔ position, caret geometry, line navigation, selection rectangles.
//!
//! # 中文标注：业务流程总述与子模块关系
//!
//! 坐标系约定：页面坐标，原点在页面左上角，单位为点（1/72 英寸），y 轴向下。
//!
//! 总体业务流程（总入口为下方 [`layout()`]，函数处另有详细标注）：
//! 1. 计算环境指纹 env_hash（样式表/编号定义/默认制表位/脚注格式 + 视图相关开关），变化则整体清空段落缓存；
//! 2. `note_numbers` 按阅读顺序预分配脚注/尾注编号（脚注、尾注各自计数），并生成 notes_hash 供缓存键使用；
//! 3. 逐节驱动 [`PageBuilder`] 走"节 → 页/栏"分页主循环：段落经 `place_para`、表格经 `place_table`
//!    依次落页落栏，期间处理浮动对象绕排、keep-with-next/孤行寡行控制、脚注压入页底、行号、
//!    强制分页/分栏符、跨页表格与表头重复；
//! 4. 正文排完后：尾注追加到最后一页 → `apply_valign` 节的垂直对齐 → 脚注延续页（carry/flush_notes）；
//! 5. `headers_footers` 为每页补排页眉页脚（仅打印视图）；按"本轮是否用到"淘汰段落缓存（上限 4×used）；
//! 6. 建立 (story, 段落路径) → [(页号, 条目号)] 的 `index` 反查索引，供 hit 模块与编辑器定位。
//!
//! 子模块关系：
//! - `para`：单段落 → 行（整形、首次适配断行、制表位、列表编号、对齐），被本文件的
//!   `Ctx::para_labelled`/`layout_box`/`next_first_line` 调用；结果按段落 rev+环境记忆化进 [`LayoutCache`]。
//! - `table`：表格测量、逐行/逐格布局与跨页拆行（`layout_table`/`split_row`），被 `layout_box` 与 `place_table` 调用。
//! - `display`：把页上的 [`Placed`] "版面事实"翻译成 `display::Draw` 绘制指令（字形串/线/填充/图片），供渲染器消费。
//! - `hit`：命中测试与光标几何（点↔文档位置、选区矩形、行导航），消费 [`Page`] 与 [`DocLayout::index`][DocLayout]。
//! - `fields`：域取值上下文 [`FieldCtx`]（页码/总页数/节号/脚注编号表），Ctx 分页时持续更新，para 排域时读取。
//! - `kinsoku`：中日韩禁则与拉丁词任意断行规则，由 para 断行时调用（`crate::kinsoku::apply`）。
//! - `math`：公式排版（`MathLayout`），para 内嵌公式时调用；公式自动编号由本文件 `Ctx::eq_count` 顺序驱动。
//!
//! 上方 `#![deny(...)]` 是健壮性门禁：全模块禁用 unwrap/expect/panic/todo! 等失败性写法，强制防御式编程。
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod display;
pub mod fields;
pub mod hit;
pub mod kinsoku;
pub mod math;
pub mod para;
mod table;

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use wordcraft_doc::numbering::{Counters, Level};
use wordcraft_doc::para::{Anchor, Float, FloatAlign, InlineObject, Wrap};
use wordcraft_doc::props::{Border, CharProps, Rgb, TableFloat, TextDirection};
use wordcraft_doc::section::{SectionProps, SectionStart};
use wordcraft_doc::{Block, Blocks, Document, Paragraph, Path, StoryRef};
use wordcraft_geom::{Point, Rect, Spin};

pub use fields::FieldCtx;
pub use hit::VisualStep;
pub use para::{LineEnd, ParaLayout};
pub use table::{autofit_widths, measure_table_columns};

/// How the document is viewed.
/// 视图模式：决定分页策略——打印视图真实分页；Web/草稿视图排成"一张长页"。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ViewMode {
    /// 打印视图（默认）：按节的页宽/页边距/分栏真实分页，页眉页脚、页底脚注齐全。
    #[default]
    Print,
    /// One page as wide as the window, no page breaks.
    /// Web 版式：一页与窗口同宽、永不分页（整篇文档排成一张长页）。
    Web,
    /// Draft/Outline: like web, page breaks shown as rules.
    /// 草稿/大纲视图：与 Web 同为一张长页，分页位置只以分隔线示意（疑为渲染层处理）。
    Draft,
}

/// 排版选项：视图模式与影响排版结果的显示开关（都参与环境指纹 env_hash）。
#[derive(Clone, Debug, Default)]
pub struct LayoutOptions {
    /// 当前视图模式。
    pub view: ViewMode,
    /// Width of the window in Web/Draft views, points.
    /// Web/草稿视图下"长页"的页宽（点），排版时下限 144 点。
    pub web_width: f32,
    /// 显示隐藏文字：隐藏文字参与排版占位。
    pub show_hidden: bool,
    /// Final text ("No Markup"): tracked deletions take no space and draw nothing.
    /// 最终文本（"无标记"）：修订删除的内容不占位、不绘制。
    pub hide_deleted: bool,
    /// Check spelling and grammar (squiggles).
    /// 拼写/语法检查（波浪线）：改变段落布局输出的标记，进环境指纹。
    pub proofing: bool,
}

/// Something placed on a page (page coordinates, points, y down).
/// 页面上的一项"版面事实"（页面坐标系：原点在页左上角，单位为点，y 轴向下）。
/// 它与 display::Draw 的"绘制指令"刻意分离：Placed 只回答"什么内容放在了哪里"，
/// 供命中测试/选区/编辑逻辑复用；渲染器再把它翻译成 Draw（字形串、线、填充、图片）去画。
#[derive(Clone, Debug)]
pub enum Placed {
    /// Lines `l0..l1` of a paragraph; `y` is the top of line `l0`; `x` the column's left edge.
    /// Turned lines (a table cell's text direction) run as `turn` says from the page point
    /// (`x`, `y`): see [`turn_point`].
    /// 段落的第 l0..l1 行（左闭右开）：x 为栏左缘，y 为首行行顶；行数据在 para 里共享，
    /// 竖排（turn 非 Horizontal）时 (x,y) 是行框原点，行内坐标按 turn 仿射到页面（见 turn_point）。
    Lines {
        /// 所属故事：正文 Body，或某部件（文本框/页眉/脚注等）的 id。
        story: StoryRef,
        /// 段落在所属故事里的块索引路径。
        path: Path,
        /// 段落布局（行、簇等），与排版缓存共享同一份 Arc。
        para: Arc<ParaLayout>,
        /// 首行行号（含）。
        l0: usize,
        /// 末行行号（不含）。
        l1: usize,
        /// 放置原点 x（未旋转时为栏左缘）。
        x: f32,
        /// 放置原点 y（首行行顶）。
        y: f32,
        /// 行走向（表格单元格的竖排方向）。
        turn: TextDirection,
    },
    /// 矩形填充（段落底纹、页面背景等）。
    Fill {
        /// 填充矩形。
        rect: Rect,
        /// 填充色。
        color: Rgb,
    },
    /// 线段/边框（段落边框、脚注分隔线、分栏线、页面边框等）。
    Rule {
        /// 起点 x。
        x0: f32,
        /// 起点 y。
        y0: f32,
        /// 终点 x。
        x1: f32,
        /// 终点 y。
        y1: f32,
        /// 边框样式（线型/粗细/颜色/与文字的距离）。
        border: Border,
    },
    /// A picture in `rect`, turned about the rect's centre by `spin` (as are shapes and charts).
    /// 图片：位于 rect，绕矩形中心旋转 spin（形状、图表同理）。
    Image {
        /// 图片矩形（未旋转时的外框）。
        rect: Rect,
        /// 媒体资源标识（文档媒体库键）。
        media: String,
        /// 裁剪 [左, 上, 右, 下]（比例，疑为与文档模型约定一致）。
        crop: [f32; 4],
        /// 锚点段落所属故事。
        story: StoryRef,
        /// 锚点段落路径。
        path: Path,
        /// 对象在段落文本里的字节偏移（U+FFFC 处）。
        off: usize,
        /// 旋转（绕中心）。
        spin: Spin,
    },
    /// 形状（矩形/椭圆/自由曲线等）。
    Shape {
        /// 形状矩形（未旋转外框）。
        rect: Rect,
        /// 形状种类。
        kind: wordcraft_doc::para::ShapeKind,
        /// 填充色（None=无填充）。
        fill: Option<Rgb>,
        /// 描边色（None=无描边）。
        stroke: Option<Rgb>,
        /// 描边宽度（点）。
        stroke_width: f32,
        /// A freeform's paths (ink strokes among them).
        /// 自由形状的路径数据（含墨迹笔划）。
        freeform: Option<Arc<wordcraft_doc::freeform::Freeform>>,
        /// 形状效果（阴影等）。
        effects: wordcraft_doc::effects::ShapeEffects,
        /// 旋转（绕中心）。
        spin: Spin,
    },
    /// A floating chart or diagram, drawn from its items inside `rect`. The object is the U+FFFC at
    /// byte `off` of paragraph `path` (its alt text).
    /// 浮动图表/示意图：在 rect 内按其条目绘制；对象即段落 path 中字节 off 处的 U+FFFC（备选文本）。
    Graphic {
        /// 图表矩形。
        rect: Rect,
        /// 图表数据（条目列表）。
        graphic: Arc<wordcraft_doc::graphic::Graphic>,
        /// 锚点段落所属故事。
        story: StoryRef,
        /// 锚点段落路径。
        path: Path,
        /// 对象在段落文本里的字节偏移。
        off: usize,
        /// 旋转（绕中心）。
        spin: Spin,
    },
    /// A table cell's area (for hit testing and cell selection).
    /// 表格单元格的区域（供命中测试与单元格选择）。
    Cell {
        /// 单元格矩形。
        rect: Rect,
        /// 所属表格路径。
        table: Path,
        /// 行号。
        row: usize,
        /// 格号。
        cell: usize,
        /// 表格所属故事。
        story: StoryRef,
    },
    /// The area of a picture, shape or text box, for hit testing, selection handles and dragging
    /// (drawn by `Image`/`Shape`/`Lines`). The object is the U+FFFC at byte `off` of paragraph
    /// `path` in `story`. `rect` is its unrotated frame; `spin` turns it about the centre.
    /// 图片/形状/文本框的"对象区"（供命中测试、选择手柄与拖拽；绘制由 Image/Shape/Lines 承担）。
    /// rect 是未旋转外框；spin 绕中心旋转。
    Object {
        /// 对象矩形（未旋转外框）。
        rect: Rect,
        /// 旋转（绕中心）。
        spin: Spin,
        /// 锚点段落所属故事。
        story: StoryRef,
        /// 锚点段落路径。
        path: Path,
        /// 对象在段落文本里的字节偏移。
        off: usize,
        /// The text box story it shows (`Document::parts` id).
        /// 若是文本框：其内容故事的 parts id。
        text_box: Option<u32>,
        /// 环绕方式。
        wrap: Wrap,
        /// Its paragraph's column left and top: where offsets relative to the column and the
        /// paragraph (`Anchor::Column` / `Anchor::Paragraph`) start.
        /// 锚点段的栏左缘与段顶：相对栏/相对段落（Anchor::Column/Paragraph）的偏移从这里起算。
        origin: wordcraft_geom::Point,
    },
}

impl Placed {
    /// 平移条目 (dx,dy)：各变体只挪自己的定位字段（旋转与内容不变）。用于把盒内坐标搬到页面坐标。
    fn translate(&mut self, dx: f32, dy: f32) {
        match self {
            Placed::Lines { x, y, .. } => {
                *x += dx;
                *y += dy;
            }
            Placed::Fill { rect, .. }
            | Placed::Image { rect, .. }
            | Placed::Shape { rect, .. }
            | Placed::Graphic { rect, .. }
            | Placed::Cell { rect, .. } => {
                rect.x += dx;
                rect.y += dy;
            }
            Placed::Object { rect, origin, .. } => {
                rect.x += dx;
                rect.y += dy;
                origin.x += dx;
                origin.y += dy;
            }
            Placed::Rule { x0, y0, x1, y1, .. } => {
                *x0 += dx;
                *x1 += dx;
                *y0 += dy;
                *y1 += dy;
            }
        }
    }

    /// The page area of turned lines (`None` for anything else).
    /// 竖排行片的页面包围盒（非行片或未竖排返回 None）：宽取各行右缘最大值，高取首末行跨度，经 turn_rect 仿射。
    pub fn turned_bounds(&self) -> Option<Rect> {
        let Placed::Lines { para, l0, l1, x, y, turn, .. } = self else { return None };
        if !turn.is_turned() {
            return None;
        }
        let first = para.lines.get(*l0)?;
        let last = para.lines.get(l1.checked_sub(1)?)?;
        let len = para.lines.get(*l0..*l1).unwrap_or(&[]).iter().map(|l| l.right).fold(0.0f32, f32::max);
        Some(turn_rect(*turn, *x, *y, Rect::new(0.0, 0.0, len, last.top + last.height - first.top)))
    }

    /// Move an item laid out in a turned frame (a table cell's text running `turn`) onto the
    /// page, the frame's origin landing on page point (`x`, `y`).
    /// 把在竖排框内排好的条目搬到页面：框原点落在页面 (x,y)。未竖排时退化为平移；竖排时
    /// 行片原点经 turn_point 变换、矩形经 turn_rect（宽高互换）；已竖排的行片保持自身方向。
    fn turn(&mut self, turn: TextDirection, x: f32, y: f32) {
        if !turn.is_turned() {
            self.translate(x, y);
            return;
        }
        match self {
            Placed::Lines { x: lx, y: ly, turn: t, .. } => {
                (*lx, *ly) = turn_point(turn, x, y, *lx, *ly);
                // Turned text inside turned text keeps its own direction (no 180° text).
                if !t.is_turned() {
                    *t = turn;
                }
            }
            Placed::Fill { rect, .. }
            | Placed::Image { rect, .. }
            | Placed::Shape { rect, .. }
            | Placed::Graphic { rect, .. }
            | Placed::Cell { rect, .. } => {
                *rect = turn_rect(turn, x, y, *rect);
            }
            Placed::Object { rect, origin, .. } => {
                *rect = turn_rect(turn, x, y, *rect);
                (origin.x, origin.y) = turn_point(turn, x, y, origin.x, origin.y);
            }
            Placed::Rule { x0, y0, x1, y1, .. } => {
                (*x0, *y0) = turn_point(turn, x, y, *x0, *y0);
                (*x1, *y1) = turn_point(turn, x, y, *x1, *y1);
            }
        }
    }
}

/// Where point (`u`, `v`) of a frame turned `turn` lands on the page, the frame's origin being
/// page point (`x`, `y`): `u` runs along the lines, `v` across them (down for horizontal text).
/// 竖排仿射：把"行框内坐标 (u,v)"映射为页面坐标——行框原点在页面 (x,y)，u 沿行进方向、
/// v 沿行堆叠方向。Horizontal 平移；Down（向下竖排）x 取 -v、y 取 u；Up 相反。
pub fn turn_point(turn: TextDirection, x: f32, y: f32, u: f32, v: f32) -> (f32, f32) {
    match turn {
        TextDirection::Horizontal => (x + u, y + v),
        TextDirection::Down => (x - v, y + u),
        TextDirection::Up => (x + v, y - u),
    }
}

/// The inverse of [`turn_point`]: the frame point at page point (`px`, `py`).
/// turn_point 的逆变换：由页面点 (px,py) 反推行框内坐标。
pub fn unturn_point(turn: TextDirection, x: f32, y: f32, px: f32, py: f32) -> (f32, f32) {
    match turn {
        TextDirection::Horizontal => (px - x, py - y),
        TextDirection::Down => (py - y, x - px),
        TextDirection::Up => (y - py, px - x),
    }
}

/// The page rectangle a frame rectangle covers (see [`turn_point`]).
/// 竖排时矩形宽高互换并按 turn_point 仿射：返回行框矩形 r 在页面上覆盖的矩形。
pub fn turn_rect(turn: TextDirection, x: f32, y: f32, r: Rect) -> Rect {
    match turn {
        TextDirection::Horizontal => Rect::new(x + r.x, y + r.y, r.w, r.h),
        TextDirection::Down => Rect::new(x - r.y - r.h, y + r.x, r.h, r.w),
        TextDirection::Up => Rect::new(x + r.y, y - r.x - r.w, r.h, r.w),
    }
}

/// One laid-out page.
/// 一页排好的结果：页面尺寸、正文区、页面条目与页眉页脚条目。
#[derive(Clone, Debug, Default)]
pub struct Page {
    /// 页宽（点）。
    pub w: f32,
    /// 页高（点；Web/草稿视图为最终收口后的"长页"高度）。
    pub h: f32,
    /// Index into `Document::sections()`.
    /// 所属节在文档节列表中的下标。
    pub section: usize,
    /// Page number as displayed (after section restarts).
    /// 显示页码（考虑节内重新编号后的值）。
    pub number: u32,
    /// 页面条目（正文内容 + 装饰：页框、分栏线、脚注及分隔线、行号等）。
    pub items: Vec<Placed>,
    /// Header / footer items (drawn dimmed while editing the body).
    /// 页眉条目（编辑正文时渲染层画成淡色）。
    pub header: Vec<Placed>,
    /// 页脚条目。
    pub footer: Vec<Placed>,
    /// The body text area (margins).
    /// 正文区（页边距内的矩形）。
    pub body: Rect,
    /// 本页页眉所用部件 id（无页眉为 None）。
    pub header_story: Option<u32>,
    /// 本页页脚所用部件 id。
    pub footer_story: Option<u32>,
    /// Top-level body block index at the start of the page.
    /// 本页起始时的顶层正文块下标（用于定位/滚动）。
    pub first_block: usize,
}

/// The whole layout.
/// 整篇文档的排版结果。
#[derive(Clone, Debug, Default)]
pub struct DocLayout {
    /// 全部页（Web/草稿视图只有一"长页"）。
    pub pages: Vec<Page>,
    /// Index: (story, paragraph path) → (page, item index) of each placed piece, in order.
    /// 反查索引：段落 → 它每一片所在 (页号, 页面条目号)，按顺序排列。
    pub index: HashMap<(StoryRef, Path), Vec<(usize, usize)>>,
    /// Layout time, milliseconds.
    /// 本次排版耗时（毫秒，统计用）。
    pub ms: f64,
    /// Text box stories laid out (each time a box's text was).
    /// 文本框排版次数（每次排文本框文字计一次，统计用）。
    pub text_boxes: usize,
}

/// 段落排版结果的缓存键：f32 宽度用 to_bits 转成 u32 进哈希，避免浮点直接比较。
/// 每个成分对应一个会影响断行结果的输入（构造处见 Ctx::para_labelled）。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    /// 段落内容版本号（编辑一次递增）：内容变了键必变。
    rev: u64,
    /// 可用排版宽度（f32 位模式）：宽度不同断行不同。
    width: u32,
    /// 列表编号标签摘要（编号文本+缩进/悬挂/后缀等）：改变行内前缀与可用宽度。
    label: Option<String>,
    /// 段落含页码类域（PAGE/NUMPAGES/SECTIONPAGES/SECTION）时为 (页,总页,节页数,节号)；
    /// 域取值随页变化，必须并入键。None=与页无关，可跨页复用。
    page: Option<(u32, u32, u32, u32)>,
    /// 所在表格单元格表样字符格式的摘要哈希（0=不在表内）。
    table: u64,
    /// 段落引用脚注时为 notes_hash（编号表哈希，引用标记文本随编号变），否则 0。
    notes: u64,
    /// 浮动对象绕排区列表的哈希（0=无绕排）：绕排区不同换行点不同。
    excl: u64,
    /// 公式自动编号值（仅当本段含自动编号公式时非 0）。
    eq: u32,
}

/// Memoised paragraph layouts.
/// 段落排版缓存：键 → 排好的段落；used 记录"本轮实际用到"的键，供缓存淘汰。
#[derive(Default)]
pub struct LayoutCache {
    /// 缓存本体。
    paras: HashMap<Key, Arc<ParaLayout>>,
    /// 环境指纹（样式/编号/设置/开关）：变化即整体失效。
    env: u64,
    /// 本轮排版用到的键集合（每轮 layout 开头清空）。
    used: HashSet<Key>,
    /// 命中次数（统计）。
    pub hits: u64,
    /// 未命中次数（统计）。
    pub misses: u64,
}

impl LayoutCache {
    /// 新建空缓存。
    pub fn new() -> Self {
        Self::default()
    }
    /// 当前缓存条目数。
    pub fn len(&self) -> usize {
        self.paras.len()
    }
    /// 缓存是否为空。
    pub fn is_empty(&self) -> bool {
        self.paras.is_empty()
    }
    /// 清空全部段落缓存（env 指纹保留，供下轮比对）。
    pub fn clear(&mut self) {
        self.paras.clear();
    }
}

/// 对任意可哈希值算一个 u64 指纹（默认 SipHash）。
fn hash_of<T: Hash>(t: &T) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    t.hash(&mut h);
    h.finish()
}

/// 环境指纹：样式表+编号定义+默认制表位+脚注格式序列化后，连同显示隐藏文字/最终文本/
/// 拼写检查开关、用户词典大小（拼写标记随词典变）、自动断字设置一起哈希。
/// 这些输入变了，几乎所有段落的布局都可能变，因此作为缓存整体失效的依据。
fn env_hash(doc: &Document, opts: &LayoutOptions) -> u64 {
    let s = serde_json::to_string(&(&doc.styles, &doc.numbering, doc.settings.default_tab, &doc.settings.footnote_format)).unwrap_or_default();
    hash_of(&(s, opts.show_hidden, opts.hide_deleted, opts.proofing, wordcraft_proof::user_dictionary().len(), doc.settings.auto_hyphenation))
}

/// 段落里是否含页码类域（PAGE/NUMPAGES/SECTIONPAGES/SECTION）：这类域的取值随页变化，
/// 含它们的段落进缓存时必须把域取值四元组并入缓存键（见 Key::page）。
fn has_page_fields(p: &Paragraph) -> bool {
    p.objects.iter().any(|o| match o {
        InlineObject::Field { instr, .. } => {
            matches!(fields::field_name(instr).as_str(), "PAGE" | "NUMPAGES" | "SECTIONPAGES" | "SECTION")
        }
        _ => false,
    })
}

/// Walk state while paginating.
/// 分页过程中的游走状态：文档、缓存、选项、计数器与域上下文，贯穿整次排版。
struct Ctx<'a> {
    /// 被排版的文档（只读引用）。
    doc: &'a Document,
    /// 段落排版缓存（可变：命中/未命中计数、写入与淘汰都经它）。
    cache: &'a mut LayoutCache,
    /// 排版选项。
    opts: &'a LayoutOptions,
    /// 列表/标题编号计数器（按文档顺序推进，与 next_label 配合）。
    counters: Counters,
    /// 域取值上下文（页码/总页数/节/脚注编号表等），分页推进中回填。
    fields: FieldCtx,
    /// 脚注编号表的哈希（进缓存键 notes 成分）。
    notes_hash: u64,
    /// 行号"行"的段落布局缓存（按号码复用，避免每行重排）。
    numbers: HashMap<u32, Arc<ParaLayout>>,
    /// Automatic equation numbers so far.
    /// 已累计的公式自动编号个数（按文档顺序推进）。
    eq_count: u32,
    /// Bounds laying out text boxes inside text boxes, for the whole layout.
    /// 文本框嵌套排版的预算（防盒子套自己/过深/过多），整次排版共享。
    boxes: wordcraft_doc::BoxBudget,
}

impl Ctx<'_> {
    /// A laid-out line number (cached).
    /// 排一个行号数字（带缓存）：固定 10 号灰色、宽 60 点的小段落，供页边行号显示。
    fn number_para(&mut self, n: u32) -> Arc<ParaLayout> {
        if let Some(p) = self.numbers.get(&n) {
            return p.clone();
        }
        let mut p = Paragraph::with_text(
            &n.to_string(),
            CharProps { size: Some(10.0), color: Some(wordcraft_doc::TextColor::Rgb(Rgb(0x60, 0x60, 0x60))), ..Default::default() },
        );
        p.props.space_after = Some(0.0);
        p.props.space_before = Some(0.0);
        let env = para::ParaEnv {
            doc: self.doc,
            width: 60.0,
            label: None,
            fields: &self.fields,
            show_hidden: false,
            hide_deleted: false,
            table: None,
            proofing: false,
            exclusions: &[],
            eq_number: 0,
        };
        let pl = Arc::new(para::layout_para(&p, &env));
        self.numbers.insert(n, pl.clone());
        pl
    }

    /// The list label of `p`, advancing the list counters: call it once per paragraph, in
    /// document order (only for paragraphs that will show).
    /// 取段落 p 的列表编号标签并推进计数器：必须按文档顺序、每段只调一次
    /// （且只对会显示的段落调），否则编号会跳号或重复。
    fn next_label(&mut self, p: &Paragraph) -> Option<(String, Level)> {
        let n = p.props.numbering.or_else(|| self.doc.styles.resolve_para(&p.props).numbering).filter(|n| n.num != 0)?;
        self.counters.next_label(&self.doc.numbering, n.num, n.level)
    }

    /// Lay out `p` with its already counted `label`, so laying a paragraph out again (around
    /// floating objects, or after it moved) doesn't advance the list counters twice.
    /// 段落排版 + 记忆化的统一入口：label 必须是调用方已经用 next_label 计好数的，
    /// 这样同一段落因绕排浮动对象或移位而重排多次时，列表计数器不会重复推进。
    /// 先拼缓存键查缓存；未命中才真正走 para::layout_para，并把结果存入缓存。
    fn para_labelled(
        &mut self,
        p: &Paragraph,
        width: f32,
        table: Option<&para::CellText>,
        exclusions: &[para::Exclusion],
        label: Option<(String, Level)>,
    ) -> Arc<ParaLayout> {
        let page = if has_page_fields(p) { Some(self.fields.page_key()) } else { None };
        // Automatic equation numbers count through the document.
        // 公式自动编号跨文档累计：本段含几个自动编号公式就占几个号。
        let eq_here: u32 = p
            .objects
            .iter()
            .map(|o| match o {
                InlineObject::Equation { math, display, .. } => math::auto_numbers(math, *display),
                _ => 0,
            })
            .sum();
        let eq_number = self.eq_count;
        self.eq_count = self.eq_count.saturating_add(eq_here);
        // 缓存键逐成分构造：任何影响断行结果的输入都体现在键里。
        let key = Key {
            // 段落内容版本：编辑后键变，旧布局结果自然失效。
            rev: p.rev,
            // 宽度转位模式进键：f32 不实现 Eq，用位模式参与相等比较。
            width: width.to_bits(),
            // 编号标签的格式摘要（缩进/悬挂/后缀/对齐/制表/字符），决定前缀与可用宽度。
            label: label.as_ref().map(|(t, l)| format!("{t}|{}|{}|{:?}|{:?}|{:?}|{:?}", l.indent, l.hanging, l.suffix, l.align, l.tab, l.chr)),
            // 页码类域取值四元组（无页域为 None，可跨页复用）。
            page,
            // 单元格表样字符格式摘要哈希（不在表内为 0）。
            table: table.map(|t| hash_of(&format!("{t:?}"))).unwrap_or(0),
            // 段落引用脚注才把编号表哈希入键（编号变→引用标记文本变）；无脚注引用共用 0。
            notes: if p.objects.iter().any(|o| matches!(o, InlineObject::NoteRef { .. })) { self.notes_hash } else { 0 },
            // 有绕排区才哈希入键（空列表共用 0，省一次哈希）。
            excl: if exclusions.is_empty() { 0 } else { hash_of(&format!("{exclusions:?}")) },
            // 含自动编号公式才把当前编号值入键。
            eq: if eq_here > 0 { eq_number } else { 0 },
        };
        // 先记入"本轮用到"集合：即使本次没命中，这个键也被认为活跃，淘汰时保留。
        self.cache.used.insert(key.clone());
        // 命中：直接共享缓存里的 Arc，不重排。
        if let Some(pl) = self.cache.paras.get(&key) {
            self.cache.hits += 1;
            return pl.clone();
        }
        // 未命中：真正排一次并存入缓存。
        self.cache.misses += 1;
        let env = para::ParaEnv {
            doc: self.doc,
            width,
            label,
            fields: &self.fields,
            show_hidden: self.opts.show_hidden,
            hide_deleted: self.opts.hide_deleted,
            table,
            proofing: self.opts.proofing,
            exclusions,
            eq_number,
        };
        let pl = Arc::new(para::layout_para(p, &env));
        self.cache.paras.insert(key, pl.clone());
        pl
    }
}

/// Which of `p`'s objects (by index) the layout leaves out ([`para::left_out`]): anchored in hidden
/// text unless `show_hidden`, or in a tracked deletion with `hide_deleted`. `table_chr` is the
/// cell's table-style character formatting ([`para::CellText::chr`]), resolved as the paragraph is
/// laid out with (`StyleSheet::resolve_char_in`).
/// 判定段落 p 里哪些（按对象下标的）内联对象被排版丢弃：锚在隐藏文字里（除非显示隐藏文字），
/// 或锚在修订删除里（最终文本模式下）。返回闭包按下标查询；table_chr 是单元格表样字符格式，
/// 判定时与段落样式一起经样式表解析（与实际排版同一套解析规则）。
fn left_out_objects(doc: &Document, p: &Paragraph, table_chr: Option<&CharProps>, show_hidden: bool, hide_deleted: bool) -> impl Fn(usize) -> bool {
    let mut left = Vec::new();
    // 显示隐藏文字且保留修订标记时，任何对象都不会被丢弃。
    // Nothing can be left out with hidden text shown and markup on.
    if (hide_deleted || !show_hidden) && !p.objects.is_empty() {
        let style = p.props.style.as_deref();
        let is_left = |c: &CharProps| {
            let rc = doc.styles.resolve_char_in(style, table_chr, c);
            para::left_out(&rc, show_hidden, hide_deleted)
        };
        // run 与对象都按顺序排列：一趟扫描即可定位每个对象所属的 run（越过最后一个 run 即段落标记），
        // 同一 run 的格式只解析一次（last 缓存上一次的解析结果）。
        // Runs and objects are both in order: one walk finds each object's run (past the last run,
        // the paragraph mark, as `props_of_char` gives), resolving a run's formatting only once.
        let mut runs = p.run_ranges().peekable();
        let mut last: Option<(usize, bool)> = None;
        for off in p.object_offsets() {
            while runs.next_if(|(r, _)| r.end <= off).is_some() {}
            let l = match runs.peek() {
                Some((r, c)) => match last {
                    Some((end, l)) if end == r.end => l,
                    _ => {
                        let l = is_left(c);
                        last = Some((r.end, l));
                        l
                    }
                },
                None => is_left(&p.mark),
            };
            left.push(l);
        }
    }
    move |k| left.get(k).copied().unwrap_or(false)
}

/// Note part ids in document order → numbers (footnotes and endnotes numbered separately). With
/// `hide_deleted`, notes whose reference mark is a tracked deletion are left out, as in the final text.
/// 脚注/尾注编号分配：按文档阅读顺序给每个笔记引用编号（脚注、尾注各自从 1 计数），
/// 返回 笔记部件 id → 编号。hide_deleted 时引用标记被修订删除的笔记不占号（同最终文本）；
/// 只是被隐藏的引用仍占号（同 Word）。
fn note_numbers(doc: &Document, hide_deleted: bool) -> HashMap<u32, u32> {
    let mut m = HashMap::new();
    let (mut f, mut e) = (0u32, 0u32);
    // 按阅读顺序遍历（文本框按其在文中的位置参与）；cur 缓存当前段落的"对象是否被丢弃"表，同段只解析一次。
    // Reading order, text boxes included where they are. A hidden reference mark still takes
    // its number, as in Word; a deleted one (or one in a deleted text box) does not.
    let mut cur: Option<(usize, Vec<bool>)> = None;
    doc.objects_in_reading_order_at(&mut |p, k| {
        let key = p as *const Paragraph as usize;
        if cur.as_ref().is_none_or(|(at, _)| *at != key) {
            let deleted = left_out_objects(doc, p, None, true, hide_deleted);
            cur = Some((key, (0..p.objects.len()).map(deleted).collect()));
        }
        if cur.as_ref().is_some_and(|(_, deleted)| deleted.get(k).copied().unwrap_or(false)) {
            return false;
        }
        if let Some(InlineObject::NoteRef { kind, id, .. }) = p.objects.get(k) {
            let n = match kind {
                wordcraft_doc::para::NoteKind::Footnote => {
                    f += 1;
                    f
                }
                wordcraft_doc::para::NoteKind::Endnote => {
                    e += 1;
                    e
                }
            };
            m.insert(*id, n);
        }
        true
    });
    m
}

/// Lay out a block list into a free-standing box of `width` (no page breaks): table cells,
/// headers, footers, notes, text boxes. Returns items relative to (0, 0) and the height. `frame`
/// says where the box sits on its page, for floating objects positioned relative to the page or
/// its margins; without it (cells, notes, text boxes) they're positioned in the box.
/// 无分页的自由盒排版：把块列表排进宽 width 的盒子里（表格单元格、页眉页脚、脚注正文、文本框）。
/// 返回相对 (0,0) 的条目与内容总高。frame 给出盒子在页面上的位置，供相对页面/页边距定位的
/// 浮动对象换算；没有 frame（单元格、脚注、文本框）时这类定位退化为盒内定位。
#[allow(clippy::too_many_arguments)]
fn layout_box(
    ctx: &mut Ctx,
    story: StoryRef,
    blocks: &Blocks,
    prefix: &[u32],
    width: f32,
    table: Option<&para::CellText>,
    depth: usize,
    frame: Option<PageFrame>,
) -> BoxLayout {
    let mut items = Vec::new();
    // Pictures behind the text go first; `behind` counts them.
    // 衬于文字下方的图片要最先画：behind 记录它们的个数（插入位置依据）。
    let mut behind = 0;
    // Wrap areas of the box's floating objects.
    // 盒内浮动对象的文字避让区（矩形 + 是否上下环绕）。
    let mut excl: Vec<(Rect, bool)> = Vec::new();
    // Areas of the box's floating tables, which other floating tables may have to avoid.
    // 盒内浮动表格的占位区：后续浮动表格可能要避让它们。
    let mut float_tables: Vec<Rect> = Vec::new();
    let mut y = 0.0f32;
    let mut prev_after = 0.0f32;
    let mut prev_style: Option<(String, bool)> = None;
    for (i, b) in blocks.iter().enumerate() {
        let mut path = prefix.to_vec();
        path.push(i as u32);
        match &**b {
            Block::Para(p) => {
                let label = ctx.next_label(p);
                let mut pl = ctx.para_labelled(p, width, table, &[], label.clone());
                let ctxl = pl.rp.contextual_spacing;
                let same = prev_style.as_ref().is_some_and(|(s, c)| *s == pl.rp.style && (*c || ctxl));
                let before = if same && ctxl { 0.0 } else { pl.rp.space_before };
                if same && ctxl {
                    y -= prev_after;
                }
                // Word: space between paragraphs is before + after (no collapsing).
                // 上下文间距：同风格相邻段且任一方要求时，吃掉上一段的段后距与本段的段前距。
                // Word 的段间距是前段 after + 后段 before 累加，不做折叠。
                y += if i == 0 { before } else { before.max(0.0) };
                // Floating objects anchored here: place them, then wrap the text around them.
                // One left out of the layout (hidden, or deleted in the final text) takes no room.
                // 锚在本段的浮动对象：先定位，再把文字绕开它们；被丢弃的对象（隐藏/修订删除）不占位。
                let mut floats = HashMap::new();
                let left_out = left_out_objects(ctx.doc, p, table.map(|t| &t.chr), ctx.opts.show_hidden, ctx.opts.hide_deleted);
                for (oi, o) in p.objects.iter().enumerate() {
                    let Some((w, h, float)) = floating(o) else { continue };
                    if left_out(oi) {
                        continue;
                    }
                    let r = float_rect(frame, (0.0, width), y, w, h, float);
                    excl.extend(wrap_area(r, float));
                    floats.insert(oi, r);
                }
                let rel = rel_exclusions(&excl, 0.0, y);
                // 有避让区则带着绕排约束重排一次（缓存键的 excl 成分会不同）。
                if !rel.is_empty() {
                    pl = ctx.para_labelled(p, width, table, &rel, label);
                }
                let lead = pl.lines.first().map_or(0.0, |l| l.top.max(0.0));
                push_para(&mut items, story, &path, &pl, 0, pl.lines.len(), 0.0, y + lead, width);
                // Pictures, shapes and text boxes: drawn, their areas and text boxes' text.
                if !p.objects.is_empty() {
                    let at = ObjFrame { page: frame, col: (0.0, width), para_y: y, table_chr: table.map(|t| &t.chr) };
                    let (back, front) = place_objects(ctx, story, &path, p, &pl, (0, pl.lines.len()), (0.0, y + lead), &at, &floats, depth);
                    let at = behind.min(items.len());
                    behind += back.len();
                    items.splice(at..at, back);
                    items.extend(front);
                }
                y += pl.height + pl.rp.space_after;
                prev_after = if same && ctxl { 0.0 } else { pl.rp.space_after };
                prev_style = Some((pl.rp.style.clone(), ctxl));
            }
            Block::Table(t) => {
                // 防御：表格嵌套超过 8 层直接丢弃该表（防病态文档递归爆炸）。
                if depth > 8 {
                    continue;
                }
                let tl = table::layout_table(ctx, story, t, &path, width, depth + 1);
                let (tx, ty) = match t.props.float {
                    // A floating table stands at its own position and takes no room; the text
                    // after it wraps around it.
                    Some(f) => {
                        // 旧版兼容（compat_mode<15）：偏移定位的是首格文本，表缘要再退一个单元格边距。
                        let legacy = if ctx.doc.settings.compat_mode < 15 { table::first_cell_left_margin(ctx, t) } else { 0.0 };
                        let (r, area) = box_float_table_rect(frame, width, y, &tl, &f, legacy, &float_tables);
                        excl.push((area, false));
                        float_tables.push(area);
                        (r.x, r.y)
                    }
                    // A table in the text flow starts below the floating objects in its way.
                    // 文流中的表格不绕排：直接沉到挡路浮动对象的下方。
                    None => (tl.x, below(&excl, y, tl.x, tl.x + tl.width)),
                };
                let mut row_y = ty;
                for row in &tl.rows {
                    for it in &row.items {
                        let mut it = it.clone();
                        it.translate(tx, row_y);
                        items.push(it);
                    }
                    row_y += row.height;
                }
                if t.props.float.is_none() {
                    y = row_y;
                    prev_after = 0.0;
                    prev_style = None;
                }
            }
        }
    }
    let floats_bottom = float_tables.iter().map(|r| r.bottom()).fold(0.0, f32::max);
    // height 取文字流游标与 0 的较大者；floats_bottom 让单元格能长到装下浮动表。
    BoxLayout { items, height: y.max(0.0), floats_bottom }
}

/// A laid-out box: items relative to its top-left, the height of its text flow, and where its
/// floating tables end (a table cell grows to hold them).
/// 排好的盒子：items 相对盒左上角；height 是文字流高度；floats_bottom 是盒内浮动表的
/// 最低底部（表格单元格要长到能装下它们）。
struct BoxLayout {
    items: Vec<Placed>,
    height: f32,
    floats_bottom: f32,
}

/// Where floating table `tl` goes in a box `width` wide whose next paragraph starts at `y`, and
/// the area text keeps clear of. Positions relative to the page or its margins need the box's
/// `frame`; without one (a table cell) they're relative to the box.
/// 盒内浮动表格的落位：返回 (表格矩形, 文字避让区)。y 是当前文字流位置。
/// 相对页面/页边距的定位需要 frame 换算进盒内坐标；盒内（单元格）没有参照系时按盒自身处理。
fn box_float_table_rect(
    frame: Option<PageFrame>,
    width: f32,
    y: f32,
    tl: &table::TableLayout,
    f: &TableFloat,
    legacy: f32,
    float_tables: &[Rect],
) -> (Rect, Rect) {
    // 防御性上限：浮动表高/宽夹进 1..100000 点，避免病态数据。
    let h = tl.rows.iter().map(|r| r.height).sum::<f32>().clamp(0.0, 100_000.0);
    let w = tl.width.clamp(1.0, 100_000.0);
    let page_x = |s: &SectionProps| match f.h_rel {
        Anchor::Page => Some((0.0, s.page_w)),
        Anchor::Margin => Some((s.margin_left + s.gutter, s.text_width())),
        _ => None,
    };
    let page_y = |s: &SectionProps| match f.v_rel {
        Anchor::Page => Some((0.0, s.page_h)),
        Anchor::Margin => Some((s.margin_top, s.text_height())),
        _ => None,
    };
    let h_area = frame.and_then(|fr| page_x(fr.sect).map(|(a, e)| (a - fr.origin.0, e))).unwrap_or((0.0, width));
    // In a cell an offset from the page or margin has nothing to measure from: the table stands
    // where the text is.
    // 单元格内没有页面参照：纵向区域退化为当前文字位置；相对页面/页边距的偏移清零。
    let (v_area, y_off) = match frame.and_then(|fr| page_y(fr.sect).map(|(a, e)| (a - fr.origin.1, e))) {
        Some(area) => (area, f.y),
        None if matches!(f.v_rel, Anchor::Page | Anchor::Margin) => ((y, 0.0), 0.0),
        None => ((y, 0.0), f.y),
    };
    // Nor is there an area to align in: the table starts where the text is.
    // 同理，纵向对齐也无处对：退化为随文字位置。
    let v_align = if frame.is_none() && matches!(f.v_rel, Anchor::Page | Anchor::Margin) { None } else { f.v_align };
    let x = align_in(h_area, f.x, w, f.h_align) - if f.h_align.is_none() { legacy } else { 0.0 };
    let r = clear_of_float_tables(Rect::new(x, align_in(v_area, y_off, h, v_align), w, h), f, legacy, h_area.0, float_tables);
    let [dl, dt, dr, db] = f.dist_from_text();
    (r, r.inset(-dl, -dt, -dr, -db))
}

/// Floating table `f` at `r`, moved below the floating tables in its way when it may not
/// overlap them. In Word 2013+ layout one placed by an offset keeps its distance from the text's
/// left edge (`text_left`).
/// 浮动表落位后与既有浮动表避让：不允许重叠时不断下移，直到不再相交。
/// Word 2013+ 排版下，按偏移放置的表还要与文字左缘保持距离（text_left）。
fn clear_of_float_tables(mut r: Rect, f: &TableFloat, legacy: f32, text_left: f32, others: &[Rect]) -> Rect {
    let [dl, dt, dr, db] = f.dist_from_text();
    let keeps_text_distance = legacy == 0.0 && f.h_align.is_none() && f.x >= 0.0 && matches!(f.h_rel, Anchor::Margin | Anchor::Column);
    // 最多 64 轮避让迭代（防御上限，防病态堆叠输入死循环；正常一两轮即收敛）。
    for _ in 0..64 {
        let area = r.inset(-dl, -dt, -dr, -db);
        let Some(below) = others.iter().filter(|o| !f.overlap && o.intersects(&area)).map(|o| o.bottom()).reduce(f32::max) else { break };
        // 有相交的浮动表：移到其中最低底部之下，再检查一轮。
        r.y = below + dt;
        if keeps_text_distance {
            r.x = r.x.max(text_left + dl);
        }
    }
    r
}

/// A text box's internal margins (Word's defaults: 0.1" left/right, 0.05" top/bottom).
/// 文本框内边距（Word 默认：左右 0.1 英寸=7.2 点；上下 0.05 英寸=3.6 点）。
const BOX_INSET_X: f32 = 7.2;
/// 纵向内边距（见上）。
const BOX_INSET_Y: f32 = 3.6;

/// Keep what fits in a box `height` tall (items relative to its top): lines that don't fit are
/// dropped (whole lines; a line that only partly fits too) and so is anything starting below.
/// The first line always stays, so even a tiny box shows the start of its text.
/// 只保留能装进高 height 盒子的条目（条目相对盒顶）：装不下的整行丢弃（只差一点也整行丢），
/// 其后的一切同样丢弃；首行永远保留——再小的盒子也能看到文字开头（文本框裁剪语义）。
fn fit_box(items: Vec<Placed>, height: f32) -> Vec<Placed> {
    // 0.5 点容差：抵消浮点误差，恰好贴底的行保留。
    let limit = height + 0.5;
    let mut out = Vec::with_capacity(items.len());
    // 是否已保留过至少一行（首行保底标记）。
    let mut kept_line = false;
    for it in items {
        if let Some(r) = it.turned_bounds() {
            // Turned text (in a table cell) stays whole.
            if r.y < limit {
                out.push(it);
            }
            continue;
        }
        match it {
            Placed::Lines { story, path, para, l0, l1, x, y, turn } => {
                let Some(first) = para.lines.get(l0) else { continue };
                let mut end = l0;
                for k in l0..l1 {
                    let Some(l) = para.lines.get(k) else { break };
                    // 首行保底（kept_line 之前不放行），之后超出盒底即截断。
                    if y + (l.top - first.top) + l.height > limit && kept_line {
                        break;
                    }
                    end = k + 1;
                    kept_line = true;
                }
                if end > l0 {
                    out.push(Placed::Lines { story, path, para, l0, l1: end, x, y, turn });
                }
            }
            Placed::Fill { rect, color } if rect.y < limit => {
                out.push(Placed::Fill { rect: Rect::new(rect.x, rect.y, rect.w, rect.h.min(limit - rect.y)), color });
            }
            Placed::Rule { x0, y0, x1, y1, border } if y0.min(y1) < limit => {
                out.push(Placed::Rule { x0, y0: y0.min(limit), x1, y1: y1.min(limit), border });
            }
            Placed::Image { rect, .. }
            | Placed::Shape { rect, .. }
            | Placed::Graphic { rect, .. }
            | Placed::Cell { rect, .. }
            | Placed::Object { rect, .. }
                if rect.y < limit =>
            {
                out.push(it);
            }
            _ => {}
        }
    }
    out
}

/// Paragraph lines plus their shading and borders.
/// 把段落的行片连同底纹、边框一起压入条目表：先底纹，再行，最后四边边框线。
fn push_para(items: &mut Vec<Placed>, story: StoryRef, path: &[u32], pl: &Arc<ParaLayout>, l0: usize, l1: usize, x: f32, y: f32, width: f32) {
    let (Some(first), Some(last)) = (pl.lines.get(l0), l1.checked_sub(1).and_then(|k| pl.lines.get(k))) else {
        return;
    };
    let h = last.top + last.height - first.top;
    // Indents are logical: a right-to-left paragraph's start indent (and hanging indent) is on the right.
    // 缩进是逻辑方向：RTL 段的"起始缩进"（含悬挂缩进）在右侧。
    let start = pl.rp.indent_left.min(pl.rp.indent_left + pl.rp.indent_first);
    let (li, ri) = if pl.rp.bidi { (pl.rp.indent_right, start) } else { (start, pl.rp.indent_right) };
    let left = x + li;
    let right = x + width - ri;
    if let Some(c) = pl.rp.shading {
        // 底纹向左右各外扩"边框与文字的距离"（无对应边框时 Word 默认 4 点）。
        let pad = pl.rp.borders.as_ref().map(|b| b.left.map(|l| l.space).unwrap_or(4.0)).unwrap_or(0.0);
        items.push(Placed::Fill { rect: Rect::new(left - pad, y, right - left + pad * 2.0, h), color: c });
    }
    items.push(Placed::Lines { story, path: Path(path.to_vec()), para: pl.clone(), l0, l1, x, y, turn: TextDirection::Horizontal });
    if let Some(b) = &pl.rp.borders {
        // 边框线位置 = 文字区向外退各自的 space 距离。
        let sp = |o: &Option<Border>| o.map(|b| b.space).unwrap_or(0.0);
        let (lx, rx) = (left - sp(&b.left), right + sp(&b.right));
        let (ty, by) = (y - sp(&b.top), y + h + sp(&b.bottom));
        // 上边框只在段首行片（l0==0）画，下边框只在排到段落末行（l1==总行数）时画：
        // 段落被分页拆开时，中间片不画横线。
        if let Some(t) = b.top.filter(|_| l0 == 0)
            && t.is_visible()
        {
            items.push(Placed::Rule { x0: lx, y0: ty, x1: rx, y1: ty, border: t });
        }
        if let Some(t) = b.bottom.filter(|_| l1 >= pl.lines.len())
            && t.is_visible()
        {
            items.push(Placed::Rule { x0: lx, y0: by, x1: rx, y1: by, border: t });
        }
        if let Some(t) = b.left.filter(Border::is_visible) {
            items.push(Placed::Rule { x0: lx, y0: ty, x1: lx, y1: by, border: t });
        }
        if let Some(t) = b.right.filter(Border::is_visible) {
            items.push(Placed::Rule { x0: rx, y0: ty, x1: rx, y1: by, border: t });
        }
    }
}

/// 分页状态机：当前节的一串页面、栏框与 y 游标，以及本页待落的脚注、绕排区等。
struct PageBuilder<'a> {
    /// 已排好的全部页。
    pages: Vec<Page>,
    /// 当前节属性。
    sect: &'a SectionProps,
    /// 当前节在文档节列表中的下标。
    sect_idx: usize,
    /// 当前栏号（0 起）。
    col: usize,
    /// 当前节各栏的 (栏内 x 偏移, 栏宽)。
    cols: Vec<(f32, f32)>,
    /// 当前栏内的 y 游标（下一个块落的纵向位置）。
    y: f32,
    /// 正文区顶（页眉之下）。
    top: f32,
    /// 正文区底（随脚注占用而向上压缩）。
    bottom: f32,
    /// 当前页的显示页码（考虑节内重新编号）。
    number: u32,
    /// Web/草稿视图（"一张长页"）标记。
    web: bool,
    /// Footnotes waiting for the bottom of the current page (first the rest of notes continued
    /// from the page before).
    /// 等待落到本页页底的脚注（上一页延续下来的排在最前）。
    notes: Vec<NoteBox>,
    /// The parts of footnotes that didn't fit on their page, continued on the next one.
    /// 本页装不下的脚注余部，下一页页底继续。
    carry: Vec<NoteBox>,
    /// Page bottom before footnotes took space.
    /// 未被脚注压缩前的原正文底（脚注区在它与正文之间分配）。
    orig_bottom: f32,
    /// The previous paragraph: (style, contextual spacing, space after) for contextual spacing.
    /// 上一段的信息 (风格, 上下文间距要求, 段后距)：供下一段的上下文间距判断。
    prev: Option<(String, bool, f32)>,
    /// Wrap areas of floating objects on this page (see [`wrap_area`]).
    /// 本页浮动对象的文字避让区（见 wrap_area）。
    excl: Vec<(Rect, bool)>,
    /// Wrap areas of the floating tables on this page, which others may not overlap.
    /// 本页浮动表格的占位区：其他浮动表不得与其重叠。
    float_tables: Vec<Rect>,
    /// Line numbering counter.
    /// 行号计数器（按页/节/文档重置规则清零）。
    line_no: u32,
    /// Index of the first body item of the current page (vertical alignment shifts from here).
    /// 当前页第一个正文条目的下标（垂直对齐/脚注统计从这里跳过装饰）。
    page_items_start: usize,
}

/// Gap above the footnote separator and its length.
/// 脚注分隔条高度（12 点）：分隔线画在条带中线，也是脚注区与上方的总留白。
const NOTE_SEP: f32 = 12.0;
/// The most pages added after the body for footnotes still continuing.
/// 正文之后为延续脚注追加页数的防御上限（1 万页）：正常每页至少消化一行，远早于上限结束。
const MAX_NOTE_PAGES: usize = 10_000;

/// A footnote waiting for the bottom of a page: its items laid out from y = 0, and its height.
/// 一条等待落到页底的脚注：条目已按左上原点排出，h 为总高。
struct NoteBox {
    /// 笔记部件 id。
    id: u32,
    /// 脚注内容的排版条目（相对自身原点）。
    items: Vec<Placed>,
    /// 总高。
    h: f32,
    /// The rest of a note that didn't fit on an earlier page (drawn below the continuation
    /// separator).
    /// 是否为上一页延续下来的余部（上方画延续分隔线）。
    cont: bool,
}

/// The vertical extent of an item laid out in a horizontal frame.
/// 条目在横排框内的纵向范围 (顶, 底)：竖排行片取其旋转后包围盒；线取两端 y 的较小/较大者。
fn v_span(it: &Placed) -> (f32, f32) {
    if let Some(r) = it.turned_bounds() {
        return (r.y, r.y + r.h);
    }
    match it {
        Placed::Lines { para, l0, l1, y, .. } => (*y, item_bottom(*y, para, *l0, *l1).unwrap_or(*y)),
        Placed::Fill { rect, .. }
        | Placed::Image { rect, .. }
        | Placed::Shape { rect, .. }
        | Placed::Graphic { rect, .. }
        | Placed::Cell { rect, .. }
        | Placed::Object { rect, .. } => (rect.y, rect.y + rect.h),
        Placed::Rule { y0, y1, .. } => (y0.min(*y1), y0.max(*y1)),
    }
}

impl NoteBox {
    /// Lines of the note's own paragraphs, which may be split between pages (not those of its
    /// tables or text boxes).
    /// 该条目是否为脚注自身段落的行（可在页间拆分）；脚注里的表格/文本框条目不可拆。
    fn splittable(&self, it: &Placed) -> bool {
        matches!(it, Placed::Lines { story: StoryRef::Part(s), path, turn: TextDirection::Horizontal, .. } if *s == self.id && path.0.len() == 1)
    }

    /// Where the note may be split, ascending: the bottoms of its lines that have more lines
    /// below them and don't cut through a picture, table or other unsplittable item.
    /// 可拆分位置（升序）：每行行底的候选切线——要求其下还有内容，且不切穿图片/表格等不可拆条目。
    fn cuts(&self) -> Vec<f32> {
        // 先收集不可拆条目的纵向范围：切线不得落在它们内部。
        let blocked: Vec<(f32, f32)> =
            self.items.iter().filter(|it| !self.splittable(it) && !matches!(it, Placed::Fill { .. } | Placed::Rule { .. })).map(v_span).collect();
        let mut last_top = f32::MIN;
        let mut cuts = Vec::new();
        for it in &self.items {
            let Placed::Lines { para, l0, l1, y, .. } = it else { continue };
            if !self.splittable(it) {
                continue;
            }
            let Some(first) = para.lines.get(*l0) else { continue };
            for l in para.lines.get(*l0..*l1).unwrap_or(&[]) {
                last_top = last_top.max(y + l.top - first.top);
                cuts.push(y + l.top - first.top + l.height);
            }
        }
        // 过滤：切线要为正、不超过最后一块内容的顶部（其下仍有内容）、且不切穿不可拆条目；
        // 再排序并按 0.01 点容差去重。
        cuts.retain(|&b| b > 0.01 && b <= last_top + 0.01 && !blocked.iter().any(|&(t, e)| t < b - 0.01 && e > b + 0.01));
        cuts.sort_by(f32::total_cmp);
        cuts.dedup_by(|a, b| (*a - *b).abs() < 0.01);
        cuts
    }

    /// The least of the note that must stay with its reference: its first line (all of it if it
    /// can't be split).
    /// 脚注必须随引用标记留在本页的最少高度：第一条可切线之下（不可拆则整条）。
    fn first_piece(&self) -> f32 {
        self.cuts().first().copied().unwrap_or(self.h).min(self.h)
    }

    /// Split the note at line bottom `cut`: the part above (`cut` tall) and the rest, moved up to
    /// start at 0 and marked as a continuation.
    /// 在行底 cut 处把脚注一分为二：上半（高 cut）留本页；余部上移到从 0 开始并标记为延续。
    fn split(self, cut: f32) -> (NoteBox, NoteBox) {
        let (mut head, mut tail) = (Vec::new(), Vec::new());
        // 按条目与切线的位置关系三分类：整体在上→head；整体在下→tail；跨越→拆开。
        for it in self.items {
            let (top, bottom) = v_span(&it);
            if bottom <= cut + 0.01 {
                head.push(it);
                continue;
            }
            if top >= cut - 0.01 {
                tail.push(it);
                continue;
            }
            match it {
                Placed::Lines { story, path, para, l0, l1, x, y, turn } => {
                    let Some(first) = para.lines.get(l0) else { continue };
                    let ft = first.top;
                    // The first line that ends below the cut starts the tail.
                    let k = (l0..l1).find(|&k| para.lines.get(k).is_none_or(|l| y + l.top - ft + l.height > cut + 0.01)).unwrap_or(l1);
                    let ky = para.lines.get(k).map_or(y, |l| y + l.top - ft);
                    if k > l0 {
                        head.push(Placed::Lines { story, path: path.clone(), para: para.clone(), l0, l1: k, x, y, turn });
                    }
                    if k < l1 {
                        tail.push(Placed::Lines { story, path, para, l0: k, l1, x, y: ky, turn });
                    }
                }
                Placed::Fill { rect, color } => {
                    head.push(Placed::Fill { rect: Rect::new(rect.x, rect.y, rect.w, cut - rect.y), color });
                    tail.push(Placed::Fill { rect: Rect::new(rect.x, cut, rect.w, rect.y + rect.h - cut), color });
                }
                Placed::Rule { x0, y0, x1, y1, border } => {
                    head.push(Placed::Rule { x0, y0: y0.min(cut), x1, y1: y1.min(cut), border });
                    tail.push(Placed::Rule { x0, y0: y0.max(cut), x1, y1: y1.max(cut), border });
                }
                // Not split (cuts avoid them): stays above.
                other => head.push(other),
            }
        }
        // The rest starts at its first line (the space between paragraphs goes with the break).
        // 余部从它的第一个行片顶部起算（段间距留在断口处），整体平移到 0 起。
        let start = tail.iter().filter(|it| matches!(it, Placed::Lines { .. })).map(|it| v_span(it).0).fold(f32::MAX, f32::min);
        let start = if start == f32::MAX { cut } else { start.max(cut) };
        for it in &mut tail {
            it.translate(0.0, -start);
            // Shading cut at the break starts at the rest's top.
            if let Placed::Fill { rect, .. } = it
                && rect.y < 0.0
            {
                rect.h = (rect.h + rect.y).max(0.0);
                rect.y = 0.0;
            }
        }
        let rest = (self.h - start).max(0.0);
        (NoteBox { id: self.id, items: head, h: cut, cont: self.cont }, NoteBox { id: self.id, items: tail, h: rest, cont: true })
    }
}

impl PageBuilder<'_> {
    /// Space footnotes take at the bottom of the page.
    /// 本页页底脚注区将要占据的总高（分隔条 + 各条脚注高）。
    fn notes_h(&self) -> f32 {
        if self.notes.is_empty() { 0.0 } else { NOTE_SEP + self.notes.iter().map(|n| n.h).sum::<f32>() }
    }
    /// The lowest bottom of the body text and tables on the current page.
    /// 本页正文文字/单元格的最低底部（正文实际用到哪里）。
    fn body_floor(&self) -> f32 {
        let Some(pg) = self.pages.last() else { return self.top };
        pg.items
            .iter()
            .skip(self.page_items_start)
            .filter(|it| matches!(it, Placed::Lines { story: StoryRef::Body, .. } | Placed::Cell { story: StoryRef::Body, .. }))
            .map(|it| v_span(it).1)
            .fold(self.top, f32::max)
    }
    /// Place the collected footnotes at the bottom of the current page. When they don't all fit
    /// below the body (`bottom`), the note that doesn't is split at a line and its rest, with
    /// the notes after it, continues on the next page.
    /// 把收集到的脚注落到本页页底。放不下时：在可切线处切分，余部连同其后的脚注
    /// 顺延到下一页（carry）。
    fn flush_notes(&mut self) {
        if self.notes.is_empty() {
            return;
        }
        let total = self.notes_h();
        let x = self.sect.margin_left + self.sect.gutter;
        // The note area starts below the text (a page of continued notes alone: at its top).
        // 脚注区顶 = 正文之下（整页都是延续脚注时 = 正文顶）。
        let area = self.bottom.max(self.body_floor());
        // 全部脚注能否塞进"原正文底"以下（0.01 容差）。
        let fits = total <= self.orig_bottom - area + 0.01;
        // 放得下：从原底往上倒排（脚注贴底）；放不下：紧贴正文往下塞。
        let top = if fits { self.orig_bottom - total } else { area };
        let mut y = top + NOTE_SEP;
        let notes = std::mem::take(&mut self.notes);
        // A page that starts with a continued note gets the full-width continuation separator.
        // 本页第一张是延续脚注：分隔线画通栏（否则 144 点短线）。
        let cont = notes.first().is_some_and(|n| n.cont);
        let mut placed = Vec::new();
        let mut rest = notes.into_iter();
        // 逐条往下排：放得下直接落；放不下时尝试按行切分。
        while let Some(n) = rest.next() {
            let room = self.orig_bottom - y;
            if fits || n.h <= room + 0.01 {
                let ny = y;
                y += n.h;
                placed.push((n, ny));
                continue;
            }
            // Split at the last line that fits. The page's first note always leaves something
            // here (its first line, or all of it if it can't be split), so continuing ends.
            // 放不下：在放得下的最后一条切线处切开。本页第一张脚注必须在此留下内容
            // （至少一行，不可拆则整条），保证延续过程必然终止。
            let first = placed.is_empty();
            let cuts = n.cuts();
            match cuts.iter().rev().find(|&&c| c <= room + 0.01).or(cuts.first().filter(|_| first)).copied() {
                // 找得到放得下的切线（或第一张脚注取首条切线）：上半留本页，余部进 carry。
                Some(c) => {
                    let (head, tail) = n.split(c);
                    placed.push((head, y));
                    self.carry.push(tail);
                }
                // 切不了但它是本页第一张：整条硬放（可能溢出），延续仍然终止。
                None if first => placed.push((n, y)),
                // 否则整条顺延到下一页。
                None => self.carry.push(n),
            }
            // 切分发生后的其余脚注全部顺延，结束本页脚注落位。
            self.carry.extend(rest.by_ref());
            break;
        }
        if let Some(pg) = self.pages.last_mut() {
            // 分隔线：延续时通栏，普通脚注 144 点短线；画在分隔条带中线。
            let len = if cont { self.sect.text_width() } else { 144.0 };
            let sy = top + NOTE_SEP / 2.0;
            pg.items.push(Placed::Rule { x0: x, y0: sy, x1: x + len, y1: sy, border: Border::single(0.5) });
            for (n, ny) in placed {
                for mut it in n.items {
                    it.translate(x, ny);
                    pg.items.push(it);
                }
            }
        }
    }
    /// Section vertical alignment: move the page's body down (centre/bottom).
    /// 节的垂直对齐：把本页正文条目整体下移（居中/贴底按剩余空间的比例）。
    fn apply_valign(&mut self) {
        let k = match self.sect.valign {
            wordcraft_doc::props::VAlign::Top => return,
            wordcraft_doc::props::VAlign::Center => 0.5,
            wordcraft_doc::props::VAlign::Bottom => 1.0,
        };
        if self.web {
            return;
        }
        // 可用富余 = 原正文底 − 脚注区 − 当前正文游标；按居中(1/2)或贴底(1)取比例。
        let space = (self.orig_bottom - self.notes_h() - self.y).max(0.0) * k;
        let start = self.page_items_start;
        if let Some(pg) = self.pages.last_mut() {
            for it in pg.items.iter_mut().skip(start) {
                it.translate(0.0, space);
            }
        }
    }
    /// 开新页：先收尾上一页（垂直对齐、落脚注），清本页绕排状态，再初始化栏框与游标。
    fn new_page(&mut self, first_block: usize, body_top: f32) {
        self.apply_valign();
        self.flush_notes();
        self.excl.clear();
        self.float_tables.clear();
        // 行号按页重新计数时清零。
        if self.sect.line_numbers.as_ref().is_some_and(|l| l.restart == wordcraft_doc::section::LineNumberRestart::Page) {
            self.line_no = 0;
        }
        let s = self.sect;
        self.number = self.number.saturating_add(1);
        // Web/草稿视图：页高给一个巨大的占位值（f32::MAX/4），最后按实际内容收口。
        let (w, h) = if self.web { (s.page_w, f32::MAX / 4.0) } else { (s.page_w, s.page_h) };
        // 正文区：左右页边距+装订线定位，顶部 body_top，高度至少 36 点（防御 0 高正文区）。
        let body = Rect::new(s.margin_left + s.gutter, body_top, s.text_width(), (s.page_h - s.margin_bottom - body_top).max(36.0));
        let mut decor = Vec::new();
        if !self.web {
            // 页面边框从页面边缘量起（Word 默认），因此框住页眉、页脚与行号。
            // Page borders, measured from the page edge (Word's default), so they surround the
            // header, footer and line numbers.
            if let Some(b) = &s.page_borders {
                let sp = |e: &Option<Border>| e.map(|x| x.space).unwrap_or(24.0);
                let (x0, x1, y0, y1) = (sp(&b.left), s.page_w - sp(&b.right), sp(&b.top), s.page_h - sp(&b.bottom));
                for (e, a, bb) in
                    [(b.top, (x0, y0), (x1, y0)), (b.bottom, (x0, y1), (x1, y1)), (b.left, (x0, y0), (x0, y1)), (b.right, (x1, y0), (x1, y1))]
                {
                    if let Some(e) = e.filter(Border::is_visible) {
                        decor.push(Placed::Rule { x0: a.0, y0: a.1, x1: bb.0, y1: bb.1, border: e });
                    }
                }
            }
            // Lines between columns.
            // 分栏分隔线：相邻两栏中点画纵线（通到下页边距）。
            if s.columns.separator && s.columns.count > 1 {
                let cols = s.column_boxes();
                for w in cols.windows(2) {
                    if let [(ax, aw), (bx, _)] = w {
                        let x = s.margin_left + s.gutter + (ax + aw + bx) / 2.0;
                        decor.push(Placed::Rule { x0: x, y0: body_top, x1: x, y1: s.page_h - s.margin_bottom, border: Border::single(0.5) });
                    }
                }
            }
        }
        // 正文条目从装饰（页框/分栏线）之后开始计数。
        self.page_items_start = decor.len();
        self.pages.push(Page { w, h, section: self.sect_idx, number: self.number, body, first_block, items: decor, ..Default::default() });
        self.col = 0;
        // 新页重取本节栏框，游标回到正文顶。
        self.cols = s.column_boxes();
        self.top = body_top;
        // Web/草稿视图：正文底也放到近乎无限（一张长页不分页）。
        self.bottom = if self.web { f32::MAX / 8.0 } else { s.page_h - s.margin_bottom };
        self.orig_bottom = self.bottom;
        self.y = self.top;
        // Notes continued from the page before come first in this page's note area.
        // 上一页延续来的脚注排在本页脚注区最前，并预先压缩本页正文底。
        if !self.carry.is_empty() && !self.web {
            self.notes = std::mem::take(&mut self.carry);
            self.bottom -= self.notes_h();
        }
    }
    /// 当前栏左缘的页面 x（页左边距+装订线+栏内偏移）。
    fn col_x(&self) -> f32 {
        self.sect.margin_left + self.sect.gutter + self.cols.get(self.col).map(|c| c.0).unwrap_or(0.0)
    }
    /// 当前栏宽（无栏框时退化为整幅正文宽）。
    fn col_w(&self) -> f32 {
        self.cols.get(self.col).map(|c| c.1).unwrap_or_else(|| self.sect.text_width())
    }
    /// 游标是否在栏顶（0.01 点容差）。
    fn at_top(&self) -> bool {
        self.y <= self.top + 0.01
    }
    /// Move to the next column or page.
    /// 前进到下一栏；已是最后一栏则开新页。
    fn advance(&mut self, block: usize, body_top: f32) {
        if self.col + 1 < self.cols.len() {
            self.col += 1;
            self.y = self.top;
        } else {
            self.new_page(block, body_top);
        }
    }
    /// 当前页（pages 最后一页）的可变引用。
    fn page(&mut self) -> Option<&mut Page> {
        self.pages.last_mut()
    }
}

/// Lay out the whole document.
/// 排版总入口：文档 → 全部页。流程：环境指纹变化则清空缓存 → 预分配脚注/尾注编号 →
/// 逐节驱动 PageBuilder 分页（place_para/place_table）→ 尾注 → 垂直对齐 → 脚注延续页 →
/// Web 页高收口 → 页眉页脚 → 缓存淘汰 → 建反查索引。
pub fn layout(doc: &Document, cache: &mut LayoutCache, opts: &LayoutOptions) -> DocLayout {
    let t0 = now_ms();
    let env = env_hash(doc, opts);
    // 环境（样式/编号/设置/开关）变了：全部段落缓存整体失效。
    if env != cache.env {
        cache.paras.clear();
        cache.env = env;
    }
    // 开始新一轮"本轮用到的键"统计（淘汰依据）。
    cache.used.clear();
    // 脚注/尾注编号先于分页确定：编号不随分页变化（同 Word）。
    let notes = note_numbers(doc, opts.hide_deleted);
    // 编号表排序后哈希，作为缓存键成分（含脚注引用的段落才用到）。
    let notes_hash = hash_of(&{
        let mut v: Vec<_> = notes.iter().map(|(a, b)| (*a, *b)).collect();
        v.sort();
        v
    });
    let mut ctx = Ctx {
        doc,
        cache,
        opts,
        counters: Counters::default(),
        // 页码类域先给初值 1：真实值在分页推进/headers_footers 中回填（预估排版用）。
        fields: FieldCtx {
            page: 1,
            pages: 1,
            section_pages: 1,
            section: 1,
            notes: Arc::new(notes),
            title: doc.core.title.as_str().into(),
            author: doc.core.creator.as_str().into(),
            ..Default::default()
        },
        notes_hash,
        numbers: HashMap::new(),
        eq_count: 0,
        boxes: wordcraft_doc::BoxBudget::default(),
    };
    let sections = doc.sections();
    let web = opts.view != ViewMode::Print;
    let default_sect = SectionProps::default();
    let mut web_sect;
    let first_sect = match sections.first() {
        Some((_, s)) => *s,
        None => &default_sect,
    };
    // Web/草稿视图的"一张长页"特判：克隆第一节属性改出长页节——
    // 页宽取窗口宽（下限 144 点），页边距固定 18 点、无装订线、无分栏；
    // 所有节都沿用这一套属性（节差异只在打印视图生效）。
    let sect_ref = if web {
        web_sect = first_sect.clone();
        web_sect.page_w = opts.web_width.max(144.0);
        web_sect.margin_left = 18.0;
        web_sect.margin_right = 18.0;
        web_sect.margin_top = 18.0;
        web_sect.gutter = 0.0;
        web_sect.columns = Default::default();
        &web_sect
    } else {
        first_sect
    };
    let mut pb = PageBuilder {
        pages: Vec::new(),
        sect: sect_ref,
        sect_idx: 0,
        col: 0,
        cols: Vec::new(),
        y: 0.0,
        top: 0.0,
        bottom: 0.0,
        number: 0,
        web,
        notes: Vec::new(),
        carry: Vec::new(),
        orig_bottom: 0.0,
        prev: None,
        excl: Vec::new(),
        float_tables: Vec::new(),
        line_no: 0,
        page_items_start: 0,
    };
    let mut block = 0usize;
    // 分页主循环：逐节处理，节内的块依次落页/落栏。
    for (si, (end, sect)) in sections.iter().enumerate() {
        let sect: &SectionProps = if web { sect_ref } else { sect };
        pb.sect = sect;
        pb.sect_idx = si;
        // 正文起始 y：页眉可能比上边距高，把正文往下顶。
        let body_top = if web { sect.margin_top } else { body_top_for(&mut ctx, sect, sect.headers.default) };
        // "首页不同"（title_page）时首页正文顶单独按首页页眉计算。
        let first_top = if web || !sect.title_page { body_top } else { body_top_for(&mut ctx, sect, sect.headers.first) };
        let restart = sect.page_num_start;
        // 行号按节重新计数时清零。
        if sect.line_numbers.as_ref().is_some_and(|l| l.restart == wordcraft_doc::section::LineNumberRestart::Section) {
            pb.line_no = 0;
        }
        // 文档第一节强制"另起一页"开始（保证至少有一页）。
        let start = if si == 0 { SectionStart::NextPage } else { sect.start };
        if web && si > 0 {
            // one long page
            // Web/草稿视图：整篇一张长页，后续节不开新页。
        } else if pb.pages.is_empty() || start != SectionStart::Continuous {
            // 非连续节：开新页；页码重编为 n 时先把内部计数器设为 n-1（new_page 会 +1）。
            if let Some(n) = restart {
                pb.number = n.saturating_sub(1);
            }
            pb.new_page(block, first_top);
            // 奇/偶页起始：当前页码奇偶不符时再补一个空页，让节从正确的奇/偶页开始（书刊对页习惯）。
            if !web && matches!(start, SectionStart::EvenPage | SectionStart::OddPage) {
                let want_even = start == SectionStart::EvenPage;
                if pb.number.is_multiple_of(2) != want_even {
                    pb.new_page(block, body_top);
                }
            }
        } else {
            // Continuous: new column layout below the current text.
            // 连续节：不开新页，只在当前文字下方换上本节的栏框接着排。
            pb.cols = sect.column_boxes();
            pb.col = 0;
            pb.top = pb.y;
        }
        // 本节范围内（到下一节起点前）的块逐个落页。
        let last = (*end).min(doc.body.len().saturating_sub(1));
        while block <= last {
            let Some(b) = doc.body.get(block) else { break };
            match &**b {
                Block::Para(p) => place_para(&mut ctx, &mut pb, p, block, body_top),
                Block::Table(t) => place_table(&mut ctx, &mut pb, t, block, body_top),
            }
            block += 1;
        }
    }
    // 空文档也保证至少有一页。
    if pb.pages.is_empty() {
        pb.new_page(0, sect_ref.margin_top);
    }
    // Endnotes after the last paragraph.
    // 尾注排在正文之后：按编号排序，先画一条 144 点分隔线，再逐条排入当前栏（放不下则换栏/页）。
    let endnotes: Vec<u32> = {
        let mut ids: Vec<(u32, u32)> = ctx
            .fields
            .notes
            .iter()
            .filter(|(id, _)| doc.parts.get(id).is_some_and(|p| p.kind == wordcraft_doc::PartKind::Endnote))
            .map(|(a, b)| (*b, *a))
            .collect();
        ids.sort();
        ids.into_iter().map(|(_, id)| id).collect()
    };
    if !endnotes.is_empty() && !web {
        // 与正文留 12 点空隙画分隔线，再留 8 点开始排尾注（144 点短线，同脚注分隔线样式）。
        pb.y += 12.0;
        let (x, ry) = (pb.col_x(), pb.y);
        if let Some(pg) = pb.page() {
            pg.items.push(Placed::Rule {
                x0: x,
                y0: ry,
                x1: x + 144.0,
                y1: ry,
                border: Border { style: wordcraft_doc::props::BorderStyle::Single, width: 0.5, color: None, space: 0.0 },
            });
        }
        pb.y += 8.0;
        for id in endnotes {
            let Some(part) = doc.parts.get(&id) else { continue };
            let blocks = part.blocks.clone();
            let BoxLayout { items, height: h, .. } = layout_box(&mut ctx, StoryRef::Part(id), &blocks, &[], pb.col_w(), None, 0, None);
            if pb.y + h > pb.bottom && !pb.at_top() {
                pb.advance(block, pb.top);
            }
            let (x, y) = (pb.col_x(), pb.y);
            if let Some(pg) = pb.page() {
                for mut it in items {
                    it.translate(x, y);
                    pg.items.push(it);
                }
            }
            pb.y += h;
        }
    }
    // 最后一页的垂直对齐与脚注落位。
    pb.apply_valign();
    pb.flush_notes();
    // Footnotes still continuing after the last page get pages of their own. Each page takes at
    // least a line of the first note, so this ends; the cap is a backstop.
    // 仍在延续的脚注：逐页追加"纯脚注页"直到排完。每页至少消化第一条脚注的一行，必然终止；
    // MAX_NOTE_PAGES 只是兜底上限。
    let last_block = doc.body.len().saturating_sub(1);
    for _ in 0..MAX_NOTE_PAGES {
        if pb.carry.is_empty() {
            break;
        }
        let top = pb.top;
        pb.new_page(last_block, top);
        pb.flush_notes();
    }
    // 兜底清理：万页上限用尽时放弃残余延续脚注。
    pb.carry.clear();
    let mut pages = pb.pages;
    // Web/草稿视图收口：把"长页"的巨大占位页高改为实际内容底 + 36 点余量。
    if web && let Some(p) = pages.first_mut() {
        let bottom = p
            .items
            .iter()
            .filter_map(|it| if let Placed::Lines { y, para, l0, l1, .. } = it { item_bottom(*y, para, *l0, *l1) } else { None })
            .fold(0.0f32, f32::max);
        p.h = bottom + 36.0;
    }
    // 打印视图：为每页补排页眉页脚（PAGE 等域在此拿到真实取值）。
    if !web {
        headers_footers(&mut ctx, &mut pages, &sections);
    }
    // Evict paragraph layouts that weren't used this pass if the cache grew large.
    // 缓存淘汰策略：条目数超过"本轮用到的键数（下限 1024）的 4 倍"时，
    // 只保留本轮用过的键——既防无限增长，又给文档翻页/滚动留出复用余量。
    if ctx.cache.paras.len() > 4 * ctx.cache.used.len().max(1024) {
        let used = std::mem::take(&mut ctx.cache.used);
        ctx.cache.paras.retain(|k, _| used.contains(k));
        ctx.cache.used = used;
    }
    // 反查索引：(story, 段落路径) → 各片所在 (页, 条目号)，供命中测试与编辑定位。
    let mut index: HashMap<(StoryRef, Path), Vec<(usize, usize)>> = HashMap::new();
    for (pi, p) in pages.iter().enumerate() {
        for (ii, it) in p.items.iter().enumerate() {
            if let Placed::Lines { story, path, .. } = it {
                index.entry((*story, path.clone())).or_default().push((pi, ii));
            }
        }
    }
    DocLayout { pages, index, ms: now_ms() - t0, text_boxes: ctx.boxes.total() }
}

/// 行片 (l0..l1) 放置在 y 时的底边（相对其自身放置原点）。
fn item_bottom(y: f32, para: &ParaLayout, l0: usize, l1: usize) -> Option<f32> {
    let f = para.lines.get(l0)?;
    let l = para.lines.get(l1.checked_sub(1)?)?;
    Some(y + l.top + l.height - f.top)
}

/// Where body text starts: the top margin, or below the header if it's taller.
/// 正文起始 y：页眉存在且排出来高过上边距时，正文紧贴页眉之下开始；否则取页顶边距。
fn body_top_for(ctx: &mut Ctx, sect: &SectionProps, header: Option<u32>) -> f32 {
    let Some(id) = header else { return sect.margin_top };
    let Some(part) = ctx.doc.parts.get(&id) else { return sect.margin_top };
    let blocks = part.blocks.clone();
    // Placed as `headers_footers` draws it, so page-relative floats wrap the same way.
    let frame = PageFrame { sect, origin: (sect.margin_left + sect.gutter, sect.header) };
    let h = layout_box(ctx, StoryRef::Part(id), &blocks, &[], sect.text_width(), None, 0, Some(frame)).height;
    // Word starts the body right below a header that reaches past the top margin, no gap.
    sect.margin_top.max(sect.header + h)
}

/// Where a box of laid-out items sits on its page, for floating objects positioned relative to
/// the page or its margins.
/// 盒子在页面上的定位框架：浮动对象按页面/页边距定位时经它换算进盒内坐标。
#[derive(Clone, Copy)]
struct PageFrame<'a> {
    sect: &'a SectionProps,
    /// Page position of the box's (0, 0); (0, 0) for body text.
    /// 盒子原点 (0,0) 的页面坐标；正文盒为 (0,0)。
    origin: (f32, f32),
}

/// A floating (not inline) picture, chart or shape: its size and placement.
/// 取出浮动（非嵌入式）对象的尺寸与定位描述；嵌入式对象返回 None。
fn floating(o: &InlineObject) -> Option<(f32, f32, &Float)> {
    o.frame().filter(|(_, _, float)| float.wrap != Wrap::Inline)
}

/// Where a floating object goes, in the coordinates of the box `frame` places on the page.
/// `col` is the column (or cell) left edge and width, `y0` the anchor paragraph's top. Without a
/// frame (table cells, text boxes, notes) positions relative to the page or margins fall back
/// to the box itself.
/// 浮动对象的落位矩形（frame 所定盒子的坐标系内）：col 为栏（或单元格）的左缘与宽，y0 为锚段落顶。
/// 没有 frame（表格单元格、文本框、脚注）时，相对页面/页边距的定位退化为以盒子自身为参照。
fn float_rect(frame: Option<PageFrame>, col: (f32, f32), y0: f32, w: f32, h: f32, float: &Float) -> Rect {
    // 防御：宽高夹进 1..4000 点，病态数据不产生超大对象。
    let (w, h) = (w.clamp(1.0, 4000.0), h.clamp(1.0, 4000.0));
    // 每个轴：参照区域的 (起点, 范围)，页面坐标；有 frame 时减去盒原点换算进盒内坐标。
    // Each axis: the reference area's start and extent, in page coordinates.
    let page_x = |s: &SectionProps| {
        let left = s.margin_left + s.gutter;
        match float.h_rel {
            Anchor::Page => Some((0.0, s.page_w)),
            Anchor::Margin => Some((left, s.text_width())),
            Anchor::LeftMargin | Anchor::InsideMargin => Some((0.0, left)),
            Anchor::RightMargin | Anchor::OutsideMargin => Some((s.page_w - s.margin_right, s.margin_right)),
            _ => None,
        }
    };
    let page_y = |s: &SectionProps| match float.v_rel {
        Anchor::Page => Some((0.0, s.page_h)),
        Anchor::Margin => Some((s.margin_top, s.page_h - s.margin_top - s.margin_bottom)),
        Anchor::TopMargin | Anchor::InsideMargin => Some((0.0, s.margin_top)),
        Anchor::BottomMargin | Anchor::OutsideMargin => Some((s.page_h - s.margin_bottom, s.margin_bottom)),
        _ => None,
    };
    let hx = frame.and_then(|f| page_x(f.sect).map(|(a, e)| (a - f.origin.0, e))).unwrap_or(col);
    let vy = frame.and_then(|f| page_y(f.sect).map(|(a, e)| (a - f.origin.1, e))).unwrap_or((y0, 0.0));
    Rect::new(align_in(hx, float.x, w, float.h_align), align_in(vy, float.y, h, float.v_align), w, h)
}

/// Where an object of `size` starts on one axis of `area` (start, extent): at `offset` from the
/// area's start, or aligned in it when `align` is set.
/// 对象在单一轴向上的起点：参照区 (start, extent) 内，有 align 则区内对齐（起点/居中/终点），
/// 否则取 start + offset。
fn align_in((start, extent): (f32, f32), offset: f32, size: f32, align: Option<FloatAlign>) -> f32 {
    match align {
        // 防御：偏移夹进 ±31680 点（=440 英寸），NaN 按 0 处理，防病态数据炸坐标。
        None => start + if offset.is_finite() { offset.clamp(-31_680.0, 31_680.0) } else { 0.0 },
        Some(FloatAlign::Start | FloatAlign::Inside) => start,
        Some(FloatAlign::Center) => start + (extent - size) / 2.0,
        Some(FloatAlign::End | FloatAlign::Outside) => start + extent - size,
    }
}

/// The area text keeps clear of around a floating object at `r` (its rectangle grown by the
/// distances from text), and whether text may only go above and below it. `None` when text
/// flows over or under the object.
/// 浮动对象 r 周围文字要避让的区域（矩形向四周外扩"与文字的距离"），以及是否只允许上下环绕
/// （TopAndBottom=true，文字不能从左右两侧排）。文字从对象上/下方流过时返回 None。
fn wrap_area(r: Rect, float: &Float) -> Option<(Rect, bool)> {
    if matches!(float.wrap, Wrap::Inline | Wrap::BehindText | Wrap::InFrontOfText) {
        return None;
    }
    // 防御：与文字的距离夹进 0..1584 点（1584 点 = 22 英寸，Word 页面尺寸上限），NaN 按 0。
    let d = |v: f32| if v.is_finite() { v.clamp(0.0, 1584.0) } else { 0.0 };
    let (side, top, bottom) = (d(float.dist), d(float.dist_top), d(float.dist_bottom));
    // 旋转过的对象按其旋转后的包围盒避让。
    // A rotated object keeps text clear of its rotated bounds.
    let r = float.spin().bounds(r);
    Some((Rect::new(r.x - side, r.y - top, r.w + side * 2.0, r.h + top + bottom), float.wrap == Wrap::TopAndBottom))
}

/// Wrap areas relative to a paragraph whose column starts at `x0` and whose top is `y0`.
/// 把页面坐标的避让区换算成相对段落的（栏左缘 x0、段顶 y0 起算），供 para 断行绕排使用；
/// 底边不在段落之下的区（bottom<=0）过滤掉。
fn rel_exclusions(excl: &[(Rect, bool)], x0: f32, y0: f32) -> Vec<para::Exclusion> {
    excl.iter()
        .map(|(r, tb)| para::Exclusion { top: r.y - y0, bottom: r.bottom() - y0, left: r.x - x0, right: r.right() - x0, top_bottom: *tb })
        .filter(|e| e.bottom > 0.0)
        .collect()
}

/// The items drawing floating object `o` at `rect` (a group: its members), turned by its own
/// spin inside `outer` (its group's, about `outer`'s centre point).
/// 在 rect 处绘制浮动对象 o 的条目（组合对象则递归展开成员）；自身旋转发生在 outer
/// （所在组合的旋转，绕组合中心）之内，成员的实际旋转为两者复合。
fn float_items(o: &InlineObject, rect: Rect, outer: (Spin, Point), story: StoryRef, path: &[u32], off: usize) -> Vec<Placed> {
    let own = o.frame().map(|(_, _, f)| f.spin()).unwrap_or_default();
    let (ospin, oc) = outer;
    let rect = carried(rect, ospin, oc);
    let spin = own.within(ospin);
    match o {
        InlineObject::Image { media, crop, .. } => {
            vec![Placed::Image { rect, media: media.clone(), crop: *crop, story, path: Path(path.to_vec()), off, spin }]
        }
        InlineObject::Shape { kind, fill, stroke, stroke_width, effects, freeform, .. } => {
            vec![Placed::Shape {
                rect,
                kind: *kind,
                fill: *fill,
                stroke: *stroke,
                stroke_width: *stroke_width,
                effects: *effects,
                freeform: freeform.clone(),
                spin,
            }]
        }
        InlineObject::Graphic { graphic, .. } => {
            vec![Placed::Graphic { rect, graphic: graphic.clone(), story, path: Path(path.to_vec()), off, spin }]
        }
        InlineObject::Group { .. } => {
            let centre = Point::new(rect.x + rect.w / 2.0, rect.y + rect.h / 2.0);
            group_members(o, rect).into_iter().flat_map(|(r, c)| float_items(c, r, (spin, centre), story, path, off)).collect()
        }
        _ => Vec::new(),
    }
}

/// A group's members, each with its rectangle when the group is at `rect`.
/// `rect` moved (not turned) to where `spin` about `centre` takes its centre: a group member's
/// frame inside its turned group.
/// rect 平移（不旋转）到"spin 绕 centre 旋转后其中心所在的位置"：组合成员的框在已旋转组合内
/// 的承载位置（成员各自的自转另计，见 float_items）。
fn carried(rect: Rect, spin: Spin, centre: Point) -> Rect {
    let (cx, cy) = (rect.x + rect.w / 2.0, rect.y + rect.h / 2.0);
    let (nx, ny) = spin.apply(centre.x, centre.y, cx, cy);
    Rect::new(rect.x + nx - cx, rect.y + ny - cy, rect.w, rect.h)
}

/// 组合对象在 rect 处的成员列表：(成员矩形, 成员对象)。
fn group_members(o: &InlineObject, rect: Rect) -> Vec<(Rect, &InlineObject)> {
    o.group_rects(rect.x, rect.y, rect.w, rect.h).into_iter().map(|([x, y, w, h], c)| (Rect::new(x, y, w, h), c)).collect()
}

/// Position the floating objects anchored in `p` for a paragraph whose text starts at `y0` in
/// the current column, add their wrap areas to the page (replacing any from an earlier attempt,
/// from `excl_mark` on) and lay the paragraph out around them, with its list `label`.
/// 为锚在段落 p 上的浮动对象定位（假设段落文字从当前栏 y0 处开始），把绕排区并入页面避让表
/// （从 excl_mark 起截断，覆盖上一次尝试的残留），再带着绕排约束（列表 label）排段落。
/// 返回 (对象下标 → 矩形, 段落布局)。
fn anchor_floats(
    ctx: &mut Ctx,
    pb: &mut PageBuilder,
    p: &Paragraph,
    y0: f32,
    excl_mark: usize,
    label: &Option<(String, Level)>,
) -> (HashMap<usize, Rect>, Arc<ParaLayout>) {
    // 回滚到上次尝试的标记：段落换位重排时丢弃旧的绕排区。
    pb.excl.truncate(excl_mark);
    // 页码类域按当前页取值（影响含 PAGE 域段落的排版与缓存键）。
    ctx.fields.page = pb.number;
    let (col_x, width) = (pb.col_x(), pb.col_w());
    let mut rects = HashMap::new();
    // Web/草稿视图不生成绕排区：浮动对象照常绘制，只是正文不让位（一张长页无分页绕排需求）。
    if !pb.web {
        let frame = PageFrame { sect: pb.sect, origin: (0.0, 0.0) };
        // 被丢弃的对象（隐藏/修订删除）不定位，也就不占位。
        // An object left out of the layout (hidden, or deleted in the final text) is not placed, so
        // it takes no room either.
        let left_out = left_out_objects(ctx.doc, p, None, ctx.opts.show_hidden, ctx.opts.hide_deleted);
        for (oi, o) in p.objects.iter().enumerate() {
            let Some((w, h, float)) = floating(o) else { continue };
            if left_out(oi) {
                continue;
            }
            let r = float_rect(Some(frame), (col_x, width), y0, w, h, float);
            rects.insert(oi, r);
            if let Some(area) = wrap_area(r, float) {
                pb.excl.push(area);
            }
        }
    }
    let rel = rel_exclusions(&pb.excl, col_x, y0);
    (rects, ctx.para_labelled(p, width, None, &rel, label.clone()))
}

/// Where a paragraph's floating objects are positioned from, when they aren't placed already:
/// the box on its page, the column (left, width) and the paragraph's top. Also where offsets
/// relative to the column and paragraph start, for dragging.
/// 段落浮动对象的定位参照（尚未放置时）：所在页框、栏 (左缘,宽) 与段落顶；也是相对栏/相对段落
/// 偏移的起算点（供拖拽换算）。
#[derive(Clone, Copy)]
struct ObjFrame<'a> {
    /// 页面定位框（None=以盒子自身为参照）。
    page: Option<PageFrame<'a>>,
    /// 栏 (左缘, 宽)。
    col: (f32, f32),
    /// 段落顶 y。
    para_y: f32,
    /// A table's character formatting under the paragraph's (see `left_out_objects`).
    /// 段落之下的表样字符格式（单元格内段落用，见 left_out_objects）。
    table_chr: Option<&'a CharProps>,
}

/// The pictures, shapes and text boxes in lines `l0..l1` of paragraph `p` (story `story`, at
/// `path`), whose lines start at (x, y): floating ones drawn, every one's area (hit testing,
/// selection) and text boxes' text. Floating rects come from `floats` (by object index) or are
/// worked out from `at`. Returns what goes behind the text, and what goes in front.
/// 排出段落 p（story，路径 path）第 l0..l1 行（行片起点 (x,y)）内的对象：浮动的绘制出来；
/// 每个对象都生成 Object 区（供命中测试/选择手柄/拖拽）；文本框的文字排进框内。
/// 浮动矩形优先取 floats（按对象下标），没有则由 at 现算。返回 (衬于文字下方者, 浮于文字上方者)。
#[allow(clippy::too_many_arguments)]
fn place_objects(
    ctx: &mut Ctx,
    story: StoryRef,
    path: &[u32],
    p: &Paragraph,
    pl: &ParaLayout,
    (l0, l1): (usize, usize),
    (x, y): (f32, f32),
    at: &ObjFrame,
    floats: &HashMap<usize, Rect>,
    depth: usize,
) -> (Vec<Placed>, Vec<Placed>) {
    let (mut behind, mut front) = (Vec::new(), Vec::new());
    let Some(fl) = pl.lines.get(l0) else { return (behind, front) };
    // 被丢弃的对象（隐藏/修订删除）不排。
    // Objects left out of the layout (hidden, or deleted in the final text) aren't placed.
    let left_out = left_out_objects(ctx.doc, p, at.table_chr, ctx.opts.show_hidden, ctx.opts.hide_deleted);
    for li in l0..l1 {
        let Some(line) = pl.lines.get(li) else { continue };
        for k in line.c0..line.c1 {
            let Some(c) = pl.clusters.get(k) else { continue };
            let para::ClKind::Object(oi) = c.kind else { continue };
            if left_out(oi) {
                continue;
            }
            let Some(obj) = p.objects.get(oi) else { continue };
            let Some((w, h, float)) = obj.frame() else { continue };
            let floating = float.wrap != Wrap::Inline;
            let rect = if floating {
                floats.get(&oi).copied().unwrap_or_else(|| float_rect(at.page, at.col, at.para_y, w, h, float))
            } else {
                let cx = x + line.cl_left(k).unwrap_or(0.0);
                display::inline_rect(Some(obj), cx, y + (line.baseline - fl.top), c.adv, c.obj_h)
            };
            let layer = if float.wrap == Wrap::BehindText { &mut behind } else { &mut front };
            // 衬于文字下方的进 behind，其余进 front；浮动对象在此绘制，嵌入式对象随其所在行绘制。
            // Floating ones are drawn here; inline ones with their line.
            if floating {
                layer.extend(float_items(obj, rect, (Spin::default(), Point::default()), story, path, c.start));
            }
            let text_box = match obj {
                InlineObject::Shape { story: Some(id), .. } if ctx.doc.parts.get(id).is_some_and(|p| p.kind == wordcraft_doc::PartKind::TextBox) => {
                    Some(*id)
                }
                _ => None,
            };
            front.push(Placed::Object {
                rect,
                spin: float.spin(),
                story,
                path: Path(path.to_vec()),
                off: c.start,
                text_box,
                wrap: float.wrap,
                origin: Point::new(at.col.0, at.para_y),
            });
            // 排出文本框的文字（组合：其成员文本框的，各自随组合旋转承载，文字本身保持正立）；
            // 盒子预算超限（盒子套自己/嵌套过深/数量过多）时跳过。
            // Its text (a group: its text boxes', each where the group's turn carries it; the
            // text itself stays upright), unless the box budget says no (a box inside itself,
            // too deep, too many).
            let centre = Point::new(rect.x + rect.w / 2.0, rect.y + rect.h / 2.0);
            let boxes: Vec<(u32, Rect)> = match obj {
                InlineObject::Group { .. } => group_members(obj, rect)
                    .into_iter()
                    .filter_map(|(r, c)| {
                        c.text_box()
                            .filter(|id| ctx.doc.parts.get(id).is_some_and(|p| p.kind == wordcraft_doc::PartKind::TextBox))
                            .map(|id| (id, carried(r, float.spin(), centre)))
                    })
                    .collect(),
                _ => text_box.map(|id| (id, rect)).into_iter().collect(),
            };
            for (id, rect) in boxes {
                if !ctx.boxes.enter(id) {
                    continue;
                }
                let blocks = ctx.doc.parts.get(&id).map(|p| p.blocks.clone()).unwrap_or_default();
                let inner = layout_box(ctx, StoryRef::Part(id), &blocks, &[], (rect.w - 2.0 * BOX_INSET_X).max(12.0), None, depth + 1, None).items;
                ctx.boxes.leave();
                let layer = if float.wrap == Wrap::BehindText { &mut behind } else { &mut front };
                // 装不进框的文字直接隐藏（同 Word 的文本框裁剪），按内边距平移进框。
                // Text that doesn't fit inside the margins is hidden, as in Word.
                for mut it in fit_box(inner, rect.h - 2.0 * BOX_INSET_Y) {
                    it.translate(rect.x + BOX_INSET_X, rect.y + BOX_INSET_Y);
                    layer.push(it);
                }
            }
        }
    }
    (behind, front)
}

/// Footnotes referenced in a paragraph laid out as `pl`: in its text, and inside its text boxes
/// (on the box's cluster), as (cluster, note id).
/// 段落按 pl 排版后引用的全部脚注：正文行内的（pl.notes），加上其文本框里的（挂在文本框对象
/// 所在的簇上）。返回 (簇下标, 笔记 id)。
fn para_notes(doc: &Document, p: &Paragraph, pl: &ParaLayout) -> Vec<(usize, u32)> {
    let mut notes = pl.notes.clone();
    if p.objects.iter().any(|o| !o.text_boxes().is_empty()) {
        for (ci, c) in pl.clusters.iter().enumerate() {
            if let para::ClKind::Object(oi) = c.kind
                && let Some(o) = p.objects.get(oi)
            {
                for id in o.text_boxes() {
                    notes.extend(doc.notes_in_text_box(id).into_iter().map(|n| (ci, n)));
                }
            }
        }
    }
    notes
}

/// 把一个正文段落落进当前页/栏（分页最复杂的函数）：先按当前游标预放浮动对象试排；
/// 处理"段前分页"、上下文间距与 keep-lines/keep-next 整体移栏移页；落点变化则就地重放浮动
/// 对象重排；再逐行适配页底——本行引用的脚注压入页底（放不下时切分、余部下页续）、
/// 孤行寡行控制、绕排行不拆；最后落条目与行号，推进游标并执行强制分页/分栏。
fn place_para(ctx: &mut Ctx, pb: &mut PageBuilder, p: &Paragraph, block: usize, body_top: f32) {
    let width = pb.col_w();
    // 锚在本段的浮动对象先并入页面绕排区、再排文字；若段落最终落到别处（换栏/换页，或
    // 上下文间距吃掉段前距使游标移动），就按新位置重放一次。
    // Floating objects anchored here join the page's wrap areas before the text is laid out; if
    // the paragraph then starts somewhere else (another column or page, or contextual spacing
    // drops its space before), they are placed again there.
    let space_before = ctx.doc.styles.resolve_para(&p.props).space_before;
    let excl_mark = pb.excl.len();
    let guess = pb.y + space_before;
    let label = ctx.next_label(p);
    let (mut float_rects, mut pl) = anchor_floats(ctx, pb, p, guess, excl_mark, &label);
    // 浮动对象定位时所用的锚段落顶（后续以它判断是否需要重放）。
    // The anchor paragraph's top the floats were placed from.
    let mut anchor_y = guess;
    let spot = |pb: &PageBuilder| (pb.pages.len(), pb.col);
    let placed_at = spot(pb);
    // "段前分页"：不在栏顶才生效（在栏顶等于已经分过页），web 长页不分页。
    if pl.rp.page_break_before && !pb.at_top() && !pb.web {
        pb.new_page(block, body_top);
    }
    // 上下文间距：同风格且不在栏顶时，按双方要求吃掉上段段后距/本段段前距。
    // Contextual spacing: no space between paragraphs of the same style when either asks for it.
    let mut before = pl.rp.space_before;
    if let Some((style, ctxl, after)) = pb.prev.take()
        && style == pl.rp.style
        && !pb.at_top()
    {
        if ctxl {
            pb.y -= after;
        }
        if pl.rp.contextual_spacing {
            before = 0.0;
        }
    }
    pb.y += before;
    // 段中不分页/与下段同页：当前栏放不下、而空栏（从栏顶起）放得下时，整体移到下一栏/页。
    // Keep lines together: if it doesn't fit but would on an empty column, move it.
    if pl.rp.keep_lines || pl.rp.keep_next {
        // need = 段落全部行高；keep-next 时加上下一段首行的高度（含其段前距）。
        let need = pl.height + if pl.rp.keep_next { next_first_line(ctx, block, width) } else { 0.0 };
        if pb.y + need > pb.bottom && !pb.at_top() && need <= pb.bottom - pb.top {
            pb.advance(block, body_top);
            pb.y += pl.rp.space_before.min(0.0);
        }
    }
    // 落点变了（换栏/换页，或游标移动）且页上有绕排区：按新位置重放浮动对象并重排段落。
    // A different top only matters when there is something to wrap around.
    if spot(pb) != placed_at || ((pb.y - guess).abs() > 0.01 && !pb.excl.is_empty()) {
        let y0 = pb.y;
        (float_rects, pl) = anchor_floats(ctx, pb, p, y0, excl_mark, &label);
        anchor_y = y0;
    }
    // 行片主循环：每轮把行 [l0..l1) 落进当前栏；放不下/强制分页则推进后继续排余下行。
    let mut n = pl.lines.len();
    let mut l0 = 0;
    while l0 < n {
        // 本栏还能放下多少行？
        // How many lines fit?
        let Some(first) = pl.lines.get(l0) else { break };
        // 段落首行的行顶可能低于段顶（被浮动对象压下去），lead 是这段下坠量（仅首行片有）。
        // A paragraph's first line may start below its top (pushed down by a floating object).
        let lead = if l0 == 0 { first.top.max(0.0) } else { 0.0 };
        let mut l1 = l0;
        let mut forced_break = None;
        // 本片新收的脚注（尚未计入页底占用）。
        let mut new_notes: Vec<NoteBox> = Vec::new();
        // 段落全部脚注引用（含文本框内的）：(簇下标, 笔记 id)。
        let notes = para_notes(ctx.doc, p, &pl);
        while l1 < n {
            let Some(l) = pl.lines.get(l1) else { break };
            // 本行引用的脚注排出来，等着落本页页底（同页不重复收）。
            // Footnotes referenced on this line go to the bottom of this page.
            let mut line_notes = Vec::new();
            if !pb.web {
                for (ci, id) in &notes {
                    if *ci >= l.c0
                        && *ci < l.c1
                        && !pb.notes.iter().chain(new_notes.iter()).any(|x| x.id == *id)
                        && let Some(part) = ctx.doc.parts.get(id).filter(|p| p.kind == wordcraft_doc::PartKind::Footnote)
                    {
                        let blocks = part.blocks.clone();
                        let BoxLayout { items, height: h, .. } =
                            layout_box(ctx, StoryRef::Part(*id), &blocks, &[], pb.sect.text_width(), None, 0, None);
                        line_notes.push(NoteBox { id: *id, items, h, cont: false });
                    }
                }
            }
            // 脚注区占用：本行新增脚注高度；本页第一张脚注还要加上分隔条高度。
            let extra: f32 = line_notes.iter().map(|n| n.h).sum::<f32>()
                + if pb.notes.is_empty() && new_notes.is_empty() && !line_notes.is_empty() { NOTE_SEP } else { 0.0 };
            // 本行能用的页底上限 = 压缩后的正文底 − 已收脚注（+分隔条）− 本行新增脚注。
            let limit = pb.bottom
                - new_notes.iter().map(|n| n.h).sum::<f32>()
                - if pb.notes.is_empty() && !new_notes.is_empty() { NOTE_SEP } else { 0.0 }
                - extra;
            // 本行的行底。
            let bottom = pb.y + lead + (l.top + l.height - first.top);
            // 超长脚注顺延下页：只要"此前脚注 + 最后一张的首段"能塞进页底，本行仍算放得下，
            // 脚注占据行下与页底之间的剩余空间，余部下页续。
            // Notes too long for the rest of the page continue on the next one: the line still
            // fits if the notes before its last one, and that one's first line, fit below it.
            // The notes then take the rest of the page.
            let split = bottom > limit + 0.01
                && line_notes.split_last().is_some_and(|(last, others)| {
                    let before: f32 = pb.notes.iter().chain(&new_notes).chain(others).map(|n| n.h).sum();
                    pb.body_floor().max(bottom) + NOTE_SEP + before + last.first_piece() <= pb.orig_bottom + 0.01
                });
            // 放不下且本栏已收到行：停在这里（余行下页）。
            if bottom > limit + 0.01 && l1 > l0 && !split {
                break;
            }
            // 放不下且一行未收、又不在栏顶：整段推到下页（脚注可切分时除外）。
            if bottom > limit + 0.01 && l1 == l0 && !pb.at_top() && !split {
                break;
            }
            new_notes.extend(line_notes);
            l1 += 1;
            // 行以强制分页/分栏结束：记下断点，行片到此为止。
            if matches!(l.end, LineEnd::PageBreak | LineEnd::ColumnBreak) && !pb.web {
                forced_break = Some(l.end);
                break;
            }
            // 脚注切分成立：本行收入，但页底脚注区已满，行片到此收口（其余脚注全部顺延）。
            if split {
                break;
            }
        }
        // 孤行/寡行控制（段首段尾各至少 2 行；强制分页不受控）。
        // Widow/orphan control (2-line minimum at either end).
        if forced_break.is_none() && l1 < n && pl.rp.widow_control && n >= 2 {
            // 段首孤行：本栏只放得下 1 行 → 撤回，让整段（至少 2 行）从下页起。
            if l1 - l0 == 1 && l0 == 0 && !pb.at_top() {
                l1 = l0; // orphan: move the first line to the next page
            // 段尾寡行：下页将只剩 1 行 → 本页多带一行下去。
            } else if n - l1 == 1 && l1 - l0 >= 3 {
                l1 -= 1; // widow: take one more line along
            }
        }
        // 绕排行（文字排在浮动对象两侧的行）不可拆：行片若停在 beside 行上，回退一行。
        // A row (text both sides of a floating object) stays together.
        while l1 > l0 && l1 < n && pl.lines.get(l1).is_some_and(|l| l.beside) {
            l1 -= 1;
        }
        // 一行都放不下：栏顶（含空页）则硬放一行保证推进；否则换栏/页后重试。
        if l1 == l0 {
            if pb.at_top() {
                // 空页也放不下哪怕一行：硬放（溢出）。
                l1 = l0 + 1; // can't fit even one line on an empty page: overflow
            } else {
                pb.advance(block, body_top);
                // 首片换栏/页后：浮动对象按新位置重放，段落重排（行数可能变化）。
                if l0 == 0 {
                    let y0 = pb.y;
                    (float_rects, pl) = anchor_floats(ctx, pb, p, y0, excl_mark, &label);
                    anchor_y = y0;
                    n = pl.lines.len();
                }
                continue;
            }
        }
        // 只保留已落行引用的脚注（被孤行控制/绕排回退掉的行，其脚注撤回）。
        // Keep only the notes of the lines that are placed.
        let placed_end = pl.lines.get(l1.saturating_sub(1)).map(|l| l.c1).unwrap_or(0);
        new_notes.retain(|n| notes.iter().any(|(ci, nid)| *nid == n.id && *ci < placed_end));
        // 收下本片的脚注，并按其总高压缩本页正文底（脚注区从页底向上生长）。
        let before = pb.notes_h();
        pb.notes.extend(new_notes);
        pb.bottom -= pb.notes_h() - before;
        let x = pb.col_x();
        let y = pb.y + lead;
        // 文字底越过压缩后的正文底（超长脚注或上页延续的余部）：正文底下推到文字底
        // （不越过原底），脚注挤到文字之下，放不下的部分下页续。
        // Notes that no longer fit below the text (a long one, or the rest of one continued from
        // the page before) get what's left under it and continue on the next page.
        if !pb.notes.is_empty()
            && !pb.web
            && let Some(placed_bottom) = item_bottom(y, &pl, l0, l1)
            && placed_bottom > pb.bottom + 0.01
        {
            pb.bottom = pb.body_floor().max(placed_bottom).min(pb.orig_bottom);
        }
        let mut items = Vec::new();
        // 行片连同底纹、边框压入条目表。
        push_para(&mut items, StoryRef::Body, &[block as u32], &pl, l0, l1, x, y, width);
        // 本片各行里的图片/形状/文本框（web 视图：浮动对象按本片位置现算放置）。
        // Pictures, shapes and text boxes in these lines (web view: floats placed by this piece).
        let at = ObjFrame {
            page: Some(PageFrame { sect: pb.sect, origin: (0.0, 0.0) }),
            col: (x, width),
            para_y: if pb.web { y } else { anchor_y },
            table_chr: None,
        };
        let (behind, front) = place_objects(ctx, StoryRef::Body, &[block as u32], p, &pl, (l0, l1), (x, y), &at, &float_rects, 0);
        items.extend(front);
        // 行号：写在正文左缘之外；Header 样式段落与被抑制的段落不编号（web 长页无行号）。
        // Line numbers in the left margin.
        if let Some(ln) = pb.sect.line_numbers.clone().filter(|_| !pb.web && pl.rp.style != "Header" && p.props.suppress_line_numbers != Some(true))
            && let Some(first) = pl.lines.get(l0)
        {
            for li in l0..l1 {
                let Some(line) = pl.lines.get(li).filter(|l| !l.beside) else { continue };
                pb.line_no = pb.line_no.saturating_add(1);
                // 显示值 = 行计数 + 起始号 − 1。
                let n = pb.line_no.saturating_add(ln.start.saturating_sub(1));
                // 每 count_by 行显示一个：非倍数的行跳过（但仍计数）。
                if ln.count_by > 1 && !n.is_multiple_of(ln.count_by) {
                    continue;
                }
                let num = ctx.number_para(n);
                let w = num.lines.first().and_then(|l| l.xs.last().copied()).unwrap_or(10.0);
                let dist = if ln.distance > 0.0 { ln.distance } else { 18.0 };
                let ly = y + (line.top - first.top) + (line.baseline - line.top) - num.lines.first().map(|l| l.baseline).unwrap_or(10.0);
                items.push(Placed::Lines {
                    story: StoryRef::Part(u32::MAX),
                    path: Path(vec![]),
                    para: num.clone(),
                    l0: 0,
                    l1: 1,
                    x: pb.sect.margin_left + pb.sect.gutter - dist - w,
                    y: ly,
                    turn: TextDirection::Horizontal,
                });
            }
        }
        // 衬底图片插到页条目最前（保持被文字覆盖的层序），其余条目追加。
        if let Some(pg) = pb.page() {
            for (i, it) in behind.into_iter().enumerate() {
                pg.items.insert(i.min(pg.items.len()), it);
            }
            pg.items.extend(items);
        }
        if let (Some(f), Some(l)) = (pl.lines.get(l0), pl.lines.get(l1 - 1)) {
            pb.y = y + (l.top + l.height - f.top);
        }
        l0 = l1;
        // 行片收口后的推进：强制分页→新页；强制分栏→下一栏；还有余行→下一栏/页。
        match forced_break {
            Some(LineEnd::PageBreak) => pb.new_page(block, body_top),
            Some(LineEnd::ColumnBreak) => pb.advance(block, body_top),
            _ if l0 < n => pb.advance(block, body_top),
            _ => {}
        }
    }
    // 游标推进段后距，并记录本段信息供下一段的上下文间距判断。
    pb.y += pl.rp.space_after;
    pb.prev = Some((pl.rp.style.clone(), pl.rp.contextual_spacing, pl.rp.space_after));
}

/// Height of the first line of the block after `block` (for keep-with-next).
/// block 之后那块的首行高度（keep-with-next 用）：下一段现排一次量高（label 传 None，不推进
/// 列表计数器）；下一个是表格按 24 点估；没有下一块为 0。
/// 注意：这里直接调 para::layout_para，绕过缓存（每次 keep-next 判断都会重排一遍）。
fn next_first_line(ctx: &mut Ctx, block: usize, width: f32) -> f32 {
    match ctx.doc.body.get(block + 1).map(|b| &**b) {
        Some(Block::Para(p)) => {
            // 预看不推进列表编号：不带 label 排版。
            // Don't advance list counters for a lookahead: lay out without the label.
            let rp = ctx.doc.styles.resolve_para(&p.props);
            let env = para::ParaEnv {
                doc: ctx.doc,
                width,
                label: None,
                fields: &ctx.fields,
                show_hidden: ctx.opts.show_hidden,
                hide_deleted: ctx.opts.hide_deleted,
                table: None,
                proofing: false,
                exclusions: &[],
                eq_number: ctx.eq_count,
            };
            let _ = rp;
            let pl = para::layout_para(p, &env);
            pl.rp.space_before + pl.lines.first().map(|l| l.height).unwrap_or(0.0)
        }
        Some(Block::Table(_)) => 24.0,
        None => 0.0,
    }
}

/// 把一个表格落进当前页/栏：浮动表（tblpPr）整体定位、不占文流（整表超过一页高时退化为
/// 文流表跨页）；文流表先沉到挡路浮动对象之下，再逐行落位——可拆行按页底切分，整行放不下
/// 推下页并在新页重复表头行，空页也放不下的行同 Word 硬放、伸进下边距。
fn place_table(ctx: &mut Ctx, pb: &mut PageBuilder, t: &wordcraft_doc::Table, block: usize, body_top: f32) {
    // 表格打断上下文间距的链（下一段不再与"上一段"发生上下文间距关系）。
    pb.prev = None;
    let width = pb.col_w();
    let mut tl = table::layout_table(ctx, StoryRef::Body, t, &[block as u32], width, 1);
    // 浮动表（web 长页不分页，按文流表处理）。
    if let Some(f) = t.props.float.filter(|_| !pb.web) {
        // Word 2013 前排版（兼容模式 <15）：偏移定位的是首格文字，表缘要再退一个单元格边距。
        // Before Word 2013 layout (compatibility mode 15) an offset places the first cell's text,
        // so the edge sits a cell margin further out.
        let legacy = if ctx.doc.settings.compat_mode < 15 { table::first_cell_left_margin(ctx, t) } else { 0.0 };
        let h: f32 = tl.rows.iter().map(|r| r.height).sum();
        // 整表不超过一页高：按浮动表定位，收工。
        if h <= pb.bottom - pb.top + 0.01 {
            place_floating_table(pb, &tl, &f, legacy, block, body_top);
            return;
        }
        // 整表超过一页高：同 Word，让它像文流表一样跨页，从自身左缘起排。
        // Too tall for a page: Word lets it run across pages like a table in the text, from its
        // own left edge.
        tl.x = float_table_x(pb, tl.width, &f, legacy) - pb.col_x();
    }
    // 文流中的表格不绕排：先沉到挡路浮动对象之下。
    // A table in the text flow starts below the floating objects in its way.
    below_floats(pb, pb.col_x() + tl.x, pb.col_x() + tl.x + tl.width);
    // 连续的表头行（header 属性）：跨页时在新页顶部重复。
    let header_rows: Vec<usize> = (0..t.rows.len()).take_while(|r| t.rows.get(*r).is_some_and(|row| row.props.header)).collect();
    // 落一行：条目平移到 (栏左缘+表偏移, 当前游标)，游标推进行高。
    let place = |pb: &mut PageBuilder, row: &table::RowLayout| {
        let (x, y) = (pb.col_x() + tl.x, pb.y);
        if let Some(pg) = pb.page() {
            for it in &row.items {
                let mut it = it.clone();
                it.translate(x, y);
                pg.items.push(it);
            }
        }
        pb.y += row.height;
    };
    for (ri, row) in tl.rows.iter().enumerate() {
        // 表头行与 cant_split 行不可拆分。
        let splittable = !header_rows.contains(&ri) && !t.rows.get(ri).is_some_and(|r| r.props.cant_split);
        // 本行放不下的余部（逐轮变小）。
        let mut rest: Option<table::RowLayout> = None;
        // 是否已翻到只重复了表头行的新页。
        // On a new page holding only the repeated header rows.
        let mut fresh = false;
        // 每轮先放行内放得下的部分，再翻页；防御上限 1000 轮兜底，正常几轮内结束。
        // Each pass places the part of the row that fits, then breaks the page.
        for _ in 0..1000 {
            let cur = rest.as_ref().unwrap_or(row);
            // 当前行（或余部）放得下：结束循环，落到本页。
            if pb.y + cur.height <= pb.bottom + 0.01 {
                break;
            }
            // 可拆行按页底剩余高度切分。
            let split = if splittable { table::split_row(cur, pb.bottom - pb.y) } else { None };
            match split {
                Some((a, b)) => {
                    place(pb, &a);
                    rest = Some(b);
                }
                // 同 Word：空页上（或重复表头之下）仍放不下的整行照放，伸进下边距。
                // Like Word, a row that does not fit on an empty page, or under the header rows
                // repeated there, is placed anyway and runs into the bottom margin.
                None if pb.at_top() || fresh => break,
                None => {}
            }
            // 翻页/换栏。
            pb.advance(block, body_top);
            // 新页先重复表头行（本行不是表头时）。
            // Repeat header rows.
            if !header_rows.contains(&ri) {
                for hr in &header_rows {
                    if let Some(h) = tl.rows.get(*hr) {
                        place(pb, h);
                    }
                }
            }
            fresh = true;
        }
        place(pb, rest.as_ref().unwrap_or(row));
    }
}

/// Place floating table `tl` (`w:tblpPr`): at its own position, which takes no room in the text
/// flow; the text after it wraps around it.
/// 浮动表落位：站在自己的位置、不占文流，其后文字绕排。锚着段落（v_rel=Paragraph）且当前
/// 位置放不下、又不在栏顶时，先换栏/页再试一次（至多两轮）。
fn place_floating_table(pb: &mut PageBuilder, tl: &table::TableLayout, f: &TableFloat, legacy: f32, block: usize, body_top: f32) {
    let h: f32 = tl.rows.iter().map(|r| r.height).sum::<f32>().clamp(0.0, 100_000.0);
    let w = tl.width.clamp(1.0, 100_000.0);
    let [dl, dt, dr, db] = f.dist_from_text();
    for attempt in 0..2 {
        let s = pb.sect;
        let h_area = float_table_h_area(pb, f);
        // 表所锚的文字区：即下一段的起点（v_rel=Paragraph 时）。
        // The text a table anchored to it stands in: where the next paragraph starts.
        let v_area = match f.v_rel {
            Anchor::Page => (0.0, s.page_h),
            Anchor::Margin => (s.margin_top, s.text_height()),
            _ => (pb.y, 0.0),
        };
        let x = float_table_x(pb, w, f, legacy);
        let r = clear_of_float_tables(Rect::new(x, align_in(v_area, f.y, h, f.v_align), w, h), f, legacy, h_area.0, &pb.float_tables);
        // 第一次尝试放不下（且不在栏顶、锚着段落）：换栏/页后重算参照区再来一轮。
        if attempt == 0 && r.bottom() > pb.bottom + 0.01 && !pb.at_top() && f.v_rel == Anchor::Paragraph {
            pb.advance(block, body_top);
            continue;
        }
        if let Some(pg) = pb.page() {
            let mut y = r.y;
            for row in &tl.rows {
                for it in &row.items {
                    let mut it = it.clone();
                    it.translate(r.x, y);
                    pg.items.push(it);
                }
                y += row.height;
            }
        }
        // 登记避让区：文字绕排（excl），其他浮动表避让（float_tables）。
        let area = r.inset(-dl, -dt, -dr, -db);
        pb.excl.push((area, false));
        pb.float_tables.push(area);
        return;
    }
}

/// The area a floating table's `x` is measured in: its start and width on the page.
/// 浮动表 x 的量取区域：页面坐标 (起点, 宽)。相对页面/页边距用整页/正文区，相对栏用当前栏。
fn float_table_h_area(pb: &PageBuilder, f: &TableFloat) -> (f32, f32) {
    let s = pb.sect;
    match f.h_rel {
        Anchor::Page => (0.0, s.page_w),
        Anchor::Margin => (s.margin_left + s.gutter, s.text_width()),
        _ => (pb.col_x(), pb.col_w()),
    }
}

/// A floating table's left edge on the page (`legacy`: the first cell's left margin before Word
/// 2013 layout, when an offset places the cell's text).
/// 浮动表左缘的页面 x：按偏移/对齐落在量取区内；旧版排版按偏移放置时再退 legacy（首格左边距）。
fn float_table_x(pb: &PageBuilder, w: f32, f: &TableFloat, legacy: f32) -> f32 {
    let x = align_in(float_table_h_area(pb, f), f.x, w, f.h_align);
    if f.h_align.is_none() { x - legacy } else { x }
}

/// Move `pb.y` below the floating objects in the way of something spanning `x0..x1` that
/// starts there (a table in the text flow doesn't wrap around them).
/// 把 pb.y 压到行进路上避让区的最低底部之下（文流表格不绕排，只能被顶下去）。
fn below_floats(pb: &mut PageBuilder, x0: f32, x1: f32) {
    pb.y = below(&pb.excl, pb.y, x0, x1);
}

/// `y`, moved below the wrap areas in `excl` in the way of something spanning `x0..x1` that
/// starts there.
/// 把 y 下移到 excl 中"挡路"（横向与 x0..x1 相交、纵向压着 y）避让区的最低底部之下，
/// 反复直到不再相交。
fn below(excl: &[(Rect, bool)], mut y: f32, x0: f32, x1: f32) -> f32 {
    // 防御上限 64 轮：避让链正常一两轮收敛，防病态堆叠输入死循环。
    for _ in 0..64 {
        let in_the_way = excl.iter().filter(|(r, _)| r.y <= y + 0.01 && r.bottom() > y && r.x < x1 && r.right() > x0);
        let Some(bottom) = in_the_way.map(|(r, _)| r.bottom()).reduce(f32::max) else { break };
        y = bottom;
    }
    y
}

/// 为每页补排页眉页脚（仅打印视图；web 长页没有页眉页脚）。部件按"链接到前一节"向前继承，
/// "首页不同/奇偶页不同"选择对应部件；排页眉（页顶 sect.header 处）与页脚（页底 sect.footer
/// 之上，页相关浮动对象以其 frame 为准二次排）；同时回填 PAGE/NUMPAGES/SECTION 等域的真实取值。
fn headers_footers(ctx: &mut Ctx, pages: &mut [Page], sections: &[(usize, &SectionProps)]) {
    // 总页数（NUMPAGES 域）。
    let total = pages.len() as u32;
    // 每节页数（SECTIONPAGES 域）。
    // Pages per section.
    let mut per_section: HashMap<usize, u32> = HashMap::new();
    for p in pages.iter() {
        *per_section.entry(p.section).or_default() += 1;
    }
    // "奇偶页不同"设置。
    let even_odd = ctx.doc.settings.even_odd_headers;
    // 每节第一页的页下标（"首页不同"判断用）。
    let mut first_of_section: HashSet<usize> = HashSet::new();
    let mut seen = HashSet::new();
    for (i, p) in pages.iter().enumerate() {
        if seen.insert(p.section) {
            first_of_section.insert(i);
        }
    }
    for (i, page) in pages.iter_mut().enumerate() {
        let Some((_, sect)) = sections.get(page.section) else { continue };
        // 从前面的节继承页眉/页脚引用（Word 的"链接到前一节"）。
        // Inherit header/footer references from earlier sections (Word's "link to previous").
        let pick = |get: &dyn Fn(&SectionProps) -> wordcraft_doc::section::HeaderSet| -> Option<u32> {
            let mut set = get(sect);
            for k in (0..page.section).rev() {
                if set.default.is_some() && set.first.is_some() && set.even.is_some() {
                    break;
                }
                if let Some((_, s)) = sections.get(k) {
                    let o = get(s);
                    set.default = set.default.or(o.default);
                    set.first = set.first.or(o.first);
                    set.even = set.even.or(o.even);
                }
            }
            // "首页不同"：节内第一页用首页部件。
            if sect.title_page && first_of_section.contains(&i) {
                return set.first;
            }
            // "奇偶页不同"：偶数页用偶页部件（未设则回落 default）。
            if even_odd && page.number % 2 == 0 {
                return set.even.or(set.default);
            }
            set.default
        };
        // 选出本页的页眉/页脚部件。
        let hid = pick(&|s: &SectionProps| s.headers);
        let fid = pick(&|s: &SectionProps| s.footers);
        // 回填本页域取值：页眉页脚里的页码域在此拿到真实值。
        ctx.fields.page = page.number;
        ctx.fields.page_format = sect.page_num_format;
        ctx.fields.pages = total;
        ctx.fields.section_pages = per_section.get(&page.section).copied().unwrap_or(1);
        ctx.fields.section = page.section as u32 + 1;
        let w = sect.text_width();
        let x = sect.margin_left + sect.gutter;
        // 排页眉：从页顶 sect.header 处起排。
        if let Some(id) = hid
            && let Some(part) = ctx.doc.parts.get(&id)
        {
            let blocks = part.blocks.clone();
            let frame = PageFrame { sect, origin: (x, sect.header) };
            let mut items = layout_box(ctx, StoryRef::Part(id), &blocks, &[], w, None, 0, Some(frame)).items;
            for it in &mut items {
                it.translate(x, sect.header);
            }
            page.header = items;
            page.header_story = Some(id);
        }
        if let Some(id) = fid
            && let Some(part) = ctx.doc.parts.get(&id)
        {
            let blocks = part.blocks.clone();
            // 页脚高度决定其位置（页相关浮动对象需要）：先无 frame 排一次量高，再带正确 frame 重排。
            // The footer's height fixes where it sits, which page-relative floats need.
            let h = layout_box(ctx, StoryRef::Part(id), &blocks, &[], w, None, 0, None).height;
            let frame = PageFrame { sect, origin: (x, sect.page_h - sect.footer - h) };
            let mut items = layout_box(ctx, StoryRef::Part(id), &blocks, &[], w, None, 0, Some(frame)).items;
            for it in &mut items {
                it.translate(x, sect.page_h - sect.footer - h);
            }
            page.footer = items;
            page.footer_story = Some(id);
        }
    }
}

/// Is an object floating (not laid out inline)?
/// 该对象是否浮动（非嵌入式排版）？
pub fn is_floating(o: &InlineObject) -> bool {
    o.is_floating()
}

/// Milliseconds since an arbitrary epoch (monotonic on native; the browser's clock on the web).
/// 距任意纪元的毫秒数（原生平台用单调时钟；web 平台用浏览器时钟）。
pub fn now_ms() -> f64 {
    #[cfg(not(target_arch = "wasm32"))]
    {
        static T0: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        T0.get_or_init(std::time::Instant::now).elapsed().as_secs_f64() * 1000.0
    }
    // wasm32-unknown-unknown 上 Instant::now() 会 panic，改问浏览器时钟；异常值按 0 兜底。
    // `Instant::now()` panics on wasm32-unknown-unknown, so ask the browser's clock.
    #[cfg(target_arch = "wasm32")]
    {
        let ms = js_sys::Date::now();
        if ms.is_finite() { ms } else { 0.0 }
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_bidi;
