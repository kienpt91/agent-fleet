//! Reading what a forge answered. The answers are JSON documents with dozens of fields that fleet
//! does not use; the ones it uses are required with their type, bounded in size and cleaned of
//! terminal control, invisible and secret-looking text before anything else sees them. A field
//! that is missing or has the wrong type makes the whole answer an error: fleet never works with
//! half a record.

use crate::limits::{
    MAX_BODY, MAX_CHECKS, MAX_ERROR_MESSAGE, MAX_LABELS, MAX_NUMBER, MAX_PAGE_ITEMS, MAX_REVIEWS,
    MAX_SHORT, MAX_TITLE, MAX_URL,
};
use crate::model::{
    CheckState, Checks, CreatedPull, Issue, IssueState, IssueSummary, Page, Pull, PullState,
    PullStatus, PullSummary, Review, ReviewState,
};
use fleet_domain::error::{FleetError, Result};
use fleet_domain::ids::Sha;
use fleet_domain::sanitize::{flatten_line, head_str};
use fleet_domain::value::Value;
use std::collections::BTreeMap;

type Map = BTreeMap<String, Value>;

/// Reads answers. `clean` turns raw forge text into text that is safe to store and show: the
/// adapter passes its sanitizer and redactor, so no secret or escape sequence in an answer reaches
/// the event log, a brief or a screen.
pub struct Reader<'a> {
    clean: &'a dyn Fn(&str) -> String,
}

fn bad(what: &str) -> FleetError {
    FleetError::invalid(format!(
        "the answer of the forge is not what fleet expects ({what})"
    ))
}

fn object<'v>(v: &'v Value, what: &str) -> Result<&'v Map> {
    v.as_object()
        .ok_or_else(|| bad(&format!("{what} is not an object")))
}

fn array<'v>(v: &'v Value, what: &str) -> Result<&'v [Value]> {
    v.as_array()
        .ok_or_else(|| bad(&format!("{what} is not a list")))
}

fn req_str<'v>(m: &'v Map, key: &str) -> Result<&'v str> {
    m.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| bad(&format!("`{key}` is missing or not text")))
}

fn opt_str<'v>(m: &'v Map, key: &str) -> Result<Option<&'v str>> {
    match m.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_str()
            .map(Some)
            .ok_or_else(|| bad(&format!("`{key}` is not text"))),
    }
}

fn req_number(m: &Map, key: &str) -> Result<u64> {
    m.get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| bad(&format!("`{key}` is missing or not a number")))
}

fn opt_count(m: &Map, key: &str) -> Result<u64> {
    match m.get(key) {
        None | Some(Value::Null) => Ok(0),
        Some(v) => v
            .as_u64()
            .ok_or_else(|| bad(&format!("`{key}` is not a count"))),
    }
}

fn opt_bool(m: &Map, key: &str) -> Result<Option<bool>> {
    match m.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_bool()
            .map(Some)
            .ok_or_else(|| bad(&format!("`{key}` is not true or false"))),
    }
}

fn item_number(m: &Map) -> Result<u64> {
    let n = req_number(m, "number")?;
    if n == 0 || n > MAX_NUMBER {
        return Err(bad("`number` is out of range"));
    }
    Ok(n)
}

