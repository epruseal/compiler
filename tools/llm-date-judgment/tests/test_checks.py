"""Unit tests of the mechanical checks."""
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import checks  # noqa: E402


def judgment(outcome, date, quote, field='부칙', label=''):
    return {'outcome': outcome, 'date': date, 'basis_quote': quote, 'basis_field': field, 'section_label': label,
            'proviso': False, 'proviso_quote': '', 'partial_enforcement': [], 'attribution': '', 'rules_applied': [], 'reason': ''}


def meta(prom, dataset='ordinances'):
    return {'dataset': dataset, 'promulgation_date': prom}


class Counterexamples(unittest.TestCase):
    def test_application_clause_date_is_not_an_effective_date(self):
        text = '부칙 <2012.03.08>\n제1조(시행일) 이 규칙은 공포한 날부터 시행한다.\n제2조(개정규칙 시행에 따른 적용례) 이 규칙은 2012년 1월 1일 이후 시행되는 임용시험부터 적용한다.'
        q = '이 규칙은 2012년 1월 1일 이후 시행되는 임용시험부터 적용한다.'
        ok, r = checks.check(judgment('explicit_date', '2012-01-01', q), meta('2012-03-08'), {'부칙': text})
        self.assertFalse(ok)
        self.assertFalse(r['derivation'])

    def test_label_date_inside_quote_is_not_an_explicit_date(self):
        text = '부     칙 (2002. 8. 1.) 제1조(시행일) 이 조례는 공포한 날부터 시행한다.'
        ok, r = checks.check(judgment('explicit_date', '2002-08-01', text), meta('2002-08-21'), {'부칙': text})
        self.assertFalse(ok)
        ok, r = checks.check(judgment('promulgation_day', '2002-08-21', text), meta('2002-08-21'), {'부칙': text})
        self.assertTrue(ok, r)

    def test_relative_period_is_not_the_promulgation_day(self):
        text = '부칙\n이 규칙은 공포한 날부터 6개월이 경과한 날부터 시행한다.'
        q = '이 규칙은 공포한 날부터 6개월이 경과한 날부터 시행한다.'
        ok, r = checks.check(judgment('promulgation_day', '2010-01-05', q), meta('2010-01-05'), {'부칙': text})
        self.assertFalse(ok)
        self.assertFalse(r['derivation'])


class ExplicitDates(unittest.TestCase):
    def accept(self, quote, value):
        ok, r = checks.check(judgment('explicit_date', value, quote, field='개정문'), meta('2000-01-01', 'admrules'), {'개정문': '부  칙\n' + quote})
        return ok, r

    def test_forms(self):
        for quote, value in [('이 예규는 2009. 6. 27.부터 시행한다.', '2009-06-27'),
                             ('이 예규는 1998.10. 7.부터 시행한다. ', '1998-10-07'),
                             ('이 규칙은 1996년 7월 1일 시행한다.', '1996-07-01'),
                             ('이 조례는 2010년 1월 1일부터 시행한다.', '2010-01-01'),
                             ('이 규칙은 2011년 1월 1일부터 그 효력을 발생한다.', '2011-01-01')]:
            ok, r = self.accept(quote, value)
            self.assertTrue(ok, (quote, r))

    def test_wrong_date_and_noun_verb(self):
        self.assertFalse(self.accept('이 예규는 2009. 6. 27.부터 시행한다.', '2009-06-28')[0])
        self.assertFalse(self.accept('2009. 6. 27. 시행령 개정에 따른다.', '2009-06-27')[0])
        self.assertFalse(self.accept('이 예규는 2009. 6.  .부터 시행한다.', '2009-06-01')[0])

    def test_quote_must_be_in_field(self):
        ok, r = checks.check(judgment('explicit_date', '2009-06-27', '이 예규는 2009. 6. 27.부터 시행한다.', field='부칙'), meta('2000-01-01', 'admrules'), {'부칙': '', '개정문': '이 예규는 2009. 6. 27.부터 시행한다.'})
        self.assertFalse(ok)
        self.assertFalse(r['quote_in_field'])


