"""Test API transport boundaries and their route connections."""
import contextlib
import http.server
import io
import json
import os
from pathlib import Path
import socketserver
import threading
import unittest
from unittest.mock import patch

from test_openai import ApiTests, JUDGMENT, server
import judge
import openai_backend


@contextlib.contextmanager
def wire_server(response):
    class Handler(socketserver.BaseRequestHandler):
        def handle(self):
            self.request.recv(65536)
            self.request.sendall(response)
    with socketserver.TCPServer(('127.0.0.1', 0), Handler) as srv:
        thread = threading.Thread(target=srv.serve_forever, daemon=True)
        thread.start()
        try:
            yield f'http://127.0.0.1:{srv.server_address[1]}/v1'
        finally:
            srv.shutdown()
            thread.join(timeout=2)
            assert not thread.is_alive()


class BoundaryTests(ApiTests):
    def route(self, route, url, folder):
        output = io.StringIO()
        error = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(error):
            if route == 'file':
                entry = judge.judge_file('admrules', self.cache / 'admrule' / '2200000088451.xml', 'm', 'default',
                    None, folder, backend='openai', base_url=url, api_key_env='TEST_API_KEY')
                ok = entry['judgment'] is not None
            elif route == 'one':
                result = judge.main(['one', '--dataset', 'admrules', '--source', str(self.cache / 'admrule' / '2200000088451.xml'),
                    '--model', 'm', '--workdir', str(self.tmp), '--backend', 'openai', '--base-url', url,
                    '--api-key-env', 'TEST_API_KEY'])
                ok = result == 0
            else:
                old = self.ops
                self.ops = folder
                try:
                    result = self.api_run(url)
                    self.assertEqual(result, 0)
                    doc = self.canonical()
                    ok = bool(doc['entries'])
                    if not ok:
                        self.assertTrue(list(folder.rglob('failures-admrules.json')))
                        self.assertTrue((folder / 'ledger' / 'admrules.json').exists())
                finally:
                    self.ops = old
        text = output.getvalue() + error.getvalue()
        if folder.exists():
            text += ''.join(p.read_text() for p in folder.rglob('*') if p.is_file())
        return ok, text

    def test_protocol_errors_all_routes(self):
        responses = [b'HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nabc',
                     b'fake-secret TEST_API_KEY invalid status\r\n\r\n']
        for i, response in enumerate(responses):
            for route in ('one', 'run', 'file'):
                with self.subTest(i=i, route=route), wire_server(response) as url:
                    ok, text = self.route(route, url, self.tmp / f'wire-{i}-{route}')
                    self.assertFalse(ok)
                    self.assertIn('transport_error', text)
                    for secret in ('fake-secret', 'TEST_API_KEY', 'invalid status', url):
                        self.assertNotIn(secret, text)

    def test_exponent_overflow_at_each_depth(self):
        for value in ('1e400', '-1e400'):
            for location in ('"created":OVERFLOW', '"metadata":{"nested":[OVERFLOW]}', '"usage":{"prompt_tokens":OVERFLOW}'):
                payload = json.dumps({'choices': [{'finish_reason': 'stop', 'message': {'content': json.dumps(JUDGMENT)}}]})
                payload = payload[:-1] + ',' + location.replace('OVERFLOW', value) + '}'
                with self.subTest(location=location, value=value), server(payload.encode()) as (url, _):
                    ok, text = self.route('one', url, self.tmp / 'unused')
                    self.assertFalse(ok)
                    self.assertIn('invalid_json', text)
            with self.assertRaises(openai_backend.BackendError):
                openai_backend.loads('{"nested":[{"number":' + value + '}]}')
        self.assertEqual(openai_backend.loads('{"n":1e3}'), {'n': 1000.0})

    def test_http_sensitive_echoes_all_routes(self):
        for route in ('one', 'run', 'file'):
            for kind in ('key', 'env', 'base', 'endpoint', 'nested'):
                secrets = []
                def reflected(url):
                    secret = {'key': 'fake-secret', 'env': 'TEST_API_KEY', 'base': url,
                              'endpoint': url + '/chat/completions', 'nested': 'fake-secret'}[kind]
                    secrets.append(secret)
                    j = dict(JUDGMENT, reason=' ' + secret + ' ')
                    if kind == 'nested':
                        j['reason'] = 'ok'
                        j['partial_enforcement'] = [{'quote': secret, 'date': ''}]
                    return {'choices': [{'finish_reason': 'stop', 'message': {'content': json.dumps(j)}}]}
                with server(reflected) as (url, _):
                    ok, text = self.route(route, url, self.tmp / f'echo-{route}-{kind}')
                self.assertFalse(ok)
                for sensitive in ('fake-secret', 'TEST_API_KEY', *secrets):
                    self.assertNotIn(sensitive, text)
        j = dict(JUDGMENT, reason='https://source.invalid/legal')
        with server({'choices': [{'finish_reason': 'stop', 'message': {'content': json.dumps(j)}}]}) as (url, _):
            ok, text = self.route('file', url, self.tmp / 'unrelated')
            self.assertTrue(ok)
            self.assertIn('https://source.invalid/legal', text)

    def test_redirect_destination_observes_get(self):
        seen = []
        class Destination(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                seen.append(self.command)
                self.send_response(200); self.end_headers()
            do_POST = do_GET
            do_PUT = do_GET
            do_DELETE = do_GET
            do_HEAD = do_GET
            def log_message(self, *args): pass
        dest = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Destination)
        thread = threading.Thread(target=dest.serve_forever, daemon=True)
        thread.start()
        class Source(Destination):
            def do_POST(self):
                self.send_response(302)
                self.send_header('Location', f'http://127.0.0.1:{dest.server_port}/target')
                self.end_headers()
        source = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Source)
        source_thread = threading.Thread(target=source.serve_forever, daemon=True)
        source_thread.start()
        try:
            ok, _ = self.route('one', f'http://127.0.0.1:{source.server_port}', self.tmp / 'unused')
            self.assertFalse(ok)
            self.assertEqual(seen, [])
        finally:
            source.shutdown(); source.server_close(); source_thread.join(timeout=2)
            dest.shutdown(); dest.server_close(); thread.join(timeout=2)

    def test_environment_proxy_receives_nothing(self):
        with server() as (url, target), server(b'proxy') as (proxy, proxied):
            env = {key: proxy for key in ('http_proxy', 'https_proxy', 'all_proxy', 'HTTP_PROXY', 'HTTPS_PROXY', 'ALL_PROXY')}
            env.update({'no_proxy': '', 'NO_PROXY': ''})
            with patch.dict(os.environ, env), patch('urllib.request.proxy_bypass', return_value=False):
                ok, _ = self.route('one', url, self.tmp / 'unused')
            self.assertTrue(ok)
            self.assertEqual(len(target), 1)
            self.assertEqual(proxied, [])


if __name__ == '__main__':
    unittest.main()
