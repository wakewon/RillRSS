//! Quality checks for translated segments.

use std::collections::{HashMap, HashSet};

use super::lang;
use super::segment::{Segment, check_placeholders, strip_placeholders};
use crate::util::text::{cjk_ratio, letter_count, tokens};

#[derive(Debug, Clone, PartialEq)]
pub enum Problem {
    Missing,
    Empty,
    /// Placeholder tags lost, duplicated or mis-nested.
    Placeholders,
    /// Output (or a long part of it) is still in the source language.
    Untranslated,
    /// Output repeats itself (generation loop).
    Repetition,
    /// Output far shorter than expected (truncated / omitted content).
    TooShort,
    /// Output far longer than expected (added content).
    TooLong,
    /// Output echoes the source text in addition to the translation.
    ContainsSource,
    /// Same output as a neighbouring segment with different source (misalignment).
    Misaligned,
    /// The provider refused the segment (too long / moderation).
    Rejected,
}

impl Problem {
    pub fn as_str(&self) -> &'static str {
        match self {
            Problem::Missing => "missing",
            Problem::Empty => "empty",
            Problem::Placeholders => "placeholders",
            Problem::Untranslated => "untranslated",
            Problem::Repetition => "repetition",
            Problem::TooShort => "too_short",
            Problem::TooLong => "too_long",
            Problem::ContainsSource => "contains_source",
            Problem::Misaligned => "misaligned",
            Problem::Rejected => "rejected",
        }
    }
}

fn nonspace_chars(s: &str) -> usize {
    s.chars().filter(|c| !c.is_whitespace()).count()
}

/// Expected output/input character ratio for a language pair (rough).
fn expected_ratio(src_plain: &str, target: &str) -> f64 {
    let src_cjk = cjk_ratio(src_plain) > 0.5;
    let tgt_cjk = lang::is_cjk(target);
    match (src_cjk, tgt_cjk) {
        (false, true) => 0.33,
        (true, false) => 2.8,
        _ => 1.0,
    }
}

/// Fraction of 4-token shingles that are duplicates (0 = no repetition).
fn repetition_score(text: &str) -> (f64, usize) {
    let toks = tokens(text);
    if toks.len() < 24 {
        return (0.0, toks.len());
    }
    let mut set = HashSet::new();
    let mut total = 0usize;
    for w in toks.windows(4) {
        set.insert(w.join("\u{1}"));
        total += 1;
    }
    (1.0 - set.len() as f64 / total as f64, toks.len())
}

/// Longest run of consecutive Latin-script words of the output that also appears in
/// the source (detects untranslated sentences inside CJK output).
fn longest_copied_latin_run(src: &str, out: &str) -> usize {
    let src_l = src.to_lowercase();
    let mut best = 0;
    let mut run: Vec<String> = Vec::new();
    let flush = |run: &mut Vec<String>, best: &mut usize| {
        if run.len() > *best {
            // shrink until the run occurs in the source
            let mut n = run.len();
            while n > *best {
                let found = run.windows(n).any(|w| src_l.contains(&w.join(" ")));
                if found {
                    *best = n;
                    break;
                }
                n -= 1;
            }
        }
        run.clear();
    };
    for word in out.split(|c: char| c.is_whitespace() || (c.is_ascii_punctuation() && c != '\'' && c != '-')) {
        let w = word.trim();
        if w.is_empty() {
            continue;
        }
        if w.chars().all(|c| c.is_ascii_alphabetic() || c == '\'' || c == '-') && w.chars().any(|c| c.is_ascii_alphabetic()) {
            run.push(w.to_lowercase());
        } else {
            flush(&mut run, &mut best);
        }
    }
    flush(&mut run, &mut best);
    best
}

/// Validate one translated segment against its source.
pub fn check_segment(seg: &Segment, out: Option<&str>, target: &str) -> Option<Problem> {
    let Some(out) = out else { return Some(Problem::Missing) };
    let src_plain = seg.plain();
    let out_plain = strip_placeholders(out);
    let src_letters = letter_count(&src_plain);
    if out_plain.trim().is_empty() {
        return if src_letters > 0 { Some(Problem::Empty) } else { None };
    }
    if !check_placeholders(seg, out).is_ok() {
        return Some(Problem::Placeholders);
    }

    let same = normalize(&src_plain) == normalize(&out_plain);
    let already_target = lang::already_in(&src_plain, target);

    // Echo of the source in addition to the translation.
    if !already_target && nonspace_chars(&src_plain) >= 30 && !same && normalize(&out_plain).contains(&normalize(&src_plain)) {
        return Some(Problem::ContainsSource);
    }

    // Untranslated?
    if src_letters >= 12 && !already_target {
        if same {
            return Some(Problem::Untranslated);
        }
        let tgt_cjk = lang::is_cjk(target);
        let src_cjk = cjk_ratio(&src_plain) > 0.5;
        let out_cjk = cjk_ratio(&out_plain);
        if tgt_cjk && !src_cjk {
            if letter_count(&out_plain) >= 20 && out_cjk < 0.3 {
                return Some(Problem::Untranslated);
            }
            // a sentence left in the source language inside the translation
            if longest_copied_latin_run(&src_plain, &out_plain) >= 7 {
                return Some(Problem::Untranslated);
            }
        } else if !tgt_cjk && src_cjk && out_cjk > 0.5 {
            return Some(Problem::Untranslated);
        } else if !tgt_cjk && !src_cjk && out_plain.chars().count() >= 60
            && let (Some(sl), Some(ol)) = (lang::detect(&src_plain), lang::detect(&out_plain))
                && sl == ol
                && Some(ol) != lang::whatlang_of(target)
            {
                return Some(Problem::Untranslated);
            }
    }

    // Generation loops.
    let (rep_out, n_out) = repetition_score(&out_plain);
    let (rep_src, _) = repetition_score(&src_plain);
    if n_out >= 24 && rep_out > 0.35 && rep_out > rep_src + 0.25 {
        return Some(Problem::Repetition);
    }

    // Length sanity.
    let src_n = nonspace_chars(&src_plain) as f64;
    let out_n = nonspace_chars(&out_plain) as f64;
    if src_n >= 40.0 && !already_target {
        let ratio = out_n / src_n;
        let exp = expected_ratio(&src_plain, target);
        if ratio < exp * 0.3 {
            return Some(Problem::TooShort);
        }
        if ratio > exp * 3.5 && out_n > 80.0 {
            return Some(Problem::TooLong);
        }
    }
    None
}

fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// Validate a batch of outputs. Returns problems per segment id.
pub fn check_batch(segs: &[&Segment], outputs: &HashMap<usize, String>, target: &str) -> HashMap<usize, Problem> {
    let mut problems = HashMap::new();
    for s in segs {
        if let Some(p) = check_segment(s, outputs.get(&s.id).map(|x| x.as_str()), target) {
            problems.insert(s.id, p);
        }
    }
    // Misalignment: neighbours with identical output but different sources.
    for w in segs.windows(2) {
        let (a, b) = (w[0], w[1]);
        if let (Some(oa), Some(ob)) = (outputs.get(&a.id), outputs.get(&b.id))
            && normalize(oa) == normalize(ob)
            && normalize(&a.text) != normalize(&b.text)
            && nonspace_chars(oa) >= 8
        {
            problems.entry(a.id).or_insert(Problem::Misaligned);
            problems.entry(b.id).or_insert(Problem::Misaligned);
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::translate::segment::segment_html;

    fn seg(html: &str) -> Segment {
        segment_html(html).segments.remove(0)
    }

    #[test]
    fn accepts_good_translation() {
        let s = seg("<p>The prime minister said on Monday that taxes would rise next year.</p>");
        assert_eq!(check_segment(&s, Some("首相周一表示，明年将会加税。"), "zh-CN"), None);
    }

    #[test]
    fn detects_untranslated() {
        let s = seg("<p>The prime minister said on Monday that taxes would rise next year.</p>");
        assert_eq!(check_segment(&s, Some("The prime minister said on Monday that taxes would rise next year."), "zh-CN"), Some(Problem::Untranslated));
        // Partially translated: a full English sentence left over.
        let s2 = seg("<p>Taxes will rise. The prime minister said on Monday that the government had no other choice.</p>");
        assert_eq!(
            check_segment(&s2, Some("税收将上涨。The prime minister said on Monday that the government had no other choice."), "zh-CN"),
            Some(Problem::Untranslated)
        );
        // Short names are fine untranslated.
        let s3 = seg("<p>BBC News</p>");
        assert_eq!(check_segment(&s3, Some("BBC News"), "zh-CN"), None);
    }

    #[test]
    fn detects_repetition_and_length() {
        let s = seg("<p>The prime minister said on Monday that taxes would rise next year, and the opposition criticised the plan.</p>");
        let looped = "首相周一表示明年将加税，".repeat(12);
        assert_eq!(check_segment(&s, Some(&looped), "zh-CN"), Some(Problem::Repetition));
        assert_eq!(check_segment(&s, Some("首相。"), "zh-CN"), Some(Problem::TooShort));
    }

    #[test]
    fn detects_source_echo_and_placeholders() {
        let s = seg("<p>The prime minister said on <b>Monday</b> that taxes would rise.</p>");
        assert_eq!(
            check_segment(&s, Some("The prime minister said on Monday that taxes would rise. 首相<g1>周一</g1>表示税收将上涨。"), "zh-CN"),
            Some(Problem::ContainsSource)
        );
        assert_eq!(check_segment(&s, Some("首相周一表示税收将上涨。"), "zh-CN"), Some(Problem::Placeholders));
        assert_eq!(check_segment(&s, Some("首相<g1>周一</g1>表示税收将上涨。"), "zh-CN"), None);
    }

    #[test]
    fn detects_misalignment() {
        let a = segment_html("<p>First paragraph about the weather today.</p><p>Second paragraph about sport results.</p>");
        let segs: Vec<&Segment> = a.segments.iter().collect();
        let mut out = HashMap::new();
        out.insert(1, "关于今天天气的第一段，以及体育结果。".to_string());
        out.insert(2, "关于今天天气的第一段，以及体育结果。".to_string());
        let p = check_batch(&segs, &out, "zh-CN");
        assert_eq!(p.get(&1), Some(&Problem::Misaligned));
        let mut out2 = HashMap::new();
        out2.insert(1, "关于今天天气的第一段。".to_string());
        let p2 = check_batch(&segs, &out2, "zh-CN");
        assert_eq!(p2.get(&2), Some(&Problem::Missing));
    }

    #[test]
    fn chinese_to_english() {
        let s = seg("<p>法国多地上周末爆发骚乱及抢掠，当局其后在巴黎市中心部分地区实施夜间宵禁。</p>");
        assert_eq!(check_segment(&s, Some("Riots and looting broke out in several parts of France last weekend, and the authorities then imposed a night curfew in parts of central Paris."), "en"), None);
        assert_eq!(check_segment(&s, Some("法国多地上周末爆发骚乱及抢掠，当局其后在巴黎市中心部分地区实施夜间宵禁。"), "en"), Some(Problem::Untranslated));
    }
}
