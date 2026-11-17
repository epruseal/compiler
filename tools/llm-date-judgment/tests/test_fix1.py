"""Tests from the post-implementation review of the first commit: counterexamples of the review and of
the surviving mutations. Each test fails when the behaviour it names is removed."""
import json
import os
import shutil
import sys
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))
sys.path.insert(0, str(HERE))
import checks  # noqa: E402
import judge  # noqa: E402
import ledger  # noqa: E402
import queue_build  # noqa: E402
import source_text  # noqa: E402
from test_runner import ADM, ADM_SHA, Env  # noqa: E402

KEY = {'dataset': 'admrules', 'identity': '2111925', 'revision': ADM, 'source_sha256': ADM_SHA}
POLICY = {'model': 'm1', 'effort': 'default', 'prompt_version': 'p', 'runner_version': 'r1'}
FKEY = '|'.join(('admrules', '2111925', ADM, ADM_SHA))


def J(outcome, date, quote, field='부칙', label=''):
    return {'outcome': outcome, 'date': date, 'basis_quote': quote, 'basis_field': field, 'section_label': label,
            'proviso': False, 'proviso_quote': '', 'partial_enforcement': [], 'attribution': '', 'rules_applied': [], 'reason': ''}


def M(prom, dataset='ordinances'):
    return {'dataset': dataset, 'promulgation_date': prom}


def adoptable(j, m, fields, nxt=None):
    return checks.check(j, m, fields, nxt)[0]


class FinalEndingFollower(unittest.TestCase):
    """Contract item 4 (lead-compiler2-check-contract-20261116) and lead-compiler2-line-end-20261116."""

    def reason(self, sentence):
        return adoptable(J('explicit_date', '2012-01-01', sentence, field='제개정이유'), M('2011-12-01'), {'제개정이유': sentence})

    def test_quoted_connective_and_nominal_followers_are_refused(self):
        for verb in ('시행토록 한다는', '시행키로 한다는', '시행하기로 한다면', '시행하였다면', '시행되었다면', '시행한다던',
                     '시행한다며', '시행된다지만', '시행하였다거나', '시행한다니', '시행한다든지', '시행됐다더니',
                     '시행함에', '시행함과', '시행됨으로', '효력이 발생하였다면', '시행합니다만'):
            sentence = f'이 조례는 2012. 1. 1.부터 {verb} 점을 고려한다.'
            self.assertFalse(self.reason(sentence), verb)

    def test_line_end_is_not_a_sentence_end(self):
        for first, rest in (('시행됨', '에 따라 정비함.'), ('시행한다', '는 점을 정한다.'), ('시행한다', '며 정한다.'),
                            ('시행한다', '제2조(경과조치) 종전의 규정에 따른다.')):
            text = f'이 조례는 2012. 1. 1.부터 {first}\n{rest}'
            quote = f'이 조례는 2012. 1. 1.부터 {first}'
            ok = adoptable(J('explicit_date', '2012-01-01', quote, field='제개정이유'), M('2011-12-01'), {'제개정이유': text})
            self.assertFalse(ok, (first, rest))

    def test_sentence_and_clause_marks_and_field_end_are_accepted(self):
        for tail in ('.', ', 다만 제2조는 따로 정한다.', ''):
            sentence = f'이 조례는 2012. 1. 1.부터 시행한다{tail}'
            self.assertTrue(self.reason(sentence), tail)
        for verb in ('시행토록 한다.', '시행키로 한다.', '시행하였다.', '시행함.'):
            self.assertTrue(self.reason(f'이 조례는 2012. 1. 1.부터 {verb}'), verb)
        text = '(이 조례는 2012. 1. 1.부터 시행한다)'
        self.assertTrue(adoptable(J('explicit_date', '2012-01-01', '이 조례는 2012. 1. 1.부터 시행한다', field='제개정이유'),
                                  M('2011-12-01'), {'제개정이유': text}))

    def test_causative_nominal_is_refused(self):
        self.assertFalse(self.reason('이 조례는 2012. 1. 1.부터 시행시킴에 따라 종전 규칙을 폐지한다.'))
        self.assertTrue(self.reason('이 조례는 2012. 1. 1.부터 시행시킴.'))

    def test_lone_carriage_return_splits_lines(self):
        text = '부칙 <2011.12.29>\r이 규칙은 2012년 1월 10일부터 시행한다.\r부칙 <2012.03.08>\r이 규칙은 공포한 날부터 시행한다.'
        ok = adoptable(J('explicit_date', '2012-01-10', '이 규칙은 2012년 1월 10일부터 시행한다.', label='<2012.03.08>'), M('2012-03-08'), {'부칙': text})
        self.assertFalse(ok)
        ok = adoptable(J('explicit_date', '2012-01-10', '이 규칙은 2012년 1월 10일부터 시행한다.', label='<2011.12.29>'), M('2011-12-29'), {'부칙': text})
        self.assertTrue(ok)

    def test_carriage_return_lines(self):
        text = '부칙\r\n제2조 중 “이 조례는\r\n2012. 1. 1.부터 시행한다”를 삭제한다.'
        self.assertFalse(adoptable(J('explicit_date', '2012-01-01', '2012. 1. 1.부터 시행한다'), M('2011-12-01'), {'부칙': text}))


