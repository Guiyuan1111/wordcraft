//! Equation layout: the [`wordcraft_doc::math`] tree → positioned glyphs and rules.
//!
//! Follows the OpenType `MATH` model (the one Word's own math engine and Cambria Math were built
//! around): font constants for script shifts, fraction and radical gaps, limits and rules; size
//! variants and glyph assemblies for stretchy delimiters, radicals, big operators and wide
//! accents; atom classes with TeX/Word inter-atom spacing. Without a math font the layout uses
//! fixed proportions of the em.
//!
//! Coordinates: x right from the equation's left edge, y *up* from its baseline.
//!
//! 【中文标注·业务流程】公式排版引擎，流程分四步：
//!   1. 输入：`wordcraft_doc::math` 的公式树（MNode，来自 OMML 解析或线性公式 parse_linear）。
//!   2. 载入 OpenType MATH 表（固定 Cambria Math；无数学字体时按 em 比例取 fallback 常数）。
//!   3. `layout_equation` 建立上下文 Ctx，按样式 St（正文/上下标/次上下标层级、display/
//!      inline、cramped 紧缩）递归布局每个节点：分数 frac、根式 radical、上下标 attach、
//!      N 元算子 op_atoms、定界符 delimiter、矩阵 matrix、方程组 eq_rows、长公式折行
//!      wrapped、自动编号 numbered……每个参数槽位同时登记光标位置 Slot，供编辑器画
//!      光标与命中测试。
//!   4. 产出 MathLayout：绝对定位的绘制项 MItem（x 向右、y 自基线向上）、宽度与上下
//!      伸展、光标槽位表。
//! 要点一（stretchy 伸展）：MathVariants 表给每个可伸字符两张表——尺寸变体 variants 与
//! 部件表 parts（GlyphPart：首端/尾端连接件长度、整字宽、是否延伸件）。stretch 先挑第一个
//! 够高的尺寸变体；不够高且部件齐全时做"拼接"：固定件 + 延伸件×重复次数 − 相邻件最小
//! 重叠（min_connector_overlap）≥ 目标高，多余量摊到各接缝作额外重叠（受连接件长度约束）。
//! 要点二（基线与模式）：所有 y 坐标相对公式自身基线向上；display（独占一行）下大算子
//! 放大居中于数学轴、上下限摆算子上下、分数用 Display 常数、可折行可编号；inline 模式
//! 一律上下标侧旁、不折行。
//! 要点三（自动编号）：display 行内顶层 `#` 切分公式与编号（numbered）；`#` 后为空时
//! 用计数器 counter 自增生成 "(n)"，counter 由文档级公式计数经 ParaEnv.eq_number 传入。
//! 要点四（字距）：原子按 TeX 八类查 SPACING 表（mu=1/18 em，负值只在正文层级生效）；
//! 结构间距（分数/根式/上下标/上下限/横线等）全部读 MATH 表 MathConstants（Ctx::k）。

use std::sync::Arc;

use wordcraft_doc::math::{
    Arg, BrkBin, BrkBinSub, ColJc, FracKind, LimLoc, MAX_DEPTH, MClass, MNode, MRun, MScr, MSty, MathProps, ScriptKind, is_integral, math_alnum,
    parse_linear,
};
use wordcraft_doc::props::{Rgb, TextColor};
use wordcraft_doc::resolve::ResolvedChar;
use wordcraft_fonts::math::{C, Construction, MathTable, glyph_bounds};
use wordcraft_fonts::{FaceRef, FontDb};

/// Something to draw.
/// 【中文标注】公式产出的绘制项（交给渲染器）；坐标 x 向右、y 自基线向上。
#[derive(Clone, Debug)]
pub enum MItem {
    // 【中文标注】一组字形：字体面、字号、颜色、是否合成粗体/斜体；glyphs 为
    // (字形 id, x, y) 列表（y 为基线，向上为正）；text 保留原文本（供提取算子字符等）。
    Glyphs {
        face: FaceRef,
        size: f32,
        color: Rgb,
        synth_bold: bool,
        synth_italic: bool,
        /// Glyph id, x, y (baseline, up).
        glyphs: Vec<(u32, f32, f32)>,
        text: String,
    },
    /// A filled rectangle; `y` is its bottom edge.
    /// 【中文标注】实心矩形：分数线、根式横杠、顶/底线、边框等；y 为矩形底边。
    Rect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        color: Rgb,
    },
    // 【中文标注】线段：(x0,y0)-(x1,y1)，width 线宽，dotted 虚线（空槽位提示框用）。
    Line {
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        width: f32,
        color: Rgb,
        dotted: bool,
    },
    /// Drawn on screen only (placeholders for empty slots, the empty-equation hint).
    /// 【中文标注】仅在屏幕编辑视图绘制、打印/导出时省略（空槽虚线框、空公式提示）。
    ScreenOnly(Box<MItem>),
}

impl MItem {
    /// 【中文标注】平移绘制项 (dx, dy)：子盒并入父盒时由 Bx::put 调用。
    fn shift(&mut self, dx: f32, dy: f32) {
        match self {
            MItem::Glyphs { glyphs, .. } => {
                for g in glyphs {
                    g.1 += dx;
                    g.2 += dy;
                }
            }
            MItem::Rect { x, y, .. } => {
                *x += dx;
                *y += dy;
            }
            MItem::Line { x0, y0, x1, y1, .. } => {
                *x0 += dx;
                *x1 += dx;
                *y0 += dy;
                *y1 += dy;
            }
            MItem::ScreenOnly(i) => i.shift(dx, dy),
        }
    }
}

/// A caret position inside an equation, where it is drawn: `x`, the baseline `y` (up from the
/// equation's baseline) and the caret's extent above (`a`) and below (`d`) it.
/// 【中文标注】公式内的一个光标落点：path 是从根到所在参数的 (节点序号, 参数序号) 路径，
/// off 是参数内的字符偏移；x 为横坐标，y 为基线高度（相对公式基线，向上为正），a/d 为
/// 光标向上/向下的延伸长度。编辑器据此画光标、做点击命中。
#[derive(Clone, Debug, PartialEq)]
pub struct Slot {
    /// 【中文标注】所在参数路径：(节点在其参数列表中的序号, 第几个参数)，逐层叠加。
    pub path: Vec<(usize, usize)>,
    /// 【中文标注】参数内的字符偏移（0 = 参数最前）。
    pub off: usize,
    /// 【中文标注】光标横坐标 x 与基线高度 y。
    pub x: f32,
    pub y: f32,
    /// 【中文标注】光标在基线上方/下方的延伸量。
    pub a: f32,
    pub d: f32,
}

/// A laid-out equation.
/// 【中文标注】排好的公式：宽度、相对基线的上下伸展（点）、绘制项与光标槽位表。
#[derive(Clone, Debug, Default)]
pub struct MathLayout {
    /// 【中文标注】公式总宽度。
    pub width: f32,
    /// Extent above and below the baseline, points.
    /// 【中文标注】基线上方/下方的伸展量（点），决定公式占用的行高；行内公式与文字
    /// 共用基线，display 公式基线由行布局放在数学轴附近。
    pub ascent: f32,
    pub descent: f32,
    /// 【中文标注】全部绘制项（字形组/矩形/线条），坐标已定位。
    pub items: Vec<MItem>,
    /// Every caret position, for drawing the caret and hit testing.
    /// 【中文标注】所有光标槽位：画光标与点击命中用（slot()/hit()）。
    pub slots: Vec<Slot>,
}

impl MathLayout {
    /// Where the caret at (`path`, `off`) is drawn.
    /// 【中文标注】查 (path, off) 处的光标槽位：先找精确匹配，找不到退而取同 path 下
    /// 偏移最接近的槽位（例如点击落到组合字形中间时）。
    pub fn slot(&self, path: &[(usize, usize)], off: usize) -> Option<&Slot> {
        self.slots
            .iter()
            .find(|s| s.path == path && s.off == off)
            .or_else(|| self.slots.iter().filter(|s| s.path == path).min_by_key(|s| s.off.abs_diff(off)))
    }
    /// The caret position nearest to (`x`, `y`) (y up from the baseline).
    /// 【中文标注】点击命中：返回距 (x, y)（y 向上）最近的槽位，距离由 slot_dist 评估。
    pub fn hit(&self, x: f32, y: f32) -> Option<&Slot> {
        self.slots.iter().min_by(|a, b| slot_dist(a, x, y).total_cmp(&slot_dist(b, x, y)))
    }
}

/// 【中文标注】点到槽位的距离：横向差 + 纵向差（在槽位高度范围内记 0）×3；同样高度时
/// 稍偏向更"小"（更内层）的参数，让点击优先落入嵌套深处。
fn slot_dist(s: &Slot, x: f32, y: f32) -> f32 {
    let (top, bottom) = (s.y + s.a, s.y - s.d);
    let dy = if y > top {
        y - top
    } else if y < bottom {
        bottom - y
    } else {
        0.0
    };
    // Inside a slot's height, prefer the innermost (shortest) argument.
    let h = (s.a + s.d).max(0.1);
    (s.x - x).abs() + dy * 3.0 + if dy == 0.0 { h * 0.05 } else { 0.0 }
}

/// Numbered rows (`#`) in an equation: they advance the automatic numbering.
/// 【中文标注】统计本公式占用几个自动编号：仅 display 模式且顶层为方程组 EqArr 时，
/// 数带 `#` 的行数。lib.rs 在段落排版前调用它，把段落内各公式的占用数累加进文档级
/// 公式计数器（Ctx.eq_count），再作为 ParaEnv.eq_number 起始值传入 layout_equation。
pub fn auto_numbers(math: &wordcraft_doc::math::Math, display: bool) -> u32 {
    if !display {
        return 0;
    }
    match math.nodes.as_slice() {
        [MNode::EqArr { rows }] => rows.iter().filter(|r| split_number(r).is_some()).count() as u32,
        _ => 0,
    }
}

/// A row's equation and number parts around its top-level `#`, and the units before the `#`.
/// 【中文标注】按行内首个非字面 `#` 把一行切成 (公式部分, 编号部分, 公式部分单位数)：
/// `#` 前为公式、`#` 后为编号（编号为空即"自动编号"）；单位数供光标偏移换算。
/// 整行没有 `#` 时返回 None。
fn split_number(row: &[MNode]) -> Option<(Arg, Arg, usize)> {
    let mut eq = Vec::new();
    let mut units = 0usize;
    for (i, n) in row.iter().enumerate() {
        if let MNode::Run(r) = n
            && !r.lit
            && let Some(b) = r.text.find('#')
        {
            let mut left = r.clone();
            left.text = r.text.get(..b).unwrap_or("").to_string();
            let mut right = r.clone();
            right.text = r.text.get(b + 1..).unwrap_or("").to_string();
            units += left.text.chars().count();
            if !left.text.is_empty() {
                eq.push(MNode::Run(left));
            }
            let mut num = Vec::new();
            if !right.text.is_empty() {
                num.push(MNode::Run(right));
            }
            num.extend(row.get(i + 1..).unwrap_or(&[]).iter().cloned());
            return Some((eq, num, units));
        }
        units += match n {
            MNode::Run(r) => r.text.chars().count(),
            _ => 1,
        };
        eq.push(n.clone());
    }
    None
}

/// Lay out an equation set at the size and colour of `rc`. `display`: an equation on a line of
/// its own (bigger operators, limits above and below, full-size fractions).
/// `avail` is the width of the line (numbered display equations fill it, wider display equations
/// wrap to it as `props` says); `counter` counts the document's automatic equation numbers so far
/// and advances past this equation's.
/// 【中文标注】公式排版总入口：`math` 为公式树（空且非线性时用 `linear` 文本现解析），
/// `rc` 提供字号与颜色，`display` 区分行内/显示模式，`avail` 为行宽，`counter` 是文档级
/// 自动编号计数器（本函数推进它，返回后调用方续用）。
pub fn layout_equation(
    math: &wordcraft_doc::math::Math,
    linear: &str,
    rc: &ResolvedChar,
    display: bool,
    avail: f32,
    props: &MathProps,
    counter: &mut u32,
) -> MathLayout {
    // 公式树为空且不是纯线性模式时，退回解析线性文本（用户直接键入的 a/b 形式等）。
    let parsed;
    let nodes: &Arg = if math.nodes.is_empty() && !math.linear {
        parsed = parse_linear(linear);
        &parsed
    } else {
        &math.nodes
    };
    // 载入数学字体与 OpenType MATH 表（固定 Cambria Math；无表时各间距退回 fallback 常数）。
    let (face, table) = wordcraft_fonts::math::resolve_math("Cambria Math");
    let color = match rc.color {
        TextColor::Rgb(c) => c,
        TextColor::Auto => Rgb::BLACK,
    };
    let base = rc.draw_size().clamp(1.0, 1638.0);
    let mut cx = Ctx {
        face,
        t: table,
        upem: face.upem.max(1.0) as f32,
        base,
        color,
        text_font: rc.font.clone(),
        nodes: 0,
        path: Vec::new(),
        node_stack: Vec::new(),
    };
    // 初始样式：0 级（正文尺寸）、是否 display、不紧缩。
    let st = St { level: 0, display, cramped: false, upright: false };
    let avail = if avail.is_finite() { avail.clamp(0.0, 1e5) } else { 0.0 };
    // 分派：空公式→屏幕提示；display 且带 `#` 行→自动编号布局；display→可折行布局；
    // 行内→普通排版（行内公式永不折行）。
    let b = match nodes.as_slice() {
        [] => cx.hint(st),
        [MNode::EqArr { rows }] if display && rows.iter().any(|r| split_number(r).is_some()) => cx.numbered(rows, st, avail, counter),
        _ if display => cx.wrapped(nodes, st, avail, props),
        _ => cx.arg(nodes, st, 0),
    };
    // 没有高度的槽位（如纯算子之间的槽）给一个 em 高度兜底，保证光标可见可点。
    let mut slots = b.slots;
    let em = base;
    for s in &mut slots {
        if s.a <= 0.0 && s.d <= 0.0 {
            s.a = em * 0.75;
            s.d = em * 0.25;
        }
    }
    MathLayout { width: b.w.max(0.0), ascent: b.a.max(0.0), descent: b.d.max(0.0), items: b.items, slots }
}

/// Layout style: script level (0 = text size, 1 = script, 2 = script-script), display or inline,
/// cramped (superscripts lowered: inside radicals, denominators, subscripts).
/// 【中文标注】排版样式：level 为上下标层级（0 正文 →1 上/下标 →2 上/下标的上/下标），
/// 决定字号缩放（MATH 表 ScriptPercentScaleDown）；display=显示模式；cramped="紧缩"
/// （根号内、分母、下标中，上标压低）；upright=字母默认正立（函数名 lim、sin 等）。
#[derive(Clone, Copy, Debug)]
struct St {
    level: u8,
    display: bool,
    cramped: bool,
    /// Letters default to upright (function names).
    upright: bool,
}

impl St {
    /// 【中文标注】进入下一层上下标（最高 2 级）；display 一律取消——上/下标内不摆上下限。
    fn script(self) -> St {
        St { level: (self.level + 1).min(2), display: false, ..self }
    }
    /// 【中文标注】紧缩样式：上标按 Cramped 常数压低。
    fn cramp(self) -> St {
        St { cramped: true, ..self }
    }
    /// Numerator / denominator style.
    /// 【中文标注】分数分子/分母的样式：display 分数取正文尺寸（分子不缩），行内分数
    /// 按上下标尺寸缩小。
    fn frac(self) -> St {
        if self.display { St { display: false, ..self } } else { self.script() }
    }
}

/// TeX atom classes (spacing).
/// 【中文标注】TeX 原子类别，决定原子间字距：Ord 普通、Op 大算子、Bin 二元运算、
/// Rel 关系符、Open/Close 括号、Punct 标点、Inner（如分数）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum Cls {
    #[default]
    Ord,
    Op,
    Bin,
    Rel,
    Open,
    Close,
    Punct,
    Inner,
}

impl Cls {
    /// 【中文标注】类别 → SPACING 表下标。
    fn idx(self) -> usize {
        self as usize
    }
}

