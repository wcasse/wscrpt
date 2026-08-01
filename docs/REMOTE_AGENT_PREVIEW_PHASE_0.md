# Remote agent preview: Phase 0 technical plan

Status: **approved for Phase 0 implementation on 2026-07-29**

This plan turns the agreed preview goals into a repository-specific validation
spike. It deliberately separates facts about the current application from the
proposed work.

## 1. Audit record

### Checkout and instructions

- The requested checkout `/path/to/wscrpt` is a symlink to
  the canonical Git worktree `/path/to/wscrpt`.
- The audited revision is `main` at
  `905d0102e7fb4f36a6bba253f230adbcf301a8d0`, aligned with `origin/main`.
- The existing untracked, empty file `demovid` is user-owned and must remain
  untouched.
- There is no `AGENTS.md` in the tracked tree, working tree, Git history
  available from this checkout, or checked parent directories. The requested
  instruction file therefore could not be read. The applicable repository
  guidance is in [CONTRIBUTING.md](../CONTRIBUTING.md),
  [ARCHITECTURE.md](ARCHITECTURE.md), and
  [CONTRIBUTOR_MAP.md](CONTRIBUTOR_MAP.md).

### Actual application framework

`wscrpt` is one Rust 2024/MSRV 1.88 command-line package and one binary. It is a
single-process terminal IDE, not an iPad-native app:

- `src/main.rs` requires interactive stdin/stdout, constructs `App`, enters a
  `TerminalSession`, and owns the event loop.
- `src/terminal.rs` owns Crossterm raw mode, alternate screen, bracketed paste,
  optional mouse capture, and exact restoration.
- `src/render.rs` builds terminal-cell rows and emits ANSI/Crossterm output. It
  has no pixel surface.
- `src/app.rs` is the runtime facade for UI, project, LSP, tasks, persistence,
  read-only Git, and background-service orchestration.
- There are no Swift, SwiftUI, UIKit, WebKit, HTML/JavaScript, WebRTC, HTTP, or
  WebSocket targets or dependencies in the tracked tree.

The current main loop coalesces as many as 64 terminal events per wake, uses a
16 ms minimum frame interval, polls at 20 ms while active and 100 ms while idle,
and differentially repaints changed terminal rows. Background-service redraws
are separately throttled to roughly 15 FPS to protect Blink/mosh responsiveness.
Those mechanisms are for text UI performance, not video.

### Actual SSH, mosh, terminal, and tmux architecture

SSH, mosh, and Blink are outside the application. The user establishes a remote
shell, optionally attaches tmux, and starts `wscrpt` on the development host.
Production code only observes `SSH_CONNECTION`, `MOSH_IP`/
`MOSH_CONNECTION`, and `TMUX` for diagnostics.

There is currently no:

- SSH client, server, exec channel, tunnel manager, key store, or known-hosts
  handling in this repository;
- host, agent, tmux-pane, browser-target, or preview-session registry;
- application-owned tmux lifecycle or discovery;
- embedded PTY or terminal emulator; or
- IPC/control channel to a long-running sidecar.

`Esc t t` / `:terminal` is a blocking full-screen shell handoff. The main
process checkpoints state, restores the TTY, starts `$SHELL` in the workspace,
waits for it to exit, and then re-enters the TUI. It is not a service manager.

tmux is an external persistence layer. `tests/tmux_smoke.rs` creates an isolated
tmux server and launches the editor in it, but it neither tests detach/reconnect
nor discovers an existing user session. Reconnect remains a human iPad gate in
[IPAD_BLINK_QA.md](IPAD_BLINK_QA.md).

The current session file is one XDG `session.toml` containing paths,
cursor/viewport state, recent files, bookmarks, and layout flags. It never
contains process, tmux, browser, media, or agent state. Phase 0 must not overload
that file.

### Actual UI composition

The renderer composes terminal rows in this order:

1. buffer-tab header;
2. optional left project sidebar;
3. source editor rows;
4. status row;
5. footer or prompt row; and
6. help/candidate text overlays.

All geometry is terminal columns and rows. A 960x540 or 1280x720 video cannot be
embedded in this renderer, and sending frames through it would put media back
onto SSH/mosh. Version 0.2 also deliberately removed the old embedded PTY,
terminal split, and terminal panel. Phase 0 must not resurrect those systems.

### Current dependency and test baseline

