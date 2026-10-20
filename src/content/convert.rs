//! Chinese script conversion with OpenCC (pure-Rust implementation with the official
//! OpenCC dictionaries). Only text nodes are converted; URLs/attributes are untouched.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use ferrous_opencc::OpenCC;
use ferrous_opencc::config::BuiltinConfig;

/// Supported conversion modes (OpenCC config names).
pub const MODES: &[(&str, &str)] = &[
    ("t2s", "繁体 → 简体 (OpenCC t2s)"),
    ("hk2s", "繁体(香港) → 简体"),
    ("tw2s", "繁体(台湾) → 简体"),
    ("tw2sp", "繁体(台湾) → 简体，含词汇转换"),
    ("s2t", "简体 → 繁体"),
    ("s2hk", "简体 → 繁体(香港)"),
    ("s2tw", "简体 → 繁体(台湾)"),
    ("s2twp", "简体 → 繁体(台湾)，含词汇转换"),
    ("t2tw", "繁体 → 台湾正体"),
    ("t2hk", "繁体 → 香港繁体"),
];

fn builtin(mode: &str) -> Option<BuiltinConfig> {
    Some(match mode {
        "t2s" => BuiltinConfig::T2s,
        "hk2s" => BuiltinConfig::Hk2s,
        "tw2s" => BuiltinConfig::Tw2s,
        "tw2sp" => BuiltinConfig::Tw2sp,
        "s2t" => BuiltinConfig::S2t,
        "s2hk" => BuiltinConfig::S2hk,
        "s2tw" => BuiltinConfig::S2tw,
        "s2twp" => BuiltinConfig::S2twp,
        "t2tw" => BuiltinConfig::T2tw,
        "t2hk" => BuiltinConfig::T2hk,
        _ => return None,
    })
}

/// Lazily created, shared converters.
#[derive(Clone, Default)]
pub struct Converter {
    cache: Arc<Mutex<HashMap<String, Arc<OpenCC>>>>,
}

impl Converter {
    pub fn get(&self, mode: &str) -> Result<Arc<OpenCC>> {
        let mode = mode.trim().to_ascii_lowercase();
        if let Some(c) = self.cache.lock().unwrap().get(&mode) {
            return Ok(c.clone());
        }
        let cfg = builtin(&mode).ok_or_else(|| anyhow!("unknown OpenCC mode: {mode}"))?;
        let cc = Arc::new(OpenCC::from_config(cfg).map_err(|e| anyhow!("OpenCC init {mode}: {e}"))?);
        self.cache.lock().unwrap().insert(mode, cc.clone());
        Ok(cc)
    }

    pub fn convert_text(&self, mode: &str, text: &str) -> Result<String> {
        Ok(self.get(mode)?.convert(text))
    }

    /// Convert the text nodes of an HTML fragment (skipping code/pre). Image alt texts
    /// and titles are converted as well.
    pub fn convert_html(&self, mode: &str, html: &str) -> Result<String> {
        let cc = self.get(mode)?;
        let doc = super::dom::parse_fragment(html);
        for n in doc.root().descendants() {
            if n.is_text() {
                if super::dom::in_skip_context(&n) {
                    continue;
                }
                let t = n.text().to_string();
                if t.trim().is_empty() {
                    continue;
                }
                let out = cc.convert(&t);
                if out != t {
                    n.set_text(out);
                }
            } else if n.is_element() {
                for attr in ["alt", "title"] {
                    if let Some(v) = n.attr(attr) {
                        let out = cc.convert(&v);
                        if out != *v {
                            n.set_attr(attr, &out);
                        }
                    }
                }
            }
        }
        Ok(super::dom::fragment_html(&doc))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn t2s_converts_text_only() {
        let c = Converter::default();
        let html = r#"<p>法國多地上週末爆發騷亂</p><img src="https://x.com/騷亂.jpg" alt="巴黎廣場"><code>繁體</code>"#;
        let out = c.convert_html("t2s", html).unwrap();
        assert!(out.contains("法国多地上周末爆发骚乱"), "{out}");
        assert!(out.contains("alt=\"巴黎广场\""), "{out}");
        assert!(out.contains("https://x.com/騷亂.jpg"), "{out}");
        assert!(out.contains("<code>繁體</code>"), "{out}");
    }

    #[test]
    fn all_modes_load() {
        let c = Converter::default();
        for (m, _) in MODES {
            assert!(c.get(m).is_ok(), "{m}");
        }
        assert!(c.get("bogus").is_err());
        assert_eq!(c.convert_text("s2t", "汉字").unwrap(), "漢字");
    }
}
