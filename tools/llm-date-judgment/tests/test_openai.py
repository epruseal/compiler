"""Test the optional API backend with local fake responses."""
import contextlib
import http.server
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
import judge
import ledger
from test_runner import Env, ADM

JUDGMENT = {'outcome': 'explicit_date', 'date': '2009-06-27',
            'basis_quote': '이 예규는 2009. 6. 27.부터 시행한다.', 'basis_field': '개정문',
            'section_label': '부칙', 'proviso': False, 'proviso_quote': '',
            'partial_enforcement': [], 'attribution': 'a', 'rules_applied': ['R3'], 'reason': 'r'}


@contextlib.contextmanager
def server(body=None, status=200, delay=0):
    requests = []
    class Handler(http.server.BaseHTTPRequestHandler):
        def do_POST(self):
            import time
            requests.append((self.path, dict(self.headers), json.loads(self.rfile.read(int(self.headers['Content-Length'])))))
            if delay:
                time.sleep(delay)
            self.send_response(status)
            self.send_header('Location', '/redirected')
            self.end_headers()
            value = body if body is not None else {'choices': [{'finish_reason': 'stop', 'message': {'content': json.dumps(JUDGMENT)}}],
                                                  'usage': {'prompt_tokens': 12, 'completion_tokens': 5, 'total_tokens': 17}}
            if callable(value):
                value = value(f'http://127.0.0.1:{self.server.server_port}/v1')
            try:
                self.wfile.write(value if isinstance(value, bytes) else json.dumps(value).encode())
            except BrokenPipeError:
                pass
        def log_message(self, *args):
            pass
    httpd = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    thread = threading.Thread(target=httpd.serve_forever, daemon=True)
    thread.start()
    try:
        yield f'http://127.0.0.1:{httpd.server_port}/v1', requests
    finally:
        httpd.shutdown()
        httpd.server_close()
        thread.join()


