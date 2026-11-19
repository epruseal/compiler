"""LLM judge of general effective dates (compiler#2). The compiler never runs this file.

Commands:
  one  judge one source file and print the entry (measurement and debugging; writes no ledger)
  run  judge the targets of a queue file into the ledger of an operations folder

run refuses to start when ANTHROPIC_API_KEY is set, takes the operations lock, refuses a model,
effort or prompt that differs from the ledger policy unless --replace-policy is given, compacts the
ledger, skips protected revisions and keys that hold a current-policy entry, restores an entry from a
raw response only for a key without an entry, calls the CLI for the rest up to --max-calls, stops
after 5 consecutive calls without a judgment (a stop on keys other than those of the previous stop
does not count against them; a stop on the keys of the previous stop counts), skips a target that failed 3 times and records it in the ledger as
kind failed (no date; --retry-failed or a new policy tries it again), and compacts again at the end. See docs/llm-date-judgment.md.
"""
import argparse
import datetime
import fcntl
import hashlib
import json
import os
import re
import subprocess
import sys
import time
import uuid
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import checks  # noqa: E402
import ledger as ledger_mod  # noqa: E402
import source_text  # noqa: E402
import openai_backend  # noqa: E402


def backend_config(args):
    refuse_paid_key()
    return openai_backend.configure(getattr(args, 'backend', 'claude'), getattr(args, 'base_url', None),
                                    getattr(args, 'api_key_env', None), getattr(args, 'response_format', None),
                                    args.model, args.effort, args.timeout)


def dispatch(claude_bin, prompt, schema, model, effort, workdir, timeout, config=None):
    refuse_paid_key()
    if config is None:
        return call_cli(claude_bin, prompt, schema, model, effort, workdir, timeout)
    return openai_backend.call(config, prompt, schema, model, SYSTEM, timeout, one_line)


def policy_extension(policy):
    return {key: policy[key] for key in ledger_mod.API_POLICY_KEYS if key in policy}

RUNNER_VERSION = 'r2'
PROMPT_FILE = HERE / 'prompts' / 'v2.md'
SCHEMA_FILE = HERE / 'schema' / 'judgment.schema.json'
PROMPT_TAG = 'p2'
SYSTEM = ('너는 한국 법규 원문에서 시행일 문장을 판독하는 도구다. 사용자 메시지의 기준과 원문만 근거로 쓴다. '
          '도구를 쓰지 않는다. 지정한 JSON 형식으로만 답한다.')
STOP_AFTER_FAILURES = 5
TARGET_FAILURE_LIMIT = 3
FREE_TEXT = ('basis_quote', 'section_label', 'proviso_quote', 'attribution', 'reason')
# The policy fields that decide a call; runner_version changes only the checks (recheck, no new call).
JUDGE_POLICY_KEYS = ('model', 'effort', 'prompt_version')


def prompt_version(template, schema):
    h = hashlib.sha256()
    for part in (SYSTEM, template, schema):
        h.update(part.encode('utf-8'))
        h.update(b'\0')
    return PROMPT_TAG + '-' + h.hexdigest()[:16]


def current_prompt():
    template = PROMPT_FILE.read_text(encoding='utf-8')
    schema = SCHEMA_FILE.read_text(encoding='utf-8')
    return template, schema, prompt_version(template, schema)


