//! Hard caps. Everything that grows with input from workers, files or sockets has one of these.

/// Longest accepted control-socket message (one JSON line).
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;
/// Simultaneous control connections.
pub const MAX_CONNECTIONS: usize = 64;
pub const MAX_RUNS: usize = 64;
pub const MAX_TASKS_PER_RUN: usize = 256;
pub const MAX_TOTAL_TASKS: usize = 1024;
pub const MAX_DEPS_PER_TASK: usize = 32;
/// Open decision gates and pending approvals kept in memory.
pub const MAX_OPEN_DECISIONS: usize = 512;
pub const MAX_APPROVAL_RECORDS: usize = 1024;
/// Absolute ceiling for concurrent pty sessions; the configured value may not exceed it.
pub const MAX_SESSIONS_HARD: usize = 64;
pub const DEFAULT_RING_BYTES: usize = 256 * 1024;
pub const MAX_RING_BYTES: usize = 4 * 1024 * 1024;
/// One unterminated output line is flushed after this many bytes.
pub const MAX_PARTIAL_LINE: usize = 8 * 1024;

pub const MAX_ID_LEN: usize = 40;
pub const MAX_TITLE: usize = 200;
pub const MAX_OBJECTIVE: usize = 2 * 1024;
pub const MAX_SPEC: usize = 16 * 1024;
pub const MAX_SUMMARY: usize = 2 * 1024;
pub const MAX_NOTE: usize = 4 * 1024;
pub const MAX_NOTES_PER_TASK: usize = 12;
pub const MAX_ANSWER: usize = 4 * 1024;
pub const MAX_SEND_BYTES: usize = 4 * 1024;
pub const MAX_QUESTIONS_PER_TASK: u32 = 8;
pub const MAX_CHOICES: usize = 8;
pub const MAX_ACTION_ARGS: usize = 64;
pub const MAX_ACTION_ARG_LEN: usize = 4 * 1024;
pub const MAX_BRIEF_ARG_BYTES: usize = 16 * 1024;
pub const MAX_BRIEF_FILE_BYTES: usize = 128 * 1024;

/// Files a worker can write (`.fleet/result.json`, `.fleet/question.json`).
pub const MAX_CONTRACT_FILE: u64 = 64 * 1024;
/// Largest `fleet plan` file.
pub const MAX_PLAN_FILE: u64 = 1024 * 1024;
/// Largest `fleet.toml`.
pub const MAX_CONFIG_FILE: u64 = 256 * 1024;

/// One line of the event log.
pub const MAX_EVENT_LINE: usize = 128 * 1024;
/// The event log refuses new events once it has grown to this size.
pub const MAX_EVENT_LOG_BYTES: u64 = 64 * 1024 * 1024;
/// Output excerpt stored in an event.
pub const EVENT_OUTPUT_TAIL: usize = 2 * 1024;

/// Captured stdout of one quality gate or external command.
pub const MAX_GATE_OUTPUT: usize = 4 * 1024 * 1024;
/// Output of gates fed back into a retry brief.
pub const GATE_FEEDBACK_BYTES: usize = 3 * 1024;
pub const MAX_GATES_CONFIGURED: usize = 16;
pub const MAX_WORKERS_CONFIGURED: usize = 32;
pub const MAX_REDACT_RULES: usize = 32;
pub const MAX_REDACT_LITERALS: usize = 64;
pub const MAX_ENV_ALLOW: usize = 64;
pub const MAX_CONCURRENT_JOBS: usize = 8;
pub const MAX_RETRIES_CEILING: u32 = 10;

/// Bytes of one attach frame payload.
pub const MAX_FRAME_PAYLOAD: usize = 32 * 1024;
/// Chunks buffered for a slow attach client before output is dropped for it.
pub const ATTACH_QUEUE_CHUNKS: usize = 64;
