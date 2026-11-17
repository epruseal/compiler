"""LLM effective-date judgment ledger (compiler#2, docs/llm-date-judgment.md).

Layout under an operations folder:
  ledger/<dataset>.json          canonical ledger; the compiler reads it
  ledger/<dataset>.journal.jsonl one line per new judgment; compaction merges it
  ledger-history/<dataset>.jsonl replaced entries, one line each

The canonical file is {"schema_version": 1, "dataset", "policy", "protected_list_sha256",
"entries": [...]}. Entries are sorted by key and a key appears once. Serialization sorts object
keys, uses UTF-8 and ends with one newline, so equal content gives equal bytes.
"""
import hashlib
import json
import os
from pathlib import Path

SCHEMA_VERSION = 1
TOP_KEYS = {'schema_version', 'dataset', 'policy', 'protected_list_sha256', 'entries'}
POLICY_KEYS = ('model', 'effort', 'prompt_version', 'runner_version')
KEY_FIELDS = ('dataset', 'identity', 'revision', 'source_sha256')
EXCLUDED_REASON = '사용자 결정 보호'
PROTECTION_DECISION = 'lead-user-decision-protection-20261116'


class LedgerError(ValueError):
    pass


def key_of(entry):
    k = entry['key']
    return tuple(k[f] for f in KEY_FIELDS)


def dumps(obj):
    return json.dumps(obj, ensure_ascii=False, sort_keys=True, indent=1) + '\n'


def _unique_object(pairs):
    out = {}
    for k, v in pairs:
        if k in out:
            raise LedgerError(f'duplicate key {k!r}')
        out[k] = v
    return out


def atomic_write(path, text):
    """Writes text through a temporary file in the same folder: fsync, rename, fsync the folder."""
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.parent / f'.tmp-{path.name}-{os.getpid()}'
    with open(tmp, 'w', encoding='utf-8') as fh:
        fh.write(text)
        fh.flush()
        os.fsync(fh.fileno())
    os.replace(tmp, path)
    fd = os.open(path.parent, os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def append_line(path, obj):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, 'a', encoding='utf-8') as fh:
        fh.write(json.dumps(obj, ensure_ascii=False, sort_keys=True) + '\n')
        fh.flush()
        os.fsync(fh.fileno())


def read_lines(path):
    """Complete JSON lines of an append-only file. A cut last line is dropped and reported.

    The file is split into byte lines before decoding, so a line cut inside a multi-byte UTF-8
    character spoils only that line."""
    path = Path(path)
    if not path.exists():
        return [], False
    lines = path.read_bytes().split(b'\n')
    cut = False
    if lines and lines[-1] != b'':
        cut = True
        lines = lines[:-1]
    out = []
    for line in lines:
        if not line:
            continue
        try:
            out.append(json.loads(line.decode('utf-8'), object_pairs_hook=_unique_object))
        except ValueError:  # UnicodeDecodeError is a ValueError
            cut = True
    return out, cut


def repair(path):
    """Rewrites an append-only file without its unreadable lines, so that a later append does not
    join a cut line. Returns the complete lines."""
    lines, cut = read_lines(path)
    if cut:
        atomic_write(path, ''.join(json.dumps(x, ensure_ascii=False, sort_keys=True) + '\n' for x in lines))
    return lines


def remove_temporary(*folders):
    """Removes the .tmp-* files that a stopped atomic_write left, in the folders and below them."""
    for folder in folders:
        folder = Path(folder)
        if folder.exists():
            for tmp in folder.rglob('.tmp-*'):
                tmp.unlink()


