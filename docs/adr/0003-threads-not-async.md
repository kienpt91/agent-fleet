# 3. Threads and one engine thread, no async runtime

Status: accepted

## Context
The daemon needs a few dozen long-lived sessions, a socket server and periodic supervision. An async
runtime would add a large dependency tree to a security-focused tool for no benefit at this scale.

## Decision
The orchestrator is single-threaded and owned by one engine thread that handles every request and tick
in order. Socket connections run on their own threads and talk to the engine over a channel. Each
pty has one reader thread feeding a capped buffer. Quality gates and approved commands run on job
threads. Writes into a worker's terminal happen on the connection thread, so a worker that never reads
its input cannot block the engine.

## Consequences
No locks inside the application layer. Thread counts are bounded by connection, session and job caps.
SIGINT and SIGTERM are not handled (that needs unsafe or another dependency): they stop the daemon
immediately and the crash-recovery path takes over; `fleet shutdown` is the graceful stop.
