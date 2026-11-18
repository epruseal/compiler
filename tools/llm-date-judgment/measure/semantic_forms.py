"""Count, in the LLM target population, the revisions whose text holds the semantic forms of D4
(lead-compiler2-check-contract-20261116) and list example revisions.

Usage: python3 -I semantic_forms.py UNUSABLE_JSONL OUT_JSON
Forms (searched in compact text: white space removed, entities decoded once):
  application   a date, then 부터 or 이후, then 시행되는/시행하는 (an application clause)
  upper_law     a date, then 부터, then 시행됨에따라/시행되므로/시행됨으로 (another law's effect)
  quoted        a quoted sentence ending in 시행한다 followed by 를/을 삭제 or 로 한다 (an instruction)
"""
import json
import re
import sys

from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import source_text  # noqa: E402

DATE = r'\d{4}(?:년|[.·\-/])\d{1,2}(?:월[.·]?|[.·\-/])\d{1,2}일?\.?'
FORMS = {
    'application': re.compile(DATE + r'(?:부터|이후)시행(?:되는|하는)'),
    'upper_law': re.compile(DATE + r'부터시행(?:됨에따라|되므로|됨으로|됨에)'),
    'quoted': re.compile(r'["“‘\'][^"”’\']{0,80}시행한다\.?["”’\'](?:를|을)(?:삭제|"|“)|["“][^"”]{0,80}시행한다\.?["”]로한다'),
}


def main():
    counts = {k: 0 for k in FORMS}
    examples = {k: [] for k in FORMS}
    total = 0
    for line in open(sys.argv[1], encoding='utf-8'):
        r = json.loads(line)
        if r.get('text_blank') or r['dataset'] not in ('ordinances', 'admrules'):
            continue
        total += 1
        try:
            _, fields = source_text.read_source(r['path'], r['dataset'])
        except Exception:
            continue
        text = re.sub(r'\s+', '', '\n'.join(fields.values()))
        for k, rx in FORMS.items():
            m = rx.search(text)
            if m:
                counts[k] += 1
                if len(examples[k]) < 6:
                    examples[k].append({'dataset': r['dataset'], 'revision': r['serial'], 'match': m.group(0)[:80]})
    out = {'population': total, 'counts': counts, 'examples': examples}
    open(sys.argv[2], 'w', encoding='utf-8').write(json.dumps(out, ensure_ascii=False, indent=1))
    print(json.dumps(out, ensure_ascii=False, indent=1))


if __name__ == '__main__':
    main()
