//! Untrusted markdown (READMEs, issue and comment bodies) to HTML: pulldown-cmark renders,
//! ammonia sanitizes (no script, no event handlers, no `javascript:` links, no inline style).
//! A README is rendered with [`render_in`], which also resolves its relative links and
//! images against the repo (`blob` for links, `raw` for images, so they stay same-origin).

use std::borrow::Cow;

use pulldown_cmark::{Options, Parser, html};

/// Where a README lives: repo id (hex), rev (already one path segment) and directory.
pub struct Links<'a> {
    pub rid: &'a str,
    pub rev: &'a str,
    pub dir: &'a str,
}

pub fn render(md: &str) -> String {
    clean(md, ammonia::Builder::default())
}

/// [`render`], with relative `href` and `src` values resolved against `at`.
pub fn render_in(md: &str, at: &Links) -> String {
    let (rid, rev, dir) = (at.rid.to_owned(), at.rev.to_owned(), at.dir.to_owned());
    let mut b = ammonia::Builder::default();
    b.attribute_filter(move |tag, attr, val| {
        let resolved = match (tag, attr) {
            ("img", "src") => resolve(&rid, "raw", &rev, &dir, val),
            ("a", "href") => resolve(&rid, "blob", &rev, &dir, val),
            _ => None,
        };
        Some(resolved.map_or(Cow::Borrowed(val), Cow::Owned))
    })
    .set_tag_attribute_value("img", "loading", "lazy");
    clean(md, b)
}

fn clean(md: &str, mut b: ammonia::Builder) -> String {
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut out = String::with_capacity(md.len() * 3 / 2);
    html::push_html(&mut out, Parser::new_ext(md, opts));
    b.link_rel(Some("noopener noreferrer nofollow"))
        .clean(&out)
        .to_string()
}

/// `/r/<rid>/<kind>/<rev>/<dir + url>` for a relative `url`; `None` leaves the value alone
/// (absolute, root-relative, fragment-only or empty). A path ending in `/` is a directory,
/// so a link to it goes to `tree` instead of `blob`.
fn resolve(rid: &str, kind: &str, rev: &str, dir: &str, url: &str) -> Option<String> {
    let cut = url.find(['?', '#']).unwrap_or(url.len());
    let (path, tail) = url.split_at(cut);
    let first = path.split('/').next().unwrap_or("");
    if path.is_empty() || path.starts_with('/') || first.contains(':') {
        return None;
    }
    let mut parts: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            // Above the repo root there is nothing to climb to: stay at the root.
            ".." => drop(parts.pop()),
            s => parts.push(s),
        }
    }
    let is_dir = matches!(path.rsplit('/').next(), Some("" | "." | ".."));
    let kind = if kind == "blob" && (is_dir || parts.is_empty()) {
        "tree"
    } else {
        kind
    };
    Some(format!("/r/{rid}/{kind}/{rev}/{}{tail}", parts.join("/")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const AT: Links = Links {
        rid: "ab12",
        rev: "main",
        dir: "docs/sub",
    };

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

    #[test]
    fn relative_links_go_to_blob_and_images_to_raw() {
        let h = render_in("[g](guide.md#top) ![d](../img/a.png)", &AT);
        assert!(
            h.contains(r#"href="/r/ab12/blob/main/docs/sub/guide.md#top""#),
            "{h}"
        );
        assert!(
            h.contains(r#"src="/r/ab12/raw/main/docs/img/a.png""#),
            "{h}"
        );
        assert!(h.contains(r#"loading="lazy""#), "{h}");
    }

    #[test]
    fn directory_links_go_to_tree_and_dots_cannot_escape_the_root() {
        let h = render_in("[d](../) [e](../../../../x.md) [r](..)", &AT);
        assert!(h.contains(r#"href="/r/ab12/tree/main/docs""#), "{h}");
        assert!(h.contains(r#"href="/r/ab12/blob/main/x.md""#), "{h}");
        assert_eq!(
            h.matches(r#"href="/r/ab12/tree/main/docs""#).count(),
            2,
            "{h}"
        );
        assert!(!h.contains(".."), "{h}");
    }

    #[test]
    fn absolute_root_relative_and_fragment_urls_are_left_alone() {
        let h = render_in(
            "[a](https://example.com/x) [b](/assets/app.css) [c](#sec) [d](mailto:a@b.c)",
            &AT,
        );
        for keep in [
            "https://example.com/x",
            "\"/assets/app.css\"",
            "\"#sec\"",
            "mailto:a@b.c",
        ] {
            assert!(h.contains(keep), "{keep} in {h}");
        }
        assert!(!h.contains("/r/ab12"), "{h}");
    }

    #[test]
    fn resolved_readmes_are_still_sanitized() {
        let h = render_in(
            "[x](javascript:alert(1)) <img src=y.png onerror=alert(1)>",
            &AT,
        );
        for bad in ["javascript:", "onerror"] {
            assert!(!h.contains(bad), "{bad} in {h}");
        }
        assert!(h.contains("/r/ab12/raw/main/docs/sub/y.png"), "{h}");
    }
}
