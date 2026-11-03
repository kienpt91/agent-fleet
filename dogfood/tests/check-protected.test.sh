#!/bin/sh
# Tests of dogfood/check-protected.sh against temporary repositories.
. "$(dirname "$0")/lib.sh"
cp_="$dogfood/check-protected.sh"
list="$dogfood/protected-paths.json"

# Runs the checker over base..HEAD of a repository ($1) after the change made by the caller.
run_range() { "$cp_" --repo "$1" "${2:-base}..HEAD"; }
tag_base() { git -C "$1" tag -f base >/dev/null; }

# ---- exact match, directory prefix, and what is not protected ------------------------------------------
r=$(new_repo exact SECURITY.md README.md docs/neo.md crates/fleet-domain/src/dag.rs crates/fleet-infra/src/server.rs)
tag_base "$r"
echo y >> "$r/SECURITY.md"; commit_all "$r"
expect_exit 1 "an exact file is protected" run_range "$r"
expect_out "the offending path is named" "PROTECTED: SECURITY.md"
expect_out "the entry that matched is named" "matches security.md"

r=$(new_repo prefix crates/fleet-infra/src/server.rs docs/neo.md)
tag_base "$r"
echo y >> "$r/crates/fleet-infra/src/server.rs"; commit_all "$r"
expect_exit 1 "a file below a protected directory" run_range "$r"
r=$(new_repo prefix2 docs/neo.md)
tag_base "$r"
mkdir -p "$r/crates/fleet-infra/src/deep/er"; echo y > "$r/crates/fleet-infra/src/deep/er/new.rs"; commit_all "$r"
expect_exit 1 "a new file deep below a protected directory" run_range "$r"
r=$(new_repo prefix3 docs/neo.md)
tag_base "$r"
echo y > "$r/dogfood"; commit_all "$r"
expect_exit 1 "a file that takes the place of a protected directory (dir/** matches dir)" run_range "$r"
r=$(new_repo prefix4 docs/neo.md)
tag_base "$r"
mkdir -p "$r/crates/newcrate"; echo y > "$r/crates/newcrate/Cargo.toml"; commit_all "$r"
expect_exit 1 "a Cargo.toml of a crate added later (crates/*/Cargo.toml)" run_range "$r"

r=$(new_repo free README.md docs/neo.md crates/fleet-domain/src/dag.rs)
tag_base "$r"
echo y >> "$r/docs/neo.md"; echo y >> "$r/crates/fleet-domain/src/dag.rs"
mkdir -p "$r/crates/neo-knowledge/src"; echo y > "$r/crates/neo-knowledge/src/fact.rs"; commit_all "$r"
expect_exit 0 "unprotected paths pass" run_range "$r"
expect_out "the count is reported" "3 changed path(s)"
r=$(new_repo near docs/neo.md)
tag_base "$r"
mkdir -p "$r/crates"; echo y > "$r/SECURITY.md.bak"; echo y > "$r/dogfood2.txt"; echo y > "$r/crates/Cargo.toml"
mkdir -p "$r/crates/fleet-infra-extra" "$r/crates/x"; echo y > "$r/crates/fleet-infra-extra/a.rs"; echo y > "$r/crates/x/Cargo.lock"; commit_all "$r"
expect_exit 0 "near misses are not protected (a glob is not a prefix, * stays inside a segment)" run_range "$r"
r=$(new_repo empty docs/neo.md)
tag_base "$r"
commit_all "$r"
expect_exit 0 "an empty range passes" run_range "$r"
expect_out "and says so" "0 changed path(s)"

# ---- renames into and out of a protected path -----------------------------------------------------------
r=$(new_repo rename-in docs/a.md crates/fleet-domain/src/dag.rs)
tag_base "$r"
git -C "$r" mv docs/a.md SECURITY.md; commit_all "$r"
expect_exit 1 "a rename onto a protected path" run_range "$r"
expect_out "the target is named" "PROTECTED: SECURITY.md"
r=$(new_repo rename-out SECURITY.md docs/neo.md)
tag_base "$r"
git -C "$r" mv SECURITY.md docs/security-copy.md; commit_all "$r"
expect_exit 1 "a rename away from a protected path" run_range "$r"
expect_out "the source is named" "PROTECTED: SECURITY.md"
r=$(new_repo rename-dir crates/fleet-infra/src/server.rs crates/fleet-infra/src/git.rs docs/keep.md)
tag_base "$r"
git -C "$r" mv crates/fleet-infra docs/old-infra; commit_all "$r"
expect_exit 1 "moving a whole protected directory away" run_range "$r"
r=$(new_repo rename-free docs/a.md)
tag_base "$r"
git -C "$r" mv docs/a.md docs/b.md; commit_all "$r"
expect_exit 0 "a rename between unprotected paths passes" run_range "$r"
r=$(new_repo delete SECURITY.md)
tag_base "$r"
git -C "$r" rm -q SECURITY.md; commit_all "$r"
expect_exit 1 "deleting a protected file" run_range "$r"

