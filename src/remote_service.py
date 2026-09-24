"""Remote per-user service setup. No root access or auth/policy edits."""
import json
import hashlib
import os
from pathlib import Path
import plistlib
import subprocess
import sys
import tempfile
import time
import urllib.request


def run(*args, **kwargs):
    return subprocess.run(args, check=True, **kwargs)


def preflight():
    if sys.platform == 'linux':
        run('systemctl', '--user', 'show-environment', stdout=subprocess.DEVNULL)
    elif sys.platform == 'darwin':
        run('launchctl', 'print', f'gui/{os.getuid()}', stdout=subprocess.DEVNULL)
    else:
        raise RuntimeError('Rollout supports Linux with a systemd user session and macOS with a GUI login session')


def atomic(path, data, mode, backup=False):
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists() and path.read_bytes() == data:
        return
    if backup and path.exists():
        copy = path.with_name(path.name + f'.backup-{time.time_ns()}')
        atomic(copy, path.read_bytes(), mode)
        print(f'Backup: {copy}', flush=True)
    fd, name = tempfile.mkstemp(dir=path.parent)
    try:
        with os.fdopen(fd, 'wb') as file:
            file.write(data)
            file.flush()
            os.fsync(file.fileno())
        os.chmod(name, mode)
        os.replace(name, path)
    finally:
        if os.path.exists(name):
            os.unlink(name)


def restart_macos(data, previous=None):
    home = Path.home()
    domain = f'gui/{os.getuid()}'
    target = domain + '/com.hey-proxy'
    loaded = subprocess.run(['launchctl', 'print', target], capture_output=True).returncode == 0
    # Replacing an executable/config at the same paths needs only a restart.
    # Re-registering a healthy service introduces an unnecessary outage.
    if loaded and previous is not None and plistlib.loads(data) == plistlib.loads(previous):
        run('launchctl', 'kickstart', '-k', target)
        return
    if loaded:
        run('launchctl', 'bootout', target, capture_output=True)
    # launchd can reject a recently booted-out registration path with EIO over
    # SSH. A fresh private path avoids that cache; the login plist remains the
    # persistent definition. Registration reads the file before returning.
    for attempt in range(5):
        fd, name = tempfile.mkstemp(prefix='service-registration-', suffix='.plist', dir=home / '.hey-proxy')
        try:
            with os.fdopen(fd, 'wb') as file:
                file.write(data)
                file.flush()
                os.fsync(file.fileno())
            result = subprocess.run(['launchctl', 'bootstrap', domain, name], capture_output=True)
            if result.returncode == 0:
                return
        finally:
            os.unlink(name)
        time.sleep(1)
    raise RuntimeError('launchd registration failed after five attempts')


def install(binary, source_config, base_url, codex_home="", model=""):
    home = Path.home()
    config_path = home / '.hey-proxy/config.json'
    deployment_path = Path(source_config).with_name('deployment.json')
    deployment = json.loads(deployment_path.read_text()) if deployment_path.exists() else {}
    config = json.loads(Path(source_config).read_bytes())
    retained = deployment.get('retained_openai_keys', {})
    retained_gemini = deployment.get('retained_gemini_key')
    if retained or retained_gemini:
        existing = json.loads(config_path.read_bytes()) if config_path.exists() else {}
        keys = existing.get('providers', {}).get('openai', existing).get('api_keys', {})
        for project, digest in retained.items():
            secret = keys.get(project, '')
            if not secret or hashlib.sha256(secret.encode()).hexdigest() != digest:
                raise RuntimeError('Remote OpenAI key differs or is missing; provision a protected credential source before rollout')
            config['providers']['openai']['api_keys'][project] = secret
        if retained_gemini:
            provider = existing.get('providers', {}).get('gemini', existing.get('gemini', {}))
            secret = provider.get('api_key', '')
            if not secret or hashlib.sha256(secret.encode()).hexdigest() != retained_gemini:
                raise RuntimeError('Remote Gemini key differs or is missing; provision a protected credential source before rollout')
            config['providers']['gemini']['api_key'] = secret
    prepared = (json.dumps(config, indent=2) + '\n').encode()
    # stdin avoids another plaintext candidate config or secret process argument.
    run(str(binary), '--config', '/dev/stdin', 'check-credentials', input=prepared, capture_output=True)
    profile = deployment.get('gemini_profile')
    if profile:
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        # A tunneled profile can be checked before any service is replaced.
        if profile['base_url'] != base_url:
            with opener.open(profile['base_url'].removesuffix('/v1') + '/logs/api', timeout=20) as response:
                if response.status != 200:
                    raise RuntimeError('Gemini profile endpoint unavailable')
    paths = [home / '.cargo/bin/hey-proxy', config_path,
             home / 'Library/LaunchAgents/com.hey-proxy.plist',
             home / '.hey-proxy/service-registration.plist',
             home / '.config/systemd/user/hey-proxy.service']
    snapshots = {p: (p.read_bytes(), p.stat().st_mode & 0o777) if p.exists() else None for p in paths}
    try:
        _install(binary, source_config, base_url, codex_home, model, prepared, profile)
    except BaseException:
        plist = home / 'Library/LaunchAgents/com.hey-proxy.plist'
        installed_plist = plist.read_bytes() if plist.exists() else None
        for path, snapshot in snapshots.items():
            if snapshot is None:
                path.unlink(missing_ok=True)
            else:
                atomic(path, *snapshot)
        if snapshots[home / '.cargo/bin/hey-proxy'] is not None:
            if sys.platform == 'linux':
                run('systemctl', '--user', 'daemon-reload')
                run('systemctl', '--user', 'restart', 'hey-proxy.service')
            else:
                if plist.exists():
                    restart_macos(plist.read_bytes(), installed_plist)
                else:
                    subprocess.run(['launchctl', 'bootout', f'gui/{os.getuid()}/com.hey-proxy'], capture_output=True)
        raise RuntimeError('Rollout failed; previous binary and proxy configuration restored') from None


