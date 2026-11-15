# Security Policy

## Supported Versions

| Version | Supported |
| ------- | --------- |
| 2.3.x   | ✅ |
| < 2.3   | ❌ |

Only the latest release line receives security fixes. If you are on an older
version, please upgrade to the latest release first and check whether the
issue still reproduces.

## Reporting a Vulnerability

**Do not open a public issue for a security problem.** Pylon's whole job is
keeping a remote caller inside one folder — a bypass of that boundary is
exactly the kind of thing that should be reported privately first.

The preferred way is GitHub's **private vulnerability reporting** on this
repository (Security → Report a vulnerability). It goes straight to the
maintainer and stays private until a fix is ready.

If that is not available to you, open an issue with no technical details and
write only "security issue, please contact me" — the maintainer will reach
out to arrange a private channel.

Please include, if you can:

- what you expected the boundary to do, and what actually happened;
- the exact steps or input that reproduces it (a minimal `bridge-config.json`
  and the request you sent are ideal);
- the Pylon version (`/health` reports it) and your Windows version.

## What We Treat as In Scope

- Path-traversal escapes from the allowlisted root (`..`, absolute paths,
  drive letters, UNC paths, symlinks, NTFS alternate data streams, and
  case / trailing-dot variants of protected paths).
- Reading or writing `bridge-config.json` or `pylon-activity.log` through
  any API, including via a path that does not literally spell the filename.
- Running a command that is not on the allowlist, or running `cmd` /
  `powershell` / `pwsh` / `bash` / `sh` at any permission level.
- Shell-injection via `/api/run` arguments (`&&`, `|`, `>`, and friends
  reaching a shell).
- Token leakage: the token appearing in a response, a log line, or a file
  the bridge itself writes.

## What Is Out of Scope

- The allowlisted commands themselves executing code inside the allowlisted
  folder (`cargo run / test / check / build` run your code by design — that
  is the feature, not a bypass).
- Anyone who already has your token calling the API (protect the token the
  way you protect a house key; regenerate it from the settings dialog if
  you suspect a leak).
- Tailscale's own network security — that is Tailscale's responsibility.

## Disclosure

Once a report is confirmed, the maintainer will fix it, cut a release, and
credit the reporter in the release notes unless they ask not to be named.
Please give the fix a reasonable window before disclosing publicly.
