"""Manual, billable checks for strict Gemini JSON output and signed tool replay."""
import argparse
import json
from pathlib import Path
import time
import urllib.request


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--base-url', default='http://127.0.0.1:8080/v1')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    schema = {'type': 'object', 'properties': {'answer': {'type': 'integer', 'minimum': 0, 'maximum': 100},
              'status': {'enum': ['ok']}}, 'required': ['answer', 'status'], 'additionalProperties': False}
    records = []
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

    def call(history, *, stream, probe=False):
        payload = {'model': 'gemini/gemini-3.1-pro-preview', 'input': history, 'stream': stream,
                   'reasoning': {'effort': 'low', 'summary': 'auto'}, 'max_output_tokens': 1024,
                   'text': {'format': {'type': 'json_schema', 'strict': True, 'name': 'answer', 'schema': schema}}}
        if probe:
            payload['tools'] = [{'type': 'function', 'name': 'get_answer', 'parameters': {'type': 'object', 'properties': {}}}]
            payload['tool_choice'] = {'type': 'function', 'name': 'get_answer'}
        request = urllib.request.Request(args.base_url.rstrip('/') + '/responses',
            data=json.dumps(payload).encode(), headers={'Content-Type': 'application/json'})
        start = time.monotonic()
        with opener.open(request, timeout=180) as response:
            if stream:
                events = []
                for line in response:
                    if line.startswith(b'data:') and line[5:].strip() != b'[DONE]':
                        events.append(json.loads(line[5:]))
                assert events[-1]['type'] == 'response.completed'
                assert [e['sequence_number'] for e in events] == list(range(len(events)))
                result = events[-1]['response']
            else:
                result = json.load(response)
        assert result['status'] == 'completed'
        assert any(i.get('encrypted_content') for i in result['output'] if i['type'] == 'reasoning')
        if probe:
            assert any(i['type'] == 'function_call' for i in result['output'])
        else:
            text = ''.join(p.get('text', '') for i in result['output'] if i['type'] == 'message' for p in i['content'])
            assert json.loads(text) == {'answer': 42, 'status': 'ok'}
        records.append({'stream': stream, 'tool_call': probe, 'http_status': response.status,
                        'status': result['status'], 'seconds': round(time.monotonic()-start, 2)})
        print(json.dumps(records[-1]), flush=True)
        return result['output']

    for stream in [False, True]:
        call('Return answer 42 and status ok.', stream=stream)
    history = [{'role': 'user', 'content': 'Call get_answer, then return its value as answer with status ok.'}]
    output = call(history, stream=True, probe=True)
    history.extend(output)
    for item in output:
        if item['type'] == 'function_call':
            history.append({'type': 'function_call_output', 'call_id': item['call_id'], 'output': '42'})
    call(history, stream=True)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps({'status': 'passed', 'checks': records}, indent=2)+'\n')


if __name__ == '__main__':
    main()
