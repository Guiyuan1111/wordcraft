//! Hit testing and caret geometry.
//!
//! 【中文标注】命中测试与光标几何：屏幕点 (x, y) ↔ 文档位置（Pos），文档位置 ↔ 光标矩形
//! （Caret）。按用途分四组：
//!   1. 点击定位：story_at 判定点下的文字流（正文/脚注/尾注/文本框；页眉页脚另走专用
//!      接口），hit 在该流里找最近的行再折算偏移；object_at 找点中的对象（图片/形状/
//!      文本框）；cell_at 找表格单元格；turned_hit 处理竖排文字（表格单元格文字方向）。
//!   2. 光标几何：caret/caret_on 把 Pos 换算成页面上的光标矩形；vertical 做 ↑/↓ 行间
//!      移动（保持目标列 x=goal_x）；line_bounds 做行首/行尾（Home/End）。
//!   3. 选区与步进：selection_rects 生成跨行/跨段/跨页的选区高亮矩形（双向文本中一段
//!      逻辑选区可拆成多个视觉矩形）；column_segments 做列（矩形块）选择；visual_step
//!      实现 ←/→ 的“视觉一步”（双向文本按屏幕位置移动，而非按逻辑顺序）。
//!   4. 公式：equation_geom 定位行内公式对象的绘制位置（页、左缘 x、基线），
//!      equation_hit 把点换算成公式内部的光标位置（MathPos）；两者配合 math.rs 的
//!      MathLayout（slot 表）完成公式的点选与光标绘制。
//! 坐标约定：除特别说明外均为页面坐标（pt）。竖排文字的行在自身坐标系里仍是“横排”的，
//! 命中前后经 unturn_point / turn_rect 在页面坐标系与行坐标系之间换算。

use wordcraft_doc::para::Wrap;
use wordcraft_doc::props::TextDirection;
use wordcraft_doc::{Document, Path, Pos, StoryRef};
use wordcraft_geom::{Point, Rect};

use crate::para::ParaLayout;
use crate::{DocLayout, Page, Placed};

/// Caret geometry on a page: a bar from (`x`, `top`) to (`x + width`, `top + height`). It is
/// upright (`width` 0) except in turned text (a table cell's text direction), where it lies
/// across the page (`height` 0).
/// 【中文标注】页面上的光标矩形：通常是一条细竖线（width=0）；竖排文字中横躺成一条
/// 横线（height=0）。见 caret_on 的构造过程。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Caret {
    /// 【中文标注】光标所在页的下标。
    pub page: usize,
    /// 【中文标注】光标 x（页面坐标）：竖排文字中是横置光标的起点。
    pub x: f32,
    /// 【中文标注】光标顶端 y（页面坐标，向下为正）。
    pub top: f32,
    /// 【中文标注】光标高度（竖排文字中为 0，改由 width 表示横线长）。
    pub height: f32,
    /// 【中文标注】光标宽度：横排文字恒为 0（画成细竖线）；竖排文字中 >0（光标横躺，
    /// 画成一条横线，见 caret_on 里 turn_rect 的结果）。
    pub width: f32,
}

/// Result of [`DocLayout::visual_step`].
/// 【中文标注】visual_step（方向键 ←/→ 的一步）的结果：Moved=仍在同一行内移动；
/// Edge=已到达该行该方向的视觉端点，附带行信息供调用方跳到相邻行的对应端。
#[derive(Clone, Debug, PartialEq)]
pub enum VisualStep {
    /// The position one visual step away on the same line.
    /// 【中文标注】同行内的一步：目标偏移。双向文本中“视觉上的下一个字符”在逻辑序里
    /// 可能是往回走的（例如 LTR 段落中的一段 RTL 文字）。
    Moved(Pos),
    /// Already at that visual end of the line: the line's byte range, whether it is the
    /// paragraph's last line, and whether the paragraph reads right to left.
    /// 【中文标注】已在行端：start/stop 为该行的字节区间；last_line 标记是否段落末行；
    /// rtl 标记段落方向——调用方据此决定跳到下一行/上一行的哪一端（RTL 段落首尾互换）。
    Edge { start: usize, stop: usize, last_line: bool, rtl: bool },
}

/// One placed line, resolved to page coordinates.
/// 【中文标注】一行命中信息：把放置项（Placed::Lines）里的某一行连同段落布局解析成
/// 页面坐标，供命中打分、光标与选区计算使用。
#[derive(Clone, Debug)]
pub struct LineHit<'a> {
    /// 【中文标注】该行所属文字流（正文或某个页眉/页脚 part）。
    pub story: StoryRef,
    /// 【中文标注】所在段落（或表格单元格）的路径。
    pub path: &'a Path,
    /// 【中文标注】段落布局：偏移↔x 的换算（x_of / off_at_x / x_spans）都走它的方法。
    pub para: &'a ParaLayout,
    /// 【中文标注】行号（para.lines 中的下标）。
    pub li: usize,
    /// 【中文标注】该行所在放置项的页面原点 x；行内坐标需加上它才是页面 x。
    pub x: f32,
    /// 【中文标注】行顶/行底的页面 y（放置项 y + 本行相对首行顶的偏移）。
    pub top: f32,
    pub bottom: f32,
    /// 【中文标注】行墨迹的左/右页面 x（x + 行的 left / right）。
    pub left: f32,
    pub right: f32,
}

