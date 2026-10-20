//! Plain-text extraction, normalization and change measurement.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use unicode_normalization::UnicodeNormalization;

const BLOCK_TAGS: &[&str] = &[
    "p", "div", "br", "li", "ul", "ol", "h1", "h2", "h3", "h4", "h5", "h6", "blockquote", "pre",
    "section", "article", "header", "footer", "figure", "figcaption", "table", "tr", "td", "th",
    "dt", "dd", "hr", "aside", "main", "nav", "details", "summary",
];

/// Convert HTML to plain text: drops tags (and script/style contents), decodes entities,
/// inserts line breaks for block elements and collapses whitespace.
pub fn html_to_text(html: &str) -> String {
    let bytes = html.as_bytes();
    let mut out = String::with_capacity(html.len() / 2);
    let mut i = 0;
    let mut text_start = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            // flush text
            if text_start < i {
                out.push_str(&html[text_start..i]);
            }
            // comment
            if html[i..].starts_with("<!--") {
                match html[i + 4..].find("-->") {
                    Some(end) => i = i + 4 + end + 3,
                    None => i = bytes.len(),
                }
                text_start = i;
                continue;
            }
            let Some(end_rel) = html[i..].find('>') else {
                // unterminated tag: treat rest as text
                text_start = i;
                break;
            };
            let tag_src = &html[i + 1..i + end_rel];
            let closing = tag_src.starts_with('/');
            let name: String = tag_src
                .trim_start_matches('/')
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
                .to_ascii_lowercase();
            i += end_rel + 1;
            if !closing && (name == "script" || name == "style" || name == "noscript" || name == "template") {
                // Skip to the close tag, matching it case-insensitively in place (copying
                // the rest of the document for every such element made this quadratic).
                let close = format!("</{name}");
                i = match find_ignore_ascii_case(&bytes[i..], close.as_bytes()) {
                    Some(pos) => {
                        let after = i + pos;
                        match html[after..].find('>') {
                            Some(e) => after + e + 1,
                            None => bytes.len(),
                        }
                    }
                    None => bytes.len(),
                };
            } else if BLOCK_TAGS.contains(&name.as_str()) {
                out.push('\n');
            } else if name == "img" {
                out.push(' ');
            }
            text_start = i;
        } else {
            i += 1;
        }
    }
    if text_start < bytes.len() {
        out.push_str(&html[text_start..]);
    }
    let decoded = html_escape::decode_html_entities(&out);
    collapse_whitespace(&decoded)
}

/// Position of `needle` (which starts with '<') in `hay`, ignoring ASCII case.
fn find_ignore_ascii_case(hay: &[u8], needle: &[u8]) -> Option<usize> {
    let mut from = 0;
    while let Some(p) = hay[from..].iter().position(|&b| b == b'<') {
        let at = from + p;
        if hay.len() - at >= needle.len() && hay[at..at + needle.len()].eq_ignore_ascii_case(needle) {
            return Some(at);
        }
        from = at + 1;
    }
    None
}

/// Collapse runs of horizontal whitespace into one space and runs of newlines into one,
/// trimming each line.
pub fn collapse_whitespace(s: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    for line in s.split('\n') {
        let l = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if !l.is_empty() {
            lines.push(l);
        }
    }
    lines.join("\n")
}

/// A short plain-text excerpt.
pub fn excerpt(html: &str, max_chars: usize) -> String {
    let t = html_to_text(html).replace('\n', " ");
    super::truncate_chars(&t, max_chars)
}

static VOLATILE: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        // English relative times / update stamps
        r"(?i)\b\d+\s*(?:sec|second|min|minute|hr|hour|day|week)s?\s+ago\b",
        r"(?i)\b(?:updated|published|last updated|posted)\s*(?:on|at)?\s*:?\s*\d{1,2}[:.]\d{2}\s*(?:am|pm)?(?:\s*[a-z]{2,4})?",
        r"(?i)\b(?:just now|moments ago|yesterday)\b",
        r"(?i)\b\d+\s*(?:min|minute)s?\s+read\b",
        // Chinese / Japanese relative times
        r"\d+\s*(?:秒|分钟|分鐘|小时|小時|天|日|周|週)前",
        r"(?:刚刚|剛剛|昨天|今天)",
        r"(?:阅读时间|閱讀時間)\s*[:：]?\s*\d+\s*(?:分钟|分鐘)",
        r"(?:更新时间|更新時間|发布时间|發佈時間|發布時間)\s*[:：]?\s*[\d\-/年月日:\s]+",
    ]
    .iter()
    .map(|p| Regex::new(p).unwrap())
    .collect()
});

/// Normalize text for comparisons: NFKC, lowercase, strip volatile time stamps,
/// collapse whitespace.
pub fn normalize_for_compare(text: &str) -> String {
    let mut t: String = text.nfkc().collect::<String>().to_lowercase();
    for re in VOLATILE.iter() {
        t = re.replace_all(&t, " ").into_owned();
    }
    collapse_whitespace(&t)
}

