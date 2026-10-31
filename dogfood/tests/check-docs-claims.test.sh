#!/bin/sh
# Tests of dogfood/check-docs-claims.sh.
. "$(dirname "$0")/lib.sh"
dc="$dogfood/check-docs-claims.sh"
run() { "$dc" --repo "$1" base..HEAD; }
tag_base() { git -C "$1" tag -f base >/dev/null; }

readme='# Project

1385 tests, all passing in the final run.

| Layer | What | Tests |
| --- | --- | --- |
| Domain | pure unit tests | 317 |
| Application | the orchestrator | 298 |
| Adapters | adapters | 1 |
| Soak | memory | 1 |
| End to end | the binary | 260 |

```text
layer               lines  covered  percent
domain               4852     4726    97.4%
overall             15611    14642    93.8%
```

Prose that mentions no figures.
'
mk() {
  r=$work/$1; mkdir -p "$r/docs"
  git -C "$r" init -q -b main
  printf '%s' "$readme" > "$r/README.md"
  printf 'the page lists 334 items\nPlain text.\n' > "$r/docs/neo.md"
  printf 'Plain text.\n' > "$r/docs/domain.md"
  git -C "$r" add -A; git -C "$r" commit -q -m base; tag_base "$r"
  printf '%s\n' "$r"
}
edit() { # file old new
  sed "s#$2#$3#" "$1/$4" > "$1/.tmp" && mv "$1/.tmp" "$1/$4"
}

r=$(mk prose)
printf 'A new paragraph without any figure.\n' >> "$r/README.md"
printf 'Another sentence.\n' >> "$r/docs/domain.md"
commit_all "$r"
expect_exit 0 "prose edits pass" run "$r"
expect_out "and are counted" "2 changed documentation line(s)"

r=$(mk count)
edit "$r" "1385 tests" "1400 tests" README.md; commit_all "$r"
expect_exit 1 "the headline test count" run "$r"
expect_out "the line is shown" "1400 tests"
r=$(mk table)
edit "$r" "| 317 |" "| 320 |" README.md
commit_all "$r"
expect_exit 1 "a row of the test table" run "$r"
r=$(mk table2)
edit "$r" "| End to end | the binary | 260 |" "| End to end | the binary | 261 |" README.md; commit_all "$r"
expect_exit 1 "the end to end row" run "$r"
r=$(mk cov)
edit "$r" "97.4%" "98.0%" README.md; commit_all "$r"
expect_exit 1 "a coverage figure" run "$r"
r=$(mk cov2)
edit "$r" "overall             15611    14642    93.8%" "overall             15700    14700    93.6%" README.md; commit_all "$r"
expect_exit 1 "a coverage row" run "$r"
r=$(mk deleted)
grep -v '^1385 tests' "$r/README.md" > "$r/.tmp" && mv "$r/.tmp" "$r/README.md"; commit_all "$r"
expect_exit 1 "deleting a claim is a change too" run "$r"
r=$(mk added)
printf 'Now 9999 tests pass.\n' >> "$r/docs/domain.md"; commit_all "$r"
expect_exit 1 "a new claim in another document" run "$r"
r=$(mk checks)
edit "$r" "lists 334 items" "lists 400 items" docs/neo.md; commit_all "$r"
expect_exit 0 "a number that is not followed by the word is not a claim" run "$r"
r=$(mk checks2)
printf 'node scripts/web-smoke.mjs (500 checks)\n' >> "$r/docs/neo.md"; commit_all "$r"
expect_exit 1 "N checks is" run "$r"
r=$(mk crates)
printf 'check-crates-age (122 locked crates)\n' >> "$r/README.md"; commit_all "$r"
expect_exit 1 "the locked crate count" run "$r"
r=$(mk soak)
printf 'soak: 264 MiB/s\n' >> "$r/README.md"; commit_all "$r"
expect_exit 1 "a throughput figure" run "$r"
r=$(mk layers)
printf 'domain: 400 passed, 0 failed\n' >> "$r/README.md"; commit_all "$r"
expect_exit 1 "a line of the layered runner" run "$r"
r=$(mk dash)
printf -- '- a list item\n-- another\n' >> "$r/docs/domain.md"; commit_all "$r"
expect_exit 0 "lines that start with dashes are content, not headers" run "$r"
expect_out "and counted as such" "2 changed documentation line(s)"
r=$(mk other)
mkdir -p "$r/src"; printf '1400 tests\n' > "$r/src/notes.txt"; commit_all "$r"
expect_exit 0 "other files are not documentation" run "$r"
r=$(mk hostile)
printf 'text\033[2J with 7 tests\n' >> "$r/README.md"; commit_all "$r"
expect_exit 1 "a claim behind control characters" run "$r"
expect_no_out "no escape sequence is printed" "$(printf '\033')"

r=$(mk ranges)
printf 'one more line.\n' >> "$r/README.md"; commit_all "$r"
printf '1 tests\n' >> "$r/README.md"; commit_all "$r"
expect_exit 1 "base and head as two arguments" "$dc" --repo "$r" base HEAD
expect_exit 0 "up to the first commit" "$dc" --repo "$r" base HEAD~1
expect_exit 2 "an unknown ref" "$dc" --repo "$r" nonexistent..HEAD
expect_exit 2 "an option as the range" "$dc" --repo "$r" -- --output=x..HEAD
expect_exit 2 "a range with metacharacters" "$dc" --repo "$r" 'base..HEAD;x'
expect_exit 2 "no range" "$dc" --repo "$r"
expect_exit 2 "not a directory" "$dc" --repo "$work/none" base..HEAD
expect_exit 2 "an unknown option" "$dc" --frobnicate base..HEAD

finish
