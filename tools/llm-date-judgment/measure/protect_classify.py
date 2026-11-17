"""Classify every candidate number of protect_candidates.py and write the protection list.

Usage: python3 -I protect_classify.py CANDIDATES_JSON CACHE_ROOT OUT_DIR

Fails when a candidate has no class or a class names a number that is not a candidate. The
protected rows get identity and SHA-256 of every current source copy (ordinance and history).
Decision: lead-user-decision-protection-20261116.
"""
import hashlib
import json
import re
import sys
from pathlib import Path

PROTECTED = {
    '1000238': ('20071201', 'approval registry; comment 6437632923 (user decision, one-month calculation)'),
    '1011016': ('20110101', 'approval registry; issue body (explicit date earlier than promulgation kept as written), comment 6571062276'),
    '2200000088451': ('20090627', 'approval registry; user-amendment-explicit-dates-20261113 (comment 6841451678)'),
    '2200000104677': ('19981007', 'approval registry; user-amendment-explicit-dates-20261113 (comment 6841451678)'),
    '1063983': ('20070601', 'approval registry; user-1063983-explicit-clause-date-20261113 (comment 6841521863)'),
    '1006223': ('20020821', 'comment 6437566094 (2026-10-27 user correction), comment 6593104875 (user decision lists the result)'),
    '847263': ('20060628', 'comment 6593104875 (user decision lists the result)'),
    '806099': ('20080321', 'comment 6593104875 (user decision lists the result)'),
    '1081693': ('20141021', 'comment 6437602564 (2026-10-27 user confirmation), comment 6571062276'),
    '1079521': ('19991231', 'comment 6437602564 (2026-10-27 user confirmation), comment 6571062276'),
    '873698': ('19990101', 'user-explicit-date-kept-20261104 (comment 6623245364), comments 6624108041 and 6731875583'),
    '1018380': ('20100101', 'user-explicit-date-kept-20261104 (comment 6623245364 names the result that the chosen option keeps)'),
    '2113963': ('19960701', 'user-explicit-date-without-buteo-20261104 (comment 6730055022 names the result)'),
    '879919': ('keep current behaviour (21-day default)', 'user-879919-twenty-day-kept-20261104 (comment 6624108041 item 5)'),
    '931559': ('promulgation day (body clause first)', 'issue body item: the chosen option "본문 시행 문구 우선" gives the promulgation day for this example'),
    '509050': ('enactment, not repeal', 'comment 6620840356 user decision 3 names the result (repeal-and-enact with articles is an enactment)'),
    '2120871': ('repealed from 2026-04-10', 'comment 6620840356 user decision 3 names the result (user-repeal-and-enact-repeal-only-is-repeal-20261103)'),
    '1025947': ('20110101', 'enforcement-date-exceptions.md E-CASES: the sentence gives 2011-01-01 and names user-proviso-scope-enforcement-sentence-20261103'),
    '496220': ('19990101 (valid source effective date, replayed on it)', 'issue body item: example of the broken promulgation date question; user selection "시행일로 재생, 정렬은 다음 키로" fixes the result'),
    '2200000097543': ('19950825', 'issue body item: user selection (2026-10-26) "부칙처럼 해석" names this example and its sentence 본 지침은 1995년 8월 25일부터 시행한다 fixes one date under the chosen option'),
}

NAMED_UNRESOLVED = {  # the user named the revision and left it unresolved: decision C makes it an LLM target
    '814749': 'comment 6620840356 user decision 1 (label of the next revision)',
    '814378': 'comment 6620840356 user decision 1',
    '935863': 'comment 6620840356 user decision 1',
    '780669': 'comment 6620840356 user decision 1',
    '932910': 'comment 6620840356 user decision 2 example',
    '1045663': 'user-hadoe-proviso-20261110 (comment 6763095088)',
    '817067': 'user-hadoe-proviso-20261110 (comment 6763095088)',
    '1151198': 'user-hadoe-proviso-20261110 and user-broad-detection-20261110 (comment 6763095088)',
    '1164288': 'user-r22-full-vocabulary-20261111 accepted change (comment 6784389397)',
    '837988': 'user-r22-full-vocabulary-20261111', '934923': 'user-r22-full-vocabulary-20261111',
    '853867': 'user-r22-full-vocabulary-20261111; also in the lead ruling of 13 partial-enforcement revisions',
    '939409': 'user-r22-full-vocabulary-20261111',
    '2200000090201': 'user-r22-full-vocabulary-20261111', '2200000090501': 'user-r22-full-vocabulary-20261111',
    '2200000094443': 'user-r22-full-vocabulary-20261111', '2200000097123': 'user-r22-full-vocabulary-20261111',
    '2200000101907': 'user-r22-full-vocabulary-20261111',
    '638550': 'issue body: copy pair group C; user chose "A만 합침, 나머지 미해결"',
    '638551': 'issue body: copy pair group C; user chose "A만 합침, 나머지 미해결"',
    '2094753': 'issue body: copy pair group D; user chose "A만 합침, 나머지 미해결"',
    '638543': 'exceptions document E-DUP and comment 6410023657: copy pair group C (638543부터 638551까지)',
    '638544': 'comment 6410023657: copy pair group C (range 638543부터 638551까지); user chose "A만 합침, 나머지 미해결"',
    '638545': 'copy pair group C (range)', '638546': 'copy pair group C (range)', '638547': 'copy pair group C (range)',
    '638548': 'copy pair group C (range)', '638549': 'copy pair group C (range)',
}

