# 8. Few dependencies, exact pins, dated

Status: accepted

## Decision
serde and serde_json (config and wire documents), sha2 (digests and the chain), toml (config), portable-pty
(ptys), nix (safe wrappers for signals, peer credentials, flock, openat), ratatui and crossterm (dashboard).
No async runtime, no web framework, no CLI framework, no regex engine. Every version in `Cargo.lock` was
published at least a week before the cutoff, versions are pinned with `=`, and the lock file is committed.
Crates with build scripts and procedural macros are listed in SECURITY.md.