/// Inter-atom space in mu (1/18 em): TeX's table; negative = only outside scripts.
/// 【中文标注】原子间字距表（单位 mu = 1/18 em），照搬 TeX 的 8×8 矩阵；负值表示
/// 只在正文层级（level=0）生效、上下标中不加（见 spacing_mu）；排版时按当前 em
/// 换算成点。
const SPACING: [[i8; 8]; 8] = [
    // Ord  Op  Bin  Rel Open Close Punct Inner
    [0, 3, -4, -5, 0, 0, 0, -3],     // Ord
    [3, 3, 0, -5, 0, 0, 0, -3],      // Op
    [-4, -4, 0, 0, -4, 0, 0, -4],    // Bin
    [-5, -5, 0, 0, -5, 0, 0, -5],    // Rel
    [0, 0, 0, 0, 0, 0, 0, 0],        // Open
    [0, 3, -4, -5, 0, 0, 0, -3],     // Close
    [-3, -3, 0, -3, -3, -3, -3, -3], // Punct
    [-3, 3, -4, -5, -3, 0, -3, -3],  // Inner
];

/// 【中文标注】查 (左类别, 右类别) 的字距（mu）：上下标层级中负值视为 0（不缩格），
/// 返回值由调用方乘 em/18 换算为点。
fn spacing_mu(l: Cls, r: Cls, st: St) -> f32 {
    let v = SPACING.get(l.idx()).and_then(|row| row.get(r.idx())).copied().unwrap_or(0);
    if v < 0 && st.level > 0 { 0.0 } else { v.unsigned_abs() as f32 }
}

/// A box under construction.
/// 【中文标注】布局中的"盒子"：递归布局的基本单元。w/a/d 为宽度与相对自身基线的
/// 上/下伸展；items 为内部绘制项；first/last 记两端原子类别（供相邻原子算字距）。
#[derive(Clone, Debug, Default)]
struct Bx {
    /// 【中文标注】宽度、基线上方/下方伸展（a 向上、d 向下为正）。
    w: f32,
    a: f32,
    d: f32,
    /// 【中文标注】盒内绘制项（坐标相对盒子自身基线）。
    items: Vec<MItem>,
    /// Italic correction after the box (superscripts move right by it).
    /// 【中文标注】斜体修正量（MATH 表 ItalicsCorrections）：后接上标时右移它，
    /// 避免上标撞进斜体字的甩尾。
    ic: f32,
    /// 【中文标注】盒子首/末原子的类别（相邻盒间字距查询用）。
    first: Cls,
    last: Cls,
    /// The box is one glyph: (face, glyph, top-accent attachment x).
    /// 【中文标注】盒子恰为单字形时记录 (字体面, 字形 id, 顶部重音附着点 x)：供重音
    /// 对位，以及"单字符底"上下标不做整体下沉的特例判断。
    glyph: Option<(FaceRef, u32, f32)>,
    /// Takes no part in inter-atom spacing (spaces).
    /// 【中文标注】透明盒：空格、函数应用符等零宽占位，不参与原子间字距。
    transparent: bool,
    /// Caret positions inside, relative to the box.
    /// 【中文标注】盒内光标槽位（相对盒子）。
    slots: Vec<Slot>,
    /// Text atoms: x of each character boundary (one more than the characters).
    /// 【中文标注】文本原子的各字符边界 x（比字符数多一个），用于生成逐字符光标槽位。
    cuts: Vec<f32>,
    /// A manual line break before this atom (`m:brk`; the value is its `m:alnAt`).
    /// 【中文标注】此原子前有手动断行（m:brk），值为 m:alnAt 对齐位置。
    brk: Option<u8>,
}

impl Bx {
    /// 【中文标注】把盒子两端类别都设为 c（单原子盒）。
    fn cls(mut self, c: Cls) -> Bx {
        self.first = c;
        self.last = c;
        self
    }
    /// Draw `o` at (dx, dy) into this box (extents grow; width is the caller's business).
    /// 【中文标注】把子盒 o 平移 (dx, dy)（dy 向上为正）后并入：上/下伸展取并集，
    /// 绘制项与光标槽位随之平移；宽度由调用方维护。
    fn put(&mut self, o: Bx, dx: f32, dy: f32) {
        self.a = self.a.max(o.a + dy);
        self.d = self.d.max(o.d - dy);
        for mut it in o.items {
            it.shift(dx, dy);
            self.items.push(it);
        }
        for mut sl in o.slots {
            sl.x += dx;
            sl.y += dy;
            self.slots.push(sl);
        }
    }
    /// 【中文标注】登记一个光标槽位（x 处、暂无高度，稍后由 finalize 撑起）。
    fn slot(&mut self, path: &[(usize, usize)], off: usize, x: f32) {
        self.slots.push(Slot { path: path.to_vec(), off, x, y: 0.0, a: 0.0, d: 0.0 });
    }
    /// 【中文标注】画一个实心矩形并扩张盒子的上/下伸（零宽/零高忽略）。
    fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: Rgb) {
        if w > 0.0 && h > 0.0 {
            self.items.push(MItem::Rect { x, y, w, h, color });
            self.a = self.a.max(y + h);
            self.d = self.d.max(-y);
        }
    }
}

/// 【中文标注】一次公式排版的共享上下文：字体面、MATH 表、字体 units-per-em、基准字号、
/// 颜色、正文字体；nodes 为已排节点计数（防失控输入），path 为当前参数路径（光标槽位
/// 归属用），node_stack 记当前节点在各层参数中的序号（构造 path 用）。
struct Ctx {
    face: FaceRef,
    /// 【中文标注】OpenType MATH 表（MathConstants/ItalicsCorrections/TopAccentAttachments/
    /// MathVariants 等）；无数学字体时为 None，各间距退回 em 比例常数。
    t: Option<Arc<MathTable>>,
    upem: f32,
    base: f32,
    color: Rgb,
    text_font: String,
    /// Nodes laid out so far (caps work on hostile input).
    nodes: usize,
    /// The argument being laid out (for caret slots).
    path: Vec<(usize, usize)>,
    /// Index of the node being laid out in its argument, innermost last.
    node_stack: Vec<usize>,
}

/// 【中文标注】单次公式排版可处理的节点数上限（防恶意/失控输入）。
const MAX_NODES: usize = 200_000;

/// Constants as fractions of the em when there's no `MATH` table.
/// 【中文标注】MATH 表缺省时的 MathConstants 兜底值（em 的比例）：涵盖上下标升降、
/// 上下限间距、分数上下移与分式线粗细、根式间隙与度数 Kern、顶/底线与边框的间距线粗
/// 等；与 Ctx::k 配合——有表按表值（字体单位换算点数），无表按此处比例乘字号。
fn fallback(c: C) -> f32 {
    match c {
        C::MathLeading => 0.15,
        C::AxisHeight => 0.25,
        C::AccentBaseHeight => 0.45,
        C::FlattenedAccentBaseHeight => 0.66,
        C::SubscriptShiftDown => 0.2,
        C::SubscriptTopMax => 0.36,
        C::SubscriptBaselineDropMin => 0.05,
        C::SuperscriptShiftUp => 0.36,
        C::SuperscriptShiftUpCramped => 0.29,
        C::SuperscriptBottomMin => 0.11,
        C::SuperscriptBaselineDropMax => 0.25,
        C::SubSuperscriptGapMin => 0.2,
        C::SuperscriptBottomMaxWithSubscript => 0.36,
        C::SpaceAfterScript => 0.05,
        C::UpperLimitGapMin => 0.1,
        C::UpperLimitBaselineRiseMin => 0.2,
        C::LowerLimitGapMin => 0.1,
        C::LowerLimitBaselineDropMin => 0.6,
        C::StackTopShiftUp => 0.44,
        C::StackTopDisplayStyleShiftUp => 0.68,
        C::StackBottomShiftDown => 0.34,
        C::StackBottomDisplayStyleShiftDown => 0.69,
        C::StackGapMin => 0.12,
        C::StackDisplayStyleGapMin => 0.28,
        C::StretchStackTopShiftUp => 0.2,
        C::StretchStackBottomShiftDown => 0.6,
        C::StretchStackGapAboveMin => 0.1,
        C::StretchStackGapBelowMin => 0.1,
        C::FractionNumeratorShiftUp => 0.39,
        C::FractionNumeratorDisplayStyleShiftUp => 0.68,
        C::FractionDenominatorShiftDown => 0.34,
        C::FractionDenominatorDisplayStyleShiftDown => 0.69,
        C::FractionNumeratorGapMin => 0.04,
        C::FractionNumDisplayStyleGapMin => 0.12,
        C::FractionRuleThickness => 0.05,
        C::FractionDenominatorGapMin => 0.04,
        C::FractionDenomDisplayStyleGapMin => 0.12,
        C::SkewedFractionHorizontalGap => 0.35,
        C::SkewedFractionVerticalGap => 0.1,
        C::OverbarVerticalGap => 0.15,
        C::OverbarRuleThickness => 0.05,
        C::OverbarExtraAscender => 0.05,
        C::UnderbarVerticalGap => 0.15,
        C::UnderbarRuleThickness => 0.05,
        C::UnderbarExtraDescender => 0.05,
        C::RadicalVerticalGap => 0.05,
        C::RadicalDisplayStyleVerticalGap => 0.15,
        C::RadicalRuleThickness => 0.05,
        C::RadicalExtraAscender => 0.05,
        C::RadicalKernBeforeDegree => 0.28,
        C::RadicalKernAfterDegree => -0.55,
    }
}

/// Characters drawn as operators with their own spacing class.
/// 【中文标注】查字符的数学类别（wordcraft_doc::math::math_class 的 Unicode Math_Class
/// 表）并映射到本文件的 Cls；非 Ord 类字符自成原子、按类别取字距。
fn class_of(c: char) -> Cls {
    match wordcraft_doc::math::math_class(c) {
        MClass::Ord => Cls::Ord,
        MClass::Op => Cls::Op,
        MClass::Bin => Cls::Bin,
        MClass::Rel => Cls::Rel,
        MClass::Open => Cls::Open,
        MClass::Close => Cls::Close,
        MClass::Punct => Cls::Punct,
        MClass::Inner => Cls::Inner,
    }
}

/// Accents drawn across the whole base (stretched horizontally).
/// 【中文标注】宽重音集合（矢量箭头、上/下花括号线、上/下方括号等）：底足够宽时横向
/// 伸展（stretch_h）铺满整个底。
fn wide_accent(c: char) -> bool {
    matches!(
        c,
        '\u{20D6}'
            | '\u{20D7}'
            | '\u{20E1}'
            | '\u{20D0}'
            | '\u{20D1}'
            | '\u{2190}'
            | '\u{2192}'
            | '\u{2194}'
            | '\u{23DC}'
            | '\u{23DD}'
            | '\u{23DE}'
            | '\u{23DF}'
            | '\u{23B4}'
            | '\u{23B5}'
    )
}

impl Ctx {
    /// 【中文标注】上下标层级字号比例：读 MATH 表 ScriptPercentScaleDown /
    /// ScriptScriptPercentScaleDown（非法值按 70%/50% 兜底，钳制在合理区间）。
    fn scale(&self, level: u8) -> f32 {
        let (s1, s2) = match &self.t {
            Some(t) => (t.script_percent, t.script_script_percent),
            None => (70.0, 50.0),
        };
        let s1 = if (30.0..=100.0).contains(&s1) { s1 } else { 70.0 };
        let s2 = if (20.0..=100.0).contains(&s2) { s2 } else { 50.0 };
        match level {
            0 => 1.0,
            1 => s1 / 100.0,
            _ => s2 / 100.0,
        }
    }
    /// 【中文标注】当前样式层级的字号 = 基准字号 × 层级缩放比例。
    fn size(&self, st: St) -> f32 {
        self.base * self.scale(st.level)
    }
    /// A constant in points at the style's size.
    /// 【中文标注】取一个 MathConstants 度量并换算为当前样式的点数：有 MATH 表按
    /// 字体单位 / upem × 字号；无表用 fallback 的 em 比例。全部结构间距（分数/根式/
    /// 上下标位移/上下限/横线线粗等）统一经此读取，保证跟随字体而非写死。
    fn k(&self, c: C, st: St) -> f32 {
        let size = self.size(st);
        match &self.t {
            Some(t) => t.get(c) * size / self.upem,
            None => fallback(c) * size,
        }
    }
    /// 【中文标注】数学轴高度（AxisHeight）：=、+、分式线等沿它对齐；分式线位置、
    /// 大算子居中、矩阵/方程组垂直居中都以此为基准。
    fn axis(&self, st: St) -> f32 {
        self.k(C::AxisHeight, st)
    }
    /// 【中文标注】一个 em 的点数（同 size，语义别名）。
    fn em(&self, st: St) -> f32 {
        self.size(st)
    }

    /// 【中文标注】排一个参数（节点序列）：先拆成原子（row），再按字距拼合（compose），
    /// 最后 finalize 把本参数的光标槽位撑到参数高度。
    fn arg(&mut self, a: &[MNode], st: St, depth: usize) -> Bx {
        let atoms = self.row(a, st, depth, 0);
        let em = self.em(st);
        let mut b = compose(atoms.into_iter().map(|(b, _)| b).collect(), st, em);
        finalize(&mut b, &self.path, em);
        b
    }

    /// Child argument `c` of the node being laid out; an empty one shows a placeholder.
    /// 【中文标注】排当前节点的第 c 个子参数：把 (当前节点序号, c) 压入光标路径后递归；
    /// 空参数显示屏幕占位虚线框（placeholder）。
    fn sub(&mut self, c: usize, a: &[MNode], st: St, depth: usize) -> Bx {
        let me = self.node_stack.last().copied().unwrap_or(0);
        self.path.push((me, c));
        let b = if a.is_empty() { self.placeholder(st) } else { self.arg(a, st, depth) };
        self.path.pop();
        b
    }

    /// The dotted box an empty argument shows on screen.
    /// 【中文标注】空参数的屏幕占位：虚线小方框（ScreenOnly，不打印），并登记光标槽位。
    fn placeholder(&mut self, st: St) -> Bx {
        let em = self.em(st);
        let (w, a, d, pad) = (em * 0.56, em * 0.7, em * 0.06, em * 0.05);
        let mut b = Bx { w: w + 2.0 * pad, a, d, ..Default::default() };
        let (x0, x1, y0, y1) = (pad, pad + w, -d, a);
        let width = (em * 0.035).max(0.4);
        for ((ax, ay), (bx, by)) in [((x0, y0), (x1, y0)), ((x1, y0), (x1, y1)), ((x1, y1), (x0, y1)), ((x0, y1), (x0, y0))] {
            let line = MItem::Line { x0: ax, y0: ay, x1: bx, y1: by, width, color: Rgb(0x60, 0x60, 0x60), dotted: true };
            b.items.push(MItem::ScreenOnly(Box::new(line)));
        }
        let path = self.path.clone();
        b.slot(&path, 0, pad);
        finalize(&mut b, &path, em);
        b
    }

    /// An empty equation: a grey prompt (screen only).
    /// 【中文标注】空公式提示："Type an equation" 灰字，仅屏幕显示，打印/导出时空白。
    fn hint(&mut self, st: St) -> Bx {
        let size = self.size(st);
        let rs = wordcraft_fonts::word::resolve(&self.text_font, false, false);
        let mut g = self.glyphs(rs.face, "Type an equation", size, 0, Rgb(0x80, 0x80, 0x80), (rs.synth_bold, rs.synth_italic));
        g.items = std::mem::take(&mut g.items).into_iter().map(|i| MItem::ScreenOnly(Box::new(i))).collect();
        g.glyph = None;
        g.cuts.clear();
        g.slots.clear();
        g.slot(&[], 0, 0.0);
        finalize(&mut g, &[], size);
        g
    }

