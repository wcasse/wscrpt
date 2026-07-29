# Architecture

`wscrpt` is a single-process terminal editor with bounded background services. The design favors explicit limits, recoverable failures, and a small set of mutation paths over a broad in-editor automation surface.

## Runtime boundary

`src/main.rs` owns terminal entry/restoration, event polling, redraw cadence, full-screen shell handoff, session bootstrap, and startup ordering. It creates the active workspace and `App`, enters terminal UI mode, publishes the terminal size, and only then starts background services. This keeps filesystem indexing, Git, and recovery scans off the first-frame path.

`App` is the runtime and renderer façade. Its top-level state is grouped into:

- `UiState`: prompts, keymap, status, redraw state, editing search, navigation history, bookmarks, and terminal handoff intent.
- `ProjectState`: immutable index/tree snapshots, explicit refresh state, search worker, and sidebar state.
- `LspState`: one configured service, synchronized-document registry, requests, diagnostics, capabilities, quarantine, and logs.
- `TaskState`: trusted task configuration/runner, active process, bounded output, and last task.
- `PersistenceState`: recovery journals, session store, recent files, and service status.
- `GitState`: read-only repository, branch, change-count, and loading state.

The active `Workspace` and runtime `Config` are private. Renderers receive read-only accessors; mutations enter through editor commands.

## Commands and prompts

`src/keymap.rs::COMMANDS` is authoritative for action IDs, titles, namespaces, bindings, palette search, and reference documentation. `wscrpt --print-command-reference` and `docs/COMMANDS.md` are generated from it; a test refuses drift in the committed reference.

Prompt routing uses typed `PromptFlow` variants. Each flow describes its visible prefix, optional UTF-8 input bound, whether it owns fixed candidates, and how Enter completes. Candidates and completion payloads are typed `PromptEntry` values rather than untrusted display strings.

## Background services

`ServiceCoordinator` owns a bounded result channel and cancellation token for project indexing, Git discovery/status, and recovery scanning. Every `ServiceEvent` carries:

- an application workspace identity;
- a service-specific generation;
- a typed result payload.

Refresh cancels the old token and advances the generation. `App::poll_services` is the single admission point and drops results that do not match both values. A failed refresh retains the last usable project snapshot. Dropping the coordinator cancels all jobs and invalidates their generations.

Project-, Git-, and recovery-dependent commands remain disabled while their snapshot is pending and report that state to the user. Small task configuration loading remains synchronous, but no task runs without the existing trust prompt.

## Mutation boundaries

Supported file mutation inside the editor is intentionally narrow:

- ordinary buffer edits and in-buffer replacement;
- atomic explicit saves and safe Save As/Save Copy As;
- explicit project file creation and clean file rename;
- single-document LSP formatting applied as one undoable edit.

Cross-file LSP workspace edits, LSP rename/code actions, project-wide replacement, and Git mutation are outside the 0.2 editor boundary. Use a trusted task or the full-screen workspace shell, then review and explicitly refresh/reload as needed.

## Persistence and recovery

Session version 2 stores paths and navigation/layout metadata, never buffer text. The explicit version 1 decoder preserves supported state while dropping legacy embedded-terminal visibility flags. Unknown future versions produce a recoverable startup notice instead of speculative decoding.

Unsaved text belongs only to bounded recovery journals. Saves remain atomic where the platform permits, refuse unsafe hard-link replacement, and detect external changes before overwrite.

## Safety and limits

Filesystem traversal, searches, task output, Git subprocess output, LSP queues/documents/JSON, prompt input, terminal rendering, recent files, bookmarks, and recovery/session files all have explicit ceilings. Partial results remain labeled. Terminal modes are restored on normal exit, error, and panic paths.