def file_sha256(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


class Ledger:
    def __init__(self, ops, dataset):
        self.ops = Path(ops)
        self.dataset = dataset
        self.path = self.ops / 'ledger' / f'{dataset}.json'
        self.journal = self.ops / 'ledger' / f'{dataset}.journal.jsonl'
        self.history = self.ops / 'ledger-history' / f'{dataset}.jsonl'

    def load(self):
        """The canonical ledger or an empty one. Format errors raise LedgerError."""
        if not self.path.exists():
            return {'schema_version': SCHEMA_VERSION, 'dataset': self.dataset, 'policy': None,
                    'protected_list_sha256': None, 'entries': []}
        doc = json.loads(self.path.read_text(encoding='utf-8'), object_pairs_hook=_unique_object)
        validate(doc, self.dataset)
        return doc

    def add(self, entry):
        append_line(self.journal, entry)

    def compact(self, policy, protected, protected_sha256, recheck=None):
        """Merges the journal into the canonical ledger.

        policy     the accepted (model, effort, prompt_version, runner_version)
        protected  {(dataset, revision): set(source_sha256)} from the protection list
        recheck    optional function(entry) -> entry that re-runs the mechanical checks on a stored
                   judgment; used for judged entries whose runner_version differs from the policy
        Returns a report dict. Replaced entries go to history before the rename.
        """
        doc = self.load()
        # A policy change keeps old entries: the compiler does not adopt them and the runner judges
        # their keys again.
        current = {key_of(e): e for e in doc['entries']}
        journal, cut = read_lines(self.journal)
        replaced = []
        for e in journal:
            if e.get('key', {}).get('dataset') != self.dataset:
                continue
            k = key_of(e)
            old = current.get(k)
            if old == e:
                # The same line merged again after a stop between the canonical rename and the
                # journal truncation.
                continue
            if old is not None:
                replaced.append(old)
            current[k] = e
        for k, e in list(current.items()):
            ds, _identity, revision, sha = k
            if (ds, revision) in protected:
                if e.get('kind') != 'excluded' and sha in protected[(ds, revision)]:
                    replaced.append(e)
                    current[k] = excluded_entry(e['key'])
            elif e.get('kind') == 'judged' and recheck is not None and e.get('runner_version') != policy['runner_version']:
                fixed = recheck(e)
                if fixed is not None and fixed != e:
                    replaced.append(e)
                    current[k] = fixed
        seen_history = {mark_of(h) for h in repair(self.history)}
        for old in replaced:
            mark = mark_of(old)
            if mark not in seen_history:
                append_line(self.history, old)
                seen_history.add(mark)
        new = {'schema_version': SCHEMA_VERSION, 'dataset': self.dataset, 'policy': dict(policy),
               'protected_list_sha256': protected_sha256,
               'entries': [current[k] for k in sorted(current)]}
        validate(new, self.dataset)
        atomic_write(self.path, dumps(new))
        atomic_write(self.journal, '')
        return {'merged': len(journal), 'journal_cut_line': cut, 'replaced': len(replaced), 'entries': len(new['entries'])}

    def protect(self, protected, revisions):
        """Journals an excluded entry for each listed copy that the ledger does not hold yet."""
        doc = self.load()
        have = {key_of(e) for e in doc['entries'] if e.get('kind') == 'excluded'}
        added = 0
        for r in revisions:
            if r['dataset'] != self.dataset:
                continue
            for c in r['copies']:
                key = {'dataset': r['dataset'], 'identity': r['identity'], 'revision': r['revision'], 'source_sha256': c['source_sha256']}
                if tuple(key[f] for f in KEY_FIELDS) not in have:
                    self.add(excluded_entry(key, r.get('source')))
                    added += 1
        return added


def mark_of(entry):
    """Identity of a history line: the whole entry, so two entries of one key never collide."""
    return json.dumps(entry, ensure_ascii=False, sort_keys=True)


def excluded_entry(key, source=None):
    return {'key': dict(key), 'kind': 'excluded', 'reason': EXCLUDED_REASON, 'decision': PROTECTION_DECISION,
            'protection_source': source, 'judged_at': None}


def validate(doc, dataset):
    if not isinstance(doc, dict) or set(doc) != TOP_KEYS:
        raise LedgerError(f'top-level keys must be {sorted(TOP_KEYS)}')
    if doc['schema_version'] != SCHEMA_VERSION:
        raise LedgerError('unknown schema_version')
    if doc['dataset'] != dataset:
        raise LedgerError('dataset mismatch')
    if doc['policy'] is not None and (not isinstance(doc['policy'], dict) or set(doc['policy']) != set(POLICY_KEYS)):
        raise LedgerError('policy must hold exactly ' + ', '.join(POLICY_KEYS))
    keys = []
    for e in doc['entries']:
        if e.get('kind') not in ('judged', 'excluded'):
            raise LedgerError('entry kind must be judged or excluded')
        if set(e['key']) != set(KEY_FIELDS) or e['key']['dataset'] != dataset:
            raise LedgerError('entry key fields or dataset mismatch')
        keys.append(key_of(e))
    if keys != sorted(keys) or len(set(keys)) != len(keys):
        raise LedgerError('entries must be sorted by key without duplicates')


def policy_matches(entry, policy):
    return entry.get('kind') == 'judged' and all(entry.get(k) == policy[k] for k in POLICY_KEYS)


def load_protected(path):
    """{(dataset, revision): set of listed source_sha256} and the list file SHA-256."""
    doc = json.loads(Path(path).read_text(encoding='utf-8'), object_pairs_hook=_unique_object)
    if doc.get('schema_version') != 1 or not isinstance(doc.get('revisions'), list):
        raise LedgerError('protection list format')
    out = {}
    for r in doc['revisions']:
        if (r.get('dataset'), r.get('revision')) in out:
            raise LedgerError(f'protection list holds {r.get("dataset")} {r.get("revision")} twice')
        for f in ('dataset', 'revision', 'identity', 'copies', 'decision_id', 'source', 'expected'):
            if f not in r:
                raise LedgerError(f'protection list entry lacks {f}')
        shas = set()
        for c in r['copies']:
            sha = c['source_sha256']
            if not (isinstance(sha, str) and len(sha) == 64 and all(ch in '0123456789abcdef' for ch in sha)):
                raise LedgerError('protection list SHA-256 format')
            shas.add(sha)
        out[(r['dataset'], r['revision'])] = shas
    return out, doc['revisions'], file_sha256(path)
