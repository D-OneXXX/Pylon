## What this PR does

<!-- One or two sentences. Link the issue it closes, if any: "Closes #123". -->

## How I tested it

<!-- `cargo check` / `cargo test` output, and on which platform. If you touched `ui/`, did you run `node --check`? -->

- [ ] `cargo check` — zero warnings
- [ ] `cargo test` — all green (Windows: 102 / Linux: 100)
- [ ] `node --check` on any changed JS (or N/A — no UI change)

## Checklist

- [ ] This does not weaken the three red lines (no shell, no escaping the allowlisted folder, no remote `bridge-config.json` edits).
- [ ] A regression test is included, or I explain why one is not possible.
- [ ] The version is bumped in all four places if this is a release (`Cargo.toml`, `src/bridge.rs` `VERSION`, `tauri.conf.json`, README title).
- [ ] The English README is updated if a user-facing behavior changed.

## Notes for the reviewer

<!-- Anything that needs a second pair of eyes. -->
