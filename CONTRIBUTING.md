# Contributing to Pylon

Thanks for thinking about contributing. Pylon is a small, focused tool, and
the best contributions are the ones that keep it that way.

## How to Contribute

1. **Open an issue first** for anything bigger than a typo. Describe the
   problem, not just the fix you have in mind — the maintainer may already
   know a simpler way, or the change may conflict with a design decision
   (for example, the three red lines in the security model are not
   negotiable).
2. Fork the repo, create a branch, make your change.
3. Run the checks (below) and make sure everything is green.
4. Open a pull request against `main`, using the PR template.

## Development

```cmd
cargo check          :: most used — if it compiles, it's fine
cargo test           :: all green (Windows: 102 tests; Linux: 100)
cargo build --release
```

- On Linux only the core service layer compiles; the GUI is fully behind
  `#[cfg(windows)]` and needs no system libraries there.
- Windows-specific code is judged by a real `cargo check` on Windows.
  Cloud Linux cannot compile Windows-only code, so do not be surprised if
  a Windows-only change passes CI on Linux and fails on your machine —
  run `cargo check` on Windows before opening the PR.
- **After touching `ui/` (the Tauri frontend), run `node --check`** on the
  JavaScript before reporting anything as verified. A single stray character
  can kill the whole frontend silently, and a DOM-ID check alone will not
  catch it.
- Keep the version in sync in four places when you bump it: `Cargo.toml`,
  the `VERSION` constant in `src/bridge.rs`, `tauri.conf.json`, and the
  README title.

## What Gets Merged

- Bug fixes with a regression test, ideally one that fails before your fix
  and passes after it.
- Small, focused improvements that do not change the security model.
- Documentation fixes and translations.

## What Does Not Get Merged

- Anything that weakens the three red lines: no shell execution (`cmd` /
  `powershell` / `bash` …), no escaping the allowlisted folder, no remote
  modification of `bridge-config.json`.
- New network modes. Pylon is Tailscale-only by design; earlier relay / WS /
  TLS modes were deliberately removed.
- Large refactors or new features without a prior issue and maintainer
  agreement.

## Code Style

- Rust: `cargo fmt` defaults, no new warnings (`cargo check` must be
  warning-free).
- UI text is Chinese-first; keep user-facing strings in Chinese and update
  the English README when a user-facing behavior changes.
- If you add a field to a struct, grep the whole repo for `StructName {`
  literals and fill them all in — the test doubles do not cover every
  construction site.

## Reporting Security Issues

Do not open a public issue for a security problem. See [SECURITY.md](SECURITY.md).