class SurvivingMutations(unittest.TestCase):
    """Counterexamples of the review execution channel for the surviving mutations."""

    def test_quotation_tracking(self):
        cases = [
            (J('promulgation_day', '2010-01-05', '제3조"는 삭제하고 이 조례는 공포한 날부터 시행한다.'), M('2010-01-05'),
             {'부칙': '부칙\n종전의 "제3조"는 삭제하고 이 조례는 공포한 날부터 시행한다.'}),
            (J('explicit_date', '2012-01-01', '이 조례는 "2012. 1. 1.부터 시행한다.'), M('2011-12-01'),
             {'부칙': '부칙\n이 조례는\n"2012. 1. 1.부터\n시행한다.'}),
            (J('explicit_date', '2012-01-01', '이 조례는 2012. 1. 1.부터 시행한다'), M('2011-12-01'),
             {'부칙': '부칙\n이 조례는 2012. 1. 1.부터\n시행한다"를 삭제함'}),
            (J('explicit_date', '2012-01-01', '2012. 1. 1.부터 시행한다'), M('2011-12-01'),
             {'부칙': '부칙\n제2조 중 “이 조례는\n2012. 1. 1.부터 시행한다”를 삭제한다.'}),
            (J('explicit_date', '2012-01-01', '이 조례는 2012. 1. 1.부터 시행한다.'), M('2011-12-01'),
             {'부칙': '부칙\n이 조례는 2012. 1. 1.부터 시행한다. 제2조 중 “가'}),
            (J('promulgation_day', '2024-03-01', '이 조례는 공포한 날부터 시행한다', field='제개정이유'), M('2024-03-01'),
             {'제개정이유': '부칙 제2조 중 “이 조례는 공포한 날부터 시행한다”를 삭제함.'}),
        ]
        for j, m, f in cases:
            self.assertFalse(adoptable(j, m, f), f)

    def test_calendar(self):
        for d, written in (('2009-02-30', '2009. 2. 30.'), ('9999-12-31', '9999. 12. 31.')):
            q = f'이 조례는 {written}부터 시행한다.'
            self.assertFalse(adoptable(J('explicit_date', d, q), M('2009-01-01'), {'부칙': '부칙\n' + q}))

    def test_verb_stem_inside_quote(self):
        self.assertFalse(adoptable(J('explicit_date', '2012-01-01', '이 조례는 2012. 1. 1.부터 시'), M('2011-12-01'),
                                   {'부칙': '부칙\n이 조례는 2012. 1. 1.부터 시행한다.'}))

    def test_label_and_promulgation_dates(self):
        self.assertFalse(adoptable(J('label_date', '2006-07-15', '이 조례는 공포한 날부터 시행한다.', label='(2006.6.28)'), M('2006-06-20'),
                                   {'부칙': '부 칙 (2006.6.28)\n이 조례는 공포한 날부터 시행한다.'}))
        self.assertFalse(adoptable(J('label_date', '2006-06-10', '이 조례는 공포한 날부터 시행한다.', label='(2006.6.10)'), M('2006-06-20'),
                                   {'부칙': '부 칙 (2006.6.10)\n이 조례는 공포한 날부터 시행한다.'}))
        self.assertFalse(adoptable(J('promulgation_day', '2010-02-01', '이 조례는 공포한 날부터 시행한다.'), M('2010-01-05'),
                                   {'부칙': '부칙\n이 조례는 공포한 날부터 시행한다.'}))

    def test_sections(self):
        s3 = ('부칙 <2011.12.29>\n종전 규칙 <2012.03.08> 참조.\n이 규칙은 공포한 날부터 시행한다.\n'
              '부칙 <2012.03.08>\n제1조(시행일) 이 규칙은 2012년 5월 1일부터 시행한다.')
        self.assertFalse(adoptable(J('promulgation_day', '2012-03-08', '이 규칙은 공포한 날부터 시행한다.', label='<2012.03.08>'),
                                   M('2012-03-08'), {'부칙': s3}))
        s4 = '부칙 <2011.12.29>\n이 규칙은 2012년 1월 10일부터 시행한다.\n부칙 <2012.03.08>\n이 규칙은 공포한 날부터 시행한다.'
        self.assertFalse(adoptable(J('explicit_date', '2012-01-10', '이 규칙은 2012년 1월 10일부터 시행한다. 부칙 <2012.03.08>',
                                     label='<2012.03.08>'), M('2012-03-08'), {'부칙': s4}))

    def test_known_limit_previous_revision_addendum(self):
        """Known limit (followup stale-addendum-attribution, gold 2200000095587): with one heading the
        checks cannot tell that an explicit date belongs to an earlier revision's addendum. The LLM
        decides; accuracy is measured."""
        text = '부칙\n이 예규는 2008. 3. 1.부터 시행한다.'
        self.assertTrue(adoptable(J('explicit_date', '2008-03-01', '이 예규는 2008. 3. 1.부터 시행한다.'), M('2009-05-01', 'admrules'), {'부칙': text}))


