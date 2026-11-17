"""Daily queue of revisions that the LLM judge has not judged yet (compiler#2 requirement 8).

Usage: python3 -I queue_build.py --cache CACHE --ops OPS --protected LIST [--manifest MANIFEST ...]
                           [--date YYYYMMDD]

Reads the source cache (header scan), the canonical ledgers and the protection list. Never takes
the runner lock; the canonical ledger changes only by rename, so a read sees one whole version.
Takes its own lock queue/lock without waiting: when another queue build holds it, prints a skip
line and exits 0. Writes, in this order: queue/new-<date>.jsonl (keys seen for the first time),
queue/seen.jsonl, queue/pending.json (temporary file and rename). At start it drops a cut last line
of the two jsonl files and adds to seen every key that a new file holds.
"""
import argparse
import datetime
import fcntl
import json
import os
import sys
from collections import defaultdict
from multiprocessing import Pool
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
sys.path.insert(0, str(HERE / 'measure'))
import full_scan  # noqa: E402
import ledger as ledger_mod  # noqa: E402
import manifest_categories  # noqa: E402

ORD_LABELS = set(full_scan.ORD_KINDS.values())


def scan(cache):
    rows = []
    with Pool(2) as pool:
        for row in pool.imap_unordered(full_scan.scan, full_scan.jobs(cache), chunksize=400):
            if 'error' in row or row['dataset'] == 'laws':
                continue
            if row['dataset'] == 'ordinances' and row['kind'] not in ORD_LABELS:
                continue
            rows.append(row)
    return rows


def next_promulgation(by_identity, dataset, identity, prom):
    prom = (prom or '').strip()
    if not (len(prom) == 8 and prom.isdigit()):
        return None
    later = [p for p in by_identity.get((dataset, identity), ()) if p > prom]
    if not later:
        return None
    p = min(later)
    return f'{p[:4]}-{p[4:6]}-{p[6:]}'


def parser_map(manifests):
    out = {}
    for path in manifests or []:
        doc = json.loads(Path(path).read_text(encoding='utf-8'))
        dataset = doc['date_index']['dataset']
        for rev in doc['date_index']['revisions']:
            ev = rev.get('date_evidence') or {}
            out[(dataset, rev.get('identity'), rev.get('revision'), rev.get('copy_id'))] = {
                'category': manifest_categories.category(rev), 'status': rev.get('status'),
                'date': rev.get('enforcement_date'), 'rule': ev.get('rule') or rev.get('reason') or ''}
    return out


def repair(path):
    lines, cut = ledger_mod.read_lines(path)
    if cut:
        ledger_mod.atomic_write(path, ''.join(json.dumps(x, ensure_ascii=False, sort_keys=True) + '\n' for x in lines))
    return lines


def build(cache, ops, protected_path, manifests, day, rows=None):
    qdir = Path(ops) / 'queue'
    qdir.mkdir(parents=True, exist_ok=True)
    lock = open(qdir / 'lock', 'w')
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError:
        print(json.dumps({'skipped': 'another queue build holds queue/lock'}))
        return 0
    seen_path = qdir / 'seen.jsonl'
    new_path = qdir / f'new-{day}.jsonl'
    seen = {tuple(x['key']) for x in repair(seen_path)}
    for x in repair(new_path):
        k = tuple(x['key'][f] for f in ledger_mod.KEY_FIELDS)
        if k not in seen:
            ledger_mod.append_line(seen_path, {'key': list(k), 'first_seen': day})
            seen.add(k)
    protected, revisions, list_sha = ledger_mod.load_protected(protected_path)
    rows = rows if rows is not None else scan(cache)
    by_identity = defaultdict(list)
    for r in rows:
        p = (r.get('prom_raw') or '').strip()
        if len(p) == 8 and p.isdigit() and p != '99991231' and p[:4] != '0000':
            by_identity[(r['dataset'], r['identity'])].append(p)
    parsers = parser_map(manifests)
    current = {}
    policies = {}
    for ds in ('ordinances', 'admrules'):
        doc = ledger_mod.Ledger(ops, ds).load()
        policies[ds] = doc['policy']
        for e in doc['entries']:
            current[ledger_mod.key_of(e)] = e
    targets, confirm, new_lines = [], [], []
    for r in sorted(rows, key=lambda x: (x['dataset'], x['path'])):
        sha = r.get('sha256')
        if (r['dataset'], r['serial']) in protected:
            if sha is None:
                sha = ledger_mod.file_sha256(r['path'])
            if sha not in protected[(r['dataset'], r['serial'])]:
                confirm.append({'category': 'protected_source_changed', 'dataset': r['dataset'], 'revision': r['serial'],
                                'identity': r['identity'], 'path': os.path.relpath(r['path'], cache), 'source_sha256': sha})
            continue
        if r['eff_class'] == 'valid' or r.get('text_blank') or r.get('html'):
            continue
        key = {'dataset': r['dataset'], 'identity': r['identity'], 'revision': r['serial'], 'source_sha256': sha}
        k = tuple(key[f] for f in ledger_mod.KEY_FIELDS)
        policy = policies[r['dataset']]
        if k in current and policy is not None and ledger_mod.policy_matches(current[k], policy):
            continue
        target = {'key': key, 'path': os.path.relpath(r['path'], cache), 'raw_promulgation': r['prom_raw'],
                  'raw_enforcement': r['eff_raw'],
                  'parser': parsers.get(k[:3] + (sha,), {'category': 'parser_category_pending'}),
                  'next_promulgation': next_promulgation(by_identity, r['dataset'], r['identity'], r['prom_raw'])}
        targets.append(target)
        if k not in seen:
            new_lines.append({'key': key, 'path': target['path'], 'first_seen': day})
    for line in new_lines:
        ledger_mod.append_line(new_path, line)
    for line in new_lines:
        ledger_mod.append_line(seen_path, {'key': [line['key'][f] for f in ledger_mod.KEY_FIELDS], 'first_seen': day})
    doc = {'generated': day, 'protected_list_sha256': list_sha, 'policies': policies,
           'targets': targets, 'user_confirmation': confirm}
    ledger_mod.atomic_write(qdir / 'pending.json', ledger_mod.dumps(doc))
    print(json.dumps({'targets': len(targets), 'new': len(new_lines), 'user_confirmation': len(confirm)}))
    return 0


def main(argv=None):
    ap = argparse.ArgumentParser()
    ap.add_argument('--cache', required=True)
    ap.add_argument('--ops', required=True)
    ap.add_argument('--protected', required=True)
    ap.add_argument('--manifest', action='append')
    ap.add_argument('--date', default=datetime.date.today().strftime('%Y%m%d'))
    a = ap.parse_args(argv)
    return build(a.cache, a.ops, a.protected, a.manifest, a.date)


if __name__ == '__main__':
    sys.exit(main())
