"""Exercise the installed CLI's SSH orchestration without touching real hosts."""
import json
import os
from pathlib import Path
import subprocess
import tempfile

binary = os.environ.get('HEY_PROXY_TEST_BINARY', str(Path.home() / '.cargo/bin/hey-proxy'))
with tempfile.TemporaryDirectory(prefix='hey-proxy-test-') as folder:
    root = Path(folder)
    config = root / 'config.json'
    subprocess.run([binary, '--config', str(config), '--init'], check=True, capture_output=True)
    data = json.loads(config.read_text())
    data['aliases'] = [{'from':'coding','to':'gpt-4.1-mini'}]
    data['fallbacks'] = {'gpt-4.1':['gpt-4.1-mini']}
    data['ssh_hosts'] = ['fixture-ok', 'fixture-fail']
    data['providers']['openai']['api_keys']['default'] = 'op://Agents/hey-proxy/codex'
    config.write_text(json.dumps(data))
    shim = root / 'ssh'
    shim.write_text('''#!/usr/bin/env python3
import json, os, pathlib, shlex, subprocess, sys, tempfile
host, command = sys.argv[-2:]
with open(os.environ['FIXTURE_LOG'], 'a') as log:
    log.write(host + ' ' + command.split()[0] + '\\n')
if command.startswith('umask'):
    base = pathlib.Path(os.environ['FIXTURE_LOG']).parent / '.hey-proxy/staging'
    base.mkdir(parents=True, exist_ok=True)
    print(tempfile.mkdtemp(prefix='hey-proxy-rollout.', dir=base))
elif command.startswith('tar '):
    subprocess.run(shlex.split(command), check=True)
elif command.startswith('exec '):
    args = shlex.split(shlex.split(command)[-1])
    stage = pathlib.Path(args[2])
    config = json.loads((stage / 'remote-config.json').read_text())
    assert config.get('ssh_hosts', []) == []
    assert config['providers']['openai']['api_keys']['default'] == 'op://Agents/hey-proxy/codex'
    assert 'check-credentials' in (stage / 'src/remote_service.py').read_text()
    assert config['aliases'][0]['to'] == 'gpt-4.1-mini'
    assert config['fallbacks'] == {'gpt-4.1':['gpt-4.1-mini']}
    assert (stage / 'README.md').is_file() and (stage / 'LICENSE').is_file()
    assert (stage / 'src/rollout.rs').is_file()
    assert (stage / 'src/proxy/logs.html').is_file()
    assert (stage / 'tests/gemini_conversion.rs').is_file()
    assert (stage / 'tests/credential_sources.rs').is_file()
    assert (stage / 'tests/gemini_regression.rs').is_file()
    assert (stage / 'tests/gemini_hardening.rs').is_file()
    assert (stage / 'src/proxy/gemini/hardening_tests.rs').is_file()
    assert (stage / 'src/gemini/validate.rs').is_file()
    assert (stage / 'tests/gemini_stream_partitions.rs').is_file()
    assert args[3] == 'http://127.0.0.1:8080/v1'
    if host == 'fixture-fail': sys.exit(1)
elif command.startswith('rm '):
    subprocess.run(shlex.split(command), check=True)
else:
    raise AssertionError(command)
''')
    shim.chmod(0o755)
    env = dict(os.environ, PATH=str(root) + ':' + os.environ['PATH'], FIXTURE_LOG=str(root / 'calls'))
    cmd = [binary, '--config', str(config), 'rollout']
    successful = subprocess.run(cmd + ['--host', 'fixture-ok'], env=env, capture_output=True, text=True)
    assert successful.returncode == 0, successful.stderr
    assert 'fixture-ok: rollout verified' in successful.stdout
    mixed = subprocess.run(cmd, env=env, capture_output=True, text=True)
    assert mixed.returncode != 0
    assert 'fixture-ok: rollout verified' in mixed.stdout
    assert 'Rollout failed on: fixture-fail' in mixed.stderr
    calls = (root / 'calls').read_text()
    assert calls.count(' rm') == 3, 'remote staging not cleaned up'
    print('SSH transport fixture passed: upload, selected hosts, partial failure, cleanup')