class Sections(unittest.TestCase):
    TEXT = ('부칙 <2011.12.29>\n이 규칙은 2012년 1월 10일부터 시행하되, 제3조는 2012년 7월 1일부터 시행한다.\n'
            '부칙 <2012.03.08>\n제1조(시행일) 이 규칙은 공포한 날부터 시행한다.')

    def test_label_must_head_the_quote(self):
        q = '이 규칙은 2012년 1월 10일부터 시행하되'
        ok, r = checks.check(judgment('explicit_date', '2012-01-10', q, label='<2012.03.08>'), meta('2012-03-08'), {'부칙': self.TEXT})
        self.assertFalse(ok)
        self.assertFalse(r['section'])

    def test_own_section(self):
        q = '이 규칙은 공포한 날부터 시행한다.'
        ok, r = checks.check(judgment('promulgation_day', '2012-03-08', q, label='<2012.03.08>'), meta('2012-03-08'), {'부칙': self.TEXT})
        self.assertTrue(ok, r)
        ok, r = checks.check(judgment('promulgation_day', '2012-03-08', q, label='<2011.12.29>'), meta('2012-03-08'), {'부칙': self.TEXT})
        self.assertFalse(ok)

    def test_multi_section_requires_label(self):
        q = '이 규칙은 공포한 날부터 시행한다.'
        ok, r = checks.check(judgment('promulgation_day', '2012-03-08', q, label=''), meta('2012-03-08'), {'부칙': self.TEXT})
        self.assertFalse(ok)


class LabelDates(unittest.TestCase):
    TEXT = '부 칙 (2006.6.28)\n이 조례는 공포한 날부터 시행한다.'
    Q = '이 조례는 공포한 날부터 시행한다.'

    def test_label_later_than_promulgation(self):
        ok, r = checks.check(judgment('label_date', '2006-06-28', self.Q, label='(2006.6.28)'), meta('2006-06-20'), {'부칙': self.TEXT})
        self.assertTrue(ok, r)

    def test_next_revision_bound(self):
        ok, r = checks.check(judgment('label_date', '2006-06-28', self.Q, label='(2006.6.28)'), meta('2006-06-20'), {'부칙': self.TEXT}, '2006-06-28')
        self.assertFalse(ok)
        self.assertFalse(r['before_next_revision'])

    def test_two_headings_are_not_a_single_addendum(self):
        text = '부칙 <2001.1.1>\n이 조례는 공포한 날부터 시행한다.\n' + self.TEXT
        ok, r = checks.check(judgment('label_date', '2006-06-28', self.Q, label='(2006.6.28)'), meta('2006-06-20'), {'부칙': text})
        self.assertFalse(ok)
        self.assertFalse(r['single_addendum'])

    def test_admrules_have_no_label_date(self):
        ok, r = checks.check(judgment('label_date', '2006-06-28', self.Q, label='(2006.6.28)'), meta('2006-06-20', 'admrules'), {'부칙': self.TEXT})
        self.assertFalse(ok)

    def test_two_digit_year_nearest_century(self):
        text = '부 칙 <99.2.18>\n이 조례는 공포한 날부터 시행한다.'
        ok, r = checks.check(judgment('label_date', '1999-02-18', self.Q, label='<99.2.18>'), meta('1999-02-10'), {'부칙': text})
        self.assertTrue(ok, r)


class Undated(unittest.TestCase):
    def test_undated_outcome_is_never_adoptable(self):
        ok, r = checks.check(judgment('relative_period', '', '이 조례는 공포 후 1개월이 경과한 날부터 시행한다.'), meta('2007-10-31'), {'부칙': '이 조례는 공포 후 1개월이 경과한 날부터 시행한다.'})
        self.assertFalse(ok)
        self.assertTrue(r['date_empty'])


class FollowupForms(unittest.TestCase):
    """compiler#1 follow-up items that the lead marked as LLM targets (followup-candidates-issue1.md)."""

    def accept(self, quote, value, field_text=None):
        text = field_text if field_text is not None else '부칙\n' + quote
        return checks.check(judgment('explicit_date', value, quote), meta('2000-01-01'), {'부칙': text})

    def test_mixed_dates(self):
        for quote, value in [('이 조례는 2006. 7. 1일부터 시행한다.', '2006-07-01'),
                             ('이 조례는 2013.12.12일부터 시행한다.', '2013-12-12'),
                             ('이 조례는 2009년 1월 1부터 시행한다.', '2009-01-01'),
                             ('이 조례는 1994. 1월 1일부터 시행한다.', '1994-01-01')]:
            ok, r = self.accept(quote, value)
            self.assertTrue(ok, (quote, r))

    def test_date_without_year_is_not_explicit(self):
        ok, r = self.accept('이 조례는 1. 1일부터 시행한다.', '2000-01-01')
        self.assertFalse(ok)

    def test_quoted_amendment_instruction_is_not_a_clause(self):
        text = '부칙 제2조 중 "이 조례는 공포한 날부터 시행한다"를 삭제함.'
        q = '이 조례는 공포한 날부터 시행한다'
        ok, r = checks.check(judgment('promulgation_day', '2024-03-01', q, field='제개정이유'), meta('2024-03-01'), {'제개정이유': text})
        self.assertFalse(ok)
        self.assertFalse(r['not_quoted'])

    def test_unquoted_copy_of_the_same_sentence_passes(self):
        text = '부칙 제2조 중 "이 조례는 공포한 날부터 시행한다"를 삭제함.\n부칙\n이 조례는 공포한 날부터 시행한다.'
        q = '이 조례는 공포한 날부터 시행한다'
        ok, r = checks.check(judgment('promulgation_day', '2024-03-01', q, field='제개정이유', label='부칙'), meta('2024-03-01'), {'제개정이유': text})
        self.assertTrue(r['not_quoted'], r)


