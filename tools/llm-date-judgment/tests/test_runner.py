"""Tests of the ledger, the runner (fake claude executable), the queue build and the protection list."""
import json
import os
import re
import shutil
import stat
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
REPO = ROOT.parents[1]
sys.path.insert(0, str(ROOT))
import judge  # noqa: E402
import ledger  # noqa: E402
import queue_build  # noqa: E402
import source_text  # noqa: E402

FIX = HERE / 'fixtures'
ADM = '2200000088451'
ADM_SHA = '9f137cda5dbf9672d4f736830bf94d73a14d8726b035b72c5c008fec120da000'
ORD = '1081693'
ORD_SHA = 'e8b86db540c13872c090984d657b708ab0af486009ea0063bda0814f698a94c1'

FAKE = '''#!/usr/bin/env python3
import json, os, sys
sys.stdin.read()
mode = os.environ.get('FAKE_MODE', 'ok')
if mode == 'exit1':
    print(json.dumps({"is_error": True, "result": "Prompt is too long"})); sys.exit(1)
if mode == 'error':
    print(json.dumps({"is_error": True, "result": "usage limit"})); sys.exit(0)
out = {"outcome": "explicit_date", "date": "2009-06-27", "basis_quote": "이 예규는 2009. 6. 27.부터 시행한다.",
       "basis_field": "개정문", "section_label": "부  칙", "proviso": False, "proviso_quote": "",
       "partial_enforcement": [], "attribution": "a", "rules_applied": ["R3"], "reason": "r\\nline"}
print(json.dumps({"is_error": False, "structured_output": out, "usage": {"input_tokens": 1}, "total_cost_usd": 0.01}))
'''


class Env(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix='ldj-test-'))
        self.cache = self.tmp / 'cache'
        (self.cache / 'admrule').mkdir(parents=True)
        (self.cache / 'ordinance').mkdir(parents=True)
        shutil.copy(FIX / f'{ADM}.xml', self.cache / 'admrule' / f'{ADM}.xml')
        shutil.copy(FIX / f'{ORD}.xml', self.cache / 'ordinance' / f'{ORD}.xml')
        self.ops = self.tmp / 'ops'
        self.fake = self.tmp / 'claude'
        self.fake.write_text(FAKE, encoding='utf-8')
        self.fake.chmod(self.fake.stat().st_mode | stat.S_IEXEC)
        self.protected = self.tmp / 'protected.json'
        self.write_protected([])
        self.targets = self.tmp / 'pending.json'
        self.targets.write_text(json.dumps({'targets': [self.target()]}), encoding='utf-8')
        self.saved = (judge.STOP_AFTER_FAILURES, judge.TARGET_FAILURE_LIMIT)
        os.environ['FAKE_MODE'] = 'ok'

    def tearDown(self):
        judge.STOP_AFTER_FAILURES, judge.TARGET_FAILURE_LIMIT = self.saved
        shutil.rmtree(self.tmp)

    def write_protected(self, revisions):
        self.protected.write_text(json.dumps({'schema_version': 1, 'revisions': revisions}), encoding='utf-8')

    def target(self, parser=None):
        return {'key': {'dataset': 'admrules', 'identity': '2111925', 'revision': ADM, 'source_sha256': ADM_SHA},
                'path': f'admrule/{ADM}.xml', 'next_promulgation': None,
                'parser': parser or {'category': 'cross-arrival', 'status': 'estimated', 'date': '2009-04-10', 'rule': 'x'}}

    def run_judge(self, effort='default', max_calls=10):
        return judge.main(['run', '--dataset', 'admrules', '--model', 'm1', '--effort', effort, '--claude', str(self.fake),
                           '--ops', str(self.ops), '--cache', str(self.cache), '--targets', str(self.targets),
                           '--protected', str(self.protected), '--max-calls', str(max_calls)])

    def canonical(self):
        return ledger.Ledger(self.ops, 'admrules').load()


