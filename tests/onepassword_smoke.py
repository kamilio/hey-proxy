"""Synthetic 1Password transport and deployment-failure checks; no real secrets."""
import concurrent.futures
import contextlib
import http.server
import importlib.util
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time
import urllib.request
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get("HEY_PROXY_TEST_BINARY", ROOT / "target/debug/hey-proxy"))

class Upstream(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if self.path == "/v1/responses":
            assert self.headers["Authorization"] == "Bearer synthetic-op-secret"
            response = {"id": "openai-synthetic", "output": []}
        else:
            assert self.headers["x-goog-api-key"] == "synthetic-op-secret"
            assert not self.headers.get("Authorization")
            response = {"candidates": [{"content": {"parts": [{"text": "Gemini works"}]}, "finishReason": "STOP"}]}
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(json.dumps(response).encode())

    def log_message(self, *_):
        pass

with tempfile.TemporaryDirectory(prefix="hey-proxy-op-test-") as directory:
    root = Path(directory)
    cli = root / "op"
    cli.write_text("""#!/usr/bin/env python3
import json,sys
from pathlib import Path
assert sys.argv[1:] == ["read", "--no-newline", "--", "op://Agents/hey-proxy/credential"]
with Path(__file__).with_suffix(".calls").open("a") as file:
    file.write("called\\n")
print("synthetic-op-secret", end="")
""")
    cli.chmod(0o700)
    upstream = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    config = root / "config.json"
    config.write_text(json.dumps({
        "listen": f"127.0.0.1:{port}",
        "providers": {
            "openai": {
                "upstream_url": f"http://127.0.0.1:{upstream.server_port}",
                "api_keys": {"default": "op://Agents/hey-proxy/credential"},
            },
            "gemini": {
                "upstream_url": f"http://127.0.0.1:{upstream.server_port}",
                "auth": "api_key",
                "api_key": "op://Agents/hey-proxy/credential",
            },
        },
        "aliases": [],
    }))
    env = dict(os.environ, HEY_PROXY_OP_CLI=str(cli))
    checked = subprocess.run([str(BINARY), "--config", str(config), "check-credentials"], env=env, capture_output=True, text=True)
    assert checked.returncode == 0, checked.stderr
    assert "synthetic-op-secret" not in checked.stdout + checked.stderr
    assert cli.with_suffix(".calls").read_text() == "called\n"
    with (root / "proxy.log").open("w") as output:
        process = subprocess.Popen([str(BINARY), "--config", str(config)], env=env, stdout=output, stderr=output)
        try:
            for _ in range(100):
                assert process.poll() is None
                try:
                    with urllib.request.urlopen(f"http://127.0.0.1:{port}/logs/api", timeout=1):
                        break
                except OSError:
                    time.sleep(.05)
            else:
                raise AssertionError("proxy did not become ready")

            def call(index):
                model = "gemini/test" if index % 2 else "openai-test"
                data = json.dumps({"model": model, "input": "synthetic"}).encode()
                request = urllib.request.Request(f"http://127.0.0.1:{port}/v1/responses", data=data, headers={"Content-Type": "application/json"})
                with urllib.request.urlopen(request, timeout=10) as response:
                    body = json.load(response)
                if index % 2:
                    assert body["status"] == "completed"
                else:
                    assert body["id"] == "openai-synthetic"

            with concurrent.futures.ThreadPoolExecutor(max_workers=16) as pool:
                list(pool.map(call, range(32)))
            # One execution in preflight, one in the separately started proxy.
            assert cli.with_suffix(".calls").read_text() == "called\ncalled\n"
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/logs/api", timeout=5) as response:
                assert "synthetic-op-secret" not in response.read().decode()
        finally:
            process.terminate()
            process.wait(timeout=10)
            upstream.shutdown()
    assert "synthetic-op-secret" not in (root / "proxy.log").read_text()

    spec = importlib.util.spec_from_file_location("remote_service", ROOT / "src/remote_service.py")
    remote = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(remote)
    home = root / "remote"
    installed = home / ".cargo/bin/hey-proxy"
    installed.parent.mkdir(parents=True)
    installed.write_bytes(b"existing binary")
    current = home / ".hey-proxy/config.json"
    current.parent.mkdir(parents=True)
    current.write_bytes(b"existing config")
    failing = root / "candidate"
    failing.write_text("#!/bin/sh\nexit 1\n")
    failing.chmod(0o700)
    with patch.object(Path, "home", return_value=home), contextlib.redirect_stdout(open(os.devnull, "w")):
        try:
            remote.install(str(failing), str(config), "http://127.0.0.1:8080/v1")
        except subprocess.CalledProcessError:
            pass
        else:
            raise AssertionError("failed credential preflight was ignored")
    assert installed.read_bytes() == b"existing binary"
    assert current.read_bytes() == b"existing config"
print("1Password smoke passed: 32 HTTP requests, one runtime lookup, no secret output, failed remote preflight preserves service")
