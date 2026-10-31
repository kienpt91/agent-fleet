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
  *)
    echo "agent: no script for task $task" >&2
    exit 3
    ;;
esac
sleep 1
echo "agent: finished $task"
