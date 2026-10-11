//! Asian typography line-breaking rules (the paragraph's `kinsoku` and `word_wrap` flags), applied
//! on top of the Unicode line-break opportunities (UAX #14).
//!
//! - Kinsoku on (the default): no line starts with closing East Asian punctuation (、。，）」…)
//!   and none ends with opening punctuation (（「【…). UAX #14 already forbids most of these
//!   breaks; the lists make sure of it for the standard Japanese/Chinese sets.
//! - Kinsoku off: a line may break between any two East Asian characters, punctuation included.
//! - Word wrap off: Latin words may break at any character.
//!
//! Only East Asian characters are in the lists, and the extra breaks need letters on both sides,
//! so text with no East Asian characters breaks exactly as before unless word wrap is off.
//!
//! 【中文标注·业务流程】东亚排版禁则模块：在 UAX #14（`unicode_linebreak`）给出的基础断点
//! 之上，按段落的 `kinsoku` 与 `word_wrap` 两个开关做三类修正——
//!   1. kinsoku 开启（默认）：撤销会令行首撞上收尾标点（、。，）」…）或行尾挂着起始标点
//!      （（「【…）的断点；UAX #14 已禁止其中大部分，这两张字符表保证标准日/中文集合万无一失。
//!   2. kinsoku 关闭：在任意两个相邻东亚字符之间（含标点）追加断点，允许在标点处断行。
//!   3. word_wrap 关闭：在拉丁词内部的任意字素簇之间追加断点（重音等组合记号跟随基字符）。
//! `apply` 输出按字节偏移排序去重后的断点表，供段落逐行排版取用；纯拉丁文本在前两类
//! 修正下断点保持不变。

use unicode_segmentation::UnicodeSegmentation;

/// East Asian characters a line may not start with: closing brackets and quotes, the
/// ideographic comma and full stop, fullwidth and halfwidth punctuation, the middle dot,
/// iteration and sound marks.
/// 【中文标注】行首禁则字符表（NO_START）：断点若落在这些字符之前（下一行以它们开头），
/// kinsoku 开启时该断点被撤销。成员包括：顿号「、」句号「。」逗号「，」全角句点、
/// 中点「・」、冒号分号、问号叹号、浊音/半浊音符「゛゜」、叠字/反复记号「ヽヾゝゞ々〻」、
/// 各类后括号后引号（〉》」』】〕〗〙〛〞〟）］｝｠）、半角片假名标点（｡｣､･ﾞﾟ）、
/// 百分号「％」与分币符等收尾类标点。
const NO_START: &str = "、。，．・：；？！゛゜ヽヾゝゞ々〻〉》」』】〕〗〙〛〞〟）］｝｠｡｣､･ﾞﾟ％￠";

/// East Asian characters a line may not end with: opening brackets and quotes, currency signs.
/// 【中文标注】行尾禁则字符表（NO_END）：断点若紧跟这些字符之后（本行以它们结尾），
/// kinsoku 开启时该断点被撤销。成员为各类前括号前引号（〈《「『【〔〖〘〚〝（［｛｟｢）
/// 与货币符号（＄￡￥）——起始类标点必须和它后面的内容待在同一行。
const NO_END: &str = "〈《「『【〔〖〘〚〝（［｛｟｢＄￡￥";

/// Whether `c` is East Asian text: CJK ideographs, kana, bopomofo, CJK punctuation, and
/// fullwidth/halfwidth forms (not spaces, not combining sound marks).
/// 【中文标注】东亚字符判定：按 Unicode 区段匹配——CJK 部首与表意描述符、CJK 符号标点、
/// 平假名、声调记号与片假名、注音字母、Kanbun/笔画/片假名扩展、CJK 扩展 A、统一表意
/// 文字主区、兼容表意文字、CJK 兼容形式、全角/半角形式，以及增补表意文字区段。
/// 不含表意空格与组合声调记号；kinsoku 关闭时用它判断"相邻两字符均为东亚字符"以追加断点。
pub fn is_east_asian(c: char) -> bool {
    matches!(c as u32,
        0x2E80..=0x2FFF          // radicals, ideographic description
        | 0x3001..=0x303F        // CJK symbols and punctuation (not the ideographic space)
        | 0x3040..=0x3098        // hiragana
        | 0x309B..=0x30FF        // sound marks, katakana
        | 0x3100..=0x312F        // bopomofo
        | 0x3190..=0x31FF        // kanbun, CJK strokes, katakana extensions
        | 0x3400..=0x4DBF        // CJK extension A
        | 0x4E00..=0x9FFF        // CJK unified ideographs
        | 0xF900..=0xFAFF        // compatibility ideographs
        | 0xFE30..=0xFE4F        // CJK compatibility forms
        | 0xFF01..=0xFFEF        // fullwidth and halfwidth forms
        | 0x20000..=0x3FFFF) // supplementary ideographs
}