def render(template, meta, fields, next_promulgation):
    prom_label = '공포일' if meta['dataset'] == 'ordinances' else '발령일'
    lines = [
        f'- 종류: {"자치법규" if meta["dataset"] == "ordinances" else "행정규칙"} {meta.get("kind", "")}',
        f'- 법규명: {meta.get("name", "")}',
        f'- 기관: {meta.get("issuer", "")}',
        f'- {prom_label}: {meta.get("promulgation_date") or "(원천 값 없음)"}',
        f'- {"공포번호" if meta["dataset"] == "ordinances" else "발령번호"}: {meta.get("promulgation_number", "")}',
        f'- 제개정구분: {meta.get("amendment_type", "")}',
        f'- 다음 판본 {prom_label}: {next_promulgation or "알 수 없음"}',
    ]
    body, excerpt = source_text.body_for_prompt(fields.get('조문', ''))
    blocks = []
    addenda = meta.get('addenda') or []
    if addenda:
        parts, cut = [], False
        for i, a in enumerate(addenda, 1):
            head = f'(부칙 칸 {i}: 원천 부칙공포일자 {a["promulgation_raw"] or "없음"}, 부칙공포번호 {a["number"] or "없음"})'
            text, short = source_text.addenda_for_prompt(a['text'] or '', meta.get('promulgation_raw'), meta.get('promulgation_number'))
            cut = cut or short
            parts.append(head + '\n' + (text or '(비어 있음)'))
        note = ' (누적 부칙이 길어 마지막 구역들과 제목에 이 판본의 공포일이나 공포번호가 든 구역만 싣는다)' if cut else ''
        blocks.append('[부칙]' + note + '\n' + '\n'.join(parts))
        excerpt = excerpt or cut
    else:
        blocks.append('[부칙]\n(비어 있음)')
    blocks.append('[조문]' + (' (원문이 길어 시행, 효력, 부칙이 들어간 줄과 앞뒤 한 줄만 싣는다)' if excerpt and len(body) < len(fields.get('조문', '')) else '') + '\n' + (body or '(비어 있음)'))
    blocks.append('[제개정이유]\n' + (fields.get('제개정이유') or '(비어 있음)'))
    blocks.append('[개정문]\n' + (fields.get('개정문') or '(비어 있음)'))
    return template.replace('{meta}', '\n'.join(lines)).replace('{fields}', '\n\n'.join(blocks)), excerpt


CONTROL = re.compile(r'[\x00-\x1f\x7f]')


def flat(text):
    """Control characters and line breaks become one space; white space runs become one space."""
    return ' '.join(CONTROL.sub(' ', text).split())


def one_line(judgment):
    out = dict(judgment)
    for k in FREE_TEXT:
        if isinstance(out.get(k), str):
            out[k] = flat(out[k])
    if isinstance(out.get('partial_enforcement'), list):
        out['partial_enforcement'] = [dict(p, quote=flat(p['quote'])) if isinstance(p, dict) and isinstance(p.get('quote'), str) else p
                                      for p in out['partial_enforcement']]
    return out


class PaidKeyError(RuntimeError):
    pass


def refuse_paid_key():
    """user-compiler2-shunt-path-20261116: no call runs with a paid API key in the environment."""
    if 'ANTHROPIC_API_KEY' in os.environ:
        raise PaidKeyError('ANTHROPIC_API_KEY is set; the judge does not use a paid API key')


def call_cli(claude_bin, prompt, schema, model, effort, workdir, timeout):
    refuse_paid_key()
    cmd = [claude_bin, '-p', '--model', model, '--tools', '', '--output-format', 'json', '--json-schema', schema,
           '--system-prompt', SYSTEM, '--no-session-persistence', '--setting-sources', 'project',
           '--strict-mcp-config', '--disable-slash-commands']
    if effort and effort != 'default':
        cmd += ['--effort', effort]
    started = time.time()
    proc = subprocess.run(cmd, input=prompt, capture_output=True, text=True, cwd=workdir, timeout=timeout)
    return proc, round(time.time() - started, 1)


def parse_response(stdout):
    """(judgment or None, call info). A judgment needs exit 0, is_error false and structured_output."""
    try:
        response = json.loads(stdout)
    except ValueError:
        return None, {'error': 'stdout is not JSON'}
    info = {'is_error': response.get('is_error'), 'usage': response.get('usage'),
            'total_cost_usd': response.get('total_cost_usd'), 'result': (response.get('result') or '')[:200]
            if response.get('is_error') else None}
    judgment = response.get('structured_output')
    if response.get('is_error') or not isinstance(judgment, dict):
        return None, info
    return judgment, info


