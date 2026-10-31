//! The read-only repository routes: `GET /api/repo/{tree,file,blob,markdown,diff}`.
//!
//! They sit behind everything the other routes sit behind (the Host check, `Sec-Fetch-Site`,
//! authentication, the refusal of worker processes) and map onto the daemon's own `repo.*`
//! commands, which name a task and one of the commits the daemon recorded for it. This module
//! only turns a query string into those arguments (strictly: unknown parameters, repeated ones,
//! odd escapes and oversized values are refused), bounds how many reads run at once, and shapes the
//! reply. The only response that is not JSON is an image, sent with a content type from a fixed
//! list, `nosniff`, no filename and a content security policy that allows nothing at all.

use super::http::{Request, Response};
use super::routes::{
    error_response, fleet_error_response, ok_value, parse_query_limited, query_u64,
};
use super::Shared;
use fleet_domain::error::{ErrorCode, FleetError};
use fleet_domain::repo::limits::{MAX_IMAGE_BYTES, MAX_PATH_BYTES};
use fleet_domain::value::Value;
use std::sync::atomic::Ordering;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RepoRoute {
    Tree,
    File,
    Blob,
    Markdown,
    Diff,
}

impl RepoRoute {
    fn command(self) -> &'static str {
        match self {
            RepoRoute::Tree => "repo.tree",
            RepoRoute::File => "repo.file",
            RepoRoute::Blob => "repo.blob",
            RepoRoute::Markdown => "repo.markdown",
            RepoRoute::Diff => "repo.diff",
        }
    }

    fn parameters(self) -> &'static [&'static str] {
        match self {
            RepoRoute::Diff => &["task", "base", "head", "context"],
            _ => &["task", "commit", "path"],
        }
    }
}

/// The content security policy of an image response: nothing may load, run or embed, and the
/// document is sandboxed even if someone opens it in a tab.
pub(crate) const IMAGE_CSP: &str = "default-src 'none'; sandbox";

/// The content types an image response may have.
const IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// Keeps a read counted while it runs.
struct ReadSlot(Arc<Shared>);

impl Drop for ReadSlot {
    fn drop(&mut self) {
        self.0.repo_active.fetch_sub(1, Ordering::AcqRel);
    }
}

fn claim(shared: &Arc<Shared>) -> Result<ReadSlot, Response> {
    let prior = shared.repo_active.fetch_add(1, Ordering::AcqRel);
    let slot = ReadSlot(Arc::clone(shared));
    if prior >= shared.limits.max_repo_reads {
        // dropping the slot gives the count back
        return Err(error_response(
            503,
            ErrorCode::Limit,
            "too many repository reads are running; try again in a moment",
        )
        .with_header("Retry-After", "1"));
    }
    Ok(slot)
}

/// The arguments of the daemon command for this query string.
fn arguments(route: RepoRoute, query: &str) -> Result<Value, Response> {
    // a path may be up to the path limit; every other value is short
    let q = parse_query_limited(query, route.parameters(), MAX_PATH_BYTES + 16)?;
    let task = q
        .get("task")
        .filter(|t| !t.is_empty() && t.len() <= 128)
        .ok_or_else(|| {
            error_response(
                400,
                ErrorCode::Invalid,
                "`task` is required (run/task, at most 128 bytes)",
            )
        })?;
    let mut args: Vec<(&str, Value)> = vec![("task", Value::str(task))];
    for key in ["commit", "path", "base", "head"] {
        if let Some(v) = q.get(key) {
            args.push((key, Value::str(v)));
        }
    }
    if route == RepoRoute::Diff {
        if let Some(n) = query_u64(&q, "context")? {
            args.push(("context", Value::int(n)));
        }
    }
    Ok(Value::object(args))
}

/// The `subject` the application gives the error "this file is not an image"
/// (`fleet_app::browse::NOT_AN_IMAGE`; an end-to-end test keeps the two in step).
const NOT_AN_IMAGE: &str = "not_an_image";

/// What the backend sent as an image, checked once more here: the type must be one of the four and
/// the size within the limit, whatever the backend says.
fn image_response(content_type: &'static str, bytes: Vec<u8>) -> Response {
    if !IMAGE_TYPES.contains(&content_type) || bytes.len() > MAX_IMAGE_BYTES {
        return error_response(500, ErrorCode::Internal, "the image could not be served");
    }
    Response::new(200, content_type, bytes)
        .with_csp(IMAGE_CSP)
        .with_header("Content-Disposition", "inline")
}