/// A picture, shape or text box as laid out (see [`Placed::Object`]).
/// 【中文标注】一个已摆放对象（图片/形状/文本框）的命中快照。
#[derive(Clone, Debug, PartialEq)]
pub struct ObjectHit {
    /// 【中文标注】对象所在页的下标。
    pub page: usize,
    /// Its unrotated frame; `spin` turns it about the centre.
    /// 【中文标注】未旋转的外框；spin 描述绕框中心的旋转角。
    pub rect: Rect,
    /// 【中文标注】旋转（角度与方向）；几何判断统一经 unspin/bounds 处理。
    pub spin: wordcraft_geom::Spin,
    /// 【中文标注】锚定该对象内联字符（U+FFFC）所属的文字流。
    pub story: StoryRef,
    /// 【中文标注】锚定段落路径与字符偏移——对象的“文档位置”即这个 U+FFFC 的 Pos。
    pub path: Path,
    pub off: usize,
    /// 【中文标注】文本框编号：非 None 表示这是承载某文字流的文本框。
    pub text_box: Option<u32>,
    /// 【中文标注】环绕方式（嵌入/四周/紧密/衬于文字下/浮于文字上）。
    pub wrap: Wrap,
    /// Where column- and paragraph-relative offsets start (see [`Placed::Object`]).
    /// 【中文标注】“相对栏/相对段落”定位的参考原点（页面坐标）。
    pub origin: Point,
}

impl ObjectHit {
    /// The object's position (its U+FFFC).
    /// 【中文标注】对象的文档位置：即锚定它的 U+FFFC 字符的 Pos。
    pub fn pos(&self) -> Pos {
        Pos { story: self.story, path: self.path.clone(), off: self.off }
    }
    /// 【中文标注】是否浮动对象（非“嵌入型”）：浮动对象参与文字环绕、可自由拖动。
    pub fn floating(&self) -> bool {
        self.wrap != Wrap::Inline
    }
    /// 【中文标注】是否“衬于文字下方”：只在没有文字的地方才能被点中。
    pub fn behind(&self) -> bool {
        self.wrap == Wrap::BehindText
    }
    /// Page point (`x`, `y`) in the object's own unrotated frame (the point itself unrotated).
    /// 【中文标注】把页面点换回对象未旋转时的自身坐标系（绕框中心逆转 spin）。
    pub fn unspin(&self, x: f32, y: f32) -> Point {
        let (cx, cy) = (self.rect.x + self.rect.w / 2.0, self.rect.y + self.rect.h / 2.0);
        let (u, v) = self.spin.unapply(cx, cy, x, y);
        Point::new(u, v)
    }
    /// The page area it covers: its frame's rotated bounds.
    /// 【中文标注】对象实际覆盖的页面区域：外框旋转后的包围盒。
    pub fn bounds(&self) -> Rect {
        self.spin.bounds(self.rect)
    }
}

/// A page's objects (not its header's or footer's), topmost first.
/// 【中文标注】页面自身的对象（不含页眉/页脚里的），上层在前。
fn objects(page: &Page, index: usize) -> impl Iterator<Item = ObjectHit> + '_ {
    objects_in(&page.items, index)
}

/// A page's objects including its header's and footer's.
/// 【中文标注】页面全部对象：正文项 + 页眉项 + 页脚项（上层在前）。
fn all_objects(page: &Page, index: usize) -> impl Iterator<Item = ObjectHit> + '_ {
    objects_in(&page.items, index).chain(objects_in(&page.header, index)).chain(objects_in(&page.footer, index))
}

/// 【中文标注】从放置项列表里筛出对象：倒序遍历（后放的画在上层），
/// 因此命中/抓取时先看到“最上层”的对象。
fn objects_in(items: &[Placed], index: usize) -> impl Iterator<Item = ObjectHit> + '_ {
    items.iter().rev().filter_map(move |it| match it {
        Placed::Object { rect, spin, story, path, off, text_box, wrap, origin } => Some(ObjectHit {
            page: index,
            rect: *rect,
            spin: *spin,
            story: *story,
            path: path.clone(),
            off: *off,
            text_box: *text_box,
            wrap: *wrap,
            origin: *origin,
        }),
        _ => None,
    })
}

/// The story of the text (not a text box's) near (x, y): body, footnotes, endnotes.
/// 【中文标注】点 (x, y) 附近的正文类文字流（正文/脚注/尾注；不含文本框内的文字——
/// 文本框内的行在这里被跳过，由 story_at 里的文本框区域判定接管）。
fn text_at(p: &Page, index: usize, x: f32, y: f32) -> Option<StoryRef> {
    // 文本框承载的 story 集合：这些正文行不算“页面文字”。
    let boxes: Vec<StoryRef> = objects(p, index).filter_map(|o| o.text_box.map(StoryRef::Part)).collect();
    p.items.iter().find_map(|it| match it {
        Placed::Lines { story, .. } if boxes.contains(story) => None,
        // 竖排文字块：外扩 2pt 后做包围盒判定。
        Placed::Lines { story, .. } if it.turned_bounds().is_some() => {
            it.turned_bounds().filter(|r| r.expand(2.0).contains(Point::new(x, y))).map(|_| *story)
        }
        // 横排文字块：垂直 ±2pt、水平左 −40pt / 右 +40pt 的宽松边界——行墨迹两侧的
        // 空白（如居中行两端）也能点到。
        Placed::Lines { story, para, l0, l1, x: lx, y: ly, .. } => {
            let first = para.lines.get(*l0)?;
            let last = para.lines.get(l1.checked_sub(1)?)?;
            let bottom = ly + last.top + last.height - first.top;
            (y >= *ly - 2.0 && y <= bottom + 2.0 && x >= *lx - 40.0 && x <= lx + last.right + 40.0).then_some(*story)
        }
        _ => None,
    })
}