    /// The atoms of a row with their alignment segment (`&` starts a new segment). `u0`: the
    /// unit offset of the row's first node (for caret slots).
    /// 【中文标注】把一行节点拆成原子列表 (原子, 对齐段号)：& 开启新对齐段（方程组按
    /// 段对齐）；每个原子登记光标槽位（Run 逐字符、结构节点首尾各一）；u 累计字符单位
    /// 偏移；depth 超限或节点数超 MAX_NODES 即停（防失控输入）。
    fn row(&mut self, a: &[MNode], st: St, depth: usize, u0: usize) -> Vec<(Bx, usize)> {
        let mut out = Vec::new();
        if depth > MAX_DEPTH {
            return out;
        }
        let mut seg = 0usize;
        let mut u = u0;
        for (i, n) in a.iter().enumerate() {
            self.nodes += 1;
            if self.nodes > MAX_NODES {
                break;
            }
            // path：当前参数路径，原子用它登记光标槽位。
            let path = self.path.clone();
            match n {
                MNode::Run(r) => {
                    let first = out.len();
                    for part in self.run_atoms(r, st, u, &path) {
                        match part {
                            RunPart::Atom(b) => out.push((b, seg)),
                            RunPart::Align(b) => {
                                seg += 1;
                                out.push((b, seg));
                            }
                        }
                    }
                    if let Some((b, _)) = out.get_mut(first) {
                        b.brk = r.brk;
                    }
                    u += r.text.chars().count();
                }
                MNode::Nary { .. } | MNode::Func { .. } => {
                    self.node_stack.push(i);
                    let mut atoms = self.op_atoms(n, st, depth + 1);
                    self.node_stack.pop();
                    if let Some(f) = atoms.first_mut() {
                        f.slot(&path, u, 0.0);
                    }
                    if let Some(l) = atoms.last_mut() {
                        let w = l.w;
                        l.slot(&path, u + 1, w);
                    }
                    for b in atoms {
                        out.push((b, seg));
                    }
                    u += 1;
                }
                _ => {
                    self.node_stack.push(i);
                    let mut b = self.node(n, st, depth + 1);
                    self.node_stack.pop();
                    let w = b.w;
                    b.slot(&path, u, 0.0);
                    b.slot(&path, u + 1, w);
                    out.push((b, seg));
                    u += 1;
                }
            }
        }
        out
    }

    /// Glyph run for `text` in `face` at `size`: a box with ink extents.
    /// 【中文标注】整形一段文本为字形盒：level>0 时打开 ssty 特性取 MATH 表的数学
    /// 上/下标变体字形；按墨迹边界（glyph_bounds）累计盒子的上/下伸展；记录每个字符
    /// 边界的 x（cuts）供光标槽位；末字形查斜体修正；单字形时记录顶部重音附着点。
    fn glyphs(&self, face: FaceRef, text: &str, size: f32, level: u8, color: Rgb, synth: (bool, bool)) -> Bx {
        // ssty（math size variant）特性：1=上标尺寸变体，2=下标尺寸变体。
        let feats = if level > 0 { vec![wordcraft_fonts::feature_value(b"ssty", level as u32)] } else { Vec::new() };
        let shaped = wordcraft_fonts::shape(&face, text, &feats, |c| c);
        let k = size / face.upem.max(1.0) as f32;
        let mut x = 0.0f32;
        let mut b = Bx::default();
        let mut glyphs = Vec::with_capacity(shaped.len());
        let mut last_gid = None;
        for g in &shaped {
            let gx = x + g.x_offset as f32 * k;
            let gy = g.y_offset as f32 * k;
            if let Some((x0, y0, x1, y1)) = glyph_bounds(&face, g.gid) {
                b.a = b.a.max(y1 * k + gy);
                b.d = b.d.max(-y0 * k - gy);
                let _ = (x0, x1);
            }
            glyphs.push((g.gid, gx, gy));
            x += g.x_advance as f32 * k;
            last_gid = Some(g.gid);
        }
        b.w = x;
        // Character boundaries: where the first glyph of each character starts.
        let mut cuts = Vec::with_capacity(text.len() + 1);
        for (cb, _) in text.char_indices() {
            let mut gx = 0.0f32;
            let mut found = None;
            for g in &shaped {
                if g.cluster >= cb {
                    found = Some(gx);
                    break;
                }
                gx += g.x_advance as f32 * k;
            }
            cuts.push(found.unwrap_or(x));
        }
        cuts.push(x);
        b.cuts = cuts;
        if let (Some(t), Some(g)) = (&self.t, last_gid)
            && face.id() == self.face.id()
        {
            b.ic = t.italics.get(&g).copied().unwrap_or(0.0) * k;
        }
        if shaped.len() == 1
            && let Some(g) = last_gid
        {
            let attach = match (&self.t, glyph_bounds(&face, g)) {
                (Some(t), _) if face.id() == self.face.id() && t.top_accent.contains_key(&g) => t.top_accent.get(&g).copied().unwrap_or(0.0) * k,
                (_, Some((x0, _, x1, _))) => (x0 + x1) / 2.0 * k,
                _ => b.w / 2.0,
            };
            b.glyph = Some((face, g, attach));
        }
        if !glyphs.is_empty() {
            b.items.push(MItem::Glyphs { face, size, color, synth_bold: synth.0, synth_italic: synth.1, glyphs, text: text.to_string() });
        }
        b
    }

    /// A single glyph by id (size variants, assembly parts).
    /// 【中文标注】按字形 id 直接建盒（尺寸变体与拼接部件共用）：宽度取 advance，
    /// 伸展取墨迹边界，斜体修正查 MATH 表。
    fn glyph_id(&self, gid: u32, size: f32, color: Rgb) -> Bx {
        let face = self.face;
        let k = size / face.upem.max(1.0) as f32;
        let mut b = Bx { w: face.advance(gid) as f32 * k, ..Default::default() };
        if let Some((_, y0, _, y1)) = glyph_bounds(&face, gid) {
            b.a = y1 * k;
            b.d = -y0 * k;
        }
        if let Some(t) = &self.t {
            b.ic = t.italics.get(&gid).copied().unwrap_or(0.0) * k;
        }
        b.items.push(MItem::Glyphs { face, size, color, synth_bold: false, synth_italic: false, glyphs: vec![(gid, 0.0, 0.0)], text: String::new() });
        b
    }

    /// A text atom with caret slots at its character boundaries (`k0`: the unit offset of its
    /// first character).
    /// 【中文标注】文本原子：在字形盒基础上按字符边界（cuts）铺光标槽位——每个字符
    /// 前后各一个落点；k0 是首字符在参数内的单位偏移。
    fn text_atom(&self, face: FaceRef, text: &str, size: f32, level: u8, color: Rgb, synth: (bool, bool), path: &[(usize, usize)], k0: usize) -> Bx {
        let mut b = self.glyphs(face, text, size, level, color, synth);
        let cuts = std::mem::take(&mut b.cuts);
        for (j, x) in cuts.iter().enumerate() {
            b.slot(path, k0 + j, *x);
        }
        b
    }

    /// 【中文标注】把一个 MRun 拆成原子序列（RunPart）——原子是字距与结构定位的最小
    /// 单位。规则：
    /// - nor（正体文本）：用正文字体排一整个 Ord 原子；
    /// - 其余逐字符处理：映射数学字母表（math_alnum）并查类别（class_of）；连续的 Ord
    ///   字符且同字体、同斜体合成一个原子；非 Ord 字符（运算符/关系符/括号/…）自成
    ///   原子；非字面的 `-`/`*`/`'` 换成数学减号/星号/撇号；
    /// - `&`（非字面）产生对齐段 Align；空格与函数应用符等不可见字符产出透明原子
    ///   （不参与字距，但保留光标槽位）。
    /// 字号取 run 自带 size（滤掉非有限值）否则基准字号，再乘层级缩放。
    fn run_atoms(&self, r: &MRun, st: St, u0: usize, path: &[(usize, usize)]) -> Vec<RunPart> {
        let mut out = Vec::new();
        let size = r.size.filter(|s| s.is_finite()).unwrap_or(self.base).clamp(1.0, 1638.0) * self.scale(st.level);
        let color = r.color.unwrap_or(self.color);
        if r.nor {
            // 【中文标注】正体文本（m:nor）：用正文字体（字体名恰为 "Cambria Math" 视作
            // 未指定，退回正文字体）；按 m:sty 决定粗/斜，不再做数学斜体化。
            // Normal text: the text font, upright unless asked.
            let fam = r.font.clone().filter(|f| !f.eq_ignore_ascii_case("Cambria Math")).unwrap_or_else(|| self.text_font.clone());
            let (bold, italic) = match r.sty {
                Some(MSty::Bold) => (true, false),
                Some(MSty::Italic) => (false, true),
                Some(MSty::BoldItalic) => (true, true),
                _ => (false, false),
            };
            let rs = wordcraft_fonts::word::resolve(&fam, bold, italic);
            let mut b = self.text_atom(rs.face, &r.text, size, 0, color, (rs.synth_bold, rs.synth_italic), path, u0);
            b.glyph = None;
            out.push(RunPart::Atom(b.cls(Cls::Ord)));
            return out;
        }
        // Map characters (math alphabets, operators) and split into atoms by class.
        let mut cur = String::new();
        let mut cur_k0 = 0usize;
        let mut cur_face: Option<FaceRef> = None;
        let mut synth_italic = false;
        let flush = |cur: &mut String, k0: usize, face: Option<FaceRef>, synth_italic: bool, out: &mut Vec<RunPart>| {
            if !cur.is_empty() {
                let f = face.unwrap_or(self.face);
                let b = self.text_atom(f, cur, size, st.level, color, (false, synth_italic), path, u0 + k0);
                out.push(RunPart::Atom(b.cls(Cls::Ord)));
                cur.clear();
            }
        };
        // A zero-width (or space) atom that takes no part in spacing, with its two slots.
        // 【中文标注】透明原子工厂：空格/零宽字符专用——不参与原子间字距，前后各留
        // 一个光标槽位。
        let blank = |w: f32, k: usize| {
            let mut b = Bx { w, transparent: true, ..Default::default() };
            b.slot(path, u0 + k, 0.0);
            b.slot(path, u0 + k + 1, w);
            b
        };
        for (k, c) in r.text.chars().enumerate() {
            if c == '&' && !r.lit {
                flush(&mut cur, cur_k0, cur_face, synth_italic, &mut out);
                out.push(RunPart::Align(blank(0.0, k)));
                continue;
            }
            let c = if r.lit {
                c
            } else {
                match c {
                    '-' => '−',
                    '*' => '∗',
                    '\'' => '′',
                    _ => c,
                }
            };
            let is_letter = c.is_alphabetic();
            let greek_cap = ('\u{391}'..='\u{3A9}').contains(&c);
            // 【中文标注】样式决定粗/斜；未指定时：拉丁字母默认斜体（数学惯例），大写
            // 希腊字母（Α..Ω）、函数名（upright）、非 Roman 书体与字面文本不正体化。
            let (bold, italic) = match r.sty {
                Some(MSty::Plain) => (false, false),
                Some(MSty::Bold) => (true, false),
                Some(MSty::Italic) => (false, true),
                Some(MSty::BoldItalic) => (true, true),
                None => (false, is_letter && !greek_cap && !st.upright && r.scr == MScr::Roman && !r.lit),
            };
            let mapped = math_alnum(c, bold, italic, r.scr);
            // Fall back to the plain letter (slanted) when the face lacks the math alphabet.
            // 【中文标注】数学字体缺该字母表字形时退回普通字母，用合成斜体替代。
            let (ch, slant) = if mapped != c && !self.face.covers(mapped) { (c, italic) } else { (mapped, false) };
            let face = if self.face.covers(ch) || ch.is_whitespace() {
                None
            } else {
                FontDb::global().fallback_for(ch, self.face.id()).map(|f| FaceRef::of(&f))
            };
            let cls = if r.lit { Cls::Ord } else { class_of(ch) };
            // 【中文标注】空格与不可见算子（U+2061 函数应用、U+2062 乘、U+2063 点乘、
            // U+200B 零宽空格）：产出透明原子；手工键入的空格宽约 0.4 em（Word 惯例）。
            if ch == ' ' || ch == '\u{2061}' || ch == '\u{2062}' || ch == '\u{2063}' || ch == '\u{200B}' {
                flush(&mut cur, cur_k0, cur_face, synth_italic, &mut out);
                // Word sets a typed space in an equation about 0.4 em wide.
                out.push(RunPart::Atom(blank(if ch == ' ' { size * 0.4 } else { 0.0 }, k)));
                continue;
            }
            let same_face = cur_face.map(|f| f.id()) == face.map(|f| f.id());
            if cls != Cls::Ord || !same_face || slant != synth_italic {
                flush(&mut cur, cur_k0, cur_face, synth_italic, &mut out);
            }
            if cls != Cls::Ord {
                let f = face.unwrap_or(self.face);
                let b = self.text_atom(f, &ch.to_string(), size, st.level, color, (false, slant), path, u0 + k);
                out.push(RunPart::Atom(b.cls(cls)));
                continue;
            }
            if cur.is_empty() {
                cur_k0 = k;
            }
            cur_face = face;
            synth_italic = slant;
            cur.push(ch);
        }
        flush(&mut cur, cur_k0, cur_face, synth_italic, &mut out);
        out
    }

    /// N-ary operators and functions: the operator atom (class Op) then the operand.
    /// 【中文标注】N 元算子（∑ ∫ 等）与函数名：先算子原子（类别 Op）再操作数。
    /// display 下放大算子：目标高 = DisplayOperatorMinHeight（无表 1.5em），grow 时至少
    /// 包住操作数；grow 走纵向伸展（尺寸变体/部件拼接），不 grow 时挑最接近 1.2em 的
    /// 尺寸变体（差距大则缩放近似）。算子居中于数学轴。上下限位置 LimLoc：积分号默认
    /// 侧旁、其余默认上下；行内一律侧旁。UndOvr 走 limits，SubSup 走 attach_op（下标
    /// 按斜体修正向左收）。函数名（Func）以 upright 样式排、操作数正常。
    fn op_atoms(&mut self, n: &MNode, st: St, depth: usize) -> Vec<Bx> {
        match n {
            MNode::Nary { chr, lim_loc, grow, sub_hide, sup_hide, sub, sup, e } => {
                let e = self.sub(2, e, st, depth);
                // 【中文标注】上下限位置：积分号默认侧旁，其余默认上下限；行内一律侧旁。
                let loc = lim_loc.unwrap_or(if is_integral(*chr) { LimLoc::SubSup } else { LimLoc::UndOvr });
                let loc = if st.display { loc } else { LimLoc::SubSup };
                let size = self.size(st);
                let base_gid = self.face.glyph_for(*chr);
                // 【中文标注】目标高：display 读 DisplayOperatorMinHeight；grow 时至少
                // 包住操作数的上/下伸。
                let mut target = 0.0f32;
                if st.display {
                    target = match &self.t {
                        Some(t) => t.display_operator_min_height * size / self.upem,
                        None => size * 1.5,
                    };
                }
                if *grow {
                    target = target.max(e.a + e.d);
                }
                let op = if *grow && target > 0.0 && self.t.is_some() {
                    self.stretch_v(base_gid, target, size).unwrap_or_else(|| self.glyph_id(base_gid, size, self.color))
                } else if target > 0.0 && self.t.is_some() {
                    // Word sets display operators about 1.2 em tall: the size variant nearest that.
                    // 【中文标注】不伸展时挑 advance 最接近 1.2 em 的尺寸变体。
                    let want = target.min(size * 1.2) * self.upem / size;
                    let (g, adv) = self
                        .t
                        .as_ref()
                        .and_then(|t| t.vert.get(&base_gid))
                        .and_then(|c| c.variants.iter().min_by(|a, b| (a.1 - want).abs().total_cmp(&(b.1 - want).abs())))
                        .copied()
                        .unwrap_or((base_gid, want));
                    // A font without a variant near that size: scale the nearest one.
                    // 【中文标注】最近变体差距仍大时按比例缩放近似（0.9..1.1 视作不缩）。
                    let f = if adv > 0.0 { (want / adv).clamp(0.6, 1.5) } else { 1.0 };
                    let f = if (0.9..=1.1).contains(&f) { 1.0 } else { f };
                    self.glyph_id(g, size * f, self.color)
                } else if base_gid != 0 {
                    self.glyph_id(base_gid, size, self.color)
                } else {
                    let s = if st.display { size * 1.4 } else { size };
                    self.glyphs(self.face, &chr.to_string(), s, 0, self.color, (false, false))
                };
                // Centre the operator on the math axis.
                // 【中文标注】算子居中于数学轴。
                let dy = self.axis(st) - (op.a - op.d) / 2.0;
                let mut ob = Bx { w: op.w, ..Default::default() };
                let ic = op.ic;
                ob.put(op, 0.0, dy);
                let sub = if *sub_hide { None } else { Some(self.sub(0, sub, st.script().cramp(), depth)) };
                let sup = if *sup_hide { None } else { Some(self.sub(1, sup, st.script(), depth)) };
                let opb = match loc {
                    LimLoc::UndOvr => self.limits(ob, sub, sup, ic, st, true),
                    LimLoc::SubSup => {
                        ob.ic = ic;
                        self.attach_op(ob, sub, sup, st)
                    }
                };
                vec![opb.cls(Cls::Op), e]
            }
            MNode::Func { name, e } => {
                let name = self.sub(0, name, St { upright: true, ..st }, depth);
                let e = self.sub(1, e, St { upright: false, ..st }, depth);
                vec![name.cls(Cls::Op), e]
            }
            _ => Vec::new(),
        }
    }

