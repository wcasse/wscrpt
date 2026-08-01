# Native iPad terminal and player workspace

Status: **native container implemented; device build and simulator launch have
supporting proof; physical iPad, Magic Keyboard, and remote-host performance gates
remain open**

This is the follow-on product container anticipated by
[the Phase 0 plan](REMOTE_AGENT_PREVIEW_PHASE_0.md). It hosts a real SSH
terminal and the existing WebRTC preview in one iPad app. It does not rewrite
the historical Phase 0 spike or turn simulator evidence into real-device
acceptance.

The Xcode target and scheme remain named `PreviewHarness` so existing project,
CI, and test references keep working. The installed app's display name is
`wscrpt`.

## Scope and ownership

One app scene owns exactly one of each stateful surface:

- a SwiftTerm `TerminalView` attached to an interactive SSH PTY;
- a `WKWebView` that receives the view-only gameplay preview;
- a NIOSSH connection used by the PTY, bounded preview-control exec channels,
  and an SSH `direct-tcpip` signaling forward; and
- a `PreviewCoordinator` that discovers, attaches, replaces, and detaches the
  current preview.

The Rust `wscrpt` binary still runs on the development host. The native app is
an SSH client and terminal emulator; it is not a Swift port of the editor. It
also does not add mosh, background socket execution, gameplay input, audio,
recording, or a public preview endpoint.

## Architecture

```text
iPad app: wscrpt (Xcode target: PreviewHarness)
+--------------------------------------------------------------------+
| CombinedWorkspaceView                                              |
|                                                                    |
|  stable SwiftTerm TerminalView        stable WKWebView             |
|           | PTY bytes + resize              | HTTP/WebSocket        |
|           v                                 v                      |
|                    one SSHTransport                                 |
|             session | exec | direct-tcpip                           |
+----------------------+----------+-----------------------------------+
                       | SSH      | SSH-forwarded signaling only
                       v          v
Remote development host
+--------------------------------------------------------------------+
| sshd -> PTY -> tmux new-session -A -> wscrpt                       |
| sshd -> previewctl list/status/describe -> previewd on 127.0.0.1   |
| agent tmux pane + exact Chrome target + exact canvas                |
+--------------------------------------+-----------------------------+
                                       |
                                       | direct WebRTC media
                                       +--------------------> WKWebView
```

The SSH forward carries preview HTTP/WebSocket signaling, not video frames.
After signaling, WebKit and the selected browser target exchange WebRTC media
directly over the available LAN or Tailscale candidate path.

### Connection and preview flow

1. The app validates a saved or entered remote profile and opens one NIOSSH
   connection.
2. The user confirms the host's first-use SHA-256 key fingerprint. Later
   connections require the exact saved algorithm and fingerprint.
3. The app requests an `xterm-256color` PTY, then either launches `wscrpt`
   directly or runs `tmux new-session -A` and launches it there.
4. On the same authenticated connection, `PreviewCoordinator` runs
   `previewctl list`. A single healthy session is attached automatically;
   multiple sessions require an explicit choice.
5. Attach rechecks the selected session with `previewctl status`, starts an
   ephemeral `127.0.0.1` listener on the iPad, and creates an SSH
   `direct-tcpip` channel to the remote loopback signaling port.
6. Only after that listener is bound does the app ask `previewctl describe` to
   mint a short-lived, one-use credential for the exact local port.
7. The strict descriptor is loaded into the persistent view-only `WKWebView`.
   Replacing or detaching a preview closes its receiver and local forward but
   does not stop the remote editor, browser, agent tmux pane, or preview daemon.

The terminal and WebKit objects live for the scene's lifetime. Rotation,
SwiftUI recomputation, and mini/expanded transitions change their frames, not
their identity. That preserves terminal scrollback, first-responder state, the
WebRTC receiver, and the one-use attachment.

## Security invariants

These are implementation contracts, not optional deployment advice:

1. **Host identity fails closed.** First use requires a native confirmation of
   the SSH key algorithm and OpenSSH-style SHA-256 fingerprint. The resulting
   pin is scoped to normalized host plus port. A changed key or damaged pin
   store is rejected; it is not treated as another first use. Verify the first
   fingerprint through a trusted route before accepting it.
2. **Secrets stay out of profiles.** Remote profile metadata and public host
   fingerprints use app-owned preferences. Remembered passwords and generated
   32-byte Ed25519 private identities use the data-protection Keychain with
   `WhenUnlockedThisDeviceOnly`. A successful foreground read migrates an
   older `AfterFirstUnlockThisDeviceOnly` item to the stronger accessibility
   class. A password record is bound to the exact
   normalized profile ID, host, port, and username, so editing an endpoint
   cannot reuse another server's password. Automatic resume is armed only
   after a successful PTY launch. An unremembered password may remain in
   process memory for that successful tmux reconnect lifecycle, but is not
   written to the profile store. Explicit Disconnect and exhausted reconnects
   clear that cache.
3. **Remote commands are bounded and quoted.** Host, port, username, workspace,
   preview-tools path, tmux session, preview session ID, relative `previewctl`
   path, PTY size, input chunk, command length, output size, and timeout all
   pass validation gates. Dynamic shell words are POSIX-quoted. Only a leading
   `~` in a workspace or preview-tools path receives deliberate `$HOME`
   expansion.
4. **Control and signaling stay on numeric loopback.** CDP accepts only a
   numeric loopback origin (IPv4 `127/8` or `::1`); the native signaling attach
   is narrower and requires remote `127.0.0.1`. The iPad listener binds
   `127.0.0.1:0`, accepts exact-loopback peers only, and forwards only to remote
   `127.0.0.1`. Do not replace these values with `0.0.0.0`, a LAN address, or
   even `localhost`.
5. **Attach uses current, healthy identity.** Session lists have a strict
   schema and size limit. Attach re-reads status, requires the current session
   ID, active heartbeat and tmux health, and passes the status-discovered
   signaling port back to `previewctl describe`. Under the same session lock,
   `previewctl` requires the exact remote port and numeric-loopback host before
   it revokes or issues a token. The issued descriptor must then repeat the
   bound local port, requested quality profile, and presentation.
6. **Preview credentials are ephemeral.** The one-use token is minted only
   after the forward is listening. The descriptor is protocol-versioned,
   size-limited, exact-schema, WebRTC-only, and accepts no unknown fields. The
   credential is placed in a URL fragment, never a query or cookie.
7. **WebKit is view-only and origin-locked.** It uses a non-persistent website
   data store, suppresses cookie handling on the initial navigation, disables
   AirPlay, picture-in-picture, scrolling, link previews, and user interaction,
   and permits only the exact loopback page and attachment URL. Script messages
   must come from the allowed main frame and
   use an exact shallow schema with bounded scalar fields. The native bridge
   accepts at most 64 messages per second and detaches on overflow. Camera,
   microphone, device-orientation, and motion permission requests are denied.
   No frame payload crosses the Swift message bridge.
8. **Terminal escape integrations fail closed.** OSC 52 clipboard read/write
   and output-driven URL opening are ignored. Gameplay input is not routed
   through the preview surface.
9. **Native attachments are app-owned.** The combined target does not register
   the historical `wscrpt-preview` custom URL scheme. A preview can load only
   after this scene's authenticated SSH coordinator has bound its exact
   loopback forward and issued the one-use descriptor.
10. **I/O backlogs are bounded.** Terminal input and output use single-owner,
    bounded pumps. Resize events coalesce. A terminal peer that exceeds its
    safety ceiling fails the SSH session visibly instead of silently dropping
    or retaining unbounded terminal bytes. Each tunnel direction separately
    disables automatic reads and pauses until the peer is writable before
    advancing one read, bounding signaling memory without turning normal
    backpressure into an SSH failure.
