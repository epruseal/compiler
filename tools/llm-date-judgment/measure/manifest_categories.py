"""Count compiler#1 prototype date-index revisions by LLM-relevant category and write a target list.

Usage: python3 -I manifest_categories.py RUNS_DIR BASELINE_DIR OUT_DIR

RUNS_DIR holds ledger.json and one folder per run with a *.manifest.json; a run without a manifest
falls back to BASELINE_DIR/<run>/*.manifest.json.

Reads (read only) the p6-runs ledger of the isolated r25c3 prototype. A run without a manifest
falls back to the r25c3 baseline manifest of the same name. Each revision row is keyed by
(dataset, identity, revision, copy_id); the same key in two runs counts once.

Categories (issue compiler#2 requirement 4):
  source-date      confirmed by a valid XML effective date: not an LLM target
  user-approved    confirmed by an individual user approval: not an LLM target
  judge            unresolved or undated: the LLM decides (requirement 4, last bullet)
  cross-sentence   confirmed from a sentence without a valid XML date: the LLM cross-checks
  cross-default    confirmed by the approved default period (21 or 6 days): cross-check
  cross-arrival    estimated arrival date: cross-check
  repealed         repeal rows, reported separately
"""
import collections
import json
import sys
from pathlib import Path



def category(rev):
    status = rev.get('status')
    ev = rev.get('date_evidence') or {}
    rule = ev.get('rule') or rev.get('reason') or ''
    if status in ('unresolved', 'undated'):
        return 'judge'
    if status == 'repealed':
        return 'repealed'
    if status == 'estimated':
        return 'cross-arrival'
    if rule == 'valid source enforcement date':
        return 'source-date'
    if ev.get('approved_date_decision') or ev.get('approval_source') or 'individually approved' in rule:
        return 'user-approved'
    if 'default' in rule or 'twenty' in rule or '21' in rule or 'five-day' in rule or 'period' in rule:
        return 'cross-default'
    return 'cross-sentence'


def main():
    NEW, BASE, out = Path(sys.argv[1]), Path(sys.argv[2]), Path(sys.argv[3])
    out.mkdir(parents=True, exist_ok=True)
    ledger = json.loads((NEW / 'ledger.json').read_text(encoding='utf-8'))
    inputs = {}
    for entry in ledger:
        argv = entry.get('argv') or []
        if len(argv) > 1:
            inputs[entry['run']] = argv[1]
    rows = {}
    per_run = {}
    for run in sorted(p for p in NEW.iterdir() if p.is_dir() and not p.name.startswith('attempt')):
        mans = sorted(run.glob('*.manifest.json'))
        origin = 'p6'
        if not mans:
            mans = sorted((BASE / run.name).glob('*.manifest.json'))
            origin = 'r25c3-baseline'
        if not mans:
            per_run[run.name] = ('missing', None, 0, {})
            continue
        data = json.loads(mans[0].read_text(encoding='utf-8'))
        di = data['date_index']
        dataset = di.get('dataset')
        counts = collections.Counter()
        for rev in di['revisions']:
            cat = category(rev)
            counts[cat] += 1
            ev = rev.get('date_evidence') or {}
            key = (dataset, rev.get('identity'), rev.get('revision'), rev.get('copy_id'))
            row = {
                'dataset': dataset, 'identity': rev.get('identity'), 'revision': rev.get('revision'),
                'copy_id': rev.get('copy_id'), 'category': cat, 'status': rev.get('status'),
                'rule': ev.get('rule') or rev.get('reason') or '',
                'enforcement_date': rev.get('enforcement_date'), 'promulgation_date': rev.get('promulgation_date'),
                'raw_enforcement_date': ev.get('raw_enforcement_date'), 'raw_promulgation_date': ev.get('raw_promulgation_date'),
                'source_path': ev.get('source_path'), 'input_root': inputs.get(run.name), 'run': run.name,
                'category_name': rev.get('category'), 'name': rev.get('name'), 'repeal': rev.get('repeal'),
                'evidence_keys': sorted(k for k in ev.keys() if k not in ('rule', 'source_path')),
            }
            if key in rows and rows[key]['category'] != cat:
                row['conflict_with_run'] = rows[key]['run']
            rows.setdefault(key, row)
        per_run[run.name] = (origin, dataset, sum(counts.values()), dict(counts))
        del data, di
    uniq = collections.Counter(r['category'] for r in rows.values())
    by_ds = collections.Counter((r['dataset'], r['category']) for r in rows.values())
    rules = collections.Counter((r['category'], r['rule']) for r in rows.values() if r['category'] not in ('source-date',))
    conflicts = [r for r in rows.values() if 'conflict_with_run' in r]
    with (out / 'targets.jsonl').open('w', encoding='utf-8') as f:
        for key in sorted(rows, key=lambda k: tuple(str(x) for x in k)):
            r = rows[key]
            if r['category'] in ('judge', 'cross-sentence', 'cross-default', 'cross-arrival', 'user-approved'):
                f.write(json.dumps(r, ensure_ascii=False) + '\n')
    lines = []
    for name, (origin, dataset, n, c) in per_run.items():
        lines.append(f'{name:24} {origin:15} {str(dataset):10} {n:6} {c}')
    lines.append(f'UNIQUE rows {len(rows)} {dict(uniq)}')
    for (ds, cat), n in sorted(by_ds.items()):
        lines.append(f'  {ds:12} {cat:15} {n}')
    lines.append(f'category conflicts across runs: {len(conflicts)}')
    lines.append('--- rules (category, count, rule)')
    for (cat, rule), n in sorted(rules.items(), key=lambda x: (x[0][0], -x[1])):
        lines.append(f'{cat:15} {n:6} {rule}')
    (out / 'summary.txt').write_text('\n'.join(lines) + '\n', encoding='utf-8')
    print('\n'.join(lines))


if __name__ == '__main__':
    main()