class LedgerFixes(Env):
    def entry(self, judged_at, runner='r1', sha=ADM_SHA):
        return {'key': dict(KEY, source_sha256=sha), 'kind': 'judged', 'judged_at': judged_at, 'runner_version': runner,
                'model': 'm1', 'effort': 'default', 'prompt_version': 'p', 'adoptable': True, 'date': '2009-06-27'}

    def history(self):
        return ledger.read_lines(self.ops / 'ledger-history' / 'admrules.jsonl')[0]

    def test_cut_inside_a_multibyte_character(self):
        led = ledger.Ledger(self.ops, 'admrules')
        led.add(self.entry('t1'))
        with open(led.journal, 'ab') as fh:
            fh.write('{"key": {"dataset": "admrules"}, "reason": "'.encode() + b'\xea\xb3')
        report = led.compact(POLICY, {}, None)
        self.assertTrue(report['journal_cut_line'])
        self.assertEqual(len(led.load()['entries']), 1)
        path = self.tmp / 'q.jsonl'
        path.write_bytes(b'{"a": 1}\n{"b": "\xea\xb3')
        self.assertEqual(ledger.repair(path), ([{'a': 1}], True))
        self.assertEqual(path.read_bytes(), b'{"a": 1}\n')

    def test_history_cut_line_is_repaired_before_append(self):
        led = ledger.Ledger(self.ops, 'admrules')
        led.add(self.entry('t1'))
        led.compact(POLICY, {}, None)
        led.history.parent.mkdir(parents=True, exist_ok=True)
        led.history.write_bytes(b'{"cut": "\xea')
        led.add(self.entry('t2'))
        led.compact(POLICY, {}, None)
        self.assertEqual([h['judged_at'] for h in self.history()], ['t1'])

    def test_history_once_after_stop_before_rename(self):
        led = ledger.Ledger(self.ops, 'admrules')
        led.add(self.entry('t1'))
        led.compact(POLICY, {}, None)
        led.add(self.entry('t2'))
        original = ledger.atomic_write

        def stop(path, text):
            if Path(path) == led.path:
                raise OSError('stop before rename')
            return original(path, text)
        ledger.atomic_write = stop
        try:
            with self.assertRaises(OSError):
                led.compact(POLICY, {}, None)
        finally:
            ledger.atomic_write = original
        led.compact(POLICY, {}, None)
        self.assertEqual(len(self.history()), 1)

    def test_protected_judged_entry_becomes_excluded_and_goes_to_history(self):
        led = ledger.Ledger(self.ops, 'admrules')
        led.add(self.entry('t1'))
        led.compact(POLICY, {}, None)
        led.compact(POLICY, {('admrules', ADM): {ADM_SHA}}, 'list')
        self.assertEqual([e['kind'] for e in led.load()['entries']], ['excluded'])
        self.assertEqual([h['kind'] for h in self.history()], ['judged'])

    def test_same_second_judgment_with_new_content_replaces(self):
        led = ledger.Ledger(self.ops, 'admrules')
        led.add(self.entry('2026-01-01T00:00:00Z'))
        led.compact(POLICY, {}, None)
        led.add(dict(self.entry('2026-01-01T00:00:00Z'), date='2009-07-01'))
        led.compact(POLICY, {}, None)
        self.assertEqual(led.load()['entries'][0]['date'], '2009-07-01')
        self.assertEqual([h['date'] for h in self.history()], ['2009-06-27'])
        led.add(dict(self.entry('2026-01-01T00:00:00Z'), date='2009-07-01'))
        led.compact(POLICY, {}, None)
        self.assertEqual(len(self.history()), 1)

    def test_two_rechecks_keep_both_in_history(self):
        led = ledger.Ledger(self.ops, 'admrules')
        led.add(self.entry('t1', runner='r0'))
        led.compact(dict(POLICY, runner_version='r0'), {}, None)
        led.compact(POLICY, {}, None, lambda e: dict(e, runner_version='r1'))
        led.compact(dict(POLICY, runner_version='r2'), {}, None, lambda e: dict(e, runner_version='r2'))
        self.assertEqual([h['runner_version'] for h in self.history()], ['r0', 'r1'])

    def test_unsorted_or_duplicate_canonical_is_refused(self):
        path = self.ops / 'ledger' / 'admrules.json'
        path.parent.mkdir(parents=True)
        a, b = self.entry('t', sha='a' * 64), self.entry('t', sha='b' * 64)
        for ents in ([b, a], [a, a]):
            path.write_text(json.dumps({'schema_version': 1, 'dataset': 'admrules', 'policy': None, 'protected_list_sha256': None,
                                        'entries': ents}))
            with self.assertRaises(ledger.LedgerError):
                ledger.Ledger(self.ops, 'admrules').load()

    def test_canonical_bytes_are_sorted_and_stable(self):
        led = ledger.Ledger(self.ops, 'admrules')
        led.add(self.entry('t1'))
        led.compact(POLICY, {}, None)
        first = led.path.read_bytes()
        self.assertEqual(first.decode('utf-8'), ledger.dumps(json.loads(first)))
        order = [first.index(f'"{k}"'.encode()) for k in ('dataset', 'entries', 'policy', 'protected_list_sha256', 'schema_version')]
        self.assertEqual(order, sorted(order))
        led.compact(POLICY, {}, None)
        self.assertEqual(led.path.read_bytes(), first)

    def test_runner_change_rechecks_entry(self):
        led = ledger.Ledger(self.ops, 'admrules')
        led.add(self.entry('t1', runner='r0'))
        led.compact(dict(POLICY, runner_version='r0'), {}, None)
        led.compact(POLICY, {}, None, lambda e: dict(e, runner_version='r1', rechecked=True))
        e = led.load()['entries'][0]
        self.assertEqual((e['runner_version'], e.get('rechecked')), ('r1', True))
        self.assertEqual(len(self.history()), 1)

    def test_recheck_sets_cross_check_again(self):
        self.run_judge()
        e = self.canonical()['entries'][0]
        self.assertEqual(e['cross_check_at_judgment'], 'disagree')
        e['judgment']['basis_quote'] = '이 문장은 원문에 없다'
        fixed = judge.recheck_factory(str(self.cache))(e)
        self.assertFalse(fixed['adoptable'])
        self.assertEqual(fixed['cross_check_at_judgment'], 'inconclusive')

    def test_protection_list_with_a_revision_twice_is_refused(self):
        rev = {'dataset': 'admrules', 'revision': ADM, 'identity': '2111925', 'copies': [{'path': 'x', 'source_sha256': ADM_SHA}],
               'decision_id': 'd', 'source': 's', 'expected': {}}
        self.write_protected([rev, dict(rev, copies=[{'path': 'y', 'source_sha256': 'b' * 64}])])
        with self.assertRaises(ledger.LedgerError):
            ledger.load_protected(self.protected)