    /// Limits under and over `base` (centred), spaced as for big operators (`big`) or as
    /// accents-like limits.
    /// 【中文标注】上下限布局：底居中于最宽者。限的位移取大者——big（大算子）用
    /// BaselineRiseMin/DropMin 与 GapMin+限对侧伸展的较大者；非 big（lim 等）只用
    /// GapMin+对侧伸展。斜体修正 ic 对半摊：上限右移 ic/2、下限左移 ic/2（TeX 惯例）。
    fn limits(&self, base: Bx, under: Option<Bx>, over: Option<Bx>, ic: f32, st: St, big: bool) -> Bx {
        let w = base.w.max(under.as_ref().map_or(0.0, |b| b.w)).max(over.as_ref().map_or(0.0, |b| b.w));
        let mut out = Bx { w, ..Default::default() };
        let (ba, bd) = (base.a, base.d);
        let bw = base.w;
        out.put(base, (w - bw) / 2.0, 0.0);
        if let Some(o) = over {
            let rise = if big {
                self.k(C::UpperLimitBaselineRiseMin, st).max(self.k(C::UpperLimitGapMin, st) + o.d)
            } else {
                self.k(C::UpperLimitGapMin, st) + o.d
            };
            let x = (w - o.w) / 2.0 + ic / 2.0;
            out.put(o, x, ba + rise);
        }
        if let Some(u) = under {
            let drop = if big {
                self.k(C::LowerLimitBaselineDropMin, st).max(self.k(C::LowerLimitGapMin, st) + u.a)
            } else {
                self.k(C::LowerLimitGapMin, st) + u.a
            };
            let x = (w - u.w) / 2.0 - ic / 2.0;
            out.put(u, x, -bd - drop);
        }
        out
    }

    /// Limits beside a big operator: the superscript at its advance, the subscript tucked left
    /// under the slant by the italic correction.
    /// 【中文标注】大算子的侧旁上下标：上标接在 advance 之后；下标按斜体修正向左收
    /// （贴进算子斜出的尾部），下移量按 Subscript 三常数取大；宽度补 SpaceAfterScript。
    fn attach_op(&self, base: Bx, sub: Option<Bx>, sup: Option<Bx>, st: St) -> Bx {
        let ic = base.ic;
        let mut b = base;
        b.ic = 0.0;
        let (bw, bd) = (b.w, b.d);
        let mut out = self.attach(b, None, sup, st, false);
        if let Some(s) = sub {
            let v = self.k(C::SubscriptShiftDown, st).max(s.a - self.k(C::SubscriptTopMax, st)).max(bd + self.k(C::SubscriptBaselineDropMin, st));
            let x = (bw - ic).max(0.0);
            let sw = s.w;
            out.put(s, x, -v);
            out.w = out.w.max(x + sw + self.k(C::SpaceAfterScript, st));
        }
        out
    }

    /// Sub/superscripts after `base`. `char_base`: the base is a single character (no baseline drop).
    /// 【中文标注】普通上下标：位移按 MATH 表常数取大——上标 u = max(ShiftUp[Cramped],
    /// 上标底沿 + BottomMin；非单字符底再取 底上伸 − BaselineDropMax)；下标 v =
    /// max(ShiftDown, 下标顶沿 − TopMax；非单字符底再取 底下伸 + BaselineDropMin)。
    /// 同时有上下标时保证两者视觉间隙 ≥ SubSuperscriptGapMin：不够先压下标，再在
    /// BottomMaxWithSubscript 限额内上抬上标补足。char_base（底为单字符，如 x²）不做
    /// 基线下降。宽度补 SpaceAfterScript（只有底、无上下标时不加）。
    fn attach(&self, base: Bx, sub: Option<Bx>, sup: Option<Bx>, st: St, char_base: bool) -> Bx {
        let (bw, ba, bd, ic) = (base.w, base.a, base.d, base.ic);
        let (first, last) = (base.first, base.last);
        let mut out = Bx { w: bw, ..Default::default() };
        out.put(base, 0.0, 0.0);
        let mut u = 0.0f32;
        let mut v = 0.0f32;
        if let Some(p) = &sup {
            let shift = if st.cramped { self.k(C::SuperscriptShiftUpCramped, st) } else { self.k(C::SuperscriptShiftUp, st) };
            u = shift.max(p.d + self.k(C::SuperscriptBottomMin, st));
            if !char_base {
                u = u.max(ba - self.k(C::SuperscriptBaselineDropMax, st));
            }
        }
        if let Some(b) = &sub {
            v = self.k(C::SubscriptShiftDown, st).max(b.a - self.k(C::SubscriptTopMax, st));
            if !char_base {
                v = v.max(bd + self.k(C::SubscriptBaselineDropMin, st));
            }
        }
        if let (Some(p), Some(b)) = (&sup, &sub) {
            let gap = (u - p.d) - (b.a - v);
            let min = self.k(C::SubSuperscriptGapMin, st);
            if gap < min {
                v += min - gap;
                let lift = self.k(C::SuperscriptBottomMaxWithSubscript, st) - (u - p.d);
                if lift > 0.0 {
                    u += lift;
                    v -= lift;
                }
            }
        }
        let space = self.k(C::SpaceAfterScript, st);
        let mut w = bw;
        if let Some(p) = sup {
            let x = bw + ic;
            w = w.max(x + p.w);
            out.put(p, x, u);
        }
        if let Some(b) = sub {
            let x = bw;
            w = w.max(x + b.w);
            out.put(b, x, -v);
        }
        out.w = w + if w > bw { space } else { 0.0 };
        out.first = first;
        out.last = last;
        out
    }

    /// A vertical size variant or assembly of `gid` at least `target` tall (`None` without a
    /// `MATH` table or construction for the glyph).
    /// 【中文标注】纵向伸展（定界符/根号/大算子）：取至少 target 高的尺寸变体或部件
    /// 拼接；无 MATH 表或该字形无构造记录时 None，调用方退回原字形。
    fn stretch_v(&self, gid: u32, target: f32, size: f32) -> Option<Bx> {
        let t = self.t.as_ref()?;
        let con = t.vert.get(&gid)?;
        self.stretch(con, t, target, size, true)
    }

    /// 【中文标注】横向伸展（宽重音/组合括号线等）：取至少 target 宽的尺寸变体或拼接。
    fn stretch_h(&self, gid: u32, target: f32, size: f32) -> Option<Bx> {
        let t = self.t.as_ref()?;
        let con = t.horiz.get(&gid)?;
        self.stretch(con, t, target, size, false)
    }

    /// 【中文标注】伸展核心（OpenType MATH 表 MathVariants 的 variants + parts）：先挑
    /// 尺寸变体，不够再用 GlyphPart 拼接。垂直拼接时部件自下而上堆叠（y = 已排长度 −
    /// 部件墨迹底，使墨迹连续），总盒 a=总高、d=0；水平拼接从左到右排，总盒 w=总长。
    fn stretch(&self, con: &Construction, t: &MathTable, target: f32, size: f32, vertical: bool) -> Option<Bx> {
        let k = size / self.upem;
        let target_fu = target / k;
        // The first variant big enough.
        // 【中文标注】优先尺寸变体：取第一个够高/宽（advance ≥ 目标−1）的。
        if let Some((g, _)) = con.variants.iter().find(|(_, adv)| *adv >= target_fu - 1.0) {
            return Some(self.glyph_id(*g, size, self.color));
        }
        if con.parts.is_empty() {
            return con.variants.last().map(|(g, _)| self.glyph_id(*g, size, self.color));
        }
        // Assembly: repeat extenders until the parts span the target.
        // 【中文标注】部件拼接（GlyphPart）：固定件各 1 个 + 延伸件重复 reps 个，总长
        // （减去接缝最小重叠 min_connector_overlap）达到目标为止；reps ≤100 防失控。
        let o = t.min_connector_overlap;
        let fixed: f32 = con.parts.iter().filter(|p| !p.extender).map(|p| p.full_advance).sum();
        let ext: f32 = con.parts.iter().filter(|p| p.extender).map(|p| p.full_advance).sum();
        let n_fixed = con.parts.iter().filter(|p| !p.extender).count();
        let n_ext = con.parts.len() - n_fixed;
        let mut reps = 0usize;
        if n_ext > 0 {
            while reps < 100 {
                let n = n_fixed + n_ext * reps;
                let len = fixed + ext * reps as f32 - o * n.saturating_sub(1) as f32;
                if len >= target_fu {
                    break;
                }
                reps += 1;
            }
        }
        let seq: Vec<_> = con.parts.iter().flat_map(|p| std::iter::repeat_n(*p, if p.extender { reps } else { 1 })).collect();
        if seq.is_empty() {
            return None;
        }
        let joints = seq.len().saturating_sub(1);
        let max_len: f32 = seq.iter().map(|p| p.full_advance).sum::<f32>() - o * joints as f32;
        // Spread the excess over the joints as extra overlap (within each connector's length).
        // 【中文标注】reps 拼出的最大长度通常超过目标：超出量均摊到各接缝作额外重叠
        // （重叠越多总长越短），但不超过相邻两件连接器长度的较小值（room）。
        let excess = (max_len - target_fu).max(0.0);
        let per = if joints > 0 { excess / joints as f32 } else { 0.0 };
        let mut b = Bx::default();
        let mut pos = 0.0f32;
        let mut glyphs = Vec::with_capacity(seq.len());
        for (i, p) in seq.iter().enumerate() {
            if vertical {
                let y0 = glyph_bounds(&self.face, p.gid).map(|bb| bb.1).unwrap_or(0.0);
                glyphs.push((p.gid, 0.0, (pos - y0) * k));
                b.w = b.w.max(self.face.advance(p.gid) as f32 * k);
            } else {
                glyphs.push((p.gid, pos * k, 0.0));
                if let Some((_, y0, _, y1)) = glyph_bounds(&self.face, p.gid) {
                    b.a = b.a.max(y1 * k);
                    b.d = b.d.max(-y0 * k);
                }
            }
            if let Some(next) = seq.get(i + 1) {
                let room = p.end_connector.min(next.start_connector).max(o);
                let ov = (o + per).min(room);
                pos += p.full_advance - ov;
            } else {
                pos += p.full_advance;
            }
        }
        if vertical {
            b.a = pos * k;
            b.d = 0.0;
        } else {
            b.w = pos * k;
        }
        b.items.push(MItem::Glyphs { face: self.face, size, color: self.color, synth_bold: false, synth_italic: false, glyphs, text: String::new() });
        Some(b)
    }

    /// A delimiter (or separator) glyph covering `a`/`d` around the axis (or the content exactly
    /// with `exact`).
    /// 【中文标注】定界符：目标高——exact（如 |x| 的竖线）= 内容高 a+d 精确包住；
    /// 括号类 = 2×max(上伸−轴, 下伸+轴)，即跨数学轴对称伸展。grow=false 直接用原字形；
    /// 内容只比原字形高 0.02em 以内不折腾；伸展失败退回原字形。定位：exact 按内容
    /// 垂直居中，否则按数学轴居中。
    fn delimiter(&self, c: char, a: f32, d: f32, st: St, grow: bool, exact: bool) -> Bx {
        let size = self.size(st);
        let axis = self.axis(st);
        let mut glyph = self.glyphs(self.face, &c.to_string(), size, 0, self.color, (false, false));
        glyph.glyph = None;
        if !grow {
            return glyph;
        }
        let target = if exact { a + d } else { 2.0 * (a - axis).max(d + axis) };
        // Small content keeps the plain glyph where it sits.
        // 【中文标注】内容不高出原字形（0.02em 容差）时保留原字形原位。
        if glyph.a + glyph.d >= target - size * 0.02 {
            return glyph;
        }
        let gid = self.face.glyph_for(c);
        let Some(big) = (if gid != 0 { self.stretch_v(gid, target, size) } else { None }) else { return glyph };
        let dy = if exact { (a - d) / 2.0 - (big.a - big.d) / 2.0 } else { axis - (big.a - big.d) / 2.0 };
        let mut out = Bx { w: big.w, ..Default::default() };
        out.put(big, 0.0, dy);
        out
    }

