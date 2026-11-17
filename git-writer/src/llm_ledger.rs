//! Reads the LLM effective-date judgment ledger of compiler#2 (docs/llm-date-judgment.md).
//!
//! The judge (`tools/llm-date-judgment/judge.py`) writes one canonical ledger file per dataset:
//! `<ops>/ledger/<dataset>.json`. The compiler reads only that file, never the journal. This
//! module checks the format and keeps the entries that the integration may use: judged entries
//! whose dates passed the mechanical checks, made under the policy of the ledger. Entries for
//! revisions that the parser could not date (category `judge`, phase 1) can give a date. Entries
//! of the phase 2 cross-check (categories `cross-*`) can only take a parser date away
//! (design-v3.5-crosscheck.md 2). Every other entry stays out.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;

/// The canonical ledger format the judge writes.
const SCHEMA_VERSION: u64 = 1;
/// The policy fields of the ledger head and of each judged entry.
const POLICY_KEYS: [&str; 4] = ["model", "effort", "prompt_version", "runner_version"];
/// The key fields of an entry.
const KEY_FIELDS: [&str; 4] = ["dataset", "identity", "revision", "source_sha256"];
/// The parser category of a phase 1 judgment: the parser could not date the revision.
const JUDGE_CATEGORIES: [&str; 1] = ["judge"];
/// The parser categories of a phase 2 cross-check: the parser dated the revision without a source
/// effective date.
const CROSS_CATEGORIES: [&str; 3] = ["cross-sentence", "cross-default", "cross-arrival"];

/// What the parser gave when the revision was judged (the manifest of that time).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedParser {
    /// The status of the revision in that manifest.
    pub status: String,
    /// The effective date of the revision in that manifest, if any.
    pub date: Option<String>,
    /// The rule or reason of the revision in that manifest.
    pub rule: String,
}

/// One usable judgment.
#[derive(Clone, Debug)]
pub struct LlmEntry {
    /// The ISO date the judgment gave.
    pub date: String,
    /// `explicit_date`, `promulgation_day` or `label_date`.
    pub outcome: String,
    /// The source field the quote comes from: 부칙, 조문, 제개정이유 or 개정문.
    pub field: String,
    /// The quoted sentence with white space folded to one space.
    pub quote: String,
    /// The section label the judgment named.
    pub section_label: String,
    /// The judgment found a proviso.
    pub proviso: bool,
    /// The proviso sentence the judgment quoted.
    pub proviso_quote: String,
    /// The next revision promulgation date the judge used, if any.
    pub next_promulgation: Option<String>,
    /// The parser result at judgment time (the B3 condition).
    pub parser: RecordedParser,
    /// When the judgment was made.
    pub judged_at: String,
    /// The cross-check result the judge recorded (an entry field). Only cross-check entries
    /// carry it.
    pub cross_check_at_judgment: Option<String>,
}

/// The usable judgments of one dataset and what identifies the ledger they came from.
#[derive(Clone, Debug)]
pub struct LlmLedger {
    /// The dataset of the ledger.
    pub dataset: String,
    /// The SHA-256 of the canonical ledger file.
    pub sha256: String,
    /// The policy of the ledger head.
    pub policy: BTreeMap<String, String>,
    /// The SHA-256 of the protection list the ledger was compacted with.
    pub protected_list_sha256: Option<String>,
    /// Usable phase 1 entries by (identity, revision, source SHA-256).
    entries: HashMap<(String, String, String), LlmEntry>,
    /// Usable cross-check entries by (identity, revision, source SHA-256). They are kept apart
    /// from `entries`: `llm_approval` would add `llm_not_applied` evidence to every dated
    /// cross-check revision (design-v3.5-crosscheck.md 2).
    cross: HashMap<(String, String, String), LlmEntry>,
}

impl LlmLedger {
    /// Reads `<ops>/ledger/<dataset>.json`. A format error stops the build.
    pub fn read(ops: &Path, dataset: &str) -> Result<Self> {
        let path = ops.join("ledger").join(format!("{dataset}.json"));
        let bytes =
            std::fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
        Self::parse(&bytes, dataset)
            .with_context(|| format!("invalid LLM ledger {}", path.display()))
    }

