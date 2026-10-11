//! Field results computed at layout time (page numbers) and note numbering.
//!
//! 【中文标注·业务流程】布局期（分页后）才能求值的字段（域）上下文。页码类域
//! （PAGE/NUMPAGES/SECTIONPAGES/SECTION）的值取决于文本最终落在第几页，须随分页推进刷新：
//! - 填充时机一：文档总入口 `layout()`（lib.rs）构造 `Ctx` 时初始化 `FieldCtx`——页码/
//!   总页数/节页数/节号先置 1；脚注编号表 `notes` 由 `note_numbers()` 按文档出现顺序
//!   预先编号；title/author/filename 取自文档核心属性（core）。
//! - 填充时机二：分页完成后的 `headers_footers()`（lib.rs）逐页排版页眉页脚时刷新全部
//!   页面字段（page/page_format/pages/section_pages/section）。
//! - 填充时机三：给含浮动对象的段落排版前（lib.rs 的 `anchor_floats`）把 `page` 同步为
//!   当前页号；含页面域的段落还以 `page_key()` 作为缓存键的一部分，同段落落在不同页时
//!   缓存各存一份。
//! - `field_name` 解析指令首词识别域类型；`field_text` 按域类型产出显示文本，并返回
//!   "是否依赖所在页"（决定该段是否需要按页重排）；`note_number` 供 NoteRef 内联对象
//!   排版时查脚注/尾注编号。

use std::collections::HashMap;
use std::sync::Arc;

use wordcraft_doc::section::NumFormat;

/// Context for fields that depend on where text lands.
/// 【中文标注】依赖"文本落在哪"的字段求值上下文：随分页推进而更新（填充时机见文件头）。
#[derive(Clone, Debug, Default)]
pub struct FieldCtx {
    /// Current page number (as numbered) and its format.
    /// 【中文标注】当前页码（按该节编号设置计）与页码格式；headers_footers 逐页刷新，
    /// anchor_floats 在段落排版前也会同步 page。
    pub page: u32,
    /// 【中文标注】当前节页码格式（十进制/罗马/字母等）：PAGE 域未显式给 \* 格式时使用。
    pub page_format: NumFormat,
    /// Total pages in the document.
    /// 【中文标注】全文档总页数（NUMPAGES 域）。
    pub pages: u32,
    /// Pages in the current section.
    /// 【中文标注】当前节内页数（SECTIONPAGES 域）与节号（SECTION 域，从 1 起）。
    pub section_pages: u32,
    pub section: u32,
    /// Note part id → number, in document order.
    /// 【中文标注】脚注/尾注 part id → 编号，按文档出现顺序；layout() 开场由
    /// note_numbers() 一次性算好（Arc 共享），NoteRef 排版时经 note_number() 查号。
    pub notes: Arc<HashMap<u32, u32>>,
    /// Document title / author for TITLE / AUTHOR fields.
    /// 【中文标注】文档核心属性：标题（TITLE 域）、作者（AUTHOR 域）、文件名
    /// （FILENAME 域），三者均只在域无缓存结果时填入。
    pub title: Arc<str>,
    pub author: Arc<str>,
    pub filename: Arc<str>,
}

impl FieldCtx {
    /// 【中文标注】查脚注/尾注编号；不在表中（如引用被隐藏）时按 1 兜底，保证排版
    /// 不因缺号而失败。
    pub fn note_number(&self, id: u32) -> u32 {
        self.notes.get(&id).copied().unwrap_or(1)
    }
    /// A key for caches: the parts of the context a page-dependent paragraph depends on.
    /// 【中文标注】页面相关段落的缓存键：只取影响排版结果的页码四元组（当前页/总页数/
    /// 节页数/节号）；分页推进后若该键变化，段落重排而非命中旧缓存。
    pub fn page_key(&self) -> (u32, u32, u32, u32) {
        (self.page, self.pages, self.section_pages, self.section)
    }
}

/// The field's name (first word of the instruction, upper-cased).
/// 【中文标注】取域指令的第一个单词作为域名并大写（如 " PAGE \* ROMAN " → "PAGE"、
/// "=SUM(ABOVE)" → "SUM(ABOVE)"）；首部可能出现的 "="（公式域前缀）被剥掉。
pub fn field_name(instr: &str) -> String {
    instr.split_whitespace().next().unwrap_or("").trim_start_matches('=').to_ascii_uppercase()
}