/// Normalized title key: NFKC, lowercase, letters and digits only.
pub fn title_key(title: &str) -> String {
    title
        .nfkc()
        .flat_map(|c| c.to_lowercase())
        .filter(|c| c.is_alphanumeric())
        .collect()
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30FF   // Hiragana + Katakana
        | 0x3400..=0x4DBF // CJK ext A
        | 0x4E00..=0x9FFF // CJK unified
        | 0xAC00..=0xD7AF // Hangul syllables
        | 0xF900..=0xFAFF // compatibility ideographs
        | 0x20000..=0x2FA1F)
}

/// Tokenize: CJK characters are individual tokens, other alphanumeric runs are words.
pub fn tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    for c in text.chars() {
        if is_cjk(c) {
            if !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
            out.push(c.to_string());
        } else if c.is_alphanumeric() {
            word.extend(c.to_lowercase());
        } else if !word.is_empty() {
            out.push(std::mem::take(&mut word));
        }
    }
    if !word.is_empty() {
        out.push(word);
    }
    out
}

fn fnv1a(parts: &[String]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for p in parts {
        for b in p.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn shingles(toks: &[String], k: usize) -> HashSet<u64> {
    let mut set = HashSet::new();
    if toks.is_empty() {
        return set;
    }
    if toks.len() < k {
        set.insert(fnv1a(toks));
        return set;
    }
    for w in toks.windows(k) {
        set.insert(fnv1a(w));
    }
    set
}

/// Measurement of the difference between two texts.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChangeStats {
    /// Jaccard similarity of 3-token shingles (1.0 = identical).
    pub similarity: f64,
    /// Shingles only in the new text (~ number of added tokens).
    pub added: usize,
    /// Shingles only in the old text.
    pub removed: usize,
    pub old_tokens: usize,
    pub new_tokens: usize,
}

pub fn change_stats(old: &str, new: &str) -> ChangeStats {
    let a_t = tokens(&normalize_for_compare(old));
    let b_t = tokens(&normalize_for_compare(new));
    let a = shingles(&a_t, 3);
    let b = shingles(&b_t, 3);
    let inter = a.intersection(&b).count();
    let union = a.len() + b.len() - inter;
    let similarity = if union == 0 { 1.0 } else { inter as f64 / union as f64 };
    ChangeStats {
        similarity,
        added: b.len() - inter,
        removed: a.len() - inter,
        old_tokens: a_t.len(),
        new_tokens: b_t.len(),
    }
}

/// Thresholds deciding whether a change is substantive.
#[derive(Debug, Clone, Copy)]
pub struct ChangeThresholds {
    /// Minimum (1 - similarity) for a change to count.
    pub min_change_ratio: f64,
    /// Minimum changed tokens (added + removed) — scaled down for short texts.
    pub min_changed_tokens: usize,
    /// Added tokens that always count as substantive (a new paragraph).
    pub major_added_tokens: usize,
}

impl Default for ChangeThresholds {
    fn default() -> Self {
        ChangeThresholds { min_change_ratio: 0.10, min_changed_tokens: 25, major_added_tokens: 80 }
    }
}

impl ChangeStats {
    pub fn is_substantive(&self, t: &ChangeThresholds) -> bool {
        if self.old_tokens == 0 {
            return self.new_tokens >= 5;
        }
        let changed = self.added + self.removed;
        let min_tokens = t
            .min_changed_tokens
            .min(((self.old_tokens as f64) * 0.3).ceil() as usize)
            .max(3);
        let ratio = 1.0 - self.similarity;
        (ratio >= t.min_change_ratio && changed >= min_tokens) || self.added >= t.major_added_tokens
    }
}

/// Character-bigram similarity of two titles (0..1).
pub fn title_similarity(a: &str, b: &str) -> f64 {
    let ka: Vec<char> = title_key(a).chars().collect();
    let kb: Vec<char> = title_key(b).chars().collect();
    if ka == kb {
        return 1.0;
    }
    let grams = |v: &[char]| -> HashSet<(char, char)> {
        if v.len() < 2 {
            return v.iter().map(|c| (*c, ' ')).collect();
        }
        v.windows(2).map(|w| (w[0], w[1])).collect()
    };
    let (ga, gb) = (grams(&ka), grams(&kb));
    let inter = ga.intersection(&gb).count();
    let union = ga.len() + gb.len() - inter;
    if union == 0 { 1.0 } else { inter as f64 / union as f64 }
}

/// Fraction of letters in `text` that are CJK ideographs/kana/hangul.
pub fn cjk_ratio(text: &str) -> f64 {
    let mut letters = 0usize;
    let mut cjk = 0usize;
    for c in text.chars() {
        if is_cjk(c) {
            cjk += 1;
            letters += 1;
        } else if c.is_alphabetic() {
            letters += 1;
        }
    }
    if letters == 0 { 0.0 } else { cjk as f64 / letters as f64 }
}

/// Count letters (alphabetic chars) in a string.
pub fn letter_count(text: &str) -> usize {
    text.chars().filter(|c| c.is_alphabetic()).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_to_text_basics() {
        let h = "<p>Hello <b>world</b></p><script>var x = '<p>';</script><p>Second&nbsp;para &amp; more</p><!-- c -->";
        assert_eq!(html_to_text(h), "Hello world\nSecond para & more");
        assert_eq!(html_to_text("a<br>b"), "a\nb");
        assert_eq!(html_to_text("<style>p{}</style>x"), "x");
        assert_eq!(html_to_text("<SCRIPT>if (a < b) {}</Script >é<noscript><p>no</noscript>ü<style>"), "éü");
    }

    #[test]
    fn html_to_text_is_linear_in_raw_text_elements() {
        // 4 MB of empty style elements: quadratic before (the rest of the document was
        // copied for each one), well under a second now.
        let h = format!("{}<p>end</p>", "<style></style>".repeat(4 << 20 >> 4));
        let t = std::time::Instant::now();
        assert_eq!(html_to_text(&h), "end");
        assert!(t.elapsed() < std::time::Duration::from_secs(10), "{:?}", t.elapsed());
    }

    #[test]
    fn tokens_mixed() {
        let t = tokens("BBC 中文 news, 2026!");
        assert_eq!(t, vec!["bbc", "中", "文", "news", "2026"]);
    }

    #[test]
    fn identical_texts_not_substantive() {
        let a = "The quick brown fox jumps over the lazy dog. ".repeat(20);
        let s = change_stats(&a, &a);
        assert_eq!(s.similarity, 1.0);
        assert!(!s.is_substantive(&ChangeThresholds::default()));
    }

    #[test]
    fn volatile_timestamps_ignored() {
        let base = "Police said the curfew would be in force between 23:00 and 06:00 in Paris. ".repeat(10);
        let a = format!("Published 2 hours ago. {base}");
        let b = format!("Published 5 hours ago. {base}");
        let s = change_stats(&a, &b);
        assert!(!s.is_substantive(&ChangeThresholds::default()), "{s:?}");
    }

    #[test]
    fn typo_fix_not_substantive() {
        let base = "French authorities have imposed overnight curfews in parts of central Paris and other cities after a weekend of riots and looting across the country. ".repeat(8);
        let a = format!("{base} The minister said the situation was under contorl.");
        let b = format!("{base} The minister said the situation was under control.");
        let s = change_stats(&a, &b);
        assert!(!s.is_substantive(&ChangeThresholds::default()), "{s:?}");
    }

    #[test]
    fn new_paragraphs_substantive() {
        let base = "French authorities have imposed overnight curfews in parts of central Paris and other cities after a weekend of riots. ";
        let a = base.repeat(3);
        let b = format!(
            "{a} More than 2,500 people have been arrested, officials said on Monday evening. One demonstrator's hand was blown off by a police crowd-control grenade, according to the AFP news agency. About 1,000 police officers were injured during the clashes, the interior ministry added in a statement released late at night."
        );
        let s = change_stats(&a, &b);
        assert!(s.is_substantive(&ChangeThresholds::default()), "{s:?}");
    }

    #[test]
    fn cjk_change_detection() {
        let a = "法國多地上週末爆發騷亂及搶掠，當局其後在巴黎市中心部分地區及其他城市實施夜間宵禁。".repeat(3);
        let b = format!("{a}巴黎警方表示，宵禁將於當地時間23:00至06:00期間執行，涉及週末曾有示威者聚集的共和廣場以及大堂區購物區。南部城市包括馬賽、波爾多、尼斯及里昂先後宣佈局部宵禁。");
        let s = change_stats(&a, &b);
        assert!(s.is_substantive(&ChangeThresholds::default()), "{s:?}");
        let c = a.replace("騷亂", "骚乱");
        let s2 = change_stats(&a, &c);
        assert!(s2.similarity < 1.0);
    }

    #[test]
    fn short_summary_rewrite_is_substantive() {
        let a = "Influential Labour figures had been pushing for equalising CGT and income tax.";
        let b = "The chancellor has ruled out raising capital gains tax, saying it would cost the government money.";
        let s = change_stats(a, b);
        assert!(s.is_substantive(&ChangeThresholds::default()), "{s:?}");
    }

    #[test]
    fn title_similarity_works() {
        assert_eq!(title_similarity("Hello, World!", "hello world"), 1.0);
        assert!(title_similarity("PM says taxes will rise", "PM vows taxes will rise") > 0.6);
        assert!(title_similarity("Earthquake hits Japan", "Football results") < 0.3);
    }

    #[test]
    fn cjk_ratio_works() {
        assert!(cjk_ratio("这是中文句子") > 0.99);
        assert!(cjk_ratio("This is English") < 0.01);
    }
}