SEQ_FAKE = '''#!/usr/bin/env python3
import json, os, sys
sys.stdin.read()
mode = os.environ.get('FAKE_MODE', 'ok')
if mode == 'seq':
    counter = os.environ['FAKE_COUNTER']
    n = int(open(counter).read()) if os.path.exists(counter) else 0
    open(counter, 'w').write(str(n + 1))
    seq = os.environ['FAKE_SEQ'].split(',')
    mode = seq[n] if n < len(seq) else seq[-1]
if 'env' in os.environ.get('FAKE_RECORD', ''):
    open(os.environ['FAKE_RECORD'], 'w').write(json.dumps(sorted(os.environ)))
out = {"outcome": "explicit_date", "date": "2009-06-27", "basis_quote": "이 예규는 2009. 6. 27.부터 시행한다.",
       "basis_field": "개정문", "section_label": "부  칙", "proviso": False, "proviso_quote": "",
       "partial_enforcement": [{"quote": "a\\x07b\\nc", "date": "", "scope": ""}], "attribution": "a", "rules_applied": ["R3"],
       "reason": "r\\x00s"}
if mode == 'exit1':
    print(json.dumps({"is_error": True, "result": "Prompt is too long"})); sys.exit(1)
if mode == 'ok_exit1':
    print(json.dumps({"is_error": False, "structured_output": out, "usage": {}})); sys.exit(1)
if mode == 'iserr_struct':
    print(json.dumps({"is_error": True, "structured_output": out, "result": "error after output", "usage": {}})); sys.exit(0)
if mode == 'error':
    print(json.dumps({"is_error": True, "result": "usage limit"})); sys.exit(0)
print(json.dumps({"is_error": False, "structured_output": out, "usage": {"input_tokens": 1}, "total_cost_usd": 0.01}))
'''


