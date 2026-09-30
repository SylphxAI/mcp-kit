//! Code-aware tokenizer: identifiers are kept whole and also split on
//! camelCase, PascalCase, snake_case and digits, all lowercased.

use std::collections::HashMap;

/// Push the whole identifier and its sub-words.
pub fn push_ident(word: &str, out: &mut Vec<String>) {
    if word.len() < 2 || word.len() > 64 {
        return;
    }
    let lower = word.to_ascii_lowercase();
    let mut parts: Vec<String> = Vec::new();
    for piece in word.split('_').filter(|p| !p.is_empty()) {
        let chars: Vec<char> = piece.chars().collect();
        let mut cur = String::new();
        for i in 0..chars.len() {
            let c = chars[i];
            let boundary = i > 0
                && ((c.is_ascii_uppercase()
                    && (chars[i - 1].is_ascii_lowercase()
                        || chars[i - 1].is_ascii_digit()
                        || (i + 1 < chars.len() && chars[i + 1].is_ascii_lowercase() && chars[i - 1].is_ascii_uppercase())))
                    || (c.is_ascii_digit() != chars[i - 1].is_ascii_digit()));
            if boundary && !cur.is_empty() {
                parts.push(std::mem::take(&mut cur));
            }
            cur.push(c.to_ascii_lowercase());
        }
        if !cur.is_empty() {
            parts.push(cur);
        }
    }
    let multi = parts.len() > 1;
    out.push(lower);
    if multi {
        for p in parts {
            if p.len() >= 2 && !p.chars().all(|c| c.is_ascii_digit()) {
                out.push(p);
            }
        }
    }
}

/// Tokenize free text or code.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for (i, ch) in text.char_indices() {
        let word = ch.is_ascii_alphanumeric() || ch == '_';
        match (word, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                push_ident(&text[s..i], &mut out);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        push_ident(&text[s..], &mut out);
    }
    out
}

/// Terms contributed by a file path (directory and file stems).
pub fn path_terms(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    for seg in path.split('/') {
        let stem = seg.split('.').next().unwrap_or(seg);
        for t in tokenize(stem) {
            if !out.contains(&t) {
                out.push(t);
            }
        }
    }
    out
}

/// Term frequencies for a chunk: body tokens, the symbol name boosted, and
/// path terms once each.
pub fn chunk_terms(body: &str, symbol: Option<&str>, path_terms: &[String]) -> (Vec<(String, u16)>, u32) {
    let mut tf: HashMap<String, u16> = HashMap::new();
    let mut len = 0u32;
    // Very long chunks (minified or generated code) are capped.
    let body = if body.len() > 64 * 1024 { &body[..floor_char(body, 64 * 1024)] } else { body };
    for t in tokenize(body) {
        let e = tf.entry(t).or_insert(0);
        *e = e.saturating_add(1);
        len += 1;
    }
    if let Some(name) = symbol {
        for t in tokenize(name) {
            let e = tf.entry(t).or_insert(0);
            *e = e.saturating_add(3);
            len += 3;
        }
    }
    for t in path_terms {
        let e = tf.entry(t.clone()).or_insert(0);
        *e = e.saturating_add(1);
        len += 1;
    }
    let mut terms: Vec<(String, u16)> = tf.into_iter().collect();
    terms.sort();
    (terms, len)
}

/// Floor a byte limit to a UTF-8 boundary, clamped to the text length.
pub fn floor_char(s: &str, i: usize) -> usize {
    let mut i = i.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_chunk_boundary() {
        let text = "é".repeat(40_000);
        assert_eq!(floor_char(&text, 3), 2);
        assert_eq!(floor_char("é", 99), 2);
        assert_eq!(chunk_terms(&text, None, &[]), (vec![], 0));
        assert_eq!(path_terms("src/user_id.rs"), ["src", "user_id", "user", "id"]);
        assert_eq!(chunk_terms("read read", Some("read"), &["src".into()]), (vec![("read".into(), 5), ("src".into(), 1)], 6));
    }

    #[test]
    fn splits_identifiers() {
        let t = tokenize("parseHTTPResponse user_id XMLParser v2");
        for want in ["parsehttpresponse", "parse", "http", "response", "user_id", "user", "id", "xmlparser", "xml", "parser", "v2"] {
            assert!(t.contains(&want.to_string()), "missing {want} in {t:?}");
        }
    }
}
