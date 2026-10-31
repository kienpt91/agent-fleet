# 19. Peer message content is untrusted and never reaches a trusted place

Status: accepted. Builds on ADR 4 (output is untrusted), ADR 5, ADR 13 and ADR 16.

## Context
A message is written by an agent that may have been steered by hostile text (a web page, a file, another message). Its
words can try to look like fleet, the operator, an approval or a gate answer. A language model reading it can be
influenced by it; nothing in this repository can prevent that.

## Decision
- **Everything is sanitized, redacted, bounded, labelled.** Text goes through the domain sanitizer, then the secret-shape
  scan (a match refuses the whole message and stores no body; the rules are the ones of `scripts/secret-scan.sh`,
  re-implemented in `msg::secrets`, also checked after removing invisible characters), then the daemon's redactor, then
  is clipped. Kinds are a closed set (`info`, `question`, `result`, `request`); a file whose kind says `approval` is
  invalid and a body that says "I approve" is only text.
- **One label everywhere:** `from task <id> (unverified peer message)` (the operator's own messages say `from operator`).
  The CLI, the inbox files (`label`, `trust`, a fixed note), briefs and the read views carry it; the CLI quotes bodies.
- **Never in a trusted field.** Message text is not in a verification contract, a goal's acceptance criteria, an
  approval (approvals name the message id, channel, sender, recipients and a digest), a decision gate (ids and numbers
  only), an event headline, the notes of a task, or the context layers 1 to 5 of the planned compiler (N2: see
  docs/neo.md). A message cannot carry or ask for an approval, a token, a credential or a command the daemon would run,
  and it cannot change the recipient's spec or contract (the brief is built from the task record only).
- **In briefs:** a worker's brief names its channels and says what waits for it. Of the waiting messages only the
  sender label and the *subject*, flattened to one quoted line of at most 80 characters (three at most), appear; the body
  never does. The same rule as a dependency's summary.

## Consequences
A model that reads its inbox can still be talked into things; the blast radius is what that agent could do anyway,
plus whatever it can ask other agents to do within the channel limits. What is not verified here: how real agents behave
when they read injected messages (only scripted agents and unit-level hostile text were used).