    /// 【中文标注】单个 MNode 的布局分派：每种结构一个分支，子参数经 sub() 递归
    /// （depth+1，超 MAX_DEPTH 返回空盒）。结构间距全部读 MATH 表常数（Ctx::k）：
    /// 分数/根式/上下标/上下限/顶底线/边框/矩阵各有专属常数组。
    fn node(&mut self, n: &MNode, st: St, depth: usize) -> Bx {
        if depth > MAX_DEPTH {
            return Bx::default();
        }
        let d = depth + 1;
        let em = self.em(st);
        match n {
            MNode::Run(r) => {
                let atoms: Vec<Bx> = self
                    .run_atoms(r, st, 0, &self.path.clone())
                    .into_iter()
                    .map(|p| match p {
                        RunPart::Atom(b) | RunPart::Align(b) => b,
                    })
                    .collect();
                compose(atoms, st, em)
            }
            MNode::Frac { kind, num, den } => self.frac(*kind, num, den, st, d),
            MNode::Script { kind, base, sub, sup } => {
                let b = self.sub(0, base, st, d);
                // 【中文标注】底恰为单字形（如 x²）时上下标不做基线下降（见 attach）。
                let char_base = b.glyph.is_some();
                let sub_b = if matches!(kind, ScriptKind::Sub | ScriptKind::SubSup | ScriptKind::Pre) {
                    Some(self.sub(1, sub, st.script().cramp(), d))
                } else {
                    None
                };
                let sup_b = if matches!(kind, ScriptKind::Sup | ScriptKind::SubSup | ScriptKind::Pre) {
                    Some(self.sub(2, sup, st.script(), d))
                } else {
                    None
                };
                if *kind == ScriptKind::Pre {
                    // Pre-scripts: attach to an empty base, then put the base after them.
                    // 【中文标注】前置上下标（ⁿPᵣ 型）：先在空底上排上下标，再把真正的底
                    // 接在其后；首末类别沿用底的（整体仍当一个“底”参与字距）。
                    let empty = Bx::default();
                    let mut pre = self.attach(empty, sub_b, sup_b, st, false);
                    let pw = pre.w;
                    let (f, l) = (b.first, b.last);
                    pre.w = pw + b.w;
                    pre.put(b, pw, 0.0);
                    pre.first = f;
                    pre.last = l;
                    return pre;
                }
                self.attach(b, sub_b, sup_b, st, char_base)
            }
            MNode::Rad { deg, deg_hide, e } => self.radical(deg, *deg_hide, e, st, d),
            MNode::Nary { .. } | MNode::Func { .. } => {
                let atoms = self.op_atoms(n, st, d);
                compose(atoms, st, em)
            }
            // 【中文标注】定界符组：上/下伸取各部件最大值，grow 时首/尾/分隔符随之伸展；
            // 内部不加原子间字距（hcat）；盒子首末类别按有无首/尾括号设定。
            MNode::Delim { beg, end, sep, grow, shp_match, elems } => {
                let parts: Vec<Bx> = elems.iter().enumerate().map(|(k, e)| self.sub(k, e, st, d)).collect();
                let a = parts.iter().map(|b| b.a).fold(0.0f32, f32::max);
                let dd = parts.iter().map(|b| b.d).fold(0.0f32, f32::max);
                let mut row: Vec<Bx> = Vec::new();
                if let Some(c) = beg {
                    row.push(self.delimiter(*c, a, dd, st, *grow, *shp_match).cls(Cls::Open));
                }
                let n = parts.len();
                for (i, p) in parts.into_iter().enumerate() {
                    row.push(p.cls(Cls::Inner));
                    if i + 1 < n
                        && let Some(c) = sep
                    {
                        row.push(self.delimiter(*c, a, dd, st, *grow, *shp_match).cls(Cls::Ord));
                    }
                }
                if let Some(c) = end {
                    row.push(self.delimiter(*c, a, dd, st, *grow, *shp_match).cls(Cls::Close));
                }
                // No spacing inside the brackets.
                let mut out = hcat(row);
                out.first = if beg.is_some() { Cls::Open } else { Cls::Inner };
                out.last = if end.is_some() { Cls::Close } else { Cls::Inner };
                out
            }
            // 【中文标注】极限（lim 的上下限）：底正常排，限取 script 层级且不正体；
            // 在下方时用紧缩样式（cramp）；upper 决定摆上方还是下方。
            MNode::Lim { upper, e, lim } => {
                let base = self.sub(0, e, st, d);
                let ls = St { upright: false, ..st.script() };
                let l = self.sub(1, lim, if *upper { ls } else { ls.cramp() }, d);
                let (f, la) = (base.first, base.last);
                let mut out =
                    if *upper { self.limits(base, None, Some(l), 0.0, st, false) } else { self.limits(base, Some(l), None, 0.0, st, false) };
                out.first = f;
                out.last = la;
                out
            }
            MNode::Acc { chr, e } => self.accent(*chr, e, st, d),
            // 【中文标注】顶/底线（overline/underline）：内容上（下）方留 Overbar/
            // UnderbarVerticalGap 后画一条规则线（线粗常数，≥0.4pt），并计入
            // ExtraAscender/ExtraDescender。
            MNode::Bar { top, e } => {
                let b = self.sub(0, e, st, d);
                let (w, a, dd) = (b.w, b.a, b.d);
                let mut out = Bx { w, ..Default::default() };
                out.put(b, 0.0, 0.0);
                let color = self.color;
                if *top {
                    let t = self.k(C::OverbarRuleThickness, st).max(0.4);
                    let y = a + self.k(C::OverbarVerticalGap, st);
                    out.rect(0.0, y, w, t, color);
                    out.a = out.a.max(y + t + self.k(C::OverbarExtraAscender, st));
                } else {
                    let t = self.k(C::UnderbarRuleThickness, st).max(0.4);
                    let y = -dd - self.k(C::UnderbarVerticalGap, st) - t;
                    out.rect(0.0, y, w, t, color);
                    out.d = out.d.max(-y + self.k(C::UnderbarExtraDescender, st));
                }
                out
            }
            // 【中文标注】边框盒：内边距 0.12em，线粗取分式线常数（≥0.4pt）；hide 逐边
            // 隐藏（上/下/左/右）；strike 为四种删除线——水平/垂直/左下→右上/左上→右下。
            MNode::BorderBox { hide, strike, e } => {
                let b = self.sub(0, e, st, d);
                let t = self.k(C::FractionRuleThickness, st).max(0.4);
                let pad = em * 0.12;
                let (w, a, dd) = (b.w + 2.0 * pad, b.a + pad, b.d + pad);
                let mut out = Bx { w: w + 2.0 * t, ..Default::default() };
                out.put(b, pad + t, 0.0);
                let color = self.color;
                let [ht, hb, hl, hr] = *hide;
                let (x0, x1, y0, y1) = (0.0, w + 2.0 * t, -dd - t, a + t);
                if !ht {
                    out.rect(x0, a, x1 - x0, t, color);
                }
                if !hb {
                    out.rect(x0, y0, x1 - x0, t, color);
                }
                if !hl {
                    out.rect(x0, y0, t, y1 - y0, color);
                }
                if !hr {
                    out.rect(x1 - t, y0, t, y1 - y0, color);
                }
                let [sh, sv, bltr, tlbr] = *strike;
                let mid = (a - dd) / 2.0;
                let line = |out: &mut Bx, p: (f32, f32, f32, f32)| {
                    out.items.push(MItem::Line { x0: p.0, y0: p.1, x1: p.2, y1: p.3, width: t, color, dotted: false })
                };
                if sh {
                    line(&mut out, (x0, mid, x1, mid));
                }
                if sv {
                    line(&mut out, ((x0 + x1) / 2.0, y0, (x0 + x1) / 2.0, y1));
                }
                if bltr {
                    line(&mut out, (x0, y0, x1, y1));
                }
                if tlbr {
                    line(&mut out, (x0, y1, x1, y0));
                }
                out.a = out.a.max(y1);
                out.d = out.d.max(-y0);
                out
            }
            MNode::Boxed { e } => self.sub(0, e, st, d),
            // 【中文标注】组合符（底上/下方的花括号、箭头等）：符号横向伸展铺满底宽，
            // 按 StretchStackGapAbove/BelowMin 摆在底上方或下方，两者整体居中。
            MNode::GroupChr { chr, top, e } => {
                let b = self.sub(0, e, st, d);
                let size = self.size(st);
                let gid = self.face.glyph_for(*chr);
                let g =
                    self.stretch_h(gid, b.w, size).unwrap_or_else(|| self.glyphs(self.face, &chr.to_string(), size, 0, self.color, (false, false)));
                let w = b.w.max(g.w);
                let (ba, bd, bw) = (b.a, b.d, b.w);
                let (ga, gd, gw) = (g.a, g.d, g.w);
                let mut out = Bx { w, ..Default::default() };
                out.put(b, (w - bw) / 2.0, 0.0);
                if *top {
                    let y = ba + self.k(C::StretchStackGapAboveMin, st) + gd;
                    out.put(g, (w - gw) / 2.0, y);
                } else {
                    let y = -bd - self.k(C::StretchStackGapBelowMin, st) - ga;
                    out.put(g, (w - gw) / 2.0, y);
                }
                out
            }
            MNode::EqArr { rows } => self.eq_arr(rows, st, d),
            MNode::Matrix { rows, col_jc } => self.matrix(rows, col_jc, st, d),
            // 【中文标注】幻影（Phantom）：保留占位但可分别隐藏墨迹/宽度/上伸/下伸。
            MNode::Phant { show, zero_wid, zero_asc, zero_desc, e } => {
                let mut b = self.sub(0, e, st, d);
                if !show {
                    b.items.clear();
                }
                if *zero_wid {
                    b.w = 0.0;
                }
                if *zero_asc {
                    b.a = 0.0;
                }
                if *zero_desc {
                    b.d = 0.0;
                }
                b.glyph = None;
                b
            }
        }
    }

    /// 【中文标注】分数三种形态：
    /// - Linear 线性：分子/斜杠/分母一行，均用当前层级字号；
    /// - Skewed 斜式：分子抬到数学轴上方、分母压到下方（SkewedFraction 常数），中间放
    ///   分数斜杠 U+2044，左右各留 HorizontalGap/2；
    /// - Bar/NoBar 堆叠式：分子保持外层 cramped、分母一律紧缩；Bar 时位移 = max(位移
    ///   常数, 线半粗 ± 数学轴 + 对侧伸展 + 分式线间隙常数)（display 与行内读不同的
    ///   Display/普通常数组）；NoBar 用 Stack 常数、间隙不足时上下各让一半。盒宽 =
    ///   最宽者 + 2×0.08em 内边距；分式线画在数学轴上（厚 max(RuleThickness, 0.4pt)）。
    fn frac(&mut self, kind: FracKind, num: &[MNode], den: &[MNode], st: St, d: usize) -> Bx {
        let em = self.em(st);
        match kind {
            FracKind::Linear => {
                let n = self.sub(0, num, st, d);
                let dn = self.sub(1, den, st, d);
                let slash = self.glyphs(self.face, "/", self.size(st), st.level, self.color, (false, false)).cls(Cls::Ord);
                hcat(vec![n, slash, dn])
            }
            FracKind::Skewed => {
                let n = self.sub(0, num, st.script(), d);
                let dn = self.sub(1, den, st.script().cramp(), d);
                let size = self.size(st);
                let slash = self.glyphs(self.face, "\u{2044}", size, 0, self.color, (false, false));
                let gap = self.k(C::SkewedFractionHorizontalGap, st) / 2.0;
                let (nw, sw) = (n.w, slash.w);
                let mut out = Bx { w: n.w + gap + slash.w + gap + dn.w, ..Default::default() };
                let up = self.axis(st) + self.k(C::SkewedFractionVerticalGap, st) / 2.0;
                out.put(n, 0.0, up.max(0.0));
                out.put(slash, nw + gap, 0.0);
                out.put(dn, nw + gap + sw + gap, -self.k(C::SkewedFractionVerticalGap, st) / 2.0);
                out
            }
            FracKind::Bar | FracKind::NoBar => {
                let cs = st.frac();
                // 【中文标注】分子保持外层的 cramped；分母一律紧缩（TeX 惯例）。
                let n = self.sub(0, num, St { cramped: st.cramped, ..cs }, d);
                let dn = self.sub(1, den, cs.cramp(), d);
                let axis = self.axis(st);
                let (u, v) = if kind == FracKind::Bar {
                    let t = self.k(C::FractionRuleThickness, st);
                    // 【中文标注】分子上移 u / 分母下移 v：位移常数与“线半粗 ± 数学轴 +
                    // 对侧伸展 + 分式线间隙常数”取大（间隙不小于 MATH 表常数）；display
                    // 与行内读不同的常数组。
                    let (su, sd, gn, gd) = if st.display {
                        (
                            self.k(C::FractionNumeratorDisplayStyleShiftUp, st),
                            self.k(C::FractionDenominatorDisplayStyleShiftDown, st),
                            self.k(C::FractionNumDisplayStyleGapMin, st),
                            self.k(C::FractionDenomDisplayStyleGapMin, st),
                        )
                    } else {
                        (
                            self.k(C::FractionNumeratorShiftUp, st),
                            self.k(C::FractionDenominatorShiftDown, st),
                            self.k(C::FractionNumeratorGapMin, st),
                            self.k(C::FractionDenominatorGapMin, st),
                        )
                    };
                    (su.max(gn + t / 2.0 + axis + n.d), sd.max(gd + t / 2.0 - axis + dn.a))
                } else {
                    // 【中文标注】无横线（Stack）：上下位移用 Stack 常数组；实际间隙不足
                    // 时上下各让一半补足。
                    let (mut su, mut sd, gap) = if st.display {
                        (
                            self.k(C::StackTopDisplayStyleShiftUp, st),
                            self.k(C::StackBottomDisplayStyleShiftDown, st),
                            self.k(C::StackDisplayStyleGapMin, st),
                        )
                    } else {
                        (self.k(C::StackTopShiftUp, st), self.k(C::StackBottomShiftDown, st), self.k(C::StackGapMin, st))
                    };
                    let g = (su - n.d) - (dn.a - sd);
                    if g < gap {
                        su += (gap - g) / 2.0;
                        sd += (gap - g) / 2.0;
                    }
                    (su, sd)
                };
                let pad = em * 0.08;
                let w = n.w.max(dn.w) + 2.0 * pad;
                let (nw, dw) = (n.w, dn.w);
                let mut out = Bx { w, ..Default::default() };
                out.put(n, (w - nw) / 2.0, u);
                out.put(dn, (w - dw) / 2.0, -v);
                if kind == FracKind::Bar {
                    let t = self.k(C::FractionRuleThickness, st).max(0.4);
                    out.rect(pad * 0.5, axis - t / 2.0, w - pad, t, self.color);
                }
                out
            }
        }
    }

    /// 【中文标注】根式：被开方数用紧缩样式；目标高 = 被开方数高 + 根式垂直间隙
    /// （display 读 RadicalDisplayStyleVerticalGap）+ 线粗；√ 号纵向伸展到目标（失败
    /// 退回原字形），多余高度摊回间隙（extra/2），使横杠恰好贴着被开方数顶；横杠为
    /// 矩形（厚 max(RuleThickness, 0.4pt)）。次数按 level+2 的紧缩样式排，以
    /// RadicalDegreeBottomRaisePercent 抬到 √ 的弯钩处，前后按 RadicalKernBefore/
    /// AfterDegree 收放（After 为负，把 √ 往左拉）；上伸另计 RadicalExtraAscender。
    fn radical(&mut self, deg: &[MNode], deg_hide: bool, e: &[MNode], st: St, d: usize) -> Bx {
        let b = self.sub(1, e, st.cramp(), d);
        let size = self.size(st);
        let t = self.k(C::RadicalRuleThickness, st).max(0.4);
        let mut gap = if st.display { self.k(C::RadicalDisplayStyleVerticalGap, st) } else { self.k(C::RadicalVerticalGap, st) };
        let target = b.a + b.d + gap + t;
        let gid = self.face.glyph_for('√');
        let g = self.stretch_v(gid, target, size).unwrap_or_else(|| {
            let mut g = self.glyphs(self.face, "√", size, 0, self.color, (false, false));
            g.glyph = None;
            g
        });
        let extra = (g.a + g.d) - target;
        if extra > 0.0 {
            gap += extra / 2.0;
        }
        // The sign's top meets the rule over the base.
        // 【中文标注】√ 顶对齐横杠顶：dy = 横杠顶 − √ 的上伸。
        let top = b.a + gap + t;
        let dy = top - g.a;
        let (ga, gd, gw) = (g.a, g.d, g.w);
        let mut sign = Bx { w: gw, ..Default::default() };
        sign.put(g, 0.0, dy);
        // Degree.
        // 【中文标注】次数（ⁿ√ 的 n）：抬高到弯钩处，前后按 Kern 常数收放。
        let mut x0 = 0.0f32;
        let mut out = Bx::default();
        if !deg_hide {
            let db = self.sub(0, deg, St { level: (st.level + 2).min(2), display: false, cramped: true, upright: st.upright }, d);
            let kb = self.k(C::RadicalKernBeforeDegree, st);
            let ka = self.k(C::RadicalKernAfterDegree, st);
            // 【中文标注】次数抬升比例 RadicalDegreeBottomRaisePercent；无 MATH 表时
            // 取 60% 兜底（Cambria Math 的实际值附近）。
            let raise = self.t.as_ref().map_or(60.0, |t| t.radical_degree_bottom_raise_percent) / 100.0 * (ga + gd);
            let y = (dy - gd) + raise;
            let dw = db.w;
            out.put(db, kb, y);
            x0 = (kb + dw + ka).max(0.0);
        }
        let bw = b.w;
        out.put(sign, x0, 0.0);
        out.rect(x0 + gw, top - t, bw, t, self.color);
        out.put(b, x0 + gw, 0.0);
        out.w = x0 + gw + bw;
        out.a = out.a.max(top + self.k(C::RadicalExtraAscender, st));
        out
    }

