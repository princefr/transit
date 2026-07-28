//! Normalize GTFS-RT / SIRI alert text for a France-facing UI.
//!
//! SNCF often ships multi-language translations with **German first** and HTML
//! bodies. Prefer French, strip tags, collapse whitespace, and support dedupe.

/// Language preference for passenger-facing France apps.
const LANG_PREF: &[&str] = &["fr", "fr-fr", "fr_fr", "en", "en-gb", "en-us", "und", ""];

/// Pick best translation from `(language, text)` pairs.
pub fn pick_translation(translations: &[(String, String)]) -> Option<String> {
    if translations.is_empty() {
        return None;
    }
    for pref in LANG_PREF {
        for (lang, text) in translations {
            let l = lang.trim().to_ascii_lowercase();
            if l == *pref && !text.trim().is_empty() {
                return Some(clean_alert_text(text));
            }
        }
    }
    // Prefer any non-German text if FR/EN missing
    for (lang, text) in translations {
        let l = lang.trim().to_ascii_lowercase();
        if l.starts_with("de") {
            continue;
        }
        if !text.trim().is_empty() {
            return Some(clean_alert_text(text));
        }
    }
    translations
        .first()
        .map(|(_, t)| clean_alert_text(t))
        .filter(|s| !s.is_empty())
}

/// Strip HTML tags, decode a few entities, collapse whitespace.
pub fn clean_alert_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut in_tag = false;
    for ch in raw.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    let decoded = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("<br>", " ")
        .replace("<br/>", " ")
        .replace("<br />", " ");
    // second pass if entities left tags (rare)
    let mut cleaned = String::with_capacity(decoded.len());
    let mut in_tag = false;
    for ch in decoded.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                cleaned.push(' ');
            }
            _ if !in_tag => cleaned.push(ch),
            _ => {}
        }
    }
    collapse_ws(&cleaned)
}

fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = true;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    out.trim().to_string()
}

/// Content key for deduplicating near-identical alerts (ignore entity ids).
pub fn alert_dedupe_key(header: Option<&str>, description: Option<&str>) -> String {
    let h = header.unwrap_or("").trim().to_lowercase();
    let d = description.unwrap_or("").trim().to_lowercase();
    // first 160 chars of body is enough to merge QOM broadcast clones
    let d_short: String = d.chars().take(160).collect();
    format!("{h}|{d_short}")
}

/// True if active_periods contain "now" or periods are empty (always show).
pub fn alert_active_now(periods: &[(Option<i64>, Option<i64>)], now_ts: i64) -> bool {
    if periods.is_empty() {
        return true;
    }
    periods.iter().any(|(start, end)| {
        let after_start = start.map(|s| now_ts >= s).unwrap_or(true);
        let before_end = end.map(|e| now_ts <= e).unwrap_or(true);
        after_start && before_end
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_french_over_german() {
        let tr = vec![
            ("de".into(), "Hitzewarnung".into()),
            ("fr".into(), "Alerte canicule".into()),
            ("en".into(), "Heat warning".into()),
        ];
        assert_eq!(pick_translation(&tr).as_deref(), Some("Alerte canicule"));
    }

    #[test]
    fn strips_html_paragraphs() {
        let raw = "<p>Votre train <b>879502</b></p><p>reste en gare.</p>";
        let c = clean_alert_text(raw);
        assert!(!c.contains('<'));
        assert!(c.contains("879502"));
        assert!(c.contains("reste en gare"));
    }

    #[test]
    fn dedupe_key_stable() {
        let k1 = alert_dedupe_key(Some("Foo"), Some("<p>Bar</p>"));
        let k2 = alert_dedupe_key(Some("foo"), Some("<p>Bar</p>"));
        // raw description differs by case only in key via lowercasing of cleaned? we pass raw
        assert_eq!(
            alert_dedupe_key(Some("Foo"), Some("Bar")),
            alert_dedupe_key(Some("foo"), Some("bar"))
        );
        let _ = (k1, k2);
    }

    #[test]
    fn active_window() {
        assert!(alert_active_now(&[], 100));
        assert!(alert_active_now(&[(Some(50), Some(150))], 100));
        assert!(!alert_active_now(&[(Some(50), Some(90))], 100));
    }
}
