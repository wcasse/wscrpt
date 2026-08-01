# Architecture

`wscrpt` is a single-process terminal editor with bounded background services. The design favors explicit limits, recoverable failures, and a small set of mutation paths over a broad in-editor automation surface.

## Runtime boundary

`src/main.rs` owns terminal entry/restoration, event polling, redraw cadence, full-screen shell handoff, session bootstrap, and startup ordering. It creates the active workspace and `App`, enters terminal UI mode, publishes the terminal size, and only then starts background services. This keeps filesystem indexing, Git, and recovery scans off the first-frame path.

`App` is the runtime and renderer façade. Its top-level state is grouped into:

- `UiState`: prompts, keymap, status, redraw state, editing search, navigation history, bookmarks, terminal handoff intent, and the revision-driven first-edit transition cue. Action exit arms the cue; only a changed document state ID consumes it, and the event loop paints onset and one 360 ms expiry rather than animating continuously.
- `ProjectState`: immutable index/tree snapshots, explicit refresh state, search worker, and sidebar state.
- `LspState`: one configured service, synchronized-document registry, requests, diagnostics, capabilities, quarantine, and logs.
- `TaskState`: trusted task configuration/runner, active process, bounded output, and last task.
- `PersistenceState`: recovery journals, session store, recent files, and service status.
- `GitState`: repository, branch, change-count, snapshot state, and at most one trusted local mutation in flight.

The active `Workspace` and runtime `Config` are private. Renderers receive read-only accessors; mutations enter through editor commands.

## Commands and prompts

`src/keymap.rs::COMMANDS` is authoritative for action IDs, titles, namespaces, bindings, palette search, and reference documentation. `wscrpt --print-command-reference` and `docs/COMMANDS.md` are generated from it; a test refuses drift in the committed reference.

Prompt routing uses typed `PromptFlow` variants. Each flow describes its visible prefix, optional UTF-8 input bound, whether it owns fixed candidates, and how Enter completes. Candidates and completion payloads are typed `PromptEntry` values rather than untrusted display strings.

## Background services

`ServiceCoordinator` owns a bounded result channel and cancellation token for project indexing, Git discovery/status, trusted Git mutation, and recovery scanning. Every `ServiceEvent` carries:

- an application workspace identity;
- a service-specific generation;
- a typed result payload.

Refresh cancels the old snapshot token and advances its generation. Git mutation has a distinct generation/token, is single-flight, and is never cancelled by a status refresh. `App::poll_services` is the single admission point and drops results that do not match both values. Every mutation outcome, including failure, starts a fresh Git snapshot before another operation is admitted. A failed project refresh retains the last usable project snapshot. Dropping the coordinator cancels all jobs and invalidates their generations.

Project-, Git-, and recovery-dependent commands remain disabled while their snapshot is pending and report that state to the user. Small task configuration loading remains synchronous, but no task runs without the existing trust prompt.

## Mutation boundaries

Supported file mutation inside the editor is intentionally narrow:

- ordinary buffer edits and in-buffer replacement;
- atomic explicit saves and safe Save As/Save Copy As;
- explicit project file creation and clean file rename;
- single-document LSP formatting applied as one undoable edit;
- after an explicit trust prompt, stage or unstage only the current saved file, or commit only the already staged index with one bounded single-line message.

Git mutations use fixed direct arguments on a bounded non-interactive worker. Stage/unstage rejects dirty buffers, submodules, and unmerged paths; commit rejects an empty index, unmerged paths, and configured signing. Git filters or hooks can still execute repository code, so every operation is trust-gated. Branch changes, network operations, discard/reset/clean, arbitrary paths/arguments, cross-file LSP workspace edits, LSP rename/code actions, and project-wide replacement remain outside the editor boundary. Use a trusted task or the full-screen workspace shell for those workflows.

## Persistence and recovery

Session version 2 stores paths and navigation/layout metadata, never buffer text. The explicit version 1 decoder preserves supported state while dropping legacy embedded-terminal visibility flags. Unknown future versions produce a recoverable startup notice instead of speculative decoding.

Unsaved text belongs only to bounded recovery journals. Saves remain atomic where the platform permits, refuse unsafe hard-link replacement, and detect external changes before overwrite.

## Safety and limits

Filesystem traversal, searches, task output, Git subprocess output, LSP queues/documents/JSON, prompt input, terminal rendering, recent files, bookmarks, and recovery/session files all have explicit ceilings. Partial results remain labeled. Terminal modes are restored on normal exit, error, and panic paths.

## Agent-native orchestration (W0 library)

`src/agent_contract.rs` and `src/agent.rs` define the host-side orchestration
contracts (work packet, bounded events, Stickies, review packets) and a single
`AgentCoordinator` admission point. Events carry workspace identity, run
generation, and a monotonic sequence. Path-touch events must fall under the
packet's writable scopes and outside protected scopes. Cancelling or replacing
a run advances generation so stale traffic cannot affect the current workspace.

W0 does not launch agents, speak ACP, mutate files, or expose TUI controls.
Those arrive in later roadmap phases behind the same contracts. See
[AGENT_NATIVE_ROADMAP.md](AGENT_NATIVE_ROADMAP.md).

## Remote preview Phase 0 boundary

The remote agent-preview spike is deliberately outside the Rust terminal
runtime. `previewd` is a separate Node process on the remote development host;
the reference `PreviewSurface` is browser JavaScript that can run in Safari or
inside the standalone iPad `WKWebView` harness. No preview dependency is linked
into the `wscrpt` Cargo package, and no video frame enters `App`, Crossterm,
SSH/mosh terminal bytes, or `session.toml`.

The control and media paths are distinct:

- authenticated SSH commands and a parallel SSH local forward carry bounded
  lifecycle, discovery, and WebRTC signaling metadata;
- tmux keeps only the named `previewd` process alive and is never scraped for
  state;
- Chrome DevTools Protocol selects and instruments one explicit target and one
  explicit canvas on a loopback-only debugging endpoint; and
- browser-native WebRTC carries the video directly from
  `canvas.captureStream(...)` to a view-only `<video>` element.

Runtime manifests live in a private preview-specific store rather than the
editor session file. The browser-side provider contract is transport agnostic,
so a future Unreal Pixel Streaming provider does not require changing surface
composition.

The Phase 0 harness remains a validation host, not a claim that video is
embedded in the Blink-hosted TUI. The follow-on native-container decision is now
implemented as a separate iPad app that combines a NIOSSH/SwiftTerm terminal
with the same view-only WebRTC surface; it does not link preview code into the
Rust editor. Connection epochs cross a resource-retirement barrier before a
replacement SSH transport or preview coordinator may issue one-use tokens, and
the signaling relay applies writable-peer backpressure in both directions. Its
architecture and still-open physical-device/remote-host gates are
recorded in [NATIVE_IPAD_WORKSPACE.md](NATIVE_IPAD_WORKSPACE.md).