    /// 【中文标注】重音（帽子/箭头/划线等）：划线类（U+0305、¯、U+0304、‾）直接画
    /// Overbar 矩形；宽重音（wide_accent）且底足够宽时横向伸展铺满底，否则单字形。
    /// 水平对位：底的单字符附着点对准重音自身的 TopAccentAttachments 点（无表取墨迹
    /// 中点）；负偏移且重音比底宽时改为居中。垂直：底高于 AccentBaseHeight（x 字高）
    /// 时把重音抬起。完成后清除单字符记录（带重音的整体不再当单字符底），保留斜体修正。
    fn accent(&mut self, chr: char, e: &[MNode], st: St, d: usize) -> Bx {
        let b = self.sub(0, e, st.cramp(), d);
        let size = self.size(st);
        let color = self.color;
        // Overline-style accents are rules.
        // 【中文标注】划线类重音退化为顶线矩形（同 Bar top 的画法）。
        if matches!(chr, '\u{305}' | '\u{AF}' | '\u{304}' | '\u{203E}') {
            let (w, a) = (b.w, b.a);
            let mut out = Bx { w, ..Default::default() };
            let (f, l) = (b.first, b.last);
            out.put(b, 0.0, 0.0);
            let t = self.k(C::OverbarRuleThickness, st).max(0.4);
            let y = a + self.k(C::OverbarVerticalGap, st);
            out.rect(0.0, y, w, t, color);
            out.first = f;
            out.last = l;
            return out;
        }
        let gid = self.face.glyph_for(chr);
        // Combining marks have no advance: compare the base with the accent's ink.
        // 【中文标注】组合符号无步进宽度：用墨迹宽判断底是否“窄”（≤1.2× 重音墨迹），
        // 窄底不做宽重音伸展。
        let ink = glyph_bounds(&self.face, gid).map(|(x0, _, x1, _)| (x1 - x0) * size / self.upem).unwrap_or(0.0);
        let narrow = gid != 0 && b.w <= ink * 1.2;
        let mut acc = if wide_accent(chr) && gid != 0 && !narrow {
            self.stretch_h(gid, b.w, size).unwrap_or_else(|| self.glyph_id(gid, size, color))
        } else if gid != 0 {
            self.glyph_id(gid, size, color)
        } else {
            self.glyphs(self.face, &chr.to_string(), size, 0, color, (false, false))
        };
        acc.glyph = None;
        // Horizontal: the base's attachment point under the accent's.
        // 【中文标注】水平对位：x = 底附着点 − 重音附着点。
        let base_x = b.glyph.map(|(_, _, ax)| ax).unwrap_or(b.w / 2.0);
        let acc_x = {
            let single = acc.items.first().and_then(|it| match it {
                MItem::Glyphs { glyphs, .. } if glyphs.len() == 1 => glyphs.first().map(|g| g.0),
                _ => None,
            });
            let k = size / self.upem;
            match single {
                Some(g) => match self.t.as_ref().and_then(|t| t.top_accent.get(&g).copied()) {
                    Some(v) => v * k,
                    None => glyph_bounds(&self.face, g).map(|(x0, _, x1, _)| (x0 + x1) / 2.0 * k).unwrap_or(acc.w / 2.0),
                },
                None => acc.w / 2.0,
            }
        };
        // Vertical: accents sit on an x-height base; taller bases push them up.
        // 【中文标注】垂直定位：重音按 AccentBaseHeight（x 字高）摆，更高的底把它抬起。
        let abh = self.k(C::AccentBaseHeight, st);
        let raise = (b.a - abh).max(0.0);
        let (f, l, w, ic) = (b.first, b.last, b.w, b.ic);
        let glyph = b.glyph;
        let mut out = Bx { w, ..Default::default() };
        out.put(b, 0.0, 0.0);
        let mut x = base_x - acc_x;
        if x < 0.0 && acc.w > w {
            x = (w - acc.w) / 2.0;
        }
        out.put(acc, x, raise);
        out.first = f;
        out.last = l;
        out.ic = ic;
        out.glyph = glyph.filter(|_| false);
        out
    }

    /// Rows stacked and centred on the math axis; `gap`: least space between rows, `min_skip`:
    /// least baseline-to-baseline distance.
    /// 【中文标注】多行堆叠（方程组/矩阵公用）：第 n 行基线 = 上一行基线 − max(上一行
    /// 下伸 + gap + 本行上伸, min_skip)；整体沿数学轴垂直居中。返回 (总盒, 各行基线)，
    /// 基线表供 numbered 把编号对齐到各行。
    fn stack_rows(&self, rows: Vec<(Bx, f32)>, st: St, gap: f32, min_skip: f32) -> (Bx, Vec<f32>) {
        // 【中文标注】自上而下累计各行基线 y（向下为负）。
        // (row box, x) → baselines going down.
        let mut ys = Vec::with_capacity(rows.len());
        let mut y = 0.0f32;
        let mut prev_d: Option<f32> = None;
        for (b, _) in &rows {
            if let Some(pd) = prev_d {
                y -= (pd + gap + b.a).max(min_skip);
            }
            ys.push(y);
            prev_d = Some(b.d);
        }
        let top = rows.first().map_or(0.0, |(b, _)| b.a);
        let bottom = rows.last().map_or(0.0, |(b, _)| b.d) - ys.last().copied().unwrap_or(0.0);
        // Centre on the axis.
        // 【中文标注】整体平移：数学轴对准内容块的垂直中点。
        let shift = self.axis(st) + (bottom - top) / 2.0;
        let mut out = Bx::default();
        let mut bases = Vec::with_capacity(ys.len());
        for ((b, x), yy) in rows.into_iter().zip(ys) {
            out.w = out.w.max(x + b.w);
            out.put(b, x, yy + shift);
            bases.push(yy + shift);
        }
        (out, bases)
    }

    /// 【中文标注】方程组（无编号路径）：只取行布局，丢弃基线表。
    fn eq_arr(&mut self, rows: &[Arg], st: St, d: usize) -> Bx {
        self.eq_rows(rows, st, d).0
    }

    /// Equation-array rows aligned at `&`, and each row's baseline.
    /// 【中文标注】方程组按 & 对齐：每行拆成对齐段（row() 里按 & 分段，≤64 段、≤1000
    /// 行）；原子前的字距并入段盒（列宽计算含字距）；列宽 = 各行同段最大宽；摆放为
    /// “偶数段（= 左列）右对齐、奇数段左对齐”（等号上下对齐的 Word/TeX 惯例），无 &
    /// 的行与第一列右缘对齐，单列时整组水平居中。行距经 stack_rows（gap=0.1em，最小
    /// 基线距 1.1em）。返回 (总盒, 各行基线)。
    fn eq_rows(&mut self, rows: &[Arg], st: St, d: usize) -> (Bx, Vec<f32>) {
        let em = self.em(st);
        let me = self.node_stack.last().copied().unwrap_or(0);
        // Each row: segments split at `&`.
        // 【中文标注】逐行：光标路径压入行号后拆 & 段并计算段前字距。
        let mut segs_rows: Vec<Vec<Bx>> = Vec::new();
        let mut row_paths = Vec::new();
        for (ri, r) in rows.iter().take(1000).enumerate() {
            self.path.push((me, ri));
            row_paths.push(self.path.clone());
            let atoms = if r.is_empty() { vec![(self.placeholder(st), 0)] } else { self.row(r, st, d, 0) };
            self.path.pop();
            let n = atoms.iter().map(|(_, s)| *s + 1).max().unwrap_or(1).min(64);
            let all: Vec<Bx> = atoms.iter().map(|(b, _)| b.clone()).collect();
            let gaps = gaps_between(&all, st, em);
            let mut segs: Vec<Vec<Bx>> = vec![Vec::new(); n];
            for (i, (mut b, s)) in atoms.into_iter().enumerate() {
                // Keep the spacing before an atom with the atom.
                // 【中文标注】把原子前的字距包进原子盒——列对齐时字距跟着原子走。
                let g = gaps.get(i).copied().unwrap_or(0.0);
                if g > 0.0 {
                    let mut wrapped = Bx { w: g + b.w, a: b.a, d: b.d, first: b.first, last: b.last, ..Default::default() };
                    wrapped.put(b, g, 0.0);
                    b = wrapped;
                }
                if let Some(seg) = segs.get_mut(s.min(n - 1)) {
                    seg.push(b);
                }
            }
            segs_rows.push(segs.into_iter().map(hcat).collect());
        }
        let ncols = segs_rows.iter().map(|r| r.len()).max().unwrap_or(1);
        let mut col_w = vec![0.0f32; ncols];
        for r in &segs_rows {
            for (i, b) in r.iter().enumerate() {
                if let Some(c) = col_w.get_mut(i) {
                    *c = c.max(b.w);
                }
            }
        }
        let total: f32 = col_w.iter().sum();
        let mut placed = Vec::new();
        for (r, path) in segs_rows.into_iter().zip(row_paths) {
            let mut row = Bx::default();
            let mut x = 0.0f32;
            let single = r.len() == 1 && ncols > 1;
            for (i, b) in r.into_iter().enumerate() {
                let cw = col_w.get(i).copied().unwrap_or(0.0);
                let bw = b.w;
                let dx = if ncols == 1 {
                    0.0
                } else if single {
                    // A row without `&` aligns with the first column's right edge.
                    // 【中文标注】无 & 的行与第一列（等号左列）右缘对齐。
                    cw - bw
                } else if i % 2 == 0 {
                    // 【中文标注】偶数段 = 等号左列：右对齐。
                    cw - bw
                } else {
                    0.0
                };
                row.put(b, x + dx, 0.0);
                x += cw;
                row.w = row.w.max(x);
            }
            let rx = if ncols == 1 { (total - row.w) / 2.0 } else { 0.0 };
            finalize(&mut row, &path, em);
            placed.push((row, rx.max(0.0)));
        }
        let (mut out, bases) = self.stack_rows(placed, st, em * 0.1, em * 1.1);
        out.w = out.w.max(total);
        (out, bases)
    }

    /// A display equation with numbers (`#`): the equation centred on the line, each row's
    /// number at the right margin. Automatic numbers (`#` alone) count up from `counter`.
    /// 【中文标注】带编号的 display 公式：每行按顶层 # 切成 (公式, 编号)（split_number）。
    /// 编号为空 = 自动编号：counter 自增生成 "(n)"（Plain 正体；每个带 # 的行都消耗一个
    /// 编号，光标槽位只留编号前一位）；非空则排用户编号。公式整体居中；居中后放不下
    /// “公式 + 1em + 编号”时改左对齐让位；编号贴行右缘、与各行基线平齐；盒宽补足整行。
    fn numbered(&mut self, rows: &[Arg], st: St, avail: f32, counter: &mut u32) -> Bx {
        let em = self.em(st);
        let parts: Vec<(Arg, Arg, usize)> =
            rows.iter().take(1000).map(|r| split_number(r).unwrap_or_else(|| (r.clone(), Vec::new(), usize::MAX))).collect();
        self.node_stack.push(0);
        let eq_rows: Vec<Arg> = parts.iter().map(|(e, _, _)| e.clone()).collect();
        let (eq, bases) = self.eq_rows(&eq_rows, st, 1);
        // Numbers.
        // 【中文标注】逐行排编号。
        let mut nums = Vec::new();
        for (ri, (_, num, hash)) in parts.iter().enumerate() {
            if *hash == usize::MAX {
                nums.push(None);
                continue;
            }
            self.path.push((0, ri));
            let path = self.path.clone();
            // Every numbered equation counts; `#` alone shows the count.
            // 【中文标注】每个带 # 的行都推进文档级计数器；# 后为空时显示该计数。
            *counter = counter.saturating_add(1);
            let b = if num.is_empty() {
                let r = MRun { text: format!("({counter})"), sty: Some(MSty::Plain), ..Default::default() };
                let mut b = compose(
                    self.run_atoms(&r, St { upright: true, ..st }, 0, &path)
                        .into_iter()
                        .map(|p| match p {
                            RunPart::Atom(b) | RunPart::Align(b) => b,
                        })
                        .collect(),
                    st,
                    em,
                );
                b.slots.clear();
                b.slot(&path, hash + 1, 0.0);
                b
            } else {
                let atoms = self.row(num, st, 2, hash + 1);
                let mut b = compose(atoms.into_iter().map(|(b, _)| b).collect(), st, em);
                b.slot(&path, hash + 1, 0.0);
                b
            };
            let mut b = b;
            finalize(&mut b, &path, em);
            self.path.pop();
            nums.push(Some(b));
        }
        self.node_stack.pop();
        let num_w = nums.iter().flatten().map(|b| b.w).fold(0.0f32, f32::max);
        let gap = em;
        // 【中文标注】公式默认居中；居中后放不下“公式+1em 间距+编号”时改为左对齐。
        let mut x_eq = ((avail - eq.w) / 2.0).max(0.0);
        if x_eq + eq.w + gap + num_w > avail {
            x_eq = (avail - num_w - gap - eq.w).max(0.0);
        }
        let width = avail.max(x_eq + eq.w + gap + num_w);
        let mut out = Bx { w: width, ..Default::default() };
        out.put(eq, x_eq, 0.0);
        for (b, y) in nums.into_iter().zip(bases) {
            if let Some(b) = b {
                let bw = b.w;
                out.put(b, width - bw, y);
            }
        }
        out
    }

