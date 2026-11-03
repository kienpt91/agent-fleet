# 24. Agents talk through the daemon and files, never through each other's terminal

Status: accepted. Builds on ADR 4 (output is untrusted), ADR 7 (config is daemon-side) and ADR 15 (one origin rule).

## Context
Until now the only thing that reached another task was a finished task's summary in its brief. The owner wants direct
messaging between running agents. The obvious implementation, typing one agent's text into another agent's pty, would
hand a hostile or confused agent keystrokes in a terminal program that may treat them as a prompt, a shortcut or a
command. Workers are also refused on the control socket and the web port by design (ADR 9, 15), so a worker cannot call
the daemon to send anything. The only worker-to-daemon channel is files, as for questions.

## Decision
- **The daemon mediates everything.** A worker writes `.fleet/outbox/<n>.json` (a schema-checked JSON file). The daemon
  reads it with the care it gives every worker file (`fsx`/`contract_fs`: no link followed, no FIFO waited on, size and
  count caps, descriptors checked), decides under the operator's policy, records the decision as an event and, for an
  accepted message, writes `.fleet/inbox/<message id>.json` and `index.json` into the worktree of a *running* recipient.
  Nothing is typed into a terminal. The optional `[messages] notify_input = true` types one fixed line
  (`[fleet] new message m3 in .fleet/inbox`, printable ASCII, no peer text) **without Enter**; it is off by default.
  Its risk, which is why it stays off: the *timing* of those keystrokes is a peer's to choose (the line appears when
  the peer's message is delivered), so a sender can make characters land in a prompt the recipient is looking at or in a
  half-typed command. Without a carriage return they can never submit a line or confirm a prompt by themselves, and they
  carry no peer text; they can still garble what the agent was typing. An earlier version ended the line with a carriage
  return, which let a peer time a confirmation (a security review found it); the Enter was removed.
- **Channels are declared by the operator**, in a plan (`[[channels]]`), never by a worker: members (`task:<id>` or
  `role:<worker>`, never a bare word), mode, direction, a lead, size/rate/total limits, ttl, `requires_approval`. A task
  that is a member of no channel is never read and never written to. Unknown keys are errors.
- **The inbox belongs to the daemon.** `FsInbox::sync` opens `.fleet` and `inbox` without following links (every
  component relative to the previous descriptor) and refuses, fail closed, when either is a link or not a directory; the
  message stays queued. It writes each file by temporary name and rename, rewrites `index.json` (the real deliveries with
  their SHA-256), and removes every file it did not write (a file a worker made to look like a delivery). `prepare` runs
  while no worker runs and replaces such things. A delivery path is always below the recipient's own `.fleet/inbox`.
- **Delivery is idempotent.** The file is written first and `message_delivered` recorded after, so a crash in between
  redelivers the same file under the same name and the log says "delivered" once; replay rebuilds the board, so waiting
  messages survive a restart and delivered ones are not delivered again. A file read twice (restart, failed rename) is one
  message (its source is recorded).
- **The event log is the message store** (ADR 2): `channel_opened`, `message_submitted`, `message_delivered`,
  `message_blocked`, `message_expired`, `channel_muted`, `channel_unmuted` in the hash chain. No second store.
- **The daemon never delivers to a task that is not running.** Messages queue until it starts and expire by ttl, or at
  once when the recipient ended.

## Consequences
Latency is a daemon tick (50 ms) plus whatever the agent's polling is; an agent has to look in its inbox (the brief says
so and `notify_input` can nudge it). A worker can still write a hostile file into its own outbox and can make its own
inbox unwritable (the message then waits and expires); it cannot reach another worker's directories through the daemon.
Workers are not sandboxed (see SECURITY.md): a worker can still write into another worktree directly as the same user;
the design limits what *the daemon* does on its behalf and what the daemon records and shows, not what a same-user
process can do to files.

## Decisions that were open when this was written
- **The notice has no Enter** (see above): a nudge that cannot press a key that matters is the only kind this design
  accepts. An agent that needs the nudge to *act* has to read its inbox on its own.
- **A `lead` key.** `direction = "to_lead"` has to say who receives, and the plan format had no way to. A channel
  therefore takes an optional `lead` (one of its members), required for `to_lead` and refused for `both`. Naming the
  first member instead would have made a security-relevant fact implicit.
- **Members are `task:<id>` or `role:<worker>`, never a bare word.** A bare `api` could be a task or a worker, and
  guessing widens who may talk (a role stands for every task that runs that worker). The prefix costs four characters
  and removes the ambiguity. Roles must name a worker of `fleet.toml`; tasks must exist in the run when the plan is
  loaded (checked before anything is recorded).
- **A worker's outbox is read once more when its session ends**, before the task's end is recorded: the natural
  pattern "send the final `result` message, then exit" would otherwise lose the message.