class RunnerFixes(Env):
    def setUp(self):
        super().setUp()
        self.fake.write_text(SEQ_FAKE, encoding='utf-8')
        self.saved_runner = judge.RUNNER_VERSION
        for k in ('FAKE_SEQ', 'FAKE_COUNTER', 'FAKE_RECORD'):
            os.environ.pop(k, None)

    def tearDown(self):
        judge.RUNNER_VERSION = self.saved_runner
        os.environ.pop('ANTHROPIC_API_KEY', None)
        super().tearDown()

    def copies(self, n):
        data = (self.cache / 'admrule' / f'{ADM}.xml').read_bytes()
        out = []
        for i in range(n):
            p = self.cache / 'admrule' / f'copy{i}.xml'
            p.write_bytes(data + b'\n' * (i + 1))
            out.append(dict(self.target(), key=dict(KEY, source_sha256=ledger.file_sha256(p)), path=f'admrule/copy{i}.xml'))
        return out

    def set_targets(self, targets):
        self.targets.write_text(json.dumps({'targets': targets}), encoding='utf-8')

    def run_report(self, *extra, model='m1', max_calls=10):
        import contextlib
        import io
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            code = judge.main(['run', '--dataset', 'admrules', '--model', model, '--effort', 'default', '--claude', str(self.fake),
                               '--ops', str(self.ops), '--cache', str(self.cache), '--targets', str(self.targets),
                               '--protected', str(self.protected), '--max-calls', str(max_calls), *extra])
        lines = [json.loads(x) for x in buf.getvalue().splitlines() if x.strip()]
        return code, lines

    def calls_logged(self):
        p = self.ops / 'calls.log'
        return p.read_text().count('\n') if p.exists() else 0

    def test_default_limits(self):
        self.assertEqual((judge.STOP_AFTER_FAILURES, judge.TARGET_FAILURE_LIMIT), (5, 3))
        self.set_targets(self.copies(7))
        os.environ['FAKE_MODE'] = 'exit1'
        code, lines = self.run_report()
        self.assertEqual((code, lines[-1]['calls']), (3, 5))
        self.assertIn('Prompt is too long', lines[-1]['stopped'])

    def test_target_limit_is_three_failures(self):
        os.environ['FAKE_MODE'] = 'exit1'
        runs = [self.run_report()[1][-1] for _ in range(4)]
        self.assertEqual([r['calls'] for r in runs], [1, 1, 1, 0])
        self.assertEqual(runs[-1]['skipped_failed_keys'], [FKEY])

    def test_same_stop_three_times_skips_and_records_failed(self):
        """lead-compiler2-a2-a3-20261117: five targets that always fail stop every run; the first stop
        is forgiven, the same stop again counts, and after the third same stop the targets are skipped,
        recorded as failed and the run reaches the next target."""
        bad = self.copies(5)
        good = self.target()
        self.set_targets(bad + [good])
        os.environ.update({'FAKE_MODE': 'seq', 'FAKE_SEQ': 'exit1', 'FAKE_COUNTER': str(self.tmp / 'n')})
        stops = []
        for _ in range(3):
            code, lines = self.run_report()
            stops.append((code, lines[-1]['calls'], sorted(lines[-1]['stop_keys'])))
        bad_keys = sorted('|'.join(t['key'][f] for f in ledger.KEY_FIELDS) for t in bad)
        self.assertEqual(stops, [(3, 5, bad_keys)] * 3)
        fails = json.loads(next((self.ops / 'failures').rglob('failures-admrules.json')).read_text())
        self.assertEqual(sorted(fails['counts'].values()), [3] * 5)
        os.environ['FAKE_MODE'] = 'ok'
        code, lines = self.run_report()
        self.assertEqual((code, lines[-1]['calls'], lines[-1]['skipped_failed']), (0, 1, 5))
        self.assertEqual(sorted(lines[-1]['skipped_failed_keys']), bad_keys)
        kinds = sorted(e['kind'] for e in self.canonical()['entries'])
        self.assertEqual(kinds, ['failed'] * 5 + ['judged'])
        failed = [e for e in self.canonical()['entries'] if e['kind'] == 'failed'][0]
        self.assertEqual((failed['reason'], failed['failures']), ('지속 실패', 3))
        self.assertIn('Prompt is too long', failed['last_error'])
        self.assertNotIn('date', failed)
        before = len(ledger.read_lines(self.ops / 'ledger-history' / 'admrules.jsonl')[0])
        self.run_report()
        self.assertEqual(len(ledger.read_lines(self.ops / 'ledger-history' / 'admrules.jsonl')[0]), before)
        code, lines = self.run_report('--retry-failed')
        self.assertEqual(lines[-1]['calls'], 5)
        self.assertEqual(sorted(e['kind'] for e in self.canonical()['entries']), ['judged'] * 6)

    def test_a_different_stop_is_forgiven(self):
        first, second = self.copies(5), self.copies(10)[5:]
        os.environ['FAKE_MODE'] = 'exit1'
        self.set_targets(first)
        self.run_report()
        self.set_targets(second)
        self.run_report()
        fails = json.loads(next((self.ops / 'failures').rglob('failures-admrules.json')).read_text())
        self.assertEqual(set(fails['counts'].values()), {0})

    def test_new_policy_tries_failed_targets_again(self):
        judge.TARGET_FAILURE_LIMIT = 1
        os.environ['FAKE_MODE'] = 'exit1'
        self.run_report()
        os.environ['FAKE_MODE'] = 'ok'
        code, lines = self.run_report()
        self.assertEqual(lines[-1]['skipped_failed'], 1)
        code, lines = self.run_report('--replace-policy', model='m2')
        self.assertEqual(lines[-1]['calls'], 1)
        self.assertEqual(self.canonical()['entries'][0]['kind'], 'judged')

    def test_success_resets_the_run_of_failures(self):
        judge.STOP_AFTER_FAILURES = 2
        self.set_targets(self.copies(3))
        os.environ.update({'FAKE_MODE': 'seq', 'FAKE_SEQ': 'exit1,ok,exit1', 'FAKE_COUNTER': str(self.tmp / 'n')})
        code, lines = self.run_report()
        self.assertEqual((code, lines[-1]['calls'], lines[-1]['stopped']), (0, 3, None))

    def test_max_calls(self):
        self.set_targets(self.copies(2))
        code, lines = self.run_report(max_calls=0)
        self.assertEqual((lines[-1]['calls'], lines[-1]['stopped'], self.calls_logged()), (0, 'max-calls', 0))
        code, lines = self.run_report(max_calls=1)
        self.assertEqual((lines[-1]['calls'], lines[-1]['stopped'], self.calls_logged()), (1, 'max-calls', 1))

    def test_nonzero_exit_or_is_error_gives_no_judgment(self):
        for mode in ('ok_exit1', 'iserr_struct'):
            shutil.rmtree(self.ops, ignore_errors=True)
            os.environ['FAKE_MODE'] = mode
            self.run_report()
            self.assertEqual(self.canonical()['entries'], [], mode)
            os.environ['FAKE_MODE'] = 'error'
            code, lines = self.run_report(max_calls=0)
            self.assertEqual((lines[-1]['restored'], self.canonical()['entries']), (0, []), mode)

    def test_restore_ignores_runner_but_checks_call_args(self):
        self.run_report()
        shutil.rmtree(self.ops / 'ledger')
        judge.RUNNER_VERSION = 'r9'
        code, lines = self.run_report(max_calls=0)
        self.assertEqual(lines[-1]['restored'], 1)
        shutil.rmtree(self.ops / 'ledger')
        args_file = next((self.ops / 'raw').rglob('call-args.json'))
        args = json.loads(args_file.read_text())
        args_file.write_text(json.dumps(dict(args, model='other')))
        code, lines = self.run_report(max_calls=0)
        self.assertEqual((lines[-1]['restored'], lines[-1]['calls']), (0, 0))

    def test_restored_entry_keeps_call_time_inputs(self):
        self.set_targets([dict(self.target(), next_promulgation='2010-01-01')])
        self.run_report()
        shutil.rmtree(self.ops / 'ledger')
        self.set_targets([self.target()])
        self.run_report(max_calls=0)
        e = self.canonical()['entries'][0]
        self.assertEqual(e['inputs']['next_promulgation'], '2010-01-01')
        self.assertIsNotNone(e['usage']['seconds'])
        self.assertEqual(e['usage']['total_cost_usd'], 0.01)

    def test_old_runner_entry_that_fails_recheck_is_judged_again(self):
        """fix1-design-1 blocking 2: an entry of an old runner whose recheck cannot read its source path
        is judged again, not restored from its own raw response."""
        self.run_report()
        judge.RUNNER_VERSION = 'r9'
        moved = self.cache / 'admrule' / 'moved.xml'
        (self.cache / 'admrule' / f'{ADM}.xml').rename(moved)
        self.set_targets([dict(self.target(), path='admrule/moved.xml')])
        code, lines = self.run_report()
        self.assertEqual((lines[-1]['calls'], lines[-1]['restored']), (1, 0))
        self.assertEqual(self.canonical()['entries'][0]['runner_version'], 'r9')
        code, lines = self.run_report()
        self.assertEqual((lines[-1]['calls'], lines[-1]['skipped_done']), (0, 1))

    def test_policy_change_needs_explicit_argument(self):
        self.run_report()
        code, lines = self.run_report(model='m2')
        self.assertEqual(code, 4)
        self.assertEqual(lines[-1]['ledger_policy']['model'], 'm1')
        self.assertEqual(self.calls_logged(), 1)
        code, lines = self.run_report('--replace-policy', model='m2')
        self.assertEqual(lines[0]['replace_policy']['targets_to_judge'], 1)
        self.assertEqual((code, lines[-1]['calls']), (0, 1))
        self.assertEqual(self.canonical()['policy']['model'], 'm2')

    def test_paid_api_key_is_refused(self):
        os.environ['ANTHROPIC_API_KEY'] = 'x'
        code, lines = self.run_report()
        self.assertEqual(code, 2)
        self.assertFalse(self.ops.exists())
        source = str(self.cache / 'admrule' / f'{ADM}.xml')
        code = judge.main(['one', '--dataset', 'admrules', '--model', 'm1', '--claude', str(self.fake), '--source', source,
                           '--workdir', str(self.tmp)])
        self.assertEqual(code, 2)
        with self.assertRaises(judge.PaidKeyError):
            judge.judge_file('admrules', source, 'm1', 'default', None, self.tmp / 'one', claude_bin=str(self.fake))
        self.assertFalse((self.tmp / 'one').exists())
        with self.assertRaises(judge.PaidKeyError):
            judge.call_cli(str(self.fake), '', '{}', 'm1', 'default', self.tmp, 10)

    def test_cut_journal_is_reported_and_kept_entries_survive(self):
        self.write_protected([{'dataset': 'admrules', 'revision': ADM, 'identity': '2111925', 'copies': [{'path': 'x', 'source_sha256': ADM_SHA}],
                               'decision_id': 'd', 'source': 's', 'expected': {}}])
        journal = self.ops / 'ledger' / 'admrules.journal.jsonl'
        journal.parent.mkdir(parents=True)
        journal.write_bytes('{"key": {"dataset": "admrules"}, "reason": "'.encode() + b'\xea\xb3')
        code, lines = self.run_report()
        self.assertTrue(lines[-1]['journal_cut_line_at_start'])
        self.assertEqual([e['kind'] for e in self.canonical()['entries']], ['excluded'])
        code, lines = self.run_report()
        self.assertFalse(lines[-1]['journal_cut_line_at_start'])

    def test_source_change_and_missing_source_are_both_listed(self):
        changed = self.cache / 'admrule' / 'changed.xml'
        changed.write_bytes((self.cache / 'admrule' / f'{ADM}.xml').read_bytes() + b'\n')
        self.set_targets([dict(self.target(), path='admrule/missing.xml'), dict(self.target(), path='admrule/changed.xml')])
        code, lines = self.run_report()
        self.assertEqual((lines[-1]['skipped_source_changed'], lines[-1]['skipped_source_changed_keys']), (2, [FKEY, FKEY]))
        self.assertEqual(lines[-1]['calls'], 0)

    def test_replace_policy_counts_exactly_the_keys_to_judge(self):
        done, fresh = self.copies(2)
        protected_key = dict(KEY, revision='2200000000001')
        other = dict(self.target(), key=dict(KEY, dataset='ordinances'))
        self.write_protected([{'dataset': 'admrules', 'revision': '2200000000001', 'identity': '2111925',
                               'copies': [{'path': 'x', 'source_sha256': ADM_SHA}], 'decision_id': 'd', 'source': 's', 'expected': {}}])
        self.set_targets([done])
        self.run_report()
        self.set_targets([done, fresh, dict(self.target(), key=protected_key), other])
        code, lines = self.run_report('--replace-policy', model='m2', max_calls=0)
        self.assertEqual(lines[0]['replace_policy']['targets_to_judge'], 2)
        code, lines = self.run_report('--replace-policy', model='m2', max_calls=1)
        code, lines = self.run_report(model='m2', max_calls=0)
        self.assertEqual(code, 0)

    def test_restored_entry_keeps_call_time_parser(self):
        self.run_report()
        shutil.rmtree(self.ops / 'ledger')
        self.set_targets([self.target(parser={'category': 'judge'})])
        self.run_report(max_calls=0)
        e = self.canonical()['entries'][0]
        self.assertEqual(e['parser_at_judgment']['category'], 'cross-arrival')
        self.assertEqual(e['cross_check_at_judgment'], 'disagree')

    def test_gateway_environment_is_passed_on(self):
        """user-compiler2-shunt-path-20261116: the runner keeps ANTHROPIC_BASE_URL for the CLI."""
        record = self.tmp / 'env.json'
        os.environ.update({'FAKE_RECORD': str(record) + '-env', 'ANTHROPIC_BASE_URL': 'http://gateway.invalid'})
        try:
            self.run_report()
        finally:
            os.environ.pop('ANTHROPIC_BASE_URL')
        self.assertIn('ANTHROPIC_BASE_URL', json.loads(Path(str(record) + '-env').read_text()))

    def test_report_lists_skipped_source_changes_and_control_characters(self):
        self.set_targets([dict(self.target(), path='admrule/missing.xml'), self.target()])
        code, lines = self.run_report()
        self.assertEqual(lines[-1]['skipped_source_changed_keys'], [FKEY])
        e = self.canonical()['entries'][0]
        self.assertEqual(e['judgment']['reason'], 'r s')
        self.assertEqual(e['judgment']['partial_enforcement'][0]['quote'], 'a b c')
        self.assertEqual(e['usage']['total_cost_usd'], 0.01)

    def test_failures_are_per_policy_and_temporary_files_go(self):
        for folder in ('ledger-history', 'failures/a/b'):
            (self.ops / folder).mkdir(parents=True)
            (self.ops / folder / '.tmp-x').write_text('')
        os.environ['FAKE_MODE'] = 'exit1'
        self.run_report()
        self.assertEqual(list(self.ops.rglob('.tmp-*')), [])
        fails = list((self.ops / 'failures').rglob('failures-admrules.json'))
        self.assertEqual(len(fails), 1)
        self.assertEqual(fails[0].parent.name, 'default')
        self.assertEqual(fails[0].parent.parent.name, 'm1')


