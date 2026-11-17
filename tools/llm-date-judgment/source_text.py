"""Read one source XML into the metadata and text fields that the LLM judge sees.

The fields keep the source order and line breaks. Entity markup that CDATA text carries
literally (&lt; &gt; &amp;) is decoded once, the same way the compiler decodes it.
"""
import hashlib
import re
import xml.etree.ElementTree as ET
from pathlib import Path

BODY_LIMIT = 30000
ADDENDA_LIMIT = 60000
KEEP_LAST_SECTIONS = 8
HEADING_LINE_RE = re.compile(r'^\s*(?:부\s*칙|附\s*則)')
EXCERPT_RE = re.compile(r'시\s*행|효\s*력|부\s*칙|施\s*行')


def decode(text):
    return text.replace('&lt;', '<').replace('&gt;', '>').replace('&amp;', '&')


def text_of(element):
    if element is None:
        return ''
    return decode(''.join(element.itertext()))


def find_text(root, path):
    node = root.find(path)
    return (node.text or '').strip() if node is not None and node.text else ''


def iso(raw):
    raw = (raw or '').strip()
    if re.fullmatch(r'\d{8}', raw):
        return f'{raw[:4]}-{raw[4:6]}-{raw[6:]}'
    return raw


ORD_KINDS = {'C0001': '조례', 'C0002': '규칙', 'C0003': '훈령', 'C0004': '예규', 'C0006': '기타', 'C0010': '고시', 'C0011': '의회규칙'}


def read_source(path, dataset):
    data = Path(path).read_bytes()
    root = ET.fromstring(data)
    meta = {'dataset': dataset, 'source_sha256': hashlib.sha256(data).hexdigest(), 'source_bytes': len(data)}
    fields = {}
    addenda = []
    if dataset == 'ordinances':
        info = root.find('자치법규기본정보')
        get = lambda tag: (info.findtext(tag) or '').strip() if info is not None else ''
        meta.update({'identity': get('자치법규ID'), 'revision': get('자치법규일련번호'), 'name': get('자치법규명'),
                     'kind': ORD_KINDS.get(get('자치법규종류'), get('자치법규종류')), 'issuer': get('지자체기관명'),
                     'promulgation_raw': get('공포일자'), 'promulgation_number': get('공포번호'),
                     'enforcement_raw': get('시행일자'), 'amendment_type': get('제개정정보')})
        lines = []
        for article in root.iter('조'):
            title = text_of(article.find('조제목')).strip()
            body = text_of(article.find('조내용'))
            if body.strip():
                lines.append(body.rstrip('\n'))
            elif title:
                lines.append(title)
        body = '\n'.join(lines)
        if not body.strip():
            body = text_of(root.find('.//조문내용'))
        fields['조문'] = body
    else:
        info = root.find('행정규칙기본정보')
        get = lambda tag: (info.findtext(tag) or '').strip() if info is not None else ''
        meta.update({'identity': get('행정규칙ID'), 'revision': get('행정규칙일련번호'), 'name': get('행정규칙명'),
                     'kind': get('행정규칙종류'), 'issuer': get('소관부처명'),
                     'promulgation_raw': get('발령일자'), 'promulgation_number': get('발령번호'),
                     'enforcement_raw': get('시행일자'), 'amendment_type': get('제개정구분명')})
        fields['조문'] = '\n'.join(text_of(node) for node in root.iter('조문내용'))
    for block in root.iter('부칙'):
        content = text_of(block.find('부칙내용')) if block.find('부칙내용') is not None else text_of(block)
        addenda.append({'promulgation_raw': (block.findtext('부칙공포일자') or '').strip(),
                        'number': (block.findtext('부칙공포번호') or '').strip(), 'text': content})
    fields['부칙'] = '\n'.join(a['text'] for a in addenda)
    fields['제개정이유'] = '\n'.join(text_of(n) for n in root.iter('제개정이유내용'))
    fields['개정문'] = '\n'.join(text_of(n) for n in root.iter('개정문내용'))
    meta['promulgation_date'] = iso(meta['promulgation_raw'])
    meta['addenda'] = addenda
    meta['text_blank'] = all(not re.sub(r'\s+', '', v) for v in fields.values())
    return meta, fields


def body_for_prompt(body):
    """The body as the prompt shows it. A long body becomes an excerpt of the lines that name an
    effective date, an effect or an addendum, with one line of context on each side."""
    if len(body) <= BODY_LIMIT:
        return body, False
    lines = body.split('\n')
    keep = set()
    for i, line in enumerate(lines):
        if EXCERPT_RE.search(line):
            keep.update({i - 1, i, i + 1})
    out = []
    last = -2
    for i in sorted(k for k in keep if 0 <= k < len(lines)):
        if i != last + 1:
            out.append('…')
        out.append(lines[i])
        last = i
    return '\n'.join(out), True


def addenda_for_prompt(text, promulgation_raw, number):
    """The addenda as the prompt shows them. Cumulative addenda longer than ADDENDA_LIMIT become the
    last KEEP_LAST_SECTIONS sections plus every section whose heading line names this revision's
    promulgation date or number, in source order. Sections stay whole with their heading lines. If
    that is still too long, whole sections are dropped from the front; a last section that alone is
    too long keeps its heading line and its end. The mechanical checks still read the whole field
    (user decision 2026-11-16 on the addenda excerpt)."""
    text = text.replace('\r\n', '\n').replace('\r', '\n')
    if len(text) <= ADDENDA_LIMIT:
        return text, False
    lines = text.split('\n')
    starts = [i for i, line in enumerate(lines) if HEADING_LINE_RE.match(line)] or [0]
    if starts[0] != 0:
        starts = [0] + starts
    sections = [(starts[k], starts[k + 1] if k + 1 < len(starts) else len(lines)) for k in range(len(starts))]
    marks = []
    raw = (promulgation_raw or '').strip()
    if len(raw) == 8 and raw.isdigit():
        y, m, d = raw[:4], int(raw[4:6]), int(raw[6:])
        marks.append(re.compile(y + r'\s*[.·ㆍ\-/,년]\s*0?' + str(m) + r'\s*[.·ㆍ\-/,월]\s*0?' + str(d) + r'(?!\d)'))
        marks.append(re.compile(r'(?<!\d)' + y[2:] + r'\s*[.·ㆍ\-/,]\s*0?' + str(m) + r'\s*[.·ㆍ\-/,]\s*0?' + str(d) + r'(?!\d)'))
    if (number or '').strip().isdigit():
        marks.append(re.compile(r'(?<!\d)' + str(int(number)) + r'\s*호'))
    keep = set(range(max(0, len(sections) - KEEP_LAST_SECTIONS), len(sections)))
    for k, (a, _) in enumerate(sections):
        if any(rx.search(lines[a]) for rx in marks):
            keep.add(k)
    def joined(kept):
        parts, last = [], -1
        for k in kept:
            if k != last + 1:
                parts.append('…')
            a, b = sections[k]
            parts.append('\n'.join(lines[a:b]))
            last = k
        return '\n'.join(parts)
    kept = sorted(keep)
    out = joined(kept)
    while len(out) > ADDENDA_LIMIT and len(kept) > 1:
        kept = kept[1:]
        out = joined(kept)
    if len(out) > ADDENDA_LIMIT:
        a, b = sections[kept[0]]
        head = lines[a][:ADDENDA_LIMIT // 2]
        room = ADDENDA_LIMIT - len(head) - 5  # the two marks and three line breaks
        out = '…\n' + head + '\n…\n' + '\n'.join(lines[a + 1:b])[-room:]
    return out, True
