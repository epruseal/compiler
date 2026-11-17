"""Mechanical checks of one LLM judgment against its source text (compiler#2 requirement 3).

The fields arrive decoded once (source_text.py). Comparisons run on compact text: all white space
removed, nothing decoded again. A Field keeps, for each compact character, the quotation depth of
its line and the positions of addendum headings (a line that starts with 부 칙 or 附則, the
compiler#1 heading rule). A judgment with a date is adoptable only when every required check passes:
  outcome_known  the outcome is one of the defined values
  quote_in_field the basis quote is in the named source field
  date_valid     the date is a real calendar date and not 9999-12-31
  verb           the quote holds an enforcement verb in predicate form: 시행 or 施行 followed by a
                 final or connective ending (한다, 합니다, 함, 된다, 됩니다, 됨, 하며, 하고, 하되,
                 하나, 시킨다, sentence end) or 효력 발생 in the same forms. A final ending (one that
                 ends in 다, 함, 됨 or 킴) counts only when the next character of the field is a
                 sentence or clause mark (. 。 , ， ;), a closing mark (" ' ” ’ » ) ] 」 』) or the
                 field end. Every other follower (는, 고, 면, 던, 며, 지, 니, 에, 과 and so on) makes a
                 quoted, connective or nominal use, and the followers are an open set, so the check
                 lists the allowed followers and not the refused ones. A line end is not a sentence
                 end (user decision 2026-11-09: forced line breaks split sentences after 다 and 함;
                 lead-compiler2-line-end-20261116). An attributive or nominal form (시행되는, 시행된,
                 시행됨에 따라, 시행함에 있어) is not a predicate.
  section        when the field holds two or more headings, or for label_date: the section label
                 lies on a heading line and the quote lies inside that heading's section
  not_quoted     the occurrence that passes the section check starts outside quotation marks, and
                 the date or promulgation phrase that gives the date is outside quotation marks
                 (an amendment instruction such as 제2조 중 "…시행한다"를 삭제함 is quoted text).
                 Corner brackets 「」『』 mark law titles and are not tracked.
  derivation     the date comes from the quote:
                   explicit_date     a date expression reads as the date and a predicate verb
                                     follows it directly (only 부터, 로부터, 에, 자로, 자, 그 and a
                                     dot may lie between)
                   promulgation_day  a promulgation (issue) day phrase is followed directly by a
                                     predicate verb, and the date is the valid source promulgation
                                     date
                   label_date        ordinances only; one heading in the field; a label date
                                     expression reads as the date; the date is after the
                                     promulgation date; the quote has the promulgation phrase; the
                                     date is before the next revision's promulgation date when known
  section_not_later_revision  promulgation_day and label_date only: when the next revision's
                 promulgation date is known, the quote's section heading carries no label date on
                 or after it
                 (user-label-not-on-or-after-next-revision-20261103: such a label is the later
                 revision's)
"""
import re
from datetime import date

DATED = ('explicit_date', 'promulgation_day', 'label_date')
UNDATED = ('no_enforcement_clause', 'relative_period', 'conditional_or_external', 'attribution_unclear', 'insufficient_text')

END = '¶'  # end-of-field mark appended to a predicate window; never in source text
# Final endings carry no look-ahead here: Field.anchors reads the character after them (FINAL_TAIL).
ENDING = (r'(?:하기로한다|토록한다|키로한다|한다|합니다|하였다|했다|하며|하고(?![있자])|하되|하나(?![의])|함'
          r'|된다|됩니다|되었다|됐다|되며|되고(?![있자])|되되|됨'
          r'|시킨다|시킵니다|시킴|(?=[.。' + END + r']))')
PRED = (r'(?:(?P<st>시행|施行(?![令規]))' + ENDING +
        r'|(?P<ef>효력)(?:을|이)?발생(?:한다|합니다|하였다|했다|하며|하고(?![있자])|하되|함|(?=[.。' + END + r'])))')
FINAL_ENDS = ('다', '함', '됨', '킴')
FINAL_TAIL = set('.。,，;"\'”’»)]」』')
PRED_RE = re.compile(PRED)
SEP = r'[.·ㆍ\-/,]'
# One grammar for every written date with a four-digit year: 년 or a separator after the year,
# 월 (optionally followed by a separator) or a separator after the month, an optional 일 after the
# day. It covers 2011년 1월 1일, 2009. 6. 27., 1998.10. 7. and the mixed forms 2006. 7. 1일,
# 2009년 1월 1, 1994. 1월 1일 (compiler#1 follow-up items marked as LLM targets).
DATE_ANY = r'(?<!\d)(\d{4})(?:년|' + SEP + r')(\d{1,2})(?:월' + SEP + r'?|' + SEP + r')(\d{1,2})(?!\d)일?'
DATE_TWO = r'(?<!\d)(\d{2})' + SEP + r'(\d{1,2})' + SEP + r'(\d{1,2})(?!\d)'
AFTER_DATE = r'\.?(?:부터|로부터|에|자로|자)?(?:그)?' + PRED
EXPLICIT_RE = re.compile(DATE_ANY + AFTER_DATE)
DATE_RE = re.compile(DATE_ANY)
TWO_RE = re.compile(DATE_TWO)
PROM_ADJ_RE = re.compile(r'(?:공포|발령|공고|고시)(?:한|된|하는|되는)?(?:날|일)(?:로)?(?:부터|에|즉시)?(?:그)?' + PRED)
HEADING_LINE_RE = re.compile(r'^\s*(?:부\s*칙|附\s*則)')
PAIRS = {'“': '”', '‘': '’', '«': '»'}
CLOSERS = {v: k for k, v in PAIRS.items()}
TOGGLES = '"\''
WINDOW_TAIL = 24