# ---- symlinks and submodules ---------------------------------------------------------------------------
r=$(new_repo link-away docs/a.md SECURITY.md)
tag_base "$r"
ln -s ../SECURITY.md "$r/docs/link.md"; commit_all "$r"
expect_exit 1 "a new symlink to a protected file is refused" run_range "$r"
expect_out "and called a symlink" "SYMLINK"
r=$(new_repo link-harmless docs/a.md)
tag_base "$r"
ln -s a.md "$r/docs/alias.md"; commit_all "$r"
expect_exit 1 "even a harmless symlink is left to a human" run_range "$r"
r=$(new_repo link-over SECURITY.md)
tag_base "$r"
rm "$r/SECURITY.md"; ln -s /etc/passwd "$r/SECURITY.md"; commit_all "$r"
expect_exit 1 "a protected file replaced by a symlink" run_range "$r"
r=$(new_repo link-dir docs/a.md)
tag_base "$r"
ln -s ../crates/fleet-infra "$r/docs/infra"; commit_all "$r"
expect_exit 1 "a symlink to a protected directory" run_range "$r"
r=$(new_repo submodule docs/a.md)
tag_base "$r"
git -C "$r" update-index --add --cacheinfo "160000,$(git -C "$r" rev-parse HEAD),vendor/foreign"; git -C "$r" commit -q -m submodule
expect_exit 1 "a submodule entry is refused" run_range "$r"
expect_out "and called a submodule" "SUBMODULE"

# ---- case differences ----------------------------------------------------------------------------------
for p in "security.MD" "Security.md" "CRATES/FLEET-INFRA/src/new.rs" "Crates/fleet-infra/SRC/a.rs" "DogFood/Plan.TOML" ".GITHUB/workflows/ci.yml" "Cargo.LOCK" "CRATES/x/CARGO.toml"; do
  r=$(new_repo "case-$(printf '%s' "$p" | tr '/.' '__')" docs/a.md)
  tag_base "$r"
  mkdir -p "$r/$(dirname "$p")"; echo y > "$r/$p"; commit_all "$r"
  expect_exit 1 "case variant $p" run_range "$r"
done

# ---- spaces, newlines and other unusual names ----------------------------------------------------------
r=$(new_repo spaces docs/a.md)
tag_base "$r"
echo y > "$r/docs/my notes.md"; commit_all "$r"
expect_exit 0 "a space inside a name is fine" run_range "$r"
expect_out "and is counted" "1 changed path(s)"
r=$(new_repo space-protected docs/a.md)
tag_base "$r"
mkdir -p "$r/crates/fleet-infra-extra"; echo y > "$r/crates/fleet-infra-extra/x.rs"; commit_all "$r"
expect_exit 0 "a sibling directory whose name merely starts like a protected one is not protected" run_range "$r"
r=$(new_repo star-crosses docs/a.md)
tag_base "$r"
mkdir -p "$r/crates/a/b"; echo y > "$r/crates/a/b/Cargo.toml"; commit_all "$r"
expect_exit 1 "the shell matcher is stricter than the glob: * also crosses a slash, so crates/a/b/Cargo.toml is refused" run_range "$r"
for p in " SECURITY.md" "SECURITY.md " "docs/ SECURITY.md" "docs /x.md" "docs/x.md "; do
  r=$(new_repo "sp-$(printf '%s' "$p" | tr ' /.' '___')" docs/a.md)
  tag_base "$r"
  mkdir -p "$r/$(dirname "$p")"; echo y > "$r/$p"; commit_all "$r"
  expect_exit 1 "a segment that starts or ends with a space: '$p'" run_range "$r"
  expect_out "is called unusual" "UNUSUAL PATH"
