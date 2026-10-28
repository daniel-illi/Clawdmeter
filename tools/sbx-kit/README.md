# Clawdmeter sbx kit

A [Docker Sandboxes](https://docs.docker.com/ai/sandboxes/) (sbx) **mixin kit**
that gives Claude Code inside a sandbox full Clawdmeter integration — the same
activity events, attention alerts and alert clearing the app gets from
locally installed `clawdmeter hook`s.

## Why a kit

`clawdmeter hook` works by writing files (`events.log`, `attention`,
`clear-<session>`) into the app's state dir. Inside an sbx sandbox none of
that is reachable: the sandbox is a microVM with its own filesystem, and
`clawdmeter` isn't installed there. The only host channel a sandbox always
has is its egress proxy — so this kit reports hook events over HTTP instead,
and packages everything needed to make that zero-touch:

```text
┌─ sbx microVM ──────────────────────┐         ┌─ host ─────────────────────┐
│ Claude Code                        │         │                            │
│   hook → sandbox-hook.py           │         │  sbx egress proxy          │
│     POST host.docker.internal:PORT ┼────────►│     │ forwards to loopback   │
│     /hook?mode=clear+event         │ policy  │     ▼                      │
└────────────────────────────────────┘ allow   │  clawdmeter 127.0.0.1:PORT │
                                               │     └─► hooks::run()       │
                                               │         events.log etc.    │
                                               └────────────────────────────┘
```

The kit's `spec.yaml` declares the three things that would otherwise be
manual setup: the network rule (`permissions.network.allow
localhost:<port>`), the hook endpoint URL (`CLAWDMETER_HOOK_URL`), and a
setup step that merges the hook entries into the sandbox's own
`~/.claude/settings.json`. Because the hooks live in the sandbox's config —
not a project's `settings.local.json` — they can never fire for a Claude
session on the host, and no `SANDBOX_NAME` guard is needed.

The endpoint binds loopback only; the sandbox reaches it because the host
proxy connects *from* the host. Nothing is exposed on the LAN.

## Requirements

Clawdmeter with the hook listener enabled on the host — it binds
`127.0.0.1:<hook_port>` (default `47991`; `hook_port = 0` in the app config
disables it). The daemon logs `hook listener on http://127.0.0.1:<port>/hook`
when it's up.

## Use

```sh
sbx run claude --kit ./tools/sbx-kit my-project

# custom port — must match the app's hook_port
sbx run claude --kit ./tools/sbx-kit -a hook_port=48001 my-project
```

`sbx create`/`run` applies the kit once: the hook install happens inside
`setup.install`. Kits that ship `files/` can't be added to an existing
sandbox with `sbx kit add` — recreate the sandbox with `--kit` instead.

## What the app does with it

`POST /hook?mode=<modes>` carries the raw Claude Code hook JSON as the body
and feeds it to the same code path as `clawdmeter hook <mode>` — so a POST
writes exactly `events.log` / `attention` / `clear-<session>` and the daemon
consumes them like local hooks: sessions appear on the meter, alerts
animate, clears dismiss them. Sandbox events are unattributed to a Claude
account — the listener strips `transcript_path` from POSTed bodies, so a
sandbox path can never be mistaken for a host `config_dirs` entry — and
polling is scheduled off the usage endpoint. Same behaviour as mounting the
state dir.

## Files

- `spec.yaml` — kit manifest: mixin for `claude`, `hook_port` arg (default
  `47991`), network allow-rule, `CLAWDMETER_HOOK_URL`, `setup.install`.
- `files/home/.local/share/clawdmeter/sandbox-hook.py` — the hook command.
  POSTs stdin to the app and **never fails Claude Code**: refused or
  timed-out connections (app not running) exit 0.
- `files/home/.local/share/clawdmeter/install-hooks.py` — idempotently
  merges the five hooks (`UserPromptSubmit`/`PostToolUse` → `clear event`,
  `Stop` → `event`, `Notification`/`PreToolUse[AskUserQuestion]` →
  `alert event`) into `~/.claude/settings.json`.

## Verify

Inside the running sandbox:

```sh
echo '{"hook_event_name":"UserPromptSubmit","session_id":"sbx-test"}' \
  | python3 -I ~/.local/share/clawdmeter/sandbox-hook.py clear event
```

On the host, `~/.config/clawdmeter/events.log` gains
`<ts> UserPromptSubmit sbx-test` and a `clear-sbx-test` file appears.

If it doesn't:

- App log — should say `hook listener on http://127.0.0.1:<port>/hook`.
- Sandbox policy — the kit asks for `allow network localhost:<port>`;
  `sbx inspect <name>` shows the sandbox's effective policy.
- Port match — `-a hook_port=…` must equal the app's `hook_port`.