    /// Parses the bytes of a canonical ledger file.
    pub fn parse(bytes: &[u8], dataset: &str) -> Result<Self> {
        let doc: Value = serde_json::from_slice(bytes)?;
        let object = doc.as_object().context("ledger is no object")?;
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        ensure!(
            keys == [
                "dataset",
                "entries",
                "policy",
                "protected_list_sha256",
                "schema_version"
            ],
            "ledger top-level keys"
        );
        ensure!(
            doc["schema_version"].as_u64() == Some(SCHEMA_VERSION),
            "unknown ledger schema_version"
        );
        ensure!(doc["dataset"] == dataset, "ledger dataset mismatch");
        let head = doc["policy"]
            .as_object()
            .context("ledger without a policy")?;
        ensure!(
            head.len() == POLICY_KEYS.len(),
            "ledger policy must hold exactly {POLICY_KEYS:?}"
        );
        let mut policy = BTreeMap::new();
        for key in POLICY_KEYS {
            let value = head
                .get(key)
                .and_then(Value::as_str)
                .with_context(|| format!("ledger policy lacks {key}"))?;
            policy.insert(key.to_owned(), value.to_owned());
        }
        let protected_list_sha256 = match &doc["protected_list_sha256"] {
            Value::Null => None,
            Value::String(sha) => Some(sha.clone()),
            _ => bail!("protected_list_sha256 must be a string or null"),
        };
        let list = doc["entries"].as_array().context("ledger entries")?;
        let mut previous: Option<Vec<String>> = None;
        let mut entries = HashMap::new();
        let mut cross = HashMap::new();
        for entry in list {
            let key = entry["key"].as_object().context("entry key")?;
            ensure!(key.len() == KEY_FIELDS.len(), "entry key fields");
            let mut parts = Vec::with_capacity(KEY_FIELDS.len());
            for field in KEY_FIELDS {
                parts.push(
                    key.get(field)
                        .and_then(Value::as_str)
                        .with_context(|| format!("entry key lacks {field}"))?
                        .to_owned(),
                );
            }
            ensure!(parts[0] == dataset, "entry dataset mismatch");
            ensure!(
                previous.as_ref().is_none_or(|before| *before < parts),
                "ledger entries must be sorted by key without duplicates"
            );
            previous = Some(parts.clone());
            let kind = entry["kind"].as_str().context("entry kind")?;
            ensure!(
                matches!(kind, "judged" | "excluded" | "failed"),
                "entry kind must be judged, excluded or failed"
            );
            let key = (parts[1].clone(), parts[2].clone(), parts[3].clone());
            if let Some(usable) = usable(entry, kind, &policy, &JUDGE_CATEGORIES)? {
                entries.insert(key, usable);
            } else if let Some(usable) = usable(entry, kind, &policy, &CROSS_CATEGORIES)? {
                cross.insert(key, usable);
            }
        }
        Ok(Self {
            dataset: dataset.to_owned(),
            sha256: crate::date_index::sha256(bytes),
            policy,
            protected_list_sha256,
            entries,
            cross,
        })
    }

    /// The usable judgment of one exact source copy.
    pub fn find(&self, identity: &str, revision: &str, copy_id: &str) -> Option<&LlmEntry> {
        self.entries
            .get(&(identity.to_owned(), revision.to_owned(), copy_id.to_owned()))
    }

    /// The usable cross-check entry of one exact source copy.
    pub fn find_cross(&self, identity: &str, revision: &str, copy_id: &str) -> Option<&LlmEntry> {
        self.cross
            .get(&(identity.to_owned(), revision.to_owned(), copy_id.to_owned()))
    }

    /// Tells whether any usable judgment or cross-check names this revision, whatever the copy.
    pub fn names_revision(&self, revision: &str) -> bool {
        self.entries
            .keys()
            .chain(self.cross.keys())
            .any(|(_, named, _)| named == revision)
    }

    /// The number of usable judgments.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// The number of usable cross-check entries.
    pub fn cross_len(&self) -> usize {
        self.cross.len()
    }

