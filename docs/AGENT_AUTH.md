# Agent authentication for beta hosts

wscrpt does **not** store API keys, OAuth tokens, or provider passwords.
Authentication belongs to the **host agent CLI** (or the shell environment you
start wscrpt from). That keeps secrets out of the repo, out of `.wscrpt/`, and
out of session files that ride along Blink/SSH reconnects.

## Mental model

```text
iPad Blink  --SSH/mosh-->  host shell  -->  wscrpt
                                              |
                                              | spawns process / probes health
                                              v
                                    pi (RPC) | grok | claude | codex | custom
                                              |
                                              +--> ~/.pi/… / Pi OAuth & keys
                                              +--> ~/.grok/auth.json  (CLI-owned)
                                              +--> $XAI_API_KEY       (env-owned)
```

| Layer | Owns | Must not own |
| --- | --- | --- |
| wscrpt `~/.config/wscrpt/config.toml` | `argv`, profile label, readiness probes | secrets, tokens |
| Agent CLI / Pi | login flow, token refresh, key files, **model catalog** | project source |
| Shell env | optional API keys for headless hosts | committed files |
| In-repo gate | `pi/extensions/wscrpt-permission-gate.ts` (tool confirm → Needs You) | API keys |

## Quick check

```sh
wscrpt --health | grep '^agent\.'
```

Look for:

- `agent.mode=fake` — demo loop, no provider account needed  
- `agent.mode=process` — real CLI configured  
- `agent.ready=yes|no`  
- `agent.auth_marker=...` — known credential file present/missing  
- `agent.summary=...` — one-line human guidance  

## Pi Coding Agent (recommended harness)

Pi is a **model-agnostic coding harness**: you pick providers/models inside Pi
(API keys or OAuth). wscrpt never stores those secrets and never becomes a
provider marketplace. See [pi.dev](https://pi.dev/).

1. On the **host** (not the iPad), install Node 22+ and Pi, then configure at
   least one provider in Pi (interactive `pi` or Pi’s docs for env keys).

   ```sh
   npm install -g @earendil-works/pi-coding-agent
   pi --version
   # complete Pi auth / models.json as you prefer
   ```

2. Point wscrpt at Pi RPC in `~/.config/wscrpt/config.toml`:

   ```toml
   [agent]
   use_fake = false
   profile = "pi"
   argv = ["pi", "--mode", "rpc"]
   auth_check_argv = ["pi", "--version"]
   ```

3. **Permission gate (required for Needs You):** Pi does not prompt before
   tools by default. wscrpt ships `pi/extensions/wscrpt-permission-gate.ts` and
   injects `--extension <that path>` when spawning Pi. The gate hooks
   `tool_call`, calls `ctx.ui.confirm`, and blocks on deny — that confirm is
   what becomes an RPC `extension_ui_request` for the Agents dashboard.

   Override path if needed:

   ```sh
   export WSCRPT_PI_PERMISSION_GATE=/absolute/path/to/wscrpt-permission-gate.ts
   ```

4. Re-check:

   ```sh
   wscrpt --health | grep '^agent\.'
   ```

   Look for `agent.profile=pi`, resolved `pi` on PATH, and a note naming the
   permission gate file.

5. Inside wscrpt: with `use_fake = false` and readiness, `Esc w a` spawns **Pi
   RPC** when `profile = "pi"` (injects the permission gate). Other process
   profiles still use ACP over `agent.argv`. Tool confirms currently
   **auto-deny** (fail-closed) until the approve chord lands — you will see
   Needs You receipt lines. **Lane baseline:** `Esc w A` will be **approve**
   (not sticky write-back). Sticky receipt apply stays on `:apply-receipt` /
   a non-`A` chord. Fake remains the default for CI and demos.

## Grok Build (ACP process)

1. On the **host** (not the iPad), install and sign in:

   ```sh
   # install grok CLI however you normally do
   grok login
   # or headless:
   export XAI_API_KEY="..."
   ```

2. Point wscrpt at it in `~/.config/wscrpt/config.toml`:

   ```toml
   [agent]
   use_fake = false
   profile = "grok"
   argv = ["grok", "agent", "stdio"]
   # optional hard requirement if you rely only on API keys:
   # required_env = ["XAI_API_KEY"]
   # optional binary/auth probe for --health:
   # auth_check_argv = ["grok", "--version"]
   ```

3. Re-check:

   ```sh
   wscrpt --health | grep '^agent\.'
   ```

4. Inside wscrpt: `Esc w a` launches the **ACP process** (`agent.argv`) when
   `use_fake = false` and readiness passes. The default remains the fake
   plan-first loop. Auth readiness is enforced so beta testers fail **early**
   with a clear message instead of a silent hang. ACP permission requests are
   cancelled with a Needs You notice until an approve chord is answered —
   never auto-approved.

Grok stores session credentials under `~/.grok/auth.json` (owner-only). wscrpt
only checks that the **path exists**; it never opens the file.

## Claude / Codex / custom

Same pattern: install the CLI on the host, complete **that** product’s login,
then set `agent.argv` and optionally `required_env` / `auth_check_argv`.

```toml
[agent]
use_fake = false
profile = "custom"
argv = ["your-agent", "stdio"]
required_env = ["SOME_API_KEY"]   # names only — values stay in the shell
```

## What beta testers should never do

- Put API keys in `config.toml`, `.wscrpt/`, Stickies, or README samples  
- Commit `~/.grok/auth.json`, Pi credential stores, or any credential file  
- Expect Blink to hold provider secrets — the host session must already be
  authenticated before `wscrpt` starts  
- Expect tool permission prompts without the **permission gate** extension
  (Pi runs tools freely unless the gate is loaded)

## Related

- Pi gameplan / agentic layer: Agents lane roadmap notes in
  [AGENT_NATIVE_ROADMAP.md](AGENT_NATIVE_ROADMAP.md)
- Gate source: `pi/extensions/wscrpt-permission-gate.ts`

- Agent roadmap: [AGENT_NATIVE_ROADMAP.md](AGENT_NATIVE_ROADMAP.md)  
- Default config template: `wscrpt --print-default-config`  
- Runtime keys: `Esc w a` run · `Esc w D` Agents dashboard (roster + receipt) · `Esc w x` cancel · `Esc w A` append receipt log to sticky · `Esc w C`/`Y` checklist fan-out/apply
