"""Exercise launchd restart/registration behavior without touching services."""
import importlib.util
from pathlib import Path
import plistlib
import subprocess
import tempfile
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('remote_service', Path(__file__).resolve().parents[1] / 'src/remote_service.py')
service = importlib.util.module_from_spec(spec)
spec.loader.exec_module(service)
data = plistlib.dumps({'Label': 'com.hey-proxy', 'ProgramArguments': ['/fixture/hey-proxy']})
previous = plistlib.dumps({'Label': 'com.hey-proxy', 'ProgramArguments': ['/fixture/old-proxy']})

with tempfile.TemporaryDirectory() as directory:
    home = Path(directory)
    (home / '.hey-proxy').mkdir()
    with patch.object(Path, 'home', return_value=home), patch.object(service.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0)) as run:
        service.restart_macos(data, data)
        assert [call.args[0][1] for call in run.call_args_list] == ['print', 'kickstart']

    for loaded, old in [(False, None), (True, previous)]:
        operations = []
        paths = []
        def fake_run(argv, **kwargs):
            operations.append(argv[1])
            if argv[1] == 'print':
                return subprocess.CompletedProcess(argv, 0 if loaded else 1)
            if argv[1] == 'bootstrap':
                path = Path(argv[-1])
                assert path.parent == home / '.hey-proxy'
                assert path.read_bytes() == data and path.stat().st_mode & 0o777 == 0o600
                paths.append(path)
                return subprocess.CompletedProcess(argv, 5 if len(paths) == 1 else 0)
            assert argv[1] == 'bootout'
            return subprocess.CompletedProcess(argv, 0)
        with patch.object(Path, 'home', return_value=home), patch.object(service.subprocess, 'run', side_effect=fake_run), patch.object(service.time, 'sleep'):
            service.restart_macos(data, old)
        assert operations == ['print'] + (['bootout'] if loaded else []) + ['bootstrap', 'bootstrap']
        assert len(set(paths)) == 2 and not any(p.exists() for p in paths)

print('macOS restart checks passed: reuse, changed definition, EIO retry, private registration, cleanup')
