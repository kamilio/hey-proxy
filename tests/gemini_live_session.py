"""Manual live replay: retains signed history in memory; saves only safe statistics.

Uses the configured proxy, with billable Gemini requests. Never resumes or edits
the source Codex session and never executes model-generated tool calls.
"""
import argparse
from collections import Counter
import json
from pathlib import Path
import time
import urllib.request


def generate(base_url, history, *, probe=False):
    body = {
        'model': 'gemini/gemini-3.1-pro-preview', 'input': history, 'stream': True,
        'reasoning': {'effort': 'low', 'summary': 'auto'},
        'max_output_tokens': 2048, 'tools': [], 'tool_choice': 'none',
    }
    if probe:
        body['tools'] = [{'type': 'function', 'name': 'diagnostic_probe',
                         'description': 'An inert diagnostic; no external actions.',
                         'parameters': {'type': 'object', 'properties': {}}}]
        body['tool_choice'] = {'type': 'function', 'name': 'diagnostic_probe'}
    request = urllib.request.Request(base_url.rstrip('/') + '/responses',
        data=json.dumps(body).encode(), headers={'Content-Type': 'application/json'})
    events = Counter()
    completed = None
    start = time.monotonic()
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    with opener.open(request, timeout=240) as response:
        for line in response:
            if not line.startswith(b'data:'):
                continue
            payload = line[5:].strip()
            if payload == b'[DONE]':
                continue
            event = json.loads(payload)
            kind = event.get('type', '')
            events[kind] += 1
            assert kind not in ('error', 'response.failed', 'response.incomplete'), kind
            if kind == 'response.completed':
                completed = event['response']
    assert completed and completed['status'] == 'completed', 'Missing completed response'
    assert events['response.completed'] == 1
    output = completed['output']
    assert any(i.get('encrypted_content') for i in output if i['type'] == 'reasoning')
    return output, {'http_status': response.status, 'status': completed['status'],
                    'seconds': round(time.monotonic() - start, 2),
                    'events': dict(events), 'output_types': [i['type'] for i in output]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--session', type=Path)
    parser.add_argument('--base-url', default='http://127.0.0.1:8080/v1')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    history = []
    if args.session:
        for line in args.session.open():
            row = json.loads(line)
            if row.get('type') == 'response_item':
                history.append(row['payload'])
            elif row.get('type') == 'compacted':
                replacement = row['payload'].get('replacement_history')
                if replacement is None:
                    raise ValueError('Session compaction has no replayable replacement history')
                # Match Codex resume: superseded windows must not be appended to
                # current context (that can exceed Gemini's million-token cap).
                history = list(replacement)
    result = {'history_items': len(history), 'rounds': []}
    history.append({'role': 'user', 'content':
        'Diagnostic check only: reply exactly OK. Do not continue the task or call tools.'})
    output, stats = generate(args.base_url, history)
    result['rounds'].append({'case': 'full_history_stream', **stats})
    history.extend(output)
    history.append({'role': 'user', 'content':
        'Diagnostic only: call diagnostic_probe once with an empty object. Do no other work.'})
    output, stats = generate(args.base_url, history, probe=True)
    calls = [i for i in output if i['type'] == 'function_call']
    assert calls and all(i['name'] == 'diagnostic_probe' for i in calls)
    history.extend(output)
    for call in calls:
        history.append({'type': 'function_call_output', 'call_id': call['call_id'],
                        'output': 'Diagnostic succeeded; no external action performed.'})
    result['rounds'].append({'case': 'signed_tool_call', **stats})
    history.append({'role': 'developer', 'content':
        'Diagnostic complete. From now on, reply exactly LATE_INSTRUCTION_OK. Do not call tools.'})
    history.append({'role': 'user', 'content': 'Finish the diagnostic only.'})
    output, stats = generate(args.base_url, history)
    text = ''.join(p.get('text', '') for i in output if i['type'] == 'message'
                   for p in i.get('content', [])).strip()
    assert text == 'LATE_INSTRUCTION_OK', 'Late instruction was not followed'
    result['rounds'].append({'case': 'retired_tool_and_late_instruction', **stats})
    for initial_role, late_role, expected in [
        ('system', 'developer', 'INITIAL'),
        ('developer', 'developer', 'UPDATED'),
        ('developer', 'system', 'UPDATED'),
    ]:
        instructions = [
            {'role': initial_role, 'content': 'Reply exactly INITIAL.'},
            {'role': 'user', 'content': 'Begin.'},
            {'role': 'assistant', 'content': 'INITIAL'},
            {'role': late_role, 'content': 'From now on reply exactly UPDATED.'},
            {'role': 'user', 'content': 'Reply according to the applicable instruction.'},
        ]
        output, stats = generate(args.base_url, instructions)
        text = ''.join(p.get('text', '') for i in output if i['type'] == 'message'
                       for p in i.get('content', [])).strip()
        assert text == expected, f'Instruction priority failed: {initial_role}/{late_role}'
        result['rounds'].append({'case': f'priority_{initial_role}_then_{late_role}', **stats})
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2) + '\n')
    args.output.chmod(0o600)
    print(json.dumps(result), flush=True)


if __name__ == '__main__':
    main()
