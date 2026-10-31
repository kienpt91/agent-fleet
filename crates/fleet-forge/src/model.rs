//! What a forge tells us about issues and pull requests, as plain values. Every text in them was
//! read through [`crate::schema`], which cleaned and capped it; none of it is trusted.

use fleet_domain::ids::Sha;
use fleet_domain::value::Value;

macro_rules! word_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            pub fn as_str(self) -> &'static str {
                match self {
                    $($name::$variant => $text),+
                }
            }

            pub fn parse(s: &str) -> Option<$name> {
                match s {
                    $($text => Some($name::$variant),)+
                    _ => None,
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

word_enum!(
    /// Which items a list asks for.
    StateFilter {
        Open => "open",
        Closed => "closed",
        All => "all",
    }
);

word_enum!(
    IssueState {
        Open => "open",
        Closed => "closed",
    }
);

word_enum!(
    PullState {
        Open => "open",
        Closed => "closed",
        Merged => "merged",
    }
);

word_enum!(
    /// The combined state of the checks of a commit.
    CheckState {
        None => "none",
        Pending => "pending",
        Passing => "passing",
        Failing => "failing",
    }
);

word_enum!(
    ReviewState {
        Approved => "approved",
        ChangesRequested => "changes_requested",
        Commented => "commented",
        Dismissed => "dismissed",
    }
);

word_enum!(
    /// What the reviews of a pull request add up to.
    ReviewDecision {
        None => "none",
        Approved => "approved",
        ChangesRequested => "changes_requested",
    }
);

/// One page of a list, with whether the forge says there is more.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub page: u64,
    pub has_more: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssueSummary {
    pub number: u64,
    pub title: String,
    pub state: IssueState,
    pub labels: Vec<String>,
    pub author: String,
    pub comments: u64,
    pub updated: String,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issue {
    pub summary: IssueSummary,
    pub body: String,
    /// The body was longer than what is kept.
    pub body_cut: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullSummary {
    pub number: u64,
    pub title: String,
    pub state: PullState,
    pub draft: bool,
    pub labels: Vec<String>,
    pub author: String,
    pub head_branch: String,
    pub base_branch: String,
    pub head_sha: Sha,
    pub comments: u64,
    pub updated: String,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pull {
    pub summary: PullSummary,
    pub body: String,
    pub body_cut: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Checks {
    pub state: CheckState,
    pub total: u64,
    pub passed: u64,
    pub failed: u64,
    pub pending: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Review {
    pub author: String,
    pub state: ReviewState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullStatus {
    pub pull: PullSummary,
    pub checks: Checks,
    pub reviews: Vec<Review>,
}

impl PullStatus {
    pub fn decision(&self) -> ReviewDecision {
        if self
            .reviews
            .iter()
            .any(|r| r.state == ReviewState::ChangesRequested)
        {
            ReviewDecision::ChangesRequested
        } else if self
            .reviews
            .iter()
            .any(|r| r.state == ReviewState::Approved)
        {
            ReviewDecision::Approved
        } else {
            ReviewDecision::None
        }
    }
}

/// A pull request the forge created for us.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreatedPull {
    pub number: u64,
    pub url: String,
}

fn strings(items: &[String]) -> Value {
    Value::array(items.iter().map(|s| Value::str(s.as_str())))
}

impl IssueSummary {
    pub fn to_value(&self) -> Value {
        Value::object([
            ("number", Value::int(self.number)),
            ("title", Value::str(self.title.as_str())),
            ("state", Value::str(self.state.as_str())),
            ("labels", strings(&self.labels)),
            ("author", Value::str(self.author.as_str())),
            ("comments", Value::int(self.comments)),
            ("updated", Value::str(self.updated.as_str())),
            ("url", Value::str(self.url.as_str())),
        ])
    }
}

impl Issue {
    pub fn to_value(&self) -> Value {
        let mut v = self.summary.to_value();
        if let Value::Object(m) = &mut v {
            m.insert("body".to_string(), Value::str(self.body.as_str()));
            m.insert("body_cut".to_string(), Value::Bool(self.body_cut));
        }
        v
    }
}

impl PullSummary {
    pub fn to_value(&self) -> Value {
        Value::object([
            ("number", Value::int(self.number)),
            ("title", Value::str(self.title.as_str())),
            ("state", Value::str(self.state.as_str())),
            ("draft", Value::Bool(self.draft)),
            ("labels", strings(&self.labels)),
            ("author", Value::str(self.author.as_str())),
            ("head_branch", Value::str(self.head_branch.as_str())),
            ("base_branch", Value::str(self.base_branch.as_str())),
            ("head_sha", Value::str(self.head_sha.as_str())),
            ("comments", Value::int(self.comments)),
            ("updated", Value::str(self.updated.as_str())),
            ("url", Value::str(self.url.as_str())),
        ])
    }
}

impl Pull {
    pub fn to_value(&self) -> Value {
        let mut v = self.summary.to_value();
        if let Value::Object(m) = &mut v {
            m.insert("body".to_string(), Value::str(self.body.as_str()));
            m.insert("body_cut".to_string(), Value::Bool(self.body_cut));
        }
        v
    }
}

impl Checks {
    pub fn to_value(&self) -> Value {
        Value::object([
            ("state", Value::str(self.state.as_str())),
            ("total", Value::int(self.total)),
            ("passed", Value::int(self.passed)),
            ("failed", Value::int(self.failed)),
            ("pending", Value::int(self.pending)),
        ])
    }
}

impl PullStatus {
    pub fn to_value(&self) -> Value {
        Value::object([
            ("pull", self.pull.to_value()),
            ("checks", self.checks.to_value()),
            (
                "reviews",
                Value::array(self.reviews.iter().map(|r| {
                    Value::object([
                        ("author", Value::str(r.author.as_str())),
                        ("state", Value::str(r.state.as_str())),
                    ])
                })),
            ),
            ("decision", Value::str(self.decision().as_str())),
        ])
    }
}

impl<T> Page<T> {
    pub fn to_value(&self, item: impl Fn(&T) -> Value) -> Value {
        Value::object([
            ("page", Value::int(self.page)),
            ("has_more", Value::Bool(self.has_more)),
            ("items", Value::array(self.items.iter().map(item))),
        ])
    }
}