The Cargo package has twelve direct runtime dependencies and one development
dependency. It has no feature table, async runtime, networking stack, browser
automation stack, or media library.

Current test enumeration (listing, not a full execution during this audit):

- 586 library test functions, with one ignored subprocess helper;
- 6 binary test functions;
- 1 tmux integration test;
- 9 Unix outer-PTY integration tests; and
- 3 documentation tests.

CI covers stable Rust on Ubuntu/macOS and Rust 1.88 on Ubuntu. Linux requires a
working isolated tmux server; macOS may skip that smoke. There is no iOS,
WebKit, Chrome, media, performance, or hardware lane. `WSCRPT_PERF` reports only
one TUI frame duration and changed/total text rows.

## 2. Phase 0 decision boundary

The current binary has nowhere to host `WKWebView`. Therefore Phase 0 should
prove the media and control path in a **standalone iPad WebKit harness kept in
this repository**, while leaving the Rust TUI unchanged.

The harness is not yet the finished embedded wscrpt experience. Shipping an
embedded mini player beside the editor requires a later product decision:

1. create a native iPad wscrpt container that owns both a terminal/SSH client
   and `PreviewSurface`; or
2. keep Blink as the editor host and accept a companion Safari/WebKit surface.

There is no third option in the current architecture that embeds pixel video in
the terminal without violating the WebRTC/no-PTY requirement. Phase 0 must not
pretend the harness resolves that product-host decision.

Recommended review decision: approve the standalone WebKit harness for Phase 0
so the 24 FPS path can be measured now, then choose the production host with
real evidence rather than coupling the media spike to a new SSH terminal app.

## 3. Phase 0 scope and non-goals

### In scope

- Select the exact Chrome target and exact game canvas already controlled by an
  agent on the remote development host.
- Capture it with `canvas.captureStream(24)` and send the video track through a
  direct WebRTC peer connection.
- Display a view-only `<video autoplay muted playsinline>` in Safari/WebKit and
  in a minimal `WKWebView` harness.
- Prove mini 960x540 at sustained 24 FPS, expanded 1280x720 at sustained 24
  FPS, and 1280x720 at 30 FPS as headroom.
- Prove LAN latency below 250 ms and an automatic 12 FPS fallback.
- Ensure stale work is replaced/dropped rather than queued.
- Use SSH/tmux for daemon lifetime, authenticated discovery, control, and an
  SSH-forwarded signaling connection.
- Keep lifecycle control as local `previewctl` execution over authenticated SSH;
  bind CDP and the signaling HTTP/WebSocket listener to loopback only.
- Preserve a CDP JPEG provider as a time-boxed diagnostic/fallback lane that is
  explicitly outside the production 24 FPS claim.

### Out of scope

- User input, game control, audio, microphone/camera capture, multi-viewer
  broadcast, recording, or replay.
- Public exposure of Chrome debugging, preview control, or signaling.
- TURN/public-Internet operation. Phase 0 is LAN/Tailscale-host-candidate proof.
- Unreal Pixel Streaming implementation. The provider contract must leave room
  for it.
- IINA, mpv, VLC, FFmpeg playback, GStreamer playback, transcoding, or a
  general-purpose media-player stack.
- Video frames, JPEGs, SDP, or ICE candidates in the terminal renderer,
  Crossterm event loop, `ServiceCoordinator`, normal PTY byte stream, or
  `session.toml`.
- Changes to `src/app.rs`, `src/render.rs`, `src/main.rs`, `src/session.rs`,
  `src/keymap.rs`, or `docs/COMMANDS.md` during the Phase 0 spike.

## 4. Proposed architecture

```text
 iPad: Preview WebKit harness                   Remote host: agent-owned browser
 +----------------------------------+           +-----------------------------+
 | PreviewSurface                   |           | exact Chrome target         |
 |  -> WebRtcPreviewProvider        | WebRTC    | exact canvas                |
 |     -> <video>                   |<--------->| canvas.captureStream(24)    |
 +----------------+-----------------+  media    | injected RTCPeerConnection  |
                  |                            +---------------+-------------+
                  | SSH-forwarded signaling                    | CDP bridge
                  v                                              v
          127.0.0.1:iPadPort                         127.0.0.1:RemoteHostPort
                                                        +------------------+
 authenticated SSH discovery/control ----------------->| previewd         |
 tmux process lifetime -------------------------------->| previewctl       |
                                                        +------------------+
```