pub(crate) fn handle(
    shared: &Arc<Shared>,
    route: RepoRoute,
    req: &Request,
) -> Result<Response, Response> {
    let args = arguments(route, &req.query)?;
    let _slot = claim(shared)?;
    if route == RepoRoute::Blob {
        return match shared.backend.image(args) {
            Ok(image) => Ok(image_response(image.content_type, image.bytes)),
            // a file that is not an image is "unsupported media"; a malformed request is a 400
            Err(e)
                if e.code == ErrorCode::Invalid && e.subject.as_deref() == Some(NOT_AN_IMAGE) =>
            {
                Err(unsupported(&e))
            }
            Err(e) => Err(fleet_error_response(&e)),
        };
    }
    match shared.backend.call(route.command(), args) {
        Ok(doc) => Ok(ok_value(&doc)),
        Err(e) => Err(fleet_error_response(&e)),
    }
}

fn unsupported(e: &FleetError) -> Response {
    let mut r = fleet_error_response(e);
    r.status = 415;
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(route: RepoRoute, q: &str) -> Result<Value, u16> {
        arguments(route, q).map_err(|r| r.status)
    }

    #[test]
    fn a_query_becomes_the_arguments_of_one_command() {
        let v = args(
            RepoRoute::Tree,
            "task=r1%2Fa&commit=head&path=src%2Fmain.rs",
        )
        .unwrap();
        assert_eq!(v.get("task").and_then(Value::as_str), Some("r1/a"));
        assert_eq!(v.get("commit").and_then(Value::as_str), Some("head"));
        assert_eq!(v.get("path").and_then(Value::as_str), Some("src/main.rs"));
        let d = args(RepoRoute::Diff, "task=a&base=base&head=head&context=7").unwrap();
        assert_eq!(d.get("context").and_then(Value::as_u64), Some(7));
        assert_eq!(d.get("base").and_then(Value::as_str), Some("base"));
        assert!(args(RepoRoute::File, "task=a")
            .unwrap()
            .get("path")
            .is_none());
        assert_eq!(RepoRoute::Blob.command(), "repo.blob");
    }

    #[test]
    fn malformed_queries_are_refused_before_the_daemon_sees_them() {
        for (route, q) in [
            (RepoRoute::Tree, ""),
            (RepoRoute::Tree, "commit=head"),
            (RepoRoute::Tree, "task="),
            (RepoRoute::Tree, "task=a&task=b"),
            (RepoRoute::Tree, "task=a&base=base"),
            (RepoRoute::Tree, "task=a&context=3"),
            (RepoRoute::Tree, "task=a&refs=x"),
            (RepoRoute::Tree, "task=a&path=%zz"),
            (RepoRoute::Tree, "task=a&path=%ff"),
            (RepoRoute::Tree, "task=a&path=a%00b"),
            (RepoRoute::Diff, "task=a&path=x"),
            (RepoRoute::Diff, "task=a&commit=head"),
            (RepoRoute::Diff, "task=a&context=-1"),
            (RepoRoute::Diff, "task=a&context=x"),
            (RepoRoute::Diff, "task=a&context=99999999999999999999999"),
        ] {
            assert_eq!(args(route, q), Err(400), "{route:?} {q}");
        }
        let long_task = format!("task={}", "a".repeat(129));
        assert_eq!(args(RepoRoute::Tree, &long_task), Err(400));
        let long_path = format!("task=a&path={}", "a".repeat(MAX_PATH_BYTES + 17));
        assert_eq!(args(RepoRoute::Tree, &long_path), Err(400));
        // a path at the limit is passed on (the domain validates it)
        let at_limit = format!("task=a&path={}", "a".repeat(MAX_PATH_BYTES));
        assert!(args(RepoRoute::Tree, &at_limit).is_ok());
    }

    #[test]
    fn an_image_response_has_a_fixed_type_nosniff_a_closed_policy_and_no_filename() {
        let r = image_response("image/png", vec![1, 2, 3]);
        assert_eq!(r.status, 200);
        let mut wire = Vec::new();
        r.write_to(&mut wire).unwrap();
        let text = String::from_utf8_lossy(&wire).to_string();
        assert!(text.contains("Content-Type: image/png\r\n"), "{text}");
        assert!(text.contains("X-Content-Type-Options: nosniff\r\n"));
        assert!(text.contains("Content-Security-Policy: default-src 'none'; sandbox\r\n"));
        assert_eq!(text.matches("Content-Security-Policy:").count(), 1);
        assert!(text.contains("Content-Disposition: inline\r\n"));
        assert!(!text.to_ascii_lowercase().contains("filename"));
        assert!(text.contains("Cache-Control: no-store\r\n"));
        assert!(text.contains("Cross-Origin-Resource-Policy: same-origin\r\n"));
        // whatever the backend claims, only the four types and a bounded size go out
        for bad in [
            "image/svg+xml",
            "text/html",
            "application/octet-stream",
            "image/png; x=y",
            "",
        ] {
            assert_eq!(image_response(bad, vec![1]).status, 500, "{bad}");
        }
        assert_eq!(
            image_response("image/gif", vec![0; MAX_IMAGE_BYTES + 1]).status,
            500
        );
        assert_eq!(
            image_response("image/webp", vec![0; MAX_IMAGE_BYTES]).status,
            200
        );
    }
}
