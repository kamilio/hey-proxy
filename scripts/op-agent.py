#!/usr/bin/env python3
"""Run official op using hey-proxy's OS-stored service identity. No disk cache."""
import os
from pathlib import Path
import resource
import shutil
import subprocess
import sys

SERVICE = "hey-proxy.1password.agent"


def capture(arguments):
    result = subprocess.run(arguments, capture_output=True, timeout=10)
    if result.returncode:
        raise RuntimeError("OS credential unavailable")
    return result.stdout


def bootstrap():
    if sys.platform == "darwin":
        return capture(["/usr/bin/security", "find-generic-password", "-s", SERVICE,
                        "-a", "Agents", "-w"]).strip()
    if sys.platform == "linux":
        keyctl = shutil.which("keyctl") or "/usr/bin/keyctl"
        key = capture([keyctl, "search", "@u", "user", SERVICE]).decode().strip()
        if not key.isdecimal():
            raise RuntimeError("Invalid key identifier")
        return capture([keyctl, "pipe", key]).strip()
    raise RuntimeError("Unsupported OS credential store")


def main():
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    token = bootstrap().decode("utf-8")
    if not token or len(token) > 16384 or any(c.isspace() for c in token):
        raise RuntimeError("Invalid bootstrap credential")
    candidates = [Path.home() / ".local/bin/op", Path("/opt/homebrew/bin/op"),
                  Path("/usr/local/bin/op"), Path("/usr/bin/op")]
    program = next((str(p) for p in candidates if p.is_file()), None)
    if program is None:
        raise RuntimeError("Install the official 1Password CLI")
    env = {k: v for k, v in os.environ.items()
           if not k.startswith("OP_SESSION_") and k not in ("OP_ACCOUNT", "OP_SERVICE_ACCOUNT_TOKEN")}
    env["OP_SERVICE_ACCOUNT_TOKEN"] = token
    os.execve(program, [program, *sys.argv[1:]], env)


if __name__ == "__main__":
    try:
        main()
    except Exception:
        print("hey-proxy: agent credential unavailable; check OS storage and op installation", file=sys.stderr)
        sys.exit(1)