LEAD_OR_MEASURED = {  # results come from lead rulings, lead corrections or measured parser output, not user decisions
    '687475': 'comment 6620840356 lead ruling 5 (13 partial-enforcement revisions)', '704732': 'lead ruling 5',
    '912509': 'lead ruling 5', '917991': 'lead ruling 5', '918040': 'lead ruling 5', '921824': 'lead ruling 5',
    '923482': 'lead ruling 5', '964107': 'lead ruling 5', '995940': 'lead ruling 5', '1079520': 'lead ruling 5',
    '1176220': 'lead ruling 5',
    '1001316': 'comment 6601455686 lead correction; exceptions document r14 measured', '2114201': 'lead correction', '1056142': 'lead correction',
    '1081604': 'lead correction', '840056': 'lead correction', '1163937': 'lead correction', '1001786': 'lead correction',
    '1089045': 'exceptions document r14 measured', '1038457': 'exceptions document r14 measured', '1038920': 'r14 measured unresolved (rule result)',
    '932911': 'r14 measured', '1139512': 'lead fix, r14 measured', '959662': 'r14 measured', '969605': 'r14 measured',
    '1148409': 'r14 measured', '669104': 'r14 measured unresolved (proviso rule)', '734546': 'r14 measured unresolved',
    '1138933': 'r14 measured unresolved', '816776': 'r14 measured unresolved', '828108': 'r14 measured unresolved',
    '1076163': 'r14 measured unresolved, user confirmation item', '912881': 'r14 measured unresolved, user confirmation item',
    '812096': 'r14 measured (label rule result)', '819124': 'r14 measured (label rule result)',
    '1038222': 'exceptions document: rule result unresolved', '1088912': 'rule result unresolved', '799970': 'rule result unresolved',
    '1126690': 'rule result unresolved', '1139146': 'rule result unresolved',
    '1048338': 'comment 6624108041 lead ruling 7 prediction', '1089196': 'lead ruling 7 prediction',
    '668282': 'lead ruling, valid source date', '680931': 'lead record, unresolved', '694865': 'lead criterion', '1060098': 'lead record, valid source date',
    '1175019': 'approved 21-day control (rule result)',
    '2200000102323': 'issue body observation; no decision', '1071782': 'issue body example of the reason-field rule; exceptions document rule result',
    '891343': 'issue body example of the reason-field rule; rule result',
    '680996': 'issue body example; no result stated', '2200000091441': 'issue body example; no result stated',
    '722881': 'issue body measurement note',
    '659799': 'comment 6410023657 lead measurement table (broken date example)', '904643': 'comment 6410023657 lead measurement table',
    '1021168': 'comment 6410023657 lead measurement table', '1072547': 'comment 6410023657 lead measurement table',
    '1117406': 'comment 6410023657 lead measurement table', '933418': 'exceptions document example of a line end; no result', '925634': 'comment 6791425420 expected change note; no result',
    '906750': 'exceptions document E-CONDITION; no decision', '1017994': 'exceptions document E-CONDITION; no date is made',
    '1000212': 'empty source', '1024729': 'empty source', '1296202': 'empty source',
    '1054245': 'broken dates example', '1020676': 'broken dates example', '712688': 'broken dates example', '1082664': 'broken dates example',
    '2200000105453': 'broken date example', '2200000091273': 'lead record arrival estimate', '2200000092429': 'lead record arrival estimate',
    '2200000092585': 'lead record arrival estimate', '2200000092707': 'lead record arrival estimate', '2200000097957': 'lead record arrival estimate',
    '2200000102885': 'lead record arrival estimate', '2200000102951': 'lead record arrival estimate', '2200000103269': 'lead record arrival estimate',
    '2200000104659': 'lead record arrival estimate', '2200000097429': 'lead record arrival estimate', '2200000093417': 'lead record arrival estimate',
    '2200000091671': 'lead record explicit date', '2200000093153': 'lead record unresolved', '2200000095059': 'lead record unresolved',
    '2200000097467': 'lead record unresolved', '2200000091997': 'lead record unresolved', '2200000095587': 'lead record unresolved',
}

