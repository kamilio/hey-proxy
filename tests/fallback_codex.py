"""Record real Codex recovering through model-primary -> model-secondary.

Synthetic upstreams/credentials and an isolated proxy. No existing service or
Codex config is changed. Run with --binary target/debug/hey-proxy.
"""
import argparse
import http.server
import json
import socket
import subprocess
import threading
import time
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def tool_with_cmd(tools, namespace=None):
    for tool in tools:
        if tool.get('type') == 'namespace':
            found = tool_with_cmd(tool['tools'], tool['name'])
            if found:
                return found
        elif 'cmd' in tool.get('parameters', {}).get('properties', {}):
            return tool['name'], namespace
    return None


def run(binary, output, scenarios):
    output.mkdir(parents=True, exist_ok=True)
    results = []
    for scenario in scenarios:
        stage = output / scenario
        workspace = stage / 'workspace'
        workspace.mkdir(parents=True, exist_ok=True)
        marker = workspace / 'executions.txt'
        marker.unlink(missing_ok=True)
        traces, errors = [], []

        class Upstream(http.server.BaseHTTPRequestHandler):
            protocol_version = 'HTTP/1.1'

            def log_message(self, *_):
                pass

            def do_POST(self):
                try:
                    self.respond()
                except (BrokenPipeError, ConnectionResetError):
                    pass
                except Exception as error:
                    errors.append(str(error))
                    self.close_connection = True

            def respond(self):
                body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
                (stage / 'request-shape.json').write_text(json.dumps({'path': self.path, 'keys': list(body), 'tools': body.get('tools', [])}, indent=2))
                model = body['model']
                history = body.get('input', [])
                calls = [item for item in history if item.get('type') == 'function_call']
                outputs = [item for item in history if item.get('type') == 'function_call_output']
                traces.append({'model': model, 'calls': len(calls), 'outputs': len(outputs)})
                assert self.headers['Authorization'] == ('Bearer primary-synthetic' if model == 'model-primary' else 'Bearer fallback-synthetic')
                if model == 'model-primary' and scenario == 'upstream_504':
                    time.sleep(.5)
                    data = b'{"error":{"code":"server_error","message":"Synthetic upstream failure"}}'
                    self.send_response(504)
                    self.send_header('Content-Type', 'application/json')
                    self.send_header('Content-Length', str(len(data)))
                    self.end_headers()
                    self.wfile.write(data)
                    return
                if model == 'model-primary' and scenario == 'http_503':
                    data = json.dumps({'error': {'code': 'server_error', 'message': 'Synthetic unavailable primary'}}).encode()
                    self.send_response(503)
                    self.send_header('Content-Type', 'application/json')
                    self.send_header('Content-Length', str(len(data)))
                    self.end_headers()
                    self.wfile.write(data)
                    return
                self.send_response(200)
                self.send_header('Content-Type', 'text/event-stream')
                self.send_header('Transfer-Encoding', 'chunked')
                self.end_headers()
                sequence = 0

                def event(kind, **fields):
                    nonlocal sequence
                    data = ('event: ' + kind + '\ndata: ' + json.dumps(dict(type=kind, sequence_number=sequence, **fields)) + '\n\n').encode()
                    sequence += 1
                    self.wfile.write(f'{len(data):x}\r\n'.encode() + data + b'\r\n')
                    self.wfile.flush()

                def finish():
                    self.wfile.write(b'0\r\n\r\n')
                    self.wfile.flush()

                if model == 'model-primary':
                    assert scenario == 'sse_refusal'
                    event('response.created', response={'id': 'rejected-primary-id', 'output': [], 'status': 'in_progress'})
                    event('response.failed', response={'id': 'rejected-primary-id', 'output': [], 'error': {'code': 'server_error', 'message': 'Synthetic overload'}})
                    finish()
                    return
                assert model == 'model-secondary'
                response_id = 'response-tool' if not outputs else 'response-final'
                event('response.created', response={'id': response_id, 'model': model, 'status': 'in_progress', 'output': []})
                if not outputs:
                    assert not calls, 'Rejected attempt leaked tool history'
                    name, namespace = tool_with_cmd(body['tools'])
                    command = "python3 -c \"from pathlib import Path; p=Path('executions.txt'); p.write_text((p.read_text() if p.exists() else '')+'executed\\n')\""
                    reasoning = {'type': 'reasoning', 'id': 'rs-synthetic', 'summary': [{'type': 'summary_text', 'text': 'Check the workspace.'}], 'encrypted_content': 'synthetic-signed-reasoning'}
                    call = {'type': 'function_call', 'id': 'fc-synthetic', 'call_id': 'call-synthetic', 'name': name,
                            'arguments': json.dumps({'cmd': command}), 'status': 'completed'}
                    if namespace:
                        call['namespace'] = namespace
                    items = [reasoning, call]
                    for index, item in enumerate(items):
                        event('response.output_item.added', output_index=index, item=item)
                        event('response.output_item.done', output_index=index, item=item)
                else:
                    assert len(outputs) == 1 and len(calls) == 1, 'Tool executed or replayed twice'
                    assert any(item.get('encrypted_content') == 'synthetic-signed-reasoning' for item in history), 'Signed reasoning was lost'
                    message = {'type': 'message', 'id': 'msg-synthetic', 'role': 'assistant', 'status': 'completed',
                               'content': [{'type': 'output_text', 'text': 'FALLBACK_OK', 'annotations': []}]}
                    items = [message]
                    event('response.output_item.added', output_index=0, item=dict(message, content=[]))
                    event('response.content_part.added', output_index=0, content_index=0, item_id=message['id'], part={'type': 'output_text', 'text': '', 'annotations': []})
                    event('response.output_text.delta', output_index=0, content_index=0, item_id=message['id'], delta='FALLBACK_OK')
                    event('response.output_item.done', output_index=0, item=message)
                event('response.completed', response={'id': response_id, 'model': model, 'status': 'completed', 'output': items,
                      'usage': {'input_tokens': 100, 'output_tokens': 10, 'total_tokens': 110}})
                finish()

        server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Upstream)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0))
            port = sock.getsockname()[1]
        config = {'listen': f'127.0.0.1:{port}', 'providers': {'openai': {
            'upstream_url': f'http://127.0.0.1:{server.server_port}',
            'api_keys': {'default': 'primary-synthetic', 'fallback': 'fallback-synthetic'}}},
            'aliases': [{'from': 'fallback-test-alias', 'to': 'model-primary'}, {'from': 'model-secondary', 'api_key': 'fallback'}],
            'fallbacks': {'model-primary': ['model-secondary'], 'fallback-test-alias': ['must-not-run']},
            'logging': {'database': str(stage / 'requests.sqlite3')}}
        config_path = stage / 'config.json'
        config_path.write_text(json.dumps(config))
        config_path.chmod(0o600)
        command = ['codex', 'exec', '--ignore-user-config', '--ignore-rules', '--ephemeral', '--skip-git-repo-check',
                   '-C', str(workspace), '-s', 'workspace-write', '-c', 'approval_policy="never"',
                   '-c', 'model="fallback-test-alias"', '-c', 'model_provider="fallback_test"',
                   '-c', 'features.enable_request_compression=false', '-c', 'features.multi_agent=false', '-c', 'web_search="disabled"',
                   '-c', 'model_providers.fallback_test.name="Fallback Test"',
                   '-c', f'model_providers.fallback_test.base_url="http://127.0.0.1:{port}/v1"',
                   '-c', 'model_providers.fallback_test.wire_api="responses"',
                   '-c', 'model_providers.fallback_test.requires_openai_auth=false',
                   '-c', 'model_providers.fallback_test.supports_websockets=true',
                   '-c', 'model_providers.fallback_test.request_max_retries=0',
                   '-c', 'model_providers.fallback_test.stream_max_retries=0',
                   '--json', '-o', str(stage / 'final.txt'),
                   'Use Python once to append executed followed by a newline to executions.txt, then report FALLBACK_OK. Work only here. Do not use other agents.']
        (stage / 'command.json').write_text(json.dumps(command, indent=2))
        with (stage / 'proxy.log').open('w') as log:
            proxy = subprocess.Popen([str(binary), '--config', str(config_path)], stdin=subprocess.DEVNULL, stdout=log, stderr=log)
            try:
                for _ in range(100):
                    assert proxy.poll() is None
                    try:
                        urllib.request.urlopen(f'http://127.0.0.1:{port}/logs/api', timeout=1).close()
                        break
                    except OSError:
                        time.sleep(.05)
                with (stage / 'codex.jsonl').open('w') as stdout, (stage / 'codex.stderr').open('w') as stderr:
                    result = subprocess.run(command, stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr, timeout=90)
                assert not errors, errors
                assert result.returncode == 0, f'Codex failed; see {stage}'
                assert marker.read_text() == 'executed\n', 'Tool must execute exactly once'
                assert 'FALLBACK_OK' in (stage / 'final.txt').read_text()
                assert [t['model'] for t in traces] == ['model-primary', 'model-secondary'] * 2, traces
                assert 'rejected-primary-id' not in (stage / 'codex.jsonl').read_text()
                with urllib.request.urlopen(f'http://127.0.0.1:{port}/logs/api?local=true', timeout=5) as response:
                    logs = json.load(response)['entries']
                assert any(e.get('status') == 426 for e in logs), 'Codex did not exercise WebSocket downgrade'
                requests = [e for e in logs if e.get('method') == 'POST']
                assert len(requests) == 2 and all(e.get('requested_model') == 'fallback-test-alias' and e.get('routed_model') == 'model-secondary' and e.get('state') == 'succeeded' for e in requests)
                record = {'scenario': scenario, 'status': 'passed', 'requests': len(traces), 'tool_executions': 1,
                          'signed_reasoning_preserved': True, 'post_rewrite_fallback': True, 'websocket_downgraded': True, 'client_retries': 0}
                results.append(record)
                (stage / 'trace.json').write_text(json.dumps(traces, indent=2))
                print(json.dumps(record), flush=True)
            finally:
                proxy.terminate()
                try:
                    proxy.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    proxy.kill()
                    proxy.wait()
                server.shutdown()
                server.server_close()
    (output / 'verification.json').write_text(json.dumps({'status': 'passed', 'cases': results}, indent=2))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=ROOT / 'target/debug/hey-proxy')
    parser.add_argument('--output', type=Path, default=ROOT / 'output/fallback-validation/codex')
    parser.add_argument('--scenarios', nargs='+', choices=['http_503', 'upstream_504', 'sse_refusal'], default=['http_503', 'upstream_504', 'sse_refusal'])
    args = parser.parse_args()
    run(args.binary.resolve(), args.output.resolve(), args.scenarios)