/// 【中文标注】某文字流在页上的所有放置项：正文在 page.items；页眉/页脚（连同其中的
/// 文本框）与页眉页脚一起绘制，故一并遍历。
fn items_of(page: &Page, story: StoryRef) -> Box<dyn Iterator<Item = &Placed> + '_> {
    match story {
        StoryRef::Body => Box::new(page.items.iter()),
        // Headers and footers, and the text boxes in them, are drawn with the page's header and
        // footer; everything else is in its items.
        StoryRef::Part(_) => Box::new(page.header.iter().chain(page.footer.iter()).chain(page.items.iter())),
    }
}

/// Every upright line on a page that belongs to `story` (body, or a header/footer part).
/// Turned lines (see [`turned_hit`]) are left out.
/// 【中文标注】页面上属于 `story` 的所有正立（非竖排）行：把每个放置项内的各行逐一
/// 折算成页面坐标的 LineHit（top 相对首行顶累计）。竖排行不在此列。
pub fn page_lines(page: &Page, story: StoryRef) -> Vec<LineHit<'_>> {
    let mut v = Vec::new();
    for it in items_of(page, story) {
        if let Placed::Lines { story: s, path, para, l0, l1, x, y, turn } = it {
            if *s != story || turn.is_turned() {
                continue;
            }
            let Some(first) = para.lines.get(*l0) else { continue };
            // 行顶 = 放置项 y + (本行顶 − 首行顶)；左右缘 = 放置项 x + 行的 left/right。
            for li in *l0..*l1 {
                let Some(l) = para.lines.get(li) else { continue };
                let top = y + (l.top - first.top);
                v.push(LineHit { story: *s, path, para, li, x: *x, top, bottom: top + l.height, left: x + l.left, right: x + l.right });
            }
        }
    }
    v
}

/// The position in turned text (a table cell's text direction) of `story` under page point
/// (x, y), if there is turned text there.
/// 【中文标注】竖排文字的命中：把页面点换回行的“横排”坐标系后按普通行定位。
fn turned_hit(page: &Page, story: StoryRef, x: f32, y: f32) -> Option<Pos> {
    items_of(page, story).find_map(|it| {
        let Placed::Lines { story: s, path, para, l0, l1, x: ox, y: oy, turn } = it else { return None };
        // 外扩 2pt 的竖排包围盒判定；不在其中的放置项直接跳过。
        if *s != story || !it.turned_bounds()?.expand(2.0).contains(Point::new(x, y)) {
            return None;
        }
        // 逆转“竖排变换”：页面点 (x, y) → 行自身坐标系的 (u, v)。
        let (u, v) = crate::unturn_point(*turn, *ox, *oy, x, y);
        let first = para.lines.get(*l0)?;
        // The line across whose band v falls, else the nearest.
        // 【中文标注】v 落在哪一行的“行带”内；都不落在时取带距离最近的一行（band_dist）。
        let li = (*l0..*l1)
            .filter_map(|li| para.lines.get(li).map(|l| (li, l.top - first.top, l.height)))
            .min_by(|a, b| band_dist(v, a.1, a.2).total_cmp(&band_dist(v, b.1, b.2)))?
            .0;
        Some(Pos { story: *s, path: path.clone(), off: para.off_at_x(li, u) })
    })
}

/// How far `v` is outside the band `top..top + h`.
/// 【中文标注】v 到行带 [top, top+h] 的距离：带内为 0，带外取到最近边的距离。
fn band_dist(v: f32, top: f32, h: f32) -> f32 {
    if v < top {
        top - v
    } else if v > top + h {
        v - top - h
    } else {
        0.0
    }
}

impl DocLayout {
    /// The position nearest to (x, y) on `page`, in `story`.
    /// 【中文标注】点→文档位置的主入口（行级命中）。流程：
    ///   1. 先试竖排文字（turned_hit）；
    ///   2. 否则收集该 story 的全部正立行，用 score（垂直距离×4 + 水平距离，带 4pt
    ///      水平容差）打分取最近的一行；
    ///   3. 行内用 off_at_x 把 x 折算成字节偏移（双向行在其可达偏移集合里取最近）。
    /// 对象与文本框不在这里判定：调用方先用 story_at 确定文字流（文本框区域会接管），
    /// 再对本函数做行级命中。
    pub fn hit(&self, page: usize, x: f32, y: f32, story: StoryRef) -> Option<Pos> {
        let p = self.pages.get(page)?;
        if let Some(pos) = turned_hit(p, story, x, y) {
            return Some(pos);
        }
        let lines = page_lines(p, story);
        let best = lines.iter().min_by(|a, b| score(a, x, y).total_cmp(&score(b, x, y)))?;
        let off = best.para.off_at_x(best.li, x - best.x);
        Some(Pos { story: best.story, path: best.path.clone(), off })
    }

