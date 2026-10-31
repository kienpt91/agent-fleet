#!/bin/sh
# Quality gate used by the demo: a CHANGELOG.md, when present, must have a "Release notes" heading.
if [ -f CHANGELOG.md ] && ! grep -q '^# Release notes' CHANGELOG.md; then
  echo "CHANGELOG.md is missing the '# Release notes' heading"
  exit 1
fi
echo "changelog ok"