class QueueFixes(Env):
    def rows(self, path=None, sha=ADM_SHA, eff_class='sentinel_99991231'):
        return {'dataset': 'admrules', 'path': str(path or self.cache / 'admrule' / f'{ADM}.xml'), 'serial': ADM, 'identity': '2111925',
                'prom_raw': '20090410', 'eff_raw': '99991231', 'eff_class': eff_class, 'sha256': sha,
                'text_blank': False, 'html': False, 'kind': '등기예규'}

    def build(self, day, rows):
        import contextlib
        import io
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            queue_build.build(str(self.cache), str(self.ops), str(self.protected), None, day, rows=rows)
        return json.loads(buf.getvalue().strip().splitlines()[-1])

    def lines(self, name):
        return ledger.read_lines(self.ops / 'queue' / name)[0]

    def stop_between_appends(self, day, rows):
        original = ledger.append_line
        state = {'n': 0}

        def patched(path, obj):
            state['n'] += 1
            if state['n'] == 2:
                raise OSError('stop between new and seen')
            return original(path, obj)
        ledger.append_line = patched
        try:
            with self.assertRaises(OSError):
                self.build(day, rows)
        finally:
            ledger.append_line = original

    def test_new_is_written_before_seen(self):
        self.stop_between_appends('20261117', [self.rows()])
        self.assertEqual((len(self.lines('new-20261117.jsonl')), len(self.lines('seen.jsonl'))), (1, 0))
        self.build('20261117', [self.rows()])
        self.assertEqual((len(self.lines('new-20261117.jsonl')), len(self.lines('seen.jsonl'))), (1, 1))

    def test_stop_then_next_day_keeps_first_day(self):
        self.stop_between_appends('20261117', [self.rows()])
        self.build('20261118', [self.rows()])
        self.assertFalse((self.ops / 'queue' / 'new-20261118.jsonl').exists())
        self.assertEqual([s['first_seen'] for s in self.lines('seen.jsonl')], ['20261117'])
        pending = json.loads((self.ops / 'queue' / 'pending.json').read_text())
        self.assertEqual(pending['targets'][0]['first_seen'], '20261117')

    def test_protected_valid_copy_is_hashed(self):
        self.write_protected([{'dataset': 'admrules', 'revision': ADM, 'identity': '2111925', 'copies': [{'path': 'x', 'source_sha256': ADM_SHA}],
                               'decision_id': 'd', 'source': 's', 'expected': {}}])
        out = self.build('20261117', [self.rows(sha=None, eff_class='valid')])
        self.assertEqual((out['user_confirmation'], out['targets']), (0, 0))

    def test_protected_valid_copy_is_not_stale(self):
        self.write_protected([{'dataset': 'admrules', 'revision': ADM, 'identity': '2111925', 'copies': [{'path': 'x', 'source_sha256': ADM_SHA}],
                               'decision_id': 'd', 'source': 's', 'expected': {}}])
        led = ledger.Ledger(self.ops, 'admrules')
        protected, revisions, _ = ledger.load_protected(self.protected)
        led.protect(protected, revisions)
        led.compact(POLICY, protected, None)
        out = self.build('20261117', [self.rows(sha=None, eff_class='valid')])
        self.assertEqual(out['stale_entries'], {'ordinances': 0, 'admrules': 0})

    def test_same_day_rerun_leaves_no_cut_line(self):
        copy = self.cache / 'admrule' / 'other.xml'
        copy.write_bytes((self.cache / 'admrule' / f'{ADM}.xml').read_bytes() + b'\n')
        other = dict(self.rows(path=copy), sha256=ledger.file_sha256(copy), serial='2200000000002')
        self.build('20261117', [self.rows()])
        with open(self.ops / 'queue' / 'new-20261117.jsonl', 'ab') as fh:
            fh.write(b'{"key": {"dataset": "adm')
        self.build('20261117', [self.rows(), other])
        lines, cut = ledger.read_lines(self.ops / 'queue' / 'new-20261117.jsonl')
        self.assertFalse(cut)
        self.assertEqual(len(lines), 2)

    def test_byte_equal_copies_give_one_target(self):
        copy = self.cache / 'admrule' / 'history-copy.xml'
        shutil.copy(self.cache / 'admrule' / f'{ADM}.xml', copy)
        out = self.build('20261117', [self.rows(), self.rows(path=copy)])
        self.assertEqual((out['targets'], out['new']), (1, 1))
        pending = json.loads((self.ops / 'queue' / 'pending.json').read_text())
        self.assertEqual(pending['targets'][0]['other_paths'], ['admrule/history-copy.xml'])

    def test_stale_entries_are_counted(self):
        led = ledger.Ledger(self.ops, 'admrules')
        led.add({'key': dict(KEY, source_sha256='c' * 64), 'kind': 'judged', 'judged_at': 't'})
        led.add({'key': dict(KEY), 'kind': 'judged', 'judged_at': 't'})
        led.compact(POLICY, {}, None)
        out = self.build('20261117', [self.rows()])
        self.assertEqual(out['stale_entries'], {'ordinances': 0, 'admrules': 1})
        pending = json.loads((self.ops / 'queue' / 'pending.json').read_text())
        self.assertEqual(pending['stale_entries'], {'ordinances': 0, 'admrules': 1})

    def test_manifest_sha256_in_parser_record(self):
        manifest = self.tmp / 'm.json'
        manifest.write_text(json.dumps({'date_index': {'dataset': 'admrules', 'revisions': [
            {'identity': '2111925', 'revision': ADM, 'copy_id': ADM_SHA, 'status': 'estimated', 'enforcement_date': '2009-04-10',
             'date_evidence': {'rule': 'x'}}]}}))
        parsers = queue_build.parser_map([str(manifest)])
        self.assertEqual(parsers[('admrules', '2111925', ADM, ADM_SHA)]['manifest_sha256'], ledger.file_sha256(manifest))