    /// The story whose text is under a point (body, footnotes, endnotes, text boxes; not
    /// headers/footers). A text box wins inside its area, except one behind the text, which only
    /// wins where no body text is.
    /// 【中文标注】点下的文字流判定，优先级：不衬底的文本框 > 正文/脚注/尾注文字 >
    /// 衬于文字下方的文本框（后者只在没有文字的地方“露出来”可点）。
    pub fn story_at(&self, page: usize, x: f32, y: f32) -> Option<StoryRef> {
        let p = self.pages.get(page)?;
        let text_box = |behind: bool| {
            objects(p, page).find(|o| o.behind() == behind && o.text_box.is_some() && o.rect.contains(Point::new(x, y))).and_then(|o| o.text_box)
        };
        text_box(false).map(StoryRef::Part).or_else(|| text_at(p, page, x, y)).or_else(|| text_box(true).map(StoryRef::Part))
    }

    /// The object a press at (x, y) grabs: a picture or shape anywhere on it, a text box on its
    /// border (within `edge` points either side; inside is its text). Topmost first; objects
    /// behind the text only where there's no text.
    /// 【中文标注】按下抓取的对象：图片/形状整个框内可抓；文本框只能抓“边框带”（框缘
    /// 两侧各 edge 内；框内部属于其文字）。自上层向下找；衬底对象只在没有文字处可抓。
    pub fn object_at(&self, page: usize, x: f32, y: f32, edge: f32) -> Option<ObjectHit> {
        let p = self.pages.get(page)?;
        // A rotated object is grabbed on its rotated shape: the point is taken into its frame.
        // 【中文标注】旋转对象按旋转后的形状抓取：先把点逆转回对象自身坐标系再判框。
        // 文本框的例外：点落在外扩 edge 内、且不在内缩 edge 内（即边框带上）才可抓；
        // 极小文本框（短边 ≤ 3×edge）则整框可抓（边框带太窄没法点）。
        let grabs = |o: &ObjectHit| {
            let pt = o.unspin(x, y);
            o.rect.expand(edge).contains(pt) && (o.text_box.is_none() || !o.rect.expand(-edge).contains(pt) || o.rect.w.min(o.rect.h) <= edge * 3.0)
        };
        // 先找不衬底的对象；找不到且该处也没有文字时，才允许抓衬底对象。
        objects(p, page)
            .find(|o| !o.behind() && grabs(o))
            .or_else(|| if text_at(p, page, x, y).is_some() { None } else { objects(p, page).find(|o| o.behind() && grabs(o)) })
    }

    /// The text box in the header or footer at (x, y) on `page`.
    /// 【中文标注】页眉/页脚区域内在 (x, y) 处的文本框（整个矩形内命中）。
    pub fn header_footer_text_box_at(&self, page: usize, x: f32, y: f32) -> Option<u32> {
        let p = self.pages.get(page)?;
        objects_in(&p.header, page)
            .chain(objects_in(&p.footer, page))
            .find(|o| o.text_box.is_some() && o.rect.contains(Point::new(x, y)))
            .and_then(|o| o.text_box)
    }

    /// Where the object at `pos` (its U+FFFC) is laid out.
    /// 【中文标注】按文档位置（U+FFFC 的 Pos）找对象的摆放快照。
    pub fn object(&self, pos: &Pos, page_hint: usize) -> Option<ObjectHit> {
        self.find_object(page_hint, |o| o.story == pos.story && o.path == pos.path && o.off == pos.off)
    }

    /// Where the text box showing story `part` is laid out.
    /// 【中文标注】按文本框编号找承载该文字流的文本框的摆放快照。
    pub fn text_box(&self, part: u32, page_hint: usize) -> Option<ObjectHit> {
        self.find_object(page_hint, |o| o.text_box == Some(part))
    }

    /// The first object matching `f`, looking on `page_hint` first (where the caret is: one page
    /// to scan, not the whole document).
    /// 【中文标注】全文档找第一个满足条件的对象：先扫提示页（通常是光标所在页，只扫一页
    /// 更快），再扫其余页；含页眉/页脚中的对象。
    pub fn find_object(&self, page_hint: usize, f: impl Fn(&ObjectHit) -> bool) -> Option<ObjectHit> {
        let on = |i: usize| self.pages.get(i).and_then(|p| all_objects(p, i).find(|o| f(o)));
        on(page_hint).or_else(|| (0..self.pages.len()).filter(|i| *i != page_hint).find_map(on))
    }

    /// Which header/footer (if any) is at (x, y) on a page — for double-click editing.
    /// 【中文标注】双击进入页眉/页脚编辑：y 高于正文区顶 → 页眉，低于正文区底 → 页脚；
    /// 返回 (文字流, 是否页眉)。x 不参与判定。
    pub fn header_footer_at(&self, page: usize, y: f32) -> Option<(StoryRef, bool)> {
        let p = self.pages.get(page)?;
        if y < p.body.y {
            return p.header_story.map(|id| (StoryRef::Part(id), true));
        }
        if y > p.body.bottom() {
            return p.footer_story.map(|id| (StoryRef::Part(id), false));
        }
        None
    }

    /// Pieces of a paragraph: (page, item index).
    /// 【中文标注】一个段落被摆放成的各片段（跨栏/跨页会拆成多个 Placed::Lines）：
    /// 正文用 index 索引直达；(页眉页脚每页重复) 优先提示页，否则取第一个显示该 part 的页。
    fn pieces(&self, story: StoryRef, path: &Path, page_hint: usize) -> Vec<(usize, &Placed)> {
        if story == StoryRef::Body {
            return self
                .index
                .get(&(story, path.clone()))
                .map(|v| v.iter().filter_map(|(pi, ii)| self.pages.get(*pi).and_then(|p| p.items.get(*ii)).map(|it| (*pi, it))).collect())
                .unwrap_or_default();
        }
        // Headers/footers repeat: prefer the hinted page, else the first that shows the part.
        // 【中文标注】页眉/页脚每页都画一遍：把提示页排到遍历顺序最前，找不到再逐页试。
        let mut order: Vec<usize> = (0..self.pages.len()).collect();
        if page_hint < order.len() {
            order.retain(|p| *p != page_hint);
            order.insert(0, page_hint);
        }
        for pi in order {
            let Some(p) = self.pages.get(pi) else { continue };
            let found: Vec<(usize, &Placed)> = items_of(p, story)
                .filter(|it| matches!(it, Placed::Lines { story: s, path: q, .. } if *s == story && q == path))
                .map(|it| (pi, it))
                .collect();
            if !found.is_empty() {
                return found;
            }
        }
        Vec::new()
    }

