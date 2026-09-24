"""Metadata-only monitor regressions; no real proxy or SSH requests."""
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
from urllib.parse import parse_qs, urlsplit

SOURCE = Path(__file__).resolve().parents[1] / 'scripts/monitor-gemini.py'
SPEC = importlib.util.spec_from_file_location('gemini_monitor', SOURCE)
monitor = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(monitor)


class MonitorTests(unittest.TestCase):
    def test_ssh_banner_timeout_retries_within_one_deadline(self):
        error = subprocess.CalledProcessError(255, ['ssh'], stderr='Connection timed out during banner exchange')
        success = subprocess.CompletedProcess(['ssh'], 0, stdout='[]')
        with patch.object(monitor.subprocess, 'run', side_effect=[error, success]) as run, \
                patch.object(monitor.time, 'sleep') as sleep, \
                patch.object(monitor.time, 'monotonic', side_effect=[0, 0, 20, 20.5]):
            self.assertIs(monitor.ssh_read('example', 'synthetic'), success)
        self.assertEqual(run.call_count, 2)
        self.assertIn('ConnectTimeout=20', run.call_args.args[0])
        self.assertEqual(run.call_args.kwargs['timeout'], 34.5)
        sleep.assert_called_once_with(.5)

    def test_ssh_auth_and_remote_errors_are_not_retried_or_logged_verbatim(self):
        for code, stderr in [(255, 'Permission denied: NEVER_RECORD_SECRET'),
                             (1, 'remote error: NEVER_RECORD_SECRET')]:
            error = subprocess.CalledProcessError(code, ['ssh'], stderr=stderr)
            with patch.object(monitor.subprocess, 'run', side_effect=error) as run:
                with self.assertRaises(subprocess.CalledProcessError):
                    monitor.ssh_read('example', 'synthetic')
            self.assertEqual(run.call_count, 1)
            self.assertNotIn('NEVER_RECORD_SECRET', json.dumps(monitor.error_metadata(error)))

    def test_ssh_banner_retry_stops_after_second_failure_or_exhausted_budget(self):
        error = subprocess.CalledProcessError(255, ['ssh'], stderr='Connection timed out during banner exchange')
        for clock, expected_calls in [([0, 0, 20, 20.5], 2), ([0, 0, 54.5], 1)]:
            with patch.object(monitor.subprocess, 'run', side_effect=error) as run, \
                    patch.object(monitor.time, 'sleep'), \
                    patch.object(monitor.time, 'monotonic', side_effect=clock):
                with self.assertRaises(subprocess.CalledProcessError):
                    monitor.ssh_read('example', 'synthetic')
            self.assertEqual(run.call_count, expected_calls)
        self.assertEqual(monitor.error_metadata(error)['phase'], 'ssh_banner')

    def entry(self, identity, state='failed', code='server_error'):
        return {'request_id': identity, 'state': state, 'error_code': code,
                'request_body': 'NEVER_RECORD_SECRET', 'response_body': 'NEVER_RECORD_SECRET'}

    def test_paginates_all_states_with_fixed_bounds_and_metadata_only(self):
        queries = []

        def read(url):
            q = parse_qs(urlsplit(url).query)
            queries.append(q)
            self.assertEqual(q['start'], ['100'])
            self.assertEqual(q['end'], ['200'])
            state = q['state'][0]
            if state != 'failed':
                return {'entries': [self.entry(state, state)], 'next_cursor': None}
            if 'cursor' not in q:
                return {'entries': [self.entry(str(n)) for n in range(500)],
                        'next_cursor': 'opaque+/=& cursor'}
            self.assertEqual(q['cursor'], ['opaque+/=& cursor'])
            return {'entries': [self.entry('500')], 'next_cursor': None}

        rows = monitor.collect_history(read, 100, 200, monitor.FIELDS, set())
        self.assertEqual(len(rows), 503)
        self.assertEqual(len(queries), 4)
        self.assertNotIn('NEVER_RECORD_SECRET', json.dumps(rows))
        self.assertEqual({r['state'] for r in rows}, {'failed', 'cancelled', 'interrupted'})

    def test_native_error_code_is_recovered_without_recording_messages(self):
        def read(url):
            if '/requests/' in url:
                return {'events': [{'kind': 'attempt_finished', 'details': {
                    'error_code': 'INVALID_ARGUMENT', 'message': 'NEVER_RECORD_SECRET'}}]}
            q = parse_qs(urlsplit(url).query)
            return {'entries': [self.entry('bad', code=None)] if q['state'] == ['failed'] else []}

        rows = monitor.collect_history(read, 0, 1, monitor.FIELDS, set())
        self.assertEqual(rows[0]['error_code'], 'INVALID_ARGUMENT')
        self.assertEqual(rows[0]['error_code_source'], 'upstream_attempt')
        self.assertNotIn('NEVER_RECORD_SECRET', json.dumps(rows))

    def test_client_forwarding_failures_are_included_on_every_page_and_state(self):
        def read(url):
            q = parse_qs(urlsplit(url).query)
            # Match the real history API's default exclusion of client records.
            if q.get('include_clients') != ['true']:
                return {'entries': [], 'next_cursor': None}
            state = q['state'][0]
            page = q.get('cursor', ['first'])[0]
            entry = {**self.entry(state + '-' + page, state), 'mode': 'client'}
            return {'entries': [entry], 'next_cursor': 'second' if page == 'first' else None}

        rows = monitor.collect_history(read, 100, 200, monitor.FIELDS, set())
        self.assertEqual({r['request_id'] for r in rows}, {
            state + '-' + page
            for state in ('failed', 'interrupted', 'cancelled')
            for page in ('first', 'second')})
        self.assertNotIn('NEVER_RECORD_SECRET', json.dumps(rows))

    def test_recovered_attempt_does_not_misclassify_later_failure(self):
        def read(url):
            if '/requests/' in url:
                return {'events_truncated': True, 'events': [
                    {'kind': 'attempt_finished', 'details': {'error_code': 'UNAVAILABLE'}},
                    {'kind': 'attempt_finished', 'details': {'error_code': None, 'outcome': 'accepted'}}]}
            q = parse_qs(urlsplit(url).query)
            return {'entries': [self.entry('bad', code=None)] if q['state'] == ['failed'] else []}

        rows = monitor.collect_history(read, 0, 1, monitor.FIELDS, set())
        self.assertIsNone(rows[0]['error_code'])
        self.assertTrue(rows[0]['detail_events_truncated'])

    def test_detail_failure_cannot_hide_observation_and_known_ids_skip_detail(self):
        details = []

        def read(url):
            if '/requests/' in url:
                details.append(url)
                raise OSError('NEVER_RECORD_SECRET')
            q = parse_qs(urlsplit(url).query)
            return {'entries': [self.entry('new', code=None), self.entry('known', code=None)]
                    if q['state'] == ['failed'] else []}

        rows = monitor.collect_history(read, 0, 1, monitor.FIELDS, {'known'})
        self.assertEqual(len(rows), 2)
        self.assertEqual(len(details), 1)
        self.assertEqual(rows[0]['detail_error_type'], 'OSError')
        self.assertNotIn('NEVER_RECORD_SECRET', json.dumps(rows))

    def test_cursor_cycle_is_reported_instead_of_looping_or_silently_truncating(self):
        with self.assertRaisesRegex(ValueError, 'repeated'):
            monitor.collect_history(lambda _: {'entries': [], 'next_cursor': 'same'},
                                    0, 1, monitor.FIELDS, set())

    def test_singleton_and_read_only_check_preserve_live_state(self):
        with tempfile.TemporaryDirectory() as directory:
            program = ('import importlib.util,sys\n'
                       f's=importlib.util.spec_from_file_location("monitor",{str(SOURCE)!r})\n'
                       'm=importlib.util.module_from_spec(s);s.loader.exec_module(m);m.HOSTS=[]\n'
                       'm.main()\n')
            base = [sys.executable, '-c', program, '--state-dir', directory]
            process = subprocess.Popen(base + ['--watch'], stdout=subprocess.PIPE,
                                       stderr=subprocess.PIPE, text=True)
            try:
                self.assertTrue(process.stdout.readline().startswith('{'))
                state = Path(directory) / 'state.json'
                original = state.read_bytes()
                duplicate = subprocess.run(base + ['--watch'], capture_output=True, text=True, timeout=5)
                self.assertNotEqual(duplicate.returncode, 0)
                self.assertIn('already owns', duplicate.stderr)
                once = subprocess.run(base, capture_output=True, text=True, timeout=5)
                self.assertEqual(once.returncode, 0)
                self.assertEqual(state.read_bytes(), original)
                self.assertEqual(int((Path(directory) / 'monitor.pid').read_text()), process.pid)
            finally:
                process.terminate()
                process.communicate(timeout=5)


if __name__ == '__main__':
    unittest.main()
