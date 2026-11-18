"""Count, over the whole source cache, the revisions whose XML effective date the compiler cannot use.

Usage: python3 -I full_scan.py CACHE_ROOT OUT_DIR

Read only. The candidate set follows the compiler input rules:
  laws        detail/*.xml
  admrules    admrule/*.xml and admrule/retired/*.xml when present
  ordinances  ordinance/*.xml and ordinance/history/*.xml; the kind filter keeps
              조례, 규칙, 훈령, 예규, 고시, 의회규칙 (codes C0001 C0002 C0003 C0004 C0010 C0011
              or the same labels); one candidate per 자치법규일련번호, the history copy wins
The first 시행일자 value is classified. Usable means an exact YYYYMMDD or YYYY-MM-DD real calendar
date with year 0001-9999 that is not 99991231. Every other value is unusable (sentinel, malformed,
empty, missing tag). Unusable files are read in full: SHA-256, text length of the fields an LLM
would read, and whether every text field is blank (issue compiler#2 requirement 5).
Outputs: OUT_DIR/summary.json, OUT_DIR/unusable.jsonl and OUT_DIR/index.tsv (one row per candidate).
"""
import hashlib
import json
import os
import re
import sys
from collections import Counter
from datetime import date
from multiprocessing import Pool

HEAD = 8192
ORD_KINDS = {'C0001': '조례', 'C0002': '규칙', 'C0003': '훈령', 'C0004': '예규', 'C0010': '고시', 'C0011': '의회규칙'}
ORD_LABELS = set(ORD_KINDS.values())
TEXT_TAGS = ('조내용', '조문내용', '부칙내용', '부칙', '제개정이유내용', '제개정이유', '개정문내용', '개정문')
TAG_RE = {}


def first(text, tag):
    rx = TAG_RE.get(tag)
    if rx is None:
        rx = TAG_RE[tag] = re.compile('<%s(?:\\s[^>]*)?>(.*?)</%s>' % (tag, tag), re.S)
    m = rx.search(text)
    if m is None:
        return None
    v = m.group(1).strip()
    if v.startswith('<![CDATA[') and v.endswith(']]>'):
        v = v[9:-3]
    return v


def classify(value):
    if value is None:
        return 'no_tag'
    v = value.strip()
    if v == '':
        return 'empty'
    if v == '99991231':
        return 'sentinel_99991231'
    compact = v.replace('-', '') if re.fullmatch(r'\d{4}-\d{2}-\d{2}', v) else v
    if not re.fullmatch(r'\d{8}', compact):
        return 'malformed'
    y, m, d = int(compact[:4]), int(compact[4:6]), int(compact[6:])
    if y == 0:
        return 'malformed'
    try:
        date(y, m, d)
    except ValueError:
        return 'malformed'
    return 'valid'


def all_text(text, tag):
    rx = TAG_RE.get(('all', tag))
    if rx is None:
        rx = TAG_RE[('all', tag)] = re.compile('<%s(?:\\s[^>]*)?>(.*?)</%s>' % (tag, tag), re.S)
    out = []
    for m in rx.finditer(text):
        v = m.group(1)
        v = re.sub(r'<!\[CDATA\[(.*?)\]\]>', r'\1', v, flags=re.S)
        v = re.sub(r'<[^>]+>', ' ', v)
        out.append(v)
    return ''.join(out)


def scan(job):
    dataset, folder, name, prio = job
    path = os.path.join(folder, name)
    try:
        with open(path, 'rb') as fh:
            head = fh.read(HEAD)
            text = head.decode('utf-8', 'replace')
            eff = first(text, '시행일자')
            if eff is None and len(head) == HEAD:
                rest = fh.read()
                head += rest
                text = head.decode('utf-8', 'replace')
                eff = first(text, '시행일자')
    except OSError as error:
        return {'dataset': dataset, 'path': path, 'error': str(error)}
    if dataset == 'ordinances':
        serial = first(text, '자치법규일련번호')
        kind_raw = first(text, '자치법규종류') or ''
        kind = ORD_KINDS.get(kind_raw.strip(), kind_raw.strip())
        prom = first(text, '공포일자')
        ident = first(text, '자치법규ID')
    elif dataset == 'admrules':
        serial = first(text, '행정규칙일련번호')
        kind = first(text, '행정규칙종류') or ''
        prom = first(text, '발령일자')
        ident = first(text, '행정규칙ID')
    else:
        serial = first(text, '법령일련번호') or name[:-4]
        kind = first(text, '법종구분') or ''
        prom = first(text, '공포일자')
        ident = first(text, '법령ID')
    cls = classify(eff)
    row = {'dataset': dataset, 'path': path, 'name': name, 'prio': prio, 'serial': serial, 'identity': ident,
           'kind': kind, 'eff_raw': eff, 'eff_class': cls, 'prom_raw': prom, 'prom_class': classify(prom)}
    if cls != 'valid':
        with open(path, 'rb') as fh:
            data = fh.read()
        full = data.decode('utf-8', 'replace')
        row['sha256'] = hashlib.sha256(data).hexdigest()
        row['bytes'] = len(data)
        fields = {}
        for tag in TEXT_TAGS:
            fields[tag] = len(re.sub(r'\s+', '', all_text(full, tag)))
        row['text_chars'] = fields
        row['text_blank'] = all(n == 0 for n in fields.values())
        row['html'] = full.lstrip().lower().startswith('<!doctype html') or '<html' in full[:500].lower()
    return row


