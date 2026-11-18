//! Reviewer text: limits, clean-up and HTML escaping. Everything a reviewer sends is hostile.

/// Longest reviewer name kept (characters).
pub const MAX_NAME: usize = 60;
/// Longest comment kept (characters).
pub const MAX_COMMENT: usize = 2000;
/// Words of the comment that go into the marker's name.
pub const MARKER_WORDS: usize = 6;

/// `s` without control characters (newlines and tabs become spaces when `single_line`), with
/// runs of whitespace kept as typed, trimmed, and cut to `max` characters. `None` when nothing is
/// left.
pub fn clean(s: &str, max: usize, single_line: bool) -> Option<String> {
    let mut out = String::with_capacity(s.len().min(max * 4));
    let mut n = 0;
    for c in s.chars() {
        if n >= max {
            break;
        }
        let c = match c {
            '\n' if !single_line => '\n',
            '\n' | '\t' | '\r' => ' ',
            // bidi overrides can make a marker name read differently from what was typed
            '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' => continue,
            c if c.is_control() => continue,
            c => c,
        };
        out.push(c);
        n += 1;
    }
    let t = out.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// The marker name for a comment: `"<author>: <first words>"`, with an ellipsis when cut.
pub fn marker_name(author: &str, text: &str) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut head = words.iter().take(MARKER_WORDS).copied().collect::<Vec<_>>().join(" ");
    if head.chars().count() > 48 {
        head = head.chars().take(47).collect();
        head.push('…');
    } else if words.len() > MARKER_WORDS {
        head.push('…');
    }
    if author.is_empty() { head } else { format!("{author}: {head}") }
}

/// Escape text for HTML element content and quoted attribute values.
pub fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Whether `s` is `len` lowercase hex digits (ids and tokens).
pub fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostile_text_is_cleaned_and_cut() {
        assert_eq!(clean("  hi\u{0}\u{7}\tthere\r\n ", 100, true).as_deref(), Some("hi there"));
        assert_eq!(clean("a\nb", 100, false).as_deref(), Some("a\nb"));
        assert_eq!(clean("\u{202E}evil", 100, true).as_deref(), Some("evil"));
        assert_eq!(clean(" \u{0} \n", 100, false), None);
        assert_eq!(clean(&"é".repeat(5000), 10, false).unwrap().chars().count(), 10);
    }

    #[test]
    fn marker_names_keep_the_first_words() {
        assert_eq!(marker_name("Ana", "cut this a beat earlier please, it drags"), "Ana: cut this a beat earlier please,…");
        assert_eq!(marker_name("Ana", "too loud"), "Ana: too loud");
        assert_eq!(marker_name("", "x"), "x");
        assert!(marker_name("Ana", &"long".repeat(40)).chars().count() < 60);
    }

    #[test]
    fn html_is_escaped() {
        assert_eq!(html_escape(r#"<script>alert("x")</script>&'"#), "&lt;script&gt;alert(&quot;x&quot;)&lt;/script&gt;&amp;&#39;");
    }

    #[test]
    fn hex_ids_are_checked() {
        assert!(is_hex("0a1b", 4));
        assert!(!is_hex("0A1B", 4));
        assert!(!is_hex("0a1", 4));
        assert!(!is_hex("../x", 4));
    }
}
