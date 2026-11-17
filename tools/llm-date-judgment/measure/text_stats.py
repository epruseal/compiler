"""Text size distribution of the unusable-date revisions (input for the call cost estimate).

Usage: python3 -I text_stats.py UNUSABLE_JSONL
Counts each field once: 조내용 + 조문내용 + 부칙 + 제개정이유 + 개정문 (the parent tags contain the
child tags 부칙내용, 제개정이유내용, 개정문내용). Prints percentiles per dataset.
"""
import json
import sys
from collections import defaultdict

PARENTS = ('조내용', '조문내용', '부칙', '제개정이유', '개정문')


def pct(values, p):
    if not values:
        return 0
    values = sorted(values)
    return values[min(len(values) - 1, int(p / 100 * len(values)))]


sizes = defaultdict(list)
addenda = defaultdict(list)
for line in open(sys.argv[1], encoding='utf-8'):
    r = json.loads(line)
    if r.get('text_blank'):
        continue
    t = r['text_chars']
    sizes[r['dataset']].append(sum(t[k] for k in PARENTS))
    addenda[r['dataset']].append(t['부칙'] + t['제개정이유'] + t['개정문'])
for ds in sorted(sizes):
    v, a = sizes[ds], addenda[ds]
    print(ds, 'n', len(v), 'total chars', sum(v), 'mean', round(sum(v) / len(v)),
          'p50', pct(v, 50), 'p90', pct(v, 90), 'p99', pct(v, 99), 'max', max(v))
    print('   addendum+reason+amendment chars: mean', round(sum(a) / len(a)), 'p50', pct(a, 50), 'p90', pct(a, 90), 'p99', pct(a, 99), 'max', max(a))
    over = [x for x in v if x > 30000]
    print('   revisions over 30000 chars:', len(over), 'chars in them', sum(over))