def jobs(cache):
    plan = [('laws', 'detail', 1), ('admrules', 'admrule', 1), ('admrules', 'admrule/retired', 1),
            ('ordinances', 'ordinance', 1), ('ordinances', 'ordinance/history', 2)]
    for dataset, sub, prio in plan:
        folder = os.path.join(cache, sub)
        if not os.path.isdir(folder):
            continue
        with os.scandir(folder) as it:
            for entry in it:
                if entry.name.endswith('.xml') and entry.is_file():
                    yield (dataset, folder, entry.name, prio)


def main():
    cache, out = sys.argv[1], sys.argv[2]
    os.makedirs(out, exist_ok=True)
    stats = {d: Counter() for d in ('laws', 'admrules', 'ordinances')}
    chosen = {}
    index_meta = {}
    keep = []
    with Pool(2) as pool:
        for row in pool.imap_unordered(scan, jobs(cache), chunksize=400):
            ds = row['dataset']
            s = stats[ds]
            s['files'] += 1
            if 'error' in row:
                s['read_error'] += 1
                continue
            s['file:' + row['eff_class']] += 1
            if ds == 'ordinances':
                if row['kind'] not in ORD_LABELS:
                    s['skipped_kind'] += 1
                    continue
                key = ('ordinances', row['serial'] or 'path:' + row['path'])
                prev = chosen.get(key)
                if prev is not None:
                    s['duplicate_serial'] += 1
                if prev is None or row['prio'] >= prev[0]:
                    chosen[key] = (row['prio'], row['eff_class'], row if row['eff_class'] != 'valid' else None)
                    index_meta[key] = (row['identity'], row['serial'], row['prom_raw'], row['path'])
            else:
                key = (ds, row['path'])
                chosen[key] = (row['prio'], row['eff_class'], row if row['eff_class'] != 'valid' else None)
                index_meta[key] = (row['identity'], row['serial'], row['prom_raw'], row['path'])
    with open(os.path.join(out, 'index.tsv'), 'w', encoding='utf-8') as fh:
        fh.write('dataset\tidentity\tserial\tpromulgation_raw\teffective_class\tpath\n')
        for key, (_prio, cls, _row) in sorted(chosen.items(), key=lambda kv: (kv[0][0], str(kv[0][1]))):
            ds = key[0]
            meta = index_meta.get(key)
            if meta is not None:
                fh.write('\t'.join([ds, meta[0] or '', meta[1] or '', (meta[2] or '').replace('\t', ' '), cls, meta[3]]) + '\n')
    for (ds, _), (_, cls, row) in chosen.items():
        s = stats[ds]
        s['candidates'] += 1
        s['candidate:' + cls] += 1
        if row is not None:
            keep.append(row)
            if row['text_blank']:
                s['unusable_text_blank'] += 1
            if row.get('html'):
                s['unusable_html'] += 1
            if row['prom_class'] == 'valid':
                s['unusable_prom_valid'] += 1
            s['unusable_text_chars_total'] += sum(row['text_chars'].values())
    keep.sort(key=lambda r: (r['dataset'], r['path']))
    with open(os.path.join(out, 'unusable.jsonl'), 'w', encoding='utf-8') as fh:
        for row in keep:
            fh.write(json.dumps(row, ensure_ascii=False) + '\n')
    summary = {d: dict(sorted(c.items())) for d, c in stats.items()}
    with open(os.path.join(out, 'summary.json'), 'w', encoding='utf-8') as fh:
        json.dump({'cache': cache, 'summary': summary}, fh, ensure_ascii=False, indent=1)
    print(json.dumps(summary, ensure_ascii=False, indent=1))


if __name__ == '__main__':
    main()