class PredicateAndQuotes(unittest.TestCase):
    """The checks refuse attributive and nominal verb forms, quoted text and amendment instructions."""

    def test_attributive_verb_after_date_is_not_a_clause(self):
        text = '부칙\n제1조(시행일) 이 규칙은 공포한 날부터 시행한다.\n제2조(적용례) 이 규칙은 2012년 1월 1일부터 시행되는 임용시험부터 적용한다.'
        q = '이 규칙은 2012년 1월 1일부터 시행되는 임용시험부터 적용한다.'
        ok, r = checks.check(judgment('explicit_date', '2012-01-01', q), meta('2012-03-08'), {'부칙': text})
        self.assertFalse(ok)
        self.assertFalse(r['derivation'])

    def test_nominal_verb_in_reason_text_is_not_a_clause(self):
        text = '상위 법령이 2009. 6. 27.부터 시행됨에 따라 관련 조문을 정비함.'
        ok, r = checks.check(judgment('explicit_date', '2009-06-27', text, field='제개정이유'), meta('2009-04-10', 'admrules'), {'제개정이유': text})
        self.assertFalse(ok)
        q2 = '이 조례는 2010. 1. 1.부터 시행함에 있어 필요한 사항을 정한다.'
        ok, r = checks.check(judgment('explicit_date', '2010-01-01', q2, field='제개정이유'), meta('2009-04-10', 'admrules'), {'제개정이유': q2})
        self.assertFalse(ok)
        self.assertFalse(r['verb'])

    def test_quote_inside_quotation_marks_cut_or_whole(self):
        text = '부칙 제2조 중 "이 조례는 공포한 날부터 시행한다"를 삭제함.'
        for q in ('공포한 날부터 시행한다', text):
            ok, r = checks.check(judgment('promulgation_day', '2024-03-01', q, field='제개정이유'), meta('2024-03-01'), {'제개정이유': text})
            self.assertFalse(ok, (q, r))
            self.assertFalse(r['not_quoted'])

    def test_polite_endings_and_law_title_brackets(self):
        for quote in ('이 규칙은 2020년 1월 1일부터 시행합니다.', '이 규칙은 2020년 1월 1일부터 시행됩니다.',
                      '「지방자치법」에 따라 이 조례는 2020년 1월 1일부터 시행한다.'):
            ok, r = checks.check(judgment('explicit_date', '2020-01-01', quote), meta('2019-12-01'), {'부칙': '부칙\n' + quote})
            self.assertTrue(ok, (quote, r))

    def test_inline_mention_of_addendum_is_not_a_heading(self):
        text = '부칙\n제1조 이 조례는 공포한 날부터 시행한다.\n제2조 종전의 부칙 제3조는 이 조례에 따른다.'
        q = '이 조례는 공포한 날부터 시행한다.'
        ok, r = checks.check(judgment('promulgation_day', '2010-05-01', q), meta('2010-05-01'), {'부칙': text})
        self.assertTrue(ok, r)
        self.assertNotIn('section', r)


