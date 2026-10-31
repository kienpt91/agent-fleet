# 4. Worker output is untrusted: sanitize, redact, bound

Status: accepted

## Context
A worker's terminal output can contain sequences that rewrite the screen, set the title, write to the
clipboard or make the terminal answer into the input; Unicode controls that reorder or hide text; and
secrets the worker found.

## Decision
A streaming sanitizer turns raw bytes into plain text before anything is stored or shown: escape
sequences are removed, other controls and invisible Unicode become visible `\xNN`/`\u{...}` tokens, a
bare carriage return becomes a line break so nothing can be overwritten. Stored lines pass through a
redactor (built-in key shapes, configured rules, values of secret-looking daemon variables, PEM
blocks). The buffer is a ring with a hard byte cap. `attach` is the one place full-screen output is
needed; it uses a second mode of the same state machine that passes a small allowlist (colors, cursor
movement, a few private modes) and drops everything else.

## Consequences
The dashboard, `tail`, logs and the audit log never contain control sequences. Redaction is
pattern-based and will miss secrets that do not match a rule; the README says so.