NOT_REVISION = {  # numbers that are not revisions of the decision scope
    '509033': 'next revision reference', '780670': 'next revision reference', '795251': 'next revision reference',
    '803241': 'next revision reference', '803247': 'next revision reference', '916826': 'next revision reference',
    '1813257': 'next revision reference', '2037891': 'identity (자치법규ID), not a revision',
    '144549': 'file count', '144550': 'file count', '881616': 'file count', '881847': 'file count', '831928': 'count',
    '199919': 'broken date value', '1996071': 'broken date value', '2001027': 'broken date value', '1997115': 'number in text',
    '393374': 'number in text', '586268': 'number in text',
   
    '8455953': 'number in text',
}


def copies(cache, number):
    out = []
    if number.startswith('2200000'):
        paths = [cache / 'admrule' / f'{number}.xml']
        dataset, id_tag = 'admrules', '행정규칙ID'
    else:
        paths = [cache / 'ordinance' / f'{number}.xml', cache / 'ordinance' / 'history' / f'{number}.xml']
        dataset, id_tag = 'ordinances', '자치법규ID'
    for p in paths:
        if p.is_file():
            data = p.read_bytes()
            m = re.search(('<' + id_tag + '>(.*?)</' + id_tag + '>').encode(), data)
            out.append({'path': str(p.relative_to(cache)), 'identity': m.group(1).decode() if m else None,
                        'source_sha256': hashlib.sha256(data).hexdigest()})
    return dataset, out


def main():
    rows = json.loads(Path(sys.argv[1]).read_text(encoding='utf-8'))
    cache, out = Path(sys.argv[2]), Path(sys.argv[3])
    numbers = {r['number'] for r in rows}
    classes = {}
    for table, name in ((PROTECTED, 'protected'), (NAMED_UNRESOLVED, 'llm_target_named_unresolved'),
                        (LEAD_OR_MEASURED, 'not_user_decision'), (NOT_REVISION, 'not_revision')):
        for n in table:
            assert n not in classes, f'{n} classified twice'
            classes[n] = name
    missing = sorted(numbers - set(classes))
    extra = sorted(set(classes) - numbers)
    assert not missing, f'unclassified candidates: {missing}'
    assert not extra, f'classified numbers that are not candidates: {extra}'
    protected = []
    for n, (result, source) in PROTECTED.items():
        dataset, cps = copies(cache, n)
        assert cps, f'no source copy for {n}'
        protected.append({'dataset': dataset, 'revision': n, 'result': result, 'source': source, 'copies': cps})
    protected.sort(key=lambda r: (r['dataset'], int(r['revision'])))
    doc = {'decision': 'lead-user-decision-protection-20261116', 'recorded': '2026-11-16',
           'key': 'identity, revision (MST), source_sha256 of each copy; a new source SHA-256 is not protected automatically and goes to the user-confirmation queue',
           'revisions': protected}
    (out / 'protected-revisions.json').write_text(json.dumps(doc, ensure_ascii=False, indent=1) + '\n', encoding='utf-8')
    audit = []
    for r in rows:
        n = r['number']
        reason = (PROTECTED.get(n, (None, None))[1] or NAMED_UNRESOLVED.get(n) or LEAD_OR_MEASURED.get(n) or NOT_REVISION.get(n))
        audit.append({'number': n, 'class': classes[n], 'reason': reason, 'user_decision_context': r['user_decision_context'],
                      'locations': [l['loc'] for l in r['locations']]})
    (out / 'protection-audit.json').write_text(json.dumps(audit, ensure_ascii=False, indent=1) + '\n', encoding='utf-8')
    from collections import Counter
    print('candidates', len(rows), dict(Counter(classes[r['number']] for r in rows)))
    for p in protected:
        print(p['dataset'], p['revision'], p['result'], [(c['path'], c['identity'], c['source_sha256'][:12]) for c in p['copies']])


if __name__ == '__main__':
    main()