Control is small metadata. Media is WebRTC/DTLS-SRTP directly between Chrome
and WebKit. The SSH tunnel carries HTTP/WebSocket signaling only; it does not
carry video frames. mosh does not replace this TCP tunnel, so the Phase 0 route
uses the normal mosh/tmux editor session plus a parallel SSH forwarding session.

### Source-side lifecycle

1. The agent starts a dedicated Chrome/Chrome-for-Testing profile with remote
   debugging bound to `127.0.0.1` and an explicit non-default user-data
   directory. `previewd` refuses non-loopback CDP addresses.
2. The agent registers the canonical workspace, tmux session/pane, explicit CDP
   target ID, expected page URL pattern, and explicit canvas selector. Missing
   or multiple matches fail closed.
3. `previewctl ensure` starts or rediscovers one `previewd` session under a
   deterministic tmux name. tmux supplies process lifetime only; discovery is
   the private runtime manifest read through the local CLI, never pane scraping.
4. `previewd` attaches to the selected target using CDP. It installs a fixed,
   versioned sender script and a `Runtime.addBinding` bridge. The page does not
   receive a general CDP proxy or arbitrary commands from the client.
5. The sender resolves the selected canvas, records its real dimensions, calls
   `canvas.captureStream(24)`, and adds the video-only track to one
   `RTCPeerConnection`.
6. Offer/answer/ICE messages move through the CDP binding, `previewd`, and the
   SSH-forwarded signaling WebSocket. The selected LAN ICE candidate pair is
   recorded. No STUN/TURN server is required for the Phase 0 LAN gate.

### Receiver lifecycle

1. `previewctl describe --issue-token --json` is invoked through authenticated
   SSH. It returns a short-lived one-use token and the remote loopback signaling
   port. Tokens are not written to the normal manifest or logs.
2. Blink establishes a parallel SSH local forward from an iPad loopback port to
   the remote host's loopback signaling port. Cross-app loopback access and forward
   survival must pass the preflight below before the media spike proceeds.
3. For the harness only, the operator supplies the forwarded local port and the
   one-use descriptor through a strict `wscrpt-preview://attach#...` test deep
   link. The harness constructs the forwarded loopback URL. The token remains
   in a fragment and is sent in the first WebSocket message, not in an HTTP
   request line. This manual handoff is test plumbing, not the production
   discovery experience.
4. `PreviewSurface` selects `WebRtcPreviewProvider`. The returned `MediaStream`
   is assigned directly to `video.srcObject`; the app does not decode, copy, or
   queue frames.
5. `requestVideoFrameCallback` and WebRTC stats provide 1 Hz metrics. Swift only
   receives low-rate state/metrics through the script-message bridge; no frame
   payload crosses it.

## 5. Proposed interfaces

### Ephemeral session descriptor

The private on-disk manifest is mode 0600 in a mode 0700 runtime directory. It
contains no reusable credential:

```json
{
  "protocolVersion": 1,
  "sessionId": "p-0123456789abcdef...",
  "ensureKey": "sha256-derived deterministic identity",
  "runId": "per-launch UUID",
  "generation": 3,
  "activeGeneration": 3,
  "workspace": {
    "canonicalRoot": "/srv/project",
    "revision": "abc1234"
  },
  "tmux": { "session": "wscrpt-preview-019d", "pane": "%12" },
  "target": {
    "id": "CDP_TARGET_ID",
    "urlHash": "sha256:...",
    "canvasSelector": "canvas#game",
    "sourceWidth": 1280,
    "sourceHeight": 720
  },
  "signaling": { "host": "127.0.0.1", "port": 7331, "path": "/signal" },
  "state": "ready",
  "heartbeatAt": "2026-07-29T22:00:00Z"
}
```

The exact target ID, not a best-effort URL search, is the identity of the
captured browser session. Raw URLs/query strings, auth tokens, SDP, ICE
candidates, and browser cookies are excluded from normal logs and evidence.
`runId` is a non-secret launch-ownership nonce: daemon callbacks may update a
manifest only while it still matches, preventing an exited process from
clobbering a stop or immediate replacement.

### `previewctl`

Machine-readable commands, designed to be called through SSH exec or from the
agent's existing tmux shell:

```text
previewctl targets --cdp http://127.0.0.1:9222 --json
previewctl ensure --workspace PATH --tmux-pane PANE --target-id ID \
  --canvas-selector SELECTOR --json
previewctl list --workspace PATH --json
previewctl describe --session ID --issue-token --json
previewctl status --session ID --json
previewctl stop --session ID --json
```

`ensure` is idempotent. `stop` affects only the named preview session and does
not close the agent's browser or tmux session unless that ownership is explicit
in its manifest.

### Browser surface/provider boundary

The browser reference implementation is the provider-neutral contract that can
run in Safari now and unchanged inside `WKWebView`:

```js
class PreviewSurface {
  async open({ session, provider, profile, signal }) {}
  async setPresentation(presentation) {} // "mini" | "expanded"
  close() {}
}

class PreviewProvider {
  async connect({ session, profile, signal }) {
    return {
      stream,                       // MediaStream
      setProfile: async profile => {},
      sampleStats: async () => ({}),
      close: () => {}
    };
  }
}
```

`PreviewSurface` owns presentation, lifecycle state, error/reconnect UI, and the
video element. `WebRtcPreviewProvider` owns signaling and peer-connection
details. A later Unreal provider implements the same contract without changing
surface composition.

### Native harness boundary

The Swift harness contains no SSH implementation in Phase 0. It consumes the
already-forwarded loopback descriptor:

```swift
@MainActor
protocol PreviewSurfaceController: AnyObject {
    var state: PreviewState { get }
    func attach(
        _ session: PreviewSessionDescriptor,
        presentation: PreviewPresentation
    ) async throws
    func setPresentation(_ presentation: PreviewPresentation)
    func detach()
}
```

`WKWebRTCPreviewSurface` uses an ephemeral `WKWebsiteDataStore`, allows only the
served loopback origin, disables link navigation and playback controls, and
loads the browser `PreviewSurface`. A future production iPad container supplies
an SSH-backed descriptor/tunnel coordinator above this interface.

### Signaling protocol

Versioned JSON messages:

```text
join, joined, offer, answer, ice, profile, stats, state, error, leave
```

Phase 0 permits one sender and one receiver per session. Enforce a 64 KiB
message limit, bounded candidate count, rate limits, monotonic generation, and
session/nonce checks. Reconnect replaces the previous receiver generation.
Queued messages for an old generation are discarded.

## 6. Quality profiles and stale-frame policy

| Profile | Encoded target | Capture/send target | Starting bitrate cap |
| --- | --- | --- | --- |
| `mini` | 960x540 | 24 FPS | 4 Mbit/s |
| `expanded` | 1280x720 | 24 FPS | 6 Mbit/s |
| `expanded-headroom` | 1280x720 | 30 FPS | 8 Mbit/s |
| `fallback` | 960x540 | 12 FPS | 1.5 Mbit/s |

These bitrates are initial spike controls, not product guarantees. Record the
negotiated codec and actual bitrate. Prefer hardware-efficient H.264 when both
peers negotiate it; accept VP8 if that is the working WebKit/Chrome pair.

The sender feature-probes `RTCRtpSender.setParameters`. Resolution is verified
from actual sender/receiver stats, not assumed from requested scaling. If exact
960x540 scaling is unsupported, the spike may use one bounded mirror canvas as
a documented fallback; failure to reach 1280x720 remains a failure of the
expanded gate.

For deterministic cadence changes, fallback replaces the current track with a
fresh `canvas.captureStream(12)` track. Recovery replaces it with a new
`canvas.captureStream(24)` track. The 30 FPS headroom test similarly uses
`captureStream(30)` and is never the default.

Adaptation samples once per second:

- degrade after three consecutive bad samples;
- recover only after ten consecutive good samples;
- enter fallback within 5 seconds of sustained poor conditions; and
- restore 24 FPS within 15 seconds of sustained recovery.

A sample is bad when presented FPS, frame age, packet-loss delta, or selected
candidate-pair RTT crosses the recorded limits; implementations must tolerate a
missing browser-specific stats field and use presented-frame data as the common
minimum.

There is no application video queue. WebRTC feeds `<video>` directly and the
browser compositor may drop late frames. Any asynchronous state/statistics path
is latest-value-wins. If measured frame age exceeds 500 ms for three samples,
the receiver tears down and creates a fresh peer generation instead of waiting
for latency to drain.