class RunnerTests(Env):
    def test_judges_and_writes_entry_raw_and_call_args(self):
        self.assertEqual(self.run_judge(), 0)
        doc = self.canonical()
        self.assertEqual(len(doc['entries']), 1)
        e = doc['entries'][0]
        self.assertTrue(e['adoptable'])
        self.assertEqual(e['date'], '2009-06-27')
        self.assertEqual(e['cross_check_at_judgment'], 'disagree')
        self.assertNotIn('\n', e['judgment']['reason'])
        self.assertEqual(doc['policy']['runner_version'], judge.RUNNER_VERSION)
        raw = list((self.ops / 'raw').rglob('call-args.json'))
        self.assertEqual(len(raw), 1)
        self.assertEqual(json.loads(raw[0].read_text())['effort'], 'default')

    def test_rerun_skips_current_policy_entry(self):
        self.run_judge()
        before = (self.ops / 'calls.log').read_text().count('\n')
        self.run_judge()
        self.assertEqual((self.ops / 'calls.log').read_text().count('\n'), before)

    def test_restores_from_raw_without_call_only_when_policy_matches(self):
        self.run_judge()
        shutil.rmtree(self.ops / 'ledger')
        os.environ['FAKE_MODE'] = 'error'
        self.assertEqual(self.run_judge(max_calls=0), 0)
        self.assertEqual(len(self.canonical()['entries']), 1)
        shutil.rmtree(self.ops / 'ledger')
        self.run_judge(effort='high', max_calls=0)
        self.assertEqual(len(self.canonical()['entries']), 0)

    def test_failed_raw_is_called_again(self):
        os.environ['FAKE_MODE'] = 'error'
        self.run_judge()
        self.assertEqual(len(self.canonical()['entries']), 0)
        os.environ['FAKE_MODE'] = 'ok'
        self.run_judge()
        self.assertEqual(len(self.canonical()['entries']), 1)

    def test_consecutive_failures_stop_and_target_limit(self):
        judge.STOP_AFTER_FAILURES = 1
        os.environ['FAKE_MODE'] = 'exit1'
        self.assertEqual(self.run_judge(), 3)
        # The run of failures that stopped the run does not count against the target.
        fails = next((self.ops / 'failures').rglob('failures-admrules.json'))
        self.assertEqual(json.loads(fails.read_text())['counts'], {'|'.join(('admrules', '2111925', ADM, ADM_SHA)): 0})
        judge.STOP_AFTER_FAILURES = 5
        judge.TARGET_FAILURE_LIMIT = 1
        self.assertEqual(self.run_judge(), 0)
        os.environ['FAKE_MODE'] = 'ok'
        self.run_judge()
        self.assertEqual([e['kind'] for e in self.canonical()['entries']], ['failed'])

    def test_protected_revision_is_not_judged_and_gets_excluded_entry(self):
        self.write_protected([{'dataset': 'admrules', 'revision': ADM, 'identity': '2111925', 'copies': [{'path': 'x', 'source_sha256': ADM_SHA}],
                               'decision_id': 'd', 'source': 's', 'expected': {}}])
        self.run_judge()
        self.assertFalse((self.ops / 'calls.log').exists())
        e = self.canonical()['entries'][0]
        self.assertEqual((e['kind'], e['reason']), ('excluded', ledger.EXCLUDED_REASON))

    def test_newly_protected_judged_entry_moves_to_history(self):
        self.run_judge()
        self.write_protected([{'dataset': 'admrules', 'revision': ADM, 'identity': '2111925', 'copies': [{'path': 'x', 'source_sha256': ADM_SHA}],
                               'decision_id': 'd', 'source': 's', 'expected': {}}])
        self.run_judge()
        self.assertEqual(self.canonical()['entries'][0]['kind'], 'excluded')
        history = ledger.read_lines(self.ops / 'ledger-history' / 'admrules.jsonl')[0]
        self.assertEqual([h['kind'] for h in history], ['judged'])

    def test_runner_lock_is_exclusive(self):
        import fcntl
        self.ops.mkdir(parents=True)
        held = open(self.ops / 'lock', 'w')
        fcntl.flock(held, fcntl.LOCK_EX)
        try:
            self.assertEqual(self.run_judge(), 75)
        finally:
            held.close()


