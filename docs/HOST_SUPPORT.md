# Development-host support

wscrpt is installed and executed on the development host. The iPad or other
thin client connects to that host over SSH or mosh; it does not run the Rust
editor locally.

## Support matrix

| Host | Status | Evidence and boundary |
| --- | --- | --- |
| macOS | Supported | Stable Rust verification runs on macOS CI. The current native-arm64 development host passes the local build/test/package gate. |
| Linux | Supported | Stable and Rust 1.88 jobs run on Ubuntu with tmux required, including the Unix PTY integration suite. The preview sidecar is Node 22+ and uses a POSIX shell. |
| Windows with WSL 2 | Viable Windows route; acceptance open | Install wscrpt, OpenSSH, and tmux inside the Linux distribution. From the iPad, SSH to the WSL endpoint and use the same Linux workflow. Networking, sleep/reconnect, and real iPad acceptance still need a recorded pass on the chosen machine. |
| Native Windows | Not supported yet | The Rust code has non-Unix fallbacks in several subsystems, but workspace-shell selection, preview control, tmux, PTY behavior, and descendant-process cleanup do not have a Windows-native release gate. |

The native iPad terminal/player is remote-OS neutral at the SSH protocol layer:
it requests a PTY, opens a shell, and creates loopback `direct-tcpip` forwards.
Its current launch and preview-control commands intentionally target a POSIX
shell, so a Mac, Linux machine, or WSL 2 distribution is required.

## Host prerequisites

- Rust 1.88 or newer to install from source.
- A UTF-8 locale and interactive terminal.
- OpenSSH server for remote iPad use.
- tmux for reconnectable sessions and the documented release route.
- `git` for version-control inspection; `rg` is optional.
- Node 22+, Chrome/Chromium, and a direct LAN/Tailscale media route only when
  using the browser-game preview sidecar.

Install the exact reviewed checkout:

```sh
cargo install --path . --locked --force
wscrpt --version
wscrpt --health
```

The Cargo binary directory must be present in the login shell's `PATH`. Verify
from a directory outside the checkout rather than relying on the current shell:

```sh
cd /tmp
command -v wscrpt
wscrpt --health
```

On macOS, `/usr/bin/w` is an unrelated system command; use the unambiguous
`wscrpt` executable. The optional `w` compatibility workflow is separate from
the product binary.

## Release evidence

`.github/workflows/verify.yml` is the source of truth for the supported host
matrix. `scripts/verify.sh` runs formatting, strict Clippy, all targets, doc
tests, packaging, an isolated install, and noninteractive binary probes. Linux
sets `WSCRPT_REQUIRE_TMUX=1`, so an unavailable or unusable tmux route is a
failure rather than a skip.

Automation does not replace the physical route:

```text
iPad + Magic Keyboard -> Blink -> mosh/SSH -> tmux -> macOS/Linux/WSL host
```

Record that pass with `docs/IPAD_BLINK_QA.md`. For the combined terminal/player,
retain the additional real-device SSH, WebRTC, lifecycle, and performance gates
in `docs/NATIVE_IPAD_WORKSPACE.md`.
