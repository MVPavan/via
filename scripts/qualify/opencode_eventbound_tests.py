"""Closed facts for an owned SSE frame over the C1 §9 line bound (§13)."""
import http.server
import json
from pathlib import Path
import tempfile
import threading
import time
import unittest
from unittest import mock

from opencode_driver import Driver
from opencode_safety import Identity

PROMPT = 1024 * 1024 - 8192 - 64
SECRET = 'FAKE_secret_content_never_retained'


def enqueued(text):
    return {'id': 'evt_' + SECRET, 'type': 'session.inbox.enqueued',
            'data': {'sessionID': 'ses_' + SECRET, 'inboxID': 'msg_' + SECRET,
                     'item': {'type': 'user', 'delivery': 'queue',
                              'payload': {'text': text, 'metadata': {SECRET: SECRET * 40}}}}}


class EventBoundFactsTests(unittest.TestCase):
    def facts(self, value):
        from opencode_eventbound import event_bound_facts
        raw = value if type(value) is bytes else json.dumps(value, separators=(',', ':')).encode()
        return event_bound_facts(raw)

    def test_known_type_and_fixture_field_sizes_without_content(self):
        facts = self.facts(enqueued('x' * PROMPT))
        self.assertEqual(facts['event_type'], 'session.inbox.enqueued')
        # The inbox item schema is not reviewed, so its keys are wildcards.
        self.assertEqual(facts['fixture_fields'], [{'path': 'data.*.*.*',
                         'field_bytes': PROMPT, 'x_run_bytes': PROMPT, 'equals': True}])
        paths = {row['path']: row for row in facts['field_bytes']}
        self.assertEqual(paths['data.*.*.*']['count'], 2)  # The prompt and its metadata.
        self.assertIn('data.*.*.*.*', paths)
        self.assertNotIn(SECRET, json.dumps(facts))

    def test_field_that_contains_the_prompt_reports_its_wrapping(self):
        facts = self.facts(enqueued('<wrap ' + SECRET + '>' + 'x' * PROMPT + '</wrap>'))
        row, = facts['fixture_fields']
        self.assertEqual((row['equals'], row['x_run_bytes']), (False, PROMPT))
        self.assertEqual(row['field_bytes'], PROMPT + len('<wrap ' + SECRET + '></wrap>'))
        self.assertNotIn(SECRET, json.dumps(facts))

    def test_unknown_type_is_other_and_not_retained(self):
        value = enqueued('x' * PROMPT)
        value['type'] = 'vendor.' + SECRET.lower()
        facts = self.facts(value)
        self.assertEqual(facts['event_type'], 'other')
        self.assertNotIn(SECRET.lower(), json.dumps(facts))

    def test_unparseable_frame_keeps_only_closed_type_literals_and_run_size(self):
        raw = json.dumps(enqueued('x' * PROMPT), separators=(',', ':')).encode()[:-40]
        raw += b',"type":"vendor.' + SECRET.lower().encode() + b'"'
        facts = self.facts(raw)
        self.assertEqual(facts['event_type'], 'unparsed')
        self.assertEqual(facts['type_literals'], ['session.inbox.enqueued'])
        self.assertEqual(facts['payload_x_run_bytes'], PROMPT)
        self.assertNotIn(SECRET.lower(), json.dumps(facts))


class EventBoundCaptureTests(unittest.TestCase):
    def test_oversized_owned_sse_frame_retains_closed_event_facts(self):
        body = json.dumps(enqueued('x' * (1024 * 1024)), separators=(',', ':')).encode()
        frame = b'data: ' + body + b'\n\n'
        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = 'HTTP/1.1'  # The pinned vendor streams SSE chunked.
            def log_message(self, *_args): pass
            def do_GET(self):
                self.send_response(200); self.send_header('Content-Type', 'text/event-stream')
                self.send_header('Transfer-Encoding', 'chunked'); self.end_headers()
                self.wfile.write(b'%x\r\n' % len(frame) + frame + b'\r\n'); self.wfile.flush()
                time.sleep(1)
        server = http.server.HTTPServer(('127.0.0.1', 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True); thread.start()
        try:
            with tempfile.TemporaryDirectory(prefix='via-ocbound-') as root:
                d = Driver('release', 'fp', 'pin', Path(root) / 'evidence', initialize=False)
                d.project = d.evidence / 'project'; d.ensure_vendor = mock.Mock()
                d._http = mock.Mock(); d._http.origin = 'http://127.0.0.1:' + str(server.server_port)
                d._http.password = b'FAKE-private-password'
                d.vendor_identity = Identity(71, 123); d.proc = mock.Mock()
                try:
                    d.start_event_capture()
                    end = time.monotonic() + 10
                    while d.events_error is None and time.monotonic() < end: time.sleep(.01)
                    self.assertEqual(d.events_error, 'Blocked')
                    d._retain_event_failure()
                    row = json.loads(next(d.evidence.glob('*native-event-bound.json')).read_text())
                finally: d.close_event_capture()
            self.assertEqual(row['line_bytes'], len(body) + 7)
            self.assertEqual(row['event']['event_type'], 'session.inbox.enqueued')
            self.assertEqual(row['event']['payload_bytes'], len(body))
            self.assertEqual([field['field_bytes'] for field in row['event']['fixture_fields']],
                             [1024 * 1024])
            self.assertNotIn(SECRET, json.dumps(row))
        finally:
            server.shutdown(); server.server_close(); thread.join(timeout=5)


if __name__ == '__main__':
    unittest.main()
