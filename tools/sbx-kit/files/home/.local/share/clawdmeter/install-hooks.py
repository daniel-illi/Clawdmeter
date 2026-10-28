# Merge the Clawdmeter sandbox hooks into the sandbox's own
# ~/.claude/settings.json. Run once by the kit's setup.install, as the agent
# user. Idempotent: existing settings are kept, our entries are added once,
# and stale sandbox-hook entries (old paths, changed args) are replaced.
# A settings.json that cannot be read is moved aside, and the write itself
# is atomic. Any unexpected failure prints a warning and exits 0 — a hook
# install must never break `sbx create`.
import json, os, pwd, sys

BASE = "python3 -I " + os.path.join(os.path.dirname(os.path.abspath(__file__)), "sandbox-hook.py")
MARK = "sandbox-hook.py"

# The same five hooks the app installs (`clawdmeter hook <mode>`).
SPECS = [
    ("UserPromptSubmit", None, "clear event"),
    ("PostToolUse", None, "clear event"),
    ("Stop", None, "event"),
    ("Notification", None, "alert event"),
    ("PreToolUse", "AskUserQuestion", "alert event"),
]


def main():
    # The home of the user this runs as — setup.install's environment may
    # carry a foreign HOME, so the uid decides.
    try:
        home = pwd.getpwuid(os.getuid()).pw_dir
    except Exception:
        home = os.environ.get("HOME") or "/home/agent"
    p = os.path.join(home, ".claude", "settings.json")
    os.makedirs(os.path.dirname(p), exist_ok=True)

    s = {}
    if os.path.exists(p):
        try:
            s = json.load(open(p, encoding="utf-8"))
        except Exception:
            os.replace(p, p + ".bak")  # keep the unreadable file
    if not isinstance(s, dict):
        s = {}
    hooks = s.get("hooks")
    if not isinstance(hooks, dict):
        hooks = {}
        s["hooks"] = hooks

    # Drop our entries wherever they sit, and any broken shapes; groups left
    # without hooks were ours alone and go too.
    for ev, groups in list(hooks.items()):
        if not isinstance(groups, list):
            del hooks[ev]
            continue
        for g in groups:
            if isinstance(g, dict) and isinstance(g.get("hooks"), list):
                g["hooks"] = [h for h in g["hooks"] if isinstance(h, dict) and MARK not in str(h.get("command", ""))]
        hooks[ev] = [g for g in groups if isinstance(g, dict) and g.get("hooks")]

    for ev, matcher, mode in SPECS:
        cmd = BASE + " " + mode
        groups = hooks.setdefault(ev, [])
        if not any(h.get("command") == cmd for g in groups for h in g.get("hooks", [])):
            g = {"hooks": [{"type": "command", "command": cmd, "timeout": 5}]}
            if matcher:
                g["matcher"] = matcher
            groups.append(g)

    tmp = p + ".tmp"
    json.dump(s, open(tmp, "w", encoding="utf-8"), indent=2)
    os.replace(tmp, p)
    print("clawdmeter hooks installed in", p)


try:
    main()
except Exception as e:
    print(f"clawdmeter: hook install failed: {e}", file=sys.stderr)