done
nlname=$(printf 'docs/a\n.github/workflows/ci.yml')
r=$(new_repo newline docs/a.md)
tag_base "$r"
mkdir -p "$(dirname "$r/$nlname")"; printf 'y\n' > "$r/$nlname"; commit_all "$r"
expect_exit 1 "a newline in a name (it could fake a second path) is refused" run_range "$r"
expect_out "as unusual, not as a clean pass" "UNUSUAL PATH"
expect_no_out "no real newline is printed from the name" "docs/a
.github"
r=$(new_repo newline-plain docs/a.md)
tag_base "$r"
printf 'y\n' > "$r/$(printf 'docs/harmless\nname.md')"; commit_all "$r"
expect_exit 1 "any newline in a name is refused, protected or not" run_range "$r"
r=$(new_repo tab docs/a.md)
tag_base "$r"
printf 'y\n' > "$r/$(printf 'docs/a\tb.md')"; commit_all "$r"
expect_exit 1 "a tab in a name is refused" run_range "$r"
r=$(new_repo quote docs/a.md)
tag_base "$r"
printf 'y\n' > "$r/docs/say\"hi\".md"; commit_all "$r"
expect_exit 1 "a double quote in a name is refused" run_range "$r"
r=$(new_repo unicode docs/a.md)
tag_base "$r"
printf 'y\n' > "$r/docs/caf$(printf '\303\251').md"; commit_all "$r"
expect_exit 1 "a non-ASCII name is refused (no normalization tricks)" run_range "$r"
git -C "$r" config core.quotepath false
expect_exit 1 "also when the repository's own configuration says not to quote" run_range "$r"
r=$(new_repo backslash docs/a.md)
tag_base "$r"
printf 'y\n' > "$r/docs/a\\b.md"; commit_all "$r"
expect_exit 1 "a backslash in a name is refused" run_range "$r"

# ---- ranges, refs and errors ---------------------------------------------------------------------------
r=$(new_repo ranges docs/a.md SECURITY.md)
tag_base "$r"
echo y >> "$r/docs/a.md"; commit_all "$r" one
tip=$(git -C "$r" rev-parse HEAD)
echo y >> "$r/SECURITY.md"; commit_all "$r" two
expect_exit 1 "base and head as two arguments" "$cp_" --repo "$r" base HEAD
expect_exit 0 "only the first commit does not touch it" "$cp_" --repo "$r" base "$tip"
expect_exit 0 "from the first commit to its tip" "$cp_" --repo "$r" "$tip..$tip"
expect_exit 1 "relative refs" "$cp_" --repo "$r" "HEAD~1..HEAD"
git -C "$r" checkout -q -b side base
echo y >> "$r/docs/a.md"; commit_all "$r" side
git -C "$r" checkout -q main
expect_exit 0 "three dots compare with the merge base, not with what main did meanwhile" "$cp_" --repo "$r" "base...side"
expect_exit 1 "two dots compare the tips, so main's own change shows up" "$cp_" --repo "$r" "side..main"
expect_exit 2 "an unknown ref is an error, not a pass" "$cp_" --repo "$r" "nonexistent..HEAD"
expect_exit 2 "a range that starts with a dash is refused" "$cp_" --repo "$r" -- "--output=$work/x..HEAD"
expect_exit 2 "an option as the range is refused" "$cp_" --repo "$r" "--stat"
expect_exit 2 "a range with shell metacharacters is refused" "$cp_" --repo "$r" 'base..HEAD;touch'
expect_exit 2 "no range" "$cp_" --repo "$r"
expect_exit 2 "a single ref without dots" "$cp_" --repo "$r" HEAD
expect_exit 2 "a directory that is not a repository" "$cp_" --repo "$work" base..HEAD
expect_exit 2 "a missing directory" "$cp_" --repo "$work/nope" base..HEAD
expect_exit 2 "an unknown option" "$cp_" --frobnicate base..HEAD
[ ! -e "$work/x" ] && ok || bad "--output was honoured"