    /// Caret geometry for a position.
    /// 【中文标注】光标几何（提示页 0 起找）。见 caret_on。
    pub fn caret(&self, pos: &Pos) -> Option<Caret> {
        self.caret_on(pos, 0)
    }

    /// 【中文标注】光标几何的实际计算：在该位置的各段落片段里找到含它的那一行，
    /// 行内 x 由 x_of 折算；光标高度取文字的 ascent+descent（caret_metrics，而非整倍
    /// 行距），顶端不越过行顶，高度至少 4pt；最后 turn_rect 把行坐标映射到页面
    /// （竖排文字中光标由竖线变成横线）。
    pub fn caret_on(&self, pos: &Pos, page_hint: usize) -> Option<Caret> {
        for (pi, it) in self.pieces(pos.story, &pos.path, page_hint) {
            let Placed::Lines { para, l0, l1, x, y, turn, .. } = it else { continue };
            let li = para.line_of(pos.off);
            // 偏移不属于本片段的行区间时，试下一个片段（跨栏/跨页的段落分片）。
            if li < *l0 || li >= *l1 {
                continue;
            }
            let first = para.lines.get(*l0)?;
            let l = para.lines.get(li)?;
            let cx = para.x_of(li, pos.off)?;
            // In the lines' own frame (turned text turns it onto the page).
            // 【中文标注】以下都在行自身坐标系里算（竖排文字随后整体转到页面）。
            let top = l.top - first.top;
            // Caret spans the text height (ascent + descent), not the full line spacing.
            // 【中文标注】光标顶/底：基线 ± caret_metrics 给出的上/下沿，顶端不高于行顶。
            let (asc, desc) = para.caret_metrics(li);
            let base = top + (l.baseline - l.top);
            let ctop = (base - asc).max(top);
            let r = crate::turn_rect(*turn, *x, *y, Rect::new(cx, ctop, 0.0, (base + desc - ctop).max(4.0)));
            return Some(Caret { page: pi, x: r.x, top: r.y, height: r.h, width: r.w });
        }
        None
    }

    /// The position one line up (`dir < 0`) or down from `pos`, keeping `goal_x` (page x).
    /// 【中文标注】↑/↓ 移动：保持“目标列” x（goal_x；缺省取当前光标 x，返回值带回它
    /// 供下次续用）。候选行要求：行的垂直中点严格越过当前光标中点 1pt 以上，且整行
    /// 与当前光标所在行带不重叠（上移取行底 ≤ 光标竖直中点的行，下移对称）。
    /// 优先选横向范围覆盖 gx（±2pt）的行里方向上最近的一行，否则退而取最近的一行；
    /// 本页没有则跨到上/下一页，取其最底/最顶一行。
    pub fn vertical(&self, pos: &Pos, goal_x: Option<f32>, dir: i32, page_hint: usize) -> Option<(Pos, f32)> {
        let c = self.caret_on(pos, page_hint)?;
        let gx = goal_x.unwrap_or(c.x);
        let page = self.pages.get(c.page)?;
        let lines = page_lines(page, pos.story);
        let cur_mid = c.top + c.height / 2.0;
        // 方向过滤：剔除与当前行“同行高”的行（ beside 行、绕排分段行），避免原地踏步。
        let cands: Vec<&LineHit> = lines
            .iter()
            .filter(|l| {
                let mid = (l.top + l.bottom) / 2.0;
                if dir < 0 {
                    mid < cur_mid - 1.0 && l.bottom <= c.top + c.height * 0.5
                } else {
                    mid > cur_mid + 1.0 && l.top >= c.top + c.height * 0.5
                }
            })
            .collect();
        // 有列目标的行优先（gx 落在其 [left−2, right+2] 内），再按 top 取方向上最近的。
        let target = if dir < 0 {
            cands
                .iter()
                .filter(|l| gx >= l.left - 2.0 && gx <= l.right + 2.0)
                .max_by(|a, b| a.top.total_cmp(&b.top))
                .or_else(|| cands.iter().max_by(|a, b| a.top.total_cmp(&b.top)))
        } else {
            cands
                .iter()
                .filter(|l| gx >= l.left - 2.0 && gx <= l.right + 2.0)
                .min_by(|a, b| a.top.total_cmp(&b.top))
                .or_else(|| cands.iter().min_by(|a, b| a.top.total_cmp(&b.top)))
        };
        if let Some(l) = target {
            let off = l.para.off_at_x(l.li, gx - l.x);
            return Some((Pos { story: l.story, path: l.path.clone(), off }, gx));
        }
        // Next/previous page.
        // 【中文标注】跨页：上移取上一页最底行，下移取下一页最顶行；行内 x 仍按 gx 折算。
        let np = if dir < 0 { c.page.checked_sub(1)? } else { c.page + 1 };
        let p = self.pages.get(np)?;
        let lines = page_lines(p, pos.story);
        let l = if dir < 0 { lines.iter().max_by(|a, b| a.top.total_cmp(&b.top))? } else { lines.iter().min_by(|a, b| a.top.total_cmp(&b.top))? };
        let off = l.para.off_at_x(l.li, gx - l.x);
        Some((Pos { story: l.story, path: l.path.clone(), off }, gx))
    }