11. **Attachment replacement is ordered.** The coordinator keeps committed
    attachment identity separate from discovery/attach operation state. It
    retires a superseded presenter and forward together, and it waits for an
    older preview-control exec result before a newer token command can begin.
    A workspace-level retirement barrier also waits for the old coordinator and
    SSH transport before a new connection epoch may authenticate or mutate
    token state. A stale late command therefore cannot revoke a replacement
    one-use token.
12. **Inactive content is obscured.** An opaque scene-level privacy window sits
    above app sheets and alerts as soon as the scene leaves the active phase,
    before asynchronous SSH/WebRTC teardown completes. A root-view shield is a
    synchronous fallback. The loopback listener also caps concurrent accepted
    browser sockets at 16 before opening SSH children.

Keep tokens, descriptors, SDP, ICE candidates, raw browser URLs, cookies, and
private keys out of committed logs and screenshots. Use the redacted evidence
rules in [the Phase 0 results ledger](REMOTE_AGENT_PREVIEW_PHASE_0_RESULTS.md).

## UI and keyboard behavior

In mini mode the terminal occupies the workspace and the player floats at the
bottom right. Expanded mode uses a side-by-side split in landscape and a
terminal-above-player split in portrait. The terminal coalesces pending grid
changes to the latest PTY size and retains up to 10,000 scrollback lines.

SwiftTerm is configured for a monospaced 13-point font, `xterm-256color`, Option
as Meta, and terminal mouse reporting. Terminal input is disabled unless the
SSH state is connected, including during teardown/reconnect transitions. The
app focuses the terminal after a successful connection, flushes the latest grid
size after a slow authentication or host-key prompt, and does not resign it
merely because the player changes size.

Before opening SSH, the connection sheet checks iOS's coalesced physical
keyboard state. A detected keyboard permits the normal path; an absent or
unavailable state requires an explicit per-attempt acknowledgement of limited
software-keyboard mode. A later keyboard disconnect shows a nonmodal warning
without closing the SSH connection, tmux session, or gameplay player. This
detects the presence of a physical keyboard, not its make or model.

| Magic Keyboard shortcut | Action |
| --- | --- |
| Command-Option-T | Focus the terminal |
| Command-Option-P | Toggle mini/expanded player |
| Command-Option-V | Open gameplay preview discovery |
| Command-comma | Open remote connection settings |

These shortcuts and the simulator layout are implemented, but a simulator
does not validate physical Magic Keyboard behavior. Escape, arrows,
Shift-arrows, key repeat, Option/Meta, multiline paste, resize during typing,
and the complete `wscrpt` command contract remain real-device gates. Use the
matrix in [IPAD_BLINK_QA.md](IPAD_BLINK_QA.md) as a behavioral reference while
recording a separate result for this native SSH path.

## Dependencies

The app is iPad-only (`TARGETED_DEVICE_FAMILY = 2`) with an iOS 17.0 minimum.
Swift packages are locked in
`clients/ipad-preview-harness/PreviewHarness.xcodeproj/project.xcworkspace/xcshareddata/swiftpm/Package.resolved`.

| Direct package | Locked version | Purpose |
| --- | --- | --- |
| SwiftTerm | 1.15.0 | UIKit terminal emulator and hardware-key input |
| swift-nio-ssh | 0.15.0 | SSH authentication, PTY, exec, and direct-tcpip channels |
| swift-nio | 2.101.3 | TCP transport, event loop, and loopback listener |
| swift-crypto | 4.5.1 | Cryptographic package dependency; app identity uses Apple's CryptoKit API |

Local development requires Xcode 16.3 or newer, a Swift 6.1-or-newer
toolchain, and an iOS 17-or-newer SDK. The combined verification script also
requires Node 22 or newer and npm for `previewd`.

The checked-in target is ready for development signing, not App Store
submission. Before external distribution, add and review a production app icon,
the app privacy manifest, and a complete third-party notices/license artifact
for the locked Swift packages; then rerun archive, signing, and
store-validation gates.

## Remote prerequisites

