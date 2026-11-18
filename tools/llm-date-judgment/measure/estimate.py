"""Estimate the bulk judgment cost from the gold runs and the full-scan text sizes.

Usage: python3 -I estimate.py GOLD_RUN_DIR UNUSABLE_JSONL MODEL [EXCLUDE_REVISIONS_CSV]

Fits input_tokens = a + b * prompt_chars by least squares over the model's gold calls. The
population prompt size is the template and metadata size plus the source fields, with the body
capped at the excerpt limit (an upper bound: a real excerpt is shorter). Output tokens, seconds
and list price per call are the gold means. Prints the totals for the population.
"""
import json
import statistics
import sys
from pathlib import Path

PARENTS = ('조내용', '조문내용', '부칙', '제개정이유', '개정문')
BODY_LIMIT = 30000


def main():
    run_dir, unusable, model = Path(sys.argv[1]), sys.argv[2], sys.argv[3]
    exclude = set(sys.argv[4].split(',')) if len(sys.argv) > 4 and sys.argv[4] else set()
    xs, ys, outs, secs, usd, ratios = [], [], [], [], [], []
    for entry_file in sorted((run_dir / model).glob('*/entry.json')):
        e = json.loads(entry_file.read_text(encoding='utf-8'))
        call = e.get('call') or {}
        u = call.get('usage') or {}
        if not u:
            continue
        prompt = (entry_file.parent / 'prompt.txt').read_text(encoding='utf-8')
        tokens = (u.get('input_tokens') or 0) + (u.get('cache_creation_input_tokens') or 0) + (u.get('cache_read_input_tokens') or 0)
        xs.append(len(prompt))
        ys.append(tokens)
        outs.append(u.get('output_tokens') or 0)
        secs.append(call.get('seconds') or 0)
        usd.append(call.get('total_cost_usd') or 0)
        ratios.append((call.get('total_cost_usd') or 0) / max(tokens, 1))
    n = len(xs)

    def fit(ys_):
        mx, my = statistics.mean(xs), statistics.mean(ys_)
        b_ = sum((x - mx) * (y - my) for x, y in zip(xs, ys_)) / max(sum((x - mx) ** 2 for x in xs), 1)
        return my - b_ * mx, b_

    a, b = fit(ys)
    ao, bo = fit(outs)
    asec, bsec = fit(secs)
    ausd, busd = fit(usd)
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
    import judge
    template = judge.current_prompt()[0]
    stub = {'dataset': 'ordinances', 'kind': '조례', 'name': 'x' * 20, 'issuer': 'x' * 8, 'promulgation_date': '2000-01-01',
            'promulgation_number': '1234', 'amendment_type': '일부개정', 'addenda': []}
    fixed = len(judge.render(template, stub, {'조문': '', '제개정이유': '', '개정문': ''}, '2001-01-01')[0])
    pop = 0
    chars = 0
    for line in open(unusable, encoding='utf-8'):
        r = json.loads(line)
        if r.get('text_blank') or r.get('serial') in exclude:
            continue
        t = r['text_chars']
        body = min(t['조내용'] + t['조문내용'], BODY_LIMIT)
        chars += body + t['부칙'] + t['제개정이유'] + t['개정문']
        pop += 1
    total_chars = pop * fixed + chars

    def total(a_, b_):
        return pop * a_ + b_ * total_chars

    print(json.dumps({
        'model': model, 'gold_calls': n, 'gold_mean_prompt_chars': round(statistics.mean(xs)),
        'population': pop, 'population_mean_prompt_chars': round(total_chars / max(pop, 1)),
        'fit_input_tokens_per_char': round(b, 3),
        'est_input_tokens_total': round(total(a, b)), 'est_output_tokens_total': round(total(ao, bo)),
        'est_hours_sequential': round(total(asec, bsec) / 3600, 1),
        'est_list_usd_total': round(total(ausd, busd)),
        'gold_mean_seconds': round(statistics.mean(secs), 1), 'gold_mean_list_usd': round(statistics.mean(usd), 4),
    }, ensure_ascii=False, indent=1))


if __name__ == '__main__':
    main()