# ---- git is not trusted: repository configuration cannot change what is seen --------------------------
r=$(new_repo hostile-config docs/a.md SECURITY.md)
tag_base "$r"
echo y >> "$r/SECURITY.md"; commit_all "$r"
git -C "$r" config diff.external "$work/never-run"
git -C "$r" config diff.renames true
git -C "$r" config core.fsmonitor "$work/never-run"
git -C "$r" config diff.textconv "$work/never-run"
printf '#!/bin/sh\ntouch %s/ran\n' "$work" > "$work/never-run"; chmod +x "$work/never-run"
expect_exit 1 "hostile repository configuration changes nothing" run_range "$r"
[ ! -e "$work/ran" ] && ok || bad "a configured program ran"

# ---- the protected list ------------------------------------------------------------------------------
r=$(new_repo lists docs/a.md x.txt)
tag_base "$r"
echo y >> "$r/x.txt"; commit_all "$r"
printf '{"paths": [\n{"path": "x.txt"}\n]}\n' > "$work/mine.json"
expect_exit 1 "a custom list is honoured" "$cp_" --repo "$r" --list "$work/mine.json" base..HEAD
printf '{"paths": [\n{"path": "y.txt"}\n]}\n' > "$work/other.json"
expect_exit 0 "and only that list" "$cp_" --repo "$r" --list "$work/other.json" base..HEAD
printf '{"paths": []}\n' > "$work/empty.json"
expect_exit 2 "a list without entries is an error, never everything allowed" "$cp_" --repo "$r" --list "$work/empty.json" base..HEAD
printf '{"paths": [\n{"path": "a b"}\n]}\n' > "$work/space.json"
expect_exit 2 "an entry with a space is unusable" "$cp_" --repo "$r" --list "$work/space.json" base..HEAD
printf '{"paths": [\n{"path": "[a]*"}\n]}\n' > "$work/bracket.json"
expect_exit 2 "an entry with a character class is unusable" "$cp_" --repo "$r" --list "$work/bracket.json" base..HEAD
printf '{"paths": [\n{"path": "/abs"}\n]}\n' > "$work/abs.json"
expect_exit 2 "an absolute entry is unusable" "$cp_" --repo "$r" --list "$work/abs.json" base..HEAD
printf '{"paths": [\n{"path": "../up"}\n]}\n' > "$work/up.json"
expect_exit 2 "an entry with .. is unusable" "$cp_" --repo "$r" --list "$work/up.json" base..HEAD
printf '{"paths": [\n{"path": "a", "x": {"path": "b"}}\n]}\n' > "$work/twice.json"
expect_exit 2 "two path keys on one line do not count as one entry" "$cp_" --repo "$r" --list "$work/twice.json" base..HEAD
expect_exit 2 "a missing list" "$cp_" --repo "$r" --list "$work/nope.json" base..HEAD
printf '{"paths": [\n{"path": "docs/**/a.md"}\n]}\n' > "$work/mid.json"
r2=$(new_repo mid docs/a.md)
tag_base "$r2"
echo y >> "$r2/docs/a.md"; commit_all "$r2"
expect_exit 1 "docs/**/a.md matches docs/a.md" "$cp_" --repo "$r2" --list "$work/mid.json" base..HEAD
r3=$(new_repo mid2 docs/x/y/a.md)
tag_base "$r3"
echo y >> "$r3/docs/x/y/a.md"; commit_all "$r3"
expect_exit 1 "and docs/x/y/a.md" "$cp_" --repo "$r3" --list "$work/mid.json" base..HEAD

# ---- every entry of the real list is enforced ----------------------------------------------------------
# For each entry build a concrete path that its glob matches, and see that the checker refuses it.
n=0
sed -n 's/.*"path": *"\([^"]*\)".*/\1/p' "$list" | while IFS= read -r glob; do
  concrete=$(printf '%s' "$glob" | sed 's|/\*\*$|/x/y.txt|; s|\*\*|x|g; s|\*|x|g; s|?|x|g')
  printf '%s\n' "$concrete"
done > "$work/concrete.txt"
entries=$(grep -c '"path"' "$list")
lines=$(wc -l < "$work/concrete.txt" | tr -d ' ')
expect_eq "one concrete path per entry" "$entries" "$lines"
while IFS= read -r p; do
  n=$((n + 1))
  r=$(new_repo "real-$n" docs/a.md)
  tag_base "$r"
  mkdir -p "$r/$(dirname "$p")"; echo y > "$r/$p"; commit_all "$r"
  expect_exit 1 "entry $n of the real list is enforced: $p" run_range "$r"
