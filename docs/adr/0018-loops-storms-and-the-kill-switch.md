# 18. Loop and storm protection, and a human kill switch

Status: accepted. Builds on ADR 17.

## Context
Two agents can keep each other busy for ever (A asks, B answers, A answers the answer), agents that know they are watched
leave `in_reply_to` out, and a hostile agent can flood the log or coordinate with another to get around a limit one
agent alone would hit. Messages cost tokens at the recipient and bytes in an append-only, size-capped log.

## Decision
All of it is pure domain (`fleet-domain::msg`, clock injected) and applied in a fixed order by `MessageBoard::submit`:
membership, task and channel mutes, size, sanitized text, **secret shapes**, recipients and fan-out, reply target,
redaction, **duplicate suppression** (same sender, channel, recipients and words within the channel ttl), loop limits,
per-task and per-channel totals, per-channel and per-task-per-minute rates, the global cap.
- **Loops without trusting `in_reply_to`.** A message from B to A directly after one from A to B is a reply whether it
  says so or not: its depth is the previous depth + 1 (the larger of that and the explicit chain). A message that ends a
  chain longer than `max_reply_depth` (default 6) is refused. Independently a **pair budget** counts the messages two
  tasks exchange on a channel in either direction (default 12). Crossing either **mutes the channel and opens a decision
  gate** (kind `channel`, choices `unmute` / `keep muted`) naming ids and numbers only. The gate blocks no task. Answering
  `unmute` (or `fleet msg unmute`) lifts the mute and gives the pair a fresh budget.
- **Storms.** A task whose messages are refused more than 20 times in a minute (invalid files included) is muted as a
  flood with the same gate. A muted task's outbox is not read. Refused messages keep no body (only size and digest), are
  the first thing forgotten in memory, and cannot grow the log faster than the flood rule allows.
- **Totals and rates** are per task per channel, per channel (members x per-task total), per task over all channels per
  minute, and global (`max_total_messages`, hard cap 2000); fan-out per message (default 8, hard 32).
- **Automatic mute.** A channel is muted when fewer than two of its members can still take part, or when the lead of a
  `to_lead` channel is gone (`endpoint-ended`): for a two-member channel that is "either endpoint ended or failed".
- **The kill switch.** `fleet msg mute|unmute <channel|task>` (operator only; `channel:<ref>` / `task:<ref>` when a name
  is both). A mute stops acceptance *and* delivery in both directions. The operator's own `fleet msg send` is allowed on
  a `to_lead` channel and bounded like any message (size, secret shapes, mute) but not rate limited and never needs an
  approval.
- **Approval.** `requires_approval` channels hold each message until a person approves an approval of purpose `message`
  bound to a digest of channel, sender, recipients and the words (ADR 5). The approval names the message and its digest,
  not its text; it blocks no task; it is consumed at delivery and re-checked against the stored message.

## Consequences
A legitimate long exchange needs an operator to unmute or higher limits. A determined pair can still burn the budget
before it is noticed; the budget bounds the cost. Limits are per daemon lifetime of the log (counters are rebuilt by
replay). Not covered: collusion through a channel that is allowed (the limits and the audit trail make it visible and
bounded, they do not make it impossible), and anything two agents do outside the daemon.
