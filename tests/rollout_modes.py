"""Use local SSH fixtures with real host/client processes and connection checks."""
import http.server
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import urllib.request

BINARY = os.environ.get('HEY_PROXY_TEST_BINARY', str(Path.home() / '.cargo/bin/hey-proxy'))

class Upstream(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        assert self.headers.get('Authorization') == 'Bearer something'
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.end_headers()
        self.wfile.write(b'{"data":[{"id":"gpt-4.1-mini"}]}')
    def do_POST(self):
        assert self.headers.get('Authorization') == 'Bearer something'
        data = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.end_headers()
        self.wfile.write(json.dumps(data).encode())
    def log_message(self, *args):
        pass


def port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]

upstream = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Upstream)
threading.Thread(target=upstream.serve_forever, daemon=True).start()
with tempfile.TemporaryDirectory(prefix='hey-proxy-role-test-') as folder:
    root = Path(folder)
    config_path = root / 'controller.json'
    subprocess.run([BINARY, '--config', str(config_path), '--init'], check=True, capture_output=True)
    data = json.loads(config_path.read_text())
    data['providers']['openai']['api_keys'] = {'default':'something'}
    data['aliases'] = [{'from':'gpt-4.1','to':'gpt-4.1-mini','reasoning':'low'}]
    host_port, client_port, solo_port = port(), port(), port()
    data['providers']['openai']['upstream_url'] = f'http://127.0.0.1:{upstream.server_port}'
    # Client is deliberately listed first: rollout must reorder its dependency.
    data['ssh_hosts'] = [
        {'host':'fixture-client','mode':'client','via':'fixture-host','listen':f'127.0.0.1:{client_port}'},
        {'host':'fixture-solo','listen':f'127.0.0.1:{solo_port}'},
        {'host':'fixture-host','mode':'host','url':f'http://127.0.0.1:{host_port}','listen':f'127.0.0.1:{host_port}'},
    ]
    config_path.write_text(json.dumps(data))
    shim = root / 'ssh'
    shim.write_text('''#!/usr/bin/env python3
import json, os, pathlib, shlex, subprocess, sys, tempfile, time, urllib.request
host, command = sys.argv[-2:]
root = pathlib.Path(os.environ['ROLE_FIXTURE'])
home = root / host
home.mkdir(exist_ok=True)
binary = os.environ['ROLE_BINARY']
with open(root / 'calls', 'a') as log: log.write(host + ' ' + command.split()[0] + '\\n')
if command.startswith('umask'):
    base = home / '.hey-proxy/staging'
    base.mkdir(parents=True, exist_ok=True)
    print(tempfile.mkdtemp(prefix='hey-proxy-rollout.', dir=base))
elif command.startswith('tar ') or command.startswith('rm '):
    subprocess.run(shlex.split(command), check=True)
elif command.startswith('exec '):
    args = shlex.split(shlex.split(command)[-1])
    stage = pathlib.Path(args[2])
    config = json.loads((stage / 'remote-config.json').read_text())
    assert config.get('ssh_hosts', []) == []
    if config['mode'] == 'client':
        assert config.get('providers', {}) == {} and config['aliases'] == []
        assert 'something' not in json.dumps(config)
    deployment = json.loads((stage / 'deployment.json').read_text())
    if config['mode'] != 'client':
        assert config['providers']['openai']['api_keys']['default'] == 'retained-on-destination'
        existing_path = home/'.hey-proxy/config.json'
        if not existing_path.exists():
            existing_path.parent.mkdir(parents=True, exist_ok=True)
            existing = json.loads(json.dumps(config))
            existing['providers']['openai']['api_keys'] = json.loads((root/'controller.json').read_text())['providers']['openai']['api_keys']
            existing_path.write_text(json.dumps(existing))
    # Exercise the real installer; fake only the OS service-manager commands.
    import importlib.util, plistlib, types
    spec = importlib.util.spec_from_file_location('fixture_service', stage/'src/remote_service.py')
    service = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(service)
    os.environ['HOME'] = str(home)
    os.environ.pop('CODEX_HOME', None)
    service.sys = types.SimpleNamespace(platform='linux' if host == 'fixture-client' else 'darwin')
    original_run = subprocess.run
    pidfile = home/'pid'
    def stop():
        if pidfile.exists():
            try: os.kill(int(pidfile.read_text()), 15)
            except ProcessLookupError: pass
            time.sleep(0.15)
    def start():
        if service.sys.platform == 'darwin':
            plist = plistlib.loads((home/'Library/LaunchAgents/com.hey-proxy.plist').read_bytes())
            argv = plist['ProgramArguments']
            variables = plist['EnvironmentVariables']
        else:
            unit = (home/'.config/systemd/user/hey-proxy.service').read_text()
            argv = shlex.split(next(line.removeprefix('ExecStart=') for line in unit.splitlines() if line.startswith('ExecStart=')))
            setting = shlex.split(next(line.removeprefix('Environment=') for line in unit.splitlines() if line.startswith('Environment=')))[0]
            variables = dict([setting.split('=',1)])
        assert variables['TMPDIR'] == str(home/'.hey-proxy/tmp')
        log = open(home/'service.log','ab')
        process = subprocess.Popen(argv, env=dict(os.environ, **variables), stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True)
        pidfile.write_text(str(process.pid))
    def managed_run(argv, **kwargs):
        if argv[0] not in ('launchctl','systemctl'):
            return original_run(argv, **kwargs)
        if 'bootout' in argv: stop()
        if 'bootstrap' in argv or 'restart' in argv or 'kickstart' in argv:
            stop()
            start()
        stdout = 'state = running\\n' if kwargs.get('text') else b''
        return subprocess.CompletedProcess(argv, 0, stdout=stdout, stderr='')
    service.subprocess = types.SimpleNamespace(run=managed_run, DEVNULL=subprocess.DEVNULL)
    service.install(binary, str(stage/'remote-config.json'), args[3])
elif command.startswith('"$HOME/.cargo/bin/hey-proxy" host-keys'):
    args = shlex.split(command)[2:]
    subprocess.run([binary,'--config',str(home/'.hey-proxy/config.json'),'host-keys',*args], check=True)
else: raise AssertionError(command)
''')
    shim.chmod(0o755)
    env = dict(os.environ, PATH=str(root)+':'+os.environ['PATH'], ROLE_FIXTURE=str(root), ROLE_BINARY=BINARY)
    cmd = [BINARY, '--config', str(config_path), 'rollout']
    try:
        result = subprocess.run(cmd, env=env, capture_output=True, text=True)
        assert result.returncode == 0, result.stdout + result.stderr
        calls = (root/'calls').read_text().splitlines()
        assert calls[0].startswith('fixture-host ')
        host_keys = json.loads((root/'fixture-host/.hey-proxy/config.access-keys.json').read_text())
        client_config = json.loads((root/'fixture-client/.hey-proxy/config.json').read_text())
        assert client_config['connection']['api_key'] == host_keys['clients']['fixture-client']
        response = urllib.request.urlopen(urllib.request.Request(f'http://127.0.0.1:{client_port}/v1/responses', data=b'{"model":"gpt-4.1","input":"fixture"}', headers={'Content-Type':'application/json'}))
        assert json.load(response)['model'] == 'gpt-4.1-mini'
        # Selecting only the client must also update its host and reuse the access key.
        result = subprocess.run(cmd+['--host','fixture-client'], env=env, capture_output=True, text=True)
        assert result.returncode == 0, result.stdout + result.stderr
        assert json.loads((root/'fixture-host/.hey-proxy/config.access-keys.json').read_text()) == host_keys
        assert host_keys['local'] not in result.stdout and host_keys['clients']['fixture-client'] not in result.stdout
        # Verification must fail when the actual upstream connection fails.
        upstream.shutdown()
        upstream.server_close()
        result = subprocess.run(cmd+['--host','fixture-solo'], env=env, capture_output=True, text=True)
        assert result.returncode != 0 and 'fixture-solo' in result.stderr
        calls_before = (root/'calls').read_text().count('fixture-client ')
        result = subprocess.run(cmd+['--host','fixture-client'], env=env, capture_output=True, text=True)
        assert result.returncode != 0 and 'skipped because its host failed' in result.stderr
        assert (root/'calls').read_text().count('fixture-client ') == calls_before
        print('Role rollout passed: ordering, real connections, key distribution/reuse, secret isolation, failure propagation')
    finally:
        for pidfile in root.glob('*/pid'):
            try: os.kill(int(pidfile.read_text()), 15)
            except ProcessLookupError: pass
        upstream.shutdown()
        upstream.server_close()
