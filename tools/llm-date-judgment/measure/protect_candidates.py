"""Extract every revision number that the decision records mention, with its locations and a
user-decision context flag (input for the D1 protection list, lead-user-decision-protection-20261116).

Usage: python3 -I protect_candidates.py ISSUES_DIR DOCS_DIR OUT_JSON

ISSUES_DIR holds compiler1-body.md (compiler#1 body), compiler1-comments-rest.json (every compiler#1
comment from the REST API) and compiler2.json (compiler#2 from gh issue view --json body).
DOCS_DIR holds enforcement-date-exceptions.md and enforcement-date-exceptions-2.md.

Sources (read only): compiler#1 issue body, every compiler#1 comment, compiler#2 issue body, the two
exception documents. A number is a candidate when it has 6 or 7 digits, or 13 digits starting with
2200000. 8-digit numbers (dates) and 10-digit numbers (comment ids) are not candidates. A range written as "N부터 M까지" adds
the numbers between N and M when the span is at most 100.
A location has user-decision context when its unit holds a user-decision marker. The unit is the
whole comment for a comment whose title line names a user decision, else the paragraph.
"""
import json
import re
import sys
from collections import defaultdict
from pathlib import Path

NUM = re.compile(r'(?<!\d)(?<!\d\.)(2200000\d{6}|\d{6,7})(?!\d)(?!\.\d)')
MARK = re.compile(r'사용자 결정|사용자 정정|사용자 확인|사용자 선택|사용자 재정|질문 도구|user-[a-z0-9-]+|사용자는')
RANGE = re.compile(r'(?<!\d)(\d{6,7})부터\s*(\d{6,7})까지')
TITLE_MARK = re.compile(r'사용자 (결정|재정|정정|확인)')


def units_of_text(name, text):
    out = []
    line = 1
    for para in re.split(r'\n\s*\n', text):
        out.append((f'{name}:{line}', para))
        line += para.count('\n') + 2
    return out


def main():
    ISSUES, DOCS = Path(sys.argv[1]), Path(sys.argv[2])
    sources = []
    body = (ISSUES / 'compiler1-body.md').read_text(encoding='utf-8')
    sources += [(loc, para, bool(MARK.search(para))) for loc, para in units_of_text('compiler1-body.md', body)]
    for c in json.loads((ISSUES / 'compiler1-comments-rest.json').read_text(encoding='utf-8')):
        text = c['body']
        title = text.strip().splitlines()[0] if text.strip() else ''
        whole = bool(TITLE_MARK.search(title)) and '대체됨' not in title
        for loc, para in units_of_text(f'comment {c["id"]}', text):
            sources.append((loc, para, whole or bool(MARK.search(para))))
    c2 = json.loads((ISSUES / 'compiler2.json').read_text(encoding='utf-8'))
    sources += [(loc, para, bool(MARK.search(para))) for loc, para in units_of_text('compiler2-body', c2['body'])]
    for doc in ('enforcement-date-exceptions.md', 'enforcement-date-exceptions-2.md'):
        text = (DOCS / doc).read_text(encoding='utf-8')
        sources += [(loc, para, bool(MARK.search(para))) for loc, para in units_of_text(doc, text)]
    found = defaultdict(list)
    for loc, para, decision in sources:
        for m in RANGE.finditer(para):
            lo, hi = int(m.group(1)), int(m.group(2))
            if 0 < hi - lo <= 100:
                for n in range(lo + 1, hi):
                    found[str(n)].append({'loc': loc, 'user_decision_context': decision, 'range': m.group(0),
                                          'excerpt': re.sub(r'\s+', ' ', para[max(0, m.start() - 90):m.end() + 110])})
        for m in NUM.finditer(para):
            start = max(0, m.start() - 90)
            found[m.group(1)].append({'loc': loc, 'user_decision_context': decision,
                                      'excerpt': re.sub(r'\s+', ' ', para[start:m.end() + 110])})
    rows = []
    for number in sorted(found, key=lambda n: (len(n), n)):
        locs = found[number]
        rows.append({'number': number, 'mentions': len(locs),
                     'user_decision_context': any(l['user_decision_context'] for l in locs), 'locations': locs})
    Path(sys.argv[3]).write_text(json.dumps(rows, ensure_ascii=False, indent=1), encoding='utf-8')
    print('candidates', len(rows), 'with user-decision context', sum(r['user_decision_context'] for r in rows))


if __name__ == '__main__':
    main()