def response_of(stdout, exit_code):
    """(judgment or None, call info). A judgment needs exit 0; a non-zero exit keeps the error summary
    of the response but never gives a judgment."""
    judgment, info = parse_response(stdout)
    if exit_code != 0:
        return None, dict(info, error=f'exit {exit_code}')
    return judgment, info


def cross_check(target, entry):
    category = (target.get('parser') or {}).get('category')
    if category in (None, 'parser_category_pending', 'judge'):
        return 'not_applicable'
    if entry['adoptable']:
        return 'agree' if entry['date'] == (target.get('parser') or {}).get('date') else 'disagree'
    return 'inconclusive'


def build_entry(target, meta, fields, judgment, policy, raw_ref, usage, excerpt, judged_at):
    judgment = one_line(judgment)
    adoptable, results = checks.check(judgment, meta, fields, target.get('next_promulgation'))
    entry = {
        'key': dict(target['key']), 'kind': 'judged', 'judgment': judgment, 'checks': results,
        'adoptable': adoptable, 'date': judgment.get('date') if adoptable else '',
        'model': policy['model'], 'effort': policy['effort'], 'prompt_version': policy['prompt_version'],
        'runner_version': policy['runner_version'], 'judged_at': judged_at,
        'inputs': {'next_promulgation': target.get('next_promulgation'), 'body_excerpt': excerpt,
                   'source_path': target.get('path')},
        'parser_at_judgment': target.get('parser'), 'raw_ref': raw_ref, 'usage': usage,
    }
    entry.update(policy_extension(policy))
    entry['cross_check_at_judgment'] = cross_check(target, entry)
    return entry


def recheck_factory(cache):
    def recheck(entry):
        path = (entry.get('inputs') or {}).get('source_path')
        if not path or not (Path(cache) / path).exists():
            return None
        meta, fields = source_text.read_source(Path(cache) / path, entry['key']['dataset'])
        if meta['source_sha256'] != entry['key']['source_sha256']:
            return None
        adoptable, results = checks.check(entry['judgment'], meta, fields, entry['inputs'].get('next_promulgation'))
        fixed = dict(entry)
        fixed.update({'checks': results, 'adoptable': adoptable, 'date': entry['judgment'].get('date') if adoptable else '',
                      'runner_version': RUNNER_VERSION})
        fixed['cross_check_at_judgment'] = cross_check({'parser': entry.get('parser_at_judgment')}, fixed)
        return fixed
    return recheck


def policy_root(ops, name, policy):
    root = Path(ops) / name
    model = policy['model']
    if ledger_mod.has_api_policy(policy):
        root = root / 'openai' / policy['endpoint_sha256'] / policy['response_format']
        model = hashlib.sha256(model.encode()).hexdigest()
    return root / policy['prompt_version'] / model / policy['effort']


def raw_root(ops, policy):
    return policy_root(ops, 'raw', policy)


def find_raw(ops, policy, sha, config=None):
    """The newest raw call for this source whose call arguments match the policy (model, effort,
    prompt_version; design section 17), that ended with exit 0 and that holds a usable judgment, or None."""
    root = raw_root(ops, policy)
    if not root.exists():
        return None
    for d in sorted(root.glob(sha + '-*'), reverse=True):
        try:
            args = json.loads((d / 'call-args.json').read_text(encoding='utf-8'))
            judgment, info = parse_response((d / 'stdout.json').read_text(encoding='utf-8'))
        except (OSError, ValueError):
            continue
        if ledger_mod.same_policy(args, policy, call=True) and judgment is not None and args.get('exit') == 0:
            if ledger_mod.has_api_policy(policy):
                try:
                    if config is None:
                        continue
                    envelope = openai_backend.loads((d / 'stdout.json').read_text(encoding='utf-8'))
                    judgment = openai_backend.validate_judgment(envelope['structured_output'],
                                openai_backend.loads(current_prompt()[1]), config['sensitive'], one_line)
                except (ValueError, KeyError, TypeError, RecursionError):
                    continue
            return d, args, judgment, info
    return None