class LedgerTests(Env):
    def test_compaction_is_deterministic_and_idempotent(self):
        self.run_judge()
        first = (self.ops / 'ledger' / 'admrules.json').read_bytes()
        led = ledger.Ledger(self.ops, 'admrules')
        policy = self.canonical()['policy']
        led.compact(policy, {}, None)
        doc = json.loads((self.ops / 'ledger' / 'admrules.json').read_text())
        self.assertEqual(doc['entries'], json.loads(first)['entries'])

    def test_journal_cut_line_and_history_dedupe(self):
        self.run_judge()
        led = ledger.Ledger(self.ops, 'admrules')
        entry = self.canonical()['entries'][0]
        newer = dict(entry, judged_at='2099-01-01T00:00:00Z')
        led.add(newer)
        led.add(newer)
        with open(led.journal, 'a', encoding='utf-8') as fh:
            fh.write('{"key": ')
        report = led.compact(self.canonical()['policy'], {}, None)
        self.assertTrue(report['journal_cut_line'])
        history = ledger.read_lines(led.history)[0]
        self.assertEqual(len(history), 1)
        led.add(newer)
        led.compact(self.canonical()['policy'], {}, None)
        self.assertEqual(len(ledger.read_lines(led.history)[0]), 1)

    def test_format_errors(self):
        path = self.ops / 'ledger' / 'admrules.json'
        path.parent.mkdir(parents=True)
        path.write_text('{"schema_version": 1, "schema_version": 1}')
        with self.assertRaises(ledger.LedgerError):
            ledger.Ledger(self.ops, 'admrules').load()
        path.write_text(json.dumps({'schema_version': 1, 'dataset': 'ordinances', 'policy': None, 'protected_list_sha256': None, 'entries': []}))
        with self.assertRaises(ledger.LedgerError):
            ledger.Ledger(self.ops, 'admrules').load()
        path.write_text(json.dumps({'schema_version': 1, 'dataset': 'admrules', 'policy': None, 'protected_list_sha256': None, 'entries': [], 'extra': 1}))
        with self.assertRaises(ledger.LedgerError):
            ledger.Ledger(self.ops, 'admrules').load()