Each remote profile has two independent paths. **Remote path** is the project
opened by `wscrpt .`. **Preview tools path** is the wscrpt checkout containing
`previewd/bin/previewctl.mjs`; it may be absolute, begin with `~/`, or be
relative to the project. The default `.` preserves the single-checkout case,
but a game project normally points the tools field at a separate wscrpt
checkout. The app changes into the project before running the tools command, so
`previewctl --workspace .` still resolves the exact project identity.

The development host must provide:

- macOS or Linux with OpenSSH. The native app does not use a Mac-only remote
  protocol; Linux is a first-class host. On Windows, use an OpenSSH-enabled
  WSL 2 distribution for this POSIX command path;
- reachable SSH with PTY, exec-channel, and `direct-tcpip` forwarding enabled;
- password authentication or the app-generated Ed25519 public key installed in
  the account's `~/.ssh/authorized_keys`;
- `wscrpt`, tmux, Node 22 or newer, npm, and Git, with `wscrpt` and `node`
  resolvable by the account's login shell;
- locked preview dependencies installed with
  `npm --prefix previewd ci --ignore-scripts --no-audit --no-fund`;
- a dedicated Chrome/Chromium profile with remote debugging bound only to
  `127.0.0.1`, plus one exact target ID and canvas selector;
- a live agent tmux pane and a healthy `previewctl ensure` session registered
  for the same canonical workspace; and
- a LAN or Tailscale route on which Chrome and the iPad can form a direct ICE
  candidate pair. SSH reachability alone does not prove this media route.

Native Windows is not currently claimed: preview discovery and launch use
POSIX quoting, `$HOME`, and tmux, and the release gate has no Windows-native
PTY/process-tree acceptance. See [`HOST_SUPPORT.md`](HOST_SUPPORT.md).

The latest read-only remote-host preflight succeeded through the Mac's configured
SSH identity. A `direct-tcpip` probe to the host's numeric-loopback SSH port
returned its OpenSSH 9.6 banner, which establishes that the server currently
permits the forwarding mechanism used by the player. It does **not** establish
NIOSSH authentication from the iPad. The route is not yet runnable: remote
`wscrpt`, `previewctl`, the expected BIRDWORLD workspace path, and authorization
of the iPad's generated public key are absent. The earlier Phase 0 preflight
also found no selected browser and found that non-login SSH commands did not
inherit the user Node toolchain path. Install and recheck every prerequisite
before treating server compatibility as end-to-end readiness.

## Build and test commands

Run commands from the repository root. Resolve the locked Swift packages, then
compile the app and test bundle for a generic physical iOS device:

```sh
native_packages="$(mktemp -d)"

xcodebuild \
  -resolvePackageDependencies \
  -project clients/ipad-preview-harness/PreviewHarness.xcodeproj \
  -scheme PreviewHarness \
  -onlyUsePackageVersionsFromResolvedFile \
  -clonedSourcePackagesDirPath "$native_packages/SourcePackages"

xcodebuild \
  -project clients/ipad-preview-harness/PreviewHarness.xcodeproj \
  -scheme PreviewHarness \
  -configuration Debug \
  -sdk iphoneos \
  -destination 'generic/platform=iOS' \
  -onlyUsePackageVersionsFromResolvedFile \
  -clonedSourcePackagesDirPath "$native_packages/SourcePackages" \
  -disableAutomaticPackageResolution \
  -derivedDataPath "$native_packages/DerivedData" \
  CODE_SIGNING_ALLOWED=NO \
  build-for-testing

xcodebuild \
  -project clients/ipad-preview-harness/PreviewHarness.xcodeproj \
  -scheme PreviewHarness \
  -configuration Release \
  -sdk iphoneos \
  -destination 'generic/platform=iOS' \
  -onlyUsePackageVersionsFromResolvedFile \
  -clonedSourcePackagesDirPath "$native_packages/SourcePackages" \
  -disableAutomaticPackageResolution \
  -derivedDataPath "$native_packages/ReleaseDerivedData" \
  CODE_SIGNING_ALLOWED=NO \
  build
```

Run the complete native suite on an installed iPad simulator by supplying its
UDID from `xcrun simctl list devices available`:

```sh
SIMULATOR_ID='PASTE-AVAILABLE-IPAD-SIMULATOR-UDID'
: "${native_packages:?run the locked package-resolve block in this shell first}"

xcodebuild \
  -project clients/ipad-preview-harness/PreviewHarness.xcodeproj \
  -scheme PreviewHarness \
  -configuration Debug \
  -destination "platform=iOS Simulator,id=$SIMULATOR_ID" \
  -onlyUsePackageVersionsFromResolvedFile \
  -clonedSourcePackagesDirPath "$native_packages/SourcePackages" \
  -disableAutomaticPackageResolution \
  -derivedDataPath "$native_packages/SimulatorDerivedData" \
  CODE_SIGNING_ALLOWED=NO \
  test
```

The focused evidence lanes can be reproduced with the same command plus:

```sh
-only-testing:PreviewHarnessTests/HostKeyTrustTests
```

or:

```sh
-only-testing:PreviewHarnessTests/PreviewCoordinatorTests \
-only-testing:PreviewHarnessTests/RemotePreviewControlTests
```

The repository verifier installs and tests the Node sidecar, optionally runs a
real local Chromium smoke, resolves Swift packages, and performs both the
generic Debug build-for-testing and optimized Release build:

```sh
WSCRPT_PREVIEW_CHROME='/path/to/Chrome' ./scripts/verify-preview-phase0.sh
```

Read its output carefully: the script may explicitly skip Chromium or Xcode
when their prerequisites are absent. A successful generic build is not a
simulator run, physical-device run, or remote-host-to-iPad acceptance result.

## Background, tmux, and reconnect semantics

iOS backgrounding is treated as a disconnect boundary. The app deliberately
closes the preview receiver, local forward, and SSH transport rather than
claiming that sockets remain alive in the background.

| Event | Native app behavior | Remote-process consequence |
| --- | --- | --- |
| App backgrounds with tmux enabled | Close local network state and mark the profile for resume | The named tmux session, `wscrpt`, and the separately owned previewd tmux session remain remote |
| App becomes active after tmux background | Try up to four reconnects after 1, 2, 4, and 8 seconds; run `tmux new-session -A`; refresh previews; prefer the last attached ID or the only ready session | Reattach to the durable editor/session if the host and network are available |
| App backgrounds in direct-launch mode | Close SSH and discard automatic-resume credentials | The direct PTY process is not promised to survive; reconnect must be initiated by the user |
| Unexpected foreground SSH loss | If cached credentials exist, use the same four-attempt reconnect loop | tmux is the only supported guarantee that editor process state survives transport loss |
| Explicit Disconnect | Cancel reconnect, clear the in-memory connection cache and last preview choice, close preview/forward/SSH | Does not run `previewctl stop` and does not intentionally kill remote tmux, browser, or agent state |

The stable terminal view keeps local scrollback when the terminal destination
is unchanged. Changing host, port, username, workspace, or launch style clears
the display so output from one destination cannot masquerade as another.

## Current validation boundary

Evidence recorded 2026-07-31 and extended 2026-08-01:

| Evidence | Observed | What it establishes | What it does not establish |
| --- | --- | --- | --- |
| Generic iOS Debug build-for-testing | Passed | Native app and test bundle compile/link for `iphoneos` | Installation, signing, launch, networking, or keyboard behavior on hardware |
| Generic iOS optimized Release build | Passed | Optimized native app compiles and links for `iphoneos` | Archive validation, signing, installation, or store readiness |
| iPad Pro 13-inch simulator, iOS 26.5 | App launch plus portrait-mini and landscape-mini/expanded visual pass | The stable native toolbar, terminal, and player compose correctly in both orientations and the expanded landscape layout becomes side by side | Human layout approval, real SSH, direct-tcpip, WebRTC, local-network permission, GPU cadence, thermal behavior, or Magic Keyboard input |
| Simulator app-switcher privacy check | Passed; app card was covered by the opaque shield and content returned on resume | The scene-level shield obscures the inactive simulator snapshot above the normal app content | Physical-device snapshot behavior, every interruption path, or protection while the device itself is unlocked |
| `HostKeyTrustTests` | 8/8 passed | Fingerprint, TOFU, mismatch, pin race, identity restore, and bounds policy | A real sshd handshake or human verification of the remote host's key |
| `SSHTransportIntegrationTests` | 12/12 passed | Authentication, PTY launch, terminal I/O, resize, exec cancellation, direct-tcpip, and teardown behavior pass against the test SSH peer | A connection to a production sshd |
| `PreviewCoordinatorTests` plus `RemotePreviewControlTests` | 16/16 passed | Strict response parsing, command quoting, numeric loopback, listen-before-token sequencing, replacement, committed-attachment restoration, and stale-intent handling | A real iPad forward, one-use token exchange, or direct ICE media route |
| Complete native XCTest suite | 73/73 passed | All current native unit, keyboard-preflight, policy, parser, controller, and transport-integration tests pass on the simulator lane | A real SSH host, physical iPad, Magic Keyboard, or media-performance route |
| Node sidecar suite | 77 passed, 1 environment-gated Chrome test skipped | Current control-plane, token, runtime-store, and security regressions pass | The separately invoked real-Chromium path or native WebKit playback |
| Local Chrome WebRTC E2E | 1/1 passed and exited cleanly; 22.97 presented FPS, 24 decoded FPS, 62 frames, 67 ms maximum presentation gap, 88.7 ms request-to-glass, 24.6 ms maximum no-backlog frame age, 116.13 ms recovery after a 750 ms blocked receiver, and a fresh track after reload | The exact-target browser/previewd WebRTC mechanism, cadence probes, backlog recovery, and replacement generation work in a short local desktop run | Sustained 24 FPS, native WebKit playback, or remote-host-to-iPad acceptance |
| Remote-host read-only SSH preflight | Existing Mac key login passed; Linux arm64, tmux 3.4, Node 22.22.2; direct-tcpip to remote `127.0.0.1:22` returned OpenSSH 9.6 | The configured host is reachable and currently permits an SSH loopback forward | App-generated-key login, app NIOSSH interop, preview tooling, browser capture, or iPad media |
| Physical-device discovery | `xcrun devicectl list devices` reported no devices | No attached device was silently skipped | Any signed install, physical keyboard behavior, or real-device networking |

The following acceptance gates remain open:

- signed install and sustained foreground/background use on a physical iPad;
- the complete physical Magic Keyboard and terminal-resize matrix;
- password and device-key login, first-use fingerprint confirmation, changed-key
  rejection, and real `direct-tcpip` forwarding against the remote host;
- tmux preservation across backgrounding, foreground resume, network loss, and
  explicit reconnect;
- human approval of mini, expanded, portrait, and landscape workspace feel; and
- every real remote-host-to-iPad gate in the Phase 0 ledger: 10-minute mini and
  expanded 24 FPS, 30 FPS headroom, 100-probe latency, adaptive fallback,
  no-backlog recovery, exact-session confirmation, view-only confirmation, and
  lifecycle cleanup.

## Practical physical-iPad runbook

### 1. Prepare the remote workspace

Use explicit absolute roots for the game project and the separate wscrpt tools
checkout. Install the sidecar under the tools root, then run preview control
from the project root so `--workspace .` identifies the game rather than the
tools checkout:

```sh
PROJECT_ROOT='/absolute/path/to/game-project'
PREVIEW_TOOLS_ROOT='/absolute/path/to/wscrpt'

node --version
tmux -V
wscrpt --version
npm --prefix "$PREVIEW_TOOLS_ROOT/previewd" ci --ignore-scripts --no-audit --no-fund
cd -- "$PROJECT_ROOT"
test -f "$PREVIEW_TOOLS_ROOT/previewd/bin/previewctl.mjs"
```

