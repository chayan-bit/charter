//! Untrusted markdown (READMEs, issue and comment bodies) to HTML: pulldown-cmark renders,
//! ammonia sanitizes (no script, no event handlers, no `javascript:` links, no inline style).

use pulldown_cmark::{Options, Parser, html};

pub fn render(md: &str) -> String {
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut out = String::with_capacity(md.len() * 3 / 2);
    html::push_html(&mut out, Parser::new_ext(md, opts));
    ammonia::Builder::default()
        .link_rel(Some("noopener noreferrer nofollow"))
        .clean(&out)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_is_rendered_then_sanitized() {
        let h = render(
            "# T\n\n<script>alert(1)</script><img src=x onerror=alert(1)>\n\n[a](javascript:alert(1)) <b style=\"color:red\">b</b>",
        );
        assert!(h.contains("<h1>T</h1>"), "{h}");
        for bad in ["<script", "onerror", "javascript:", "style="] {
            assert!(!h.contains(bad), "{bad} in {h}");
        }
    }
}