    /// Tells whether no judgment is usable.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// The usable judgment of an entry, or nothing. An entry is usable when it is judged, its date
/// passed the mechanical checks (`adoptable`), it was made under the policy of the ledger head,
/// and its parser category is one of `categories`. A cross-check entry must carry the entry field
/// `cross_check_at_judgment`.
fn usable(
    entry: &Value,
    kind: &str,
    policy: &BTreeMap<String, String>,
    categories: &[&str],
) -> Result<Option<LlmEntry>> {
    if kind != "judged" || entry["adoptable"] != Value::Bool(true) {
        return Ok(None);
    }
    if POLICY_KEYS
        .iter()
        .any(|key| entry[*key].as_str() != policy.get(*key).map(String::as_str))
    {
        return Ok(None);
    }
    let parser = &entry["parser_at_judgment"];
    if !categories
        .iter()
        .any(|category| parser["category"] == *category)
    {
        return Ok(None);
    }
    let cross_check_at_judgment = if categories == CROSS_CATEGORIES {
        Some(
            entry["cross_check_at_judgment"]
                .as_str()
                .context("cross-check entry lacks cross_check_at_judgment")?
                .to_owned(),
        )
    } else {
        None
    };
    let judgment = &entry["judgment"];
    let text = |value: &Value, name: &str| -> Result<String> {
        value
            .as_str()
            .map(str::to_owned)
            .with_context(|| format!("judged entry lacks {name}"))
    };
    let date = text(&entry["date"], "date")?;
    ensure!(!date.is_empty(), "adoptable entry without a date");
    Ok(Some(LlmEntry {
        date,
        outcome: text(&judgment["outcome"], "outcome")?,
        field: text(&judgment["basis_field"], "basis_field")?,
        quote: text(&judgment["basis_quote"], "basis_quote")?,
        section_label: judgment["section_label"].as_str().unwrap_or("").to_owned(),
        proviso: judgment["proviso"] == Value::Bool(true),
        proviso_quote: judgment["proviso_quote"].as_str().unwrap_or("").to_owned(),
        next_promulgation: entry["inputs"]["next_promulgation"]
            .as_str()
            .map(str::to_owned),
        parser: RecordedParser {
            status: text(&parser["status"], "parser status")?,
            date: parser["date"].as_str().map(str::to_owned),
            rule: text(&parser["rule"], "parser rule")?,
        },
        judged_at: text(&entry["judged_at"], "judged_at")?,
        cross_check_at_judgment,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger(entries: &str) -> String {
        format!(
            r#"{{"schema_version": 1, "dataset": "ordinances", "policy": {{"model": "m", "effort": "default", "prompt_version": "p", "runner_version": "r2"}}, "protected_list_sha256": null, "entries": [{entries}]}}"#
        )
    }

    fn judged(revision: &str, extra: &str) -> String {
        format!(
            r#"{{"key": {{"dataset": "ordinances", "identity": "1", "revision": "{revision}", "source_sha256": "a"}}, "kind": "judged", "adoptable": true, "date": "2011-04-07", "model": "m", "effort": "default", "prompt_version": "p", "runner_version": "r2", "judged_at": "t", "inputs": {{"next_promulgation": null}}, "parser_at_judgment": {{"category": "judge", "status": "unresolved", "date": null, "rule": "x"}}, "judgment": {{"outcome": "promulgation_day", "basis_field": "부칙", "basis_quote": "이 조례는 공포일부터 시행한다.", "section_label": "", "proviso": false, "proviso_quote": ""}}{extra}}}"#
        )
    }

    fn cross(revision: &str, category: &str) -> String {
        judged(revision, r#", "cross_check_at_judgment": "disagree""#).replace(
            r#""category": "judge""#,
            &format!(r#""category": "{category}""#),
        )
    }

    #[test]
    fn keeps_only_usable_phase_one_judgments() {
        let text = ledger(
            &[
                judged("10", ""),
                judged("11", "").replace(r#""adoptable": true"#, r#""adoptable": false"#),
                cross("12", "cross-sentence"),
                judged("13", "").replace(r#""model": "m""#, r#""model": "other""#),
                judged("14", "").replace(r#""kind": "judged""#, r#""kind": "failed""#),
            ]
            .join(","),
        );
        let read = LlmLedger::parse(text.as_bytes(), "ordinances").unwrap();
        assert_eq!(read.len(), 1);
        let entry = read.find("1", "10", "a").unwrap();
        assert_eq!(entry.date, "2011-04-07");
        assert_eq!(entry.parser.status, "unresolved");
        assert_eq!(entry.parser.date, None);
        assert_eq!(entry.cross_check_at_judgment, None);
        assert!(read.find("1", "12", "a").is_none());
        assert!(read.names_revision("10"));
        assert!(!read.names_revision("11"));
    }

    #[test]
    fn keeps_usable_cross_checks_in_their_own_map() {
        // design-v3.5-crosscheck.md 2: the three cross categories go to the cross-check map,
        // the judge category to the judgment map, an entry that is not adoptable to neither.
        let text = ledger(
            &[
                judged("10", ""),
                cross("11", "cross-sentence"),
                cross("12", "cross-default"),
                cross("13", "cross-arrival"),
                cross("14", "cross-sentence")
                    .replace(r#""adoptable": true"#, r#""adoptable": false"#),
                cross("15", "other"),
            ]
            .join(","),
        );
        let read = LlmLedger::parse(text.as_bytes(), "ordinances").unwrap();
        assert_eq!((read.len(), read.cross_len()), (1, 3));
        for revision in ["11", "12", "13"] {
            let entry = read.find_cross("1", revision, "a").unwrap();
            assert_eq!(entry.cross_check_at_judgment.as_deref(), Some("disagree"));
            assert!(read.find("1", revision, "a").is_none());
            assert!(read.names_revision(revision));
        }
        assert!(read.find_cross("1", "10", "a").is_none());
        assert!(read.find_cross("1", "14", "a").is_none());
        assert!(!read.names_revision("14"));
        assert!(!read.names_revision("15"));
    }

    #[test]
    fn refuses_a_cross_check_without_its_recorded_result() {
        let text = ledger(
            &cross("11", "cross-sentence")
                .replace(r#", "cross_check_at_judgment": "disagree""#, ""),
        );
        assert!(LlmLedger::parse(text.as_bytes(), "ordinances").is_err());
    }

    #[test]
    fn refuses_format_errors() {
        for bad in [
            ledger(&judged("10", "")).replace(r#""schema_version": 1"#, r#""schema_version": 2"#),
            ledger(&judged("10", "")).replace(
                r#""dataset": "ordinances", "policy""#,
                r#""dataset": "admrules", "policy""#,
            ),
            ledger(&[judged("11", ""), judged("10", "")].join(",")),
            ledger(&[judged("10", ""), judged("10", "")].join(",")),
            ledger(&judged("10", "")).replace(r#""protected_list_sha256": null, "#, ""),
        ] {
            assert!(
                LlmLedger::parse(bad.as_bytes(), "ordinances").is_err(),
                "{bad}"
            );
        }
    }
}