Start Chrome/Chromium with a dedicated user-data directory and
`--remote-debugging-address=127.0.0.1 --remote-debugging-port=9222`. Never bind
the debugging port to the LAN. In the agent's live tmux pane, record the exact
pane ID:

```sh
tmux display-message -p '#{pane_id}'
```

Discover the exact browser target and register the exact canvas:

```sh
TMUX_PANE='%12'
TARGET_ID='PASTE-EXACT-CDP-TARGET-ID'
CANVAS_SELECTOR='canvas#game'

node -- "$PREVIEW_TOOLS_ROOT/previewd/bin/previewctl.mjs" targets \
  --cdp http://127.0.0.1:9222 \
  --json

node -- "$PREVIEW_TOOLS_ROOT/previewd/bin/previewctl.mjs" ensure \
  --workspace . \
  --tmux-pane "$TMUX_PANE" \
  --target-id "$TARGET_ID" \
  --canvas-selector "$CANVAS_SELECTOR" \
  --cdp http://127.0.0.1:9222 \
  --json
```

Confirm that `list --workspace . --json` reports the session as healthy and
active. Do not continue with a stale heartbeat, dead tmux owner, multiple
candidate canvases, or an inferred browser target.

### 2. Install and sign the app

Open
`clients/ipad-preview-harness/PreviewHarness.xcodeproj`, select the
`PreviewHarness` scheme, choose a development team and an iPad running iOS 17
or newer, then Run. Expect `wscrpt` beneath the installed icon even though the
target and scheme retain the historical name.

Allow the local-network permission when iPadOS asks. Denying it blocks the
intended LAN WebRTC path.

### 3. Create the remote profile

Open Connection Settings and enter the SSH host without a URL scheme, port,
username, exact project path, and the separate wscrpt checkout path containing
`previewd` (or `.` when they are intentionally the same directory). Leave
**Keep wscrpt in tmux** enabled and use a stable session name unless deliberately
testing the non-durable direct path.

Choose one authentication method:

- **Password:** enter it and decide whether to persist it in Keychain.
- **Device key:** generate and copy the displayed public key, install it in the
  remote account's `authorized_keys` through an already trusted channel, then
  connect. The private key never leaves the iPad Keychain.

On first connection, compare the presented host-key algorithm and SHA-256
fingerprint with the development host through a separate trusted route. Accept
only an exact match. For an Ed25519 host key, an administrator can obtain the
authoritative value from a trusted console or management path with:

```sh
ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub -E sha256
```

Do not derive the comparison value through the same first, still-unverified
SSH connection.

### 4. Attach and use the workspace

After authentication, confirm that the terminal runs `wscrpt` in the requested
directory and that the status pill says SSH connected. If exactly one healthy
preview exists, it should attach automatically. Otherwise press
Command-Option-V, refresh, and select the exact session.

Use Command-Option-T to return keyboard focus to the terminal and
Command-Option-P to compare mini and expanded player layouts. Confirm the
player is view-only and terminal typing never becomes gameplay input.

### 5. Exercise durability and record evidence

With tmux enabled, background the app, wait long enough for iOS to suspend it,
then foreground it. Confirm a bounded reconnect, the same tmux editor state,
fresh preview discovery, and no stale frame replay. Repeat after a brief network
loss and after rotating in both player modes.

Run the full physical keyboard matrix and the Phase 0 real-route measurements.
Store private raw evidence under `output/preview-phase0/`; commit only redacted
summaries. Simulator screenshots and focused unit counts must remain labeled as
supporting evidence.

### 6. Stop only what you intend

Disconnecting the iPad closes only its local receiver, forward, and SSH
connection. When the preview daemon itself should stop, run this explicitly on
the host:

```sh
SESSION_ID='PASTE-PREVIEW-SESSION-ID'
cd -- "$PROJECT_ROOT"
node -- "$PREVIEW_TOOLS_ROOT/previewd/bin/previewctl.mjs" stop \
  --session "$SESSION_ID" \
  --json
```

That preview operation must not be reported as stopping the editor, agent tmux
pane, or browser unless separate operator action did so.