/// Adjust the break opportunities `opps` (byte offsets into `text`: a line may start there) for
/// the paragraph's kinsoku and word-wrap settings. The result is sorted and has no duplicates.
/// 【中文标注】在 UAX #14 断点（`opps`：text 内的字节偏移，表示一行可从该处开始）之上
/// 应用禁则与逐字断行开关，结果排序去重。分两步：先按 kinsoku 撤销（开）或追加（关）
/// 东亚断点，再按 word_wrap 决定是否追加拉丁词内断点。
pub fn apply(text: &str, opps: &mut Vec<usize>, kinsoku: bool, word_wrap: bool) {
    // 两个小工具：取字节偏移 i 处的下一个/前一个字符（UTF-8 安全，越界得 None）。
    let next_char = |i: usize| text.get(i..).and_then(|t| t.chars().next());
    let prev_char = |i: usize| text.get(..i).and_then(|t| t.chars().next_back());
    // kinsoku 开启：只做"撤销"——断点后是行首禁则字符、或断点前是行尾禁则字符的，
    // 一律从 opps 中移除（retain 即过滤），UAX #14 的其余断点原样保留。
    if kinsoku {
        opps.retain(|&i| !next_char(i).is_some_and(|c| NO_START.contains(c)) && !prev_char(i).is_some_and(|c| NO_END.contains(c)));
    } else {
        // Between any two East Asian characters.
        // 【中文标注】kinsoku 关闭：追加断点——任意两个相邻东亚字符（含标点）之间都允许
        // 断行；逐字符扫描，前一个字符也是东亚字符时把该字节偏移加入 opps。
        for (i, c) in text.char_indices().skip(1) {
            if is_east_asian(c) && prev_char(i).is_some_and(is_east_asian) {
                opps.push(i);
            }
        }
    }
    // word_wrap 关闭：拉丁词内可任意断——在相邻两个字素簇（首字符为非东亚字母/数字）
    // 之间追加断点；按字素簇而非码点切分，重音符号等组合记号不会脱离其基字母。
    if !word_wrap {
        // Between any two letters or digits that are not East Asian (whole graphemes, so accents
        // stay with their letters).
        // 【中文标注】latin：判断一个字素簇是否"拉丁"（首字符为字母或数字且非东亚字符）。
        let latin = |g: &str| g.chars().next().is_some_and(|c| c.is_alphanumeric() && !is_east_asian(c));
        let mut prev: Option<&str> = None;
        for (i, g) in text.grapheme_indices(true) {
            if prev.is_some_and(latin) && latin(g) {
                opps.push(i);
            }
            prev = Some(g);
        }
    }
    // 断点表最终按字节偏移升序排序并去重，保证调用方可顺序取行、断点不重复。
    opps.sort_unstable();
    opps.dedup();
}

// 【中文标注】单元测试：验证 kinsoku 开/关与 word_wrap 开/关下断点的变化——开启时行首
// 不出现收尾标点（、」。漢）；关闭后任何东亚字符（含标点）都可起行；拉丁文本不受
// kinsoku 影响，word_wrap 关闭时逐字符（含 é 等重音字素）可断。
#[cfg(test)]
mod tests {
    use super::apply;

    /// Where a line may start, as the text from there to the next opportunity.
    fn breaks(text: &str, kinsoku: bool, word_wrap: bool) -> Vec<String> {
        let mut opps: Vec<usize> = unicode_linebreak::linebreaks(text).map(|(i, _)| i).filter(|&i| i < text.len()).collect();
        apply(text, &mut opps, kinsoku, word_wrap);
        opps.iter().map(|&i| text.get(i..).and_then(|t| t.chars().next()).map(String::from).unwrap_or_default()).collect()
    }

    #[test]
    fn kinsoku_and_word_wrap_break_opportunities() {
        let ja = "日本語、「漢字」。テスト";
        let on = breaks(ja, true, true);
        for c in ["、", "」", "。", "漢"] {
            assert!(!on.iter().any(|s| s == c), "kinsoku: no line starts with {c}: {on:?}");
        }
        assert!(on.iter().any(|s| s == "「"), "a line may start at an opening bracket: {on:?}");
        assert!(on.iter().any(|s| s == "テ"), "{on:?}");
        let off = breaks(ja, false, true);
        for c in ["、", "」", "。", "漢"] {
            assert!(off.iter().any(|s| s == c), "kinsoku off: a line may start with {c}: {off:?}");
        }
        // Latin text: unchanged by kinsoku, broken anywhere without word wrap.
        let en = "Hello wonderful (world), 50%.";
        assert_eq!(breaks(en, true, true), breaks(en, false, true));
        assert_eq!(breaks(en, true, true), ["w", "(", "5"]);
        let any = breaks("Hello café", true, false);
        assert_eq!(any, ["e", "l", "l", "o", "c", "a", "f", "é"]);
    }
}