def failures_root(ops, policy):
    return policy_root(ops, 'failures', policy)


def run(args):
    try:
        backend_config(args)
    except (PaidKeyError, openai_backend.BackendError) as error:
        print(error, file=sys.stderr)
        return 2
    ops = Path(args.ops)
    ops.mkdir(parents=True, exist_ok=True)
    lock = open(ops / 'lock', 'w')
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError:
        lock.close()
        print('another runner holds the operations lock', file=sys.stderr)
        return 75
    try:
        return run_locked(args, ops)
    finally:
        lock.close()


def run_locked(args, ops):
    """The body of run; the caller holds the operations lock."""
    config = backend_config(args)
    ledger_mod.remove_temporary(ops / 'ledger', ops / 'ledger-history', ops / 'failures')
    template, schema, pversion = current_prompt()
    policy = {'model': args.model, 'effort': args.effort, 'prompt_version': pversion, 'runner_version': RUNNER_VERSION}
    if config is not None:
        policy.update(config['policy'])
    protected, revisions, list_sha = ledger_mod.load_protected(args.protected)
    led = ledger_mod.Ledger(ops, args.dataset)
    targets = json.loads(Path(args.targets).read_text(encoding='utf-8'))['targets']
    held = led.load()['policy']
    if held is not None and not ledger_mod.same_policy(held, policy, call=True):
        # lead-compiler2-f3-20261116: a policy change makes every key a new call, so it needs an
        # explicit argument and the count comes first.
        if not args.replace_policy:
            print(json.dumps({'refused': 'run policy differs from the ledger policy; pass --replace-policy to change it',
                              'ledger_policy': held, 'run_policy': policy}, ensure_ascii=False))
            return 4
        current = {ledger_mod.key_of(e): e for e in led.load()['entries']}
        count = sum(1 for t in targets if t['key']['dataset'] == args.dataset
                    and (t['key']['dataset'], t['key']['revision']) not in protected
                    and not ledger_mod.policy_matches(current.get(tuple(t['key'][f] for f in ledger_mod.KEY_FIELDS), {}), policy))
        print(json.dumps({'replace_policy': {'from': held, 'to': policy, 'targets_to_judge': count, 'max_calls': args.max_calls}},
                         ensure_ascii=False))
    journal_cut = ledger_mod.repair(led.journal)[1]
    led.protect(protected, revisions)
    report = {'journal_cut_line_at_start': journal_cut,
              'compaction_start': led.compact(policy, protected, list_sha, recheck_factory(args.cache))}
    entries = {ledger_mod.key_of(e): e for e in led.load()['entries']}
    failures_path = failures_root(ops, policy) / f'failures-{args.dataset}.json'
    # counts: failures per target key; last_stop: the keys of the last run of failures that stopped a
    # run; forgiven: those of them whose failure was taken back; errors: last error summary per key.
    failures = json.loads(failures_path.read_text(encoding='utf-8')) if failures_path.exists() else {}
    failures = {'counts': failures.get('counts', {}), 'last_stop': failures.get('last_stop', []),
                'forgiven': failures.get('forgiven', []), 'errors': failures.get('errors', {})}
    counts = failures['counts']
    calls = restored = skipped_protected = skipped_done = 0
    skipped_failed_keys, skipped_source_changed_keys = [], []
    streak = []
    stopped = None
    workdir = ops / 'empty-cwd'
    workdir.mkdir(exist_ok=True)
    calls_log = ops / 'calls.log'

    def save_failures():
        ledger_mod.atomic_write(failures_path, json.dumps(failures, ensure_ascii=False, indent=1, sort_keys=True) + '\n')

    if args.retry_failed:
        # The reset is written at once, so it does not depend on what the calls of this run do.
        failures = {'counts': {}, 'last_stop': [], 'forgiven': [], 'errors': {}}
        counts = failures['counts']
        save_failures()

    for t in targets:
        key = t['key']
        if key['dataset'] != args.dataset:
            continue
        k = tuple(key[f] for f in ledger_mod.KEY_FIELDS)
        fkey = '|'.join(k)
        if (key['dataset'], key['revision']) in protected:
            skipped_protected += 1
            continue
        if k in entries and ledger_mod.policy_matches(entries[k], policy):
            skipped_done += 1
            continue
        if counts.get(fkey, 0) >= TARGET_FAILURE_LIMIT:
            skipped_failed_keys.append(fkey)
            # lead-compiler2-a2-a3-20261117: the ledger records the skip; it carries no date.
            new = failed_entry(key, policy, counts[fkey], failures['errors'].get(fkey))
            old = entries.get(k)
            # Written again only when something but the time differs (a new error after a retry).
            if old is None or {f: v for f, v in old.items() if f != 'judged_at'} != {f: v for f, v in new.items() if f != 'judged_at'}:
                led.add(new)
            continue
        source = Path(args.cache) / t['path']
        if not source.exists():
            skipped_source_changed_keys.append(fkey)
            continue
        meta, fields = source_text.read_source(source, key['dataset'])
        if meta['source_sha256'] != key['source_sha256']:
            skipped_source_changed_keys.append(fkey)
            continue
        # Design section 17: a raw response is restored only for a key that has no entry; a key with
        # an entry of another policy or an old runner that failed its recheck is judged again.
        found = find_raw(ops, policy, key['source_sha256'], config) if k not in entries else None
        if found is not None:
            d, call_args, judgment, info = found
            raw_ref = {'path': str(d.relative_to(ops)), 'stdout_sha256': ledger_mod.file_sha256(d / 'stdout.json')}
            # The entry records the inputs of the call: next promulgation and parser result at call time.
            seen_target = dict(t, next_promulgation=call_args.get('next_promulgation'), parser=call_args.get('parser', t.get('parser')))
            usage = dict(info.get('usage') or {}, seconds=call_args.get('seconds'), total_cost_usd=info.get('total_cost_usd'))
            led.add(build_entry(seen_target, meta, fields, judgment, policy, raw_ref, usage, call_args.get('body_excerpt'), call_args['judged_at']))
            restored += 1
            continue
        if calls >= args.max_calls:
            stopped = 'max-calls'
            break
        prompt, excerpt = render(template, meta, fields, t.get('next_promulgation'))
        judged_at = datetime.datetime.now(datetime.timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')
        d = raw_root(ops, policy) / f'{key["source_sha256"]}-{judged_at.replace(":", "")}-{uuid.uuid4().hex[:8]}'
        d.mkdir(parents=True, exist_ok=False)
        calls += 1
        with open(calls_log, 'a', encoding='utf-8') as fh:
            fh.write(f'{judged_at}\t{policy["model"]}\t{"|".join(k)}\n')
        try:
            proc, seconds = dispatch(args.claude, prompt, schema, policy['model'], policy['effort'], workdir, args.timeout, config)
            stdout, stderr, exit_code = proc.stdout, proc.stderr, proc.returncode
        except subprocess.TimeoutExpired:
            stdout, stderr, exit_code, seconds = '', 'timeout', 124, args.timeout
        (d / 'prompt.txt').write_text(prompt, encoding='utf-8')
        (d / 'stdout.json').write_text(stdout, encoding='utf-8')
        (d / 'stderr.txt').write_text(stderr, encoding='utf-8')
        call_args = dict(policy, key=key, exit=exit_code, seconds=seconds, judged_at=judged_at,
                         next_promulgation=t.get('next_promulgation'), parser=t.get('parser'), body_excerpt=excerpt)
        (d / 'call-args.json').write_text(json.dumps(call_args, ensure_ascii=False, indent=1), encoding='utf-8')
        judgment, info = response_of(stdout, exit_code)
        if judgment is None:
            counts[fkey] = counts.get(fkey, 0) + 1
            failures['errors'][fkey] = one_error(info)
            save_failures()
            streak.append(fkey)
            if len(streak) >= STOP_AFTER_FAILURES:
                # lead-compiler2-a2-a3-20261117: a stop on keys other than those of the previous stop
                # is taken as a shared cause (usage limit, login) and its failures are taken back. A
                # stop on the keys of the previous stop counts, with the failures taken back before,
                # so the third same stop reaches the target limit.
                if sorted(streak) == sorted(failures['last_stop']):
                    for f in failures['forgiven']:
                        counts[f] += 1
                    failures['forgiven'] = []
                else:
                    for f in streak:
                        counts[f] -= 1
                    failures['forgiven'] = list(streak)
                failures['last_stop'] = list(streak)
                save_failures()
                stopped = f'{STOP_AFTER_FAILURES} consecutive calls without a judgment; last: {info}'
                report['stop_keys'] = list(streak)
                break
            continue
        streak = []
        raw_ref = {'path': str(d.relative_to(ops)), 'stdout_sha256': ledger_mod.file_sha256(d / 'stdout.json')}
        usage = dict(info.get('usage') or {}, seconds=seconds, total_cost_usd=info.get('total_cost_usd'))
        led.add(build_entry(t, meta, fields, judgment, policy, raw_ref, usage, excerpt, judged_at))
    report.update({'calls': calls, 'restored': restored, 'skipped_protected': skipped_protected, 'skipped_done': skipped_done,
                   'skipped_failed': len(skipped_failed_keys), 'skipped_failed_keys': skipped_failed_keys,
                   'skipped_source_changed': len(skipped_source_changed_keys),
                   'skipped_source_changed_keys': skipped_source_changed_keys, 'stopped': stopped})
    report['compaction_end'] = led.compact(policy, protected, list_sha, recheck_factory(args.cache))
    print(json.dumps(report, ensure_ascii=False))
    return 3 if stopped and stopped != 'max-calls' else 0


def one_error(info):
    return json.dumps(info, ensure_ascii=False, sort_keys=True, default=str)[:300]


def failed_entry(key, policy, failures, last_error):
    """A target skipped after TARGET_FAILURE_LIMIT failures. It holds no date; the compiler keeps the
    parser result or unresolved."""
    return {'key': dict(key), 'kind': 'failed', 'reason': '지속 실패', 'failures': failures, 'last_error': last_error,
            'model': policy['model'], 'effort': policy['effort'], 'prompt_version': policy['prompt_version'],
            'runner_version': policy['runner_version'],
            'judged_at': datetime.datetime.now(datetime.timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ'), **policy_extension(policy)}


def judge_file(dataset, source, model, effort, next_promulgation, out, claude_bin='claude', timeout=900,
               *, backend='claude', base_url=None, api_key_env=None, response_format=None):
    """Judges one source file for measurement: writes prompt.txt, stdout.json, stderr.txt and
    entry.json into the new folder out. Writes no ledger. Raises PaidKeyError when
    ANTHROPIC_API_KEY is set."""
    refuse_paid_key()
    config = openai_backend.configure(backend, base_url, api_key_env, response_format, model, effort, timeout)
    out = Path(out)
    out.mkdir(parents=True, exist_ok=False)
    template, schema, pversion = current_prompt()
    meta, fields = source_text.read_source(source, dataset)
    prompt, excerpt = render(template, meta, fields, next_promulgation)
    (out / 'prompt.txt').write_text(prompt, encoding='utf-8')
    entry = {'key': {'dataset': dataset, 'identity': meta['identity'], 'revision': meta['revision'],
                     'source_sha256': meta['source_sha256']},
             'model': model, 'effort': effort, 'prompt_version': pversion, 'runner_version': RUNNER_VERSION,
             'body_excerpt': excerpt, 'next_promulgation': next_promulgation,
             'judged_at': datetime.datetime.now(datetime.timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')}
    if config is not None:
        entry.update(config['policy'])
    workdir = out.parent / 'empty-cwd'
    workdir.mkdir(exist_ok=True)
    try:
        proc, seconds = dispatch(claude_bin, prompt, schema, model, effort, workdir, timeout, config)
    except subprocess.TimeoutExpired:
        entry.update({'call': {'error': 'timeout', 'seconds': timeout}, 'adoptable': False, 'judgment': None, 'date': ''})
        (out / 'entry.json').write_text(json.dumps(entry, ensure_ascii=False, indent=1), encoding='utf-8')
        return entry
    (out / 'stdout.json').write_text(proc.stdout, encoding='utf-8')
    (out / 'stderr.txt').write_text(proc.stderr, encoding='utf-8')
    judgment, info = response_of(proc.stdout, proc.returncode)
    entry['call'] = dict(info, exit=proc.returncode, seconds=seconds)
    entry['judgment'] = one_line(judgment) if judgment else None
    if judgment:
        adoptable, results = checks.check(entry['judgment'], meta, fields, next_promulgation)
        entry.update({'checks': results, 'adoptable': adoptable, 'date': judgment.get('date') if adoptable else ''})
    else:
        entry.update({'adoptable': False, 'date': ''})
    (out / 'entry.json').write_text(json.dumps(entry, ensure_ascii=False, indent=1), encoding='utf-8')
    return entry


def one(args):
    try:
        config = backend_config(args)
    except (PaidKeyError, openai_backend.BackendError) as error:
        print(error, file=sys.stderr)
        return 2
    template, schema, pversion = current_prompt()
    meta, fields = source_text.read_source(args.source, args.dataset)
    prompt, excerpt = render(template, meta, fields, args.next_promulgation)
    proc, seconds = dispatch(args.claude, prompt, schema, args.model, args.effort, Path(args.workdir), args.timeout, config)
    judgment, info = response_of(proc.stdout, proc.returncode)
    out = {'prompt_version': pversion, 'seconds': seconds, 'call': info}
    if config is not None:
        out.update(config['policy'])
    if judgment is not None:
        adoptable, results = checks.check(one_line(judgment), meta, fields, args.next_promulgation)
        out.update({'judgment': judgment, 'checks': results, 'adoptable': adoptable})
    print(json.dumps(out, ensure_ascii=False, indent=1))
    return 0 if judgment is not None else 3


def main(argv=None):
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest='cmd', required=True)
    for name in ('one', 'run'):
        p = sub.add_parser(name)
        p.add_argument('--dataset', required=True, choices=['ordinances', 'admrules'])
        p.add_argument('--model', required=True)
        p.add_argument('--effort', default='default')
        p.add_argument('--claude', default='claude')
        p.add_argument('--timeout', type=int, default=900)
        p.add_argument('--backend', choices=('claude', 'openai'), default='claude')
        p.add_argument('--base-url')
        p.add_argument('--api-key-env')
        p.add_argument('--response-format', choices=('json_schema', 'json_object'))
    o = sub.choices['one']
    o.add_argument('--source', required=True)
    o.add_argument('--next-promulgation')
    o.add_argument('--workdir', required=True)
    r = sub.choices['run']
    r.add_argument('--ops', required=True)
    r.add_argument('--cache', required=True)
    r.add_argument('--targets', required=True)
    r.add_argument('--protected', required=True)
    r.add_argument('--max-calls', type=int, required=True)
    r.add_argument('--retry-failed', action='store_true',
                   help='forget the failure counts of this policy and call skipped targets again')
    r.add_argument('--replace-policy', action='store_true',
                   help='judge again under a model, effort or prompt that differs from the ledger policy')
    args = ap.parse_args(argv)
    return run(args) if args.cmd == 'run' else one(args)


if __name__ == '__main__':
    sys.exit(main())
