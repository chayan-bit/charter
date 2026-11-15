//! Short text from the chain that pages show inline (labels, milestones, titles, aliases,
//! search rows): the bidi and direction-mark characters that could reorder the text around
//! them on screen (Trojan Source, CVE-2021-42574) never reach a page.

/// A bidi embedding, override or isolate, or a direction mark.
pub fn mark(c: char) -> bool {
    matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// A character an inline field must not carry: a control character or a [`mark`].
pub fn hidden(c: char) -> bool {
    c.is_control() || mark(c)
}

/// `s` without its [`mark`]s (control characters are left to each field's own rule).
pub fn strip(s: &str) -> String {
    s.chars().filter(|c| !mark(*c)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bidi_marks_and_controls_are_taken_out() {
        let s = "a\u{202e}b\u{2066}c\u{200f}d\u{061c}e";
        assert_eq!(strip(s), "abcde");
        assert!(hidden('\u{7}') && hidden('\u{2069}') && !hidden('x'));
        assert_eq!(strip("plain text, 名前"), "plain text, 名前");
    }
}
