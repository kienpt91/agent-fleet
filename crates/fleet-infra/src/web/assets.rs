//! The dashboard's static files, compiled into the binary. Hand-written HTML, one script and one
//! stylesheet: no external requests, no inline code, no build step.

pub const INDEX_HTML: &str = include_str!("../../web/index.html");
pub const APP_JS: &str = include_str!("../../web/app.js");
pub const APP_CSS: &str = include_str!("../../web/app.css");

/// The file served for a path, with its content type.
pub fn lookup(path: &str) -> Option<(&'static str, &'static str)> {
    match path {
        "/" | "/index.html" => Some(("text/html; charset=utf-8", INDEX_HTML)),
        "/app.js" => Some(("text/javascript; charset=utf-8", APP_JS)),
        "/app.css" => Some(("text/css; charset=utf-8", APP_CSS)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whole-word search that ignores case, for scans of the shipped sources.
    fn contains_ci(haystack: &str, needle: &str) -> bool {
        haystack
            .to_ascii_lowercase()
            .contains(&needle.to_ascii_lowercase())
    }

    #[test]
    fn every_file_is_served_and_nothing_else() {
        for p in ["/", "/index.html", "/app.js", "/app.css"] {
            assert!(lookup(p).is_some(), "{p}");
        }
        for p in [
            "/app.js.map",
            "/favicon.ico",
            "/../app.js",
            "/APP.JS",
            "/api/status",
            "",
        ] {
            assert!(lookup(p).is_none(), "{p}");
        }
    }

    /// The script renders everything the daemon sends with `textContent` or text nodes. These are
    /// the ways of turning a string into markup or code; none may appear in the source, not even
    /// in a comment, so the scan stays simple and strict.
    const FORBIDDEN_IN_JS: &[&str] = &[
        "innerHTML",
        "outerHTML",
        "insertAdjacentHTML",
        "document.write",
        "eval(",
        "new Function",
        "Function(",
        "createContextualFragment",
        "DOMParser",
        "srcdoc",
        "javascript:",
        "setAttribute('style'",
        "setAttribute(\"style\"",
        "cssText",
        "setTimeout(\"",
        "setTimeout('",
        "setInterval(\"",
        "setInterval('",
        "localStorage",
        "document.cookie",
        // EventSource cannot send the session header; the stream is read with fetch instead
        "EventSource",
        "import(",
        "importScripts",
        "XMLHttpRequest",
        "WebSocket",
        "http://",
        "https://",
    ];

    #[test]
    fn the_script_never_turns_text_into_markup_or_code() {
        for bad in FORBIDDEN_IN_JS {
            assert!(!APP_JS.contains(bad), "app.js contains {bad:?}");
        }
        assert!(
            APP_JS.contains("textContent"),
            "the script must use textContent"
        );
    }

    #[test]
    fn the_script_sends_the_csrf_header_and_the_digest() {
        assert!(APP_JS.contains("X-CSRF-Token"));
        assert!(APP_JS.contains("digest"));
    }

    /// The credential is a header held in the page, never a cookie: cookies are not isolated by
    /// port and would travel to every other server on the host.
    #[test]
    fn the_script_keeps_the_session_in_a_header_and_in_session_storage_only() {
        assert!(APP_JS.contains("X-Fleet-Session"));
        assert!(!APP_JS.to_ascii_lowercase().contains("cookie"));
        assert!(
            !APP_JS.contains("credentials: 'include'")
                && !APP_JS.contains("credentials: \"include\"")
        );
        assert!(APP_JS.contains("credentials: 'omit'"));
        // the only thing ever stored is the session under one key, only in sessionStorage
        for line in APP_JS.lines().filter(|l| l.contains("sessionStorage")) {
            assert!(
                line.contains("fleet.session") || line.contains("STORE_KEY"),
                "{line}"
            );
        }
    }

    #[test]
    fn the_page_has_no_inline_code_no_inline_style_and_no_external_resources() {
        let html = INDEX_HTML;
        assert!(contains_ci(html, "<script src=\"/app.js\""));
        assert!(contains_ci(html, "href=\"/app.css\""));
        // every <script> element has a src and no body
        let mut rest = html.to_ascii_lowercase();
        let mut scripts = 0;
        while let Some(i) = rest.find("<script") {
            let tail = rest.split_off(i);
            let end = tail.find('>').expect("script tag is closed");
            let tag = &tail[..end];
            assert!(
                tag.contains("src=\"/app.js\""),
                "unexpected script tag: {tag}"
            );
            let after = &tail[end + 1..];
            assert!(after.starts_with("</script>"), "inline script body");
            scripts += 1;
            rest = after.to_string();
        }
        assert_eq!(scripts, 1);
        for bad in [
            "<style", " style=", "http://", "https://", "//cdn", "<iframe", "<object", "<embed",
            "<form", "<base", "@import",
        ] {
            assert!(!contains_ci(html, bad), "index.html contains {bad:?}");
        }
        // no event-handler attributes (onclick=, onerror=, ...)
        let lower = html.to_ascii_lowercase();
        for (i, _) in lower.match_indices(" on") {
            let word: String = lower[i + 1..]
                .chars()
                .take_while(|c| c.is_ascii_alphabetic())
                .collect();
            let after = lower[i + 1 + word.len()..].chars().next();
            assert!(
                !(word.len() > 2 && after == Some('=')),
                "event handler attribute {word} in index.html"
            );
        }
    }

    #[test]
    fn the_stylesheet_loads_nothing_from_elsewhere() {
        for bad in [
            "@import",
            "url(http",
            "url(\"http",
            "url('http",
            "//",
            "expression(",
        ] {
            assert!(!APP_CSS.contains(bad), "app.css contains {bad:?}");
        }
    }

    #[test]
    fn the_assets_are_small_enough_to_review() {
        assert!(INDEX_HTML.len() < 16 * 1024);
        assert!(APP_JS.len() < 192 * 1024);
        assert!(APP_CSS.len() < 48 * 1024);
    }
}