    /// A display equation, broken into lines where it is wider than `avail` or a run asks for a
    /// manual break (`m:brk`). Breaks fall only at top-level binary operators and relations (never
    /// inside a fraction, radical, script or matrix), before or after the operator as `m:brkBin`
    /// says; lines are filled greedily and a term wider than the line overflows it. The first line
    /// starts at the left; the rest are indented by `m:wrapIndent`, set flush right with
    /// `m:wrapRight`, or aligned at an operator of the first line (`m:alnAt`).
    /// 【中文标注】display 长公式折行：只在顶层二元运算/关系符处可断（绝不进入分数/
    /// 根式/上下标/矩阵内部）；断点在算子前还是后由 brk_bin 决定；Repeat 时算子在下一
    /// 行开头重复（字符按 brk_bin_sub 变换：−−/−+/+−）；只有手动断行（m:brk）或超宽才
    /// 分多行；贪婪装填，单个超宽项允许溢出行宽。首行贴左，后续行按 wrapIndent 缩进
    /// （≤半行宽）/ wrapRight 右对齐 / m:alnAt 对齐首行第 alnAt 个算子的 x。行距
    /// gap=0.2em、最小基线距 1.25em，自首行基线向下堆叠。
    fn wrapped(&mut self, a: &[MNode], st: St, avail: f32, mp: &MathProps) -> Bx {
        let em = self.em(st);
        let atoms: Vec<Bx> = self.row(a, st, 0, 0).into_iter().map(|(b, _)| b).collect();
        let gaps = gaps_between(&atoms, st, em);
        let total: f32 = atoms.iter().map(|b| b.w).sum::<f32>() + gaps.iter().sum::<f32>();
        let manual = atoms.iter().skip(1).any(|b| b.brk.is_some());
        if atoms.len() < 2 || (!manual && total <= avail + 0.01) {
            let mut b = compose(atoms, st, em);
            finalize(&mut b, &self.path, em);
            return b;
        }
        let cls = classes(&atoms);
        let n = atoms.len();
        let is_op = |i: usize| -> bool {
            atoms.get(i).is_some_and(|b| b.glyph.is_some() && !b.transparent)
                && cls.get(i).is_some_and(|c| matches!(c.0, Cls::Bin | Cls::Rel) && c.0 == c.1)
        };
        // Chunk starts (after the first): the atom index → forced (`Some(alnAt)`) and the
        // operator repeated at the start of the line it begins.
        // 【中文标注】候选断点：顶层 Bin/Rel 单字符算子（brk_bin≠Before 时断点在算子后
        // i+1，否则在算子前 i）；值 = (手动 alnAt, 需在下行重复的算子下标)。
        let mut starts: std::collections::BTreeMap<usize, (Option<u8>, Option<usize>)> = std::collections::BTreeMap::new();
        for i in 1..n {
            let op = is_op(i);
            let at = if op && mp.brk_bin != BrkBin::Before { i + 1 } else { i };
            if at == 0 || at >= n {
                continue;
            }
            let repeat = (op && mp.brk_bin == BrkBin::Repeat).then_some(i);
            let forced = atoms.get(i).and_then(|b| b.brk);
            if op || forced.is_some() {
                let e = starts.entry(at).or_insert((None, None));
                if forced.is_some() {
                    e.0 = forced;
                }
                if repeat.is_some() {
                    e.1 = repeat;
                }
            }
        }
        // Chunks: [s, e) with their width inside and the gap before them.
        // 【中文标注】按断点把原子序列切块 [s,e)；贪心装填以块为单位（块内永不断开）。
        let mut bounds: Vec<usize> = vec![0];
        bounds.extend(starts.keys().copied());
        bounds.push(n);
        let width_of = |s: usize, e: usize| -> f32 {
            (s..e).map(|i| atoms.get(i).map_or(0.0, |b| b.w) + if i > s { gaps.get(i).copied().unwrap_or(0.0) } else { 0.0 }).sum()
        };
        // The operator glyphs a repeated operator shows at the end of one line and the start of
        // the next.
        // 【中文标注】重复算子对：(上行尾字形, 下行首字形)——字符按 brk_bin_sub 变换
        // （−−/−+/+−），字形沿用原算子的字体/字号/颜色。
        let repeats: std::collections::HashMap<usize, (Option<Bx>, Bx)> = starts
            .values()
            .filter_map(|v| v.1)
            .filter_map(|i| {
                let b = atoms.get(i)?;
                let (end, start) = match op_char(b) {
                    Some('−' | '-') => match mp.brk_bin_sub {
                        BrkBinSub::MinusMinus => ('−', '−'),
                        BrkBinSub::MinusPlus => ('−', '+'),
                        BrkBinSub::PlusMinus => ('+', '−'),
                    },
                    Some(c) => (c, c),
                    None => return None,
                };
                let end = (Some(end) != op_char(b)).then(|| self.op_glyph(b, end, st)).flatten();
                Some((i, (end, self.op_glyph(b, start, st)?)))
            })
            .collect();
        let indent = if mp.wrap_indent.is_finite() { mp.wrap_indent.clamp(0.0, 1584.0) } else { 0.0 }.min(avail * 0.5);
        // Greedy fill: lines as lists of chunks (index into `bounds`).
        // 【中文标注】贪心装填：装得下并入当前行，装不下开新行；手动断点强制换行。
        // 首两行或右对齐模式用整行宽，其余行扣除缩进。
        let mut lines: Vec<(usize, usize)> = Vec::new();
        let mut cur_w = 0.0f32;
        let mut auto = false;
        for c in 0..bounds.len().saturating_sub(1) {
            let (s, e) = (bounds.get(c).copied().unwrap_or(0), bounds.get(c + 1).copied().unwrap_or(n));
            let w = width_of(s, e);
            let lead = gaps.get(s).copied().unwrap_or(0.0);
            let info = starts.get(&s).copied();
            let copy_w = info.and_then(|v| v.1).and_then(|i| repeats.get(&i)).map_or(0.0, |r| r.1.w + lead);
            let line_avail = if lines.len() <= 1 || mp.wrap_right { avail } else { avail - indent };
            match lines.last_mut() {
                Some(l) if info.is_some_and(|v| v.0.is_none()) && cur_w + lead + w <= line_avail + 0.01 => {
                    l.1 = c + 1;
                    cur_w += lead + w;
                }
                Some(_) if info.is_some_and(|v| v.0.is_none()) => {
                    auto = true;
                    lines.push((c, c + 1));
                    cur_w = copy_w + w;
                }
                _ => {
                    lines.push((c, c + 1));
                    cur_w = copy_w + w;
                }
            }
        }
        // Build each line's box.
        // 【中文标注】逐行造盒：先放下行将重复的算子（若本行尾需要“收尾字形”，在此行
        // 摆放的就是换形后的算子），再依块内顺序放入原子；记录首行算子 x 供 alnAt 对齐。
        let ops: Vec<bool> = (0..n).map(is_op).collect();
        let path = self.path.clone();
        let mut atoms: Vec<Option<Bx>> = atoms.into_iter().map(Some).collect();
        let mut boxes: Vec<(Bx, Option<u8>)> = Vec::with_capacity(lines.len());
        let mut ops_x: Vec<f32> = Vec::new();
        for (li, &(c0, c1)) in lines.iter().enumerate() {
            let (s, e) = (bounds.get(c0).copied().unwrap_or(0), bounds.get(c1).copied().unwrap_or(n));
            let info = starts.get(&s).copied();
            let next = lines.get(li + 1).and_then(|l| bounds.get(l.0)).and_then(|s| starts.get(s)).and_then(|v| v.1);
            let mut row = Bx::default();
            let mut x = 0.0f32;
            let copy = info.and_then(|v| v.1).and_then(|i| repeats.get(&i)).map(|r| r.1.clone());
            let has_copy = copy.is_some();
            if let Some(cb) = copy {
                let w = cb.w;
                row.put(cb, 0.0, 0.0);
                x = w;
            }
            for i in s..e {
                let Some(mut b) = atoms.get_mut(i).and_then(Option::take) else { continue };
                if i > s || has_copy {
                    x += gaps.get(i).copied().unwrap_or(0.0);
                }
                // 【中文标注】下一行将以重复算子 i 开头 ⇒ 本行的这个算子画成“收尾字形”
                // （如 −），其光标槽位按新旧宽度比例缩放。
                if next == Some(i)
                    && let Some((Some(end), _)) = repeats.get(&i)
                {
                    let mut r = end.clone();
                    let k = if b.w > 0.0 { r.w / b.w } else { 1.0 };
                    r.slots = std::mem::take(&mut b.slots)
                        .into_iter()
                        .map(|mut sl| {
                            sl.x *= k;
                            sl
                        })
                        .collect();
                    b = r;
                }
                // 【中文标注】记录首行各算子的 x（m:alnAt 对齐目标）。
                if li == 0 && ops.get(i).copied().unwrap_or(false) {
                    ops_x.push(x);
                }
                let w = b.w;
                row.put(b, x, 0.0);
                x += w;
            }
            row.w = x;
            finalize(&mut row, &path, em);
            boxes.push((row, info.and_then(|v| v.0)));
        }
        let widest = boxes.iter().map(|(b, _)| b.w).fold(0.0f32, f32::max);
        let right = if auto { avail.max(widest) } else { widest };
        let mut placed = Vec::with_capacity(boxes.len());
        // 【中文标注】行定位：首行贴左；后续行优先 m:alnAt（对齐首行算子的 x），否则
        // wrapRight 右对齐，否则缩进 wrapIndent。
        for (li, (b, aln)) in boxes.into_iter().enumerate() {
            let x = if li == 0 {
                0.0
            } else if let Some(ax) = aln.filter(|a| *a > 0).and_then(|a| ops_x.get(a as usize - 1)) {
                *ax
            } else if mp.wrap_right {
                (right - b.w).max(0.0)
            } else {
                indent
            };
            placed.push((b, x));
        }
        // Stack the lines down from the first one's baseline.
        // 【中文标注】行堆叠：自首行基线向下（gap=0.2em，最小基线距 1.25em）。
        let (gap, skip) = (em * 0.2, em * 1.25);
        let mut out = Bx::default();
        let mut y = 0.0f32;
        let mut prev_d: Option<f32> = None;
        for (b, x) in placed {
            if let Some(pd) = prev_d {
                y -= (pd + gap + b.a).max(skip);
            }
            prev_d = Some(b.d);
            out.w = out.w.max(x + b.w);
            out.put(b, x, y);
        }
        out.w = out.w.max(right);
        out
    }

    /// Operator `c` drawn like the operator atom `like` (its face, size and colour), without
    /// caret slots.
    /// 【中文标注】用 like 原子的字体/字号/颜色/样式画算子字符 c（折行处的重复算子用），
    /// 无光标槽位（cuts 清空）；like 的字体盖不住 c 时退回数学字体。
    fn op_glyph(&self, like: &Bx, c: char, st: St) -> Option<Bx> {
        let MItem::Glyphs { face, size, color, synth_bold, synth_italic, .. } = like.items.iter().find(|i| matches!(i, MItem::Glyphs { .. }))? else {
            return None;
        };
        let face = if face.covers(c) { *face } else { self.face };
        let mut b = self.glyphs(face, &c.to_string(), *size, st.level, *color, (*synth_bold, *synth_italic));
        b.cuts.clear();
        Some(b.cls(like.first))
    }

    /// 【中文标注】矩阵：单元格按“扁平序号”作子参数编号（跨行连续，光标路径用）；
    /// 行数 ≤1000、每行 ≤64 列；列宽 = 各列最宽单元格，列间 0.8em；列对齐按 col_jc
    /// （居中/左/右）；行距经 stack_rows（gap=0.25em，最小基线距 1.2em）；盒宽 =
    /// 列宽和 + 列间距×(列数−1)。
    fn matrix(&mut self, rows: &[Vec<Arg>], col_jc: &[ColJc], st: St, d: usize) -> Bx {
        let em = self.em(st);
        let mut cells: Vec<Vec<Bx>> = Vec::new();
        let mut flat = 0usize;
        for r in rows.iter().take(1000) {
            let mut row = Vec::new();
            for (ci, c) in r.iter().enumerate() {
                if ci < 64 {
                    row.push(self.sub(flat, c, st, d));
                }
                flat += 1;
            }
            cells.push(row);
        }
        let ncols = cells.iter().map(|r| r.len()).max().unwrap_or(0);
        let mut col_w = vec![0.0f32; ncols];
        for r in &cells {
            for (i, b) in r.iter().enumerate() {
                if let Some(c) = col_w.get_mut(i) {
                    *c = c.max(b.w);
                }
            }
        }
        let gap = em * 0.8;
        let mut placed = Vec::new();
        for r in cells {
            let mut row = Bx::default();
            let mut x = 0.0f32;
            for (i, b) in r.into_iter().enumerate() {
                let cw = col_w.get(i).copied().unwrap_or(0.0);
                let bw = b.w;
                let dx = match col_jc.get(i).copied().unwrap_or_default() {
                    ColJc::Center => (cw - bw) / 2.0,
                    ColJc::Left => 0.0,
                    ColJc::Right => cw - bw,
                };
                row.put(b, x + dx, 0.0);
                x += cw + gap;
            }
            row.w = (x - gap).max(0.0);
            placed.push((row, 0.0));
        }
        let (mut out, _) = self.stack_rows(placed, st, em * 0.25, em * 1.2);
        out.w = col_w.iter().sum::<f32>() + gap * ncols.saturating_sub(1) as f32;
        out
    }
}

/// The character an operator atom draws.
/// 【中文标注】算子原子画的那个字符（仅当其文本恰为单字符时 Some）——判断折行断点
/// 算子与生成重复字形时用。
fn op_char(b: &Bx) -> Option<char> {
    b.items.iter().find_map(|i| match i {
        MItem::Glyphs { text, .. } => {
            let mut cs = text.chars();
            let c = cs.next()?;
            cs.next().is_none().then_some(c)
        }
        _ => None,
    })
}

/// 【中文标注】run 拆分产物：普通原子，或 & 对齐标记（零宽透明原子，携带其前后光标
/// 槽位；eq_rows 按它分段对齐）。
enum RunPart {
    Atom(Bx),
    /// An `&` alignment mark (a zero-width atom holding its caret slots).
    /// 【中文标注】& 对齐标记。
    Align(Bx),
}

/// Give the caret slots of argument `path` in `b` the argument's height.
/// 【中文标注】参数收尾：把 path 参数内尚无高度的光标槽位撑到参数高度（上 ≥0.72em、
/// 下 ≥0.22em 兜底）——a = 高度 − 槽位 y、d = 深度 + 槽位 y，保证光标上下沿覆盖参数。
fn finalize(b: &mut Bx, path: &[(usize, usize)], em: f32) {
    let (a, d) = (b.a.max(em * 0.72), b.d.max(em * 0.22));
    for s in &mut b.slots {
        if s.path == path && s.a <= 0.0 && s.d <= 0.0 {
            s.a = a - s.y;
            s.d = d + s.y;
        }
    }
}

/// Spacing before each atom (TeX rules: a binary operator after nothing, an operator, a
/// relation, an opening bracket or punctuation is ordinary, as is one before a relation,
/// closing bracket or punctuation).
/// 【中文标注】每个原子前的字距：查 SPACING 表（左原子末类别 × 右原子首类别），
/// mu × em/18 换算成点；透明原子（空格等）跳过，不产生字距；Bin 的一元化降级在
/// classes 里做。
fn gaps_between(atoms: &[Bx], st: St, em: f32) -> Vec<f32> {
    let cls = classes(atoms);
    let solid: Vec<usize> = (0..atoms.len()).filter(|i| atoms.get(*i).is_some_and(|b| !b.transparent)).collect();
    let mut gaps = vec![0.0f32; atoms.len()];
    for k in 1..solid.len() {
        let (Some(&p), Some(&i)) = (solid.get(k - 1), solid.get(k)) else { continue };
        let (Some(l), Some(r)) = (cls.get(p), cls.get(i)) else { continue };
        if let Some(g) = gaps.get_mut(i) {
            *g = spacing_mu(l.1, r.0, st) * em / 18.0;
        }
    }
    gaps
}

/// Each atom's classes (first, last) in context: a binary operator where TeX makes it ordinary
/// (a unary minus) becomes ordinary.
/// 【中文标注】各原子的 (首类别, 末类别)：TeX 一元化规则——Bin 前接行首/Bin/Op/Rel/
/// Open/Punct，或后接行尾/Rel/Close/Punct 时降级为 Ord（一元负号 −x 由此不加字距）。
fn classes(atoms: &[Bx]) -> Vec<(Cls, Cls)> {
    let mut cls: Vec<(Cls, Cls)> = atoms.iter().map(|b| (b.first, b.last)).collect();
    let solid: Vec<usize> = (0..atoms.len()).filter(|i| atoms.get(*i).is_some_and(|b| !b.transparent)).collect();
    for (k, &i) in solid.iter().enumerate() {
        let prev = k.checked_sub(1).and_then(|p| solid.get(p)).and_then(|p| cls.get(*p)).map(|c| c.1);
        let next = solid.get(k + 1).and_then(|n| cls.get(*n)).map(|c| c.0);
        if let Some(c) = cls.get_mut(i) {
            if c.0 == Cls::Bin && matches!(prev, None | Some(Cls::Bin | Cls::Op | Cls::Rel | Cls::Open | Cls::Punct)) {
                c.0 = Cls::Ord;
                if c.1 == Cls::Bin {
                    c.1 = Cls::Ord;
                }
            }
            if c.1 == Cls::Bin && matches!(next, None | Some(Cls::Rel | Cls::Close | Cls::Punct)) {
                c.1 = Cls::Ord;
                if c.0 == Cls::Bin {
                    c.0 = Cls::Ord;
                }
            }
        }
    }
    cls
}

/// Atoms side by side with inter-atom spacing.
/// 【中文标注】原子横排并逐个加字距（单原子直接返回）；盒子首末类别取两端非透明
/// 原子、斜体修正取末原子——参数整体对外等价于一个原子参与外层字距。
fn compose(mut atoms: Vec<Bx>, st: St, em: f32) -> Bx {
    if atoms.len() == 1
        && let Some(b) = atoms.pop()
    {
        return b;
    }
    let gaps = gaps_between(&atoms, st, em);
    let first = atoms.iter().find(|b| !b.transparent).map(|b| b.first).unwrap_or_default();
    let last = atoms.iter().rev().find(|b| !b.transparent).map(|b| b.last).unwrap_or_default();
    let ic = atoms.last().map(|b| b.ic).unwrap_or(0.0);
    let mut out = Bx::default();
    let mut x = 0.0f32;
    for (b, g) in atoms.into_iter().zip(gaps) {
        x += g;
        let w = b.w;
        out.put(b, x, 0.0);
        x += w;
    }
    out.w = x;
    out.first = first;
    out.last = last;
    out.ic = ic;
    out
}

/// Boxes side by side, no spacing.
/// 【中文标注】纯横排不加字距（定界符组内部、线性分数等）。
fn hcat(parts: Vec<Bx>) -> Bx {
    let first = parts.first().map(|b| b.first).unwrap_or_default();
    let last = parts.last().map(|b| b.last).unwrap_or_default();
    let mut out = Bx::default();
    let mut x = 0.0f32;
    for b in parts {
        let w = b.w;
        out.put(b, x, 0.0);
        x += w;
    }
    out.w = x;
    out.first = first;
    out.last = last;
    out
}

#[cfg(test)]
mod tests {
    // 【中文标注】排版结果断言：长公式折行与手动断行、短公式不折、分数堆叠、上下标
    // 升级缩放、原子字距与一元负号、display 大算子上下限、定界符伸展、各结构健康性、
    // 光标槽位完备性（含命中测试）、自动编号、敌意输入不 panic。
    use super::*;
    use wordcraft_doc::math::Math;