The optional CDP JPEG provider has a single replace-latest frame slot. It
acknowledges every `Page.screencastFrame`, displays only the newest undecoded
frame, and drops every superseded frame. Its metrics and UI are labeled
`diagnostic/fallback`; it cannot satisfy the 24 FPS production gate.

## 7. Exact proposed files

### New remote service and browser surface

| File | Responsibility |
| --- | --- |
| `previewd/package.json` | Private Node package, scripts, engine, exact dependency ranges. |
| `previewd/package-lock.json` | Reproducible transitive dependency lock. |
| `previewd/bin/previewctl.mjs` | Machine-readable lifecycle/discovery CLI. |
| `previewd/src/previewd.mjs` | Loopback HTTP/WebSocket daemon and session ownership. |
| `previewd/src/runtime-store.mjs` | 0700 directory, 0600 atomic manifests/tokens, heartbeat cleanup. |
| `previewd/src/protocol.mjs` | Versioned, bounded signaling validation. |
| `previewd/src/cdp-session.mjs` | Explicit target attach, fixed injection, navigation reattach. |
| `previewd/src/adaptation.mjs` | Hysteretic 24/12 profile controller. |
| `previewd/src/cdp-jpeg-source.mjs` | Optional capacity-one diagnostic screencast source. |
| `previewd/injected/canvas-sender.mjs` | Canvas capture and sender-side peer connection/CDP binding. |
| `previewd/public/index.html` | Autoplay, muted, plays-inline receiver shell. |
| `previewd/public/preview-surface.mjs` | Provider-neutral surface and mini/expanded layout. |
| `previewd/public/webrtc-provider.mjs` | WebRTC receiver/signaling implementation. |
| `previewd/public/jpeg-provider.mjs` | Clearly labeled diagnostic fallback. |
| `previewd/public/metrics.mjs` | Presented-frame, WebRTC, freeze, and latency JSONL metrics. |
| `previewd/fixtures/clock-game.html` | Deterministic 1280x720 animated canvas fixture. |
| `previewd/fixtures/clock-game.mjs` | Sequence/flash telemetry for latency/stale-frame proof. |

### New iPad harness

| File | Responsibility |
| --- | --- |
| `clients/ipad-preview-harness/PreviewHarness.xcodeproj/project.pbxproj` | Minimal iPad application project generated by Xcode. |
| `clients/ipad-preview-harness/PreviewHarness/PreviewHarnessApp.swift` | Harness composition root. |
| `clients/ipad-preview-harness/PreviewHarness/Info.plist` | Test URL scheme, scoped local-network explanation, and local-only transport policy. |
| `clients/ipad-preview-harness/PreviewHarness/PreviewLaunchConfiguration.swift` | Strict test deep-link parsing; rejects non-loopback player URLs. |
| `clients/ipad-preview-harness/PreviewHarness/PreviewSessionDescriptor.swift` | Strict ephemeral descriptor model and loopback validation. |
| `clients/ipad-preview-harness/PreviewHarness/PreviewSurface.swift` | Mini/expanded SwiftUI composition and lifecycle state. |
| `clients/ipad-preview-harness/PreviewHarness/WKWebRTCPreviewSurface.swift` | `WKWebView` implementation and low-rate metrics bridge. |
| `clients/ipad-preview-harness/PreviewHarnessTests/PreviewDescriptorTests.swift` | Decode/security/profile tests. |
| `clients/ipad-preview-harness/PreviewHarnessTests/PreviewSurfaceTests.swift` | Attach, replace, teardown, and presentation tests. |

### New validation and evidence files

| File | Responsibility |
| --- | --- |
| `previewd/test/protocol.test.mjs` | Schema, bounds, generation, token, and isolation tests. |
| `previewd/test/runtime-store.test.mjs` | Permissions, atomic publication, expiry, and ownership tests. |
| `previewd/test/previewctl-lifecycle.test.mjs` | Concurrent ensure/describe/stop and restart-token regressions. |
| `previewd/test/previewd-startup.test.mjs` | Transactional startup rollback and superseded-run regressions. |
| `previewd/test/target-selection.test.mjs` | Exact target/canvas fail-closed tests. |
| `previewd/test/adaptation.test.mjs` | 24/12 hysteresis and timing tests. |
| `previewd/test/jpeg-latest-frame.test.mjs` | Capacity-one stale JPEG drop. |
| `previewd/test/webrtc.e2e.test.mjs` | Two local Chromium targets and deterministic canvas smoke. |
| `scripts/verify-preview-phase0.sh` | Node tests, loopback assertions, Chrome smoke, artifact summary. |
| `docs/REMOTE_AGENT_PREVIEW_PHASE_0_RESULTS.md` | Real-route evidence template; initially contains no claimed result. |
| `.github/workflows/preview-phase0.yml` | Unit/Chromium functional smoke only; never a hardware/FPS claim. |

