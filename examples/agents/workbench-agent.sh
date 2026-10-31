#!/bin/bash
# A scripted stand-in for a coding agent that leaves a repository full of the things the review
# workbench has to show: Markdown (table, code fence, list, links, an image), a PNG, an SVG with script,
# a symlink, a binary file, a rename with an edit, a mode change, a large file, and hostile names and
# contents. Used by scripts/workbench-demo.sh. Bash is needed for $'...' names.
set -e
mkdir -p src docs img .fleet
mv src/lib.txt src/renamed.txt
sed 's/^line 5$/line five changed here/' src/renamed.txt > src/renamed.new && mv src/renamed.new src/renamed.txt
cat > src/main.rs <<'EOF2'
fn main() {
    let greeting = "hello";
    println!("{greeting}, world");
}
EOF2
cat > README.md <<'EOF2'
# Workbench fixture

A **bold** claim, some `code`, and a [guide](docs/guide.md) next to a [site](https://example.com/docs).

| name | meaning |
| ---- | ------- |
| one  | first   |
| two  | second  |

- a list item
- another item with *emphasis*

```rust
fn main() { println!("fenced"); }
```

![the logo](img/logo.png)
EOF2
printf '# Guide\n\nSee the [readme](../README.md).\n' > docs/guide.md
echo 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==' | base64 -d > img/logo.png
printf '\0\1\2\0binary' > blob.bin
printf '<svg xmlns="x" onload="alert(1)"><script>alert(2)</script></svg>\n' > evil.svg
ln -s docs/guide.md guide-link
ln -s /etc/passwd absolute-link
printf '#!/bin/sh\necho hi\n' > run.sh; chmod +x run.sh
yes 'a long line of ordinary text that takes some room in a diff, line' | head -n 3000 > big.txt
cat > evil.md <<'EOF2'
# Hostile <script>alert(1)</script>

<img src=x onerror=alert(1)> and <a href="javascript:alert(2)">raw html</a>

[click](javascript:alert(3)) [data](data:text/html,<script>alert(4)</script>) [proto](//evil.example/x)
[ok](https://example.com/ok) [rel](docs/guide.md)

![pixel](data:image/png;base64,iVBORw0KGgo=) ![remote](https://tracker.example/p.png) ![local](img/logo.png)

\u001b[31mred\u001b[0m and a bidi override: abc\u202egnp.exe
EOF2
printf 'alert(1)\n<script>alert(1)</script>\n<img src=x onerror=alert(1)>\n\x1b[31mred\x1b[0m\n' > hostile-content.txt
: > $'we\nird\x1b[31mname.txt'
: > $'bidi\xe2\x80\xaetxt.exe'
: > '<img src=x onerror=alert(1)>.txt'
printf '{"version":1,"status":"done","summary":"left a repository for review"}' > .fleet/result.json
