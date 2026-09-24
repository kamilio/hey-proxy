"""Fault-inject native Gemini streams and verify recovery using the actual Codex CLI.

No real provider credentials or running services are touched. Records synthetic
requests, Codex events and assertions; partial tool calls must never execute.
"""
import argparse
import contextlib
import http.client
import http.server
import json
import os
import re
import shlex
from pathlib import Path
import socket
import subprocess
import threading
import time
import urllib.request

ROOT = Path(__file__).resolve().parents[1]


def run(binary, output, scenarios):
    output.mkdir(parents=True, exist_ok=True)
    results = []
    for scenario in scenarios:
        client_disconnect = scenario.startswith('client_disconnect')
        terminal_expected = scenario in ('permanent', 'client_disconnect_no_retries')
        stage = output / scenario
        stage.mkdir(exist_ok=True)
        workspace = stage / 'workspace'
        workspace.mkdir(exist_ok=True)
        marker = workspace / 'executions.txt'
        marker.unlink(missing_ok=True)
        traces = []
        errors = []

        class Gemini(http.server.BaseHTTPRequestHandler):
            protocol_version = 'HTTP/1.1'

            def do_POST(self):
                try:
                    self.reply()
                except (BrokenPipeError, ConnectionResetError):
                    pass
                except Exception as e:
                    errors.append(str(e))
                    self.close_connection = True

            def reply(self):
                body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
                assert self.headers.get('Authorization') == 'Bearer synthetic'
                contents = body['contents']
                parts = [p for c in contents for p in c['parts']]
                outputs = [p for p in parts if 'functionResponse' in p]
                calls = [p for p in parts if 'functionCall' in p]
                traces.append({'number': len(traces)+1, 'function_outputs': len(outputs),
                               'function_calls': len(calls), 'roles': [c['role'] for c in contents]})
                first = len(traces) == 1
                declarations = body.get('tools', [{}])[0].get('functionDeclarations', [])
                tool = next(t for t in declarations if 'cmd' in t.get('parametersJsonSchema', {}).get('properties', {}))
                command = "python3 -c \"from pathlib import Path; p=Path('executions.txt'); p.write_text((p.read_text() if p.exists() else '')+'executed\\n')\""
                call = {'functionCall': {'name': tool['name'], 'args': {'cmd': command}}, 'thoughtSignature': 'synthetic-call'}
                thought = {'text': 'Check the workspace.', 'thought': True, 'thoughtSignature': 'synthetic-thought'}
                self.send_response(200)
                self.send_header('Content-Type', 'text/event-stream')
                self.send_header('Transfer-Encoding', 'chunked')
                self.end_headers()

                def raw(data):
                    self.wfile.write(f'{len(data):x}\r\n'.encode()+data+b'\r\n')
                    self.wfile.flush()

                def event(value):
                    raw(('data: '+json.dumps(value)+'\n\n').encode())

                def content(value):
                    event({'candidates': [{'index': 0, 'content': {'role': 'model', 'parts': [value]}}]})

                def finish():
                    self.wfile.write(b'0\r\n\r\n')
                    self.wfile.flush()

                if first and scenario != 'credential_refresh' and not client_disconnect:
                    raw(b': accepted\n\n')
                    if scenario in ('disconnect_reasoning', 'disconnect_text', 'disconnect_tool', 'truncated', 'malformed'):
                        content(thought)
                    if scenario == 'disconnect_text':
                        content({'text': 'Partial answer that must not enter retry history.'})
                    if scenario in ('disconnect_tool', 'truncated', 'malformed'):
                        content(call)
                    if scenario.startswith('disconnect'):
                        time.sleep(.05)
                        self.close_connection = True  # No final HTTP chunk: transport failure.
                        self.connection.shutdown(socket.SHUT_RDWR)
                        return
                    if scenario == 'truncated':
                        finish()  # Legal HTTP EOF but missing Gemini finishReason.
                        return
                    if scenario == 'malformed':
                        raw(b'data: {broken-json}\n\n')
                        finish()
                        return
                    status, code = ('RESOURCE_EXHAUSTED', 429) if scenario == 'rate_limit' else ('INVALID_ARGUMENT', 400) if scenario == 'permanent' else ('UNAVAILABLE', 503)
                    event({'error': {'status': status, 'code': code, 'message': 'Synthetic failure',
                                    'details': [{'@type': 'type.googleapis.com/google.rpc.RetryInfo', 'retryDelay': '0.05s'}]}})
                    finish()
                    return
                if not outputs:
                    assert not calls, 'Failed attempt was committed to retry history'
                    assert not any(p.get('text', '').startswith('Partial answer') for p in parts)
                    content(thought)
                    content(call)
                else:
                    assert len(outputs) == 1 and len(calls) == 1
                    assert calls[0]['thoughtSignature'] == 'synthetic-call'
                    assert any(p.get('thoughtSignature') == 'synthetic-thought' for p in parts)
                    content({'text': 'RECOVERY_OK', 'thoughtSignature': 'synthetic-final'})
                event({'candidates': [{'index': 0, 'finishReason': 'STOP'}]})
                event({'usageMetadata': {'promptTokenCount': 100, 'candidatesTokenCount': 10, 'thoughtsTokenCount': 5}})
                finish()

            def log_message(self, *_):
                pass

        server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Gemini)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0))
            port = sock.getsockname()[1]
        relay = None
        relay_requests = []
        if client_disconnect:
            class Relay(http.server.BaseHTTPRequestHandler):
                protocol_version = 'HTTP/1.1'

                def do_POST(self):
                    try:
                        request = self.rfile.read(int(self.headers['Content-Length']))
                        with contextlib.closing(http.client.HTTPConnection('127.0.0.1', port, timeout=20)) as connection:
                            connection.request('POST', self.path, body=request,
                                               headers={'Content-Type': 'application/json'})
                            response = connection.getresponse()
                            payload = response.read()
                            assert response.status == 200
                        first = not relay_requests
                        relay_requests.append({'number': len(relay_requests)+1,
                                               'upstream_completed': b'event: response.completed' in payload,
                                               'client_disconnected': first})
                        self.send_response(200)
                        self.send_header('Content-Type', 'text/event-stream')
                        self.send_header('Transfer-Encoding', 'chunked')
                        self.end_headers()
                        if first:
                            # A tunnel/proxy cut can lose the client stream even
                            # when upstream logged success. Deliver reasoning,
                            # then sever HTTP framing before any tool is committed.
                            frames = payload.split(b'\n\n')
                            stop = next(i for i, frame in enumerate(frames)
                                        if b'event: response.reasoning_summary_text.delta' in frame)
                            payload = b'\n\n'.join(frames[:stop+1])+b'\n\n'
                        self.wfile.write(f'{len(payload):x}\r\n'.encode()+payload+b'\r\n')
                        self.wfile.flush()
                        if first:
                            self.close_connection = True
                            self.connection.shutdown(socket.SHUT_RDWR)
                        else:
                            self.wfile.write(b'0\r\n\r\n')
                            self.wfile.flush()
                    except (BrokenPipeError, ConnectionResetError):
                        pass
                    except Exception as error:
                        errors.append(str(error))
                        self.close_connection = True

                def log_message(self, *_):
                    pass

            relay = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Relay)
            threading.Thread(target=relay.serve_forever, daemon=True).start()
        client_port = relay.server_port if relay else port
        config = {'listen': f'127.0.0.1:{port}', 'providers': {
            'openai': {'api_keys': {'default': 'synthetic'}},
            'gemini': {'upstream_url': f'http://127.0.0.1:{server.server_port}', 'auth': 'bearer', 'api_key': 'synthetic'}},
            'logging': {'enabled': True, 'database': str(stage/'requests.sqlite3')}}
        if scenario == 'credential_refresh':
            ready, calls = stage/'credential-ready', stage/'credential-calls'
            ready.unlink(missing_ok=True)
            calls.unlink(missing_ok=True)
            ready_path, calls_path = shlex.quote(str(ready)), shlex.quote(str(calls))
            config['providers']['gemini']['api_key'] = (
                f'sh://printf x >> {calls_path}; if test -f {ready_path}; then printf synthetic; '
                f'else touch {ready_path}; exit 1; fi')
        path = stage/'config.json'
        path.write_text(json.dumps(config))
        path.chmod(0o600)
        profile_home = stage/'codex-config'
        profile_home.mkdir(exist_ok=True)
        subprocess.run([str(binary), 'configure-gemini', '--base-url', f'http://127.0.0.1:{client_port}/v1',
                        '--model', 'gemini/test', '--codex-home', str(profile_home)],
                       check=True, stdout=subprocess.DEVNULL, stdin=subprocess.DEVNULL)
        # Only inspect generated integer settings, so this harness also runs on
        # remote system Python 3.9 without installing a TOML dependency.
        profile = (profile_home/'gemini.config.toml').read_text()
        block = profile.split('[model_providers.hey_proxy_gemini]', 1)[1].split('\n[', 1)[0]
        provider = {key: int(re.search(rf'(?m)^{key}\s*=\s*(\d+)', block)[1])
                    for key in ('request_max_retries', 'stream_max_retries')}
        assert provider['request_max_retries'] == 4 and provider['stream_max_retries'] == 5
        stream_retries = 0 if scenario == 'client_disconnect_no_retries' else provider['stream_max_retries']
        command = ['codex', 'exec', '--ignore-user-config', '--ignore-rules', '--ephemeral', '--skip-git-repo-check',
                   '-C', str(workspace), '-s', 'workspace-write', '-c', 'approval_policy="never"',
                   '-c', 'model_provider="recovery"', '-c', 'model="gemini/test"',
                   '-c', 'features.enable_request_compression=false', '-c', 'features.multi_agent=false', '-c', 'web_search="disabled"',
                   '-c', 'model_providers.recovery.name="Recovery"',
                   '-c', f'model_providers.recovery.base_url="http://127.0.0.1:{client_port}/v1"',
                   '-c', 'model_providers.recovery.wire_api="responses"',
                   '-c', 'model_providers.recovery.requires_openai_auth=false',
                   '-c', 'model_providers.recovery.supports_websockets=false',
                   '-c', f'model_providers.recovery.request_max_retries={provider["request_max_retries"]}',
                   '-c', f'model_providers.recovery.stream_max_retries={stream_retries}',
                   '--json', '-o', str(stage/'final.txt'),
                   'Write executed followed by a newline into executions.txt using Python once, then report RECOVERY_OK. Work only here. Do not use other agents.']
        (stage/'command.json').write_text(json.dumps(command, indent=2))
        with (stage/'proxy.log').open('w') as log:
            proxy = subprocess.Popen([str(binary), '--config', str(path)], stdin=subprocess.DEVNULL, stdout=log, stderr=log)
            try:
                for _ in range(100):
                    assert proxy.poll() is None
                    try:
                        with urllib.request.urlopen(f'http://127.0.0.1:{port}/logs/api', timeout=1):
                            break
                    except OSError:
                        time.sleep(.05)
                with (stage/'codex.jsonl').open('w') as stdout, (stage/'codex.stderr').open('w') as stderr:
                    result = subprocess.run(command, stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr, timeout=120)
                assert not errors, errors
                if terminal_expected:
                    assert result.returncode != 0 and len(traces) == 1 and not marker.exists()
                else:
                    assert result.returncode == 0, f'{scenario}: Codex exit {result.returncode}; see recording'
                    assert len(traces) == (2 if scenario == 'credential_refresh' else 3), traces
                    assert marker.read_text() == 'executed\n', 'Tool executed more than once'
                    assert 'RECOVERY_OK' in (stage/'final.txt').read_text()
                    if scenario == 'credential_refresh':
                        assert calls.read_text() == 'xx', 'Credential result was not cached after recovery'
                        with urllib.request.urlopen(f'http://127.0.0.1:{port}/logs/api', timeout=5) as response:
                            logs = json.load(response)['entries']
                        assert any(e.get('status') == 502 and e.get('error_code') == 'gemini_credential_error'
                                   for e in logs), 'Credential failure was not classified in proxy logs'
                    else:
                        assert 'Reconnecting' in (stage/'codex.jsonl').read_text(), 'Codex did not record a retry'
                if client_disconnect:
                    assert 'error decoding response body' in (stage/'codex.jsonl').read_text()
                    assert len(relay_requests) == (1 if terminal_expected else 3)
                    assert relay_requests[0]['upstream_completed']
                    assert sum(r['client_disconnected'] for r in relay_requests) == 1
                    (stage/'relay-trace.json').write_text(json.dumps(relay_requests, indent=2))
                record = {'scenario': scenario, 'status': 'passed', 'codex_exit': result.returncode,
                          'requests': len(traces), 'tool_executions': 0 if terminal_expected else 1}
                results.append(record)
                (stage/'trace.json').write_text(json.dumps(traces, indent=2))
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
                if relay:
                    relay.shutdown()
                    relay.server_close()
    (output/'verification.json').write_text(json.dumps({'status': 'passed', 'cases': results}, indent=2))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=ROOT/'target/release/hey-proxy')
    parser.add_argument('--output', type=Path, default=ROOT/'output/gemini-validation/recovery')
    parser.add_argument('--scenarios', nargs='+', default=['unavailable', 'rate_limit', 'disconnect_before', 'disconnect_reasoning', 'disconnect_text', 'disconnect_tool', 'client_disconnect', 'client_disconnect_no_retries', 'truncated', 'malformed', 'credential_refresh', 'permanent'])
    args = parser.parse_args()
    run(args.binary.resolve(), args.output.resolve(), args.scenarios)