### Existing files changed only when implementation is approved

| File | Planned change |
| --- | --- |
| `.gitignore` | Ignore `previewd/node_modules/` and `output/preview-phase0/`. |
| `Cargo.toml` | Exclude `previewd/`, `clients/`, and validation output from the crates.io package. Do not add preview dependencies to the Rust binary. |
| `README.md` | Add the preview command/route only after a real iPad gate passes. |
| `docs/ARCHITECTURE.md` | Record the sidecar/media/control boundary after review. |
| `SECURITY.md` | Record loopback CDP, token, tunnel, log-redaction, and ICE rules. |
| `scripts/verify.sh` | Keep the existing Rust gate intact; optionally invoke only fast preview unit tests if Node is explicitly made a release prerequisite. |

## 8. Dependencies

Phase 0 uses a small Node sidecar because the browser already supplies the
WebRTC media implementation:

- Node.js, minimum version selected after checking the remote host; target Node 22 or
  newer for the spike;
- `chrome-remote-interface` `0.34.x` for explicit CDP target attachment; and
- `ws` `8.21.x` for the loopback signaling WebSocket.

Use Node built-ins for HTTP, crypto, filesystem, CLI parsing, and tests. Commit
the lockfile. Do not add `wrtc`, an SFU, a transcoder, or a native media player.
The iPad harness uses Apple SwiftUI/WebKit only and adds no third-party package.
Its `Info.plist` may allow local networking for the loopback player and explain
the LAN WebRTC connection, but must not enable arbitrary cleartext loads.

This keeps the existing Cargo dependency graph and crates.io release artifact
unchanged. A Rust `previewd` can be reconsidered after the protocol and metrics
are proven; rewriting the spike in Rust is not a Phase 0 success condition.

## 9. Implementation sequence after approval

### P0.0: route and security preflight

1. Confirm remote-host OS/architecture, Node version, Chrome version, hardware
   encoding availability, tmux version, LAN/Tailscale addresses, and firewall.
2. Launch a dedicated Chrome profile with CDP on loopback. Verify with `lsof` or
   `ss` that no non-loopback listener exists and verify the CDP port from another
   LAN device is unreachable.
3. Serve a static loopback page through an SSH local forward. On the real iPad,
   prove Safari and the WebKit harness can reach that forward for 15 minutes
   while switching to/from Blink. If the forward dies when Blink backgrounds,
   stop: the production architecture needs an in-app SSH tunnel or a revised
   signaling boundary.
4. Establish a direct WebRTC data-only peer connection and record the selected
   candidate pair. If no routable LAN/Tailscale candidate works, stop and make
   TURN a separately reviewed requirement; do not move media into SSH.

### P0.1: exact target and diagnostic image proof

1. Implement the runtime store, `previewctl targets/ensure/describe/stop`, and
   exact target/canvas selection.
2. Optionally time-box the CDP JPEG provider to one day to prove that the
   selected target and displayed session are identical.
3. Demonstrate replace-latest behavior under a deliberately slow receiver.
   This proves selection and staleness semantics, not production FPS.

### P0.2: minimum WebRTC vertical slice

1. Implement the fixed CDP signaling bridge and page sender.
2. Implement the browser `PreviewSurface` and `WebRtcPreviewProvider`.
3. Run the deterministic 1280x720 fixture from the agent-owned Chrome target.
4. On the actual iPad, hold 960x540 at 24 FPS for ten minutes and save metrics.
5. Attach to one real agent-controlled browser game target and record an agent
   action appearing in the iPad surface. The target ID and canvas selector must
   match the descriptor.

P0.2 is the smallest acceptable end-to-end spike. A CDP JPEG demo alone is a
failed Phase 0 result.

### P0.3: expanded, headroom, adaptation, and WebKit harness

1. Add expanded and headroom profiles and verify actual encoded dimensions.
2. Add deterministic 24/12 adaptation and fresh-peer recovery for stale age.
3. Add the minimal iPad harness and run the same browser `PreviewSurface` inside
   `WKWebView`.