def compact(text):
    return re.sub(r'\s+', '', text or '')


class Field:
    """Compact text of one source field with quotation depth, heading positions and line ends.

    text    the compact text (no white space, no line breaks); quotes are searched here and
            enforcement predicates are read here, so a quote that the model cut short cannot make
            its own end look like a sentence end, and a verb split by a forced line break
            (시행 / 되는) is read whole
    depth   quotation depth of each compact character within its line; a line whose marks do not
            pair up (a wrong closer, or an opener left open at the line end) counts as quoted
    """

    def __init__(self, raw):
        chars, depth, self.heads, self.line_starts = [], [], [], []
        for line in (raw or '').replace('\r\n', '\n').replace('\r', '\n').split('\n'):
            self.line_starts.append(len(chars))
            if HEADING_LINE_RE.match(line):
                self.heads.append(len(chars))
            stack, line_chars, line_depth, broken = [], [], [], False
            for ch in line:
                if ch.isspace():
                    continue
                if ch in PAIRS:
                    line_chars.append(ch)
                    line_depth.append(len(stack))
                    stack.append(ch)
                elif ch in CLOSERS:
                    if stack and stack[-1] == CLOSERS[ch]:
                        stack.pop()
                    else:
                        broken = True
                    line_chars.append(ch)
                    line_depth.append(len(stack))
                elif ch in TOGGLES:
                    if stack and stack[-1] == ch:
                        stack.pop()
                        line_chars.append(ch)
                        line_depth.append(len(stack))
                    else:
                        line_chars.append(ch)
                        line_depth.append(len(stack))
                        stack.append(ch)
                else:
                    line_chars.append(ch)
                    line_depth.append(len(stack))
            if broken or stack:
                line_depth = [1] * len(line_chars)
            chars.extend(line_chars)
            depth.extend(line_depth)
        self.text = ''.join(chars)
        self.depth = depth
        self.line_starts.append(len(chars))

    def occurrences(self, quote):
        out = []
        pos = self.text.find(quote) if quote else -1
        while pos >= 0:
            out.append(pos)
            pos = self.text.find(quote, pos + 1)
        return out

    def line_end(self, pos):
        return min((s for s in self.line_starts if s > pos), default=len(self.text))

    def on_heading_line(self, pos):
        return any(h <= pos < self.line_end(h) for h in self.heads)

    def section_of(self, pos):
        start = max((h for h in self.heads if h <= pos), default=None)
        end = min((h for h in self.heads if h > pos), default=len(self.text))
        return start, end

    def in_section(self, label, quote, pos):
        """The quote at pos lies inside one heading's section and the label is on that heading line."""
        start, end = self.section_of(pos)
        if pos + len(quote) > end or start is None or not label:
            return False
        if label in quote:
            return self.on_heading_line(pos + quote.find(label))
        lpos = self.text.find(label, start)
        return 0 <= lpos < self.line_end(start) and lpos + len(label) <= pos

    def final_ok(self, q, matched):
        """A match that ends in a final ending is a predicate only when the field character at q is
        an allowed follower or q is the field end."""
        if not matched.endswith(FINAL_ENDS):
            return True
        return q >= len(self.text) or self.text[q] in FINAL_TAIL

    def free(self, pos):
        return 0 <= pos < len(self.depth) and self.depth[pos] == 0

    def anchors(self, quote, pos, regex, value_of):
        """Predicate matches read in the field around the quote at pos. A match must start inside the
        quote and its verb stem must lie inside the quote; the ending is read from the field, so a
        cut quote does not hide the field text that follows. Returns (date, start_free, end_free)."""
        if not quote:
            return []
        stop = pos + len(quote) + WINDOW_TAIL
        window = self.text[pos:stop] + (END if stop >= len(self.text) else '')
        span = len(quote)
        out = []
        for m in regex.finditer(window):
            stem = m.start('st') if m.group('st') else m.start('ef')
            if m.start() >= span or stem + 2 > span:
                continue
            if not self.final_ok(pos + m.end(), m.group(0)):
                continue
            value = value_of(m)
            if value is None:
                continue
            out.append((value, self.free(pos + m.start()), self.free(pos + stem)))
        return out


