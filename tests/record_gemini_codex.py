"""Record an isolated Codex run against the real proxy, with native mock or Vertex.

python3 tests/record_gemini_codex.py --mock
python3 tests/record_gemini_codex.py --config ~/.hey-proxy/gemini-preview.json
python3 tests/record_gemini_codex.py --config ~/.hey-proxy/gemini-preview.json --adc --task repo-tests
No running service is stopped, installed or modified. Credentials are never recorded.
"""
import argparse
import difflib
import http.server
import json
import os
import pathlib
import socket
import subprocess
import threading
import time
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument('--mock', action='store_true')
parser.add_argument('--config', type=pathlib.Path)
parser.add_argument('--adc', action='store_true', help='Use the official ADC resolver for this isolated run')
parser.add_argument('--model', default='gemini-test')
parser.add_argument('--reasoning-effort', choices=['minimal', 'low', 'medium', 'high', 'xhigh'], default='high')
parser.add_argument('--task', choices=['sorting', 'repo-tests'], default='sorting')
parser.add_argument('--timeout', type=int, default=1800)
parser.add_argument('--binary', type=pathlib.Path, default=ROOT / 'target/debug/hey-proxy')
args = parser.parse_args()
if not args.mock and not args.config:
    parser.error('provide --mock or --config')
if args.mock and args.task != 'sorting':
    parser.error('the deterministic mock supports only the sorting task')
if args.timeout <= 0:
    parser.error('--timeout must be positive')
stamp = time.strftime('%Y%m%d-%H%M%S')
run = ROOT / 'output/gemini-validation' / (('repo-tests-' if args.task == 'repo-tests' else '') + ('mock-' if args.mock else 'vertex-adc-' if args.adc else 'vertex-') + stamp)
run.mkdir(parents=True, mode=0o700)
scratch = run / 'workspace'
scratch.mkdir()
traces = []
regression_file = ROOT / 'tests/gemini_regression.rs'
before_tests = regression_file.read_text() if regression_file.exists() else ''
if args.task == 'repo-tests':
    working_directory = ROOT
    prompt = '''Inspect this Rust hey-proxy repository and add at least eight meaningful new regression tests in tests/gemini_regression.rs for the Gemini/Responses transformation library. Read src/gemini/ and the existing tests first; focus on behavior the existing suite does not cover, especially composed multi-turn signed replay, duplicate tool leaf names in namespaces, custom tools, interleaved parallel partial calls, nontext parts, stream failure/state transitions, tampered projections and late usage metadata. Work in small batches: write and run the first two tests before designing the rest. Save progress promptly; do not try to design the entire suite in one response. Choose concrete edge cases based on the implementation. Use only public library APIs and deterministic synthetic data. Include realistic multi-step scenarios; do not just mirror implementation details. Run CARGO_INCREMENTAL=0 cargo test --test gemini_regression --test gemini_conversion --test credential_sources and cargo fmt --all -- --check. You may run cargo fmt to format your new file. If you uncover a genuine bug, leave a precise failing regression test and explain it; do not weaken assertions or encode a bug as expected behavior. Only modify tests/gemini_regression.rs. Do not modify implementation, manifests, existing tests or docs. Do not read user configuration, credentials, output recordings, hidden directories or files outside this repository. Do not operate services, use the network, create Git metadata or use other agents. Report each scenario and actual test results.'''
else:
    working_directory = scratch
    prompt = 'Create sort_numbers.py containing sort_numbers(values) that returns a sorted copy. Run Python assertions for duplicates, negative numbers, and empty input. Only work in this directory. Do not use other agents. Report what you verified.'

