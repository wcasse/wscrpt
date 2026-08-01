# Security policy

## Supported versions

Security fixes target the latest released version on the default branch. Pre-1.0 releases may require upgrading rather than backports.

## What wscrpt trusts (and what it does not)

wscrpt runs on a development host and can edit files, run configured tasks, and launch language servers. Treat an open workspace like any other local dev tool.

| Surface | Policy |
| --- | --- |
| Language servers | Launched only from the user-owned global config (`~/.config/wscrpt/config.toml`). Project files cannot silently enable executables. |
| Tasks | Defined in `.wscrpt/tasks.toml` as argument vectors (no shell string). Every run requires an explicit trust confirmation. |
| Git | Inspection is read-only. Stage/unstage current saved file and commit staged are fixed-argument, bounded, non-interactive workers that require trust every run because clean filters and commit hooks can execute repository code. Branch/network/destructive/arbitrary operations remain in the workspace shell. |
| Clipboard | Optional OSC 52 attempts can be disabled (`--no-osc52` or config). |
| Search / index | Filesystem traversal and result sizes are bounded; partial results are labeled. |
| Remote preview and native iPad container | Opt-in sidecar; exact browser target/canvas; authenticated SSH control; loopback-only debugging/signaling; strict host-key pinning; Keychain-backed client credentials; and a view-only receiver. |

Opening an untrusted repository should not auto-start untrusted language servers. It may still contain task definitions, Git filters, and commit hooks—do not approve task or Git trust prompts you have not reviewed.

## Reporting a vulnerability

Please **do not** open a public issue for exploitable vulnerabilities.

**Preferred:** GitHub private security advisories for this repo:

https://github.com/wcasse/wscrpt/security/advisories/new

If that form is unavailable, do not publish exploit details in an issue. Use a
private contact channel published by the repository owner.

Include:

- wscrpt version (`wscrpt --version`)
- host OS/arch
- whether the route is local, SSH, or mosh/tmux
- minimal reproduction steps
- impact (data loss, code execution, terminal escape, etc.)

You should receive an acknowledgment when the report is reviewed.

## Hardening tips for operators

- Keep language server `argv` minimal and absolute when possible.
- Review `.wscrpt/tasks.toml` before first trust approval in a new clone.
- Review repository filters/hooks before approving a Git operation; configured commit signing is refused in the non-interactive worker.
- Prefer full-screen shell (`Esc t t`) for interactive or privileged work.
- Run `wscrpt --health` on the real remote route before relying on clipboard or LSP.

## Remote preview Phase 0

The preview spike is not part of the Rust editor process or its release
artifact. When running it, preserve these boundaries:

- Start Chrome/Chromium with a dedicated non-default user-data directory and
  bind its debugging endpoint to `127.0.0.1`. Never publish or LAN-bind CDP.
- Bind `previewd` HTTP/WebSocket signaling to loopback. Reach it through an
  authenticated SSH local forward; mosh and terminal/PTY bytes never carry
  video, SDP, ICE candidates, or JPEG frames.
- Select an explicit CDP target ID, expected URL pattern, and canvas selector.
  Missing or ambiguous identity is an error, not a best-effort fallback.
- Store manifests in a mode-0700 directory and mode-0600 files. Join tokens are
  random, short-lived, one-use credentials and must not appear in HTTP request
  targets, normal manifests, process arguments, or logs. The Phase 0 harness
  carries one token in a deep-link/loopback URL fragment (never sent to HTTP),
  scrubs the fragment before async attach, and consumes the token on first join.
- Treat SDP, ICE candidates, raw target URLs, cookies, and browser storage as
  sensitive. Evidence should contain hashes or redacted summaries unless the
  raw value is indispensable and stored privately.
- Permit one view-only receiver per session during Phase 0. The receiver sends
  no keyboard, pointer, touch, microphone, camera, or gameplay input.
- WebRTC media is direct and encrypted with DTLS-SRTP. A peer necessarily learns
  the selected ICE route; Phase 0 is limited to the authenticated operator's
  LAN/Tailscale environment and does not add a public TURN service.
- Tear down and replace stale peer generations. Do not queue application video
  frames while waiting for a slow receiver.

The historical standalone iPad harness accepts only loopback player URLs, uses
an ephemeral WebKit data store, blocks external navigation, bridges low-rate
state/metrics only, and contains no SSH credentials or tunnel implementation.

The follow-on native iPad container keeps those WebKit constraints and adds an
in-app SSH client. First use requires explicit confirmation of the server's
SHA-256 host-key fingerprint; later connections require the exact host-and-port
pin and reject changed or damaged pins. Remembered passwords and generated
Ed25519 private identities stay in the data-protection Keychain with
`WhenUnlockedThisDeviceOnly`; successful foreground reads migrate legacy
`AfterFirstUnlockThisDeviceOnly` items. Passwords are also scoped to the exact
normalized host, port, and username. The native target does not register the
historical external deep-link scheme. Preview control exec output and terminal
I/O backlogs are bounded, and tunnel relays manually advance reads only while
their peers are writable. The local signaling listener binds and accepts only
exact `127.0.0.1`, allows at most 16 concurrent accepted sockets, and each
accepted socket opens one SSH `direct-tcpip` channel to the exact remote
loopback port. The status-discovered port and numeric-loopback host are
revalidated under the session lock before token replacement. Preview-control
operations serialize token-issuing exec commands, and a workspace-level
retirement barrier closes an old coordinator and transport before the next
connection epoch, so an older result cannot revoke a replacement attachment.
WebKit explicitly denies camera, microphone, device-orientation, and motion
permission requests. An opaque scene-level native shield covers sheets, alerts,
terminal, and player content whenever the app becomes inactive. See
[`docs/NATIVE_IPAD_WORKSPACE.md`](docs/NATIVE_IPAD_WORKSPACE.md) for the full
native security and lifecycle contract.