class FieldReadPredicates(unittest.TestCase):
    """The checks read the predicate in the field, not at the end of the quote."""

    def check_explicit(self, text, quote, value, field='부칙'):
        return checks.check(judgment('explicit_date', value, quote, field=field), meta('2000-01-01'), {field: text})

    def test_cut_quotes_do_not_end_a_sentence(self):
        cases = [
            ('부칙\n제2조(적용례) 이 규칙은 2012년 1월 1일부터 시행되는 임용시험부터 적용한다.', '이 규칙은 2012년 1월 1일부터 시행', '2012-01-01', '부칙'),
            ('상위 법령이 2009. 6. 27.부터 시행됨에 따라 관련 조문을 정비함.', '상위 법령이 2009. 6. 27.부터 시행', '2009-06-27', '제개정이유'),
            ('외국인토지법(법률 제9186호, 2008. 12. 26. 공포, 2009. 6. 27. 시행) 제4조제2항의 일부개정에 따라', '2009. 6. 27. 시행', '2009-06-27', '제개정이유'),
        ]
        for text, quote, value, field in cases:
            ok, r = self.check_explicit(text, quote, value, field)
            self.assertFalse(ok, (quote, r))

    def test_continuing_and_nominal_forms(self):
        for text in ('2012. 1. 1.부터 시행되고 있는 제도의 미비점을 개선함.',
                     '이 조례는 2012. 1. 1.부터 시행된다는 점을 고려한다.',
                     '이 조례는 2012. 1. 1.부터 시행함과 동시에 종전 규칙을 폐지한다.',
                     '이 조례는 2012. 1. 1.부터 시행하고 있는 제도를 고친다.',
                     '이 조례는 2012. 1. 1.부터 시행한다는 점을 고려한다.'):
            ok, r = self.check_explicit(text, text, '2012-01-01', '제개정이유')
            self.assertFalse(ok, (text, r))

    def test_forced_line_break_inside_the_verb(self):
        text = '부칙\n이 규칙은 2012년 1월 1일부터 시행\n되는 임용시험부터 적용한다.'
        ok, r = self.check_explicit(text, '이 규칙은 2012년 1월 1일부터 시행', '2012-01-01')
        self.assertFalse(ok, r)
        text2 = '부칙\n이 규칙은 2012년 1월 1일부터 시행\n한다.'
        ok, r = self.check_explicit(text2, '이 규칙은 2012년 1월 1일부터 시행한다.', '2012-01-01')
        self.assertTrue(ok, r)

    def test_bare_stem_with_period_and_field_end(self):
        ok, r = self.check_explicit('부칙\n이 규칙은 2012년 1월 1일부터 시행.', '이 규칙은 2012년 1월 1일부터 시행.', '2012-01-01')
        self.assertTrue(ok, r)
        ok, r = self.check_explicit('부칙\n이 규칙은 2012년 1월 1일부터 시행', '이 규칙은 2012년 1월 1일부터 시행', '2012-01-01')
        self.assertTrue(ok, r)

    def test_label_inside_quote_must_be_on_heading_line(self):
        text = '부 칙\n이 조례는 공포한 날부터 시행한다. 다만, 제5조는 2006. 7. 1.부터 시행한다.'
        q = '이 조례는 공포한 날부터 시행한다. 다만, 제5조는 2006. 7. 1.부터 시행한다.'
        ok, r = checks.check(judgment('label_date', '2006-07-01', q, label='2006. 7. 1'), meta('2006-06-20'), {'부칙': text})
        self.assertFalse(ok, r)

    def test_mismatched_quotation_marks_make_the_line_quoted(self):
        text = '부칙\n가 "나” 제2조 중 "이 조례는 공포한 날부터 시행한다"를 삭제함'
        ok, r = checks.check(judgment('promulgation_day', '2010-01-01', '이 조례는 공포한 날부터 시행한다'), meta('2010-01-01'), {'부칙': text})
        self.assertFalse(ok, r)


class LaterRevisionLabel(unittest.TestCase):
    """Gold-run finding: a heading labelled with the next revision's date is that revision's addendum."""

    TEXT = '부　　칙 <개정 2004.01.26 규칙제333호)\n제1조(시행일) 이 규칙은 공포한 날부터 시행한다.'
    Q = '제1조(시행일) 이 규칙은 공포한 날부터 시행한다.'

    def test_next_revision_label_rejects(self):
        ok, r = checks.check(judgment('promulgation_day', '2001-07-30', self.Q, label='<개정 2004.01.26 규칙제333호)'), meta('2001-07-30'), {'부칙': self.TEXT}, '2004-01-26')
        self.assertFalse(ok)
        self.assertFalse(r['section_not_later_revision'])

    def test_unknown_or_later_next_revision_keeps(self):
        ok, r = checks.check(judgment('promulgation_day', '2001-07-30', self.Q, label='<개정 2004.01.26 규칙제333호)'), meta('2001-07-30'), {'부칙': self.TEXT}, '2005-01-01')
        self.assertTrue(ok, r)


    def test_explicit_date_keeps_priority_over_label(self):
        text = '부 칙 (전문개정 1999·12·30 규칙 제159호)\n이 규칙은 1999년 1월 1일부터 시행한다.'
        q = '이 규칙은 1999년 1월 1일부터 시행한다.'
        ok, r = checks.check(judgment('explicit_date', '1999-01-01', q, label='(전문개정 1999·12·30 규칙 제159호)'), meta('1998-12-30'), {'부칙': text}, '1999-11-25')
        self.assertTrue(ok, r)


if __name__ == '__main__':
    unittest.main()
