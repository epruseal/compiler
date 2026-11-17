"""Run a frozen gold or observation sample through the judge for each model and score it.

Usage:
  python3 -I gold_run.py run --models M1,M2 --spec SPEC_JSON --index INDEX_TSV --out DIR [--cap N]
  (environment: LDJ_CACHE source cache, LDJ_CALL_LOG call log file)
  python3 -I gold_run.py score --out DIR

run   resolves each gold case to the compiler-selected source file (ordinance history copy first),
      finds the next revision's promulgation date from the full-cache index, and judges each case
      once per model. A case with an entry.json is skipped, so a stopped run resumes. Every call
      is appended to the call log; the run stops before the total reaches the cap.
score compares each entry with the gold expectation and writes score.json and score.md.
"""
import argparse
import collections
import json
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(HERE))
import judge  # noqa: E402
import checks  # noqa: E402,F401
import source_text  # noqa: E402,F401

import os
CACHE = Path(os.environ['LDJ_CACHE']) if 'LDJ_CACHE' in os.environ else None
CALL_LOG = Path(os.environ.get('LDJ_CALL_LOG', 'llm-calls.log'))


def calls_so_far():
    if not CALL_LOG.exists():
        return 0
    return sum(1 for line in CALL_LOG.read_text(encoding='utf-8').splitlines() if line.strip())


def source_path(dataset, revision):
    if dataset == 'ordinances':
        for p in (CACHE / 'ordinance' / 'history' / f'{revision}.xml', CACHE / 'ordinance' / f'{revision}.xml'):
            if p.is_file():
                return p
    else:
        p = CACHE / 'admrule' / f'{revision}.xml'
        if p.is_file():
            return p
    return None


def load_index(path):
    by_identity = collections.defaultdict(list)
    with open(path, encoding='utf-8') as fh:
        next(fh)
        for line in fh:
            ds, identity, serial, prom, _cls, _p = line.rstrip('\n').split('\t')
            prom = prom.strip()
            if len(prom) == 8 and prom.isdigit() and prom != '99991231' and prom[:4] != '0000':
                by_identity[(ds, identity)].append((prom, serial))
    for v in by_identity.values():
        v.sort()
    return by_identity


def next_promulgation(index, dataset, identity, prom_raw):
    prom_raw = (prom_raw or '').strip()
    if not (len(prom_raw) == 8 and prom_raw.isdigit()):
        return None
    later = [p for p, _ in index.get((dataset, identity), []) if p > prom_raw]
    if not later:
        return None
    p = min(later)
    return f'{p[:4]}-{p[4:6]}-{p[6:]}'


def run(models, index_path, out, cap, spec=None):
    gold = json.loads(Path(spec).read_text(encoding='utf-8'))
    index = load_index(index_path)
    out = Path(out)
    resolved = []
    for case in gold['cases']:
        path = source_path(case['dataset'], case['revision'])
        if path is None:
            resolved.append({'case': case, 'error': 'source not found'})
            continue
        meta, _ = source_text.read_source(path, case['dataset'])
        nxt = next_promulgation(index, case['dataset'], meta['identity'], meta['promulgation_raw'])
        resolved.append({'case': case, 'path': str(path), 'identity': meta['identity'], 'sha256': meta['source_sha256'],
                         'promulgation_raw': meta['promulgation_raw'], 'next_promulgation': nxt})
    current_version = judge.current_prompt()[2]
    out.mkdir(parents=True, exist_ok=True)
    (out / 'resolved.json').write_text(json.dumps(resolved, ensure_ascii=False, indent=1), encoding='utf-8')
    for model in models:
        for item in resolved:
            if 'error' in item:
                continue
            case = item['case']
            target = out / model / f'{case["dataset"]}-{case["revision"]}'
            if (target / 'entry.json').exists():
                done = json.loads((target / 'entry.json').read_text(encoding='utf-8'))
                failed = (done.get('call') or {}).get('exit') != 0 or (done.get('call') or {}).get('is_error') or not done.get('judgment')
                if done.get('prompt_version') == current_version and not failed:
                    continue
            if target.exists():
                (target.parent / (target.name + '.abandoned')).mkdir(parents=True, exist_ok=True)
                target.rename(target.parent / (target.name + '.abandoned') / str(len(list((target.parent / (target.name + '.abandoned')).iterdir()))))
            if calls_so_far() + 1 > cap:
                print(f'cap {cap} reached; stop before {model} {case["revision"]}', flush=True)
                return 3
            judge.refuse_paid_key()  # before the call log, so a refused call is not counted
            with CALL_LOG.open('a', encoding='utf-8') as fh:
                fh.write(f'gold {model} {case["dataset"]} {case["revision"]}\n')
            entry = judge.judge_file(case['dataset'], item['path'], model, 'default', item['next_promulgation'], target)
            print(model, case['revision'], entry.get('adoptable'), entry.get('date'),
                  (entry.get('judgment') or {}).get('outcome'), entry.get('call', {}).get('seconds'), flush=True)
    return 0