done < "$work/concrete.txt"
# and the paths that tasks are meant to change are not protected by the real list
r=$(new_repo real-free README.md)
tag_base "$r"
for p in README.md docs/neo.md docs/domain.md docs/parity.md examples/plan.example.toml crates/fleet-domain/src/dag.rs crates/fleet-domain/src/error.rs crates/fleet-domain/src/status.rs crates/neo-knowledge/src/fact.rs crates/neo-knowledge/tests/props.rs crates/neo-governor/src/budget.rs crates/neo-broker/src/lease.rs; do
  mkdir -p "$r/$(dirname "$p")"; echo y > "$r/$p"
done
commit_all "$r"
expect_exit 0 "the paths the roadmap lets tasks change are not protected" run_range "$r"

# ---- reviewed exceptions (--except) -----------------------------------------------------------------------
X=crates/fleet-infra/src/server.rs
r=$(new_repo except $X docs/a.md)
tag_base "$r"
echo y >> "$r/$X"; commit_all "$r"
expect_exit 1 "without the exception the file is protected" run_range "$r"
expect_exit 0 "with the exact path as an exception it passes" "$cp_" --repo "$r" --except "$X" base..HEAD
expect_out "and the exception is printed, not hidden" "REVIEWED EXCEPTION: $X"
expect_out "and named at the end" "exceptions: $X"
echo y >> "$r/crates/fleet-infra/src/git.rs" 2>/dev/null || { mkdir -p "$r/crates/fleet-infra/src"; echo y > "$r/crates/fleet-infra/src/git.rs"; }
commit_all "$r"
expect_exit 1 "a second protected file is still refused" "$cp_" --repo "$r" --except "$X" base..HEAD
expect_exit 0 "unless it is excepted too" "$cp_" --repo "$r" --except "$X" --except crates/fleet-infra/src/git.rs base..HEAD

r=$(new_repo except-case $X)
tag_base "$r"
mkdir -p "$r/crates/fleet-infra/src"; echo y > "$r/crates/fleet-infra/src/SERVER.rs"; commit_all "$r"
expect_exit 1 "a path that differs in case is not excepted" "$cp_" --repo "$r" --except "$X" base..HEAD
r=$(new_repo except-other $X)
tag_base "$r"
echo y >> "$r/SECURITY.md"; commit_all "$r"
expect_exit 1 "an exception for one file does not excuse another" "$cp_" --repo "$r" --except "$X" base..HEAD
r=$(new_repo except-link $X)
tag_base "$r"
rm "$r/$X"; ln -s /etc/passwd "$r/$X"; commit_all "$r"
expect_exit 1 "a symlink in place of the excepted file is still refused" "$cp_" --repo "$r" --except "$X" base..HEAD
expect_out "as a symlink" "SYMLINK"
r=$(new_repo except-dir $X)
tag_base "$r"
mkdir -p "$r/crates/fleet-infra/src/extra"; echo y > "$r/crates/fleet-infra/src/extra/new.rs"; commit_all "$r"
expect_exit 1 "an exception is one file, not the directory around it" "$cp_" --repo "$r" --except "$X" base..HEAD

for bad in Cargo.toml Cargo.lock crates/fleet-infra/Cargo.toml crates/fleet-infra/build.rs crates/fleet-cli/tests/architecture.rs \
           'crates/fleet-infra/src/*.rs' 'crates/**' crates/fleet-infra/src/ crates/fleet-infra/src/../../x.rs /crates/a/src/x.rs \
           dogfood/check-protected.sh scripts/secret-scan.sh SECURITY.md .github/workflows/ci.yml 'crates/a/src/x y.rs' crates/a/src/x.md ''; do
  expect_exit 2 "no exception can be made for: '$bad'" "$cp_" --repo "$r" --except "$bad" base..HEAD
done
expect_exit 2 "--except needs a value" "$cp_" --repo "$r" --except
r=$(new_repo except-ok docs/a.md)
tag_base "$r"
echo y >> "$r/docs/a.md"; commit_all "$r"
expect_exit 0 "an exception that is not needed does no harm" "$cp_" --repo "$r" --except "$X" base..HEAD

finish
