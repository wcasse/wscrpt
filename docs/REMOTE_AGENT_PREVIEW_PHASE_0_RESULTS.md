# Remote agent preview: Phase 0 results

Status: **local implementation and Chromium functional spike complete;
remote-host-to-iPad FPS and latency gates remain open**

This document is the evidence ledger for
[the approved Phase 0 plan](REMOTE_AGENT_PREVIEW_PHASE_0.md). Desktop/unit
checks are supporting evidence only. A gate is complete only when its recorded
route and device match the table below.

## Implementation revision

| Field | Value |
| --- | --- |
| Repository | `wscrpt` |
| Starting branch | `main` |
| Starting revision | `905d0102e7fb4f36a6bba253f230adbcf301a8d0` |
| Current integration base | `main` at `df2bf563afd0a4b825e3a0f3713f34014f7e38da` |
| Historical Phase 0 rescue revision | `codex/preview-phase0-rescue-20260731` at `7ac19cb3779b452a19a79e153aaa3abf0fc6be10`; preserves the convergence fixes newer than `preview-phase0` at `4594db7` |
| Native follow-on worktree | `codex/native-ipad-terminal-player`, based on `7ac19cb3779b452a19a79e153aaa3abf0fc6be10`; the final implementation commit will be reported in repository history and the handoff after validation |
| Fixture/source revision | Historical Phase 0 rescue revision above |

## P0.0 prerequisite record

Checked 2026-07-29. Network addresses and credentials are intentionally not
committed here.

| Host | Observed state | Result |
| --- | --- | --- |
| Local macOS development host | Node 25.9.0, npm 11.12.1, Xcode 26.6, tmux 3.6a, Google Chrome present | Ready for unit, Swift compile, and local Chromium smoke |
| Remote host | Ubuntu 24.04.4 LTS on arm64; Node 22.22.2 in the user Hermes toolchain; tmux 3.4; Docker 29.2.1; NVIDIA GB10/driver 580.173.02; Tailscale present | Partially ready |
| Remote-host browser | No Chrome/Chromium package or running process found; no GUI display exported to the SSH session | **Open prerequisite** |
| iPad SSH-forward reachability | Not run | **Open real-device gate** |
| Direct iPad/remote-host ICE route | Not run | **Open real-device gate** |

The remote host's user toolchain path must be supplied explicitly when starting
`previewd`; a non-login SSH command does not currently find `node` or `npm`.
Installing or selecting the dedicated browser remains an operator decision and
must preserve loopback-only CDP.

## Automated validation

| Check | Command/evidence | Result |
| --- | --- | --- |
| Locked Node install | `npm --prefix previewd ci --ignore-scripts --no-audit --no-fund` | Passed; 4 locked packages installed |
| Node unit/security/browser tests | `npm --prefix previewd test` | Passed: 77 passed, 1 expected environment-gated E2E skip, 0 failed |
| Production dependency audit | `npm audit --omit=dev --json` | Passed: 0 known vulnerabilities |
| Local Chromium functional smoke | `WSCRPT_PREVIEW_CHROME=... npm --prefix previewd run test:e2e` | Passed: 1/1; supporting receipt below |
| Combined Node/Chromium lane | Unit/security verifier plus `WSCRPT_PREVIEW_CHROME=... npm --prefix previewd run test:e2e` | Passed on the rescue branch; supporting receipt below |
| Independent lifecycle/security recheck | Full unit rerun, startup/restart ownership audit, `git diff --check`, and verifier shell syntax | Passed; no high- or medium-severity Phase 0 blocker remained |
| iPad harness app/test-bundle compile | `xcodebuild ... -sdk iphoneos -destination 'generic/platform=iOS' ... build-for-testing` | Passed on Xcode 26.6 with the iOS 26.5 SDK |
| iPad harness CI execution | Dynamic available-iPad simulator selection plus `xcodebuild test` in `preview-phase0.yml` | Configured; not run locally because no iPad simulator is installed, and the rescue branch has not been pushed |
| Existing Rust verification | `scripts/verify.sh` | Passed: formatting, strict Clippy, 589 library tests (1 ignored), 6 binary tests, 1 real tmux smoke, 9 outer-PTY tests, and 3 doc tests |
| Isolated package/install smoke | Package, install into a temporary root, then `wscrpt --version` and `wscrpt --health` | Passed for `wscrpt 0.2.1`; crate SHA-256 `5622a196dc11bfa08e886bf8e854bb014e49bc7040bd0b19be5516f8f51850ac`; installed binary SHA-256 `4f8cf2b1ca3d9901f51ebac855ec34b3e2675a2f77addb7c92fff4351f6da942` |

## Local Chromium supporting receipt

Recorded again 2026-07-31 on local macOS with Google Chrome 150.0.7871.187. The
source and receiver ran in separate isolated Chrome profiles, with CDP bound to
numeric loopback. The table is the settled standalone receipt taken after the
rescue verification lanes passed. This short run proves the complete mechanism,
not a sustained performance gate.

| Signal | Observed |
| --- | --- |
| Exact source | Explicit CDP target plus `canvas#clock-game`; source 1280x720 |
| Receiver | 960x540; H.264; host-to-host ICE; direct `<video>` |
| Cadence | 23.49 presented FPS; 24 decoded FPS; 60 decoded frames |
| Bitrate | 373,968 bps during the sampled interval |
| Presentation gap | 74.2 ms maximum during the short baseline window |
| Fixture latency | One request-to-glass sample at 58.0 ms |
| Forced receiver stall | 750 ms block; recovered in 52.68 ms; media time jumped 795 ms across 3 callbacks instead of replaying queued frames |
| Post-stall age | `receiveTime` basis; 30.3 ms on the recovery sample; 28.3 ms maximum in the no-backlog observation |
| Source reload | Same signaling generation, fresh media track, 14 additional frames before acceptance |
| Teardown | Receiver detached; daemon, CDP clients, targets, browser profiles, and temporary runtime closed |

