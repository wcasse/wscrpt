# Agent-native wscrpt roadmap

Status: W0 contracts and deterministic fake agent are implemented in
`src/agent_contract.rs` and `src/agent.rs` (unit-tested admission). W1+ remain
unshipped product surface.

This roadmap covers three related product ideas:

1. a minimal agent-native development loop;
2. Markdown-backed Stickies;
3. collaboration designed for an iPad/mobile workflow.

The terminal and gameplay player remain wscrpt's two stable work surfaces.
The proposal adds one bounded activity/review drawer and optional cards above
those surfaces. It does not add a second terminal, task runner, Git client, or
preview implementation.

## Product rule

Give a person one place to assign bounded work, see meaningful progress,
approve risk, review the exact change, run proof, and take control. Reuse the
existing file, diff, Problems, task, terminal, and preview surfaces for the
actual artifacts.

Version one should include:

- one host-local agent adapter;
- an explicit work packet defining scope and authority;
- a bounded event stream for plan, status, approvals, changed paths, checks,
  artifacts, and completion;
- a five-state activity drawer: Brief, Working, Needs You, Review, Closed;
- pause/cancel, terminal takeover, and a durable receipt;
- review handoff to existing wscrpt files, Git diff, tasks, Problems, and the
  exact selected gameplay preview.

Version one should not include:

- autonomous merge, push, deploy, or secret access;
- a provider marketplace or iPad API-key vault;
- tmux-output scraping as an agent protocol;
- a permanent pane per agent;
- a general chat, issue-tracking, or CI product;
- simultaneous CRDT code editing.

## Agent integration

Prefer Agent Client Protocol v1 for agents that expose it. Run the agent on
the development host and speak JSON-RPC over stdio through a small adapter.
Provider-specific CLI adapters may implement the same interface as fallbacks.

Do not expose ACP messages directly to the renderer or iPad. Normalize them:

```text
host-local agent process
  <-> ACP or provider adapter
  <-> AgentCoordinator
  <-> bounded AgentEvent queue
  <-> wscrpt TUI and native activity drawer
```

The coordinator should follow the same rules as existing background services:

- every event carries workspace identity, session identity, generation, and
  sequence;
- cancelling or replacing a run invalidates the prior generation;
- there is one stale-event admission point;
- all channels, text, diff data, event rate, and run duration are bounded;
- project files cannot silently authorize an executable;
- process groups are retired and terminal state is restored visibly;
- raw prompts, tool payloads, source, secrets, and full environment data do
  not enter shared logs by default.

ACP remote-agent support is not the iPad transport boundary. The native client
should use wscrpt's small host protocol over the existing authenticated SSH
forward, while ACP remains local to the host.

## Work packet

A work packet is the contract between the user, wscrpt, and one agent run:

```text
id
workspace identity
goal
base commit
current or linked worktree and exact path
protected and writable paths
required verification commands
edit, command, network, commit, and push authority
creator and creation time
```

Offer a linked Git worktree as the default isolation path, but require
confirmation before creating one. Record the exact base and worktree so review
can distinguish implementation, proof, and integration. Pushing is out of
scope for the first version.

## Agent event contract

The internal event shape should contain only:

- session and workspace identity;
- generation and monotonically increasing sequence;
- timestamp;
- kind: state, plan, approval, path touched, check result, artifact,
  review-ready, or notice;
- a short summary;
- an optional bounded path, Git-object, or artifact reference;
- a sensitive-data marker.

This is a receipt, not a telemetry exhaust. Detailed output remains local and
opens on demand.

## Stickies

Stickies are spatial working context, not a second docs or issue system. Use
ordinary Markdown files for content and keep window geometry per device.

Team/repository note:

```text
.wscrpt/stickies/<uuid>.md
```

Personal note:

```text
$XDG_STATE_HOME/wscrpt/stickies/<workspace-id>/<uuid>.md
```

Device-local placement:

```text
$XDG_STATE_HOME/wscrpt/stickies-layout/<workspace-id>.json
```

The native client stores its placement locally under the same workspace
identity. Do not commit x/y position, size, z-order, collapsed state, or other
viewport-specific geometry.

The Markdown profile should retain headings, lists, task lists, links,
emphasis, blockquotes, code spans/fences, and tables where supported.
Unsupported syntax remains visible as source. Sanitize terminal escapes,
pasted HTML, and link schemes.

Supported anchors:

- workspace;
- file;
- selection with base blob, line range, and context hash;
- commit;
- preview session identity, never an auth token or raw URL.

Stale anchors remain visible and explicitly stale; they must not silently move
to unrelated content. An agent receives a Sticky only when the user includes
it in the work packet.

The TUI should use a full-screen overlay or sidebar stack. The native iPad app
may use draggable/resizable SwiftUI cards above the stable terminal/player,
without rebuilding or reparenting either UIKit surface.

## Mobile collaboration

Start with asynchronous Git/worktree review packets:

```text
assign work packet
  -> teammate or agent works in an isolated worktree
  -> review packet names base, head, paths, checks, and artifacts
  -> reviewer opens it on iPad
  -> reviewer comments, approves, or requests changes
  -> author updates or integrates through normal Git authority
```

A review packet needs exact base/head object IDs, changed paths, check results,
artifact references, revision, and state. Line comments carry the packet
revision, path, base blob, optional line/context hash, body Markdown, and a
client nonce. Replayed offline mutations are idempotent; comments against an
older blob remain readable but visibly stale.

The iPad toolbar gains a compact activity/presence pill and one drawer for:

- active teammates and agents;
- review requests and approvals that need the user;
- packet summaries and line comments;
- following the same exact view-only preview;
- an explicit writer handoff for optional shared-terminal pairing.

Touch handles review, comments, and Stickies. The physical keyboard retains
the fast terminal path. On reconnect, replay only idempotent state and comment
events—never a stale approval, shell command, commit, merge, or writer handoff.

Shared terminal pairing, if added, uses one visible writer lease. The lease
expires on disconnect and cannot be silently reacquired from queued state.
GitHub/GitLab and team-chat integrations are later adapters; local review
packets remain provider-neutral.

## Host boundary

For the first single-user agent run, the TUI may own the subprocess. Before
native review or team presence ships, factor the coordinator behind a
loopback-only host mode of the existing `wscrpt` binary. Reach it through the
native client's authenticated SSH direct-tcpip forward. No new listener binds
to the LAN by default.

Keep ownership separate:

| Path | Owns | Must not own |
| --- | --- | --- |
| SSH PTY | Terminal bytes | Agent parsing or media |
| wscrpt host control | Agent events, review packets, presence, Sticky metadata | Preview frames |
| previewd/WebRTC | Exact view-only browser target and media | Agent lifecycle or code review |
| Git/filesystem | Source and review objects | Ephemeral presence |

## Phases and gates

### W0 — contracts and deterministic fake

**Done (library only):** Define agent, packet, event, review, and Sticky
contracts. Build a fake agent and adversarial event tests.

Gate: stale, oversized, invalid-path, replayed, and cancelled events cannot
affect the current workspace.

### W1 — Stickies v1

**Done (host TUI):** Markdown storage, atomic saves, personal/team separation,
list/filter/archive, and the stickies picker overlay (`Esc w k` / `Esc w K`).
Native iPad floating cards remain W4.

Gate: formatting round-trips, no committed layout churn, sanitization, and
recovery from truncated/invalid metadata.

### W2 — one agent, one packet

Implement one ACP agent, one fallback adapter, approvals, cancellation, and
review handoff to existing surfaces.

Gate: dirty-tree protection, exact authority, process/PTY restoration, bounded
output, crash recovery, and a real useful run reviewed by a human.

### W3 — review packets

Implement base/head packets, line comments, approve/request-changes,
supersession, and confirmed linked-worktree creation.

Gate: offline replay is idempotent, stale comments are visible, and unrelated
dirty work is never included.

### W4 — native iPad flow

Add the activity drawer, Stickies cards, review/comments, foreground catch-up,
and exact-preview following.

Gate: physical iPad and keyboard, network change/reconnect, privacy snapshot,
low-bandwidth review, and no terminal/player reparenting.

### W5 — same-host team service

Add role-scoped presence, packet/comment sync, retention, audit, and
revocation through the SSH-forwarded loopback control plane.

Gate: two users, revoked access, replay/rate/security tests, backup/restore,
and explicit privacy review.

### W6 — optional live note collaboration

Evaluate Yjs only for team Stickies after real concurrent demand exists. Live
code CRDTs remain deferred until there is one canonical save/LSP/agent
authority and a demonstrated use case.

## Decisions before W1/W2

1. Should runs default to a confirmed linked worktree or the current tree?
2. Which agent must work first, and does it expose ACP v1 today?
3. May an agent commit after explicit approval, or stop at an uncommitted
   review packet?
4. Are team Stickies opt-in per note or enabled per repository?
5. Can collaborators SSH into the same host, or is a hosted relay required?
6. Does shared-terminal pairing belong in wscrpt or remain normal tmux?
7. What retention is acceptable for events, comments, and team notes?

## Primary references

- [Agent Client Protocol introduction](https://agentclientprotocol.com/get-started/introduction)
- [Agent Client Protocol repository](https://github.com/agentclientprotocol/agent-client-protocol)
- [Git linked worktrees](https://git-scm.com/docs/git-worktree.html)
- [Yjs documentation](https://docs.yjs.dev/)
- [OWASP WebSocket Security Cheat Sheet](https://cheatsheetseries.owasp.org/cheatsheets/WebSocket_Security_Cheat_Sheet.html)
- [OWASP Logging Cheat Sheet](https://cheatsheetseries.owasp.org/cheatsheets/Logging_Cheat_Sheet.html)