4. Exercise a recorded network impairment profile without applying a broad,
   unscoped host network change. Any `tc netem` helper must require an explicit
   interface/test namespace and install a cleanup trap.
5. Fill the results document and record both automated metrics and the real
   human-on-iPad verdict.

## 10. Tests and acceptance gates

### Automated unit/security gates

- Reject non-loopback CDP and HTTP/WebSocket bindings; lifecycle control has no
  network listener and runs locally through authenticated SSH exec.
- Reject absent/ambiguous target IDs, page patterns, and canvas selectors.
- Reject expired/reused tokens, wrong session/generation, oversized messages,
  excessive ICE candidates, and cross-session signaling.
- Prove 0700 runtime directories, 0600 files, atomic descriptor publication,
  same-UID ownership, heartbeat expiry, and redacted logs.
- Prove idempotent `ensure`, rediscovery after the SSH caller exits, and scoped
  stop that leaves the browser and agent tmux process alive.
- Prove stale generations and superseded state/stat samples are discarded.
- Prove the JPEG diagnostic path retains at most one undecoded frame.
- Prove `PreviewSurface` has autoplay/muted/plays-inline/no-controls, replaces an
  old connection on attach, and stops tracks/peer connections on detach.
- Prove adaptation enters fallback after three bad samples and recovers only
  after ten good samples.
- Keep the existing Cargo format, Clippy, test, package, and isolated-install
  gate green.

### Local Chromium functional gate

- Launch a deterministic canvas source and separate receiver.
- Attach by exact target ID, negotiate WebRTC, display frames, reconnect after
  navigation, and stop cleanly.
- Verify requested versus actual dimensions/FPS from WebRTC stats.
- Verify a deliberately blocked receiver does not create an application queue.

This is CI evidence only. It cannot prove WebKit, iPad hardware, LAN latency,
thermal behavior, or human presentation quality.

### Real remote-host-to-iPad acceptance

Exclude the first ten seconds of each run as warm-up. Save 1 Hz JSONL metrics,
the browser/iPad/OS versions, codec, selected ICE pair, source/encoded
dimensions, git revision, target ID, canvas selector, and tmux identity.

| Gate | Required result |
| --- | --- |
| Mini baseline | 960x540 for 10 min; overall presented FPS >= 23.8; at least 95% of rolling 5 s windows >= 23.6 FPS; no freeze > 500 ms. |
| Expanded baseline | 1280x720 for 10 min with the same 24 FPS/freeze thresholds. |
| 30 FPS headroom | 1280x720 for 3 min; overall >= 29 FPS; at least 95% of rolling 5 s windows >= 28.5 FPS. |
| LAN latency | 100 fixture flash probes; p95 request-to-glass < 250 ms; record p50/p95/p99 and ICE RTT. |
| Adaptive fallback | Under the recorded impaired route, enter 960x540/12 within 5 s and present 11.5-12.5 FPS; restore 24 within 15 s after recovery. |
| No backlog | After impairment clears, measured frame age returns below 250 ms within 2 s. Sequence jumps are allowed; replay of queued old frames is not. |
| Exact session | Descriptor target ID/canvas plus synchronized agent action and iPad recording prove the displayed game is the controlled session. |
| View-only | No pointer, keyboard, touch, or gameplay input is sent from the preview surface. Test-only latency telemetry is disabled for the real game. |
| Lifecycle | Losing/recreating signaling replaces the peer generation cleanly; stopping preview leaves wscrpt, agent tmux, and the game browser session intact. |

The latency fixture uses a test-only data-channel request that changes a known
corner patch on the controlled canvas, then detects that patch when WebKit
presents it. The resulting request-to-glass duration is a conservative bound
that includes the request leg. It must not be enabled on the real game target.

Automated green checks do not replace the real iPad/WebKit run or a human visual
confirmation that the target is exact and the mini/expanded presentation is
usable.

## 11. Risk register

