#!/bin/sh
# Drives the review workbench in a real headless Chromium against a real daemon: a scripted agent
# (examples/agents/workbench-agent.sh) leaves a repository with Markdown, an image, links, binary files,
# a rename, a large file and hostile names and contents; the page is loaded for each pane and for a split
# layout, the rendered DOM is checked (facts, not only screenshots) and screenshots are written to
# $WORKBENCH_SHOTS (default docs/img). Exits 1 when a check fails.
#
# For the disposable container only: the login token is passed to Chromium as part of its URL argument (visible
# in the process list while a page loads) and Chromium runs with --no-sandbox while it renders hostile content.
# Do not run it against a daemon you care about or on a shared machine.
#
# Usage: scripts/workbench-demo.sh [path-to-fleet-binary]   (inside the image of docker/Dockerfile.web)
set -eu
FLEET=${1:-/target/release/fleet}
HERE=$(cd "$(dirname "$0")/.." && pwd)
SHOTS=${WORKBENCH_SHOTS:-$HERE/docs/img}
WORK=$(mktemp -d)
export HOME="$WORK/home" XDG_STATE_HOME="$WORK/state"
mkdir -p "$HOME" "$SHOTS"
REPO="$WORK/repo"
FAILED=0

git init -q -b main "$REPO"
git -C "$REPO" config user.name demo
git -C "$REPO" config user.email demo@example.invalid
mkdir -p "$REPO/src"
printf '# fixture\n' > "$REPO/README.md"
seq 1 12 | sed "s/^/line /" > "$REPO/src/lib.txt"
git -C "$REPO" add -A
git -C "$REPO" commit -q -m "initial commit"

cat > "$WORK/fleet.toml" <<CONFIG
[daemon]
max_sessions = 2
default_worker = "scripted"
kill_grace_secs = 2

[env]
allow = ["PATH", "HOME", "LANG", "TERM"]

[workers.scripted]
command = "$HERE/examples/agents/workbench-agent.sh"
timeout_secs = 60
idle_timeout_secs = 30
CONFIG