class AddendaExcerpt(unittest.TestCase):
    """User decision 2026-11-16: the excerpt keeps whole addendum sections with their headings."""

    def test_fallback_drops_whole_sections(self):
        saved = source_text.ADDENDA_LIMIT
        source_text.ADDENDA_LIMIT = 200
        try:
            sections = [f'부칙 <20{i:02d}. 1. 1.>\n' + '가' * 60 + f'\n이 조례는 20{i:02d}. 1. 1.부터 시행한다.' for i in range(10)]
            out, short = source_text.addenda_for_prompt('\n'.join(sections), '20090101', '')
            self.assertTrue(short)
            self.assertLessEqual(len(out), 200 + 10)
            for part in out.split('…'):
                part = part.strip('\n')
                if part:
                    self.assertTrue(part.startswith('부칙'), part[:20])
            long_one = '부칙 <2009. 1. 1.>\n' + '나' * 500 + '\n이 조례는 2009. 1. 1.부터 시행한다.'
            out, _ = source_text.addenda_for_prompt(long_one, '20090101', '')
            self.assertIn('부칙 <2009. 1. 1.>', out)
            self.assertTrue(out.endswith('시행한다.'))
            self.assertLessEqual(len(out), 200)
            long_head = '부칙 ' + '다' * 400 + '\n본문'
            out, _ = source_text.addenda_for_prompt(long_head, '20090101', '')
            self.assertLessEqual(len(out), 200)
            self.assertTrue(out.endswith('본문'))
            sections = '부칙 <2001>\r' + '가' * 250 + '\r부칙 <2002>\r이 조례는 시행한다.'
            out, _ = source_text.addenda_for_prompt(sections, '20020101', '')
            self.assertTrue(out.startswith('…\n부칙 <2002>'), out[:30])
        finally:
            source_text.ADDENDA_LIMIT = saved


if __name__ == '__main__':
    unittest.main()