    /// Start and end of the visual line holding `pos` (Home / End).
    /// 【中文标注】行首/行尾（Home/End）：返回 pos 所在视觉行的 [start, end)。两类行尾
    /// 需要修正——显式换行（软换行/分页/分栏符）之后不放光标，取换行符之前簇的起点；
    /// 自动换行行尾的悬挂空格之后不放光标，取 line_offsets 的最大偏移（逻辑序；RTL
    /// 段落里这个“远端”在视觉左边）。
    pub fn line_bounds(&self, pos: &Pos, page_hint: usize) -> Option<(Pos, Pos)> {
        for (_, it) in self.pieces(pos.story, &pos.path, page_hint) {
            let Placed::Lines { para, l0, l1, .. } = it else { continue };
            let li = para.line_of(pos.off);
            if li < *l0 || li >= *l1 {
                continue;
            }
            let l = para.lines.get(li)?;
            let mut end = l.stop;
            // Don't put the caret after a trailing break on that line, nor past wrap spaces.
            // 【中文标注】行尾是显式换行符：end 收回到换行符本身（最后一个簇的 start）。
            if matches!(l.end, crate::LineEnd::LineBreak | crate::LineEnd::PageBreak | crate::LineEnd::ColumnBreak) && end > l.start {
                end = para.clusters.get(l.c1.saturating_sub(1)).map(|c| c.start).unwrap_or(end);
            } else if l.end == crate::LineEnd::Wrap {
                // Before the wrapped line's trailing space: its last reachable offset, in logical
                // order (the far edge is the left one in right-to-left text).
                // 【中文标注】自动换行行：end 收回到悬挂空格之前的最后一个可达偏移。
                end = para.line_offsets(li).into_iter().max().unwrap_or(end);
            }
            let mk = |off| Pos { story: pos.story, path: pos.path.clone(), off };
            return Some((mk(l.start), mk(end)));
        }
        None
    }

    /// Column (block) selection: the piece of every line from the line holding `a` to the line
    /// holding `b` (document order) that lies between page x `left` and `right`. One `(start,
    /// end)` pair per line, both in the same paragraph; lines shorter than `left` give an empty
    /// pair. At most `max` lines.
    /// 【中文标注】列（矩形块）选区：把页面横向区间 [left, right] 投到 a、b 之间的每一行上。
    /// a、b 先按文档序归一化；对 a..b 之间每个段落路径的每个片段的每行（a/b 所在段落内
    /// 只取含 a/b 及其后的行），用 off_at_x 把 left/right 折算成该行的 (起,止) 偏移对
    /// （left 超出该行右缘时得到空对）；最多收集 max 行防止拖选失控。
    pub fn column_segments(&self, doc: &Document, a: &Pos, b: &Pos, left: f32, right: f32, page_hint: usize, max: usize) -> Vec<(Pos, Pos)> {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        let (left, right) = if left <= right { (left, right) } else { (right, left) };
        let mut out = Vec::new();
        // 不同文字流（如正文↔文本框）之间不做列选。
        if a.story != b.story || !left.is_finite() || !right.is_finite() {
            return out;
        }
        for path in doc.paths_between(a, b) {
            for (_, it) in self.pieces(a.story, &path, page_hint) {
                let Placed::Lines { para, l0, l1, x, .. } = it else { continue };
                // a、b 所在段落只取从 a 行起到 b 行止（其余段落整段参与）。
                let first = if path == a.path { para.line_of(a.off) } else { 0 };
                let last = if path == b.path { para.line_of(b.off) } else { usize::MAX };
                for li in *l0..*l1 {
                    if li < first || li > last {
                        continue;
                    }
                    if out.len() >= max {
                        return out;
                    }
                    let o0 = para.off_at_x(li, left - x);
                    let o1 = para.off_at_x(li, right - x);
                    let (o0, o1) = if o0 <= o1 { (o0, o1) } else { (o1, o0) };
                    let mk = |off| Pos { story: a.story, path: path.clone(), off };
                    out.push((mk(o0), mk(o1)));
                }
            }
        }
        out
    }

