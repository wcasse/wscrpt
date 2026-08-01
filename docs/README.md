# wscrpt docs

| Doc | What it covers |
| --- | --- |
| [ARCHITECTURE.md](ARCHITECTURE.md) | Runtime boundaries, `App` state grouping, background-service admission, mutation boundaries, persistence limits |
| [CONTRIBUTOR_MAP.md](CONTRIBUTOR_MAP.md) | "Where do I change X?" — concern-to-module map so you can skip reading `src/app.rs` |
| [COMMANDS.md](COMMANDS.md) | Generated command reference (regenerate after keymap changes; a test enforces drift) |
| [RELEASING.md](RELEASING.md) | Release gate, evidence fields, checkpoint archive protocol |
| [IPAD_BLINK_QA.md](IPAD_BLINK_QA.md) | The human iPad/Blink/mosh/tmux acceptance matrix that gates releases |
| [HOST_SUPPORT.md](HOST_SUPPORT.md) | macOS/Linux host contract, CI evidence, WSL 2 route, and native Windows boundary |
| [NATIVE_IPAD_WORKSPACE.md](NATIVE_IPAD_WORKSPACE.md) | Native iPad SSH terminal plus view-only WebRTC player architecture, security contracts, validation boundary, and device runbook |
| [OPEN_SOURCE_CHECKLIST.md](OPEN_SOURCE_CHECKLIST.md) | Launch status: done, open items, explicit deferrals |
| [PUBLIC_SOURCE_AUDIT.md](PUBLIC_SOURCE_AUDIT.md) | Current-snapshot privacy, secret, relevance, license, and reachable-history boundary |
| [../THIRD_PARTY_NOTICES.md](../THIRD_PARTY_NOTICES.md) | Dependency license inventory for the Rust editor, preview sidecar, and native iPad client |
| [releases/](releases/) | Drafted GitHub release notes per version |
| [demo.tape](demo.tape) | VHS script that renders the README demo GIF (`vhs docs/demo.tape`) |