impl<'a> Reader<'a> {
    pub fn new(clean: &'a dyn Fn(&str) -> String) -> Reader<'a> {
        Reader { clean }
    }

    /// One line of at most `max` bytes: cleaned, then flattened.
    fn line(&self, raw: &str, max: usize) -> String {
        flatten_line(&(self.clean)(head_str(raw, max.saturating_mul(4))), max)
    }

    /// Several lines, cleaned and cut at `MAX_BODY`; says whether it was cut.
    fn block(&self, raw: &str) -> (String, bool) {
        let cleaned = (self.clean)(head_str(raw, MAX_BODY.saturating_mul(4)));
        let cut = raw.len() > MAX_BODY || cleaned.len() > MAX_BODY;
        (head_str(&cleaned, MAX_BODY).to_string(), cut)
    }

    fn author(&self, m: &Map) -> Result<String> {
        match m.get("user") {
            None | Some(Value::Null) => Ok("ghost".to_string()),
            Some(u) => Ok(self.line(req_str(object(u, "`user`")?, "login")?, MAX_SHORT)),
        }
    }

    fn labels(&self, m: &Map) -> Result<Vec<String>> {
        let Some(v) = m.get("labels") else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for item in array(v, "`labels`")?.iter().take(MAX_LABELS) {
            let name = match item {
                Value::Str(s) => s.as_str(),
                other => req_str(object(other, "a label")?, "name")?,
            };
            out.push(self.line(name, MAX_SHORT));
        }
        Ok(out)
    }

    fn url(&self, m: &Map) -> Result<String> {
        Ok(self.line(req_str(m, "html_url")?, MAX_URL))
    }

    fn updated(&self, m: &Map) -> Result<String> {
        Ok(self.line(opt_str(m, "updated_at")?.unwrap_or(""), 40))
    }

    fn issue_summary(&self, m: &Map) -> Result<IssueSummary> {
        let state = IssueState::parse(req_str(m, "state")?)
            .ok_or_else(|| bad("`state` is neither open nor closed"))?;
        Ok(IssueSummary {
            number: item_number(m)?,
            title: self.line(req_str(m, "title")?, MAX_TITLE),
            state,
            labels: self.labels(m)?,
            author: self.author(m)?,
            comments: opt_count(m, "comments")?,
            updated: self.updated(m)?,
            url: self.url(m)?,
        })
    }

    /// A page of issues. Pull requests, which the issues endpoint lists too, are left out.
    pub fn issue_page(&self, v: &Value, page: u64, has_more: bool) -> Result<Page<IssueSummary>> {
        let items = array(v, "the list of issues")?;
        if items.len() > MAX_PAGE_ITEMS * 2 {
            return Err(bad("the page has more items than were asked for"));
        }
        let mut out = Vec::new();
        for item in items {
            let m = object(item, "an issue")?;
            if m.contains_key("pull_request") {
                continue;
            }
            out.push(self.issue_summary(m)?);
        }
        Ok(Page {
            items: out,
            page,
            has_more,
        })
    }

    /// One issue. A pull request answered by the issues endpoint is refused: it has its own
    /// command, and treating it as an issue would hide what it is.
    pub fn issue(&self, v: &Value) -> Result<Issue> {
        let m = object(v, "the issue")?;
        if m.contains_key("pull_request") {
            return Err(FleetError::invalid(
                "that number is a pull request, not an issue; use `fleet pr show`",
            ));
        }
        let (body, body_cut) = self.block(opt_str(m, "body")?.unwrap_or(""));
        Ok(Issue {
            summary: self.issue_summary(m)?,
            body,
            body_cut,
        })
    }

    fn pull_summary(&self, m: &Map) -> Result<PullSummary> {
        let head = object(
            m.get("head").ok_or_else(|| bad("`head` is missing"))?,
            "`head`",
        )?;
        let base = object(
            m.get("base").ok_or_else(|| bad("`base` is missing"))?,
            "`base`",
        )?;
        let merged = match opt_bool(m, "merged")? {
            Some(b) => b,
            None => !matches!(m.get("merged_at"), None | Some(Value::Null)),
        };
        let state = match (req_str(m, "state")?, merged) {
            ("open", _) => PullState::Open,
            ("closed", true) => PullState::Merged,
            ("closed", false) => PullState::Closed,
            _ => return Err(bad("`state` is neither open nor closed")),
        };
        let head_sha =
            Sha::parse(req_str(head, "sha")?).map_err(|_| bad("`head.sha` is not a commit id"))?;
        Ok(PullSummary {
            number: item_number(m)?,
            title: self.line(req_str(m, "title")?, MAX_TITLE),
            state,
            draft: opt_bool(m, "draft")?.unwrap_or(false),
            labels: self.labels(m)?,
            author: self.author(m)?,
            head_branch: self.line(req_str(head, "ref")?, MAX_SHORT),
            base_branch: self.line(req_str(base, "ref")?, MAX_SHORT),
            head_sha,
            comments: opt_count(m, "comments")?.saturating_add(opt_count(m, "review_comments")?),
            updated: self.updated(m)?,
            url: self.url(m)?,
        })
    }

    pub fn pull_page(&self, v: &Value, page: u64, has_more: bool) -> Result<Page<PullSummary>> {
        let items = array(v, "the list of pull requests")?;
        if items.len() > MAX_PAGE_ITEMS * 2 {
            return Err(bad("the page has more items than were asked for"));
        }
        let out = items
            .iter()
            .map(|i| self.pull_summary(object(i, "a pull request")?))
            .collect::<Result<Vec<_>>>()?;
        Ok(Page {
            items: out,
            page,
            has_more,
        })
    }

    pub fn pull(&self, v: &Value) -> Result<Pull> {
        let m = object(v, "the pull request")?;
        let (body, body_cut) = self.block(opt_str(m, "body")?.unwrap_or(""));
        Ok(Pull {
            summary: self.pull_summary(m)?,
            body,
            body_cut,
        })
    }

    /// The checks of a commit from the answer of the check-runs endpoint and, when there is
    /// one, the combined commit status (older tools report through that).
    pub fn checks(&self, runs: &Value, status: Option<&Value>) -> Result<Checks> {
        let m = object(runs, "the check runs")?;
        let list = array(
            m.get("check_runs")
                .ok_or_else(|| bad("`check_runs` is missing"))?,
            "`check_runs`",
        )?;
        let (mut passed, mut failed, mut pending) = (0u64, 0u64, 0u64);
        for run in list.iter().take(MAX_CHECKS) {
            let r = object(run, "a check run")?;
            match (req_str(r, "status")?, opt_str(r, "conclusion")?) {
                ("completed", Some("success" | "neutral" | "skipped")) => passed += 1,
                (
                    "completed",
                    Some(
                        "failure" | "cancelled" | "timed_out" | "action_required"
                        | "startup_failure" | "stale",
                    ),
                ) => failed += 1,
                _ => pending += 1,
            }
        }
        if let Some(s) = status {
            let s = object(s, "the commit status")?;
            if opt_count(s, "total_count")? > 0 {
                match req_str(s, "state")? {
                    "success" => passed += 1,
                    "failure" | "error" => failed += 1,
                    _ => pending += 1,
                }
            }
        }
        let total = passed + failed + pending;
        let state = if failed > 0 {
            CheckState::Failing
        } else if pending > 0 {
            CheckState::Pending
        } else if total > 0 {
            CheckState::Passing
        } else {
            CheckState::None
        };
        Ok(Checks {
            state,
            total,
            passed,
            failed,
            pending,
        })
    }

    /// The standing review of each reviewer, oldest reviewer first: the latest review that is not
    /// a plain comment decides, a comment only counts for someone who has said nothing else.
    pub fn reviews(&self, v: &Value) -> Result<Vec<Review>> {
        let mut order: Vec<String> = Vec::new();
        let mut standing: BTreeMap<String, ReviewState> = BTreeMap::new();
        for item in array(v, "the reviews")?.iter().take(MAX_REVIEWS) {
            let m = object(item, "a review")?;
            let author = self.author(m)?;
            let state = match req_str(m, "state")? {
                "APPROVED" => ReviewState::Approved,
                "CHANGES_REQUESTED" => ReviewState::ChangesRequested,
                "COMMENTED" => ReviewState::Commented,
                "DISMISSED" => ReviewState::Dismissed,
                "PENDING" => continue,
                _ => return Err(bad("`state` of a review is not known")),
            };
            if !standing.contains_key(&author) {
                order.push(author.clone());
            }
            let keep = state != ReviewState::Commented || !standing.contains_key(&author);
            if keep {
                standing.insert(author, state);
            }
        }
        Ok(order
            .into_iter()
            .filter_map(|a| standing.remove(&a).map(|state| Review { author: a, state }))
            .collect())
    }

    pub fn pull_status(
        &self,
        pull: &Value,
        runs: &Value,
        status: Option<&Value>,
        reviews: &Value,
    ) -> Result<PullStatus> {
        Ok(PullStatus {
            pull: self.pull_summary(object(pull, "the pull request")?)?,
            checks: self.checks(runs, status)?,
            reviews: self.reviews(reviews)?,
        })
    }

    /// The answer to creating a pull request.
    pub fn created_pull(&self, v: &Value) -> Result<CreatedPull> {
        let m = object(v, "the created pull request")?;
        Ok(CreatedPull {
            number: item_number(m)?,
            url: self.url(m)?,
        })
    }

    /// The `message` of an error answer, or an empty text when there is none.
    pub fn error_message(&self, v: &Value) -> String {
        v.get("message")
            .and_then(Value::as_str)
            .map(|s| self.line(s, MAX_ERROR_MESSAGE))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_domain::sanitize::sanitize_text;

    fn clean(s: &str) -> String {
        sanitize_text(s.as_bytes())
    }

    fn reader() -> Reader<'static> {
        Reader::new(&clean)
    }

    fn obj(pairs: Vec<(&str, Value)>) -> Value {
        Value::object(pairs)
    }

    fn s(t: &str) -> Value {
        Value::str(t)
    }

    fn issue_json(number: u64) -> Value {
        obj(vec![
            ("number", Value::int(number)),
            ("title", s("Crash on start")),
            ("state", s("open")),
            (
                "labels",
                Value::array([obj(vec![("name", s("bug"))]), s("p1")]),
            ),
            ("user", obj(vec![("login", s("octocat"))])),
            ("comments", Value::int(3)),
            ("updated_at", s("2026-10-30T10:00:00Z")),
            ("html_url", s("https://github.com/o/r/issues/7")),
            ("body", s("Steps:\n1. run it")),
            ("unknown_key", Value::int(1)),
        ])
    }

    fn pull_json(number: u64, sha: &str) -> Value {
        obj(vec![
            ("number", Value::int(number)),
            ("title", s("Fix it")),
            ("state", s("open")),
            ("draft", Value::Bool(true)),
            ("labels", Value::array([])),
            ("user", obj(vec![("login", s("octocat"))])),
            ("head", obj(vec![("ref", s("fleet/r1/a")), ("sha", s(sha))])),
            ("base", obj(vec![("ref", s("main"))])),
            ("comments", Value::int(2)),
            ("review_comments", Value::int(1)),
            ("updated_at", s("2026-10-30T10:00:00Z")),
            ("html_url", s("https://github.com/o/r/pull/9")),
            ("body", Value::Null),
        ])
    }

    #[test]
    fn an_issue_is_read_with_its_fields() {
        let i = reader().issue(&issue_json(7)).unwrap();
        assert_eq!(i.summary.number, 7);
        assert_eq!(i.summary.title, "Crash on start");
        assert_eq!(i.summary.state, IssueState::Open);
        assert_eq!(i.summary.labels, ["bug", "p1"]);
        assert_eq!(i.summary.author, "octocat");
        assert_eq!(i.summary.comments, 3);
        assert_eq!(i.body, "Steps:\n1. run it");
        assert!(!i.body_cut);
        let v = i.to_value();
        assert_eq!(v.get("number").and_then(Value::as_u64), Some(7));
        assert_eq!(
            v.get("body").and_then(Value::as_str),
            Some("Steps:\n1. run it")
        );
    }

    #[test]
    fn an_issue_list_leaves_pull_requests_out() {
        let mut pr_as_issue = issue_json(8);
        if let Value::Object(m) = &mut pr_as_issue {
            m.insert("pull_request".into(), obj(vec![]));
        }
        let page = reader()
            .issue_page(&Value::array([issue_json(7), pr_as_issue.clone()]), 2, true)
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!((page.page, page.has_more), (2, true));
        assert!(reader()
            .issue(&pr_as_issue)
            .unwrap_err()
            .message
            .contains("pull request"));
    }

    #[test]
    fn missing_or_mistyped_fields_make_the_answer_an_error() {
        let drop = |key: &str| {
            let mut v = issue_json(7);
            if let Value::Object(m) = &mut v {
                m.remove(key);
            }
            v
        };
        for key in ["number", "title", "state", "html_url"] {
            assert!(reader().issue(&drop(key)).is_err(), "{key}");
        }
        let set = |key: &str, val: Value| {
            let mut v = issue_json(7);
            if let Value::Object(m) = &mut v {
                m.insert(key.into(), val);
            }
            v
        };
        for (key, val) in [
            ("number", s("7")),
            ("number", Value::int(0)),
            ("number", Value::int(-1)),
            ("number", Value::int(u64::MAX)),
            ("title", Value::int(1)),
            ("state", s("merged")),
            ("state", s("OPEN")),
            ("labels", s("bug")),
            ("labels", Value::array([Value::int(1)])),
            ("user", s("octocat")),
            ("user", obj(vec![("login", Value::int(1))])),
            ("comments", s("3")),
            ("comments", Value::Float(1.5)),
            ("body", Value::int(1)),
            ("updated_at", Value::int(1)),
            ("html_url", Value::Null),
        ] {
            assert!(
                reader().issue(&set(key, val.clone())).is_err(),
                "{key} {val:?}"
            );
        }
        assert!(reader().issue(&Value::array([])).is_err());
        assert!(reader().issue(&Value::Null).is_err());
        assert!(reader().issue_page(&obj(vec![]), 1, false).is_err());
        assert!(reader()
            .issue_page(&Value::array([Value::int(1)]), 1, false)
            .is_err());
        // a deleted account has no user
        let ghost = reader().issue(&set("user", Value::Null)).unwrap();
        assert_eq!(ghost.summary.author, "ghost");
    }

    #[test]
    fn text_is_cleaned_flattened_and_capped() {
        let mut v = issue_json(7);
        if let Value::Object(m) = &mut v {
            m.insert(
                "title".into(),
                s("a\u{1b}[31mred\u{1b}]0;title\u{7}\nnext\u{202e}line"),
            );
            m.insert(
                "body".into(),
                s(&format!("{}{}", "x".repeat(MAX_BODY), "y".repeat(500))),
            );
            m.insert(
                "labels".into(),
                Value::array((0..100).map(|i| s(&format!("l{i}")))),
            );
            m.insert("user".into(), obj(vec![("login", s(&"u".repeat(500)))]));
        }
        let i = reader().issue(&v).unwrap();
        assert!(
            !i.summary
                .title
                .contains(['\u{1b}', '\u{7}', '\n', '\u{202e}']),
            "{:?}",
            i.summary.title
        );
        assert!(i.body.len() <= MAX_BODY && i.body_cut);
        assert_eq!(i.summary.labels.len(), MAX_LABELS);
        assert!(i.summary.author.len() <= MAX_SHORT + 4);
        let long = "t".repeat(3000);
        if let Value::Object(m) = &mut v {
            m.insert("title".into(), s(&long));
        }
        assert!(reader().issue(&v).unwrap().summary.title.len() <= MAX_TITLE + 4);
    }

    #[test]
    fn the_cleaner_of_the_adapter_is_applied_to_every_text() {
        let redact = |t: &str| clean(t).replace("SECRETWORD", "[x]");
        let r = Reader::new(&redact);
        let mut v = issue_json(7);
        if let Value::Object(m) = &mut v {
            m.insert("title".into(), s("SECRETWORD title"));
            m.insert("body".into(), s("body SECRETWORD"));
            m.insert("labels".into(), Value::array([s("SECRETWORD")]));
            m.insert("user".into(), obj(vec![("login", s("SECRETWORD"))]));
        }
        let i = r.issue(&v).unwrap();
        assert!(!format!("{i:?}").contains("SECRETWORD"), "{i:?}");
    }

    #[test]
    fn a_pull_request_is_read_and_its_state_derived() {
        let sha = "a".repeat(40);
        let p = reader().pull(&pull_json(9, &sha)).unwrap();
        assert_eq!(p.summary.number, 9);
        assert_eq!(p.summary.state, PullState::Open);
        assert!(p.summary.draft);
        assert_eq!(p.summary.head_branch, "fleet/r1/a");
        assert_eq!(p.summary.base_branch, "main");
        assert_eq!(p.summary.head_sha.as_str(), sha);
        assert_eq!(p.summary.comments, 3);
        assert_eq!(p.body, "");
        let mut closed = pull_json(9, &sha);
        if let Value::Object(m) = &mut closed {
            m.insert("state".into(), s("closed"));
        }
        assert_eq!(
            reader().pull(&closed).unwrap().summary.state,
            PullState::Closed
        );
        if let Value::Object(m) = &mut closed {
            m.insert("merged_at".into(), s("2026-10-30T11:00:00Z"));
        }
        assert_eq!(
            reader().pull(&closed).unwrap().summary.state,
            PullState::Merged
        );
        if let Value::Object(m) = &mut closed {
            m.insert("merged".into(), Value::Bool(false));
        }
        assert_eq!(
            reader().pull(&closed).unwrap().summary.state,
            PullState::Closed,
            "`merged` wins"
        );
    }

    #[test]
    fn a_pull_request_with_a_bad_commit_or_branch_is_refused() {
        for sha in ["", "xyz", "--upload-pack=x", &"A".repeat(40)] {
            assert!(reader().pull(&pull_json(9, sha)).is_err(), "{sha}");
        }
        let mut v = pull_json(9, &"a".repeat(40));
        if let Value::Object(m) = &mut v {
            m.remove("head");
        }
        assert!(reader().pull(&v).is_err());
        let page = reader()
            .pull_page(&Value::array([pull_json(1, &"b".repeat(40))]), 1, false)
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert!(page.to_value(PullSummary::to_value).get("items").is_some());
    }

    fn runs(items: &[(&str, Option<&str>)]) -> Value {
        obj(vec![
            ("total_count", Value::int(items.len() as u64)),
            (
                "check_runs",
                Value::array(items.iter().map(|(st, c)| {
                    obj(vec![
                        ("name", s("job")),
                        ("status", s(st)),
                        ("conclusion", c.map_or(Value::Null, s)),
                    ])
                })),
            ),
        ])
    }

    #[test]
    fn checks_add_up() {
        let r = reader();
        let none = r.checks(&runs(&[]), None).unwrap();
        assert_eq!((none.state, none.total), (CheckState::None, 0));
        let ok = r
            .checks(
                &runs(&[
                    ("completed", Some("success")),
                    ("completed", Some("skipped")),
                ]),
                None,
            )
            .unwrap();
        assert_eq!((ok.state, ok.passed), (CheckState::Passing, 2));
        let pending = r
            .checks(
                &runs(&[
                    ("completed", Some("success")),
                    ("in_progress", None),
                    ("queued", None),
                ]),
                None,
            )
            .unwrap();
        assert_eq!((pending.state, pending.pending), (CheckState::Pending, 2));
        let failing = r
            .checks(
                &runs(&[("completed", Some("failure")), ("in_progress", None)]),
                None,
            )
            .unwrap();
        assert_eq!((failing.state, failing.failed), (CheckState::Failing, 1));
        for c in [
            "cancelled",
            "timed_out",
            "action_required",
            "startup_failure",
            "stale",
        ] {
            assert_eq!(
                r.checks(&runs(&[("completed", Some(c))]), None)
                    .unwrap()
                    .state,
                CheckState::Failing,
                "{c}"
            );
        }
        // the combined status counts once, and not at all when it holds no statuses
        let st = |state: &str, total: u64| {
            obj(vec![
                ("state", s(state)),
                ("total_count", Value::int(total)),
            ])
        };
        assert_eq!(
            r.checks(&runs(&[]), Some(&st("success", 0))).unwrap().state,
            CheckState::None
        );
        assert_eq!(
            r.checks(&runs(&[]), Some(&st("success", 2))).unwrap().state,
            CheckState::Passing
        );
        assert_eq!(
            r.checks(
                &runs(&[("completed", Some("success"))]),
                Some(&st("failure", 1))
            )
            .unwrap()
            .state,
            CheckState::Failing
        );
        assert_eq!(
            r.checks(
                &runs(&[("completed", Some("success"))]),
                Some(&st("pending", 1))
            )
            .unwrap()
            .state,
            CheckState::Pending
        );
        assert!(r.checks(&obj(vec![]), None).is_err());
        assert!(r
            .checks(
                &runs(&[("completed", Some("success"))]),
                Some(&Value::int(1))
            )
            .is_err());
    }

    fn review(login: &str, state: &str) -> Value {
        obj(vec![
            ("user", obj(vec![("login", s(login))])),
            ("state", s(state)),
        ])
    }

    #[test]
    fn the_standing_review_of_each_reviewer_counts() {
        let r = reader();
        let v = Value::array([
            review("ann", "CHANGES_REQUESTED"),
            review("bob", "COMMENTED"),
            review("ann", "COMMENTED"),
            review("bob", "APPROVED"),
            review("cy", "PENDING"),
            review("dee", "APPROVED"),
            review("dee", "DISMISSED"),
        ]);
        let got = r.reviews(&v).unwrap();
        let text: Vec<(String, ReviewState)> =
            got.iter().map(|x| (x.author.clone(), x.state)).collect();
        assert_eq!(
            text,
            [
                ("ann".to_string(), ReviewState::ChangesRequested),
                ("bob".to_string(), ReviewState::Approved),
                ("dee".to_string(), ReviewState::Dismissed),
            ]
        );
        assert!(r.reviews(&Value::array([review("x", "WHATEVER")])).is_err());
        assert!(r.reviews(&obj(vec![])).is_err());
    }

    #[test]
    fn a_status_document_carries_the_decision() {
        let r = reader();
        let st = r
            .pull_status(
                &pull_json(9, &"c".repeat(40)),
                &runs(&[("completed", Some("success"))]),
                None,
                &Value::array([
                    review("ann", "APPROVED"),
                    review("bob", "CHANGES_REQUESTED"),
                ]),
            )
            .unwrap();
        assert_eq!(
            st.decision(),
            crate::model::ReviewDecision::ChangesRequested
        );
        let v = st.to_value();
        assert_eq!(
            v.get("decision").and_then(Value::as_str),
            Some("changes_requested")
        );
        assert_eq!(
            v.get("checks")
                .and_then(|c| c.get("state"))
                .and_then(Value::as_str),
            Some("passing")
        );
        let approved = r
            .pull_status(
                &pull_json(9, &"c".repeat(40)),
                &runs(&[]),
                None,
                &Value::array([review("ann", "APPROVED")]),
            )
            .unwrap();
        assert_eq!(approved.decision(), crate::model::ReviewDecision::Approved);
        let none = r
            .pull_status(
                &pull_json(9, &"c".repeat(40)),
                &runs(&[]),
                None,
                &Value::array([]),
            )
            .unwrap();
        assert_eq!(none.decision(), crate::model::ReviewDecision::None);
    }

    #[test]
    fn created_pulls_and_error_messages() {
        let r = reader();
        let created = r
            .created_pull(&obj(vec![
                ("number", Value::int(12)),
                ("html_url", s("https://github.com/o/r/pull/12")),
            ]))
            .unwrap();
        assert_eq!(created.number, 12);
        assert!(r
            .created_pull(&obj(vec![("number", Value::int(12))]))
            .is_err());
        assert_eq!(
            r.error_message(&obj(vec![("message", s("Validation\nFailed"))])),
            "Validation Failed"
        );
        assert_eq!(r.error_message(&Value::Null), "");
        assert_eq!(r.error_message(&obj(vec![("message", Value::int(1))])), "");
    }
}