    /// Highlight rectangles for the selection `a..b`, per page.
    /// 【中文标注】选区高亮矩形（按页分组）：a、b 按文档序归一化后，遍历 a..b 之间每个
    /// 段落路径的每个片段，逐行裁剪出选中区间并画矩形。要点：
    ///   - 行裁剪：from/to 收进本行 [start, stop]；整行在选区外的三种情形（a 越过行尾、
    ///     b 未进行首、区间倒空）都跳过；
    ///   - 双向行（vis 非空）：一段逻辑选区可能对应多个视觉段，x_spans 逐段给矩形；
    ///   - 段落标记：选区越过段尾时，在段末行尾多画一个 5pt 宽小矩形代表段落标记
    ///     （RTL 段落画在行尾左侧）；
    ///   - 纯 LTR 行：x0/x_of(from)、x1/x_of(to)；b 停在本行 stop 之前但选区还继续时，
    ///     把 x1 收到 stop（不高亮悬挂空格之后）；空矩形跳过（段落标记情形除外）。
    ///   矩形先在行自身坐标系里生成，再 turn_rect 转到页面（竖排文字）。
    pub fn selection_rects(&self, doc: &Document, a: &Pos, b: &Pos, page_hint: usize) -> Vec<(usize, Rect)> {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        let mut out = Vec::new();
        for path in doc.paths_between(a, b) {
            for (pi, it) in self.pieces(a.story, &path, page_hint) {
                let Placed::Lines { para, l0, l1, x, y, turn, .. } = it else { continue };
                let Some(first) = para.lines.get(*l0) else { continue };
                // Rectangles in the lines' own frame, turned onto the page.
                // 【中文标注】put：在行自身坐标系里造矩形并立即转到页面坐标。
                let (ox, oy, turn) = (*x, *y, *turn);
                let mut put = |r: Rect| out.push((pi, crate::turn_rect(turn, ox, oy, r)));
                for li in *l0..*l1 {
                    let Some(l) = para.lines.get(li) else { continue };
                    // 本行与选区 [a.off, b.off] 求交：from/to 裁剪进本行字节区间。
                    let from = if path == a.path { a.off.max(l.start) } else { l.start };
                    let to = if path == b.path { b.off.min(l.stop) } else { l.stop };
                    if path == a.path && a.off > l.stop {
                        continue;
                    }
                    if path == b.path && b.off < l.start {
                        continue;
                    }
                    if from > to {
                        continue;
                    }
                    let top = l.top - first.top;
                    if !l.vis.is_empty() {
                        // Bidirectional line: the selected text can be several visual pieces.
                        // 【中文标注】双向行：逻辑区间 → 若干视觉段，每段一个矩形。
                        for (x0, x1) in para.x_spans(li, from, to) {
                            put(Rect::new(x0, top, (x1 - x0).max(0.0), l.height));
                        }
                        // Selection continuing past the paragraph end shows the paragraph mark.
                        // 【中文标注】段末行且选区继续到段外：行尾补一个 5pt 矩形显示段落
                        // 标记（RTL 时画在行尾左端）。
                        if li + 1 == para.lines.len() && path != b.path {
                            let ex = l.end_x();
                            let x0 = if l.rtl { ex - 5.0 } else { ex };
                            put(Rect::new(x0, top, 5.0, l.height));
                        }
                        continue;
                    }
                    let x0 = para.x_of(li, from).unwrap_or(l.left);
                    let mut x1 = para.x_of(li, to).unwrap_or(x0);
                    // Selection continuing past the paragraph end shows the paragraph mark.
                    // 【中文标注】同上：纯 LTR 行的段落标记直接把矩形右端延长 5pt。
                    let last_line = li + 1 == para.lines.len();
                    if last_line && path != b.path {
                        x1 += 5.0;
                    }
                    // 【中文标注】b 停在本行 stop 之前但后面还有行（选区继续）：把 x1 收回
                    // 到 stop——换行行尾的悬挂空格不高亮。
                    if path == b.path && to >= l.stop && li + 1 < para.lines.len() && b.off > l.stop {
                        x1 = para.x_of(li, l.stop).unwrap_or(x1);
                    }
                    // 【中文标注】空矩形跳过；段落标记情形（上面延长过）保留。
                    if x1 <= x0 && !(last_line && path != b.path) {
                        continue;
                    }
                    put(Rect::new(x0, top, (x1 - x0).max(0.0), l.height));
                }
            }
        }
        out
    }

    /// One step left or right from `pos` in visual order on its line (arrow keys in
    /// bidirectional text). `Edge` when `pos` is already at that end of the line.
    /// 【中文标注】←/→ 的“视觉一步”（双向文本方向键的核心）：不是逻辑序 ±1，而是
    /// “屏幕位置上相邻的一个光标落点”。算法：
    ///   1. 取当前偏移在行内的 x（here）；
    ///   2. 遍历行上全部可达光标偏移（line_offsets：各簇起点 + 行尾。制表、内联对象、
    ///      换行符各占一个簇，因此各算一步；软换行之后、悬挂空格之后不可达）；
    ///   3. 取严格位于请求方向前方（0.01pt 容差）且距离最近者 → Moved；
    ///   4. 没有更靠前的落点 → Edge（已在行的该视觉端），附带行区间/末行/RTL 信息，
    ///      由调用方决定跳到相邻行的哪一端。
    pub fn visual_step(&self, pos: &Pos, left: bool, page_hint: usize) -> Option<VisualStep> {
        for (_, it) in self.pieces(pos.story, &pos.path, page_hint) {
            let Placed::Lines { para, l0, l1, .. } = it else { continue };
            let li = para.line_of(pos.off);
            if li < *l0 || li >= *l1 {
                continue;
            }
            let l = para.lines.get(li)?;
            let here = para.x_of(li, pos.off)?;
            // 在可达偏移里找视觉方向上“下一个”的落点（最近者胜出）。
            let mut best: Option<(usize, f32)> = None;
            for off in para.line_offsets(li) {
                let Some(x) = para.x_of(li, off) else { continue };
                let ahead = if left { x < here - 0.01 } else { x > here + 0.01 };
                let d = (x - here).abs();
                if ahead && best.is_none_or(|b| d < b.1) {
                    best = Some((off, d));
                }
            }
            return Some(match best {
                Some((off, _)) => VisualStep::Moved(Pos { story: pos.story, path: pos.path.clone(), off }),
                None => VisualStep::Edge { start: l.start, stop: l.stop, last_line: li + 1 == para.lines.len(), rtl: l.rtl },
            });
        }
        None
    }