"$FLEET" daemon --config "$WORK/fleet.toml" --web 127.0.0.1:0 --web-print-login-url 2> "$WORK/terminal.txt" &
DAEMON=$!
cleanup() { [ -z "${WORKBENCH_DOM:-}" ] || cp "$WORK"/dom-*.html "$WORKBENCH_DOM"/ 2>/dev/null; "$FLEET" shutdown >/dev/null 2>&1 || true; kill $DAEMON 2>/dev/null || true; rm -rf "$WORK"; }
trap cleanup EXIT
i=0; until "$FLEET" status >/dev/null 2>&1; do i=$((i + 1)); [ "$i" -lt 100 ] || exit 1; sleep 0.1; done
i=0; until grep -q 'login URL' "$WORK/terminal.txt"; do i=$((i + 1)); [ "$i" -lt 100 ] || exit 1; sleep 0.1; done
LOGIN=$(sed -n 's/.*login URL.*: //p' "$WORK/terminal.txt")
BASE=${LOGIN%%/#*}
TOKEN=${LOGIN##*#token=}
printf 'Authorization: Bearer %s\n' "$TOKEN" > "$WORK/bearer.hdr"

"$FLEET" run create --objective "Review what the agent left" --repo "$REPO" --id r1 >/dev/null
"$FLEET" task add --run r1 --id review --title "Leave a repository to review" >/dev/null
"$FLEET" start --run r1 >/dev/null
i=0; until "$FLEET" status --json | grep -q '"completed":1'; do
  i=$((i + 1)); [ "$i" -lt 600 ] || { echo "the task never completed" >&2; exit 1; }; sleep 0.1
done

echo "== the API, with curl (token as a bearer header)"
api() { curl -sS --max-time 20 -H @"$WORK/bearer.hdr" "$BASE$1"; }
echo "GET /api/repo/tree?task=r1/review&commit=head&path="
api "/api/repo/tree?task=r1%2Freview&commit=head&path=" | sed 's/},{/}\n{/g' | sed -n '1,4p' | cut -c1-200
echo "..."
echo "GET /api/repo/file?task=r1/review&commit=head&path=src/main.rs"
api "/api/repo/file?task=r1%2Freview&commit=head&path=src%2Fmain.rs" | cut -c1-300
echo
echo "GET /api/repo/diff?task=r1/review (statuses)"
api "/api/repo/diff?task=r1%2Freview" | grep -o '"path":"[^"]*","path_ok":[a-z]*,"similarity":[a-z0-9]*,"status":"[a-z_]*"' | head -n 20 || true
echo "GET /api/repo/blob (headers of the image)"
curl -sS -D - -o /dev/null --max-time 20 -H @"$WORK/bearer.hdr" "$BASE/api/repo/blob?task=r1%2Freview&commit=head&path=img%2Flogo.png" | tr -d '\r' | grep -iE '^(HTTP|content-type|x-content-type|content-security|content-disposition|cache-control)'
echo "GET /api/repo/blob of an svg with script (refused)"
curl -sS -i --max-time 20 -H @"$WORK/bearer.hdr" "$BASE/api/repo/blob?task=r1%2Freview&commit=head&path=evil.svg" | tr -d '\r' | sed -n '1p;$p' | cut -c1-200
echo "GET with a traversal path (refused)"
curl -sS -i --max-time 20 -H @"$WORK/bearer.hdr" "$BASE/api/repo/file?task=r1%2Freview&path=..%2F..%2Fetc%2Fpasswd" | tr -d '\r' | sed -n '1p;$p' | cut -c1-200

BROWSER=$(command -v chromium || command -v chromium-browser || true)
if [ -z "$BROWSER" ]; then echo "no Chromium here; the browser checks were not run" >&2; exit 1; fi
PROFILE="$WORK/profile"
enc() { printf '%s' "$1" | sed 's/%/%25/g; s|/|%2F|g; s/?/%3F/g; s/&/%26/g; s/=/%3D/g'; }

# shoot <name> <route> <pattern>...   patterns that start with ! must be absent, the others present
shoot() {
  name=$1; route=$2; shift 2
  url="$BASE/#token=$TOKEN&next=$(enc "$route")"
  "$BROWSER" --headless=new --no-sandbox --disable-gpu --disable-dev-shm-usage --user-data-dir="$PROFILE" \
    --window-size=1400,${SHOT_HEIGHT:-900} --screenshot="$SHOTS/workbench-$name.png" \
    --virtual-time-budget=15000 --dump-dom "$url" > "$WORK/dom-$name.html" 2> "$WORK/chromium-$name.err" || true
  for pattern in "$@"; do
    case $pattern in
      '!'*) p=${pattern#!}
        if grep -qE -- "$p" "$WORK/dom-$name.html"; then echo "  [$name] FOUND (must be absent): $p"; FAILED=1; else echo "  [$name] absent: $p"; fi ;;
      *) if grep -qF -- "$pattern" "$WORK/dom-$name.html"; then echo "  [$name] found: $pattern"; else echo "  [$name] MISSING: $pattern"; FAILED=1; fi ;;
    esac
  done
  # the page must stay inert whatever the repository holds
  scripts=$(grep -o '<script' "$WORK/dom-$name.html" | wc -l)
  if [ "$scripts" -ne 3 ]; then echo "  [$name] FAILED: $scripts script elements (3 expected)"; FAILED=1; fi
  for bad in '<img[^>]*onerror' '<svg' '<iframe' '<object' '<embed' '<form' '<a [^>]*href="(javascript|vbscript|data):' 'src="javascript:' ' onclick=' ' onload=' '<a [^>]*href="//' '<img[^>]*src="https?:' '<img[^>]*src="data:text'; do
    if grep -qiE -- "$bad" "$WORK/dom-$name.html"; then echo "  [$name] FAILED: forbidden markup $bad"; FAILED=1; fi
  done
  if grep -q "$TOKEN" "$WORK/dom-$name.html"; then echo "  [$name] FAILED: the token is in the page"; FAILED=1; fi
}

echo "== the real page in headless Chromium"
shoot explorer "/work/r1/review?open=explorer&layout=single" 'pane-explorer' 'tree-row' 'README.md' 'guide-link' 'blob.bin' 'src' 'unverified'
shoot file "/work/r1/review?open=file&path=src/main.rs&layout=single" 'pane-file' 'code-row' 'println!' 'let greeting'
shoot unified "/work/r1/review?open=diff&mode=unified&path=src/renamed.txt&layout=single" 'pane-diff' 'diff-body diff-unified' 'src/renamed.txt' 'renamed' 'diff-add' 'diff-del' 'class="dw"'
shoot split "/work/r1/review?open=diff&mode=split&path=src/renamed.txt&layout=single" 'pane-diff' 'diff-body diff-split' 'diff-srow' 'src/renamed.txt'
shoot markdown "/work/r1/review?open=markdown&path=README.md&layout=single" 'pane-markdown' 'md-doc' '<table' '<th>name' '<pre' 'fenced' '<ul' '<li' 'href="https://example.com/docs"' 'rel="noopener noreferrer"' 'Workbench fixture' '<img'
shoot image "/work/r1/review?open=image&path=img/logo.png&layout=single" 'pane-image' 'img-stage' 'src="data:image/png;base64,'
shoot image-huge "/work/r1/review?open=image&path=img/huge.png&layout=single" 'pane-image' 'Image too large to show' '40000' '!<img[^>]*src='
shoot file-cr "/work/r1/review?open=file&path=lone-cr.txt&layout=single" 'pane-file' 'first[U+000D]second'
shoot layout "/work/r1/review?layout=review&open=diff&mode=split" 'dock-h' 'dock-group' 'pane-explorer' 'pane-diff' 'role="separator"' 'role="tablist"'
SHOT_HEIGHT=1100
echo "== hostile names and contents driven through the real page"
shoot hostile-tree "/work/r1/review?open=explorer&layout=single" 'pane-explorer' '&lt;img src=x onerror=alert(1)&gt;.txt' '\u{202e}' '\u{1b}'
shoot hostile-md "/work/r1/review?open=markdown&path=evil.md&layout=single" 'pane-markdown' '&lt;script&gt;alert(1)&lt;/script&gt;' '&lt;img src=x onerror=alert(1)&gt;' 'href="https://example.com/ok"' '!<a [^>]*href="(javascript|data):' '!<script>alert'
shoot hostile-file "/work/r1/review?open=file&path=hostile-content.txt&layout=single" 'pane-file' '&lt;script&gt;alert(1)&lt;/script&gt;' '[U+001B]'
shoot hostile-diff "/work/r1/review?open=diff&mode=split&layout=single" 'pane-diff' 'onerror=alert(1)' 'hostile-content.txt'
SHOT_HEIGHT=900
echo "  occurrences of the token in the rendered pages (must be 0): $(cat "$WORK"/dom-*.html | grep -c "$TOKEN" || true)"
[ "$FAILED" -eq 0 ] || { echo "workbench demo: FAILED" >&2; exit 1; }
echo "workbench demo: ok"
ls -1 "$SHOTS"/workbench-*.png
