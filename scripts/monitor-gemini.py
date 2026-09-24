"""Poll local/remote Gemini failures. Records metadata only, never request bodies."""
import argparse
import concurrent.futures
import fcntl
import inspect
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
import urllib.request
import urllib.parse

HOSTS = ['local', 'kamils-macbook-pro.local', 'devbox']
FIELDS = ['request_id', 'timestamp_ms', 'status', 'error_code', 'state', 'path',
          'attempts', 'routed_model', 'response_bytes', 'total_duration_ms']


def collect_history(read_json, start, end, fields, known):
    """Fixed query bounds and opaque pagination; returned values are metadata only.

    This function is also sent over SSH stdin to run beside each remote proxy.
    It intentionally uses no module helpers or non-stdlib dependencies.
    """
    rows = {}
    for state in ('failed', 'interrupted', 'cancelled'):
        cursor = None
        cursors = set()
        while True:
            # Dashboard reports hide client-mode records to avoid double counting.
            # Monitoring needs them: forwarding can fail before reaching the controller.
            query = {'q': 'gemini', 'state': state, 'limit': 500, 'start': start, 'end': end,
                     'include_clients': 'true'}
            if cursor is not None:
                query['cursor'] = cursor
            page = read_json('http://127.0.0.1:8080/logs/api/history?' + urllib.parse.urlencode(query))
            for entry in page['entries']:
                record = {k: entry.get(k) for k in fields}
                identity = record['request_id']
                if not identity:
                    raise ValueError('History entry has no request ID')
                # A request can transition between states during the scan. Keep
                # one observation; the next scan will see its latest state.
                rows[identity] = record
            cursor = page.get('next_cursor')
            if cursor is None:
                break
            if cursor in cursors:
                raise ValueError('History pagination repeated its cursor')
            cursors.add(cursor)
    for identity, record in rows.items():
        if identity in known or record['error_code']:
            continue
        try:
            detail = read_json('http://127.0.0.1:8080/logs/api/requests/' + urllib.parse.quote(identity, safe=''))
            if detail.get('events_truncated'):
                record['detail_events_truncated'] = True
            for event in reversed(detail.get('events', [])):
                if event.get('kind') != 'attempt_finished':
                    continue
                code = event.get('details', {}).get('error_code')
                if isinstance(code, str) and code:
                    record['error_code'] = code[:128]
                    record['error_code_source'] = 'upstream_attempt'
                break  # Earlier retry failures cannot explain a later accepted attempt.
        except Exception as error:
            # Detail failure must not hide the failure itself. No exception
            # messages or response bodies enter the monitor's persisted output.
            record['detail_error_type'] = type(error).__name__
    return list(rows.values())


def read_json(url):
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    with opener.open(url, timeout=15) as response:
        return json.load(response)


def ssh_read(host, source):
    # The configured sft SSH route can exceed ten seconds while opening its
    # tunnel. Retry only a proven pre-session banner timeout, with one shared
    # deadline. Authentication failures and remote collector errors stay visible.
    deadline = time.monotonic() + 55
    for attempt in range(2):
        try:
            return subprocess.run(['ssh', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=20',
                                   host, 'python3', '-'], input=source, text=True,
                                  capture_output=True, check=True,
                                  timeout=max(.1, deadline - time.monotonic()))
        except subprocess.CalledProcessError as error:
            if (attempt != 0 or error.returncode != 255
                    or 'Connection timed out during banner exchange' not in (error.stderr or '')
                    or deadline - time.monotonic() <= 1):
                raise
            time.sleep(.5)


def error_metadata(error):
    result = {'error_type': type(error).__name__}
    if isinstance(error, subprocess.CalledProcessError):
        result['ssh_exit_code'] = error.returncode
        result['phase'] = ('ssh_banner' if error.returncode == 255
                           and 'Connection timed out during banner exchange' in (error.stderr or '')
                           else 'ssh_command' if error.returncode == 255 else 'remote_collection')
    return result


def poll(host, known=()):
    end = int(time.time() * 1000)
    start = end - 86400000
    if host == 'local':
        return collect_history(read_json, start, end, FIELDS, set(known))
    source = ('import json,urllib.request,urllib.parse\n' + inspect.getsource(read_json) + '\n'
              + inspect.getsource(collect_history) + '\n'
              + f'print(json.dumps(collect_history(read_json,{start},{end},{FIELDS!r},set({list(known)!r}))))\n')
    result = ssh_read(host, source)
    return json.loads(result.stdout)


def atomic_json(path, value):
    fd, name = tempfile.mkstemp(dir=path.parent, prefix=path.name + '.')
    try:
        with os.fdopen(fd, 'w') as output:
            json.dump(value, output, indent=2)
            output.write('\n')
            output.flush()
            os.fsync(output.fileno())
        os.replace(name, path)
    finally:
        if os.path.exists(name):
            os.unlink(name)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--watch', action='store_true')
    parser.add_argument('--state-dir', type=Path, default=Path.home() / '.hey-proxy/gemini-monitor')
    args = parser.parse_args()
    os.umask(0o077)
    args.state_dir.mkdir(parents=True, exist_ok=True)
    # One watch process owns durable state. One-off checks are read-only and can
    # safely run while that process is polling or committing a snapshot.
    if args.watch:
        lock = (args.state_dir / 'monitor.lock').open('a')
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise SystemExit('A monitor already owns this state directory') from None
        (args.state_dir / 'monitor.pid').write_text(str(os.getpid()) + '\n')
    state_file = args.state_dir / 'state.json'
    seen = set(json.loads(state_file.read_text()).get('seen', [])) if state_file.exists() else set()
    while True:
        new = []
        checks = {}
        with concurrent.futures.ThreadPoolExecutor(max_workers=3) as executor:
            jobs = {executor.submit(poll, h, [identity[len(h)+1:] for identity in seen
                                             if identity.startswith(h + '/')]): h for h in HOSTS}
            for job in concurrent.futures.as_completed(jobs):
                host = jobs[job]
                try:
                    entries = job.result()
                    checks[host] = {'status': 'ok', 'failures_last_day': sum(e['state'] == 'failed' for e in entries),
                                    'interrupted_last_day': sum(e['state'] == 'interrupted' for e in entries),
                                    'cancelled_last_day': sum(e['state'] == 'cancelled' for e in entries)}
                    for entry in entries:
                        identity = host + '/' + entry['request_id']
                        if identity not in seen:
                            if args.watch:
                                seen.add(identity)
                            new.append({'host': host, **entry})
                except Exception as error:
                    checks[host] = {'status': 'unavailable', **error_metadata(error)}
        state = {'checked_at': time.time(), 'hosts': checks, 'new_failures': new, 'seen': sorted(seen)}
        if args.watch:
            # Persist observations before their seen markers: a crash may cause
            # a duplicate observation, but cannot silently lose a new failure.
            if new:
                with (args.state_dir / 'failures.jsonl').open('a') as out:
                    for entry in new:
                        out.write(json.dumps(entry) + '\n')
                    out.flush()
                    os.fsync(out.fileno())
            atomic_json(state_file, state)
        print(json.dumps({'hosts': checks, 'new_failures': new}), flush=True)
        if not args.watch:
            return
        time.sleep(30)


if __name__ == '__main__':
    main()