class QueueTests(Env):
    def rows(self, sha=ADM_SHA):
        return [{'dataset': 'admrules', 'path': str(self.cache / 'admrule' / f'{ADM}.xml'), 'serial': ADM, 'identity': '2111925',
                 'prom_raw': '20090410', 'eff_raw': '99991231', 'eff_class': 'sentinel_99991231', 'sha256': sha,
                 'text_blank': False, 'html': False, 'kind': '등기예규'},
                {'dataset': 'admrules', 'path': '/x', 'serial': '2200000099999', 'identity': '2111925', 'prom_raw': '20100101',
                 'eff_raw': '20100101', 'eff_class': 'valid', 'sha256': None, 'kind': '등기예규'}]

    def build(self, sha=ADM_SHA):
        return queue_build.build(str(self.cache), str(self.ops), str(self.protected), None, '20261117', rows=self.rows(sha))

    def test_pending_new_and_seen(self):
        self.build()
        pending = json.loads((self.ops / 'queue' / 'pending.json').read_text())
        self.assertEqual(len(pending['targets']), 1)
        t = pending['targets'][0]
        self.assertEqual(t['next_promulgation'], '2010-01-01')
        self.assertEqual(t['parser'], {'category': 'parser_category_pending', 'manifest_sha256': None})
        self.assertEqual(t['first_seen'], '20261117')
        self.build()
        self.assertEqual(len(ledger.read_lines(self.ops / 'queue' / 'new-20261117.jsonl')[0]), 1)
        self.assertEqual(len(ledger.read_lines(self.ops / 'queue' / 'seen.jsonl')[0]), 1)

    def test_cut_new_line_is_repaired_and_seen_reconciled(self):
        self.build()
        (self.ops / 'queue' / 'seen.jsonl').write_text('')
        with open(self.ops / 'queue' / 'new-20261117.jsonl', 'a') as fh:
            fh.write('{"key"')
        self.build()
        self.assertEqual(len(ledger.read_lines(self.ops / 'queue' / 'new-20261117.jsonl')[0]), 1)
        self.assertEqual(len(ledger.read_lines(self.ops / 'queue' / 'seen.jsonl')[0]), 1)

    def test_protected_source_change_goes_to_user_confirmation(self):
        self.write_protected([{'dataset': 'admrules', 'revision': ADM, 'identity': '2111925', 'copies': [{'path': 'x', 'source_sha256': '0' * 64}],
                               'decision_id': 'd', 'source': 's', 'expected': {}}])
        self.build()
        pending = json.loads((self.ops / 'queue' / 'pending.json').read_text())
        self.assertEqual(pending['targets'], [])
        self.assertEqual(pending['user_confirmation'][0]['category'], 'protected_source_changed')

    def test_queue_lock_skips_without_waiting(self):
        import fcntl
        (self.ops / 'queue').mkdir(parents=True)
        held = open(self.ops / 'queue' / 'lock', 'w')
        fcntl.flock(held, fcntl.LOCK_EX)
        try:
            self.assertEqual(self.build(), 0)
            self.assertFalse((self.ops / 'queue' / 'pending.json').exists())
        finally:
            held.close()

    def test_queue_ignores_runner_lock(self):
        import fcntl
        self.ops.mkdir(parents=True)
        held = open(self.ops / 'lock', 'w')
        fcntl.flock(held, fcntl.LOCK_EX)
        try:
            self.build()
            self.assertTrue((self.ops / 'queue' / 'pending.json').exists())
        finally:
            held.close()


class ProtectionListTests(unittest.TestCase):
    DATA = REPO / 'git-writer' / 'data' / 'llm-protected-revisions.json'
    DOC = REPO / 'docs' / 'llm-date-judgment.md'

    def test_list_loads_and_holds_the_approval_registry(self):
        protected, revisions, _ = ledger.load_protected(self.DATA)
        for rev in ('1000238', '1011016', '1063983'):
            self.assertIn(('ordinances', rev), protected)
        for rev in ('2200000088451', '2200000104677'):
            self.assertIn(('admrules', rev), protected)

    def test_policy_document_table_matches_data_file(self):
        _, revisions, _ = ledger.load_protected(self.DATA)
        text = self.DOC.read_text(encoding='utf-8')
        section = text.split('<!-- protected-revisions:start -->')[1].split('<!-- protected-revisions:end -->')[0]
        rows = [[c.strip() for c in line.strip().strip('|').split('|')] for line in section.splitlines()
                if re.match(r'^\| \d+ \|', line)]
        expected = [[r['revision'], r['dataset'], r['record_result'] or '', r['decision_id'], r['source']] for r in revisions]
        self.assertEqual(sorted(rows), sorted(expected))


class SourceTextTests(unittest.TestCase):
    def test_amendment_lines_and_entities(self):
        meta, fields = source_text.read_source(FIX / f'{ADM}.xml', 'admrules')
        self.assertEqual(meta['source_sha256'], ADM_SHA)
        self.assertIn('이 예규는 2009. 6. 27.부터 시행한다.', fields['개정문'])
        meta, fields = source_text.read_source(FIX / f'{ORD}.xml', 'ordinances')
        self.assertIn('부칙 <2012. 8. 7 조례 제2409호>', fields['부칙'])
        self.assertNotIn('&lt;', fields['부칙'])


if __name__ == '__main__':
    unittest.main()
