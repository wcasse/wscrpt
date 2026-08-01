# Agent authentication for beta hosts

wscrpt does **not** store API keys, OAuth tokens, or provider passwords.
Authentication belongs to the **host agent CLI** (or the shell environment you
start wscrpt from). That keeps secrets out of the repo, out of `.wscrpt/`, and
out of session files that ride along Blink/SSH reconnects.

## Mental model

```text
iPad Blink  --SSH/mosh-->  host shell  -->  wscrpt
                                              |
                                              | spawns (future) / probes (today)
                                              v
                                         grok | claude | codex | custom argv
                                              |
                                              +--> ~/.grok/auth.json  (CLI-owned)
                                              +--> $XAI_API_KEY       (env-owned)
```

| Layer | Owns | Must not own |
| --- | --- | --- |
| wscrpt `~/.config/wscrpt/config.toml` | `argv`, profile label, readiness probes | secrets, tokens |
| Agent CLI | login flow, token refresh, key files | project source |
| Shell env | optional API keys for headless hosts | committed files |

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

## Grok Build (recommended for iPad SSH)

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

4. Inside wscrpt: `Esc w a` still runs the **fake plan-first loop** until ACP
   process launch is wired. Auth readiness is enforced so beta testers fail
   **early** with a clear message instead of a silent hang.

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
- Commit `~/.grok/auth.json` or any credential file  
- Expect Blink to hold provider secrets — the host session must already be
  authenticated before `wscrpt` starts  

## Related

- Agent roadmap: [AGENT_NATIVE_ROADMAP.md](AGENT_NATIVE_ROADMAP.md)  
- Default config template: `wscrpt --print-default-config`  
- Runtime keys: `Esc w a` run · `Esc w D` Agents dashboard (roster + receipt) · `Esc w x` cancel · `Esc w A` append receipt log to sticky · `Esc w C`/`Y` checklist fan-out/apply
