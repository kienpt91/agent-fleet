# 17. The context compiler is a pure, deterministic function with fixed layers and explicit budgets

Status: accepted

## Context
NEO (docs/neo.md) wants every dispatch to give an agent the same context in the same order, checkable afterwards, with
text that agents wrote kept apart from text fleet vouches for. A model-written briefing, a retrieval step or a template with
a clock in it cannot give that.

## Decision
- **Pure and deterministic.** `neo-context` is a std-only domain crate (`#![forbid(unsafe_code)]`, no clock, no randomness,
  no I/O). `Compiler::compile(input, hasher)` is a function of plain data: the application layer collects the input from the
  fleet state, the goal ledger, the event log, the configuration and the repository at commits fleet recorded. Maps are
  ordered, numbers are integers, hashing comes in through the domain's `Digest` port, so the same input gives the same bytes
  and the same SHA-256 on every run and platform. Golden digests are checked with a reference SHA-256 in the domain tests and
  with the adapter's real hash in `fleet-infra/tests/golden_pack.rs`.
- **Eight layers in a fixed order**: invariants, charter, plan position, task and acceptance contract, verified knowledge,
  working set, recent events, scratch. A lower layer never displaces a higher one. Layers 1-5 hold only text written by fleet
  or its operator. Layer 6 shows files as the repository has them at the run's base commit; which files it shows first
  follows the paths the task's commits changed (so layer 6 is outside the stable digest), but their content never comes from
  the worktree. Text an agent or one of its programs wrote (worker notes, summaries, the output and summaries of checks, the
  worker's committed files) is accepted only through `UnverifiedInput` and the handoff summary, rendered in layer 7 alone,
  introduced by a line that starts with `UNVERIFIED (` (made of fleet's words only) and quoted with `> `, or left out when
  `[context] unverified = "exclude"`. The other lines of layer 7 are fleet's own record.
- **Output.** A canonical text with length-prefixed framing (`@@neo-layer N name trust= bytes= tokens= sha256=` ... `@@neo-end`),
  every content line that would start with `@@` or a backslash written with a backslash in front, and a canonical JSON
  manifest (per layer: tokens, bytes, budget, digest, trust, items, dropped keys, excluded and unverified counts). The pack
  digest covers the manifest and the text; the *stable digest* covers layers 1-5 only. For the same plan, goal ledger, results of
  fleet's own checks and configuration, layers 1-5 do not depend on the attempt number, the events, the working set, the paths
  a worker committed or any handoff, so they are identical across a retry and across a crash. They change when that state
  does: layer 3 lists the plan and layer 4 what fleet's own runs said (the retry pack of a failed check says so). Facts that
  carry `paths` are scoped by the base commit's tree and the paths the task's contracts allow, and goals are listed by id, so
  neither the worker's commits nor the order goals were added in change layers 1-5.
- **Token estimator.** `tokens = ceil(bytes x safety% / (chars_per_token x 100))` over UTF-8 bytes, per profile
  (`[context.profile.<name>] max_tokens, chars_per_token, safety_percent`; defaults 32000, 4, 125). It is monotone and
  sub-additive, so summing the estimates of the frames never undercounts the whole. It is an estimate: real tokenizers differ.
- **Budgets and shrink order.** Every layer has a hard budget (a share of `max_tokens`: 12, 15, 35, 50, 15, 35, 15, 5 percent;
  the shares add up to more than 100). Step 1 fits each layer to its own budget: layers 1, 3 and 4 are never shortened (an
  overflow is `LayerOverflow`, an error and no pack); layer 2 drops whole sections by priority (constraints, then glossary; the
  goal never); layers 5, 6 and 7 drop whole items, lowest priority first. Step 2: if the pack is still over `max_tokens`, items
  go from layer 7, then 6, then 5. Step 3: sections of layer 2, then `PackOverflow`. Everything dropped is named in the manifest
  and in a line of the layer; nothing is cut silently.
- **Safety.** Every string is sanitized with the domain sanitizer, redacted with the daemon's redactor and clipped; a line that
  matches a secret rule is replaced whole; a file whose name or content matches the secret rules (reimplemented without a regex
  engine from `scripts/secret-scan.sh`, matched case-insensitively, with more credential file names and directories, run in
  time linear in the line length) is named and not shown; files are read from the real repository (ADR 16), through the existing
  hardened git adapter, bounded in size and count. The rules are a filter and not a guarantee (docs/neo.md lists what they miss).
- **Stored packs are verified, not believed.** A pack is shown, diffed or verified only after its stored text is re-hashed
  (layer digests, stable digest, pack digest) and its manifest compared with the `context_compiled` event of the hash-chained
  log; the digest comes from the log, not from the store's file names or sequence numbers. A mismatch, a missing newest pack
  or an unreadable file is `tampered` (exit status 11), with no content; `verify` then recompiles as of the logged pack and
  reports which layers changed.

## Consequences
The pack is reproducible and auditable (`ContextCompiled` carries the digest and the manifest; `fleet context verify`
recompiles as of the stored pack and reports which layers changed). It does not make a model obey it, and no claim is made
about hallucination. A task whose layers 1, 3 or 4 do not fit the profile fails to start with a message that names the layer.
Someone who can write the state directory and the audit log together can rewrite both; the log is tamper-evident, not
tamper-proof (ADR 2).