def score(out):
    out = Path(out)
    resolved = json.loads((out / 'resolved.json').read_text(encoding='utf-8'))
    models = sorted(p.name for p in out.iterdir() if p.is_dir())
    report = {}
    rows = []
    for model in models:
        versions = set()
        m = collections.Counter()
        tiers = collections.defaultdict(collections.Counter)
        cost = collections.Counter()
        for item in resolved:
            if 'error' in item:
                m['unresolved_source'] += 1
                continue
            case = item['case']
            f = out / model / f'{case["dataset"]}-{case["revision"]}' / 'entry.json'
            if not f.exists():
                m['not_run'] += 1
                continue
            e = json.loads(f.read_text(encoding='utf-8'))
            versions.add((e.get('prompt_version'), e.get('effort')))
            j = e.get('judgment') or {}
            if j and (e.get('call') or {}).get('exit') == 0 and not (e.get('call') or {}).get('is_error'):
                meta_, fields_ = source_text.read_source(item['path'], case['dataset'])
                ok_, res_ = checks.check(j, meta_, fields_, item.get('next_promulgation'))
                e['stored_adoptable'] = e.get('adoptable')
                e['adoptable'] = ok_
                e['date'] = j.get('date') if ok_ else ''
                e['checks'] = res_
            call = e.get('call') or {}
            u = call.get('usage') or {}
            cost['calls'] += 1
            cost['seconds'] += call.get('seconds') or 0
            cost['usd_list'] += call.get('total_cost_usd') or 0
            cost['input_tokens'] += (u.get('input_tokens') or 0) + (u.get('cache_creation_input_tokens') or 0) + (u.get('cache_read_input_tokens') or 0)
            cost['output_tokens'] += u.get('output_tokens') or 0
            exp = case.get('expect') or {}
            tier = case.get('tier', 'obs')
            verdict = None
            if 'date' in exp:
                if e.get('adoptable') and e.get('date') == exp['date']:
                    verdict = 'correct'
                elif e.get('adoptable'):
                    verdict = 'wrong_adopt'
                else:
                    verdict = 'abstain_or_rejected'
                m['raw_date_match' if j.get('date') == exp['date'] else 'raw_date_mismatch'] += 1
                if exp.get('partial'):
                    m['partial_found' if j.get('partial_enforcement') else 'partial_missed'] += 1
            elif exp.get('no_date'):
                verdict = 'wrong_adopt' if e.get('adoptable') else 'correct'
                m['outcome_match' if j.get('outcome') in exp.get('outcomes', []) else 'outcome_other'] += 1
            if exp.get('proviso'):
                m['proviso_found' if j.get('proviso') else 'proviso_missed'] += 1
                verdict = verdict or ('structure_ok' if j.get('proviso') else 'structure_missed')
            if not exp:
                verdict = 'observed'
            if verdict:
                m[verdict] += 1
                tiers[tier][verdict] += 1
            if call.get('error') or call.get('exit') not in (0, None) or not j:
                m['call_failed'] += 1
            rows.append({'model': model, 'tier': tier, 'dataset': case['dataset'], 'revision': case['revision'],
                         'expect': exp, 'verdict': verdict, 'adoptable': e.get('adoptable'), 'date': e.get('date'),
                         'llm_date': j.get('date'), 'outcome': j.get('outcome'), 'proviso': j.get('proviso'),
                         'checks_failed': [k for k, v in (e.get('checks') or {}).items() if v is False]})
        assert len(versions) <= 1, f'{model}: mixed prompt versions {versions}'
        n = cost['calls'] or 1
        report[model] = {'counts': dict(m), 'by_tier': {t: dict(c) for t, c in tiers.items()},
                         'per_call': {'seconds': round(cost['seconds'] / n, 1), 'usd_list': round(cost['usd_list'] / n, 4),
                                      'input_tokens': round(cost['input_tokens'] / n), 'output_tokens': round(cost['output_tokens'] / n)},
                         'calls': cost['calls']}
    (out / 'score.json').write_text(json.dumps({'report': report, 'rows': rows}, ensure_ascii=False, indent=1), encoding='utf-8')
    lines = ['| model | calls | correct | wrong_adopt | abstain_or_rejected | structure_ok | raw_date_match | outcome_match | s/call | input tok/call | output tok/call | list USD/call |', '|---|---|---|---|---|---|---|---|---|---|---|---|']
    for model, r in report.items():
        c = r['counts']
        p = r['per_call']
        lines.append(f"| {model} | {r['calls']} | {c.get('correct', 0)} | {c.get('wrong_adopt', 0)} | {c.get('abstain_or_rejected', 0)} | {c.get('structure_ok', 0)} | {c.get('raw_date_match', 0)} | {c.get('outcome_match', 0)} | {p['seconds']} | {p['input_tokens']} | {p['output_tokens']} | {p['usd_list']} |")
    lines.append('')
    for model, r in report.items():
        lines.append(f'{model} by tier: {json.dumps(r["by_tier"], ensure_ascii=False)}')
    lines.append('')
    lines.append('wrong adoptions and abstentions:')
    for row in rows:
        if row['verdict'] in ('wrong_adopt', 'abstain_or_rejected', 'structure_missed', 'observed'):
            lines.append(f"- {row['model']} {row['tier']} {row['revision']} {row['verdict']} expect={json.dumps(row['expect'], ensure_ascii=False)} llm={row['llm_date']} outcome={row['outcome']} failed={row['checks_failed']}")
    (out / 'score.md').write_text('\n'.join(lines) + '\n', encoding='utf-8')
    print('\n'.join(lines))


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest='cmd', required=True)
    r = sub.add_parser('run')
    r.add_argument('--models', required=True)
    r.add_argument('--index', required=True)
    r.add_argument('--out', required=True)
    r.add_argument('--cap', type=int, default=480)
    r.add_argument('--spec', required=True)
    s = sub.add_parser('score')
    s.add_argument('--out', required=True)
    a = ap.parse_args()
    if a.cmd == 'run':
        sys.exit(run(a.models.split(','), a.index, a.out, a.cap, a.spec))
    score(a.out)


if __name__ == '__main__':
    main()
