//! The user's query as search terms and as an FTS5 expression.

/// Whitespace as the regular expression `\s` sees it in JavaScript.
///
/// It is not Rust's `char::is_whitespace`: JavaScript counts U+FEFF and does not count U+0085.
fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'..='\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

fn is_quote(c: char) -> bool {
    matches!(c, '"' | '\u{201C}' | '\u{201D}')
}

/// The runs of letters, numbers and `_` in already lowercased text.
fn runs(text: &str) -> Vec<String> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|run| !run.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The terms of a query: lowercased runs of letters, numbers and `_`.
pub fn query_tokens(q: &str) -> Vec<String> {
    runs(&q.to_lowercase())
}

/// The FTS5 expression for a query, or `None` when it has no usable term.
///
/// Segments between quote characters alternate loose and quoted; the quoted ones are required
/// phrases. A loose segment with several tokens adds a whole-phrase term beside its tokens. The
/// last token carries a prefix `*` once it has three characters and the raw query does not end
/// in white space or a quote.
pub fn match_query(q: &str) -> Option<String> {
    let lowered = q.to_lowercase();
    let segments: Vec<&str> = lowered.split(is_quote).collect();
    let typing = !q
        .chars()
        .next_back()
        .is_some_and(|c| is_js_whitespace(c) || is_quote(c));
    let mut required: Vec<String> = Vec::new();
    let mut loose: Vec<String> = Vec::new();
    for (i, segment) in segments.iter().enumerate() {
        let tokens = runs(segment);
        let Some(last) = tokens.last() else { continue };
        let star = if i == segments.len() - 1 && typing && last.encode_utf16().count() >= 3 {
            "*"
        } else {
            ""
        };
        let phrase = format!("\"{}\"{star}", tokens.join(" "));
        if i % 2 == 1 {
            required.push(phrase);
        } else {
            if tokens.len() > 1 {
                loose.push(phrase);
            }
            for token in &tokens[..tokens.len() - 1] {
                loose.push(format!("\"{token}\""));
            }
            loose.push(format!("\"{last}\"{star}"));
        }
    }
    if loose.is_empty() {
        return (!required.is_empty()).then(|| required.join(" AND "));
    }
    // The OR group is parenthesised because FTS5 binds AND tighter than OR.
    let loose_expr = if loose.len() > 1 {
        format!("({})", loose.join(" OR "))
    } else {
        loose.remove(0)
    };
    Some(if required.is_empty() {
        loose_expr
    } else {
        format!("{} AND {loose_expr}", required.join(" AND "))
    })
}