class MockGemini(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        native = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        assert self.headers.get('Authorization') == 'Bearer synthetic-gemini'
        contents = native['contents']
        outputs = [p['functionResponse'] for c in contents for p in c['parts'] if 'functionResponse' in p]
        declarations = native.get('tools', [{}])[0].get('functionDeclarations', [])
        traces.append({'path': self.path, 'roles': [c['role'] for c in contents],
                       'part_types': [[list(p) for p in c['parts']] for c in contents],
                       'declarations': declarations, 'generation_config': native.get('generationConfig'),
                       'tool_output_count': len(outputs)})
        if not outputs:
            command_tool = next(t for t in declarations if 'cmd' in t.get('parametersJsonSchema', {}).get('properties', {}))
            cmd = "python3 - <<'PY'\nfrom pathlib import Path\nPath('sort_numbers.py').write_text('def sort_numbers(values):\\n    return sorted(values)\\n')\nfrom sort_numbers import sort_numbers\nassert sort_numbers([3, -1, 3, 0]) == [-1, 0, 3, 3]\nassert sort_numbers([]) == []\nprint('sorting checks passed')\nPY"
            parts = [{'text': 'I will create and test the sorting function.', 'thought': True, 'thoughtSignature': 'synthetic-thought'},
                     {'functionCall': {'name': command_tool['name'], 'args': {'cmd': cmd}}, 'thoughtSignature': 'synthetic-call'}]
        else:
            call = next(p for c in contents for p in c['parts'] if 'functionCall' in p)
            assert call['thoughtSignature'] == 'synthetic-call', 'Codex replay lost the function-call signature'
            assert any(p.get('thoughtSignature') == 'synthetic-thought' for c in contents for p in c['parts']), 'Codex replay lost reasoning'
            parts = [{'text': 'Created sort_numbers.py and verified duplicates, negative values, and empty input.', 'thoughtSignature': 'synthetic-answer'}]
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.end_headers()
        for part in parts:
            event = {'candidates': [{'index': 0, 'content': {'role': 'model', 'parts': [part]}}]}
            self.wfile.write(('data: ' + json.dumps(event) + '\n\n').encode())
            self.wfile.flush()
        for event in [{'candidates': [{'index': 0, 'finishReason': 'STOP'}]},
                      {'usageMetadata': {'promptTokenCount': 100, 'candidatesTokenCount': 20, 'thoughtsTokenCount': 30, 'totalTokenCount': 150}}]:
            self.wfile.write(('data: ' + json.dumps(event) + '\n\n').encode())
        self.wfile.flush()
    def log_message(self, *_):
        pass

with socket.socket() as sock:
    sock.bind(('127.0.0.1', 0))
    port = sock.getsockname()[1]
server = None
if args.mock:
    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), MockGemini)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    config = {'providers': {'openai': {'api_keys': {'default': 'synthetic-openai'}},
                           'gemini': {'upstream_url': f'http://127.0.0.1:{server.server_port}', 'auth': 'bearer', 'api_key': 'synthetic-gemini'}},
              'aliases': [{'from': 'gemini-test', 'to': 'gemini/gemini-3.1-pro-preview'}]}
else:
    # Private copy only; no credential values appear in stdout or report artifacts.
    config = json.loads(args.config.expanduser().read_text())
    if args.adc:
        config['providers']['gemini']['auth'] = 'adc'
        config['providers']['gemini'].pop('api_key', None)
config['listen'] = f'127.0.0.1:{port}'
config['logging'] = {'enabled': True, 'database': str(run / 'requests.sqlite3')}
private = run / 'private-config.json'
with os.fdopen(os.open(private, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), 'w') as private_file:
    json.dump(config, private_file)
command = ['codex', 'exec', '--ignore-user-config', '--ignore-rules', '--ephemeral', '--skip-git-repo-check',
           '-C', str(working_directory), '-s', 'workspace-write', '-c', 'approval_policy="never"',
           '-c', 'model_provider="gemini-validation"', '-c', f'model="{args.model}"',
           '-c', f'model_reasoning_effort="{args.reasoning_effort}"', '-c', 'model_reasoning_summary="auto"',
           '-c', 'features.enable_request_compression=false', '-c', 'features.multi_agent=false', '-c', 'web_search="disabled"',
           '-c', 'model_providers.gemini-validation.name="Gemini validation"',
           '-c', f'model_providers.gemini-validation.base_url="http://127.0.0.1:{port}/v1"',
           '-c', 'model_providers.gemini-validation.wire_api="responses"',
           '-c', 'model_providers.gemini-validation.requires_openai_auth=false',
           '-c', 'model_providers.gemini-validation.supports_websockets=false',
           '-c', 'model_providers.gemini-validation.request_max_retries=0',
           '-c', 'model_providers.gemini-validation.stream_max_retries=0',
           '--json', '-o', str(run / 'final.txt'),
           prompt]
