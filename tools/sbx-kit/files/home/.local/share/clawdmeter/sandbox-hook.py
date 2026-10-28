# Clawdmeter hook for Claude Code in a Docker Sandbox: POSTs the hook JSON
# to the Clawdmeter app's hook listener on the host (POST /hook?mode=…),
# which writes the same files `clawdmeter hook <mode>` writes. Installed by
# the clawdmeter sbx kit as `sandbox-hook.py event|alert event|clear event`.
#
# Never fails: a refused or timed-out connection means the app isn't
# listening — the hook exits 0 either way, so Claude Code is unaffected.
import os, sys, urllib.request

URL = os.environ.get("CLAWDMETER_HOOK_URL") or "http://host.docker.internal:47991/hook"

try:
    body = "" if sys.stdin.isatty() else sys.stdin.read()
    args = sys.argv[1:3]
    if body and args:
        req = urllib.request.Request(
            f"{URL}?mode={'+'.join(args)}",
            data=body.encode(),
            method="POST",
            headers={"Content-Type": "application/json"},
        )
        urllib.request.urlopen(req, timeout=2).close()
except Exception:
    pass