/// `\* FORMAT` switch value (Arabic, roman, ROMAN, alphabetic, ALPHABETIC, CardText, Ordinal…).
/// 【中文标注】解析域指令中的 `\* 格式` 开关，映射为 NumFormat 数字格式；大小写即语义
/// （roman=小写罗马，ROMAN/Roman=大写罗马；alphabetic=小写字母，ALPHABETIC/Alphabetic=
/// 大写字母；CardText/OrdText/Ordinal 为英文文字序数）。遇到 `\*` 后取下一个词匹配，
/// 未匹配的开关值跳过继续向后找。
fn format_switch(instr: &str) -> Option<NumFormat> {
    let mut it = instr.split_whitespace();
    while let Some(w) = it.next() {
        if w == "\\*" {
            let v = it.next()?;
            return Some(match v {
                "roman" => NumFormat::LowerRoman,
                "ROMAN" | "Roman" => NumFormat::UpperRoman,
                "alphabetic" => NumFormat::LowerLetter,
                "ALPHABETIC" | "Alphabetic" => NumFormat::UpperLetter,
                "CardText" | "cardtext" => NumFormat::CardinalText,
                "OrdText" | "ordtext" => NumFormat::OrdinalText,
                "Ordinal" | "ordinal" => NumFormat::Ordinal,
                "Arabic" | "arabic" => NumFormat::Decimal,
                _ => continue,
            });
        }
    }
    None
}

/// Display text for a field and whether it depends on the page it lands on.
/// 【中文标注】按域类型产出显示文本，返回 (文本, 是否依赖所在页)：
/// - PAGE：当前页码按格式渲染，无 \* 开关时用本节 page_format；依赖页。
/// - NUMPAGES / SECTIONPAGES / SECTION：文档总页数 / 节内页数 / 节号，缺省十进制；依赖页。
/// - TITLE / AUTHOR / FILENAME：取文档属性，仅当上次结果 result 为空（无缓存值）时
///   填充；不依赖页。
/// - 其余域（DATE、公式等）：原样返回上次结果 result；不依赖页。
pub fn field_text(instr: &str, result: &str, ctx: &FieldCtx) -> (String, bool) {
    // 域名识别 + \* 数字格式开关（多数域支持）。
    let name = field_name(instr);
    let fmt = format_switch(instr);
    match name.as_str() {
        "PAGE" => (fmt.unwrap_or(ctx.page_format).format(ctx.page), true),
        "NUMPAGES" => (fmt.unwrap_or(NumFormat::Decimal).format(ctx.pages), true),
        "SECTIONPAGES" => (fmt.unwrap_or(NumFormat::Decimal).format(ctx.section_pages), true),
        "SECTION" => (fmt.unwrap_or(NumFormat::Decimal).format(ctx.section), true),
        "TITLE" if result.is_empty() => (ctx.title.to_string(), false),
        "AUTHOR" if result.is_empty() => (ctx.author.to_string(), false),
        "FILENAME" if result.is_empty() => (ctx.filename.to_string(), false),
        _ => (result.to_string(), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 【中文标注】测试：PAGE 按当前页码与格式渲染、\* 开关各格式（ROMAN/CardText）生效、
    // 非页面域（DATE）原样返回缓存结果、公式域的 "=" 前缀被剥掉。
    #[test]
    fn page_fields() {
        let ctx = FieldCtx { page: 4, pages: 12, ..Default::default() };
        assert_eq!(field_text("PAGE", "", &ctx), ("4".into(), true));
        assert_eq!(field_text(" PAGE \\* ROMAN ", "", &ctx).0, "IV");
        assert_eq!(field_text("NUMPAGES \\* CardText", "", &ctx).0, "Twelve");
        assert_eq!(field_text("DATE \\@ \"M/d/yyyy\"", "1/2/2026", &ctx), ("1/2/2026".into(), false));
        assert_eq!(field_name("=SUM(ABOVE)"), "SUM(ABOVE)");
    }
}
