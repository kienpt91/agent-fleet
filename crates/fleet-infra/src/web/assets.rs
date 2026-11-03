//! The dashboard's static files, compiled into the binary. Hand-written HTML, three scripts and two
//! stylesheets: no external requests, no inline code, no build step.

pub const INDEX_HTML: &str = include_str!("../../web/index.html");
pub const APP_JS: &str = include_str!("../../web/app.js");
pub const WORKBENCH_JS: &str = include_str!("../../web/workbench.js");
pub const PANES_JS: &str = include_str!("../../web/panes.js");
pub const APP_CSS: &str = include_str!("../../web/app.css");
pub const PANES_CSS: &str = include_str!("../../web/panes.css");

/// The scripts, in the order the page loads them.
pub const SCRIPTS: [(&str, &str); 3] = [
    ("/app.js", APP_JS),
    ("/workbench.js", WORKBENCH_JS),
    ("/panes.js", PANES_JS),
];
/// The stylesheets, in the order the page loads them.
pub const STYLESHEETS: [(&str, &str); 2] = [("/app.css", APP_CSS), ("/panes.css", PANES_CSS)];

/// The file served for a path, with its content type.
pub fn lookup(path: &str) -> Option<(&'static str, &'static str)> {
    match path {
        "/" | "/index.html" => Some(("text/html; charset=utf-8", INDEX_HTML)),
        "/app.js" => Some(("text/javascript; charset=utf-8", APP_JS)),
        "/workbench.js" => Some(("text/javascript; charset=utf-8", WORKBENCH_JS)),
        "/panes.js" => Some(("text/javascript; charset=utf-8", PANES_JS)),
        "/app.css" => Some(("text/css; charset=utf-8", APP_CSS)),
        "/panes.css" => Some(("text/css; charset=utf-8", PANES_CSS)),
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
        for p in [
            "/",
            "/index.html",
            "/app.js",
            "/workbench.js",
            "/panes.js",
            "/app.css",
            "/panes.css",
        ] {
            assert!(lookup(p).is_some(), "{p}");
        }
        for p in [
            "/app.js.map",
            "/favicon.ico",
            "/../app.js",
            "/APP.JS",
            "/api/status",
            "/web/panes.js",
            "/panes.js/",
            "",
        ] {
            assert!(lookup(p).is_none(), "{p}");
        }
        for (path, body) in SCRIPTS.iter().chain(STYLESHEETS.iter()) {
            assert_eq!(lookup(path).map(|(_, b)| b), Some(*body), "{path}");
        }
    }

    /// The scripts render everything the daemon sends with `textContent` or text nodes. These are
    /// the ways of turning a string into markup or code; none may appear in the source, not even
    /// in a comment, so the scan stays simple and strict.
    const FORBIDDEN_IN_JS: &[&str] = &[
        "innerHTML",
        "outerHTML",
        "insertAdjacentHTML",
        "insertAdjacentElement",
        "document.write",
        "eval(",
        "new Function",
        "Function(",
        "createContextualFragment",
        "DOMParser",
        "srcdoc",
        "javascript:",
        "vbscript:",
        "setAttribute('style'",
        "setAttribute(\"style\"",
        "cssText",
        "setTimeout(\"",
        "setTimeout('",
        "setInterval(\"",
        "setInterval('",
        "localStorage",
        "document.cookie",
        "document.domain",
        // EventSource cannot send the session header; the stream is read with fetch instead
        "EventSource",
        "import(",
        "importScripts",
        "XMLHttpRequest",
        "WebSocket",
        "SharedWorker",
        "ServiceWorker",
        "serviceWorker",
        "sendBeacon",
        // images are shown from data: URLs only; an object URL would need `blob:` in the CSP
        "createObjectURL",
        // nothing the repository says may move the page: links are opened only by a click, in a new
        // tab, through window.open
        "location.assign",
        "location.replace",
        "location.href",
        "http://",
        "https://",
    ];

    #[test]
    fn the_scripts_never_turn_text_into_markup_or_code() {
        for (path, body) in SCRIPTS {
            for bad in FORBIDDEN_IN_JS {
                assert!(!body.contains(bad), "{path} contains {bad:?}");
            }
            assert!(body.contains("textContent"), "{path} must use textContent");
            assert!(body.contains("'use strict'"), "{path} must be strict");
        }
    }

    #[test]
    fn no_script_touches_a_style_attribute_or_an_event_handler_attribute() {
        for (path, body) in SCRIPTS {
            for bad in [
                "setAttribute('on",
                "setAttribute(\"on",
                "setAttribute('srcdoc",
                "setAttribute('formaction",
                "setAttributeNS",
            ] {
                // the SVG helper of the DAG is the only user of namespaced attributes
                if bad == "setAttributeNS" && path == "/app.js" {
                    continue;
                }
                assert!(!body.contains(bad), "{path} contains {bad:?}");
            }
        }
    }

    #[test]
    fn the_script_sends_the_csrf_header_and_the_digest() {
        assert!(APP_JS.contains("X-CSRF-Token"));
        assert!(APP_JS.contains("digest"));
    }

    /// The Messages page only reads. Sending, muting and unmuting have no web route, so the
    /// script must not even name one.
    #[test]
    fn the_script_can_read_messages_and_has_no_way_to_send_mute_or_unmute_them() {
        assert!(APP_JS.contains("'/api/messages?limit='"));
        assert!(APP_JS.contains("'/api/message?id='"));
        for bad in [
            "/api/msg",
            "msg.send",
            "msg.mute",
            "msg.unmute",
            "/api/messages/",
            "/api/message/",
        ] {
            assert!(!APP_JS.contains(bad), "app.js contains {bad:?}");
        }
        // every api() call that names a message route is a GET
        for line in APP_JS.lines().filter(|l| l.contains("/api/message")) {
            assert!(line.contains("api('GET'"), "{line}");
        }
    }

    /// The credential is a header held in the page, never a cookie: cookies are not isolated by
    /// port and would travel to every other server on the host.
    #[test]
    fn the_scripts_keep_the_session_in_a_header_and_in_session_storage_only() {
        assert!(APP_JS.contains("X-Fleet-Session"));
        for (path, body) in SCRIPTS {
            assert!(
                !body.to_ascii_lowercase().contains("cookie"),
                "{path} mentions a cookie"
            );
            assert!(
                !body.contains("credentials: 'include'")
                    && !body.contains("credentials: \"include\""),
                "{path}"
            );
            // the only things ever stored are the session and the layout, each under one key,
            // only in sessionStorage
            for line in body.lines().filter(|l| l.contains("sessionStorage")) {
                assert!(
                    line.contains("fleet.session")
                        || line.contains("STORE_KEY")
                        || line.contains("fleet.layout")
                        || line.contains("LAYOUT_KEY"),
                    "{path}: {line}"
                );
            }
        }
        assert!(APP_JS.contains("credentials: 'omit'"));
        // requests are made by one function that sends the credential as a header; the other
        // scripts go through it
        for (path, body) in [("/workbench.js", WORKBENCH_JS), ("/panes.js", PANES_JS)] {
            assert!(!body.contains("fetch("), "{path} must use fleetHost.api");
        }
    }

    #[test]
    fn the_page_has_no_inline_code_no_inline_style_and_no_external_resources() {
        let html = INDEX_HTML;
        for (path, _) in SCRIPTS {
            assert!(
                contains_ci(html, &format!("<script src=\"{path}\" defer></script>")),
                "{path}"
            );
        }
        for (path, _) in STYLESHEETS {
            assert!(contains_ci(html, &format!("href=\"{path}\"")), "{path}");
        }
        // the scripts are loaded in the order the bridge needs
        let at = |p: &str| html.find(&format!("src=\"{p}\"")).expect("script tag");
        assert!(at("/app.js") < at("/workbench.js") && at("/workbench.js") < at("/panes.js"));
        // every <script> element has a src that is one of ours and no body
        let mut rest = html.to_ascii_lowercase();
        let mut scripts = 0;
        while let Some(i) = rest.find("<script") {
            let tail = rest.split_off(i);
            let end = tail.find('>').expect("script tag is closed");
            let tag = &tail[..end];
            assert!(
                SCRIPTS
                    .iter()
                    .any(|(p, _)| tag.contains(&format!("src=\"{p}\""))),
                "unexpected script tag: {tag}"
            );
            let after = &tail[end + 1..];
            assert!(after.starts_with("</script>"), "inline script body");
            scripts += 1;
            rest = after.to_string();
        }
        assert_eq!(scripts, SCRIPTS.len());
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
    fn the_stylesheets_load_nothing_from_elsewhere() {
        for (path, body) in STYLESHEETS {
            for bad in [
                "@import",
                "url(http",
                "url(\"http",
                "url('http",
                "//",
                "expression(",
                "@font-face",
                "behavior:",
                "-moz-binding",
            ] {
                assert!(!body.contains(bad), "{path} contains {bad:?}");
            }
            // url() may only name a data: URI (the page's CSP allows img-src data:), never a path
            for (i, _) in body.match_indices("url(") {
                let after = body[i + 4..].trim_start_matches(['"', '\'', ' ']);
                assert!(
                    after.starts_with("data:"),
                    "{path}: url({}",
                    &after[..after.len().min(20)]
                );
            }
        }
    }

    #[test]
    fn the_assets_are_small_enough_to_review() {
        assert!(INDEX_HTML.len() < 16 * 1024);
        assert!(APP_JS.len() < 192 * 1024);
        assert!(WORKBENCH_JS.len() < 96 * 1024);
        assert!(PANES_JS.len() < 128 * 1024);
        assert!(APP_CSS.len() < 48 * 1024);
        assert!(PANES_CSS.len() < 48 * 1024);
    }
}