| Risk | Consequence | Phase 0 mitigation / decision |
| --- | --- | --- |
| No native iPad host in current wscrpt | Cannot truthfully call the player embedded in the current editor | Use the standalone harness for media proof; require a production-host decision before integration. |
| Blink-owned SSH forward is suspended or invisible cross-app | Signaling page fails although media design is sound | Make it the first real-iPad preflight; stop rather than expose signaling publicly. |
| LAN ICE candidates are filtered/mDNS-only or blocked by firewall | SSH signaling connects but media does not | Record candidate pair; test actual LAN/Tailscale route; review TURN separately if direct ICE fails. |
| Wrong browser tab/canvas | Preview is not the agent-controlled session | Explicit target ID + selector + URL pattern; fail closed; exact-session evidence. |
| Navigation replaces execution context | Stream silently freezes or switches identity | Generation heartbeat, `Page.addScriptToEvaluateOnNewDocument`, explicit revalidation, fresh peer. |
| A delayed old daemon tears down a replacement sender | Immediate restart loses media even though the new manifest is healthy | Per-launch `runId` ownership plus a distinct hashed CDP isolated-world name for every run. |
| Canvas is origin-tainted, OffscreenCanvas-only, smaller than target, or one of many | `captureStream` fails or target dimensions are false | Feature probe and explicit diagnostics; no silent screenshot substitution; expanded gate fails if source is insufficient. |
| Background tab throttling | Source cadence misses 24/30 | Dedicated Chrome profile, record visibility/throttling flags, use headed display, verify source and encoded FPS. |
| Encoder/codec/WebKit differences | 30 FPS headroom or exact scale fails | Record negotiated codec/hardware path; feature-probe parameters; real-device gate. |
| WebKit autoplay/lifecycle/thermal limits | Local Chromium passes but iPad fails | Muted/plays-inline/no-controls and repeated real-iPad sustained runs. |
| Browser jitter buffer grows latency | Stream remains smooth but late | Direct video element, no app queue, frame-age gate, and fresh-peer reset rather than draining. |
| CDP/token/SDP/ICE data leaks | Browser-session compromise or network disclosure | Loopback-only CDP/control/signaling, dedicated profile, one-use token, strict permissions, redacted evidence. |
| Node sidecar expands release/tooling surface | Rust crate/release becomes fragile | Isolated private package, committed lock, separate verification lane, Cargo package exclusions. |

## 12. Review gates before implementation

Implementation should begin only after explicit approval of these four points:

1. **Host boundary:** Phase 0 is a standalone Safari/WKWebView harness; embedding
   beside the current Blink-hosted terminal is not claimed.
2. **Sidecar choice:** `previewd` is a small Node/CDP/WebSocket service with
   browser-native WebRTC, not a new Rust/WebRTC/media stack inside `wscrpt`.
3. **Network boundary:** SSH-forwarded loopback signaling plus direct LAN
   WebRTC is the Phase 0 route; TURN is deferred unless the preflight proves it
   necessary.
4. **Success gate:** only a real remote-host-to-iPad WebKit result meeting the metrics
   above counts. CDP JPEG and desktop Chromium are supporting evidence only.

## Upstream basis for the spike

- [W3C Media Capture from DOM Elements](https://www.w3.org/TR/mediacapture-fromelement/)
  defines `HTMLCanvasElement.captureStream(frameRequestRate)` and its
  origin-clean restriction.
- [W3C WebRTC](https://www.w3.org/TR/webrtc/) and
  [WebRTC Stats](https://www.w3.org/TR/webrtc-stats/) define peer connections,
  RTP/candidate-pair metrics, encoded/decoded/rendered frames, drops, dimensions,
  loss, and RTT.
- [Chrome DevTools Protocol Runtime](https://chromedevtools.github.io/devtools-protocol/tot/Runtime/)
  provides `Runtime.addBinding`; the
  [Page domain](https://chromedevtools.github.io/devtools-protocol/tot/Page/)
  provides the explicitly non-production `Page.startScreencast` diagnostic.
- [Chrome's remote-debugging security guidance](https://developer.chrome.com/blog/remote-debugging-port/)
  requires a non-default data directory for modern Chrome debugging and
  reinforces using a dedicated profile.
- [WebKit's WebRTC implementation note](https://webkit.org/blog/7763/a-closer-look-into-webrtc/)
  confirms `RTCPeerConnection` and data channels in web views; the
  [iOS video policy](https://webkit.org/blog/6784/new-video-policies-for-ios/)
  supports the muted/autoplay/plays-inline receiver shape.
- [`chrome-remote-interface`](https://www.npmjs.com/package/chrome-remote-interface)
  and [`ws`](https://www.npmjs.com/package/ws) are the only proposed runtime
  packages; exact installed versions must remain lockfile-controlled.