    /// Where the equation at `pos` (its U+FFFC) is drawn: page, x of its left edge and its
    /// baseline (page coordinates), and its layout.
    /// 【中文标注】行内公式对象的绘制定位：在 pos 所在行的簇里找 start==pos.off 的
    /// 对象簇（即公式的 U+FFFC 锚点），从 para.maths 取它的 MathLayout，返回
    /// (页, 左缘页面 x, 基线页面 y, 布局)。竖排文字中的公式不就地编辑，直接跳过。
    pub fn equation_geom(&self, pos: &Pos, page_hint: usize) -> Option<(usize, f32, f32, std::sync::Arc<crate::math::MathLayout>)> {
        for (pi, it) in self.pieces(pos.story, &pos.path, page_hint) {
            // Equations in turned text aren't edited in place.
            let Placed::Lines { para, l0, l1, x, y, turn: TextDirection::Horizontal, .. } = it else { continue };
            let li = para.line_of(pos.off);
            if li < *l0 || li >= *l1 {
                continue;
            }
            let first = para.lines.get(*l0)?;
            let l = para.lines.get(li)?;
            // 扫本行簇：对象簇且 start 恰为 pos.off 才是目标公式。
            for k in l.c0..l.c1 {
                let Some(c) = para.clusters.get(k) else { continue };
                let crate::para::ClKind::Object(oi) = c.kind else { continue };
                if c.start != pos.off {
                    continue;
                }
                // maths: (对象下标, MathLayout) 表；cl_left 取簇左缘；基线折算到页面。
                let ml = para.maths.iter().find(|(o, _)| *o == oi)?.1.clone();
                let cx = l.cl_left(k)?;
                let base = y + (l.baseline - first.top);
                return Some((pi, x + cx, base, ml));
            }
        }
        None
    }

    /// The equation under (x, y) on `page` and the caret position inside it nearest the point.
    /// 【中文标注】点选公式：遍历页上该文字流的所有行，对每个带 MathLayout 的公式对象
    /// 做“外扩 2pt 包围盒”粗测（页码/命中在盒内才继续），再用 ml.hit（slot_dist 最近
    /// 槽位）把点换算成公式内部光标位置。返回 (公式 U+FFFC 的 Pos, 公式内 MathPos)。
    pub fn equation_hit(&self, page: usize, x: f32, y: f32, story: StoryRef) -> Option<(Pos, wordcraft_doc::math_edit::MathPos)> {
        let p = self.pages.get(page)?;
        for l in page_lines(p, story) {
            let Some(line) = l.para.lines.get(l.li) else { continue };
            let base = l.top + (line.baseline - line.top);
            for k in line.c0..line.c1 {
                let Some(c) = l.para.clusters.get(k) else { continue };
                let crate::para::ClKind::Object(oi) = c.kind else { continue };
                let Some((_, ml)) = l.para.maths.iter().find(|(o, _)| *o == oi) else { continue };
                // 公式包围盒：左缘 cl_left、宽高取 MathLayout，四周外扩 2pt 容差。
                let x0 = l.x + line.cl_left(k).unwrap_or(0.0);
                let pad = 2.0;
                if x < x0 - pad || x > x0 + ml.width + pad || y < base - ml.ascent - pad || y > base + ml.descent + pad {
                    continue;
                }
                // 命中：换算到公式自身坐标系（x 相对左缘，y 自基线向上）取最近槽位。
                let slot = ml.hit(x - x0, base - y)?;
                let pos = Pos { story: l.story, path: l.path.clone(), off: c.start };
                return Some((pos, wordcraft_doc::math_edit::MathPos { path: slot.path.clone(), off: slot.off }));
            }
        }
        None
    }

    /// Page index for a body position (first page showing its line).
    /// 【中文标注】某文档位置所在页（其行的首页）。
    pub fn page_of(&self, pos: &Pos) -> Option<usize> {
        self.caret(pos).map(|c| c.page)
    }

    /// Table cell under a point: (story, table path, row, cell).
    /// 【中文标注】点下的表格单元格：倒序找（后画的在上层），返回 (文字流, 表格路径,
    /// 行号, 列号)。
    pub fn cell_at(&self, page: usize, x: f32, y: f32) -> Option<(StoryRef, Path, usize, usize)> {
        let p = self.pages.get(page)?;
        p.items.iter().rev().find_map(|it| match it {
            Placed::Cell { rect, table, row, cell, story } if rect.contains(wordcraft_geom::Point::new(x, y)) => {
                Some((*story, table.clone(), *row, *cell))
            }
            _ => None,
        })
    }
}

/// 【中文标注】命中行的代价函数：垂直距离 ×4 + 水平距离；水平方向有 4pt 容差带
/// （点在行墨迹左右 4pt 内视作不偏）。垂直权重更大意味着“先选对行，再定行内 x”
/// ——x 的精确定位交给 off_at_x。
fn score(l: &LineHit, x: f32, y: f32) -> f32 {
    let dy = if y < l.top {
        l.top - y
    } else if y > l.bottom {
        y - l.bottom
    } else {
        0.0
    };
    let dx = if x < l.left - 4.0 {
        l.left - 4.0 - x
    } else if x > l.right + 4.0 {
        x - l.right - 4.0
    } else {
        0.0
    };
    dy * 4.0 + dx
}