class ApiTests(Env):
    def setUp(self):
        super().setUp()
        self.env = patch.dict(os.environ, {'TEST_API_KEY': 'fake-secret'}, clear=True)
        self.env.start()
        self.addCleanup(self.env.stop)

    def api_file(self, url, **kwargs):
        return judge.judge_file('admrules', self.cache / 'admrule' / f'{ADM}.xml', 'model/with/slash',
                                'default', None, self.tmp / 'result', backend='openai', base_url=url,
                                api_key_env='TEST_API_KEY', **kwargs)

    def api_run(self, url, *extra):
        return judge.main(['run', '--dataset', 'admrules', '--model', 'model/with/slash', '--backend', 'openai',
                           '--base-url', url, '--api-key-env', 'TEST_API_KEY', '--ops', str(self.ops),
                           '--cache', str(self.cache), '--targets', str(self.targets), '--protected', str(self.protected),
                           '--max-calls', '1', *extra])

    def test_request_and_valid_judgment(self):
        with server() as (url, requests):
            entry = self.api_file(url)
        self.assertTrue(entry['adoptable'])
        self.assertEqual(entry['backend'], 'openai')
        self.assertEqual(entry['call']['usage']['prompt_tokens'], 12)
        path, headers, request = requests[0]
        self.assertEqual(path, '/v1/chat/completions')
        self.assertEqual(headers['Authorization'], 'Bearer fake-secret')
        self.assertEqual(request['messages'][0]['content'], judge.SYSTEM)
        self.assertEqual(request['response_format']['json_schema']['schema'], json.loads(judge.current_prompt()[1]))
        artifacts = ''.join(p.read_text() for p in (self.tmp / 'result').iterdir())
        for value in (url, 'TEST_API_KEY', 'fake-secret'):
            self.assertNotIn(value, artifacts)

    def test_json_object(self):
        with server() as (url, requests):
            self.assertTrue(self.api_file(url, response_format='json_object')['adoptable'])
        self.assertEqual(requests[0][2]['response_format'], {'type': 'json_object'})

    def test_invalid_responses(self):
        bad = [b'not json', b'{"choices":[],"choices":[]}', {'choices': []},
               {'choices': [{'finish_reason': 'length', 'message': {'content': json.dumps(JUDGMENT)}}]},
               {'choices': [{'finish_reason': 'stop', 'message': {'content': json.dumps(JUDGMENT), 'refusal': 'secret'}}]}]
        for change in ({'proviso': 1}, {'outcome': 'wrong'}, {'partial_enforcement': [{'quote': 1, 'date': ''}]},
                       {'extra': 'x'}, {'reason': None}):
            j = dict(JUDGMENT, **change)
            bad.append({'choices': [{'finish_reason': 'stop', 'message': {'content': json.dumps(j)}}]})
        missing = dict(JUDGMENT)
        del missing['date']
        bad.append({'choices': [{'finish_reason': 'stop', 'message': {'content': json.dumps(missing)}}]})
        for i, value in enumerate(bad):
            with self.subTest(i=i), server(value) as (url, requests):
                out = self.tmp / f'bad-{i}'
                e = judge.judge_file('admrules', self.cache / 'admrule' / f'{ADM}.xml', 'm', 'default', None, out,
                                     backend='openai', base_url=url, api_key_env='TEST_API_KEY')
                self.assertIsNone(e['judgment'])
                self.assertEqual(len(requests), 1)

    def test_sensitive_echoes(self):
        import openai_backend
        for field in ('reason', 'attribution'):
            for sensitive in ('fake-secret', 'TEST_API_KEY'):
                j = dict(JUDGMENT, **{field: sensitive})
                with self.assertRaises(openai_backend.BackendError):
                    openai_backend.validate_judgment(j, json.loads(judge.current_prompt()[1]), (sensitive,), judge.one_line)
        with server() as (url, _):
            for value in (url, url + '/chat/completions'):
                with self.assertRaises(openai_backend.BackendError):
                    openai_backend.validate_judgment(dict(JUDGMENT, reason=value), json.loads(judge.current_prompt()[1]), (url,), judge.one_line)

    def test_errors_redirect_timeout_and_no_secrets(self):
        for status in (302, 401, 500):
            with self.subTest(status=status), server(b'fake-secret TEST_API_KEY', status) as (url, requests):
                e = judge.judge_file('admrules', self.cache / 'admrule' / f'{ADM}.xml', 'm', 'default', None,
                                     self.tmp / str(status), backend='openai', base_url=url, api_key_env='TEST_API_KEY')
                self.assertIsNone(e['judgment'])
                self.assertEqual(len(requests), 1)
                self.assertNotIn('fake-secret', json.dumps(e))
        with server(delay=.2) as (url, _):
            self.assertIsNone(self.api_file(url, timeout=.02)['judgment'])

    def test_invalid_config_creates_no_files(self):
        for url in ('https://u:p@example.invalid/v1', 'https://x.invalid/?x=1', 'https://x.invalid/#f',
                    'https://x.invalid:bad', 'https://x.invalid\\evil', 'https://%65xample.invalid', ' https://x.invalid'):
            with self.subTest(url=url):
                with self.assertRaises(ValueError):
                    self.api_file(url)
                self.assertFalse((self.tmp / 'result').exists())
        with patch.dict(os.environ, {'TEST_API_KEY': 'fake secret'}):
            with self.assertRaises(ValueError):
                self.api_file('http://127.0.0.1/v1')
        with patch.dict(os.environ, {}, clear=True):
            with self.assertRaises(ValueError):
                self.api_file('http://127.0.0.1/v1')

    def test_run_policy_recovery_and_transitions(self):
        import shutil
        with server() as (url, requests):
            self.assertEqual(self.api_run(url), 0)
            first = self.canonical()
            self.assertEqual(self.api_run(url), 0)
            self.assertEqual(len(requests), 1)
            shutil.rmtree(self.ops / 'ledger')
            self.assertEqual(self.api_run(url), 0)
            self.assertEqual(len(requests), 1)
            self.assertEqual(self.canonical()['entries'], first['entries'])
            self.assertEqual(self.api_run(url, '--response-format', 'json_object'), 4)
            self.assertEqual(self.api_run(url, '--response-format', 'json_object', '--replace-policy'), 0)
            self.assertEqual(len(requests), 2)
            self.assertEqual(self.run_judge(), 4)
            self.assertNotIn('model/with/slash', str(next((self.ops / 'raw').rglob('call-args.json'))))

    def test_failed_entries_and_failure_namespace(self):
        with server(b'bad') as (url, _):
            judge.TARGET_FAILURE_LIMIT = 1
            self.assertEqual(self.api_run(url), 0)
            self.assertEqual(self.api_run(url), 0)
            entry = self.canonical()['entries'][0]
            self.assertEqual(entry['kind'], 'failed')
            self.assertEqual(entry['backend'], 'openai')
            one = judge.failures_root(self.ops, self.canonical()['policy'])
            other = judge.failures_root(self.ops, dict(self.canonical()['policy'], endpoint_sha256='b' * 64))
            self.assertNotEqual(one, other)
            self.assertFalse(other.exists())

    def test_raw_revalidation(self):
        import shutil
        with server() as (url, requests):
            self.api_run(url)
            shutil.rmtree(self.ops / 'ledger')
            raw = next((self.ops / 'raw').rglob('stdout.json'))
            value = json.loads(raw.read_text())
            value['structured_output']['proviso'] = 1
            raw.write_text(json.dumps(value))
            self.api_run(url)
            self.assertEqual(len(requests), 2)

    def test_normalization_sensitive_echo(self):
        import openai_backend
        with self.assertRaises(openai_backend.BackendError):
            openai_backend.validate_judgment(dict(JUDGMENT, reason='fake-\nsecret'),
                json.loads(judge.current_prompt()[1]), ('fake- secret',), judge.one_line)

    def test_paid_key_and_cli_api_options_refusal(self):
        with patch.dict(os.environ, {'ANTHROPIC_API_KEY': 'not-real'}):
            with self.assertRaises(judge.PaidKeyError):
                self.api_file('http://127.0.0.1/v1')
        self.assertEqual(judge.main(['run', '--dataset', 'admrules', '--model', 'm', '--base-url', 'http://127.0.0.1/v1',
                        '--ops', str(self.ops), '--cache', str(self.cache), '--targets', str(self.targets),
                        '--protected', str(self.protected), '--max-calls', '1']), 2)
        self.assertFalse(self.ops.exists())

    def test_one_route(self):
        with server() as (url, requests):
            self.assertEqual(judge.main(['one', '--dataset', 'admrules', '--source', str(self.cache / 'admrule' / f'{ADM}.xml'),
                                         '--model', 'm', '--workdir', str(self.tmp), '--backend', 'openai', '--base-url', url,
                                         '--api-key-env', 'TEST_API_KEY']), 0)
            self.assertEqual(len(requests), 1)