def real_date(y, m, d):
    try:
        value = date(int(y), int(m), int(d))
    except ValueError:
        return None
    return None if value == date(9999, 12, 31) else value


def parse_iso(text):
    m = re.fullmatch(r'(\d{4})-(\d{2})-(\d{2})', text or '')
    return real_date(*m.groups()) if m else None


def explicit_value(m):
    return real_date(*m.groups()[:3])


def label_dates(label, promulgation):
    found = set()
    for m in DATE_RE.finditer(label):
        value = real_date(*m.groups())
        if value:
            found.add(value)
    if promulgation is not None:
        for m in TWO_RE.finditer(label):
            yy, mm, dd = m.groups()
            options = [o for o in (real_date(1900 + int(yy), mm, dd), real_date(2000 + int(yy), mm, dd)) if o]
            if len(options) == 2:
                d0, d1 = (abs((o - promulgation).days) for o in options)
                if d0 != d1:
                    found.add(options[0] if d0 < d1 else options[1])
            elif len(options) == 1:
                found.add(options[0])
    return found


def check(judgment, meta, fields, next_promulgation=None):
    """Returns (adoptable, results). results maps each check name to True, False or None (not run)."""
    outcome = judgment.get('outcome')
    results = {'outcome_known': outcome in DATED + UNDATED}
    quote = compact(judgment.get('basis_quote'))
    field_name = judgment.get('basis_field') or ''
    field = Field(fields.get(field_name, '') if field_name else '')
    occs = field.occurrences(quote)
    results['quote_in_field'] = bool(occs) if quote else None
    every = compact('\n'.join(fields.values()))
    pq = compact(judgment.get('proviso_quote'))
    results['proviso_quote_in_source'] = (pq in every) if pq else None
    partial = judgment.get('partial_enforcement') or []
    results['partial_quotes_in_source'] = all(compact(p.get('quote')) and compact(p.get('quote')) in every for p in partial) if partial else None
    prom = parse_iso(meta.get('promulgation_date'))
    if outcome not in DATED:
        results['date_empty'] = (judgment.get('date') or '') == ''
        return False, results
    value = parse_iso(judgment.get('date'))
    results['date_valid'] = value is not None
    results['verb'] = any(field.anchors(quote, p, PRED_RE, lambda m: True) for p in occs)
    label = compact(judgment.get('section_label'))
    if len(field.heads) >= 2 or outcome == 'label_date':
        occs = [p for p in occs if field.in_section(label, quote, p)]
        results['section'] = bool(occs)
    if outcome == 'explicit_date':
        found = {p: [a for a in field.anchors(quote, p, EXPLICIT_RE, explicit_value) if a[0] == value] for p in occs}
    else:
        found = {p: field.anchors(quote, p, PROM_ADJ_RE, lambda m: prom) if prom else [] for p in occs}
    if outcome == 'explicit_date':
        results['derivation'] = value is not None and any(found.values())
    elif outcome == 'promulgation_day':
        results['derivation'] = value is not None and value == prom and any(found.values())
    else:
        results['single_addendum'] = len(field.heads) <= 1
        results['derivation'] = (meta.get('dataset') == 'ordinances' and value is not None and prom is not None
                                 and value > prom and any(found.values()) and value in label_dates(label, prom))
        if next_promulgation:
            nxt = parse_iso(next_promulgation)
            results['before_next_revision'] = nxt is not None and value is not None and value < nxt
    nxt = parse_iso(next_promulgation) if next_promulgation else None
    if nxt is not None and field.heads and outcome != 'explicit_date':
        # user-label-not-on-or-after-next-revision-20261103: a heading label dated on or after the
        # next revision's promulgation belongs to that later revision, so its section is not this one's.
        # An explicit date keeps priority over labels (user-explicit-date-kept-20261104: label
        # attribution applies only to effective-date sentences without a date), so explicit_date skips it.
        def own(p):
            start, _ = field.section_of(p)
            if start is None:
                return True
            heading = field.text[start:field.line_end(start)]
            return not any(d >= nxt for d in label_dates(heading, prom))
        occs = [p for p in occs if own(p)]
        results['section_not_later_revision'] = bool(occs)
    results['not_quoted'] = any(field.free(p) and any(a[1] and a[2] for a in found[p]) for p in occs)
    required = ['outcome_known', 'quote_in_field', 'date_valid', 'verb', 'not_quoted', 'derivation']
    if 'section' in results:
        required.append('section')
    if 'section_not_later_revision' in results:
        required.append('section_not_later_revision')
    if outcome == 'label_date':
        required.append('single_addendum')
        if 'before_next_revision' in results:
            required.append('before_next_revision')
    return all(results.get(k) is True for k in required), results
