//! Preserves revision sources and replays effective-date events.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;

use anyhow::{Context, Result, bail, ensure};
use quick_xml::Reader;
use quick_xml::events::Event;
use regex::Regex;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use time::{Date, Duration, Month, PrimitiveDateTime, Time, UtcOffset};

use crate::{
    BareRepoWriter, GitTimestampKst, PreparedBlob, RepoPathBuf, blob_id, hex, parallel_map,
    precompute_blob,
};

/// One source occurrence before date planning.
#[derive(Clone)]
pub struct SourceRevision {
    /// Legal identity from the source.
    pub identity: String,
    /// Source revision identifier.
    pub revision: String,
    /// Source title.
    pub name: String,
    /// Source document category.
    pub category: String,
    /// Path assigned by the original publication planner.
    pub canonical_path: String,
    /// Path relative to the input directory.
    pub source_path: String,
    /// Exact source XML bytes.
    pub xml: Vec<u8>,
    /// Original publication or issue date.
    pub promulgation: String,
    /// Original effective date.
    pub enforcement: String,
    /// Source amendment label.
    pub amendment: String,
    /// Explicit source repeal flag.
    pub repeal: bool,
    /// Original rendered Markdown bytes.
    pub markdown: Vec<u8>,
    /// Existing secondary title key for laws.
    pub secondary_name: String,
    /// Existing numeric publication key for laws.
    pub secondary_number: u64,
    /// Existing numeric serial key.
    pub serial: u64,
    /// Copy preference when one revision has several source copies. The higher value wins.
    pub copy_priority: u8,
    /// Set when copies of this revision differ in content or dates. The revision stays unresolved.
    pub copy_conflict: Option<String>,
}

/// A dated occurrence with independent source evidence.
#[derive(Clone, Serialize)]
pub struct Occurrence {
    source_path: String,
    /// Exact publication date text from the source.
    pub raw_promulgation_date: String,
    /// Exact enforcement date text from the source.
    pub raw_enforcement_date: String,
    /// Final publication date from the approved resolver.
    pub promulgation_date: Option<String>,
    /// Final enforcement date from the approved resolver.
    pub enforcement_date: Option<String>,
    /// Final date status from the approved resolver.
    pub status: String,
    exposure_date: Option<String>,
    /// Source evidence from the approved resolver.
    pub date_evidence: Value,
    observed_metadata: Value,
    /// Final reason from the approved resolver.
    pub reason: String,
    promulgation_rank: Option<u64>,
    enforcement_rank: Option<u64>,
    /// Date semantics for which this occurrence has no usable date. Absent means none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub undated_semantics: Vec<String>,
    /// What the gate needs; it is no part of the index.
    #[serde(skip)]
    gate: GateMark,
}

/// A distinct XML copy with all its occurrences.
#[derive(Clone, Serialize)]
struct Revision {
    identity: String,
    revision: String,
    copy_id: String,
    name: String,
    category: String,
    canonical_path: String,
    promulgation_date: Option<String>,
    enforcement_date: Option<String>,
    status: Option<String>,
    repeal: bool,
    reason: String,
    date_evidence: Value,
    occurrences: Vec<Occurrence>,
    body: Value,
}

/// Values that stay fixed while snapshots are built one date after another.
struct SnapshotCache {
    first_source: Option<Vec<usize>>,
    body_hash: Vec<Option<String>>,
    fragments: Vec<Option<(usize, Vec<u8>)>>,
}

impl SnapshotCache {
    fn new(revisions: usize) -> Self {
        Self {
            first_source: None,
            body_hash: vec![None; revisions],
            fragments: vec![None; revisions],
        }
    }

    /// Maps each revision to its first source once, instead of scanning per snapshot.
    fn first_source(&mut self, source_revision: &[usize]) -> &[usize] {
        self.first_source.get_or_insert_with(|| {
            let mut first = vec![usize::MAX; self.body_hash.len()];
            for (source, revision) in source_revision.iter().enumerate().rev() {
                first[*revision] = source;
            }
            first
        })
    }
}

/// Full source index with date-specific ordering.
pub struct DatePlan {
    dataset: String,
    basis: String,
    revisions: Vec<Revision>,
    sources: Vec<SourceRevision>,
    source_revision: Vec<usize>,
    source_occurrence: Vec<usize>,
    snapshots: Vec<Value>,
    bodies: BTreeMap<(String, String), Value>,
    /// SHA-256 of each source Markdown when a renderer already computed it; empty otherwise.
    markdown_hashes: Vec<String>,
    /// Source copies left out of the index because they could not be parsed (`{file, error}`).
    unparsable: Vec<Value>,
    /// The LLM ledger the plan was built with (compiler#2): its identity for the index. When set,
    /// the occurrence status `llm` is valid and the date index has schema version 2.
    llm: Option<Value>,
}

/// Tracks a repository staging directory owned by an enforcement run.
pub struct StagedRepository {
    owned: PathBuf,
    final_path: PathBuf,
    armed: bool,
}

impl Drop for StagedRepository {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_dir_all(&self.owned);
        }
    }
}

/// Date of the first commit in every build: 2026-03-30 12:00 KST, as in the default build.
pub const INITIAL_COMMIT_EPOCH: i64 = 1_774_839_600;

/// Returns the exact SHA-256 of bytes.
pub fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Builds a date. The one place that fixes the valid range: years 0001 to 9999 and a real
/// calendar day, the range of the consumer's `date.fromisoformat`. Every code path that
/// creates a date from text uses it, so a year such as 0000 never reaches an index.
fn make_date(year: i32, month: u8, day: u8) -> Option<Date> {
    if !(1..=9999).contains(&year) {
        return None;
    }
    Date::from_calendar_date(year, Month::try_from(month).ok()?, day).ok()
}

/// Reads `YYYYMMDD` or `YYYY-MM-DD` as the eight-digit form. The source text is kept as it is
/// in the index; only comparisons use this form. Anything else, including a text that holds
/// several dates, is not a date.
fn normalize_compact(raw: &str) -> Option<String> {
    let digits = match raw.len() {
        8 => raw.to_owned(),
        10 if raw.as_bytes()[4] == b'-' && raw.as_bytes()[7] == b'-' => raw.replace('-', ""),
        _ => return None,
    };
    (digits.len() == 8 && digits.bytes().all(|byte| byte.is_ascii_digit())).then_some(digits)
}

/// Tells whether a source date is the "no date yet" sentinel in either written form.
fn is_sentinel(raw: &str) -> bool {
    normalize_compact(raw).as_deref() == Some("99991231")
}

/// Accepts an exact Gregorian date written as `YYYYMMDD` or `YYYY-MM-DD`.
fn compact_date(raw: &str) -> Option<Date> {
    let digits = normalize_compact(raw)?;
    if digits == "99991231" {
        return None;
    }
    make_date(
        digits[..4].parse().ok()?,
        digits[4..6].parse().ok()?,
        digits[6..].parse().ok()?,
    )
}

/// Formats a calendar date independently of Git timestamp limits.
fn iso(date: Date) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
}

/// Converts an ISO event date to the existing noon-KST timestamp representation.
fn epoch(date: &str) -> Result<i64> {
    let compact = date.replace('-', "");
    let date = compact_date(&compact).context("invalid replay date")?;
    let date = date.max(Date::from_calendar_date(1970, Month::January, 1)?);
    Ok(PrimitiveDateTime::new(date, Time::from_hms(12, 0, 0)?)
        .assume_offset(UtcOffset::from_hms(9, 0, 0)?)
        .unix_timestamp())
}

/// One addendum with its source publication label.
#[derive(Default)]
struct Addendum {
    label: String,
    text: String,
    /// The text as the source wrote it (entity markup not decoded). Only the gate (`added_any`, `full_any`) reads it.
    raw: String,
}

/// Extracts addenda and body text without metadata date substitution.
fn date_text(xml: &[u8]) -> Result<DateText> {
    date_text_with(xml, true)
}

/// `date_text` that collects the SourceView only when `collect` is set or the diagnostic trace is
/// on. Without it the view stays empty; only the gate and the approvals with a gate policy read it.
fn date_text_with(xml: &[u8], collect: bool) -> Result<DateText> {
    let mut reader = Reader::from_reader(xml);
    let mut stack: Vec<String> = Vec::new();
    let mut addenda = Vec::new();
    let mut current: Option<Addendum> = None;
    let mut found = DateText::default();
    // The gate reads the SourceView, so the product build collects it whenever `collect` is set.
    // The test build can also turn on the diagnostic output and the B trace
    // (`SOURCEVIEW_RECONCILE`); the product build reads no such variable.
    let mut source_view = SourceView {
        collect,
        ..SourceView::default()
    };
    #[cfg(test)]
    {
        source_view.enabled = std::env::var_os("SOURCEVIEW_RECONCILE").is_some();
        source_view.collect = collect || source_view.enabled;
    }
    let mut element_sequence = 0_u64;
    let mut event_sequence = 0_u64;
    let mut run_sequence = 0_u64;
    let mut element_ids = Vec::new();
    // The text events of one element, as r24 joins them (a line feed after each event) and as the
    // source wrote them (no line feed added). Only the gate reads the second form.
    let mut run = SplitRun::default();
    #[cfg(test)]
    let mut source_run = SourceSegments::default();
    loop {
        let event = reader.read_event()?;
        // A text event adds its text to the field of its element; a start or an end of an element
        // closes the run of text events.
        let piece: Option<String> = match &event {
            Event::Text(text) => {
                let decoded = text.xml_content()?;
                Some(quick_xml::escape::unescape(&decoded)?.into_owned())
            }
            Event::CData(text) => Some(text.decode()?.into_owned()),
            Event::GeneralRef(reference) => Some(if let Some(ch) = reference.resolve_char_ref()? {
                ch.to_string()
            } else {
                quick_xml::escape::unescape(&format!("&{};", reference.decode()?))?.into_owned()
            }),
            _ => None,
        };
        if let Some(piece) = piece {
            source_view.observe_piece(
                &stack,
                element_ids.last().copied(),
                event_sequence,
                run_sequence,
                &event,
                current.is_some(),
                &piece,
            );
            event_sequence += 1;
            if collect_date_text(&stack, &piece, &mut current, &mut found) {
                // Only the trace of the legacy event-run comparison reads the run views.
                #[cfg(test)]
                if source_view.enabled {
                    let kind = match &event {
                        Event::Text(_) => "Text",
                        Event::CData(_) => "CData",
                        Event::GeneralRef(_) => "GeneralRef",
                        _ => unreachable!(),
                    };
                    source_run.append(
                        &piece,
                        element_ids.last().copied(),
                        event_sequence - 1,
                        run_sequence,
                        kind,
                        true,
                    );
                }
                run.events += 1;
                run.with_line_feeds.push_str(&piece);
                run.with_line_feeds.push('\n');
                run.as_written.push_str(&piece);
            }
            continue;
        }
        if matches!(event, Event::Start(_) | Event::End(_) | Event::Eof) && run.events > 1 {
            #[cfg(test)]
            if source_view.enabled {
                source_view.split.push(std::mem::take(&mut source_run));
            }
            found.split.push((
                std::mem::take(&mut run.with_line_feeds),
                std::mem::take(&mut run.as_written),
            ));
        }
        run = SplitRun::default();
        #[cfg(test)]
        {
            source_run = SourceSegments::default();
        }
        run_sequence += 1;
        match event {
            Event::Start(event) => {
                element_sequence += 1;
                element_ids.push(element_sequence);
                let name = String::from_utf8(event.name().as_ref().to_vec())?;
                if name == "부칙단위" {
                    current = Some(Addendum::default());
                    source_view.begin_addendum(element_sequence);
                }
                if name == "부칙내용" && current.is_none() {
                    current = Some(Addendum::default());
                    source_view.begin_addendum(element_sequence);
                }
                stack.push(name);
            }
            Event::End(event) => {
                let name = String::from_utf8(event.name().as_ref().to_vec())?;
                if (name == "부칙단위"
                    || (name == "부칙내용" && !stack.iter().any(|part| part == "부칙단위")))
                    && let Some(block) = current.take()
                {
                    addenda.push(block);
                    source_view.end_addendum();
                }
                stack.pop();
                element_ids.pop();
            }
            Event::Eof => break,
            _ => {}
        }
    }
    found.addenda = addenda;
    #[cfg(test)]
    source_view.reconcile(&found)?;
    found.source_view = Some(source_view);
    Ok(found)
}

/// One source-backed or synthetic segment of a baseline text.
#[derive(Default, Clone)]
struct SourceSegments {
    text: String,
    segments: Vec<Value>,
}

impl SourceSegments {
    /// Takes a UTF-8 byte slice and clips each source-backed segment to the same range.
    fn slice(&self, start: usize, end: usize) -> Result<Self> {
        ensure!(
            start <= end
                && self.text.is_char_boundary(start)
                && self.text.is_char_boundary(end)
                && end <= self.text.len(),
            "invalid SourceView slice"
        );
        let mut result = Self {
            text: self.text[start..end].to_owned(),
            segments: Vec::new(),
        };
        let segment_end = |segment: &Value| {
            segment["output_byte_range"][1]
                .as_u64()
                .map_or(usize::MAX, |end| end as usize)
        };
        let first = self
            .segments
            .partition_point(|segment| segment_end(segment) <= start);
        for segment in &self.segments[first..] {
            let range = segment["output_byte_range"]
                .as_array()
                .context("segment range")?;
            let a = range[0].as_u64().context("segment start")? as usize;
            let b = range[1].as_u64().context("segment end")? as usize;
            if a >= end {
                break;
            }
            let left = a.max(start);
            let right = b.min(end);
            if left >= right {
                continue;
            }
            let mut clipped = segment.clone();
            if clipped.get("inherited_origins").is_some() {
                ensure!(left == a && right == b, "partial transformed segment slice");
            }
            clipped["output_byte_range"] = json!([left - start, right - start]);
            if let Some(source) = clipped.get_mut("source").and_then(Value::as_object_mut) {
                let old = source["piece_byte_range"]
                    .as_array()
                    .context("piece range")?;
                let base = old[0].as_u64().context("piece start")? as usize;
                source.insert(
                    "piece_byte_range".to_owned(),
                    json!([base + left - a, base + right - a]),
                );
            }
            result.segments.push(clipped);
        }
        Ok(result)
    }

    /// Appends a B join separator with the source separator that it replaces.
    fn append_join_separator(&self, result: &mut Self, boundary: usize) -> Result<()> {
        ensure!(
            boundary > 0 && self.text.as_bytes()[boundary - 1] == b'\n',
            "missing separator origin"
        );
        let start = if boundary >= 2 && self.text.as_bytes()[boundary - 2] == b'\r' {
            boundary - 2
        } else {
            boundary - 1
        };
        let origin = self.slice(start, boundary)?;
        ensure!(
            !origin.segments.is_empty(),
            "missing separator origin segments"
        );
        let output = result.text.len();
        result.text.push('\n');
        result.segments.push(json!({"output_byte_range":[output,output+1],"source":null,"separator":"join-lf","inherited_origins":origin.segments}));
        Ok(())
    }

    /// Reconstructs B's `lines().join("\n")` and carries the original separator metadata.
    fn lines_join(&self) -> Result<Self> {
        let mut result = Self::default();
        let mut offset = 0;
        for (index, line) in self.text.lines().enumerate() {
            if index > 0 {
                self.append_join_separator(&mut result, offset)?;
            }
            result.append_view(&self.slice(offset, offset + line.len())?);
            offset += line.len();
            if self.text[offset..].starts_with("\r\n") {
                offset += 2;
            } else if self.text[offset..].starts_with('\n') {
                offset += 1;
            }
        }
        Ok(result)
    }

    /// Replays the existing three sequential entity replacements on source-backed segments.
    fn decoded(&self) -> Result<Self> {
        let mut current = self.clone();
        for (needle, replacement) in [("&lt;", "<"), ("&gt;", ">"), ("&amp;", "&")] {
            let mut next = Self::default();
            let mut offset = 0;
            while let Some(relative) = current.text[offset..].find(needle) {
                let start = offset + relative;
                next.append_view(&current.slice(offset, start)?);
                let origins = current.slice(start, start + needle.len())?.segments;
                let out_start = next.text.len();
                next.text.push_str(replacement);
                next.segments.push(json!({"output_byte_range":[out_start,next.text.len()], "source":null, "transform":"decode-markup", "inherited_origins":origins}));
                offset = start + needle.len();
            }
            next.append_view(&current.slice(offset, current.text.len())?);
            current = next;
        }
        Ok(current)
    }

    /// Reconstructs the body of each section from source-backed line slices.
    #[cfg(test)]
    fn section_bodies(&self, publication: Option<Date>) -> Result<Vec<Self>> {
        let mut sections: Vec<Self> = Vec::new();
        let mut offset = 0;
        for line in self.text.lines() {
            let line_start = offset;
            offset += line.len();
            if self.text[offset..].starts_with("\r\n") {
                offset += 2;
            } else if self.text[offset..].starts_with('\n') {
                offset += 1;
            }
            if let Some(rest) = heading_rest(line) {
                let (_, _, rest) = heading_label(rest, publication);
                let start = line_start + line.len() - rest.len();
                sections.push(self.slice(start, line_start + line.len())?);
            } else if let Some(section) = sections.last_mut() {
                self.append_join_separator(section, line_start)?;
                section.append_view(&self.slice(line_start, line_start + line.len())?);
            }
        }
        Ok(sections)
    }

    /// Reconstructs an amendment part after removing a first-line mark.
    fn amendment_part(&self, from: usize, to: usize, mark: usize) -> Result<Self> {
        let selected = self.slice(from, to)?;
        let mut joined = selected.lines_join()?;
        ensure!(
            mark <= joined.text.len() && joined.text.is_char_boundary(mark),
            "invalid amendment mark"
        );
        joined = joined.slice(mark, joined.text.len())?;
        Ok(joined)
    }

    fn append_view(&mut self, other: &Self) {
        let base = self.text.len();
        self.text.push_str(&other.text);
        for segment in &other.segments {
            let mut shifted = segment.clone();
            let range = segment["output_byte_range"]
                .as_array()
                .expect("segment range");
            shifted["output_byte_range"] = json!([
                base + range[0].as_u64().unwrap() as usize,
                base + range[1].as_u64().unwrap() as usize
            ]);
            self.segments.push(shifted);
        }
    }

    fn append(
        &mut self,
        piece: &str,
        element: Option<u64>,
        event: u64,
        run: u64,
        kind: &str,
        line_feed: bool,
    ) {
        let start = self.text.len();
        self.text.push_str(piece);
        self.segments.push(json!({
            "output_byte_range": [start, self.text.len()],
            "source": {"element_id":element, "event_id":event, "run_id":run, "piece_byte_range":[0,piece.len()]},
            "event_kind":kind,
        }));
        if line_feed {
            let start = self.text.len();
            self.text.push('\n');
            self.segments.push(json!({
                "output_byte_range": [start, self.text.len()], "source":null,
                "separator":"event-inserted-lf", "run_id":run,
            }));
        }
    }
}

/// A separate reconstruction from the parser pieces; rules do not consume it. The gate reads it.
#[derive(Default)]
struct SourceView {
    /// The diagnostic output and the B trace are on (`SOURCEVIEW_RECONCILE`). Test builds only.
    #[cfg(test)]
    enabled: bool,
    /// The view is collected: the gate or an approval with a gate policy can read it (in a test
    /// build also when the diagnostic trace is on).
    collect: bool,
    body: SourceSegments,
    amendment: SourceSegments,
    outside: SourceSegments,
    declaration: SourceSegments,
    #[cfg(test)]
    split: Vec<SourceSegments>,
    addenda: Vec<SourceSegments>,
    /// The element ID of the `부칙단위` or of the independent `부칙내용` that opened each addendum:
    /// the stable container ID of the addendum (r25c-normalization-plan.v3.md 2).
    addendum_elements: Vec<u64>,
    active: Option<usize>,
}

impl SourceView {
    fn begin_addendum(&mut self, element: u64) {
        if !self.collect {
            return;
        }
        self.addenda.push(SourceSegments::default());
        self.addendum_elements.push(element);
        self.active = Some(self.addenda.len() - 1);
    }

    fn end_addendum(&mut self) {
        self.active = None;
    }

    #[allow(clippy::too_many_arguments)]
    fn observe_piece(
        &mut self,
        stack: &[String],
        element: Option<u64>,
        event_id: u64,
        run_id: u64,
        event: &Event<'_>,
        active: bool,
        piece: &str,
    ) {
        if !self.collect {
            return;
        }
        let kind = match event {
            Event::Text(_) => "Text",
            Event::CData(_) => "CData",
            Event::GeneralRef(_) => "GeneralRef",
            _ => "other",
        };
        #[cfg(test)]
        if self.enabled {
            BODY_TRACE.with(|slot| {
                if let Some(trace) = slot.borrow_mut().as_mut() {
                    trace.pieces.push(json!({"element_id":element, "event_id":event_id, "run_id":run_id, "kind":kind, "bytes_hex":hex_bytes(piece)}));
                }
            });
            eprintln!(
                "{}",
                json!({"event":"parser-piece", "element_id":element, "event_id":event_id, "run_id":run_id, "kind":kind, "piece_utf8":piece, "piece_bytes_hex":piece.as_bytes().iter().map(|b| format!("{b:02x}")).collect::<String>()})
            );
        }
        let last = stack.last().map(String::as_str);
        let (target, lf) = if last == Some("제개정이유내용") {
            (Some(&mut self.amendment), true)
        } else if last == Some("부칙공포일자") {
            (None, false)
        } else if stack.iter().any(|tag| tag == "부칙내용") {
            (
                if active {
                    self.active.and_then(|i| self.addenda.get_mut(i))
                } else {
                    None
                },
                false,
            )
        } else if matches!(last, Some("조문내용" | "조내용" | "본문" | "내용")) {
            (Some(&mut self.body), true)
        } else if matches!(last, Some("항내용" | "호내용" | "목내용")) {
            (Some(&mut self.outside), true)
        } else if last == Some("개정문내용") {
            (Some(&mut self.declaration), true)
        } else {
            (None, false)
        };
        if let Some(target) = target {
            target.append(piece, element, event_id, run_id, kind, lf);
        }
    }

    #[cfg(test)]
    fn reconcile(&self, found: &DateText) -> Result<()> {
        if std::env::var_os("SOURCEVIEW_RECONCILE").is_none() {
            return Ok(());
        }
        for (name, view, actual) in [
            ("body", &self.body, &found.body),
            ("amendment", &self.amendment, &found.amendment),
            ("outside", &self.outside, &found.outside),
            ("declaration", &self.declaration, &found.declaration),
        ] {
            ensure!(view.text == *actual, "SourceView B mismatch: {name}");
            eprintln!(
                "{}",
                json!({"phase":"sourceview", "representation":"B", "field":name,
                "utf8_byte_length":view.text.len(), "bytes_hex":view.text.as_bytes().iter().map(|b| format!("{b:02x}")).collect::<String>(), "segments":view.segments})
            );
        }
        ensure!(
            self.addenda.len() == found.addenda.len(),
            "SourceView addendum count mismatch"
        );
        for (index, (view, block)) in self.addenda.iter().zip(&found.addenda).enumerate() {
            ensure!(
                view.text == block.text,
                "SourceView B addendum mismatch: {index}"
            );
            eprintln!(
                "{}",
                json!({"phase":"sourceview", "representation":"B", "field":"addendum",
                "index":index, "utf8_byte_length":view.text.len(), "bytes_hex":view.text.as_bytes().iter().map(|b| format!("{b:02x}")).collect::<String>(), "segments":view.segments})
            );
        }
        Ok(())
    }
}

/// What `date_text` takes from a source: the addenda, the body and the amendment text that the
/// rules read, and the text that only the gate reads.
#[derive(Default)]
struct DateText {
    source_view: Option<SourceView>,
    addenda: Vec<Addendum>,
    body: String,
    amendment: String,
    /// The text of paragraphs, items and subitems.
    outside: String,
    /// The text of the amendment statement (`개정문내용`).
    declaration: String,
    /// The text of every element that arrived in several events, as r24 joins the events (a line
    /// feed after each) and as the source wrote it.
    split: Vec<(String, String)>,
}

/// The text events of one element.
#[derive(Default)]
struct SplitRun {
    events: usize,
    with_line_feeds: String,
    as_written: String,
}

/// Collects only source fields that can contain effective-date language. Returns whether the
/// text went into a field that r24 joins with a line feed after each event.
fn collect_date_text(
    stack: &[String],
    text: &str,
    current: &mut Option<Addendum>,
    found: &mut DateText,
) -> bool {
    let last = stack.last().map(String::as_str);
    let line_fed = |field: &mut String| {
        field.push_str(text);
        field.push('\n');
        true
    };
    if last == Some("제개정이유내용") {
        line_fed(&mut found.amendment)
    } else if last == Some("부칙공포일자") {
        if let Some(block) = current {
            block.label.push_str(text);
        }
        false
    } else if stack.iter().any(|tag| tag == "부칙내용") {
        if let Some(block) = current {
            block.text.push_str(text);
        }
        false
    } else if matches!(last, Some("조문내용" | "조내용" | "본문" | "내용")) {
        line_fed(&mut found.body)
    } else if matches!(last, Some("항내용" | "호내용" | "목내용")) {
        line_fed(&mut found.outside)
    } else if last == Some("개정문내용") {
        line_fed(&mut found.declaration)
    } else {
        false
    }
}

/// Tells whether a text starts with a proviso word: "다만", "단," or "단" and a space.
///
/// Spaces, full stops and paragraph marks before the word do not count. A word such as "단체"
/// is no proviso.
fn starts_proviso(text: &str) -> bool {
    starts_proviso_word(skip_omitted_mark(text).0)
}

/// Tells whether a proviso-omitted mark follows an effective sentence.
fn starts_with_omitted_mark(text: &str) -> bool {
    skip_omitted_mark(text).1
}

/// Skips the spaces before a sentence tail and a proviso-omitted mark in it.
///
/// A proviso-omitted mark is not itself a proviso (user decision). The words after the mark
/// still count, so a later "다만" is not lost. The flag tells whether a mark was skipped.
fn skip_omitted_mark(text: &str) -> (&str, bool) {
    let mut text = text.trim_start_matches(|ch: char| {
        ch.is_whitespace()
            || ch == '.'
            || matches!(ch, '\u{2460}'..='\u{2473}' | '\u{2474}'..='\u{2487}')
    });
    for (open, close) in [('<', '>'), ('(', ')')] {
        if let Some(rest) = text.strip_prefix(open)
            && let Some(end) = rest.find(close)
            && rest[..end]
                .chars()
                .filter(|ch| !ch.is_whitespace())
                .collect::<String>()
                == "단서생략"
        {
            text = rest[end + close.len_utf8()..].trim_start();
            return (text, true);
        }
    }
    (text, false)
}

/// Tells whether a text starts with a proviso word, with nothing before it.
///
/// An opening bracket of `PROVISO_OPEN` may stand before the word, with white space of every
/// kind (a line feed too) after it ("(다만", "（다만", "[다만", "( 다만", "(\n다만", "(단 ").
fn starts_proviso_word(text: &str) -> bool {
    match text.chars().next() {
        Some(first) if PROVISO_OPEN.contains(&first) => {
            proviso_word(text[first.len_utf8()..].trim_start())
        }
        _ => proviso_word(text),
    }
}

/// Tells whether a text starts with "다만", "단," or "단" and white space.
fn proviso_word(text: &str) -> bool {
    text.starts_with("다만")
        || text.starts_with("단,")
        || text.starts_with("단，")
        || text
            .strip_prefix('단')
            .and_then(|rest| rest.chars().next())
            .is_some_and(char::is_whitespace)
}

/// Tells whether a proviso follows an effective sentence only across a paragraph mark such as
/// "②" (lead ruling). Without the mark the words are not adjacent.
fn proviso_across_paragraph_mark(text: &str) -> bool {
    static SENTENCE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"시행\s*한다|효력을\s*발생한다").expect("valid effective sentence pattern")
    });
    SENTENCE.find_iter(text).any(|found| {
        let rest = &text[found.end()..];
        starts_proviso(rest)
            && !starts_proviso_word(
                rest.trim_start_matches(|ch: char| ch.is_whitespace() || ch == '.'),
            )
    })
}

/// Tells whether a proviso follows a sentence that states an effective date.
///
/// Only the words right after a sentence with "시행한다" or "효력을 발생한다" count. A proviso
/// inside a transitional article or inside a quoted amendment text is not attached to the
/// effective-date sentence and is ignored (user decision, 2026-11-03).
fn proviso_after_effective_sentence(text: &str) -> bool {
    static SENTENCE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"시행\s*한다|효력을\s*발생한다").expect("valid effective sentence pattern")
    });
    SENTENCE
        .find_iter(text)
        .any(|found| starts_proviso(&text[found.end()..]))
}

/// What one effective-date sentence says.
#[derive(Clone, Copy, PartialEq)]
enum ClauseKind {
    /// The day of promulgation ("공포한 날부터 시행한다").
    Plain,
    /// A written date.
    Explicit(Date),
    /// Anything else: a condition, a relative period or an unknown form.
    Other,
}

/// One effective-date sentence and what follows it.
struct Clause {
    kind: ClauseKind,
    /// A proviso follows the sentence.
    proviso: bool,
    /// The sentence writes "시행 한다" with a space.
    spaced: bool,
    /// The sentence names the day of promulgation without "부터".
    without_buteo: bool,
    /// The sentence writes an explicit date with no particle after it.
    explicit_without_buteo: bool,
    /// A proviso-omitted mark follows the sentence.
    omitted_mark: bool,
    /// The detection found the sentence and the reader did not read it.
    unread: bool,
}

/// Reads the effective-date sentences of one section of an addendum.
fn read_section_clauses(section: &Section) -> Vec<Clause> {
    #[cfg(test)]
    if let Some(view) = &section.source_view {
        observe(
            "section_clauses",
            "read_section_clauses",
            section.after_heading,
            vec![("decoded", &section.body, view.clone())],
        );
    }
    read_clauses(&section.body, section.after_heading)
}

/// Tells whether the added detection finds a sentence in a text that r24 scanned (the body and
/// the addendum blocks of the source; user-r25c-structural-fix-20261112, user-holistic-design-20261112).
/// The added detection is the extended subjects (`bracket_subject_starts`) of the decoded text and
/// the sentences that the detection (r24 detection and extended subjects) finds in the text as the
/// source wrote it at a place where it finds none in the decoded text. Such a sentence is shown by
/// the written text only: an entity such as "&gt;" ends a sentence of the decoded text and not one
/// of the written text.
///
/// No sentence of the added detection is a sentence that the reader reads. The reader needs the
/// kind of text right before "은" or "는", and an extended subject has a note in between. A sentence
/// shown by the written text only holds an entity between its subject and its verb, so a clause
/// that the reader cuts out of it holds "<", ">" or "&" and is neither a plain nor an explicit
/// clause. So the gate does not ask whether the reader read the sentence (the first version did,
/// and the lazy match of the reader made it count a sentence as read when a later sentence of the
/// same line ended with "시행한다").
///
/// The r24 detection of the decoded text is not part of it: its rules and `names_effective_date`
/// keep their r24 results. A sentence that both texts show is the sentence of that detection.
#[cfg(test)]
fn legacy_added_any(text: &str, raw: &str) -> bool {
    if shown_after_cleaning(text, raw) || !bracket_subject_starts(text).is_empty() {
        return true;
    }
    if raw == text {
        return false;
    }
    let shown_by_both = written_offsets(raw, &extended_sentence_starts(text, false));
    extended_sentence_starts(raw, false)
        .iter()
        .any(|start| !shown_by_both.contains(start))
}

/// Tells whether the whole detection (r24 detection and extended subjects) finds a sentence in
/// the decoded text or in the text as the source wrote it. It serves the texts that r24 never
/// scanned (the addendum parts of an amendment text that no rule took, the amendment statement,
/// and the paragraph, item and subitem text of an article): any sentence of them that names an
/// effective date counts.
#[cfg(test)]
fn legacy_full_any(text: &str, raw: &str) -> bool {
    shown_after_cleaning(text, raw) || starts_total(text, raw) > 0
}

/// What a text that r24 never scanned adds to the two kinds of date: the first value holds back a
/// date that a rule read, the second a date that the default period gave.
///
/// A date that a rule read needs a reason that the text contradicts the rule: a sentence of the
/// added detection, or a sentence of the r24 detection that the reader does not read (the
/// criterion of the unread clause of r24, on the decoded text). A plain sentence that the reader
/// reads, such as the restatement of the addendum in the amendment text, holds nothing back. A
/// date from the default period is a date that no rule took from any sentence, so every sentence of
/// the whole detection holds it back, read or not.
#[cfg(test)]
fn legacy_gate_only_any(text: &str, raw: &str) -> (bool, bool) {
    let default = legacy_full_any(text, raw);
    (
        default && (legacy_added_any(text, raw) || unread_by_the_reader(text)),
        default,
    )
}

/// Tells whether the detection of r24 finds a sentence in the decoded text that the reader does not
/// read: the criterion that r24 uses for the unread clause (`read_clauses`).
#[cfg(test)]
fn unread_by_the_reader(text: &str) -> bool {
    let read: Vec<usize> = CLAUSE.find_iter(text).map(|found| found.start()).collect();
    detected_sentence_starts(text, false)
        .iter()
        .any(|start| !read.contains(start))
}

/// The number of starts that the whole detection finds in a text and in its written form.
#[cfg(test)]
fn starts_total(text: &str, raw: &str) -> usize {
    extended_sentence_starts(text, false).len()
        + if raw == text {
            0
        } else {
            extended_sentence_starts(raw, false).len()
        }
}

/// Tells whether the detection finds more sentences once the format characters (general category
/// Cf: zero-width space, soft hyphen, word joiner, byte order mark and the like) are taken out.
/// The characters are no white space, so a subject split by one stays unseen. The count is
/// compared, not the places: a sentence that both forms show is a sentence of the detection of
/// r24, and the places of the cleaned text differ from the places of the text.
#[cfg(test)]
fn shown_after_cleaning(text: &str, raw: &str) -> bool {
    static FORMAT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\p{Cf}").expect("valid format character pattern"));
    if !FORMAT.is_match(text) && !FORMAT.is_match(raw) {
        return false;
    }
    let clean = |text: &str| FORMAT.replace_all(text, "").into_owned();
    let (clean_text, clean_raw) = (clean(text), clean(raw));
    starts_total(&clean_text, &clean_raw) > starts_total(text, raw)
}

/// Tells whether an element whose text arrived in several XML events holds a sentence that r24
/// does not see. r24 joins the events with a line feed after each one, and the line feeds split a
/// subject ("이 조례", "(제1234호)는") that the source wrote in one piece. Each element is read
/// again as the source wrote it, and a larger count of sentences than in r24's form counts. A
/// sentence that both forms show stays a sentence of r24.
#[cfg(test)]
fn legacy_split_any(split: &[(String, String)], source: Option<&SourceView>) -> bool {
    split
        .iter()
        .enumerate()
        .any(|(_index, (with_line_feeds, as_written))| {
            // Each run is counted twice, as the source wrote it and as r24 joins it, and each count
            // reads the decoded text and the text as written.
            if let Some(source) = source.filter(|v| v.enabled) {
                let with_lf = &source.split[_index];
                let mut written = SourceSegments::default();
                for segment in &with_lf.segments {
                    if segment["source"].is_object() {
                        let range = segment["output_byte_range"].as_array().unwrap();
                        written.append_view(
                            &with_lf
                                .slice(
                                    range[0].as_u64().unwrap() as usize,
                                    range[1].as_u64().unwrap() as usize,
                                )
                                .unwrap(),
                        );
                    }
                }
                for (site, actual, view) in [
                    ("split_any:written", as_written.as_str(), written),
                    (
                        "split_any:linefeeds",
                        with_line_feeds.as_str(),
                        with_lf.clone(),
                    ),
                ] {
                    let decoded = view.decoded().expect("decode event run");
                    observe(
                        "event_run_gate",
                        site,
                        false,
                        vec![
                            ("decoded", decode_markup(actual).as_str(), decoded),
                            ("raw", actual, view),
                        ],
                    );
                }
            }
            let count = |text: &str| starts_total(&decode_markup(text), text);
            count(as_written) > count(with_line_feeds)
        })
}

/// The offsets in a text as the source wrote it that match offsets in its decoded text. The
/// decoding changes three entities only ("&lt;", "&gt;" and "&amp;" become one character each), the
/// three do not overlap in the source text, and the decoding changes no line feed, so the two
/// texts map offset by offset and no entity is cut in two.
#[cfg(test)]
fn written_offsets(raw: &str, decoded_offsets: &[usize]) -> Vec<usize> {
    let mut order = decoded_offsets.to_vec();
    order.sort_unstable();
    order.dedup();
    let (mut at, mut decoded) = (0, 0);
    let mut found = Vec::with_capacity(order.len());
    for target in order {
        while at < raw.len() && decoded < target {
            let rest = &raw[at..];
            let (raw_len, decoded_len) = if rest.starts_with("&lt;") || rest.starts_with("&gt;") {
                (4, 1)
            } else if rest.starts_with("&amp;") {
                (5, 1)
            } else {
                let len = rest.chars().next().map_or(1, char::len_utf8);
                (len, len)
            };
            at += raw_len;
            decoded += decoded_len;
        }
        found.push(at);
    }
    found
}

// The gate evidence of r25c (r25c-collection-contract.md, r25c-normalization-plan.v3.md). Each gate
// partition is one container of the SourceView. B is the view as the rules read it; N is the same
// container with the allowed normalization. Both views and their decoded forms are detected with
// the rules of B, and every success is a key of stable source coordinates. The gate compares the
// sets of keys, not the numbers of starts.

/// Tells whether a partition that r24 scanned (the body, an addendum block, an amendment part that
/// r24 read) holds a key that the r24 detection of B-decoded does not hold (E - R,
/// r25c-normalization-plan.v3.md 3): an extended subject, a sentence that only the written text
/// shows, or a sentence that only N shows. A source provenance error counts as such a key, so the
/// gate withholds the date and never takes the error for a missing sentence.
fn added_any(text: &str, raw: &str, evidence: &PartitionEvidence) -> bool {
    #[cfg(test)]
    let _entry = trace_entry("added_any", false, &[("decoded", text), ("raw", raw)]);
    #[cfg(not(test))]
    let _ = (text, raw);
    evidence.added()
}

/// What a partition that r24 never scanned (the paragraph, item and subitem text, the amendment
/// statement, an amendment part that r24 did not read) adds to the two kinds of date. The second
/// value holds back a date of the default period: any key of the part or a new key of the
/// partition. The first value holds back a date that a rule read: the second value and a new key,
/// or a sentence of the r24 detection that the reader does not read (`unread_by_the_reader` on the
/// B-decoded text).
fn gate_only_any(text: &str, raw: &str, evidence: &PartitionEvidence, part: usize) -> (bool, bool) {
    #[cfg(test)]
    let _entry = trace_entry("gate_only_any", false, &[("decoded", text), ("raw", raw)]);
    #[cfg(not(test))]
    let _ = (text, raw);
    evidence.unscanned_marks(part, PhraseVariant::All)
}

/// How the gate counts the PHRASE keys. The product uses `All`; the other two exist for the
/// comparison of the lead rulings (P5b).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PhraseVariant {
    /// PHRASE keys do not count (the P4 gate).
    None,
    /// New PHRASE keys (P_E - P_R) count everywhere; a text that r24 never scanned counts no
    /// PHRASE key of B.
    #[cfg_attr(not(test), allow(dead_code))]
    New,
    /// New PHRASE keys count everywhere, and every PHRASE key of a text that r24 never scanned
    /// counts for its default (lead ruling after P5b).
    All,
}

/// One logical source range: the element ID, the event ID of the parser piece and the byte range
/// in the piece (r25c-collection-contract.md 1).
type SourceRange = (u64, u64, usize, usize);

/// The text of every parser piece of a raw container view, by element ID and event ID.
type PieceTexts<'a> = std::collections::HashMap<(u64, u64), &'a str>;

/// One fixed-start success: the matcher, the start, the end and the end of the core (the end of
/// "행" for the terminal matcher, the end of the match for every other matcher).
type FixedMatch = (&'static str, usize, usize, usize);

/// One output unit of a view and the source ranges it stands for. A character of a parser piece is
/// one unit. A decoded entity and a join separator are one unit each and carry every range that
/// they replace. A unit without a range is synthetic (a line feed that the collector inserted).
#[derive(Clone, Debug)]
struct ViewUnit {
    start: usize,
    end: usize,
    sources: Vec<SourceRange>,
}

/// One evidence key of a gate partition (r25c-normalization-plan.v3.md 3). A range is an ordered
/// list of source ranges, so a range that crosses pieces keeps its pieces in order. The partition
/// ID is the partition that holds the key.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum EvidenceKey {
    /// An r24 subject and one successful verb of its sentence.
    Subject {
        subject: Vec<SourceRange>,
        verb: Vec<SourceRange>,
    },
    /// An extended (bracket note) subject and one successful verb of its sentence.
    Extended {
        subject: Vec<SourceRange>,
        verb: Vec<SourceRange>,
    },
    /// The start of a heading candidate after the r24 A/B calculation and dedup, and one
    /// successful verb of a residual fragment.
    Heading {
        candidate: (u64, u64, usize),
        verb: Vec<SourceRange>,
    },
    /// A success of the PHRASE pattern of `names_effective_date`.
    Phrase { phrase: Vec<SourceRange> },
}

/// One fixed-start matcher: its name, the one character that it can start with, its pattern and
/// its regex. The first character is the literal right after `\A` (and an optional opening group)
/// of the pattern; `matcher_first_characters_follow_the_patterns` derives it from the pattern and
/// fails when a pattern changes so that it could start with another character.
struct FixedMatcher {
    name: &'static str,
    first: char,
    #[cfg_attr(not(test), allow(dead_code))]
    pattern: &'static str,
    regex: Regex,
    /// The pattern has a core group (the terminal matcher); the core ends where the group ends.
    core: bool,
}

fn fixed_matcher(name: &'static str, first: char, pattern: &'static str) -> FixedMatcher {
    FixedMatcher {
        name,
        first,
        pattern,
        regex: Regex::new(pattern).expect("valid fixed-start matcher"),
        core: pattern.starts_with(r"\A("),
    }
}

/// The success matchers of `has_effective_verb`, each anchored at one start.
static VERB_MATCHERS: LazyLock<Vec<FixedMatcher>> = LazyLock::new(|| {
    vec![
        fixed_matcher(
            "korean_stem",
            '시',
            r"\A시\s*행\s*(?:[하한함할합해했되된됨될됩돼됐토키코케치]|시[키켜킨킬킴켰킵])",
        ),
        fixed_matcher("terminal", '시', r"\A(시\s*행)\s*$"),
        fixed_matcher("effect", '효', r"\A효력\s*[을이]\s*발생"),
        fixed_matcher("from_effect", '부', r"\A부터\s*(?:그\s*)?효력"),
        fixed_matcher("hanja", '施', r"\A施\s*行"),
    ]
});

/// The alternatives of the PHRASE pattern of `names_effective_date`, each anchored at one start.
static PHRASE_MATCHERS: LazyLock<Vec<FixedMatcher>> = LazyLock::new(|| {
    vec![
        fixed_matcher("phrase_from_enforcement", '부', r"\A부터\s*시행"),
        fixed_matcher("effect", '효', r"\A효력\s*[을이]\s*발생"),
        fixed_matcher("from_effect", '부', r"\A부터\s*(?:그\s*)?효력"),
    ]
});

/// Any character of the general category Cf.
static FORMAT_ANY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\p{Cf}").expect("valid format character pattern"));

/// The PHRASE pattern of `names_effective_date`. The enumeration of its alternatives must find a
/// success exactly when this pattern matches.
static PHRASE_ANY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"부터\s*시행|효력\s*[을이]\s*발생|부터\s*(?:그\s*)?효력")
        .expect("valid effective phrase pattern")
});

/// The subject pattern of `detected_sentence_starts`. The evidence repeats the r24 steps beside the
/// r24 function, and every view checks that the starts are the r24 starts.
static EVIDENCE_SUBJECT: LazyLock<Regex> = LazyLock::new(|| {
    let space = INLINE_SPACE;
    let word = r"[^\s.。]{0,6}?";
    Regex::new(&format!(
        r"(?:이|본|本)\s*(?:[^\s]{{0,6}}?|{word}(?:{space}+{word}){{0,2}})(?:{DETECT_KINDS})(?:[(（][^)）\n]{{0,4}}[)）])?{space}*(?:은|는|이|가)"
    ))
    .expect("valid effective sentence subject pattern")
});

/// The article mark pattern of `detected_sentence_starts`.
static EVIDENCE_ARTICLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"^\s*(?:제\s*\d+\s*조(?:의\s*\d+)?{}*(?:[(（][^)）\n]{{0,20}}[)）])?)?\s*",
        INLINE_SPACE
    ))
    .expect("valid article mark pattern")
});

/// The r22 heading tail pattern of `detected_sentence_starts`.
static EVIDENCE_AFTER_HEADING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^\s*(?:[<(\[〈＜（［][^>)\]〉＞）］\n]{0,60}[>)\]〉＞）］])?(?:\s*제\s*\d+\s*조\s*(?:\([^)\n]{0,20}\))?)?\s*",
    )
    .expect("valid heading tail pattern")
});

/// Tells whether a character has the general category Cf.
fn is_format_char(ch: char) -> bool {
    static FORMAT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^\p{Cf}$").expect("valid format character pattern"));
    let mut buffer = [0_u8; 4];
    FORMAT.is_match(ch.encode_utf8(&mut buffer))
}

/// Runs every matcher at every character boundary of a fragment and returns every success, the
/// overlapping ones too (r25c-normalization-plan.v3.md 4). The end of the fragment is the end of
/// `$`, as in `has_effective_verb`. A Chinese-character verb that "令" or "規" follows is no success.
fn fixed_start_matches(fragment: &str, matchers: &[FixedMatcher]) -> Vec<FixedMatch> {
    // The positions of the first characters of the matchers, in order; a matcher runs only where its
    // first character stands, and it cannot match anywhere else.
    static FIRST_CHARACTERS: LazyLock<Regex> = LazyLock::new(|| {
        let mut set: Vec<char> = VERB_MATCHERS
            .iter()
            .chain(PHRASE_MATCHERS.iter())
            .map(|matcher| matcher.first)
            .collect();
        set.sort_unstable();
        set.dedup();
        Regex::new(&format!(
            "[{}]",
            set.iter()
                .map(|ch| regex::escape(&ch.to_string()))
                .collect::<String>()
        ))
        .expect("valid first character set")
    });
    let mut found = Vec::new();
    for candidate in FIRST_CHARACTERS.find_iter(fragment) {
        let at = candidate.start();
        let first = fragment[at..]
            .chars()
            .next()
            .expect("a candidate character");
        if !matchers.iter().any(|matcher| matcher.first == first) {
            continue;
        }
        let suffix = &fragment[at..];
        for matcher in matchers.iter().filter(|matcher| matcher.first == first) {
            let (end, core) = if matcher.core {
                let Some(captures) = matcher.regex.captures(suffix) else {
                    continue;
                };
                let whole = captures.get(0).expect("whole match");
                (
                    whole.end(),
                    captures.get(1).map_or(whole.end(), |core| core.end()),
                )
            } else {
                let Some(whole) = matcher.regex.find(suffix) else {
                    continue;
                };
                (whole.end(), whole.end())
            };
            if matcher.name == "hanja"
                && matches!(suffix[end..].trim_start().chars().next(), Some('令' | '規'))
            {
                continue;
            }
            found.push((matcher.name, at, at + end, at + core));
        }
    }
    found
}

/// The successful verbs of one fragment at `offset` of a view. The enumeration must agree with
/// the bool of `has_effective_verb`; a difference is an error of the evidence.
fn fragment_verbs(fragment: &str, offset: usize, errors: &mut Vec<String>) -> Vec<FixedMatch> {
    #[cfg(test)]
    FRAGMENT_LOG.with(|slot| {
        if let Some(log) = slot.borrow_mut().as_mut() {
            log.push(fragment.to_owned());
        }
    });
    let verbs: Vec<FixedMatch> = fixed_start_matches(fragment, &VERB_MATCHERS)
        .into_iter()
        .map(|(name, start, end, core)| (name, start + offset, end + offset, core + offset))
        .collect();
    if verbs.is_empty() == has_effective_verb(fragment) {
        errors.push(format!(
            "verb enumeration at {offset} disagrees with has_effective_verb"
        ));
    }
    verbs
}

/// The r24 detection of one text, step by step as `detected_sentence_starts`, with every verb.
struct R24Evidence {
    /// Each detected subject: its start, its end and the verbs of its sentence.
    subjects: Vec<(usize, usize, Vec<FixedMatch>)>,
    /// Each detected heading candidate start and the verbs of its residual fragments.
    headings: Vec<(usize, Vec<FixedMatch>)>,
    /// The starts in the order of `detected_sentence_starts`.
    starts: Vec<usize>,
}

/// Repeats `detected_sentence_starts` and keeps each subject range, each heading candidate start
/// and each verb. The residual fragments of a heading are checked one by one, each with its own
/// end as the end of `$` (r25c-normalization-plan.v3.md 3-4).
fn r24_evidence(text: &str, after_heading: bool, errors: &mut Vec<String>) -> R24Evidence {
    let mut starts = Vec::new();
    let mut sentences: Vec<(usize, usize)> = Vec::new();
    let mut subjects = Vec::new();
    for subject in EVIDENCE_SUBJECT.find_iter(text) {
        let tail = &text[subject.end()..];
        let end = sentence_end(tail);
        let verbs = fragment_verbs(&tail[..end], subject.end(), errors);
        if has_effective_verb(&tail[..end]) {
            starts.push(subject.start());
            sentences.push((subject.start(), subject.end() + end));
            subjects.push((subject.start(), subject.end(), verbs));
        }
    }
    let mut candidates: Vec<usize> = Vec::new();
    let article = |at: usize| {
        at + EVIDENCE_ARTICLE
            .find(&text[at..])
            .map_or(0, |mark| mark.end())
    };
    if after_heading {
        candidates.push(article(0));
        candidates.push(
            EVIDENCE_AFTER_HEADING
                .find(text)
                .map_or(0, |mark| mark.end()),
        );
    }
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if line.trim_start().starts_with('부')
            && let Some(heading) = heading_line(&text[offset..])
        {
            let (_, _, after) = heading_label(heading.rest, None);
            candidates.push(article(after.as_ptr() as usize - text.as_ptr() as usize));
            let tail_at = heading.tail.as_ptr() as usize - text.as_ptr() as usize;
            candidates.push(
                tail_at
                    + EVIDENCE_AFTER_HEADING
                        .find(heading.tail)
                        .map_or(0, |mark| mark.end()),
            );
        }
        offset += line.len();
    }
    candidates.sort_unstable();
    candidates.dedup();
    let mut headings = Vec::new();
    for start in candidates {
        let end = start + sentence_end(&text[start..]);
        let mut cursor = start;
        let mut left_over = false;
        let mut verbs = Vec::new();
        for &(from, to) in sentences
            .iter()
            .filter(|(from, _)| (start..end).contains(from))
        {
            if from > cursor {
                verbs.extend(fragment_verbs(&text[cursor..from], cursor, errors));
                left_over |= has_effective_verb(&text[cursor..from]);
            }
            cursor = cursor.max(to.min(end));
        }
        if cursor < end {
            verbs.extend(fragment_verbs(&text[cursor..end], cursor, errors));
            left_over |= has_effective_verb(&text[cursor..end]);
        }
        if left_over {
            starts.push(start);
            headings.push((start, verbs));
        }
    }
    R24Evidence {
        subjects,
        headings,
        starts,
    }
}

/// Adds the source ranges of one segment (and of the segments it inherits) in order.
fn origin_ranges(segment: &Value, out: &mut Vec<SourceRange>) -> Result<()> {
    if let Some(source) = segment.get("source").filter(|source| source.is_object()) {
        let range = source["piece_byte_range"]
            .as_array()
            .context("origin piece range")?;
        out.push((
            source["element_id"].as_u64().context("origin element")?,
            source["event_id"].as_u64().context("origin event")?,
            range[0].as_u64().context("origin start")? as usize,
            range[1].as_u64().context("origin end")? as usize,
        ));
    } else if let Some(inherited) = segment.get("inherited_origins").and_then(Value::as_array) {
        for origin in inherited {
            origin_ranges(origin, out)?;
        }
    } else {
        ensure!(
            segment.get("separator").and_then(Value::as_str).is_some(),
            "segment without origin"
        );
    }
    Ok(())
}

/// The output units of a view, in order. The units cover the view without a gap. The engine uses
/// `unit_index`, which gives the same units only for the ranges that it maps; this eager list is
/// the reference of the tests.
#[cfg(test)]
fn view_units(view: &SourceSegments) -> Result<Vec<ViewUnit>> {
    let mut units = Vec::new();
    let mut cursor = 0;
    for segment in &view.segments {
        let range = segment["output_byte_range"]
            .as_array()
            .context("segment range")?;
        let start = range[0].as_u64().context("segment start")? as usize;
        let end = range[1].as_u64().context("segment end")? as usize;
        ensure!(
            start == cursor && end >= start && end <= view.text.len(),
            "segment coverage"
        );
        cursor = end;
        // An empty parser piece (an empty CDATA section) has no output unit.
        if start == end {
            continue;
        }
        let direct = segment.get("source").is_some_and(Value::is_object)
            && segment.get("inherited_origins").is_none();
        let mut sources = Vec::new();
        origin_ranges(segment, &mut sources)?;
        if direct {
            let (element, event, base, piece_end) = sources[0];
            ensure!(piece_end - base == end - start, "piece range length");
            for (offset, ch) in view.text[start..end].char_indices() {
                let at = start + offset;
                let len = ch.len_utf8();
                units.push(ViewUnit {
                    start: at,
                    end: at + len,
                    sources: vec![(element, event, base + offset, base + offset + len)],
                });
            }
        } else {
            units.push(ViewUnit {
                start,
                end,
                sources,
            });
        }
    }
    ensure!(cursor == view.text.len(), "segment coverage end");
    Ok(units)
}

/// The text of each parser piece of a raw container view (each piece stands there whole).
fn piece_texts(raw: &SourceSegments) -> PieceTexts<'_> {
    let mut pieces = PieceTexts::new();
    for segment in &raw.segments {
        let Some(source) = segment.get("source").filter(|source| source.is_object()) else {
            continue;
        };
        let (Some(element), Some(event), Some(piece), Some(output)) = (
            source["element_id"].as_u64(),
            source["event_id"].as_u64(),
            source["piece_byte_range"].as_array(),
            segment["output_byte_range"].as_array(),
        ) else {
            continue;
        };
        if piece[0].as_u64() == Some(0)
            && let (Some(from), Some(to)) = (output[0].as_u64(), output[1].as_u64())
            && let Some(text) = raw.text.get(from as usize..to as usize)
        {
            pieces.insert((element, event), text);
        }
    }
    pieces
}

/// Joins the ranges of one piece that touch, or that only deleted Cf characters of the piece
/// separate: the deleted characters inside a range stay in it (r25c-normalization-plan.v3.md 3).
fn canonical_ranges(ranges: &[SourceRange], pieces: &PieceTexts) -> Vec<SourceRange> {
    let mut result: Vec<SourceRange> = Vec::new();
    for &(element, event, start, end) in ranges {
        if let Some(last) = result.last_mut()
            && last.0 == element
            && last.1 == event
            && last.3 <= start
            && (last.3 == start
                || pieces
                    .get(&(element, event))
                    .and_then(|text| text.get(last.3..start))
                    .is_some_and(|gap| gap.chars().all(is_format_char)))
        {
            last.3 = end;
            continue;
        }
        result.push((element, event, start, end));
    }
    result
}

/// One non-empty segment of a view: a direct piece segment (each character is one unit), or one
/// unit that carries every source range it stands for (none for a synthetic separator).
enum IndexedSegment {
    Direct {
        element: u64,
        event: u64,
        base: usize,
    },
    Whole(Vec<SourceRange>),
}

/// The segments of a view with their output ranges. It gives the units of a requested range only,
/// so a view costs memory per segment, not per character. The units are the units of `view_units`.
struct UnitIndex<'a> {
    text: &'a str,
    segments: Vec<(usize, usize, IndexedSegment)>,
}

/// Builds the segment index of a view with the checks of `view_units`.
fn unit_index(view: &SourceSegments) -> Result<UnitIndex<'_>> {
    let mut segments = Vec::new();
    let mut cursor = 0;
    for segment in &view.segments {
        let range = segment["output_byte_range"]
            .as_array()
            .context("segment range")?;
        let start = range[0].as_u64().context("segment start")? as usize;
        let end = range[1].as_u64().context("segment end")? as usize;
        ensure!(
            start == cursor && end >= start && end <= view.text.len(),
            "segment coverage"
        );
        cursor = end;
        // An empty parser piece (an empty CDATA section) has no output unit.
        if start == end {
            continue;
        }
        let direct = segment.get("source").is_some_and(Value::is_object)
            && segment.get("inherited_origins").is_none();
        let mut sources = Vec::new();
        origin_ranges(segment, &mut sources)?;
        if direct {
            let (element, event, base, piece_end) = sources[0];
            ensure!(piece_end - base == end - start, "piece range length");
            segments.push((
                start,
                end,
                IndexedSegment::Direct {
                    element,
                    event,
                    base,
                },
            ));
        } else {
            segments.push((start, end, IndexedSegment::Whole(sources)));
        }
    }
    ensure!(cursor == view.text.len(), "segment coverage end");
    Ok(UnitIndex {
        text: view.text.as_str(),
        segments,
    })
}

impl UnitIndex<'_> {
    /// The start of the character that holds a byte offset inside a direct segment.
    fn char_start(&self, mut at: usize) -> usize {
        while !self.text.is_char_boundary(at) {
            at -= 1;
        }
        at
    }

    /// The units that overlap a range, in order.
    fn overlapping(&self, start: usize, end: usize) -> Vec<ViewUnit> {
        let mut units = Vec::new();
        let first = self
            .segments
            .partition_point(|(_, segment_end, _)| *segment_end <= start);
        for (segment_start, segment_end, segment) in &self.segments[first..] {
            if *segment_start >= end {
                break;
            }
            match segment {
                IndexedSegment::Whole(sources) => units.push(ViewUnit {
                    start: *segment_start,
                    end: *segment_end,
                    sources: sources.clone(),
                }),
                IndexedSegment::Direct {
                    element,
                    event,
                    base,
                } => {
                    let mut at = self.char_start(start.max(*segment_start));
                    while at < (*segment_end).min(end) {
                        let len = self.text[at..].chars().next().map_or(1, char::len_utf8);
                        let offset = at - segment_start;
                        units.push(ViewUnit {
                            start: at,
                            end: at + len,
                            sources: vec![(*element, *event, base + offset, base + offset + len)],
                        });
                        at += len;
                    }
                }
            }
        }
        units
    }

    /// The units of a view range. Both ends must be unit boundaries.
    fn units_in(&self, start: usize, end: usize) -> Result<Vec<ViewUnit>> {
        ensure!(start < end, "empty range");
        let selected = self.overlapping(start, end);
        ensure!(!selected.is_empty(), "range outside the view");
        ensure!(
            selected[0].start == start && selected[selected.len() - 1].end == end,
            "range boundary inside a unit"
        );
        Ok(selected)
    }
}

/// Maps a subject range: its first and its last unit need a source. A synthetic unit inside
/// adds no range.
fn map_bounded(units: &UnitIndex, start: usize, end: usize) -> Result<Vec<SourceRange>> {
    let selected = units.units_in(start, end)?;
    ensure!(
        !selected[0].sources.is_empty() && !selected[selected.len() - 1].sources.is_empty(),
        "subject boundary without source"
    );
    Ok(selected
        .iter()
        .flat_map(|unit| unit.sources.iter().copied())
        .collect())
}

/// Maps a verb or a phrase. Every unit before the end of the core needs a source. After it only
/// synthetic white space may end the match; it adds no range and is counted as consumed
/// (r25c-collection-contract.md 3.1). A real unit after a synthetic one is an error.
fn map_verb(
    units: &UnitIndex,
    text: &str,
    start: usize,
    end: usize,
    core: usize,
) -> Result<(Vec<SourceRange>, usize)> {
    let selected = units.units_in(start, end)?;
    let mut ranges = Vec::new();
    let mut consumed = 0;
    for unit in selected {
        if unit.sources.is_empty() {
            ensure!(unit.start >= core, "verb core without source");
            ensure!(
                text[unit.start..unit.end].chars().all(char::is_whitespace),
                "synthetic verb character"
            );
            consumed += 1;
        } else {
            ensure!(consumed == 0, "synthetic separator inside a verb");
            ranges.extend(unit.sources.iter().copied());
        }
    }
    ensure!(!ranges.is_empty(), "verb without source");
    Ok((ranges, consumed))
}

/// Maps a heading candidate start: the unit that starts there needs a source.
fn map_point(units: &UnitIndex, at: usize) -> Result<(u64, u64, usize)> {
    let index = units.segments.partition_point(|(_, end, _)| *end <= at);
    let (start, _, segment) = units
        .segments
        .get(index)
        .context("candidate start outside the view")?;
    match segment {
        IndexedSegment::Whole(sources) => {
            ensure!(*start == at, "candidate start inside a unit");
            let first = sources.first().context("candidate start without source")?;
            Ok((first.0, first.1, first.2))
        }
        IndexedSegment::Direct {
            element,
            event,
            base,
        } => {
            ensure!(
                units.char_start(at.max(*start)) == at,
                "candidate start inside a unit"
            );
            Ok((*element, *event, base + (at - start)))
        }
    }
}

/// The N form of a raw container view (r25c-collection-contract.md 3): the line feed that
/// `collect_date_text` inserted between two adjacent pieces of the same element and run goes, and
/// every source character of the general category Cf goes. Every other character and separator
/// stays with its origin.
fn normalized_raw(raw: &SourceSegments) -> Result<SourceSegments> {
    let direct = |segment: &Value| {
        segment
            .get("source")
            .filter(|source| source.is_object())
            .map(|source| (source["element_id"].as_u64(), source["run_id"].as_u64()))
    };
    // The clipped copy of one segment, as `slice` and `append_view` give it.
    let push = |result: &mut SourceSegments,
                segment: &Value,
                segment_start: usize,
                from: usize,
                to: usize|
     -> Result<()> {
        ensure!(
            from < to && raw.text.is_char_boundary(from) && raw.text.is_char_boundary(to),
            "invalid SourceView slice"
        );
        let mut clipped = segment.clone();
        let output = result.text.len();
        clipped["output_byte_range"] = json!([output, output + (to - from)]);
        if let Some(source) = clipped.get_mut("source").and_then(Value::as_object_mut) {
            let old = source["piece_byte_range"]
                .as_array()
                .context("piece range")?;
            let base = old[0].as_u64().context("piece start")? as usize;
            source.insert(
                "piece_byte_range".to_owned(),
                json!([base + from - segment_start, base + to - segment_start]),
            );
        }
        result.text.push_str(&raw.text[from..to]);
        result.segments.push(clipped);
        Ok(())
    };
    let mut result = SourceSegments::default();
    for (index, segment) in raw.segments.iter().enumerate() {
        let range = segment["output_byte_range"]
            .as_array()
            .context("segment range")?;
        let start = range[0].as_u64().context("segment start")? as usize;
        let end = range[1].as_u64().context("segment end")? as usize;
        if direct(segment).is_some() {
            if start == end {
                continue;
            }
            if !FORMAT_ANY.is_match(&raw.text[start..end]) {
                push(&mut result, segment, start, start, end)?;
                continue;
            }
            // Each Cf character goes; every run of other characters stays.
            let mut from = start;
            for format in FORMAT_ANY.find_iter(&raw.text[start..end]) {
                let at = start + format.start();
                if at > from {
                    push(&mut result, segment, start, from, at)?;
                }
                from = start + format.end();
            }
            if from < end {
                push(&mut result, segment, start, from, end)?;
            }
            continue;
        }
        ensure!(
            segment.get("inherited_origins").is_none(),
            "transformed segment in a raw container view"
        );
        let separator = segment
            .get("separator")
            .and_then(Value::as_str)
            .context("segment without origin")?;
        if separator == "event-inserted-lf" {
            let run = segment["run_id"].as_u64();
            let before = index
                .checked_sub(1)
                .and_then(|at| raw.segments.get(at))
                .and_then(direct);
            let after = raw.segments.get(index + 1).and_then(direct);
            if let (Some((Some(left), left_run)), Some((Some(right), right_run))) = (before, after)
                && left == right
                && run.is_some()
                && left_run == run
                && right_run == run
            {
                continue;
            }
        }
        push(&mut result, segment, start, start, end)?;
    }
    Ok(result)
}

/// Tells whether a text holds an entity that `SourceSegments::decoded` replaces. Without one, the
/// decoded view shows the same text and units as the view itself.
fn has_entity(text: &str) -> bool {
    text.contains("&lt;") || text.contains("&gt;") || text.contains("&amp;")
}

/// Tells whether `normalized_raw` removes anything from a raw container view (a Cf character or an
/// event line feed between two pieces of one run), with the same checks. Without a removal, the N
/// view shows the same text and units as the raw view.
fn normalization_changes(raw: &SourceSegments) -> Result<bool> {
    let direct = |segment: &Value| {
        segment
            .get("source")
            .filter(|source| source.is_object())
            .map(|source| (source["element_id"].as_u64(), source["run_id"].as_u64()))
    };
    let mut changes = false;
    for (index, segment) in raw.segments.iter().enumerate() {
        let range = segment["output_byte_range"]
            .as_array()
            .context("segment range")?;
        let start = range[0].as_u64().context("segment start")? as usize;
        let end = range[1].as_u64().context("segment end")? as usize;
        if direct(segment).is_some() {
            changes |= start < end && FORMAT_ANY.is_match(&raw.text[start..end]);
            continue;
        }
        ensure!(
            segment.get("inherited_origins").is_none(),
            "transformed segment in a raw container view"
        );
        let separator = segment
            .get("separator")
            .and_then(Value::as_str)
            .context("segment without origin")?;
        if separator == "event-inserted-lf" {
            let run = segment["run_id"].as_u64();
            let before = index
                .checked_sub(1)
                .and_then(|at| raw.segments.get(at))
                .and_then(direct);
            let after = raw.segments.get(index + 1).and_then(direct);
            if let (Some((Some(left), left_run)), Some((Some(right), right_run))) = (before, after)
                && left == right
                && run.is_some()
                && left_run == run
                && right_run == run
            {
                changes = true;
            }
        }
    }
    Ok(changes)
}

/// The keys of the B views of one B part.
#[derive(Default)]
struct PartKeys {
    keys: BTreeSet<EvidenceKey>,
    /// The PHRASE keys of the B views of the part.
    phrases: BTreeSet<EvidenceKey>,
    /// The r24 detection of B-decoded finds a sentence that the reader does not read, outside the
    /// consumed approved sentence (`unread_by_the_reader` without C).
    unread: bool,
}

/// The evidence of one gate partition `(source file, call family, stable container)`
/// (r25c-normalization-plan.v3.md 2). One resolve reads one source file.
struct PartitionEvidence {
    family: &'static str,
    container: String,
    /// R: the r24 subject and heading keys of B-decoded.
    r: BTreeSet<EvidenceKey>,
    /// E: the r24 subject, extended subject and heading keys of the four views, each with the
    /// first view and text that showed it.
    all: BTreeMap<EvidenceKey, Value>,
    /// The PHRASE keys of the B views and of the N views (P_E is their union).
    phrase_b: BTreeMap<EvidenceKey, Value>,
    phrase_n: BTreeMap<EvidenceKey, Value>,
    /// P_R: the PHRASE keys of B-decoded.
    phrase_r: BTreeSet<EvidenceKey>,
    /// The source ranges of the approved sentence that a selected approval consumed. Every key
    /// inside them (C) is left out of every signal of the gate.
    consumed_spans: Vec<SourceRange>,
    /// The keys of the B views of each B part, in the order of the parts.
    parts: Vec<PartKeys>,
    /// Source provenance and evidence errors, of the PHRASE keys too. Each one holds back the date.
    errors: Vec<String>,
    /// The synthetic white space that the end of a verb consumed (collection contract 3.1).
    consumed_synthetic: usize,
    #[cfg(test)]
    views: Vec<(String, BTreeMap<EvidenceKey, Value>)>,
    /// The views that the partition detected, for the views that are the same.
    cache: Vec<CachedView>,
}

impl PartitionEvidence {
    fn new(family: &'static str, container: String, consumed: &[SourceRange]) -> Self {
        Self {
            family,
            container,
            r: BTreeSet::new(),
            all: BTreeMap::new(),
            phrase_b: BTreeMap::new(),
            phrase_n: BTreeMap::new(),
            phrase_r: BTreeSet::new(),
            consumed_spans: consumed.to_vec(),
            parts: Vec::new(),
            errors: Vec::new(),
            consumed_synthetic: 0,
            #[cfg(test)]
            views: Vec::new(),
            cache: Vec::new(),
        }
    }

    /// The key belongs to the consumed approved sentence (C).
    fn consumed(&self, key: &EvidenceKey) -> bool {
        key_within(&self.consumed_spans, key)
    }

    /// E - R - C is not empty.
    fn new_keys(&self) -> bool {
        self.all
            .keys()
            .any(|key| !self.r.contains(key) && !self.consumed(key))
    }

    /// pnew: P_E - P_R - C is not empty.
    fn new_phrases(&self) -> bool {
        self.phrase_b
            .keys()
            .chain(self.phrase_n.keys())
            .any(|key| !self.phrase_r.contains(key) && !self.consumed(key))
    }

    /// The added signal of the product gate: an error, E - R - C or pnew.
    fn added(&self) -> bool {
        self.added_with(PhraseVariant::All)
    }

    fn added_with(&self, variant: PhraseVariant) -> bool {
        !self.errors.is_empty()
            || self.new_keys()
            || (variant != PhraseVariant::None && self.new_phrases())
    }

    /// The rule and default marks of one B part that r24 scanned. A date that an approval with a
    /// gate policy selected passed no rule that reads text, so a sentence that the reader does not
    /// read (outside C) holds it back too (r25c-design.md 8).
    fn scanned_marks(&self, index: usize, variant: PhraseVariant, approval: bool) -> (bool, bool) {
        let added = self.added_with(variant);
        let unread = approval && self.parts.get(index).is_none_or(|part| part.unread);
        (added || unread, added)
    }

    /// The rule and default marks of one B part that r24 never scanned (the paragraph text, the
    /// amendment statement, an amendment part that r24 did not read).
    fn unscanned_marks(&self, index: usize, variant: PhraseVariant) -> (bool, bool) {
        let added = self.added_with(variant);
        let Some(part) = self.parts.get(index) else {
            return (true, true);
        };
        let keys = part.keys.iter().any(|key| !self.consumed(key));
        let phrases =
            variant == PhraseVariant::All && part.phrases.iter().any(|key| !self.consumed(key));
        let default = added || keys || phrases;
        (default && (added || part.unread), default)
    }

    /// Detects one view with the rules of B and adds its keys. `part` is the B part of a B view. A
    /// view with the same text and the same non-empty segments as a view that the partition already
    /// detected has the same outcome, so the outcome is applied again instead of detected again.
    fn add_view(
        &mut self,
        name: &'static str,
        view: &SourceSegments,
        pieces: &PieceTexts,
        after_heading: bool,
        part: Option<usize>,
    ) -> usize {
        let found = self.cache.iter().position(|entry| {
            entry.after_heading == after_heading
                && entry.text == view.text
                && entry.segments.iter().eq(view
                    .segments
                    .iter()
                    .filter(|segment| !empty_segment(segment)))
        });
        let index = match found {
            Some(index) => index,
            None => {
                let outcome = detect_view(view, pieces, after_heading);
                self.cache.push(CachedView {
                    text: view.text.clone(),
                    segments: view
                        .segments
                        .iter()
                        .filter(|segment| !empty_segment(segment))
                        .cloned()
                        .collect(),
                    after_heading,
                    outcome,
                });
                self.cache.len() - 1
            }
        };
        let unread = if name == "B-decoded" && part.is_some() {
            if self.cache[index].outcome.unread.is_none() {
                let value = unread_outside(
                    view,
                    &self.cache[index].outcome,
                    after_heading,
                    &self.consumed_spans,
                );
                self.cache[index].outcome.unread = Some(value);
            }
            self.cache[index].outcome.unread.unwrap_or(false)
        } else {
            false
        };
        self.apply(name, index, part, unread);
        index
    }

    /// Applies the outcome of an N view whose view equals the view of a detected entry.
    fn apply_cached(&mut self, name: &'static str, index: usize) {
        self.apply(name, index, None, false);
    }

    fn apply(&mut self, name: &'static str, index: usize, part: Option<usize>, unread: bool) {
        let outcome = &self.cache[index].outcome;
        let named = |value: &Value| {
            let mut value = value.clone();
            value["view"] = json!(name);
            value
        };
        self.consumed_synthetic += outcome.consumed_synthetic;
        self.errors.extend(
            outcome
                .errors
                .iter()
                .map(|error| format!("{}/{}/{name}: {error}", self.family, self.container)),
        );
        if name == "B-decoded" {
            self.r.extend(outcome.r24.iter().cloned());
            self.phrase_r.extend(outcome.phrases.keys().cloned());
        }
        if let Some(index) = part
            && let Some(part) = self.parts.get_mut(index)
        {
            part.keys.extend(outcome.found.keys().cloned());
            part.phrases.extend(outcome.phrases.keys().cloned());
            if name == "B-decoded" {
                part.unread = unread;
            }
        }
        let target = if name.starts_with('B') {
            &mut self.phrase_b
        } else {
            &mut self.phrase_n
        };
        for (key, value) in &outcome.phrases {
            if !target.contains_key(key) {
                target.insert(key.clone(), named(value));
            }
        }
        #[cfg(test)]
        self.views.push((
            name.to_owned(),
            outcome
                .found
                .iter()
                .map(|(key, value)| (key.clone(), named(value)))
                .collect(),
        ));
        for (key, value) in &outcome.found {
            if !self.all.contains_key(key) {
                self.all.insert(key.clone(), named(value));
            }
        }
    }
}

/// A segment with an empty output range (an empty parser piece). It has no unit, so it does not
/// change what a view shows.
fn empty_segment(segment: &Value) -> bool {
    segment["output_byte_range"][0] == segment["output_byte_range"][1]
}

/// A detected view of a partition and its outcome.
struct CachedView {
    text: String,
    segments: Vec<Value>,
    after_heading: bool,
    outcome: ViewOutcome,
}

/// What one view shows before the view name is attached: the keys with their first description,
/// the r24 keys, the PHRASE keys, the r24 starts and the errors.
struct ViewOutcome {
    found: BTreeMap<EvidenceKey, Value>,
    phrases: BTreeMap<EvidenceKey, Value>,
    r24: BTreeSet<EvidenceKey>,
    starts: Vec<usize>,
    units_ok: bool,
    /// The reader criterion of the view, once a B-decoded part asked for it.
    unread: Option<bool>,
    errors: Vec<String>,
    consumed_synthetic: usize,
}

/// Detects one view with the rules of B.
fn detect_view(view: &SourceSegments, pieces: &PieceTexts, after_heading: bool) -> ViewOutcome {
    let mut outcome = ViewOutcome {
        found: BTreeMap::new(),
        phrases: BTreeMap::new(),
        r24: BTreeSet::new(),
        starts: Vec::new(),
        units_ok: false,
        unread: None,
        errors: Vec::new(),
        consumed_synthetic: 0,
    };
    let units = match unit_index(view) {
        Err(error) => {
            outcome.errors.push(format!("view units: {error}"));
            return outcome;
        }
        Ok(units) => units,
    };
    outcome.units_ok = true;
    let text = view.text.as_str();
    let mut errors = Vec::new();
    let evidence = r24_evidence(text, after_heading, &mut errors);
    // The r24 starts of the evidence must be the starts of r24; a difference is an error that holds
    // the date back, so this check is part of the result.
    if evidence.starts != detected_sentence_starts(text, after_heading) {
        errors.push("r24 starts differ from detected_sentence_starts".to_owned());
    }
    let mut consumed_synthetic = 0;
    let mut verb_key = |verb: &FixedMatch, errors: &mut Vec<String>| -> Option<Vec<SourceRange>> {
        match map_verb(&units, text, verb.1, verb.2, verb.3) {
            Ok((ranges, consumed)) => {
                consumed_synthetic += consumed;
                Some(canonical_ranges(&ranges, pieces))
            }
            Err(error) => {
                errors.push(format!("verb {}..{}: {error}", verb.1, verb.2));
                None
            }
        }
    };
    let describe = |kind: &str, start: usize, end: usize, verb: &FixedMatch| {
        json!({"view": "", "kind": kind, "text": &text[start..end], "output": [start, end],
            "matcher": verb.0, "verb": &text[verb.1..verb.2], "verb_output": [verb.1, verb.2]})
    };
    for (start, end, verbs) in &evidence.subjects {
        match map_bounded(&units, *start, *end) {
            Err(error) => errors.push(format!("subject {start}..{end}: {error}")),
            Ok(subject) => {
                let subject = canonical_ranges(&subject, pieces);
                for verb in verbs {
                    if let Some(ranges) = verb_key(verb, &mut errors) {
                        let key = EvidenceKey::Subject {
                            subject: subject.clone(),
                            verb: ranges,
                        };
                        outcome.r24.insert(key.clone());
                        outcome
                            .found
                            .entry(key)
                            .or_insert_with(|| describe("subject", *start, *end, verb));
                    }
                }
            }
        }
    }
    for (start, verbs) in &evidence.headings {
        match map_point(&units, *start) {
            Err(error) => errors.push(format!("heading {start}: {error}")),
            Ok(candidate) => {
                for verb in verbs {
                    if let Some(ranges) = verb_key(verb, &mut errors) {
                        let key = EvidenceKey::Heading {
                            candidate,
                            verb: ranges,
                        };
                        outcome.r24.insert(key.clone());
                        outcome
                            .found
                            .entry(key)
                            .or_insert_with(|| describe("heading", *start, *start, verb));
                    }
                }
            }
        }
    }
    for (start, end) in bracket_subject_spans(text) {
        let tail = &text[end..];
        let verbs = fragment_verbs(&tail[..sentence_end(tail)], end, &mut errors);
        match map_bounded(&units, start, end) {
            Err(error) => errors.push(format!("extended subject {start}..{end}: {error}")),
            Ok(subject) => {
                let subject = canonical_ranges(&subject, pieces);
                for verb in &verbs {
                    if let Some(ranges) = verb_key(verb, &mut errors) {
                        let key = EvidenceKey::Extended {
                            subject: subject.clone(),
                            verb: ranges,
                        };
                        outcome
                            .found
                            .entry(key)
                            .or_insert_with(|| describe("extended", start, end, verb));
                    }
                }
            }
        }
    }
    let matches = fixed_start_matches(text, &PHRASE_MATCHERS);
    if matches.is_empty() == PHRASE_ANY.is_match(text) {
        errors.push("phrase enumeration disagrees with the PHRASE pattern".to_owned());
    }
    // A phrase is a success of `names_effective_date` on the whole text: its first and last
    // character need a source, as for a subject.
    for phrase in &matches {
        match map_bounded(&units, phrase.1, phrase.2) {
            Ok(ranges) => {
                let key = EvidenceKey::Phrase {
                    phrase: canonical_ranges(&ranges, pieces),
                };
                outcome
                    .phrases
                    .entry(key)
                    .or_insert_with(|| describe("phrase", phrase.1, phrase.2, phrase));
            }
            Err(error) => errors.push(format!("phrase {}..{}: {error}", phrase.1, phrase.2)),
        }
    }
    outcome.consumed_synthetic = consumed_synthetic;
    outcome.starts = evidence.starts;
    outcome.errors = errors;
    outcome
}

/// The reader criterion of r24 (`unread_by_the_reader`) on a view, without the consumed sentence.
fn unread_outside(
    view: &SourceSegments,
    outcome: &ViewOutcome,
    after_heading: bool,
    consumed: &[SourceRange],
) -> bool {
    if !outcome.units_ok {
        return false;
    }
    let Ok(units) = unit_index(view) else {
        return false;
    };
    let text = view.text.as_str();
    let read: Vec<usize> = CLAUSE.find_iter(text).map(|found| found.start()).collect();
    let starts = if after_heading {
        detected_sentence_starts(text, false)
    } else {
        outcome.starts.clone()
    };
    starts.iter().any(|start| {
        !read.contains(start)
            && !map_point(&units, *start)
                .is_ok_and(|point| ranges_within(consumed, &[(point.0, point.1, point.2, point.2)]))
    })
}

/// The evidence of one container partition (the body, an addendum block, the paragraph text, the
/// amendment statement). B is the container view as collected; N removes what the collection
/// contract allows. The B bytes must equal the text that the rules read.
fn container_evidence(
    family: &'static str,
    container: String,
    raw: &SourceSegments,
    text: &str,
    raw_text: &str,
    consumed: &[SourceRange],
) -> PartitionEvidence {
    container_evidence_with(family, container, raw, text, raw_text, false, consumed)
}

fn container_evidence_with(
    family: &'static str,
    container: String,
    raw: &SourceSegments,
    text: &str,
    raw_text: &str,
    after_heading: bool,
    consumed: &[SourceRange],
) -> PartitionEvidence {
    let mut evidence = PartitionEvidence::new(family, container, consumed);
    evidence.parts.push(PartKeys::default());
    let pieces = piece_texts(raw);
    // A view that would equal a view at hand is not built: the raw view decodes to itself without an
    // entity, and normalizes to itself when N removes nothing.
    let built = (|| -> Result<(Option<SourceSegments>, Option<SourceSegments>, Option<SourceSegments>)> {
        let decoded = if has_entity(&raw.text) { Some(raw.decoded()?) } else { None };
        let decoded_text = decoded.as_ref().map_or(raw.text.as_str(), |view| view.text.as_str());
        ensure!(decoded_text == text && raw.text == raw_text, "B view bytes differ from the text of the rules");
        let normalized = if normalization_changes(raw)? { Some(normalized_raw(raw)?) } else { None };
        let normalized_decoded = match &normalized {
            Some(view) if has_entity(&view.text) => Some(view.decoded()?),
            _ => None,
        };
        Ok((decoded, normalized, normalized_decoded))
    })();
    #[cfg(test)]
    if FORCE_PROVENANCE_ERROR.with(|flag| flag.get()) {
        evidence
            .errors
            .push(format!("{family}: injected provenance error"));
    }
    match built {
        Err(error) => evidence
            .errors
            .push(format!("{family}/{}: {error}", evidence.container)),
        Ok((decoded, normalized, normalized_decoded)) => {
            let b_decoded = decoded.as_ref().unwrap_or(raw);
            let n_raw = normalized.as_ref().unwrap_or(raw);
            let n_decoded = match (&normalized, &normalized_decoded) {
                (_, Some(view)) => view,
                (Some(view), None) => view,
                (None, None) => b_decoded,
            };
            evidence.add_view("B-raw", raw, &pieces, after_heading, Some(0));
            evidence.add_view("B-decoded", b_decoded, &pieces, after_heading, Some(0));
            evidence.add_view("N-raw", n_raw, &pieces, after_heading, None);
            evidence.add_view("N-decoded", n_decoded, &pieces, after_heading, None);
        }
    }
    evidence
}

/// The evidence of one addendum block. The block is the container of the `부칙단위` or of the
/// independent `부칙내용` that opened it.
fn block_evidence(
    view: &SourceView,
    index: usize,
    text: &str,
    raw: &str,
    consumed: &[SourceRange],
) -> PartitionEvidence {
    match (view.addenda.get(index), view.addendum_elements.get(index)) {
        (Some(block), Some(element)) => container_evidence(
            "block_gate",
            format!("addendum:{element}"),
            block,
            text,
            raw,
            consumed,
        ),
        _ => {
            let mut evidence =
                PartitionEvidence::new("block_gate", format!("addendum-index:{index}"), consumed);
            evidence.parts.push(PartKeys::default());
            evidence
                .errors
                .push("the SourceView addenda differ from the addenda of the rules".to_owned());
            evidence
        }
    }
}

/// The decoded and the written view of one amendment part, cut as `amendment_parts` cuts the text.
fn part_views(
    decoded: &SourceSegments,
    raw: &SourceSegments,
    part: &AmendmentPart,
) -> Result<(SourceSegments, SourceSegments)> {
    let (from, to, mark) = part.view_range;
    let part_decoded = decoded.amendment_part(from, to, mark)?;
    let part_raw = if part.aligned {
        amendment_part_written(raw, part.line_range, mark)?
    } else {
        part_decoded.clone()
    };
    Ok((part_decoded, part_raw))
}

/// The evidence of the amendment text partition. B keeps the parts of the rules; N cuts its own
/// parts from its whole view with the same rules and the same `extracted` decision. All parts share
/// one partition, so a part boundary that moves does not make a key new.
fn amendment_evidence(
    text: &str,
    raw_text: &str,
    extracted: bool,
    raw: &SourceSegments,
    parts: &[AmendmentPart],
    consumed: &[SourceRange],
) -> PartitionEvidence {
    let mut evidence =
        PartitionEvidence::new("amendment_part_gate", "amendment_text".to_owned(), consumed);
    let pieces = piece_texts(raw);
    #[cfg(test)]
    if FORCE_PROVENANCE_ERROR.with(|flag| flag.get()) {
        evidence
            .errors
            .push("amendment_part_gate: injected provenance error".to_owned());
    }
    let entity = has_entity(&raw.text);
    let decoded_view = if entity {
        raw.decoded().map(Some)
    } else {
        Ok(None)
    };
    let decoded_view = match decoded_view {
        Ok(view) => view,
        Err(error) => {
            evidence
                .parts
                .extend(parts.iter().map(|_| PartKeys::default()));
            evidence.errors.push(format!("amendment decode: {error}"));
            return evidence;
        }
    };
    let decoded = decoded_view.as_ref().unwrap_or(raw);
    if decoded.text != text || raw.text != raw_text {
        evidence
            .errors
            .push("amendment B view bytes differ from the text of the rules".to_owned());
    }
    // The outcome entries of the B parts (raw, decoded) or the error of a part, for the N parts.
    let mut b_parts: Vec<std::result::Result<(usize, usize), String>> = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        evidence.parts.push(PartKeys::default());
        // Without an entity the decoded part and the written part are the same view.
        let views = if entity {
            part_views(decoded, raw, part)
                .map(|(part_decoded, part_raw)| (part_decoded, Some(part_raw)))
        } else {
            let (from, to, mark) = part.view_range;
            raw.amendment_part(from, to, mark).map(|view| (view, None))
        };
        match views {
            Err(error) => {
                b_parts.push(Err(error.to_string()));
                evidence.errors.push(format!("B part {index}: {error}"));
            }
            Ok((part_decoded, part_raw)) => {
                let part_raw = part_raw.as_ref().unwrap_or(&part_decoded);
                if part_decoded.text != part.text || part_raw.text != part.raw {
                    evidence.errors.push(format!(
                        "B part {index} bytes differ from the part of the rules"
                    ));
                }
                let raw_entry = evidence.add_view("B-raw", part_raw, &pieces, false, Some(index));
                let decoded_entry =
                    evidence.add_view("B-decoded", &part_decoded, &pieces, false, Some(index));
                b_parts.push(Ok((raw_entry, decoded_entry)));
            }
        }
    }
    // When N removes nothing, N is the raw view: it cuts the same parts as B from the same text, and
    // each N part shows what its B part shows.
    let changes = match normalization_changes(raw) {
        Ok(changes) => changes,
        Err(error) => {
            evidence.errors.push(format!("N amendment view: {error}"));
            return evidence;
        }
    };
    if !changes {
        for (index, entry) in b_parts.iter().enumerate() {
            match entry {
                Err(error) => evidence.errors.push(format!("N part {index}: {error}")),
                Ok((raw_entry, decoded_entry)) => {
                    evidence.apply_cached("N-raw", *raw_entry);
                    evidence.apply_cached("N-decoded", *decoded_entry);
                }
            }
        }
        return evidence;
    }
    let normalized = normalized_raw(raw).and_then(|normalized| {
        let normalized_decoded = normalized.decoded()?;
        Ok((normalized, normalized_decoded))
    });
    match normalized {
        Err(error) => evidence.errors.push(format!("N amendment view: {error}")),
        Ok((normalized, normalized_decoded)) => {
            for (index, part) in
                amendment_parts(&normalized_decoded.text, &normalized.text, extracted)
                    .iter()
                    .enumerate()
            {
                match part_views(&normalized_decoded, &normalized, part) {
                    Err(error) => evidence.errors.push(format!("N part {index}: {error}")),
                    Ok((part_decoded, part_raw)) => {
                        evidence.add_view("N-raw", &part_raw, &pieces, false, None);
                        evidence.add_view("N-decoded", &part_decoded, &pieces, false, None);
                    }
                }
            }
        }
    }
    evidence
}

/// The sentence the date reader reads: "이 조례는 ... 시행한다" and its kin.
static CLAUSE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"(?:이|본)\s*(?:{SUBJECT_KINDS})(?:은|는)\s*(?:\(시행일\)\s*)?(.+?)(시행\s*한다|효력을\s*발생한다)")).expect("valid clause pattern")
});

/// Reads every effective-date sentence of a text, in order. `after_heading` tells that the text
/// starts right after an addendum heading.
fn read_clauses(text: &str, after_heading: bool) -> Vec<Clause> {
    #[cfg(test)]
    let _entry = trace_entry("read_clauses", after_heading, &[("decoded", text)]);
    read_clauses_at(text, after_heading)
        .into_iter()
        .map(|(_, clause)| clause)
        .collect()
}

/// `read_clauses` with the byte offset in `text` where each sentence starts: the start of the
/// CLAUSE match for a read sentence and the detected start for an unread one. The order is the
/// order of `read_clauses`. It leaves no test trace; the LLM selection (compiler#2) reads the
/// offsets to tell the quoted sentence itself from the other sentences of its section.
fn read_clauses_at(text: &str, after_heading: bool) -> Vec<(usize, Clause)> {
    static EXPLICIT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^(\d{4})년\s*(\d{1,2})월\s*(\d{1,2})일(?:(부터|에|로부터))?\s*$")
            .expect("valid date pattern")
    });
    // A date written with dots, such as "1994. 7. 1.부터", reads the same as the Korean form.
    static DOTTED: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^(\d{4})\s*\.\s*(\d{1,2})\s*\.\s*(\d{1,2})\s*\.?\s*(?:(부터|에|로부터))?\s*$")
            .expect("valid dotted date pattern")
    });
    let mut clauses = Vec::new();
    let mut read_starts = Vec::new();
    for captures in CLAUSE.captures_iter(text) {
        let start = captures.get(0).map_or(0, |found| found.start());
        read_starts.push(start);
        let end = captures.get(0).map_or(text.len(), |found| found.end());
        let value = captures[1].trim();
        let compact = value.replace(' ', "");
        let (kind, without_buteo, explicit_without_buteo) =
            if let Some(date) = EXPLICIT.captures(value).or_else(|| DOTTED.captures(value)) {
                let made = date[1]
                    .parse()
                    .ok()
                    .and_then(|year| make_date(year, date[2].parse().ok()?, date[3].parse().ok()?));
                (
                    made.map_or(ClauseKind::Other, ClauseKind::Explicit),
                    false,
                    date.get(4).is_none(),
                )
            } else if matches!(
                compact.as_str(),
                "공포한날부터" | "공포한날로부터" | "발령한날부터" | "고시한날부터"
            ) {
                (ClauseKind::Plain, false, false)
            } else if compact == "공포한날" {
                (ClauseKind::Plain, true, false)
            } else {
                (ClauseKind::Other, false, false)
            };
        clauses.push((
            start,
            Clause {
                kind,
                proviso: starts_proviso(&text[end..]),
                spaced: captures[2].contains(char::is_whitespace),
                without_buteo,
                explicit_without_buteo,
                omitted_mark: starts_with_omitted_mark(&text[end..]),
                unread: false,
            },
        ));
    }
    // A sentence the detection found but this reader did not read (a sentence that breaks across
    // lines, the verb "시행함", a subject this reader does not know) is a sentence no approved
    // rule reads, so it stays as an unread clause and no date is taken from its section.
    for start in detected_sentence_starts(text, after_heading) {
        if !read_starts.contains(&start) {
            clauses.push((
                start,
                Clause {
                    kind: ClauseKind::Other,
                    proviso: false,
                    spaced: false,
                    without_buteo: false,
                    explicit_without_buteo: false,
                    omitted_mark: false,
                    unread: true,
                },
            ));
        }
    }
    clauses
}

/// Resolves one approved clause without inferring unknown legal conditions.
fn clause_date(text: &str, publication: Option<Date>) -> Option<Date> {
    let mut dates = Vec::new();
    for clause in read_clauses(text, false) {
        // A proviso right after the sentence changes its meaning, so no date is read.
        if clause.proviso {
            return None;
        }
        dates.push(match clause.kind {
            ClauseKind::Explicit(date) => date,
            ClauseKind::Plain => publication?,
            ClauseKind::Other => return None,
        });
    }
    let first = *dates.first()?;
    dates.iter().all(|date| *date == first).then_some(first)
}

/// Encodes text as lowercase hex of its UTF-8 bytes.
fn hex_text(text: &str) -> String {
    text.as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Names an identity in tree paths: its hex, or `sha256-` and the hash hex when too long.
///
/// A name stays within 200 bytes including the ".json" suffix of an identity file.
fn identity_name(identity: &str) -> String {
    let name = hex_text(identity);
    if name.len() + ".json".len() > 200 {
        format!("sha256-{}", sha256(identity.as_bytes()))
    } else {
        name
    }
}

/// Builds `{dataset}/{aa}/{bb}/{name}` where aa and bb are the first hash hex pairs.
fn identity_directory(dataset: &str, identity: &str) -> String {
    let hash = sha256(identity.as_bytes());
    format!(
        "{dataset}/{}/{}/{}",
        &hash[..2],
        &hash[2..4],
        identity_name(identity)
    )
}

/// Path of the per-identity date index file in an enforcement tree.
fn identity_index_path(dataset: &str, identity: &str) -> String {
    format!(
        ".legalize/date-index/{}.json",
        identity_directory(dataset, identity)
    )
}

/// What the final gate does with a date that an approval selected (r25c-design.md 8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GatePolicy {
    /// The two individual ordinance approvals before r25c: the gate does not check the date.
    Exempt,
    /// `user-amendment-explicit-dates-20261113`: the approved sentence is consumed and the gate
    /// checks every other signal.
    ConsumeApprovedSentence,
    /// `user-1063983-explicit-clause-date-20261113`: the entry is checked before a valid source
    /// date. Only the conflict between the source date and the sentence date that the user
    /// resolved is excepted; the gate checks every other signal.
    ConditionalException,
}

impl GatePolicy {
    fn name(self) -> &'static str {
        match self {
            Self::Exempt => "exempt",
            Self::ConsumeApprovedSentence => "consume-approved-sentence",
            Self::ConditionalException => "conditional-exception",
        }
    }
}

/// An effective date approved for one named revision. Other revisions never use it.
struct Approval {
    dataset: &'static str,
    /// The source identity. The two approvals before r25c keep their old condition (`None`).
    identity: Option<&'static str>,
    revision: &'static str,
    promulgation: &'static str,
    /// The raw source effective date of the registration. The two approvals before r25c keep their
    /// old condition (`None`).
    enforcement: Option<&'static str>,
    sentence: &'static str,
    /// The source fields that must each hold the sentence exactly. The two approvals before r25c
    /// look for it in the addenda that the rules read (empty list).
    fields: &'static [&'static str],
    date: &'static str,
    source: &'static str,
    /// The words of the user decision.
    user_choice: &'static str,
    gate_policy: GatePolicy,
    /// What the rule chain gives for the revision without any approval, before the gate: the
    /// state of the revision when the user approved the date (r25c-prototype-delta-20261116.md 8,
    /// B3). The approval applies only to a revision whose chain result is exactly this. The two
    /// approvals before r25c keep their old condition (`None`).
    chain: Option<ChainResult>,
}

/// The result of the rule chain before the gate: the status, the ISO date and the date path
/// (r25c-design.md 4: P0 to P10) of the occurrence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ChainResult {
    status: &'static str,
    date: &'static str,
    path: &'static str,
}

/// The date path of an occurrence that an approval with a gate policy dated.
const APPROVAL_PATH: &str = "P10";

/// The individually approved revisions: one shared registry and one shared selection point
/// (`approved_date` for the exempt entries, `gated_approval` for the entries with a gate policy).
/// An entry applies only when every condition of its registration matches the input.
const APPROVALS: &[Approval] = &[
    Approval {
        dataset: "ordinances",
        identity: None,
        revision: "1000238",
        promulgation: "20071031",
        enforcement: None,
        sentence: "이 조례는 공포 후 1개월이 경과한 날부터 시행한다",
        fields: &[],
        date: "20071201",
        source: "issue comment 6437632923",
        user_choice: "",
        gate_policy: GatePolicy::Exempt,
        chain: None,
    },
    Approval {
        dataset: "ordinances",
        identity: None,
        revision: "1011016",
        promulgation: "20110218",
        enforcement: None,
        sentence: "이 규칙은 2011년 1월 1일부터 시행한다",
        fields: &[],
        date: "20110101",
        source: "issue body: an explicit effective date earlier than promulgation is kept as written",
        user_choice: "",
        gate_policy: GatePolicy::Exempt,
        chain: None,
    },
    Approval {
        dataset: "admrules",
        identity: Some("2111925"),
        revision: "2200000088451",
        promulgation: "20090410",
        enforcement: Some("99991231"),
        sentence: "이 예규는 2009. 6. 27.부터 시행한다.",
        fields: &["개정문내용"],
        date: "20090627",
        source: AMENDMENT_EXPLICIT_DATES_DECISION,
        user_choice: "개정문의 명시 날짜로 확정",
        gate_policy: GatePolicy::ConsumeApprovedSentence,
        chain: Some(ChainResult {
            status: "estimated",
            date: "2009-04-10",
            path: "P8",
        }),
    },
    Approval {
        dataset: "admrules",
        identity: Some("2113631"),
        revision: "2200000104677",
        promulgation: "19981002",
        enforcement: Some("99991231"),
        sentence: "이 예규는 1998.10. 7.부터 시행한다.",
        fields: &["개정문내용"],
        date: "19981007",
        source: AMENDMENT_EXPLICIT_DATES_DECISION,
        user_choice: "개정문의 명시 날짜로 확정",
        gate_policy: GatePolicy::ConsumeApprovedSentence,
        chain: Some(ChainResult {
            status: "estimated",
            date: "1998-10-02",
            path: "P8",
        }),
    },
    Approval {
        dataset: "ordinances",
        identity: Some("2153239"),
        revision: "1063983",
        promulgation: "20070530",
        enforcement: Some("20070530"),
        sentence: "이 규칙은 2007년 6월 1일부터 시행한다.",
        fields: &["부칙내용", "제개정이유내용"],
        date: "20070601",
        source: CLAUSE_DATE_1063983_DECISION,
        user_choice: "이 판본은 문장 날짜",
        gate_policy: GatePolicy::ConditionalException,
        chain: Some(ChainResult {
            status: "confirmed",
            date: "2007-05-30",
            path: "P0",
        }),
    },
];

/// User decision: the two revisions take the explicit date of their amendment statement.
pub const AMENDMENT_EXPLICIT_DATES_DECISION: &str = "user-amendment-explicit-dates-20261113";
/// User decision: MST 1063983 takes the date of its addendum sentence.
pub const CLAUSE_DATE_1063983_DECISION: &str = "user-1063983-explicit-clause-date-20261113";

/// Removes all whitespace so quoted sentences compare by their letters.
fn compact_text(text: &str) -> String {
    text.chars().filter(|ch| !ch.is_whitespace()).collect()
}

/// Decodes entity markup that CDATA text can carry literally. `resolve` decodes the body, the
/// addenda and the amendment text once, at the start; no other function decodes them, so the
/// section split, the detection and the extraction of an amendment text read the same text.
fn decode_markup(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// Finds the exempt individual approval whose revision, promulgation date and sentence match.
fn approved_date(
    source: &SourceRevision,
    addenda: &[Addendum],
) -> Option<(&'static Approval, Date)> {
    APPROVALS
        .iter()
        .filter(|approval| approval.gate_policy == GatePolicy::Exempt)
        .find_map(|approval| {
            let sentence = compact_text(approval.sentence);
            let quoted = addenda
                .iter()
                .any(|block| compact_text(&block.text).contains(&sentence));
            (source.revision == approval.revision
                && normalize_compact(source.promulgation.trim()).as_deref()
                    == normalize_compact(approval.promulgation).as_deref()
                && quoted)
                .then(|| compact_date(approval.date))
                .flatten()
                .map(|date| (approval, date))
        })
}

/// The date that an approval with a gate policy selected and the evidence it consumed.
struct ApprovalSelection {
    approval: &'static Approval,
    date: Date,
    /// The source ranges of every occurrence of the approved sentence in the required fields. The
    /// gate leaves out the keys inside them (the consumed evidence C).
    spans: Vec<SourceRange>,
    /// The field, the stable container and the source ranges of each occurrence.
    found: Vec<Value>,
    /// What in the section of the approved sentence keeps the date from being confirmed: a
    /// proviso after the sentence, or another read sentence with another date.
    blocking: Vec<String>,
}

impl ApprovalSelection {
    fn reason(&self) -> String {
        match self.approval.gate_policy {
            GatePolicy::ConditionalException => format!(
                "individually approved addendum date before the source effective date ({})",
                self.approval.source
            ),
            _ => format!(
                "individually approved explicit date of the amendment statement ({})",
                self.approval.source
            ),
        }
    }

    /// The evidence of the selection. The keys start with `approved_date_` and do not overlap the
    /// keys of the exempt approvals (`approval_source`, `approved_sentence`).
    fn record(&self, source: &SourceRevision, extra: &mut serde_json::Map<String, Value>) {
        extra.insert(
            "approved_date_decision".to_owned(),
            json!(self.approval.source),
        );
        extra.insert(
            "approved_date_sentence".to_owned(),
            json!(self.approval.sentence),
        );
        extra.insert(
            "approved_date_policy".to_owned(),
            json!(self.approval.gate_policy.name()),
        );
        extra.insert("approved_date_sources".to_owned(), json!(self.found));
        extra.insert(
            "approved_date_user_choice".to_owned(),
            json!(self.approval.user_choice),
        );
        if self.approval.gate_policy == GatePolicy::ConditionalException {
            extra.insert(
                "approved_date_conflict".to_owned(),
                json!({"source_enforcement_date": source.enforcement, "selected_enforcement_date": iso(self.date),
                    "resolved_by": "user", "decision": self.approval.source}),
            );
        }
        if !self.blocking.is_empty() {
            extra.insert("approved_date_blocked".to_owned(), json!(self.blocking));
        }
    }
}

/// The selection range of an approved sentence at byte `at` (r25c-prototype-delta-20261116.md 8,
/// B2). When a heading line ("부칙" at the start of a line, the line of `at` included) stands at or
/// before the sentence, the range runs from that heading line to the next heading line or "□"
/// line: the addendum section of the sentence. Without such a heading the range is the "□"
/// section of the sentence: from the nearest "□" line at or before the sentence (the field start
/// when there is none) to the next "□" line (the field end when there is none). A heading after
/// the sentence does not cut the "□" section, so the range does not depend on the order of the
/// sentences around the approved one.
fn sentence_section(text: &str, at: usize) -> (usize, usize) {
    let mut starts = Vec::new();
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        starts.push((offset, line));
        offset += line.len();
    }
    let index = starts
        .iter()
        .rposition(|(start, _)| *start <= at)
        .unwrap_or(0);
    let boxed = |line: &str| line.trim_start().starts_with('□');
    let heading = starts[..=index]
        .iter()
        .rev()
        .find(|(_, line)| heading_rest(line).is_some());
    let (from, to) = match heading {
        Some((start, _)) => (
            *start,
            starts[index + 1..]
                .iter()
                .find(|(_, line)| heading_rest(line).is_some() || boxed(line)),
        ),
        None => (
            starts[..=index]
                .iter()
                .rev()
                .find(|(_, line)| boxed(line))
                .map_or(0, |(start, _)| *start),
            starts[index + 1..].iter().find(|(_, line)| boxed(line)),
        ),
    };
    (from, to.map_or(text.len(), |(start, _)| *start))
}

/// Selects the approval with a gate policy whose every condition matches (r25c-design.md 8): the
/// dataset, the identity, the MST, the raw promulgation date, the raw effective date and the exact
/// approved sentence in each required field of the source. The sentence is found in the decoded
/// text that the rules read, and its source ranges come from the SourceView. A read sentence with
/// another date in the same section, or a proviso right after the sentence, keeps the date from
/// being confirmed; the selection then blocks the date instead of falling back to the source date.
fn gated_approval(
    dataset: &str,
    source: &SourceRevision,
    view: &SourceView,
    addenda: &[Addendum],
    amendment: &str,
    declaration: &str,
) -> Option<ApprovalSelection> {
    let approval = approval_entry(dataset, source)?;
    let date = compact_date(approval.date)?;
    let mut fields: Vec<(&'static str, String, &str, &SourceSegments)> = Vec::new();
    for &field in approval.fields {
        match field {
            "개정문내용" => fields.push((
                field,
                "declaration".to_owned(),
                declaration,
                &view.declaration,
            )),
            "제개정이유내용" => fields.push((
                field,
                "amendment_text".to_owned(),
                amendment,
                &view.amendment,
            )),
            "부칙내용" => {
                for (index, block) in addenda.iter().enumerate() {
                    if block.text.contains(approval.sentence) {
                        let element = view.addendum_elements.get(index)?;
                        fields.push((
                            field,
                            format!("addendum:{element}"),
                            block.text.as_str(),
                            view.addenda.get(index)?,
                        ));
                    }
                }
            }
            _ => return None,
        }
    }
    let mut spans = Vec::new();
    let mut found = Vec::new();
    let mut blocking = Vec::new();
    for &field in approval.fields {
        if !fields
            .iter()
            .any(|(name, _, text, _)| *name == field && text.contains(approval.sentence))
        {
            return None;
        }
    }
    for (field, container, text, raw) in fields {
        let decoded = raw.decoded().ok()?;
        if decoded.text != text {
            return None;
        }
        let units = unit_index(&decoded).ok()?;
        let pieces = piece_texts(raw);
        for (at, sentence) in text.match_indices(approval.sentence) {
            let ranges =
                canonical_ranges(&map_bounded(&units, at, at + sentence.len()).ok()?, &pieces);
            found.push(json!({"field": field, "container": container, "source_ranges": ranges.iter().map(|(e, p, s, t)| json!([e, p, s, t])).collect::<Vec<_>>()}));
            spans.extend(ranges);
            let (from, to) = sentence_section(text, at);
            #[cfg(test)]
            let clauses = untraced(|| read_clauses(&text[from..to], false));
            #[cfg(not(test))]
            let clauses = read_clauses(&text[from..to], false);
            for clause in clauses.iter().filter(|clause| !clause.unread) {
                if clause.kind != ClauseKind::Explicit(date) {
                    blocking.push(format!(
                        "{field}: another read sentence in the section of the approved sentence"
                    ));
                }
                if clause.proviso {
                    blocking.push(format!("{field}: a proviso follows a read sentence in the section of the approved sentence"));
                }
            }
            if starts_proviso(&text[at + sentence.len()..]) {
                blocking.push(format!("{field}: a proviso follows the approved sentence"));
            }
        }
    }
    Some(ApprovalSelection {
        approval,
        date,
        spans,
        found,
        blocking,
    })
}

/// The approval with a gate policy whose metadata conditions match the source (the dataset, the
/// identity, the MST, the raw promulgation date and the raw effective date).
fn approval_entry(dataset: &str, source: &SourceRevision) -> Option<&'static Approval> {
    APPROVALS.iter().find(|approval| {
        approval.gate_policy != GatePolicy::Exempt
            && approval.dataset == dataset
            && approval.identity == Some(source.identity.as_str())
            && approval.revision == source.revision
            && normalize_compact(source.promulgation.trim()).as_deref()
                == normalize_compact(approval.promulgation).as_deref()
            && approval.enforcement == Some(source.enforcement.as_str())
    })
}

/// The date path of an occurrence that a usable LLM judgment dated (compiler#2).
const LLM_PATH: &str = "P11";
/// The approval mark of an LLM selection in the gate (`GateMark::approval`). No registry entry
/// uses it: their marks are revision numbers.
const LLM_APPROVAL: &str = "llm";
/// The reason prefix of an occurrence dated by an LLM judgment.
const LLM_REASON: &str = "LLM judgment of the effective-date sentence (compiler#2)";
/// The gate reason that a recorded parser result can carry when the gate, not the rule chain,
/// left the revision unresolved.
const GATE_REASON: &str =
    "effective date is unresolved; the effective-date sentence names no approved form";

/// The date that a usable LLM judgment selected (lead-c2-integration-contract-20261117) and the
/// evidence it consumed. It plays the part of `ApprovalSelection` for the second approval source.
struct LlmSelection {
    date: Date,
    /// The source ranges of every occurrence of the quote in its field (the consumed evidence C).
    spans: Vec<SourceRange>,
    /// What keeps the date from being confirmed (B2, user-c2-b2-llm-20261117,
    /// user-c2-proviso-357-20261117, user-c2-quote-proviso-hold-20261117).
    blocking: Vec<String>,
    /// The `llm_judgment` evidence.
    evidence: Value,
    /// The reason of the occurrence.
    reason: String,
}

/// Removes white space and keeps, for each remaining character, its byte offset and length in
/// `text`, so a quote whose white space the judge folded still finds its place in the source.
fn compact_positions(text: &str) -> (String, Vec<(usize, usize)>) {
    let mut compact = String::new();
    let mut places = Vec::new();
    for (at, ch) in text.char_indices() {
        if !ch.is_whitespace() {
            compact.push(ch);
            places.push((at, ch.len_utf8()));
        }
    }
    (compact, places)
}

/// Every place of the quote in `text`, compared without white space: (start byte, end byte).
fn quote_places(text: &str, quote: &str) -> Vec<(usize, usize)> {
    let wanted: String = quote.chars().filter(|ch| !ch.is_whitespace()).collect();
    if wanted.is_empty() {
        return Vec::new();
    }
    let (compact, places) = compact_positions(text);
    let width = wanted.chars().count();
    let starts: Vec<usize> = compact.char_indices().map(|(at, _)| at).collect();
    compact
        .match_indices(&wanted)
        .filter_map(|(byte, _)| {
            let first = starts.binary_search(&byte).ok()?;
            let (start, _) = places[first];
            let (last, length) = places[first + width - 1];
            Some((start, last + length))
        })
        .collect()
}

/// Tells whether a read sentence of the section gives the date of the judgment, by its outcome
/// (user-c2-b2-llm-20261117 나).
fn same_llm_date(kind: &ClauseKind, outcome: &str, date: Date, publication: Option<Date>) -> bool {
    match (outcome, kind) {
        ("explicit_date", ClauseKind::Explicit(written)) => *written == date,
        ("promulgation_day", ClauseKind::Plain) => true,
        ("promulgation_day", ClauseKind::Explicit(written)) => Some(*written) == publication,
        ("label_date", ClauseKind::Plain) => true,
        _ => false,
    }
}

/// Gives the identity lines of a revision commit in the enforcement mode, with a blank line after
/// them. The keys and their order are the ones of the default-mode commit of each dataset, so a
/// consumer that finds a revision by these lines reads both modes the same way
/// (user-enforcement-mst-trailer-20261117). An unknown dataset gets no lines.
fn identity_lines(dataset: &str, identity: &str, revision: &str) -> String {
    match dataset {
        "ordinances" => format!("자치법규ID: {identity}\n자치법규일련번호: {revision}\n\n"),
        "admrules" => format!("행정규칙일련번호: {revision}\n행정규칙ID: {identity}\n\n"),
        "laws" => format!("법령MST: {revision}\n\n"),
        _ => String::new(),
    }
}

/// What selects no LLM date: the evidence `llm_not_applied`.
fn llm_not_applied(reason: &str, detail: Value) -> Value {
    let mut value = json!({"reason": reason, "decision": "lead-c2-integration-contract-20261117"});
    if let (Some(object), Some(more)) = (value.as_object_mut(), detail.as_object()) {
        object.extend(more.clone());
    }
    value
}

/// Selects the date of a usable LLM judgment for one source (design-v3-integration.md 3.2, v3.1,
/// v3.3, v3.4). `None` when the ledger holds no usable judgment of this exact copy; `Err` holds
/// the evidence `llm_not_applied` when a judgment exists and a condition fails.
#[allow(clippy::too_many_arguments)]
fn llm_approval(
    dataset: &str,
    source: &SourceRevision,
    view: &SourceView,
    addenda: &[Addendum],
    texts: [(&str, &str); 4],
    publication: Option<Date>,
    ledger: &crate::llm_ledger::LlmLedger,
) -> Result<Option<std::result::Result<LlmSelection, Value>>> {
    let Some(entry) = ledger.find(&source.identity, &source.revision, &sha256(&source.xml)) else {
        return Ok(None);
    };
    if compact_date(&source.enforcement).is_some() {
        return Ok(Some(Err(llm_not_applied("valid-source-date", json!({})))));
    }
    // B3: the rule chain without any approval must give what the parser gave at judgment time.
    let chain = rule_chain(dataset, source)?;
    let recorded = &entry.parser;
    let same = chain.status == recorded.status
        && chain.enforcement_date == recorded.date
        && chain.gate.path.is_none()
        && chain.reason == recorded.rule;
    if !same {
        let after_gate = recorded.rule == GATE_REASON
            || recorded.rule.starts_with(&format!(
                "effective date is unresolved; {LABEL_AFTER_NEXT_DECISION}:"
            ));
        return Ok(Some(Err(llm_not_applied(
            if after_gate {
                "recorded-after-gate"
            } else {
                "chain-changed"
            },
            json!({"observed": {"status": chain.status, "date": chain.enforcement_date, "path": chain.gate.path, "rule": chain.reason},
                "expected": {"status": recorded.status, "date": recorded.date, "path": null, "rule": recorded.rule}}),
        ))));
    }
    let Some(date) = compact_date(&entry.date.replace('-', "")) else {
        return Ok(Some(Err(llm_not_applied(
            "invalid-date",
            json!({"date": entry.date}),
        ))));
    };
    let [(_, body), (_, outside), (_, amendment), (_, declaration)] = texts;
    let mut fields: Vec<(String, &str, &SourceSegments)> = Vec::new();
    match entry.field.as_str() {
        "부칙" => {
            for (index, block) in addenda.iter().enumerate() {
                let (Some(element), Some(raw)) =
                    (view.addendum_elements.get(index), view.addenda.get(index))
                else {
                    continue;
                };
                fields.push((format!("addendum:{element}"), block.text.as_str(), raw));
            }
        }
        "조문" => {
            fields.push(("body".to_owned(), body, &view.body));
            fields.push(("outside".to_owned(), outside, &view.outside));
        }
        "제개정이유" => fields.push(("amendment_text".to_owned(), amendment, &view.amendment)),
        "개정문" => fields.push(("declaration".to_owned(), declaration, &view.declaration)),
        other => {
            return Ok(Some(Err(llm_not_applied(
                "unknown-field",
                json!({"field": other}),
            ))));
        }
    }
    let mut spans = Vec::new();
    let mut found = Vec::new();
    let mut blocking = Vec::new();
    let mut label_dates = BTreeSet::new();
    // The read clauses of the quote itself, for the record: the offset of the clause start from
    // the quote start, its kind and whether it gives the date of the judgment. A clause with an
    // offset above 0 is one that a definition by the quote start would not have left out.
    let mut quote_clauses = Vec::new();
    for (container, text, raw) in fields {
        let places = quote_places(text, &entry.quote);
        if places.is_empty() {
            continue;
        }
        let Ok(decoded) = raw.decoded() else { continue };
        if decoded.text != text {
            continue;
        }
        let units = unit_index(&decoded)?;
        let pieces = piece_texts(raw);
        for (at, end) in places {
            let ranges = canonical_ranges(&map_bounded(&units, at, end)?, &pieces);
            found.push(json!({"field": entry.field, "container": container, "source_ranges": ranges.iter().map(|(e, p, s, t)| json!([e, p, s, t])).collect::<Vec<_>>()}));
            spans.extend(ranges);
            let (from, to) = sentence_section(text, at);
            if let Some(rest) = text[from..].lines().next().and_then(heading_rest) {
                let (label, _, _) = heading_label(rest, publication);
                for item in read_label_dates(&label, publication) {
                    if let Some(day) = item.date {
                        label_dates.insert(iso(day));
                    }
                }
            }
            #[cfg(test)]
            let clauses = untraced(|| read_clauses_at(&text[from..to], false));
            #[cfg(not(test))]
            let clauses = read_clauses_at(&text[from..to], false);
            // B2 reads the read sentences only; the gate takes the unread ones outside C (v3.4 1).
            for (start, clause) in clauses.iter().filter(|(_, clause)| !clause.unread) {
                let own = (at..end).contains(&(from + start));
                if own {
                    quote_clauses.push(json!({"offset": from + start - at,
                        "kind": match clause.kind { ClauseKind::Plain => "plain", ClauseKind::Explicit(_) => "explicit", ClauseKind::Other => "other" },
                        "same_date": same_llm_date(&clause.kind, &entry.outcome, date, publication)}));
                }
                if clause.proviso {
                    blocking.push(
                        if own {
                            "quote-proviso"
                        } else {
                            "section-proviso"
                        }
                        .to_owned(),
                    );
                }
                if own {
                    continue;
                }
                if clause.kind == ClauseKind::Other {
                    blocking.push("section-other-unreadable".to_owned());
                } else if !same_llm_date(&clause.kind, &entry.outcome, date, publication) {
                    blocking.push("section-other-date".to_owned());
                }
            }
            if starts_proviso(&text[end..]) {
                blocking.push("proviso-after-quote".to_owned());
            }
        }
    }
    if spans.is_empty() {
        return Ok(Some(Err(llm_not_applied(
            "quote-not-found",
            json!({"field": entry.field}),
        ))));
    }
    let proviso: String = entry
        .proviso_quote
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect();
    if entry.proviso
        && ["다만", "단,", "단，"]
            .iter()
            .any(|word| proviso.starts_with(word))
    {
        blocking.push("llm-proviso".to_owned());
    }
    blocking.sort();
    blocking.dedup();
    let evidence = json!({
        "decision": "lead-c2-integration-contract-20261117",
        "ledger_sha256": ledger.sha256,
        "model": ledger.policy.get("model"),
        "effort": ledger.policy.get("effort"),
        "prompt_version": ledger.policy.get("prompt_version"),
        "runner_version": ledger.policy.get("runner_version"),
        "judged_at": entry.judged_at,
        "outcome": entry.outcome,
        "date": entry.date,
        "field": entry.field,
        "quote": entry.quote,
        "section_label": entry.section_label,
        "section_label_dates": label_dates,
        "proviso": entry.proviso,
        "proviso_quote": entry.proviso_quote,
        "next_promulgation": entry.next_promulgation,
        "sources": found,
        "blocking": blocking,
        "quote_clauses": quote_clauses,
    });
    Ok(Some(Ok(LlmSelection {
        date,
        spans,
        blocking,
        evidence,
        reason: format!("{LLM_REASON}: {}: {}", entry.field, entry.quote),
    })))
}

/// Tells whether the individual approval registry names this revision, exempt entries included.
/// The registry decides first; an LLM judgment never applies to such a revision.
fn registered_revision(dataset: &str, revision: &str) -> bool {
    APPROVALS
        .iter()
        .any(|approval| approval.dataset == dataset && approval.revision == revision)
}

/// Tells whether every range lies inside one of the consumed spans.
fn ranges_within(spans: &[SourceRange], ranges: &[SourceRange]) -> bool {
    !spans.is_empty()
        && ranges.iter().all(|r| {
            spans
                .iter()
                .any(|s| s.0 == r.0 && s.1 == r.1 && s.2 <= r.2 && r.3 <= s.3)
        })
}

/// Tells whether a key belongs to the consumed approved sentence: its subject or candidate start
/// and its verb or phrase lie inside the consumed spans.
fn key_within(spans: &[SourceRange], key: &EvidenceKey) -> bool {
    match key {
        EvidenceKey::Subject { subject, verb } | EvidenceKey::Extended { subject, verb } => {
            ranges_within(spans, subject) && ranges_within(spans, verb)
        }
        EvidenceKey::Heading { candidate, verb } => {
            ranges_within(
                spans,
                &[(candidate.0, candidate.1, candidate.2, candidate.2)],
            ) && ranges_within(spans, verb)
        }
        EvidenceKey::Phrase { phrase } => ranges_within(spans, phrase),
    }
}

/// One addendum section that starts at its own "부칙" heading.
struct Section {
    label: String,
    body: String,
    /// The body starts right after an addendum heading, so its first sentence is the first
    /// sentence of the section (the detection reads it even when it has no subject).
    after_heading: bool,
    /// The heading text of a label that looks like a label but cannot be read as one.
    unreadable: Option<String>,
    #[cfg(test)]
    source_view: Option<SourceSegments>,
}

/// Splits accumulated addendum text at each "부칙" heading and reads its leading label groups.
fn split_sections(text: &str, publication: Option<Date>) -> Vec<Section> {
    let mut sections: Vec<Section> = Vec::new();
    for line in text.lines() {
        let Some(rest) = heading_rest(line) else {
            if let Some(section) = sections.last_mut() {
                section.body.push('\n');
                section.body.push_str(line);
            }
            continue;
        };
        let (label, mut unreadable, rest) = heading_label(rest, publication);
        // One heading that names different dates in separate groups has no readable label
        // (user decision B); each group alone may still be readable.
        if unreadable.is_none() {
            let mut dates: Vec<Date> = read_label_dates(&label, publication)
                .iter()
                .filter_map(|item| item.date)
                .collect();
            dates.sort();
            dates.dedup();
            if dates.len() > 1 {
                unreadable = Some(label.trim().to_owned());
            }
        }
        sections.push(Section {
            label,
            unreadable,
            body: rest.to_owned(),
            after_heading: true,
            #[cfg(test)]
            source_view: None,
        });
    }
    sections
}

/// An addendum heading: what follows "부칙" on its line and whether the heading is clean.
struct HeadingLine<'a> {
    /// The rest of the line that holds "칙", line feed included.
    rest: &'a str,
    /// Everything that follows "칙", to the end of the text.
    tail: &'a str,
    /// White space, "[", a label bracket or the end of the line follows "부칙". Only a clean
    /// heading starts a section; the detection reads the first sentence after any heading.
    clean: bool,
}

/// The one test of an addendum heading. The text starts at a line start. White space of every
/// kind (`char::is_whitespace`) may stand before "부" and between "부" and "칙"; a line feed
/// stands there only when the text has more than one line. The section split and the extraction
/// of the addendum of an amendment text pass one line, so their headings never span lines. The
/// detection passes the text from a line start to the end of the text, as the pattern of r22 did.
/// All callers read the same decoded text. A "부칙" inside a sentence is a quotation, no heading.
fn heading_line(text: &str) -> Option<HeadingLine<'_>> {
    let after = text
        .trim_start()
        .strip_prefix('부')?
        .trim_start()
        .strip_prefix('칙')?;
    let line_end = after.find('\n').map_or(after.len(), |at| at + 1);
    let rest = &after[..line_end];
    let clean = rest
        .chars()
        .next()
        .is_none_or(|ch| ch.is_whitespace() || ch == '[' || label_brackets(ch).is_some());
    Some(HeadingLine {
        rest,
        tail: after,
        clean,
    })
}

/// What follows "부칙" on a clean heading line, and nothing for any other line.
fn heading_rest(line: &str) -> Option<&str> {
    heading_line(line)
        .filter(|heading| heading.clean)
        .map(|heading| heading.rest)
}

/// Reads the label groups that follow "부칙" on a heading line. Returns the label, the text of a
/// group that cannot be read as a date and a number, and what follows the label.
fn heading_label(rest: &str, publication: Option<Date>) -> (String, Option<String>, &str) {
    let mut rest = rest.trim_start();
    if let Some(after) = rest.strip_prefix("개정")
        && after
            .trim_start()
            .chars()
            .next()
            .is_some_and(|ch| label_brackets(ch).is_some())
    {
        rest = after.trim_start();
    }
    let mut label = String::new();
    let mut unreadable = None;
    while let Some((opens, closes)) = rest.chars().next().and_then(label_brackets) {
        let mut depth = 0_i32;
        let mut end = None;
        for (index, ch) in rest.char_indices() {
            if opens.contains(&ch) {
                depth += 1;
            } else if closes.contains(&ch) {
                depth -= 1;
                if depth == 0 {
                    end = Some(index + ch.len_utf8());
                    break;
                }
            }
        }
        // Brackets of different kinds, such as "<...)", still make a label when
        // what they hold is exactly a date and a number.
        let end = end.or_else(|| mismatched_label_end(rest));
        let Some(end) = end else {
            unreadable = Some(rest.chars().take(60).collect());
            break;
        };
        if !label_group_readable(&rest[..end], publication) {
            unreadable = Some(rest[..end].to_owned());
        }
        label.push_str(&rest[..end]);
        label.push(' ');
        rest = rest[end..].trim_start();
    }
    (label, unreadable, rest)
}

/// Returns the open and close characters of the bracket kind a label starts with.
///
/// A label is written in angle brackets or in parentheses. Each kind also has its full-width
/// forms (＜＞, （）) and, for angle brackets, the CJK forms U+3008/U+3009 and U+2329/U+232A.
/// Open and close characters of one kind match each other in any width.
fn label_brackets(open: char) -> Option<(&'static [char], &'static [char])> {
    const ANGLE: (&[char], &[char]) = (
        &['<', '\u{FF1C}', '\u{3008}', '\u{2329}'],
        &['>', '\u{FF1E}', '\u{3009}', '\u{232A}'],
    );
    const PAREN: (&[char], &[char]) = (&['(', '\u{FF08}'], &[')', '\u{FF09}']);
    if ANGLE.0.contains(&open) {
        Some(ANGLE)
    } else if PAREN.0.contains(&open) {
        Some(PAREN)
    } else {
        None
    }
}

/// Finds the end of a label group whose brackets are of different kinds, such as "<...)".
///
/// The group counts only when what it holds is exactly one optional "개정", one date and one
/// number in any order, so a stray bracket in running text is never read as a label.
fn mismatched_label_end(rest: &str) -> Option<usize> {
    let open = rest.chars().next()?;
    label_brackets(open)?;
    let all_closes = ['>', '\u{FF1E}', '\u{3009}', '\u{232A}', ')', '\u{FF09}'];
    let (index, ch) = rest
        .char_indices()
        .skip(1)
        .find(|(_, ch)| all_closes.contains(ch))?;
    let inner = &rest[open.len_utf8()..index];
    strict_label_inner(inner).then_some(index + ch.len_utf8())
}

/// Tells whether the inside of a label is only "개정", a date and a number.
fn strict_label_inner(inner: &str) -> bool {
    static DATE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(&format!("{LABEL_DATE_PATTERN}\\.?")).expect("valid date pattern")
    });
    static NUMBER: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?:[가-힣]{1,6}\s*)?(?:제\s*)?\d+(?:-\d+)?\s*호")
            .expect("valid number pattern")
    });
    let inner = inner.trim().strip_prefix("개정").unwrap_or(inner.trim());
    let (dates, numbers) = (
        DATE.find_iter(inner).count(),
        NUMBER.find_iter(inner).count(),
    );
    if dates > 1 || numbers > 1 || dates + numbers == 0 {
        return false;
    }
    let rest = DATE.replace_all(inner, "");
    let rest = NUMBER.replace_all(&rest, "");
    rest.chars()
        .all(|ch| ch.is_whitespace() || matches!(ch, ',' | '·'))
}

/// Tells whether a bracket group of a heading can be read as a label.
///
/// A group that holds a digit must give a date and a number the label rules can use: at most
/// one date, and every date a real calendar date. A group without any digit, such as
/// "(시행일)", is a heading word and not a label.
fn label_group_readable(group: &str, publication: Option<Date>) -> bool {
    let read = read_label_dates(group, publication);
    if read.iter().any(|item| item.date.is_none()) {
        return false;
    }
    let mut dates: Vec<Date> = read.iter().filter_map(|item| item.date).collect();
    dates.sort();
    dates.dedup();
    if dates.len() > 1 {
        return false;
    }
    let (_, numbers) = label_facts(group, publication);
    // A group without a readable date or number is a heading word, unless it is written like a
    // date or a number ("98.10.16", "규칙2148호"). A title that holds a digit is no label.
    static LOOKS_LIKE_FACT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\d+\s*[.·\-]\s*\d+|\d+\s*호|제\s*\d+").expect("valid label shape pattern")
    });
    !dates.is_empty() || !numbers.is_empty() || !LOOKS_LIKE_FACT.is_match(group)
}

/// How one label field relates to the revision it must identify.
#[derive(PartialEq)]
enum Match {
    Absent,
    Same,
    Different,
}

/// Compares every value found in a label with the revision value.
fn compare_label<T: PartialEq>(found: &[T], expected: Option<&T>) -> Match {
    if found.is_empty() {
        Match::Absent
    } else if expected.is_some_and(|value| found.iter().all(|item| item == value)) {
        Match::Same
    } else {
        Match::Different
    }
}

/// Reads the original promulgation number from the source metadata.
fn promulgation_number(xml: &[u8]) -> Option<u64> {
    let text = String::from_utf8_lossy(xml);
    static NUMBER: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"<공포번호>\s*(?:<!\[CDATA\[\s*)?(\d+)").expect("valid number pattern")
    });
    NUMBER.captures(&text)?[1].parse().ok()
}

/// A date written in a label. `date` is empty when the label date cannot be read.
struct LabelDate {
    date: Option<Date>,
    /// The label wrote the year with two digits.
    two_digit: bool,
}

/// Separators between the year, month and day of a label date: a full stop, a middle dot of
/// any width, a hyphen, or only spaces.
const LABEL_DATE_PATTERN: &str =
    r"(\d{2,4})(?:\s*[.·ㆍ・‧∙⋅\-]\s*|\s+)(\d{1,2})(?:\s*[.·ㆍ・‧∙⋅\-]\s*|\s+)(\d{1,2})";

/// Reads every date of a label. A year of two digits is read as 19YY or 20YY, whichever lies
/// nearer to the promulgation date (lead ruling). A tie, or no promulgation date, leaves the
/// date unread, and so does a year of three digits or a day that does not exist.
fn read_label_dates(label: &str, publication: Option<Date>) -> Vec<LabelDate> {
    static DATE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(LABEL_DATE_PATTERN).expect("valid label date pattern"));
    let mut dates = Vec::new();
    for found in DATE.captures_iter(label) {
        let start = found.get(0).map_or(0, |whole| whole.start());
        if label[..start]
            .chars()
            .last()
            .is_some_and(|ch| ch.is_ascii_digit())
        {
            continue;
        }
        let (month, day) = (found[2].parse::<u8>().ok(), found[3].parse::<u8>().ok());
        let build = |year: i32| make_date(year, month?, day?);
        let year: i32 = found[1].parse().unwrap_or(0);
        let two_digit = found[1].len() == 2;
        let date = match found[1].len() {
            4 => build(year),
            2 => {
                let candidates: Vec<Date> = [1900 + year, 2000 + year]
                    .into_iter()
                    .filter_map(build)
                    .collect();
                match (candidates.as_slice(), publication) {
                    ([only], _) => Some(*only),
                    ([older, newer], Some(published)) => {
                        let (from_older, from_newer) = (
                            (*older - published).whole_days().abs(),
                            (*newer - published).whole_days().abs(),
                        );
                        match from_older.cmp(&from_newer) {
                            std::cmp::Ordering::Less => Some(*older),
                            std::cmp::Ordering::Greater => Some(*newer),
                            std::cmp::Ordering::Equal => None,
                        }
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        dates.push(LabelDate { date, two_digit });
    }
    dates
}

/// Reads the dates (compact YYYYMMDD) and numbers ("제N호" or "N호") written in a section label.
/// A date that cannot be read is left out, and the heading reader marks such a label unread.
fn label_facts(label: &str, publication: Option<Date>) -> (Vec<String>, Vec<u64>) {
    static NUMBER: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?:제\s*)?(\d+)\s*호").expect("valid label number pattern"));
    let dates = read_label_dates(label, publication)
        .into_iter()
        .filter_map(|item| item.date)
        .map(compact_iso)
        .collect();
    let numbers = NUMBER
        .captures_iter(label)
        .filter_map(|c| c[1].parse().ok())
        .collect();
    (dates, numbers)
}

/// Formats a date as YYYYMMDD.
fn compact_iso(date: Date) -> String {
    iso(date).replace('-', "")
}

/// Marks the label formats the user approved on 2026-11-03: a two-digit year and a number
/// written without "제". The century choice for a two-digit year is a lead ruling.
fn add_label_format_notes(
    evidence: &mut serde_json::Map<String, Value>,
    label: &str,
    publication: Option<Date>,
) {
    static BARE_NUMBER: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?:^|[^\d제\s])\s*\d+\s*호").expect("valid bare number"));
    let two_digit = read_label_dates(label, publication)
        .iter()
        .any(|item| item.two_digit);
    let bare = BARE_NUMBER.is_match(label);
    if !two_digit && !bare {
        return;
    }
    evidence.insert(
        "label_format_user_decision".to_owned(),
        json!(LABEL_FORMAT_DECISION),
    );
    let mut formats = Vec::new();
    if two_digit {
        formats.push("two-digit-year");
        add_lead_ruling(evidence, CENTURY_RULING);
    }
    if bare {
        formats.push("number-without-je");
    }
    evidence.insert("label_formats".to_owned(), json!(formats));
}

/// Returns the labelled section that provably belongs to this revision.
///
/// A label belongs to the revision when its date or its number matches and nothing in
/// the label contradicts the revision. Several candidate sections make the match unclear.
fn attributed_section<'a>(
    source: &SourceRevision,
    sections: &'a [Section],
) -> Option<(usize, &'a Section, Vec<&'static str>)> {
    let expected_number = promulgation_number(&source.xml);
    let mut found = Vec::new();
    for (index, section) in sections.iter().enumerate() {
        // A heading whose label cannot be read is never a candidate.
        if section.unreadable.is_some() {
            continue;
        }
        let (dates, numbers) =
            label_facts(&section.label, compact_date(source.promulgation.trim()));
        let promulgation = compact_date(source.promulgation.trim()).map(compact_iso);
        let by_date = compare_label(&dates, promulgation.as_ref());
        let by_number = compare_label(&numbers, expected_number.as_ref());
        if (by_date == Match::Same || by_number == Match::Same)
            && by_date != Match::Different
            && by_number != Match::Different
        {
            let mut matched = Vec::new();
            if by_date == Match::Same {
                matched.push("date");
            }
            if by_number == Match::Same {
                matched.push("number");
            }
            found.push((index, section, matched));
        }
    }
    (found.len() == 1).then(|| found.remove(0))
}

/// Amendment kinds the two addendum date rules may resolve. Amendments made by another
/// ordinance (타법개정, 타법폐지) and bulk amendments (일괄개정) stay unresolved by the user's
/// approval of the addendum rules, because the addendum may belong to the amending ordinance.
/// A repeal-and-enact revision (폐지제정) follows the rules of an enactment (제정).
fn addendum_rule_amendment(amendment: &str) -> bool {
    matches!(
        amendment,
        "제정" | "전부개정" | "일부개정" | "폐지" | "폐지제정"
    )
}

/// Tells whether the article text is one repeal sentence ("... 조례를 폐지한다.") and nothing else.
fn repeal_sentence_only(body: &str) -> bool {
    let body = body.trim();
    !body.is_empty()
        && !body.contains('\n')
        && compact_text(body)
            .trim_end_matches('.')
            .ends_with("폐지한다")
}

/// Tells whether a repeal-and-enact source (폐지제정) is a repeal: its only article is one
/// repeal sentence. Such a revision enacts nothing, so the new paths treat it as a repeal.
pub fn repeal_and_enact_is_repeal(xml: &[u8]) -> bool {
    date_text(xml).is_ok_and(|found| repeal_sentence_only(&found.body))
}

/// Result of the single-addendum label rule.
enum LabelOutcome {
    /// The effective date and the evidence to record.
    Applied(Date, serde_json::Map<String, Value>),
    /// The label has only a number and it differs from the revision. The revision stays
    /// unresolved by the lead's conservative default, which is not a user decision.
    NumberOnlyMismatch(serde_json::Map<String, Value>),
    /// One of the label date and number matches the revision and the other differs. The
    /// revision stays unresolved for every amendment kind (the lead's conservative default).
    Contradiction(serde_json::Map<String, Value>),
    /// The label date and the label number both differ from the revision. The label belongs to
    /// another amendment, so the revision stays unresolved (user decision).
    BothDiffer(serde_json::Map<String, Value>),
}

/// Applies decision single-label-date-priority-20261103 to a revision with one addendum.
///
/// When the only addendum says just "this ordinance takes effect on the day of promulgation",
/// the date written in its label wins over the promulgation date, but an earlier label date
/// yields the promulgation date. The label date alone is not a contradiction: a label whose
/// date and number disagree about the revision is left to the other rules.
fn single_label_priority(
    source: &SourceRevision,
    addenda: &[Addendum],
    publication: Date,
    #[cfg(test)] trace_addenda: Option<&[SourceSegments]>,
) -> Option<LabelOutcome> {
    let usable: Vec<&Addendum> = addenda
        .iter()
        .filter(|block| !block.text.trim().is_empty())
        .collect();
    if usable.len() != 1 {
        return None;
    }
    let block = usable[0];
    #[cfg_attr(not(test), allow(unused_mut))]
    let mut sections = split_sections(&block.text, Some(publication));
    #[cfg(test)]
    let block_view = trace_addenda.map(|views| {
        let index = addenda
            .iter()
            .position(|candidate| std::ptr::eq(candidate, block))
            .expect("addendum index");
        views[index].clone()
    });
    #[cfg(test)]
    attach_section_views(&mut sections, block_view.as_ref(), Some(publication));
    let (label, body, dates, numbers) = match sections.as_slice() {
        [section] => {
            let (dates, numbers) = label_facts(&section.label, Some(publication));
            (
                section.label.trim().to_owned(),
                section.body.as_str(),
                dates,
                numbers,
            )
        }
        // An addendum unit without a heading line carries its date in the unit label.
        [] if compact_date(block.label.trim()).is_some() => (
            block.label.trim().to_owned(),
            block.text.as_str(),
            vec![block.label.trim().to_owned()],
            Vec::new(),
        ),
        _ => return None,
    };
    #[cfg(test)]
    let body_view = match sections.as_slice() {
        [section] => section.source_view.clone(),
        _ => block_view.clone(),
    };
    label_outcome_for_section(
        source,
        &label,
        body,
        &dates,
        &numbers,
        publication,
        #[cfg(test)]
        ("single_label_priority", body_view.as_ref()),
    )
}

/// Gives each section that `split_sections` cut from a text the SourceView of its body. The
/// sections keep no view when the reconstruction does not give the same number of sections, so
/// their calls stay unobserved in the trace.
/// The written-text reconstruction of an amendment part: the same lines of the written amendment
/// view, joined, with the first-line mark removed, as `amendment_parts` cuts `raw`.
fn amendment_part_written(
    view: &SourceSegments,
    (from, to): (usize, usize),
    mark: usize,
) -> Result<SourceSegments> {
    let offsets: Vec<usize> = view
        .text
        .split_inclusive('\n')
        .scan(0, |offset, line| {
            let previous = *offset;
            *offset += line.len();
            Some(previous)
        })
        .collect();
    ensure!(from < offsets.len(), "amendment part line out of range");
    let end = if to < offsets.len() {
        offsets[to]
    } else {
        view.text.len()
    };
    view.amendment_part(offsets[from], end, mark)
}

#[cfg(test)]
fn attach_section_views(
    sections: &mut [Section],
    view: Option<&SourceSegments>,
    publication: Option<Date>,
) {
    let Some(view) = view else { return };
    let Ok(bodies) = view.section_bodies(publication) else {
        return;
    };
    if bodies.len() != sections.len() {
        return;
    }
    for (section, reconstructed) in sections.iter_mut().zip(bodies) {
        section.source_view = Some(reconstructed);
    }
}

/// The label rule for one section whose label dates (`YYYYMMDD`, or the unit label of an
/// addendum without a heading) and numbers are already read. The addendum path and the path
/// of addendum text inside the articles share it, so both give the same evidence and both
/// pass the next-revision cap of `defer_late_label_dates`.
///
/// It returns nothing when the label sets no date and its number does not differ.
fn label_outcome_for_section(
    source: &SourceRevision,
    label: &str,
    body: &str,
    dates: &[String],
    numbers: &[u64],
    publication: Date,
    #[cfg(test)] trace: (&'static str, Option<&SourceSegments>),
) -> Option<LabelOutcome> {
    // The call site and the reconstruction of `body` for the two reads of `body` below.
    #[cfg(test)]
    let observe_plain = || {
        if let Some(view) = trace.1 {
            observe(
                "label_plain",
                trace.0,
                true,
                vec![("decoded", body, view.clone())],
            );
        }
    };
    let label = label.to_owned();
    let mut distinct = dates.to_vec();
    distinct.sort();
    distinct.dedup();
    if distinct.is_empty() {
        // A number without a written date cannot set a date. A number that differs from the
        // revision may belong to another ordinance, so the revision stays unresolved.
        let mismatch = !numbers.is_empty()
            && compare_label(numbers, promulgation_number(&source.xml).as_ref())
                == Match::Different;
        #[cfg(test)]
        observe_plain();
        plain_promulgation_clause(body, &source.category).filter(|_| mismatch)?;
        let mut evidence = serde_json::Map::new();
        approved_rule(&mut evidence, "number-only-mismatch");
        evidence.insert("attributed_label".to_owned(), json!(label));
        add_label_format_notes(&mut evidence, &label, Some(publication));
        return Some(LabelOutcome::NumberOnlyMismatch(evidence));
    }
    let [only] = distinct.as_slice() else {
        return None;
    };
    let label_date = compact_date(only)?;
    let by_date = if label_date == publication {
        Match::Same
    } else {
        Match::Different
    };
    let by_number = compare_label(numbers, promulgation_number(&source.xml).as_ref());
    #[cfg(test)]
    observe_plain();
    let timing = plain_promulgation_clause(body, &source.category)?;
    if (by_date == Match::Same && by_number == Match::Different)
        || (by_date == Match::Different && by_number == Match::Same)
    {
        let mut evidence = serde_json::Map::new();
        approved_rule(&mut evidence, "label-contradiction");
        evidence.insert("attributed_label".to_owned(), json!(label));
        add_label_format_notes(&mut evidence, &label, Some(publication));
        return Some(LabelOutcome::Contradiction(evidence));
    }
    if by_date == Match::Different && by_number == Match::Different {
        let mut evidence = serde_json::Map::new();
        evidence.insert(
            "user_decision".to_owned(),
            json!(LABEL_BOTH_DIFFER_DECISION),
        );
        evidence.insert("approved_rule".to_owned(), json!("label-both-differ"));
        evidence.insert("attributed_label".to_owned(), json!(label));
        add_label_format_notes(&mut evidence, &label, Some(publication));
        return Some(LabelOutcome::BothDiffer(evidence));
    }
    let (effective, side) = if label_date > publication {
        (label_date, "label")
    } else {
        (publication, "promulgation")
    };
    let mut evidence = serde_json::Map::new();
    evidence.insert(
        "decision".to_owned(),
        json!("single-label-date-priority-20261103"),
    );
    evidence.insert("attributed_label".to_owned(), json!(label));
    evidence.insert("applied_side".to_owned(), json!(side));
    evidence.insert("label_date".to_owned(), json!(iso(label_date)));
    evidence.insert("separate_application_timing".to_owned(), json!(timing));
    add_label_format_notes(&mut evidence, &label, Some(publication));
    Some(LabelOutcome::Applied(effective, evidence))
}

/// The state of the label of one addendum section against the revision.
#[derive(Clone, Copy, PartialEq)]
enum LabelState {
    /// No date or number is written (no label, or a heading word such as "(시행일)").
    None,
    /// The label cannot be read.
    Unreadable,
    /// The date or the number matches and nothing contradicts.
    Match,
    /// A date is written and it differs; no number is written.
    DateOnlyMismatch,
    /// Only a number is written and it differs.
    NumberOnlyMismatch,
    /// The date matches and the number differs, or the reverse.
    OneSide,
    /// The date and the number both differ.
    BothDiffer,
}

/// Reads the label of a section: its state and its date.
fn label_state(
    source: &SourceRevision,
    section: &Section,
    publication: Date,
) -> (LabelState, Option<Date>) {
    if section.unreadable.is_some() {
        return (LabelState::Unreadable, None);
    }
    let (dates, numbers) = label_facts(&section.label, Some(publication));
    let first = dates.first().and_then(|date| compact_date(date));
    let by_date = compare_label(&dates, Some(&compact_iso(publication)));
    let by_number = compare_label(&numbers, promulgation_number(&source.xml).as_ref());
    let state = if dates.is_empty() && numbers.is_empty() {
        LabelState::None
    } else if by_date == Match::Different && by_number == Match::Different {
        LabelState::BothDiffer
    } else if (by_date == Match::Same && by_number == Match::Different)
        || (by_date == Match::Different && by_number == Match::Same)
    {
        LabelState::OneSide
    } else if by_date == Match::Same || by_number == Match::Same {
        LabelState::Match
    } else if dates.is_empty() {
        LabelState::NumberOnlyMismatch
    } else {
        LabelState::DateOnlyMismatch
    };
    (state, first)
}

/// Records the decision ids of the sentences that decide a date.
fn add_clause_ids(evidence: &mut serde_json::Map<String, Value>, clauses: &[Clause]) {
    if clauses.iter().any(|clause| clause.spaced) {
        add_id(evidence, "lead_rulings", SPACED_VERB_RULING);
    }
    if clauses.iter().any(|clause| clause.without_buteo) {
        add_id(evidence, "user_decisions", WITHOUT_BUTEO_DECISION);
    }
    if clauses.iter().any(|clause| clause.omitted_mark) {
        add_id(evidence, "user_decisions", PROVISO_OMITTED_DECISION);
    }
    if clauses.iter().any(|clause| clause.explicit_without_buteo) {
        add_id(evidence, "user_decisions", EXPLICIT_WITHOUT_BUTEO_DECISION);
    }
}

/// What the target section decides before the older rules run.
enum PreVerdict {
    Confirmed(Date, serde_json::Map<String, Value>, &'static str),
    Unresolved(serde_json::Map<String, Value>, String),
    /// The target section holds a sentence the detection found and the reader did not read.
    /// The older rules run, and a date that they confirm is withheld.
    Guard,
}

/// Decides the revisions the older rules read badly: a target addendum section is chosen
/// (the only section, the attributed section, or the last section when nothing is
/// attributed), and its effective-date sentences decide. It returns nothing when the older
/// rules must decide, so they run unchanged.
///
/// On the path of addendum text inside the articles (no addendum block holds text) the
/// decision is total: it never returns nothing, so the whole body is never read as one text.
fn pre_decide(
    source: &SourceRevision,
    addenda: &[Addendum],
    body: &str,
    publication: Date,
    #[cfg(test)] trace_body: Option<&SourceSegments>,
    #[cfg(test)] trace_addenda: Option<&[SourceSegments]>,
) -> Option<PreVerdict> {
    if !matches!(source.category.as_str(), "조례" | "규칙")
        || !addendum_rule_amendment(&source.amendment)
    {
        return None;
    }
    let enactment = matches!(source.amendment.as_str(), "제정" | "전부개정" | "폐지제정");
    let usable: Vec<&Addendum> = addenda
        .iter()
        .filter(|block| !block.text.trim().is_empty())
        .collect();
    let in_body = usable.is_empty();
    let mut sections: Vec<Section> = Vec::new();
    if in_body {
        #[cfg(test)]
        if let Some(view) = trace_body {
            observe(
                "body_names",
                "pre_decide:body_names",
                false,
                vec![("decoded", body, view.clone())],
            );
        }
        if !names_effective_date(body) {
            return None;
        }
        sections = split_sections(body, Some(publication));
        #[cfg(test)]
        attach_section_views(&mut sections, trace_body, Some(publication));
        if sections.is_empty() {
            sections.push(Section {
                label: String::new(),
                body: body.to_owned(),
                unreadable: None,
                after_heading: false,
                #[cfg(test)]
                source_view: trace_body.cloned(),
            });
        }
    } else {
        for block in &usable {
            #[cfg_attr(not(test), allow(unused_mut))]
            let mut next = split_sections(&block.text, Some(publication));
            #[cfg(test)]
            if let Some(views) = trace_addenda {
                let index = addenda
                    .iter()
                    .position(|candidate| std::ptr::eq(candidate, *block))
                    .expect("addendum index");
                attach_section_views(&mut next, Some(&views[index]), Some(publication));
            }
            sections.extend(next);
        }
        if sections.is_empty() {
            return None;
        }
    }
    let unresolved = |reason: &str, evidence: serde_json::Map<String, Value>| {
        Some(PreVerdict::Unresolved(
            evidence,
            format!("effective date is unresolved; {reason}"),
        ))
    };
    let count = sections.len();
    let last = count - 1;
    let attributed = attributed_section(source, &sections).map(|found| found.0);
    let (target, group) = if count == 1 {
        (0, 'S')
    } else if let Some(index) = attributed {
        (index, 'A')
    } else {
        (last, 'L')
    };
    let (state, label_date) = label_state(source, &sections[target], publication);
    let mut evidence = serde_json::Map::new();
    if in_body {
        add_id(&mut evidence, "lead_rulings", BODY_SECTIONS_RULING);
    }
    if group == 'L' {
        add_id(&mut evidence, "lead_rulings", NO_ATTRIBUTION_LAST_RULING);
    }
    let clauses = read_section_clauses(&sections[target]);
    if clauses.is_empty() {
        // The attributed heading may have no sentence of its own when the labels follow the
        // text; the sentences of the whole addendum part decide.
        if in_body && group == 'A' && state == LabelState::Match {
            let all: Vec<Clause> = sections.iter().flat_map(read_section_clauses).collect();
            if !all.is_empty()
                && all
                    .iter()
                    .all(|clause| clause.kind == ClauseKind::Plain && !clause.proviso)
            {
                // The ids of the sentences that decide are recorded as in the main branch, and
                // the enactment ruling only applies to an enactment.
                add_clause_ids(&mut evidence, &all);
                if enactment {
                    add_id(&mut evidence, "lead_rulings", ATTRIBUTION_ENACTMENT_RULING);
                }
                evidence.insert(
                    "decision".to_owned(),
                    json!("ordinance-attributed-promulgation"),
                );
                return Some(PreVerdict::Confirmed(
                    publication,
                    evidence,
                    "attributed addendum sets effect on the day of promulgation",
                ));
            }
        }
        return if in_body {
            unresolved(
                "the target addendum section has no effective-date sentence",
                evidence,
            )
        } else {
            None
        };
    }
    add_clause_ids(&mut evidence, &clauses);
    if clauses.iter().any(|clause| clause.proviso) {
        evidence.insert("proviso_after_effective_sentence".to_owned(), json!(true));
        add_id(&mut evidence, "user_decisions", PROVISO_SCOPE_DECISION);
        return unresolved("a proviso follows the effective-date sentence", evidence);
    }
    let explicit: Vec<Date> = clauses
        .iter()
        .filter_map(|clause| match clause.kind {
            ClauseKind::Explicit(date) => Some(date),
            _ => None,
        })
        .collect();
    let others = clauses
        .iter()
        .any(|clause| clause.kind == ClauseKind::Other);
    if !explicit.is_empty() {
        // The sentences of the section must agree on one date, as `clause_date` requires.
        let mut dates: Vec<Date> = explicit.clone();
        if clauses
            .iter()
            .any(|clause| clause.kind == ClauseKind::Plain)
        {
            dates.push(publication);
        }
        dates.sort();
        dates.dedup();
        if others || dates.len() != 1 {
            return unresolved(
                "the sentences of the target section name different dates",
                evidence,
            );
        }
        let date = dates[0];
        if group == 'A' && attributed != Some(last) {
            return unresolved(
                "the date is written only in a section before the last one",
                evidence,
            );
        }
        if state == LabelState::None {
            return if enactment {
                Some(PreVerdict::Confirmed(
                    date,
                    evidence,
                    "approved clause in selected addendum",
                ))
            } else if in_body {
                unresolved(
                    "a section without a label of an amendment is not read",
                    evidence,
                )
            } else {
                None
            };
        }
        match state {
            LabelState::BothDiffer => {
                add_id(
                    &mut evidence,
                    "user_decisions",
                    BOTH_DIFFER_EXPLICIT_DECISION,
                );
            }
            LabelState::OneSide => {
                add_id(&mut evidence, "user_decisions", ONE_SIDE_EXPLICIT_DECISION);
            }
            _ => {}
        }
        let reason = if in_body {
            "approved clause in source body"
        } else {
            "approved clause in selected addendum"
        };
        return Some(PreVerdict::Confirmed(date, evidence, reason));
    }
    if others {
        return if in_body {
            unresolved(
                "the effective-date sentence names no approved form",
                evidence,
            )
        } else if clauses.iter().any(|clause| clause.unread) {
            // The older rules run; a date they confirm is withheld (B3).
            Some(PreVerdict::Guard)
        } else {
            None
        };
    }
    // Every sentence names the day of promulgation.
    let promulgation = |mut evidence: serde_json::Map<String, Value>, attribute: bool| {
        if attribute {
            evidence.insert(
                "decision".to_owned(),
                json!("ordinance-attributed-promulgation"),
            );
            evidence.insert(
                "attributed_label".to_owned(),
                json!(sections[target].label.trim()),
            );
        }
        let why = if attribute {
            "attributed addendum sets effect on the day of promulgation"
        } else if in_body {
            "approved clause in source body"
        } else {
            "approved clause in selected addendum"
        };
        Some(PreVerdict::Confirmed(publication, evidence, why))
    };
    match group {
        'A' => {
            if enactment {
                add_id(&mut evidence, "lead_rulings", ATTRIBUTION_ENACTMENT_RULING);
                promulgation(evidence, true)
            } else if in_body {
                promulgation(evidence, true)
            } else {
                None
            }
        }
        'L' => {
            if state == LabelState::None {
                if enactment {
                    add_id(&mut evidence, "user_decisions", LAST_UNLABELED_DECISION);
                    promulgation(evidence, false)
                } else if in_body {
                    unresolved(
                        "a section without a label of an amendment is not read",
                        evidence,
                    )
                } else {
                    None
                }
            } else {
                unresolved(
                    "the last addendum section has a label that is not this revision's",
                    evidence,
                )
            }
        }
        _ => {
            if !in_body {
                return None;
            }
            match state {
                LabelState::None => {
                    if enactment {
                        promulgation(evidence, false)
                    } else {
                        unresolved(
                            "a section without a label of an amendment is not read",
                            evidence,
                        )
                    }
                }
                LabelState::Unreadable => unresolved("the label cannot be read", evidence),
                _ => {
                    let section = &sections[target];
                    let (dates, numbers) = label_facts(&section.label, Some(publication));
                    match label_outcome_for_section(
                        source,
                        section.label.trim(),
                        &section.body,
                        &dates,
                        &numbers,
                        publication,
                        #[cfg(test)]
                        ("pre_decide:target_section", section.source_view.as_ref()),
                    ) {
                        Some(LabelOutcome::Applied(date, mut applied)) => {
                            applied.extend(evidence);
                            Some(PreVerdict::Confirmed(
                                date,
                                applied,
                                "single addendum label date takes priority over the promulgation date",
                            ))
                        }
                        Some(_) => unresolved(
                            "the label of the only section is not this revision's",
                            evidence,
                        ),
                        None if state == LabelState::Match && label_date.is_none() => {
                            // The number alone matches: the section is the revision's own.
                            promulgation(evidence, false)
                        }
                        None => unresolved("the label of the only section sets no date", evidence),
                    }
                }
            }
        }
    }
}

/// Checks that a section sets its effective date only by "this ordinance takes effect on
/// the day of promulgation". Returns the other timing sentences it keeps as separate evidence.
/// The body is the body of a section, so it starts right after an addendum heading.
fn plain_promulgation_clause(body: &str, subject: &str) -> Option<Vec<String>> {
    #[cfg(test)]
    let _entry = trace_entry("plain_promulgation_clause", true, &[("decoded", body)]);
    static CLAUSE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"이\s*(조례|규칙)(?:은|는)[^\n]*?(?:시행(?:\s*한다|하되|하며|하고|함)|효력을\s*발생한다)",
        )
        .expect("valid promulgation clause pattern")
    });
    let clause = &*CLAUSE;
    let plain: Vec<String> = ["은", "는"]
        .iter()
        .flat_map(|particle| {
            ["", "부터", "로부터"]
                .iter()
                .map(move |tail| format!("이{subject}{particle}공포한날{tail}시행한다"))
        })
        .collect();
    // A detected sentence this reader does not find keeps the section unread.
    let found_starts: Vec<usize> = clause.find_iter(body).map(|found| found.start()).collect();
    if detected_sentence_starts(body, true)
        .iter()
        .any(|start| !found_starts.contains(start))
    {
        return None;
    }
    let mut count = 0;
    for found in clause.find_iter(body) {
        let text = compact_text(found.as_str()).replace("(시행일)", "");
        if !plain.contains(&text) || starts_proviso(&body[found.end()..]) {
            return None;
        }
        count += 1;
    }
    (count > 0).then(|| {
        body.lines()
            .map(|line| clause.replace_all(line, "").trim().to_owned())
            .filter(|line| line.contains("시행한다") || line.contains("적용한다"))
            .collect()
    })
}

/// Tells whether addendum text in the articles of this revision is excluded by its amendment kind.
///
/// Amendments made by another ordinance stay unresolved (rule 8: the text in the articles
/// follows the addendum rules). A broken publication date keeps the older body rule.
fn excluded_body_kind(dataset: &str, source: &SourceRevision, publication: Option<Date>) -> bool {
    dataset == "ordinances"
        && matches!(source.category.as_str(), "조례" | "규칙")
        && !addendum_rule_amendment(&source.amendment)
        && publication.is_some()
}

/// Tells whether a text names an effective date or time: a phrase without a subject
/// ("부터 시행", "효력을 발생", "효력이 발생", "부터 (그) 효력") or a detected sentence.
///
/// A word such as "시행령" or "시행규칙" alone is not an effective-date phrase.
fn names_effective_date(text: &str) -> bool {
    #[cfg(test)]
    let _entry = trace_entry("names_effective_date", false, &[("decoded", text)]);
    static PHRASE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"부터\s*시행|효력\s*[을이]\s*발생|부터\s*(?:그\s*)?효력")
            .expect("valid effective phrase pattern")
    });
    PHRASE.is_match(text) || sentence_names_effective_date(text)
}

/// The kinds of text a subject names ("이 조례는", "본 규칙은") that the reader reads. The reader
/// builds its subject pattern from this list; the detection knows every one of them and more
/// (`DETECT_KINDS`), so a subject the reader reads is never missed by the detection.
const SUBJECT_KINDS: &str = "법률|법|영|규칙|조례|훈령|예규|고시|공고|지침|규정";

/// The kinds the detection knows: every kind of the reader and more (령, 세칙, 요령, 기준, 내규
/// and the Chinese-character kinds). 훈령, 요령 and 訓令 stay in the list although 령 and 令
/// look enough: a prefix of six characters in front of "훈령" ("대통령경호처훈령") is too long
/// for the bare 령 to match.
const DETECT_KINDS: &str = "법률|법|영|령|규칙|조례|훈령|예규|고시|공고|지침|규정|세칙|요령|기준|내규|法律|法|令|條例|規則|訓令|例規|告示|公告|指針|規定";

/// One inline space of a subject: any white space except a line feed and a carriage return (a
/// no-break space, an ideographic space, an en space and an em space too). The subject pattern and
/// the article mark after a heading use this set. A heading line and a proviso bracket use all white
/// space (`char::is_whitespace`).
const INLINE_SPACE: &str = r"[\s&&[^\r\n]]";

/// The opening brackets that may stand before a proviso word ("(다만", "（다만", "[다만", "［다만").
const PROVISO_OPEN: &[char] = &['(', '\u{FF08}', '[', '\u{FF3B}'];

/// Opening and closing characters that hold a note inside a sentence.
const OPENERS: &[char] = &[
    '(', '（', '[', '［', '〔', '「', '『', '“', '‘', '<', '＜', '〈', '《',
];
const CLOSERS: &[char] = &[
    ')', '）', ']', '］', '〕', '」', '』', '”', '’', '>', '＞', '〉', '》',
];

/// Tells whether one sentence of the form "이 조례는 ... 시행한다" names an effective date even
/// when it has no "부터" ("...의 승인을 받은 날 시행한다", "2030년 1월 1일 시행한다").
fn sentence_names_effective_date(text: &str) -> bool {
    !detected_sentence_starts(text, false).is_empty()
}

/// Tells whether a text holds an effective-date verb or phrase, whatever its ending.
///
/// Spaces and line feeds may stand between the characters ("시\n행한다", "施 行"). The Korean verb
/// stems are 하, 한, 함, 할, 합, 해, 했, 되, 된, 됨, 될, 됩, 돼, 됐, 토, 키, 코, 케, 치 (so the
/// polite endings "시행합니다", "시행됩니다" and "시행하였습니다" count) and the causative 시키
/// forms (시키, 시켜, 시킨, 시킬, 시킴, 시켰, 시킵). A text that ends right after "시행" counts
/// too (the nominal ending "시행."). The phrases are "효력을 발생", "효력이 발생" and "부터 (그)
/// 효력". A noun use is no verb: "시행령", "시행규칙", "시행일", "시행에 관하여". A verb
/// written in Chinese characters counts unless "令" or "規" follows ("施行令", "施行規則").
///
/// Three kinds of form stay outside the detection, by user decision: a noun use of "시행" that
/// is no verb ("시행에 들어간다", "시행일은 ...로 한다"), a phrase without a subject that the
/// rule of separate articles keeps readable, and a sentence that a boundary form closes early
/// (see `sentence_end`). Any other form of the vocabulary that is missed is a defect.
fn has_effective_verb(text: &str) -> bool {
    static KOREAN: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"시\s*행\s*(?:[하한함할합해했되된됨될됩돼됐토키코케치]|시[키켜킨킬킴켰킵])|시\s*행\s*$|효력\s*[을이]\s*발생|부터\s*(?:그\s*)?효력",
        )
        .expect("valid effective verb pattern")
    });
    static HANJA: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"施\s*行").expect("valid effective verb pattern"));
    if KOREAN.is_match(text) {
        return true;
    }
    HANJA.find_iter(text).any(|found| {
        // A text that ends after "施行" cannot tell, so it counts (the wide side).
        !matches!(
            text[found.end()..].trim_start().chars().next(),
            Some('令' | '規')
        )
    })
}

/// Returns the start of every sentence of a text that holds an effective-date verb and that the
/// reader may not read: a sentence with a subject, and the first sentence after an addendum
/// heading when part of it is no sentence with a subject.
///
/// The detection is wide on purpose (user decisions user-broad-detection-20261110,
/// user-r22-full-vocabulary-20261111 and user-r23-polite-endings-20261111). The subject is "이",
/// "본" or "本", a prefix, a kind of text (`DETECT_KINDS`), an optional note in brackets and the
/// particle 은, 는, 이 or 가. The prefix has at most six characters; it may hold a full stop only
/// when no space stands in it (so "이.조례는" is a subject, as it was in r21, and "본다.이 조례는"
/// is not a subject that starts at "본"). Inline spaces (`INLINE_SPACE`) may split the prefix into
/// up to three words. The part after the subject ends at only one boundary: see `sentence_end`. A
/// line start and a length are no boundary, so a verb of another article before the next full
/// stop counts: that revision stays unresolved instead of taking the default period.
///
/// The first sentence after a heading is checked from two starts, because the date reading and
/// the pattern of r22 skip a heading label differently. Start A follows the label of the date
/// reading (`heading_label`) and an article mark such as "제1조(시행일)". Start B follows the
/// pattern of r22: white space, one bracket group of at most 60 characters, an article mark and
/// white space. A heading is any line that starts with "부칙" (`heading_line`), whatever follows
/// "부칙". From each start the sentence is cut out of the text (`sentence_end`) and the sentences
/// of detected subjects that start inside it are taken out. When one of the remaining pieces
/// holds an effective-date verb or phrase, the start is returned and the reader keeps that
/// sentence unread. `after_heading` tells that the text itself starts right after a heading (the
/// body of a section); starts A and B are then taken at the start of the text.
///
/// Three kinds of form stay outside the detection by user decision: a noun use of "시행" that is
/// no verb, a phrase without a subject that the rule of separate articles keeps readable, and a
/// sentence that a boundary form closes early (see `sentence_end`); and a subject of four or more
/// words, a prefix of seven or more characters before the kind, or a subject split by a line feed
/// (user-r24-subject-boundary-limit-20261111). Any other form that the lists name and that is
/// missed is a defect.
fn detected_sentence_starts(text: &str, after_heading: bool) -> Vec<usize> {
    #[cfg(test)]
    if std::env::var_os("SOURCEVIEW_BASELINE_TRACE").is_some() {
        eprintln!(
            "{}",
            json!({"phase":"baseline", "representation":"B", "function":"detected_sentence_starts", "after_heading":after_heading, "input_utf8":text, "byte_length":text.len(), "bytes_hex":text.as_bytes().iter().map(|b| format!("{b:02x}")).collect::<String>()})
        );
    }
    static SUBJECT: LazyLock<Regex> = LazyLock::new(|| {
        let space = INLINE_SPACE;
        let word = r"[^\s.。]{0,6}?";
        Regex::new(&format!(
            r"(?:이|본|本)\s*(?:[^\s]{{0,6}}?|{word}(?:{space}+{word}){{0,2}})(?:{DETECT_KINDS})(?:[(（][^)）\n]{{0,4}}[)）])?{space}*(?:은|는|이|가)"
        ))
        .expect("valid effective sentence subject pattern")
    });
    // The article mark that may stand between a heading and its first sentence.
    static ARTICLE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(&format!(
            r"^\s*(?:제\s*\d+\s*조(?:의\s*\d+)?{}*(?:[(（][^)）\n]{{0,20}}[)）])?)?\s*",
            INLINE_SPACE
        ))
        .expect("valid article mark pattern")
    });
    // The pattern of r22 after "칙": white space, one bracket group, an article mark, white space.
    static AFTER_HEADING: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"^\s*(?:[<(\[〈＜（［][^>)\]〉＞）］\n]{0,60}[>)\]〉＞）］])?(?:\s*제\s*\d+\s*조\s*(?:\([^)\n]{0,20}\))?)?\s*",
        )
        .expect("valid heading tail pattern")
    });
    let mut found = Vec::new();
    // A detected subject and the end of its sentence.
    let mut subjects: Vec<(usize, usize)> = Vec::new();
    for subject in SUBJECT.find_iter(text) {
        let tail = &text[subject.end()..];
        let end = sentence_end(tail);
        if has_effective_verb(&tail[..end]) {
            found.push(subject.start());
            subjects.push((subject.start(), subject.end() + end));
        }
    }
    // Every heading has two starts of its first sentence: after the label and the article mark
    // of the date reading (A) and after the pattern of r22 (B).
    let mut sentence_starts: Vec<usize> = Vec::new();
    let article = |at: usize| at + ARTICLE.find(&text[at..]).map_or(0, |mark| mark.end());
    if after_heading {
        sentence_starts.push(article(0));
        sentence_starts.push(AFTER_HEADING.find(text).map_or(0, |mark| mark.end()));
    }
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        // Only a line whose first non-white-space character is "부" starts a heading; a blank line
        // is not skipped again from every blank line before it.
        if line.trim_start().starts_with('부')
            && let Some(heading) = heading_line(&text[offset..])
        {
            let (_, _, after) = heading_label(heading.rest, None);
            sentence_starts.push(article(after.as_ptr() as usize - text.as_ptr() as usize));
            let tail_at = heading.tail.as_ptr() as usize - text.as_ptr() as usize;
            sentence_starts.push(
                tail_at
                    + AFTER_HEADING
                        .find(heading.tail)
                        .map_or(0, |mark| mark.end()),
            );
        }
        offset += line.len();
    }
    // One heading may be found from several line starts, and both starts may be the same place.
    sentence_starts.sort_unstable();
    sentence_starts.dedup();
    for start in sentence_starts {
        let end = start + sentence_end(&text[start..]);
        // The sentences with a detected subject are read elsewhere; what is left holds the rest.
        let mut cursor = start;
        let mut left_over = false;
        for &(from, to) in subjects
            .iter()
            .filter(|(from, _)| (start..end).contains(from))
        {
            left_over |= from > cursor && has_effective_verb(&text[cursor..from]);
            cursor = cursor.max(to.min(end));
        }
        left_over |= cursor < end && has_effective_verb(&text[cursor..end]);
        if left_over {
            found.push(start);
        }
    }
    found
}

/// The starts of the detection and the starts of the extended bracket subjects
/// (`bracket_subject_starts`). The starts of the r24 detection stay as they are and the extended
/// subjects only add starts, so no start of the r24 detection is lost. The rules that read a date
/// use `detected_sentence_starts` alone; this function serves the gate (`added_any`, `full_any`).
#[cfg(test)]
fn extended_sentence_starts(text: &str, after_heading: bool) -> Vec<usize> {
    let mut found = detected_sentence_starts(text, after_heading);
    for start in bracket_subject_starts(text) {
        if !found.contains(&start) {
            found.push(start);
        }
    }
    found
}

/// The starts of the sentences with a subject whose note is a bracket group, such as
/// "이 조례 (제123호)는", "이 조례([안])는", "이 조례（제1호)는" and "이 조례((가)안)는". The detection
/// of `detected_sentence_starts` takes one note of at most four characters between "(" and ")" (or
/// the fullwidth pair) and nothing else; this function takes a note of any length, any opening
/// bracket of `OPENERS` and any closing bracket of `CLOSERS`, so a mixed pair, a nested group, a
/// group that repeats and a group that no bracket closes belong to it
/// (user-r25-dual-text-detection-20261112, user-holistic-design-20261112).
///
/// A note starts at an opening bracket right after the kind of text (inline spaces may stand
/// before it). The brackets of the note are counted, and the note ends where the count returns to
/// zero. Closing brackets and inline spaces may follow, and another note may follow. A note that
/// the line never closes ends at the first particle after its opening bracket. The subject ends at
/// a particle (은, 는, 이, 가) after the note, and the sentence after it must hold an effective-date
/// verb.
///
/// A subject may stand inside the note of another subject, so the search starts again right after
/// the first character of every match, not after its end. Every place of "이", "본" or "本" where
/// the pattern matches is a candidate start.
#[cfg(test)]
fn bracket_subject_starts(text: &str) -> Vec<usize> {
    bracket_subject_spans(text)
        .into_iter()
        .map(|(start, _)| start)
        .collect()
}

/// The extended subjects of `bracket_subject_starts`: the start of each subject and the end of its
/// particle, in the same order. The gate maps both ends to the source (r25c-normalization-plan.v3.md 3).
fn bracket_subject_spans(text: &str) -> Vec<(usize, usize)> {
    static LEAD: LazyLock<Regex> = LazyLock::new(|| {
        let space = INLINE_SPACE;
        let word = r"[^\s.。]{0,6}?";
        let escape = |list: &[char]| -> String {
            list.iter()
                .map(|ch| format!("\\x{{{:X}}}", *ch as u32))
                .collect()
        };
        let (open, close) = (escape(OPENERS), escape(CLOSERS));
        Regex::new(&format!(
            r"(?:이|본|本)\s*(?:[^\s]{{0,6}}?|{word}(?:{space}+{word}){{0,2}})(?:{DETECT_KINDS}){space}*[\p{{Pe}}\p{{Pf}}{close}]*{space}*[\p{{Ps}}\p{{Pi}}{open}]"
        ))
        .expect("valid effective sentence subject note pattern")
    });
    let is_particle = |ch: char| matches!(ch, '은' | '는' | '이' | '가');
    let mut found = Vec::new();
    let mut at = 0;
    // The brackets and the particles of the line of the last match, found once for every match
    // of that line (a line with many subjects costs one pass, not one pass for each).
    let mut line: Option<LineNotes> = None;
    while let Some(lead) = LEAD.find_at(text, at) {
        // The opening bracket is the last character of the match.
        let mut note = text[..lead.end()]
            .char_indices()
            .next_back()
            .map_or(lead.end(), |(index, _)| index);
        if line
            .as_ref()
            .is_none_or(|known| note < known.start || note > known.end)
        {
            let start = text[..note].rfind('\n').map_or(0, |at| at + 1);
            line = Some(LineNotes::new(text, start, &is_particle));
        }
        let notes = line.as_ref().expect("the line of the match is known");
        let subject_end = loop {
            let Some(&close) = notes.closes.get(&note) else {
                // No bracket closes the note: it ends at the first particle after its opening.
                let opener = text[note..].chars().next().map_or(1, char::len_utf8);
                let first = notes
                    .particles
                    .partition_point(|(index, _)| *index < note + opener);
                break notes.particles.get(first).map(|(index, len)| index + len);
            };
            let mut after = close;
            while let Some(ch) = text[after..notes.end].chars().next() {
                if bracket_kind(ch) == Some(false) || (ch.is_whitespace() && ch != '\n') {
                    after += ch.len_utf8();
                } else {
                    break;
                }
            }
            match text[after..notes.end].chars().next() {
                Some(ch) if bracket_kind(ch) == Some(true) => note = after,
                Some(ch) if is_particle(ch) => break Some(after + ch.len_utf8()),
                _ => break None,
            }
        };
        if let Some(end) = subject_end {
            let tail = &text[end..];
            if has_effective_verb(&tail[..sentence_end(tail)]) {
                found.push((lead.start(), end));
            }
        }
        // Every match starts with a one-character lead ("이", "본" or "本").
        at = lead.start()
            + text[lead.start()..]
                .chars()
                .next()
                .map_or(1, char::len_utf8);
    }
    found
}

/// Tells whether a character opens (`Some(true)`) or closes (`Some(false)`) a bracket. The
/// characters of the general categories Ps and Pi open, Pe and Pf close (Unicode lists them, so a
/// bracket of any script counts, such as the lenticular brackets and U+2329/U+232A), and the
/// lists `OPENERS` and `CLOSERS` stay (they hold the angle brackets "<" and ">" too).
fn bracket_kind(ch: char) -> Option<bool> {
    static OPEN: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^[\p{Ps}\p{Pi}]$").expect("valid opening bracket pattern"));
    static CLOSE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^[\p{Pe}\p{Pf}]$").expect("valid closing bracket pattern"));
    if ch.is_ascii() {
        return match ch {
            '(' | '[' | '{' | '<' => Some(true),
            ')' | ']' | '}' | '>' => Some(false),
            _ => None,
        };
    }
    // Hangul syllables and ideographs are never brackets; skipping them keeps a long line cheap.
    if matches!(ch, '\u{AC00}'..='\u{D7A3}' | '\u{4E00}'..='\u{9FFF}') {
        return None;
    }
    if OPENERS.contains(&ch) {
        return Some(true);
    }
    if CLOSERS.contains(&ch) {
        return Some(false);
    }
    let mut buffer = [0_u8; 4];
    let text: &str = ch.encode_utf8(&mut buffer);
    if OPEN.is_match(text) {
        Some(true)
    } else if CLOSE.is_match(text) {
        Some(false)
    } else {
        None
    }
}

/// The brackets and the particles of one line, for `bracket_subject_starts`. Every closing bracket
/// closes the opening bracket that is open last, whatever pair they make, so one pass finds where
/// each note ends.
struct LineNotes {
    /// The start of the line.
    start: usize,
    /// The end of the line (the offset of its line feed, or of the end of the text).
    end: usize,
    /// For each opening bracket the offset just after the bracket that closes it.
    closes: std::collections::HashMap<usize, usize>,
    /// The offset and the byte length of each particle character, in order.
    particles: Vec<(usize, usize)>,
}

impl LineNotes {
    fn new(text: &str, from: usize, is_particle: &impl Fn(char) -> bool) -> Self {
        let end = text[from..]
            .find('\n')
            .map_or(text.len(), |offset| from + offset);
        let mut closes = std::collections::HashMap::new();
        let mut particles = Vec::new();
        let mut open: Vec<usize> = Vec::new();
        for (index, ch) in text[from..end].char_indices() {
            let index = from + index;
            match bracket_kind(ch) {
                Some(true) => open.push(index),
                Some(false) => {
                    if let Some(opener) = open.pop() {
                        closes.insert(opener, index + ch.len_utf8());
                    }
                }
                None => {}
            }
            if is_particle(ch) {
                particles.push((index, ch.len_utf8()));
            }
        }
        Self {
            start: from,
            end,
            closes,
            particles,
        }
    }
}

/// Returns the byte length of the sentence that starts a text: it ends at a full stop outside
/// brackets and quotes right after a closed sentence form, a closing bracket or quote, or the
/// verb stem "시행" ("施行"). A full stop after a digit is part of a date.
///
/// A sentence that this boundary closes early (a full stop inside "(...). ..." after a closing
/// bracket, or inside quotes) can hide a later verb from the sentence: that is the third accepted
/// limit (the sentence-end boundary forms). It is no defect of the vocabulary.
fn sentence_end(tail: &str) -> usize {
    let mut previous = None;
    let mut before = None;
    let mut depth = 0_i32;
    let mut quote = false;
    for (index, ch) in tail.char_indices() {
        if OPENERS.contains(&ch) {
            depth += 1;
        } else if CLOSERS.contains(&ch) {
            depth = (depth - 1).max(0);
        } else if matches!(ch, '"' | '\'') {
            quote = !quote;
        }
        let ends = matches!(ch, '.' | '。')
            && depth == 0
            && !quote
            && previous.is_some_and(|last: char| {
                matches!(last, '다' | '함' | '음' | '임' | '됨')
                    || CLOSERS.contains(&last)
                    || matches!(last, '"' | '\'')
                    || (matches!(last, '행' | '行') && matches!(before, Some('시' | '施')))
            });
        if ends {
            return index;
        }
        before = previous;
        previous = Some(ch);
    }
    tail.len()
}

/// Reads the addendum written inside the amendment text (`제개정이유내용`).
///
/// The addendum starts at the first line that `heading_line` takes as a heading (a clean heading,
/// or a heading that "개정" follows once white space is taken out) and ends before the next line
/// that starts a new part of the text ("□ 개정이유"). The first sentence may stand on the heading
/// line ("부칙 의회의 승인을 받은 날 시행합니다."). It counts only if it names an effective date
/// ("시행"). Returns nothing otherwise, so the caller keeps the default-period rule. The text is
/// the decoded text of `resolve`, as for every other judgment of a heading.
fn amendment_text_addenda(
    text: &str,
    raw: &str,
    #[cfg(test)] view: Option<&SourceSegments>,
) -> Option<Vec<Addendum>> {
    let lines: Vec<&str> = text.lines().collect();
    let start = amendment_extraction_start(&lines)?;
    let end = lines[start + 1..]
        .iter()
        .position(|line| line.trim_start().starts_with('□'))
        .map_or(lines.len(), |offset| start + 1 + offset);
    let joined = lines[start..end].join("\n");
    // The same lines of the decoded amendment view, joined as above.
    #[cfg(test)]
    if let Some(view) = view {
        let offsets: Vec<usize> = view
            .text
            .split_inclusive('\n')
            .scan(0, |offset, line| {
                let previous = *offset;
                *offset += line.len();
                Some(previous)
            })
            .collect();
        let to = if end < offsets.len() {
            offsets[end]
        } else {
            view.text.len()
        };
        if let Some(reconstructed) = offsets.get(start).and_then(|from| {
            view.slice(*from, to)
                .and_then(|part| part.lines_join())
                .ok()
        }) {
            observe(
                "amendment_extraction",
                "amendment_text_addenda:joined_names",
                false,
                vec![("decoded", &joined, reconstructed)],
            );
        }
    }
    // The decoding changes no line feed, so the same lines of the source text are its written text.
    let raw_lines: Vec<&str> = raw.lines().collect();
    let raw_joined = if raw_lines.len() == lines.len() {
        raw_lines[start..end].join("\n")
    } else {
        joined.clone()
    };
    names_effective_date(&joined).then(|| {
        vec![Addendum {
            label: String::new(),
            text: joined,
            raw: raw_joined,
        }]
    })
}

/// The line where the extraction of an amendment text starts: the first line that `heading_line`
/// takes as a clean heading or as a heading that "개정" follows.
fn amendment_extraction_start(lines: &[&str]) -> Option<usize> {
    lines.iter().position(|line| {
        heading_line(line)
            .is_some_and(|found| found.clean || compact_text(found.rest).starts_with("개정"))
    })
}

/// Tells whether the addendum parts of an amendment text hold a sentence that the gate counts
/// (user-holistic-design-20261112). A part is every run of lines from a line that is a heading of
/// any kind (`heading_line`, also of two lines "부", "칙", and also behind a list mark such as
/// "□", "○", "-", "1." or "가.") to the next line that starts with "□". Only the gate reads
/// the marks; r24 reads none of them.
///
/// The extraction of r24 reads the first clean heading, or the first heading that "개정"
/// follows, of the part, and only for an ordinance whose addenda and body name no effective date
/// (`extracted`). Those lines are the only lines that r24 scanned, and the added detection counts
/// there. Every other line is a text that r24 never scanned, and the whole detection counts
/// there: the other parts of an ordinance, an ordinance whose extraction was not used, and every
/// amendment text of an administrative rule or a law.
///
/// The parts share one gate partition (`amendment_part_gate`, `amendment_text`): B and N each cut
/// their own parts from their whole view, and a key that B holds in any part is no new key
/// (r25c-normalization-plan.v3.md 2). A part that r24 scanned counts the new keys; a part that r24
/// never scanned counts every key of its B part and the reader criterion, and the new keys of the
/// partition count for every part. The new keys also count when N cuts a part that B does not cut.
fn amendment_parts_any(
    text: &str,
    raw: &str,
    extracted: bool,
    source: &SourceView,
    consumed: &[SourceRange],
    approval: bool,
    errors: &mut Vec<String>,
) -> (bool, bool) {
    let mut parts = amendment_parts(text, raw, extracted);
    let evidence = amendment_evidence(text, raw, extracted, &source.amendment, &parts, consumed);
    let mut found = (false, false);
    for (index, part) in parts.iter_mut().enumerate() {
        #[cfg(test)]
        if source.enabled {
            let reconstructed = source.amendment.decoded().and_then(|view| {
                let (from, to, mark) = part.view_range;
                view.amendment_part(from, to, mark)
            });
            match reconstructed {
                Ok(view) => {
                    if view.text == part.text {
                        eprintln!(
                            "{}",
                            json!({"phase":"sourceview", "representation":"B", "family":"amendment-part", "view":"decoded", "after_heading":false, "bytes_hex":view.text.as_bytes().iter().map(|b| format!("{b:02x}")).collect::<String>(), "byte_length":view.text.len(), "segments":view.segments})
                        );
                    } else {
                        eprintln!(
                            "{}",
                            json!({"phase":"sourceview", "complete":false, "error":"amendment part byte mismatch"})
                        );
                    }
                    // The trace takes the reconstruction whether it matches or not; the
                    // comparison of the two traces decides.
                    #[cfg(test)]
                    {
                        let written = if part.aligned {
                            amendment_part_written(
                                &source.amendment,
                                part.line_range,
                                part.view_range.2,
                            )
                        } else {
                            Ok(view.clone())
                        };
                        if let Ok(written) = written {
                            let site = if part.scanned {
                                "amendment_parts_any:scanned_added_any"
                            } else {
                                "amendment_parts_any:unscanned_gate_only_any"
                            };
                            observe(
                                "amendment_part",
                                site,
                                false,
                                vec![
                                    ("decoded", &part.text, view.clone()),
                                    ("raw", &part.raw, written),
                                ],
                            );
                        }
                    }
                    part.source_view = Some(view);
                }
                Err(error) => eprintln!(
                    "{}",
                    json!({"phase":"sourceview", "complete":false, "error":error.to_string()})
                ),
            }
        }
        let (rule, default) = if part.scanned {
            let added = added_any(&part.text, &part.raw, &evidence);
            (added || (approval && evidence.parts[index].unread), added)
        } else {
            gate_only_any(&part.text, &part.raw, &evidence, index)
        };
        found = (found.0 || rule, found.1 || default);
    }
    let added = evidence.added();
    errors.extend(evidence.errors.iter().cloned());
    (found.0 || added, found.1 || added)
}

/// A run of lines of an amendment text. `scanned` tells that r24 scanned the lines.
struct AmendmentPart {
    text: String,
    raw: String,
    scanned: bool,
    view_range: (usize, usize, usize),
    #[cfg(test)]
    source_view: Option<SourceSegments>,
    /// The first line and the end line of the part, and whether `raw` took the same lines of the
    /// written text (otherwise `raw` is a copy of `text`).
    line_range: (usize, usize),
    aligned: bool,
}

/// The runs of lines of an amendment text that `amendment_parts_any` counts.
fn amendment_parts(text: &str, raw: &str, extracted: bool) -> Vec<AmendmentPart> {
    let lines: Vec<&str> = text.lines().collect();
    let raw_lines: Vec<&str> = raw.lines().collect();
    let aligned = raw_lines.len() == lines.len();
    let taken = if extracted {
        amendment_extraction_start(&lines)
    } else {
        None
    };
    let mut offsets = Vec::with_capacity(lines.len());
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        offsets.push(offset);
        offset += line.len();
    }
    // The heading of a line, behind its list mark: the length of the mark, or nothing when the
    // line is no heading.
    let heading_at = |index: usize| -> Option<usize> {
        let line = lines[index];
        let mark = list_mark_len(line);
        let rest = &line[mark..];
        if !rest.starts_with('부') {
            return None;
        }
        let found = if mark == 0 {
            heading_line(&text[offsets[index]..])
        } else {
            heading_line(rest)
        };
        found.map(|_| mark)
    };
    // The lines from `from` to `to`, the first line behind its mark.
    let join = |from: usize, to: usize, mark: usize, scanned: bool| -> AmendmentPart {
        let cut = |lines: &[&str]| {
            let mut copy: Vec<&str> = lines[from..to].to_vec();
            if mark > 0 && !copy.is_empty() {
                copy[0] = &copy[0][mark..];
            }
            copy.join("\n")
        };
        let region = cut(&lines);
        let raw_region = if aligned {
            cut(&raw_lines)
        } else {
            region.clone()
        };
        AmendmentPart {
            text: region,
            raw: raw_region,
            scanned,
            view_range: (
                offsets[from],
                if to < offsets.len() {
                    offsets[to]
                } else {
                    text.len()
                },
                mark,
            ),
            #[cfg(test)]
            source_view: None,
            line_range: (from, to),
            aligned,
        }
    };
    let mut parts = Vec::new();
    let mut part: Option<(usize, usize)> = None;
    for index in 0..=lines.len() {
        let line = lines.get(index).copied().unwrap_or("");
        let ends = index == lines.len() || line.trim_start().starts_with('□');
        if ends && let Some((start, mark)) = part.take() {
            let readable = taken
                .filter(|at| start <= *at && *at < index)
                .map(|at| at.max(start));
            let unread_to = readable.unwrap_or(index);
            if unread_to > start {
                parts.push(join(start, unread_to, mark, false));
            }
            if let Some(at) = readable {
                parts.push(join(at, index, if at == start { mark } else { 0 }, true));
            }
        }
        if index < lines.len()
            && part.is_none()
            && let Some(mark) = heading_at(index)
        {
            part = Some((index, mark));
        }
    }
    parts
}

/// The byte length of the list mark at the start of a line: "□", "○", "-", "·", a number with a
/// full stop or a bracket, a Hangul letter with a full stop, and the like, with the white space
/// around it. The mark of no kind has length zero.
fn list_mark_len(line: &str) -> usize {
    static MARK: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"^\s*(?:[□■○●◇◆△▲◎※·ㆍ•*\-]|\d{1,3}\s*[.)]|[(（]\d{1,3}[)）]|[가-힣]\s*[.)]|[\x{2460}-\x{2473}])\s*",
        )
        .expect("valid list mark pattern")
    });
    MARK.find(line).map_or(0, |found| found.end())
}

/// User decision that approved the addendum interpretation rules (2026-11-03).
pub const ADDENDUM_RULES_DECISION: &str = "user-approval-addendum-rules-20261103";

/// User decision that a label whose date and number both differ belongs to another amendment.
pub const LABEL_BOTH_DIFFER_DECISION: &str = "user-label-both-differ-unresolved-20261103";

/// User decision that a revision which a later-promulgated repeal takes away before its effective
/// date stays unresolved (enforcement basis only).
const REPEAL_REVIVAL_DECISION: &str = "user-repeal-revival-unresolved-20261117";

/// User decision that a repeal-and-enact revision with a repeal sentence only is a repeal.
pub const REPEAL_ONLY_DECISION: &str = "user-repeal-and-enact-repeal-only-is-repeal-20261103";

/// User decision that a repeal-and-enact revision (폐지제정) is handled as an enactment.
pub const REPEAL_AND_ENACT_DECISION: &str = "user-repeal-and-enact-as-enactment-20261103";

/// User decision that a label date on or after the next revision's promulgation date is unresolved.
pub const LABEL_AFTER_NEXT_DECISION: &str = "user-label-not-on-or-after-next-revision-20261103";

/// Marks evidence as the outcome of one approved addendum rule.
fn approved_rule(evidence: &mut serde_json::Map<String, Value>, rule: &str) {
    evidence.insert("user_decision".to_owned(), json!(ADDENDUM_RULES_DECISION));
    evidence.insert("approved_rule".to_owned(), json!(rule));
}

/// User decision that a two-digit label year and a label number without "제" are read.
pub const LABEL_FORMAT_DECISION: &str = "user-two-digit-year-and-bare-number-20261103";

/// User decision: a proviso counts only when it follows the effective-date sentence.
pub const PROVISO_SCOPE_DECISION: &str = "user-proviso-scope-enforcement-sentence-20261103";

/// Lead ruling: a proviso after a paragraph mark counts as attached to the effective sentence.
pub const PARAGRAPH_MARK_RULING: &str = "proviso-paragraph-mark-lead-20261103";

/// Lead ruling: an unreadable label matters only in the addendum section of this revision.
pub const LABEL_SCOPE_RULING: &str = "f4-scope-attributed-section-20261103";

/// Lead ruling: a two-digit label year takes the century nearer to the promulgation date.
pub const CENTURY_RULING: &str = "label-two-digit-year-century-nearest-promulgation";

/// User decision (2026-11-04): a text that has a proviso-omitted mark is read as having no proviso.
pub const PROVISO_OMITTED_DECISION: &str = "user-proviso-omitted-mark-ignored-20261104";
/// User decision: a label whose date and number both differ keeps the explicit date.
pub const BOTH_DIFFER_EXPLICIT_DECISION: &str = "user-both-differ-explicit-date-20261104";
/// User decision: a label that matches on one side only keeps the explicit date.
pub const ONE_SIDE_EXPLICIT_DECISION: &str = "user-one-side-match-explicit-20261104";
/// User decision: "공포한 날 시행한다" (without "부터") is the day of promulgation.
pub const WITHOUT_BUTEO_DECISION: &str = "user-promulgation-day-without-buteo-20261104";
pub const EXPLICIT_WITHOUT_BUTEO_DECISION: &str = "user-explicit-date-without-buteo-20261104";
/// User decision: a last addendum section without a label and with "공포한 날부터" is the day of promulgation.
pub const LAST_UNLABELED_DECISION: &str = "user-last-section-unlabeled-promulgation-20261104";
/// Lead ruling: with no attribution, the last addendum section is the target section.
pub const NO_ATTRIBUTION_LAST_RULING: &str = "no-attribution-last-20261104";
/// Lead ruling: the attribution rule applies to enactments (제정, 전부개정) too.
pub const ATTRIBUTION_ENACTMENT_RULING: &str = "attribution-for-enactment-20261104";
/// Lead ruling: addendum text inside the articles is split into sections like an addendum.
pub const BODY_SECTIONS_RULING: &str = "body-sections-20261104";
/// Lead ruling: "시행 한다" (with a space) is the approved verb.
pub const SPACED_VERB_RULING: &str = "spaced-effective-verb-20261104";

/// Adds an id to one of the id lists of the evidence (`user_decisions`, `lead_rulings`).
/// The lists keep every rule that applied, apart from the rule that chose the date.
fn add_id(evidence: &mut serde_json::Map<String, Value>, list: &str, id: &str) {
    let entry = evidence.entry(list.to_owned()).or_insert_with(|| json!([]));
    if let Some(items) = entry.as_array_mut()
        && !items.iter().any(|item| item == id)
    {
        items.push(json!(id));
    }
}

/// Adds a lead ruling to the evidence. The rulings are kept as a list of ids.
fn add_lead_ruling(evidence: &mut serde_json::Map<String, Value>, ruling: &str) {
    let entry = evidence
        .entry("lead_ruling".to_owned())
        .or_insert_with(|| json!([]));
    if let Some(list) = entry.as_array_mut()
        && !list.iter().any(|item| item == ruling)
    {
        list.push(json!(ruling));
    }
}

/// Prefix of the revision key of a source whose serial is empty. The key is this prefix and the
/// source path, the same internal key the default renderer uses for such a source.
pub const PATH_REVISION_PREFIX: &str = "path:";

/// Reason of an occurrence whose source dates were valid and needed no rule.
const VALID_SOURCE_DATE: &str = "valid source enforcement date";

/// What the gate needs to know about a revision. `resolve_rules` computes it from the source
/// texts; the gate reads it after the processing of the plan that gives each revision its state.
/// Only the repeal-before-effective rule of an enforcement plan (`leave_revived_unresolved`) runs
/// after the gate; it changes a confirmed or an estimated occurrence to unresolved, as the gate does.
#[derive(Clone, Copy, Default)]
struct GateMark {
    /// The date comes from a rule that reads text or from the default period, not from the valid
    /// date of the source and not from an exempt individual approval. An approval with a gate
    /// policy is text derived: only the exempt approvals (`GatePolicy::Exempt`) stand outside.
    text_derived: bool,
    /// The default period or the arrival estimate gave the date.
    by_default: bool,
    /// A text of the revision holds a sentence that holds back a date that a rule read.
    any_rule: bool,
    /// A text of the revision holds a sentence that holds back a date of the default period.
    any_default: bool,
    /// A gate partition has a source provenance error. Both marks are set then; this flag only
    /// names the reason of the withheld date.
    provenance_error: bool,
    /// The revision of the approval with a gate policy that selected the date, if any.
    approval: Option<&'static str>,
    /// The section of the approved sentence keeps the date from being confirmed.
    approval_blocked: bool,
    /// The copies of the revision do not all carry the same approval (set by the plan).
    approval_mismatch: bool,
    /// The mismatch of the copies involves an LLM selection (set by the plan, compiler#2).
    llm_mismatch: bool,
    /// The date path of the occurrence (r25c-design.md 4: P0 to P10), when it has a date. The
    /// selection of an approval with a gate policy compares it with the registered condition.
    path: Option<&'static str>,
}

impl Occurrence {
    /// The gate (user-r25c-structural-fix-20261112, user-holistic-design-20261112). A date that a
    /// rule read from text or that the default period gave is withheld when the revision holds a
    /// sentence that only the added detection finds. The gate changes a confirmed or an estimated
    /// occurrence and nothing else, so an unresolved result of any rule, a copy conflict and a cap
    /// by the next revision keep their evidence byte for byte. The new state follows the other
    /// unresolved paths: "unresolved" with a promulgation date, "undated" without one.
    /// Returns whether it changed the occurrence.
    fn apply_gate(&mut self) -> bool {
        if !self.gate_applies() {
            return false;
        }
        self.withhold();
        true
    }

    /// Tells whether the gate withholds this occurrence by its own marks.
    fn gate_applies(&self) -> bool {
        self.gate.text_derived
            && if self.gate.by_default {
                self.gate.any_default
            } else {
                self.gate.any_rule
            }
            && matches!(self.status.as_str(), "confirmed" | "estimated")
    }

    /// Makes a confirmed or an estimated occurrence unresolved (undated without a promulgation
    /// date) and records why. An occurrence in another state stays as it is.
    fn withhold(&mut self) {
        if !matches!(self.status.as_str(), "confirmed" | "estimated") {
            return;
        }
        let llm = self.gate.approval == Some(LLM_APPROVAL);
        let withheld = if self.gate.provenance_error {
            "source-provenance-error"
        } else if self.gate.approval_mismatch {
            if self.gate.llm_mismatch {
                "llm-copy-mismatch"
            } else {
                "approved-copy-mismatch"
            }
        } else if self.gate.approval_blocked {
            if llm {
                "llm-sentence-blocked"
            } else {
                "approved-sentence-blocked"
            }
        } else {
            "added-detection-sentence"
        };
        self.leave_unresolved(
            "effective date is unresolved; the effective-date sentence names no approved form",
            withheld,
        );
    }

    /// Takes the date away from an occurrence: "unresolved" with a promulgation date, "undated"
    /// without one. `reason` becomes the reason and the rule of the evidence, and `withheld` the
    /// value of `date_withheld`; `arrival_estimate` becomes false. Every other key of the evidence
    /// stays. The gate (`withhold`) and
    /// the repeal-before-effective rule (`leave_revived_unresolved`) share this state change.
    fn leave_unresolved(&mut self, reason: &str, withheld: &str) {
        let publication = self.promulgation_date.is_some();
        self.status = if publication { "unresolved" } else { "undated" }.to_owned();
        self.enforcement_date = None;
        self.exposure_date = if publication {
            self.promulgation_date.clone()
        } else {
            None
        };
        self.undated_semantics = if publication {
            Vec::new()
        } else {
            vec!["공포일자".to_owned(), "시행일자".to_owned()]
        };
        self.reason = reason.to_owned();
        if let Some(evidence) = self.date_evidence.as_object_mut() {
            evidence.insert("rule".to_owned(), json!(self.reason));
            evidence.insert("arrival_estimate".to_owned(), json!(false));
            evidence.insert("date_withheld".to_owned(), json!(withheld));
        }
    }

    /// True when the source dates were valid and no rule changed them, so a renderer that
    /// writes the dates as they are gives the same body as the default build.
    pub fn has_plain_source_dates(&self) -> bool {
        self.status == "confirmed"
            && self.reason == VALID_SOURCE_DATE
            && self.promulgation_date.is_some()
            && self.enforcement_date.is_some()
    }
}

/// A source's resolved occurrence, exact-copy hash and optional rendered-body hash.
pub struct Prepared {
    occurrence: Result<Occurrence>,
    /// The occurrence with the gate applied, when the gate changes it. A renderer reads the date
    /// before the plan has processed every source, and the plan applies the gate to the same
    /// occurrence after its own processing, so the renderer must see the gated date.
    gated: Option<Occurrence>,
    copy_id: String,
    markdown_hash: Option<String>,
}

impl Prepared {
    /// Resolves one source. A resolution error is kept until the plan reads it in order.
    pub fn resolve(dataset: &str, source: &SourceRevision) -> Self {
        Self::resolve_with(dataset, source, None)
    }

    /// Resolves one source with the usable judgments of an LLM ledger as the second approval
    /// source (compiler#2). `None` gives exactly `resolve`.
    pub fn resolve_with(
        dataset: &str,
        source: &SourceRevision,
        llm: Option<&crate::llm_ledger::LlmLedger>,
    ) -> Self {
        let occurrence = resolve_source(dataset, source, llm);
        let gated = occurrence.as_ref().ok().and_then(|found| {
            let mut view = found.clone();
            view.apply_gate().then_some(view)
        });
        Self {
            occurrence,
            gated,
            copy_id: sha256(&source.xml),
            markdown_hash: None,
        }
    }

    /// The resolved occurrence a renderer needs, when resolution succeeded.
    pub fn occurrence(&self) -> Option<&Occurrence> {
        self.gated.as_ref().or(self.occurrence.as_ref().ok())
    }

    /// Records the hash of the rendered Markdown the plan will index.
    pub fn with_markdown_hash(mut self, markdown: &[u8]) -> Self {
        self.markdown_hash = Some(sha256(markdown));
        self
    }
}

/// Resolves one source and names its file when resolution fails. The gate is not applied: it
/// waits for the processing of the whole plan.
fn resolve_source(
    dataset: &str,
    source: &SourceRevision,
    llm: Option<&crate::llm_ledger::LlmLedger>,
) -> Result<Occurrence> {
    resolve_rules_with(dataset, source, true, llm)
        .with_context(|| format!("failed to resolve source dates {}", source.source_path))
}

/// The rules and the gate of one source, without the processing of a plan. Tests use it.
#[cfg(test)]
fn resolve(dataset: &str, source: &SourceRevision) -> Result<Occurrence> {
    let mut occurrence = resolve_rules(dataset, source)?;
    occurrence.apply_gate();
    Ok(occurrence)
}

/// Resolves only valid metadata and approved source rules.
/// The B trace of one source. Two independent records meet here: every entry of a traced
/// function records the arguments it actually received (`baseline`), and every call site queues
/// the SourceView reconstruction of the call it is about to make (`pending`). An entry takes the
/// oldest queued reconstruction (`sourceview`). An entry without one, or a reconstruction that
/// no entry takes, stays visible as `unobserved` or `orphan`, so a call site without a
/// reconstruction cannot pass silently.
#[cfg(test)]
#[derive(Default)]
struct BodyTrace {
    case_id: String,
    sha: String,
    source: String,
    baseline: Vec<Value>,
    sourceview: Vec<Value>,
    pieces: Vec<Value>,
    pending: std::collections::VecDeque<PendingCall>,
    depth: usize,
    calls: usize,
}

/// The reconstruction that a call site queues for its next traced call.
#[cfg(test)]
struct PendingCall {
    family: &'static str,
    site: &'static str,
    after_heading: bool,
    /// Per view: the view name, the text that the call site passes, the reconstruction.
    views: Vec<(&'static str, String, SourceSegments)>,
}

#[cfg(test)]
thread_local! {
    static BODY_TRACE: std::cell::RefCell<Option<BodyTrace>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn hex_bytes(text: &str) -> String {
    text.as_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

/// Queues the reconstruction of the next traced call of a call site.
#[cfg(test)]
fn observe(
    family: &'static str,
    site: &'static str,
    after_heading: bool,
    views: Vec<(&'static str, &str, SourceSegments)>,
) {
    BODY_TRACE.with(|slot| {
        if let Some(trace) = slot.borrow_mut().as_mut() {
            trace.pending.push_back(PendingCall {
                family,
                site,
                after_heading,
                views: views
                    .into_iter()
                    .map(|(view, actual, reconstructed)| (view, actual.to_owned(), reconstructed))
                    .collect(),
            });
        }
    });
}

/// Keeps the nesting depth of traced functions. Only the outermost entry records a call: a
/// traced function that another traced function calls receives the same partition.
#[cfg(test)]
struct EntryGuard(bool);

#[cfg(test)]
impl Drop for EntryGuard {
    fn drop(&mut self) {
        if self.0 {
            BODY_TRACE.with(|slot| {
                if let Some(trace) = slot.borrow_mut().as_mut() {
                    trace.depth -= 1;
                }
            });
        }
    }
}

#[cfg(test)]
fn trace_row(
    trace: &BodyTrace,
    phase: &str,
    family: &str,
    site: &str,
    function: &str,
    call_index: usize,
    sequence: usize,
    view: &str,
    after_heading: bool,
    text: &str,
) -> Value {
    json!({
        "case_id": trace.case_id, "xml_sha256": trace.sha, "source": trace.source,
        "phase": phase, "representation": "B", "call_family": family, "call_site": site,
        "function": function, "call_index": call_index, "sequence": sequence, "view": view,
        "after_heading": after_heading, "byte_length": text.len(), "bytes_hex": hex_bytes(text)
    })
}

/// Records one entry of a traced function with the arguments it received, and pairs it with the
/// oldest queued reconstruction.
#[cfg(test)]
fn trace_entry(
    function: &'static str,
    after_heading: bool,
    args: &[(&'static str, &str)],
) -> EntryGuard {
    BODY_TRACE.with(|slot| {
        let mut borrowed = slot.borrow_mut();
        let Some(trace) = borrowed.as_mut() else {
            return EntryGuard(false);
        };
        trace.depth += 1;
        if trace.depth > 1 {
            return EntryGuard(true);
        }
        let call_index = trace.calls;
        trace.calls += 1;
        let pending = trace.pending.pop_front();
        let unobserved = format!("unobserved:{function}");
        let (family, site) = pending
            .as_ref()
            .map_or((unobserved.as_str(), "none"), |p| (p.family, p.site));
        for (index, (view, actual)) in args.iter().enumerate() {
            let sequence = trace.baseline.len();
            let mut row = trace_row(
                trace,
                "baseline",
                family,
                site,
                function,
                call_index,
                sequence,
                view,
                after_heading,
                actual,
            );
            let matches = pending
                .as_ref()
                .and_then(|p| p.views.get(index))
                .is_some_and(|(name, claimed, _)| name == view && claimed == actual);
            row["site_actual_matches"] = json!(matches);
            trace.baseline.push(row);
        }
        if let Some(pending) = pending {
            for (view, _, reconstructed) in &pending.views {
                let sequence = trace.sourceview.len();
                let mut row = trace_row(
                    trace,
                    "sourceview",
                    pending.family,
                    pending.site,
                    function,
                    call_index,
                    sequence,
                    view,
                    pending.after_heading,
                    &reconstructed.text,
                );
                row["segments"] = json!(reconstructed.segments);
                trace.sourceview.push(row);
            }
        }
        EntryGuard(true)
    })
}

/// Writes every queued reconstruction that no entry took as an orphan row.
#[cfg(test)]
fn flush_orphans(trace: &mut BodyTrace) {
    while let Some(pending) = trace.pending.pop_front() {
        let call_index = trace.calls;
        trace.calls += 1;
        let family = format!("orphan:{}", pending.family);
        for (view, _, reconstructed) in &pending.views {
            let sequence = trace.sourceview.len();
            let mut row = trace_row(
                trace,
                "sourceview",
                &family,
                pending.site,
                "none",
                call_index,
                sequence,
                view,
                pending.after_heading,
                &reconstructed.text,
            );
            row["segments"] = json!(reconstructed.segments);
            trace.sourceview.push(row);
        }
    }
}

#[cfg(test)]
thread_local! {
    /// The fragments that `fragment_verbs` checked, for the comparison with the matcher reference.
    static FRAGMENT_LOG: std::cell::RefCell<Option<Vec<String>>> = const { std::cell::RefCell::new(None) };
    /// Adds a provenance error to every gate partition (fault injection of the tests).
    static FORCE_PROVENANCE_ERROR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The old and the new gate marks and the evidence of every partition of each resolve.
    static GATE_REPORT: std::cell::RefCell<Option<Vec<Value>>> = const { std::cell::RefCell::new(None) };
}

/// Runs a function with the B trace taken out, so its calls stay out of the trace.
#[cfg(test)]
fn untraced<T>(run: impl FnOnce() -> T) -> T {
    let saved = BODY_TRACE.with(|slot| slot.borrow_mut().take());
    let result = run();
    BODY_TRACE.with(|slot| *slot.borrow_mut() = saved);
    result
}

/// The count comparison of the amendment parts before the evidence keys, for comparison only.
#[cfg(test)]
fn legacy_amendment_parts_any(text: &str, raw: &str, extracted: bool) -> (bool, bool) {
    let mut found = (false, false);
    for part in amendment_parts(text, raw, extracted) {
        let (rule, default) = if part.scanned {
            let added = legacy_added_any(&part.text, &part.raw);
            (added, added)
        } else {
            legacy_gate_only_any(&part.text, &part.raw)
        };
        found = (found.0 || rule, found.1 || default);
    }
    found
}

#[cfg(test)]
fn key_json(key: &EvidenceKey) -> Value {
    let ranges = |list: &[SourceRange]| -> Value {
        json!(
            list.iter()
                .map(|(e, p, s, t)| json!([e, p, s, t]))
                .collect::<Vec<_>>()
        )
    };
    match key {
        EvidenceKey::Subject { subject, verb } => {
            json!({"kind":"subject", "subject":ranges(subject), "verb":ranges(verb)})
        }
        EvidenceKey::Extended { subject, verb } => {
            json!({"kind":"extended", "subject":ranges(subject), "verb":ranges(verb)})
        }
        EvidenceKey::Heading { candidate, verb } => {
            json!({"kind":"heading", "candidate":[candidate.0, candidate.1, candidate.2], "verb":ranges(verb)})
        }
        EvidenceKey::Phrase { phrase } => json!({"kind":"phrase", "phrase":ranges(phrase)}),
    }
}

/// The keys of E - R - C, of the PHRASE N - B and of P_E - P_R - C of one partition, with the text
/// that showed them, and the consumed keys C.
#[cfg(test)]
fn partition_summary(evidence: &PartitionEvidence) -> Value {
    let list = |items: Vec<(&EvidenceKey, &Value)>| -> Vec<Value> {
        items
            .into_iter()
            .map(|(key, found)| json!({"key":key_json(key), "found":found}))
            .collect()
    };
    let new_keys = list(
        evidence
            .all
            .iter()
            .filter(|(key, _)| !evidence.r.contains(*key) && !evidence.consumed(key))
            .collect(),
    );
    let phrase_n_minus_b = list(
        evidence
            .phrase_n
            .iter()
            .filter(|(key, _)| !evidence.phrase_b.contains_key(*key))
            .collect(),
    );
    let phrase_new = list(
        evidence
            .phrase_b
            .iter()
            .chain(evidence.phrase_n.iter())
            .filter(|(key, _)| !evidence.phrase_r.contains(*key) && !evidence.consumed(key))
            .collect::<BTreeMap<_, _>>()
            .into_iter()
            .collect(),
    );
    let consumed = list(
        evidence
            .all
            .iter()
            .chain(evidence.phrase_b.iter())
            .chain(evidence.phrase_n.iter())
            .filter(|(key, _)| evidence.consumed(key))
            .collect::<BTreeMap<_, _>>()
            .into_iter()
            .collect(),
    );
    json!({"family":evidence.family, "container":evidence.container, "added":evidence.added(),
        "counts":{"R":evidence.r.len(), "E":evidence.all.len(), "phrase_B":evidence.phrase_b.len(), "phrase_N":evidence.phrase_n.len(), "phrase_R":evidence.phrase_r.len()},
        "E_minus_R":new_keys, "phrase_N_minus_B":phrase_n_minus_b, "phrase_new":phrase_new, "consumed":consumed, "errors":evidence.errors,
        "consumed_synthetic":evidence.consumed_synthetic,
        "full_keys":{"E":evidence.all.iter().map(|(key, found)| json!([key_json(key), found])).collect::<Vec<_>>(),
            "R":evidence.r.iter().map(key_json).collect::<Vec<_>>(),
            "P_B":evidence.phrase_b.iter().map(|(key, found)| json!([key_json(key), found])).collect::<Vec<_>>(),
            "P_N":evidence.phrase_n.iter().map(|(key, found)| json!([key_json(key), found])).collect::<Vec<_>>(),
            "P_R":evidence.phrase_r.iter().map(key_json).collect::<Vec<_>>(),
            "parts":evidence.parts.iter().map(|part| json!({"keys":part.keys.iter().map(key_json).collect::<Vec<_>>(),
                "phrases":part.phrases.iter().map(key_json).collect::<Vec<_>>(), "unread":part.unread})).collect::<Vec<_>>()}})
}

/// Computes the old count marks, the marks of the three PHRASE variants and the evidence of every
/// gate partition without short circuits, out of the trace, when a test asked for the report.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn record_gate_comparison(
    view: &SourceView,
    body: (&String, &String),
    blocks: &[(String, String)],
    split: &[(String, String)],
    fields: [(&String, &String); 2],
    amendment: (&String, &String),
    extracted: bool,
    marks: (bool, bool),
    provenance: &[String],
    consumed: &[SourceRange],
    blocked: bool,
) {
    if GATE_REPORT.with(|slot| slot.borrow().is_none()) {
        return;
    }
    let report = untraced(|| {
        let old_scanned = legacy_added_any(body.0, body.1)
            || blocks.iter().any(|(text, raw)| legacy_added_any(text, raw))
            || legacy_split_any(split, None);
        let mut old = (old_scanned, old_scanned);
        for (text, raw) in fields {
            if !text.trim().is_empty() {
                let (rule, default) = legacy_gate_only_any(text, raw);
                old = (old.0 || rule, old.1 || default);
            }
        }
        let (rule, default) = legacy_amendment_parts_any(amendment.0, amendment.1, extracted);
        old = (old.0 || rule, old.1 || default);
        let scanned_parts: Vec<PartitionEvidence> = std::iter::once(container_evidence(
            "body_gate",
            "body".to_owned(),
            &view.body,
            body.0,
            body.1,
            consumed,
        ))
        .chain(
            blocks
                .iter()
                .enumerate()
                .map(|(index, (text, raw))| block_evidence(view, index, text, raw, consumed)),
        )
        .collect();
        let mut gate_fields = Vec::new();
        for (index, (text, raw)) in fields.into_iter().enumerate() {
            if !text.trim().is_empty() {
                let (family, container, field) = if index == 0 {
                    ("outside_gate", "outside", &view.outside)
                } else {
                    ("declaration_gate", "declaration", &view.declaration)
                };
                gate_fields.push(container_evidence(
                    family,
                    container.to_owned(),
                    field,
                    text,
                    raw,
                    consumed,
                ));
            }
        }
        let parts = amendment_parts(amendment.0, amendment.1, extracted);
        let amendment_evidence = amendment_evidence(
            amendment.0,
            amendment.1,
            extracted,
            &view.amendment,
            &parts,
            consumed,
        );
        let variant = |variant: PhraseVariant| -> Value {
            let approval = !consumed.is_empty();
            let mut found = (false, false);
            for evidence in &scanned_parts {
                let (rule, default) = evidence.scanned_marks(0, variant, approval);
                found = (found.0 || rule, found.1 || default);
            }
            for evidence in &gate_fields {
                let (rule, default) = evidence.unscanned_marks(0, variant);
                found = (found.0 || rule, found.1 || default);
            }
            for (index, part) in parts.iter().enumerate() {
                let (rule, default) = if part.scanned {
                    amendment_evidence.scanned_marks(index, variant, approval)
                } else {
                    amendment_evidence.unscanned_marks(index, variant)
                };
                found = (found.0 || rule, found.1 || default);
            }
            let added = amendment_evidence.added_with(variant);
            found = (found.0 || added, found.1 || added);
            if blocked {
                found = (true, true);
            }
            json!([found.0, found.1])
        };
        let partitions: Vec<Value> = scanned_parts
            .iter()
            .chain(gate_fields.iter())
            .chain(std::iter::once(&amendment_evidence))
            .map(partition_summary)
            .collect();
        json!({"old":[old.0, old.1], "new":[marks.0, marks.1], "variants":{"no_phrase":variant(PhraseVariant::None), "phrase_new_default":variant(PhraseVariant::New), "phrase_all_default":variant(PhraseVariant::All)},
            "provenance_errors":provenance, "extraction_used":extracted, "approval_blocked":blocked, "consumed_spans":consumed.len(), "partitions":partitions})
    });
    GATE_REPORT.with(|slot| {
        if let Some(list) = slot.borrow_mut().as_mut() {
            list.push(report);
        }
    });
}

/// The decoded body of a SourceView, for the call sites that read the body.
#[cfg(test)]
fn body_view(view: Option<&SourceView>) -> Option<SourceSegments> {
    view.filter(|v| v.enabled)
        .map(|v| v.body.decoded().expect("decode SourceView body"))
}

fn observed_body_names(body: &str, view: Option<&SourceView>, site: &'static str) -> bool {
    #[cfg(test)]
    if let Some(reconstructed) = body_view(view) {
        observe(
            "body_names",
            site,
            false,
            vec![("decoded", body, reconstructed)],
        );
    }
    #[cfg(not(test))]
    let _ = (view, site);
    names_effective_date(body)
}

#[cfg(test)]
fn resolve_rules(dataset: &str, source: &SourceRevision) -> Result<Occurrence> {
    resolve_rules_with(dataset, source, true, None)
}

/// The rule chain without any approval, as it stands before the gate. The shared selection of an
/// approval with a gate policy asks it first (r25c-prototype-delta-20261116.md 8, B3). It computes
/// no gate marks: only the status, the date and the date path of the result are read.
fn rule_chain(dataset: &str, source: &SourceRevision) -> Result<Occurrence> {
    #[cfg(test)]
    return untraced(|| resolve_rules_with(dataset, source, false, None));
    #[cfg(not(test))]
    resolve_rules_with(dataset, source, false, None)
}

/// The evidence `approval_not_applied` when the chain result of a revision that matches the
/// conditions of an approval differs from the registered result, and nothing when it is equal.
fn approval_not_applied(
    dataset: &str,
    source: &SourceRevision,
    approval: &Approval,
) -> Result<Option<Value>> {
    let Some(expected) = approval.chain else {
        return Ok(None);
    };
    let chain = rule_chain(dataset, source)?;
    let same = chain.status == expected.status
        && chain.enforcement_date.as_deref() == Some(expected.date)
        && chain.gate.path == Some(expected.path);
    Ok((!same).then(|| {
        json!({"reason": "approved-sentence-blocked", "decision": approval.source,
            "observed": {"status": chain.status, "date": chain.enforcement_date, "path": chain.gate.path},
            "expected": {"status": expected.status, "date": expected.date, "path": expected.path}})
    }))
}

/// Resolves the dates of one source. `approvals` false skips the approvals with a gate policy and
/// the gate marks: the rule chain alone (see `rule_chain`).
#[cfg_attr(not(test), allow(clippy::unused_enumerate_index))]
fn resolve_rules_with(
    dataset: &str,
    source: &SourceRevision,
    approvals: bool,
    llm: Option<&crate::llm_ledger::LlmLedger>,
) -> Result<Occurrence> {
    let publication = compact_date(&source.promulgation);
    let DateText {
        source_view,
        mut addenda,
        body,
        amendment: amendment_text,
        outside,
        declaration,
        split,
    } = date_text_with(
        &source.xml,
        source.copy_conflict.is_none()
            && (compact_date(&source.enforcement).is_none()
                || approval_entry(dataset, source).is_some()),
    )?;
    #[cfg(not(test))]
    let _ = split;
    // The entity markup that CDATA text carries is decoded once, here: every later judgment (the
    // section split, the detection, the extraction of an amendment text) reads the same text.
    // The text as the source wrote it stays beside it; only the gate reads it
    // (user-r25-dual-text-detection-20261112).
    let body_raw = body;
    let body = decode_markup(&body_raw);
    let amendment_raw = amendment_text;
    let amendment_text = decode_markup(&amendment_raw);
    let outside_raw = outside;
    let outside = decode_markup(&outside_raw);
    let declaration_raw = declaration;
    let declaration = decode_markup(&declaration_raw);
    for block in &mut addenda {
        block.raw = std::mem::take(&mut block.text);
        block.text = decode_markup(&block.raw);
    }
    #[cfg(test)]
    let mut trace_addenda = source_view.as_ref().filter(|v| v.enabled).and_then(|v| {
        v.addenda
            .iter()
            .map(|part| part.decoded().ok())
            .collect::<Option<Vec<_>>>()
    });
    let mut effective = compact_date(&source.enforcement);
    // A valid source date and an exempt individual approval (`GatePolicy::Exempt`) are no result
    // of a rule that reads text. Every other date comes from a rule that reads the body or the
    // addenda, from the default period, or from an approval with a gate policy. The gate
    // (`Occurrence::apply_gate`) checks all of them in one place, after every later processing of
    // the plan, and reads only what is fixed here.
    let mut text_derived = effective.is_none();
    // The date path of the result (r25c-design.md 4); each branch that dates the revision sets it.
    let mut path: Option<&'static str> = effective.map(|_| "P0");
    let mut status = "confirmed";
    let mut reason = VALID_SOURCE_DATE.to_owned();
    let mut extra = serde_json::Map::new();
    // A source without a serial is keyed by its path, as the default renderer keys it.
    if source.revision.starts_with(PATH_REVISION_PREFIX) {
        extra.insert("revision_key".to_owned(), json!("source-path"));
    }
    // Copies of one revision that differ in content or dates are never given a date.
    let conflicted = source.copy_conflict.is_some();
    if conflicted {
        effective = None;
        path = None;
        extra.insert("decision".to_owned(), json!("duplicate-copy-conflict"));
        extra.insert("copy_conflict".to_owned(), json!(source.copy_conflict));
    }
    // The gate input is fixed here from the source texts, before any rule replaces `addenda`:
    // the body and every addendum block of the source (r24 scanned them), and the text that no
    // rule reads (paragraphs, items, subitems and the amendment statement). Each text is one gate
    // partition of the SourceView, and the gate compares the evidence keys of B and N
    // (r25c-normalization-plan.v3.md 2-3). An event line feed that splits a subject shows as a new
    // key of the N view of its container, so no separate count of event runs remains.
    let collected = source_view
        .as_ref()
        .context("the SourceView is always collected")?;
    // An approval with a gate policy (r25c-design.md 8) is selected before the gate reads the
    // partitions, so that the gate leaves out only the evidence of the approved sentence (C). The
    // entry of MST 1063983 is checked before a valid source date; the other entries register the
    // sentinel source date, so a valid source date keeps its priority for them. A copy conflict
    // comes first.
    // The selection applies the approval only when the rule chain without any approval gives the
    // result that the registration expects (r25c-prototype-delta-20261116.md 8, B3). Otherwise
    // nothing is selected: the chain result goes to the ordinary gate as it stands, and the
    // evidence `approval_not_applied` records the difference.
    let mut selection = if conflicted || !approvals {
        None
    } else {
        gated_approval(
            dataset,
            source,
            collected,
            &addenda,
            &amendment_text,
            &declaration,
        )
    };
    if let Some(chosen) = &selection
        && let Some(difference) = approval_not_applied(dataset, source, chosen.approval)?
    {
        extra.insert("approval_not_applied".to_owned(), difference);
        selection = None;
    }
    // The second approval source (compiler#2, lead-c2-integration-contract-20261117): a usable
    // LLM judgment of this exact copy. The registry decides first: a revision that the registry
    // names, exempt entries included, never takes an LLM date, and a ledger that names one stops
    // the build (a conflict of the two sources needs a new decision).
    let mut llm_selection: Option<LlmSelection> = None;
    if let Some(ledger) = llm
        && approvals
        && !conflicted
    {
        if registered_revision(dataset, &source.revision) {
            ensure!(
                !ledger.names_revision(&source.revision),
                "the LLM ledger names revision {} that the approval registry names",
                source.revision
            );
        } else if selection.is_none() {
            match llm_approval(
                dataset,
                source,
                collected,
                &addenda,
                [
                    ("body", body.as_str()),
                    ("outside", outside.as_str()),
                    ("amendment", amendment_text.as_str()),
                    ("declaration", declaration.as_str()),
                ],
                publication,
                ledger,
            )? {
                Some(Ok(chosen)) => llm_selection = Some(chosen),
                Some(Err(not_applied)) => {
                    extra.insert("llm_not_applied".to_owned(), not_applied);
                }
                None => {}
            }
        }
    }
    let chosen = selection.is_some() || llm_selection.is_some();
    let consumed: Vec<SourceRange> = match (&selection, &llm_selection) {
        (Some(selection), _) => selection.spans.clone(),
        (None, Some(selection)) => selection.spans.clone(),
        (None, None) => Vec::new(),
    };
    if let Some(selection) = &selection {
        effective = Some(selection.date);
        path = Some(APPROVAL_PATH);
        text_derived = true;
        reason = selection.reason();
        selection.record(source, &mut extra);
    }
    if let Some(selection) = &llm_selection {
        effective = Some(selection.date);
        path = Some(LLM_PATH);
        text_derived = true;
        reason = selection.reason.clone();
        extra.insert("llm_judgment".to_owned(), selection.evidence.clone());
    }
    let mut marks = (false, false);
    let mut extraction_used = false;
    // The chain run of `approvals` false needs no marks and writes no diagnostics.
    let gated = text_derived && !conflicted && approvals;
    let provenance = std::cell::RefCell::new(Vec::<String>::new());
    let approval_unread = std::cell::Cell::new(false);
    #[cfg(test)]
    let legacy_blocks: Vec<(String, String)> = addenda
        .iter()
        .map(|block| (block.text.clone(), block.raw.clone()))
        .collect();
    if gated {
        // The trace queues each reconstruction right before its call, so the short circuits of
        // the chain below decide which calls happen, as before.
        #[cfg(test)]
        let view = source_view.as_ref().filter(|v| v.enabled);
        let body_any = || {
            #[cfg(test)]
            if let Some(view) = view {
                observe(
                    "body_gate",
                    "resolve_rules:body_added_any",
                    false,
                    vec![
                        (
                            "decoded",
                            body.as_str(),
                            view.body.decoded().expect("decode body"),
                        ),
                        ("raw", body_raw.as_str(), view.body.clone()),
                    ],
                );
            }
            let evidence = container_evidence(
                "body_gate",
                "body".to_owned(),
                &collected.body,
                &body,
                &body_raw,
                &consumed,
            );
            provenance
                .borrow_mut()
                .extend(evidence.errors.iter().cloned());
            approval_unread.set(
                approval_unread.get() || evidence.scanned_marks(0, PhraseVariant::All, chosen).0,
            );
            added_any(&body, &body_raw, &evidence)
        };
        let blocks_any = || {
            addenda.iter().enumerate().any(|(index, block)| {
                #[cfg(test)]
                if let Some(view) = view {
                    let written = &view.addenda[index];
                    observe(
                        "block_gate",
                        "resolve_rules:block_added_any",
                        false,
                        vec![
                            (
                                "decoded",
                                block.text.as_str(),
                                written.decoded().expect("decode addendum"),
                            ),
                            ("raw", block.raw.as_str(), written.clone()),
                        ],
                    );
                }
                let evidence = block_evidence(collected, index, &block.text, &block.raw, &consumed);
                provenance
                    .borrow_mut()
                    .extend(evidence.errors.iter().cloned());
                approval_unread.set(
                    approval_unread.get()
                        || evidence.scanned_marks(0, PhraseVariant::All, chosen).0,
                );
                added_any(&block.text, &block.raw, &evidence)
            })
        };
        let scanned = body_any() || blocks_any();
        marks = (scanned || approval_unread.get(), scanned);
        for (index, (text, raw)) in [(&outside, &outside_raw), (&declaration, &declaration_raw)]
            .into_iter()
            .enumerate()
        {
            if !text.trim().is_empty() {
                #[cfg(test)]
                if let Some(view) = view {
                    let (family, site, field) = if index == 0 {
                        (
                            "outside_gate",
                            "resolve_rules:outside_gate_only_any",
                            &view.outside,
                        )
                    } else {
                        (
                            "declaration_gate",
                            "resolve_rules:declaration_gate_only_any",
                            &view.declaration,
                        )
                    };
                    observe(
                        family,
                        site,
                        false,
                        vec![
                            (
                                "decoded",
                                text.as_str(),
                                field.decoded().expect("decode gate field"),
                            ),
                            ("raw", raw.as_str(), field.clone()),
                        ],
                    );
                }
                let (family, container, field) = if index == 0 {
                    ("outside_gate", "outside", &collected.outside)
                } else {
                    ("declaration_gate", "declaration", &collected.declaration)
                };
                let evidence =
                    container_evidence(family, container.to_owned(), field, text, raw, &consumed);
                provenance
                    .borrow_mut()
                    .extend(evidence.errors.iter().cloned());
                let (rule, default) = gate_only_any(text, raw, &evidence, 0);
                marks = (marks.0 || rule, marks.1 || default);
            }
        }
    }
    let mut by_default = false;
    // The amendment text carries the addendum when neither the addenda nor the body name an
    // effective date. Addenda that hold only other provisions do not stop the search.
    let source_empty = body.trim().is_empty()
        && amendment_text.trim().is_empty()
        && addenda.iter().all(|block| block.text.trim().is_empty());
    if dataset == "ordinances"
        && !conflicted
        && effective.is_none()
        && !addenda.iter().enumerate().any(|(_index, block)| {
            #[cfg(test)]
            if let Some(views) = &trace_addenda {
                observe(
                    "block_names",
                    "resolve_rules:block_names",
                    false,
                    vec![("decoded", block.text.as_str(), views[_index].clone())],
                );
            }
            names_effective_date(&block.text)
        })
        && !observed_body_names(
            &body,
            source_view.as_ref(),
            "resolve_rules:extraction_body_names",
        )
        && let Some(found) = amendment_text_addenda(
            &amendment_text,
            &amendment_raw,
            #[cfg(test)]
            source_view
                .as_ref()
                .filter(|v| v.enabled)
                .map(|v| v.amendment.decoded().expect("decode amendment"))
                .as_ref(),
        )
    {
        // The extracted block replaces the addenda; its reconstruction replaces their views.
        #[cfg(test)]
        if let (Some(view), Some(views)) = (
            source_view.as_ref().filter(|v| v.enabled),
            trace_addenda.as_mut(),
        ) {
            let decoded = view.amendment.decoded().expect("decode amendment");
            let lines: Vec<&str> = decoded.text.lines().collect();
            let start = amendment_extraction_start(&lines).expect("extraction start");
            let end = lines[start + 1..]
                .iter()
                .position(|line| line.trim_start().starts_with('□'))
                .map_or(lines.len(), |offset| start + 1 + offset);
            let offsets: Vec<usize> = decoded
                .text
                .split_inclusive('\n')
                .scan(0, |offset, line| {
                    let previous = *offset;
                    *offset += line.len();
                    Some(previous)
                })
                .collect();
            let reconstructed = decoded
                .slice(
                    offsets[start],
                    if end < offsets.len() {
                        offsets[end]
                    } else {
                        decoded.text.len()
                    },
                )
                .expect("extraction slice")
                .lines_join()
                .expect("extraction join");
            *views = vec![reconstructed];
        }
        addenda = found;
        extraction_used = true;
        extra.insert("addendum_source".to_owned(), json!("제개정이유내용"));
    }
    // Every addendum part of the amendment text counts, whether a rule took it or not.
    if gated {
        let mut errors = Vec::new();
        let (rule, default) = amendment_parts_any(
            &amendment_text,
            &amendment_raw,
            extraction_used,
            collected,
            &consumed,
            chosen,
            &mut errors,
        );
        provenance.borrow_mut().extend(errors);
        marks = (marks.0 || rule, marks.1 || default);
    }
    // A proviso after the approved sentence or another read date in its section keeps the
    // selected date from being confirmed (r25c-design.md 8); the date is held, not replaced.
    let approval_blocked = gated
        && (selection
            .as_ref()
            .is_some_and(|selection| !selection.blocking.is_empty())
            || llm_selection
                .as_ref()
                .is_some_and(|selection| !selection.blocking.is_empty()));
    if approval_blocked {
        marks = (true, true);
    }
    let provenance = provenance.into_inner();
    #[cfg(test)]
    if collected.enabled && !provenance.is_empty() {
        eprintln!(
            "{}",
            json!({"phase":"gate", "source_path":source.source_path, "provenance_errors":provenance})
        );
    }
    #[cfg(test)]
    if gated {
        record_gate_comparison(
            collected,
            (&body, &body_raw),
            &legacy_blocks,
            &split,
            [(&outside, &outside_raw), (&declaration, &declaration_raw)],
            (&amendment_text, &amendment_raw),
            extraction_used,
            marks,
            &provenance,
            &consumed,
            approval_blocked,
        );
    }
    if dataset == "ordinances"
        && !conflicted
        && effective.is_none()
        && let Some((approval, date)) = approved_date(source, &addenda)
    {
        effective = Some(date);
        path = Some("P1");
        text_derived = false;
        reason = "individually approved revision-specific decision".to_owned();
        extra.insert("approval_source".to_owned(), json!(approval.source));
        extra.insert("approved_sentence".to_owned(), json!(approval.sentence));
    }
    let mut blocked = false;
    let mut guard = false;
    // Set when a rule would have resolved this revision but its amendment kind is excluded.
    let mut excluded_hit = false;
    // The target addendum section decides first where the older rules read it badly. When it
    // decides nothing, the older rules below run unchanged.
    // Only the test build passes the two trace views; the product build keeps the call of four
    // arguments. The closure keeps the call inside the condition chain, so it runs only when the
    // conditions before it hold.
    #[cfg(test)]
    let trace_body = source_view
        .as_ref()
        .filter(|v| v.enabled)
        .and_then(|v| v.body.decoded().ok());
    #[cfg(test)]
    let decide = |addenda: &[Addendum], body: &str, date: Date| {
        pre_decide(
            source,
            addenda,
            body,
            date,
            trace_body.as_ref(),
            trace_addenda.as_deref(),
        )
    };
    #[cfg(not(test))]
    let decide =
        |addenda: &[Addendum], body: &str, date: Date| pre_decide(source, addenda, body, date);
    if dataset == "ordinances"
        && !conflicted
        && effective.is_none()
        && let Some(date) = publication
        && let Some(verdict) = decide(&addenda, &body, date)
    {
        match verdict {
            PreVerdict::Confirmed(found, evidence, why) => {
                effective = Some(found);
                path = Some("P2");
                reason = why.to_owned();
                extra.extend(evidence);
            }
            PreVerdict::Unresolved(evidence, why) => {
                blocked = true;
                status = "unresolved";
                reason = why;
                extra.extend(evidence);
            }
            PreVerdict::Guard => guard = true,
        }
    }
    // An addendum heading with a label that cannot be read is not an addendum without a label.
    // It matters only for the addendum section of this revision, which is the last section of
    // the accumulated addenda (lead ruling). When the attribution picks the last section and
    // its label is read, unreadable sections before it are ignored and are no candidates. In
    // every other case the sections cannot tell which one belongs to this revision, so the
    // revision stays unresolved.
    if dataset == "ordinances"
        && !conflicted
        && !blocked
        && effective.is_none()
        && publication.is_some()
    {
        let sections: Vec<Section> = addenda
            .iter()
            .flat_map(|block| split_sections(&block.text, publication))
            .collect();
        let unread = sections
            .iter()
            .filter(|section| section.unreadable.is_some())
            .count();
        if unread > 0 {
            let attributed = attributed_section(source, &sections).map(|found| found.0);
            if attributed == Some(sections.len() - 1) {
                extra.insert("ignored_unreadable_sections".to_owned(), json!(unread));
                add_lead_ruling(&mut extra, LABEL_SCOPE_RULING);
            } else {
                let heading = sections
                    .iter()
                    .find_map(|section| section.unreadable.clone())
                    .unwrap_or_default();
                blocked = true;
                status = "unresolved";
                reason = "effective date is unresolved; lead-default-unreadable-label: an addendum heading has a label that cannot be read as a date and a number, and the section of this revision is not the last readable one (conservative default, not a user decision)".to_owned();
                extra.insert(
                    "lead_default".to_owned(),
                    json!("lead-default-unreadable-label"),
                );
                extra.insert("user_decision".to_owned(), json!(false));
                extra.insert("unreadable_label".to_owned(), json!(heading));
                add_lead_ruling(&mut extra, LABEL_SCOPE_RULING);
            }
        }
    }
    if effective.is_none()
        && !conflicted
        && !blocked
        && dataset == "ordinances"
        && matches!(source.category.as_str(), "조례" | "규칙")
        && let Some(date) = publication
        && let Some(outcome) = single_label_priority(
            source,
            &addenda,
            date,
            #[cfg(test)]
            trace_addenda.as_deref(),
        )
    {
        match outcome {
            LabelOutcome::Applied(..) if !addendum_rule_amendment(&source.amendment) => {
                excluded_hit = true;
            }
            LabelOutcome::NumberOnlyMismatch(_) if !addendum_rule_amendment(&source.amendment) => {}
            LabelOutcome::Contradiction(evidence) => {
                blocked = true;
                status = "unresolved";
                reason = format!(
                    "effective date is unresolved; {ADDENDUM_RULES_DECISION} (label-contradiction): the only addendum label matches this revision by date or number but differs by the other"
                );
                extra.extend(evidence);
            }
            LabelOutcome::BothDiffer(evidence) => {
                blocked = true;
                status = "unresolved";
                reason = format!(
                    "effective date is unresolved; {LABEL_BOTH_DIFFER_DECISION}: the only addendum label differs from this revision in both date and number"
                );
                extra.extend(evidence);
            }
            LabelOutcome::Applied(found, evidence) => {
                effective = Some(found);
                path = Some("P3");
                reason = "single addendum label date takes priority over the promulgation date"
                    .to_owned();
                extra.extend(evidence);
            }
            LabelOutcome::NumberOnlyMismatch(evidence) => {
                blocked = true;
                status = "unresolved";
                reason = format!(
                    "effective date is unresolved; {ADDENDUM_RULES_DECISION} (number-only-mismatch): the only addendum label gives a number that differs from this revision"
                );
                extra.extend(evidence);
            }
        }
    }
    if effective.is_none() && !blocked && !conflicted {
        let usable: Vec<_> = addenda
            .iter()
            .filter(|block| !block.text.trim().is_empty())
            .collect();
        let selected: Vec<_> = usable
            .iter()
            .filter(|block| compact_date(&block.label) == publication && publication.is_some())
            .collect();
        let selected_text = if selected.len() == 1 {
            Some(selected[0].text.as_str())
        } else if (usable.len() == 1
            && usable[0].label.is_empty()
            && matches!(source.amendment.as_str(), "제정" | "전부개정" | "폐지제정"))
            || (usable.len() > 1
                && usable.iter().all(|block| {
                    block.label.is_empty() && block.text.trim() == usable[0].text.trim()
                }))
        {
            Some(usable[0].text.as_str())
        } else {
            None
        };
        if let Some(text) = selected_text {
            #[cfg(test)]
            if let Some(views) = &trace_addenda {
                let index = addenda
                    .iter()
                    .position(|block| std::ptr::eq(block.text.as_str(), text))
                    .expect("selected addendum");
                observe(
                    "block_clause",
                    "resolve_rules:selected_block_clause_date",
                    false,
                    vec![("decoded", text, views[index].clone())],
                );
            }
            effective = clause_date(text, publication);
            path = effective.map(|_| "P4");
            reason = "approved clause in selected addendum".to_owned();
        } else if usable.is_empty()
            && observed_body_names(
                &body,
                source_view.as_ref(),
                "resolve_rules:clause_branch_body_names",
            )
            && !excluded_body_kind(dataset, source, publication)
        {
            #[cfg(test)]
            if let Some(reconstructed) = body_view(source_view.as_ref()) {
                observe(
                    "body_clause",
                    "resolve_rules:body_clause_date",
                    false,
                    vec![("decoded", body.as_str(), reconstructed)],
                );
            }
            effective = clause_date(&body, publication);
            path = effective.map(|_| "P5");
            reason = "approved clause in source body".to_owned();
        } else if is_sentinel(&source.enforcement)
            && usable.is_empty()
            && !observed_body_names(
                &body,
                source_view.as_ref(),
                "resolve_rules:sentinel_branch_body_names",
            )
            && let Some(date) = publication
        {
            if dataset == "ordinances"
                && matches!(source.category.as_str(), "조례" | "규칙" | "교육규칙")
            {
                if source_empty {
                    // No body, addendum or amendment text: the source may have lost them.
                    extra.insert("empty_source".to_owned(), json!(true));
                } else {
                    effective = date.checked_add(Duration::days(21));
                    path = effective.map(|_| "P6");
                    by_default = true;
                    reason = "accepted publication plus 21 days; no holiday deferral".to_owned();
                }
            } else if matches!(source.category.as_str(), "고시" | "공고") {
                effective = date.checked_add(Duration::days(6));
                path = effective.map(|_| "P7");
                by_default = true;
                reason = "accepted notice plus 6 days; no holiday deferral".to_owned();
            } else if source.category == "훈령" || source.category.ends_with("예규") {
                effective = Some(date);
                path = Some("P8");
                status = "estimated";
                by_default = true;
                reason = "accepted arrival estimate from publication or issue date".to_owned();
            }
        }
        if effective.is_none()
            && dataset == "ordinances"
            && matches!(source.category.as_str(), "조례" | "규칙")
            && (matches!(source.amendment.as_str(), "일부개정" | "폐지")
                || !addendum_rule_amendment(&source.amendment))
            && let Some(date) = publication
        {
            // The same sections as a flat map of `split_sections` over the addenda, in order; the
            // test build gives each section the SourceView of its body.
            let mut sections: Vec<Section> = Vec::new();
            #[cfg_attr(not(test), allow(clippy::unused_enumerate_index))]
            for (_index, block) in addenda.iter().enumerate() {
                #[cfg_attr(not(test), allow(unused_mut))]
                let mut next = split_sections(&block.text, Some(date));
                #[cfg(test)]
                if let Some(views) = &trace_addenda {
                    attach_section_views(&mut next, Some(&views[_index]), Some(date));
                }
                sections.extend(next);
            }
            if let Some((_, section, matched)) = attributed_section(source, &sections)
                && let Some(timing) = {
                    #[cfg(test)]
                    if let Some(view) = &section.source_view {
                        observe(
                            "attributed_plain",
                            "resolve_rules:attributed_plain",
                            true,
                            vec![("decoded", section.body.as_str(), view.clone())],
                        );
                    }
                    plain_promulgation_clause(&section.body, &source.category)
                }
            {
                if !addendum_rule_amendment(&source.amendment) {
                    excluded_hit = true;
                } else {
                    effective = Some(date);
                    path = Some("P9");
                    reason =
                        "attributed addendum sets effect on the day of promulgation".to_owned();
                    extra.insert(
                        "decision".to_owned(),
                        json!("ordinance-attributed-promulgation"),
                    );
                    extra.insert("attributed_label".to_owned(), json!(section.label.trim()));
                    extra.insert("attributed_by".to_owned(), json!(matched));
                    add_label_format_notes(&mut extra, &section.label, Some(date));
                    extra.insert("separate_application_timing".to_owned(), json!(timing));
                }
            }
        }
        if effective.is_none() {
            status = if publication.is_some() {
                "unresolved"
            } else {
                "undated"
            };
            reason =
                "effective date is unresolved; no approved source rule selects a date".to_owned();
            let proviso = addenda
                .iter()
                .any(|block| proviso_after_effective_sentence(&block.text))
                || proviso_after_effective_sentence(&body);
            if proviso {
                extra.insert("proviso_after_effective_sentence".to_owned(), json!(true));
                if addenda
                    .iter()
                    .any(|block| proviso_across_paragraph_mark(&block.text))
                    || proviso_across_paragraph_mark(&body)
                {
                    add_lead_ruling(&mut extra, PARAGRAPH_MARK_RULING);
                }
                extra
                    .entry("user_decision".to_owned())
                    .or_insert_with(|| json!(PROVISO_SCOPE_DECISION));
            }
            if extra.contains_key("empty_source") {
                reason = "effective date is unresolved; the source has no body, addendum or amendment text, so no default period is applied".to_owned();
            }
            if excluded_hit {
                reason = format!(
                    "effective date is unresolved; {ADDENDUM_RULES_DECISION} (amendment-by-other-excluded): the addendum of an amendment made by another or bulk ordinance may belong to the amending ordinance"
                );
                approved_rule(&mut extra, "amendment-by-other-excluded");
                extra.insert("excluded_amendment".to_owned(), json!(source.amendment));
            }
        }
    }
    // A repeal-and-enact revision (폐지제정) is an enactment under the user decision, except
    // one whose only article is a repeal sentence, which is a repeal (user decision).
    if dataset == "ordinances" && source.amendment == "폐지제정" {
        if repeal_sentence_only(&body) {
            extra.insert("amendment_treated_as".to_owned(), json!("폐지"));
            extra.insert(
                "amendment_user_decision".to_owned(),
                json!(REPEAL_ONLY_DECISION),
            );
        } else {
            extra.insert("amendment_treated_as".to_owned(), json!("제정"));
            extra.insert(
                "amendment_user_decision".to_owned(),
                json!(REPEAL_AND_ENACT_DECISION),
            );
        }
    }
    // An unread sentence in the target section: a date taken from another section is withheld.
    if guard && status == "confirmed" {
        effective = None;
        path = None;
        status = "unresolved";
        reason = "effective date is unresolved; the effective-date sentence names no approved form"
            .to_owned();
        extra.insert(
            "date_withheld".to_owned(),
            json!("unread-sentence-in-target-section"),
        );
    }
    if conflicted {
        status = if publication.is_some() {
            "unresolved"
        } else {
            "undated"
        };
        reason = format!(
            "effective date is unresolved; copies of this revision differ: {}",
            source.copy_conflict.as_deref().unwrap_or("")
        );
    }
    // Semantics for which this occurrence has no usable date; absent keys mean "none".
    let undated_semantics: Vec<String> = if status == "undated" {
        vec!["공포일자".to_owned(), "시행일자".to_owned()]
    } else if publication.is_none() {
        vec!["공포일자".to_owned()]
    } else {
        Vec::new()
    };
    let mut evidence = json!({"raw_promulgation_date": source.promulgation, "raw_enforcement_date": source.enforcement,
        "source_path": source.source_path, "rule": reason, "arrival_estimate": status == "estimated"});
    if let Some(object) = evidence.as_object_mut() {
        object.extend(extra);
    }
    Ok(Occurrence {
        source_path: source.source_path.clone(),
        raw_promulgation_date: source.promulgation.clone(),
        raw_enforcement_date: source.enforcement.clone(),
        promulgation_date: publication.map(iso),
        enforcement_date: effective.map(iso),
        status: status.to_owned(),
        exposure_date: if status == "unresolved" {
            publication.map(iso)
        } else {
            None
        },
        date_evidence: evidence,
        observed_metadata: json!({}),
        reason,
        promulgation_rank: None,
        enforcement_rank: None,
        undated_semantics,
        gate: GateMark {
            text_derived,
            by_default,
            any_rule: marks.0,
            any_default: marks.1,
            provenance_error: !provenance.is_empty(),
            approval: selection
                .as_ref()
                .map(|selection| selection.approval.revision)
                .or(llm_selection.as_ref().map(|_| LLM_APPROVAL)),
            approval_blocked,
            approval_mismatch: false,
            llm_mismatch: false,
            path,
        },
    })
}

impl DatePlan {
    /// Records a source copy that was left out of the index because it could not be parsed.
    pub fn record_unparsable(&mut self, file: &str, error: &str) {
        self.unparsable.push(json!({"file": file, "error": error}));
    }

    /// Preserves exact source copies and assigns ranks to each occurrence.
    pub fn new(dataset: &str, enforcement: bool, sources: Vec<SourceRevision>) -> Result<Self> {
        Self::new_with(dataset, enforcement, sources, None)
    }

    /// `new` with the usable judgments of an LLM ledger as the second approval source
    /// (compiler#2). `None` gives exactly `new`.
    pub fn new_with(
        dataset: &str,
        enforcement: bool,
        sources: Vec<SourceRevision>,
        llm: Option<&crate::llm_ledger::LlmLedger>,
    ) -> Result<Self> {
        // Resolving one source never depends on another, so all sources resolve in parallel.
        let prepared = parallel_map(&sources, |source| {
            Prepared::resolve_with(dataset, source, llm)
        });
        Self::assemble(
            dataset,
            enforcement,
            sources.into_iter().zip(prepared).map(Ok),
            true,
            true,
            llm,
        )
    }

    /// Preserves sources as they arrive, so a lazy source list never sits in memory whole.
    ///
    /// With `keep_xml` false each exact XML copy is dropped right after its dates resolve.
    pub fn from_sources(
        dataset: &str,
        enforcement: bool,
        sources: impl IntoIterator<Item = Result<SourceRevision>>,
        keep_xml: bool,
    ) -> Result<Self> {
        Self::from_sources_with(dataset, enforcement, sources, keep_xml, None)
    }

    /// `from_sources` with the usable judgments of an LLM ledger as the second approval source
    /// (compiler#2). `None` gives exactly `from_sources`.
    pub fn from_sources_with(
        dataset: &str,
        enforcement: bool,
        sources: impl IntoIterator<Item = Result<SourceRevision>>,
        keep_xml: bool,
        llm: Option<&crate::llm_ledger::LlmLedger>,
    ) -> Result<Self> {
        Self::assemble(
            dataset,
            enforcement,
            sources.into_iter().map(|source| {
                let source = source?;
                let prepared = Prepared::resolve_with(dataset, &source, llm);
                Ok((source, prepared))
            }),
            keep_xml,
            true,
            llm,
        )
    }

    /// Builds the plan from sources that were resolved and rendered as they were read.
    ///
    /// The exact XML is dropped at once. The caller leaves `markdown` empty for a source whose
    /// rendered body it knows the build will commit unchanged, so only its hash stays and
    /// memory does not grow with the corpus. Any other source keeps its text.
    pub fn from_prepared(
        dataset: &str,
        enforcement: bool,
        items: impl IntoIterator<Item = Result<(SourceRevision, Prepared)>>,
    ) -> Result<Self> {
        Self::assemble(dataset, enforcement, items, false, true, None)
    }

    /// Builds the plan from sources paired with their resolved occurrence.
    fn assemble(
        dataset: &str,
        enforcement: bool,
        sources: impl IntoIterator<Item = Result<(SourceRevision, Prepared)>>,
        keep_xml: bool,
        keep_markdown: bool,
        llm: Option<&crate::llm_ledger::LlmLedger>,
    ) -> Result<Self> {
        let mut plan = Self {
            dataset: dataset.to_owned(),
            basis: if enforcement {
                "enforcement"
            } else {
                "promulgation"
            }
            .to_owned(),
            revisions: Vec::new(),
            sources: Vec::new(),
            source_revision: Vec::new(),
            source_occurrence: Vec::new(),
            snapshots: Vec::new(),
            bodies: BTreeMap::new(),
            markdown_hashes: Vec::new(),
            unparsable: Vec::new(),
            llm: llm.map(|ledger| {
                json!({"ledger": format!("ledger/{}.json", ledger.dataset), "sha256": ledger.sha256,
                    "policy": ledger.policy, "protected_list_sha256": ledger.protected_list_sha256,
                    "usable_judgments": ledger.len(), "decision": "lead-c2-integration-contract-20261117"})
            }),
        };
        let mut copies = BTreeMap::new();
        for item in sources {
            let (mut source, prepared) = item?;
            let Prepared {
                occurrence,
                gated: _,
                copy_id,
                markdown_hash,
            } = prepared;
            ensure!(
                !source.identity.is_empty() && !source.revision.is_empty(),
                "source identity or revision is empty: {}",
                source.source_path
            );
            let occurrence = occurrence?;
            let key = (
                source.identity.clone(),
                source.revision.clone(),
                copy_id.clone(),
            );
            let index = *copies.entry(key).or_insert_with(|| {
                plan.revisions.push(Revision {
                    identity: source.identity.clone(),
                    revision: source.revision.clone(),
                    copy_id,
                    name: source.name.clone(),
                    category: source.category.clone(),
                    canonical_path: source.canonical_path.clone(),
                    promulgation_date: occurrence.promulgation_date.clone(),
                    enforcement_date: occurrence.enforcement_date.clone(),
                    status: Some(occurrence.status.clone()),
                    repeal: source.repeal,
                    reason: occurrence.reason.clone(),
                    date_evidence: occurrence.date_evidence.clone(),
                    occurrences: Vec::new(),
                    body: Value::Null,
                });
                plan.revisions.len() - 1
            });
            plan.source_revision.push(index);
            plan.source_occurrence
                .push(plan.revisions[index].occurrences.len());
            plan.revisions[index].occurrences.push(occurrence);
            if !keep_xml {
                source.xml = Vec::new();
            }
            if !keep_markdown {
                source.markdown = Vec::new();
            }
            plan.markdown_hashes.push(markdown_hash.unwrap_or_default());
            plan.sources.push(source);
        }
        plan.defer_late_label_dates();
        if plan.llm.is_some() {
            plan.check_llm_next_revisions();
        }
        plan.apply_gate();
        if enforcement {
            plan.leave_revived_unresolved();
        }
        if plan.llm.is_some() {
            plan.mark_llm_dates();
        }
        for enforcement in [false, true] {
            for (rank, source) in plan.ordered(enforcement).into_iter().enumerate() {
                ensure!(
                    rank as u64 <= 9_007_199_254_740_991,
                    "date rank exceeds the contract range"
                );
                let occurrence = &mut plan.revisions[plan.source_revision[source]].occurrences
                    [plan.source_occurrence[source]];
                if enforcement {
                    occurrence.enforcement_rank = Some(rank as u64);
                } else {
                    occurrence.promulgation_rank = Some(rank as u64);
                }
            }
        }
        plan.validate_occurrences()?;
        Ok(plan)
    }

    /// Leaves unresolved a revision whose label date is on or after the promulgation date of
    /// the next revision of the same identity (user decision, 2026-11-03).
    ///
    /// Revisions of one identity are ordered by promulgation date, then numeric serial, then
    /// revision text. Copies of one revision are one revision. The replay of events must not
    /// put an older revision's label date after a newer revision.
    fn defer_late_label_dates(&mut self) {
        let key = |plan: &Self, source: usize| -> Option<(String, u64, String)> {
            let occurrence = plan.occurrence(source);
            Some((
                occurrence.promulgation_date.clone()?,
                plan.sources[source].serial,
                plan.sources[source].revision.clone(),
            ))
        };
        let mut by_identity: BTreeMap<String, Vec<(String, u64, String)>> = BTreeMap::new();
        for source in 0..self.sources.len() {
            if let Some(key) = key(self, source) {
                by_identity
                    .entry(self.sources[source].identity.clone())
                    .or_default()
                    .push(key);
            }
        }
        for keys in by_identity.values_mut() {
            keys.sort();
            keys.dedup();
        }
        let mut late = Vec::new();
        for source in 0..self.sources.len() {
            let occurrence = self.occurrence(source);
            let evidence = &occurrence.date_evidence;
            if occurrence.status != "confirmed"
                || evidence["decision"] != "single-label-date-priority-20261103"
                || evidence["applied_side"] != "label"
            {
                continue;
            }
            let (Some(own), Some(label)) = (key(self, source), evidence["label_date"].as_str())
            else {
                continue;
            };
            let next = by_identity[&self.sources[source].identity]
                .iter()
                .find(|other| **other > own);
            if let Some(next) = next
                && label >= next.0.as_str()
            {
                late.push((source, label.to_owned(), next.clone()));
            }
        }
        for (source, label, next) in late {
            let revision = self.source_revision[source];
            let occurrence =
                &mut self.revisions[revision].occurrences[self.source_occurrence[source]];
            let mut evidence = json!({
                "raw_promulgation_date": occurrence.raw_promulgation_date,
                "raw_enforcement_date": occurrence.raw_enforcement_date,
                "source_path": occurrence.source_path,
                "arrival_estimate": false,
                "decision": "label-date-after-next-revision",
                "user_decision": LABEL_AFTER_NEXT_DECISION,
                "attributed_label": occurrence.date_evidence["attributed_label"],
                "label_date": label,
                "next_revision": next.2,
                "next_promulgation_date": next.0,
                "next_order_key": ["promulgation_date", "serial", "revision"],
            });
            occurrence.status = "unresolved".to_owned();
            occurrence.enforcement_date = None;
            occurrence.exposure_date = occurrence.promulgation_date.clone();
            occurrence.reason = format!(
                "effective date is unresolved; {LABEL_AFTER_NEXT_DECISION}: the label date {label} is on or after the promulgation date {} of the next revision {}",
                next.0, next.2
            );
            evidence["rule"] = json!(occurrence.reason);
            // The rules that applied stay on record next to the cap that replaced the date.
            for list in ["user_decisions", "lead_rulings"] {
                if let Some(items) = occurrence.date_evidence.get(list) {
                    evidence[list] = items.clone();
                }
            }
            occurrence.date_evidence = evidence;
            let first = self.revisions[revision].occurrences[0].clone();
            let target = &mut self.revisions[revision];
            target.status = Some(first.status);
            target.enforcement_date = first.enforcement_date;
            target.reason = first.reason;
            target.date_evidence = first.date_evidence;
        }
    }

    /// Checks each date that an LLM judgment selected against the next revision the plan holds
    /// (design-v3-integration.md v3.1, v3.2 N-b and N-c, v3.4 4). The judge recorded the earliest
    /// later promulgation date it knew; a next revision that came later, or a next revision of the
    /// same promulgation date in the order key of `defer_late_label_dates`, takes the date away.
    /// When the recorded and the actual next revision agree, a section label dated on or after it
    /// takes the date away for a promulgation-day or label-date judgment
    /// (`user-label-not-on-or-after-next-revision-20261103`; an explicit date keeps priority over
    /// labels). It runs after the cap of `defer_late_label_dates` and before the gate.
    fn check_llm_next_revisions(&mut self) {
        let key = |plan: &Self, source: usize| -> Option<(String, u64, String)> {
            Some((
                plan.occurrence(source).promulgation_date.clone()?,
                plan.sources[source].serial,
                plan.sources[source].revision.clone(),
            ))
        };
        let mut by_identity: BTreeMap<String, Vec<(String, u64, String)>> = BTreeMap::new();
        for source in 0..self.sources.len() {
            if let Some(key) = key(self, source) {
                by_identity
                    .entry(self.sources[source].identity.clone())
                    .or_default()
                    .push(key);
            }
        }
        for keys in by_identity.values_mut() {
            keys.sort();
            keys.dedup();
        }
        let mut withheld = Vec::new();
        for source in 0..self.sources.len() {
            let occurrence = self.occurrence(source);
            let judgment = &occurrence.date_evidence["llm_judgment"];
            if occurrence.status != "confirmed" || judgment.is_null() {
                continue;
            }
            let Some(own) = key(self, source) else {
                continue;
            };
            let keys = &by_identity[&self.sources[source].identity];
            let next = keys.iter().find(|other| **other > own);
            let strict = keys
                .iter()
                .find(|other| other.0 > own.0)
                .map(|other| other.0.clone());
            let recorded = judgment["next_promulgation"].as_str().map(str::to_owned);
            let found = if next.is_some_and(|next| next.0 == own.0) {
                Some((
                    "next-revision-definition",
                    "effective date is unresolved; the next revision has the same promulgation date",
                ))
            } else if strict != recorded {
                Some((
                    "next-revision-changed",
                    "effective date is unresolved; the next revision changed after the LLM judgment",
                ))
            } else if judgment["outcome"] != "explicit_date"
                && recorded.as_deref().is_some_and(|next| {
                    judgment["section_label_dates"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .any(|label| label >= next)
                })
            {
                Some((
                    "label-on-or-after-next-revision",
                    "effective date is unresolved; the quoted section belongs to a later revision",
                ))
            } else {
                None
            };
            if let Some((cause, reason)) = found {
                withheld.push((source, cause, reason, recorded, strict, next.cloned()));
            }
        }
        for (source, cause, reason, recorded, strict, next) in withheld {
            let revision = self.source_revision[source];
            let occurrence =
                &mut self.revisions[revision].occurrences[self.source_occurrence[source]];
            occurrence.leave_unresolved(reason, "llm-next-revision");
            if let Some(evidence) = occurrence.date_evidence.as_object_mut() {
                evidence.insert(
                    "llm_not_applied".to_owned(),
                    llm_not_applied(
                        cause,
                        json!({"recorded_next_promulgation": recorded, "actual_next_promulgation": strict,
                            "actual_next_revision": next.map(|next| next.2)}),
                    ),
                );
            }
            let first = self.revisions[revision].occurrences[0].clone();
            let target = &mut self.revisions[revision];
            target.status = Some(first.status);
            target.enforcement_date = first.enforcement_date;
            target.reason = first.reason;
            target.date_evidence = first.date_evidence;
        }
    }

    /// The single place where a date that an LLM judgment selected shows as status `llm`
    /// (design-v3-integration.md 3.3). It runs after every other processing of the plan, so the
    /// gate, the cap and the repeal rule read the date as confirmed. A date that one of them took
    /// away stays unresolved with its `llm_judgment` evidence.
    fn mark_llm_dates(&mut self) {
        for revision in &mut self.revisions {
            let mut changed = false;
            for occurrence in &mut revision.occurrences {
                if occurrence.status == "confirmed"
                    && !occurrence.date_evidence["llm_judgment"].is_null()
                {
                    occurrence.status = "llm".to_owned();
                    changed = true;
                }
            }
            if !changed {
                continue;
            }
            let first = revision.occurrences[0].clone();
            revision.status = Some(first.status);
            revision.enforcement_date = first.enforcement_date;
            revision.reason = first.reason;
            revision.date_evidence = first.date_evidence;
        }
    }

    /// Leaves unresolved a revision that a later-promulgated repeal takes away before its own
    /// effective date (user decision `user-repeal-revival-unresolved-20261117`, design 9). It runs
    /// for the enforcement basis only, after the gate and before the ranks, so the repeal and the
    /// revision are judged by their final state.
    ///
    /// An occurrence of a revision that is no repeal is the target when it is confirmed or
    /// estimated, has an effective date E_X and a promulgation date, and when a repeal of the same
    /// identity that is promulgated later (order key: promulgation date, numeric serial, revision
    /// text) is confirmed with an effective date E_R < E_X. Among several such repeals the one with
    /// the earliest E_R is the evidence (ties: the earliest order key). A repeal that is not
    /// confirmed, a repeal with the same effective date and a source without a promulgation date
    /// are no reason. The unit is the occurrence; the first occurrence stands for the revision, as
    /// in `defer_late_label_dates`. The evidence keeps every earlier key.
    fn leave_revived_unresolved(&mut self) {
        type Order = (String, u64, String);
        let order = |plan: &Self, source: usize| -> Option<Order> {
            Some((
                plan.occurrence(source).promulgation_date.clone()?,
                plan.sources[source].serial,
                plan.sources[source].revision.clone(),
            ))
        };
        // The confirmed repeals of each identity: (order key, effective date).
        let mut repeals: BTreeMap<&str, Vec<(Order, &str)>> = BTreeMap::new();
        for source in 0..self.sources.len() {
            let occurrence = self.occurrence(source);
            // A repeal that an LLM judgment dated is no confirmed repeal here
            // (user-c2-llm-repeal-no-20261117): an LLM date never changes another revision.
            if let (true, "confirmed", Some(effective), Some(key)) = (
                self.sources[source].repeal
                    && occurrence.date_evidence.get("llm_judgment").is_none(),
                occurrence.status.as_str(),
                occurrence.enforcement_date.as_deref(),
                order(self, source),
            ) {
                repeals
                    .entry(self.sources[source].identity.as_str())
                    .or_default()
                    .push((key, effective));
            }
        }
        let mut targets = Vec::new();
        for source in 0..self.sources.len() {
            let occurrence = self.occurrence(source);
            let (true, Some(own), Some(effective)) = (
                !self.sources[source].repeal
                    && matches!(occurrence.status.as_str(), "confirmed" | "estimated"),
                order(self, source),
                occurrence.enforcement_date.as_deref(),
            ) else {
                continue;
            };
            let found = repeals
                .get(self.sources[source].identity.as_str())
                .into_iter()
                .flatten()
                .filter(|(key, repeal_date)| *key > own && *repeal_date < effective)
                .min_by(|left, right| (left.1, &left.0).cmp(&(right.1, &right.0)));
            if let Some((key, repeal_date)) = found {
                targets.push((
                    source,
                    key.2.clone(),
                    (*repeal_date).to_owned(),
                    effective.to_owned(),
                ));
            }
        }
        for (source, repeal_revision, repeal_date, withheld_date) in targets {
            let revision = self.source_revision[source];
            let occurrence =
                &mut self.revisions[revision].occurrences[self.source_occurrence[source]];
            occurrence.leave_unresolved(
                "effective date is unresolved; a later-promulgated repeal takes effect before this revision",
                "repeal-before-effective",
            );
            if let Some(evidence) = occurrence.date_evidence.as_object_mut() {
                evidence.insert(
                    "repeal_before_effective".to_owned(),
                    json!({
                        "user_decision": REPEAL_REVIVAL_DECISION,
                        "repeal_revision": repeal_revision,
                        "repeal_enforcement_date": repeal_date,
                        "withheld_enforcement_date": withheld_date,
                        "order_key": ["promulgation_date", "serial", "revision"],
                    }),
                );
            }
            let first = self.revisions[revision].occurrences[0].clone();
            let target = &mut self.revisions[revision];
            target.status = Some(first.status);
            target.enforcement_date = first.enforcement_date;
            target.reason = first.reason;
            target.date_evidence = first.date_evidence;
        }
    }

    /// The gate of every revision group (see `Occurrence::apply_gate`), after the processing of the
    /// plan that gives each revision its state (the cap of `defer_late_label_dates`), so each result
    /// that the earlier processing made keeps its own evidence. Only `leave_revived_unresolved`
    /// comes after it. The first occurrence of a revision stands for the revision, as in
    /// `defer_late_label_dates`.
    fn apply_gate(&mut self) {
        // The unit is the revision, whatever the number of its copies: copies of one revision
        // (the same identity and revision, content that differs) get the same result. One copy that
        // the gate withholds withholds every copy that is confirmed or estimated, and so the
        // commit date of the revision follows.
        let mut groups: BTreeMap<(String, String), Vec<usize>> = BTreeMap::new();
        for (index, source) in self.sources.iter().enumerate() {
            groups
                .entry((source.identity.clone(), source.revision.clone()))
                .or_default()
                .push(index);
        }
        for members in groups.values() {
            // A copy that does not carry the same approval as another copy (it fails a condition
            // of the registration) withholds the group; the approval never passes to that copy
            // and no copy falls back to its source date (r25c-design.md 8).
            // An LLM selection also carries its date: copies that the LLM dated differently do not
            // carry the same approval (compiler#2). A registry selection carries no date here.
            let approvals: BTreeSet<(Option<&'static str>, Option<String>)> = members
                .iter()
                .map(|source| {
                    let occurrence = self.occurrence(*source);
                    let date = (occurrence.gate.approval == Some(LLM_APPROVAL))
                        .then(|| occurrence.enforcement_date.clone())
                        .flatten();
                    (occurrence.gate.approval, date)
                })
                .collect();
            let mismatch =
                approvals.len() > 1 && approvals.iter().any(|(approval, _)| approval.is_some());
            let llm_mismatch = mismatch
                && approvals
                    .iter()
                    .any(|(approval, _)| *approval == Some(LLM_APPROVAL));
            if !mismatch
                && !members
                    .iter()
                    .any(|source| self.occurrence(*source).gate_applies())
            {
                continue;
            }
            for source in members {
                let revision = self.source_revision[*source];
                let occurrence =
                    &mut self.revisions[revision].occurrences[self.source_occurrence[*source]];
                occurrence.gate.approval_mismatch |= mismatch;
                occurrence.gate.llm_mismatch |= llm_mismatch;
                occurrence.withhold();
                let first = self.revisions[revision].occurrences[0].clone();
                let target = &mut self.revisions[revision];
                target.status = Some(first.status);
                target.enforcement_date = first.enforcement_date;
                target.reason = first.reason;
                target.date_evidence = first.date_evidence;
            }
        }
    }

    /// Rejects inconsistent authority, raw evidence, and ranks before publication.
    fn validate_occurrences(&self) -> Result<()> {
        for source in 0..self.sources.len() {
            let occurrence = self.occurrence(source);
            let valid = |date: &Option<String>| {
                date.as_deref().is_none_or(|date| {
                    compact_date(&date.replace('-', "")).is_some_and(|value| iso(value) == date)
                })
            };
            ensure!(
                valid(&occurrence.promulgation_date)
                    && valid(&occurrence.enforcement_date)
                    && valid(&occurrence.exposure_date),
                "DATE_INDEX_INVALID: invalid authoritative date"
            );
            let publication = occurrence.promulgation_date.is_some();
            let effective = occurrence.enforcement_date.is_some();
            let exposed = occurrence.exposure_date.is_some();
            let coherent = match occurrence.status.as_str() {
                "confirmed" | "estimated" => effective && !exposed,
                "llm" if self.llm.is_some() => effective && !exposed,
                "unresolved" => {
                    !effective
                        && publication
                        && occurrence.exposure_date == occurrence.promulgation_date
                }
                "undated" => !publication && !effective && !exposed,
                _ => false,
            };
            ensure!(
                coherent,
                "DATE_INDEX_INVALID: inconsistent occurrence status"
            );
            ensure!(
                occurrence.raw_promulgation_date == self.sources[source].promulgation
                    && occurrence.raw_enforcement_date == self.sources[source].enforcement,
                "DATE_INDEX_INVALID: source date evidence changed"
            );
            ensure!(
                occurrence.promulgation_rank.is_some() == publication
                    && occurrence.enforcement_rank.is_some() == (effective || exposed),
                "DATE_INDEX_INVALID: date and rank mismatch"
            );
        }
        for enforcement in [false, true] {
            for (rank, source) in self.ordered(enforcement).into_iter().enumerate() {
                let occurrence = self.occurrence(source);
                let actual = if enforcement {
                    occurrence.enforcement_rank
                } else {
                    occurrence.promulgation_rank
                };
                ensure!(
                    actual == Some(rank as u64) && rank as u64 <= 9_007_199_254_740_991,
                    "DATE_INDEX_INVALID: duplicate, unsafe, or unordered rank"
                );
            }
        }
        Ok(())
    }

    /// Gets the authoritative occurrence for a source.
    fn occurrence(&self, source: usize) -> &Occurrence {
        &self.revisions[self.source_revision[source]].occurrences[self.source_occurrence[source]]
    }

    /// Gets the event date for one semantic.
    fn event_date(&self, source: usize, enforcement: bool) -> Option<&str> {
        let occurrence = self.occurrence(source);
        if enforcement {
            occurrence
                .enforcement_date
                .as_deref()
                .or(occurrence.exposure_date.as_deref())
        } else {
            occurrence.promulgation_date.as_deref()
        }
    }

    /// Sorts each whole date group with the approved uniform secondary key.
    fn ordered(&self, enforcement: bool) -> Vec<usize> {
        let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for source in 0..self.sources.len() {
            if let Some(date) = self.event_date(source, enforcement) {
                groups.entry(date.to_owned()).or_default().push(source);
            }
        }
        let mut ordered = Vec::new();
        for mut group in groups.into_values() {
            let all_publication = group
                .iter()
                .all(|source| self.occurrence(*source).promulgation_date.is_some());
            group.sort_by(|left, right| {
                let a = &self.sources[*left];
                let b = &self.sources[*right];
                let publication = if all_publication {
                    self.occurrence(*left)
                        .promulgation_date
                        .cmp(&self.occurrence(*right).promulgation_date)
                } else {
                    std::cmp::Ordering::Equal
                };
                publication
                    .then_with(|| {
                        if self.dataset == "laws" && all_publication {
                            a.secondary_name
                                .cmp(&b.secondary_name)
                                .then(a.secondary_number.cmp(&b.secondary_number))
                                .then(a.serial.cmp(&b.serial))
                        } else {
                            a.serial
                                .cmp(&b.serial)
                                .then(a.canonical_path.cmp(&b.canonical_path))
                        }
                    })
                    .then_with(|| {
                        // Copies of one revision rank by the default build's copy preference,
                        // so the copy a default repository commits sorts last. The copy hash
                        // only separates equal preferences.
                        (
                            a.identity.as_bytes(),
                            a.revision.as_bytes(),
                            a.copy_priority,
                            self.revisions[self.source_revision[*left]]
                                .copy_id
                                .as_bytes(),
                            a.source_path.as_bytes(),
                        )
                            .cmp(&(
                                b.identity.as_bytes(),
                                b.revision.as_bytes(),
                                b.copy_priority,
                                self.revisions[self.source_revision[*right]]
                                    .copy_id
                                    .as_bytes(),
                                b.source_path.as_bytes(),
                            ))
                    })
            });
            ordered.extend(group);
        }
        ordered
    }

    /// Registers a body that the original build actually commits.
    pub fn record_body(&mut self, path: &str, content: &[u8], commit: [u8; 20]) {
        self.record_body_hashed(path, sha256(content), blob_id(content), commit);
    }

    /// Registers a committed body whose SHA-256 and Git blob id the caller already knows.
    pub fn record_body_hashed(
        &mut self,
        path: &str,
        sha256: String,
        blob_sha: [u8; 20],
        commit: [u8; 20],
    ) {
        self.bodies
            .entry((path.to_owned(), sha256.clone()))
            .or_insert_with(|| {
                json!({"kind": "git", "commit_sha": hex(&commit), "path": path,
            "blob_sha": hex(&blob_sha), "sha256": sha256})
            });
    }

    /// Renders the mode-specific body without changing the default body.
    fn replay_body(&self, source: usize) -> Result<Vec<u8>> {
        let original = String::from_utf8(self.sources[source].markdown.clone())?;
        let occurrence = self.occurrence(source);
        ensure!(
            original.starts_with("---\n"),
            "source body lacks frontmatter"
        );
        let mut extra = format!(
            "날짜판단상태: {}\n법적시행일: {}\n검색배치일: {}\n시행일판단근거: {}\n",
            serde_json::to_string(&occurrence.status)?,
            serde_json::to_string(&occurrence.enforcement_date)?,
            serde_json::to_string(&self.event_date(source, true))?,
            serde_json::to_string(&occurrence.reason)?
        );
        // A date of an LLM judgment names its sentence (docs/llm-date-judgment.md, compiler#2).
        if occurrence.status == "llm" {
            extra.push_str(&format!(
                "시행일근거문장: {}\n",
                serde_json::to_string(&occurrence.date_evidence["llm_judgment"]["quote"])?
            ));
        }
        Ok(format!("---\n{extra}{}", &original[4..]).into_bytes())
    }

    /// Encodes a source identifier without path collisions.
    fn candidate_path(&self, revision: usize) -> String {
        let revision = &self.revisions[revision];
        format!(
            ".legalize/candidates/{}/{}/{}.md",
            identity_directory(&self.dataset, &revision.identity),
            identity_name(&revision.revision),
            revision.copy_id
        )
    }

    /// Replays effective-date events into an exclusively owned staging repository.
    pub fn replay(&mut self, output: &Path, readme: &[u8]) -> Result<(String, StagedRepository)> {
        self.validate_occurrences()?;
        let ordered = self.ordered(true);
        let initial_epoch = ordered
            .first()
            .and_then(|source| self.event_date(*source, true))
            .map(epoch)
            .transpose()?
            .unwrap_or(INITIAL_COMMIT_EPOCH);
        let mut repo = BareRepoWriter::create_exclusive(output)?;
        let mut staged = StagedRepository {
            owned: BareRepoWriter::staging_path(output)?,
            final_path: output.to_path_buf(),
            armed: true,
        };
        let replay_result = (|| -> Result<(String, PathBuf)> {
            let initial_message = if self.dataset == "laws" {
                "initial commit\n\nCo-authored-by: Jihyeon Kim <simnalamburt@gmail.com>"
            } else {
                "initial commit"
            };
            // The first commit keeps the fixed date of the default build (2026-03-30 12:00 KST).
            // Date lookups use the snapshot dates, not this commit.
            repo.commit_static(
                &RepoPathBuf::root_file("README.md"),
                readme,
                initial_message,
                INITIAL_COMMIT_EPOCH,
            )?;
            if self.dataset == "laws" {
                repo.commit_initial_contributor(INITIAL_COMMIT_EPOCH)?;
            }
            for source in 0..self.sources.len() {
                if self.occurrence(source).status != "undated" {
                    continue;
                }
                let path = self.candidate_path(self.source_revision[source]);
                let content = self.replay_body(source)?;
                let (blob, compressed) = precompute_blob(&content);
                repo.commit_bot_file(
                    &RepoPathBuf::file(&path),
                    &content,
                    blob,
                    &compressed,
                    "Preserve undated source copy",
                    GitTimestampKst::from_epoch(initial_epoch),
                )?;
                self.record_body(&path, &content, repo.head_commit().unwrap());
            }
            let mut cache = SnapshotCache::new(self.revisions.len());
            let mut by_identity: BTreeMap<String, Vec<usize>> = BTreeMap::new();
            for (index, revision) in self.revisions.iter().enumerate() {
                by_identity
                    .entry(revision.identity.clone())
                    .or_default()
                    .push(index);
            }
            let undated: Vec<(&str, &[usize])> = by_identity
                .iter()
                .filter(|(_, revisions)| {
                    revisions.iter().any(|index| {
                        self.revisions[*index]
                            .occurrences
                            .iter()
                            .any(|occurrence| occurrence.status == "undated")
                    })
                })
                .map(|(identity, revisions)| (identity.as_str(), revisions.as_slice()))
                .collect();
            if !undated.is_empty() {
                self.commit_identity_files(
                    &mut repo,
                    &undated,
                    None,
                    "Preserve undated source references",
                    GitTimestampKst::from_epoch(initial_epoch),
                    &mut cache,
                )?;
            }
            let mut paths: BTreeMap<String, String> = BTreeMap::new();
            let mut cursor = 0;
            while cursor < ordered.len() {
                let date = self.event_date(ordered[cursor], true).unwrap().to_owned();
                let timestamp = GitTimestampKst::from_epoch(epoch(&date)?);
                let mut changed: BTreeSet<String> = BTreeSet::new();
                while cursor < ordered.len()
                    && self.event_date(ordered[cursor], true) == Some(date.as_str())
                {
                    let source_index = ordered[cursor];
                    let source = &self.sources[source_index];
                    changed.insert(source.identity.clone());
                    let occurrence = self.occurrence(source_index);
                    let unresolved = occurrence.status == "unresolved";
                    let message = format!(
                        "{}: {}{}\n\nrevision: {}\nevent_date: {}\nstatus: {}\nreason: {}\n\n{}Effective-Date: {}",
                        source.category,
                        source.name,
                        if occurrence.status == "estimated" {
                            " (추정)"
                        } else if occurrence.status == "llm" {
                            " (LLM 판단)"
                        } else {
                            ""
                        },
                        source.revision,
                        date,
                        occurrence.status,
                        occurrence.reason,
                        identity_lines(&self.dataset, &source.identity, &source.revision),
                        date
                    );
                    if source.repeal && !unresolved {
                        let deletions = paths
                            .remove(&source.identity)
                            .into_iter()
                            .map(RepoPathBuf::file)
                            .collect::<Vec<_>>();
                        repo.commit_bot_deletions(&deletions, &message, timestamp)?;
                    } else {
                        let path = if unresolved {
                            self.candidate_path(self.source_revision[source_index])
                        } else {
                            source.canonical_path.clone()
                        };
                        let content = self.replay_body(source_index)?;
                        let deletions = if unresolved {
                            Vec::new()
                        } else {
                            paths
                                .insert(source.identity.clone(), path.clone())
                                .filter(|previous| previous != &path)
                                .into_iter()
                                .map(RepoPathBuf::file)
                                .collect()
                        };
                        let (blob, compressed) = precompute_blob(&content);
                        repo.commit_bot_file_with_deletions(
                            &RepoPathBuf::file(&path),
                            PreparedBlob::from_parts(&content, blob, &compressed),
                            &deletions,
                            &message,
                            timestamp,
                        )?;
                        self.record_body(&path, &content, repo.head_commit().unwrap());
                    }
                    cursor += 1;
                }
                let files: Vec<(&str, &[usize])> = changed
                    .iter()
                    .map(|identity| (identity.as_str(), by_identity[identity].as_slice()))
                    .collect();
                self.commit_identity_files(
                    &mut repo,
                    &files,
                    Some(&date),
                    &format!("Record date snapshot {date}\n\nEffective-Date: {date}"),
                    timestamp,
                    &mut cache,
                )?;
                self.snapshots.push(
                    json!({"snapshot_date": date, "commit_sha": hex(&repo.head_commit().unwrap())}),
                );
            }
            let (head, path) = repo.seal()?;
            Ok((hex(&head), path))
        })();
        match replay_result {
            Ok((head, path)) => {
                staged.owned = path;
                Ok((head, staged))
            }
            Err(error) => {
                match fs::remove_dir_all(&staged.owned) {
                    Ok(()) => staged.armed = false,
                    Err(cleanup) if cleanup.kind() == std::io::ErrorKind::NotFound => {
                        staged.armed = false
                    }
                    Err(cleanup) => {
                        staged.armed = false;
                        return Err(error).context(format!(
                            "failed to recover {}: {cleanup}",
                            staged.owned.display()
                        ));
                    }
                }
                Err(error)
            }
        }
    }

    /// Returns the pretty-printed JSON of one revision as exposed at `date`.
    ///
    /// The fragment is indented for an element of the file's `revisions` array. A revision with
    /// no occurrence exposed at `date` has no fragment. Git body fragments are kept while the
    /// retained occurrences stay the same, so a later file costs a copy, not a rebuild.
    fn revision_fragment(
        &self,
        index: usize,
        date: Option<&str>,
        cache: &mut SnapshotCache,
    ) -> Result<Option<Vec<u8>>> {
        let retained = |occurrence: &Occurrence| {
            occurrence.status == "undated"
                || date.is_some_and(|date| {
                    occurrence
                        .enforcement_date
                        .as_deref()
                        .or(occurrence.exposure_date.as_deref())
                        .is_some_and(|event| event <= date)
                })
        };
        let count = self.revisions[index]
            .occurrences
            .iter()
            .filter(|occurrence| retained(occurrence))
            .count();
        if count == 0 {
            return Ok(None);
        }
        if let Some((cached, fragment)) = &cache.fragments[index]
            && *cached == count
        {
            return Ok(Some(fragment.clone()));
        }
        let mut revision = self.revisions[index].clone();
        revision
            .occurrences
            .retain(|occurrence| retained(occurrence));
        Self::summarize(&mut revision);
        let source = cache.first_source(&self.source_revision)[index];
        let hash = match &cache.body_hash[index] {
            Some(hash) => hash.clone(),
            None => {
                let content = self
                    .replay_body(source)
                    .expect("source Markdown is valid UTF-8");
                let hash = sha256(&content);
                cache.body_hash[index] = Some(hash.clone());
                hash
            }
        };
        let path = if matches!(revision.status.as_deref(), Some("unresolved" | "undated")) {
            self.candidate_path(index)
        } else {
            revision.canonical_path.clone()
        };
        revision.body = self.bodies.get(&(path, hash)).cloned().unwrap_or_else(|| {
            let content = self.replay_body(source).unwrap();
            json!({"kind":"inline", "text":String::from_utf8(content.clone()).unwrap(), "sha256":sha256(&content)})
        });
        let mut fragment = Vec::new();
        fragment.extend_from_slice(b"    ");
        for byte in serde_json::to_vec_pretty(&serde_json::to_value(&revision)?)? {
            fragment.push(byte);
            if byte == b'\n' {
                fragment.extend_from_slice(b"    ");
            }
        }
        // An inline body may turn into a Git body later, so only Git bodies are kept.
        if revision.body["kind"] == "git" {
            cache.fragments[index] = Some((count, fragment.clone()));
        }
        Ok(Some(fragment))
    }

    /// Creates the pretty-printed index file of one identity as exposed at `date`.
    ///
    /// The bytes equal `to_vec_pretty` of the file value. `date` is null for the start commit,
    /// which holds only undated occurrences.
    fn identity_file_bytes(
        &self,
        identity: &str,
        revisions: &[usize],
        date: Option<&str>,
        cache: &mut SnapshotCache,
    ) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        out.extend_from_slice(
            format!(
                "{{\n  \"basis\": {},\n  \"dataset\": {},\n  \"identity\": {},\n  \"revisions\": [",
                serde_json::to_string(&self.basis)?,
                serde_json::to_string(&self.dataset)?,
                serde_json::to_string(identity)?
            )
            .as_bytes(),
        );
        let mut first = true;
        for index in revisions {
            if let Some(fragment) = self.revision_fragment(*index, date, cache)? {
                out.extend_from_slice(if first { b"\n" } else { b",\n" });
                first = false;
                out.extend_from_slice(&fragment);
            }
        }
        if !first {
            out.extend_from_slice(b"\n  ");
        }
        out.extend_from_slice(
            format!(
                "],\n  \"schema_version\": {},\n  \"snapshot_date\": {}\n}}",
                if self.llm.is_some() { 2 } else { 1 },
                serde_json::to_string(&date)?
            )
            .as_bytes(),
        );
        Ok(out)
    }

    /// Commits the index files of the given identities in one commit.
    fn commit_identity_files(
        &self,
        repo: &mut BareRepoWriter,
        identities: &[(&str, &[usize])],
        date: Option<&str>,
        message: &str,
        time: GitTimestampKst,
        cache: &mut SnapshotCache,
    ) -> Result<()> {
        let mut contents = Vec::with_capacity(identities.len());
        for (identity, revisions) in identities {
            let bytes = self.identity_file_bytes(identity, revisions, date, cache)?;
            let (blob, compressed) = precompute_blob(&bytes);
            contents.push((
                RepoPathBuf::file(identity_index_path(&self.dataset, identity)),
                bytes,
                blob,
                compressed,
            ));
        }
        let files: Vec<_> = contents
            .iter()
            .map(|(path, bytes, blob, compressed)| {
                (
                    path.clone(),
                    PreparedBlob::from_parts(bytes, *blob, compressed),
                )
            })
            .collect();
        repo.commit_bot_files(&files, message, time)
    }

    /// Clears summaries when occurrence values differ.
    fn summarize(revision: &mut Revision) {
        let first = &revision.occurrences[0];
        revision.reason = first.reason.clone();
        revision.date_evidence = first.date_evidence.clone();
        revision.promulgation_date = if revision
            .occurrences
            .iter()
            .all(|value| value.promulgation_date == first.promulgation_date)
        {
            first.promulgation_date.clone()
        } else {
            None
        };
        revision.enforcement_date = if revision
            .occurrences
            .iter()
            .all(|value| value.enforcement_date == first.enforcement_date)
        {
            first.enforcement_date.clone()
        } else {
            None
        };
        revision.status = if revision
            .occurrences
            .iter()
            .all(|value| value.status == first.status)
        {
            Some(first.status.clone())
        } else {
            None
        };
    }

    /// Checks output destinations before the compiler writes repository objects.
    ///
    /// Only enforcement refuses an existing output or index. The default basis replaces both.
    pub fn check_destination(
        output: &Path,
        manifest: Option<&Path>,
        enforcement: bool,
    ) -> Result<PathBuf> {
        let path = manifest.map(Path::to_path_buf).unwrap_or_else(|| {
            let mut path = output.as_os_str().to_os_string();
            path.push(".date-index.json");
            PathBuf::from(path)
        });
        if enforcement {
            ensure!(
                !output.exists(),
                "output already exists: {}",
                output.display()
            );
            ensure!(
                !path.exists(),
                "date index already exists: {}",
                path.display()
            );
        }
        if enforcement {
            ensure!(
                path != output && !path.starts_with(output),
                "date index must be outside the output repository"
            );
            let mut diagnostic = output.as_os_str().to_os_string();
            diagnostic.push(".label-date-applied.json");
            let diagnostic = PathBuf::from(diagnostic);
            ensure!(
                path != diagnostic,
                "date index conflicts with diagnostic path"
            );
            ensure!(
                !diagnostic.exists(),
                "diagnostic already exists: {}",
                diagnostic.display()
            );
            for destination in [&path, &diagnostic] {
                let parent = destination
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."));
                ensure!(
                    parent.is_dir(),
                    "destination parent does not exist: {}",
                    parent.display()
                );
            }
        }
        Ok(path)
    }

    /// Removes a previous index so it cannot sit beside output that replaced its repository.
    pub fn discard_stale_index(path: &Path) -> Result<()> {
        match fs::remove_file(path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error)
                .with_context(|| format!("cannot remove stale date index {}", path.display())),
            _ => Ok(()),
        }
    }

    /// Lists the revisions whose written label date was applied because it is later than the
    /// promulgation date, so a reader can audit a label that may be a typo.
    fn label_date_report(&self) -> Result<Value> {
        let mut entries = Vec::new();
        let mut deferred = Vec::new();
        for revision in &self.revisions {
            let evidence = &revision.date_evidence;
            if evidence["decision"] == "label-date-after-next-revision" {
                deferred.push(json!({
                    "dataset": self.dataset,
                    "identity": revision.identity,
                    "revision": revision.revision,
                    "promulgation_date": revision.promulgation_date,
                    "label_date": evidence["label_date"],
                    "next_revision": evidence["next_revision"],
                    "next_promulgation_date": evidence["next_promulgation_date"],
                    "attributed_label": evidence["attributed_label"],
                }));
            }
            // The report follows the final state: a label date that a hold made unresolved (the
            // guard of r24, the gate, or the repeal-before-effective rule) is no applied date.
            if revision.status.as_deref() != Some("confirmed")
                || evidence["decision"] != "single-label-date-priority-20261103"
                || evidence["applied_side"] != "label"
            {
                continue;
            }
            let day = |text: Option<&str>| {
                text.and_then(|text| compact_date(&text.replace('-', "")))
                    .context("label date report needs valid dates")
            };
            let promulgation = day(revision.promulgation_date.as_deref())?;
            let label = day(evidence["label_date"].as_str())?;
            entries.push(json!({
                "dataset": self.dataset,
                "identity": revision.identity,
                "revision": revision.revision,
                "promulgation_date": iso(promulgation),
                "label_date": iso(label),
                "days_after_promulgation": (label - promulgation).whole_days(),
                "attributed_label": evidence["attributed_label"],
            }));
        }
        // A label date that an LLM judgment selected is reported too (user decision, comment
        // 6599299359 item 4; compiler#2). The list exists only in a plan built with a ledger.
        let mut llm_entries = Vec::new();
        for revision in &self.revisions {
            let judgment = &revision.date_evidence["llm_judgment"];
            if revision.status.as_deref() == Some("llm") && judgment["outcome"] == "label_date" {
                llm_entries.push(json!({
                    "dataset": self.dataset,
                    "identity": revision.identity,
                    "revision": revision.revision,
                    "promulgation_date": revision.promulgation_date,
                    "label_date": revision.enforcement_date,
                    "section_label": judgment["section_label"],
                    "quote": judgment["quote"],
                }));
            }
        }
        for list in [&mut entries, &mut deferred, &mut llm_entries] {
            list.sort_by(|a, b| {
                (a["identity"].as_str(), a["revision"].as_str())
                    .cmp(&(b["identity"].as_str(), b["revision"].as_str()))
            });
        }
        let mut report = json!({
            "schema_version": 1,
            "basis": self.basis,
            "dataset": self.dataset,
            "decision": "single-label-date-priority-20261103",
            "label_date_applied": entries,
            "label_date_unresolved_after_next_revision": deferred,
        });
        if self.llm.is_some() {
            report["llm_label_date_applied"] = json!(llm_entries);
        }
        Ok(report)
    }

    /// Verifies reachable body references and publishes the mandatory external index last.
    ///
    /// Enforcement refuses an existing index. The default basis replaces it by rename.
    pub fn publish(
        self,
        output: Option<&Path>,
        head: &str,
        path: &Path,
        manifest: Value,
    ) -> Result<()> {
        let (bytes, _) = self.prepare_publication(output, head, manifest)?;
        let mut temporary = path.as_os_str().to_os_string();
        temporary.push(format!(".tmp.{}", std::process::id()));
        let temporary = PathBuf::from(temporary);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .with_context(|| format!("cannot stage date index {}", path.display()))?;
        let result = (|| -> Result<()> {
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, path)
                .with_context(|| format!("cannot publish date index {}", path.display()))?;
            Ok(())
        })();
        if result.is_err() {
            fs::remove_file(&temporary)?;
        }
        result?;
        eprintln!("date index: {}", path.display());
        Ok(())
    }

    /// Publishes an enforcement repository and its files without replacing existing paths.
    pub fn publish_enforcement(
        self,
        staged: StagedRepository,
        head: &str,
        path: &Path,
        manifest: Value,
    ) -> Result<()> {
        self.publish_enforcement_with_hook(staged, head, path, manifest, |_| Ok(()))
    }

    fn publish_enforcement_with_hook(
        self,
        mut staged: StagedRepository,
        head: &str,
        path: &Path,
        manifest: Value,
        hook: impl Fn(&str) -> Result<()>,
    ) -> Result<()> {
        let (bytes, diagnostic) =
            match self.prepare_publication(Some(&staged.owned), head, manifest) {
                Ok(value) => value,
                Err(error) => {
                    if let Err(cleanup) = fs::remove_dir_all(&staged.owned) {
                        return Err(error).context(format!(
                            "cannot recover {}: {cleanup}",
                            staged.owned.display()
                        ));
                    }
                    staged.armed = false;
                    return Err(error);
                }
            };
        let diagnostic = diagnostic.context("missing enforcement diagnostic")?;
        let mut diagnostic_path = staged.final_path.as_os_str().to_os_string();
        diagnostic_path.push(".label-date-applied.json");
        let diagnostic_path = PathBuf::from(diagnostic_path);
        let mut owned_files: Vec<PathBuf> = Vec::new();
        let result = (|| -> Result<()> {
            for (destination, content) in [(diagnostic_path.as_path(), &diagnostic), (path, &bytes)]
            {
                let mut temporary = destination.as_os_str().to_os_string();
                temporary.push(format!(".tmp.{}", std::process::id()));
                let temporary = PathBuf::from(temporary);
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&temporary)
                    .with_context(|| format!("cannot stage {}", destination.display()))?;
                owned_files.push(temporary);
                file.write_all(content)?;
                file.sync_all()?;
            }
            fs::create_dir(&staged.final_path)
                .with_context(|| format!("cannot reserve {}", staged.final_path.display()))?;
            if let Err(error) = hook("after_reserve") {
                return match fs::remove_dir(&staged.final_path) {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(error).context(format!(
                        "cannot recover reservation {}: {cleanup}",
                        staged.final_path.display()
                    )),
                };
            }
            // A different process could replace our empty reservation with its own empty directory.
            if let Err(error) = fs::rename(&staged.owned, &staged.final_path) {
                let cleanup = fs::remove_dir(&staged.final_path);
                return Err(error).context(format!(
                    "cannot place repository {}; reservation cleanup: {:?}",
                    staged.final_path.display(),
                    cleanup.err()
                ));
            }
            staged.owned = staged.final_path.clone();
            hook("after_place")?;
            fs::hard_link(&owned_files[0], &diagnostic_path)
                .with_context(|| format!("cannot publish {}", diagnostic_path.display()))?;
            owned_files.push(diagnostic_path.clone());
            fs::remove_file(&owned_files[0])
                .with_context(|| format!("cannot remove {}", owned_files[0].display()))?;
            fs::hard_link(&owned_files[1], path)
                .with_context(|| format!("cannot publish date index {}", path.display()))?;
            owned_files.push(path.to_path_buf());
            fs::remove_file(&owned_files[1])
                .with_context(|| format!("cannot remove {}", owned_files[1].display()))?;
            Ok(())
        })();
        if let Err(error) = result {
            let mut remaining = Vec::new();
            for file in owned_files.iter().rev() {
                if fs::remove_file(file).is_err() && file.exists() {
                    remaining.push(file.display().to_string());
                }
            }
            if fs::remove_dir_all(&staged.owned).is_err() && staged.owned.exists() {
                remaining.push(staged.owned.display().to_string());
            } else {
                staged.armed = false;
            }
            return Err(error).context(format!("recovery left paths: {remaining:?}"));
        }
        staged.armed = false;
        eprintln!("date index: {}", path.display());
        Ok(())
    }

    fn prepare_publication(
        mut self,
        output: Option<&Path>,
        head: &str,
        mut manifest: Value,
    ) -> Result<(Vec<u8>, Option<Vec<u8>>)> {
        self.validate_occurrences()?;
        let is_enforcement = self.basis == "enforcement";
        let admrule_default = self.dataset == "admrules" && !is_enforcement;
        if is_enforcement {
            manifest["entries_total"] = json!(self.sources.len());
            manifest["entries_total_basis"] = json!("source_occurrences");
        }
        manifest["commit_date_basis"] = json!(self.basis);
        manifest["source_date_field"] = json!(if is_enforcement {
            "시행일자"
        } else if self.dataset == "admrules" {
            "발령일자"
        } else {
            "공포일자"
        });
        if manifest.get("current_snapshot_applied").is_none() {
            manifest["current_snapshot_applied"] = json!(false);
        }
        manifest["final_state_deletions_marked"] = json!(admrule_default);
        manifest["current_snapshot_policy"] = json!(if admrule_default {
            "applied_when_available"
        } else {
            "not_applicable"
        });
        manifest["output_kind"] = json!(if output.is_some() { "bare" } else { "tree" });
        manifest["date_policy"] = json!({
            "git_timestamp_floor": "1970-01-01T12:00:00+09:00",
            "sort_key": if admrule_default { "clamped_date_then_serial" } else { "source_date" },
            "clamped_dates_keep_original_order": !admrule_default,
            "original_dates_in_index": true
        });
        for source_index in 0..self.sources.len() {
            let revision_index = self.source_revision[source_index];
            let replayed;
            let content: &[u8] = if self.basis == "enforcement" {
                replayed = self.replay_body(source_index)?;
                &replayed
            } else {
                &self.sources[source_index].markdown
            };
            let known = self
                .markdown_hashes
                .get(source_index)
                .filter(|hash| !hash.is_empty());
            let hash = match known {
                Some(known) if self.basis != "enforcement" => known.clone(),
                _ => sha256(content),
            };
            let path_key = if self.basis == "enforcement"
                && matches!(
                    self.occurrence(source_index).status.as_str(),
                    "unresolved" | "undated"
                ) {
                self.candidate_path(revision_index)
            } else {
                self.sources[source_index].canonical_path.clone()
            };
            let body = match self.bodies.get(&(path_key, hash.clone())) {
                Some(body) => body.clone(),
                None => {
                    ensure!(
                        !(known.is_some() && content.is_empty()),
                        "the body of revision {} was not committed and its text was not kept",
                        self.sources[source_index].revision
                    );
                    json!({"kind":"inline", "text":String::from_utf8(content.to_vec())?, "sha256":hash})
                }
            };
            if !self.revisions[revision_index].body.is_null() {
                ensure!(
                    self.revisions[revision_index].body["sha256"] == body["sha256"],
                    "same XML copy has different Markdown bodies"
                );
            } else {
                self.revisions[revision_index].body = body;
            }
        }
        for revision in &mut self.revisions {
            Self::summarize(revision);
        }
        let references: Vec<&Value> = self
            .revisions
            .iter()
            .map(|revision| &revision.body)
            .filter(|body| body["kind"] == "git")
            .collect();
        if !references.is_empty() {
            let output = output.context("Git body reference without a repository")?;
            verify_git_bodies(output, head, &references)?;
        }
        let diagnostic = if self.basis == "enforcement" && output.is_some() {
            Some(serde_json::to_vec_pretty(&self.label_date_report()?)?)
        } else {
            None
        };
        manifest["date_index"] = json!({"schema_version":1, "basis":self.basis, "dataset":self.dataset, "head_commit_sha":head,
            "revisions":self.revisions, "snapshots":self.snapshots, "unparsable":self.unparsable});
        if let Some(ledger) = &self.llm {
            manifest["date_index"]["schema_version"] = json!(2);
            manifest["date_index"]["llm_ledger"] = ledger.clone();
        }
        Ok((serde_json::to_vec_pretty(&manifest)?, diagnostic))
    }
}

/// Verifies every Git body reference with two Git processes in total.
///
/// One `rev-list` proves reachability of all commits. One `cat-file --batch` resolves each
/// `commit:path`, so the cost stays linear in the number of references.
fn verify_git_bodies(output: &Path, head: &str, references: &[&Value]) -> Result<()> {
    let reachable = String::from_utf8(git(output, &["rev-list", head])?)?;
    let reachable: std::collections::HashSet<&str> = reachable.lines().collect();
    let mut specs = Vec::with_capacity(references.len());
    for body in references {
        let commit = body["commit_sha"].as_str().unwrap();
        ensure!(reachable.contains(commit), "unreachable body commit");
        let path = body["path"].as_str().unwrap();
        ensure!(!path.contains('\n'), "body path contains a line break");
        specs.push(format!("{commit}:{path}\n"));
    }
    let mut child = Command::new("git")
        .arg("--git-dir")
        .arg(output)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdin = child.stdin.take().context("git stdin is unavailable")?;
    let mut stdout =
        std::io::BufReader::new(child.stdout.take().context("git stdout is unavailable")?);
    let checked = std::thread::scope(|scope| -> Result<()> {
        let writer = scope.spawn(move || -> std::io::Result<()> {
            for spec in &specs {
                stdin.write_all(spec.as_bytes())?;
            }
            Ok(())
        });
        let result = (|| -> Result<()> {
            let mut header = String::new();
            for body in references {
                header.clear();
                ensure!(
                    stdout.read_line(&mut header)? > 0,
                    "Git body validation failed: cat-file ended early"
                );
                let mut fields = header.split_whitespace();
                let (blob, kind, size) = (fields.next(), fields.next(), fields.next());
                ensure!(
                    kind == Some("blob") && size.is_some(),
                    "Git body validation failed: {}",
                    header.trim_end()
                );
                let mut content = vec![0_u8; size.unwrap().parse()?];
                stdout.read_exact(&mut content)?;
                let mut newline = [0_u8; 1];
                stdout.read_exact(&mut newline)?;
                ensure!(blob == body["blob_sha"].as_str(), "body blob mismatch");
                ensure!(
                    sha256(&content) == body["sha256"].as_str().unwrap(),
                    "body hash mismatch"
                );
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = child.kill();
        }
        let written = writer.join();
        result?;
        written
            .map_err(|_| anyhow::anyhow!("Git body writer panicked"))?
            .context("cannot send body references to Git")
    });
    let status = child.wait()?;
    checked?;
    ensure!(status.success(), "Git body validation failed: cat-file");
    Ok(())
}

/// Executes a local Git object query and preserves its diagnostic on failure.
fn git(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let result = Command::new("git")
        .arg("--git-dir")
        .arg(repo)
        .args(args)
        .output()?;
    if !result.status.success() {
        bail!(
            "Git body validation failed: {:?}: {}",
            args,
            String::from_utf8_lossy(&result.stderr)
        );
    }
    Ok(result.stdout)
}

/// The path of a file of the test fixtures (`git-writer/tests/fixtures/date_index/`). The tests
/// read nothing outside the repository.
#[cfg(test)]
fn fixture(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/date_index")
        .join(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAIN: &str = "제1조(시행일) 이 조례는 공포한 날부터 시행한다.";

    #[test]
    fn sourceview_slice_join_reconstruction() {
        let mut source = SourceSegments::default();
        source.append("가&lt;나&gt;\r\n", Some(1), 1, 1, "CData", true);
        source.append("둘째&amp;줄", Some(1), 2, 1, "CData", true);
        let clipped = source.slice("가".len(), "가&lt;나&gt;".len()).unwrap();
        assert_eq!(clipped.text, "&lt;나&gt;");
        assert_eq!(
            clipped.segments[0]["source"]["piece_byte_range"],
            json!(["가".len(), "가&lt;나&gt;".len()])
        );
        let joined = source.lines_join().unwrap();
        assert_eq!(
            joined.text,
            source.text.lines().collect::<Vec<_>>().join("\n")
        );
        assert!(joined.segments.iter().any(|x| x["separator"] == "join-lf" && x["inherited_origins"].as_array().is_some()));
        let decoded = joined.decoded().unwrap();
        assert_eq!(decoded.text, decode_markup(&joined.text));
        assert!(
            decoded
                .segments
                .iter()
                .any(|x| x["transform"] == "decode-markup")
        );
        let less = decoded.text.find('<').unwrap();
        let one = decoded.slice(less, less + 1).unwrap();
        assert_eq!(one.text, "<");
        assert_eq!(
            one.segments[0]["inherited_origins"][0]["source"]["piece_byte_range"],
            json!(["가".len(), "가&lt;".len()])
        );
        let amp = decoded.text.find('&').unwrap();
        let one = decoded.slice(amp, amp + 1).unwrap();
        assert_eq!(one.text, "&");
        assert!(
            !one.segments[0]["inherited_origins"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(decoded.slice(1, 2).is_err());
        let mut heading = SourceSegments::default();
        heading.append(
            "부칙 <2024.3.1 조례 제7호> 이 조례는 공포한 날부터 시행한다.\r\n둘째 줄",
            Some(2),
            3,
            2,
            "CData",
            true,
        );
        let sections = heading.section_bodies(None).unwrap();
        let expected = split_sections(&heading.text, None);
        assert_eq!(sections.len(), expected.len());
        assert_eq!(sections[0].text, expected[0].body);
        let separator = sections[0]
            .segments
            .iter()
            .find(|s| s["separator"] == "join-lf")
            .unwrap();
        assert_eq!(
            separator["inherited_origins"][0]["source"]["piece_byte_range"],
            json!([
                "부칙 <2024.3.1 조례 제7호> 이 조례는 공포한 날부터 시행한다.".len(),
                "부칙 <2024.3.1 조례 제7호> 이 조례는 공포한 날부터 시행한다.\r\n".len()
            ])
        );
        let mut amendment = SourceSegments::default();
        amendment.append(
            "○ 부칙\r\n이 조례는 시행한다.\n□ 다음\n뒤",
            Some(3),
            4,
            3,
            "CData",
            false,
        );
        let mut actual_parts = amendment_parts(&amendment.text, &amendment.text, false);
        assert_eq!(actual_parts.len(), 1);
        for actual in &mut actual_parts {
            let (from, to, mark) = actual.view_range;
            actual.source_view = Some(amendment.amendment_part(from, to, mark).unwrap());
        }
        let part = actual_parts[0].source_view.as_ref().unwrap();
        assert_eq!(part.text, actual_parts[0].text);
        assert_eq!(part.text, "부칙\n이 조례는 시행한다.");
        assert_eq!(
            part.segments[0]["source"]["piece_byte_range"][0],
            "○ ".len()
        );
        assert_eq!(
            part.segments
                .iter()
                .find(|s| s["separator"] == "join-lf")
                .unwrap()["inherited_origins"][0]["source"]["piece_byte_range"],
            json!(["○ 부칙".len(), "○ 부칙\r\n".len()])
        );
        eprintln!(
            "{}",
            json!({"case":"slice-join", "input":source.text, "input_segments":source.segments, "joined":joined.text, "joined_segments":joined.segments, "decoded":decoded.text, "decoded_segments":decoded.segments, "section":sections[0].text, "section_segments":sections[0].segments})
        );
    }

    /// Runs every input of a manifest through `resolve` with the B trace and writes the two
    /// independent traces. `SOURCEVIEW_MANIFEST` names the manifest and `SOURCEVIEW_TRACE_OUT`
    /// an existing empty folder; the test is skipped when either is unset. An entry of the
    /// manifest may give `revision` (`category`, `amendment`, `promulgation`, `number`,
    /// `enforcement`); the default is an enactment of an ordinance of 2024-03-01.
    #[test]
    fn b_families_actual_trace() {
        use std::io::Write;
        let (Some(manifest_path), Some(out)) = (
            std::env::var_os("SOURCEVIEW_MANIFEST"),
            std::env::var_os("SOURCEVIEW_TRACE_OUT"),
        ) else {
            eprintln!(
                "b_families_actual_trace: SOURCEVIEW_MANIFEST or SOURCEVIEW_TRACE_OUT unset; nothing traced"
            );
            return;
        };
        let root = std::path::PathBuf::from(out);
        assert!(
            std::fs::read_dir(&root).unwrap().next().is_none(),
            "trace folder is not empty"
        );
        let manifest: Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        let mut baseline = std::fs::File::create(root.join("baseline.jsonl")).unwrap();
        let mut sourceview = std::fs::File::create(root.join("sourceview.jsonl")).unwrap();
        let mut pieces = serde_json::Map::new();
        let mut total = 0;
        for (case, metadata) in manifest.as_object().unwrap() {
            let path = metadata["path"].as_str().unwrap();
            let xml = std::fs::read(path).unwrap();
            let sha = sha256(&xml);
            assert_eq!(sha, metadata["sha256"].as_str().unwrap());
            BODY_TRACE.with(|slot| {
                *slot.borrow_mut() = Some(BodyTrace {
                    case_id: case.clone(),
                    sha: sha.clone(),
                    source: path.to_owned(),
                    ..Default::default()
                })
            });
            let given = &metadata["revision"];
            let text = |key: &str, default: &str| given[key].as_str().unwrap_or(default).to_owned();
            let mut source = revision(
                &text("category", "조례"),
                &text("amendment", "제정"),
                &text("promulgation", "20240301"),
                given["number"].as_u64().unwrap_or(7),
                "",
            );
            if let Some(enforcement) = given["enforcement"].as_str() {
                source.enforcement = enforcement.to_owned();
            }
            source.xml = xml;
            let resolved = resolve("ordinances", &source);
            let mut trace = BODY_TRACE.with(|slot| slot.borrow_mut().take().unwrap());
            flush_orphans(&mut trace);
            assert_eq!(trace.depth, 0, "unbalanced trace entries");
            total += trace.calls;
            let exit = if resolved.is_ok() { 0 } else { 1 };
            for (file, phase, rows) in [
                (&mut baseline, "baseline", trace.baseline),
                (&mut sourceview, "sourceview", trace.sourceview),
            ] {
                let count = rows.len();
                for row in rows {
                    writeln!(file, "{row}").unwrap();
                }
                writeln!(file, "{}", json!({"case_id":case, "xml_sha256":sha, "source":path, "phase":phase, "event":"complete", "complete":true, "exit":exit, "call_count":count, "calls":trace.calls})).unwrap();
                file.flush().unwrap();
            }
            pieces.insert(case.clone(), json!(trace.pieces));
        }
        std::fs::write(
            root.join("parser-pieces-not-independent.json"),
            serde_json::to_vec_pretty(&pieces).unwrap(),
        )
        .unwrap();
        assert!(total > 0, "no traced calls");
        eprintln!("traced calls: {total}");
    }

    #[test]
    fn sourceview_pinned_inputs() {
        let root = fixture("sourceview-inputs");
        for name in [
            "comment",
            "pi",
            "empty",
            "nested",
            "adjacent",
            "actual_lf",
            "actual_crlf",
            "heading_two",
            "addendum_unit",
            "addendum_independent",
            "body_multiple",
            "entity_pieces",
            "repeated",
        ] {
            let xml = std::fs::read(root.join(format!("{name}.xml"))).unwrap();
            eprintln!(
                "{}",
                json!({"case_id":name, "xml_sha256":sha256(&xml), "event":"begin"})
            );
            date_text(&xml).unwrap();
            let mut source = revision("조례", "제정", "20240301", 7, "");
            source.xml = xml;
            let result = resolve("ordinances", &source);
            eprintln!(
                "{}",
                json!({"case_id":name, "event":"end", "resolve_ok":result.is_ok()})
            );
        }
    }

    #[test]
    fn sourceview_small_baseline_fields() {
        for (name, xml) in [
            (
                "comment",
                "<LawService><본문><![CDATA[이 조례]]><!--x--><![CDATA[는 시행한다.]]></본문></LawService>",
            ),
            (
                "unit",
                "<LawService><부칙><부칙단위><부칙내용><![CDATA[앞]]></부칙내용><부칙내용><![CDATA[뒤]]></부칙내용></부칙단위></부칙></LawService>",
            ),
            (
                "sequential-unit",
                "<LawService><부칙><부칙단위><부칙내용><![CDATA[앞]]></부칙내용></부칙단위><부칙단위><부칙내용><![CDATA[뒤]]></부칙내용></부칙단위></부칙></LawService>",
            ),
            (
                "independent",
                "<LawService><부칙><부칙내용><![CDATA[앞]]></부칙내용><부칙내용><![CDATA[뒤]]></부칙내용></부칙></LawService>",
            ),
        ] {
            let value = date_text(xml.as_bytes()).unwrap();
            match name {
                "comment" => assert_eq!(value.body, "이 조례\n는 시행한다.\n"),
                "unit" => assert_eq!(
                    value
                        .addenda
                        .iter()
                        .map(|x| x.text.as_str())
                        .collect::<Vec<_>>(),
                    vec!["앞뒤"]
                ),
                _ => assert_eq!(
                    value
                        .addenda
                        .iter()
                        .map(|x| x.text.as_str())
                        .collect::<Vec<_>>(),
                    vec!["앞", "뒤"]
                ),
            }
        }
    }

    /// Builds one ordinance revision whose addendum text sits in a CDATA section.
    fn revision(
        category: &str,
        amendment: &str,
        promulgation: &str,
        number: u64,
        addendum: &str,
    ) -> SourceRevision {
        let xml = format!(
            "<LawService><공포번호>{number}</공포번호><부칙><부칙공포일자>{promulgation}</부칙공포일자><부칙내용><![CDATA[{addendum}]]></부칙내용></부칙></LawService>"
        );
        SourceRevision {
            identity: "1".to_owned(),
            revision: "9".to_owned(),
            name: "test".to_owned(),
            category: category.to_owned(),
            canonical_path: "p.md".to_owned(),
            source_path: "9.xml".to_owned(),
            xml: xml.into_bytes(),
            promulgation: promulgation.to_owned(),
            enforcement: "99991231".to_owned(),
            amendment: amendment.to_owned(),
            repeal: amendment.contains("폐지"),
            markdown: Vec::new(),
            secondary_name: String::new(),
            secondary_number: 0,
            serial: 9,
            copy_priority: 0,
            copy_conflict: None,
        }
    }

    fn outcome(dataset: &str, source: &SourceRevision) -> (String, Option<String>, Value) {
        let found = resolve(dataset, source).unwrap();
        (found.status, found.enforcement_date, found.date_evidence)
    }

    fn accumulated(label: &str, clause: &str) -> String {
        format!(
            "부칙  제1조(시행일) 이 조례는 공포한 날부터 시행한다.\n부칙 &lt;2012. 8. 7 조례 제2409호&gt; 이 조례는 공포한 날부터 시행한다.\n부칙 {label} {clause}\n제2조(다른 조례의 폐지) 「가나」는 폐지한다.\n"
        )
    }

    #[test]
    fn general_rule_confirms_entity_label_in_accumulated_addenda() {
        let text = accumulated("&lt;2014. 10. 21 조례 제2556호&gt;", PLAIN);
        let (status, date, evidence) = outcome(
            "ordinances",
            &revision("조례", "일부개정", "20141021", 2556, &text),
        );
        assert_eq!(
            (status.as_str(), date.as_deref()),
            ("confirmed", Some("2014-10-21"))
        );
        assert_eq!(evidence["decision"], "ordinance-attributed-promulgation");
        assert_eq!(evidence["attributed_by"], json!(["date", "number"]));
        let decoded = text.replace("&lt;", "<").replace("&gt;", ">");
        let (status, date, _) = outcome(
            "ordinances",
            &revision("조례", "일부개정", "20141021", 2556, &decoded),
        );
        assert_eq!(
            (status.as_str(), date.as_deref()),
            ("confirmed", Some("2014-10-21"))
        );
    }

    #[test]
    fn general_rule_reads_revision_prefix_and_keeps_application_timing_apart() {
        let text = "부칙 (1998.1.31 조례 제370호) 제1조(시행일) 이 조례는 공포한 날부터 시행한다.\n제2조(요금) 징수는 공포한 달의 익월 1일부터 시행한다.\n부칙 개정(1999.12.31 조례 제459호) 제1조(시행일) 이 조례는 공포한 날부터 시행한다.\n제2조(주차요금의 조정시기) 주차요금 징수는 2000년 2월 1일부터 시행한다.\n";
        let (status, date, evidence) = outcome(
            "ordinances",
            &revision("조례", "일부개정", "19991231", 459, text),
        );
        assert_eq!(
            (status.as_str(), date.as_deref()),
            ("confirmed", Some("1999-12-31"))
        );
        let timing = evidence["separate_application_timing"].to_string();
        assert!(timing.contains("2000년 2월 1일부터 시행한다"), "{timing}");
        assert!(!timing.contains("1998"), "{timing}");
    }

    #[test]
    fn general_rule_confirms_repeal_revision_and_rule_subject() {
        let text = "부칙 <제2865호, 2011.2.18> 이 규칙은 공포한 날부터 시행한다.\n";
        let (status, date, _) = outcome(
            "ordinances",
            &revision("규칙", "폐지", "20110218", 2865, text),
        );
        assert_eq!(
            (status.as_str(), date.as_deref()),
            ("confirmed", Some("2011-02-18"))
        );
        let (status, date, _) = outcome(
            "ordinances",
            &revision("조례", "폐지", "20110218", 2865, text),
        );
        assert_eq!((status.as_str(), date), ("unresolved", None));
    }

    #[test]
    fn attribution_needs_a_label_that_matches_and_does_not_contradict() {
        for (label, expected) in [
            ("<2014. 10. 21>", "confirmed"),
            ("<조례 제2556호>", "confirmed"),
            ("<2014. 10. 21 조례 제2556호>", "confirmed"),
            ("<2014. 10. 21 조례 제9999호>", "unresolved"),
            ("<2013. 10. 21 조례 제2556호>", "unresolved"),
            // A label whose date and number both differ belongs to another amendment.
            ("<2013. 10. 21 조례 제9999호>", "unresolved"),
            // A single addendum with an earlier label date takes the promulgation date.
            ("<2013. 10. 21>", "confirmed"),
            ("", "unresolved"),
        ] {
            let text = format!("부칙 {label} 제1조(시행일) 이 조례는 공포한 날부터 시행한다.\n");
            let (status, _, _) = outcome(
                "ordinances",
                &revision("조례", "일부개정", "20141021", 2556, &text),
            );
            assert_eq!(status, expected, "label {label:?}");
        }
        let twice = "부칙 <2014. 10. 21> 이 조례는 공포한 날부터 시행한다.\n부칙 <2014. 10. 21> 이 조례는 공포한 날부터 시행한다.\n";
        let (status, _, _) = outcome(
            "ordinances",
            &revision("조례", "일부개정", "20141021", 2556, twice),
        );
        assert_eq!(status, "unresolved");
    }

    #[test]
    fn general_rule_rejects_conditions_relative_periods_and_other_kinds() {
        for clause in [
            "이 조례는 공포 후 1개월이 경과한 날부터 시행한다.",
            "이 조례는 공포한 날부터 시행한다. 다만, 제3조는 2020년 1월 1일부터 시행한다.",
            "이 조례는 공포한 날부터 시행하되, 제3조는 2020년 1월 1일부터 시행한다.",
            "이 조례는 공포한 날부터 시행한다.\n이 조례는 2020년 1월 1일부터 시행한다.",
        ] {
            let text = format!("부칙 <2014. 10. 21 조례 제2556호> {clause}\n");
            let (status, _, _) = outcome(
                "ordinances",
                &revision("조례", "일부개정", "20141021", 2556, &text),
            );
            assert_eq!(status, "unresolved", "{clause}");
        }
        let text = format!("부칙 <2014. 10. 21 조례 제2556호> {PLAIN}\n");
        for (dataset, category, amendment) in [
            ("admrules", "조례", "일부개정"),
            ("ordinances", "훈령", "일부개정"),
        ] {
            let (status, _, _) = outcome(
                dataset,
                &revision(category, amendment, "20141021", 2556, &text),
            );
            assert_ne!(status, "confirmed", "{dataset} {category} {amendment}");
        }
    }

    fn label_outcome(
        category: &str,
        amendment: &str,
        promulgation: &str,
        text: &str,
    ) -> (String, Option<String>, Value) {
        outcome(
            "ordinances",
            &revision(category, amendment, promulgation, 2556, text),
        )
    }

    #[test]
    fn single_label_date_wins_when_later_and_yields_to_an_earlier_promulgation() {
        let clause = "제1조(시행일) 이 조례는 공포한 날부터 시행한다.";
        for (label, expected, side) in [
            ("(2010.7.1)", "2010-07-01", "label"),
            ("(2010. 6. 1.)", "2010-06-01", "promulgation"),
            ("<2004.6.10>", "2010-06-01", "promulgation"),
            ("&lt;2011. 1. 1&gt;", "2011-01-01", "label"),
        ] {
            for amendment in ["제정", "전부개정", "일부개정", "폐지"] {
                let text = format!("부칙 {label} {clause}\n");
                let (status, date, evidence) = label_outcome("조례", amendment, "20100601", &text);
                assert_eq!(
                    (status.as_str(), date.as_deref()),
                    ("confirmed", Some(expected)),
                    "{label} {amendment}"
                );
                assert_eq!(evidence["decision"], "single-label-date-priority-20261103");
                assert_eq!(evidence["applied_side"], side, "{label}");
                assert_eq!(
                    evidence["attributed_label"],
                    label.replace("&lt;", "<").replace("&gt;", ">")
                );
            }
        }
        let (_, _, evidence) = label_outcome(
            "조례",
            "일부개정",
            "20100601",
            "부칙 (2010.7.1) 이 조례는 공포한 날부터 시행한다.\n",
        );
        assert_eq!(evidence["label_date"], "2010-07-01");
    }

    #[test]
    fn single_label_rule_reads_the_real_label_shapes_and_the_rule_subject() {
        // MST 847263 and 806099 shapes: wide spaces, an old enactment label, extra sections text.
        let (status, date, evidence) = label_outcome(
            "규칙",
            "일부개정",
            "20060628",
            "　　 부　 칙 (2004.6.10) 이 규칙은 공포한 날부터 시행한다.\n",
        );
        assert_eq!(
            (status.as_str(), date.as_deref()),
            ("confirmed", Some("2006-06-28"))
        );
        assert_eq!(evidence["applied_side"], "promulgation");
        let (status, date, _) = label_outcome(
            "조례",
            "일부개정",
            "20080321",
            "부   칙 (2003.01.06) ①(시행일) 이 조례는 공포한 날부터 시행한다.\n②(경과조치) 이 조례 시행전에 민간위탁된 사무는 이 조례에 의하여 위탁된 것으로 본다.\n",
        );
        assert_eq!(
            (status.as_str(), date.as_deref()),
            ("confirmed", Some("2008-03-21"))
        );
        // The sentence subject must match the document kind.
        let (status, _, _) = label_outcome(
            "규칙",
            "일부개정",
            "20060628",
            "부칙 (2007.1.1) 이 조례는 공포한 날부터 시행한다.\n",
        );
        assert_eq!(status, "unresolved");
    }

    #[test]
    fn single_label_rule_leaves_other_addenda_to_the_older_rules() {
        let plain = "이 조례는 공포한 날부터 시행한다.";
        // Accumulated addenda: the label rule needs exactly one addendum, so these stay unresolved
        // unless the attribution rule finds the revision's own label.
        let cumulative = format!("부칙 (2003.1.6) {plain}\n부칙 (2005.2.2) {plain}\n");
        assert_eq!(
            label_outcome("조례", "일부개정", "20100601", &cumulative).0,
            "unresolved"
        );
        // Date and number disagree about the revision: contradiction, unresolved.
        for label in ["<2010. 7. 1 조례 제2556호>", "<2010. 6. 1 조례 제9999호>"] {
            let text = format!("부칙 {label} {plain}\n");
            let (status, _, _) = outcome(
                "ordinances",
                &revision("조례", "일부개정", "20100601", 2556, &text),
            );
            assert_eq!(status, "unresolved", "{label}");
        }
        // Conditions, relative periods and provisos never use the label date.
        for clause in [
            "이 조례는 공포 후 1개월이 경과한 날부터 시행한다.",
            "이 조례는 규칙이 정하는 날부터 시행한다.",
            "이 조례는 공포한 날부터 시행한다. 다만, 제3조는 2020년 1월 1일부터 시행한다.",
        ] {
            let text = format!("부칙 (2010.7.1) {clause}\n");
            assert_eq!(
                label_outcome("조례", "일부개정", "20100601", &text).0,
                "unresolved",
                "{clause}"
            );
        }
        // Not for administrative rules, other document kinds, or a label without a date.
        let text = format!("부칙 (2010.7.1) {plain}\n");
        let source = revision("조례", "일부개정", "20100601", 2556, &text);
        assert_eq!(outcome("admrules", &source).0, "unresolved");
        assert_eq!(
            label_outcome("훈령", "일부개정", "20100601", &text).0,
            "unresolved"
        );
        assert_eq!(
            label_outcome("조례", "일부개정", "20100601", &format!("부칙 {plain}\n")).0,
            "unresolved"
        );
    }

    #[test]
    fn label_with_both_date_and_number_different_is_another_amendment() {
        // Like fixture 9100016: label number 1499 and 2010-12-01 against a 1501 / 2011-01-01 revision.
        let plain = "이 조례는 공포한 날부터 시행한다.";
        for label in ["<제1499호, 2010.12.1>", "<제1499호, 2011.3.1>"] {
            let text = format!("부칙 {label} {plain}\n");
            let source = revision("조례", "일부개정", "20110101", 1501, &text);
            let found = resolve("ordinances", &source).unwrap();
            assert_eq!(found.status, "unresolved", "{label}");
            assert_eq!(found.enforcement_date, None, "{label}");
            assert_eq!(
                found.date_evidence["user_decision"], LABEL_BOTH_DIFFER_DECISION,
                "{label}"
            );
            assert_eq!(found.date_evidence["approved_rule"], "label-both-differ");
        }
        // A label with a date only keeps the label-date rule.
        let text = format!("부칙 <2011.3.1> {plain}\n");
        let source = revision("조례", "일부개정", "20110101", 1501, &text);
        assert_eq!(
            outcome("ordinances", &source).1.as_deref(),
            Some("2011-03-01")
        );
    }

    #[test]
    fn number_only_label_that_differs_stays_unresolved_by_the_approved_rule() {
        let plain = "이 조례는 공포한 날부터 시행한다.";
        for amendment in ["제정", "일부개정", "폐지"] {
            let text = format!("부칙 <제9999호> {plain}\n");
            let source = revision("조례", amendment, "20110101", 1501, &text);
            let found = resolve("ordinances", &source).unwrap();
            assert_eq!(found.status, "unresolved", "{amendment}");
            assert_eq!(found.enforcement_date, None);
            assert!(
                found.reason.contains("number-only-mismatch"),
                "{}",
                found.reason
            );
            assert_eq!(found.date_evidence["approved_rule"], "number-only-mismatch");
            assert_eq!(
                found.date_evidence["user_decision"],
                ADDENDUM_RULES_DECISION
            );
            assert!(found.date_evidence.get("lead_default").is_none());
        }
        // A number that matches the revision is still attributed to it.
        let text = format!("부칙 <제1501호> {plain}\n");
        let source = revision("조례", "일부개정", "20110101", 1501, &text);
        assert_eq!(outcome("ordinances", &source).0, "confirmed");
        // A conditional sentence is not this case and keeps its old result.
        let text = "부칙 <제9999호> 이 조례는 규칙이 정하는 날부터 시행한다.\n";
        let source = revision("조례", "일부개정", "20110101", 1501, text);
        let found = resolve("ordinances", &source).unwrap();
        assert!(!found.reason.contains("lead-default"));
    }

    #[test]
    fn amendments_by_another_ordinance_and_bulk_amendments_are_excluded_from_both_rules() {
        let plain = "이 조례는 공포한 날부터 시행한다.";
        let single = format!("부칙 (2010.7.1) {plain}\n");
        let cumulative = format!("부칙 (2003.1.6) {plain}\n부칙 <제2556호, 2014.10.21> {plain}\n");
        for amendment in ["타법개정", "일괄개정", "타법폐지"] {
            for (text, promulgation) in [(&single, "20100601"), (&cumulative, "20141021")] {
                let source = revision("조례", amendment, promulgation, 2556, text);
                let found = resolve("ordinances", &source).unwrap();
                assert_eq!(found.status, "unresolved", "{amendment} {text}");
                assert!(
                    found.reason.contains("(amendment-by-other-excluded)"),
                    "{amendment}: {}",
                    found.reason
                );
                assert_eq!(
                    found.date_evidence["user_decision"],
                    ADDENDUM_RULES_DECISION
                );
                assert_eq!(found.date_evidence["excluded_amendment"], amendment);
            }
        }
        // The same texts resolve for the kinds the decisions name.
        for amendment in ["제정", "전부개정", "일부개정", "폐지", "폐지제정"] {
            let source = revision("조례", amendment, "20100601", 2556, &single);
            assert_eq!(
                resolve("ordinances", &source).unwrap().status,
                "confirmed",
                "{amendment}"
            );
        }
        for amendment in ["일부개정", "폐지"] {
            let source = revision("조례", amendment, "20141021", 2556, &cumulative);
            assert_eq!(
                resolve("ordinances", &source).unwrap().status,
                "confirmed",
                "{amendment}"
            );
        }
        // A text neither rule would resolve gets no exclusion marker.
        let source = revision(
            "조례",
            "타법개정",
            "20100601",
            2556,
            "부칙 이 조례는 규칙이 정하는 날부터 시행한다.\n",
        );
        let found = resolve("ordinances", &source).unwrap();
        assert!(!found.reason.contains("lead-default"), "{}", found.reason);
    }

    #[test]
    fn label_date_report_lists_label_applications_in_enforcement_only() {
        let text = |label: &str| format!("부칙 {label} 이 조례는 공포한 날부터 시행한다.\n");
        let build = |enforcement: bool| {
            let mut sources = Vec::new();
            for (id, identity, promulgation, label) in [
                ("1", "id1", "20100601", "(2010.7.1)"),
                ("2", "id2", "20100601", "(2010.6.1)"),
                ("3", "id3", "20100601", "(2009.1.1)"),
            ] {
                let mut source = revision("조례", "일부개정", promulgation, 1, &text(label));
                source.revision = id.to_owned();
                source.identity = identity.to_owned();
                source.canonical_path = format!("p{id}.md");
                source.source_path = format!("{id}.xml");
                source.serial = id.parse().unwrap();
                source.markdown = format!("---\nname: {id}\n---\nbody {id}\n").into_bytes();
                sources.push(source);
            }
            DatePlan::new("ordinances", enforcement, sources).unwrap()
        };
        let report = build(true).label_date_report().unwrap();
        let entries = report["label_date_applied"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["identity"], "id1");
        assert_eq!(entries[0]["revision"], "1");
        assert_eq!(entries[0]["promulgation_date"], "2010-06-01");
        assert_eq!(entries[0]["label_date"], "2010-07-01");
        assert_eq!(entries[0]["days_after_promulgation"], 30);
        assert_eq!(entries[0]["attributed_label"], "(2010.7.1)");

        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("out.git");
        let index = temp.path().join("index.json");
        let mut plan = build(true);
        let (head, staged) = plan.replay(&output, b"readme").unwrap();
        plan.publish_enforcement(staged, &head, &index, json!({}))
            .unwrap();
        let written: Value = serde_json::from_slice(
            &fs::read(temp.path().join("out.git.label-date-applied.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(written, report);
        // The default basis writes no report and no tree files.
        let default_output = temp.path().join("default.git");
        let default_index = temp.path().join("default.json");
        build(false)
            .publish(Some(&default_output), "", &default_index, json!({}))
            .unwrap();
        assert!(
            !temp
                .path()
                .join("default.git.label-date-applied.json")
                .exists()
        );
        assert!(default_index.exists());
        // An enforcement build without labels still writes an empty list.
        let empty_output = temp.path().join("empty.git");
        let mut empty = small_plan_without_labels();
        let (head, staged) = empty.replay(&empty_output, b"readme").unwrap();
        empty
            .publish_enforcement(staged, &head, &temp.path().join("empty.json"), json!({}))
            .unwrap();
        let none: Value = serde_json::from_slice(
            &fs::read(temp.path().join("empty.git.label-date-applied.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(none["label_date_applied"], json!([]));
    }

    fn small_plan_without_labels() -> DatePlan {
        let mut source = revision(
            "조례",
            "일부개정",
            "20200101",
            1,
            "부칙 (2020.1.1) 이 조례는 공포한 날부터 시행한다.\n",
        );
        source.markdown = b"---\nname: x\n---\nbody\n".to_vec();
        DatePlan::new("ordinances", true, vec![source]).unwrap()
    }

    #[test]
    fn enforcement_recovers_owned_staging_after_placement_failure() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("repo.git");
        let manifest = temp.path().join("manifest.json");
        let diagnostic = temp.path().join("repo.git.label-date-applied.json");
        let mut plan = small_plan_without_labels();
        let (head, staged) = plan.replay(&output, b"readme").unwrap();
        let staging = staged.owned.clone();
        let error = plan
            .publish_enforcement_with_hook(staged, &head, &manifest, json!({}), |stage| {
                if stage == "after_place" {
                    anyhow::bail!("injected failure after place");
                }
                Ok(())
            })
            .unwrap_err();
        assert!(format!("{error:#}").contains("injected failure after place"));
        assert!(!output.exists());
        assert!(!staging.exists());
        assert!(!diagnostic.exists());
        assert!(!manifest.exists());
    }

    #[test]
    fn enforcement_does_not_remove_contents_added_after_reservation() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("repo.git");
        let manifest = temp.path().join("manifest.json");
        let mut plan = small_plan_without_labels();
        let (head, staged) = plan.replay(&output, b"readme").unwrap();
        let staging = staged.owned.clone();
        let error = plan
            .publish_enforcement_with_hook(staged, &head, &manifest, json!({}), |stage| {
                if stage == "after_reserve" {
                    fs::write(output.join("other"), b"keep")?;
                }
                Ok(())
            })
            .unwrap_err();
        assert!(format!("{error:#}").contains("cannot place repository"));
        assert_eq!(fs::read(output.join("other")).unwrap(), b"keep");
        assert!(!staging.exists());
        assert!(!manifest.exists());
        assert!(format!("{error:#}").contains(&output.display().to_string()));
    }

    #[test]
    fn enforcement_preserves_a_preexisting_diagnostic_temporary_file() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("repo.git");
        let manifest = temp.path().join("manifest.json");
        let temporary = temp.path().join(format!(
            "repo.git.label-date-applied.json.tmp.{}",
            std::process::id()
        ));
        fs::write(&temporary, b"existing").unwrap();
        let mut plan = small_plan_without_labels();
        let (head, staged) = plan.replay(&output, b"readme").unwrap();
        let staging = staged.owned.clone();
        assert!(
            plan.publish_enforcement(staged, &head, &manifest, json!({}))
                .is_err()
        );
        assert_eq!(fs::read(&temporary).unwrap(), b"existing");
        assert!(!staging.exists());
        assert!(!output.exists());
        assert!(!manifest.exists());
    }

    #[test]
    fn enforcement_refuses_an_empty_final_directory_created_before_publication() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("repo.git");
        let manifest = temp.path().join("manifest.json");
        let mut plan = small_plan_without_labels();
        let (head, staged) = plan.replay(&output, b"readme").unwrap();
        let staging = staged.owned.clone();
        fs::create_dir(&output).unwrap();
        let error = plan
            .publish_enforcement(staged, &head, &manifest, json!({}))
            .unwrap_err();
        assert!(format!("{error:#}").contains("cannot reserve"), "{error:#}");
        assert!(output.is_dir() && fs::read_dir(&output).unwrap().next().is_none());
        assert!(!staging.exists());
        assert!(!manifest.exists());
    }

    #[test]
    fn enforcement_preserves_a_preexisting_staging_path() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("repo.git");
        let staging = BareRepoWriter::staging_path(&output).unwrap();
        fs::create_dir(&staging).unwrap();
        fs::write(staging.join("keep"), b"existing").unwrap();
        assert!(BareRepoWriter::create_exclusive(&output).is_err());
        assert_eq!(fs::read(staging.join("keep")).unwrap(), b"existing");
        assert!(!output.exists());
    }

    #[test]
    fn explicit_written_date_is_kept_even_when_earlier_than_promulgation() {
        // "적힌 날짜 그대로": a first enactment whose only addendum names an earlier date.
        let text = "제1조(시행일) 이 조례는 2009년 1월 1일부터 시행한다.\n";
        let (status, date, evidence) =
            outcome("ordinances", &revision("조례", "제정", "20100601", 1, text));
        assert_eq!(
            (status.as_str(), date.as_deref()),
            ("confirmed", Some("2009-01-01"))
        );
        assert_eq!(evidence["rule"], "approved clause in selected addendum");
    }

    #[test]
    fn both_bases_order_by_their_own_date_and_replay_commits_in_event_order() {
        // A is promulgated first but takes effect last; B and C share both dates.
        let mut sources = Vec::new();
        for (id, promulgation, enforcement) in [
            ("1", "20200101", "20300101"),
            ("3", "20210101", "20220101"),
            ("2", "20210101", "20220101"),
        ] {
            let mut source = revision("조례", "일부개정", promulgation, 1, "");
            source.revision = id.to_owned();
            source.identity = format!("id{id}");
            source.canonical_path = format!("p{id}.md");
            source.source_path = format!("{id}.xml");
            source.enforcement = enforcement.to_owned();
            source.serial = id.parse().unwrap();
            source.markdown = format!("---\nname: {id}\n---\nbody {id}\n").into_bytes();
            sources.push(source);
        }
        let mut plan = DatePlan::new("ordinances", true, sources).unwrap();
        let ranks = |plan: &DatePlan| -> Vec<(String, u64, u64)> {
            let mut found: Vec<_> = plan
                .revisions
                .iter()
                .map(|revision| {
                    let occurrence = &revision.occurrences[0];
                    (
                        revision.revision.clone(),
                        occurrence.promulgation_rank.unwrap(),
                        occurrence.enforcement_rank.unwrap(),
                    )
                })
                .collect();
            found.sort();
            found
        };
        // Publication order is A, then B (serial 2), then C (serial 3).
        // Effect order is B, then C, then A.
        assert_eq!(
            ranks(&plan),
            vec![("1".into(), 0, 2), ("2".into(), 1, 0), ("3".into(), 2, 1)]
        );
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("out.git");
        let (_, staged) = plan.replay(&output, b"readme").unwrap();
        let log = String::from_utf8(
            git(
                &staged.owned,
                &["log", "--reverse", "--format=%x01%aI%x02%b"],
            )
            .unwrap(),
        )
        .unwrap();
        let events: Vec<(String, String)> = log
            .split('\u{1}')
            .filter(|entry| entry.contains("revision: "))
            .map(|entry| {
                let (date, body) = entry.split_once('\u{2}').unwrap();
                let revision = body
                    .lines()
                    .next()
                    .unwrap()
                    .trim_start_matches("revision: ");
                (date.to_owned(), revision.to_owned())
            })
            .collect();
        assert_eq!(
            events,
            vec![
                ("2022-01-01T12:00:00+09:00".to_owned(), "2".to_owned()),
                ("2022-01-01T12:00:00+09:00".to_owned(), "3".to_owned()),
                ("2030-01-01T12:00:00+09:00".to_owned(), "1".to_owned()),
            ]
        );
        let default = DatePlan::new("ordinances", false, plan.sources.clone()).unwrap();
        assert_eq!(default.basis, "promulgation");
        assert_eq!(ranks(&default), ranks(&plan));
    }

    #[test]
    fn label_contradiction_stays_unresolved_for_every_amendment_kind() {
        let plain = "이 조례는 공포한 날부터 시행한다.";
        // The Codex counterexample: a first enactment whose label date differs but whose
        // number matches. The older first-enactment branch must not confirm it again.
        for amendment in [
            "제정",
            "전부개정",
            "일부개정",
            "폐지",
            "타법개정",
            "일괄개정",
        ] {
            for label in ["<2010. 7. 1 조례 제2556호>", "<2010. 6. 1 조례 제9999호>"] {
                let text = format!("부칙 {label} {plain}\n");
                let found = resolve(
                    "ordinances",
                    &revision("조례", amendment, "20100601", 2556, &text),
                )
                .unwrap();
                assert_eq!(found.status, "unresolved", "{amendment} {label}");
                assert_eq!(found.enforcement_date, None, "{amendment} {label}");
                assert!(
                    found.reason.contains("(label-contradiction)"),
                    "{}",
                    found.reason
                );
                assert_eq!(
                    found.date_evidence["user_decision"],
                    ADDENDUM_RULES_DECISION
                );
            }
        }
        // A written explicit date is not this case and keeps its written date.
        let explicit = "부칙 <2010. 7. 1 조례 제2556호> 이 조례는 2009년 1월 1일부터 시행한다.\n";
        let (status, date, _) = label_outcome("조례", "제정", "20100601", explicit);
        assert_eq!(
            (status.as_str(), date.as_deref()),
            ("confirmed", Some("2009-01-01"))
        );
    }

    #[test]
    fn relative_period_sentences_are_unresolved_except_the_approved_revision() {
        let month = "이 조례는 공포 후 1개월이 경과한 날부터 시행한다.";
        for (dataset, amendment) in [
            ("ordinances", "제정"),
            ("ordinances", "전부개정"),
            ("ordinances", "일부개정"),
        ] {
            let found = resolve(
                dataset,
                &revision(
                    "조례",
                    amendment,
                    "20071031",
                    1,
                    &format!("제1조(시행일) {month}\n"),
                ),
            )
            .unwrap();
            assert_eq!(found.status, "unresolved", "{amendment}");
            assert_eq!(found.enforcement_date, None, "{amendment}");
        }
        // The same sentence in a source body (no addendum) is not read either.
        let mut law = revision("조례", "제정", "20071031", 1, "");
        law.xml =
            "<법령><조문내용>이 법은 공포 후 1개월이 경과한 날부터 시행한다.</조문내용></법령>"
                .as_bytes()
                .to_vec();
        assert_eq!(resolve("laws", &law).unwrap().status, "unresolved");
        // The approved revision keeps its approved date.
        let mut approved = revision(
            "조례",
            "일부개정",
            "20071031",
            1,
            &format!("제1조(시행일) {month}\n"),
        );
        approved.revision = "1000238".to_owned();
        let found = resolve("ordinances", &approved).unwrap();
        assert_eq!(found.enforcement_date.as_deref(), Some("2007-12-01"));
    }

    #[test]
    fn a_proviso_after_the_sentence_blocks_every_branch_but_other_subjects_do_not() {
        let plain = "이 조례는 공포한 날부터 시행한다.";
        for amendment in ["제정", "전부개정"] {
            for tail in [
                " 다만, 제3조는 2020년 1월 1일부터 시행한다.",
                "\n다만, 제3조는 2020년 1월 1일부터 시행한다.",
            ] {
                let text = format!("제1조(시행일) {plain}{tail}\n");
                let found = resolve(
                    "ordinances",
                    &revision("조례", amendment, "20100601", 1, &text),
                )
                .unwrap();
                assert_eq!(found.status, "unresolved", "{amendment} {tail:?}");
            }
        }
        // The body branch reads the same function, so the proviso blocks it as well.
        let mut body = revision("조례", "제정", "20100601", 1, "");
        body.xml = "<법령><조문내용>이 법은 공포한 날부터 시행한다. 다만, 제3조는 2020년 1월 1일부터 시행한다.</조문내용></법령>"
            .as_bytes()
            .to_vec();
        assert_eq!(resolve("laws", &body).unwrap().status, "unresolved");
        // A separate article about another subject is an application time, not a proviso.
        let text = format!(
            "제1조(시행일) {plain}\n제2조(요금) 주차요금 징수는 2000년 2월 1일부터 시행한다.\n"
        );
        let found = resolve(
            "ordinances",
            &revision("조례", "제정", "20100601", 1, &text),
        )
        .unwrap();
        assert_eq!(found.enforcement_date.as_deref(), Some("2010-06-01"));
        let labelled = format!("부칙 (2010.6.1) {text}");
        let (status, date, evidence) = label_outcome("조례", "일부개정", "20100601", &labelled);
        assert_eq!(
            (status.as_str(), date.as_deref()),
            ("confirmed", Some("2010-06-01"))
        );
        assert!(
            evidence["separate_application_timing"]
                .to_string()
                .contains("2000년 2월 1일부터 시행한다")
        );
    }

    #[test]
    fn enforcement_commits_carry_an_effective_date_trailer_even_before_1970() {
        let mut source = revision("조례", "일부개정", "19600101", 1, "");
        source.enforcement = "19600810".to_owned();
        source.markdown = b"---\nname: x\n---\nbody\n".to_vec();
        let mut plan = DatePlan::new("ordinances", true, vec![source]).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("out.git");
        let (_, staged) = plan.replay(&output, b"readme").unwrap();
        let found = String::from_utf8(
            git(
                &staged.owned,
                &[
                    "log",
                    "--format=%aI %s",
                    "--grep=^Effective-Date: 1960-08-10$",
                ],
            )
            .unwrap(),
        )
        .unwrap();
        // The source commit and the snapshot commit are both found by the trailer, although
        // Git timestamps stop at 1970.
        assert_eq!(found.lines().count(), 2, "{found}");
        assert!(
            found.lines().all(|line| line.starts_with("1970-01-01T")),
            "{found}"
        );
        let none = git(
            &staged.owned,
            &["log", "--format=%H", "--grep=^Effective-Date: 1960-08-11$"],
        )
        .unwrap();
        assert!(none.is_empty());
    }

    /// Builds an ordinance source whose addendum and articles are empty and whose amendment
    /// text holds the given lines, one per CDATA part as the real files do.
    fn amendment_text_source(amendment: &str, lines: &[&str]) -> SourceRevision {
        let parts: String = lines
            .iter()
            .map(|line| format!("<![CDATA[{line}]]>\n"))
            .collect();
        let mut source = revision("조례", amendment, "20041105", 702, "");
        source.xml = format!(
            "<LawService><공포번호>702</공포번호><조문></조문><부칙></부칙><제개정이유><제개정이유내용>\n{parts}</제개정이유내용></제개정이유></LawService>"
        )
        .into_bytes();
        source
    }

    #[test]
    fn amendment_text_addendum_is_read_when_addendum_and_articles_name_no_date() {
        let enactment = [
            "개정문",
            "부       칙 ",
            "이 조례는 공포한 날부터 시행한다. ",
            "□ 개정이유 ",
            "이 조례는 시행 전 의견을 듣는다.",
        ];
        // A first enactment reads like an unlabelled single addendum: the day of promulgation.
        let found = resolve("ordinances", &amendment_text_source("제정", &enactment)).unwrap();
        assert_eq!(found.enforcement_date.as_deref(), Some("2004-11-05"));
        assert_eq!(found.date_evidence["addendum_source"], "제개정이유내용");
        // A partial amendment follows the same rule as an unlabelled single addendum.
        let found = resolve("ordinances", &amendment_text_source("일부개정", &enactment)).unwrap();
        assert_eq!(found.status, "unresolved");
        assert_eq!(found.date_evidence["addendum_source"], "제개정이유내용");
        assert!(!found.reason.contains("21 days"), "{}", found.reason);
        // A labelled heading follows the label rules (here a later label date wins).
        let labelled = [
            "부칙 (2004.12.1)",
            "이 조례는 공포한 날부터 시행한다.",
            "□ 개정이유",
        ];
        let found = resolve("ordinances", &amendment_text_source("일부개정", &labelled)).unwrap();
        assert_eq!(found.enforcement_date.as_deref(), Some("2004-12-01"));
        // Excluded amendment kinds, provisos and relative periods stay unresolved as before.
        for lines in [
            vec![
                "부 칙",
                "이 조례는 공포한 날부터 시행한다. 다만, 제3조는 따로 정한다.",
            ],
            vec!["부 칙", "이 조례는 공포 후 1개월이 경과한 날부터 시행한다."],
        ] {
            let found = resolve("ordinances", &amendment_text_source("제정", &lines)).unwrap();
            assert_eq!(found.status, "unresolved", "{lines:?}");
        }
        let found = resolve("ordinances", &amendment_text_source("타법개정", &labelled)).unwrap();
        assert_eq!(found.status, "unresolved");
    }

    #[test]
    fn default_period_applies_only_when_no_text_names_a_date_and_not_to_empty_sources() {
        // Text exists but names no effective date: the approved default period still applies.
        let plain = amendment_text_source("일부개정", &["개정문", "□ 개정이유", "이유 설명"]);
        let found = resolve("ordinances", &plain).unwrap();
        assert_eq!(found.enforcement_date.as_deref(), Some("2004-11-26"));
        assert!(found.reason.contains("21 days"), "{}", found.reason);
        // A revision with an empty addendum and articles but amendment text keeps the period too
        // when its heading has no effective sentence.
        let heading_only = amendment_text_source("일부개정", &["부 칙", "경과조치를 둔다."]);
        assert!(
            resolve("ordinances", &heading_only)
                .unwrap()
                .reason
                .contains("21 days")
        );
        // Nothing at all: no default period, the revision stays unresolved.
        let mut empty = revision("조례", "폐지", "20000418", 274, "");
        empty.xml = "<LawService><조문></조문><부칙></부칙></LawService>"
            .as_bytes()
            .to_vec();
        let found = resolve("ordinances", &empty).unwrap();
        assert_eq!(found.status, "unresolved");
        assert_eq!(found.enforcement_date, None);
        assert_eq!(found.date_evidence["empty_source"], true);
        // Only the addendum is empty and the articles hold text: the default period applies.
        let mut body_only = revision("조례", "일부개정", "20000418", 274, "");
        body_only.xml = "<LawService><조문><조><조내용>제1조 목적</조내용></조></조문><부칙></부칙></LawService>"
            .as_bytes()
            .to_vec();
        assert_eq!(
            resolve("ordinances", &body_only)
                .unwrap()
                .enforcement_date
                .as_deref(),
            Some("2000-05-09")
        );
    }

    #[test]
    fn copies_rank_by_preference_and_conflicting_copies_stay_unresolved() {
        let build = |pad: &str, conflict: bool| {
            let mut sources = Vec::new();
            for (priority, path) in [(1_u8, "7.xml"), (2_u8, "history/7.xml")] {
                let mut source = revision("조례", "일부개정", "20100601", 1, "");
                source.revision = "7".to_owned();
                source.identity = "id7".to_owned();
                source.source_path = path.to_owned();
                source.copy_priority = priority;
                source.enforcement = "20100701".to_owned();
                source.xml = format!("<LawService><공포번호>1</공포번호><조문내용>{path}{pad}</조문내용></LawService>").into_bytes();
                source.markdown = format!("---\nname: {path}\n---\n").into_bytes();
                source.copy_conflict = conflict.then(|| "content differs at 조문내용".to_owned());
                sources.push(source);
            }
            DatePlan::new("ordinances", false, sources).unwrap()
        };
        // The copy hash changes with the padding, but the preferred copy always ranks last.
        for pad in ["", "a", "bb", "ccc", "dddd", "eeeee"] {
            let plan = build(pad, false);
            let rank = |path: &str| {
                plan.revisions
                    .iter()
                    .flat_map(|revision| &revision.occurrences)
                    .find(|occurrence| occurrence.source_path == path)
                    .unwrap()
                    .promulgation_rank
                    .unwrap()
            };
            assert!(rank("history/7.xml") > rank("7.xml"), "{pad:?}");
        }
        let plan = build("", true);
        for revision in &plan.revisions {
            let occurrence = &revision.occurrences[0];
            assert_eq!(occurrence.status, "unresolved");
            assert_eq!(occurrence.enforcement_date, None);
            assert_eq!(
                occurrence.date_evidence["decision"],
                "duplicate-copy-conflict"
            );
            assert!(occurrence.reason.contains("copies of this revision differ"));
        }
    }

    #[test]
    fn a_broken_promulgation_date_with_a_valid_effective_date_is_undated_for_promulgation_lookups()
    {
        let mut source = revision("조례", "일부개정", "1999.1  ", 1, "");
        source.enforcement = "19990101".to_owned();
        let found = resolve("ordinances", &source).unwrap();
        assert_eq!(found.status, "confirmed");
        assert_eq!(found.enforcement_date.as_deref(), Some("1999-01-01"));
        assert_eq!(found.promulgation_date, None);
        assert_eq!(found.undated_semantics, vec!["공포일자".to_owned()]);
        let value = serde_json::to_value(&found).unwrap();
        assert_eq!(value["undated_semantics"], json!(["공포일자"]));
        // Both dates unusable: undated for both lookups.
        let mut both = revision("조례", "일부개정", "00000000", 1, "");
        both.enforcement = "99991231".to_owned();
        let found = resolve("ordinances", &both).unwrap();
        assert_eq!(found.status, "undated");
        assert_eq!(
            found.undated_semantics,
            vec!["공포일자".to_owned(), "시행일자".to_owned()]
        );
        // A normal revision carries no such key.
        let mut normal = revision("조례", "일부개정", "20100601", 1, "");
        normal.enforcement = "20100701".to_owned();
        let value = serde_json::to_value(resolve("ordinances", &normal).unwrap()).unwrap();
        assert!(value.get("undated_semantics").is_none());
    }

    #[test]
    fn default_builds_accept_an_index_inside_the_output_and_enforcement_does_not() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("out.git");
        let inside = output.join("index.json");
        DatePlan::check_destination(&output, Some(&inside), false).unwrap();
        let error = DatePlan::check_destination(&output, Some(&inside), true).unwrap_err();
        assert!(
            error.to_string().contains("outside the output repository"),
            "{error}"
        );
    }

    #[test]
    fn the_first_commit_keeps_the_fixed_default_date_in_enforcement_repositories() {
        let mut source = revision("조례", "일부개정", "19600101", 1, "");
        source.enforcement = "19600810".to_owned();
        source.markdown = b"---\nname: x\n---\nbody\n".to_vec();
        let mut plan = DatePlan::new("ordinances", true, vec![source]).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("out.git");
        let (_, staged) = plan.replay(&output, b"readme").unwrap();
        let log = String::from_utf8(
            git(&staged.owned, &["log", "--reverse", "--format=%aI %s"]).unwrap(),
        )
        .unwrap();
        assert_eq!(
            log.lines().next().unwrap(),
            "2026-03-30T12:00:00+09:00 initial commit",
            "{log}"
        );
        // The dated commits follow with their own (clamped) dates.
        assert!(
            log.lines()
                .skip(1)
                .all(|line| line.starts_with("1970-01-01T")),
            "{log}"
        );
    }

    #[test]
    fn a_word_like_enforcement_decree_is_not_an_effective_date_phrase() {
        assert!(!names_effective_date(
            "시행령에 따른다. 시행규칙은 별도로 정한다."
        ));
        assert!(names_effective_date("이 조례는 공포한 날부터 시행한다."));
        assert!(names_effective_date("이 조례는 공포한 날부터\n시행한다."));
        assert!(names_effective_date("공포한 날부터 효력을 발생한다."));
        // An ordinance whose articles only mention such a word gets the approved default period.
        let mut source = revision("조례", "제정", "20070405", 1, "");
        source.xml = "<LawService><조문><조><조내용>동법 시행령에 따른다.</조내용></조></조문><부칙></부칙></LawService>"
            .as_bytes()
            .to_vec();
        let found = resolve("ordinances", &source).unwrap();
        assert_eq!(found.enforcement_date.as_deref(), Some("2007-04-26"));
        // Administrative rules reach the same branches, so an empty-addendum guideline of the
        // arrival-estimate kinds is estimated at its issue date.
        let mut rule = revision("등기예규", "제정", "19980615", 1, "");
        rule.xml = "<AdmRulService><조문내용>시행규칙에 따른다.</조문내용></AdmRulService>"
            .as_bytes()
            .to_vec();
        let found = resolve("admrules", &rule).unwrap();
        assert_eq!(found.status, "estimated");
        assert_eq!(found.enforcement_date.as_deref(), Some("1998-06-15"));
        // A body that does name a phrase is still read by the body branch.
        let mut named = revision("조례", "제정", "20070405", 1, "");
        named.xml = "<LawService><조문><조><조내용>이 조례는 공포한 날부터 시행한다.</조내용></조></조문><부칙></부칙></LawService>"
            .as_bytes()
            .to_vec();
        let found = resolve("ordinances", &named).unwrap();
        assert_eq!(found.enforcement_date.as_deref(), Some("2007-04-05"));
        assert_eq!(found.reason, "approved clause in source body");
    }

    #[test]
    fn amendment_text_addendum_is_read_even_when_articles_mention_the_decree() {
        let mut source =
            amendment_text_source("일부개정", &["부 칙", "이 조례는 공포한 날부터 시행한다."]);
        source.xml = String::from_utf8(source.xml)
            .unwrap()
            .replace(
                "<조문></조문>",
                "<조문><조><조내용>동법 시행령을 따른다.</조내용></조></조문>",
            )
            .into_bytes();
        let found = resolve("ordinances", &source).unwrap();
        assert_eq!(found.date_evidence["addendum_source"], "제개정이유내용");
        assert_eq!(found.status, "unresolved");
        // A labelled addendum in the amendment text follows the attribution and label rules.
        let number = amendment_text_source(
            "일부개정",
            &[
                "부칙 <제702호, 2004.11.5>",
                "이 조례는 공포한 날부터 시행한다.",
            ],
        );
        let found = resolve("ordinances", &number).unwrap();
        assert_eq!(found.enforcement_date.as_deref(), Some("2004-11-05"));
        let contradiction = amendment_text_source(
            "제정",
            &[
                "부칙 <제702호, 2004.12.5>",
                "이 조례는 공포한 날부터 시행한다.",
            ],
        );
        assert_eq!(
            resolve("ordinances", &contradiction).unwrap().status,
            "unresolved"
        );
    }

    #[test]
    fn cjk_and_full_width_label_brackets_read_like_angle_brackets() {
        // MST 1089045 shape: older labelled sections, then the revision's own label in U+3008.
        let addendum = |open: &str, close: &str| {
            format!(
                "부칙 {open}2010. 9. 20 규칙 제688호{close} 제1조(시행일) 이 규칙은 공포한 날부터 시행한다.\n\
                 부칙 {open}2011. 5. 9 규칙 제718호{close} 이 규칙은 공포한 날부터 시행한다.\n"
            )
        };
        for (open, close) in [
            ("<", ">"),
            ("\u{3008}", "\u{3009}"),
            ("\u{2329}", "\u{232A}"),
            ("\u{FF1C}", "\u{FF1E}"),
            ("(", ")"),
            ("\u{FF08}", "\u{FF09}"),
            ("\u{FF08}", ")"),
            ("\u{3008}", ">"),
        ] {
            let (status, date, evidence) = outcome(
                "ordinances",
                &revision("규칙", "일부개정", "20110509", 718, &addendum(open, close)),
            );
            assert_eq!(
                (status.as_str(), date.as_deref()),
                ("confirmed", Some("2011-05-09")),
                "{open}{close}"
            );
            assert_eq!(
                evidence["decision"], "ordinance-attributed-promulgation",
                "{open}{close}"
            );
        }
        // The heading is also read without a space before the label and after "개정".
        let (status, date, _) = outcome(
            "ordinances",
            &revision(
                "규칙",
                "일부개정",
                "20110509",
                718,
                "부칙\u{3008}2011. 5. 9 규칙 제718호\u{3009} 이 규칙은 공포한 날부터 시행한다.\n",
            ),
        );
        assert_eq!(
            (status.as_str(), date.as_deref()),
            ("confirmed", Some("2011-05-09"))
        );
        // Brackets of another kind are not labels.
        let (status, _, _) = outcome(
            "ordinances",
            &revision(
                "규칙",
                "일부개정",
                "20110509",
                718,
                "부칙 [2011. 5. 9 규칙 제718호] 이 규칙은 공포한 날부터 시행한다.\n",
            ),
        );
        assert_eq!(status, "unresolved");
    }

    #[test]
    fn amendment_text_is_read_when_the_addendum_names_no_effective_date() {
        let mut source = amendment_text_source(
            "일부개정",
            &[
                "부칙 <제702호, 2004.11.5>",
                "이 조례는 공포한 날부터 시행한다.",
            ],
        );
        // The addendum holds a transitional provision only, no effective sentence.
        source.xml = String::from_utf8(source.xml)
            .unwrap()
            .replace(
                "<부칙></부칙>",
                "<부칙><부칙내용><![CDATA[제2조(경과조치) 이 조례 시행 전에 한 처분은 종전의 규정에 따른다.]]></부칙내용></부칙>",
            )
            .into_bytes();
        let found = resolve("ordinances", &source).unwrap();
        assert_eq!(found.enforcement_date.as_deref(), Some("2004-11-05"));
        assert_eq!(found.date_evidence["addendum_source"], "제개정이유내용");
        // An addendum that names the effective date is read as it is.
        let mut named = amendment_text_source(
            "일부개정",
            &[
                "부칙 <제702호, 2004.12.5>",
                "이 조례는 공포한 날부터 시행한다.",
            ],
        );
        named.xml = String::from_utf8(named.xml)
            .unwrap()
            .replace(
                "<부칙></부칙>",
                "<부칙><부칙내용><![CDATA[부칙 <제702호, 2004.11.5> 이 조례는 공포한 날부터 시행한다.]]></부칙내용></부칙>",
            )
            .into_bytes();
        // The amendment text holds a part that no rule reads, and the reader reads its sentence
        // (a plain restatement of the addendum): it holds a date of a rule back from nothing. The
        // gate holds back a date of a rule only for a sentence that the reader does not read
        // (r25c v3, the answer to adversary F1d).
        let found = resolve("ordinances", &named).unwrap();
        assert_eq!(found.enforcement_date.as_deref(), Some("2004-11-05"));
        assert!(found.date_evidence.get("addendum_source").is_none());
        assert!(found.date_evidence.get("date_withheld").is_none());
    }

    #[test]
    fn a_source_keyed_by_its_path_records_the_key_kind() {
        let mut source = revision(
            "조례",
            "제정",
            "20200101",
            1,
            "부칙 이 조례는 공포한 날부터 시행한다.",
        );
        source.revision = format!("{PATH_REVISION_PREFIX}/cache/2000111.xml");
        let found = resolve("ordinances", &source).unwrap();
        assert_eq!(found.date_evidence["revision_key"], "source-path");
        let found = resolve("ordinances", &revision("조례", "제정", "20200101", 1, "")).unwrap();
        assert!(found.date_evidence.get("revision_key").is_none());
    }

    #[test]
    fn only_a_proviso_after_the_effective_sentence_leaves_the_revision_unresolved() {
        // The words that start a proviso, and the words that only look like one.
        for text in [
            "이 조례는 공포한 날부터 시행한다. 다만, 제4조는 따로 정한다.",
            "이 조례는 공포한 날부터 시행한다. 단, 제4조는 2008.1.1부터 시행한다.",
            "이 조례는 공포한 날부터 시행한다. 단 제4조는 따로 정한다.",
            "이 조례는 공포한 날부터 시행한다.\n다만, 시상은 12월에 한다.",
        ] {
            assert!(proviso_after_effective_sentence(text), "{text}");
        }
        for text in [
            "이 조례는 공포한 날부터 시행한다.",
            "이 조례는 공포한 날부터 시행한다.\n단체의 장은 이를 알린다.",
            "이 조례는 공포한 날부터 시행한다. 분단, 단위 사업은 계속한다.",
            // A proviso of a transitional article is not attached to the effective sentence.
            "②(경과조치) 종전의 규정에 따른다. 다만, 제5조의 업무는 2000년 8월 9일부터 시행한다.",
            "제7조제1항 단서를 다음과 같이 한다.\n다만, 구청장이 이를 대행하게 하면 그러하지 아니하다.",
        ] {
            assert!(!proviso_after_effective_sentence(text), "{text}");
        }
        let plain = "이 조례는 공포한 날부터 시행한다.";
        // Directly after the effective sentence, in the general branch, the label branch and the
        // attributed branch alike: unresolved, whatever the proviso says (MST 669104, 734546,
        // 1138933, 816776, 828108, 1076163, 912881).
        for tail in [
            " 단, 제4조 및 제5조에 관한 사항은 2009년 7월 1일부터 시행한다.",
            " 단, 1998. 10. 1부터 적용한다.",
            " 단, 지방자치단체장의 선거가 있는 해는 선거 이후에 장학금을 지급한다.",
            " 단 제4조는 따로 정한다.",
            " 다만, 제11조(시상) 문학상은 2001년도에 한해 12월에 시상한다.",
        ] {
            let general = revision(
                "조례",
                "전부개정",
                "20100601",
                1,
                &format!("{plain}{tail}\n"),
            );
            let found = resolve("ordinances", &general).unwrap();
            assert_eq!(found.status, "unresolved", "{tail}");
            assert_eq!(found.date_evidence["user_decision"], PROVISO_SCOPE_DECISION);
            assert_eq!(
                found.date_evidence["proviso_after_effective_sentence"],
                true
            );
            let labelled = format!("부칙 (2010.6.1) {plain}{tail}\n");
            let single = revision("조례", "일부개정", "20100601", 1, &labelled);
            assert_eq!(outcome("ordinances", &single).0, "unresolved", "{tail}");
            let more = format!("부칙 (2003.1.6) {plain}\n{labelled}");
            let attributed = revision("조례", "일부개정", "20100601", 1, &more);
            assert_eq!(outcome("ordinances", &attributed).0, "unresolved", "{tail}");
        }
        // A proviso of another sentence is ignored (MST 1148409, 959662, 969605): the date
        // of the effective sentence stands, in all three branches.
        for other in [
            "②(경과조치) 접수된 민원은 종전의 규정에 따라 처리한다. 다만, [별표 1]의 감염성 폐기물에 관한 사항은 2000년 8월 9일부터 시행한다.",
            "제2조(경과조치) 설치된 센터는 이 조례에 의하여 설치된 것으로 본다. 다만, 이 조례 시행일로부터 1년 이내에 변경하여야 한다.",
            "제4조(다른 조례의 개정) 제7조제1항 단서를 다음과 같이 한다.\n다만, 구청장이 이를 대행하게 하면 그러하지 아니하다.",
        ] {
            let general = revision(
                "조례",
                "전부개정",
                "20100601",
                1,
                &format!("제1조(시행일) {plain}\n{other}\n"),
            );
            let found = resolve("ordinances", &general).unwrap();
            assert_eq!(
                found.enforcement_date.as_deref(),
                Some("2010-06-01"),
                "{other}"
            );
            assert!(
                found
                    .date_evidence
                    .get("proviso_after_effective_sentence")
                    .is_none()
            );
            let labelled = format!("부칙 (2010.6.1) 제1조(시행일) {plain}\n{other}\n");
            let (status, date, _) = label_outcome("조례", "일부개정", "20100601", &labelled);
            assert_eq!(
                (status.as_str(), date.as_deref()),
                ("confirmed", Some("2010-06-01")),
                "{other}"
            );
            let more = format!("부칙 (2003.1.6) {plain}\n{labelled}");
            let attributed = revision("조례", "일부개정", "20100601", 1, &more);
            let (status, date, _) = outcome("ordinances", &attributed);
            assert_eq!(
                (status.as_str(), date.as_deref()),
                ("confirmed", Some("2010-06-01")),
                "{other}"
            );
        }
        // A proviso across a paragraph mark counts and carries the lead ruling; one without a
        // mark does not need it.
        let marked = revision(
            "조례",
            "전부개정",
            "20100601",
            1,
            &format!("{plain}\n② 다만, 제4조는 따로 정한다.\n"),
        );
        let found = resolve("ordinances", &marked).unwrap();
        assert_eq!(found.status, "unresolved");
        assert_eq!(
            found.date_evidence["lead_ruling"],
            json!([PARAGRAPH_MARK_RULING])
        );
        let plain_proviso = revision(
            "조례",
            "전부개정",
            "20100601",
            1,
            &format!("{plain} 다만, 제4조는 따로 정한다.\n"),
        );
        let found = resolve("ordinances", &plain_proviso).unwrap();
        assert_eq!(found.status, "unresolved");
        assert!(found.date_evidence.get("lead_ruling").is_none());
        // Another subject's application time is not a proviso and stays separate evidence.
        let timing = "제2조(요금) 주차요금 징수는 2000년 2월 1일부터 시행한다.";
        let text = format!("부칙 (2010.6.1) 제1조(시행일) {plain}\n{timing}\n");
        let (status, date, evidence) = label_outcome("조례", "일부개정", "20100601", &text);
        assert_eq!(
            (status.as_str(), date.as_deref()),
            ("confirmed", Some("2010-06-01"))
        );
        assert!(
            evidence["separate_application_timing"]
                .to_string()
                .contains("2000년 2월 1일부터 시행한다")
        );
    }

    #[test]
    fn a_label_that_cannot_be_read_matters_only_in_the_section_of_this_revision() {
        let plain = "이 규칙은 공포한 날부터 시행한다.";
        // MST 932910: brackets of two kinds around a date and a number. The label is read as
        // it is written; its date and number both differ from the revision, so it is unresolved.
        let source = revision(
            "규칙",
            "제정",
            "20010730",
            273,
            &format!("부칙 <개정 2004.01.26 규칙제333호) {plain}\n"),
        );
        let (status, date, evidence) = outcome("ordinances", &source);
        assert_eq!((status.as_str(), date), ("unresolved", None));
        assert_eq!(evidence["user_decision"], LABEL_BOTH_DIFFER_DECISION);
        let run = |sections: &[&str]| {
            let text: String = sections
                .iter()
                .map(|section| format!("부칙 {section} {plain}\n"))
                .collect();
            resolve(
                "ordinances",
                &revision("규칙", "일부개정", "20100601", 7, &text),
            )
            .unwrap()
        };
        // (a) An unreadable section before the last section is ignored when the last section
        // is read and matches the revision.
        let found = run(&["<2010.13.1>", "<2009.1.1>", "<2010. 6. 1>"]);
        assert_eq!(found.enforcement_date.as_deref(), Some("2010-06-01"));
        assert_eq!(found.date_evidence["ignored_unreadable_sections"], 1);
        assert_eq!(
            found.date_evidence["lead_ruling"],
            json!([LABEL_SCOPE_RULING])
        );
        // (b) An unreadable last section leaves the revision unresolved.
        let found = run(&["<2010. 6. 1>", "<2010.13.1>"]);
        assert_eq!(found.status, "unresolved");
        assert_eq!(
            found.date_evidence["lead_default"],
            "lead-default-unreadable-label"
        );
        assert_eq!(
            found.date_evidence["lead_ruling"],
            json!([LABEL_SCOPE_RULING])
        );
        assert!(found.date_evidence["unreadable_label"].is_string());
        // (c) An unreadable section after the section the attribution picks leaves it unresolved,
        // and so does an attribution that picks a section before the last one.
        let found = run(&["<2009.1.1>", "<2010. 6. 1>", "<2010.13.1>"]);
        assert_eq!(found.status, "unresolved");
        let found = run(&["<2010. 6. 1>", "<2009.1.1>", "<2010.13.1>"]);
        assert_eq!(found.status, "unresolved");
        // The same sections without an unreadable one are attributed to the matching section.
        let found = run(&["<2009.1.1>", "<2010. 6. 1>"]);
        assert_eq!(found.enforcement_date.as_deref(), Some("2010-06-01"));
        assert!(
            found
                .date_evidence
                .get("ignored_unreadable_sections")
                .is_none()
        );
        // Two label dates, an impossible label date, a bracket never closed and a bracket
        // that holds other words are not labels without a reading in a single section.
        for label in [
            "<2010.6.1, 2010.7.1>",
            "<2010.13.1>",
            "<2010.6.1",
            "<개정 2010.6.1 조례 제1호 시행 후)",
            "〈2010.6.1 조례 제2호, 2010.6.2〉",
            "(98.13.16)",
            "<99.2.18, 00.1.1>",
            "<123.4.5>",
        ] {
            let source = revision(
                "조례",
                "제정",
                "20100601",
                1,
                &format!("부칙 {label} {plain}\n"),
            );
            let found = resolve("ordinances", &source).unwrap();
            assert_eq!(found.status, "unresolved", "{label}");
            assert_eq!(
                found.date_evidence["lead_default"],
                "lead-default-unreadable-label"
            );
            assert_eq!(found.date_evidence["user_decision"], false);
            assert!(
                found.date_evidence["unreadable_label"].is_string(),
                "{label}"
            );
        }
        // A title that holds a digit is not a label.
        let source = revision(
            "조례",
            "제정",
            "20100601",
            1,
            &format!("부칙 (제주4ㆍ3사건 지원 조례) {plain}\n"),
        );
        assert_eq!(outcome("ordinances", &source).0, "confirmed");
        // A heading word in brackets is not a label, and a readable label is unchanged.
        for text in [
            "부칙 (시행일) 이 조례는 공포한 날부터 시행한다.\n",
            "부칙\n이 조례는 공포한 날부터 시행한다.\n",
        ] {
            let source = revision("조례", "제정", "20100601", 1, text);
            assert_eq!(outcome("ordinances", &source).0, "confirmed", "{text}");
        }
    }

    #[test]
    fn two_digit_years_and_numbers_without_je_are_read() {
        let plain = "이 조례는 공포한 날부터 시행한다.";
        let run = |promulgation: &str, number: u64, label: &str| {
            outcome(
                "ordinances",
                &revision(
                    "조례",
                    "일부개정",
                    promulgation,
                    number,
                    &format!("부칙 {label} {plain}\n"),
                ),
            )
        };
        // The century is the one nearer to the promulgation date, in both directions.
        let (status, date, evidence) = run("19990305", 1, "<99.4.1>");
        assert_eq!(
            (status.as_str(), date.as_deref()),
            ("confirmed", Some("1999-04-01"))
        );
        assert_eq!(
            evidence["label_format_user_decision"],
            LABEL_FORMAT_DECISION
        );
        assert_eq!(evidence["lead_ruling"], json!([CENTURY_RULING]));
        assert_eq!(evidence["label_formats"], json!(["two-digit-year"]));
        let (status, date, _) = run("20241220", 1, "(25.01.02)");
        assert_eq!(
            (status.as_str(), date.as_deref()),
            ("confirmed", Some("2025-01-02"))
        );
        let (status, date, _) = run("19990305", 1, "(05.4.1)");
        assert_eq!(
            (status.as_str(), date.as_deref()),
            ("confirmed", Some("2005-04-01"))
        );
        // A label date before the promulgation date yields the promulgation date.
        let (status, date, evidence) = run("19990305", 1, "<99.2.18>");
        assert_eq!(
            (status.as_str(), date.as_deref()),
            ("confirmed", Some("1999-03-05"))
        );
        assert_eq!(evidence["applied_side"], "promulgation");
        // A tie between the two centuries and a broken promulgation date leave the label unread.
        let (status, date, _) = run("19500101", 1, "<00.1.1>");
        assert_eq!((status.as_str(), date), ("unresolved", None));
        assert!(read_label_dates("<99.2.18>", None)[0].date.is_none());
        // Dots, middle dots of any width, hyphens and spaces separate the parts.
        for label in [
            "<91.8.30>",
            "<91·8·30>",
            "<91ㆍ8ㆍ30>",
            "<91-8-30>",
            "<91 8 30>",
            "(91. 8. 30)",
        ] {
            let (status, date, _) = run("19910830", 1, label);
            assert_eq!(
                (status.as_str(), date.as_deref()),
                ("confirmed", Some("1991-08-30")),
                "{label}"
            );
            let found = read_label_dates(label, compact_date("19910830"));
            assert_eq!(found.len(), 1, "{label}");
            assert_eq!(found[0].date, compact_date("19910830"), "{label}");
            assert!(found[0].two_digit, "{label}");
        }
        // A number without "제" is read like a number with it, for a match and for a mismatch.
        let (status, date, evidence) = run("20100601", 2148, "(규칙2148호)");
        assert_eq!(
            (status.as_str(), date.as_deref()),
            ("confirmed", Some("2010-06-01"))
        );
        assert_eq!(evidence["attributed_by"], json!(["number"]));
        assert_eq!(evidence["label_formats"], json!(["number-without-je"]));
        assert_eq!(
            evidence["label_format_user_decision"],
            LABEL_FORMAT_DECISION
        );
        let (status, date, _) = run("20100601", 2148, "(조례 2149호)");
        assert_eq!((status.as_str(), date), ("unresolved", None));
        let (status, date, evidence) = run("20100601", 2148, "(제2149호)");
        assert_eq!((status.as_str(), date), ("unresolved", None));
        assert_eq!(evidence["approved_rule"], "number-only-mismatch");
        // A date and a number that disagree stay a contradiction.
        let (status, date, _) = run("19990305", 2148, "<99.3.5 규칙2149호>");
        assert_eq!((status.as_str(), date), ("unresolved", None));
        // A four-digit year is unchanged and carries no format mark.
        let (_, _, evidence) = run("20100601", 1, "(2010.6.1)");
        assert!(evidence.get("label_format_user_decision").is_none());
        // Two dates in one label stay unresolved and are not chosen between.
        let source = revision(
            "조례",
            "제정",
            "20030130",
            1,
            &format!("부칙 <제정 95. 3. 17, 개정 95. 10. 20> {plain}\n"),
        );
        let found = resolve("ordinances", &source).unwrap();
        assert_eq!(found.status, "unresolved");
        assert_eq!(
            found.date_evidence["lead_default"],
            "lead-default-unreadable-label"
        );
    }

    #[test]
    fn a_repeal_and_enact_revision_follows_the_rules_of_an_enactment() {
        let plain = "이 조례는 공포한 날부터 시행한다.";
        // The unlabelled single addendum rule of an enactment applies.
        let source = revision("조례", "폐지제정", "20100601", 1, &format!("{plain}\n"));
        let found = resolve("ordinances", &source).unwrap();
        assert_eq!(found.status, "confirmed");
        assert_eq!(found.enforcement_date.as_deref(), Some("2010-06-01"));
        assert_eq!(found.date_evidence["amendment_treated_as"], "제정");
        assert_eq!(
            found.date_evidence["amendment_user_decision"],
            REPEAL_AND_ENACT_DECISION
        );
        // The label rules apply as well, and a valid source date is kept.
        let labelled = revision(
            "조례",
            "폐지제정",
            "20100601",
            1,
            &format!("부칙 (2010.7.1) {plain}\n"),
        );
        assert_eq!(
            outcome("ordinances", &labelled).1.as_deref(),
            Some("2010-07-01")
        );
        let mut dated = revision("조례", "폐지제정", "20100601", 1, "");
        dated.enforcement = "20100701".to_owned();
        dated.xml = "<LawService><조문><조><조내용>제1조(목적) 새 조례다.</조내용></조></조문></LawService>"
            .as_bytes()
            .to_vec();
        let found = resolve("ordinances", &dated).unwrap();
        assert_eq!(found.enforcement_date.as_deref(), Some("2010-07-01"));
        // MST 2120871 shape: the only article is one repeal sentence, so it is a repeal.
        let mut bare = revision("조례", "폐지제정", "20260410", 1, "");
        bare.enforcement = "20260410".to_owned();
        bare.xml = "<LawService><조문><조><조내용>세종교육대상 조례를 폐지한다.\n</조내용></조></조문></LawService>"
            .as_bytes()
            .to_vec();
        let found = resolve("ordinances", &bare).unwrap();
        assert_eq!(found.status, "confirmed");
        assert_eq!(found.enforcement_date.as_deref(), Some("2026-04-10"));
        assert_eq!(found.date_evidence["amendment_treated_as"], "폐지");
        assert_eq!(
            found.date_evidence["amendment_user_decision"],
            REPEAL_ONLY_DECISION
        );
        assert!(repeal_and_enact_is_repeal(&bare.xml));
        // Two articles, a repeal sentence among other text, or no repeal sentence is an enactment.
        for body in [
            "<조><조내용>제1조(목적) 새 조례다.</조내용></조><조><조내용>이 조례를 폐지한다.</조내용></조>",
            "<조><조내용>제1조 이 조례는 다음과 같이 제정한다. 종전 조례를 폐지한다.\n제2조 둘째 조문이다.</조내용></조>",
            "<조><조내용>제1조(목적) 새 조례다.</조내용></조>",
        ] {
            let xml = format!("<LawService><조문>{body}</조문></LawService>").into_bytes();
            assert!(!repeal_and_enact_is_repeal(&xml), "{body}");
        }
        // A plain repeal keeps its date and an excluded kind stays excluded.
        let mut repeal = revision("조례", "폐지", "20100601", 1, "");
        repeal.enforcement = "20100701".to_owned();
        assert_eq!(resolve("ordinances", &repeal).unwrap().status, "confirmed");
    }

    #[test]
    fn a_label_date_later_than_the_next_revision_is_left_unresolved() {
        let text = |label: &str| format!("부칙 {label} 이 조례는 공포한 날부터 시행한다.\n");
        let build = |rows: &[(&str, &str, &str, &str)]| {
            let mut sources = Vec::new();
            for (serial, identity, promulgation, label) in rows {
                let mut source = revision("조례", "일부개정", promulgation, 1, &text(label));
                source.revision = (*serial).to_owned();
                source.identity = (*identity).to_owned();
                source.canonical_path = format!("p{identity}.md");
                source.source_path = format!("{serial}.xml");
                source.serial = serial.parse().unwrap();
                source.markdown = format!("---\nname: {serial}\n---\nbody\n").into_bytes();
                sources.push(source);
            }
            DatePlan::new("ordinances", true, sources).unwrap()
        };
        // Revision 1 promulgated 2010-06-01 carries a label date of 2010-09-01. Revision 2 of
        // the same identity is promulgated 2010-08-01, before the label date: unresolved.
        let plan = build(&[
            ("1", "a", "20100601", "(2010.9.1)"),
            ("2", "a", "20100801", "(2010.8.1)"),
            // Another identity is not the next revision.
            ("3", "b", "20100801", "(2010.8.1)"),
        ]);
        let first = &plan.revisions[0].occurrences[0];
        assert_eq!(first.status, "unresolved");
        assert_eq!(first.enforcement_date, None);
        assert_eq!(first.exposure_date.as_deref(), Some("2010-06-01"));
        assert_eq!(
            first.date_evidence["user_decision"],
            LABEL_AFTER_NEXT_DECISION
        );
        assert_eq!(first.date_evidence["next_revision"], "2");
        assert_eq!(first.date_evidence["next_promulgation_date"], "2010-08-01");
        assert_eq!(plan.revisions[0].status.as_deref(), Some("unresolved"));
        assert_eq!(plan.revisions[1].occurrences[0].status, "confirmed");
        assert_eq!(plan.revisions[2].occurrences[0].status, "confirmed");
        // The report lists it apart from the applied label dates.
        let report = plan.label_date_report().unwrap();
        assert_eq!(report["label_date_applied"], json!([]));
        let deferred = report["label_date_unresolved_after_next_revision"]
            .as_array()
            .unwrap();
        assert_eq!(deferred.len(), 1);
        assert_eq!(deferred[0]["revision"], "1");
        assert_eq!(deferred[0]["next_revision"], "2");
        // A label date equal to the next promulgation date is unresolved as well.
        let plan = build(&[
            ("1", "a", "20100601", "(2010.8.1)"),
            ("2", "a", "20100801", "(2010.8.1)"),
        ]);
        let first = &plan.revisions[0].occurrences[0];
        assert_eq!(first.status, "unresolved");
        assert_eq!(first.date_evidence["next_revision"], "2");
        // A label date before the next promulgation date, and a label date without a next
        // revision, keep the label date.
        for (rows, expected) in [
            (
                vec![
                    ("1", "a", "20100601", "(2010.7.1)"),
                    ("2", "a", "20100801", "(2010.8.1)"),
                ],
                "2010-07-01",
            ),
            (vec![("1", "a", "20100601", "(2010.9.1)")], "2010-09-01"),
        ] {
            let plan = build(&rows);
            let first = &plan.revisions[0].occurrences[0];
            assert_eq!(first.status, "confirmed", "{rows:?}");
            assert_eq!(
                first.enforcement_date.as_deref(),
                Some(expected),
                "{rows:?}"
            );
        }
        // The same promulgation date orders by serial, so the higher serial is the next one.
        let plan = build(&[
            ("1", "a", "20100601", "(2010.9.1)"),
            ("2", "a", "20100601", "(2010.6.1)"),
        ]);
        assert_eq!(plan.revisions[0].occurrences[0].status, "unresolved");
        assert_eq!(
            plan.revisions[0].occurrences[0].date_evidence["next_revision"],
            "2"
        );
    }

    #[test]
    fn a_separate_article_timing_is_evidence_and_a_proviso_form_is_unresolved() {
        let plain = "이 조례는 공포한 날부터 시행한다.";
        // Independent sentences of other articles: the general date stays, the timing is kept.
        for timing in [
            "제2조(주차요금의 조정시기) 개정된 별표의 주차요금 징수는 2000년 2월 1일부터 시행한다.",
            "③ 제36조 내지 제47조를 삭제한 부분은 2000년 5월 9일부터 시행한다.",
            "②(요금) 제22조제8항의 개정규정은 2002년 9월 26일부터 시행한다.",
            "제2조(시행일에 관한 규정) 경기도 행정기구 조례는 이 규칙의 공포일부터 시행한다.",
        ] {
            let text = format!("부칙 (2010.6.1) 제1조(시행일) {plain}\n{timing}\n");
            let (status, date, evidence) = label_outcome("조례", "일부개정", "20100601", &text);
            assert_eq!(
                (status.as_str(), date.as_deref()),
                ("confirmed", Some("2010-06-01")),
                "{timing}"
            );
            assert!(
                evidence["separate_application_timing"]
                    .to_string()
                    .contains(
                        timing
                            .split_whitespace()
                            .last()
                            .unwrap()
                            .trim_end_matches('.')
                    ),
                "{timing}"
            );
        }
        // The same sentences written as a proviso of the general sentence are unresolved.
        for proviso in [
            "다만, 제4조는 2008년 1월 1일부터 시행한다.",
            "단, 제4조는 2008년 1월 1일부터 시행한다.",
            "단 제4조는 2008년 1월 1일부터 시행한다.",
        ] {
            for text in [
                format!("부칙 (2010.6.1) 제1조(시행일) {plain} {proviso}\n"),
                format!("부칙 (2010.6.1) 제1조(시행일) {plain}\n② {proviso}\n"),
            ] {
                let (status, date, _) = label_outcome("조례", "일부개정", "20100601", &text);
                assert_eq!((status.as_str(), date), ("unresolved", None), "{text}");
            }
        }
    }

    #[test]
    fn one_date_parser_gives_every_path_the_same_range_and_forms() {
        // Both written forms read the same; the range is years 0001 to 9999.
        assert_eq!(compact_date("20200101"), compact_date("2020-01-01"));
        assert!(compact_date("20200101").is_some());
        assert!(compact_date("00000101").is_none());
        assert!(compact_date("0000-01-01").is_none());
        assert!(compact_date("00010101").is_some());
        assert!(compact_date("20200230").is_none());
        // Several dates in one field, partial dates and other separators are not dates.
        for text in [
            "20200101, 20200201",
            "2020-01-01 / 2030-01-01",
            "2020.01.01",
            "2020--1-01",
            "202001",
            "",
        ] {
            assert!(compact_date(text).is_none(), "{text}");
        }
        // The sentinel is the same in both forms and is never a date.
        assert!(is_sentinel("99991231") && is_sentinel("9999-12-31"));
        assert!(!is_sentinel("20200101"));
        assert!(compact_date("9999-12-31").is_none());
        // The paths that build a date from running text use the same range.
        let publication = compact_date("20200101");
        let mut source = revision(
            "조례",
            "제정",
            "20200101",
            1,
            "이 조례는 0000년 1월 1일부터 시행한다.",
        );
        source.enforcement = "99991231".to_owned();
        assert_eq!(resolve("ordinances", &source).unwrap().status, "unresolved");
        assert!(
            read_label_dates("<0000.1.1>", publication)[0]
                .date
                .is_none()
        );
        assert!(
            read_label_dates("<00.1.1>", compact_date("00010101"))[0]
                .date
                .is_some()
        );
    }

    #[test]
    fn iso_and_compact_inputs_give_the_same_result_in_every_comparison() {
        // Attribution: a label date is compared with the promulgation date in either form.
        let plain = "이 조례는 공포한 날부터 시행한다.";
        let text = format!("부칙 <2010.1.1> {plain}\n부칙 <2020.1.1> {plain}\n");
        for promulgation in ["20200101", "2020-01-01"] {
            let source = revision("조례", "일부개정", promulgation, 1, &text);
            let found = resolve("ordinances", &source).unwrap();
            assert_eq!(found.status, "confirmed", "{promulgation}");
            assert_eq!(found.enforcement_date.as_deref(), Some("2020-01-01"));
            assert_eq!(found.raw_promulgation_date, promulgation);
        }
        // Sentinel: the default period applies to either written sentinel.
        for sentinel in ["99991231", "9999-12-31"] {
            let mut source = revision("조례", "제정", "20200101", 1, "");
            source.xml = "<LawService><조문><조><조내용>제1조(목적) 본문이다.</조내용></조></조문></LawService>"
                .as_bytes()
                .to_vec();
            source.enforcement = sentinel.to_owned();
            let found = resolve("ordinances", &source).unwrap();
            assert_eq!(
                found.enforcement_date.as_deref(),
                Some("2020-01-22"),
                "{sentinel}"
            );
        }
        // Individual approval: MST 1000238 is found with an ISO promulgation date.
        let approval = &APPROVALS[0];
        for promulgation in [
            approval.promulgation.to_owned(),
            iso_of(approval.promulgation),
        ] {
            let mut source = revision(
                "조례",
                "제정",
                &promulgation,
                1,
                &format!("제1조(시행일) {}\n", approval.sentence),
            );
            source.revision = approval.revision.to_owned();
            let found = resolve("ordinances", &source).unwrap();
            assert_eq!(
                found.reason, "individually approved revision-specific decision",
                "{promulgation}"
            );
        }
    }

    fn iso_of(compact: &str) -> String {
        format!("{}-{}-{}", &compact[..4], &compact[4..6], &compact[6..])
    }

    /// A revision whose addendum block and articles are given. The addendum block has no
    /// publication label, as in most real sources.
    fn case(
        addendum: &str,
        body: &str,
        promulgation: &str,
        amendment: &str,
        number: u64,
    ) -> SourceRevision {
        let mut source = revision("조례", amendment, promulgation, number, "");
        let addendum_xml = if addendum.is_empty() {
            "<부칙></부칙>".to_owned()
        } else {
            format!("<부칙><부칙내용><![CDATA[{addendum}]]></부칙내용></부칙>")
        };
        source.xml = format!(
            "<LawService><공포번호>{number}</공포번호><조문><조><조내용><![CDATA[{body}]]></조내용></조></조문>{addendum_xml}</LawService>"
        )
        .into_bytes();
        source
    }

    /// The final state and date of one case.
    fn settle(source: &SourceRevision) -> (String, Option<String>) {
        let found = resolve("ordinances", source).unwrap();
        (found.status, found.enforcement_date)
    }

    fn expect(source: &SourceRevision, status: &str, date: Option<&str>, name: &str) {
        let (got_status, got_date) = settle(source);
        assert_eq!(
            (got_status.as_str(), got_date.as_deref()),
            (status, date),
            "{name}"
        );
    }

    #[test]
    fn explicit_date_without_buteo_records_its_own_decision_only_then() {
        let decisions = |text: &str| -> (String, Option<String>, Vec<String>) {
            let source = case("", &format!("부칙 {text}"), "19960614", "전부개정", 3004);
            let found = resolve("ordinances", &source).unwrap();
            let ids = found.date_evidence["user_decisions"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .map(|item| item.as_str().unwrap().to_owned())
                        .collect()
                })
                .unwrap_or_default();
            (found.status, found.enforcement_date, ids)
        };
        let id = EXPLICIT_WITHOUT_BUTEO_DECISION.to_owned();
        let (status, date, ids) = decisions("이 규칙은 1996년 7월 1일 시행한다.");
        assert_eq!(
            (status.as_str(), date.as_deref()),
            ("confirmed", Some("1996-07-01"))
        );
        assert_eq!(ids, vec![id.clone()]);
        let (_, date, ids) = decisions("이 규칙은 1996. 7. 1. 시행한다.");
        assert_eq!(date.as_deref(), Some("1996-07-01"));
        assert_eq!(ids, vec![id.clone()]);
        for with_particle in [
            "이 규칙은 1996년 7월 1일부터 시행한다.",
            "이 규칙은 1996년 7월 1일로부터 시행한다.",
            "이 규칙은 1996년 7월 1일에 시행한다.",
        ] {
            let (_, date, ids) = decisions(with_particle);
            assert_eq!(date.as_deref(), Some("1996-07-01"), "{with_particle}");
            assert!(!ids.contains(&id), "{with_particle}");
        }
        let (_, date, ids) = decisions("이 규칙은 공포한 날 시행한다.");
        assert_eq!(date.as_deref(), Some("1996-06-14"));
        assert_eq!(ids, vec![WITHOUT_BUTEO_DECISION.to_owned()]);
    }

    /// The user decision ids recorded for one case.
    fn user_ids(source: &SourceRevision) -> Vec<String> {
        let found = resolve("ordinances", source).unwrap();
        found.date_evidence["user_decisions"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|item| item.as_str().unwrap().to_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn omitted_proviso_mark_is_recorded_and_changes_no_date() {
        let p = "이 조례는 공포한 날부터 시행한다.";
        for mark in ["<단서생략>", "(단서생략)", "<단서 생략>"] {
            let source = case("", &format!("부칙 {p} {mark}"), "20200101", "제정", 7);
            expect(&source, "confirmed", Some("2020-01-01"), mark);
            assert_eq!(user_ids(&source), vec![PROVISO_OMITTED_DECISION], "{mark}");
            // The mark does not hide a real proviso behind it.
            let proviso = case(
                "",
                &format!("부칙 {p} {mark} 다만, 별도로 정한다."),
                "20200101",
                "제정",
                7,
            );
            expect(&proviso, "unresolved", None, mark);
        }
        let plain = case("", &format!("부칙 {p}"), "20200101", "제정", 7);
        expect(&plain, "confirmed", Some("2020-01-01"), "no mark");
        assert!(!user_ids(&plain).contains(&PROVISO_OMITTED_DECISION.to_owned()));
    }

    #[test]
    fn one_heading_with_different_label_dates_is_unresolved_in_every_path() {
        let p = "이 조례는 공포한 날부터 시행한다.";
        // Codex blocker 1: the addendum field and the articles give the same result.
        for amendment in ["제정", "전부개정"] {
            expect(
                &case(
                    &format!("부칙 <2020.1.1> <2021.1.1> {p}"),
                    "본문",
                    "20200101",
                    amendment,
                    7,
                ),
                "unresolved",
                None,
                "addendum field",
            );
            expect(
                &case(
                    "",
                    &format!("부칙 <2020.1.1> <2021.1.1> {p}"),
                    "20200101",
                    amendment,
                    7,
                ),
                "unresolved",
                None,
                "articles",
            );
        }
        // Controls: the same date twice, or a date and a number, stays readable.
        for label in [
            "<2020.1.1> <2020.1.1>",
            "<2020.1.1> <조례 제7호>",
            "<2020.1.1>",
        ] {
            expect(
                &case(&format!("부칙 {label} {p}"), "본문", "20200101", "제정", 7),
                "confirmed",
                Some("2020-01-01"),
                label,
            );
        }
    }

    #[test]
    fn a_sentence_broken_by_a_line_feed_stops_the_default_period() {
        // Codex blocker 2: the sentence names a date or a condition, so 21 days is no answer.
        for body in [
            "이 조례는 2030년 1월 1일\n시행한다.",
            "이 조례는 의회의 승인을 받은 날\n시행한다.",
        ] {
            expect(
                &case("", body, "20200101", "제정", 7),
                "unresolved",
                None,
                body,
            );
        }
        // Controls: the same sentence on one line, no sentence, and a sentence of another subject.
        expect(
            &case(
                "",
                "이 조례는 2030년 1월 1일 시행한다.",
                "20200101",
                "제정",
                7,
            ),
            "confirmed",
            Some("2030-01-01"),
            "one line",
        );
        expect(
            &case(
                "",
                "이 조례는 의회의 승인을 받은 날 시행한다.",
                "20200101",
                "제정",
                7,
            ),
            "unresolved",
            None,
            "one line condition",
        );
        expect(
            &case(
                "",
                "제1조(목적) 이 조례는 운영을 정한다.",
                "20200101",
                "제정",
                7,
            ),
            "confirmed",
            Some("2020-01-22"),
            "no sentence",
        );
        expect(
            &case("", "제1조 시장은 사업을 시행한다.", "20200101", "제정", 7),
            "confirmed",
            Some("2020-01-22"),
            "operating sentence",
        );
    }

    #[test]
    fn addendum_text_in_the_articles_of_another_ordinances_amendment_stays_unresolved() {
        let p = "부칙 <조례 제8호> 이 조례는 공포한 날부터 시행한다.";
        // Codex blocker 3: the articles follow the addendum field.
        for amendment in ["타법개정", "타법폐지", "일괄개정"] {
            expect(
                &case("", p, "20200101", amendment, 7),
                "unresolved",
                None,
                amendment,
            );
            expect(
                &case(p, "본문", "20200101", amendment, 7),
                "unresolved",
                None,
                amendment,
            );
        }
        // Controls: a broken publication date keeps the older body rule, and an enactment
        // is read by the body rules as before.
        let mut broken = case(
            "",
            "이 조례는 2030년 1월 1일부터 시행한다.",
            "00000101",
            "타법개정",
            7,
        );
        broken.enforcement = "99991231".to_owned();
        expect(
            &broken,
            "confirmed",
            Some("2030-01-01"),
            "broken publication",
        );
        expect(
            &case("", p, "20200101", "일부개정", 8),
            "confirmed",
            Some("2020-01-01"),
            "일부개정 matching number",
        );
    }

    #[test]
    fn a_later_article_with_the_verb_keeps_the_default_periods() {
        let body = "제1조(목적) 이 조례는 운영을 정한다.\n제2조 시장은 사업을 시행한다.";
        expect(
            &case("", body, "20200101", "제정", 7),
            "confirmed",
            Some("2020-01-22"),
            "ordinance 21 days",
        );
        // The notice rule (6 days) and the arrival estimate pass the same detection.
        let mut notice = case("", body, "20200101", "제정", 7);
        notice.category = "고시".to_owned();
        expect(&notice, "confirmed", Some("2020-01-07"), "notice 6 days");
        let mut order = case("", body, "20200101", "제정", 7);
        order.category = "훈령".to_owned();
        let found = resolve("ordinances", &order).unwrap();
        assert_eq!(
            (found.status.as_str(), found.enforcement_date.as_deref()),
            ("estimated", Some("2020-01-01"))
        );
        // The sentences of the Codex probes stay as before.
        for text in [
            "이 조례는 2030년 1월 1일\n시행한다.",
            "이 조례는 의회의 승인을 받은 날\n시행한다.",
        ] {
            expect(
                &case("", text, "20200101", "제정", 7),
                "unresolved",
                None,
                text,
            );
        }
        expect(
            &case(
                "",
                "이 조례는 2030년 1월 1일 시행한다.",
                "20200101",
                "제정",
                7,
            ),
            "confirmed",
            Some("2030-01-01"),
            "one line control",
        );
    }

    #[test]
    fn the_attributed_whole_part_branch_records_the_ids_of_its_sentences() {
        let ids = |source: &SourceRevision, key: &str| -> Vec<String> {
            let found = resolve("ordinances", source).unwrap();
            let mut items: Vec<String> = found.date_evidence[key]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .map(|item| item.as_str().unwrap().to_owned())
                        .collect()
                })
                .unwrap_or_default();
            items.sort();
            items
        };
        // Claude blocker 2: an amendment names no enactment ruling and records the sentence ids.
        let body = "부칙\n이 조례는 공포한 날 시행한다. <단서생략>\n부칙 <2020.1.1 조례 제7호>";
        let source = case("", body, "20200101", "일부개정", 7);
        expect(&source, "confirmed", Some("2020-01-01"), "amendment");
        assert_eq!(
            ids(&source, "user_decisions"),
            vec![WITHOUT_BUTEO_DECISION, PROVISO_OMITTED_DECISION]
        );
        assert_eq!(ids(&source, "lead_rulings"), vec![BODY_SECTIONS_RULING]);
        // An enactment adds the enactment ruling and the same sentence ids.
        let source = case("", body, "20200101", "제정", 7);
        expect(&source, "confirmed", Some("2020-01-01"), "enactment");
        assert_eq!(
            ids(&source, "user_decisions"),
            vec![WITHOUT_BUTEO_DECISION, PROVISO_OMITTED_DECISION]
        );
        assert_eq!(
            ids(&source, "lead_rulings"),
            vec![ATTRIBUTION_ENACTMENT_RULING, BODY_SECTIONS_RULING]
        );
        // Spaced verb: the spaced-verb ruling joins.
        let spaced = "부칙\n이 조례는 공포한 날부터 시행 한다.\n부칙 <2020.1.1 조례 제7호>";
        let source = case("", spaced, "20200101", "일부개정", 7);
        expect(&source, "confirmed", Some("2020-01-01"), "spaced");
        assert_eq!(
            ids(&source, "lead_rulings"),
            vec![BODY_SECTIONS_RULING, SPACED_VERB_RULING]
        );
    }

    // User decision user-broad-detection-20261110: the detection ends a sentence at only one
    // boundary, a full stop that ends a sentence. A line start and a length are no boundary. A
    // verb of another article before the next full stop counts, so such a revision stays
    // unresolved instead of taking the default period (user-approved conservative limit).
    #[test]
    fn r21_broad_detection_ends_only_at_the_sentence_end() {
        let detected: &[&str] = &[
            "이 조례는 가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가 시행한다.",
            "이 조례는 사무(시행령에서 정한다. 별도 고시)가 이양되는 날 시행한다.",
            "이 조례는 사무(시행령 개정일\n운영을 정한다. 시장은 사업을 시행한다.",
            "이 조례는 \"시행령에서 정한다. 별도 고시\"가 있는 날 시행한다.",
            "이 조례는 시행령 개정일. 별도 고시 후 시행한다.",
            "이 조례는 2030. 1. 1. 시행한다.",
            "이 조례는 운영을 정하며\n제2조 시장은 사업을 시행한다.",
            "이 조례는 운영을 정하며\n부칙\n시장은 사업을 시행한다",
            "이 조례는 운영을 정하며\n② 시장은 사업을 시행한다.",
            "이 조례는 사무(시행령 개정일\n제2조 시장은 사업을 시행한다.",
            "이 조례는 사무(다음 각 호\n1. 시가 직접 시행함에 따라 생기는 사업)가 이양되는 날 적용한다.",
            "이 조례는 \"다음 각 호\n1. 시가 직접 시행함에 따라 생기는 사업\"이 있는 날 적용한다.",
            "이 조례는 2030. 1.\n1. 시행한다.",
            "이 조례는 2030년 1월\n1. 시행한다.",
            "이 조례는 운영을 정하며\n1. 시장은 사업을 시행한다.",
            "이 조례는 운영을 정하며\n1) 시장은 사업을 시행한다.",
            "이 조례는 운영을 정하며\n(1) 시장은 사업을 시행한다.",
            "이 조례는 운영을 정하며\n가. 시장은 사업을 시행한다.",
            "이 조례는 운영을 정하며\n거. 시장은 사업을 시행한다.",
            "이 조례는 주민세 균등\n분) 비과세로 하되 의회의 승인을 받은 날 시행한다.",
            "이 조례는 사업을 시행함에 있어 필요한 사항을 정한다.",
            "이 조례는 의회의 승인을 받은 날 시행한다.",
            "이 조례는 사무(위임사무를 포함한다\n이하 같다)가 이양되는 날 시행한다.",
            "이 조례는 사무(위임사무를 포\n함) 이양일에 시행한다.",
            "이 조례는 사무(위임사무를\n포함한다. 이하 같다)가 이양되는 날 시행한다.",
            "이 조례는 공포한 날의 다\n음 달 1일 시행한다.",
            "이 조례는 사무가 위임\n된 날 시행한다.",
            "이 조례는 위원회(이하 위원회라 한다)\n의 구성을 마친 날 시행한다.",
            "이 조례는 사무를 규정함\n을 목적으로 하며 의회의 승인을 받은 날 시행한다.",
            "이 조례는 운영을 정한다\n시장은 사업을 시행한다.",
            "이 조례는 사무（시행령에서 정한다. 별도 고시）가 이양되는 날 시행한다.",
            "이 조례는 사무[시행령에서 정한다. 별도 고시]가 이양되는 날 시행한다.",
            "이 조례는 사무［시행령에서 정한다. 별도 고시］가 이양되는 날 시행한다.",
            "이 조례는 사무〔시행령에서 정한다. 별도 고시〕가 이양되는 날 시행한다.",
            "이 조례는 사무「시행령에서 정한다. 별도 고시」가 이양되는 날 시행한다.",
            "이 조례는 사무『시행령에서 정한다. 별도 고시』가 이양되는 날 시행한다.",
            "이 조례는 사무“시행령에서 정한다. 별도 고시”가 이양되는 날 시행한다.",
            "이 조례는 사무‘시행령에서 정한다. 별도 고시’가 이양되는 날 시행한다.",
            "이 조례는 사무<시행령에서 정한다. 별도 고시>가 이양되는 날 시행한다.",
            "이 조례는 사무＜시행령에서 정한다. 별도 고시＞가 이양되는 날 시행한다.",
            "이 조례는 사무〈시행령에서 정한다. 별도 고시〉가 이양되는 날 시행한다.",
            "이 조례는 사무《시행령에서 정한다. 별도 고시》가 이양되는 날 시행한다.",
            "이 조례는 「시험법」(법률 제7호.)이 시행되는 날 시행한다.",
            "이 조례는 사무가 이양되는 날(시행령 개정일. 별도 고시) 시행한다.",
            "이 조례는 「시험법」(법률 제7호)이 시행되는 날 시행한다.",
            "제1조(목적) ① 이 조례는 운영을 정한다\n② 시장은 사업을 시행한다.",
            "제2조(적용범위) 이 조례는 다음 각 호의 사업에 적용한다\n1. 시가 직접 시행함에 따라 생기는 사업\n2. 그 밖의 사업",
            "제2조(적용범위) 이 조례는 다음 각 호의 사업에 적용하되\n1. 시가 직접 시행함에 따라 생기는 사업",
            "이 조례는 2030년 1월 1일\n시행한다.",
            "이 조례는 의회의 승인을 받은 날\n시행한다.",
            "이 조례는 2030년 1월 1일 시행한다.",
            "제1조(목적) 이 조례는 시의 사업에 관한 사항을 정한다\n제2조 시장은 사업을 시행한다.",
            "이 조례는 운영에 관하여 정함\n시장은 사업을 시행한다.",
            "이 조례는 \"시행령에서\n정한다. 별도 고시\"가 있는 날 시행한다.",
            "이 조례는 '시행령에서 정한다. 별도 고시'가 있는 날 시행한다.",
            "이 조례는 2030. 1\n1. 시행한다.",
            "이 조례는 제1호,\n2. 의회의 승인을 받은 날 시행한다.",
            "이 조례는 2030년\n1. 1. 시행한다.",
            "이 조례는 2030년 1월 1일\n1. 시행한다.",
            "이 조례는 2030. 1. \n1. 시행한다.",
            "이 조례는 \"운영을 정하며\n제2조 시장은 사업을 시행한다.",
            "이 조례는 운영을 정하며\n1.5배의 범위에서 의회의 승인을 받은 날 시행한다.",
            "이 조례는 사무(제5조\n① 및 ②의 사무를 말한다)가 이양되는 날 시행한다.",
            "이 조례는 사업의 범위(시의 사업을 말한다. 이하 같다)를 정하고 그 사업을 시행함에 있어 필요한 사항을 정한다.",
            "이 조례는 사업(시행령 개정일. 시가 직접 시행함에 따라 생기는 사업)에 적용한다.",
            "제1조 이 조례는 운영을 정한다.\n제2조 이 조례는 사업을 시행한다.",
            "이 조례는 운영을 정한다\n제2조 시장은 사업을 시행한다",
            "이 조례는 운영을 정한다\n부칙\n시장은 사업을 시행한다",
            "이 조례는 2030. 1.\n\n1. 시행한다.",
            "이 고시는 2030년 1월 1일\n시행한다.",
            "부칙\n이 조례는 공포한 날 시행한다. <단서생략> 다만, 제2조는 따로 정한다.\n부칙 <2020.1.1 조례 제7호>",
            "이 조례는 운영을 정하며\n   제2조 시장은 사업을 시행한다.",
            "이 조례는 운영을 정하며\n  부칙\n시장은 사업을 시행한다",
            "이 조례는 운영을 정하며\n   1. 시장은 사업을 시행한다.",
            "이 조례는 운영을 정하며\n   (1) 시장은 사업을 시행한다.",
            "이 조례는 운영을 정하며\n   가. 시장은 사업을 시행한다.",
            "이 조례는 운영을 정하며\n\t② 시장은 사업을 시행한다.",
            "이 조례는 사무(위임사무를 포함한다\n① 및 ②의 사무)가 이양되는 날 시행한다.",
            "이 조례는 \"운영을 정하며\n① 시장은 사업을 시행한다.",
            "이 조례는 같은 조\n①에 따른 승인을 받은 날 시행한다.",
            "이 조례는 시험법 제5조\n①항의 개정규정이 시행되는 날 시행한다.",
            "이 조례는 운영을 정하며\n①(시행일) 시장은 사업을 시행한다.",
            "제2조(적용범위) 이 조례는 다음 각 항의 사업에 적용한다\n① 시가 직접 시행함에 따라 생기는 사업",
            "이 조례는 요금을 정하며 3. 식품접객업(영업장 면적 125㎡이상 300㎡미만 : 업체당 월 20,000원\n② 수수료는 납부한 날 시행한다.",
            "이 조례는 운영을 정하며\n  ② 시장은 사업을 시행한다.",
            "이 조례는 사무(시행령 개정일\n부칙\n시장은 사업을 시행한다.",
            "이 조례는 ２０３０. １\n１. 시행한다.",
            "이 조례는 ２０３０．１．\n１. 시행한다.",
            "이 조례는 제１호，\n２. 의회의 승인을 받은 날 시행한다.",
            "이 조례는 2030. 1\n１. 시행한다.",
            "이 조례는 ２０３０．１．\n1. 시행한다.",
            "이 조례는 제１호，\n2. 의회의 승인을 받은 날 시행한다.",
            "이 조례는 운영을 정하며\n１. 시장은 사업을 시행한다.",
            "이 조례는 ２０３０. １\n1. 시행한다.",
            "이 조례는 같은 조\n(1)에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 조\n(1)항에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조의 규정에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조의2에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제５조에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n부칙에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n1.에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n1)에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n가.에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n（1） 승인을 받은 날 시행한다.",
            "이 조례는 운영을 정하며\n  (1) 시장은 사업을 시행한다.",
            "이 조례는 운영을 정하며\n제2조(적용) 시장은 사업을 시행한다.",
            "이 조례는 운영을 정하며\n제2조의2(적용) 시장은 사업을 시행한다.",
            "이 조례는 운영을 정하며\n제2조의2 시장은 사업을 시행한다.",
            "이 조례는 운영을 정하며\n제2조\n시장은 사업을 시행한다.",
            "이 조례는 운영을 정하며\n부칙 <2020.1.1> 시장은 사업을 시행한다.",
            "이 조례는 운영을 정하며\n부칙<2020.1.1> 시장은 사업을 시행한다.",
            "이 조례는 운영을 정하며\n부칙(제7호) 시장은 사업을 시행한다.",
            "이 조례는 운영을 정하며\n부 칙\n시장은 사업을 시행한다.",
            "이 조례는 사무(시행령 개정일\n제2조(적용) 시장은 사업을 시행한다.",
            "이 조례는 사무(시행령 개정일\n부칙 <2020.1.1> 시장은 사업을 시행한다.",
            "이 조례는 2030{mark}\n1. 시행한다.",
            "이 조례는 2030年1月\n1. 시행한다.",
            "이 조례는 운영을 정한다\n1. 시행한다.",
            "이 조례는 운영을 정하며\n1. 시행한다.",
            "이 조례는 같은 법\n제5조를 적용하여 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조는 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조와 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제５조 시장은 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n부칙을 적용하는 날 시행한다.",
            "이 조례는 같은 법\n부칙은 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 \t및 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 　및 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n① \t및 ② 승인을 받은 날 시행한다.",
            "이 조례는 같은 법 제3조 및\n1. 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조 또는\n1. 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조 및\n(1) 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조 또는\n가. 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법\n제5조  및 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조\t및 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조　및 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n1.  에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 및 제6조에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 조\n① 및 ②에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 조\n1. 에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 조\n(1) 에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 조\n가. 에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n부칙 및 별표에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 및 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n① 및 ② 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n1. 및 2. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n(1) 및 (2) 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n가. 및 나. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 또는 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n① 또는 ② 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n1. 또는 2. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n(1) 또는 (2) 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n가. 또는 나. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 내지 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n① 내지 ② 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n1. 내지 2. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n(1) 내지 (2) 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n가. 내지 나. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 에 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n① 에 ② 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n1. 에 2. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n(1) 에 (2) 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n가. 에 나. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 의 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n① 의 ② 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n1. 의 2. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n(1) 의 (2) 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n가. 의 나. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 에서 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n① 에서 ② 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n1. 에서 2. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n(1) 에서 (2) 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n가. 에서 나. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 에 따른 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n① 에 따른 ② 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n1. 에 따른 2. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n(1) 에 따른 (2) 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n가. 에 따른 나. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 에따른 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n① 에따른 ② 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n1. 에따른 2. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n(1) 에따른 (2) 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n가. 에따른 나. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 에 의한 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n① 에 의한 ② 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n1. 에 의한 2. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n(1) 에 의한 (2) 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n가. 에 의한 나. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 에의한 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n① 에의한 ② 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n1. 에의한 2. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n(1) 에의한 (2) 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n가. 에의한 나. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 부터 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n① 부터 ② 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n1. 부터 2. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n(1) 부터 (2) 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n가. 부터 나. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 까지 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n① 까지 ② 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n1. 까지 2. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n(1) 까지 (2) 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n가. 까지 나. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 ㆍ 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n① ㆍ ② 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n1. ㆍ 2. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 · 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n① · ② 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n1. · 2. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 , 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n① , ② 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n1. , 2. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 ， 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n① ， ② 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n1. ， 2. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 、 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n① 、 ② 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n1. 、 2. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 제1항에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조의2 제1항에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 제1호에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조의2 제1호에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 제1목에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조의2 제1목에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 각 호에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조의2 각 호에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 단서에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조의2 단서에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 본문에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조의2 본문에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 전단에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조의2 전단에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 후단에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조의2 후단에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 제1항 중 승인을 받은 날 시행한다.",
            "이 조례는 같은 법 제3조 및\n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조 및 \n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조 및\n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조 및 \n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조 및\n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조 및 \n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조 또는\n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조 또는 \n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조 또는\n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조 또는 \n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조 또는\n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조 또는 \n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조 내지\n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조 내지 \n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조 내지\n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조 내지 \n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조 내지\n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조 내지 \n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조,\n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조, \n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조,\n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조, \n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조,\n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조, \n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조，\n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조， \n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조，\n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조， \n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조，\n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조， \n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조、\n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조、 \n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조、\n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조、 \n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조、\n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조、 \n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조､\n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조､ \n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조､\n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조､ \n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조､\n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조､ \n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조·\n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조· \n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조·\n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조· \n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조·\n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조· \n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조ㆍ\n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조ㆍ \n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조ㆍ\n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조ㆍ \n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조ㆍ\n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조ㆍ \n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조・\n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조・ \n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조・\n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조・ \n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조・\n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조・ \n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조･\n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조･ \n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조･\n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조･ \n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조･\n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조･ \n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조~\n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조~ \n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조~\n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조~ \n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조~\n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조~ \n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조～\n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조～ \n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조～\n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조～ \n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조～\n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조～ \n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조∼\n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조∼ \n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조∼\n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조∼ \n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조∼\n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조∼ \n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조〜\n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조〜 \n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조〜\n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조〜 \n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조〜\n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조〜 \n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조-\n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조- \n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조-\n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조- \n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조-\n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조- \n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조－\n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조－ \n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조－\n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조－ \n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조－\n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조－ \n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조‐\n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조‐ \n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조‐\n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조‐ \n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조‐\n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조‐ \n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조–\n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조– \n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조–\n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조– \n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조–\n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조– \n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 제1호,\n2. 시행한다.",
            "이 조례는 제1호，\n2. 시행한다.",
            "이 조례는 제1호、\n2. 시행한다.",
            "이 조례는 제1호､\n2. 시행한다.",
            "이 조례는 제1호·\n2. 시행한다.",
            "이 조례는 제1호ㆍ\n2. 시행한다.",
            "이 조례는 제1호・\n2. 시행한다.",
            "이 조례는 제1호･\n2. 시행한다.",
            "이 조례는 제1호~\n2. 시행한다.",
            "이 조례는 제1호～\n2. 시행한다.",
            "이 조례는 제1호∼\n2. 시행한다.",
            "이 조례는 제1호〜\n2. 시행한다.",
            "이 조례는 제1호-\n2. 시행한다.",
            "이 조례는 제1호－\n2. 시행한다.",
            "이 조례는 제1호‐\n2. 시행한다.",
            "이 조례는 제1호–\n2. 시행한다.",
            "이 조례는 제1호 내지\n2. 시행한다.",
            "이 조례는 운영을 정하며\n제2조\n및 시장은 사업을 시행한다.",
            "이 조례는 운영을 정하며\n제2조\n에 따라 시장은 사업을 시행한다.",
            "이 조례는 같은 조\n① 시장은 승인을 받은 날 시행한다.",
            "이 조례는 같은 조\n1. 시장은 승인을 받은 날 시행한다.",
            "이 조례는 같은 조\n(1) 시장은 승인을 받은 날 시행한다.",
            "이 조례는 같은 조\n가. 시장은 승인을 받은 날 시행한다.",
            "이 조례는 같은 조\n① 의장은 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제2조 에너지 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제2조 시장은 승인을 받은 날 시행한다.",
            "이 조례는 운영 등.\n제2조 시장은 사업을 시행한다.",
            "이 조례는 2030. 1. 1\n제2조 시장은 사업을 시행한다.",
            "이 조례는 운영 등.\n② 시장은 사업을 시행한다.",
            "이 조례는 운영 등.\n부칙 <2020.1.1> 시장은 사업을 시행한다.",
            "이 조례는 2030. 1. 1\n② 시장은 사업을 시행한다.",
            "이 조례는 2030.\n1. 시행한다.",
            "이 조례는 2030,\n1. 시행한다.",
            "이 조례는 2030．\n1. 시행한다.",
            "이 조례는 2030，\n1. 시행한다.",
            "이 조례는 2030。\n1. 시행한다.",
            "이 조례는 2030｡\n1. 시행한다.",
            "이 조례는 2030、\n1. 시행한다.",
            "이 조례는 2030､\n1. 시행한다.",
            "이 조례는 2030·\n1. 시행한다.",
            "이 조례는 2030ㆍ\n1. 시행한다.",
            "이 조례는 2030・\n1. 시행한다.",
            "이 조례는 2030･\n1. 시행한다.",
            "이 조례는 2030년\n1. 시행한다.",
            "이 조례는 2030월\n1. 시행한다.",
            "이 조례는 2030일\n1. 시행한다.",
            "이 조례는 2030年\n1. 시행한다.",
            "이 조례는 2030月\n1. 시행한다.",
            "이 조례는 2030日\n1. 시행한다.",
            "이 조례는 2030~\n1. 시행한다.",
            "이 조례는 2030～\n1. 시행한다.",
            "이 조례는 2030∼\n1. 시행한다.",
            "이 조례는 2030〜\n1. 시행한다.",
            "이 조례는 2030-\n1. 시행한다.",
            "이 조례는 2030－\n1. 시행한다.",
            "이 조례는 2030‐\n1. 시행한다.",
            "이 조례는 2030–\n1. 시행한다.",
        ];
        let not_detected: &[&str] = &[
            "이 조례는 \"운영을 정한다\". 시장은 사업을 시행한다.",
            "이 조례는 운영을 정한다。 시장은 사업을 시행한다.",
            "이 조례는 운영을 정한다. 시장은 사업을 시행한다.",
            "이 조례는 운영을 정함. 시장은 사업을 시행한다.",
            "이 조례는 운영은 다음과 같음. 시장은 사업을 시행한다.",
            "이 조례는 운영은 시장의 소관임. 시장은 사업을 시행한다.",
            "이 조례는 운영을 시장이 맡게 됨. 시장은 사업을 시행한다.",
            "이 조례는 운영을 정한다(개정 2010. 1. 1.). 시장은 사업을 시행한다.",
            "이 조례는 운영(주)을 정한다. 시장은 사업을 시행한다.",
            "이 조례는 운영（주）을 정한다. 시장은 사업을 시행한다.",
            "이 조례는 운영[주]을 정한다. 시장은 사업을 시행한다.",
            "이 조례는 운영［주］을 정한다. 시장은 사업을 시행한다.",
            "이 조례는 운영〔주〕을 정한다. 시장은 사업을 시행한다.",
            "이 조례는 운영「주」을 정한다. 시장은 사업을 시행한다.",
            "이 조례는 운영『주』을 정한다. 시장은 사업을 시행한다.",
            "이 조례는 운영“주”을 정한다. 시장은 사업을 시행한다.",
            "이 조례는 운영‘주’을 정한다. 시장은 사업을 시행한다.",
            "이 조례는 운영<주>을 정한다. 시장은 사업을 시행한다.",
            "이 조례는 운영＜주＞을 정한다. 시장은 사업을 시행한다.",
            "이 조례는 운영〈주〉을 정한다. 시장은 사업을 시행한다.",
            "이 조례는 운영《주》을 정한다. 시장은 사업을 시행한다.",
            "제1조(목적) 이 조례는 운영을 정한다.\n제2조 시장은 사업을 시행한다.",
            "제1조(목적) 이 조례는 시의 사업에 관한 사항을 정한다.\n제2조(적용범위) ① 이 조례는 시의 사업에 적용한다.\n② 시장은 필요한 사업을 시행한다.",
            "제1조 이 조례는 필요한 사항을 정한다.\n제2조 사업을 시행함에 있어 지켜야 한다.",
            "이 조례는 '운영을 정한다'. 시장은 사업을 시행한다.",
            "이 조례는 가) 운영을 정한다. 시장은 사업을 시행한다.",
            "이 조례는 운영을 정한다<개정>. 시장은 사업을 시행한다.",
            "이 조례는 필요한 사항을 정한다.\n제2조 사업을 시행함에 있어 지켜야 한다.",
            "이 조례는 운영을 정한다.\n제2조 시장은 사업을 시행한다.",
        ];
        for text in detected {
            assert!(names_effective_date(text), "{text:?}");
        }
        for text in not_detected {
            assert!(!names_effective_date(text), "{text:?}");
        }
    }

    #[test]
    fn r21_states_follow_the_detection() {
        // A detected sentence stops the default period. The two sentences that name a readable
        // explicit date are confirmed with that date; every other detected text is unresolved.
        let explicit_date: &[&str] = &[
            "이 조례는 2030. 1. 1. 시행한다.",
            "이 조례는 2030년 1월 1일 시행한다.",
        ];
        let detected: &[&str] = &[
            "이 조례는 가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가 시행한다.",
            "이 조례는 사무(시행령에서 정한다. 별도 고시)가 이양되는 날 시행한다.",
            "이 조례는 사무(시행령 개정일\n운영을 정한다. 시장은 사업을 시행한다.",
            "이 조례는 \"시행령에서 정한다. 별도 고시\"가 있는 날 시행한다.",
            "이 조례는 시행령 개정일. 별도 고시 후 시행한다.",
            "이 조례는 2030. 1. 1. 시행한다.",
            "이 조례는 운영을 정하며\n제2조 시장은 사업을 시행한다.",
            "이 조례는 운영을 정하며\n부칙\n시장은 사업을 시행한다",
            "이 조례는 운영을 정하며\n② 시장은 사업을 시행한다.",
            "이 조례는 사무(시행령 개정일\n제2조 시장은 사업을 시행한다.",
            "이 조례는 사무(다음 각 호\n1. 시가 직접 시행함에 따라 생기는 사업)가 이양되는 날 적용한다.",
            "이 조례는 \"다음 각 호\n1. 시가 직접 시행함에 따라 생기는 사업\"이 있는 날 적용한다.",
            "이 조례는 2030. 1.\n1. 시행한다.",
            "이 조례는 2030년 1월\n1. 시행한다.",
            "이 조례는 운영을 정하며\n1. 시장은 사업을 시행한다.",
            "이 조례는 운영을 정하며\n1) 시장은 사업을 시행한다.",
            "이 조례는 운영을 정하며\n(1) 시장은 사업을 시행한다.",
            "이 조례는 운영을 정하며\n가. 시장은 사업을 시행한다.",
            "이 조례는 운영을 정하며\n거. 시장은 사업을 시행한다.",
            "이 조례는 주민세 균등\n분) 비과세로 하되 의회의 승인을 받은 날 시행한다.",
            "이 조례는 사업을 시행함에 있어 필요한 사항을 정한다.",
            "이 조례는 의회의 승인을 받은 날 시행한다.",
            "이 조례는 사무(위임사무를 포함한다\n이하 같다)가 이양되는 날 시행한다.",
            "이 조례는 사무(위임사무를 포\n함) 이양일에 시행한다.",
            "이 조례는 사무(위임사무를\n포함한다. 이하 같다)가 이양되는 날 시행한다.",
            "이 조례는 공포한 날의 다\n음 달 1일 시행한다.",
            "이 조례는 사무가 위임\n된 날 시행한다.",
            "이 조례는 위원회(이하 위원회라 한다)\n의 구성을 마친 날 시행한다.",
            "이 조례는 사무를 규정함\n을 목적으로 하며 의회의 승인을 받은 날 시행한다.",
            "이 조례는 운영을 정한다\n시장은 사업을 시행한다.",
            "이 조례는 사무（시행령에서 정한다. 별도 고시）가 이양되는 날 시행한다.",
            "이 조례는 사무[시행령에서 정한다. 별도 고시]가 이양되는 날 시행한다.",
            "이 조례는 사무［시행령에서 정한다. 별도 고시］가 이양되는 날 시행한다.",
            "이 조례는 사무〔시행령에서 정한다. 별도 고시〕가 이양되는 날 시행한다.",
            "이 조례는 사무「시행령에서 정한다. 별도 고시」가 이양되는 날 시행한다.",
            "이 조례는 사무『시행령에서 정한다. 별도 고시』가 이양되는 날 시행한다.",
            "이 조례는 사무“시행령에서 정한다. 별도 고시”가 이양되는 날 시행한다.",
            "이 조례는 사무‘시행령에서 정한다. 별도 고시’가 이양되는 날 시행한다.",
            "이 조례는 사무<시행령에서 정한다. 별도 고시>가 이양되는 날 시행한다.",
            "이 조례는 사무＜시행령에서 정한다. 별도 고시＞가 이양되는 날 시행한다.",
            "이 조례는 사무〈시행령에서 정한다. 별도 고시〉가 이양되는 날 시행한다.",
            "이 조례는 사무《시행령에서 정한다. 별도 고시》가 이양되는 날 시행한다.",
            "이 조례는 「시험법」(법률 제7호.)이 시행되는 날 시행한다.",
            "이 조례는 사무가 이양되는 날(시행령 개정일. 별도 고시) 시행한다.",
            "이 조례는 「시험법」(법률 제7호)이 시행되는 날 시행한다.",
            "제1조(목적) ① 이 조례는 운영을 정한다\n② 시장은 사업을 시행한다.",
            "제2조(적용범위) 이 조례는 다음 각 호의 사업에 적용한다\n1. 시가 직접 시행함에 따라 생기는 사업\n2. 그 밖의 사업",
            "제2조(적용범위) 이 조례는 다음 각 호의 사업에 적용하되\n1. 시가 직접 시행함에 따라 생기는 사업",
            "이 조례는 2030년 1월 1일\n시행한다.",
            "이 조례는 의회의 승인을 받은 날\n시행한다.",
            "이 조례는 2030년 1월 1일 시행한다.",
            "제1조(목적) 이 조례는 시의 사업에 관한 사항을 정한다\n제2조 시장은 사업을 시행한다.",
            "이 조례는 운영에 관하여 정함\n시장은 사업을 시행한다.",
            "이 조례는 \"시행령에서\n정한다. 별도 고시\"가 있는 날 시행한다.",
            "이 조례는 '시행령에서 정한다. 별도 고시'가 있는 날 시행한다.",
            "이 조례는 2030. 1\n1. 시행한다.",
            "이 조례는 제1호,\n2. 의회의 승인을 받은 날 시행한다.",
            "이 조례는 2030년\n1. 1. 시행한다.",
            "이 조례는 2030년 1월 1일\n1. 시행한다.",
            "이 조례는 2030. 1. \n1. 시행한다.",
            "이 조례는 \"운영을 정하며\n제2조 시장은 사업을 시행한다.",
            "이 조례는 운영을 정하며\n1.5배의 범위에서 의회의 승인을 받은 날 시행한다.",
            "이 조례는 사무(제5조\n① 및 ②의 사무를 말한다)가 이양되는 날 시행한다.",
            "이 조례는 사업의 범위(시의 사업을 말한다. 이하 같다)를 정하고 그 사업을 시행함에 있어 필요한 사항을 정한다.",
            "이 조례는 사업(시행령 개정일. 시가 직접 시행함에 따라 생기는 사업)에 적용한다.",
            "제1조 이 조례는 운영을 정한다.\n제2조 이 조례는 사업을 시행한다.",
            "이 조례는 운영을 정한다\n제2조 시장은 사업을 시행한다",
            "이 조례는 운영을 정한다\n부칙\n시장은 사업을 시행한다",
            "이 조례는 2030. 1.\n\n1. 시행한다.",
            "이 고시는 2030년 1월 1일\n시행한다.",
            "부칙\n이 조례는 공포한 날 시행한다. <단서생략> 다만, 제2조는 따로 정한다.\n부칙 <2020.1.1 조례 제7호>",
            "이 조례는 운영을 정하며\n   제2조 시장은 사업을 시행한다.",
            "이 조례는 운영을 정하며\n  부칙\n시장은 사업을 시행한다",
            "이 조례는 운영을 정하며\n   1. 시장은 사업을 시행한다.",
            "이 조례는 운영을 정하며\n   (1) 시장은 사업을 시행한다.",
            "이 조례는 운영을 정하며\n   가. 시장은 사업을 시행한다.",
            "이 조례는 운영을 정하며\n\t② 시장은 사업을 시행한다.",
            "이 조례는 사무(위임사무를 포함한다\n① 및 ②의 사무)가 이양되는 날 시행한다.",
            "이 조례는 \"운영을 정하며\n① 시장은 사업을 시행한다.",
            "이 조례는 같은 조\n①에 따른 승인을 받은 날 시행한다.",
            "이 조례는 시험법 제5조\n①항의 개정규정이 시행되는 날 시행한다.",
            "이 조례는 운영을 정하며\n①(시행일) 시장은 사업을 시행한다.",
            "제2조(적용범위) 이 조례는 다음 각 항의 사업에 적용한다\n① 시가 직접 시행함에 따라 생기는 사업",
            "이 조례는 요금을 정하며 3. 식품접객업(영업장 면적 125㎡이상 300㎡미만 : 업체당 월 20,000원\n② 수수료는 납부한 날 시행한다.",
            "이 조례는 운영을 정하며\n  ② 시장은 사업을 시행한다.",
            "이 조례는 사무(시행령 개정일\n부칙\n시장은 사업을 시행한다.",
            "이 조례는 ２０３０. １\n１. 시행한다.",
            "이 조례는 ２０３０．１．\n１. 시행한다.",
            "이 조례는 제１호，\n２. 의회의 승인을 받은 날 시행한다.",
            "이 조례는 2030. 1\n１. 시행한다.",
            "이 조례는 ２０３０．１．\n1. 시행한다.",
            "이 조례는 제１호，\n2. 의회의 승인을 받은 날 시행한다.",
            "이 조례는 운영을 정하며\n１. 시장은 사업을 시행한다.",
            "이 조례는 ２０３０. １\n1. 시행한다.",
            "이 조례는 같은 조\n(1)에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 조\n(1)항에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조의 규정에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조의2에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제５조에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n부칙에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n1.에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n1)에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n가.에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n（1） 승인을 받은 날 시행한다.",
            "이 조례는 운영을 정하며\n  (1) 시장은 사업을 시행한다.",
            "이 조례는 운영을 정하며\n제2조(적용) 시장은 사업을 시행한다.",
            "이 조례는 운영을 정하며\n제2조의2(적용) 시장은 사업을 시행한다.",
            "이 조례는 운영을 정하며\n제2조의2 시장은 사업을 시행한다.",
            "이 조례는 운영을 정하며\n제2조\n시장은 사업을 시행한다.",
            "이 조례는 운영을 정하며\n부칙 <2020.1.1> 시장은 사업을 시행한다.",
            "이 조례는 운영을 정하며\n부칙<2020.1.1> 시장은 사업을 시행한다.",
            "이 조례는 운영을 정하며\n부칙(제7호) 시장은 사업을 시행한다.",
            "이 조례는 운영을 정하며\n부 칙\n시장은 사업을 시행한다.",
            "이 조례는 사무(시행령 개정일\n제2조(적용) 시장은 사업을 시행한다.",
            "이 조례는 사무(시행령 개정일\n부칙 <2020.1.1> 시장은 사업을 시행한다.",
            "이 조례는 2030{mark}\n1. 시행한다.",
            "이 조례는 2030年1月\n1. 시행한다.",
            "이 조례는 운영을 정한다\n1. 시행한다.",
            "이 조례는 운영을 정하며\n1. 시행한다.",
            "이 조례는 같은 법\n제5조를 적용하여 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조는 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조와 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제５조 시장은 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n부칙을 적용하는 날 시행한다.",
            "이 조례는 같은 법\n부칙은 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 \t및 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 　및 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n① \t및 ② 승인을 받은 날 시행한다.",
            "이 조례는 같은 법 제3조 및\n1. 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조 또는\n1. 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조 및\n(1) 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조 또는\n가. 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법\n제5조  및 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조\t및 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조　및 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n1.  에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 및 제6조에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 조\n① 및 ②에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 조\n1. 에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 조\n(1) 에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 조\n가. 에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n부칙 및 별표에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 및 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n① 및 ② 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n1. 및 2. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n(1) 및 (2) 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n가. 및 나. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 또는 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n① 또는 ② 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n1. 또는 2. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n(1) 또는 (2) 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n가. 또는 나. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 내지 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n① 내지 ② 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n1. 내지 2. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n(1) 내지 (2) 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n가. 내지 나. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 에 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n① 에 ② 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n1. 에 2. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n(1) 에 (2) 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n가. 에 나. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 의 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n① 의 ② 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n1. 의 2. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n(1) 의 (2) 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n가. 의 나. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 에서 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n① 에서 ② 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n1. 에서 2. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n(1) 에서 (2) 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n가. 에서 나. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 에 따른 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n① 에 따른 ② 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n1. 에 따른 2. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n(1) 에 따른 (2) 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n가. 에 따른 나. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 에따른 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n① 에따른 ② 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n1. 에따른 2. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n(1) 에따른 (2) 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n가. 에따른 나. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 에 의한 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n① 에 의한 ② 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n1. 에 의한 2. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n(1) 에 의한 (2) 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n가. 에 의한 나. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 에의한 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n① 에의한 ② 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n1. 에의한 2. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n(1) 에의한 (2) 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n가. 에의한 나. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 부터 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n① 부터 ② 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n1. 부터 2. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n(1) 부터 (2) 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n가. 부터 나. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 까지 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n① 까지 ② 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n1. 까지 2. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n(1) 까지 (2) 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n가. 까지 나. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 ㆍ 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n① ㆍ ② 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n1. ㆍ 2. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 · 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n① · ② 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n1. · 2. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 , 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n① , ② 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n1. , 2. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 ， 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n① ， ② 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n1. ， 2. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 、 제6조 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n① 、 ② 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n1. 、 2. 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 제1항에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조의2 제1항에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 제1호에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조의2 제1호에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 제1목에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조의2 제1목에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 각 호에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조의2 각 호에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 단서에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조의2 단서에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 본문에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조의2 본문에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 전단에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조의2 전단에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 후단에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조의2 후단에 따른 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제5조 제1항 중 승인을 받은 날 시행한다.",
            "이 조례는 같은 법 제3조 및\n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조 및 \n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조 및\n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조 및 \n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조 및\n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조 및 \n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조 또는\n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조 또는 \n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조 또는\n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조 또는 \n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조 또는\n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조 또는 \n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조 내지\n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조 내지 \n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조 내지\n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조 내지 \n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조 내지\n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조 내지 \n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조,\n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조, \n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조,\n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조, \n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조,\n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조, \n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조，\n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조， \n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조，\n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조， \n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조，\n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조， \n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조、\n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조、 \n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조、\n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조、 \n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조、\n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조、 \n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조､\n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조､ \n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조､\n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조､ \n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조､\n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조､ \n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조·\n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조· \n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조·\n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조· \n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조·\n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조· \n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조ㆍ\n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조ㆍ \n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조ㆍ\n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조ㆍ \n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조ㆍ\n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조ㆍ \n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조・\n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조・ \n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조・\n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조・ \n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조・\n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조・ \n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조･\n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조･ \n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조･\n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조･ \n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조･\n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조･ \n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조~\n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조~ \n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조~\n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조~ \n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조~\n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조~ \n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조～\n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조～ \n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조～\n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조～ \n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조～\n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조～ \n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조∼\n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조∼ \n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조∼\n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조∼ \n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조∼\n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조∼ \n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조〜\n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조〜 \n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조〜\n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조〜 \n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조〜\n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조〜 \n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조-\n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조- \n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조-\n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조- \n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조-\n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조- \n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조－\n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조－ \n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조－\n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조－ \n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조－\n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조－ \n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조‐\n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조‐ \n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조‐\n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조‐ \n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조‐\n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조‐ \n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조–\n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조– \n제5조 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조–\n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조– \n② 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조–\n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 같은 법 제3조– \n부칙 <2020.1.1> 시장이 정하는 날 시행한다.",
            "이 조례는 제1호,\n2. 시행한다.",
            "이 조례는 제1호，\n2. 시행한다.",
            "이 조례는 제1호、\n2. 시행한다.",
            "이 조례는 제1호､\n2. 시행한다.",
            "이 조례는 제1호·\n2. 시행한다.",
            "이 조례는 제1호ㆍ\n2. 시행한다.",
            "이 조례는 제1호・\n2. 시행한다.",
            "이 조례는 제1호･\n2. 시행한다.",
            "이 조례는 제1호~\n2. 시행한다.",
            "이 조례는 제1호～\n2. 시행한다.",
            "이 조례는 제1호∼\n2. 시행한다.",
            "이 조례는 제1호〜\n2. 시행한다.",
            "이 조례는 제1호-\n2. 시행한다.",
            "이 조례는 제1호－\n2. 시행한다.",
            "이 조례는 제1호‐\n2. 시행한다.",
            "이 조례는 제1호–\n2. 시행한다.",
            "이 조례는 제1호 내지\n2. 시행한다.",
            "이 조례는 운영을 정하며\n제2조\n및 시장은 사업을 시행한다.",
            "이 조례는 운영을 정하며\n제2조\n에 따라 시장은 사업을 시행한다.",
            "이 조례는 같은 조\n① 시장은 승인을 받은 날 시행한다.",
            "이 조례는 같은 조\n1. 시장은 승인을 받은 날 시행한다.",
            "이 조례는 같은 조\n(1) 시장은 승인을 받은 날 시행한다.",
            "이 조례는 같은 조\n가. 시장은 승인을 받은 날 시행한다.",
            "이 조례는 같은 조\n① 의장은 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제2조 에너지 승인을 받은 날 시행한다.",
            "이 조례는 같은 법\n제2조 시장은 승인을 받은 날 시행한다.",
            "이 조례는 운영 등.\n제2조 시장은 사업을 시행한다.",
            "이 조례는 2030. 1. 1\n제2조 시장은 사업을 시행한다.",
            "이 조례는 운영 등.\n② 시장은 사업을 시행한다.",
            "이 조례는 운영 등.\n부칙 <2020.1.1> 시장은 사업을 시행한다.",
            "이 조례는 2030. 1. 1\n② 시장은 사업을 시행한다.",
            "이 조례는 2030.\n1. 시행한다.",
            "이 조례는 2030,\n1. 시행한다.",
            "이 조례는 2030．\n1. 시행한다.",
            "이 조례는 2030，\n1. 시행한다.",
            "이 조례는 2030。\n1. 시행한다.",
            "이 조례는 2030｡\n1. 시행한다.",
            "이 조례는 2030、\n1. 시행한다.",
            "이 조례는 2030､\n1. 시행한다.",
            "이 조례는 2030·\n1. 시행한다.",
            "이 조례는 2030ㆍ\n1. 시행한다.",
            "이 조례는 2030・\n1. 시행한다.",
            "이 조례는 2030･\n1. 시행한다.",
            "이 조례는 2030년\n1. 시행한다.",
            "이 조례는 2030월\n1. 시행한다.",
            "이 조례는 2030일\n1. 시행한다.",
            "이 조례는 2030年\n1. 시행한다.",
            "이 조례는 2030月\n1. 시행한다.",
            "이 조례는 2030日\n1. 시행한다.",
            "이 조례는 2030~\n1. 시행한다.",
            "이 조례는 2030～\n1. 시행한다.",
            "이 조례는 2030∼\n1. 시행한다.",
            "이 조례는 2030〜\n1. 시행한다.",
            "이 조례는 2030-\n1. 시행한다.",
            "이 조례는 2030－\n1. 시행한다.",
            "이 조례는 2030‐\n1. 시행한다.",
            "이 조례는 2030–\n1. 시행한다.",
        ];
        let not_detected: &[&str] = &[
            "이 조례는 \"운영을 정한다\". 시장은 사업을 시행한다.",
            "이 조례는 운영을 정한다。 시장은 사업을 시행한다.",
            "이 조례는 운영을 정한다. 시장은 사업을 시행한다.",
            "이 조례는 운영을 정함. 시장은 사업을 시행한다.",
            "이 조례는 운영은 다음과 같음. 시장은 사업을 시행한다.",
            "이 조례는 운영은 시장의 소관임. 시장은 사업을 시행한다.",
            "이 조례는 운영을 시장이 맡게 됨. 시장은 사업을 시행한다.",
            "이 조례는 운영을 정한다(개정 2010. 1. 1.). 시장은 사업을 시행한다.",
            "이 조례는 운영(주)을 정한다. 시장은 사업을 시행한다.",
            "이 조례는 운영（주）을 정한다. 시장은 사업을 시행한다.",
            "이 조례는 운영[주]을 정한다. 시장은 사업을 시행한다.",
            "이 조례는 운영［주］을 정한다. 시장은 사업을 시행한다.",
            "이 조례는 운영〔주〕을 정한다. 시장은 사업을 시행한다.",
            "이 조례는 운영「주」을 정한다. 시장은 사업을 시행한다.",
            "이 조례는 운영『주』을 정한다. 시장은 사업을 시행한다.",
            "이 조례는 운영“주”을 정한다. 시장은 사업을 시행한다.",
            "이 조례는 운영‘주’을 정한다. 시장은 사업을 시행한다.",
            "이 조례는 운영<주>을 정한다. 시장은 사업을 시행한다.",
            "이 조례는 운영＜주＞을 정한다. 시장은 사업을 시행한다.",
            "이 조례는 운영〈주〉을 정한다. 시장은 사업을 시행한다.",
            "이 조례는 운영《주》을 정한다. 시장은 사업을 시행한다.",
            "제1조(목적) 이 조례는 운영을 정한다.\n제2조 시장은 사업을 시행한다.",
            "제1조(목적) 이 조례는 시의 사업에 관한 사항을 정한다.\n제2조(적용범위) ① 이 조례는 시의 사업에 적용한다.\n② 시장은 필요한 사업을 시행한다.",
            "제1조 이 조례는 필요한 사항을 정한다.\n제2조 사업을 시행함에 있어 지켜야 한다.",
            "이 조례는 '운영을 정한다'. 시장은 사업을 시행한다.",
            "이 조례는 가) 운영을 정한다. 시장은 사업을 시행한다.",
            "이 조례는 운영을 정한다<개정>. 시장은 사업을 시행한다.",
            "이 조례는 필요한 사항을 정한다.\n제2조 사업을 시행함에 있어 지켜야 한다.",
            "이 조례는 운영을 정한다.\n제2조 시장은 사업을 시행한다.",
        ];
        for text in detected {
            let source = case("", text, "20200101", "제정", 7);
            if explicit_date.contains(text) {
                expect(&source, "confirmed", Some("2030-01-01"), text);
            } else {
                expect(&source, "unresolved", None, text);
            }
        }
        for text in not_detected {
            expect(
                &case("", text, "20200101", "제정", 7),
                "confirmed",
                Some("2020-01-22"),
                text,
            );
        }
    }

    #[test]
    fn r21_cases_cut_by_the_old_line_boundaries_are_unresolved() {
        // user-broad-detection-20261110 (user-approved conservative limit): these were 21 days
        // when a line start ended the sentence (920005, 940001, 940002, 9100109 and heading forms).
        for text in [
            "제1조(목적) ① 이 조례는 운영을 정한다\n② 시장은 사업을 시행한다.",
            "제2조(적용범위) 이 조례는 다음 각 호의 사업에 적용한다\n1. 시가 직접 시행함에 따라 생기는 사업",
            "제2조(적용범위) ① 이 조례는 시의 사업에 적용한다\n② 시장은 필요한 사업을 시행한다.",
            "제2조(적용범위) 이 조례는 다음 각 항의 사업에 적용한다\n① 시가 직접 시행함에 따라 생기는 사업",
            "이 조례는 운영을 정하며\n제2조 시장은 사업을 시행한다.",
            "이 조례는 운영을 정하며\n① 시장은 사업을 시행한다.",
            "이 조례는 운영을 정하며\n부칙 <2020.1.1> 시장은 사업을 시행한다.",
        ] {
            assert!(names_effective_date(text), "{text:?}");
            expect(
                &case("", text, "20200101", "제정", 7),
                "unresolved",
                None,
                text,
            );
        }
        // Kept at 21 days: the full stop ends the sentence before the next article, and a text
        // that names no effective date.
        for text in [
            "제1조(목적) 이 조례는 운영을 정한다.\n제2조 시장은 사업을 시행한다.",
            "제1조 이 조례는 시의 사업에 관한 사항을 정한다.\n제2조(적용범위) ① 이 조례는 시의 사업에 적용한다.\n② 시장은 필요한 사업을 시행한다.",
            "제1조(목적) 이 조례는 운영을 정한다.",
        ] {
            assert!(!names_effective_date(text), "{text:?}");
            expect(
                &case("", text, "20200101", "제정", 7),
                "confirmed",
                Some("2020-01-22"),
                text,
            );
        }
    }

    #[test]
    fn r21_a_sentence_the_reader_does_not_read_keeps_its_section_unresolved() {
        // The detection finds the conditional sentence, the reader cannot read it across the
        // line feed, and the explicit date of the same section must not be taken (Codex 7).
        let body = format!(
            "이 조례는 같은 조\n(1)에 따른 승인을 받은 날 시행한다.\n이 조례는{}2030년 1월 1일 시행한다.",
            " ".repeat(128)
        );
        expect(
            &case("", &body, "20200101", "제정", 7),
            "unresolved",
            None,
            "codex 7",
        );
        // The verb "시행함" and a subject the reader does not know are read as unread sentences.
        for text in [
            "부칙\n이 조례는 공포한 날부터 시행한다.\n이 시행규칙은 승인을 받은 날 시행함.",
            "이 조례는 승인을 받은 날\n시행한다. 이 조례는 2030년 1월 1일부터 시행한다.",
        ] {
            expect(
                &case("", text, "20200101", "제정", 7),
                "unresolved",
                None,
                text,
            );
        }
        // Controls: the sentences are read, so the section keeps its date.
        expect(
            &case(
                "",
                "이 조례는 2030년 1월 1일부터 시행한다.",
                "20200101",
                "제정",
                7,
            ),
            "confirmed",
            Some("2030-01-01"),
            "read explicit date",
        );
        expect(
            &case(
                "",
                "부칙\n이 조례는 공포한 날부터 시행한다.",
                "20200101",
                "제정",
                7,
            ),
            "confirmed",
            Some("2020-01-01"),
            "read promulgation day",
        );
        // The attributed branch: a plain sentence and an unread sentence in the same text.
        assert!(plain_promulgation_clause("이 조례는 공포한 날부터 시행한다.", "조례").is_some());
        assert!(
            plain_promulgation_clause(
                "이 조례는 공포한 날부터 시행한다.\n이 조례는 승인을 받은 날\n시행한다.",
                "조례"
            )
            .is_none()
        );
    }

    #[test]
    fn r21_the_hadoe_form_and_the_applied_form_are_unresolved() {
        // User decision user-hadoe-proviso-20261110: "시행하되" counts as a proviso, so a section
        // with "공포한 날부터 시행하되, ...부터 적용한다" has no date (MST 1045663, 817067). The
        // compiler reaches that state through the unread sentence of user-broad-detection-20261110;
        // the verb "시행하되" is no key of its own, and the evidence stays as it was.
        // MST 1151198: the application article with "시행되는" is a sentence of the same kind
        // (the conservative limit of the broad detection), so the section is unresolved as well.
        for (amendment, promulgation, text) in [
            (
                "일부개정",
                "20091026",
                "부칙 <2009.10.26>\n이 조례는 공포한 날부터 시행하되, 2009년 10월 2일부터 적용한다.\n",
            ),
            (
                "일부개정",
                "20100107",
                "부칙 <2010.1.7>\n이 조례는 공포한 날부터 시행하되, 제3장에 따른 셋째 이상 자녀 교육비등은 2010년 7월 1일부터 지원한다.\n",
            ),
            (
                "일부개정",
                "20120308",
                "부칙 <2012.03.08>\n제1조(시행일) 이 규칙은 공포한 날부터 시행한다.\n제2조(적용례) 이 규칙은 2012년 1월 1일 이후 시행되는 임용시험부터 적용한다.\n",
            ),
        ] {
            let source = revision("조례", amendment, promulgation, 1, text);
            let found = resolve("ordinances", &source).unwrap();
            assert_eq!(found.status, "unresolved", "{text}");
            assert_eq!(found.enforcement_date, None, "{text}");
        }
        // The same forms in the articles of an enactment.
        for text in [
            "부칙\n이 조례는 공포한 날부터 시행하되, 2009년 10월 2일부터 적용한다.",
            "부칙\n이 규칙은 공포한 날부터 시행한다.\n이 규칙은 2012년 1월 1일 이후 시행되는 임용시험부터 적용한다.",
        ] {
            expect(
                &case("", text, "20091026", "제정", 7),
                "unresolved",
                None,
                text,
            );
        }
        // Control: the plain sentence alone keeps its date.
        expect(
            &case(
                "",
                "부칙\n이 조례는 공포한 날부터 시행한다.",
                "20091026",
                "제정",
                7,
            ),
            "confirmed",
            Some("2009-10-26"),
            "plain",
        );
    }

    #[test]
    fn r22_the_detection_holds_every_subject_and_verb_the_reader_knows() {
        // Codex 8, block 1: "효력을 발생한다" over a line feed is a sentence the reader cannot
        // read, so it stays unread and the explicit date of the same text is not taken.
        let body = "이 조례는 의회의 승인을 받은 날\n효력을 발생한다.\n이 조례는 2030년 1월 1일부터 시행한다.";
        expect(
            &case("", body, "20200101", "제정", 7),
            "unresolved",
            None,
            "block 1",
        );
        assert!(
            plain_promulgation_clause(
                "이 조례는 공포한 날부터 시행한다.\n이 조례는 의회의 승인을 받은 날\n효력을 발생한다.",
                "조례"
            )
            .is_none()
        );
        // Codex 8, block 2: the subject "이 법률은" is a subject of the reader.
        let law =
            "이 법률은 의회의 승인을 받은 날\n시행한다.\n이 법률은 2030년 1월 1일부터 시행한다.";
        expect(
            &case("", law, "20200101", "제정", 7),
            "unresolved",
            None,
            "block 2",
        );
        assert!(!detected_sentence_starts("이 법률은 승인을 받은 날\n시행한다.", false).is_empty());
        // Controls: the read sentences keep their dates.
        expect(
            &case(
                "",
                "이 조례는 2030년 1월 1일부터 시행한다.",
                "20200101",
                "제정",
                7,
            ),
            "confirmed",
            Some("2030-01-01"),
            "control",
        );
        expect(
            &case(
                "",
                "이 법률은 2030년 1월 1일부터 시행한다.",
                "20200101",
                "제정",
                7,
            ),
            "confirmed",
            Some("2030-01-01"),
            "control law",
        );
    }

    /// Pins the completeness table of the r22 detection: what the detection, the A1 position list
    /// and the reader do with each subject, verb and phrase form (user-r22-full-vocabulary-20261111).
    /// A cell is "label|detected sentences|clauses". A detected form that has no clause is a gap;
    /// the gaps are the approved limits: a phrase without a subject (separate-article timing, user
    /// decision 2026-11-03, and the proviso scope decision), noun uses after "부터", and the two
    /// general sentences of an administrative-rule answer ("효력이 발생", accepted 2026-11-11).
    #[test]
    fn r22_completeness_table_pins_every_form() {
        let mut rows: Vec<(String, String)> = Vec::new();
        for lead in ["이", "본"] {
            for kind in [
                "법률", "법", "영", "조례", "규칙", "훈령", "예규", "고시", "공고", "지침", "규정",
            ] {
                rows.push((
                    format!("subject {lead} {kind}은"),
                    format!("{lead} {kind}은 공포한 날부터 시행한다."),
                ));
            }
        }
        for subject in [
            "이 시행규칙은",
            "이 시행령은",
            "이 개정조례는",
            "이 조례 시행규칙은",
            "이 영 시행규칙은",
            "이 세칙은",
            "이 요령은",
            "이 기준은",
            "이 조례가",
            "이\n조례는",
            "이 조례는",
        ] {
            rows.push((
                format!("subject-extra {}", subject.replace('\n', "<LF>")),
                format!("{subject} 공포한 날부터 시행한다."),
            ));
        }
        for verb in [
            "시행한다",
            "시행하며",
            "시행하고",
            "시행하되",
            "시행함",
            "시행한",
            "시행할",
            "시행해",
            "시행했",
            "시행되",
            "시행된다",
            "시행됨",
            "시행될",
            "시행돼",
            "시행됐",
            "施行한다",
            "施行",
            "시행 한다",
            "시\n행한다",
            "효력을 발생한다",
            "효력을 발생하며",
            "효력을\n발생한다",
            "효력이 발생한다",
            "시행에 관하여",
            "시행령",
        ] {
            rows.push((
                format!("verb {}", verb.replace('\n', "<LF>")),
                format!("이 조례는 공포한 날부터 {verb}."),
            ));
        }
        for (label, text) in [
            (
                "phrase no-subject 제3조는 날짜 부터 시행한다",
                "제3조는 2010년 1월 1일부터 시행한다.",
            ),
            (
                "phrase no-subject 개정조례 시행일부터 시행한다",
                "개정조례 시행일부터 시행한다.",
            ),
            (
                "phrase no-subject 효력을 발생한다",
                "공포한 날부터 효력을 발생한다.",
            ),
            (
                "phrase no-subject 날짜 부터 시행한다",
                "2010년 1월 1일부터 시행한다.",
            ),
            (
                "line 조건부 시행한다",
                "이 조례는 승인을 받은 날\n시행한다.",
            ),
            (
                "line 조건부 효력을 발생한다",
                "이 조례는 승인을 받은 날\n효력을 발생한다.",
            ),
            (
                "line 공포한 날부터 LF 시행한다",
                "이 조례는 공포한 날부터\n시행한다.",
            ),
            ("line 주어 뒤 LF", "이 조례는\n공포한 날부터 시행한다."),
        ] {
            rows.push((label.to_owned(), text.to_owned()));
        }
        for (label, text) in [
            ("v2 verb 시행토록", "이 조례는 공포한 날 시행토록 한다."),
            ("v2 verb 시행키로", "이 조례는 공포한 날 시행키로 한다."),
            ("v2 verb 시행코자", "이 조례는 공포한 날 시행코자 한다."),
            ("v2 verb 시행케", "이 조례는 공포한 날 시행케 한다."),
            ("v2 verb 시행시킨다", "이 조례는 공포한 날 시행시킨다."),
            ("v2 verb 시행. (문장 끝)", "이 조례는 공포일에 시행."),
            (
                "v2 phrase 효력이 발생한다",
                "이 조례는 공포한 날부터 효력이 발생한다.",
            ),
            (
                "v2 phrase 부터 효력을 가진다",
                "이 조례는 공포한 날부터 효력을 가진다.",
            ),
            (
                "v2 subject 이 條例는 施行한다",
                "이 條例는 公布한 날 施行한다.",
            ),
            (
                "v2 subject 이 개정 조례는",
                "이 개정 조례는 공포한 날 시행한다.",
            ),
            ("v2 subject 이 조례 는", "이 조례 는 공포한 날 시행한다."),
            (
                "v2 subject 이 조례(안)는",
                "이 조례(안)는 공포한 날 시행한다.",
            ),
            (
                "v2 subject 이 기준은 (adm)",
                "이 기준은 고시한 날 시행한다.",
            ),
            (
                "v2 subject 이 요령은 (adm)",
                "이 요령은 발령한 날 시행한다.",
            ),
            (
                "v2 subject 이 세칙은 (adm)",
                "이 세칙은 고시한 날 시행한다.",
            ),
            (
                "v2 addendum subject-less 부칙 LF 공포한 날 시행한다",
                "부칙\n공포한 날 시행한다.",
            ),
            (
                "v2 addendum subject-less 부칙 날짜 공포한 날 시행한다",
                "부칙 <2024.3.1>\n공포한 날 시행한다.",
            ),
            (
                "v2 addendum subject-less 제1조(시행일)",
                "부칙\n제1조(시행일) 공포한 날 시행한다.",
            ),
            (
                "v2 proviso paren 다만 after 시행한다",
                "이 조례는 2030년 1월 1일 시행한다(다만, 제3조는 공포한 날부터 시행한다).",
            ),
            (
                "v2 noun control 시행에 관하여",
                "이 조례는 농업인회관의 시행에 관하여 필요한 사항을 정한다.",
            ),
            ("v2 plain control", "이 조례는 공포한 날부터 시행한다."),
            (
                "v3 particle 이 조례가 공포한 날 시행된다",
                "이 조례가 공포한 날 시행된다.",
            ),
            (
                "v3 particle 이 규칙이 시행되는 해",
                "이 규칙이 시행되는 해에는 정한다.",
            ),
            (
                "v3 no-subject 효력이 발생하며",
                "압류의 효력이 발생한 이후에 등기된 가처분등기는 말소한다.",
            ),
            (
                "v3 no-subject 부터 효력을 가진다",
                "공포한 날부터 효력을 가진다.",
            ),
            ("v4 subject 이 法律은", "이 法律은 公布한 날 施行한다."),
            ("v4 subject 이 法은", "이 法은 公布한 날 施行한다."),
            ("v4 subject 이 令은", "이 令은 公布한 날 施行한다."),
            ("v4 subject 이 規則은", "이 規則은 公布한 날 施行한다."),
            ("v4 subject 이 訓令은", "이 訓令은 公布한 날 施行한다."),
            ("v4 subject 이 例規는", "이 例規는 公布한 날 施行한다."),
            ("v4 subject 이 告示는", "이 告示는 公布한 날 施行한다."),
            ("v4 subject 이 公告는", "이 公告는 公布한 날 施行한다."),
            ("v4 subject 이 指針은", "이 指針은 公布한 날 施行한다."),
            ("v4 subject 이 規定은", "이 規定은 公布한 날 施行한다."),
            ("v4 subject 本條例는", "本條例는 공포한 날 施行한다."),
            (
                "v4 subject 이 훈령은 (kind 령)",
                "이 훈령은 발령한 날 시행토록 한다.",
            ),
            (
                "v4 subject 이 요령은 (kind 령)",
                "이 요령은 발령한 날 시행토록 한다.",
            ),
            ("v4 verb 시행치", "이 조례는 공포한 날 시행치 아니한다."),
            ("v4 verb 시행시키", "이 조례는 공포한 날 시행시키기로 한다."),
            ("v4 verb 시행시켜", "이 조례는 공포한 날 시행시켜야 한다."),
            ("v4 verb 시행시킬", "이 조례는 공포한 날 시행시킬 수 있다."),
            (
                "v4 phrase 효력이 발생 (no 부터)",
                "이 조례는 승인을 받은 날 효력이 발생한다.",
            ),
            (
                "v4 phrase 부터 그 효력",
                "이 조례는 공포한 날부터 그 효력을 가진다.",
            ),
            (
                "v4 phrase no-subject 효력을 발생 (no 부터)",
                "승인을 받은 날 효력을 발생한다.",
            ),
            (
                "v4 heading inside a sentence",
                "공원의 점용은 「도시공원법」 부칙 제6조의 사업을 시행할 계획이 없는 경우에 한한다.",
            ),
            (
                "v4 heading title with a verb",
                "부칙\n제1조(시행하는 날) 공포일에 정한다.",
            ),
            (
                "v4 heading label with a verb",
                "부칙 <시행하는 규칙 제3호>\n공포일에 정한다.",
            ),
            (
                "v4 heading item mark before the subject",
                "부칙\n① 이 조례는 공포한 날부터 시행한다.",
            ),
            (
                "v4 end 진행. is no end",
                "이 조례는 사업의 진행. 시장은 공포한 날 시행한다.",
            ),
            (
                "v4 prefix stops at a full stop (first)",
                "의회가 위촉한 것으로 본다.이 조례는 공포한 날부터 시행한다.",
            ),
            (
                "v4 prefix stops at a full stop (second)",
                "본다 가.이조례는 공포한 날부터 시행한다.",
            ),
            (
                "v5 heading NBSP before 부칙",
                "\u{A0}부칙\n공포한 날 시행한다.",
            ),
            (
                "v5 heading U+3000 before 부칙",
                "\u{3000}부칙\n공포한 날 시행한다.",
            ),
            (
                "v5 heading label then subject-less then plain",
                "부칙 <2024.3.1 조례 제7호>\n의회의 승인을 받은 날 시행한다.\n이 조례는 공포한 날부터 시행한다.",
            ),
            (
                "v5 heading article mark",
                "부칙\n제1조(시행일) 공포 후 30일이 경과한 날부터 시행한다.",
            ),
            (
                "v5 heading sentence with another subject",
                "부칙\n공포한 날 시행하고, 이 조례가 정한 위원회는 6개월 안에 구성한다.",
            ),
            (
                "v5 quoted 부칙 is no heading",
                "제2조 다른 조례의 \"부칙 제1조\"는 공포한 날 시행한다.",
            ),
            (
                "v5 subject NBSP before the particle",
                "이 조례\u{A0}는 공포한 날 시행한다.",
            ),
            (
                "v5 subject U+3000 before the particle",
                "이 조례\u{3000}는 공포한 날 시행한다.",
            ),
            (
                "v5 subject tab before the particle",
                "이 조례\t는 공포한 날 시행한다.",
            ),
            (
                "v5 subject two spaces before the particle",
                "이 조례  는 공포한 날 시행한다.",
            ),
            (
                "v5 subject NBSP inside",
                "이 개정\u{A0}조례는 공포한 날 시행한다.",
            ),
            (
                "v5 subject three words",
                "이 일부 개정 조례는 공포한 날 시행한다.",
            ),
            (
                "v5 subject two spaces inside",
                "이 조례  시행규칙은 공포한 날 시행한다.",
            ),
            (
                "v5 subject fullwidth note",
                "이 조례（안）는 공포한 날 시행한다.",
            ),
            (
                "v5 subject 6-char prefix 훈령",
                "이 대통령경호처훈령은 발령한 날 시행한다.",
            ),
            (
                "v5 subject 6-char prefix 요령",
                "이 새만금개발청요령은 발령한 날 시행한다.",
            ),
            (
                "v5 subject 6-char prefix 訓令",
                "이 大統領警護處訓令은 公布한 날 施行한다.",
            ),
            ("v5 subject 이.조례는", "이.조례는 공포한 날 시행한다."),
            ("v5 subject 내규", "이 내규는 발령한 날 시행한다."),
            ("v5 verb 시행합니다", "이 조례는 공포한 날 시행합니다."),
            ("v5 verb 시행됩니다", "이 조례는 공포한 날 시행됩니다."),
            (
                "v5 verb 시행하였습니다",
                "이 조례는 공포한 날 시행하였습니다.",
            ),
            ("v5 verb 시행시킴", "이 조례는 공포한 날 시행시킴."),
            ("v5 verb 시행시켰", "이 조례는 공포한 날 시행시켰다."),
            ("v5 verb 시행시킵", "이 조례는 공포한 날 시행시킵니다."),
        ] {
            rows.push((label.to_owned(), text.to_owned()));
        }
        let expected: &[&str] = &[
            "subject 이 법률은|1|Plain",
            "subject 이 법은|1|Plain",
            "subject 이 영은|1|Plain",
            "subject 이 조례은|1|Plain",
            "subject 이 규칙은|1|Plain",
            "subject 이 훈령은|1|Plain",
            "subject 이 예규은|1|Plain",
            "subject 이 고시은|1|Plain",
            "subject 이 공고은|1|Plain",
            "subject 이 지침은|1|Plain",
            "subject 이 규정은|1|Plain",
            "subject 본 법률은|1|Plain",
            "subject 본 법은|1|Plain",
            "subject 본 영은|1|Plain",
            "subject 본 조례은|1|Plain",
            "subject 본 규칙은|1|Plain",
            "subject 본 훈령은|1|Plain",
            "subject 본 예규은|1|Plain",
            "subject 본 고시은|1|Plain",
            "subject 본 공고은|1|Plain",
            "subject 본 지침은|1|Plain",
            "subject 본 규정은|1|Plain",
            "subject-extra 이 시행규칙은|1|Other",
            "subject-extra 이 시행령은|1|Other",
            "subject-extra 이 개정조례는|1|Other",
            "subject-extra 이 조례 시행규칙은|1|Other",
            "subject-extra 이 영 시행규칙은|1|Other",
            "subject-extra 이 세칙은|1|Other",
            "subject-extra 이 요령은|1|Other",
            "subject-extra 이 기준은|1|Other",
            "subject-extra 이 조례가|1|Other",
            "subject-extra 이<LF>조례는|1|Plain",
            "subject-extra 이 조례는|1|Plain",
            "verb 시행한다|1|Plain",
            "verb 시행하며|1|Other",
            "verb 시행하고|1|Other",
            "verb 시행하되|1|Other",
            "verb 시행함|1|Other",
            "verb 시행한|1|Other",
            "verb 시행할|1|Other",
            "verb 시행해|1|Other",
            "verb 시행했|1|Other",
            "verb 시행되|1|Other",
            "verb 시행된다|1|Other",
            "verb 시행됨|1|Other",
            "verb 시행될|1|Other",
            "verb 시행돼|1|Other",
            "verb 시행됐|1|Other",
            "verb 施行한다|1|Other",
            "verb 施行|1|Other",
            "verb 시행 한다|1|Plain",
            "verb 시<LF>행한다|1|Other",
            "verb 효력을 발생한다|1|Plain",
            "verb 효력을 발생하며|1|Other",
            "verb 효력을<LF>발생한다|1|Plain",
            "verb 효력이 발생한다|1|Other",
            "verb 시행에 관하여|0|",
            "verb 시행령|0|",
            "phrase no-subject 제3조는 날짜 부터 시행한다|0|",
            "phrase no-subject 개정조례 시행일부터 시행한다|0|",
            "phrase no-subject 효력을 발생한다|0|",
            "phrase no-subject 날짜 부터 시행한다|0|",
            "line 조건부 시행한다|1|Other",
            "line 조건부 효력을 발생한다|1|Other",
            "line 공포한 날부터 LF 시행한다|1|Other",
            "line 주어 뒤 LF|1|Plain",
            "v2 verb 시행토록|1|Other",
            "v2 verb 시행키로|1|Other",
            "v2 verb 시행코자|1|Other",
            "v2 verb 시행케|1|Other",
            "v2 verb 시행시킨다|1|Other",
            "v2 verb 시행. (문장 끝)|1|Other",
            "v2 phrase 효력이 발생한다|1|Other",
            "v2 phrase 부터 효력을 가진다|1|Other",
            "v2 subject 이 條例는 施行한다|1|Other",
            "v2 subject 이 개정 조례는|1|Other",
            "v2 subject 이 조례 는|1|Other",
            "v2 subject 이 조례(안)는|1|Other",
            "v2 subject 이 기준은 (adm)|1|Other",
            "v2 subject 이 요령은 (adm)|1|Other",
            "v2 subject 이 세칙은 (adm)|1|Other",
            "v2 addendum subject-less 부칙 LF 공포한 날 시행한다|1|Other",
            "v2 addendum subject-less 부칙 날짜 공포한 날 시행한다|1|Other",
            "v2 addendum subject-less 제1조(시행일)|1|Other",
            "v2 proviso paren 다만 after 시행한다|1|Explicit",
            "v2 noun control 시행에 관하여|0|",
            "v2 plain control|1|Plain",
            "v3 particle 이 조례가 공포한 날 시행된다|1|Other",
            "v3 particle 이 규칙이 시행되는 해|1|Other",
            "v3 no-subject 효력이 발생하며|0|",
            "v3 no-subject 부터 효력을 가진다|0|",
            "v4 subject 이 法律은|1|Other",
            "v4 subject 이 法은|1|Other",
            "v4 subject 이 令은|1|Other",
            "v4 subject 이 規則은|1|Other",
            "v4 subject 이 訓令은|1|Other",
            "v4 subject 이 例規는|1|Other",
            "v4 subject 이 告示는|1|Other",
            "v4 subject 이 公告는|1|Other",
            "v4 subject 이 指針은|1|Other",
            "v4 subject 이 規定은|1|Other",
            "v4 subject 本條例는|1|Other",
            "v4 subject 이 훈령은 (kind 령)|1|Other",
            "v4 subject 이 요령은 (kind 령)|1|Other",
            "v4 verb 시행치|1|Other",
            "v4 verb 시행시키|1|Other",
            "v4 verb 시행시켜|1|Other",
            "v4 verb 시행시킬|1|Other",
            "v4 phrase 효력이 발생 (no 부터)|1|Other",
            "v4 phrase 부터 그 효력|1|Other",
            "v4 phrase no-subject 효력을 발생 (no 부터)|0|",
            "v4 heading inside a sentence|0|",
            "v4 heading title with a verb|0|",
            "v4 heading label with a verb|0|",
            "v4 heading item mark before the subject|1|Plain",
            "v4 end 진행. is no end|1|Other",
            "v4 prefix stops at a full stop (first)|1|Plain",
            "v4 prefix stops at a full stop (second)|1|Plain",
            "v5 heading NBSP before 부칙|1|Other",
            "v5 heading U+3000 before 부칙|1|Other",
            "v5 heading label then subject-less then plain|2|Plain+Other",
            "v5 heading article mark|1|Other",
            "v5 heading sentence with another subject|1|Other",
            "v5 quoted 부칙 is no heading|0|",
            "v5 subject NBSP before the particle|1|Other",
            "v5 subject U+3000 before the particle|1|Other",
            "v5 subject tab before the particle|1|Other",
            "v5 subject two spaces before the particle|1|Other",
            "v5 subject NBSP inside|1|Other",
            "v5 subject three words|1|Other",
            "v5 subject two spaces inside|1|Other",
            "v5 subject fullwidth note|1|Other",
            "v5 subject 6-char prefix 훈령|1|Other",
            "v5 subject 6-char prefix 요령|1|Other",
            "v5 subject 6-char prefix 訓令|1|Other",
            "v5 subject 이.조례는|1|Other",
            "v5 subject 내규|1|Other",
            "v5 verb 시행합니다|1|Other",
            "v5 verb 시행됩니다|1|Other",
            "v5 verb 시행하였습니다|1|Other",
            "v5 verb 시행시킴|1|Other",
            "v5 verb 시행시켰|1|Other",
            "v5 verb 시행시킵|1|Other",
            "v6 heading follow \"\"|1|Other",
            "v6 heading follow \" \"|1|Other",
            "v6 heading follow \"\\u{a0}\"|1|Other",
            "v6 heading follow \"\\u{3000}\"|1|Other",
            "v6 heading follow \"\\t\"|1|Other",
            "v6 heading follow \"[\"|1|Other",
            "v6 heading follow \"<\"|1|Other",
            "v6 heading follow \"＜\"|1|Other",
            "v6 heading follow \"〈\"|1|Other",
            "v6 heading follow \"〈\"|1|Other",
            "v6 heading follow \"(\"|1|Other",
            "v6 heading follow \"（\"|1|Other",
            "v6 heading follow \"［2024.3.1］\"|2|Other+Other",
            "v6 heading follow \"【2024.3.1】\"|1|Other",
            "v6 heading follow \"〔2024.3.1〕\"|1|Other",
            "v6 heading follow \"《2024.3.1》\"|1|Other",
            "v6 heading follow \"「2024.3.1」\"|1|Other",
            "v6 heading follow \"{2024.3.1}\"|1|Other",
            "v6 heading follow \"&lt;2024.3.1&gt;\"|1|Other",
            "v6 heading follow \"&lt;\"|1|Other",
            "v6 heading follow \"제1조(시행일)\"|1|Other",
            "v6 heading follow \"제1조\"|1|Other",
            "v6 heading follow \":\"|1|Other",
            "v6 heading follow \".\"|1|Other",
            "v6 heading follow \",\"|1|Other",
            "v6 heading follow \"-\"|1|Other",
            "v6 heading follow \"·\"|1|Other",
            "v6 heading follow \"ㆍ\"|1|Other",
            "v6 heading follow \"・\"|1|Other",
            "v6 heading follow \"1\"|1|Other",
            "v6 heading follow \"2024.3.1\"|1|Other",
            "v6 heading follow \"\\u{200b}\"|1|Other",
            "v6 heading follow \"\\u{feff}\"|1|Other",
            "v6 heading follow \"\\u{2003}\"|1|Other",
            "v6 heading follow \"\\u{2002}\"|1|Other",
            "v6 heading follow \"을 다음과 같이 한다\"|1|Other",
            "v6 heading follow \"에 따라\"|1|Other",
            "v6 heading follow \"중\"|1|Other",
            "v6 first sentence: verb, stop, subject sentence|2|Explicit+Other",
            "v6 first sentence: verb, comma, subject sentence|2|Explicit+Other",
            "v6 first sentence: verb, no stop, next line|2|Explicit+Other",
            "v6 first sentence: subject first|1|Plain",
            "v6 first sentence: article mark, subject first|1|Plain",
            "v6 first sentence: noun, subject|1|Plain",
            "v6 first sentence: verb in a bracket, subject|2|Plain+Other",
            "v6 first sentence: bracket label, stop|1|Other",
            "v6 first sentence: bracketed subject sentence, verb|2|Other+Other+Other",
            "v6 first sentence: bracketed subject sentence, verb, subject sentence|3|Explicit+Other+Other",
            "v6 first sentence: bracket label, subject sentence|1|Plain",
            "v6 heading split by a line feed|1|Other",
            "v6 heading after an en space|1|Other",
            "v6 heading after CR|1|Other",
        ];
        for (follow, _) in heading_follow_forms() {
            rows.push((
                format!("v6 heading follow {follow:?}"),
                format!("부칙{follow}\n공포한 날 시행한다."),
            ));
        }
        for (label, text) in [
            (
                "v6 first sentence: verb, stop, subject sentence",
                "부칙\n공포한 날 시행한다. 이 조례는 2030년 1월 1일부터 시행한다.",
            ),
            (
                "v6 first sentence: verb, comma, subject sentence",
                "부칙\n의회의 승인을 받은 날 시행하고, 이 조례는 2030년 1월 1일부터 시행한다.",
            ),
            (
                "v6 first sentence: verb, no stop, next line",
                "부칙\n의회의 승인을 받은 날 시행한다\n이 조례는 2030년 1월 1일부터 시행한다.",
            ),
            (
                "v6 first sentence: subject first",
                "부칙\n이 조례는 공포한 날부터 시행한다.",
            ),
            (
                "v6 first sentence: article mark, subject first",
                "부칙\n제1조(시행일) 이 조례는 공포한 날부터 시행한다.",
            ),
            (
                "v6 first sentence: noun, subject",
                "부칙\n시행일은 이 조례는 공포한 날부터 시행한다.",
            ),
            (
                "v6 first sentence: verb in a bracket, subject",
                "부칙\n(시행하는 경우) 이 조례는 공포한 날부터 시행한다.",
            ),
            (
                "v6 first sentence: bracket label, stop",
                "부칙[2024.3.1].\n공포한 날 시행한다.",
            ),
            (
                "v6 first sentence: bracketed subject sentence, verb",
                "부칙\n[이 조례는 운영을 정한다.] 공포한 날 시행한다.",
            ),
            (
                "v6 first sentence: bracketed subject sentence, verb, subject sentence",
                "부칙\n[이 조례는 운영을 정한다.]\n승인을 받은 날 시행하고, 이 조례는 2030년 1월 1일부터 시행한다.",
            ),
            (
                "v6 first sentence: bracket label, subject sentence",
                "부칙[2024.3.1] 이 조례는 공포한 날부터 시행한다.",
            ),
            (
                "v6 heading split by a line feed",
                "부\n칙\n공포한 날 시행한다.",
            ),
            (
                "v6 heading after an en space",
                "\u{2002}부칙\n공포한 날 시행한다.",
            ),
            ("v6 heading after CR", "\r부칙\n공포한 날 시행한다."),
        ] {
            rows.push((label.to_owned(), text.to_owned()));
        }
        let observed: Vec<String> = rows
            .iter()
            .map(|(label, text)| {
                let detected = detected_sentence_starts(text, false).len();
                let clauses: Vec<&str> = read_clauses(text, false)
                    .iter()
                    .map(|clause| match clause.kind {
                        ClauseKind::Plain => "Plain",
                        ClauseKind::Explicit(_) => "Explicit",
                        ClauseKind::Other => "Other",
                    })
                    .collect();
                format!("{label}|{detected}|{}", clauses.join("+"))
            })
            .collect();
        assert_eq!(observed, expected);
        // The phrase test names an effective date, so a form with no clause is a gap.
        let gaps: Vec<&str> = rows
            .iter()
            .filter(|(_, text)| names_effective_date(text) && read_clauses(text, false).is_empty())
            .map(|(label, _)| label.as_str())
            .collect();
        assert_eq!(
            gaps,
            [
                "verb 시행에 관하여",
                "verb 시행령",
                "phrase no-subject 제3조는 날짜 부터 시행한다",
                "phrase no-subject 개정조례 시행일부터 시행한다",
                "phrase no-subject 효력을 발생한다",
                "phrase no-subject 날짜 부터 시행한다",
                "v3 no-subject 효력이 발생하며",
                "v3 no-subject 부터 효력을 가진다",
                "v4 phrase no-subject 효력을 발생 (no 부터)",
            ]
        );
    }

    #[test]
    fn r22_the_detection_knows_more_subjects_than_the_reader() {
        // Subjects the detection knows and the reader does not: the sentence is unread, so it
        // stays an unread sentence and the explicit date of the same text is not taken.
        for subject in [
            "이 시행령은",
            "이 조례 시행규칙은",
            "이 영 시행규칙은",
            "이 세칙은",
            "이 요령은",
            "이 기준은",
            "이 조례가",
            "이 규정이",
        ] {
            let body = format!(
                "{subject} 의회의 승인을 받은 날 시행한다.\n이 조례는 2030년 1월 1일부터 시행한다."
            );
            expect(
                &case("", &body, "20200101", "제정", 7),
                "unresolved",
                None,
                subject,
            );
            let effect = format!(
                "{subject} 의회의 승인을 받은 날 효력을 발생한다.\n이 조례는 2030년 1월 1일부터 시행한다."
            );
            expect(
                &case("", &effect, "20200101", "제정", 7),
                "unresolved",
                None,
                subject,
            );
        }
        // Controls: a subject split by a line feed is an accepted limit
        // (user-r24-subject-boundary-limit-20261111); a subject without a verb is no sentence.
        for text in [
            "이 조례\n시행규칙은 승인을 받은 날 시행한다.\n이 조례는 2030년 1월 1일부터 시행한다.",
            "이 세칙은 승인을 받은 날 정한다.\n이 조례는 2030년 1월 1일부터 시행한다.",
        ] {
            expect(
                &case("", text, "20200101", "제정", 7),
                "confirmed",
                Some("2030-01-01"),
                text,
            );
        }
    }

    #[test]
    fn r22_the_detection_vocabulary_covers_the_forms_of_fresh_verifier_17() {
        // Each form is an effective sentence the reader cannot read: no default period, no date
        // from another sentence of the same section (B1: 997101-997118, 997301-997305, 997801,
        // 997951-997956, 997961, 997962).
        for text in [
            "이 조례는 공포한 날 시행토록 한다.",
            "이 조례는 공포한 날 시행키로 한다.",
            "이 조례는 공포한 날 시행코자 한다.",
            "이 조례는 공포한 날 시행케 한다.",
            "이 조례는 공포한 날 시행시킨다.",
            "이 조례는 공포일에 시행.",
            "이 조례는 공포한 날부터 효력이 발생한다.",
            "이 조례는 공포한 날부터 효력을 가진다.",
            "이 條例는 公布한 날부터 施行한다.",
            "이 條例는 公布한 날 施行한다.",
            "이 개정 조례는 공포한 날 시행한다.",
            "이 조례 는 공포한 날 시행한다.",
            "이 조례(안)는 공포한 날 시행한다.",
            "제1조(목적) 이 조례는 운영을 정한다.\n부칙\n이 조례는 공포한 날 시행토록 한다.",
            "제1조(목적) 이 조례는 운영을 정한다.\n부칙\n공포한 날 시행한다.",
            "제1조(목적) 이 조례는 운영을 정한다.\n부칙\n2030년 1월 1일 시행한다.",
            "이 조례는 2030년 1월 1일 시행한다.\n이 조례는 공포한 날 시행토록 한다.",
            "이 조례는 2030년 1월 1일 시행한다.\n이 條例는 公布한 날부터 施行한다.",
            "이 조례는 2030년 1월 1일 시행한다.\n이 개정 조례는 공포한 날 시행한다.",
            "이 조례는 2030년 1월 1일 시행한다.\n이 조례는 공포한 날부터 효력이 발생한다.",
        ] {
            expect(
                &case("", text, "20240301", "제정", 7),
                "unresolved",
                None,
                text,
            );
        }
        // Controls: noun uses and sentences without an effective verb keep their dates.
        for (text, status, date) in [
            (
                "제1조(목적) 이 조례는 운영을 정한다.\n제2조 시장은 사업을 시행한다.",
                "confirmed",
                "2024-03-22",
            ),
            (
                "제1조(목적) 이 조례는 농업인회관의 시행에 관하여 필요한 사항을 규정함을 목적으로 한다.",
                "confirmed",
                "2024-03-22",
            ),
            ("제2조 시장은 사업을 시행한다.", "confirmed", "2024-03-22"),
        ] {
            expect(
                &case("", text, "20240301", "제정", 7),
                status,
                Some(date),
                text,
            );
        }
    }

    #[test]
    fn r22_a_parenthesised_proviso_and_an_unread_sentence_in_a_unit_are_unresolved() {
        // B2 (997403, 997404): a proviso in brackets right after the effective sentence.
        for text in [
            "이 조례는 2030년 1월 1일 시행한다(다만, 제3조는 공포한 날부터 시행한다).",
            "이 조례는 공포한 날부터 시행한다.(다만, 제3조는 2030년 1월 1일부터 시행한다.)",
            "이 조례는 공포한 날부터 시행한다.（다만, 제3조는 2030년 1월 1일부터 시행한다.）",
        ] {
            expect(
                &case("", text, "20240301", "제정", 7),
                "unresolved",
                None,
                text,
            );
        }
        // B3 (997409): the unread sentence of an addendum unit stops every older rule.
        let unit = |label: &str, text: &str| {
            format!(
                "<부칙단위><부칙공포일자>{label}</부칙공포일자><부칙내용><![CDATA[{text}]]></부칙내용></부칙단위>"
            )
        };
        let units = format!(
            "{}{}",
            unit("20240301", "부칙\n이 조례는 공포한 날부터 시행한다."),
            unit(
                "20200101",
                "부칙 <2020.1.1 조례 제3호>\n이 조례는 공포한 날부터 시행한다.\n이 조례는 의회 승인을 받은 날\n시행한다."
            )
        );
        let mut source = revision("조례", "제정", "20240301", 7, "");
        source.xml = format!("<LawService><공포번호>7</공포번호><부칙>{units}</부칙></LawService>")
            .into_bytes();
        expect(&source, "unresolved", None, "unit fallthrough");
        // Control: without the unread sentence the same units keep their older result.
        let units = format!(
            "{}{}",
            unit("20240301", "부칙\n이 조례는 공포한 날부터 시행한다."),
            unit(
                "20200101",
                "부칙 <2020.1.1 조례 제3호>\n이 조례는 공포한 날부터 시행한다."
            )
        );
        let mut control = revision("조례", "제정", "20240301", 7, "");
        control.xml =
            format!("<LawService><공포번호>7</공포번호><부칙>{units}</부칙></LawService>")
                .into_bytes();
        expect(&control, "unresolved", None, "unit control");
    }

    #[test]
    fn r22_the_guard_withholds_only_a_date_that_comes_from_another_section() {
        let reason_of = |source: &SourceRevision| {
            let found = resolve("ordinances", source).unwrap();
            (
                found.status.clone(),
                found.reason.clone(),
                found.date_evidence.get("date_withheld").cloned(),
            )
        };
        let no_form =
            "effective date is unresolved; the effective-date sentence names no approved form";
        // A section of an addendum unit holds an unread sentence and the older rules would confirm
        // the label date of another unit (997409): the date is withheld and the evidence says so.
        let unit = |label: &str, text: &str| {
            format!(
                "<부칙단위><부칙공포일자>{label}</부칙공포일자><부칙내용><![CDATA[{text}]]></부칙내용></부칙단위>"
            )
        };
        let units = format!(
            "{}{}",
            unit("20240301", "부칙\n이 조례는 공포한 날부터 시행한다."),
            unit(
                "20200101",
                "부칙 <2020.1.1 조례 제3호>\n이 조례는 공포한 날부터 시행한다.\n이 조례는 의회 승인을 받은 날\n시행한다."
            )
        );
        let mut withheld = revision("조례", "제정", "20240301", 7, "");
        withheld.xml =
            format!("<LawService><공포번호>7</공포번호><부칙>{units}</부칙></LawService>")
                .into_bytes();
        let (status, reason, note) = reason_of(&withheld);
        assert_eq!((status.as_str(), reason.as_str()), ("unresolved", no_form));
        assert_eq!(note, Some(json!("unread-sentence-in-target-section")));
        // A row the older rules leave unresolved keeps its own reason and evidence.
        let own = revision(
            "조례",
            "일부개정",
            "20100601",
            1,
            "부칙 (2010.6.1) 이 조례는 승인을 받은 날\n시행한다.\n",
        );
        let (status, reason, note) = reason_of(&own);
        assert_eq!(status, "unresolved");
        assert!(
            reason.contains("no approved source rule selects a date"),
            "{reason}"
        );
        assert_eq!(note, None);
        // An addendum inside the articles stays unresolved at once, with the reason of the section.
        let inside = case(
            "",
            "제1조(목적) 이 조례는 운영을 정한다.\n부칙\n이 조례는 의회 승인을 받은 날\n시행한다.",
            "20240301",
            "제정",
            7,
        );
        let (status, reason, note) = reason_of(&inside);
        assert_eq!((status.as_str(), reason.as_str()), ("unresolved", no_form));
        assert_eq!(note, None);
    }

    #[test]
    fn r22_no_estimate_or_default_period_meets_an_unread_sentence() {
        // The Guard needs only "confirmed": every route to "estimated" (arrival estimate of a
        // guideline or order) and to the default periods (21 days, 6 days) needs two things at
        // once: no addendum block with text, and no text in the body that names an effective date.
        // A sentence the detection found names an effective date, wherever it stands: in the body
        // it breaks the second condition, in an addendum it breaks the first. So an unread
        // sentence and an estimate cannot meet, and the estimate needs no Guard.
        let body_rule = |kind: &str, sentence: &str| {
            let mut rule = revision(kind, "제정", "19980615", 1, "");
            rule.xml = format!("<AdmRulService><조문내용>{sentence}</조문내용></AdmRulService>")
                .into_bytes();
            resolve("admrules", &rule).unwrap()
        };
        let unread = "이 예규는 의회의 승인을 받은 날\n시행한다.";
        // Control: the same rule without a sentence is estimated.
        let found = body_rule("등기예규", "시행규칙에 따른다.");
        assert_eq!(found.status, "estimated");
        // An unread sentence in the body: no estimate and no date.
        for kind in ["등기예규", "훈령"] {
            let found = body_rule(kind, unread);
            assert_eq!(found.status, "unresolved", "{kind}");
            assert_eq!(found.enforcement_date, None, "{kind}");
        }
        // The 6 days of a notice and the 21 days of an ordinance meet the same wall.
        let found = body_rule("고시", "이 고시는 의회의 승인을 받은 날\n시행한다.");
        assert_eq!(found.status, "unresolved");
        // An unread sentence in an addendum block: the addendum is usable text, so no default.
        for kind in ["등기예규", "훈령", "고시"] {
            let found =
                resolve("admrules", &revision(kind, "제정", "19980615", 1, unread)).unwrap();
            assert_ne!(found.status, "estimated", "{kind}");
            assert_eq!(found.enforcement_date, None, "{kind}");
        }
        let found = resolve(
            "ordinances",
            &revision("조례", "제정", "20240301", 7, unread),
        )
        .unwrap();
        assert_ne!(
            found.reason,
            "accepted publication plus 21 days; no holiday deferral"
        );
        assert_eq!(found.enforcement_date, None);
    }

    /// An addendum of an amendment text: the heading and its sentences stand in the reason text.
    fn amendment_text_case(reason: &str) -> SourceRevision {
        let mut source = revision("조례", "일부개정", "20240301", 7, "");
        source.xml = format!(
            "<LawService><공포번호>7</공포번호><조문><조><조내용><![CDATA[제1조(목적) 이 조례는 운영을 정한다.]]></조내용></조></조문><부칙></부칙><제개정이유><제개정이유내용><![CDATA[{reason}]]></제개정이유내용></제개정이유></LawService>"
        )
        .into_bytes();
        source
    }

    #[test]
    fn r23_the_first_sentence_of_a_section_is_read_wherever_the_section_comes_from() {
        // One test of an addendum heading (a line that starts with "부칙") serves the section split,
        // the search in the articles and the detection (Codex 9 blockers 1 and 2, fresh-verifier-19
        // C1). A first sentence without a subject next to a readable sentence leaves the section
        // unresolved: no other date is taken.
        let unresolved = |source: &SourceRevision, name: &str| {
            expect(source, "unresolved", None, name);
        };
        let first = "의회의 승인을 받은 날 시행한다.";
        let explicit = "이 조례는 2030년 1월 1일부터 시행한다.";
        // Codex 9 blocker 1, in the articles and in an addendum block.
        let text = format!("부칙\n{first}\n{explicit}");
        unresolved(&case("", &text, "20240301", "제정", 7), "codex9 1 body");
        unresolved(&case(&text, "", "20240301", "제정", 7), "codex9 1 block");
        // Codex 9 blocker 2: a quoted "부칙" inside a sentence is no heading.
        let quoted = "이 조례는 의회의 승인을 받은 날 시행한다.\n제2조 다른 조례의 \"부칙 제1조\"를 다음과 같이 고친다.\nXX 조례는 2030년 1월 1일부터 시행한다.";
        unresolved(
            &case(
                "",
                &quoted.replace("XX 조례는", "이 조례는"),
                "20240301",
                "제정",
                7,
            ),
            "codex9 2",
        );
        unresolved(
            &case(
                "",
                &quoted
                    .replace("XX 조례는", "이 조례는")
                    .replace("부칙 제1조", "시행조항"),
                "20240301",
                "제정",
                7,
            ),
            "codex9 2 control",
        );
        // fresh-verifier-19 C1 (999141, 999142, 999143, 999145, 999147, 999151, 999403).
        let days = "공포 후 30일이 경과한 날부터 시행한다.";
        unresolved(
            &case(
                &format!("부칙\n{days}\n{explicit}"),
                "",
                "20240301",
                "제정",
                7,
            ),
            "999141",
        );
        unresolved(
            &case(
                "",
                &format!("제1조(목적) 이 조례는 운영을 정한다.\n부칙\n{days}\n{explicit}"),
                "20240301",
                "제정",
                7,
            ),
            "999142",
        );
        unresolved(
            &case(
                &format!("부칙 <2024.3.1 조례 제7호>\n{first}\n이 조례는 공포한 날부터 시행한다."),
                "",
                "20240301",
                "일부개정",
                7,
            ),
            "999143",
        );
        unresolved(
            &case(
                "",
                &format!(
                    "제1조(목적) 이 조례는 운영을 정한다.\n부칙 <2024.3.1 조례 제7호>\n{first}\n이 조례는 공포한 날부터 시행한다."
                ),
                "20240301",
                "일부개정",
                7,
            ),
            "999145",
        );
        unresolved(
            &case(
                &format!("부칙\n제1조(시행일) {days}\n제2조(유효기간) {explicit}"),
                "",
                "20240301",
                "제정",
                7,
            ),
            "999147",
        );
        unresolved(
            &case(
                "",
                "제1조(목적) 이 조례는 운영을 정한다.\n부칙\n공포한 날 시행하고, 이 조례가 정한 위원회는 6개월 안에 구성한다.",
                "20240301",
                "제정",
                7,
            ),
            "999151",
        );
        let reason = format!(
            "□ 개정이유\n운영상 미비점을 보완함.\n부칙 <2024.3.1 조례 제7호>\n{first}\n이 조례는 공포한 날부터 시행한다.\n□ 주요내용\n가. 정비함."
        );
        unresolved(&amendment_text_case(&reason), "999403");
        // A heading after a no-break space or an ideographic space is a heading as well (999401).
        for space in ['\u{A0}', '\u{3000}', ' '] {
            unresolved(
                &case(
                    "",
                    &format!(
                        "제1조(목적) 이 조례는 운영을 정한다.\n{space}부칙\n공포한 날 시행한다."
                    ),
                    "20240301",
                    "제정",
                    7,
                ),
                "999401",
            );
        }
        // Controls: a section whose first sentence has a subject keeps its date; a quoted heading
        // word in a sentence without any verb changes nothing.
        expect(
            &case(
                "",
                "부칙\n이 조례는 공포한 날부터 시행한다.\n이 조례 제2조는 2030년 1월 1일부터 적용한다.",
                "20240301",
                "제정",
                7,
            ),
            "confirmed",
            Some("2024-03-01"),
            "control",
        );
        expect(
            &case(
                "",
                "제1조(목적) 이 조례는 다른 조례의 \"부칙 제1조\"를 인용한다.",
                "20240301",
                "제정",
                7,
            ),
            "confirmed",
            Some("2024-03-22"),
            "quotation control",
        );
        // Accepted limit (a phrase without a subject outside a section): without an addendum
        // heading the first sentence needs its own subject to be detected.
        expect(
            &case(
                "",
                "공포한 날 시행한다.\n이 조례는 2030년 1월 1일부터 시행한다.",
                "20240301",
                "제정",
                7,
            ),
            "confirmed",
            Some("2030-01-01"),
            "no heading, no subject",
        );
        // A section body that starts with a sentence without a subject is not a plain section.
        assert!(
            plain_promulgation_clause(
                "의회의 승인을 받은 날 시행한다.\n이 조례는 공포한 날부터 시행한다.",
                "조례"
            )
            .is_none()
        );
        assert!(plain_promulgation_clause("이 조례는 공포한 날부터 시행한다.", "조례").is_some());
        // The attributed section has no sentence of its own, so the sentences of the whole addendum
        // part decide; the first sentence of the other section counts there as well.
        unresolved(
            &case(
                "",
                "제1조(목적) 이 조례는 운영을 정한다.\n부칙\n의회의 승인을 받은 날 시행한다.\n이 조례는 공포한 날부터 시행한다.\n부칙 <2024.3.1 조례 제7호>\n",
                "20240301",
                "일부개정",
                7,
            ),
            "attributed part",
        );
        // The heading test is one function: the section split, the search and the detection agree.
        for line in ["부칙", " 부칙 <2024.3.1>", "\u{A0}부칙", "\u{3000}부 칙"] {
            assert!(heading_rest(line).is_some(), "{line:?}");
        }
        for line in [
            "제2조 다른 조례의 \"부칙 제1조\"를",
            "부칙을 고친다",
            "부칙제1조",
        ] {
            assert!(heading_rest(line).is_none(), "{line:?}");
        }
    }

    #[test]
    fn r23_the_subject_kinds_the_prefix_and_the_spaces_follow_the_r21_and_the_user_lists() {
        let unresolved_body = |text: &str| {
            expect(
                &case("", text, "20240301", "제정", 7),
                "unresolved",
                None,
                text,
            );
        };
        let adm = |kind: &str, sentence: &str| {
            let mut rule = revision(kind, "제정", "20200101", 1, "");
            rule.xml = format!("<AdmRulService><조문내용>{sentence}</조문내용></AdmRulService>")
                .into_bytes();
            resolve("admrules", &rule).unwrap()
        };
        // R1: the six-character prefix before 훈령, 요령 and 訓令 (r21 result: unresolved), and
        // "이.조례는" (r21 result: unresolved).
        for sentence in [
            "이 대통령경호처훈령은 발령한 날 시행한다.",
            "이 새만금개발청훈령은 발령한 날 시행한다.",
            "이 새만금개발청요령은 발령한 날 시행한다.",
            "이 국토교통부훈령은 발령한 날 시행한다.",
        ] {
            let found = adm("훈령", sentence);
            assert_eq!(found.status, "unresolved", "{sentence}");
            assert_eq!(found.enforcement_date, None, "{sentence}");
        }
        unresolved_body("이 大統領警護處訓令은 公布한 날 施行한다.");
        unresolved_body("이.조례는 공포한 날 시행한다.");
        // The prefix may not hold a full stop when inline spaces split it: the sentence of
        // "이 조례는" starts at "이", not at the "본" of "본다." before it.
        expect(
            &case(
                "",
                "의회가 위촉한 것으로 본다.이 조례는 공포한 날부터 시행한다.",
                "20240301",
                "제정",
                7,
            ),
            "confirmed",
            Some("2024-03-01"),
            "full stop control",
        );
        // 내규 (O1).
        let found = adm("훈령", "이 내규는 발령한 날 시행한다.");
        assert_eq!(found.status, "unresolved");
        // V1: spaces of every kind in the subject and before the particle, up to three words.
        for subject in [
            "이 조례\u{A0}는",
            "이 조례\u{2002}는",
            "이 조례\u{2003}는",
            "이 개정\u{2003}조례는",
            "이 조례\u{3000}는",
            "이 조례  는",
            "이 조례\t는",
            "이 개정\u{A0}조례는",
            "이 일부 개정 조례는",
            "이 조례  시행규칙은",
            "이 조례   시행규칙은",
            "이 조례（안）는",
            "이 조례(안)는",
            "이 조례 는",
        ] {
            unresolved_body(&format!("{subject} 공포한 날 시행한다."));
            unresolved_body(&format!(
                "{subject} 의회의 승인을 받은 날 시행한다.\n이 조례는 2030년 1월 1일부터 시행한다."
            ));
        }
        for sentence in [
            "이 고시\u{A0}는 고시한 날 시행한다.",
            "이 예규（안）는 발령한 날 시행한다.",
        ] {
            let found = adm("고시", sentence);
            assert_eq!(found.status, "unresolved", "{sentence}");
        }
        // Controls (accepted limit user-r24-subject-boundary-limit-20261111): a line feed splits a
        // subject; the detection does not follow it.
        expect(
            &case(
                "",
                "이 조례\n는 의회의 승인을 받은 날 시행한다.\n이 조례는 2030년 1월 1일부터 시행한다.",
                "20240301",
                "제정",
                7,
            ),
            "confirmed",
            Some("2030-01-01"),
            "line feed control",
        );
    }

    #[test]
    fn r23_the_polite_endings_and_the_causative_forms_are_effective_verbs() {
        let law = |sentence: &str| {
            let mut source = revision("조례", "제정", "20240301", 7, "");
            source.xml = format!(
                "<LawService><공포번호>7</공포번호><조문><조><조내용><![CDATA[{sentence}]]></조내용></조></조문><부칙></부칙></LawService>"
            )
            .into_bytes();
            source
        };
        for verb in [
            "시행합니다",
            "시행됩니다",
            "시행하였습니다",
            "시행되었습니다",
            "시행하겠습니다",
            "시행시킴",
            "시행시켰다",
            "시행시킵니다",
            "시행시키고",
            "시행시켜",
        ] {
            let sentence = format!("이 조례는 공포한 날 {verb}.");
            expect(&law(&sentence), "unresolved", None, &sentence);
            let with_explicit = format!(
                "이 조례는 2030년 1월 1일 시행한다.\n이 조례는 의회의 승인을 받은 날 {verb}."
            );
            expect(&law(&with_explicit), "unresolved", None, &with_explicit);
        }
        let adm = |kind: &str, sentence: &str| {
            let mut rule = revision(kind, "제정", "20200101", 1, "");
            rule.xml = format!("<AdmRulService><조문내용>{sentence}</조문내용></AdmRulService>")
                .into_bytes();
            resolve("admrules", &rule).unwrap()
        };
        for (kind, sentence) in [
            ("훈령", "이 훈령은 발령한 날 시행합니다."),
            ("고시", "이 고시는 고시한 날 시행시킴."),
            ("훈령", "이 훈령은 발령한 날 시행됩니다."),
        ] {
            let found = adm(kind, sentence);
            assert_eq!(found.status, "unresolved", "{sentence}");
            assert_eq!(found.enforcement_date, None, "{sentence}");
        }
        // Controls: a noun use keeps its default.
        let found = adm("훈령", "이 훈령은 업무 처리 기준을 정한다.");
        assert_eq!(found.status, "estimated");
    }

    #[test]
    fn r23_a_proviso_is_found_behind_every_opening_bracket() {
        // "(다만", "（다만", "[다만", "［다만" and the same with inline spaces inside the bracket.
        for opener in ["(", "（", "[", "［"] {
            for gap in ["", " ", "\u{A0}", "\u{3000}", "\t"] {
                let text = format!(
                    "이 조례는 2030년 1월 1일부터 시행한다.{opener}{gap}다만, 제3조는 공포한 날부터 시행한다."
                );
                expect(
                    &case("", &text, "20240301", "제정", 7),
                    "unresolved",
                    None,
                    &text,
                );
            }
        }
        for word in ["단,", "단，"] {
            let text = format!(
                "이 조례는 2030년 1월 1일부터 시행한다.({word} 제3조는 공포한 날부터 시행한다.)"
            );
            expect(
                &case("", &text, "20240301", "제정", 7),
                "unresolved",
                None,
                &text,
            );
        }
        // Controls: a note that is no proviso, and a bracket without the word.
        for text in [
            "이 조례는 2030년 1월 1일부터 시행한다.(시행일 변경)",
            "이 조례는 2030년 1월 1일부터 시행한다.[별표 1]",
        ] {
            expect(
                &case("", text, "20240301", "제정", 7),
                "confirmed",
                Some("2030-01-01"),
                text,
            );
        }
    }

    /// The 38 characters (and forms) that follow "부칙" at a line start in the probe of
    /// fresh-verifier-21, with the flag of a clean heading (a space, "[" or a label bracket follows).
    fn heading_follow_forms() -> Vec<(&'static str, bool)> {
        vec![
            ("", true),
            (" ", true),
            ("\u{A0}", true),
            ("\u{3000}", true),
            ("\t", true),
            ("[", true),
            ("<", true),
            ("\u{FF1C}", true),
            ("\u{3008}", true),
            ("\u{2329}", true),
            ("(", true),
            ("\u{FF08}", true),
            ("\u{FF3B}2024.3.1\u{FF3D}", false),
            ("\u{3010}2024.3.1\u{3011}", false),
            ("\u{3014}2024.3.1\u{3015}", false),
            ("\u{300A}2024.3.1\u{300B}", false),
            ("\u{300C}2024.3.1\u{300D}", false),
            ("{2024.3.1}", false),
            ("&lt;2024.3.1&gt;", false),
            ("&lt;", false),
            ("제1조(시행일)", false),
            ("제1조", false),
            (":", false),
            (".", false),
            (",", false),
            ("-", false),
            ("·", false),
            ("ㆍ", false),
            ("\u{30FB}", false),
            ("1", false),
            ("2024.3.1", false),
            ("\u{200B}", false),
            ("\u{FEFF}", false),
            ("\u{2003}", true),
            ("\u{2002}", true),
            ("을 다음과 같이 한다", false),
            ("에 따라", false),
            ("중", false),
        ]
    }

    #[test]
    fn r24_one_heading_judgment_serves_every_call_site() {
        // heading_line takes a line as a heading whatever follows "부칙"; only a clean heading
        // starts a section; the extraction of an amendment text takes the clean ones and "개정".
        for (follow, clean) in heading_follow_forms() {
            let line = format!("부칙{follow}\n공포한 날 시행한다.");
            let heading = heading_line(&line).unwrap_or_else(|| panic!("{follow:?}"));
            // The entity forms are decoded before any judgment, so the clean flag is judged on the
            // decoded text.
            let decoded = decode_markup(&line);
            let decoded_clean = heading_line(&decoded).unwrap().clean;
            assert_eq!(
                heading.clean || decoded_clean,
                clean || decoded_clean,
                "{follow:?}"
            );
            let sections = split_sections(&decoded, None);
            assert_eq!(sections.len(), usize::from(decoded_clean), "{follow:?}");
        }
        // A heading of two lines ("부" and "칙" apart) is a heading for the detection only.
        assert!(heading_line("부\n칙\n공포한 날 시행한다.").is_some());
        assert!(heading_line("부 칙").is_some());
        assert!(heading_line("부\n").is_none());
        assert!(split_sections("부\n칙\n공포한 날 시행한다.", None).is_empty());
        // Inline and other white space before the heading, a quotation inside a sentence is none.
        for line in [
            "\u{A0}부칙",
            "\u{2002}부칙",
            "\u{2003}부칙",
            "\r부칙",
            "\t 부 칙",
        ] {
            assert!(heading_line(line).is_some(), "{line:?}");
        }
        for line in ["제2조 다른 조례의 \"부칙 제1조\"를", "○ 부칙", "가부칙"] {
            assert!(heading_line(line).is_none(), "{line:?}");
        }
        // The extraction of an amendment text: the five forms of r23, the heading with "개정" once
        // white space is taken out, and the heading with its first sentence on the same line.
        let sentence = "이 조례는 공포한 날부터 시행한다.";
        for heading in [
            "부칙",
            "부칙(2024.3.1)",
            "부칙<2024.3.1>",
            "부칙[2024.3.1]",
            "부칙\u{FF1C}2024.3.1\u{FF1E}",
            "부칙개정<2024.3.1>",
            "부칙개 정<2024.3.1>",
            "부칙 의회의 승인을 받은 날 시행합니다.",
            "부칙 제1조(시행일) 의회의 승인을 받은 날 시행한다.",
        ] {
            let text = format!("□ 개정이유\n설명\n{heading}\n{sentence}\n□ 주요내용\n가. 정비함.");
            assert!(
                amendment_text_addenda(&text, &text, None).is_some(),
                "{heading:?}"
            );
        }
        for heading in [
            "○ 부칙 의회의 승인을 받은 날 시행한다.",
            "부칙을 신설한다.",
            "부칙을 신설하며 공포한 날부터 시행한다.",
        ] {
            let text = format!("□ 개정이유\n설명\n{heading}\n□ 주요내용\n가. 정비함.");
            assert!(
                amendment_text_addenda(&text, &text, None).is_none(),
                "{heading:?}"
            );
        }
    }

    #[test]
    fn r24_every_character_after_a_heading_keeps_the_first_sentence() {
        // r22 read the first sentence after every one of these 38 headings (r23 missed 24 of them
        // and took 21 days). The body path and the addendum block path both stay unresolved.
        for (follow, _) in heading_follow_forms() {
            let body =
                format!("제1조(목적) 이 조례는 운영을 정한다.\n부칙{follow}\n공포한 날 시행한다.");
            expect(
                &case("", &body, "20240301", "제정", 7),
                "unresolved",
                None,
                &body,
            );
            let block = format!("부칙{follow}\n공포한 날 시행한다.");
            expect(
                &case(&block, "", "20240301", "제정", 7),
                "unresolved",
                None,
                &block,
            );
        }
        // Each of the five heading forms of the amendment text and the same-line forms.
        for reason in [
            "부칙 의회의 승인을 받은 날 시행합니다.",
            "부칙 제1조(시행일) 의회의 승인을 받은 날 시행한다.",
            "부칙 이 조례는 의회의 승인을 받은 날 시행한다.",
            "부칙\n의회의 승인을 받은 날 시행합니다.",
            "부칙\u{FF08}2024.3.1\u{FF09}\n이 조례는 의회의 승인을 받은 날 시행한다.",
        ] {
            let text = format!("□ 개정이유\n설명\n{reason}\n□ 주요내용\n가. 정비함.");
            unresolved_case(&amendment_text_case(&text), reason);
        }
        // Controls: a "부칙" in the middle of a sentence is no heading (21 days); a line that starts
        // with another mark is no heading; the amendment text keeps its own default.
        for body in [
            "제1조(목적) 이 조례는 부칙에 관한 사항을 정한다.",
            "제1조(목적) 이 조례는 운영을 정한다.\n제2조 시장은 사업을 시행한다.",
            "제1조(목적) 이 조례는 운영을 정한다.\n○ 부칙 공포한 날 시행한다.",
        ] {
            expect(
                &case("", body, "20240301", "제정", 7),
                "confirmed",
                Some("2024-03-22"),
                body,
            );
        }
    }

    /// The source is unresolved without a date.
    fn unresolved_case(source: &SourceRevision, name: &str) {
        expect(source, "unresolved", None, name);
    }

    #[test]
    fn r24_entity_markup_is_decoded_once() {
        // 991005, 991006, 991007: the heading "부칙&lt;…&gt;" is a heading after one decoding.
        let heading = |marker: &str, text: &str| {
            format!("제1조(목적) 이 조례는 운영을 정한다.\n부칙{marker}\n{text}")
        };
        unresolved_case(
            &case(
                "",
                &heading("&lt;2024.3.1&gt;", "공포한 날 시행한다."),
                "20240301",
                "제정",
                7,
            ),
            "991005",
        );
        unresolved_case(
            &case(
                "",
                &heading(" &lt;2024.3.1&gt;", "공포한 날 시행한다."),
                "20240301",
                "제정",
                7,
            ),
            "991007",
        );
        // 991006: the first (entity) heading is the section of this revision; its sentence is
        // the day of promulgation and the later section must not replace it.
        let attributed = "제1조(목적) 이 조례는 운영을 정한다.\n부칙&lt;2024.3.1 조례 제7호&gt;\n이 조례는 공포한 날부터 시행한다.\n부칙 <2020.1.1 조례 제3호>\n이 조례는 2030년 1월 1일부터 시행한다.";
        expect(
            &case("", attributed, "20240301", "일부개정", 7),
            "confirmed",
            Some("2024-03-01"),
            "991006",
        );
        // The amendment text is decoded as well: a heading "부칙&lt;…&gt;" there is a heading, and
        // its first sentence has no subject.
        unresolved_case(
            &amendment_text_case(
                "□ 개정이유\n설명\n부칙&lt;2024.3.1&gt;\n공포한 날 시행한다.\n□ 주요내용\n가. 정비함.",
            ),
            "amendment text entity",
        );
        // The decoding happens once: "&amp;lt;" stays the text "&lt;".
        assert_eq!(decode_markup("&amp;lt;"), "&lt;");
        // The section split reads the text as it is: it does not decode again.
        assert!(split_sections("부칙&lt;2024.3.1&gt;\n공포한 날 시행한다.", None).is_empty());
        assert_eq!(
            split_sections("부칙<2024.3.1>\n공포한 날 시행한다.", None).len(),
            1
        );
    }

    #[test]
    fn r24_the_first_sentence_leaves_a_verb_outside_the_subject_sentences_unread() {
        let explicit = "이 조례는 2030년 1월 1일부터 시행한다.";
        let in_block = |text: &str| case(text, "", "20240301", "제정", 7);
        let in_body = |text: &str| {
            case(
                "",
                &format!("제1조(목적) 이 조례는 운영을 정한다.\n{text}"),
                "20240301",
                "제정",
                7,
            )
        };
        let plain = Some("2024-03-01");
        // The verb stands before the subject (991011-991014, 991103, 992009, 996001-996011).
        for text in [
            format!("부칙\n공포한 날 시행한다. {explicit}"),
            format!("부칙\n의회의 승인을 받은 날 시행하고, {explicit}"),
            format!("부칙\n의회의 승인을 받은 날 시행한다\n{explicit}"),
            "부칙 <2024.3.1 조례 제7호>\n제1조 승인을 받은 날 시행하며 이 조례는 공포한 날부터 시행한다.".to_owned(),
            "부칙\n(시행하는 경우) 이 조례는 공포한 날부터 시행한다.".to_owned(),
            "부칙\n공포한 날 시행하고, 이 조례가 정한 위원회는 구성한다.".to_owned(),
            // Codex 11 blockers: a bracket label that r22 skipped, and a bracketed subject sentence
            // without a verb of its own (the subject is not a detected subject).
            "부칙[2024.3.1].\n공포한 날 시행한다.".to_owned(),
            "부칙\n[이 조례는 운영을 정한다.] 공포한 날 시행한다.".to_owned(),
            format!("부칙\n[이 조례는 운영을 정한다.]\n승인을 받은 날 시행하고, {explicit}"),
        ] {
            unresolved_case(&in_block(&text), &text);
            unresolved_case(&in_body(&text), &text);
        }
        // A verb of a later sentence is no part of the first sentence (the left-over pieces are cut
        // at the end of the first sentence).
        expect(
            &in_block(
                "부칙\n공포한 날 정한다. 시장은 사업을 시행한다. 이 조례는 공포한 날부터 시행한다.",
            ),
            "confirmed",
            plain,
            "later sentence",
        );
        // A detected subject sentence inside brackets ends where the bracket state of its own
        // start says; the part after the bracket is left over and holds a verb.
        unresolved_case(
            &in_block("부칙\n[이 조례는 공포한 날부터 시행한다.] 승인을 받은 날 시행한다."),
            "bracketed plain sentence, then a verb",
        );
        // A bracket that start A and start B do not skip: the subject inside it is no detected
        // subject (its sentence ends at its own full stop), the verb stands after it.
        for text in [
            "부칙\n공포한 날(이 조례는 운영을 정한다.) 시행한다.",
            "부칙\n승인을 받은 날「이 조례는 운영을 정한다.」 시행한다.",
        ] {
            unresolved_case(&in_body(text), text);
            unresolved_case(&in_block(text), text);
        }
        // The same after an article mark, so that start B does not skip the bracket group.
        unresolved_case(
            &in_block("부칙\n제1조 [이 조례는 공포한 날부터 시행한다.] 승인을 받은 날 시행한다."),
            "article mark, bracketed plain sentence, then a verb",
        );
        // Controls: the subject starts the sentence, an article mark and a noun before the subject
        // keep the day of promulgation; a bracket label is skipped.
        for text in [
            "부칙\n이 조례는 공포한 날부터 시행한다.",
            "부칙\n제1조(시행일) 이 조례는 공포한 날부터 시행한다.",
            "부칙\n시행일은 이 조례는 공포한 날부터 시행한다.",
            "부칙[2024.3.1] 이 조례는 공포한 날부터 시행한다.",
        ] {
            expect(&in_block(text), "confirmed", plain, text);
        }
        // A bracketed subject sentence and a plain sentence stay one clause of the reader: unresolved
        // in r22 and r23 as well (the reader, not the detection).
        unresolved_case(
            &in_block("부칙\n[이 조례는 운영을 정한다.] 이 조례는 공포한 날부터 시행한다."),
            "bracketed subject and plain",
        );
        // The section of an addendum block, with its heading cut off, keeps both starts.
        assert!(!detected_sentence_starts("[2024.3.1].\n공포한 날 시행한다.", true).is_empty());
        // The sentence that starts with its own subject is the only start: no heading start joins it.
        assert_eq!(
            detected_sentence_starts("이 조례는 공포한 날부터 시행한다.", true),
            vec![0]
        );
        // Accepted limit (user-broad-detection-20261110): without a heading the first sentence
        // needs a subject of its own.
        expect(
            &case(
                "",
                "공포한 날 시행한다.\n이 조례는 2030년 1월 1일부터 시행한다.",
                "20240301",
                "제정",
                7,
            ),
            "confirmed",
            Some("2030-01-01"),
            "no heading, no subject",
        );
    }

    #[test]
    fn r24_a_proviso_bracket_takes_white_space_of_every_kind() {
        let after = |tail: &str| {
            format!("이 조례는 2030년 1월 1일 시행한다.{tail} 제3조는 공포한 날부터 시행한다.")
        };
        for opener in ["(", "（", "[", "［"] {
            for gap in [
                "", " ", "\u{A0}", "\u{3000}", "\u{2002}", "\u{2003}", "\t", "\n", "\r\n", " \n",
            ] {
                for word in ["다만,", "단,", "단，"] {
                    let text = after(&format!("{opener}{gap}{word}"));
                    expect(
                        &case("", &text, "20240301", "제정", 7),
                        "unresolved",
                        None,
                        &text,
                    );
                }
            }
            for gap in [" ", "  ", "\u{2003}", "\n"] {
                let text = after(&format!("{opener}단{gap}"));
                expect(
                    &case("", &text, "20240301", "제정", 7),
                    "unresolved",
                    None,
                    &text,
                );
            }
        }
        // Controls: a word that only starts with 단 is no proviso.
        for tail in ["(단체 제외)", "(단지 참고)", "［단체］"] {
            let text = format!("이 조례는 2030년 1월 1일부터 시행한다.{tail}");
            expect(
                &case("", &text, "20240301", "제정", 7),
                "confirmed",
                Some("2030-01-01"),
                &text,
            );
        }
        assert!(starts_proviso("(\n다만, 제3조는 ..."));
        assert!(!starts_proviso("(단체"));
    }

    #[test]
    fn r24_the_accepted_limits_are_pinned_as_controls() {
        // Accepted limits (user-r24-subject-boundary-limit-20261111): a subject of four or more
        // words, a prefix of seven or more characters before the kind, a subject split by a line
        // feed. The control only records what the detection does; it is no requirement.
        expect(
            &case(
                "",
                "이 서울특별시 일부 개정 조례는 공포한 날 시행한다.",
                "20240301",
                "제정",
                7,
            ),
            "confirmed",
            Some("2024-03-22"),
            "four words",
        );
        expect(
            &case(
                "",
                "이 국토교통부장관훈령은 공포한 날 시행한다.",
                "20240301",
                "제정",
                7,
            ),
            "confirmed",
            Some("2024-03-22"),
            "seven characters",
        );
        expect(
            &case(
                "",
                "이 조례\n는 의회의 승인을 받은 날 시행한다.\n이 조례는 2030년 1월 1일부터 시행한다.",
                "20200101",
                "제정",
                7,
            ),
            "confirmed",
            Some("2030-01-01"),
            "line feed",
        );
        // Accepted limits of the earlier decisions: a noun outside the list, a phrase without a
        // subject, a boundary form (a full stop after a closing bracket or inside quotes).
        for body in [
            "이 조례는 2030년 1월 1일 시행에 들어간다.",
            "이 조례의 시행일은 2030년 1월 1일로 한다.",
            "이 조례는 「시험법」(법률 제7호로 개정된 것을 말함). 이 시행되는 날 시행한다.",
            "이 조례는 \"공포한 날\". 시행한다.",
        ] {
            expect(
                &case("", body, "20240301", "제정", 7),
                "confirmed",
                Some("2024-03-22"),
                body,
            );
        }
    }

    /// The administrative rule of a kind, with the text as its only article.
    fn adm_rule_case(kind: &str, issue: &str, articles: &[&str]) -> Occurrence {
        let mut rule = revision(kind, "제정", issue, 1, "");
        let items: String = articles
            .iter()
            .map(|text| format!("<조문내용>{text}</조문내용>"))
            .collect();
        rule.xml = format!("<AdmRulService>{items}</AdmRulService>").into_bytes();
        resolve("admrules", &rule).unwrap()
    }

    #[test]
    fn r25_the_decoded_and_the_written_text_are_both_read() {
        // The decoding turns "&gt;." into a closing bracket and a full stop, which ends a
        // sentence; the text as the source wrote it does not end it. r22 and r23 read the written
        // text for the effective-date judgment, r24 only the decoded one. The detection reads both
        // (user-r25-dual-text-detection-20261112): the sentence after "&gt;." is not lost.
        let purpose = "제1조(목적) 이 조례는 운영을 정한다.";
        let cond = "의회의 승인을 받은 날 시행한다.";
        // 996401, 996402 (the articles), 996405 (an amendment), 996404 (an addendum block).
        for text in [
            format!("{purpose}\n부칙\n① &lt;생략&gt;. {cond}"),
            format!("{purpose}\n부칙\n제1조 &lt;시행일&gt;. {cond}"),
            format!("{purpose}\n부칙 &lt;생략&gt;. 공포한 날 시행한다."),
        ] {
            unresolved_case(&case("", &text, "20240301", "제정", 7), &text);
            unresolved_case(&case("", &text, "20240301", "일부개정", 7), &text);
        }
        for block in [
            format!("부칙\n① &lt;생략&gt;. {cond}"),
            format!("부칙\n① &lt;생략&gt;. {cond}\n제2조 이 조례는 2030년 1월 1일부터 시행한다."),
        ] {
            unresolved_case(&case(&block, "본문", "20240301", "제정", 7), &block);
        }
        // 996403: the same sentence in the amendment text.
        unresolved_case(
            &amendment_text_case(&format!(
                "□ 제정이유\n운영 기준을 정함.\n부칙\n① &lt;생략&gt;. {cond}\n□ 주요내용\n가. 정함."
            )),
            "amendment text",
        );
        // 996971, 996972, 999751: an administrative rule keeps no 6 days and no estimate.
        for (kind, issue, article) in [
            (
                "고시",
                "20200101",
                "부칙\n① &lt;생략&gt;. 고시한 날 시행한다.\n",
            ),
            (
                "훈령",
                "20200101",
                "부칙\n① &lt;생략&gt;. 발령한 날 시행한다.\n",
            ),
            (
                "고시",
                "20200101",
                "부칙\n① &lt;생략&gt;. 고시한 날 시행한다.\n이 고시는 2030년 1월 1일부터 시행한다.\n",
            ),
        ] {
            let found = adm_rule_case(
                kind,
                issue,
                &["제1조(목적) 이 규정은 업무 기준을 정한다.\n", article],
            );
            assert_eq!(found.status, "unresolved", "{kind} {article}");
            assert_eq!(found.enforcement_date, None, "{kind} {article}");
        }
        // Controls: entity markup that is no sentence end keeps what the sentence says.
        for (text, status, date) in [
            (
                "이 조례는 공포한 날부터 시행한다.(&lt;개정 2024.3.1.&gt;)",
                "confirmed",
                "2024-03-01",
            ),
            (
                "이 조례는 공포한 날부터 시행한다. &lt;개정 2020.1.1.&gt;",
                "confirmed",
                "2024-03-01",
            ),
            (
                "이 조례는 공포한 날부터 시행한다. (A&amp;B 사업은 제외한다.)",
                "confirmed",
                "2024-03-01",
            ),
            (
                "제1조(목적) 이 조례는 &lt;운영&gt;을 정한다.",
                "confirmed",
                "2024-03-22",
            ),
            // The accepted limit of the boundary forms: a bracket that is written as a bracket.
            (
                "제1조(목적) 이 조례는 운영을 정한다.\n부칙\n① <생략>. 의회의 승인을 받은 날 시행한다.",
                "confirmed",
                "2024-03-22",
            ),
        ] {
            expect(
                &case("", text, "20240301", "제정", 7),
                status,
                Some(date),
                text,
            );
        }
        // The first section of 991006 keeps the day of promulgation over a later section.
        expect(
            &case(
                "",
                "제1조(목적) 이 조례는 운영을 정한다.\n부칙&lt;2024.3.1 조례 제7호&gt;\n이 조례는 공포한 날부터 시행한다.\n부칙 <2020.1.1 조례 제3호>\n이 조례는 2030년 1월 1일부터 시행한다.",
                "20240301",
                "일부개정",
                7,
            ),
            "confirmed",
            Some("2024-03-01"),
            "991006",
        );
    }

    #[test]
    fn r25_an_amendment_text_heading_of_any_kind_stops_the_default_period() {
        // 996101, 996301 and the 19 heading forms of the probe: the extraction takes only clean
        // headings, but the detection finds the first sentence after any heading, so no default
        // period and no date of another sentence is taken.
        let forms = [
            "\u{FF3B}2024.3.1\u{FF3D}",
            "\u{3010}2024.3.1\u{3011}",
            "\u{3014}2024.3.1\u{3015}",
            "\u{300A}2024.3.1\u{300B}",
            "\u{300C}2024.3.1\u{300D}",
            "{2024.3.1}",
            "제1조(시행일)",
            "제1조",
            ":",
            ".",
            ",",
            "-",
            "·",
            "ㆍ",
            "\u{30FB}",
            "1",
            "2024.3.1",
            "\u{200B}",
            "\u{FEFF}",
        ];
        for form in forms {
            for sentence in [
                "이 조례는 의회의 승인을 받은 날 시행한다.",
                " 의회의 승인을 받은 날 시행합니다.",
                "\n의회의 승인을 받은 날 시행한다.",
            ] {
                let text = format!(
                    "□ 제정이유\n운영 기준을 정함.\n부칙{form}{sentence}\n□ 주요내용\n가. 정함."
                );
                unresolved_case(&amendment_text_case(&text), &text);
            }
        }
        // Codex 13 blocker 3: a heading of two lines, and an unclean heading in a later part of the
        // text after a first unclean heading whose own sentence holds no verb.
        for text in [
            "부\n칙\n의회의 승인을 받은 날 시행합니다.",
            "□ 제정이유\n정함.\n부\n칙\n의회의 승인을 받은 날 시행한다.\n□ 주요내용\n가. 정함.",
            "□ 제정이유\n정함.\n부칙:\n경과조치를 둔다.\n□ 주요내용\n가. 정함.\n□ 부칙\n부칙:\n의회의 승인을 받은 날 시행합니다.",
        ] {
            unresolved_case(&amendment_text_case(text), text);
        }
        unresolved_case(
            &amendment_text_case("부칙\u{FF3B}2024.3.1\u{FF3D} 의회의 승인을 받은 날 시행합니다."),
            "996301",
        );
        // Controls: a clean heading reads as before; an amendment text without a heading, a prose
        // heading line without a verb, and an item mark before the word keep the default period.
        for (reason, status, date) in [
            (
                "□ 제정이유\n정함.\n부칙\n이 조례는 공포한 날부터 시행한다.\n□ 주요내용\n가. 정함.",
                "unresolved",
                None,
            ),
            (
                "□ 개정이유\n운영을 보완함. 이 조례는 공포한 날부터 시행함.",
                "confirmed",
                Some("2024-03-22"),
            ),
            (
                "□ 제정이유\n정함.\n부칙을 신설한다. 다른 사항은 따로 정한다.\n□ 주요내용\n가. 정함.",
                "confirmed",
                Some("2024-03-22"),
            ),
            // A list mark before the word is a heading of the gate (r25c v3, adversary F3), not of r24.
            (
                "□ 제정이유\n정함.\n○ 부칙 의회의 승인을 받은 날 시행한다.\n□ 주요내용\n가. 정함.",
                "unresolved",
                None,
            ),
        ] {
            expect(&amendment_text_case(reason), status, date, reason);
        }
    }

    #[test]
    fn r25_a_bracket_group_after_the_kind_is_part_of_the_subject() {
        // 999901, 999902, 999906, 999907, 999951 and their kin: an inline space before the bracket,
        // a note of any length, every bracket of OPENERS, a group that repeats.
        for text in [
            "이 조례 (안)는 공포한 날 시행한다.",
            "이 조례(제123호)는 공포한 날 시행한다.",
            "이 조례 （안） 는 공포한 날 시행한다.",
            "이 규칙(2024. 3. 1. 개정)은 공포한 날 시행한다.",
            "이 조례(안)(제2호)는 공포한 날 시행한다.",
            "이 조례 [안]은 공포한 날 시행한다.",
            "이 조례「안」은 공포한 날 시행한다.",
            "이 조례（개정 2024.3.1.）는 공포한 날 시행한다.",
            "이 조례\u{A0}〈안〉\u{3000}는 공포한 날 시행한다.",
            "이 조례(이 조례는 운영을 정한다. 제2항 포함)는 공포한 날 시행한다.",
            // Codex 13 blocker 1: a bracket of another kind inside the group.
            "이 조례([안])는 공포한 날 시행한다.",
            "이 조례「(안)」는 공포한 날 시행한다.",
        ] {
            unresolved_case(&case("", text, "20240301", "제정", 7), text);
            let with_date = format!("{text}\n이 조례는 2030년 1월 1일부터 시행한다.");
            unresolved_case(&case("", &with_date, "20240301", "제정", 7), &with_date);
        }
        let found = adm_rule_case(
            "고시",
            "20200101",
            &["이 고시 (안)는 고시한 날 시행한다.\n"],
        );
        assert_eq!(
            (found.status.as_str(), found.enforcement_date.as_deref()),
            ("unresolved", None)
        );
        // Controls: a particle that is no subject particle, a sentence without a verb, and the
        // plain sentence keep their results.
        for (text, status, date) in [
            ("이 조례(안)를 심의한다.", "confirmed", "2024-03-22"),
            ("이 조례(안)는 운영을 정한다.", "confirmed", "2024-03-22"),
            (
                "이 조례는 공포한 날부터 시행한다.",
                "confirmed",
                "2024-03-01",
            ),
        ] {
            expect(
                &case("", text, "20240301", "제정", 7),
                status,
                Some(date),
                text,
            );
        }
        // Accepted limit (user-r24-subject-boundary-limit-20261111): a subject split by a line
        // feed before its bracket is not followed.
        expect(
            &case(
                "",
                "이 조례\n(안)는 공포한 날 시행한다.",
                "20240301",
                "제정",
                7,
            ),
            "confirmed",
            Some("2024-03-22"),
            "line feed before the bracket",
        );
    }

    #[test]
    fn r25_a_subject_with_a_verb_that_is_no_detected_subject_keeps_its_sentence_out() {
        // 999801 and 999802: "이 시행한 조례는" holds the verb "시행한" and is no detected subject
        // of the reader's kinds; the first sentence of the section keeps it. The mutation that takes
        // out every subject match instead of the detected ones takes the sentence out and loses it.
        unresolved_case(
            &case(
                "",
                "제1조(목적) 이 조례는 운영을 정한다.\n부칙\n이 시행한 조례는 운영을 정한다.",
                "20240301",
                "제정",
                7,
            ),
            "999801 body",
        );
        unresolved_case(
            &case(
                "부칙\n이 시행한 조례는 운영을 정한다.\n제2조 이 조례는 2030년 1월 1일부터 시행한다.",
                "본문",
                "20240301",
                "제정",
                7,
            ),
            "999802 block",
        );
    }

    #[test]
    fn r25_blank_lines_are_not_scanned_again_for_every_heading_start() {
        // 100,000 blank lines before a heading: the heading test runs only on a line that starts with
        // "부" (r24 skipped the blank lines again from each of them, 50 s for 200,000 lines).
        let started = std::time::Instant::now();
        let body = format!(
            "제1조(목적) 이 조례는 운영을 정한다.{}부칙\n공포한 날 시행한다.",
            "\n".repeat(100_000)
        );
        unresolved_case(&case("", &body, "20240301", "제정", 7), "blank lines");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        // The headings after the blank lines are the same as before: a heading after blank lines
        // and a heading of two lines.
        assert_eq!(
            detected_sentence_starts("\n\n\n부칙\n공포한 날 시행한다.", false).len(),
            1
        );
        assert_eq!(
            detected_sentence_starts("\n \n부\n칙\n공포한 날 시행한다.", false).len(),
            1
        );
    }

    #[test]
    fn r25_the_longer_note_keeps_every_start_of_the_r24_detection() {
        // Codex 13 blocker 2: a subject inside a long note is found by the detection that was there
        // before; the longer note adds its own start and takes none away.
        let inner = "이 조례(참고. 이 규칙은 의회의 승인을 받은 날 시행합니다)는 운영을 정한다.";
        unresolved_case(&case("", inner, "20240301", "제정", 7), inner);
        let starts = detected_sentence_starts(inner, false);
        assert!(
            starts.contains(&inner.find("이 규칙은").unwrap()),
            "{starts:?}"
        );
        // The start of the long note itself joins when its own sentence holds the verb.
        let outer = "이 조례(참고. 이 규칙은 정한다)는 공포한 날 시행한다.";
        let starts = extended_sentence_starts(outer, false);
        assert!(starts.contains(&0), "{starts:?}");
        // Every start of the sentence-part detection is in the result.
        for text in [
            inner,
            outer,
            "부칙\n이 조례(안)는 공포한 날 시행한다.",
            "이 조례(안)는 공포한 날 시행한다.",
        ] {
            let all = extended_sentence_starts(text, false);
            for start in &detected_sentence_starts(text, false) {
                assert!(all.contains(start), "{text:?} {start}");
            }
        }
    }

    #[test]
    fn r25_a_sentence_only_the_source_text_shows_keeps_the_evidence_of_a_rule_that_stays_unresolved()
     {
        // Codex 13 blocker 4: the single label rule of an addendum judges the section by its decoded
        // text. The first sentence of this section is lost in the decoded text ("<생략>." ends it)
        // and kept in the source text. The label rule leaves the revision unresolved with its own
        // evidence; the sentence that only the source text shows must not take that evidence away.
        let block = "부칙 <2020.1.1 조례 제3호>\n제1조 &lt;생략&gt;. 의회의 승인을 받은 날 시행합니다.\n제2조 이 조례는 공포한 날부터 시행한다.";
        let found = resolve("ordinances", &case(block, "본문", "20240301", "제정", 7)).unwrap();
        assert_eq!(
            (found.status.as_str(), found.enforcement_date.as_deref()),
            ("unresolved", None)
        );
        let evidence = &found.date_evidence;
        assert_eq!(evidence["user_decision"], LABEL_BOTH_DIFFER_DECISION);
        assert_eq!(evidence["approved_rule"], "label-both-differ");
        assert_eq!(evidence["attributed_label"], "<2020.1.1 조례 제3호>");
        assert!(
            found.reason.contains(LABEL_BOTH_DIFFER_DECISION),
            "{}",
            found.reason
        );
        // The same section without the entity form is the control: the same evidence.
        let plain = "부칙 <2020.1.1 조례 제3호>\n제1조 <생략>. 의회의 승인을 받은 날 시행합니다.\n제2조 이 조례는 공포한 날부터 시행한다.";
        let control = resolve("ordinances", &case(plain, "본문", "20240301", "제정", 7)).unwrap();
        assert_eq!(control.date_evidence["approved_rule"], "label-both-differ");
        // A rule that confirms a date is withheld: the section keeps no date it confirms.
        let confirmed = "부칙 <2024.3.1 조례 제7호>\n제1조 &lt;생략&gt;. 의회의 승인을 받은 날 시행합니다.\n제2조 이 조례는 공포한 날부터 시행한다.";
        let found = resolve(
            "ordinances",
            &case(confirmed, "본문", "20240301", "제정", 7),
        )
        .unwrap();
        assert_eq!(
            (found.status.as_str(), found.enforcement_date.as_deref()),
            ("unresolved", None)
        );
        assert_eq!(
            found.date_evidence["date_withheld"],
            "added-detection-sentence"
        );
    }

    /// The sentence that only the added detection finds: a subject with a note of more than four
    /// characters and a verb the reader does not read.
    const ADDED: &str = "제9조 이 규칙 (안)은 의회의 승인을 받은 날 시행합니다.";

    fn ordinance(source: SourceRevision) -> Occurrence {
        resolve("ordinances", &source).unwrap()
    }

    fn admrule(kind: &str, body: &str) -> Occurrence {
        let mut source = case("", body, "20240301", "일부개정", 7);
        source.category = kind.to_owned();
        resolve("admrules", &source).unwrap()
    }

    #[test]
    fn r25c_every_path_that_reads_a_date_ends_in_the_same_check() {
        // One case for each place of `resolve` and `pre_decide` that sets a confirmed or an
        // estimated date from text (the table of the design, 2.1). The control keeps its date and
        // names the rule that set it; the same case with ADDED in the text it read is unresolved.
        let cases: Vec<(&str, Box<dyn Fn(&str) -> Occurrence>, &str, &str)> = vec![
            (
                "pre_decide all sections",
                Box::new(|x| {
                    ordinance(case(
                        "",
                        &format!(
                            "부칙\n이 조례는 공포한 날부터 시행한다.{x}\n부칙 <2024.3.1 조례 제7호>"
                        ),
                        "20240301",
                        "제정",
                        7,
                    ))
                }),
                "2024-03-01",
                "attributed addendum sets effect on the day of promulgation",
            ),
            (
                "pre_decide explicit, no label",
                Box::new(|x| {
                    ordinance(case(
                        "",
                        &format!("부칙\n이 조례는 2030년 1월 1일부터 시행한다.{x}"),
                        "20240301",
                        "제정",
                        7,
                    ))
                }),
                "2030-01-01",
                "approved clause in selected addendum",
            ),
            (
                "pre_decide explicit, label",
                Box::new(|x| {
                    ordinance(case(
                        &format!(
                            "부칙 <2024.3.1 조례 제7호>\n이 조례는 2030년 1월 1일부터 시행한다.{x}"
                        ),
                        "본문",
                        "20240301",
                        "일부개정",
                        7,
                    ))
                }),
                "2030-01-01",
                "approved clause in selected addendum",
            ),
            (
                "pre_decide promulgation, last unlabeled",
                Box::new(|x| {
                    ordinance(case(
                        &format!(
                            "부칙\n제1조 이 조례는 공포한 날부터 시행한다.\n부칙\n이 조례는 공포한 날부터 시행한다.{x}"
                        ),
                        "본문",
                        "20240301",
                        "제정",
                        7,
                    ))
                }),
                "2024-03-01",
                "approved clause in selected addendum",
            ),
            (
                "pre_decide label date",
                Box::new(|x| {
                    ordinance(case(
                        "",
                        &format!("부칙 <2030.1.1>\n이 조례는 공포한 날부터 시행한다.{x}"),
                        "20240301",
                        "제정",
                        7,
                    ))
                }),
                "2030-01-01",
                "single addendum label date takes priority over the promulgation date",
            ),
            (
                "single label rule",
                Box::new(|x| {
                    ordinance(revision(
                        "조례",
                        "제정",
                        "20240301",
                        7,
                        &format!("이 조례는 공포한 날부터 시행한다.{x}"),
                    ))
                }),
                "2024-03-01",
                "approved clause in selected addendum",
            ),
            (
                "selected addendum",
                Box::new(|x| {
                    ordinance(revision(
                        "조례",
                        "제정",
                        "20240301",
                        7,
                        &format!("이 조례는 2030년 1월 1일부터 시행한다.{x}"),
                    ))
                }),
                "2030-01-01",
                "approved clause in selected addendum",
            ),
            (
                "source body",
                Box::new(|x| admrule("훈령", &format!("이 훈령은 공포한 날부터 시행한다.{x}"))),
                "2024-03-01",
                "approved clause in source body",
            ),
            (
                "attributed section",
                Box::new(|x| {
                    ordinance(case(
                        &format!(
                            "부칙 <2020.1.1 조례 제3호>\n이 조례는 2020년 1월 1일부터 시행한다.\n부칙 <2024.3.1 조례 제7호>\n이 조례는 공포한 날부터 시행한다.{x}"
                        ),
                        "본문",
                        "20240301",
                        "일부개정",
                        7,
                    ))
                }),
                "2024-03-01",
                "attributed addendum sets effect on the day of promulgation",
            ),
            (
                "default 21 days",
                Box::new(|x| {
                    ordinance(case(
                        "",
                        &format!("제1조(목적) 이 조례는 운영을 정한다.{x}"),
                        "20240301",
                        "제정",
                        7,
                    ))
                }),
                "2024-03-22",
                "accepted publication plus 21 days; no holiday deferral",
            ),
            (
                "default 6 days",
                Box::new(|x| admrule("고시", &format!("제1조(목적) 이 고시는 운영을 정한다.{x}"))),
                "2024-03-07",
                "accepted notice plus 6 days; no holiday deferral",
            ),
            (
                "default arrival estimate",
                Box::new(|x| admrule("훈령", &format!("제1조(목적) 이 훈령은 운영을 정한다.{x}"))),
                "2024-03-01",
                "accepted arrival estimate from publication or issue date",
            ),
        ];
        for (name, make, date, rule) in cases {
            let control = make("");
            assert_eq!(
                control.enforcement_date.as_deref(),
                Some(date),
                "{name} control"
            );
            assert_eq!(control.reason, rule, "{name} control rule");
            let found = make(&format!("\n{ADDED}"));
            assert_eq!(found.status, "unresolved", "{name}");
            assert_eq!(found.enforcement_date, None, "{name}");
            assert_eq!(
                found.date_evidence["date_withheld"], "added-detection-sentence",
                "{name}"
            );
        }
    }

    #[test]
    fn r25c_a_date_that_no_rule_read_from_text_is_kept() {
        // A valid source date and an individual approval are no result of a rule that reads text.
        let mut dated = case(
            "",
            &format!("부칙\n이 조례는 공포한 날부터 시행한다.\n{ADDED}"),
            "20240301",
            "제정",
            7,
        );
        dated.enforcement = "20240401".to_owned();
        let found = ordinance(dated);
        assert_eq!(
            (
                found.status.as_str(),
                found.enforcement_date.as_deref(),
                found.reason.as_str()
            ),
            ("confirmed", Some("2024-04-01"), VALID_SOURCE_DATE)
        );
        let mut approved = revision(
            "조례",
            "일부개정",
            "20071031",
            7,
            &format!("이 조례는 공포 후 1개월이 경과한 날부터 시행한다.\n{ADDED}"),
        );
        approved.revision = "1000238".to_owned();
        let found = ordinance(approved);
        assert_eq!(
            (found.status.as_str(), found.enforcement_date.as_deref()),
            ("confirmed", Some("2007-12-01"))
        );
    }

    #[test]
    fn r25c_the_written_text_of_a_block_without_a_heading_is_read() {
        // Codex 14 blocker 1: the label rule reads a block without a heading as one text.
        let text = "이 조례는 &lt;생략&gt;. 의회의 승인을 받은 날 시행합니다.\n이 조례는 공포한 날부터 시행한다.";
        let found = ordinance(revision("조례", "제정", "20240301", 7, text));
        assert_eq!(
            (found.status.as_str(), found.enforcement_date.as_deref()),
            ("unresolved", None)
        );
        // Control: the same text written with "<" and ">" is one text; the sentence end after "<생략>."
        // is the third accepted limit (sentence-end boundary forms).
        let plain = text.replace("&lt;", "<").replace("&gt;", ">");
        let found = ordinance(revision("조례", "제정", "20240301", 7, &plain));
        assert_eq!(
            (found.status.as_str(), found.enforcement_date.as_deref()),
            ("confirmed", Some("2024-03-01"))
        );
    }

    #[test]
    fn r25c_the_written_text_of_every_section_is_read() {
        // Codex 14 blocker 2: the branch that reads the sections of the whole addendum part.
        let text = "부칙\n이 조례는 &lt;생략&gt;. 의회의 승인을 받은 날 시행합니다.\n이 조례는 공포한 날부터 시행한다.\n부칙 <2024.3.1 조례 제7호>";
        let found = ordinance(case("", text, "20240301", "제정", 7));
        assert_eq!(
            (found.status.as_str(), found.enforcement_date.as_deref()),
            ("unresolved", None)
        );
        let plain = text.replace("&lt;", "<").replace("&gt;", ">");
        let found = ordinance(case("", &plain, "20240301", "제정", 7));
        assert_eq!(
            (found.status.as_str(), found.enforcement_date.as_deref()),
            ("confirmed", Some("2024-03-01"))
        );
    }

    #[test]
    fn r25c_an_extended_subject_inside_the_note_of_another_is_found() {
        // Codex 14 blocker 3 and Codex 13 blocker 2: the search starts again after the first
        // character of a match, so a subject inside the note of another subject is a start.
        for text in [
            "이 조례(참고. 이 규칙[안]은 의회의 승인을 받은 날 시행합니다)는 운영을 정한다.",
            "이 조례[가 이 규칙[안]은 의회의 승인을 받은 날 시행합니다]는 운영을 정한다.",
            "이 조례(참고. 이 규칙 (안)은 의회의 승인을 받은 날 시행합니다)는 운영을 정한다.",
            "이 조례(참고. 이 규칙은 의회의 승인을 받은 날 시행합니다)는 운영을 정한다.",
        ] {
            unresolved_case(&case("", text, "20240301", "제정", 7), text);
        }
        let text = "이 조례(참고. 이 규칙[안]은 의회의 승인을 받은 날 시행합니다)는 운영을 정한다.";
        let inner = text.find("이 규칙").unwrap();
        assert!(
            bracket_subject_starts(text).contains(&inner),
            "{:?}",
            bracket_subject_starts(text)
        );
        // Controls: the inner sentence holds no verb; a note without a subject.
        for text in [
            "이 조례(참고. 이 규칙[안]은 운영을 정한다)는 운영을 정한다.",
            "이 조례(참고. 안내)는 운영을 정한다.",
        ] {
            expect(
                &case("", text, "20240301", "제정", 7),
                "confirmed",
                Some("2024-03-22"),
                text,
            );
        }
    }

    #[test]
    fn r25c_an_unresolved_result_keeps_every_key_of_its_evidence() {
        // Codex 14 blocker 4: the extended subject of the second article is no unread clause of the
        // reader, so the label rule sees the same sentences as in r24 and keeps its evidence.
        let text = "부칙 <2020.1.1 조례 제3호>\n제1조 이 조례는 공포한 날부터 시행한다.\n제2조 이 규칙 (안)은 의회의 승인을 받은 날 시행합니다.";
        let found = ordinance(case(text, "본문", "20240301", "제정", 7));
        let reason = "effective date is unresolved; user-label-both-differ-unresolved-20261103: the only addendum label differs from this revision in both date and number";
        assert_eq!(
            (found.status.as_str(), found.enforcement_date.as_deref()),
            ("unresolved", None)
        );
        assert_eq!(found.reason, reason);
        assert_eq!(
            found.date_evidence,
            json!({
                "approved_rule": "label-both-differ",
                "arrival_estimate": false,
                "attributed_label": "<2020.1.1 조례 제3호>",
                "raw_enforcement_date": "99991231",
                "raw_promulgation_date": "20240301",
                "rule": reason,
                "source_path": "9.xml",
                "user_decision": "user-label-both-differ-unresolved-20261103",
            })
        );
        // The same input without the added sentence gives the same evidence.
        let without = ordinance(case(
            &text.replace(
                "\n제2조 이 규칙 (안)은 의회의 승인을 받은 날 시행합니다.",
                "",
            ),
            "본문",
            "20240301",
            "제정",
            7,
        ));
        assert_eq!(without.date_evidence, found.date_evidence);
    }

    #[test]
    fn r25c_a_sentence_only_the_added_detection_finds_withholds_the_date_in_any_section() {
        // The added detection has no section scope: an older section holds the sentence, the target
        // section holds a plain promulgation sentence. A sentence of the r24 detection in an older
        // section keeps its r24 scope (the control): no section of another revision decides.
        let older = |sentence: &str| {
            case(
                &format!(
                    "부칙 <2020.1.1 조례 제3호>\n{sentence}\n부칙 <2024.3.1 조례 제7호>\n이 조례는 공포한 날부터 시행한다."
                ),
                "본문",
                "20240301",
                "일부개정",
                7,
            )
        };
        for sentence in [
            "이 규칙 (안)은 의회의 승인을 받은 날 시행합니다.",
            "이 조례(안)는 의회의 승인을 받은 날 시행합니다.",
            "이 조례는 &lt;생략&gt;. 의회의 승인을 받은 날 시행합니다.",
        ] {
            let found = ordinance(older(sentence));
            assert_eq!(
                (found.status.as_str(), found.enforcement_date.as_deref()),
                ("unresolved", None),
                "{sentence}"
            );
        }
        let control = ordinance(older("이 조례는 공포한 날부터 시행합니다."));
        assert_eq!(
            (control.status.as_str(), control.enforcement_date.as_deref()),
            ("confirmed", Some("2024-03-01"))
        );
    }

    #[test]
    fn r25c_a_written_offset_follows_the_decoded_offset() {
        // The decoding changes three entities; the written offset of a decoded offset is the place
        // where the decoded prefix and the decoded text agree, and no entity is cut in two.
        let raw = "a&lt;b&gt;c&amp;d&amp;lt;e";
        let decoded = decode_markup(raw);
        assert_eq!(decoded, "a<b>c&d&lt;e");
        let offsets: Vec<usize> = decoded.char_indices().map(|(index, _)| index).collect();
        let written = written_offsets(raw, &offsets);
        for (index, at) in offsets.iter().zip(&written) {
            assert_eq!(decode_markup(&raw[..*at]), decoded[..*index], "{index}");
        }
        assert_eq!(written_offsets(raw, &[decoded.len()]), vec![raw.len()]);
        // The order and the repeats of the offsets do not matter.
        assert_eq!(
            written_offsets(raw, &[5, 1, 5]),
            written_offsets(raw, &[1, 5])
        );
        // Multibyte text keeps its characters whole.
        let raw = "이 조례는 &lt;생략&gt;. 시행한다.";
        let decoded = decode_markup(raw);
        let at = written_offsets(raw, &[decoded.find("시행").unwrap()])[0];
        assert!(raw[at..].starts_with("시행"));
    }

    #[test]
    fn r25c_an_entity_elsewhere_does_not_make_a_sentence_of_the_r24_detection_new() {
        // MST 1089007 of the population: its addenda hold entities ("&amp;#160;") and sentences of
        // the r24 detection in older sections. Both texts show those sentences at the same
        // places, so the added detection finds nothing and the date of r24 stays.
        let text = "부칙 <2020.1.1 조례 제3호>\n이 조례는 공포한 날부터 시행합니다.\n&amp;#160;② 보칙\n부칙 <2024.3.1 조례 제7호>\n이 조례는 공포한 날부터 시행한다.";
        let found = ordinance(case(text, "본문", "20240301", "일부개정", 7));
        assert_eq!(
            (found.status.as_str(), found.enforcement_date.as_deref()),
            ("confirmed", Some("2024-03-01"))
        );
        // A sentence that only the written text shows is found even when other entities stand
        // earlier in the text.
        let text = "부칙 <2020.1.1 조례 제3호>\n&amp;#160;② 보칙\n부칙 <2024.3.1 조례 제7호>\n이 조례는 &lt;생략&gt;. 의회의 승인을 받은 날 시행합니다.\n이 조례는 공포한 날부터 시행한다.";
        let found = ordinance(case(text, "본문", "20240301", "일부개정", 7));
        assert_eq!(
            (found.status.as_str(), found.enforcement_date.as_deref()),
            ("unresolved", None)
        );
    }

    #[test]
    fn r25c_the_default_period_counts_every_sentence_of_the_added_detection() {
        // The reader reads a sentence that only the written text shows as one clause when a later
        // sentence of the same line ends with "시행한다" (the lazy match swallows the line). No rule
        // consumes that clause when the decoded text names no date, so the default period or the
        // arrival estimate would stand: they count every sentence of the added detection.
        let line = "이 {k}은 &lt;생략&gt;. 의회의 승인을 받은 날 시행합니다. 제3조 시장은 사업을 시행한다.";
        let found = ordinance(case(
            "",
            &line.replace("{k}", "조례"),
            "20240301",
            "제정",
            7,
        ));
        assert_eq!(
            (found.status.as_str(), found.enforcement_date.as_deref()),
            ("unresolved", None)
        );
        let found = admrule("훈령", &line.replace("{k}", "훈령"));
        assert_eq!(
            (found.status.as_str(), found.enforcement_date.as_deref()),
            ("unresolved", None)
        );
        let found = admrule("고시", &line.replace("{k}", "고시"));
        assert_eq!(
            (found.status.as_str(), found.enforcement_date.as_deref()),
            ("unresolved", None)
        );
        // Controls: the same line written with "<" and ">" is the third accepted limit (a boundary
        // form closes the sentence early), so the default period stands as in r24; and a line
        // without the sentence keeps the default period.
        let plain = line
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("{k}", "조례");
        let found = ordinance(case("", &plain, "20240301", "제정", 7));
        assert_eq!(found.enforcement_date.as_deref(), Some("2024-03-22"));
        let control = ordinance(case(
            "",
            "제3조 시장은 사업을 시행한다.",
            "20240301",
            "제정",
            7,
        ));
        assert_eq!(control.enforcement_date.as_deref(), Some("2024-03-22"));
    }

    /// One source of a plan: revision, serial and a body for the index.
    fn plan_source(mut source: SourceRevision, revision: &str) -> SourceRevision {
        source.revision = revision.to_owned();
        source.serial = revision.parse().unwrap();
        source.source_path = format!("{revision}.xml");
        source.markdown = format!("---\nname: {revision}\n---\nbody\n").into_bytes();
        source
    }

    #[test]
    fn r25d_the_gate_leaves_the_results_of_the_plan_processing_as_they_are() {
        // Adversary C: a copy conflict. The conflict decides, and the gate adds no key.
        let mut conflicted = case(
            "",
            "제2조 이 조례(제1234호)는 2025년 1월 1일 시행합니다.",
            "20240301",
            "제정",
            7,
        );
        conflicted.copy_conflict = Some("the copies differ".to_owned());
        let plan = DatePlan::new("ordinances", true, vec![plan_source(conflicted, "1")]).unwrap();
        let found = &plan.revisions[0].occurrences[0];
        assert_eq!(found.status, "unresolved");
        assert!(found.date_evidence.get("date_withheld").is_none());
        assert_eq!(found.date_evidence["decision"], "duplicate-copy-conflict");
        // Adversary K: the label date is on or after the promulgation date of the next revision.
        // The cap of the plan comes first and keeps its evidence; the gate finds an unresolved result.
        let sentence = "제2조 이 조례(제1234호)는 2025년 1월 1일 시행합니다.";
        let first = case(
            "부칙 <2024.6.1>\n이 조례는 공포한 날부터 시행한다.",
            sentence,
            "20240301",
            "일부개정",
            7,
        );
        let second = case(
            "부칙 <2024.5.1>\n이 조례는 공포한 날부터 시행한다.",
            "본문",
            "20240501",
            "일부개정",
            8,
        );
        let plan = DatePlan::new(
            "ordinances",
            true,
            vec![plan_source(first, "1"), plan_source(second, "2")],
        )
        .unwrap();
        let found = &plan.revisions[0].occurrences[0];
        assert_eq!(found.status, "unresolved");
        assert_eq!(
            found.date_evidence["decision"],
            "label-date-after-next-revision"
        );
        assert_eq!(found.date_evidence["next_revision"], "2");
        assert!(found.date_evidence.get("date_withheld").is_none());
        assert_eq!(plan.revisions[0].status.as_deref(), Some("unresolved"));
        // The same revision without the sentence gives the same evidence.
        let first = case(
            "부칙 <2024.6.1>\n이 조례는 공포한 날부터 시행한다.",
            "본문",
            "20240301",
            "일부개정",
            7,
        );
        let second = case(
            "부칙 <2024.5.1>\n이 조례는 공포한 날부터 시행한다.",
            "본문",
            "20240501",
            "일부개정",
            8,
        );
        let control = DatePlan::new(
            "ordinances",
            true,
            vec![plan_source(first, "1"), plan_source(second, "2")],
        )
        .unwrap();
        assert_eq!(
            control.revisions[0].occurrences[0].date_evidence,
            found.date_evidence
        );
    }

    #[test]
    fn r25d_a_revision_without_a_promulgation_date_is_undated_and_the_plan_stands() {
        // Adversary N: the gate gives the state of the other unresolved paths, "undated" without a
        // promulgation date, and the plan validates.
        let mut source = case(
            "이 조례는 2025년 1월 1일부터 시행한다.",
            "제2조 이 조례(제1234호)는 2025년 1월 1일 시행합니다.",
            "",
            "제정",
            7,
        );
        source.promulgation = String::new();
        let control = {
            let mut control = source.clone();
            control.xml = String::from_utf8(control.xml)
                .unwrap()
                .replace(
                    "제2조 이 조례(제1234호)는 2025년 1월 1일 시행합니다.",
                    "본문",
                )
                .into_bytes();
            control
        };
        let plan = DatePlan::new("ordinances", true, vec![plan_source(control, "2")]).unwrap();
        assert_eq!(plan.revisions[0].occurrences[0].status, "confirmed");
        let plan = DatePlan::new("ordinances", true, vec![plan_source(source, "1")]).unwrap();
        let found = &plan.revisions[0].occurrences[0];
        assert_eq!(found.status, "undated");
        assert_eq!(found.enforcement_date, None);
        assert_eq!(
            found.undated_semantics,
            vec!["공포일자".to_owned(), "시행일자".to_owned()]
        );
        assert_eq!(
            found.date_evidence["date_withheld"],
            "added-detection-sentence"
        );
    }

    #[test]
    fn r25d_the_gate_reads_the_source_texts_that_the_rules_replace_or_never_read() {
        // Adversary A: the amendment text replaces the addendum block that names no date.
        let reason =
            "□ 제정이유\n정함.\n부칙\n이 조례는 공포한 날부터 시행한다.\n□ 주요내용\n가. 정함.";
        for addendum in [
            "이 조례(제1234호)는 2025년 1월 1일 시행한다.",
            "이 조례는 &lt;별표&gt;. 2025년 1월 1일 시행합니다.",
        ] {
            let mut source = amendment_text_case(reason);
            source.xml = String::from_utf8(source.xml)
                .unwrap()
                .replace(
                    "<부칙></부칙>",
                    &format!("<부칙><부칙내용><![CDATA[{addendum}]]></부칙내용></부칙>"),
                )
                .into_bytes();
            let found = ordinance(source);
            assert_eq!(
                (found.status.as_str(), found.enforcement_date.as_deref()),
                ("unresolved", None),
                "{addendum}"
            );
        }
        // Adversary E: a later part of the amendment text, with a heading no rule reads.
        let reason = "□ 개정이유\\n설명\\n부칙\\n이 조례는 공포한 날부터 시행한다.\\n□ 경과\\n부칙［2024.3.1］\\n이 조례(제1234호)는 2025년 1월 1일 시행한다.\\n□ 주요내용\\n가. 정함.".replace("\\n", "\n");
        let found = ordinance(amendment_text_case(&reason));
        assert_eq!(
            (found.status.as_str(), found.enforcement_date.as_deref()),
            ("unresolved", None)
        );
        // A later part with a clean heading and a plain sentence is a text r24 could read: it counts
        // only for the added detection, so the date of the first part stands.
        let reason = "□ 개정이유\n설명\n부칙\n이 조례는 공포한 날부터 시행한다.\n□ 경과\n부칙\n이 조례는 공포한 날부터 시행한다.\n□ 주요내용\n가. 정함.";
        let found = ordinance(amendment_text_case(reason));
        let alone = ordinance(amendment_text_case(
            "□ 개정이유\n설명\n부칙\n이 조례는 공포한 날부터 시행한다.\n□ 주요내용\n가. 정함.",
        ));
        assert_eq!(found.date_evidence, alone.date_evidence);
        assert!(found.date_evidence.get("date_withheld").is_none());
        // Adversary D: the heading is no heading that the extraction reads; the default period is
        // taken as in r24 and the gate withholds it, so the body proviso leaves no key behind.
        let mut source = amendment_text_case(
            "□ 제정이유\n정함.\n부칙［2024.3.1］\n이 조례는 의회의 승인을 받은 날 시행한다.\n□ 주요내용\n가. 정함.",
        );
        source.xml = String::from_utf8(source.xml)
            .unwrap()
            .replace("제1조(목적) 이 조례는 운영을 정한다.", "제1조(목적) 이 조례는 운영을 정한다.\n제2조 시장은 사업을 시행한다. 다만, 필요하면 따로 정한다.")
            .into_bytes();
        let found = ordinance(source);
        assert_eq!(found.status, "unresolved");
        assert!(
            found
                .date_evidence
                .get("proviso_after_effective_sentence")
                .is_none()
        );
        assert!(found.date_evidence.get("user_decision").is_none());
        assert_eq!(
            found.date_evidence["date_withheld"],
            "added-detection-sentence"
        );
        // Adversary L: the text of a paragraph, no rule reads it.
        let law = |paragraph: &str| {
            let mut source = revision("법률", "제정", "20240301", 7, "");
            source.xml = format!("<LawService><조문><조문단위><조문내용>제2조(시행일) 이 법은 공포한 날부터 시행한다.</조문내용><항><항내용>{paragraph}</항내용></항></조문단위></조문></LawService>").into_bytes();
            resolve("laws", &source).unwrap()
        };
        assert_eq!(law("본문").enforcement_date.as_deref(), Some("2024-03-01"));
        assert_eq!(
            law("이 법(법률 제1234호)은 2025년 1월 1일 시행합니다.").status,
            "unresolved"
        );
    }

    #[test]
    fn r25d_a_note_of_any_pair_and_depth_makes_a_subject() {
        // Adversary B: mixed pairs, nesting, a repeat and a note that no bracket closes.
        for subject in [
            "이 조례（제1234호)는",
            "이 조례((가)안)는",
            "이 조례(안]는",
            "이 조례[안)는",
            "이 조례〈안〉〉는",
            "이 조례(안))는",
            "이 조례「(안)」(제2호)는",
            "이 조례(제1234호는",
            "이 조례 〈제1호〉 는",
        ] {
            let text = format!("제2조 {subject} 2025년 1월 1일 시행한다.");
            unresolved_case(&case("", &text, "20240301", "제정", 7), &text);
            let starts = bracket_subject_starts(&text);
            assert_eq!(starts.len(), 1, "{subject} {starts:?}");
        }
        // Controls: a particle other than the four, no verb, a quotation mark, no note.
        for text in [
            "이 조례(안)를 2025년 1월 1일 시행한다.",
            "이 조례(안)는 운영을 정한다.",
            "이 조례\"안\"는 2025년 1월 1일 시행한다.",
        ] {
            assert!(bracket_subject_starts(text).is_empty(), "{text}");
        }
    }

    #[test]
    fn r25d_a_line_feed_between_the_lead_and_the_note_keeps_the_note_of_its_own_line() {
        // The lead of a subject may be followed by a line feed (white space after "이"); the note
        // belongs to the line of its opening bracket, and a second subject of that line is found.
        let text =
            "이\n조례(안)는 2025년 1월 1일 시행한다.\n이 조례(가)는 2026년 1월 1일 시행한다.";
        let starts = bracket_subject_starts(text);
        assert_eq!(
            starts,
            vec![0, text.rfind("이 조례").unwrap()],
            "{starts:?}"
        );
        // Many openings on one line, with no bracket that closes them and no particle after them,
        // cost one pass over the line.
        let line = "본 법(".repeat(20000);
        assert!(bracket_subject_starts(&line).is_empty());
    }

    #[test]
    fn r25d_a_partial_enforcement_sentence_of_another_article_keeps_the_general_date() {
        // user-approval-addendum-rules-20261103, item 2: a partial enforcement of separate articles
        // ("이 조례 중 제3조는 ... 시행합니다") is no effective date of the revision. The general
        // sentence gives the date, and the partial timing stays in the evidence. The detection does
        // not know the form "이 X 중 제N조는" (no kind right before the particle and no note), so
        // the gate does not count it either: r24 and r25c agree (adversary L, 802002).
        let general = "이 조례는 공포한 날부터 시행한다.";
        for (name, source) in [
            (
                "addendum, 합니다",
                case(
                    &format!("{general}\n이 조례 중 제3조는 2025년 1월 1일부터 시행합니다."),
                    "본문",
                    "20240301",
                    "제정",
                    7,
                ),
            ),
            (
                "addendum, 한다",
                case(
                    &format!("{general}\n이 조례 중 제3조는 2025년 1월 1일부터 시행한다."),
                    "본문",
                    "20240301",
                    "제정",
                    7,
                ),
            ),
            (
                "body",
                case(
                    "",
                    &format!(
                        "제1조(시행일) {general}\n② 이 조례 중 제3조는 2025년 1월 1일부터 시행합니다."
                    ),
                    "20240301",
                    "제정",
                    7,
                ),
            ),
            (
                "labelled",
                case(
                    &format!(
                        "부칙 <2024.3.1>\n{general}\n이 조례 중 제3조는 2025년 1월 1일부터 시행합니다."
                    ),
                    "본문",
                    "20240301",
                    "일부개정",
                    7,
                ),
            ),
        ] {
            expect(&source, "confirmed", Some("2024-03-01"), name);
            assert!(
                resolve("ordinances", &source)
                    .unwrap()
                    .date_evidence
                    .get("date_withheld")
                    .is_none(),
                "{name}"
            );
        }
        // The same sentence in the paragraph text of a law (adversary L, 802002).
        let law = |paragraph: &str| {
            let mut source = revision("법률", "제정", "20240301", 7, "");
            source.xml = format!("<LawService><조문><조문단위><조문내용>제2조(시행일) 이 법은 공포한 날부터 시행한다.</조문내용><항><항내용>{paragraph}</항내용></항></조문단위></조문></LawService>").into_bytes();
            resolve("laws", &source).unwrap()
        };
        let found = law("② 이 법 중 제3조는 2025년 1월 1일부터 시행합니다.");
        assert_eq!(
            (found.status.as_str(), found.enforcement_date.as_deref()),
            ("confirmed", Some("2024-03-01"))
        );
        assert!(found.date_evidence.get("date_withheld").is_none());
        // Control: a subject of the detection in the same place is withheld (not this form).
        assert_eq!(
            law("② 이 법(법률 제1234호)은 2025년 1월 1일 시행합니다.").status,
            "unresolved"
        );
    }

    /// A source of a data set with its own XML text.
    fn with_xml(mut source: SourceRevision, xml: &str) -> SourceRevision {
        source.xml = xml.as_bytes().to_vec();
        source
    }

    fn ordinance_xml(body: &str, reason: &str, addendum: &str) -> String {
        format!(
            "<LawService><공포번호>7</공포번호><조문><조><조내용>{body}</조내용></조></조문><부칙><부칙내용><![CDATA[{addendum}]]></부칙내용></부칙><제개정이유><제개정이유내용><![CDATA[{reason}]]></제개정이유내용></제개정이유></LawService>"
        )
    }

    fn withheld(found: &Occurrence) -> bool {
        found.date_evidence.get("date_withheld").is_some() && found.enforcement_date.is_none()
    }

    #[test]
    fn r25e_a_part_that_no_rule_reads_holds_a_date_back_by_the_source_of_the_date() {
        let ord = |reason: &str, addendum: &str| {
            ordinance(with_xml(
                case("", "본문", "20240301", "제정", 7),
                &ordinance_xml("제1조(목적) 이 조례는 운영을 정한다.", reason, addendum),
            ))
        };
        let first = "□ 개정이유\n설명\n부칙\n이 조례는 공포한 날부터 시행한다.\n";
        // A date that a rule read (the first part, which the extraction reads): a later part holds
        // it back only by a sentence that the reader does not read (F1a).
        let unread = format!(
            "{first}□ 경과\n부칙\n이 조례(제1234호)는 2025년 1월 1일 시행한다.\n□ 주요내용\n가. 정함."
        );
        assert!(withheld(&ord(&unread, "")), "later part, unread");
        let unread = format!(
            "{first}□ 경과\n부칙\n이 조례는 의회의 승인을 받은 날 시행합니다.\n□ 주요내용\n가. 정함."
        );
        assert!(
            withheld(&ord(&unread, "")),
            "later part, r24 sentence the reader does not read"
        );
        let alone = ord(&format!("{first}□ 주요내용\n가. 정함."), "");
        assert!(!withheld(&alone));
        assert_eq!(alone.enforcement_date.as_deref(), Some("2024-03-01"));
        // A sentence that the reader reads is a restatement. It names another date than the rule
        // read (a conflict between two sources of a date: a decision of its own, not a sentence
        // that nothing reads) and holds nothing back (F1d).
        let read = format!(
            "{first}□ 경과\n부칙\n이 조례는 2025년 1월 1일부터 시행한다.\n□ 주요내용\n가. 정함."
        );
        let found = ord(&read, "");
        assert!(!withheld(&found));
        assert_eq!(found.enforcement_date, alone.enforcement_date);
        let unused = ord(
            "□ 제정이유\n정함.\n부칙\n이 조례는 2025년 1월 1일부터 시행한다.\n□ 주요내용\n가. 정함.",
            "이 조례는 공포한 날부터 시행한다.",
        );
        assert!(!withheld(&unused), "restatement, extraction unused");
        assert_eq!(unused.enforcement_date.as_deref(), Some("2024-03-01"));
        // A date of the default period: every sentence of every part holds it back, read or not.
        let by_default = ord(
            "□ 제정이유\n정함.\n부칙［2024.3.1］\n이 조례는 2025년 1월 1일부터 시행한다.\n□ 주요내용\n가. 정함.",
            "",
        );
        assert!(withheld(&by_default), "default period, unclean heading");
        // F1b: an administrative rule never extracts, so its date is the default period.
        let adm = |kind: &str| {
            let mut source = revision(kind, "제정", "20200101", 1, "");
            source.xml = format!("<AdmRulService><조문내용>제1조 목적</조문내용><부칙></부칙><제개정이유><제개정이유내용><![CDATA[□ 개정이유\n설명\n부칙\n이 {kind}은 2025년 1월 1일부터 시행한다.]]></제개정이유내용></제개정이유></AdmRulService>").into_bytes();
            resolve("admrules", &source).unwrap()
        };
        for kind in ["고시", "훈령"] {
            assert!(withheld(&adm(kind)), "{kind}");
        }
        // F1c: a law with a date that a rule read (the sentence of the article): the reader reads the
        // restatement in the amendment text, an unread sentence holds the date back.
        let law = |sentence: &str| {
            let mut law = revision("법률", "제정", "20240301", 7, "");
            law.xml = format!("<LawService><조문><조문단위><조문내용>제1조 이 법은 공포한 날부터 시행한다.</조문내용></조문단위></조문><제개정이유><제개정이유내용><![CDATA[부칙\n{sentence}]]></제개정이유내용></제개정이유></LawService>").into_bytes();
            resolve("laws", &law).unwrap()
        };
        assert!(!withheld(&law("이 법은 2025년 1월 1일부터 시행한다.")));
        assert!(withheld(&law(
            "이 법(법률 제1234호)은 2025년 1월 1일 시행합니다."
        )));
    }

    #[test]
    fn r25e_the_amendment_statement_is_a_text_that_only_the_gate_reads() {
        // F2: the statement of an administrative rule holds the addendum; the rules never read it.
        let adm = |statement: &str, kind: &str| {
            let mut source = revision(kind, "제정", "20090410", 1, "");
            source.xml = format!("<AdmRulService><조문내용>제1조 목적</조문내용><부칙></부칙><개정문><개정문내용>\n<![CDATA[{statement}]]>\n</개정문내용></개정문></AdmRulService>").into_bytes();
            resolve("admrules", &source).unwrap()
        };
        let found = adm("부  칙\n이 예규는 2009. 6. 27.부터 시행한다.", "예규");
        assert!(withheld(&found));
        assert_eq!(found.status, "unresolved");
        let control = adm("일부를 다음과 같이 개정한다.", "예규");
        assert_eq!(control.status, "estimated");
        assert_eq!(control.enforcement_date.as_deref(), Some("2009-04-10"));
    }

    #[test]
    fn r25e_a_report_of_applied_label_dates_follows_the_final_state() {
        // LD: the gate withholds a label date; the report of applied label dates does not list it.
        let label = |body: &str| {
            let mut source = case(
                "부칙 <2024.6.1>\n이 조례는 공포한 날부터 시행한다.",
                body,
                "20240301",
                "일부개정",
                7,
            );
            source.markdown = b"---\nname: 1\n---\nbody\n".to_vec();
            DatePlan::new("ordinances", true, vec![plan_source(source, "1")]).unwrap()
        };
        let applied = label("본문").label_date_report().unwrap();
        assert_eq!(applied["label_date_applied"].as_array().unwrap().len(), 1);
        let plan = label("제2조 이 조례(제1234호)는 2025년 1월 1일 시행합니다.");
        assert_eq!(plan.revisions[0].occurrences[0].status, "unresolved");
        let report = plan.label_date_report().unwrap();
        assert_eq!(report["label_date_applied"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn r25e_a_bracket_of_any_script_and_a_closer_before_the_note_make_a_subject() {
        // B': the brackets are the characters of the Unicode categories Ps/Pi and Pe/Pf.
        for subject in [
            "이 조례【제1234호】는",
            "이 조례\u{2329}제1234호\u{232A}는",
            "이 조례{제1234호}는",
            "이 조례«제1234호»는",
            "이 조례\u{27E8}제1234호\u{27E9}는",
            "이 조례\u{3016}제1234호\u{3017}는",
            "이 조례\u{FE59}제1234호\u{FE5A}는",
            "이 조례\u{2768}제1234호\u{2769}는",
            "이 조례)(제1234호)는",
            "이 조례 ) ( 제1234호 ) 는",
        ] {
            let text = format!("제2조 {subject} 2025년 1월 1일 시행한다.");
            assert_eq!(bracket_subject_starts(&text).len(), 1, "{subject}");
            unresolved_case(&case("", &text, "20240301", "제정", 7), &text);
        }
        for text in [
            "이 조례'안'는 2025년 1월 1일 시행한다.",
            "이 조례\"안\"는 2025년 1월 1일 시행한다.",
        ] {
            assert!(bracket_subject_starts(text).is_empty(), "{text}");
        }
    }

    #[test]
    fn r25e_a_heading_behind_a_list_mark_is_a_part_of_the_amendment_text() {
        // F3: the gate takes the heading behind a mark; r24 does not.
        for mark in ["□ ", "○ ", "- ", "· ", "1. ", "가. ", "(1) ", "① "] {
            for sentence in [
                "이 조례(제1234호)는 2025년 1월 1일 시행한다.",
                "이 조례는 2025년 1월 1일부터 시행한다.",
            ] {
                let reason =
                    format!("□ 개정이유\n설명\n{mark}부칙\n{sentence}\n□ 주요내용\n가. 정함.");
                let found = ordinance(with_xml(
                    case("", "본문", "20240301", "제정", 7),
                    &ordinance_xml("제1조(목적) 이 조례는 운영을 정한다.", &reason, ""),
                ));
                assert!(withheld(&found), "{mark:?} {sentence}");
            }
        }
        // Controls: no heading, or a sentence of the heading line without a verb.
        let reason = "□ 개정이유\n설명\n○ 부칙을 신설한다.\n□ 주요내용\n가. 정함.";
        let found = ordinance(with_xml(
            case("", "본문", "20240301", "제정", 7),
            &ordinance_xml("제1조(목적) 이 조례는 운영을 정한다.", reason, ""),
        ));
        assert!(!withheld(&found));
    }

    #[test]
    fn r25e_a_subject_that_the_xml_events_or_a_format_character_split_is_found() {
        // R: r24 joins the events of one element with a line feed after each one.
        let events = |body: &str| {
            ordinance(with_xml(
                case("", "본문", "20240301", "제정", 7),
                &format!(
                    "<LawService><공포번호>7</공포번호><조문><조><조내용>{body}</조내용></조></조문><부칙></부칙></LawService>"
                ),
            ))
        };
        for body in [
            "제2조 이 조례&#40;제1234호&#41;는 2025년 1월 1일 시행한다.",
            "제2조 이 조례&lt;제1234호&gt;는 2025년 1월 1일 시행한다.",
            "제2조 이 조례<![CDATA[(제1234호)는 2025년 1월 1일 시행한다.]]>",
            "제2조 이 조<![CDATA[례는 2025년 1월 1일 시행합니다.]]>",
        ] {
            assert!(withheld(&events(body)), "{body}");
        }
        // Controls: the events hold whole sentences, or no sentence at all.
        for body in [
            "<![CDATA[제2조 시장은 사업을 정한다.]]><![CDATA[제3조 시장은 사업을 정한다.]]>",
            "제2조 A &amp; B 사업은 따로 정한다.",
        ] {
            assert!(!withheld(&events(body)), "{body}");
        }
        // ZW: a format character between the kind and the particle.
        for sentence in [
            "제2조 이 조례\u{200B}는 2025년 1월 1일 시행한다.",
            "제2조 이 조례\u{200B}(제1234호)는 2025년 1월 1일 시행한다.",
            "제2조 이 조례\u{2060}는 2025년 1월 1일 시행합니다.",
            "제2조 이 조례\u{00AD}는 2025년 1월 1일 시행한다.",
            "\u{FEFF}제2조 이 조례\u{FEFF}는 2025년 1월 1일 시행한다.",
        ] {
            assert!(
                withheld(&ordinance(case("", sentence, "20240301", "제정", 7))),
                "{sentence:?}"
            );
        }
        // A format character far from any sentence changes nothing, and so does a sentence that
        // both forms show.
        let found = ordinance(case(
            "",
            "제1조(목적)\u{200B} 이 조례는 운영을 정한다.\n제2조 이 조례는 공포한 날부터 시행한다.",
            "20240301",
            "제정",
            7,
        ));
        assert!(!withheld(&found));
    }

    #[test]
    fn r25e_copies_of_one_revision_get_the_result_of_the_copy_that_the_gate_withholds() {
        // AC: copies with the same identity and revision and content that differs.
        let copy = |body: &str| {
            let mut source = case("", body, "20200101", "제정", 7);
            source.category = "고시".to_owned();
            source.canonical_path = "p.md".to_owned();
            source.markdown = format!("---\nname: {body}\n---\nbody\n").into_bytes();
            source
        };
        let sentence = "제2조 이 고시(제1234호)는 2025년 1월 1일 시행한다.";
        let plan = DatePlan::new(
            "admrules",
            true,
            vec![
                plan_source(copy(sentence), "1"),
                plan_source(copy("제2조 장관은 사업을 정한다."), "1"),
            ],
        )
        .unwrap();
        assert_eq!(plan.revisions.len(), 2);
        for revision in &plan.revisions {
            let found = &revision.occurrences[0];
            assert_eq!(found.status, "unresolved");
            assert_eq!(
                found.date_evidence["date_withheld"],
                "added-detection-sentence"
            );
            assert_eq!(revision.status.as_deref(), Some("unresolved"));
        }
        // Control: no copy holds the sentence, and a revision of another identity is unaffected.
        let plan = DatePlan::new(
            "admrules",
            true,
            vec![
                plan_source(copy("제2조 장관은 사업을 정한다."), "1"),
                plan_source(copy("제2조 장관은 일을 정한다."), "1"),
            ],
        )
        .unwrap();
        for revision in &plan.revisions {
            assert_eq!(revision.occurrences[0].status, "confirmed");
        }
    }

    #[test]
    fn r25d_a_sentence_of_the_added_detection_counts_even_when_the_reader_swallows_it() {
        // Adversary S: the lazy match of the reader makes a sentence that only the written text
        // shows look read when a later sentence of the same line ends with "시행한다".
        let body = "제2조 이 조례는 &lt;별표&gt;. 2025년 1월 1일 시행합니다. 제3조는 공포한 날부터 시행한다.";
        let found = ordinance(case(
            "이 조례는 공포한 날부터 시행한다.",
            body,
            "20240301",
            "제정",
            7,
        ));
        assert_eq!(
            (found.status.as_str(), found.enforcement_date.as_deref()),
            ("unresolved", None)
        );
        let found = ordinance(case(
            "이 조례는 공포한 날부터 시행한다.",
            "제2조 시장은 사업을 정한다.",
            "20240301",
            "제정",
            7,
        ));
        assert_eq!(found.enforcement_date.as_deref(), Some("2024-03-01"));
    }

    #[test]
    fn r21_the_effective_verb_has_a_stem_and_the_nouns_are_no_verb() {
        // Lead ruling in the r21 scope: the verb "시행" ("施行") counts with the stems 하, 한, 함,
        // 할, 해, 했, 되, 된, 됨, 될, 돼, 됐 and with spaces or line feeds between its characters. A
        // noun use ("시행에 관하여 필요한 사항", "시행령", "시행일") is no verb. A sentence the reader
        // cannot read keeps its section unresolved.
        let found: &[&str] = &[
            "이 조례는 승인을 받은 날 시행하",
            "이 조례는 승인을 받은 날 시행한",
            "이 조례는 승인을 받은 날 시행함",
            "이 조례는 승인을 받은 날 시행할",
            "이 조례는 승인을 받은 날 시행해",
            "이 조례는 승인을 받은 날 시행했",
            "이 조례는 승인을 받은 날 시행되",
            "이 조례는 승인을 받은 날 시행된",
            "이 조례는 승인을 받은 날 시행됨",
            "이 조례는 승인을 받은 날 시행될",
            "이 조례는 승인을 받은 날 시행돼",
            "이 조례는 승인을 받은 날 시행됐",
            "이 조례는 승인을 받은 날 시행된다.",
            "이 조례는 승인을 받은 날 시행하며 공포한다.",
            "이 조례는 승인을 받은 날 시행하고 공포한다.",
            "이 조례는 공포한 날부터 시행하되, 2009년 10월 2일부터 적용한다.",
            "이 조례는 승인을 받은 날 施行한다.",
            "이 조례는 승인을 받은 날 施行된다.",
            "이 조례는 승인을 받은 날 시\n행한다.",
            "이 조례는 승인을 받은 날 시\n행된다.",
            "이 조례는 승인을 받은 날 시행\n한다.",
            "이 조례는 승인을 받은 날 시 행 한다.",
            "이 조례는 승인을 받은 날 施\n行한다.",
            "이 조례는 승인을 받은 날 施 行 한다.",
            "이 조례는 승인 후 施行",
        ];
        let nouns: &[&str] = &[
            "이 조례는 「지방자치법 시행령」에 따른다.",
            "이 조례는 시행령에 따른 사업을 정한다.",
            "이 조례는 시행규칙에 따른다.",
            "이 조례는 시\n행령에 따른다.",
            "이 조례는 시행일을 따로 정한다.",
            "이 조례는 시행 이후에 체결하는 관급공사부터 적용한다.",
            "이 조례는 시행당시 재직중인 사람에게 적용한다.",
            "이 조례는 시행정기구를 정한다.",
            "이 조례는 「익산시 시세 조례」의 시행에 따르는 절차와 그 밖에 그 시행에 필요한 사항을 규정함을 목적으로 한다.",
            "이 조례는 그 시행에 관하여 필요한 사항을 규정함을 목적으로 한다.",
            "이 조례는 施行令에 따른다.",
            "이 조례는 施行規則에 따른다.",
            "이 조례는 施 行 令에 따른다.",
            "이 조례는 施行 規則에 따른다.",
        ];
        for text in found {
            assert!(names_effective_date(text), "{text:?}");
            expect(
                &case("", text, "20200101", "제정", 7),
                "unresolved",
                None,
                text,
            );
        }
        for text in nouns {
            assert!(!names_effective_date(text), "{text:?}");
            expect(
                &case("", text, "20200101", "제정", 7),
                "confirmed",
                Some("2020-01-22"),
                text,
            );
        }
        // The arrival estimate of an order stays for a noun use ("시행에 필요한 양식").
        let mut order = case(
            "",
            "이 예규는 선박등기법 및 동법시행규칙의 시행에 필요한 양식을 정함으로써 업무통일에 이바지함을 목적으로 한다.",
            "20200101",
            "제정",
            7,
        );
        order.category = "훈령".to_owned();
        let found = resolve("ordinances", &order).unwrap();
        assert_eq!(
            (found.status.as_str(), found.enforcement_date.as_deref()),
            ("estimated", Some("2020-01-01"))
        );
    }

    #[test]
    fn r21_a_verb_far_from_the_subject_counts_until_the_sentence_ends() {
        // There is no length limit: the verb counts however far it is, until a full stop ends the
        // sentence.
        let far = format!("이 조례는{}시행하며 공포한다", "가".repeat(400));
        assert!(names_effective_date(&far));
        expect(
            &case("", &far, "20200101", "제정", 7),
            "unresolved",
            None,
            "far",
        );
        let ended = format!("이 조례는{}한다. 시행하며 공포한다", "가".repeat(400));
        assert!(!names_effective_date(&ended));
        expect(
            &case("", &ended, "20200101", "제정", 7),
            "confirmed",
            Some("2020-01-22"),
            "ended",
        );
    }

    #[test]
    fn r15g_item1_and_item2_cases() {
        // Item 1 control: broken publication, empty addendum, enactment with a body date.
        let mut source = case(
            "",
            "이 조례는 2030년 1월 1일부터 시행한다.",
            "00000101",
            "제정",
            7,
        );
        source.enforcement = "99991231".to_owned();
        expect(&source, "confirmed", Some("2030-01-01"), "item1 control");
        // Item 1: a valid publication and a body addendum without a label in an amendment.
        expect(
            &case(
                "",
                "부칙 이 조례는 2030년 1월 1일부터 시행한다.",
                "20200101",
                "일부개정",
                7,
            ),
            "unresolved",
            None,
            "item1 amendment",
        );
        // Item 2: a body label with only a number that matches decides on the promulgation day.
        expect(
            &case(
                "",
                "부칙 <조례 제7호> 이 조례는 공포한 날부터 시행한다.",
                "20200101",
                "일부개정",
                7,
            ),
            "confirmed",
            Some("2020-01-01"),
            "item2 number only",
        );
        expect(
            &case(
                "",
                "부칙 <조례 제8호> 이 조례는 공포한 날부터 시행한다.",
                "20200101",
                "일부개정",
                7,
            ),
            "unresolved",
            None,
            "item2 number mismatch",
        );
    }

    #[test]
    fn body_branch_keeps_old_result_when_the_publication_date_is_invalid() {
        // Control: pre_decide needs a valid publication date, so the old body rule decides.
        let body = "부칙 <2019.1.1> 이 조례는 2030년 1월 1일부터 시행한다.";
        for promulgation in ["20201301", "00000000"] {
            expect(
                &case("", body, promulgation, "제정", 7),
                "confirmed",
                Some("2030-01-01"),
                promulgation,
            );
        }
    }

    #[test]
    fn target_section_decides_in_every_group() {
        let p = "이 조례는 공포한 날부터 시행한다.";
        let c = |ad: &str, am: &str| case(ad, "", "20200101", am, 7);
        // Codex design 1 to 3: an unreadable label does not hide an explicit date or a last
        // section without a label.
        expect(
            &c(
                "부칙 <2019.13.1> 이 조례는 2030년 1월 1일부터 시행한다.",
                "제정",
            ),
            "confirmed",
            Some("2030-01-01"),
            "codex1a",
        );
        expect(
            &c(&format!("부칙 <2019.13.1> {p}\n부칙 {p}"), "제정"),
            "confirmed",
            Some("2020-01-01"),
            "codex1b B",
        );
        expect(
            &c(
                &format!(
                    "부칙 <2010.1.1> {p}\n부칙 <2019.13.1> 이 조례는 2030년 1월 1일부터 시행한다."
                ),
                "일부개정",
            ),
            "confirmed",
            Some("2030-01-01"),
            "last unreadable explicit",
        );
        expect(
            &c(
                &format!(
                    "부칙 <2010.1.1> {p}\n부칙 <2019.1.1> 이 조례는 2030년 1월 1일부터 시행한다."
                ),
                "일부개정",
            ),
            "confirmed",
            Some("2030-01-01"),
            "last <2019.1.1> explicit",
        );
        // Design 4 block 3: all sentences of the section must agree.
        expect(
            &c(
                "부칙 <2020.1.1>\n이 조례는 2030년 1월 1일부터 시행한다.\n이 조례는 2040년 1월 1일부터 시행한다.",
                "제정",
            ),
            "unresolved",
            None,
            "two dates",
        );
        expect(
            &c(
                "부칙 <2020.1.1>\n이 조례는 2040년 1월 1일부터 시행한다.\n이 조례는 2030년 1월 1일부터 시행한다.",
                "제정",
            ),
            "unresolved",
            None,
            "two dates reversed",
        );
        // Decisions 1 and 3: both-differ and one-side labels keep the explicit date.
        expect(
            &c(
                "부칙 <2021.1.1 조례 제12호> 이 조례는 2030년 1월 1일부터 시행한다.",
                "일부개정",
            ),
            "confirmed",
            Some("2030-01-01"),
            "both differ",
        );
        expect(
            &c(
                "부칙 <2020.1.1 조례 제12호> 이 조례는 2030년 1월 1일부터 시행한다.",
                "일부개정",
            ),
            "confirmed",
            Some("2030-01-01"),
            "one side",
        );
        // Decision 2: the attributed earlier section keeps its day of promulgation.
        expect(
            &c(
                &format!(
                    "부칙 <2020.1.1> {p}\n부칙 <2025.1.1 조례 제99호> 이 조례는 2030년 1월 1일부터 시행한다."
                ),
                "일부개정",
            ),
            "confirmed",
            Some("2020-01-01"),
            "dec2",
        );
        // Explicit date only in the attributed earlier section stays unresolved.
        expect(
            &c(
                &format!(
                    "부칙 <2020.1.1> 이 조례는 2020년 1월 1일부터 시행한다.\n부칙 <2025.1.1 조례 제99호> {p}"
                ),
                "제정",
            ),
            "unresolved",
            None,
            "NotLast",
        );
        // Rule 7: an enactment with an attributed last section.
        expect(
            &c(
                &format!(
                    "부칙 <2010.1.1> 이 조례는 2005년 1월 1일부터 시행한다.\n부칙 <2020.1.1> {p}"
                ),
                "전부개정",
            ),
            "confirmed",
            Some("2020-01-01"),
            "rule7",
        );
        expect(
            &c(
                &format!("부칙 <2010.1.1> {p}\n부칙 <2015.1.1> {p}"),
                "전부개정",
            ),
            "unresolved",
            None,
            "no attribution, labelled last",
        );
        expect(
            &c(
                &format!("부칙 <2010.1.1> {p}\n부칙 이 조례는 2030년 1월 1일부터 시행한다."),
                "제정",
            ),
            "confirmed",
            Some("2030-01-01"),
            "R4 explicit last unlabelled",
        );
        // Proviso forms (decision 4) and Design 6 block 4.
        expect(
            &c(
                &format!("부칙 <2020.1.1> {p} <단서생략> 다만, 별도로 정한다."),
                "제정",
            ),
            "unresolved",
            None,
            "omit mark then proviso",
        );
        expect(
            &c(
                &format!("부칙 <2020.1.1> {p} (단서생략) 다만, 별도로 정한다."),
                "제정",
            ),
            "unresolved",
            None,
            "paren omit mark then proviso",
        );
        expect(
            &c(&format!("부칙 <2020.1.1> {p} <단서생략>"), "제정"),
            "confirmed",
            Some("2020-01-01"),
            "omit mark alone",
        );
        expect(
            &c(
                &format!("부칙 <2020.1.1> {p} (단, 제4조는 따로 정한다.)"),
                "제정",
            ),
            "unresolved",
            None,
            "paren proviso",
        );
        expect(
            &c(
                &format!("부칙 <2020.1.1> {p} 단，제4조는 따로 정한다."),
                "제정",
            ),
            "unresolved",
            None,
            "full width comma",
        );
        expect(
            &c(
                "부칙 <2020.1.1> 이 조례는 2030년 1월 1일부터 시행한다. 다만, 제4조는 따로 정한다.",
                "일부개정",
            ),
            "unresolved",
            None,
            "explicit then proviso",
        );
        // Decision A with the single label rule (design 4 block 1).
        let a = case(
            "부칙 (2010.7.1) 이 조례는 공포한 날 시행한다.",
            "",
            "20100601",
            "일부개정",
            7,
        );
        expect(&a, "confirmed", Some("2010-07-01"), "A with later label");
        let a = case(
            "부칙 (2010.5.1) 이 조례는 공포한 날 시행한다.",
            "",
            "20100601",
            "일부개정",
            7,
        );
        expect(&a, "confirmed", Some("2010-06-01"), "A with earlier label");
        // Rule 11: the spaced verb.
        expect(
            &c("부칙 이 조례는 공포한 날부터 시행 한다.", "제정"),
            "confirmed",
            Some("2020-01-01"),
            "spaced verb",
        );
        // Design 1 (r15b): an unconditional and a conditional sentence without "부터".
        expect(
            &case(
                "",
                "이 조례는 의회의 승인을 받은 날 시행한다.",
                "20200101",
                "제정",
                7,
            ),
            "unresolved",
            None,
            "condition without buteo",
        );
        expect(
            &case(
                "",
                "이 조례는 2030년 1월 1일 시행한다.",
                "20200101",
                "제정",
                7,
            ),
            "confirmed",
            Some("2030-01-01"),
            "date without buteo",
        );
        expect(
            &case(
                "",
                "이 조례는 2030년 1월 1일부터\n시행한다.",
                "20200101",
                "제정",
                7,
            ),
            "unresolved",
            None,
            "line break",
        );
        expect(
            &case("", "2030년 1월 1일부터 시행한다.", "20200101", "제정", 7),
            "unresolved",
            None,
            "no subject stays unresolved, not 21 days",
        );
        expect(
            &case("", "제1조(목적) 본문이다.", "20200101", "제정", 7),
            "confirmed",
            Some("2020-01-22"),
            "default period control",
        );
    }

    #[test]
    fn addendum_text_in_the_articles_follows_the_addendum_rules() {
        let p = "이 조례는 공포한 날부터 시행한다.";
        let b = |body: &str, am: &str, pub_: &str, num: u64| case("", body, pub_, am, num);
        // F2: a label-less addendum of an amendment inside the articles is not read.
        expect(
            &b(
                "제1조 목적\n부칙 이 조례는 1995년 1월 1일부터 시행한다.",
                "일부개정",
                "20050101",
                7,
            ),
            "unresolved",
            None,
            "T11",
        );
        // 924410: the label number differs, so the addendum belongs to another amendment.
        expect(
            &b(
                "부 칙(규칙 제159호) 이 규칙은 공포한 날부터 시행한다.",
                "일부개정",
                "20050715",
                1108,
            ),
            "unresolved",
            None,
            "number only mismatch",
        );
        // Design 5 block 1: a cumulative addendum inside the articles.
        expect(
            &b(
                &format!(
                    "부칙 <2020.1.1> {p}\n부칙 <2025.1.1 조례 제99호> 이 조례는 2030년 1월 1일부터 시행한다."
                ),
                "일부개정",
                "20200101",
                7,
            ),
            "confirmed",
            Some("2020-01-01"),
            "attributed earlier",
        );
        expect(
            &b(
                &format!("부칙 <2010.1.1> {p}\n부칙 {p}"),
                "일부개정",
                "20200101",
                7,
            ),
            "unresolved",
            None,
            "last unlabelled, amendment",
        );
        expect(
            &b(
                &format!("부칙 <2010.1.1> {p}\n부칙 {p}"),
                "제정",
                "20200101",
                7,
            ),
            "confirmed",
            Some("2020-01-01"),
            "last unlabelled, enactment",
        );
        expect(
            &b(&format!("부칙 {p}"), "제정", "20200101", 7),
            "confirmed",
            Some("2020-01-01"),
            "label-less enactment",
        );
        expect(
            &b(
                "제1조 목적\n부 칙\n이 조례는 공포한 날부터 시행한다.\n부 칙(개정 1998. 9. 21. 조례 제430호)\n",
                "일부개정",
                "19980921",
                430,
            ),
            "confirmed",
            Some("1998-09-21"),
            "heading without its own sentence",
        );
        // Design 6 block 2: a number that alone matches confirms the day of promulgation.
        expect(
            &b(&format!("부칙 <조례 제7호> {p}"), "일부개정", "20200101", 7),
            "confirmed",
            Some("2020-01-01"),
            "number alone matches",
        );
        // Design 6 block 1: without a promulgation date the older whole-body branch still runs.
        let mut undated = case(
            "",
            "이 조례는 2030년 1월 1일부터 시행한다.",
            "00000101",
            "제정",
            7,
        );
        undated.enforcement = "99991231".to_owned();
        expect(
            &undated,
            "confirmed",
            Some("2030-01-01"),
            "no publication, explicit date",
        );
        let found = resolve("ordinances", &undated).unwrap();
        assert_eq!(found.enforcement_date.as_deref(), Some("2030-01-01"));
        assert_eq!(found.status, "confirmed");
    }

    #[test]
    fn a_label_date_in_the_articles_obeys_the_next_revision_cap_and_keeps_its_rules() {
        let p = "이 조례는 공포한 날부터 시행한다.";
        let build = |next: Option<&str>| {
            let mut sources = Vec::new();
            let mut first = case(
                "",
                &format!("부칙 <2020.7.1> {p}"),
                "20200101",
                "일부개정",
                7,
            );
            first.revision = "1".to_owned();
            first.serial = 1;
            first.markdown = b"---\nname: 1\n---\nbody\n".to_vec();
            sources.push(first);
            if let Some(next) = next {
                let mut second = case("", "제1조 본문", next, "일부개정", 8);
                second.revision = "2".to_owned();
                second.serial = 2;
                second.markdown = b"---\nname: 2\n---\nbody\n".to_vec();
                sources.push(second);
            }
            DatePlan::new("ordinances", true, sources).unwrap()
        };
        // The cap: the next revision is promulgated before the label date.
        let capped = build(Some("20200601"));
        let found = &capped.revisions[0].occurrences[0];
        assert_eq!(found.status, "unresolved");
        assert_eq!(
            found.date_evidence["decision"],
            "label-date-after-next-revision"
        );
        // The rules that applied stay on record next to the cap.
        assert_eq!(
            found.date_evidence["lead_rulings"],
            json!([BODY_SECTIONS_RULING])
        );
        // No next revision: the label date stands.
        let open = build(None);
        let found = &open.revisions[0].occurrences[0];
        assert_eq!(
            (found.status.as_str(), found.enforcement_date.as_deref()),
            ("confirmed", Some("2020-07-01"))
        );
        assert_eq!(
            found.date_evidence["decision"],
            "single-label-date-priority-20261103"
        );
        assert_eq!(found.date_evidence["applied_side"], "label");
    }

    #[test]
    fn the_shared_label_function_gives_the_addendum_path_its_old_result() {
        // The unit label of an addendum without a heading carries the date as written (here
        // 20100701), and the addendum path must read it as before.
        let mut source = revision(
            "조례",
            "일부개정",
            "20100601",
            7,
            "이 조례는 공포한 날부터 시행한다.\n",
        );
        source.xml = "<LawService><공포번호>7</공포번호><부칙><부칙단위><부칙공포일자>20100701</부칙공포일자><부칙내용><![CDATA[이 조례는 공포한 날부터 시행한다.\n]]></부칙내용></부칙단위></부칙></LawService>"
            .as_bytes()
            .to_vec();
        let found = resolve("ordinances", &source).unwrap();
        assert_eq!(found.enforcement_date.as_deref(), Some("2010-07-01"));
        assert_eq!(found.date_evidence["applied_side"], "label");
        assert_eq!(
            found.date_evidence["decision"],
            "single-label-date-priority-20261103"
        );
    }

    #[test]
    fn a_date_written_with_dots_is_read_like_the_korean_form() {
        for (written, expected) in [
            ("1994. 7. 1.부터", "1994-07-01"),
            ("1994.7.1부터", "1994-07-01"),
            ("1994년 7월 1일부터", "1994-07-01"),
        ] {
            let text = format!("부 칙\n(시행일) 이 예규는 {written} 시행한다.");
            let mut source = revision("행정예규", "제정", "19930101", 1, "");
            source.xml = format!(
                "<AdmRulService><부칙><부칙내용 ><![CDATA[{text}]]></부칙내용></부칙></AdmRulService>"
            )
            .into_bytes();
            let found = resolve("admrules", &source).unwrap();
            assert_eq!(found.status, "confirmed", "{written}");
            assert_eq!(
                found.enforcement_date.as_deref(),
                Some(expected),
                "{written}"
            );
        }
        // Dots that do not form a date are still not read.
        let mut source = revision("행정예규", "제정", "19930101", 1, "");
        source.xml = "<AdmRulService><부칙><부칙내용 ><![CDATA[이 예규는 1994. 13. 1.부터 시행한다.]]></부칙내용></부칙></AdmRulService>"
            .as_bytes()
            .to_vec();
        assert_eq!(resolve("admrules", &source).unwrap().status, "unresolved");
    }

    #[test]
    fn single_addendum_unit_label_is_the_label_date_and_cumulative_units_stay_unresolved() {
        let unit = |label: &str| {
            format!(
                "<부칙단위><부칙공포일자>{label}</부칙공포일자><부칙내용><![CDATA[제1조(시행일) 이 조례는 공포한 날부터 시행한다.]]></부칙내용></부칙단위>"
            )
        };
        let source_with = |units: &str| {
            let mut source = revision("조례", "일부개정", "20100601", 2556, "");
            source.xml =
                format!("<LawService><공포번호>2556</공포번호><부칙>{units}</부칙></LawService>")
                    .into_bytes();
            source
        };
        let (status, date, evidence) = outcome("ordinances", &source_with(&unit("20100701")));
        assert_eq!(
            (status.as_str(), date.as_deref()),
            ("confirmed", Some("2010-07-01"))
        );
        assert_eq!(evidence["applied_side"], "label");
        // Several units whose labels all precede the promulgation date were once given the
        // publication plus 21 days; they are unresolved now.
        let two = format!("{}{}", unit("20040101"), unit("20050101"));
        assert_eq!(outcome("ordinances", &source_with(&two)).0, "unresolved");
    }

    #[test]
    fn individual_approvals_apply_only_to_their_revision_date_and_sentence() {
        let cases = [
            (
                "1000238",
                "20071031",
                "이 조례는 공포 후 1개월이 경과한 날부터 시행한다.",
                "조례",
                "2007-12-01",
                "issue comment 6437632923",
            ),
            (
                "1011016",
                "20110218",
                "이 규칙은 2011년 1월 1일부터 시행한다.",
                "규칙",
                "2011-01-01",
                "issue body",
            ),
        ];
        for (mst, promulgation, clause, category, expected, origin) in cases {
            let text = format!(
                "부칙 <2000. 1. 1> 이 조례는 공포한 날부터 시행한다.\n부칙 <제77호, {promulgation}> {clause}\n"
            );
            let mut source = revision(category, "일부개정", promulgation, 1, &text);
            source.revision = mst.to_owned();
            let (status, date, evidence) = outcome("ordinances", &source);
            assert_eq!(
                (status.as_str(), date.as_deref()),
                ("confirmed", Some(expected)),
                "{mst}"
            );
            assert!(
                evidence["approval_source"]
                    .as_str()
                    .unwrap()
                    .starts_with(origin),
                "{mst}"
            );
            let mut other = source.clone();
            other.revision = "9999999".to_owned();
            // Another revision does not get the individual approval. The written date may still
            // stand by the user decision on a label that matches on one side only.
            assert_ne!(
                resolve("ordinances", &other).unwrap().reason,
                "individually approved revision-specific decision",
                "{mst} other revision"
            );
            let mut changed_date = revision(category, "일부개정", "20990101", 1, &text);
            changed_date.revision = mst.to_owned();
            assert_ne!(
                resolve("ordinances", &changed_date).unwrap().reason,
                "individually approved revision-specific decision",
                "{mst} changed date"
            );
            let mut changed_text = revision(
                category,
                "일부개정",
                promulgation,
                1,
                &text.replace("시행한다", "시행하지 않는다"),
            );
            changed_text.revision = mst.to_owned();
            assert_eq!(
                outcome("ordinances", &changed_text).0,
                "unresolved",
                "{mst} changed sentence"
            );
        }
    }

    /// Creates a repository with a main commit and an unreachable side commit.
    fn git_fixture() -> (tempfile::TempDir, std::path::PathBuf, String, String) {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo.git");
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .arg("--git-dir")
                .arg(&repo)
                .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        };
        let init = Command::new("git")
            .args(["init", "--bare", "-q", "-b", "main"])
            .arg(&repo)
            .output()
            .unwrap();
        assert!(init.status.success());
        let blob = |text: &str| {
            let file = temp.path().join("blob.txt");
            fs::write(&file, text).unwrap();
            run(&["hash-object", "-w", file.to_str().unwrap()])
        };
        let commit = |name: &str, text: &str, parent: Option<&str>, branch: &str| {
            let blob = blob(text);
            let tree = {
                let mut tree = Command::new("git")
                    .arg("--git-dir")
                    .arg(&repo)
                    .arg("mktree")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap();
                tree.stdin
                    .take()
                    .unwrap()
                    .write_all(format!("100644 blob {blob}\t{name}\n").as_bytes())
                    .unwrap();
                String::from_utf8(tree.wait_with_output().unwrap().stdout)
                    .unwrap()
                    .trim()
                    .to_owned()
            };
            let mut args = vec!["commit-tree", tree.as_str(), "-m", "m"];
            if let Some(parent) = parent {
                args.extend(["-p", parent]);
            }
            let commit = run(&args);
            run(&["update-ref", branch, &commit]);
            commit
        };
        let main = commit("a.md", "alpha\n", None, "refs/heads/main");
        let side = commit("b.md", "beta\n", None, "refs/heads/side");
        run(&["symbolic-ref", "HEAD", "refs/heads/main"]);
        run(&["update-ref", "-d", "refs/heads/side"]);
        (temp, repo, main, side)
    }

    fn reference(commit: &str, path: &str, text: &str, blob: &str) -> Value {
        json!({"kind":"git", "commit_sha":commit, "path":path, "blob_sha":blob, "sha256":sha256(text.as_bytes())})
    }

    #[test]
    fn git_body_verification_accepts_good_references_and_names_each_failure() {
        let (_temp, repo, main, side) = git_fixture();
        let head = main.clone();
        let blob = String::from_utf8(git(&repo, &["rev-parse", &format!("{main}:a.md")]).unwrap())
            .unwrap();
        let blob = blob.trim();
        let good = reference(&main, "a.md", "alpha\n", blob);
        verify_git_bodies(&repo, &head, &[&good, &good]).unwrap();
        let cases = [
            (
                reference(&main, "a.md", "alpha\n", &"0".repeat(40)),
                "body blob mismatch",
            ),
            (
                reference(&main, "a.md", "other\n", blob),
                "body hash mismatch",
            ),
            (
                reference(&side, "b.md", "beta\n", blob),
                "unreachable body commit",
            ),
            (
                reference(&main, "missing.md", "alpha\n", blob),
                "Git body validation failed",
            ),
        ];
        for (bad, message) in &cases {
            let error = verify_git_bodies(&repo, &head, &[&good, bad]).unwrap_err();
            assert!(error.to_string().contains(message), "{message}: {error}");
        }
    }

    /// Builds a small enforcement plan: id1 has a confirmed and an unresolved revision,
    /// id2 has an unresolved revision, and id3 is undated.
    fn small_plan() -> DatePlan {
        let text = "부칙 <2020. 1. 1 조례 제1호> 이 조례는 공포한 날부터 시행한다.\n";
        let mut sources = Vec::new();
        for (id, identity, promulgation) in [
            ("1", "id1", "20200101"),
            ("2", "id1", "20200301"),
            ("3", "id2", "20200301"),
            ("4", "id3", "bad"),
        ] {
            let mut source = revision("조례", "일부개정", promulgation, 1, text);
            source.revision = id.to_owned();
            source.identity = identity.to_owned();
            source.canonical_path = format!("p{identity}.md");
            source.source_path = format!("{id}.xml");
            source.serial = id.parse().unwrap();
            source.markdown = format!("---\nname: {id}\n---\nbody {id}\n").into_bytes();
            sources.push(source);
        }
        DatePlan::new("ordinances", true, sources).unwrap()
    }

    #[test]
    fn identity_files_are_canonical_pretty_json_and_stable_across_cache_use() {
        let mut plan = small_plan();
        let body = plan.replay_body(0).unwrap();
        let path = plan.sources[0].canonical_path.clone();
        plan.record_body(&path, &body, [1; 20]);
        let revisions: Vec<usize> = plan
            .revisions
            .iter()
            .enumerate()
            .filter(|(_, revision)| revision.identity == "id1")
            .map(|(index, _)| index)
            .collect();
        let mut warm = SnapshotCache::new(plan.revisions.len());
        for (date, expected) in [
            (None, 0),
            (Some("2019-12-31"), 0),
            (Some("2020-01-01"), 1),
            (Some("2020-03-01"), 2),
            (Some("2020-03-01"), 2),
        ] {
            let bytes = plan
                .identity_file_bytes("id1", &revisions, date, &mut warm)
                .unwrap();
            let fresh = plan
                .identity_file_bytes(
                    "id1",
                    &revisions,
                    date,
                    &mut SnapshotCache::new(plan.revisions.len()),
                )
                .unwrap();
            assert_eq!(bytes, fresh, "{date:?}");
            let value: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(
                serde_json::to_vec_pretty(&value).unwrap(),
                bytes,
                "{date:?}"
            );
            assert_eq!(
                value["revisions"].as_array().unwrap().len(),
                expected,
                "{date:?}"
            );
            assert_eq!(value["snapshot_date"], json!(date));
            assert_eq!(value["identity"], "id1");
        }
        assert!(
            warm.fragments.iter().flatten().count() >= 1,
            "a Git body fragment is kept"
        );
    }

    #[test]
    fn tree_paths_shard_by_identity_hash_and_shorten_long_names() {
        let hash = sha256(b"2146631");
        assert_eq!(
            identity_index_path("ordinances", "2146631"),
            format!(
                ".legalize/date-index/ordinances/{}/{}/{}.json",
                &hash[..2],
                &hash[2..4],
                hex_text("2146631")
            )
        );
        assert_eq!(hex_text("a한"), "61ed959c");
        let long = "x".repeat(97);
        assert_eq!(identity_name(&long), hex_text(&long));
        assert!(identity_name(&long).len() + 5 <= 200);
        let longer = "x".repeat(98);
        assert_eq!(
            identity_name(&longer),
            format!("sha256-{}", sha256(longer.as_bytes()))
        );
        let plan = small_plan();
        let candidate = plan.candidate_path(0);
        let id1 = sha256(b"id1");
        assert!(
            candidate.starts_with(&format!(
                ".legalize/candidates/ordinances/{}/{}/{}/",
                &id1[..2],
                &id1[2..4],
                hex_text("id1")
            )),
            "{candidate}"
        );
        assert!(candidate.ends_with(&format!(
            "/{}/{}.md",
            hex_text("1"),
            plan.revisions[0].copy_id
        )));
    }

    #[test]
    fn replay_writes_only_changed_identity_files_per_date() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("out.git");
        let mut plan = small_plan();
        let (head, staged) = plan.replay(&output, b"readme").unwrap();
        let git_text = |args: &[&str]| {
            String::from_utf8(git(&staged.owned, args).unwrap())
                .unwrap()
                .trim()
                .to_owned()
        };
        assert_eq!(git_text(&["rev-parse", "HEAD"]), head);
        let files = |commit: &str| -> Vec<String> {
            git_text(&["ls-tree", "-r", "--name-only", commit])
                .lines()
                .filter(|path| path.starts_with(".legalize/date-index/"))
                .map(str::to_owned)
                .collect()
        };
        let path = |identity: &str| identity_index_path("ordinances", identity);
        let subjects = git_text(&["log", "--reverse", "--format=%H %s"]);
        let commit_of = |subject: &str| -> String {
            subjects
                .lines()
                .find(|line| line.ends_with(subject))
                .unwrap_or_else(|| panic!("{subject} in {subjects}"))
                .split(' ')
                .next()
                .unwrap()
                .to_owned()
        };
        let start = commit_of("Preserve undated source references");
        assert_eq!(files(&start), vec![path("id3")]);
        let first = commit_of("Record date snapshot 2020-01-01");
        let mut expected = vec![path("id1"), path("id3")];
        expected.sort();
        assert_eq!(files(&first), expected);
        let second = commit_of("Record date snapshot 2020-03-01");
        assert_eq!(files(&second).len(), 3);
        let undated: Value =
            serde_json::from_str(&git_text(&["show", &format!("{start}:{}", path("id3"))]))
                .unwrap();
        assert_eq!(undated["snapshot_date"], Value::Null);
        assert_eq!(undated["revisions"].as_array().unwrap().len(), 1);
        let changed = git_text(&[
            "diff-tree",
            "-r",
            "--name-only",
            "--no-commit-id",
            &first,
            &second,
        ]);
        let mut changed: Vec<_> = changed
            .lines()
            .filter(|path| path.starts_with(".legalize/date-index/"))
            .collect();
        changed.sort();
        let mut want = vec![path("id1"), path("id2")];
        want.sort();
        assert_eq!(
            changed, want,
            "id3 is not rewritten on a date without its event"
        );
        assert_eq!(plan.snapshots.len(), 2);
    }
}

/// Tests of the N representation and the evidence keys. The expected values come from
/// `sourceview-oracle-20261113/expected-v4.json`, r25c-normalization-plan.v3.md 6 and
/// r25c-collection-contract.md 3.1; no expected value is taken from the output of this code.
#[cfg(test)]
mod n_evidence_tests {
    use super::*;

    const ZW: &str = "\u{200B}";

    fn one_piece(text: &str) -> SourceSegments {
        let mut view = SourceSegments::default();
        view.append(text, Some(1), 0, 0, "CData", false);
        view
    }

    fn evidence(text: &str, after_heading: bool) -> PartitionEvidence {
        let raw = one_piece(text);
        container_evidence_with(
            "test",
            "t".to_owned(),
            &raw,
            &decode_markup(text),
            text,
            after_heading,
            &[],
        )
    }

    fn span(ranges: &[SourceRange]) -> [usize; 2] {
        [ranges[0].2, ranges[ranges.len() - 1].3]
    }

    fn view<'a>(evidence: &'a PartitionEvidence, name: &str) -> &'a BTreeMap<EvidenceKey, Value> {
        &evidence
            .views
            .iter()
            .find(|(view, _)| view == name)
            .expect("view")
            .1
    }

    /// (kind, subject or candidate span, verb span, verb text) of each key of a map.
    fn rows(
        map: &BTreeMap<EvidenceKey, Value>,
    ) -> Vec<(&'static str, [usize; 2], [usize; 2], String)> {
        map.iter()
            .map(|(key, found)| {
                let verb_text = found["verb"].as_str().unwrap_or("").to_owned();
                match key {
                    EvidenceKey::Subject { subject, verb } => {
                        ("subject", span(subject), span(verb), verb_text)
                    }
                    EvidenceKey::Extended { subject, verb } => {
                        ("extended", span(subject), span(verb), verb_text)
                    }
                    EvidenceKey::Heading { candidate, verb } => {
                        ("heading", [candidate.2, candidate.2], span(verb), verb_text)
                    }
                    EvidenceKey::Phrase { phrase } => {
                        ("phrase", span(phrase), span(phrase), verb_text)
                    }
                }
            })
            .collect()
    }

    fn new_rows(
        evidence: &PartitionEvidence,
    ) -> Vec<(&'static str, [usize; 2], [usize; 2], String)> {
        let map: BTreeMap<EvidenceKey, Value> = evidence
            .all
            .iter()
            .filter(|(key, _)| !evidence.r.contains(*key))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        rows(&map)
    }

    fn all_rows(
        evidence: &PartitionEvidence,
    ) -> Vec<(&'static str, [usize; 2], [usize; 2], String)> {
        rows(&evidence.all)
    }

    fn clean(evidence: &PartitionEvidence) {
        assert!(evidence.errors.is_empty(), "errors: {:?}", evidence.errors);
    }

    #[test]
    fn expected_v4_a_same_count_replacement() {
        let e = evidence(
            &format!("이 조례는 施行{ZW}令이다. 이 규칙{ZW}은 시행한다."),
            false,
        );
        clean(&e);
        let b = rows(view(&e, "B-decoded"));
        assert_eq!(
            b.iter()
                .filter(|r| r.0 == "subject")
                .map(|r| r.1)
                .collect::<Vec<_>>(),
            vec![[0, 13]],
            "B_subjects"
        );
        assert_eq!(
            b.iter()
                .filter(|r| r.0 == "subject")
                .map(|r| r.2)
                .collect::<Vec<_>>(),
            vec![[14, 20]],
            "B_hanja"
        );
        assert!(
            !rows(view(&e, "N-decoded")).iter().any(|r| r.2 == [14, 20]),
            "N_first_hanja_excluded"
        );
        assert!(
            new_rows(&e)
                .iter()
                .any(|r| r.0 == "subject" && r.1[0] == 34),
            "N_second_subject_new"
        );
        assert!(
            all_rows(&e)
                .iter()
                .any(|r| r.0 == "subject" && r.1 == [0, 13] && r.2 == [14, 20]),
            "B evidence preserved"
        );
        assert!(e.added());
    }

    #[test]
    fn expected_v4_b_same_subject_other_verb() {
        let e = evidence(&format!("이 조례는 시행하고 시{ZW}행한다."), false);
        clean(&e);
        let b = rows(view(&e, "B-decoded"));
        assert_eq!(
            b.iter().map(|r| (r.0, r.1, r.2)).collect::<Vec<_>>(),
            vec![("subject", [0, 13], [14, 23])]
        );
        let new = new_rows(&e);
        assert!(
            new.iter()
                .any(|r| r.0 == "subject" && r.1 == [0, 13] && r.3 == "시행한"),
            "N_new_verb_text: {new:?}"
        );
    }

    #[test]
    fn expected_v4_c_sentence_end_only() {
        let e = evidence(&format!("이 조례는 시행한다{ZW}. 운영을 정한다."), false);
        clean(&e);
        for name in ["B-decoded", "N-decoded"] {
            assert!(
                rows(view(&e, name))
                    .iter()
                    .any(|r| r.0 == "subject" && r.1 == [0, 13] && r.2 == [14, 23]),
                "{name}"
            );
        }
        assert_eq!(
            new_rows(&e).iter().filter(|r| r.0 == "subject").count(),
            0,
            "new_subject_key_count"
        );
        assert!(!e.added());
    }

    #[test]
    fn expected_v4_d_same_start_other_subject_end() {
        let e = evidence(&format!("이 조례{ZW}는 조례는 시행한다."), false);
        clean(&e);
        assert!(
            rows(view(&e, "B-decoded"))
                .iter()
                .any(|r| r.0 == "subject" && r.1 == [0, 26] && r.2 == [27, 36]),
            "B_subject"
        );
        assert!(
            rows(view(&e, "N-decoded"))
                .iter()
                .any(|r| r.0 == "subject" && r.1 == [0, 16] && r.2 == [27, 36]),
            "N_subject_source"
        );
        assert_eq!(
            new_rows(&e).iter().filter(|r| r.0 == "subject").count(),
            1,
            "N_new_subject_key_count"
        );
    }

    #[test]
    fn expected_v4_heading_c() {
        let e = evidence(&format!("시행한다{ZW}. 운영을 정한다."), true);
        clean(&e);
        for name in ["B-decoded", "N-decoded"] {
            assert!(
                rows(view(&e, name))
                    .iter()
                    .any(|r| r.0 == "heading" && r.1[0] == 0 && r.2 == [0, 9]),
                "{name}"
            );
        }
        assert_eq!(
            new_rows(&e).iter().filter(|r| r.0 == "heading").count(),
            0,
            "new_heading_key_count"
        );
    }

    #[test]
    fn expected_v4_heading_two() {
        let e = evidence(
            &format!("부{ZW}칙 안내\n부칙 안내\n(이 조례는 시행한다.) 시행한다."),
            false,
        );
        clean(&e);
        let b: Vec<_> = rows(view(&e, "B-decoded"))
            .into_iter()
            .filter(|r| r.0 == "heading")
            .collect();
        assert_eq!(
            b.iter().map(|r| (r.1[0], r.2)).collect::<Vec<_>>(),
            vec![(24, [61, 70])],
            "old heading"
        );
        let n: Vec<_> = rows(view(&e, "N-decoded"))
            .into_iter()
            .filter(|r| r.0 == "heading")
            .collect();
        assert_eq!(
            n.iter().map(|r| (r.1[0], r.2)).collect::<Vec<_>>(),
            vec![(10, [61, 70]), (24, [61, 70])],
            "N headings"
        );
        let new: Vec<_> = new_rows(&e)
            .into_iter()
            .filter(|r| r.0 == "heading")
            .collect();
        assert_eq!(
            new.iter().map(|r| (r.1[0], r.2)).collect::<Vec<_>>(),
            vec![(10, [61, 70])],
            "new_key_count 1"
        );
        assert!(
            all_rows(&e)
                .iter()
                .any(|r| r.0 == "heading" && r.1[0] == 24 && r.2 == [61, 70]),
            "old_key_retained"
        );
    }

    #[test]
    fn expected_v4_f_heading_residual() {
        let e = evidence(&format!("시행 이 조례{ZW}는 시행한다."), true);
        clean(&e);
        assert!(
            rows(view(&e, "B-decoded"))
                .iter()
                .any(|r| r.0 == "heading" && r.1[0] == 0 && r.2 == [24, 33]),
            "B_heading_verb_source"
        );
        assert!(
            new_rows(&e)
                .iter()
                .any(|r| r.0 == "heading" && r.1[0] == 0 && r.2 == [0, 7]),
            "N_new_heading_verb_source"
        );
        assert!(
            all_rows(&e)
                .iter()
                .any(|r| r.0 == "heading" && r.2 == [24, 33]),
            "B_preserved"
        );
    }

    #[test]
    fn expected_v4_overlap() {
        let e = evidence(&format!("이 조례는 부터 효력{ZW}을 발생한다."), false);
        clean(&e);
        let b = rows(view(&e, "B-decoded"));
        assert_eq!(
            b.iter()
                .filter(|r| r.0 == "subject")
                .map(|r| r.3.as_str())
                .collect::<Vec<_>>(),
            vec!["부터 효력"],
            "B_phrase_text"
        );
        assert!(
            new_rows(&e)
                .iter()
                .any(|r| r.0 == "subject" && r.3 == "효력을 발생"),
            "N_overlapping_new_phrase_text"
        );
        let phrase_new: Vec<String> = e
            .phrase_n
            .iter()
            .filter(|(k, _)| !e.phrase_b.contains_key(*k))
            .map(|(_, v)| v["verb"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(phrase_new, vec!["효력을 발생".to_owned()], "PHRASE N - B");
        assert!(
            all_rows(&e).iter().any(|r| r.3 == "부터 효력"),
            "B_preserved"
        );
    }

    #[test]
    fn expected_v4_mixed_and_leading_cf() {
        let e = evidence("이 조례는 施行하고 시행한다.", false);
        clean(&e);
        let mut verbs: Vec<String> = rows(view(&e, "B-decoded"))
            .into_iter()
            .filter(|r| r.0 == "subject")
            .map(|r| r.3)
            .collect();
        verbs.sort();
        assert_eq!(
            verbs,
            vec!["施行".to_owned(), "시행한".to_owned()],
            "successes"
        );
        let e = evidence(&format!("{ZW}이 조례는 시행한다."), false);
        clean(&e);
        for name in ["B-decoded", "N-decoded"] {
            assert!(
                rows(view(&e, name))
                    .iter()
                    .all(|r| r.0 != "subject" || r.1[0] == 3),
                "{name} subject_source_start"
            );
        }
        assert!(!e.added(), "outside_deleted_cf_not_included");
    }

    #[test]
    fn expected_v4_entities() {
        assert_eq!(one_piece("&amp;lt;").decoded().unwrap().text, "&lt;");
        assert_eq!(one_piece("&lt;&gt;&amp;").decoded().unwrap().text, "<>&");
        let raw = one_piece(&format!("&l{ZW}t;"));
        assert_eq!(
            raw.decoded().unwrap().text,
            format!("&l{ZW}t;"),
            "B_decoded"
        );
        let n = normalized_raw(&raw).unwrap().decoded().unwrap();
        assert_eq!(n.text, "<", "N_decoded");
        let units = view_units(&n).unwrap();
        assert_eq!(
            canonical_ranges(&units[0].sources, &piece_texts(&raw)),
            vec![(1, 0, 0, 7)],
            "decoded_source_range"
        );
    }

    fn body_of(xml: &str) -> (DateText, SourceView) {
        let mut found = date_text(xml.as_bytes()).unwrap();
        let view = found.source_view.take().unwrap();
        (found, view)
    }

    fn run_ids(view: &SourceSegments) -> Vec<u64> {
        view.segments
            .iter()
            .filter_map(|s| s["source"]["run_id"].as_u64())
            .collect()
    }

    #[test]
    fn expected_v4_runs() {
        for (name, between) in [
            ("comment", "<!--x-->"),
            ("pi", "<?pi x?>"),
            ("empty", "<x/>"),
            ("adjacent", ""),
        ] {
            let xml = format!(
                "<LawService><본문><![CDATA[이 조례]]>{between}<![CDATA[는 시행한다.]]></본문></LawService>"
            );
            let (found, view) = body_of(&xml);
            assert_eq!(found.body, "이 조례\n는 시행한다.\n", "{name} B");
            assert_eq!(view.body.text, found.body, "{name} SourceView B");
            let ids = run_ids(&view.body);
            let same_run = ids[0] == ids[1];
            assert_eq!(same_run, name == "adjacent", "{name} same_run");
            let n = normalized_raw(&view.body).unwrap().text;
            let expected = if name == "adjacent" {
                "이 조례는 시행한다.\n"
            } else {
                "이 조례\n는 시행한다.\n"
            };
            assert_eq!(n, expected, "{name} N");
        }
        let (found, view) =
            body_of("<LawService><본문><![CDATA[이 조례\n는 시행한다.]]></본문></LawService>");
        assert_eq!(found.body, "이 조례\n는 시행한다.\n", "real_lf_R B");
        assert_eq!(
            normalized_raw(&view.body).unwrap().text,
            "이 조례\n는 시행한다.\n",
            "real_lf_R N"
        );
        let (found, view) = body_of(
            "<LawService><조문><조내용>이 조례는</조내용><조내용>시행한다.</조내용></조문></LawService>",
        );
        assert_eq!(found.body, "이 조례는\n시행한다.\n", "two_element_body B");
        assert_eq!(
            normalized_raw(&view.body).unwrap().text,
            "이 조례는\n시행한다.\n",
            "two_element_body N"
        );
        let e = container_evidence(
            "body_gate",
            "body".into(),
            &view.body,
            &decode_markup(&found.body),
            &found.body,
            &[],
        );
        clean(&e);
        let (found, view) = body_of(
            "<LawService><부칙><부칙단위><부칙내용>이 조례</부칙내용><부칙내용>는 시행한다.</부칙내용></부칙단위></부칙></LawService>",
        );
        assert_eq!(
            found
                .addenda
                .iter()
                .map(|a| a.text.as_str())
                .collect::<Vec<_>>(),
            vec!["이 조례는 시행한다."],
            "addendum_unit"
        );
        assert_eq!(view.addenda.len(), 1);
        assert_eq!(
            normalized_raw(&view.addenda[0]).unwrap().text,
            "이 조례는 시행한다."
        );
        let (found, view) = body_of(
            "<LawService><부칙><부칙내용>이 조례</부칙내용><부칙내용>는 시행한다.</부칙내용></부칙></LawService>",
        );
        assert_eq!(
            found
                .addenda
                .iter()
                .map(|a| a.text.as_str())
                .collect::<Vec<_>>(),
            vec!["이 조례", "는 시행한다."],
            "independent_addenda"
        );
        assert_eq!(view.addenda.len(), 2);
        assert_ne!(view.addendum_elements[0], view.addendum_elements[1]);
        let (found, view) = body_of(
            "<LawService><본문><![CDATA[이 조례]]><본문><![CDATA[는 시행한다.]]></본문><![CDATA[끝]]></본문></LawService>",
        );
        let ids = run_ids(&view.body);
        assert!(
            ids[0] != ids[1] && ids[1] != ids[2] && ids[0] != ids[2],
            "nested runs_distinct {ids:?}"
        );
        assert_eq!(
            normalized_raw(&view.body).unwrap().text,
            found.body,
            "N_cross_run_separator_deletable false"
        );
        let (_, view) =
            body_of("<LawService><본문><![CDATA[이 조례\r\n는 시행한다.]]></본문></LawService>");
        assert!(
            normalized_raw(&view.body).unwrap().text.contains("\r\n"),
            "real CRLF kept in N"
        );
        let (found, view) =
            body_of("<LawService><본문>이 조례&#40;제1호)는 시행한다.</본문></LawService>");
        eprintln!(
            "{}",
            json!({"case":"event_generalref", "pieces":view.body.segments.iter().filter(|s| s["source"].is_object()).map(|s| json!({"kind":s["event_kind"], "run":s["source"]["run_id"]})).collect::<Vec<_>>(), "B":found.body})
        );
        assert_eq!(
            normalized_raw(&view.body).unwrap().text,
            "이 조례(제1호)는 시행한다.\n",
            "event_generalref N"
        );
        let (found, view) =
            body_of("<LawService><본문><![CDATA[&l]]><![CDATA[t;]]></본문></LawService>");
        assert_eq!(
            decode_markup(&found.body),
            "&l\nt;\n",
            "B_preserves_synthetic_separator"
        );
        let n = normalized_raw(&view.body).unwrap().decoded().unwrap();
        assert_eq!(n.text, "<\n", "same_run_N_decoded");
        let units = view_units(&n).unwrap();
        let events: Vec<u64> = units[0].sources.iter().map(|s| s.1).collect();
        assert_eq!(
            units[0]
                .sources
                .iter()
                .map(|s| (s.2, s.3))
                .collect::<Vec<_>>(),
            vec![(0, 2), (0, 2)],
            "source_ranges"
        );
        assert_ne!(events[0], events[1]);
        let (found, view) = body_of(
            "<LawService><조문><조내용>이 조례는 시행한다.</조내용><조내용>이 조례는 시행한다.</조내용></조문></LawService>",
        );
        let e = container_evidence(
            "body_gate",
            "body".into(),
            &view.body,
            &decode_markup(&found.body),
            &found.body,
            &[],
        );
        clean(&e);
        let subjects: Vec<&EvidenceKey> = e
            .views
            .iter()
            .find(|(name, _)| name == "B-decoded")
            .unwrap()
            .1
            .keys()
            .filter(|k| matches!(k, EvidenceKey::Subject { .. }))
            .collect();
        assert_eq!(subjects.len(), 2, "equal_text_does_not_merge_evidence");
        let elements: BTreeSet<u64> = subjects
            .iter()
            .map(|k| match k {
                EvidenceKey::Subject { subject, .. } => subject[0].0,
                _ => 0,
            })
            .collect();
        assert_eq!(elements.len(), 2, "source_element_ids_distinct");
    }

    #[test]
    fn expected_v4_boundaries_and_terminal() {
        let piece = one_piece("이");
        let units = unit_index(&piece).unwrap();
        assert!(map_point(&units, 1).is_err(), "source_boundary_inside_utf8");
        assert!(map_bounded(&units, 1, 3).is_err());
        let terminal = |view: &SourceSegments| {
            let matches = fixed_start_matches(&view.text, &VERB_MATCHERS);
            let found = matches
                .iter()
                .find(|m| m.0 == "terminal")
                .copied()
                .expect("terminal match");
            let units = unit_index(view).unwrap();
            let (ranges, consumed) =
                map_verb(&units, &view.text, found.1, found.2, found.3).unwrap();
            (
                [found.1, found.2],
                canonical_ranges(&ranges, &piece_texts(view)),
                consumed,
            )
        };
        let payload = one_piece("시행 ");
        let mut with_lf = SourceSegments::default();
        with_lf.append("시행 ", Some(1), 0, 0, "CData", true);
        let space = terminal(&payload);
        let space_lf = terminal(&with_lf);
        assert_eq!(
            space,
            ([0, 7], vec![(1, 0, 0, 7)], 0),
            "terminal_payload_space"
        );
        assert_eq!(
            space_lf,
            ([0, 8], vec![(1, 0, 0, 7)], 1),
            "terminal_payload_plus_synthetic_lf"
        );
        assert_eq!(space.1, space_lf.1, "same_source_key_as");
        assert_ne!(
            space_lf.1,
            vec![(1, 0, 0, 8)],
            "terminal_mutation_fake_synthetic_source"
        );
        assert_ne!(
            space_lf.1,
            vec![(1, 0, 0, 6)],
            "terminal_mutation_drop_payload_space"
        );
        let real = one_piece("시행\n");
        let mut real_lf = SourceSegments::default();
        real_lf.append("시행\n", Some(1), 0, 0, "CData", true);
        assert_eq!(
            terminal(&real),
            ([0, 7], vec![(1, 0, 0, 7)], 0),
            "terminal_real_lf_only"
        );
        assert_eq!(
            terminal(&real_lf),
            ([0, 8], vec![(1, 0, 0, 7)], 1),
            "terminal_real_lf_plus_synthetic"
        );
        let mut errors = Vec::new();
        assert!(
            fragment_verbs("\n", 0, &mut errors).is_empty()
                && errors.is_empty()
                && !has_effective_verb("\n"),
            "synthetic_negative_fragment"
        );
        let null_core = SourceSegments {
            text: "시행 ".to_owned(),
            segments: vec![
                json!({"output_byte_range":[0,6], "source":null, "separator":"event-inserted-lf", "run_id":0}),
                json!({"output_byte_range":[6,7], "source":{"element_id":1, "event_id":0, "run_id":0, "piece_byte_range":[0,1]}, "event_kind":"CData"}),
            ],
        };
        let units = unit_index(&null_core).unwrap();
        assert!(
            map_verb(&units, &null_core.text, 0, 7, 6).is_err(),
            "terminal_core_source_null"
        );
    }

    #[test]
    fn plan_v3_additional_fixed_inputs() {
        // 施行<Cf>令 and 施行<Cf>規: B keeps its Chinese-character verb.
        for suffix in ["令이다.", "規則이다."] {
            let e = evidence(&format!("이 조례는 施行{ZW}{suffix}"), false);
            clean(&e);
            assert!(
                all_rows(&e).iter().any(|r| r.3 == "施行"),
                "B keeps 施行 before {suffix}"
            );
            assert!(
                !rows(view(&e, "N-decoded")).iter().any(|r| r.3 == "施行"),
                "N excludes 施行 before {suffix}"
            );
        }
        // Multi-element body: two elements and Text, GeneralRef and CData pieces form one call.
        let xml = "<LawService><조문><조내용>이 조례&#40;제1호)</조내용><조내용><![CDATA[는 ]]>시행한다.</조내용></조문></LawService>";
        let (found, view) = body_of(xml);
        let e = container_evidence(
            "body_gate",
            "body".into(),
            &view.body,
            &decode_markup(&found.body),
            &found.body,
            &[],
        );
        clean(&e);
        let units = view_units(&view.body).unwrap();
        let pieces: BTreeSet<(u64, u64)> = units
            .iter()
            .flat_map(|u| u.sources.iter().map(|s| (s.0, s.1)))
            .collect();
        let collected = view
            .body
            .segments
            .iter()
            .filter(|s| s["source"].is_object())
            .count();
        assert_eq!(pieces.len(), collected, "no source piece dropped");
        assert!(collected >= 4, "pieces {collected}");
        // Real LF and an event LF next to each other: only the event LF between pieces of one run goes.
        let (_, view) = body_of(
            "<LawService><본문><![CDATA[이 조례\n]]><![CDATA[는 시행한다.]]></본문></LawService>",
        );
        assert_eq!(
            normalized_raw(&view.body).unwrap().text,
            "이 조례\n는 시행한다.\n"
        );
    }

    #[test]
    fn r24_starts_and_bools_preserved() {
        let texts = [
            format!("이 조례는 施行{ZW}令이다. 이 규칙{ZW}은 시행한다."),
            format!("이 조례는 시행하고 시{ZW}행한다."),
            format!("이 조례는 시행한다{ZW}. 운영을 정한다."),
            format!("이 조례{ZW}는 조례는 시행한다."),
            format!("부{ZW}칙 안내\n부칙 안내\n(이 조례는 시행한다.) 시행한다."),
            format!("시행 이 조례{ZW}는 시행한다."),
            format!("이 조례는 부터 효력{ZW}을 발생한다."),
            "부칙 <2024.3.1 조례 제7호> 제1조(시행일) 이 조례는 공포한 날부터 시행한다.\n제2조 이 조례(제1234호)는 시행한다.".to_owned(),
            "이 조례는 공포한 날부터 시행한다. 다만, 제3조는 2025년 1월 1일부터 시행한다.".to_owned(),
        ];
        for text in &texts {
            for after_heading in [false, true] {
                let mut errors = Vec::new();
                let found = r24_evidence(text, after_heading, &mut errors);
                assert!(errors.is_empty(), "{text}: {errors:?}");
                assert_eq!(
                    found.starts,
                    detected_sentence_starts(text, after_heading),
                    "{text}"
                );
                let phrase = !fixed_start_matches(text, &PHRASE_MATCHERS).is_empty();
                assert_eq!(
                    names_effective_date(text),
                    phrase || !detected_sentence_starts(text, false).is_empty(),
                    "{text}"
                );
                let spans = bracket_subject_spans(text);
                assert_eq!(
                    spans.iter().map(|s| s.0).collect::<Vec<_>>(),
                    bracket_subject_starts(text)
                );
            }
        }
    }

    fn approved(addendum: &str, promulgation: &str) -> SourceRevision {
        SourceRevision {
            identity: "1".to_owned(), revision: "9".to_owned(), name: "test".to_owned(), category: "조례".to_owned(),
            canonical_path: "p.md".to_owned(), source_path: "9.xml".to_owned(),
            xml: format!("<LawService><부칙><부칙공포일자>{promulgation}</부칙공포일자><부칙내용><![CDATA[{addendum}]]></부칙내용></부칙></LawService>").into_bytes(),
            promulgation: promulgation.to_owned(), enforcement: "99991231".to_owned(), amendment: "제정".to_owned(), repeal: false,
            markdown: Vec::new(), secondary_name: String::new(), secondary_number: 0, serial: 9, copy_priority: 0, copy_conflict: None,
        }
    }

    fn with_fault<T>(run: impl FnOnce() -> T) -> T {
        FORCE_PROVENANCE_ERROR.with(|flag| flag.set(true));
        let result = run();
        FORCE_PROVENANCE_ERROR.with(|flag| flag.set(false));
        result
    }

    #[test]
    fn expected_v4_provenance_faults() {
        let present = approved("이 조례는 공포한 날부터 시행한다.", "20240301");
        let normal = resolve("ordinances", &present).unwrap();
        assert_eq!(normal.status, "confirmed", "control");
        let faulted = with_fault(|| resolve("ordinances", &present).unwrap());
        assert_eq!(faulted.status, "unresolved", "fault_pub_present");
        assert_eq!(
            faulted.date_evidence["date_withheld"], "source-provenance-error",
            "diagnostic_required"
        );
        let absent = approved("이 조례는 2024년 3월 1일부터 시행한다.", "");
        let normal = resolve("ordinances", &absent).unwrap();
        assert_eq!(normal.status, "confirmed", "control without promulgation");
        let faulted = with_fault(|| resolve("ordinances", &absent).unwrap());
        assert_eq!(faulted.status, "undated", "fault_pub_absent");
        assert_eq!(
            faulted.date_evidence["date_withheld"],
            "source-provenance-error"
        );
        let unresolved = approved("이 조례는 의회의 승인을 받은 날부터 시행한다.", "20240301");
        let normal = resolve("ordinances", &unresolved).unwrap();
        assert_eq!(normal.status, "unresolved", "control unresolved");
        let faulted = with_fault(|| resolve("ordinances", &unresolved).unwrap());
        assert_eq!(
            (
                faulted.status.as_str(),
                &faulted.date_evidence,
                &faulted.reason
            ),
            (
                normal.status.as_str(),
                &normal.date_evidence,
                &normal.reason
            ),
            "fault_existing_unresolved"
        );
    }

    /// Writes every fragment that the evidence checks and the fixed-start matches of the engine
    /// for the comparison with `matcher_reference.py` (`MATCHER_FRAGMENTS_OUT`).
    #[test]
    fn matcher_fragments_for_reference() {
        let Some(out) = std::env::var_os("MATCHER_FRAGMENTS_OUT") else {
            return;
        };
        let texts: Vec<(String, bool)> = vec![
            (
                format!("이 조례는 施行{ZW}令이다. 이 규칙{ZW}은 시행한다."),
                false,
            ),
            (format!("이 조례는 시행하고 시{ZW}행한다."), false),
            (format!("이 조례는 시행한다{ZW}. 운영을 정한다."), false),
            (format!("이 조례{ZW}는 조례는 시행한다."), false),
            (format!("시행한다{ZW}. 운영을 정한다."), true),
            (
                format!("부{ZW}칙 안내\n부칙 안내\n(이 조례는 시행한다.) 시행한다."),
                false,
            ),
            (format!("시행 이 조례{ZW}는 시행한다."), true),
            (format!("이 조례는 부터 효력{ZW}을 발생한다."), false),
            ("이 조례는 施行하고 시행한다.".to_owned(), false),
            (format!("{ZW}이 조례는 시행한다."), false),
            (format!("이 조례는 施行{ZW}規則이다."), false),
            ("시행 ".to_owned(), true),
            (
                "이 조례는 부터 시행한다. 효력이 발생하고 부터 그 효력을 가진다.".to_owned(),
                false,
            ),
        ];
        FRAGMENT_LOG.with(|slot| *slot.borrow_mut() = Some(Vec::new()));
        let mut wholes = Vec::new();
        for (text, after_heading) in &texts {
            let e = evidence(text, *after_heading);
            clean(&e);
            let raw = one_piece(text);
            let n = normalized_raw(&raw).unwrap();
            for view in [
                raw.text.clone(),
                decode_markup(&raw.text),
                n.text.clone(),
                decode_markup(&n.text),
            ] {
                wholes.push(view);
            }
        }
        let mut fragments = FRAGMENT_LOG.with(|slot| slot.borrow_mut().take()).unwrap();
        fragments.extend(wholes);
        fragments.sort();
        fragments.dedup();
        let rows: Vec<Value> = fragments.iter().map(|fragment| {
            let mut found: Vec<(&str, usize, usize)> = fixed_start_matches(fragment, &VERB_MATCHERS).into_iter().chain(fixed_start_matches(fragment, &PHRASE_MATCHERS)).map(|m| (m.0, m.1, m.2)).collect();
            found.sort();
            found.dedup();
            json!({"fragment":fragment, "engine":found.iter().map(|m| json!([m.0, m.1, m.2])).collect::<Vec<_>>()})
        }).collect();
        std::fs::write(out, serde_json::to_vec_pretty(&rows).unwrap()).unwrap();
    }

    /// Runs every input of `GATE_COMPARE_MANIFEST` through `resolve_rules` and writes the old and
    /// the new gate marks, the keys of E - R and the PHRASE N - B of each case to `GATE_COMPARE_OUT`.
    #[test]
    fn gate_comparison_inputs() {
        let (Some(manifest), Some(out)) = (
            std::env::var_os("GATE_COMPARE_MANIFEST"),
            std::env::var_os("GATE_COMPARE_OUT"),
        ) else {
            return;
        };
        let cases: Value = serde_json::from_slice(&std::fs::read(manifest).unwrap()).unwrap();
        let mut rows = Vec::new();
        for case in cases.as_array().unwrap() {
            let path = case["path"].as_str().unwrap();
            let xml = std::fs::read(path).unwrap();
            assert_eq!(sha256(&xml), case["sha256"].as_str().unwrap(), "{path}");
            let meta = &case["revision"];
            let text = |key: &str, default: &str| meta[key].as_str().unwrap_or(default).to_owned();
            let amendment = text("amendment", "제정");
            let source = SourceRevision {
                identity: text("identity", "1"),
                revision: text("revision", "9"),
                name: "test".to_owned(),
                category: text("category", "조례"),
                canonical_path: "p.md".to_owned(),
                source_path: text("source_path", "9.xml"),
                xml,
                promulgation: text("promulgation", "20240301"),
                enforcement: text("enforcement", "99991231"),
                repeal: amendment.contains("폐지"),
                amendment,
                markdown: Vec::new(),
                secondary_name: String::new(),
                secondary_number: 0,
                serial: meta["serial"].as_u64().unwrap_or(9),
                copy_priority: 0,
                copy_conflict: None,
            };
            let dataset = case["dataset"].as_str().unwrap();
            let started = std::time::Instant::now();
            let plain = resolve_rules(dataset, &source);
            let elapsed = started.elapsed().as_secs_f64();
            GATE_REPORT.with(|slot| *slot.borrow_mut() = Some(Vec::new()));
            let result = resolve_rules(dataset, &source);
            let report = GATE_REPORT.with(|slot| slot.borrow_mut().take()).unwrap();
            let row = match (plain, result) {
                (Ok(_), Ok(mut occurrence)) => {
                    let gate = occurrence.gate;
                    let status = occurrence.status.clone();
                    let dated = matches!(status.as_str(), "confirmed" | "estimated");
                    let old = report.first().map(|r| {
                        (
                            r["old"][0].as_bool().unwrap(),
                            r["old"][1].as_bool().unwrap(),
                        )
                    });
                    let old_applies = old.is_some_and(|(rule, default)| {
                        gate.text_derived && if gate.by_default { default } else { rule } && dated
                    });
                    let new_applies = occurrence.gate_applies();
                    occurrence.apply_gate();
                    json!({"case_id":case["case_id"], "group":case["group"], "dataset":dataset, "gated":!report.is_empty(),
                        "status_before_gate":status, "by_default":gate.by_default, "text_derived":gate.text_derived,
                        "old":old.map(|o| json!({"rule":o.0, "default":o.1})), "new":{"rule":gate.any_rule, "default":gate.any_default},
                        "old_gate_applies":old_applies, "new_gate_applies":new_applies, "status_new_gate":occurrence.status,
                        "provenance_error":gate.provenance_error, "approval":gate.approval, "approval_blocked":gate.approval_blocked,
                        "variants":report.first().map(|r| r["variants"].clone()), "elapsed_seconds":elapsed, "report":report})
                }
                (plain, result) => {
                    json!({"case_id":case["case_id"], "group":case["group"], "error":format!("{:?} / {:?}", plain.err(), result.err())})
                }
            };
            rows.push(row);
        }
        std::fs::write(out, serde_json::to_vec_pretty(&rows).unwrap()).unwrap();
    }
}

/// Tests of the three approvals with a gate policy (r25c-design.md 8). The expected values come
/// from `approved-date-expectations-20261113.json` (read only) and from the design table; the
/// normal cases read the real source XML and check its SHA-256 against the oracle.
#[cfg(test)]
mod approval_tests {
    use super::*;

    const ORACLE: &str = "approved-date-expectations-20261113.json";
    // The two copies of MST 1063983 that the verification used hold the same bytes (one SHA-256
    // in the oracle), so one fixture file serves both.
    const ORD_A: &str = "1063983.xml";
    const ORD_B: &str = "1063983.xml";

    fn oracle() -> Vec<Value> {
        let value: Value =
            serde_json::from_slice(&std::fs::read(fixture(ORACLE)).unwrap()).unwrap();
        value["revisions"].as_array().unwrap().clone()
    }

    fn entry(revision: &str) -> Value {
        oracle()
            .into_iter()
            .find(|entry| entry["revision"] == revision)
            .unwrap()
    }

    fn xml_of(entry: &Value) -> String {
        let revision = entry["revision"].as_str().unwrap();
        let path = if entry["dataset"] == "admrules" {
            format!("{revision}.xml")
        } else {
            ORD_A.to_owned()
        };
        let bytes = std::fs::read(fixture(&path)).unwrap();
        assert_eq!(
            sha256(&bytes),
            entry["observed_source_sha256"].as_str().unwrap(),
            "{revision} source SHA-256"
        );
        String::from_utf8(bytes).unwrap()
    }

    fn source_of(entry: &Value, xml: &str) -> SourceRevision {
        let text = |key: &str| entry[key].as_str().unwrap().to_owned();
        let admrules = entry["dataset"] == "admrules";
        SourceRevision {
            identity: text("identity"),
            revision: text("revision"),
            name: "test".to_owned(),
            category: if admrules { "등기예규" } else { "규칙" }.to_owned(),
            canonical_path: "p.md".to_owned(),
            source_path: format!("{}.xml", text("revision")),
            xml: xml.as_bytes().to_vec(),
            promulgation: text("raw_promulgation_date"),
            enforcement: text("raw_enforcement_date"),
            amendment: "일부개정".to_owned(),
            repeal: false,
            markdown: Vec::new(),
            secondary_name: String::new(),
            secondary_number: 0,
            serial: text("revision").parse().unwrap(),
            copy_priority: 0,
            copy_conflict: None,
        }
    }

    fn run(entry: &Value, source: &SourceRevision) -> Occurrence {
        resolve(entry["dataset"].as_str().unwrap(), source).unwrap()
    }

    fn approval_keys(found: &Occurrence) -> BTreeSet<String> {
        found
            .date_evidence
            .as_object()
            .unwrap()
            .keys()
            .filter(|key| key.starts_with("approved_date_"))
            .cloned()
            .collect()
    }

    fn replaced(xml: &str, old: &str, new: &str) -> String {
        assert_eq!(xml.matches(old).count(), 1, "variant anchor {old:?}");
        xml.replace(old, new)
    }

    #[test]
    fn approval_normal_selection_matches_oracle() {
        assert_eq!(
            sha256(&std::fs::read(fixture(ORD_B)).unwrap()),
            entry("1063983")["observed_source_sha256"].as_str().unwrap(),
            "second copy"
        );
        for entry in oracle() {
            let xml = xml_of(&entry);
            let source = source_of(&entry, &xml);
            GATE_REPORT.with(|slot| *slot.borrow_mut() = Some(Vec::new()));
            let found = run(&entry, &source);
            let report = GATE_REPORT.with(|slot| slot.borrow_mut().take()).unwrap();
            let name = entry["revision"].as_str().unwrap();
            assert_eq!(
                found.status,
                entry["expected_status"].as_str().unwrap(),
                "{name} status: {}",
                found.date_evidence
            );
            assert_eq!(
                found.enforcement_date.as_deref(),
                entry["expected_enforcement_date"].as_str(),
                "{name}"
            );
            assert_eq!(
                found.promulgation_date.as_deref(),
                entry["expected_promulgation_date"].as_str(),
                "{name}"
            );
            assert_eq!(
                found.raw_enforcement_date,
                entry["raw_enforcement_date"].as_str().unwrap(),
                "{name} raw date kept"
            );
            let evidence = &found.date_evidence;
            assert!(
                evidence.get("date_withheld").is_none(),
                "{name}: {evidence}"
            );
            assert_eq!(
                evidence["approved_date_decision"], entry["decision_id"],
                "{name}"
            );
            assert_eq!(
                evidence["approved_date_sentence"], entry["approved_sentence"],
                "{name}"
            );
            assert_eq!(
                evidence["approved_date_user_choice"], entry["user_choice"],
                "{name}"
            );
            let fields: BTreeSet<String> = evidence["approved_date_sources"]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x["field"].as_str().unwrap().to_owned())
                .collect();
            let expected_fields: BTreeSet<String> = match entry.get("sentence_fields") {
                Some(list) => list
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|x| x.as_str().unwrap().to_owned())
                    .collect(),
                None => [entry["sentence_field"].as_str().unwrap().to_owned()].into(),
            };
            assert_eq!(fields, expected_fields, "{name} source fields");
            let mut keys: BTreeSet<String> = [
                "approved_date_decision",
                "approved_date_sentence",
                "approved_date_policy",
                "approved_date_sources",
                "approved_date_user_choice",
            ]
            .iter()
            .map(|x| x.to_string())
            .collect();
            if entry["dataset"] == "ordinances" {
                keys.insert("approved_date_conflict".to_owned());
                assert_eq!(evidence["approved_date_policy"], "conditional-exception");
                assert_eq!(
                    evidence["approved_date_conflict"],
                    json!({"source_enforcement_date":"20070530", "selected_enforcement_date":"2007-06-01", "resolved_by":"user", "decision":entry["decision_id"]})
                );
            } else {
                assert_eq!(
                    evidence["approved_date_policy"],
                    "consume-approved-sentence"
                );
            }
            assert_eq!(approval_keys(&found), keys, "{name} evidence key names");
            for key in ["approval_source", "approved_sentence"] {
                assert!(
                    evidence.get(key).is_none(),
                    "{name}: the keys of the exempt approvals stay apart"
                );
            }
            // Every consumed key lies in the source ranges of the approved sentence: each of its ranges
            // (element ID, event ID, logical range in the piece) and its candidate start lie inside
            // one source range of an occurrence of the sentence. The check is the test's own; it
            // does not use the containment function of the product.
            let spans: Vec<(u64, u64, u64, u64)> = evidence["approved_date_sources"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|found| found["source_ranges"].as_array().unwrap().iter())
                .map(|range| {
                    (
                        range[0].as_u64().unwrap(),
                        range[1].as_u64().unwrap(),
                        range[2].as_u64().unwrap(),
                        range[3].as_u64().unwrap(),
                    )
                })
                .collect();
            assert!(
                !spans.is_empty(),
                "{name}: the approved sentence has source ranges"
            );
            let inside = |range: &Value| -> bool {
                let (element, event, start) = (
                    range[0].as_u64().unwrap(),
                    range[1].as_u64().unwrap(),
                    range[2].as_u64().unwrap(),
                );
                let end = range.get(3).and_then(Value::as_u64).unwrap_or(start);
                spans.iter().any(|span| {
                    span.0 == element && span.1 == event && span.2 <= start && end <= span.3
                })
            };
            let mut consumed = 0;
            for partition in report[0]["partitions"].as_array().unwrap() {
                for item in partition["consumed"].as_array().unwrap() {
                    consumed += 1;
                    let key = &item["key"];
                    let ranges: Vec<&Value> = match key["kind"].as_str().unwrap() {
                        "subject" | "extended" => key["subject"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .chain(key["verb"].as_array().unwrap().iter())
                            .collect(),
                        "heading" => std::iter::once(&key["candidate"])
                            .chain(key["verb"].as_array().unwrap().iter())
                            .collect(),
                        _ => key["phrase"].as_array().unwrap().iter().collect(),
                    };
                    assert!(
                        !ranges.is_empty(),
                        "{name}: consumed key without ranges {item}"
                    );
                    for range in ranges {
                        assert!(
                            inside(range),
                            "{name}: consumed key outside the approved sentence: {range} of {item}"
                        );
                    }
                }
                assert!(
                    partition["E_minus_R"].as_array().unwrap().is_empty(),
                    "{name}: {partition}"
                );
                assert!(
                    partition["errors"].as_array().unwrap().is_empty(),
                    "{name}: {partition}"
                );
            }
            assert!(consumed > 0, "{name}: the approved sentence was consumed");
            eprintln!(
                "{}",
                json!({"case":"approval-normal", "revision":name, "status":found.status, "date":found.enforcement_date, "consumed_keys":consumed, "report":report[0]["variants"]})
            );
        }
    }

    #[test]
    fn approval_attribution_mismatch_selects_nothing() {
        // F2: every axis of the registration. The existing path decides (the arrival estimate),
        // and the gate holds it because the approved sentence is no consumed evidence.
        for revision in ["2200000088451", "2200000104677"] {
            let entry = entry(revision);
            let xml = xml_of(&entry);
            let sentence = entry["approved_sentence"].as_str().unwrap();
            let mut variants: Vec<(&str, SourceRevision)> = Vec::new();
            let mut source = source_of(&entry, &xml);
            source.identity.push('9');
            variants.push(("identity", source));
            let mut source = source_of(&entry, &xml);
            source.revision = "2200000000001".to_owned();
            variants.push(("mst", source));
            let mut source = source_of(&entry, &xml);
            source.promulgation = "20000101".to_owned();
            variants.push(("promulgation", source));
            let mut source = source_of(&entry, &xml);
            source.enforcement = "20000101".to_owned();
            variants.push(("enforcement", source));
            let altered = sentence.replace("부터 시행한다", "부터 시행 한다");
            variants.push((
                "sentence",
                source_of(&entry, &replaced(&xml, sentence, &altered)),
            ));
            for (axis, source) in variants {
                let found = run(&entry, &source);
                assert!(approval_keys(&found).is_empty(), "{revision} {axis}");
                assert_ne!(
                    found.enforcement_date.as_deref(),
                    entry["expected_enforcement_date"].as_str(),
                    "{revision} {axis}"
                );
                let expected = if axis == "enforcement" {
                    "confirmed"
                } else {
                    "unresolved"
                };
                assert_eq!(
                    found.status, expected,
                    "{revision} {axis}: {}",
                    found.date_evidence
                );
            }
            // The same text for another dataset is no registered approval.
            let found = resolve("ordinances", &source_of(&entry, &xml)).unwrap();
            assert!(approval_keys(&found).is_empty(), "{revision} dataset");
        }
        // 1063983: the existing source date and its conflict protection decide.
        let entry = entry("1063983");
        let xml = xml_of(&entry);
        let mut variants: Vec<(&str, SourceRevision, &str)> = Vec::new();
        let mut source = source_of(&entry, &xml);
        source.identity = "2153240".to_owned();
        variants.push(("identity", source, "2007-05-30"));
        let mut source = source_of(&entry, &xml);
        source.revision = "1063984".to_owned();
        variants.push(("mst", source, "2007-05-30"));
        let mut source = source_of(&entry, &xml);
        source.promulgation = "20070531".to_owned();
        variants.push(("promulgation", source, "2007-05-30"));
        let mut source = source_of(&entry, &xml);
        source.enforcement = "20070601".to_owned();
        variants.push(("enforcement", source, "2007-06-01"));
        let addendum = replaced(
            &xml,
            "(2007.05.30) 이 규칙은 2007년 6월 1일부터 시행한다.",
            "(2007.05.30) 이 규칙은 2007년 6월 2일부터 시행한다.",
        );
        variants.push((
            "addendum sentence",
            source_of(&entry, &addendum),
            "2007-05-30",
        ));
        let amendment = replaced(
            &xml,
            "<![CDATA[이 규칙은 2007년 6월 1일부터 시행한다.]]>",
            "<![CDATA[이 규칙은 2007년 6월 2일부터 시행한다.]]>",
        );
        variants.push((
            "amendment sentence",
            source_of(&entry, &amendment),
            "2007-05-30",
        ));
        for (axis, source, date) in variants {
            let found = run(&entry, &source);
            assert!(approval_keys(&found).is_empty(), "1063983 {axis}");
            assert_eq!(
                (
                    found.status.as_str(),
                    found.enforcement_date.as_deref(),
                    found.reason.as_str()
                ),
                ("confirmed", Some(date), VALID_SOURCE_DATE),
                "1063983 {axis}"
            );
        }
    }

    #[test]
    fn approval_holds_unread_proviso_other_date_and_split() {
        // F2: an unread effective sentence outside the approved sentence.
        let entry88451 = entry("2200000088451");
        let xml = xml_of(&entry88451);
        let unread = replaced(
            &xml,
            "<![CDATA[부  칙 ]]>",
            "<![CDATA[이 예규는 별도로 정하는 날 시행함.]]>\n<![CDATA[부  칙 ]]>",
        );
        let found = run(&entry88451, &source_of(&entry88451, &unread));
        assert_eq!(
            (
                found.status.as_str(),
                found.date_evidence["date_withheld"].as_str()
            ),
            ("unresolved", Some("added-detection-sentence")),
            "F2 unread: {}",
            found.date_evidence
        );
        assert_eq!(
            found.date_evidence["approved_date_decision"],
            entry88451["decision_id"]
        );
        // F2: a proviso right after the approved sentence, and another date in its section.
        let sentence = entry88451["approved_sentence"].as_str().unwrap();
        for (name, xml) in [
            (
                "proviso",
                replaced(
                    &xml,
                    sentence,
                    &format!("{sentence} 다만, 제3항은 2010. 1. 1.부터 시행한다."),
                ),
            ),
            (
                "other date",
                replaced(
                    &xml,
                    &format!("<![CDATA[{sentence}]]>"),
                    &format!(
                        "<![CDATA[{sentence}]]>\n<![CDATA[이 예규는 2009. 7. 1.부터 시행한다.]]>"
                    ),
                ),
            ),
        ] {
            let found = run(&entry88451, &source_of(&entry88451, &xml));
            assert_eq!(
                (
                    found.status.as_str(),
                    found.date_evidence["date_withheld"].as_str()
                ),
                ("unresolved", Some("approved-sentence-blocked")),
                "F2 {name}: {}",
                found.date_evidence
            );
            assert!(found.date_evidence.get("approved_date_blocked").is_some());
        }
        // F2: the approved sentence split across two pieces is no exact sentence: no approval.
        let split = replaced(
            &xml,
            &format!("<![CDATA[{sentence}]]>"),
            "<![CDATA[이 예규는 2009. 6. 27.부터 시]]><![CDATA[행한다.]]>",
        );
        let found = run(&entry88451, &source_of(&entry88451, &split));
        assert!(approval_keys(&found).is_empty());
        assert_eq!(
            found.status, "unresolved",
            "F2 split: {}",
            found.date_evidence
        );
        // 1063983: an unread sentence in the scanned addendum section holds the approved date.
        let entry1063983 = entry("1063983");
        let xml = xml_of(&entry1063983);
        let unread = replaced(
            &xml,
            "2007년 6월 1일부터 시행한다.\n]]></부칙내용>",
            "2007년 6월 1일부터 시행한다.\n이 규칙은 구청장이 정하는 날 시행함.\n]]></부칙내용>",
        );
        let found = run(&entry1063983, &source_of(&entry1063983, &unread));
        assert_eq!(
            (
                found.status.as_str(),
                found.date_evidence["date_withheld"].as_str()
            ),
            ("unresolved", Some("added-detection-sentence")),
            "1063983 unread: {}",
            found.date_evidence
        );
        assert_eq!(found.raw_enforcement_date, "20070530");
        // 1063983: another read date in the section of the approved sentence: held, not the source date.
        let other = replaced(
            &xml,
            "(2007.05.30) 이 규칙은 2007년 6월 1일부터 시행한다.",
            "(2007.05.30) 이 규칙은 2007년 6월 1일부터 시행한다. 이 규칙은 2007년 7월 1일부터 시행한다.",
        );
        let found = run(&entry1063983, &source_of(&entry1063983, &other));
        assert_eq!(
            (
                found.status.as_str(),
                found.date_evidence["date_withheld"].as_str()
            ),
            ("unresolved", Some("approved-sentence-blocked")),
            "1063983 other date: {}",
            found.date_evidence
        );
        // 1063983: a sentence split in the amendment text is no exact sentence: the source date decides.
        let split = replaced(
            &xml,
            "<![CDATA[이 규칙은 2007년 6월 1일부터 시행한다.]]>",
            "<![CDATA[이 규칙은 2007년 6월 1일부터 시]]><![CDATA[행한다.]]>",
        );
        let found = run(&entry1063983, &source_of(&entry1063983, &split));
        assert!(approval_keys(&found).is_empty());
        assert_eq!(
            (found.status.as_str(), found.enforcement_date.as_deref()),
            ("confirmed", Some("2007-05-30")),
            "1063983 split"
        );
    }

    #[test]
    fn approval_copy_bundle() {
        let entry = entry("1063983");
        let first = String::from_utf8(std::fs::read(fixture(ORD_A)).unwrap()).unwrap();
        let second = String::from_utf8(std::fs::read(fixture(ORD_B)).unwrap()).unwrap();
        let copy = |xml: &str, path: &str, priority: u8| {
            let mut source = source_of(&entry, xml);
            source.source_path = path.to_owned();
            source.copy_priority = priority;
            source.markdown = b"---\nname: x\n---\nbody\n".to_vec();
            source
        };
        let statuses = |plan: &DatePlan| -> Vec<(String, Option<String>, Option<String>)> {
            (0..plan.sources.len())
                .map(|index| {
                    let found = plan.occurrence(index);
                    (
                        found.status.clone(),
                        found.enforcement_date.clone(),
                        found.date_evidence["date_withheld"]
                            .as_str()
                            .map(str::to_owned),
                    )
                })
                .collect()
        };
        // The same content at two source paths: both copies take the approved date.
        let plan = DatePlan::new(
            "ordinances",
            true,
            vec![
                copy(&first, "ord-rules/1063983.xml", 0),
                copy(&second, "history/1063983.xml", 1),
            ],
        )
        .unwrap();
        assert_eq!(
            statuses(&plan),
            vec![("confirmed".to_owned(), Some("2007-06-01".to_owned()), None); 2],
            "same content, other path"
        );
        // One copy fails a condition: the group is held and no copy falls back to its source date.
        let changed = replaced(
            &second,
            "<![CDATA[이 규칙은 2007년 6월 1일부터 시행한다.]]>",
            "<![CDATA[이 규칙은 2007년 6월 2일부터 시행한다.]]>",
        );
        let plan = DatePlan::new(
            "ordinances",
            true,
            vec![
                copy(&first, "ord-rules/1063983.xml", 0),
                copy(&changed, "history/1063983.xml", 1),
            ],
        )
        .unwrap();
        assert_eq!(
            statuses(&plan),
            vec![
                (
                    "unresolved".to_owned(),
                    None,
                    Some("approved-copy-mismatch".to_owned())
                );
                2
            ],
            "copy mismatch"
        );
        // Copies that the compiler marks as a content conflict stay unresolved by the copy conflict.
        let mut a = copy(&first, "ord-rules/1063983.xml", 0);
        let mut b = copy(&changed, "history/1063983.xml", 1);
        a.copy_conflict = Some("content differs at 제개정이유내용".to_owned());
        b.copy_conflict = a.copy_conflict.clone();
        let plan = DatePlan::new("ordinances", true, vec![a, b]).unwrap();
        for index in 0..2 {
            let found = plan.occurrence(index);
            assert_eq!(
                (found.status.as_str(), found.enforcement_date.as_deref()),
                ("unresolved", None),
                "copy conflict"
            );
            assert!(approval_keys(found).is_empty());
            assert!(found.date_evidence.get("copy_conflict").is_some());
        }
    }

    #[test]
    fn exempt_approvals_keep_their_contract() {
        // The two approvals before r25c: no gate, the old evidence keys, no new keys.
        for approval in APPROVALS
            .iter()
            .filter(|approval| approval.gate_policy == GatePolicy::Exempt)
        {
            assert!(
                approval.identity.is_none()
                    && approval.enforcement.is_none()
                    && approval.fields.is_empty()
            );
        }
        assert_eq!(
            APPROVALS
                .iter()
                .filter(|approval| approval.gate_policy != GatePolicy::Exempt)
                .map(|approval| approval.revision)
                .collect::<Vec<_>>(),
            vec!["2200000088451", "2200000104677", "1063983"]
        );
    }

    const EXPECTED: &str = "i1-expected.json";
    const EXPECTED_NB1: &str = "nb1-expected.json";
    const EXPECTED_B3X: &str = "b3x-expected.json";

    /// The expected values of the cases below. `i1-expected.json` is the file of the prototype
    /// (task i1, unchanged); `nb1-expected.json` holds the cases of the re-verification NB1 and
    /// was fixed before the code of those cases. `b3x-expected.json` holds the second B3 control.
    /// The files have the same `cases` form.
    fn expected_document() -> Value {
        let mut document: Value =
            serde_json::from_slice(&std::fs::read(fixture(EXPECTED)).unwrap()).unwrap();
        let nb1: Value =
            serde_json::from_slice(&std::fs::read(fixture(EXPECTED_NB1)).unwrap()).unwrap();
        let b3x: Value =
            serde_json::from_slice(&std::fs::read(fixture(EXPECTED_B3X)).unwrap()).unwrap();
        for extra in [&nb1, &b3x] {
            document["cases"]
                .as_array_mut()
                .unwrap()
                .extend(extra["cases"].as_array().unwrap().iter().cloned());
        }
        document
    }

    fn expected_case(id: &str) -> (String, Value) {
        let document = expected_document();
        let found = document["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|case| case["id"] == id)
            .unwrap_or_else(|| panic!("no expected case {id}"));
        (
            found["revision"].as_str().unwrap().to_owned(),
            found["expect"].clone(),
        )
    }

    fn expected_ids(prefix: &str) -> Vec<String> {
        let ids: Vec<String> = expected_document()["cases"]
            .as_array()
            .unwrap()
            .iter()
            .map(|case| case["id"].as_str().unwrap().to_owned())
            .filter(|id| id.starts_with(prefix))
            .collect();
        assert!(!ids.is_empty(), "no expected case with the prefix {prefix}");
        ids
    }

    /// The heading piece, the piece of the approved sentence and another dated sentence of the
    /// amendment statement of one of the two F2 revisions.
    fn f2_pieces(revision: &str, sentence: &str) -> (&'static str, String, &'static str) {
        if revision == "2200000088451" {
            (
                "<![CDATA[부  칙 ]]>",
                format!("<![CDATA[{sentence}]]>"),
                "이 예규는 2009. 7. 1.부터 시행한다.",
            )
        } else {
            (
                "<![CDATA[부    칙]]>",
                format!("<![CDATA[{sentence} ]]>"),
                "이 예규는 1998. 11. 1.부터 시행한다.",
            )
        }
    }

    /// The real source, or the variant that the case id names. The variants change the real input
    /// at one place; the constructions follow the counterexamples of the verification.
    fn variant(id: &str, entry: &Value, xml: &str) -> String {
        let revision = entry["revision"].as_str().unwrap();
        let sentence = entry["approved_sentence"].as_str().unwrap();
        let cdata = |text: &str| format!("<![CDATA[{text}]]>");
        if revision == "1063983" {
            let line = "(2007.05.30) 이 규칙은 2007년 6월 1일부터 시행한다.";
            let in_2005 = "(2005.06.07) 이 규칙은 공포한 날부터 시행한다.";
            let amendment_sentence = "<![CDATA[이 규칙은 2007년 6월 1일부터 시행한다.]]>";
            return match id {
                "b1-1063983-same-line-unread" => replaced(
                    xml,
                    line,
                    &format!("{line} 이 규칙은 구청장이 정하는 날 시행함."),
                ),
                "d-1063983-proviso-in-2005-section" => replaced(
                    xml,
                    in_2005,
                    &format!("{in_2005} 다만, 제3조는 2005년 7월 1일부터 시행한다."),
                ),
                "d-1063983-unread-in-2005-section" => replaced(
                    xml,
                    in_2005,
                    "(2005.06.07) 이 규칙은 구청장이 정하는 날 시행함.",
                ),
                "d-1063983-other-date-after-sentence-amendment" => replaced(
                    xml,
                    amendment_sentence,
                    &format!(
                        "{amendment_sentence}\n<![CDATA[이 규칙은 2007년 7월 1일부터 시행한다.]]>"
                    ),
                ),
                // NB1 C9 and C10: the same shape on the line of the approved sentence and on the next
                // line of the amendment text.
                "nb1-1063983-c9-subjectless-other-date-same-line" => {
                    replaced(xml, line, &format!("{line} 2007년 7월 1일부터 시행한다."))
                }
                "nb1-1063983-c10-subjectless-other-date-amendment-next-line" => replaced(
                    xml,
                    amendment_sentence,
                    &format!("{amendment_sentence}\n<![CDATA[2007년 7월 1일부터 시행한다.]]>"),
                ),
                "real-1063983" => xml.to_owned(),
                other => panic!("no construction for {other}"),
            };
        }
        let (heading, approved, other) = f2_pieces(revision, sentence);
        // The same other-date sentence without its subject ("2009. 7. 1.부터 시행한다.").
        let subjectless = other
            .strip_prefix("이 예규는 ")
            .expect("the other-date sentence starts with its subject");
        let other = cdata(other);
        let addendum = |text: &str| {
            replaced(
                xml,
                "<부칙>\n</부칙>",
                &format!("<부칙><부칙내용><![CDATA[{text}]]></부칙내용></부칙>"),
            )
        };
        let tail = id.split_once('-').map(|(_, tail)| tail).unwrap_or(id);
        let tail = tail
            .rsplit_once('-')
            .filter(|(_, tag)| matches!(*tag, "f2a" | "f2b"))
            .map_or(tail, |(head, _)| head);
        match tail {
            "2200000088451" | "2200000104677" => xml.to_owned(),
            "f2a-same-line-unread" | "f2b-same-line-unread" => replaced(
                xml,
                &approved,
                &cdata(&format!("{sentence} 이 예규는 별도로 정하는 날 시행함.")),
            ),
            "f2a-a2-other-date-before-no-heading" | "f2b-a2-other-date-before-no-heading" => {
                replaced(xml, heading, &other)
            }
            "f2a-a3-other-date-after-no-heading" | "f2b-a3-other-date-after-no-heading" => {
                replaced(
                    &replaced(xml, heading, "<![CDATA[]]>"),
                    &approved,
                    &format!("{approved}\n{other}"),
                )
            }
            "f2a-heading-only-after-sentence" | "f2b-heading-only-after-sentence" => replaced(
                &replaced(xml, heading, "<![CDATA[]]>"),
                &approved,
                &format!("{approved}\n{heading}\n{other}"),
            ),
            "f2a-other-box-after" | "f2b-other-box-after" => replaced(
                &replaced(xml, heading, "<![CDATA[]]>"),
                &approved,
                &format!("{approved}\n<![CDATA[□ 다른 구획]]>\n{other}"),
            ),
            "f2a-other-box-before" | "f2b-other-box-before" => {
                replaced(xml, heading, &format!("{other}\n<![CDATA[□ 다른 구획]]>"))
            }
            "f2a-other-date-before-heading" | "f2b-other-date-before-heading" => {
                replaced(xml, heading, &format!("{other}\n{heading}"))
            }
            "f2a-other-date-after-heading" | "f2b-other-date-after-heading" => {
                replaced(xml, heading, &format!("{heading}\n{other}"))
            }
            // NB1 A8: in the selection range, a subjectless date sentence in the piece of the
            // approved sentence is no reason to hold the approval (I4 second class).
            "f2a-a8-subjectless-other-date-same-piece"
            | "f2b-a8-subjectless-other-date-same-piece" => {
                replaced(xml, &approved, &cdata(&format!("{sentence} {subjectless}")))
            }
            "b1" => addendum("이 예규는 공포한 날부터 시행한다."),
            "b2" => addendum(
                "이 예규는 공포한 날부터 시행한다. 다만, 제3항은 2010. 1. 1.부터 시행한다.",
            ),
            "b3" => addendum(&other["<![CDATA[".len()..other.len() - "]]>".len()]),
            // The second B3 control: the sentences of b1 and b3 under an addendum heading line.
            "b6" => addendum("부칙\n이 예규는 공포한 날부터 시행한다."),
            "b7" => addendum(&format!(
                "부칙\n{}",
                &other["<![CDATA[".len()..other.len() - "]]>".len()]
            )),
            "b5" => replaced(
                xml,
                "<조문내용><![CDATA[",
                &format!(
                    "<조문내용><![CDATA[{}\n",
                    &other["<![CDATA[".len()..other.len() - "]]>".len()]
                ),
            ),
            "f2a-amendment-text-other-date" => replaced(
                xml,
                "</제개정이유내용>",
                &format!("{other}\n</제개정이유내용>"),
            ),
            "f2a-outside-other-date" => replaced(
                xml,
                "</조문내용>",
                &format!("</조문내용><항내용>{other}</항내용>"),
            ),
            _ => panic!("no construction for {id} ({tail})"),
        }
    }

    fn built(id: &str) -> (Value, SourceRevision, Value) {
        let (revision, expect) = expected_case(id);
        let entry = entry(&revision);
        let xml = variant(id, &entry, &xml_of(&entry));
        let source = source_of(&entry, &xml);
        (entry, source, expect)
    }

    fn note(bad: &mut Vec<String>, id: &str, ok: bool, what: impl FnOnce() -> String) {
        if !ok {
            bad.push(format!("{id}: {}", what()));
        }
    }

    /// Resolves a case and lists every difference from its expected values.
    fn problems(id: &str) -> Vec<String> {
        let (entry, source, expect) = built(id);
        let dataset = entry["dataset"].as_str().unwrap();
        GATE_REPORT.with(|slot| *slot.borrow_mut() = Some(Vec::new()));
        let found = resolve(dataset, &source).unwrap();
        let report = GATE_REPORT.with(|slot| slot.borrow_mut().take()).unwrap();
        let evidence = &found.date_evidence;
        let mut bad = Vec::new();
        note(
            &mut bad,
            id,
            found.status == expect["status"].as_str().unwrap(),
            || format!("status {} {evidence}", found.status),
        );
        note(
            &mut bad,
            id,
            found.enforcement_date.as_deref() == expect["enforcement_date"].as_str(),
            || format!("date {:?}", found.enforcement_date),
        );
        let withheld = evidence.get("date_withheld").and_then(Value::as_str);
        note(
            &mut bad,
            id,
            withheld == expect["date_withheld"].as_str(),
            || format!("date_withheld {withheld:?} {evidence}"),
        );
        if let Some(reason) = expect.get("reason") {
            note(&mut bad, id, reason == found.reason.as_str(), || {
                format!("reason {}", found.reason)
            });
        }
        if let Some(date) = expect.get("exposure_date") {
            note(
                &mut bad,
                id,
                found.exposure_date.as_deref() == date.as_str(),
                || format!("exposure_date {:?}", found.exposure_date),
            );
        }
        if let Some(derived) = expect.get("text_derived") {
            note(&mut bad, id, *derived == found.gate.text_derived, || {
                "text_derived".to_owned()
            });
        }
        if expect.get("proviso_after_effective_sentence").is_some() {
            note(
                &mut bad,
                id,
                evidence.get("proviso_after_effective_sentence") == Some(&json!(true)),
                || format!("proviso key {evidence}"),
            );
        }
        if let Some(blocked) = expect.get("blocked_note") {
            let listed = evidence
                .get("approved_date_blocked")
                .and_then(Value::as_array)
                .is_some_and(|list| list.contains(blocked));
            note(&mut bad, id, listed, || {
                format!("approved_date_blocked {evidence}")
            });
        }
        let applied = expect["approval_applied"].as_bool().unwrap();
        note(
            &mut bad,
            id,
            evidence.get("approved_date_decision").is_some() == applied,
            || format!("approval applied {evidence}"),
        );
        note(
            &mut bad,
            id,
            found.gate.approval.is_some() == applied,
            || "GateMark.approval".to_owned(),
        );
        let not_applied = expect["approval_not_applied"].as_bool().unwrap();
        note(
            &mut bad,
            id,
            evidence.get("approval_not_applied").is_some() == not_applied,
            || format!("approval_not_applied {evidence}"),
        );
        if not_applied {
            let document = expected_document();
            let decision = if entry["dataset"] == "admrules" {
                &document["decisions"]["f2"]
            } else {
                &document["decisions"]["1063983"]
            };
            let value = json!({"reason": document["approval_not_applied_reason"], "decision": decision, "observed": expect["observed"], "expected": expect["expected"]});
            note(
                &mut bad,
                id,
                evidence.get("approval_not_applied") == Some(&value),
                || format!("approval_not_applied value {evidence}"),
            );
            // Nothing of the approval stays: no selection key, no blocked flag, no consumed evidence.
            note(
                &mut bad,
                id,
                approval_keys(&found).is_empty()
                    && !found.gate.approval_blocked
                    && !found.gate.approval_mismatch,
                || format!("selection state {evidence}"),
            );
            note(
                &mut bad,
                id,
                report.first().is_some_and(|row| row["consumed_spans"] == 0),
                || "consumed evidence C is empty".to_owned(),
            );
            // The result is the chain result as it stands: the same input that no approval matches
            // (another identity) gives the same occurrence, apart from the new key.
            let mut other = source.clone();
            other.identity.push('9');
            let chain = resolve(dataset, &other).unwrap();
            let mut mine = found.date_evidence.clone();
            mine.as_object_mut().unwrap().remove("approval_not_applied");
            note(&mut bad, id, mine == chain.date_evidence, || {
                format!(
                    "evidence differs from the chain: {mine} / {}",
                    chain.date_evidence
                )
            });
            note(
                &mut bad,
                id,
                (
                    &found.status,
                    &found.enforcement_date,
                    &found.exposure_date,
                    &found.reason,
                ) == (
                    &chain.status,
                    &chain.enforcement_date,
                    &chain.exposure_date,
                    &chain.reason,
                ),
                || "state differs from the chain".to_owned(),
            );
            let marks = |gate: &GateMark| {
                (
                    gate.text_derived,
                    gate.by_default,
                    gate.any_rule,
                    gate.any_default,
                    gate.provenance_error,
                    gate.path,
                )
            };
            note(
                &mut bad,
                id,
                marks(&found.gate) == marks(&chain.gate),
                || "gate marks differ from the chain".to_owned(),
            );
        }
        bad
    }

    fn assert_group(prefix: &str) {
        let mut bad = Vec::new();
        for id in expected_ids(prefix) {
            bad.extend(problems(&id));
        }
        assert!(
            bad.is_empty(),
            "{} problems:\n{}",
            bad.len(),
            bad.join("\n")
        );
    }

    /// B1: a read-nothing sentence on the same line right after the approved sentence is no
    /// consumed evidence: the date is held and the evidence stays.
    #[test]
    fn approval_b1_unread_sentence_on_the_line_of_the_approved_sentence_holds() {
        assert_group("b1-");
    }

    /// B1: the consumed span is exactly the source range of the approved sentence. The expected
    /// range comes from the raw pieces, not from the selection code: the piece that holds the
    /// sentence text, the offset of the text in it and the byte length of the sentence.
    #[test]
    fn approval_b1_consumed_span_is_exactly_the_approved_sentence() {
        let document = expected_document();
        for id in [
            "real-2200000088451",
            "real-2200000104677",
            "real-1063983",
            "b1-f2a-same-line-unread",
            "b1-f2b-same-line-unread",
            "b1-1063983-same-line-unread",
        ] {
            let (entry, source, _) = built(id);
            let revision = entry["revision"].as_str().unwrap();
            let sentence = entry["approved_sentence"].as_str().unwrap();
            let length = document["sentence_utf8_bytes"][revision].as_u64().unwrap() as usize;
            assert_eq!(sentence.len(), length, "{id}: the length of the sentence");
            // The raw views of the fields that hold the sentence.
            let found = date_text_with(&source.xml, true).unwrap();
            let view = found.source_view.as_ref().unwrap();
            let mut raws: Vec<&SourceSegments> = Vec::new();
            let fields: Vec<&str> = match entry.get("sentence_fields") {
                Some(list) => list
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|x| x.as_str().unwrap())
                    .collect(),
                None => vec![entry["sentence_field"].as_str().unwrap()],
            };
            for field in fields {
                match field {
                    "개정문내용" => raws.push(&view.declaration),
                    "제개정이유내용" => raws.push(&view.amendment),
                    "부칙내용" => raws.extend(view.addenda.iter()),
                    other => panic!("{other}"),
                }
            }
            let mut expected: Vec<(u64, u64, usize, usize)> = Vec::new();
            for raw in raws {
                for (at, _) in raw.text.match_indices(sentence) {
                    let segment = raw
                        .segments
                        .iter()
                        .find(|segment| {
                            segment["source"].is_object()
                                && segment["output_byte_range"][0].as_u64().unwrap() as usize <= at
                                && at + length
                                    <= segment["output_byte_range"][1].as_u64().unwrap() as usize
                        })
                        .unwrap_or_else(|| panic!("{id}: no piece holds the sentence at {at}"));
                    let piece_start =
                        segment["source"]["piece_byte_range"][0].as_u64().unwrap() as usize + at
                            - segment["output_byte_range"][0].as_u64().unwrap() as usize;
                    expected.push((
                        segment["source"]["element_id"].as_u64().unwrap(),
                        segment["source"]["event_id"].as_u64().unwrap(),
                        piece_start,
                        piece_start + length,
                    ));
                }
            }
            assert!(!expected.is_empty(), "{id}");
            expected.sort();
            // The selection of the product.
            let mut found = date_text_with(&source.xml, true).unwrap();
            let collected = found.source_view.take().unwrap();
            let amendment = decode_markup(&found.amendment);
            let declaration = decode_markup(&found.declaration);
            for block in &mut found.addenda {
                block.raw = std::mem::take(&mut block.text);
                block.text = decode_markup(&block.raw);
            }
            let selection = untraced(|| {
                gated_approval(
                    entry["dataset"].as_str().unwrap(),
                    &source,
                    &collected,
                    &found.addenda,
                    &amendment,
                    &declaration,
                )
            })
            .unwrap_or_else(|| panic!("{id}: no selection"));
            let mut spans = selection.spans.clone();
            spans.sort();
            assert_eq!(
                spans, expected,
                "{id}: the consumed span is the sentence and nothing else"
            );
            let mut evidenced: Vec<(u64, u64, usize, usize)> = selection
                .found
                .iter()
                .flat_map(|item| item["source_ranges"].as_array().unwrap().iter())
                .map(|range| {
                    (
                        range[0].as_u64().unwrap(),
                        range[1].as_u64().unwrap(),
                        range[2].as_u64().unwrap() as usize,
                        range[3].as_u64().unwrap() as usize,
                    )
                })
                .collect();
            evidenced.sort();
            assert_eq!(evidenced, expected, "{id}: the evidence ranges");
            assert!(
                spans.iter().all(|span| span.3 - span.2 == length),
                "{id}: each range has the byte length of the sentence"
            );
        }
    }

    /// B2: the range of the approved sentence does not depend on the order of the sentences
    /// around it.
    #[test]
    fn approval_b2_selection_range_is_symmetric_and_bounded_by_the_box_section() {
        assert_group("b2-");
    }

    /// B2: the heading on the same line as the approved sentence (a real input) keeps the range
    /// inside the addendum of the sentence: the 2005 section stays out.
    #[test]
    fn approval_b2_heading_on_the_line_of_the_sentence_is_the_section_start() {
        assert_group("real-1063983");
        let text = "부　 칙 (2005.06.07) 이 규칙은 공포한 날부터 시행한다.\n부　 칙 (2007.05.30) 이 규칙은 2007년 6월 1일부터 시행한다.\n";
        let at = text.find("이 규칙은 2007년").unwrap();
        let second = text.find("부　 칙 (2007").unwrap();
        assert_eq!(sentence_section(text, at), (second, text.len()));
        // Without a heading before the sentence: the box section, whatever the order.
        let text = "가 문장\n□ 하나\n나 문장\n다 문장\n□ 둘\n라 문장\n";
        let at = text.find("나 문장").unwrap();
        assert_eq!(
            &text[sentence_section(text, at).0..sentence_section(text, at).1],
            "□ 하나\n나 문장\n다 문장\n"
        );
        let text = "가 문장\n나 문장\n부칙\n다 문장\n";
        let at = text.find("나 문장").unwrap();
        assert_eq!(
            sentence_section(text, at),
            (0, text.len()),
            "no box line: the whole field, a later heading does not cut it"
        );
    }

    /// B3: a revision whose rule chain result differs from the registered one gets no approval.
    #[test]
    fn approval_b3_a_chain_result_that_differs_from_the_registration_applies_no_approval() {
        assert_group("b3-");
    }

    /// B3, second control (N7 of the final execution verification): a chain result that differs
    /// from the registration applies no approval, whatever the shape of the input. Every case
    /// also needs the date of the approval to stay out of the result.
    #[test]
    fn approval_b3_second_control_a_differing_chain_result_applies_no_approval() {
        assert_group("b3x-");
        assert_eq!(expected_ids("b3x-").len(), 4);
        for id in expected_ids("b3x-") {
            let (entry, source, _) = built(&id);
            let found = resolve(entry["dataset"].as_str().unwrap(), &source).unwrap();
            assert!(
                found.date_evidence.get("approval_not_applied").is_some(),
                "{id}"
            );
            assert_ne!(
                found.enforcement_date.as_deref(),
                Some(match entry["revision"].as_str().unwrap() {
                    "2200000088451" => "2009-06-27",
                    _ => "1998-10-07",
                }),
                "{id}: the approved date must not apply"
            );
        }
    }

    /// NB1: a subjectless date sentence or phrase inside the selection range does not hold the
    /// approval (A8 of F2a and F2b, C9 and C10 of MST 1063983). The expected values are in
    /// `nb1-expected.json`, fixed before the cases.
    #[test]
    fn approval_nb1_subjectless_other_date_in_the_selection_range_applies() {
        assert_group("nb1-");
        assert_eq!(expected_ids("nb1-").len(), 4);
    }

    /// The side that is not blocked, and two sides that stay blocked.
    #[test]
    fn approval_applies_when_the_other_date_is_out_of_range_and_not_unread() {
        assert_group("d-");
    }

    /// The real sources: the occurrences equal the output of the previous binary (bin-p9r4), and
    /// no `approval_not_applied` key exists.
    #[test]
    fn approval_real_sources_equal_the_previous_binary_output() {
        assert_group("real-");
        let document = expected_document();
        for id in expected_ids("real-") {
            let (entry, source, _) = built(&id);
            let found = resolve(entry["dataset"].as_str().unwrap(), &source).unwrap();
            let mut actual = serde_json::to_value(&found).unwrap();
            for key in ["promulgation_rank", "enforcement_rank"] {
                actual.as_object_mut().unwrap().remove(key);
            }
            assert_eq!(
                actual,
                document["bin_p9r4_occurrences"][entry["revision"].as_str().unwrap()],
                "{id}: the whole occurrence"
            );
            assert!(
                found.date_evidence.get("approval_not_applied").is_none(),
                "{id}"
            );
        }
    }

    /// K5: a revision with an approval is not exempt from the gate. The gate computes its marks for
    /// the approved revisions and for the not-applied control B5, the date is text derived, and
    /// the gate withholds the date when its marks are set.
    #[test]
    fn approval_k5_the_gate_is_not_exempt_for_approved_revisions() {
        let k5 = expected_document()["k5"].clone();
        for id in [
            "real-2200000088451",
            "real-2200000104677",
            "real-1063983",
            "b3-b5-f2a",
        ] {
            let (entry, source, _) = built(id);
            GATE_REPORT.with(|slot| *slot.borrow_mut() = Some(Vec::new()));
            let found = resolve(entry["dataset"].as_str().unwrap(), &source).unwrap();
            let report = GATE_REPORT.with(|slot| slot.borrow_mut().take()).unwrap();
            assert_eq!(
                found.gate.text_derived,
                k5["text_derived"].as_bool().unwrap(),
                "{id}: text derived"
            );
            assert_eq!(
                !report.is_empty(),
                k5["gate_runs"].as_bool().unwrap(),
                "{id}: the gate computed its marks"
            );
            assert!(
                matches!(found.status.as_str(), "confirmed" | "estimated"),
                "{id}"
            );
            let mut forced = found.clone();
            forced.gate.any_rule = true;
            forced.gate.any_default = true;
            let withheld = forced.apply_gate();
            assert_eq!(
                !withheld,
                k5["gate_exempt"].as_bool().unwrap(),
                "{id}: the gate withholds when its marks are set"
            );
            assert_eq!(forced.status, "unresolved", "{id}");
        }
    }

    /// K1: the value of `date_withheld` when several causes meet.
    #[test]
    fn approval_k1_date_withheld_priority() {
        let (entry, source, _) = built("real-2200000088451");
        let found = resolve(entry["dataset"].as_str().unwrap(), &source).unwrap();
        for row in expected_document()["date_withheld_priority"]
            .as_array()
            .unwrap()
        {
            let mut held = found.clone();
            held.gate.provenance_error = row["provenance_error"].as_bool().unwrap();
            held.gate.approval_mismatch = row["approval_mismatch"].as_bool().unwrap();
            held.gate.approval_blocked = row["approval_blocked"].as_bool().unwrap();
            held.withhold();
            assert_eq!(
                held.date_evidence["date_withheld"], row["date_withheld"],
                "{row}"
            );
        }
    }

    fn fixture_body(case: &str) -> (DateText, SourceView) {
        let manifest: Value =
            serde_json::from_slice(&std::fs::read(fixture("bfull/input-manifest.json")).unwrap())
                .unwrap();
        let path = manifest[case]["path"].as_str().unwrap();
        let bytes = std::fs::read(fixture(&format!("bfull/{path}"))).unwrap();
        assert_eq!(sha256(&bytes), manifest[case]["sha256"].as_str().unwrap());
        let mut found = date_text(&bytes).unwrap();
        let view = found.source_view.take().unwrap();
        (found, view)
    }

    #[test]
    fn v3_line_separators_stay_payload() {
        // generalref_nel_ls: U+0085 and U+2028 are GeneralRef pieces of the same run; N removes only
        // the event line feeds between the pieces and keeps the two characters as payload.
        let (found, view) = fixture_body("generalref_nel_ls");
        assert_eq!(
            found.body,
            "이 조례\n\u{85}\n는 공포한 날부터 시행한다.\n\u{2028}\n끝\n"
        );
        let n = normalized_raw(&view.body).unwrap();
        assert_eq!(
            n.text,
            "이 조례\u{85}는 공포한 날부터 시행한다.\u{2028}끝\n"
        );
        assert_eq!(
            n.text.lines().count(),
            1,
            "no line split at U+0085 or U+2028"
        );
        let units = view_units(&n).unwrap();
        for ch in ['\u{85}', '\u{2028}'] {
            let at = n.text.find(ch).unwrap();
            let unit = units.iter().find(|unit| unit.start == at).unwrap();
            assert_eq!(unit.sources.len(), 1);
            let segment = view
                .body
                .segments
                .iter()
                .find(|s| s["source"]["event_id"].as_u64() == Some(unit.sources[0].1))
                .unwrap();
            assert_eq!(
                segment["event_kind"], "GeneralRef",
                "{ch:?} keeps its GeneralRef origin"
            );
        }
        // ps_cr_nbsp_text: the line feed that the parser returned is a payload character and stays.
        let (found, view) = fixture_body("ps_cr_nbsp_text");
        assert_eq!(
            found.body,
            "제1조 목적\u{2029}이 조례는 공포한 날부터 시행한다.\n\u{a0}끝\n"
        );
        let n = normalized_raw(&view.body).unwrap();
        assert_eq!(n.text, found.body);
        let at = n.text.find(".\n").unwrap() + 1;
        assert!(
            !view_units(&n)
                .unwrap()
                .iter()
                .find(|unit| unit.start == at)
                .unwrap()
                .sources
                .is_empty(),
            "parser LF has a source"
        );
        // ps_cr_nbsp_cdata: the lone CR and U+2029 stay payload characters and split no line.
        let (found, view) = fixture_body("ps_cr_nbsp_cdata");
        assert_eq!(found.addenda.len(), 1);
        assert_eq!(
            found.addenda[0].text,
            "부칙\u{2029}이 조례는 공포한 날부터 시행한다.\r\u{a0}끝"
        );
        let n = normalized_raw(&view.addenda[0]).unwrap();
        assert_eq!(n.text, found.addenda[0].text);
        assert_eq!(n.text.lines().count(), 1, "no line split at CR or U+2029");
    }
}

/// P9 equivalence tests of the optimizations: the segment index gives the units of the eager
/// reference for every range and point, and the first character of each matcher follows its pattern.
#[cfg(test)]
mod p9_tests {
    use super::*;

    fn views() -> Vec<SourceSegments> {
        let mut out = Vec::new();
        for xml in [
            "<LawService><본문><![CDATA[이 조례]]><![CDATA[는 &lt;시행&gt;한다.]]></본문></LawService>",
            "<LawService><본문>이 조례&#40;제1호)는 시행한다.</본문></LawService>",
            "<LawService><본문><![CDATA[&l]]><![CDATA[t;]]><![CDATA[]]><![CDATA[이 조례\u{200B}는 시행한다.\r\n끝]]></본문></LawService>",
            "<LawService><제개정이유><제개정이유내용><![CDATA[부칙\n이 조례는 &amp;lt; 시행한다.\n□ 끝]]></제개정이유내용></제개정이유></LawService>",
        ] {
            let found = date_text(xml.as_bytes()).unwrap();
            let view = found.source_view.unwrap();
            for raw in [view.body, view.amendment] {
                if raw.text.is_empty() {
                    continue;
                }
                let decoded = raw.decoded().unwrap();
                let normalized = normalized_raw(&raw).unwrap();
                let normalized_decoded = normalized.decoded().unwrap();
                let joined = decoded.lines_join().unwrap();
                out.extend([raw, decoded, normalized, normalized_decoded, joined]);
            }
        }
        out
    }

    #[test]
    fn unit_index_matches_the_eager_units() {
        let mut checked = 0;
        for view in views() {
            let eager = view_units(&view).unwrap();
            let index = unit_index(&view).unwrap();
            let len = view.text.len();
            for start in 0..=len {
                let eager_point = (|| -> Result<(u64, u64, usize)> {
                    let at = eager.partition_point(|unit| unit.end <= start);
                    let unit = eager.get(at).context("candidate start outside the view")?;
                    ensure!(unit.start == start, "candidate start inside a unit");
                    let first = unit
                        .sources
                        .first()
                        .context("candidate start without source")?;
                    Ok((first.0, first.1, first.2))
                })();
                assert_eq!(
                    format!("{:?}", map_point(&index, start)),
                    format!("{eager_point:?}"),
                    "point {start} of {:?}",
                    view.text
                );
                for end in start..=len {
                    let eager_units = (|| -> Result<Vec<(usize, usize, Vec<SourceRange>)>> {
                        ensure!(start < end, "empty range");
                        let first = eager.partition_point(|unit| unit.end <= start);
                        let last = eager.partition_point(|unit| unit.start < end);
                        ensure!(first < last, "range outside the view");
                        let selected = &eager[first..last];
                        ensure!(
                            selected[0].start == start && selected[selected.len() - 1].end == end,
                            "range boundary inside a unit"
                        );
                        Ok(selected
                            .iter()
                            .map(|unit| (unit.start, unit.end, unit.sources.clone()))
                            .collect())
                    })();
                    let lazy = index.units_in(start, end).map(|units| {
                        units
                            .into_iter()
                            .map(|unit| (unit.start, unit.end, unit.sources))
                            .collect::<Vec<_>>()
                    });
                    assert_eq!(
                        format!("{lazy:?}"),
                        format!("{eager_units:?}"),
                        "range {start}..{end} of {:?}",
                        view.text
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked > 1000, "{checked}");
    }

    /// The first character of each matcher is the literal right after `\A` (and an optional opening
    /// group) of its pattern. Any other start of the pattern fails this test. A brute-force check over
    /// the Basic Multilingual Plane confirms that no other first character matches.
    #[test]
    fn matcher_first_characters_follow_the_patterns() {
        let completions = [
            ("korean_stem", "행한다"),
            ("terminal", "행"),
            ("effect", "력을 발생"),
            ("from_effect", "터 효력"),
            ("hanja", "行"),
            ("phrase_from_enforcement", "터 시행"),
        ];
        for matcher in VERB_MATCHERS.iter().chain(PHRASE_MATCHERS.iter()) {
            let rest = matcher
                .pattern
                .strip_prefix(r"\A")
                .expect("anchored pattern");
            let rest = rest.strip_prefix('(').unwrap_or(rest);
            let derived = rest.chars().next().unwrap();
            assert!(
                !r"\[](){}|.*+?^$".contains(derived),
                "{}: the pattern starts with a literal",
                matcher.name
            );
            assert!(
                !rest[derived.len_utf8()..].starts_with(['*', '?', '{']),
                "{}: the first literal is required",
                matcher.name
            );
            assert!(
                !rest.contains('|') || rest.find('|') > rest.find("(?:"),
                "{}: no top-level alternation",
                matcher.name
            );
            assert_eq!(matcher.first, derived, "{}", matcher.name);
            let completion = completions
                .iter()
                .find(|(name, _)| *name == matcher.name)
                .unwrap()
                .1;
            assert!(
                matcher.regex.is_match(&format!("{derived}{completion}")),
                "{}",
                matcher.name
            );
            for code in 0..=0xFFFF_u32 {
                let Some(ch) = char::from_u32(code) else {
                    continue;
                };
                if ch != derived {
                    assert!(
                        !matcher.regex.is_match(&format!("{ch}{completion}")),
                        "{} starts with {ch:?}",
                        matcher.name
                    );
                }
            }
        }
    }
}

/// P8 profiler (test only): the time of `resolve_rules` per source and the cost of each part of the
/// gate evidence, measured by calling the parts again on the same partitions.
#[cfg(test)]
mod profile_tests {
    use super::*;
    use std::time::Instant;

    fn micros(start: Instant) -> u64 {
        start.elapsed().as_micros() as u64
    }

    fn amendment_label(xml: &str, dataset: &str) -> String {
        let tags: &[&str] = if dataset == "ordinances" {
            &["제개정정보", "제개정구분명", "제개정구분"]
        } else {
            &["제개정구분명", "제개정구분"]
        };
        for tag in tags {
            let open = format!("<{tag}>");
            if let Some(at) = xml.find(&open) {
                let rest = &xml[at + open.len()..];
                if let Some(end) = rest.find('<') {
                    let value = rest[..end].trim();
                    if !value.is_empty() {
                        return value.to_owned();
                    }
                }
            }
        }
        String::new()
    }

    /// The text parse of `date_text` without the SourceView (the collection of r24).
    fn plain_parse(xml: &[u8]) -> usize {
        let mut reader = Reader::from_reader(xml);
        let mut stack: Vec<String> = Vec::new();
        let mut current: Option<Addendum> = None;
        let mut found = DateText::default();
        loop {
            let event = reader.read_event().unwrap();
            let piece: Option<String> = match &event {
                Event::Text(text) => Some(
                    quick_xml::escape::unescape(&text.xml_content().unwrap())
                        .unwrap()
                        .into_owned(),
                ),
                Event::CData(text) => Some(text.decode().unwrap().into_owned()),
                Event::GeneralRef(reference) => {
                    Some(if let Some(ch) = reference.resolve_char_ref().unwrap() {
                        ch.to_string()
                    } else {
                        quick_xml::escape::unescape(&format!("&{};", reference.decode().unwrap()))
                            .unwrap()
                            .into_owned()
                    })
                }
                _ => None,
            };
            if let Some(piece) = piece {
                collect_date_text(&stack, &piece, &mut current, &mut found);
                continue;
            }
            match event {
                Event::Start(event) => {
                    let name = String::from_utf8(event.name().as_ref().to_vec()).unwrap();
                    if name == "부칙단위" || (name == "부칙내용" && current.is_none()) {
                        current = Some(Addendum::default());
                    }
                    stack.push(name);
                }
                Event::End(event) => {
                    let name = String::from_utf8(event.name().as_ref().to_vec()).unwrap();
                    if (name == "부칙단위"
                        || (name == "부칙내용" && !stack.iter().any(|part| part == "부칙단위")))
                        && current.is_some()
                    {
                        current = None;
                    }
                    stack.pop();
                }
                Event::Eof => break,
                _ => {}
            }
        }
        found.body.len()
    }

    #[derive(Default)]
    struct Cost {
        build: u64,
        units: u64,
        r24: u64,
        recheck: u64,
        bracket: u64,
        phrase: u64,
        views: u64,
        chars: u64,
        max_view_chars: u64,
    }

    impl Cost {
        fn view(&mut self, view: &SourceSegments) {
            let text = view.text.as_str();
            let started = Instant::now();
            let units = unit_index(view)
                .map(|units| units.segments.len())
                .unwrap_or(0);
            self.units += micros(started);
            self.chars += units as u64;
            self.max_view_chars = self.max_view_chars.max(units as u64);
            let started = Instant::now();
            let mut errors = Vec::new();
            let _ = r24_evidence(text, false, &mut errors);
            self.r24 += micros(started);
            let started = Instant::now();
            let _ = detected_sentence_starts(text, false);
            self.recheck += micros(started);
            let started = Instant::now();
            for (_, end) in bracket_subject_spans(text) {
                let tail = &text[end..];
                let _ = fragment_verbs(&tail[..sentence_end(tail)], end, &mut errors);
            }
            self.bracket += micros(started);
            let started = Instant::now();
            let _ = fixed_start_matches(text, &PHRASE_MATCHERS);
            self.phrase += micros(started);
            self.views += 1;
        }

        fn container(&mut self, raw: &SourceSegments) {
            let started = Instant::now();
            let decoded = raw.decoded().unwrap();
            let normalized = normalized_raw(raw).unwrap();
            let normalized_decoded = normalized.decoded().unwrap();
            self.build += micros(started);
            for view in [raw, &decoded, &normalized, &normalized_decoded] {
                self.view(view);
            }
        }

        fn amendment(&mut self, raw: &SourceSegments, text: &str, raw_text: &str, extracted: bool) {
            let started = Instant::now();
            let decoded = raw.decoded().unwrap();
            let mut views = Vec::new();
            for part in amendment_parts(text, raw_text, extracted) {
                if let Ok((a, b)) = part_views(&decoded, raw, &part) {
                    views.push(a);
                    views.push(b);
                }
            }
            let normalized = normalized_raw(raw).unwrap();
            let normalized_decoded = normalized.decoded().unwrap();
            for part in amendment_parts(&normalized_decoded.text, &normalized.text, extracted) {
                if let Ok((a, b)) = part_views(&normalized_decoded, &normalized, &part) {
                    views.push(a);
                    views.push(b);
                }
            }
            self.build += micros(started);
            for view in &views {
                self.view(view);
            }
        }
    }

    /// `PROFILE_MANIFEST` (a JSON list of cases) and `PROFILE_OUT` (JSON lines). Each case: dataset,
    /// path, run and the metadata of its baseline occurrence.
    #[test]
    fn profile_sources() {
        let (Some(manifest), Some(out)) = (
            std::env::var_os("PROFILE_MANIFEST"),
            std::env::var_os("PROFILE_OUT"),
        ) else {
            return;
        };
        use std::io::Write;
        let cases: Value = serde_json::from_slice(&std::fs::read(manifest).unwrap()).unwrap();
        let mut file = std::io::BufWriter::new(std::fs::File::create(out).unwrap());
        let components = std::env::var_os("PROFILE_COMPONENTS").is_some();
        for case in cases.as_array().unwrap() {
            let path = case["path"].as_str().unwrap();
            let Ok(xml) = std::fs::read(path) else {
                writeln!(
                    file,
                    "{}",
                    json!({"run":case["run"], "path":path, "error":"read"})
                )
                .unwrap();
                continue;
            };
            let dataset = case["dataset"].as_str().unwrap();
            let text = String::from_utf8_lossy(&xml);
            let meta = &case["meta"];
            let field = |key: &str| meta[key].as_str().unwrap_or("").to_owned();
            let amendment = amendment_label(&text, dataset);
            let source = SourceRevision {
                identity: field("identity"),
                revision: field("revision"),
                name: "p".to_owned(),
                category: field("category"),
                canonical_path: "p.md".to_owned(),
                source_path: field("source_path"),
                xml: xml.clone(),
                promulgation: field("promulgation"),
                enforcement: field("enforcement"),
                repeal: amendment.contains("폐지"),
                amendment,
                markdown: Vec::new(),
                secondary_name: String::new(),
                secondary_number: 0,
                serial: 0,
                copy_priority: 0,
                copy_conflict: None,
            };
            let started = Instant::now();
            let resolved = resolve_rules(dataset, &source);
            let resolve = micros(started);
            let mut row = json!({"run":case["run"], "path":path, "bytes":xml.len(), "resolve_us":resolve, "ok":resolved.is_ok(),
                "gated": resolved.as_ref().map(|o| o.gate.text_derived).unwrap_or(false)});
            if components {
                let started = Instant::now();
                let found = date_text(&xml);
                row["date_text_us"] = json!(micros(started));
                let started = Instant::now();
                let _ = plain_parse(&xml);
                row["plain_parse_us"] = json!(micros(started));
                if let Ok(found) = found {
                    let view = found.source_view.as_ref().unwrap();
                    row["sourceview_segments"] = json!(
                        view.body.segments.len()
                            + view.amendment.segments.len()
                            + view.outside.segments.len()
                            + view.declaration.segments.len()
                            + view.addenda.iter().map(|a| a.segments.len()).sum::<usize>()
                    );
                    let body_raw = found.body.clone();
                    let body = decode_markup(&body_raw);
                    let amendment_raw = found.amendment.clone();
                    let amendment = decode_markup(&amendment_raw);
                    let started = Instant::now();
                    let mut legacy = legacy_added_any(&body, &body_raw);
                    for block in &found.addenda {
                        legacy |= legacy_added_any(&decode_markup(&block.text), &block.text);
                    }
                    legacy |= legacy_split_any(&found.split, None);
                    for raw in [&found.outside, &found.declaration] {
                        if !raw.trim().is_empty() {
                            legacy |= legacy_gate_only_any(&decode_markup(raw), raw).1;
                        }
                    }
                    legacy |= legacy_amendment_parts_any(&amendment, &amendment_raw, false).1;
                    row["legacy_gate_us"] = json!(micros(started));
                    row["legacy_any"] = json!(legacy);
                    let started = Instant::now();
                    let mut partitions = 0;
                    let _ = container_evidence(
                        "body_gate",
                        "body".to_owned(),
                        &view.body,
                        &body,
                        &body_raw,
                        &[],
                    );
                    partitions += 1;
                    for (index, block) in found.addenda.iter().enumerate() {
                        let _ = block_evidence(
                            view,
                            index,
                            &decode_markup(&block.text),
                            &block.text,
                            &[],
                        );
                        partitions += 1;
                    }
                    for (family, raw_text, field_view) in [
                        ("outside_gate", &found.outside, &view.outside),
                        ("declaration_gate", &found.declaration, &view.declaration),
                    ] {
                        if !raw_text.trim().is_empty() {
                            let _ = container_evidence(
                                family,
                                family.to_owned(),
                                field_view,
                                &decode_markup(raw_text),
                                raw_text,
                                &[],
                            );
                            partitions += 1;
                        }
                    }
                    let parts = amendment_parts(&amendment, &amendment_raw, false);
                    let _ = amendment_evidence(
                        &amendment,
                        &amendment_raw,
                        false,
                        &view.amendment,
                        &parts,
                        &[],
                    );
                    row["engine_us"] = json!(micros(started));
                    row["partitions"] = json!(partitions + 1);
                    let mut cost = Cost::default();
                    cost.container(&view.body);
                    for block in &view.addenda {
                        cost.container(block);
                    }
                    for (raw_text, field_view) in [
                        (&found.outside, &view.outside),
                        (&found.declaration, &view.declaration),
                    ] {
                        if !raw_text.trim().is_empty() {
                            cost.container(field_view);
                        }
                    }
                    cost.amendment(&view.amendment, &amendment, &amendment_raw, false);
                    row["parts"] = json!({"build_us":cost.build, "units_us":cost.units, "r24_us":cost.r24, "recheck_us":cost.recheck,
                        "bracket_us":cost.bracket, "phrase_us":cost.phrase, "views":cost.views, "chars":cost.chars, "max_view_chars":cost.max_view_chars,
                        "unit_bytes_estimate_max_view": cost.max_view_chars * (std::mem::size_of::<ViewUnit>() as u64 + 48)});
                }
            }
            writeln!(file, "{row}").unwrap();
        }
        file.flush().unwrap();
    }
}

/// The rule that leaves a revision unresolved when a later-promulgated repeal takes effect before
/// it (design 9, user decision `user-repeal-revival-unresolved-20261117`). The expected values are
/// fixed in `repeal-impl-20261117/expectations-20261117.md`, before this code.
#[cfg(test)]
mod repeal_revival_tests {
    use super::*;

    const REASON: &str = "effective date is unresolved; a later-promulgated repeal takes effect before this revision";
    const DECISION: &str = "user-repeal-revival-unresolved-20261117";

    /// "2020-06-01" as "2020년 6월 1일".
    fn korean(date: &str) -> String {
        let part = |range: std::ops::Range<usize>| date[range].parse::<u32>().unwrap();
        format!("{}년 {}월 {}일", part(0..4), part(5..7), part(8..10))
    }

    /// An ordinance revision of identity "1" with the given addendum and article text.
    fn ordinance(
        revision: &str,
        promulgation: &str,
        addendum: &str,
        body: &str,
        repeal: bool,
    ) -> SourceRevision {
        let number: u64 = revision.parse().unwrap();
        SourceRevision {
            identity: "1".to_owned(),
            revision: revision.to_owned(),
            name: "test".to_owned(),
            category: "조례".to_owned(),
            canonical_path: "p.md".to_owned(),
            source_path: format!("{revision}.xml"),
            xml: format!(
                "<LawService><공포번호>{number}</공포번호><조문><조><조내용><![CDATA[{body}]]></조내용></조></조문><부칙><부칙내용><![CDATA[{addendum}]]></부칙내용></부칙></LawService>"
            )
            .into_bytes(),
            promulgation: promulgation.replace('-', ""),
            enforcement: "99991231".to_owned(),
            amendment: "제정".to_owned(),
            repeal,
            markdown: format!("---\nname: {revision}\n---\nbody\n").into_bytes(),
            secondary_name: String::new(),
            secondary_number: 0,
            serial: number,
            copy_priority: 0,
            copy_conflict: None,
        }
    }

    /// An ordinance revision that the rules confirm at `effective` (an explicit date sentence).
    fn dated(revision: &str, promulgation: &str, effective: &str, repeal: bool) -> SourceRevision {
        ordinance(
            revision,
            promulgation,
            &format!("이 조례는 {}부터 시행한다.", korean(effective)),
            "본문",
            repeal,
        )
    }

    /// An administrative rule whose valid source date confirms it.
    fn adm_confirmed(
        revision: &str,
        promulgation: &str,
        effective: &str,
        repeal: bool,
    ) -> SourceRevision {
        let mut source = ordinance(revision, promulgation, "", "", repeal);
        source.category = "고시".to_owned();
        source.xml = b"<AdmRulService><\xec\xa1\xb0\xeb\xac\xb8\xeb\x82\xb4\xec\x9a\xa9>x</\xec\xa1\xb0\xeb\xac\xb8\xeb\x82\xb4\xec\x9a\xa9></AdmRulService>".to_vec();
        source.enforcement = effective.replace('-', "");
        source
    }

    /// An administrative rule of an arrival-estimate kind: estimated at its promulgation date.
    fn adm_estimated(revision: &str, promulgation: &str, repeal: bool) -> SourceRevision {
        let mut source = ordinance(revision, promulgation, "", "", repeal);
        source.category = "등기예규".to_owned();
        source.amendment = "제정".to_owned();
        source.xml = "<AdmRulService><조문내용>시행규칙에 따른다.</조문내용></AdmRulService>"
            .as_bytes()
            .to_vec();
        source
    }

    fn plan_of(dataset: &str, sources: &[SourceRevision], enforcement: bool) -> DatePlan {
        DatePlan::new(dataset, enforcement, sources.to_vec()).unwrap()
    }

    fn found<'a>(plan: &'a DatePlan, path: &str) -> &'a Occurrence {
        let source = plan
            .sources
            .iter()
            .position(|source| source.source_path == path)
            .unwrap_or_else(|| panic!("no source {path}"));
        plan.occurrence(source)
    }

    /// The state of one occurrence: status and effective date.
    fn state(plan: &DatePlan, path: &str) -> (String, Option<String>) {
        let occurrence = found(plan, path);
        (
            occurrence.status.clone(),
            occurrence.enforcement_date.clone(),
        )
    }

    fn untouched(occurrence: &Occurrence) {
        assert!(
            occurrence
                .date_evidence
                .get("repeal_before_effective")
                .is_none(),
            "{}",
            occurrence.date_evidence
        );
        assert_ne!(
            occurrence.date_evidence.get("date_withheld"),
            Some(&json!("repeal-before-effective"))
        );
    }

    /// The five fixed values and the state of a target occurrence.
    fn assert_target(
        plan: &DatePlan,
        path: &str,
        repeal_revision: &str,
        repeal_date: &str,
        withheld_date: &str,
    ) {
        let occurrence = found(plan, path);
        assert_eq!(occurrence.status, "unresolved", "{path}");
        assert_eq!(occurrence.enforcement_date, None, "{path}");
        assert_eq!(
            occurrence.exposure_date, occurrence.promulgation_date,
            "{path}"
        );
        assert!(occurrence.promulgation_date.is_some(), "{path}");
        assert!(occurrence.undated_semantics.is_empty(), "{path}");
        assert_eq!(occurrence.reason, REASON, "{path}");
        let evidence = &occurrence.date_evidence;
        assert_eq!(evidence["rule"], REASON, "{path}");
        assert_eq!(evidence["arrival_estimate"], json!(false), "{path}");
        assert_eq!(
            evidence["date_withheld"], "repeal-before-effective",
            "{path}"
        );
        assert_eq!(
            evidence["repeal_before_effective"],
            json!({
                "user_decision": DECISION,
                "repeal_revision": repeal_revision,
                "repeal_enforcement_date": repeal_date,
                "withheld_enforcement_date": withheld_date,
                "order_key": ["promulgation_date", "serial", "revision"],
            }),
            "{path}"
        );
        // The first occurrence stands for its revision.
        let revision = &plan.revisions[plan.source_revision[plan
            .sources
            .iter()
            .position(|source| source.source_path == path)
            .unwrap()]];
        assert_eq!(revision.status.as_deref(), Some("unresolved"), "{path}");
        assert_eq!(revision.enforcement_date, None, "{path}");
        assert_eq!(revision.reason, REASON, "{path}");
        assert_eq!(
            revision.date_evidence,
            revision.occurrences[0].date_evidence
        );
    }

    /// The enforcement plan equals the plan of the default basis in every revision, rank and
    /// evidence key: the rule changed nothing. `states` pins the source states of the case, so
    /// that both plans cannot agree on a wrong input.
    fn assert_no_change(dataset: &str, sources: &[SourceRevision], states: &[(&str, &str, &str)]) {
        let enforcement = plan_of(dataset, sources, true);
        let default = plan_of(dataset, sources, false);
        for (path, status, date) in states {
            assert_eq!(
                state(&default, path),
                (
                    (*status).to_owned(),
                    (!date.is_empty()).then(|| (*date).to_owned())
                ),
                "{path}: the input is not the case under test"
            );
        }
        assert_eq!(
            serde_json::to_value(&enforcement.revisions).unwrap(),
            serde_json::to_value(&default.revisions).unwrap()
        );
        for source in &enforcement.sources {
            untouched(found(&enforcement, &source.source_path));
        }
    }

    const X_DATE: &str = "2020-06-01";
    const R_DATE: &str = "2020-04-01";

    fn x() -> SourceRevision {
        dated("1", "2020-01-10", X_DATE, false)
    }

    fn r() -> SourceRevision {
        dated("3", "2020-03-10", R_DATE, true)
    }

    #[test]
    fn c01_a_confirmed_revision_before_an_earlier_effective_repeal_is_unresolved() {
        let sources = [x(), r()];
        let default = plan_of("ordinances", &sources, false);
        assert_eq!(
            state(&default, "1.xml"),
            ("confirmed".to_owned(), Some(X_DATE.to_owned()))
        );
        assert_eq!(
            state(&default, "3.xml"),
            ("confirmed".to_owned(), Some(R_DATE.to_owned()))
        );
        let plan = plan_of("ordinances", &sources, true);
        assert_target(&plan, "1.xml", "3", R_DATE, X_DATE);
        // The repeal is the same as without the rule, and the ranks follow the new state.
        assert_eq!(
            state(&plan, "3.xml"),
            ("confirmed".to_owned(), Some(R_DATE.to_owned()))
        );
        untouched(found(&plan, "3.xml"));
        assert_eq!(found(&plan, "1.xml").enforcement_rank, Some(0));
        assert_eq!(found(&plan, "3.xml").enforcement_rank, Some(1));
    }

    #[test]
    fn c02_an_estimated_revision_is_a_target() {
        // X is estimated at its promulgation date 2020-06-01; the repeal takes effect earlier.
        let sources = [
            adm_estimated("1", "2020-06-01", false),
            adm_confirmed("3", "2020-07-01", "2020-05-15", true),
        ];
        let default = plan_of("admrules", &sources, false);
        assert_eq!(
            state(&default, "1.xml"),
            ("estimated".to_owned(), Some("2020-06-01".to_owned()))
        );
        assert_eq!(
            state(&default, "3.xml"),
            ("confirmed".to_owned(), Some("2020-05-15".to_owned()))
        );
        let plan = plan_of("admrules", &sources, true);
        assert_target(&plan, "1.xml", "3", "2020-05-15", "2020-06-01");
    }

    #[test]
    fn c03_a_revision_between_the_revision_and_the_repeal_does_not_hide_it() {
        let sources = [x(), dated("2", "2020-02-10", "2020-03-01", false), r()];
        let plan = plan_of("ordinances", &sources, true);
        assert_target(&plan, "1.xml", "3", R_DATE, X_DATE);
        assert_eq!(
            state(&plan, "2.xml"),
            ("confirmed".to_owned(), Some("2020-03-01".to_owned()))
        );
        untouched(found(&plan, "2.xml"));
    }

    #[test]
    fn c04_two_paths_of_one_copy_are_both_targets() {
        let mut twin = x();
        twin.source_path = "1-twin.xml".to_owned();
        let sources = [x(), twin, r()];
        let plan = plan_of("ordinances", &sources, true);
        assert_eq!(plan.revisions.len(), 2);
        assert_eq!(plan.revisions[0].occurrences.len(), 2);
        assert_target(&plan, "1.xml", "3", R_DATE, X_DATE);
        assert_target(&plan, "1-twin.xml", "3", R_DATE, X_DATE);
    }

    #[test]
    fn c05_of_two_copies_only_the_one_that_meets_the_condition_is_a_target() {
        let mut other = dated("1", "2020-01-10", "2020-03-15", false);
        other.source_path = "1-other.xml".to_owned();
        let sources = [x(), other, r()];
        let plan = plan_of("ordinances", &sources, true);
        assert_ne!(
            plan.revisions[plan.source_revision[0]].copy_id,
            plan.revisions[plan.source_revision[1]].copy_id
        );
        assert_target(&plan, "1.xml", "3", R_DATE, X_DATE);
        assert_eq!(
            state(&plan, "1-other.xml"),
            ("confirmed".to_owned(), Some("2020-03-15".to_owned()))
        );
        untouched(found(&plan, "1-other.xml"));
    }

    #[test]
    fn c06_among_later_repeals_the_earliest_effective_one_is_the_evidence() {
        // The first later repeal takes effect after X (no reason); the second takes effect before.
        let late = dated("3", "2020-03-10", "2020-07-01", true);
        let early = dated("4", "2020-04-10", "2020-05-01", true);
        let plan = plan_of("ordinances", &[x(), late, early], true);
        assert_target(&plan, "1.xml", "4", "2020-05-01", X_DATE);
        // Both repeals take effect before X: the earliest effective date is the evidence, not the
        // first later repeal.
        let first = dated("3", "2020-03-10", "2020-05-20", true);
        let second = dated("4", "2020-04-10", "2020-05-01", true);
        let plan = plan_of("ordinances", &[x(), first, second], true);
        assert_target(&plan, "1.xml", "4", "2020-05-01", X_DATE);
    }

    #[test]
    fn c07_every_earlier_evidence_key_stays() {
        // Keys that the cap of the next revision does not carry to a new object.
        let extra = json!({
            "decision": "synthetic-decision",
            "user_decision": "synthetic-user-decision",
            "attributed_label": {"date": "2020-06-01"},
            "approved_date_decision": "synthetic-approval",
            "approved_date_policy": "synthetic-policy",
            "approval_not_applied": {"reason": "approved-sentence-blocked", "decision": "synthetic"},
        });
        let sources = [x(), r()];
        let items = sources.iter().cloned().map(|source| {
            let mut prepared = Prepared::resolve("ordinances", &source);
            if source.revision == "1" {
                let evidence = prepared
                    .occurrence
                    .as_mut()
                    .unwrap()
                    .date_evidence
                    .as_object_mut()
                    .unwrap();
                evidence.extend(extra.as_object().unwrap().clone());
            }
            Ok((source, prepared))
        });
        let plan = DatePlan::from_prepared("ordinances", true, items).unwrap();
        assert_target(&plan, "1.xml", "3", R_DATE, X_DATE);
        let evidence = &found(&plan, "1.xml").date_evidence;
        for (key, value) in extra.as_object().unwrap() {
            assert_eq!(&evidence[key], value, "{key}");
        }
        let before = Prepared::resolve("ordinances", &x())
            .occurrence
            .unwrap()
            .date_evidence;
        for (key, value) in before.as_object().unwrap() {
            if !matches!(key.as_str(), "rule" | "arrival_estimate") {
                assert_eq!(&evidence[key], value, "{key}");
            }
        }
        assert_eq!(evidence["source_path"], "1.xml");
        assert_eq!(evidence["raw_promulgation_date"], "20200110");
    }

    #[test]
    fn c08_a_repeal_that_the_gate_withholds_is_no_reason() {
        let held = ordinance(
            "3",
            "2020-03-10",
            "이 조례는 2020년 4월 1일부터 시행한다.",
            "제2조 이 조례(제1234호)는 2025년 1월 1일 시행합니다.",
            true,
        );
        let plan = plan_of("ordinances", &[x(), held.clone()], true);
        assert_eq!(found(&plan, "3.xml").status, "unresolved");
        assert_eq!(
            found(&plan, "3.xml").date_evidence["date_withheld"],
            "added-detection-sentence"
        );
        assert_no_change(
            "ordinances",
            &[x(), held],
            &[("1.xml", "confirmed", X_DATE), ("3.xml", "unresolved", "")],
        );
        // The same repeal without the extra sentence is a reason (the control).
        let plan = plan_of("ordinances", &[x(), r()], true);
        assert_eq!(found(&plan, "1.xml").status, "unresolved");
    }

    #[test]
    fn c09_a_repeal_that_the_source_leaves_unresolved_is_no_reason() {
        let open = ordinance(
            "3",
            "2020-03-10",
            "이 조례는 의회의 승인을 받은 날 시행한다.",
            "본문",
            true,
        );
        assert_no_change(
            "ordinances",
            &[x(), open],
            &[("1.xml", "confirmed", X_DATE), ("3.xml", "unresolved", "")],
        );
    }

    #[test]
    fn c10_an_estimated_repeal_is_no_reason() {
        let sources = [
            adm_confirmed("1", "2020-01-10", X_DATE, false),
            adm_estimated("3", "2020-03-10", true),
        ];
        assert_no_change(
            "admrules",
            &sources,
            &[
                ("1.xml", "confirmed", X_DATE),
                ("3.xml", "estimated", "2020-03-10"),
            ],
        );
    }

    #[test]
    fn c11_the_same_effective_date_is_no_reason() {
        let sources = [dated("1", "2020-01-10", R_DATE, false), r()];
        assert_no_change(
            "ordinances",
            &sources,
            &[
                ("1.xml", "confirmed", R_DATE),
                ("3.xml", "confirmed", R_DATE),
            ],
        );
    }

    #[test]
    fn c12_a_revision_promulgated_after_the_repeal_is_no_target() {
        let after = dated("5", "2020-05-10", "2020-08-01", false);
        assert_no_change(
            "ordinances",
            &[r(), after],
            &[
                ("3.xml", "confirmed", R_DATE),
                ("5.xml", "confirmed", "2020-08-01"),
            ],
        );
    }

    #[test]
    fn c13_the_same_shape_without_a_repeal_is_unchanged() {
        let middle = dated("3", "2020-03-10", R_DATE, false);
        assert_no_change(
            "ordinances",
            &[x(), middle],
            &[
                ("1.xml", "confirmed", X_DATE),
                ("3.xml", "confirmed", R_DATE),
            ],
        );
    }

    #[test]
    fn c14_the_default_basis_is_unchanged() {
        let sources = [x(), r()];
        let plan = plan_of("ordinances", &sources, false);
        assert_eq!(plan.basis, "promulgation");
        let occurrence = found(&plan, "1.xml");
        assert_eq!(occurrence.status, "confirmed");
        assert_eq!(occurrence.enforcement_date.as_deref(), Some(X_DATE));
        untouched(occurrence);
        // The occurrence is the one that the resolver gave, key for key.
        let resolved = Prepared::resolve("ordinances", &sources[0])
            .occurrence
            .unwrap();
        assert_eq!(occurrence.date_evidence, resolved.date_evidence);
        assert_eq!(occurrence.reason, resolved.reason);
    }

    #[test]
    fn c15_a_repeal_that_the_cap_of_the_next_revision_holds_is_no_reason() {
        let first = |label: &str| {
            ordinance(
                "2",
                "2024-03-01",
                &format!("부칙 <{label}>\n이 조례는 공포한 날부터 시행한다."),
                "본문",
                true,
            )
        };
        let next = ordinance(
            "3",
            "2024-05-01",
            "부칙 <2024.5.1>\n이 조례는 공포한 날부터 시행한다.",
            "본문",
            false,
        );
        let earlier = dated("1", "2024-01-01", "2024-12-01", false);
        // The label date 2024-06-01 is after the promulgation of the next revision: capped.
        let capped = [earlier.clone(), first("2024.6.1"), next.clone()];
        let default = plan_of("ordinances", &capped, false);
        assert_eq!(
            found(&default, "2.xml").date_evidence["decision"],
            "label-date-after-next-revision"
        );
        assert_no_change(
            "ordinances",
            &capped,
            &[
                ("1.xml", "confirmed", "2024-12-01"),
                ("2.xml", "unresolved", ""),
            ],
        );
        // The control: the label date 2024-04-15 is before it, the repeal stands, X is a target.
        let stands = [earlier, first("2024.4.15"), next];
        let plan = plan_of("ordinances", &stands, true);
        assert_eq!(
            state(&plan, "2.xml"),
            ("confirmed".to_owned(), Some("2024-04-15".to_owned()))
        );
        assert_target(&plan, "1.xml", "2", "2024-04-15", "2024-12-01");
    }

    #[test]
    fn c16_a_repeal_revision_is_no_target() {
        let first_repeal = dated("1", "2020-01-10", X_DATE, true);
        assert_no_change(
            "ordinances",
            &[first_repeal, r()],
            &[
                ("1.xml", "confirmed", X_DATE),
                ("3.xml", "confirmed", R_DATE),
            ],
        );
    }

    #[test]
    fn c17_a_revision_without_a_promulgation_date_has_no_order_key() {
        let mut undated = x();
        undated.promulgation = String::new();
        let plan = plan_of("ordinances", &[undated.clone(), r()], false);
        assert_eq!(found(&plan, "1.xml").promulgation_date, None);
        assert_eq!(found(&plan, "1.xml").status, "confirmed");
        assert_no_change(
            "ordinances",
            &[undated, r()],
            &[
                ("1.xml", "confirmed", X_DATE),
                ("3.xml", "confirmed", R_DATE),
            ],
        );
    }

    /// The three real sources of 43220 (fixed in `repeal-43220/`): the revision of 2016-05-12 is
    /// effective on 2016-12-30, the repeal of 2016-09-30 is effective on 2016-09-30.
    fn real_43220() -> Vec<SourceRevision> {
        let tag = |xml: &str, name: &str| -> String {
            let open = format!("<{name}>");
            let from = xml.find(&open).unwrap() + open.len();
            let to = from + xml[from..].find(&format!("</{name}>")).unwrap();
            xml[from..to].to_owned()
        };
        ["2000000026259", "2100000047050", "2200000056252"]
            .into_iter()
            .map(|serial| {
                let bytes = std::fs::read(fixture(&format!("repeal-43220/{serial}.xml"))).unwrap();
                let xml = String::from_utf8(bytes.clone()).unwrap();
                SourceRevision {
                    identity: tag(&xml, "행정규칙ID"),
                    revision: tag(&xml, "행정규칙일련번호"),
                    name: tag(&xml, "행정규칙명")
                        .replace("<![CDATA[", "")
                        .replace("]]>", ""),
                    category: tag(&xml, "행정규칙종류"),
                    canonical_path: "국무총리/식품의약품안전처/고시/규정.md".to_owned(),
                    source_path: format!("{serial}.xml"),
                    xml: bytes,
                    promulgation: tag(&xml, "발령일자"),
                    enforcement: tag(&xml, "시행일자"),
                    amendment: tag(&xml, "제개정구분명"),
                    repeal: tag(&xml, "제개정구분코드") == "200410",
                    markdown: format!("---\nname: {serial}\n---\nbody\n").into_bytes(),
                    secondary_name: String::new(),
                    secondary_number: 0,
                    serial: serial.parse().unwrap(),
                    copy_priority: 0,
                    copy_conflict: None,
                }
            })
            .collect()
    }

    #[test]
    fn real_43220_fixture_files_are_the_verified_sources() {
        for (serial, digest) in [
            (
                "2000000026259",
                "6297fa8719567cf1ede25f7dd1505d41c9584aebc7c8ed51292896e27980f134",
            ),
            (
                "2100000047050",
                "38bbdde6cff26b5c17e4a9bd99abd7e7f12db7ead021d0014bfd46241a40879a",
            ),
            (
                "2200000056252",
                "bdd411b2eb7df4f54a57fcbee850c6b53c2fe3e64ffd5398568e6a5484ff53f2",
            ),
        ] {
            let bytes = std::fs::read(fixture(&format!("repeal-43220/{serial}.xml"))).unwrap();
            assert_eq!(sha256(&bytes), digest, "{serial}");
        }
    }

    #[test]
    fn real_43220_enforcement_leaves_the_revival_unresolved_and_out_of_head() {
        let sources = real_43220();
        let default = plan_of("admrules", &sources, false);
        assert_eq!(
            state(&default, "2100000047050.xml"),
            ("confirmed".to_owned(), Some("2016-12-30".to_owned()))
        );
        assert_eq!(
            state(&default, "2200000056252.xml"),
            ("confirmed".to_owned(), Some("2016-09-30".to_owned()))
        );
        let mut plan = plan_of("admrules", &sources, true);
        assert_target(
            &plan,
            "2100000047050.xml",
            "2200000056252",
            "2016-09-30",
            "2016-12-30",
        );
        let evidence = &found(&plan, "2100000047050.xml").date_evidence;
        // The earlier evidence stays.
        assert_eq!(evidence["raw_promulgation_date"], "20160512");
        assert_eq!(evidence["raw_enforcement_date"], "20161230");
        assert_eq!(evidence["source_path"], "2100000047050.xml");
        assert_eq!(
            found(&plan, "2100000047050.xml").exposure_date.as_deref(),
            Some("2016-05-12")
        );
        // The other two revisions are as before.
        for path in ["2000000026259.xml", "2200000056252.xml"] {
            assert_eq!(state(&plan, path), state(&default, path), "{path}");
            untouched(found(&plan, path));
        }
        // The replay: no revision of the identity stands in HEAD after the repeal.
        let temp = tempfile::tempdir().unwrap();
        let (head, staged) = plan
            .replay(&temp.path().join("out.git"), b"readme")
            .unwrap();
        let listed = |staged: &StagedRepository| {
            String::from_utf8(
                git(
                    &staged.owned,
                    &["ls-tree", "-r", "-z", "--name-only", "HEAD"],
                )
                .unwrap(),
            )
            .unwrap()
        };
        let tree = listed(&staged);
        assert!(
            !tree.contains("국무총리/식품의약품안전처/고시/규정.md"),
            "{tree}"
        );
        assert!(tree.contains(".legalize/candidates/"), "{tree}");
        let (bytes, _) = plan
            .prepare_publication(Some(&staged.owned), &head, json!({}))
            .unwrap();
        let manifest: Value = serde_json::from_slice(&bytes).unwrap();
        let revision = manifest["date_index"]["revisions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|revision| revision["revision"] == "2100000047050")
            .unwrap();
        assert_eq!(revision["status"], "unresolved");
        assert_eq!(
            revision["occurrences"][0]["date_evidence"]["date_withheld"],
            "repeal-before-effective"
        );
        // The control: without the repeal the revision stays confirmed and the canonical path
        // is in HEAD. The earlier revision has the same path, so this checks the path only.
        let mut without = plan_of("admrules", &sources[..2], true);
        assert_eq!(state(&without, "2100000047050.xml").0, "confirmed");
        let (_, staged) = without
            .replay(&temp.path().join("control.git"), b"readme")
            .unwrap();
        assert!(listed(&staged).contains("국무총리/식품의약품안전처/고시/규정.md"));
    }

    #[test]
    fn real_43220_default_basis_keeps_the_revision_as_it_was() {
        let sources = real_43220();
        let plan = plan_of("admrules", &sources, false);
        let (bytes, _) = plan.prepare_publication(None, "head", json!({})).unwrap();
        let manifest: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(manifest["date_index"]["basis"], "promulgation");
        let index = manifest["date_index"]["revisions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|revision| revision["revision"] == "2100000047050")
            .unwrap();
        let occurrence = &index["occurrences"][0];
        assert_eq!(occurrence["status"], "confirmed");
        assert_eq!(occurrence["enforcement_date"], "2016-12-30");
        assert!(
            occurrence["date_evidence"]
                .get("repeal_before_effective")
                .is_none()
        );
        assert!(occurrence["date_evidence"].get("date_withheld").is_none());
        // The occurrence equals the output of the resolver, which has no rule of the plan.
        let resolved = Prepared::resolve("admrules", &sources[1])
            .occurrence
            .unwrap();
        assert_eq!(occurrence["date_evidence"], resolved.date_evidence);
        assert_eq!(occurrence["reason"], json!(resolved.reason));
        assert_eq!(index["status"], "confirmed");
    }
}

/// The LLM ledger as the second approval source (compiler#2, design-v3-integration.md).
#[cfg(test)]
mod llm_tests {
    use super::*;
    use crate::llm_ledger::LlmLedger;

    /// An ordinance source of identity 1 with its addendum text.
    fn source(
        revision: &str,
        promulgation: &str,
        enforcement: &str,
        amendment: &str,
        addendum: &str,
    ) -> SourceRevision {
        let xml = format!(
            "<LawService><공포번호>{revision}</공포번호><조문><조><조내용>제1조(목적) 이 조례는 운영을 정한다.</조내용></조></조문><부칙><부칙공포일자>{promulgation}</부칙공포일자><부칙내용><![CDATA[{addendum}]]></부칙내용></부칙></LawService>"
        );
        SourceRevision {
            identity: "1".to_owned(),
            revision: revision.to_owned(),
            name: "test".to_owned(),
            category: "조례".to_owned(),
            canonical_path: "p.md".to_owned(),
            source_path: format!("{revision}.xml"),
            xml: xml.into_bytes(),
            promulgation: promulgation.to_owned(),
            enforcement: enforcement.to_owned(),
            amendment: amendment.to_owned(),
            repeal: amendment.contains("폐지"),
            markdown: format!("---\nname: {revision}\n---\nbody\n").into_bytes(),
            secondary_name: String::new(),
            secondary_number: 0,
            serial: revision.parse().unwrap(),
            copy_priority: 0,
            copy_conflict: None,
        }
    }

    /// One judgment: (outcome, date, quote, proviso, proviso quote, next promulgation).
    struct Judged<'a> {
        outcome: &'a str,
        date: &'a str,
        quote: &'a str,
        proviso: bool,
        proviso_quote: &'a str,
        next: Option<&'a str>,
    }

    fn judged<'a>(outcome: &'a str, date: &'a str, quote: &'a str) -> Judged<'a> {
        Judged {
            outcome,
            date,
            quote,
            proviso: false,
            proviso_quote: "",
            next: None,
        }
    }

    /// A ledger whose entries record, as the parser result at judgment time, what the rule chain
    /// gives now (so B3 holds) unless `rule` overrides the recorded rule.
    fn ledger(items: &[(&SourceRevision, Judged, Option<&str>)]) -> LlmLedger {
        let mut entries: Vec<Value> = items
            .iter()
            .map(|(source, judgment, rule)| {
                let chain = rule_chain("ordinances", source).unwrap();
                json!({"key": {"dataset": "ordinances", "identity": source.identity, "revision": source.revision,
                        "source_sha256": sha256(&source.xml)},
                    "kind": "judged", "adoptable": true, "date": judgment.date,
                    "model": "m", "effort": "default", "prompt_version": "p", "runner_version": "r2", "judged_at": "t",
                    "inputs": {"next_promulgation": judgment.next},
                    "parser_at_judgment": {"category": "judge", "status": chain.status, "date": chain.enforcement_date,
                        "rule": rule.map_or(chain.reason.clone(), str::to_owned)},
                    "judgment": {"outcome": judgment.outcome, "basis_field": "부칙", "basis_quote": judgment.quote,
                        "section_label": "", "proviso": judgment.proviso, "proviso_quote": judgment.proviso_quote}})
            })
            .collect();
        entries.sort_by_key(|entry| {
            ["dataset", "identity", "revision", "source_sha256"]
                .map(|field| entry["key"][field].as_str().unwrap().to_owned())
        });
        let doc = json!({"schema_version": 1, "dataset": "ordinances",
            "policy": {"model": "m", "effort": "default", "prompt_version": "p", "runner_version": "r2"},
            "protected_list_sha256": null, "entries": entries});
        LlmLedger::parse(&serde_json::to_vec(&doc).unwrap(), "ordinances").unwrap()
    }

    fn plan(sources: Vec<SourceRevision>, ledger: &LlmLedger) -> DatePlan {
        DatePlan::new_with("ordinances", true, sources, Some(ledger)).unwrap()
    }

    fn blocking(found: &Occurrence) -> Vec<String> {
        found.date_evidence["llm_judgment"]["blocking"]
            .as_array()
            .map(|list| {
                list.iter()
                    .map(|item| item.as_str().unwrap().to_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn a_prefixed_quote_of_an_unreadable_form_is_its_own_clause() {
        // 681330: the quote starts at "①", the clause at "이 규칙은"; the parser reads the value as
        // Other. The quote itself stays out of the same-date comparison (v3.3).
        let one = source(
            "681330",
            "19970801",
            "99991231",
            "일부개정",
            "부칙\n①(시행일) 이 규칙은 1997. 9. 1일부터 시행한다.",
        );
        let quote = "①(시행일) 이 규칙은 1997. 9. 1일부터 시행한다.";
        let built = plan(
            vec![one.clone()],
            &ledger(&[(&one, judged("explicit_date", "1997-09-01", quote), None)]),
        );
        let found = built.occurrence(0);
        assert!(blocking(found).is_empty(), "{:?}", blocking(found));
        assert_eq!(
            (found.status.as_str(), found.enforcement_date.as_deref()),
            ("llm", Some("1997-09-01"))
        );
        assert_eq!(found.gate.path, Some(LLM_PATH));
        let body = String::from_utf8(built.replay_body(0).unwrap()).unwrap();
        assert!(body.contains("날짜판단상태: \"llm\""), "{body}");
        assert!(
            body.contains(&format!(
                "시행일근거문장: {}",
                serde_json::to_string(quote).unwrap()
            )),
            "{body}"
        );
        // The old definition (a clause that starts where the quote starts) would compare it.
        let text = "부칙\n①(시행일) 이 규칙은 1997. 9. 1일부터 시행한다.";
        let at = text.find('①').unwrap();
        let own: Vec<usize> = read_clauses_at(text, false)
            .iter()
            .map(|(start, _)| *start)
            .collect();
        assert!(!own.contains(&at), "the clause does not start at the quote");
    }

    #[test]
    fn a_proviso_inside_the_quote_holds_the_date() {
        // 808395: the parser reads "단" with a space as a proviso of the quoted clause
        // (user-c2-quote-proviso-hold-20261117).
        let one = source(
            "808395",
            "20110415",
            "99991231",
            "일부개정",
            "부칙 (2011.4.15.) 이 조례는 공포한 날부터 시행한다. 단 제3조제5항은 2011년 7월 1일부터 시행한다.",
        );
        let quote =
            "이 조례는 공포한 날부터 시행한다. 단 제3조제5항은 2011년 7월 1일부터 시행한다.";
        let built = plan(
            vec![one.clone()],
            &ledger(&[(&one, judged("promulgation_day", "2011-04-15", quote), None)]),
        );
        let found = built.occurrence(0);
        assert_eq!(found.status, "unresolved");
        assert!(
            blocking(found).contains(&"quote-proviso".to_owned()),
            "{:?}",
            blocking(found)
        );
        assert_eq!(found.date_evidence["date_withheld"], "llm-sentence-blocked");
    }

    #[test]
    fn a_proviso_the_parser_does_not_read_applies() {
        // 862624: the parser reads no clause in "시행하되, 단 ..."; the proviso of the judgment does
        // not start with 다만 or 단, (user-c2-proviso-357-20261117), so the date applies.
        let one = source(
            "862624",
            "20110310",
            "99991231",
            "일부개정",
            "부칙\n이 조례는 공포한 날부터 시행하되, 단 제7조제2항은 2011년 1월 1일부터 적용한다.",
        );
        let quote =
            "이 조례는 공포한 날부터 시행하되, 단 제7조제2항은 2011년 1월 1일부터 적용한다.";
        let mut judgment = judged("promulgation_day", "2011-03-10", quote);
        judgment.proviso = true;
        judgment.proviso_quote = "단 제7조제2항은 2011년 1월 1일부터 적용한다.";
        let built = plan(vec![one.clone()], &ledger(&[(&one, judgment, None)]));
        let found = built.occurrence(0);
        assert!(blocking(found).is_empty(), "{:?}", blocking(found));
        assert_eq!(
            (found.status.as_str(), found.enforcement_date.as_deref()),
            ("llm", Some("2011-03-10"))
        );
    }

    #[test]
    fn a_proviso_that_starts_with_daman_holds_the_date() {
        let one = source(
            "700001",
            "20110310",
            "99991231",
            "일부개정",
            "부칙\n이 조례는 공포한 날부터 시행한다.\n",
        );
        let mut judgment = judged(
            "promulgation_day",
            "2011-03-10",
            "이 조례는 공포한 날부터 시행한다.",
        );
        judgment.proviso = true;
        judgment.proviso_quote = "다만, 제2조는 2012년 1월 1일부터 시행한다.";
        let found = plan(vec![one.clone()], &ledger(&[(&one, judgment, None)]))
            .occurrence(0)
            .clone();
        assert_eq!(found.status, "unresolved");
        assert_eq!(blocking(&found), vec!["llm-proviso".to_owned()]);
    }

    #[test]
    fn another_date_or_an_unreadable_sentence_in_the_section_holds_the_date() {
        for (other, reason) in [
            (
                "이 조례는 2012년 1월 1일부터 시행한다.",
                "section-other-date",
            ),
            (
                "이 조례는 의회의 승인을 받은 날부터 시행한다.",
                "section-other-unreadable",
            ),
        ] {
            let one = source(
                "700002",
                "20110310",
                "99991231",
                "일부개정",
                &format!("부칙\n이 조례는 공포일부터 시행한다.\n{other}"),
            );
            let found = plan(
                vec![one.clone()],
                &ledger(&[(
                    &one,
                    judged(
                        "promulgation_day",
                        "2011-03-10",
                        "이 조례는 공포일부터 시행한다.",
                    ),
                    None,
                )]),
            )
            .occurrence(0)
            .clone();
            assert_eq!(found.status, "unresolved", "{other}");
            assert!(
                blocking(&found).contains(&reason.to_owned()),
                "{other}: {:?}",
                blocking(&found)
            );
        }
    }

    #[test]
    fn a_changed_chain_or_a_valid_source_date_selects_nothing() {
        let one = source(
            "700003",
            "20110310",
            "99991231",
            "일부개정",
            "부칙\n이 조례는 공포일부터 시행한다.",
        );
        let quote = "이 조례는 공포일부터 시행한다.";
        let changed = plan(
            vec![one.clone()],
            &ledger(&[(
                &one,
                judged("promulgation_day", "2011-03-10", quote),
                Some("another rule"),
            )]),
        );
        let found = changed.occurrence(0);
        assert_eq!(found.status, "unresolved");
        assert_eq!(
            found.date_evidence["llm_not_applied"]["reason"],
            "chain-changed"
        );
        let gated = plan(
            vec![one.clone()],
            &ledger(&[(
                &one,
                judged("promulgation_day", "2011-03-10", quote),
                Some(GATE_REASON),
            )]),
        );
        assert_eq!(
            gated.occurrence(0).date_evidence["llm_not_applied"]["reason"],
            "recorded-after-gate"
        );
        let valid = source(
            "700004",
            "20110310",
            "20110401",
            "일부개정",
            "부칙\n이 조례는 공포일부터 시행한다.",
        );
        let found = plan(
            vec![valid.clone()],
            &ledger(&[(
                &valid,
                judged("promulgation_day", "2011-03-10", quote),
                None,
            )]),
        )
        .occurrence(0)
        .clone();
        assert_eq!(
            (found.status.as_str(), found.enforcement_date.as_deref()),
            ("confirmed", Some("2011-04-01"))
        );
        assert_eq!(
            found.date_evidence["llm_not_applied"]["reason"],
            "valid-source-date"
        );
        let missing = plan(
            vec![one.clone()],
            &ledger(&[(
                &one,
                judged("promulgation_day", "2011-03-10", "이 문장은 원문에 없다."),
                None,
            )]),
        );
        assert_eq!(
            missing.occurrence(0).date_evidence["llm_not_applied"]["reason"],
            "quote-not-found"
        );
    }

    #[test]
    fn the_registry_decides_first_and_a_conflict_stops() {
        // 1000238 is an exempt registry entry; a usable judgment of it stops the build.
        let named = source(
            "1000238",
            "20071031",
            "99991231",
            "일부개정",
            "부칙\n이 조례는 공포 후 1개월이 경과한 날부터 시행한다.",
        );
        let ledger = ledger(&[(
            &named,
            judged(
                "promulgation_day",
                "2007-10-31",
                "이 조례는 공포 후 1개월이 경과한 날부터 시행한다.",
            ),
            None,
        )]);
        assert!(DatePlan::new_with("ordinances", true, vec![named], Some(&ledger)).is_err());
    }

    #[test]
    fn a_later_next_revision_takes_the_date_away() {
        let one = source(
            "700005",
            "20110310",
            "99991231",
            "일부개정",
            "부칙\n이 조례는 공포일부터 시행한다.",
        );
        let next = source(
            "700006",
            "20110601",
            "20110601",
            "일부개정",
            "부칙\n이 조례는 공포한 날부터 시행한다.",
        );
        let ledger = ledger(&[(
            &one,
            judged(
                "promulgation_day",
                "2011-03-10",
                "이 조례는 공포일부터 시행한다.",
            ),
            None,
        )]);
        let built = plan(vec![one, next], &ledger);
        let found = built.occurrence(0);
        assert_eq!(found.status, "unresolved");
        assert_eq!(found.date_evidence["date_withheld"], "llm-next-revision");
        assert_eq!(
            found.date_evidence["llm_not_applied"]["reason"],
            "next-revision-changed"
        );
        assert_eq!(
            found.date_evidence["llm_not_applied"]["actual_next_promulgation"],
            "2011-06-01"
        );
    }

    #[test]
    fn an_llm_dated_repeal_changes_no_other_revision() {
        // user-c2-llm-repeal-no-20261117: the repeal dated by the LLM is no confirmed repeal of the
        // repeal-before-effective rule, so the earlier revision keeps its source date.
        let earlier = source(
            "700007",
            "20240101",
            "20241231",
            "일부개정",
            "부칙\n이 조례는 2024년 12월 31일부터 시행한다.",
        );
        let repeal = source(
            "700008",
            "20240601",
            "99991231",
            "폐지",
            "부칙\n이 조례는 공포일부터 시행한다.",
        );
        let mut judgment = judged(
            "promulgation_day",
            "2024-06-01",
            "이 조례는 공포일부터 시행한다.",
        );
        judgment.next = None;
        let ledger = ledger(&[(&repeal, judgment, None)]);
        let built = plan(vec![earlier, repeal], &ledger);
        assert_eq!(built.occurrence(1).status, "llm");
        let kept = built.occurrence(0);
        assert_eq!(
            (kept.status.as_str(), kept.enforcement_date.as_deref()),
            ("confirmed", Some("2024-12-31"))
        );
    }

    #[test]
    fn copies_with_and_without_an_llm_date_are_held() {
        // Two copies of one revision: only one has a usable judgment. The copy with the LLM date
        // is held with the LLM mark; the other copy stays as the parser left it (unresolved).
        let one = source(
            "700009",
            "20110310",
            "99991231",
            "일부개정",
            "부칙\n이 조례는 공포일부터 시행한다.",
        );
        let mut other = one.clone();
        other.xml.extend_from_slice(b"\n");
        let ledger = ledger(&[(
            &one,
            judged(
                "promulgation_day",
                "2011-03-10",
                "이 조례는 공포일부터 시행한다.",
            ),
            None,
        )]);
        let built = plan(vec![one, other], &ledger);
        let judged_copy = built.occurrence(0);
        assert_eq!(judged_copy.status, "unresolved");
        assert_eq!(
            judged_copy.date_evidence["date_withheld"],
            "llm-copy-mismatch"
        );
        assert!(!judged_copy.date_evidence["llm_judgment"].is_null());
        let plain = built.occurrence(1);
        assert_eq!(
            (plain.status.as_str(), plain.enforcement_date.as_deref()),
            ("unresolved", None)
        );
        assert!(plain.date_evidence["llm_judgment"].is_null());
    }

    #[test]
    fn without_a_ledger_nothing_changes() {
        let one = source(
            "700010",
            "20110310",
            "99991231",
            "일부개정",
            "부칙\n이 조례는 공포일부터 시행한다.",
        );
        let plain = DatePlan::new("ordinances", true, vec![one.clone()]).unwrap();
        let with = DatePlan::new_with("ordinances", true, vec![one], None).unwrap();
        assert_eq!(
            serde_json::to_value(&plain.revisions).unwrap(),
            serde_json::to_value(&with.revisions).unwrap()
        );
        assert!(plain.llm.is_none() && with.llm.is_none());
    }
}