The mini smoke holds the requested profile to measure that path; default
fallback transitions are tested separately and remain part of the impaired
real-route gate. Startup samples before `ontrack` no longer drive adaptation.
Earlier development runs exposed both that false-fallback race and cadence loss
when duplicate two-browser smokes ran concurrently; the verifier now serializes
unit, cadence, and fallback lanes.

Neither 23.49 FPS over this short sample nor one 58.0 ms latency probe satisfies
the 10-minute/100-probe iPad acceptance thresholds.

The browser receiver already exposes one-at-a-time correlated latency probes
and writes their redacted samples to private evidence. The standalone iPad
harness does not yet include an automated 100-probe run driver or percentile
analyzer. That test tooling must be added before the real-device latency gate;
the single-probe desktop smoke is not a substitute.

## Real remote-host-to-iPad gates

Warm-up is the first 10 seconds and is excluded. Store private JSONL evidence
under `output/preview-phase0/`; do not commit tokens, SDP, ICE addresses, raw
URLs, or cookies.

| Gate | Required | Observed | Verdict |
| --- | --- | --- | --- |
| Mini baseline | 960x540, 10 min, overall >=23.8 FPS, 95% five-second windows >=23.6 FPS, no freeze >500 ms | Not run | Open |
| Expanded baseline | 1280x720, 10 min, same 24-FPS/freeze thresholds | Not run | Open |
| Headroom | 1280x720, 3 min, overall >=29 FPS, 95% five-second windows >=28.5 FPS | Not run | Open |
| LAN latency | 100 fixture probes, request-to-glass p95 <250 ms | Not run | Open |
| Adaptive fallback | Enter 960x540/12 within 5 s; present 11.5-12.5 FPS; restore 24 within 15 s | Not run | Open |
| No backlog | Frame age <250 ms within 2 s after recovery; no replay of stale frames | Not run | Open |
| Exact session | Target ID/canvas identity plus synchronized agent action and iPad recording | Not run | Open |
| View-only | No preview-originated gameplay input | Not run | Open |
| Lifecycle | Peer replacement works; stop leaves editor, agent tmux, and game browser alive | Not run | Open |

## Per-run receipt template

Copy this block for each private run and redact network/browser secrets before
committing any summary:

```text
run_id:
date_utc:
operator:
source_revision:
preview_revision:
remotehost_os_arch:
browser_version:
ipad_model_os:
surface: safari | wkwebview
profile: mini | expanded | expanded-headroom | fallback
target_id_hash:
url_hash:
canvas_selector:
source_dimensions:
encoded_dimensions:
codec:
selected_candidate_pair_type:
duration_seconds:
warmup_seconds: 10
presented_fps_overall:
five_second_window_pass_fraction:
longest_freeze_ms:
latency_probe_count:
latency_p50_ms:
latency_p95_ms:
latency_p99_ms:
fallback_enter_seconds:
recovery_seconds:
post_recovery_frame_age_ms:
exact_session_human_verdict:
view_only_human_verdict:
artifact_paths:
notes:
```

## Follow-on native container status (2026-07-31)

The Phase 0 evidence above remains historical and unchanged. A follow-on iPad
container now combines a native NIOSSH/SwiftTerm terminal with the existing
view-only WKWebView/WebRTC player. The Xcode target and scheme remain
`PreviewHarness` for continuity; the installed display name is `wscrpt`. See
[NATIVE_IPAD_WORKSPACE.md](NATIVE_IPAD_WORKSPACE.md) for architecture, security
invariants, build commands, reconnect semantics, and the physical-iPad runbook.

Current native-container evidence is deliberately supporting evidence only:

- generic `iphoneos` Debug build-for-testing and optimized Release build:
  passed;
- iPad Pro 13-inch simulator on iOS 26.5: app launch plus portrait-mini and
  landscape-mini/expanded visual pass, with the native toolbar, terminal, and
  player rendered together and the expanded landscape layout side by side;
- simulator app-switcher privacy check: the inactive card was covered by the
  opaque shield and app content returned on resume;
- focused host-key policy tests: 8/8 passed;
- focused SSH transport integration tests: 12/12 passed;
- focused preview coordinator/control tests: 16/16 passed;
- complete native XCTest suite: 72/72 passed;
- Node sidecar suite: 77 passed with its separately invoked Chrome test
  environment-gated and skipped; and
- local Chrome WebRTC E2E: 1/1 passed and exited cleanly at 22.97 presented FPS,
  24 decoded FPS, 62 decoded frames, a 67 ms maximum presentation gap,
  88.7 ms request-to-glass, 24.6 ms maximum no-backlog frame age, 116.13 ms
  recovery after a 750 ms blocked receiver, and a fresh track after reload in a
  short supporting run.

The remote host's read-only SSH preflight is green through the Mac's existing identity,
and a `direct-tcpip` probe to remote `127.0.0.1:22` returned its OpenSSH 9.6
banner. This establishes server forwarding support, not NIOSSH authentication
from the app. The real route is not provisioned: remote `wscrpt`, `previewctl`,
the expected BIRDWORLD path, and authorization of the actual iPad device key
are absent. CoreDevice also reported no physical device attached for this run.

No item above closes human landscape visual acceptance, the physical Magic
Keyboard gate, or any real remote-host-to-iPad performance gate. The 10-minute
mini/expanded 24 FPS tests,
30 FPS headroom, 100-probe latency, fallback, no-backlog recovery,
exact-session, view-only, lifecycle, physical-keyboard, and background/tmux
reconnect gates all remain open.
