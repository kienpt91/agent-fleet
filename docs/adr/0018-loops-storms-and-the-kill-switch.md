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
per-task and per-channel totals, per-channel and per-task-per-minute rates, approvals waiting on the channel, the cap per run.
- **Loops without trusting `in_reply_to`.** A message from B to A directly after one from A to B is a reply whether it
  says so or not: its depth is the previous depth + 1 (the larger of that and the explicit chain). A message that ends a
  chain longer than `max_reply_depth` (default 6) is refused. Independently a **pair budget** counts the times two tasks
  *changed direction* (a message from one right after one from the other; default 12). Both are counted per pair of
  tasks over **every channel of the run**, so changing channels does not start a new chain. Only genuine back-and-forth
  counts: one task writing many messages to a peer that never answers, or replying to its own messages, builds neither
  depth nor budget (a reply must name a message *its sender received*; anything else is refused as `unknown-reply`
  with the words of a message that does not exist, so ids are no oracle). That traffic is bounded by the per-task and
  per-channel quotas and the rates below, not by the loop rule; this closes a way for one member to mute a shared channel
  without any exchange having happened.
  Crossing either limit refuses the message, **mutes the sender on that channel** (the whole channel only when no other
  member is left healthy) and opens a decision gate (kind `channel`, choices `unmute` / `keep muted`) naming ids and
  numbers only. The other members keep talking; what was accepted before the mute is still delivered. The gate blocks
  no task. Answering `unmute` (or `fleet msg unmute`) lifts the mute and gives the run's pairs a fresh budget.
- **Storms and the size of the log.** Every refusal is an event, so refusals are bounded three ways: (1) more than 20 in
  a minute (invalid files included) mutes the task as a flood; (2) a **budget of refusals per task**
  (`max_refused_per_task`, default 100, hard 1000, counted until a person unmutes) mutes it however slowly it spends
  them; (3) files that are not messages at all are recorded once a minute per task, the others are rejected and only
  counted (the next record says how many), and still count towards (1). A muted task's outbox is not read. Refused
  messages keep no body (only size and digest) and are the first thing forgotten in memory; the memory of which outbox
  files were seen is a high-water mark per task and attempt, not one entry per file. What this bounds: what one task can
  add to the log per run. What it does not: the log has a hard size cap (64 MiB) after which the daemon stops recording,
  and a fleet of many hostile tasks times the budget is still bytes; the claim "the log stays bounded" is only as strong
  as these caps and the cap of the log itself.
- **Totals and rates** are per task per channel, per channel (members x per-task total), per task over all channels per
  minute, a cap **per run** (`max_total_messages`, hard 2000, with the same ceiling over all runs), fan-out per message
  (default 8, hard 32) and a **share of each recipient's inbox per sender** (64 of 256), so one sender cannot fill an
  inbox with its own text and make everybody else's expire as `inbox-full`.
- **Decision pool.** Messages that wait for approval and gates about mutes have shares of their own in the pool of open
  decisions (at most 16 waiting messages per channel, 128 approvals and 32 gates in all), so they cannot starve worker
  questions and merge approvals; a message refused for that reason says `quota`.
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
replay). A loop is blamed on the sender that crossed the limit, which may be the honest side of an exchange the other
side started: the gate says which pair and the operator unmutes. Not covered: collusion through a channel that is
allowed (the limits and the audit trail make it visible and bounded, they do not make it impossible), and anything two
agents do outside the daemon.

## Decisions that were open when this was written
- **Endpoint-ended mute and messages already accepted.** The brief says a channel is muted when either endpoint's task
  ends or fails. For channels of more than two members that would silence the survivors, so the rule is "fewer than two
  members can still take part, or the lead is gone" (identical for two members). A mute for that reason stops *new*
  submissions but still delivers what was accepted before it, to recipients that are still running: otherwise a worker's
  last message before it exits (the commonest use) could never arrive. Every other mute (operator, loop, flood) holds
  everything in both directions until it is lifted. Messages to a recipient that ended expire at once.
- **Operator exemptions.** `fleet msg send` is subject to size, secret shapes, the sanitizer, fan-out, the global cap and
  mutes, but not to per-task rates, totals, duplicate suppression, loop limits or approvals: a person is not a storm
  source and a person's message needs no second person's approval. It is labelled `from operator`, allowed on `to_lead`
  channels, and refused on a muted channel (unmute first).
- **Message ids are consumed by refusals** (`m3` can be a `message_blocked`), so an id the sender or the operator sees is
  always in the log.
