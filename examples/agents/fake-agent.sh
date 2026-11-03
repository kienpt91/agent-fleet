#!/bin/sh
# A scripted stand-in for a coding agent. The demo and the end-to-end tests use it so the
# orchestrator can be exercised without any real agent: it behaves deterministically per task id.
set -eu
task="${FLEET_TASK:?FLEET_TASK is set by fleet}"
brief="${FLEET_BRIEF_FILE:?FLEET_BRIEF_FILE is set by fleet}"

result() {
  printf '{"version":1,"status":"done","summary":"%s"}\n' "$1" > .fleet/result.json
}

echo "agent: starting $task"
case "$task" in
  schema)
    mkdir -p db
    echo "create table users (id integer primary key);" > db/schema.sql
    result "added the users table"
    ;;
  api)
    if grep -q '^ *Operator says: ' "$brief"; then
      framework=$(sed -n 's/^ *Operator says: //p' "$brief" | head -n 1)
      echo "framework: $framework" > api.txt
      result "built the API with $framework"
    else
      echo "agent: I need a decision before I can go on"
      printf '{"version":1,"kind":"question","text":"Which HTTP framework should the API use?","choices":["axum","actix"]}\n' > .fleet/question.json
    fi
    ;;
  docs)
    printf '# Docs\n\nThe users table is described in db/schema.sql.\n' > docs.md
    result "wrote the docs"
    ;;
  release)
    if grep -q 'Quality gate' "$brief"; then
      echo "agent: the gate complained, fixing the changelog"
      printf '# Release notes\n\n- first release\n' > CHANGELOG.md
    else
      printf 'TODO\n' > CHANGELOG.md
    fi
    result "wrote the changelog"
    ;;
  # The starter tasks of dogfood/plan.neo-roadmap.toml, for the dry run of dogfood/RUNBOOK.md.
  starter-docs-glossary)
    # three rows after the last row of the glossary table of docs/domain.md (once: a retry finds them there)
    if ! grep -q '^| \*\*Protected path\*\* ' docs/domain.md; then
    last=$(grep -n '^| \*\*Remaining\*\* ' docs/domain.md | head -n 1 | cut -d: -f1)
    head -n "$last" docs/domain.md > docs/domain.md.new
    cat >> docs/domain.md.new <<'ROWS'
