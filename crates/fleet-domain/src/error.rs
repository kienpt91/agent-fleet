//! Error type shared by every module, with stable machine-readable codes and CLI exit codes.

use std::fmt;

/// Stable error classes. The string form appears in `--json` output and on the control socket;
/// the numeric form is the process exit code of the `fleet` binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// Unexpected internal, I/O or git failure.
    Internal,
    /// Bad command line.
    Usage,
    /// The daemon is not running or its socket is unusable.
    DaemonDown,
    /// A run, task, gate, approval or session does not exist.
    NotFound,
    /// A human decision is required before the action may proceed.
    ApprovalRequired,
    /// The action conflicts with the current state (wrong status, merge conflict, ...).
    Conflict,
    /// Input failed validation (bad name, cycle, oversized text, ...).
    Invalid,
    /// A configured hard limit was reached.
    Limit,
    /// An optional feature or a dependency of it is not available.
    Unavailable,
    /// The caller is not allowed to do this (wrong peer, worker-origin request).
    Forbidden,
    /// The event log failed its integrity check.
    Tampered,
    /// A wait ran out of time.
    Timeout,
    /// `fleet doctor` found a failing check.
    Unhealthy,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::Internal => "internal",
            ErrorCode::Usage => "usage",
            ErrorCode::DaemonDown => "daemon_down",
            ErrorCode::NotFound => "not_found",
            ErrorCode::ApprovalRequired => "approval_required",
            ErrorCode::Conflict => "conflict",
            ErrorCode::Invalid => "invalid",
            ErrorCode::Limit => "limit",
            ErrorCode::Unavailable => "unavailable",
            ErrorCode::Forbidden => "forbidden",
            ErrorCode::Tampered => "tampered",
            ErrorCode::Timeout => "timeout",
            ErrorCode::Unhealthy => "unhealthy",
        }
    }

    /// Process exit code. `0` is success and is not represented here.
    pub fn exit_code(self) -> i32 {
        match self {
            ErrorCode::Internal => 1,
            ErrorCode::Usage => 2,
            ErrorCode::DaemonDown => 3,
            ErrorCode::NotFound => 4,
            ErrorCode::ApprovalRequired => 5,
            ErrorCode::Conflict => 6,
            ErrorCode::Invalid => 7,
            ErrorCode::Limit => 8,
            ErrorCode::Unavailable => 9,
            ErrorCode::Forbidden => 10,
            ErrorCode::Tampered => 11,
            ErrorCode::Timeout => 12,
            ErrorCode::Unhealthy => 13,
        }
    }

    pub fn parse(s: &str) -> Option<ErrorCode> {
        ALL_CODES.iter().copied().find(|c| c.as_str() == s)
    }
}

pub const ALL_CODES: [ErrorCode; 13] = [
    ErrorCode::Internal,
    ErrorCode::Usage,
    ErrorCode::DaemonDown,
    ErrorCode::NotFound,
    ErrorCode::ApprovalRequired,
    ErrorCode::Conflict,
    ErrorCode::Invalid,
    ErrorCode::Limit,
    ErrorCode::Unavailable,
    ErrorCode::Forbidden,
    ErrorCode::Tampered,
    ErrorCode::Timeout,
    ErrorCode::Unhealthy,
];

#[derive(Debug, Clone)]
pub struct FleetError {
    pub code: ErrorCode,
    pub message: String,
    /// Optional id the caller can act on (for example the approval that is required).
    pub subject: Option<String>,
}

pub type Result<T, E = FleetError> = std::result::Result<T, E>;

impl FleetError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        FleetError {
            code,
            message: message.into(),
            subject: None,
        }
    }

    pub fn with_subject(mut self, subject: impl Into<String>) -> Self {
        self.subject = Some(subject.into());
        self
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, message)
    }
    pub fn usage(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Usage, message)
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Invalid, message)
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotFound, message)
    }
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Conflict, message)
    }
    pub fn limit(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Limit, message)
    }
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Forbidden, message)
    }
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unavailable, message)
    }
    pub fn io(context: &str, err: &std::io::Error) -> Self {
        Self::new(ErrorCode::Internal, format!("{context}: {err}"))
    }
}

impl fmt::Display for FleetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for FleetError {}

impl From<std::io::Error> for FleetError {
    fn from(err: std::io::Error) -> Self {
        FleetError::new(ErrorCode::Internal, err.to_string())
    }
}
