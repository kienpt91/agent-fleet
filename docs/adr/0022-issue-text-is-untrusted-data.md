# 22. Issue text is untrusted data: cleaned, redacted, quoted and labelled

Status: accepted. Extends ADR 4.

## Context
Whoever can open or edit an issue can write text that ends up in a worker's brief.

## Decision
`fleet issue to-task` builds the task from fixed sentences of fleet's own plus the issue's repository, title, state, labels,
author and body, each cleaned (terminal control, invisible and bidirectional characters, secret shapes, private-key blocks),
cut, quoted line by line with `> `, and introduced by the line `from GitHub issue #n (untrusted)`; the task title carries the
same label on one flattened line. The quote cannot start a heading, a list item or the label itself. Issue text is never placed in
a verification contract, a goal or an approval. The CLI, TUI and dashboard show issue text as sanitized plain text (quoted
in the CLI, `textContent` in the dashboard). Prompt-injection phrases remain readable as data inside the quote: no filter can
remove them, so the defence is that nothing with authority is built from them.

## Consequences
A worker that follows instructions written inside a quote is the same problem as for any worker output (ADR 4, 14): it has
the permissions of its sandbox-less process and no more approvals than a human grants.