(run / 'command.json').write_text(json.dumps(command, indent=2))
with (run / 'proxy.log').open('w') as proxy_log:
    proxy = subprocess.Popen([str(args.binary.resolve()), '--config', str(private)], stdout=proxy_log, stderr=proxy_log)
    try:
        for _ in range(100):
            if proxy.poll() is not None:
                raise RuntimeError('Preview proxy failed to start; inspect its log')
            try:
                urllib.request.urlopen(f'http://127.0.0.1:{port}/logs/api', timeout=1)
                break
            except OSError:
                time.sleep(.05)
        with (run / 'codex.jsonl').open('w') as out, (run / 'codex.stderr').open('w') as err:
            try:
                result = subprocess.run(command, stdout=out, stderr=err, timeout=args.timeout,
                                        env=dict(os.environ, CARGO_INCREMENTAL='0'))
                timed_out = False
            except subprocess.TimeoutExpired:
                result = subprocess.CompletedProcess(command, 124)
                timed_out = True
        logs = json.load(urllib.request.urlopen(f'http://127.0.0.1:{port}/logs/api', timeout=5))
        (run / 'proxy-metadata.json').write_text(json.dumps(logs, indent=2))
        success = False
        added_test_count = 0
        if args.task == 'repo-tests' and regression_file.exists():
            after_tests = regression_file.read_text()
            (run / 'gemini_regression.rs').write_text(after_tests)
            (run / 'tests.diff').write_text(''.join(difflib.unified_diff(before_tests.splitlines(keepends=True), after_tests.splitlines(keepends=True), fromfile='before/tests/gemini_regression.rs', tofile='after/tests/gemini_regression.rs')))
            added_test_count = after_tests.count('#[test]') - before_tests.count('#[test]')
            check = subprocess.run(['cargo', 'test', '--test', 'gemini_regression', '--test', 'gemini_conversion', '--test', 'credential_sources'], cwd=ROOT, capture_output=True, text=True, timeout=240,
                                   env=dict(os.environ, CARGO_INCREMENTAL='0'))
            (run / 'verification.txt').write_text(check.stdout + check.stderr)
            success = check.returncode == 0 and added_test_count >= 8
        elif args.task == 'sorting' and (scratch / 'sort_numbers.py').exists():
            check = subprocess.run(['python3', '-c', 'from sort_numbers import sort_numbers; a=[3,-1,3,0]; assert sort_numbers(a)==[-1,0,3,3]; assert a==[3,-1,3,0]; assert sort_numbers([])==[]; print("checks passed")'], cwd=scratch, capture_output=True, text=True)
            (run / 'verification.txt').write_text(check.stdout + check.stderr)
            success = check.returncode == 0
        recorded_events = [json.loads(line) for line in (run / 'codex.jsonl').read_text().splitlines() if line.strip()]
        command_count = sum(event.get('type') == 'item.completed' and event.get('item', {}).get('type') == 'command_execution' for event in recorded_events)
        summary = {'codex_exit_code': result.returncode, 'timed_out':timed_out, 'task_verified': success, 'native_mock': args.mock,
                   'task':args.task, 'added_test_count':added_test_count,
                   'reasoning_effort':args.reasoning_effort,
                   'completed_command_count':command_count,
                   'model_override': args.model, 'native_request_count': len(traces) if args.mock else sum(r['attempts'] for r in logs['entries']),
                   'auth_mode':config['providers']['gemini']['auth'],
                   'codex_version': subprocess.check_output(['codex', '--version'], text=True).strip()}
        (run / 'result.json').write_text(json.dumps(summary, indent=2))
        print(json.dumps({'recording': str(run), **summary}))
    finally:
        proxy.terminate()
        try:
            proxy.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proxy.kill()
            proxy.wait()
        # Delete only the private credential copy and its private codec key.
        private.unlink(missing_ok=True)
        private.with_suffix('.gemini-reasoning-key').unlink(missing_ok=True)
        if server:
            server.shutdown()
        (run / 'native-mock-trace.json').write_text(json.dumps(traces, indent=2))

raise SystemExit(0 if result.returncode == 0 and success else 1)