    fn rc() -> ResolvedChar {
        wordcraft_doc::styles::StyleSheet::default().resolve_char(None, &Default::default())
    }

    fn lay(lin: &str, display: bool) -> MathLayout {
        layout_equation(&Math::default(), lin, &rc(), display, 400.0, &MathProps::default(), &mut 0)
    }

    /// The glyphs of a laid-out equation by line: (baseline, [(x, text)]), top line first.
    fn lines(m: &MathLayout) -> Vec<(f32, Vec<(f32, String)>)> {
        let mut out: Vec<(f32, Vec<(f32, String)>)> = Vec::new();
        for it in &m.items {
            if let MItem::Glyphs { glyphs, text, .. } = it
                && let Some(&(_, x, y)) = glyphs.first()
            {
                match out.iter_mut().find(|l| (l.0 - y).abs() < 0.5) {
                    Some(l) => l.1.push((x, text.clone())),
                    None => out.push((y, vec![(x, text.clone())])),
                }
            }
        }
        out.sort_by(|a, b| b.0.total_cmp(&a.0));
        for l in &mut out {
            l.1.sort_by(|a, b| a.0.total_cmp(&b.0));
        }
        out
    }

    fn lay_in(math: &Math, lin: &str, display: bool, avail: f32, mp: &MathProps) -> MathLayout {
        layout_equation(math, lin, &rc(), display, avail, mp, &mut 0)
    }

    #[test]
    fn long_display_equations_wrap_before_operators_and_indent() {
        let sum = "y=a+b+c+d+e+f+g+h+i+j+k+l+m+n+o+p+q+r+s+t+u+v+w";
        let mp = MathProps::default();
        let full = lay_in(&Math::default(), sum, true, 1e4, &mp);
        assert_eq!(lines(&full).len(), 1, "fits on one line when there is room");
        let avail = full.width * 0.45;
        assert!(avail > 2.0 * mp.wrap_indent, "room for the indent: {avail}");
        let m = lay_in(&Math::default(), sum, true, avail, &mp);
        let ls = lines(&m);
        assert!(ls.len() >= 2, "{ls:?}");
        assert!((m.width - avail).abs() < 0.01, "the wrapped equation fills the line: {} vs {avail}", m.width);
        for (k, (_, gs)) in ls.iter().enumerate() {
            let (x0, first) = gs.first().cloned().unwrap();
            if k == 0 {
                // Math italic 𝑦 when a font has it; plain y where none does (FreeBSD CI has no fonts).
                assert!(x0.abs() < 0.01 && (first == "𝑦" || first == "y"), "{gs:?}");
            } else {
                // Continuation lines start with the operator, one wrap indent in.
                assert!((x0 - mp.wrap_indent).abs() < 0.01 && first == "+", "line {k}: {gs:?}");
            }
            for (x, t) in gs {
                assert!(*x < avail, "line {k}: {t} at {x} past {avail}");
            }
        }
        // Breaks after the operator, and the operator repeated, as the settings say.
        let after = MathProps { brk_bin: BrkBin::After, ..Default::default() };
        let ls = lines(&lay_in(&Math::default(), sum, true, avail, &after));
        assert!(ls.len() >= 2 && ls[0].1.last().is_some_and(|g| g.1 == "+") && ls[1].1[0].1 != "+", "{ls:?}");
        let repeat = MathProps { brk_bin: BrkBin::Repeat, wrap_right: true, ..Default::default() };
        let m = lay_in(&Math::default(), sum, true, avail, &repeat);
        let ls = lines(&m);
        assert!(ls.len() >= 2 && ls[0].1.last().is_some_and(|g| g.1 == "+") && ls[1].1[0].1 == "+", "{ls:?}");
        // Inline equations never wrap.
        assert_eq!(lines(&lay_in(&Math::default(), sum, false, avail, &mp)).len(), 1);
        // A term wider than the line overflows it rather than breaking inside.
        let frac = lay_in(&Math::default(), "y=(a+b+c+d+e+f+g+h+i+j+k+l+m+n+o+p+q+r+s+t+u+v+w)/2", true, avail, &mp);
        assert!(frac.width > avail && frac.items.iter().any(|i| matches!(i, MItem::Rect { .. })), "{}", frac.width);
    }

    #[test]
    fn manual_breaks_start_new_lines() {
        let mp = MathProps::default();
        let nodes = vec![MNode::Run(MRun::new("a=b")), MNode::Run(MRun { brk: Some(0), ..MRun::new("+c") })];
        let m = lay_in(&Math { nodes, ..Default::default() }, "", true, 400.0, &mp);
        let ls = lines(&m);
        assert_eq!(ls.len(), 2, "{ls:?}");
        assert!((ls[1].1[0].0 - mp.wrap_indent).abs() < 0.01 && ls[1].1[0].1 == "+", "{ls:?}");
        assert!(m.width < 400.0, "a manually broken equation is as wide as its lines");
        // `m:alnAt`: the new line's operator sits under the first line's first operator.
        let nodes = vec![MNode::Run(MRun::new("a=b")), MNode::Run(MRun { brk: Some(1), ..MRun::new("+c") })];
        let ls = lines(&lay_in(&Math { nodes, ..Default::default() }, "", true, 400.0, &mp));
        let eq_x = ls[0].1.iter().find(|g| g.1 == "=").unwrap().0;
        assert!((ls[1].1[0].0 - eq_x).abs() < 0.01, "{ls:?}");
    }

    #[test]
    fn short_display_equations_stay_on_one_line() {
        let mp = MathProps::default();
        let one = lay_in(&Math::default(), "a+b=c", true, 400.0, &mp);
        assert_eq!(lines(&one).len(), 1);
        let plain = lay_in(&Math::default(), "a+b=c", false, 400.0, &mp);
        assert!(one.width < 400.0 && (one.width - plain.width).abs() < 0.01, "{} vs {}", one.width, plain.width);
    }

    #[test]
    fn fraction_is_stacked_with_a_rule() {
        let flat = lay("ab", true);
        let f = lay("a/b", true);
        assert!(f.ascent > flat.ascent + 2.0 && f.descent > flat.descent + 2.0, "{f:?}");
        assert!(f.items.iter().any(|i| matches!(i, MItem::Rect { .. })), "fraction rule");
        assert!(f.width < flat.width * 1.2, "stacked, not side by side");
    }

    #[test]
    fn superscript_is_raised_and_smaller() {
        let m = lay("x^2", false);
        let sizes: Vec<(f32, f32)> = m
            .items
            .iter()
            .filter_map(|i| match i {
                MItem::Glyphs { size, glyphs, .. } => glyphs.first().map(|g| (*size, g.2)),
                _ => None,
            })
            .collect();
        assert_eq!(sizes.len(), 2, "{m:?}");
        assert!(sizes[1].0 < sizes[0].0 && sizes[1].1 > 2.0, "{sizes:?}");
    }

    #[test]
    fn operators_get_spaces_and_unary_minus_does_not() {
        let tight = lay("ab", false);
        let spaced = lay("a+b", false);
        let unary = lay("−b", false);
        let plus_w = lay("+", false).width;
        assert!(spaced.width > tight.width + plus_w + 2.0, "medium spaces around +");
        assert!(unary.width < lay("b", false).width + plus_w + 1.0, "no space after a unary minus");
    }

    #[test]
    fn display_operators_are_big_with_limits_over_and_under() {
        let d = lay("∑_(i=1)^n▒i", true);
        let t = lay("∑_(i=1)^n▒i", false);
        assert!(d.ascent + d.descent > t.ascent + t.descent, "display sum taller");
        assert!(d.width < t.width + 4.0, "limits stacked in display");
    }

    #[test]
    fn delimiters_grow_around_tall_content() {
        let small = lay("(x)", true);
        let big = lay("((a)/(b))", true);
        let h = |m: &MathLayout| m.ascent + m.descent;
        assert!(h(&big) > h(&small) * 1.4, "{} vs {}", h(&big), h(&small));
    }

    #[test]
    fn every_structure_lays_out() {
        use wordcraft_doc::math::*;
        let x = || vec![MNode::Run(MRun::new("x"))];
        let nodes = vec![
            MNode::Acc { chr: '\u{302}', e: x() },
            MNode::Acc { chr: '\u{305}', e: x() },
            MNode::Bar { top: false, e: x() },
            MNode::BorderBox { hide: [false; 4], strike: [true; 4], e: x() },
            MNode::Boxed { e: x() },
            MNode::GroupChr { chr: '⏟', top: false, e: x() },
            MNode::EqArr { rows: vec![vec![MNode::Run(MRun::new("x&=1"))], vec![MNode::Run(MRun::new("y&=22"))]] },
            MNode::Matrix { rows: vec![vec![x(), x()], vec![x(), Vec::new()]], col_jc: vec![] },
            MNode::Phant { show: false, zero_wid: false, zero_asc: false, zero_desc: true, e: x() },
            MNode::Lim { upper: false, e: vec![MNode::Run(MRun::plain("lim"))], lim: x() },
            MNode::Rad { deg: x(), deg_hide: false, e: x() },
            MNode::Script { kind: ScriptKind::Pre, base: x(), sub: x(), sup: x() },
            MNode::Frac { kind: FracKind::Skewed, num: x(), den: x() },
            MNode::Frac { kind: FracKind::Linear, num: x(), den: x() },
            MNode::Frac { kind: FracKind::NoBar, num: x(), den: x() },
            MNode::Delim { beg: None, end: Some('|'), sep: Some('|'), grow: true, shp_match: true, elems: vec![x(), x()] },
            MNode::Run(MRun { text: "text".into(), nor: true, ..Default::default() }),
            MNode::Run(MRun { text: "R".into(), scr: MScr::DoubleStruck, ..Default::default() }),
        ];
        for n in nodes {
            let m = layout_equation(&Math { nodes: vec![n.clone()], ..Default::default() }, "", &rc(), true, 400.0, &MathProps::default(), &mut 0);
            assert!(m.width > 0.0 && m.ascent + m.descent > 0.0, "{n:?} → {m:?}");
            for it in &m.items {
                if let MItem::Glyphs { glyphs, .. } = it {
                    assert!(glyphs.iter().all(|g| g.1.is_finite() && g.2.is_finite()));
                }
            }
        }
    }

    #[test]
    fn caret_slots_cover_every_position() {
        use wordcraft_doc::math_edit::{MathPos, move_right, units};
        let math = Math { nodes: wordcraft_doc::math::parse_linear("x=(-b±√(b^2-4ac))/2a+■(a&b@c&d)"), ..Default::default() };
        let m = layout_equation(&math, "", &rc(), true, 400.0, &MathProps::default(), &mut 0);
        // Walking the caret through the equation, every position has a slot.
        let mut p = MathPos::default();
        let mut seen = 0;
        loop {
            let s = m.slots.iter().find(|s| s.path == p.path && s.off == p.off);
            assert!(s.is_some(), "no slot for {p:?}");
            seen += 1;
            match move_right(&math.nodes, &p) {
                Some(n) => p = n,
                None => break,
            }
            assert!(seen < 500);
        }
        assert_eq!(p.off, units(&math.nodes));
        // Hit testing finds the denominator below the bar.
        let den = m.slots.iter().find(|s| s.path == vec![(1, 1)]).unwrap();
        let hit = m.hit(den.x + 1.0, den.y).unwrap();
        assert_eq!(hit.path, vec![(1, 1)]);
    }

    #[test]
    fn empty_slots_show_placeholders_on_screen_only() {
        let math = Math { nodes: wordcraft_doc::math::parse_linear("⬚/⬚"), ..Default::default() };
        let m = layout_equation(&math, "", &rc(), true, 400.0, &MathProps::default(), &mut 0);
        assert!(m.items.iter().all(|i| matches!(i, MItem::ScreenOnly(_) | MItem::Rect { .. })), "{:?}", m.items);
        assert!(m.slots.iter().any(|s| s.path == vec![(0, 0)] && s.off == 0));
        let empty = layout_equation(&Math::default(), "", &rc(), true, 400.0, &MathProps::default(), &mut 0);
        assert!(empty.width > 10.0 && empty.items.iter().all(|i| matches!(i, MItem::ScreenOnly(_))));
    }

    #[test]
    fn numbered_equations_fill_the_line_with_the_number_at_the_right() {
        let math = Math { nodes: wordcraft_doc::math::parse_linear("E=mc^2#(1)"), ..Default::default() };
        let m = layout_equation(&math, "", &rc(), true, 400.0, &MathProps::default(), &mut 0);
        assert!((m.width - 400.0).abs() < 0.01, "{}", m.width);
        let xs: Vec<f32> =
            m.items.iter().filter_map(|i| if let MItem::Glyphs { glyphs, .. } = i { glyphs.first().map(|g| g.1) } else { None }).collect();
        let max = xs.iter().cloned().fold(0.0f32, f32::max);
        assert!(max > 350.0, "number at the right margin: {xs:?}");
        let min = xs.iter().cloned().fold(f32::MAX, f32::min);
        assert!(min > 120.0 && min < 220.0, "equation centred: {xs:?}");
        // Automatic numbers count.
        let auto = Math { nodes: wordcraft_doc::math::parse_linear("a=b#"), ..Default::default() };
        let mut n = 4;
        let _ = layout_equation(&auto, "", &rc(), true, 400.0, &MathProps::default(), &mut n);
        assert_eq!(n, 5);
        assert_eq!(auto_numbers(&auto, true), 1);
    }

    #[test]
    fn math_alphabets() {
        assert_eq!(math_alnum('x', false, true, MScr::Roman), '𝑥');
        assert_eq!(math_alnum('h', false, true, MScr::Roman), 'ℎ');
        assert_eq!(math_alnum('R', false, false, MScr::DoubleStruck), 'ℝ');
        assert_eq!(math_alnum('α', false, true, MScr::Roman), '𝛼');
        assert_eq!(math_alnum('2', false, true, MScr::Roman), '2');
        assert_eq!(math_alnum('A', true, false, MScr::Roman), '𝐀');
    }

    #[test]
    fn hostile_trees_never_panic() {
        use wordcraft_doc::math::*;
        // Deep nesting.
        let mut n = vec![MNode::Run(MRun::new("x"))];
        for _ in 0..500 {
            n = vec![MNode::Frac { kind: FracKind::Bar, num: n.clone(), den: Vec::new() }];
        }
        let _ = layout_equation(&Math { nodes: n, ..Default::default() }, "", &rc(), true, 400.0, &MathProps::default(), &mut 0);
        // Empty everything.
        let empty = vec![
            MNode::Nary { chr: '\0', lim_loc: None, grow: true, sub_hide: false, sup_hide: false, sub: vec![], sup: vec![], e: vec![] },
            MNode::Delim { beg: Some('\u{10FFFF}'), end: None, sep: None, grow: true, shp_match: false, elems: vec![] },
            MNode::Matrix { rows: vec![vec![]], col_jc: vec![] },
            MNode::EqArr { rows: vec![] },
            MNode::Run(MRun { text: String::new(), size: Some(f32::NAN), ..Default::default() }),
        ];
        let m = layout_equation(&Math { nodes: empty, ..Default::default() }, "", &rc(), true, 400.0, &MathProps::default(), &mut 0);
        assert!(m.width.is_finite() && m.ascent.is_finite());
        // Breaks everywhere, odd wrap settings, no room at all.
        let runs: Vec<MNode> = (0..2000).map(|i| MNode::Run(MRun { brk: (i % 3 == 0).then_some((i % 256) as u8), ..MRun::new("−+=") })).collect();
        for avail in [0.0, f32::NAN, 1e9] {
            for brk_bin in [BrkBin::Before, BrkBin::After, BrkBin::Repeat] {
                let mp = MathProps { brk_bin, brk_bin_sub: BrkBinSub::PlusMinus, wrap_indent: f32::INFINITY, wrap_right: avail == 0.0 };
                let m = layout_equation(&Math { nodes: runs.clone(), ..Default::default() }, "", &rc(), true, avail, &mp, &mut 0);
                assert!(m.width.is_finite() && m.descent.is_finite());
            }
        }
    }
}