class PolicyTests(unittest.TestCase):
    def test_variants_and_absence_comparison(self):
        cli = dict(model='m', effort='default', prompt_version='p', runner_version='r')
        api = dict(cli, backend='openai', endpoint_sha256='a' * 64, response_format='json_schema')
        ledger.validate_policy(cli)
        ledger.validate_policy(api)
        for p in (dict(cli, backend='claude'), dict(cli, backend='openai'), dict(api, endpoint_sha256='bad')):
            with self.assertRaises(ledger.LedgerError):
                ledger.validate_policy(p)
        self.assertFalse(ledger.policy_matches(dict(api, kind='judged'), cli))
        self.assertFalse(ledger.policy_matches(dict(cli, kind='judged'), api))
        self.assertTrue(ledger.policy_matches(dict(api, kind='judged'), api))

    def test_schema_unsupported_and_bool_integer(self):
        import openai_backend
        for schema, value in (({'type': 'integer'}, True), ({'type': 'string', 'pattern': 'x'}, 'x'),
                              ({'type': 'object', 'properties': [], 'required': []}, {})):
            with self.assertRaises(openai_backend.BackendError):
                openai_backend.validate_schema(value, schema)

    def test_gold_resume_archives_api_and_score_refuses(self):
        import importlib.util
        spec = importlib.util.spec_from_file_location('gold_test', ROOT / 'measure' / 'gold_run.py')
        gold = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(gold)
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            gold.CACHE = root / 'cache'
            gold.CALL_LOG = root / 'calls.log'
            source = gold.CACHE / 'admrule' / f'{ADM}.xml'
            source.parent.mkdir(parents=True)
            source.write_bytes((ROOT / 'tests' / 'fixtures' / f'{ADM}.xml').read_bytes())
            case = {'dataset': 'admrules', 'revision': ADM}
            specfile = root / 'spec.json'
            specfile.write_text(json.dumps({'cases': [case]}))
            index = root / 'index.tsv'
            index.write_text('header\n')
            target = root / 'out' / 'm' / f'admrules-{ADM}'
            target.mkdir(parents=True)
            api = {'backend': 'openai', 'prompt_version': judge.current_prompt()[2], 'judgment': JUDGMENT,
                   'call': {'exit': 0, 'is_error': False}}
            (target / 'entry.json').write_text(json.dumps(api))
            with patch.object(judge, 'judge_file', return_value={}) as call, patch.object(judge, 'refuse_paid_key'):
                self.assertEqual(gold.run(['m'], index, root / 'out', 2, specfile), 0)
                call.assert_called_once()
            self.assertTrue((target.parent / (target.name + '.abandoned') / '0' / 'entry.json').exists())
            target.mkdir()
            (target / 'entry.json').write_text(json.dumps(api))
            with self.assertRaisesRegex(ledger.LedgerError, 'measurement policy mismatch'):
                gold.score(root / 'out')

    def test_estimate_rejects_api_only_mixed_and_empty_usage(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for mixed in (False, True):
                for usage in ({'prompt_tokens': 12, 'completion_tokens': 5}, {}):
                    api = root / 'm' / 'api'
                    api.mkdir(parents=True, exist_ok=True)
                    (api / 'entry.json').write_text(json.dumps({'backend': 'openai', 'call': {'usage': usage}}))
                    (api / 'prompt.txt').write_text('prompt')
                    if mixed:
                        cli = root / 'm' / 'cli'
                        cli.mkdir(exist_ok=True)
                        (cli / 'entry.json').write_text(json.dumps({'call': {'usage': {'input_tokens': 12}}}))
                        (cli / 'prompt.txt').write_text('prompt')
                    proc = subprocess.run([sys.executable, '-I', str(ROOT / 'measure' / 'estimate.py'), str(root),
                                           str(root / 'absent-population'), 'm'], text=True, capture_output=True)
                    self.assertNotEqual(proc.returncode, 0)
                    self.assertIn('measurement policy mismatch', proc.stderr)
                    self.assertEqual(proc.stdout, '')


if __name__ == '__main__':
    unittest.main()
