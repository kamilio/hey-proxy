"""Manual, billable Gemini image-detail and signed tool-image replay checks."""
import argparse
import base64
import json
from pathlib import Path
import struct
import time
import urllib.request
import zlib


def blue_png():
    def chunk(kind, data):
        return struct.pack('!I', len(data)) + kind + data + struct.pack('!I', zlib.crc32(kind + data))
    return (b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('!IIBBBBB', 1024, 1024, 8, 2, 0, 0, 0))
            + chunk(b'IDAT', zlib.compress((b'\0' + b'\0\0\xff' * 1024) * 1024)) + chunk(b'IEND', b''))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--base-url', default='http://127.0.0.1:8080/v1')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    raw = blue_png()
    uri = 'data:image/png;base64,' + base64.b64encode(raw).decode()
    records = []
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

    def call(history, case, probe=False):
        body = {'model': 'gemini/gemini-3.1-pro-preview', 'input': history,
                'reasoning': {'effort': 'low', 'summary': 'auto'}, 'max_output_tokens': 1024,
                'tools': [], 'tool_choice': 'none'}
        if probe:
            body['tools'] = [{'type': 'function', 'name': 'screenshot', 'parameters': {'type': 'object', 'properties': {}}}]
            body['tool_choice'] = {'type': 'function', 'name': 'screenshot'}
        request = urllib.request.Request(args.base_url.rstrip('/') + '/responses',
            data=json.dumps(body).encode(), headers={'Content-Type': 'application/json'})
        start = time.monotonic()
        with opener.open(request, timeout=120) as response:
            result = json.load(response)
        assert result['status'] == 'completed'
        assert any(i.get('encrypted_content') for i in result['output'] if i['type'] == 'reasoning')
        if not probe:
            text = ''.join(p.get('text', '') for i in result['output'] if i['type'] == 'message' for p in i['content'])
            assert text.strip().lower().strip('.') == 'blue', 'Incorrect image analysis'
        records.append({'case': case, 'status': response.status, 'seconds': round(time.monotonic()-start, 2), 'usage': result['usage']})
        print(json.dumps(records[-1]), flush=True)
        return result['output']

    for detail in ['auto', 'low', 'high', 'original']:
        call([{'role': 'user', 'content': [{'type': 'input_text', 'text': 'What color is this image? Reply with one word.'},
             {'type': 'input_image', 'image_url': uri, 'detail': detail}]}], 'user_' + detail)
    history = [{'role': 'user', 'content': 'Call screenshot, then identify the color of the returned images. Reply with one word.'}]
    output = call(history, 'signed_tool_call', probe=True)
    calls = [i for i in output if i['type'] == 'function_call']
    assert len(calls) == 1
    history.extend(output)
    for details in [['low'], ['high'], ['original'], ['low', 'original'], ['auto', 'low']]:
        result = {'type': 'function_call_output', 'call_id': calls[0]['call_id'],
                  'output': [{'type': 'input_image', 'image_url': uri, 'detail': d} for d in details]}
        call(history + [result], 'tool_' + '_'.join(details))
    by_case = {r['case']: r['usage']['input_tokens'] for r in records}
    assert by_case['user_low'] < by_case['user_high'] < by_case['user_original']
    assert by_case['tool_low'] < by_case['tool_high'] < by_case['tool_original']
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps({'status': 'passed', 'checks': records}, indent=2) + '\n')
    args.output.with_name('blue.png').write_bytes(raw)


if __name__ == '__main__':
    main()
