"""Credential retention, profile deployment and rollback use synthetic data only."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get('HEY_PROXY_TEST_BINARY', ROOT / 'target/debug/hey-proxy'))
spec = importlib.util.spec_from_file_location('remote_service', ROOT / 'src/remote_service.py')
service = importlib.util.module_from_spec(spec)
spec.loader.exec_module(service)
with tempfile.TemporaryDirectory() as directory:
    home = Path(directory)
    config = home / '.hey-proxy/config.json'
    config.parent.mkdir()
    binary = home / '.cargo/bin/hey-proxy'
    binary.parent.mkdir(parents=True)
    binary.write_bytes(b'previous executable')
    source = home / 'staged.json'
    original = {'listen':'127.0.0.1:8080','providers':{'openai':{'api_keys':{'default':'synthetic-retained-key'}}}}
    config.write_text(json.dumps(original))
    candidate = json.loads(json.dumps(original))
    candidate['providers']['openai']['api_keys']['default'] = 'retained-on-destination'
    source.write_text(json.dumps(candidate))
    deployment = home / 'deployment.json'
    deployment.write_text(json.dumps({'retained_openai_keys': {'default':hashlib.sha256(b'synthetic-retained-key').hexdigest()}}))
    profile = home / '.codex/gemini.config.toml'
    profile.parent.mkdir()
    profile.write_text('model_reasoning_effort="low"\n')
    initial = {p:p.read_bytes() for p in [config,binary,profile]}
    def fail_install(*args):
        prepared = json.loads(args[5])
        assert prepared['providers']['openai']['api_keys']['default'] == 'synthetic-retained-key'
        config.write_text('broken configuration')
        binary.write_bytes(b'broken executable')
        raise RuntimeError('synthetic post-install failure')
    real_run = service.run
    def run(*args,**kwargs):
        if args[0] in ('systemctl','launchctl'):
            return subprocess.CompletedProcess(args,0)
        return real_run(*args,**kwargs)
    with patch.object(Path, 'home', return_value=home), patch.dict(os.environ, {'HOME':str(home)}, clear=False), patch.object(service, '_install', side_effect=fail_install), patch.object(service, 'run', side_effect=run), patch.object(service.subprocess, 'run', wraps=subprocess.run):
        # Avoid a real macOS launchctl bootout during the synthetic rollback.
        def launch_or_real(args,**kwargs):
            if args[0] in ('launchctl','systemctl'):return subprocess.CompletedProcess(args,0)
            return original_run(args,**kwargs)
        original_run = service.subprocess.run._mock_wraps
        service.subprocess.run.side_effect = launch_or_real
        try:
            service.install(str(BINARY),str(source),'http://127.0.0.1:8080/v1',str(profile.parent))
        except RuntimeError as e:
            assert 'restored' in str(e)
        else:raise AssertionError('expected failure')
    assert all(p.read_bytes()==data for p,data in initial.items())
    assert b'synthetic-retained-key' not in source.read_bytes()
    assert not list(config.parent.glob('*.backup-*'))
    candidate['providers']['openai']['api_keys']['default']='retained-on-destination'
    deployment.write_text(json.dumps({'retained_openai_keys':{'default':'incorrect-digest'}}))
    with patch.object(Path,'home',return_value=home), patch.object(service,'_install') as install:
        try:service.install(str(BINARY),str(source),'http://127.0.0.1:8080/v1')
        except RuntimeError as e:assert 'differs or is missing' in str(e)
        else:raise AssertionError('mismatched keys accepted')
        install.assert_not_called()
    assert all(p.read_bytes()==data for p,data in initial.items())
print('Rollout retention/preflight/rollback checks passed')

# Actual service installation must not read, create, or rewrite Codex configuration.
with tempfile.TemporaryDirectory() as directory:
    home = Path(directory)
    (home / '.hey-proxy').mkdir()
    codex = home / '.codex'
    codex.mkdir()
    sentinel = b'not even TOML: preserved without parsing\n'
    (codex / 'config.toml').write_bytes(sentinel)
    (codex / 'gemini.config.toml').write_bytes(sentinel)
    source_binary = home / 'binary'
    source_binary.write_bytes(b'synthetic binary')
    prepared = json.dumps({'listen':'127.0.0.1:8080','providers':{'openai':{'api_keys':{'default':'synthetic'}}}}).encode()
    calls = []
    def fake_run(*args, **kwargs):
        calls.append(args)
        return subprocess.CompletedProcess(args, 0, stdout='state = running')
    class Ready:
        status = 200
        def __enter__(self): return self
        def __exit__(self, *_): pass
        def read(self): return b'{"entries":[]}'
    class Opener:
        def open(self, *_args, **_kwargs): return Ready()
    with patch.object(Path, 'home', return_value=home), patch.object(service, 'run', side_effect=fake_run), patch.object(service, 'restart_macos'), patch.object(service.urllib.request, 'build_opener', return_value=Opener()), patch.object(service.time, 'sleep'):
        service._install(str(source_binary),str(home/'staged.json'),'http://127.0.0.1:8080/v1',str(codex),'gpt-4.1',prepared,None)
    assert {p.name:p.read_bytes() for p in codex.iterdir()} == {'config.toml':sentinel,'gemini.config.toml':sentinel}
    assert not any('configure-codex' in args or 'configure-gemini' in args or '--codex-home' in args for args in calls)
    assert any('verify' in args for args in calls)
print('Default service installation leaves all Codex files unchanged')
