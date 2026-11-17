"""Daily queue of revisions that the LLM judge has not judged yet (compiler#2 requirement 8).

Usage: python3 -I queue_build.py --cache CACHE --ops OPS --protected LIST [--manifest MANIFEST ...]
                           [--date YYYYMMDD]

Reads the source cache (header scan), the canonical ledgers and the protection list. Never takes
the runner lock; the canonical ledger changes only by rename, so a read sees one whole version.
Takes its own lock queue/lock without waiting: when another queue build holds it, prints a skip
line and exits 0. Writes, in this order: queue/new-<date>.jsonl (keys seen for the first time),
queue/seen.jsonl, queue/pending.json (temporary file and rename). At start it drops the unreadable
lines (a cut last line) of seen and of every new file and adds to seen, with that file's first_seen, every key that a new
file of any day holds. One key (byte-equal copies) gives one target; the other paths go to
other_paths. pending.json and the output line report stale_entries: ledger entries whose key the
scan no longer finds. The collection service does not run this file (user decision Q4); run it by
hand after a collection.
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


PARSER_PENDING = {'category': 'parser_category_pending', 'manifest_sha256': None}


def parser_map(manifests):
    out = {}
    for path in manifests or []:
        manifest_sha = ledger_mod.file_sha256(path)
        doc = json.loads(Path(path).read_text(encoding='utf-8'))
        dataset = doc['date_index']['dataset']
        for rev in doc['date_index']['revisions']:
            ev = rev.get('date_evidence') or {}
            out[(dataset, rev.get('identity'), rev.get('revision'), rev.get('copy_id'))] = {
                'category': manifest_categories.category(rev), 'status': rev.get('status'),
                'date': rev.get('enforcement_date'), 'rule': ev.get('rule') or rev.get('reason') or '',
                'manifest_sha256': manifest_sha}
    return out


def build(cache, ops, protected_path, manifests, day, rows=None):
    qdir = Path(ops) / 'queue'
    qdir.mkdir(parents=True, exist_ok=True)
    lock = open(qdir / 'lock', 'w')
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError:
        lock.close()
        print(json.dumps({'skipped': 'another queue build holds queue/lock'}))
        return 0
    try:
        return build_locked(cache, ops, protected_path, manifests, day, rows, qdir)
    finally:
        lock.close()


def build_locked(cache, ops, protected_path, manifests, day, rows, qdir):
    """The body of build; the caller holds queue/lock."""
    seen_path = qdir / 'seen.jsonl'
    new_path = qdir / f'new-{day}.jsonl'
    seen = {tuple(x['key']): x['first_seen'] for x in ledger_mod.repair(seen_path)[0]}
    # A build that stopped between its new file and seen leaves keys in a new file of any earlier
    # day; seen takes them with that file's first_seen, so no key appears in two new files.
    for path in sorted(qdir.glob('new-*.jsonl')):
        for x in ledger_mod.repair(path)[0]:
            k = tuple(x['key'][f] for f in ledger_mod.KEY_FIELDS)
            if k not in seen:
                ledger_mod.append_line(seen_path, {'key': list(k), 'first_seen': x['first_seen']})
                seen[k] = x['first_seen']
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
    ledger_revisions = {k[:3] for k in current}
    scanned = set()
    targets, confirm, new_lines = {}, [], []
    for r in sorted(rows, key=lambda x: (x['dataset'], x['path'])):
        sha = r.get('sha256')
        if sha is None and ((r['dataset'], r['serial']) in protected or (r['dataset'], r['identity'], r['serial']) in ledger_revisions):
            sha = ledger_mod.file_sha256(r['path'])
        if sha is not None:
            scanned.add((r['dataset'], r['identity'], r['serial'], sha))
        if (r['dataset'], r['serial']) in protected:
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
        path = os.path.relpath(r['path'], cache)
        if k in targets:
            # Byte-equal copies (an original and its history copy) share a key: one target.
            targets[k]['other_paths'].append(path)
            continue
        if k not in seen:
            seen[k] = day
            new_lines.append({'key': key, 'path': path, 'first_seen': day})
        targets[k] = {'key': key, 'path': path, 'other_paths': [], 'first_seen': seen[k],
                      'raw_promulgation': r['prom_raw'], 'raw_enforcement': r['eff_raw'],
                      'parser': parsers.get(k[:3] + (sha,), dict(PARSER_PENDING)),
                      'next_promulgation': next_promulgation(by_identity, r['dataset'], r['identity'], r['prom_raw'])}
    stale = {ds: sum(1 for k in current if k[0] == ds and k not in scanned) for ds in ('ordinances', 'admrules')}
    for line in new_lines:
        ledger_mod.append_line(new_path, line)
    for line in new_lines:
        ledger_mod.append_line(seen_path, {'key': [line['key'][f] for f in ledger_mod.KEY_FIELDS], 'first_seen': day})
    doc = {'generated': day, 'protected_list_sha256': list_sha, 'policies': policies,
           'targets': list(targets.values()), 'user_confirmation': confirm, 'stale_entries': stale}
    ledger_mod.atomic_write(qdir / 'pending.json', ledger_mod.dumps(doc))
    print(json.dumps({'targets': len(targets), 'new': len(new_lines), 'user_confirmation': len(confirm), 'stale_entries': stale}))
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