def _install(binary, source_config, base_url, codex_home, model, prepared, profile):
    home = Path.home()
    binary_path = home / '.cargo/bin/hey-proxy'
    config_path = home / '.hey-proxy/config.json'
    log_path = home / '.hey-proxy/service.log'
    temp_path = home / '.hey-proxy/tmp'
    temp_path.mkdir(parents=True, exist_ok=True, mode=0o700)
    os.chmod(temp_path, 0o700)
    os.chmod(config_path.parent, 0o700)
    # Check external references on the destination before replacing either its
    # running executable or configuration. Resolved secrets never enter the bundle.
    atomic(binary_path, Path(binary).read_bytes(), 0o755)
    atomic(config_path, prepared, 0o600)
    config = json.loads(config_path.read_text())
    if config.get('mode', 'standalone') == 'host':
        run(str(binary_path), '--config', str(config_path), 'host-keys', stdout=subprocess.DEVNULL)
    # Codex setup is a separate, explicit CLI command. Rollout never edits its files.
    token = None
    if config.get('mode') == 'host':
        token = json.loads(config_path.with_suffix('.access-keys.json').read_text())['local']
    if sys.platform == 'linux':
        unit = home / '.config/systemd/user/hey-proxy.service'
        # systemd specifier expansion also applies inside quoted arguments.
        def escape(path):
            return json.dumps(str(path)).replace('%', '%%')
        content = '\n'.join([
            '[Unit]', 'Description=hey-proxy model routing', 'After=network.target',
            '[Service]', f'ExecStart={escape(binary_path)} --config {escape(config_path)}',
            f'Environment={escape("TMPDIR=" + str(temp_path))}',
            'Restart=on-failure', 'RestartSec=2', 'TimeoutStopSec=15', 'UMask=0077',
            '[Install]', 'WantedBy=default.target', '',
        ])
        atomic(unit, content.encode(), 0o600)
        run('systemctl', '--user', 'daemon-reload')
        run('systemctl', '--user', 'enable', 'hey-proxy.service')
        run('systemctl', '--user', 'restart', 'hey-proxy.service')
        run('systemctl', '--user', 'is-active', '--quiet', 'hey-proxy.service')
    else:
        label = 'com.hey-proxy'
        plist = home / 'Library/LaunchAgents/com.hey-proxy.plist'
        previous = plist.read_bytes() if plist.exists() else None
        data = plistlib.dumps(dict(Label=label, ProgramArguments=[str(binary_path), '--config', str(config_path)],
            RunAtLoad=True, KeepAlive=True, EnvironmentVariables={"TMPDIR": str(temp_path)}, StandardOutPath=str(log_path), StandardErrorPath=str(log_path), ThrottleInterval=2))
        atomic(plist, data, 0o600)
        restart_macos(data, previous)
    # Bypass proxy env vars for the local readiness probe.
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    for attempt in range(90):
        try:
            request = urllib.request.Request(base_url.removesuffix('/v1') + '/logs/api')
            if token:
                request.add_header('Authorization', 'Bearer ' + token)
            with opener.open(request, timeout=5) as response:
                data = json.load(response)
                if response.status != 200 or not isinstance(data.get('entries'), list):
                    raise RuntimeError('Unexpected dashboard response')
            time.sleep(0.5)
            if sys.platform == 'linux':
                run('systemctl', '--user', 'is-active', '--quiet', 'hey-proxy.service')
            else:
                state = run('launchctl', 'print', f'gui/{os.getuid()}/com.hey-proxy', capture_output=True, text=True).stdout
                if 'state = running' not in state:
                    raise RuntimeError('launchd proxy is not running; another process may own the port')
            break
        except Exception:
            if attempt == 89:
                raise RuntimeError('Proxy readiness check failed; check the user service logs')
            time.sleep(0.5)
    verify = [str(binary_path), '--config', str(config_path), 'verify']
    run(*verify)
    if profile:
        print('Verifying Gemini generation through the profile and local proxy', flush=True)
        for endpoint in sorted({profile['base_url'], base_url}):
            payload = json.dumps({'model': profile['model'], 'input': 'Reply exactly OK.',
                'reasoning': {'effort': 'low'}, 'max_output_tokens': 1024}).encode()
            headers = {'Content-Type': 'application/json'}
            if token and endpoint == base_url:
                headers['Authorization'] = 'Bearer ' + token
            request = urllib.request.Request(endpoint + '/responses', data=payload, headers=headers)
            with opener.open(request, timeout=180) as response:
                result = json.load(response)
                if response.status != 200 or result.get('status') != 'completed':
                    raise RuntimeError('Gemini generation verification failed')
    print('Proxy service and connections verified; Codex configuration unchanged', flush=True)


if __name__ == '__main__':
    try:
        preflight()
        if sys.argv[1] == 'install':
            install(*sys.argv[2:])
    except Exception as error:
        print(f'hey-proxy rollout: {error}', file=sys.stderr)
        sys.exit(1)