| **Protected path** | A file or directory NEO may never change by itself (the approval and admission code, the verification and evidence code, the secret scanner and hooks, CI, dependency manifests, the architecture tests and the dogfood directory). The list is dogfood/protected-paths.json; the reasons are in dogfood/PROTECTED_PATHS.md. A task whose commits touch one fails its contract. |
| **Human-only task** | A task of the roadmap plan that touches a protected path. It uses the worker `human-only`, a contract that no change can satisfy, and is done by a person (dogfood/human-only.md). |
| **Self-development** | Running fleet on this repository with NEO's own goal ledger and verification contracts under a person's control, as described in dogfood/RUNBOOK.md. |
ROWS
    tail -n +"$((last + 1))" docs/domain.md >> docs/domain.md.new
    mv docs/domain.md.new docs/domain.md
    fi
    result "added three glossary rows"
    ;;
  starter-error-tests)
    if ! grep -q '^mod tests' crates/fleet-domain/src/error.rs; then
    cat >> crates/fleet-domain/src/error.rs <<'RUST'

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_and_unknown_names_are_none() {
        for code in ALL_CODES {
            assert_eq!(ErrorCode::parse(code.as_str()), Some(code));
        }
        assert_eq!(ErrorCode::parse("nope"), None);
        assert_eq!(ErrorCode::parse(""), None);
        assert_eq!(ErrorCode::parse("Internal"), None);
    }

    #[test]
    fn exit_codes_are_the_documented_ones_and_distinct() {
        let table = [
            (ErrorCode::Internal, 1),
            (ErrorCode::Usage, 2),
            (ErrorCode::DaemonDown, 3),
            (ErrorCode::NotFound, 4),
            (ErrorCode::ApprovalRequired, 5),
            (ErrorCode::Conflict, 6),
            (ErrorCode::Invalid, 7),
            (ErrorCode::Limit, 8),
            (ErrorCode::Unavailable, 9),
            (ErrorCode::Forbidden, 10),
            (ErrorCode::Tampered, 11),
            (ErrorCode::Timeout, 12),
            (ErrorCode::Unhealthy, 13),
        ];
        assert_eq!(table.len(), ALL_CODES.len());
        for (code, exit) in table {
            assert_eq!(code.exit_code(), exit);
        }
    }

    #[test]
    fn constructors_set_the_code_and_keep_the_message() {
        let cases = [
            (FleetError::internal("m"), ErrorCode::Internal),
            (FleetError::usage("m"), ErrorCode::Usage),
            (FleetError::invalid("m"), ErrorCode::Invalid),
            (FleetError::not_found("m"), ErrorCode::NotFound),
            (FleetError::conflict("m"), ErrorCode::Conflict),
            (FleetError::limit("m"), ErrorCode::Limit),
            (FleetError::forbidden("m"), ErrorCode::Forbidden),
            (FleetError::unavailable("m"), ErrorCode::Unavailable),
        ];
        for (error, code) in cases {
            assert_eq!(error.code, code);
            assert_eq!(error.message, "m");
            assert_eq!(error.subject, None);
            assert_eq!(error.to_string(), "m");
        }
    }

    #[test]
    fn a_subject_and_an_io_error_are_kept() {
        let e = FleetError::conflict("busy").with_subject("a1");
        assert_eq!(e.subject.as_deref(), Some("a1"));
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
        let e = FleetError::io("reading x", &io);
        assert_eq!(e.code, ErrorCode::Internal);
        assert_eq!(e.message, "reading x: gone");
        let from: FleetError = io.into();
        assert_eq!(from.code, ErrorCode::Internal);
        assert_eq!(from.message, "gone");
    }
}
RUST
    fi
    result "added the unit tests of error.rs"
    ;;
  maint-docs-links)
    # runs after the three starters: their work is merged into this branch, so the scope of its contract allows their files too
    if ! grep -q '^A dry-run note' docs/parity.md; then
      printf '\nA dry-run note: the links of this document were checked by a scripted agent.\n' >> docs/parity.md
    fi
    result "checked the links"
    ;;
  starter-value-property)
    # a careless first attempt also "tidies" a sentence of the security policy; a retry that is told
    # which check failed takes its own line out again (the line was appended, so the file is as it was)
    if ! grep -q 'protected-diff' "$brief"; then
      echo "agent: tidying SECURITY.md while I am here"
      printf 'A note added by the agent.\n' >> SECURITY.md
    else
      echo "agent: the verification complained about a protected path; taking my note out of SECURITY.md"
      grep -v '^A note added by the agent\.$' SECURITY.md > SECURITY.md.new || true
      mv SECURITY.md.new SECURITY.md
    fi
    # the test goes before the closing brace of the existing test module (once)
    if ! grep -q 'canonical_text_is_the_same_whatever_the_order_of_the_pairs' crates/fleet-domain/src/value.rs; then
    sed '$d' crates/fleet-domain/src/value.rs > crates/fleet-domain/src/value.rs.new
    cat >> crates/fleet-domain/src/value.rs.new <<'RUST'

    #[test]
    fn canonical_text_is_the_same_whatever_the_order_of_the_pairs() {
        #[derive(Clone)]
        enum Spec {
            Null,
            Bool(bool),
            Int(i64),
            Str(String),
            Array(Vec<Spec>),
            Object(Vec<(String, Spec)>),
        }
        struct Lcg(u64);
        impl Lcg {
            fn next(&mut self, n: u64) -> u64 {
                self.0 = self
                    .0
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                (self.0 >> 33) % n
            }
        }
        fn spec(rng: &mut Lcg, depth: u32) -> Spec {
            let kinds = if depth == 0 { 4 } else { 6 };
            match rng.next(kinds) {
                0 => Spec::Null,
                1 => Spec::Bool(rng.next(2) == 0),
                2 => Spec::Int(rng.next(2000) as i64 - 1000),
                3 => Spec::Str(format!("s \"{}\" \\ end", rng.next(100))),
                4 => Spec::Array((0..rng.next(4)).map(|_| spec(rng, depth - 1)).collect()),
                _ => Spec::Object(
                    (0..rng.next(4))
                        .map(|i| (format!("k{i}"), spec(rng, depth - 1)))
                        .collect(),
                ),
            }
        }
        fn build(spec: &Spec, reversed: bool) -> Value {
            match spec {
                Spec::Null => Value::Null,
                Spec::Bool(b) => Value::Bool(*b),
                Spec::Int(n) => Value::int(*n),
                Spec::Str(s) => Value::str(s.clone()),
                Spec::Array(items) => Value::array(items.iter().map(|i| build(i, reversed))),
                Spec::Object(pairs) => {
                    let mut built: Vec<(String, Value)> = pairs
                        .iter()
                        .map(|(k, v)| (k.clone(), build(v, reversed)))
                        .collect();
                    if reversed {
                        built.reverse();
                    }
                    Value::object(built)
                }
            }
        }
        fn space_outside_strings(text: &str) -> bool {
            let (mut in_string, mut escaped) = (false, false);
            for c in text.chars() {
                if in_string {
                    if escaped {
                        escaped = false;
                    } else if c == '\\' {
                        escaped = true;
                    } else if c == '"' {
                        in_string = false;
                    }
                } else if c == '"' {
                    in_string = true;
                } else if c == ' ' {
                    return true;
                }
            }
            false
        }
        let mut rng = Lcg(2024);
        for _ in 0..300 {
            let s = spec(&mut rng, 4);
            let (forward, backward) = (build(&s, false), build(&s, true));
            assert_eq!(forward, backward);
            let text = forward.canonical();
            assert_eq!(text, backward.canonical());
            assert!(!text.contains('\n'));
            assert!(!space_outside_strings(&text), "{text}");
        }
    }
}
RUST
    mv crates/fleet-domain/src/value.rs.new crates/fleet-domain/src/value.rs
    fi
    result "added a property test of the canonical form"
    ;;
  *)
    echo "agent: no script for task $task" >&2
    exit 3
    ;;
esac
sleep 1
echo "agent: finished $task"
