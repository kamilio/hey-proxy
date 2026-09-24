"""Supervise a loopback-only Gemini tunnel; never move Google credentials."""
import hashlib
import json
import os
from pathlib import Path
import plistlib
import subprocess
import sys
import time


def setup(host, address):
    home = Path.home()
    label = 'local.hey-proxy.gemini-tunnel.' + hashlib.sha256(host.encode()).hexdigest()[:16]
    forward = '127.0.0.1:18082:' + address
    argv = ['/usr/bin/ssh', '-N', '-T', '-o', 'BatchMode=yes', '-o', 'ExitOnForwardFailure=yes',
            '-o', 'ServerAliveInterval=15', '-o', 'ServerAliveCountMax=3', '-o', 'ConnectTimeout=10',
            '-o', 'ControlMaster=no', '-o', 'ControlPath=none', '-R', forward, host]
    if sys.platform == 'darwin':
        directory = home / 'Library/LaunchAgents'
        directory.mkdir(parents=True, exist_ok=True)
        # Adopt an existing matching managed tunnel, including earlier installs.
        for path in directory.glob('local.hey-proxy.gemini-tunnel.*.plist'):
            current = plistlib.loads(path.read_bytes())
            args = current.get('ProgramArguments', [])
            if args and args[-1] == host and forward in args:
                label = current['Label']
                break
        path = directory / (label + '.plist')
        env = {'PATH': '/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin', 'HOME': str(home)}
        if os.environ.get('SSH_AUTH_SOCK'):
            env['SSH_AUTH_SOCK'] = os.environ['SSH_AUTH_SOCK']
        log = home / '.hey-proxy' / (label + '.log')
        data = plistlib.dumps(dict(Label=label, ProgramArguments=argv, EnvironmentVariables=env,
            RunAtLoad=True, KeepAlive=True, ThrottleInterval=10, StandardOutPath=str(log), StandardErrorPath=str(log)))
        domain = f'gui/{os.getuid()}'
        active = subprocess.run(['launchctl', 'print', domain + '/' + label], capture_output=True).returncode == 0
        if not path.exists() or path.read_bytes() != data:
            # This operation only changes a tunnel, never the main proxy.
            path.write_bytes(data)
            path.chmod(0o600)
            if active:
                subprocess.run(['launchctl', 'bootout', domain + '/' + label], check=True, capture_output=True)
                active = False
        if not active:
            subprocess.run(['launchctl', 'bootstrap', domain, str(path)], check=True, capture_output=True)
    elif sys.platform == 'linux':
        directory = home / '.config/systemd/user'
        directory.mkdir(parents=True, exist_ok=True)
        path = directory / (label + '.service')
        command = ' '.join(json.dumps(arg).replace('%', '%%') for arg in argv)
        data = f'[Unit]\nDescription=hey-proxy Gemini SSH tunnel\n[Service]\nExecStart={command}\nRestart=always\nRestartSec=10\n[Install]\nWantedBy=default.target\n'
        changed = not path.exists() or path.read_text() != data
        if changed:
            path.write_text(data)
            path.chmod(0o600)
            subprocess.run(['systemctl', '--user', 'daemon-reload'], check=True)
        subprocess.run(['systemctl', '--user', 'enable', '--now', path.name], check=True)
        if changed:
            subprocess.run(['systemctl', '--user', 'restart', path.name], check=True)
    else:
        raise RuntimeError('Controller tunnel requires launchd or user systemd')
    probe = "python3 -c \"import urllib.request; r=urllib.request.urlopen('http://127.0.0.1:18082/logs/api',timeout=10); assert r.status==200\""
    for attempt in range(12):
        result = subprocess.run(['/usr/bin/ssh', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=10', '--', host, probe], capture_output=True, timeout=25)
        if result.returncode == 0:
            print('Gemini tunnel verified: ' + host)
            return
        time.sleep(1)
    raise RuntimeError('Remote Gemini tunnel did not become ready')


if __name__ == '__main__':
    setup(*sys.argv[1:])
