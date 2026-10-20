//! Language codes, names and detection helpers.

use whatlang::Lang;

/// Normalize a language tag ("zh_CN", "ZH-cn " -> "zh-cn").
pub fn norm(code: &str) -> String {
    code.trim().replace('_', "-").to_ascii_lowercase()
}

/// Human/LLM friendly language name for prompts.
pub fn display_name(code: &str) -> String {
    let c = norm(code);
    let s = match c.as_str() {
        "zh" | "zh-cn" | "zh-hans" | "zh-sg" | "zh-hans-cn" => "Simplified Chinese (简体中文)",
        "zh-tw" | "zh-hant" | "zh-hant-tw" => "Traditional Chinese as used in Taiwan (繁體中文)",
        "zh-hk" | "zh-hant-hk" | "zh-mo" => "Traditional Chinese as used in Hong Kong (繁體中文)",
        "en" | "en-us" => "English",
        "en-gb" => "British English",
        "ja" => "Japanese (日本語)",
        "ko" => "Korean (한국어)",
        "fr" => "French",
        "de" => "German",
        "es" => "Spanish",
        "pt" | "pt-pt" => "Portuguese",
        "pt-br" => "Brazilian Portuguese",
        "it" => "Italian",
        "ru" => "Russian",
        "uk" => "Ukrainian",
        "ar" => "Arabic",
        "vi" => "Vietnamese",
        "th" => "Thai",
        "id" => "Indonesian",
        "ms" => "Malay",
        "tr" => "Turkish",
        "nl" => "Dutch",
        "pl" => "Polish",
        "sv" => "Swedish",
        "hi" => "Hindi",
        "fa" => "Persian",
        "he" => "Hebrew",
        _ => return code.trim().to_string(),
    };
    s.to_string()
}

/// whatlang language for a tag (for "already in target language" checks).
pub fn whatlang_of(code: &str) -> Option<Lang> {
    let c = norm(code);
    let primary = c.split('-').next().unwrap_or("");
    Some(match primary {
        "zh" => Lang::Cmn,
        "en" => Lang::Eng,
        "ja" => Lang::Jpn,
        "ko" => Lang::Kor,
        "fr" => Lang::Fra,
        "de" => Lang::Deu,
        "es" => Lang::Spa,
        "pt" => Lang::Por,
        "it" => Lang::Ita,
        "ru" => Lang::Rus,
        "uk" => Lang::Ukr,
        "ar" => Lang::Ara,
        "vi" => Lang::Vie,
        "th" => Lang::Tha,
        "id" => Lang::Ind,
        "tr" => Lang::Tur,
        "nl" => Lang::Nld,
        "pl" => Lang::Pol,
        "sv" => Lang::Swe,
        "hi" => Lang::Hin,
        "fa" => Lang::Pes,
        "he" => Lang::Heb,
        _ => return None,
    })
}

/// Does the target language use CJK script (Han/kana/hangul)?
pub fn is_cjk(code: &str) -> bool {
    matches!(norm(code).split('-').next().unwrap_or(""), "zh" | "ja" | "ko")
}

/// Text direction of a language ("rtl" for Arabic, Hebrew, Persian, Urdu…).
pub fn direction(code: &str) -> &'static str {
    match norm(code).split('-').next().unwrap_or("") {
        "ar" | "fa" | "he" | "iw" | "ur" | "ps" | "sd" | "ug" | "yi" | "dv" | "ckb" => "rtl",
        _ => "ltr",
    }
}

/// Detect the language of a text; returns None when unreliable.
pub fn detect(text: &str) -> Option<Lang> {
    let info = whatlang::detect(text)?;
    if info.is_reliable() || info.confidence() > 0.8 { Some(info.lang()) } else { None }
}

/// Is `text` (long enough) already written in the target language?
pub fn already_in(text: &str, target: &str) -> bool {
    let Some(t) = whatlang_of(target) else { return false };
    // CJK text carries more information per character.
    let min_chars = if crate::util::text::cjk_ratio(text) > 0.5 { 16 } else { 40 };
    if text.chars().count() < min_chars {
        return false;
    }
    // Traditional vs simplified Chinese both detect as Cmn; OpenCC handles scripts.
    detect(text) == Some(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_detection() {
        assert_eq!(display_name("zh-CN"), "Simplified Chinese (简体中文)");
        assert_eq!(display_name("xx-YY"), "xx-YY");
        assert!(is_cjk("zh-TW"));
        assert!(!is_cjk("en"));
        assert!(already_in("这是一段用简体中文写成的新闻报道，内容关于法国的骚乱事件以及政府的应对措施。", "zh-CN"));
        assert!(!already_in("This is a news report written in English about the riots in France and the government response.", "zh-CN"));
        assert!(already_in("This is a news report written in English about the riots in France and the government response.", "en"));
    }
}
