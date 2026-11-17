//! Builds a fresh bare Git repository from cached law.go.kr XML and JSON files.
//!
//! The compiler reads an existing `.cache` tree in two passes:
//! metadata is collected and stably sorted first, then each XML document is
//! fully parsed, rendered to Markdown, and committed into a new bare repo.
#![deny(missing_docs)]
#![deny(clippy::missing_docs_in_private_items)]

/// Structured diagnostics collected during planning and exposed via report/manifest output.
mod diagnostics;
/// Writes the output bare repository and handcrafted packfile stream.
mod git_repo;
/// Renders parsed law data into Markdown and commit messages.
mod render;
/// Parses cached XML documents into metadata and article structures.
mod xml_parser;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use git_writer::date_index::{DatePlan, Prepared, SourceRevision};
use rayon::prelude::*;
use rustc_hash::FxHashMap as HashMap;
use serde::Deserialize;

use crate::diagnostics::{
    BuildManifest, Diagnostics, OrphanRecord, UnparsableRecord, ValidationReport, write_manifest,
};
use crate::git_repo::{BareRepoWriter, GitTimestampKst, RepoPathBuf, hex, precompute_blob};
use crate::render::{
    EntryKind, PathRegistry, build_commit_message, law_to_date_markdown, law_to_markdown,
};
use crate::xml_parser::{LawDetail, LawMetadata, parse_law_body, parse_metadata_only};

/// Bundled README payload for the synthetic initial commit.
const REPOSITORY_README: &[u8] = include_bytes!("../assets/README.md");

/// Global allocator tuned for high-throughput allocation-heavy pack generation.
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Exit policy when anomalies (unparsable, empty metadata, orphan children) surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum OnAnomaly {
    /// Emit warnings but continue building the output repository.
    Warn,
    /// Exit with a non-zero status code when any anomaly is present.
    Fail,
}

/// Selects the date used for repository history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum CommitDateBasis {
    /// Preserve the existing publication-date history.
    Promulgation,
    /// Replay revisions by their approved effective dates.
    Enforcement,
}

/// Command-line interface for one-shot cache compilation.
#[derive(Debug, Parser)]
#[command(name = "legalize-kr-compiler")]
#[command(about = "Compile cached law.go.kr XML/JSON into a fresh bare Git repository")]
struct Cli {
    /// Path to the existing .cache directory
    cache_dir: PathBuf,

    /// Output bare repository path
    #[arg(short = 'o', long = "output", default_value = "output.git")]
    output: PathBuf,

    /// Pre-flight validation only: scan cache, emit JSON report, skip repo writes.
    #[arg(long)]
    validate: bool,

    /// Exit policy when orphans/unparsable/empty-meta surface.
    #[arg(long, value_enum, default_value_t = OnAnomaly::Warn)]
    on_anomaly: OnAnomaly,

    /// Alias for --on-anomaly=fail. Conflicts with --on-anomaly.
    #[arg(long, conflicts_with = "on_anomaly")]
    strict: bool,

    /// Bail if final entry count != N (N is a hard expected total).
    #[arg(long)]
    expect_laws: Option<usize>,

    /// Emit build manifest JSON to this path. Default: no manifest.
    #[arg(long)]
    manifest: Option<PathBuf>,

    /// Select the date used for repository history.
    #[arg(long, value_enum, default_value_t = CommitDateBasis::Promulgation)]
    commit_date_basis: CommitDateBasis,
}

/// Amendment metadata loaded from cached history JSON.
#[derive(Debug, Deserialize)]
struct HistoryEntry {
    /// Law MST key that matches the detail XML filename.
    #[serde(rename = "법령일련번호")]
    mst: String,
    /// Human-readable amendment type applied to that revision.
    #[serde(rename = "제개정구분명", default)]
    amendment: String,
}

/// Pass-1 planning record for one XML document.
#[derive(Debug, Clone)]
struct PlannedEntry {
    /// Law MST used for file lookup and stable ordering.
    mst: String,
    /// Original detail XML path used for targeted warning/error messages.
    source_path: PathBuf,
    /// Final repository path assigned after collision handling.
    path: RepoPathBuf,
    /// Root/Child classification assigned alongside the repository path.
    kind: EntryKind,
    /// Metadata collected during the cheap planning pass.
    metadata: LawMetadata,
}

/// Fully rendered pass-2 output that is ready to commit.
struct Rendered {
    /// Destination repository path for the Markdown file.
    path: RepoPathBuf,
    /// Final Markdown bytes stored in Git.
    markdown: Vec<u8>,
    /// Canonical Git blob id for the rendered Markdown.
    blob_sha: [u8; 20],
    /// Precompressed PACK payload for the rendered Markdown blob.
    compressed_blob: Vec<u8>,
    /// SHA-256 of the rendered Markdown, computed by the render worker for the date index.
    sha256: String,
    /// Commit message for this revision.
    message: String,
    /// Deterministic KST commit timestamp derived during pass 2.
    time: GitTimestampKst,
}

/*
 * Parsed/rendered chunks stay around 1.4 GiB here, while larger chunks grow memory
 * without materially improving throughput on the real cache workload.
 */
/// Number of entries rendered per worker batch before the writer catches up.
const CHUNK_SIZE: usize = 500;

/// Entries read and rendered together for the date index; small so memory stays bounded.
const DATE_CHUNK_SIZE: usize = 128;

/// Parses CLI flags and runs the compiler.
fn main() -> Result<()> {
    let cli = Cli::parse();
    run(cli)
}

/// Possible outcomes from the pass-1 rayon planning closure.
//
// The happy-path `Planned` variant dominates; skipped inputs are rare and storing them in the
// same vector keeps the planning loop lock-free, so the size-disparity lint is acceptable here.
#[allow(clippy::large_enum_variant)]
enum PlanOutcome {
    /// A successfully planned entry ready for path assignment and rendering.
    Planned(PlannedEntry),
    /// An XML file whose content could not be parsed.
    Unparsable {
        /// Source file basename (for diagnostics output).
        file: String,
        /// Human-readable parser error.
        error: String,
    },
    /// A cache file whose 기본정보 block was empty (typically an HTML error page).
    EmptyMetadata {
        /// Source file basename (for diagnostics output).
        file: String,
    },
}

/// Returns the stable key used to group revisions of the same law.
fn lineage_key(entry: &PlannedEntry) -> String {
    if entry.metadata.law_id.is_empty() {
        format!("mst:{}", entry.mst)
    } else {
        format!("id:{}", entry.metadata.law_id)
    }
}

/// Compares planned entries in canonical commit order.
fn planned_entry_cmp(left: &PlannedEntry, right: &PlannedEntry) -> std::cmp::Ordering {
    left.metadata
        .promulgation_date
        .cmp(&right.metadata.promulgation_date)
        .then_with(|| left.metadata.law_name.cmp(&right.metadata.law_name))
        .then_with(|| {
            left.metadata
                .promulgation_number
                .parse::<u64>()
                .unwrap_or_else(|error| {
                    panic!(
                        "cache 공포번호 must be numeric: {}: {error:?}",
                        left.source_path.display()
                    )
                })
                .cmp(
                    &right
                        .metadata
                        .promulgation_number
                        .parse::<u64>()
                        .unwrap_or_else(|error| {
                            panic!(
                                "cache 공포번호 must be numeric: {}: {error:?}",
                                right.source_path.display()
                            )
                        }),
                )
        })
        .then_with(|| {
            left.mst
                .parse::<u64>()
                .expect("detail xml filenames must be numeric MSTs")
                .cmp(
                    &right
                        .mst
                        .parse::<u64>()
                        .expect("detail xml filenames must be numeric MSTs"),
                )
        })
}

/// Assigns output paths so each 법령ID lineage uses its latest law title.
fn assign_current_paths(entries: &mut [PlannedEntry]) {
    let mut latest_by_lineage: HashMap<String, usize> = HashMap::default();
    let mut lineage_order = Vec::new();

    for (index, entry) in entries.iter().enumerate() {
        let lineage = lineage_key(entry);
        match latest_by_lineage.entry(lineage.clone()) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                lineage_order.push(lineage);
                slot.insert(index);
            }
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                let latest_index = *slot.get();
                if planned_entry_cmp(&entries[latest_index], entry).is_lt() {
                    slot.insert(index);
                }
            }
        }
    }

    let mut registry = PathRegistry::default();
    let mut path_by_lineage = HashMap::default();
    for lineage in lineage_order {
        let latest_index = latest_by_lineage[&lineage];
        let entry = &entries[latest_index];
        let (path, kind) = registry.get_law_path(
            &entry.metadata.law_name,
            &entry.metadata.law_type,
            &entry.metadata.law_id,
        );
        path_by_lineage.insert(lineage, (path, kind));
    }

    for entry in entries {
        let lineage = lineage_key(entry);
        let (path, kind) = path_by_lineage
            .get(&lineage)
            .expect("every planned entry must have a lineage path");
        entry.path = path.clone();
        entry.kind = kind.clone();
    }
}

/// Plans the ordered entry list and collects planning-time diagnostics.
///
/// Extracted from [`run`] so tests can drive pass-1 output without running the full build.
fn plan_and_diagnose(cache_dir: &Path) -> Result<(Vec<PlannedEntry>, Diagnostics)> {
    let detail_dir = cache_dir.join("detail");
    if !detail_dir.is_dir() {
        anyhow::bail!("detail cache not found: {}", detail_dir.display());
    }

    //
    // Load history-side amendment labels before planning commit order.
    //
    // History JSON overrides the amendment labels embedded in detail XML.
    let history = {
        let history_dir = cache_dir.join("history");
        if !history_dir.is_dir() {
            HashMap::default()
        } else {
            let mut files = read_sorted_files(&history_dir, "json")?;
            let mut amendments = HashMap::default();
            for path in files.drain(..) {
                let bytes = fs::read(&path)
                    .with_context(|| format!("failed to read {}", path.display()))?;
                let entries: Vec<HistoryEntry> = serde_json::from_slice(&bytes)
                    .with_context(|| format!("failed to parse {}", path.display()))?;
                for entry in entries {
                    amendments.insert(entry.mst, entry.amendment);
                }
            }
            amendments
        }
    };

    //
    // Pass 1 only touches metadata so every later full parse follows one stable order.
    //
    let mut diagnostics = Diagnostics::default();
    let files = read_sorted_files(&detail_dir, "xml")?;
    diagnostics.total_xml = files.len();
    let parsed = files
        .par_iter()
        .map(|path| -> Result<PlanOutcome> {
            let mst = path
                .file_stem()
                .and_then(|name| name.to_str())
                .map(ToOwned::to_owned)
                .with_context(|| format!("invalid file name: {}", path.display()))?;
            let file_name = path
                .file_name()
                .and_then(|name| name.to_str())
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| path.display().to_string());
            let xml =
                fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
            let mut metadata = match parse_metadata_only(&xml, &mst) {
                Ok(metadata) => metadata,
                Err(error) => {
                    return Ok(PlanOutcome::Unparsable {
                        file: file_name,
                        error: format!("{error:#}"),
                    });
                }
            };

            //
            // Some cached detail files are HTML error pages instead of law XML. Those never
            // populate the basic metadata block, so exclude them while keeping every real XML
            // field assumption strict.
            //
            if metadata.law_name.is_empty()
                && metadata.law_id.is_empty()
                && metadata.law_type.is_empty()
                && metadata.promulgation_date.is_empty()
            {
                return Ok(PlanOutcome::EmptyMetadata { file: file_name });
            }

            if let Some(amendment) = history.get(&mst) {
                metadata.amendment = amendment.clone();
            }

            Ok(PlanOutcome::Planned(PlannedEntry {
                mst,
                source_path: path.clone(),
                path: RepoPathBuf::root_file(String::new()),
                kind: EntryKind::Root,
                metadata,
            }))
        })
        .collect::<Vec<_>>();

    let mut entries = Vec::with_capacity(files.len());
    for outcome in parsed {
        match outcome? {
            PlanOutcome::Planned(entry) => entries.push(entry),
            PlanOutcome::Unparsable { file, error } => {
                eprintln!("warning: skipping unparsable cache file {file}: {error}");
                diagnostics
                    .unparsable
                    .push(UnparsableRecord { file, error });
            }
            PlanOutcome::EmptyMetadata { file } => {
                eprintln!(
                    "warning: skipping unparsable cache file {file}: missing basic law metadata",
                );
                diagnostics.empty_metadata.push(file);
            }
        }
    }

    entries.sort_by(planned_entry_cmp);
    assign_current_paths(&mut entries);

    //
    // Cross-reference root laws to catch 시행령/시행규칙 children whose parent 법률 is absent.
    //
    let parent_groups: std::collections::HashSet<String> = entries
        .iter()
        .filter(|entry| matches!(entry.kind, EntryKind::Root))
        .filter_map(|entry| match &entry.path {
            RepoPathBuf::KrFile { group, .. } => Some(group.clone()),
            RepoPathBuf::RootFile(_) => None,
        })
        .collect();
    for entry in &entries {
        if let EntryKind::Child { parent_group } = &entry.kind
            && !parent_groups.contains(parent_group)
        {
            diagnostics.orphan_children.push(OrphanRecord {
                law_name: entry.metadata.law_name.clone(),
                law_type: entry.metadata.law_type.clone(),
                parent_group: parent_group.clone(),
            });
        }
    }

    //
    // Roll up planned entry counts by law_type for both validation reports and manifests.
    //
    for entry in &entries {
        *diagnostics
            .by_type
            .entry(entry.metadata.law_type.clone())
            .or_insert(0) += 1;
    }

    Ok((entries, diagnostics))
}

/// Executes the full two-pass cache-to-Git compilation pipeline.
fn run(cli: Cli) -> Result<()> {
    let enforcement = cli.commit_date_basis == CommitDateBasis::Enforcement;
    if enforcement && cli.manifest.is_none() {
        anyhow::bail!("--commit-date-basis enforcement requires --manifest <path>");
    }
    //
    // Resolve `--strict` into the normalized anomaly policy so downstream checks only consult one.
    //
    let on_anomaly = if cli.strict {
        OnAnomaly::Fail
    } else {
        cli.on_anomaly
    };

    eprintln!("1. Loading amendment history");
    eprintln!("2. Scanning cache metadata");
    let (entries, diagnostics) = plan_and_diagnose(&cli.cache_dir)?;
    let detail_dir = cli.cache_dir.join("detail");

    let destination = if cli.validate {
        cli.manifest.clone()
    } else {
        Some(DatePlan::check_destination(
            &cli.output,
            cli.manifest.as_deref(),
            enforcement,
        )?)
    };
    // The date plan reads every body. It runs after the original checks of a build (the empty
    // input and `--expect-laws` guards), so those checks keep their order and exit codes. A
    // plain `--validate` reports from the metadata plan alone, as before. An enforcement
    // validation checks the dates before it reports, and writes no index.
    let date_plan = || -> Result<DatePlan> {
        // Sources are read, resolved and rendered in chunks, so memory stays bounded by one
        // chunk. A validation index has no repository, so it keeps every body inline.
        let keep_markdown = enforcement;
        let render = !cli.validate;
        let items = entries.chunks(DATE_CHUNK_SIZE).flat_map(|chunk| {
            chunk
                .par_iter()
                .map(|entry| prepare_date_source(&cli.cache_dir, entry, keep_markdown, render))
                .collect::<Vec<_>>()
        });
        DatePlan::from_prepared("laws", enforcement, items)
    };

    //
    // `--validate` short-circuits before the entry-empty guard so operators can see an empty
    // cache as a structured report rather than an opaque error.
    //
    if cli.validate {
        if enforcement {
            date_plan()?;
        }
        let report = ValidationReport {
            total_xml: diagnostics.total_xml,
            unparsable: &diagnostics.unparsable,
            empty_metadata: &diagnostics.empty_metadata,
            orphan_children: &diagnostics.orphan_children,
            by_type: &diagnostics.by_type,
            expected_laws: cli.expect_laws,
            actual_laws: entries.len(),
        };
        let json = serde_json::to_string_pretty(&report)
            .context("failed to serialize validation report as JSON")?;
        println!("{json}");
        if let Some(path) = &cli.manifest {
            //
            // Validate mode still emits a manifest if requested, with an empty HEAD placeholder
            // to signal "no build was performed".
            //
            let manifest = BuildManifest {
                schema_version: 1,
                head_commit_sha: String::new(),
                entries_total: entries.len(),
                unparsable: &diagnostics.unparsable,
                empty_metadata: &diagnostics.empty_metadata,
                orphan_children: &diagnostics.orphan_children,
                by_type: &diagnostics.by_type,
            };
            // A validation run builds no repository, so its manifest has no date index.
            if enforcement {
                anyhow::ensure!(
                    !path.exists(),
                    "date index already exists: {}",
                    path.display()
                );
            }
            write_manifest(path, &manifest)?;
        }
        return Ok(());
    }

    if entries.is_empty() {
        anyhow::bail!("no valid XML entries found under {}", detail_dir.display());
    }

    //
    // Guard the expected entry total before any write side-effect so misconfigured runs fail
    // fast with a dedicated exit code (3) rather than producing a misaligned repository.
    //
    if let Some(expected) = cli.expect_laws
        && entries.len() != expected
    {
        eprintln!(
            "error: expected {expected} entries, got {} (see --expect-laws)",
            entries.len()
        );
        std::process::exit(3);
    }

    let mut dates = date_plan()?;
    if enforcement {
        // Files without valid metadata are not entries. As in the default build, they are
        // reported in the manifest and `--strict` turns them into an error at the end.
        let (head, staged) = dates.replay(&cli.output, REPOSITORY_README)?;
        let manifest = BuildManifest {
            schema_version: 1,
            head_commit_sha: head.clone(),
            entries_total: entries.len(),
            unparsable: &diagnostics.unparsable,
            empty_metadata: &diagnostics.empty_metadata,
            orphan_children: &diagnostics.orphan_children,
            by_type: &diagnostics.by_type,
        };
        dates.publish_enforcement(
            staged,
            &head,
            destination.as_deref().unwrap(),
            serde_json::to_value(&manifest)?,
        )?;
        report_anomalies(on_anomaly, &diagnostics);
        return Ok(());
    }

    //
    // Seed the synthetic history commits that always come before law revisions.
    //
    eprintln!(
        "3. Writing {} commits to {}",
        entries.len(),
        cli.output.display()
    );
    let mut repo = BareRepoWriter::create(&cli.output)?;

    // 2026-03-30 12:00:00 KST (UTC+9) = 2026-03-30 03:00:00 UTC
    const INITIAL_COMMIT_EPOCH: i64 = 1_774_839_600;
    repo.commit_static(
        &RepoPathBuf::root_file("README.md"),
        REPOSITORY_README,
        "initial commit",
        INITIAL_COMMIT_EPOCH,
    )?;
    eprintln!("  committed README.md");
    repo.commit_empty_initial_contributor(
        "Add @simnalamburt as a contributor",
        INITIAL_COMMIT_EPOCH,
    )?;
    eprintln!("  committed contributor marker");

    //
    // Parse/render chunks in parallel while the main thread keeps Git writes ordered.
    //
    let total = entries.len();
    let chunks: Vec<&[PlannedEntry]> = entries.chunks(CHUNK_SIZE).collect();
    let mut pending: Option<Vec<Result<Rendered>>> = None;
    let mut committed = 0usize;

    for (index, chunk) in chunks.iter().enumerate() {
        let detail_dir = detail_dir.to_path_buf();
        let next = if index + 1 < chunks.len() {
            let next_chunk: Vec<PlannedEntry> = chunks[index + 1].to_vec();
            let next_detail_dir = detail_dir.clone();
            Some(std::thread::spawn(move || {
                next_chunk
                    .par_iter()
                    .map(|entry| render_entry(&next_detail_dir, entry))
                    .collect::<Vec<_>>()
            }))
        } else {
            None
        };

        let rendered = if let Some(previous) = pending.take() {
            previous
        } else {
            chunk
                .par_iter()
                .map(|entry| render_entry(&detail_dir, entry))
                .collect::<Vec<_>>()
        };

        for rendered in rendered {
            let rendered = rendered?;
            repo.commit_law(
                &rendered.path,
                &rendered.markdown,
                rendered.blob_sha,
                &rendered.compressed_blob,
                &rendered.message,
                rendered.time,
            )?;
            dates.record_body_hashed(
                &rendered.path.to_string(),
                rendered.sha256.clone(),
                rendered.blob_sha,
                repo.head_commit().unwrap(),
            );
            committed += 1;
            if committed.is_multiple_of(500) || committed == total {
                eprintln!("  committed {committed}/{total}");
            }
        }

        if let Some(handle) = next {
            pending = Some(handle.join().expect("render worker panicked"));
        }
    }

    eprintln!("4. Finalizing pack + index");
    let head_sha = repo.finish()?;
    let head_hex = hex(&head_sha);
    DatePlan::discard_stale_index(destination.as_deref().unwrap())?;
    let manifest = BuildManifest {
        schema_version: 1,
        head_commit_sha: head_hex.clone(),
        entries_total: entries.len(),
        unparsable: &diagnostics.unparsable,
        empty_metadata: &diagnostics.empty_metadata,
        orphan_children: &diagnostics.orphan_children,
        by_type: &diagnostics.by_type,
    };
    dates.publish(
        Some(&cli.output),
        &head_hex,
        destination.as_deref().unwrap(),
        serde_json::to_value(&manifest)?,
    )?;

    report_anomalies(on_anomaly, &diagnostics);
    Ok(())
}

/// Strict mode reports cumulative anomalies at the end so operators see the full picture
/// even though the build itself completed.
fn report_anomalies(on_anomaly: OnAnomaly, diagnostics: &Diagnostics) {
    if on_anomaly == OnAnomaly::Fail && !diagnostics.is_clean() {
        eprintln!(
            "strict: {} unparsable, {} empty-meta, {} orphans",
            diagnostics.unparsable.len(),
            diagnostics.empty_metadata.len(),
            diagnostics.orphan_children.len()
        );
        std::process::exit(2);
    }
}

/// Reads one planned source, resolves its dates and renders the body for the date index.
fn prepare_date_source(
    cache: &Path,
    entry: &PlannedEntry,
    keep_markdown: bool,
    render: bool,
) -> Result<(SourceRevision, Prepared)> {
    let xml = read_law_xml(&entry.source_path)?;
    let body = parse_law_body_at(&xml, &entry.source_path)?;
    let metadata = &entry.metadata;
    let source = SourceRevision {
        identity: if metadata.law_id.is_empty() {
            entry.mst.clone()
        } else {
            metadata.law_id.clone()
        },
        revision: entry.mst.clone(),
        name: metadata.law_name.clone(),
        category: metadata.law_type.clone(),
        canonical_path: entry.path.to_string(),
        source_path: entry
            .source_path
            .strip_prefix(cache)?
            .to_string_lossy()
            .into_owned(),
        promulgation: metadata.promulgation_date.clone(),
        enforcement: metadata.enforcement_date.clone(),
        amendment: metadata.amendment.clone(),
        repeal: matches!(metadata.amendment.as_str(), "폐지" | "타법폐지"),
        markdown: Vec::new(),
        xml,
        secondary_name: metadata.law_name.clone(),
        secondary_number: metadata.promulgation_number.parse().unwrap_or(u64::MAX),
        serial: entry.mst.parse().unwrap_or(u64::MAX),
        copy_priority: 0,
        copy_conflict: None,
    };
    let mut prepared = Prepared::resolve("laws", &source);
    let mut source = source;
    // A validation run only checks the dates, so it renders no body.
    let rendered = match prepared.occurrence().filter(|_| render) {
        Some(occurrence) => {
            let detail = LawDetail {
                metadata: metadata.clone(),
                articles: body.articles,
                addenda: body.addenda,
                attachments: body.attachments,
            };
            Some((
                law_to_date_markdown(&detail, occurrence)?,
                occurrence.has_plain_source_dates(),
            ))
        }
        None => None,
    };
    if let Some((markdown, plain)) = rendered {
        prepared = prepared.with_markdown_hash(&markdown);
        // The default build commits this exact body when the dates are the source's own, so
        // only its hash is kept. Bodies the date renderer changes keep their text.
        source.markdown = if !keep_markdown && plain {
            Vec::new()
        } else {
            markdown
        };
    }
    Ok((source, prepared))
}

/// Reads source bytes with the original path in the error chain.
fn read_law_xml(path: &Path) -> Result<Vec<u8>> {
    fs::read(path).map_err(|source| {
        let message = format!("failed to read {}: {source}", path.display());
        anyhow::Error::new(source).context(message)
    })
}

/// Parses a selected source with the original path in the error chain.
fn parse_law_body_at(xml: &[u8], path: &Path) -> Result<crate::xml_parser::LawBody> {
    parse_law_body(xml).map_err(|source| {
        let message = format!("failed to parse {}: {source}", path.display());
        source.context(message)
    })
}

/// Parses, renders, and packages one planned XML entry for pass 2.
fn render_entry(detail_dir: &Path, entry: &PlannedEntry) -> Result<Rendered> {
    // Pass 2 does the expensive XML parse only after pass 1 has fixed the final order and path.
    let xml_path = detail_dir.join(format!("{}.xml", entry.mst));
    let xml = read_law_xml(&xml_path)?;
    let body = parse_law_body_at(&xml, &xml_path)?;
    let detail = LawDetail {
        metadata: entry.metadata.clone(),
        articles: body.articles,
        addenda: body.addenda,
        attachments: body.attachments,
    };
    let time = GitTimestampKst::from_promulgation_date(&detail.metadata.promulgation_date)?;

    let markdown = law_to_markdown(&detail)?;
    let (blob_sha, compressed_blob) = precompute_blob(&markdown);
    let sha256 = git_writer::date_index::sha256(&markdown);
    let message = build_commit_message(&detail.metadata, &entry.mst)?;
    Ok(Rendered {
        path: entry.path.clone(),
        markdown,
        blob_sha,
        compressed_blob,
        sha256,
        message,
        time,
    })
}

/// Lists files with the requested extension in deterministic path order.
fn read_sorted_files(dir: &Path, extension: &str) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for item in fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))? {
        let path = item?.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some(extension) {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;

    const SAMPLE_XML_1: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<법령>
  <기본정보>
    <법령ID>000001</법령ID>
    <공포일자>20240101</공포일자>
    <공포번호>00001</공포번호>
    <법종구분>법률</법종구분>
    <법령명_한글><![CDATA[테스트법]]></법령명_한글>
    <시행일자>20240101</시행일자>
    <연락부서><부서단위><소관부처명>법무부</소관부처명></부서단위></연락부서>
  </기본정보>
  <조문>
    <조문단위>
      <조문번호>1</조문번호>
      <조문제목><![CDATA[목적]]></조문제목>
      <조문내용><![CDATA[제1조 (목적) 테스트한다.]]></조문내용>
    </조문단위>
  </조문>
</법령>
"#;

    const SAMPLE_INVALID_HTML: &str = r#"<!DOCTYPE html>
<html>
<head><title>Error</title></head>
<body>XML 파싱중 오류 발생</body>
</html>
"#;

    const SAMPLE_XML_2: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<법령>
  <기본정보>
    <법령ID>000002</법령ID>
    <공포일자>20240101</공포일자>
    <공포번호>00002</공포번호>
    <법종구분>대통령령</법종구분>
    <법령명_한글><![CDATA[테스트법 시행령]]></법령명_한글>
    <시행일자>20240101</시행일자>
    <연락부서><부서단위><소관부처명>법무부</소관부처명></부서단위></연락부서>
  </기본정보>
  <조문>
    <조문단위>
      <조문번호>1</조문번호>
      <조문제목><![CDATA[시행]]></조문제목>
      <조문내용><![CDATA[제1조 (시행) 시행한다.]]></조문내용>
    </조문단위>
  </조문>
</법령>
"#;

    const SAMPLE_XML_3: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<법령>
  <기본정보>
    <법령ID>000001</법령ID>
    <공포일자>20240101</공포일자>
    <공포번호>00002</공포번호>
    <법종구분>법률</법종구분>
    <법령명_한글><![CDATA[테스트법]]></법령명_한글>
    <시행일자>20240101</시행일자>
    <연락부서><부서단위><소관부처명>법무부</소관부처명></부서단위></연락부서>
  </기본정보>
  <조문>
    <조문단위>
      <조문번호>2</조문번호>
      <조문제목><![CDATA[개정]]></조문제목>
      <조문내용><![CDATA[제2조 (개정) 테스트를 개정한다.]]></조문내용>
    </조문단위>
  </조문>
</법령>
"#;

    const SAMPLE_XML_RENAMED: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<법령>
  <기본정보>
    <법령ID>000001</법령ID>
    <공포일자>20250101</공포일자>
    <공포번호>00003</공포번호>
    <법종구분>법률</법종구분>
    <법령명_한글><![CDATA[새테스트법]]></법령명_한글>
    <시행일자>20250101</시행일자>
    <연락부서><부서단위><소관부처명>법무부</소관부처명></부서단위></연락부서>
  </기본정보>
  <조문>
    <조문단위>
      <조문번호>3</조문번호>
      <조문제목><![CDATA[개정]]></조문제목>
      <조문내용><![CDATA[제3조 (개정) 테스트를 다시 개정한다.]]></조문내용>
    </조문단위>
  </조문>
</법령>
"#;

    fn write_sample_xml(detail_dir: &Path, mst: &str, xml: &str) {
        fs::write(detail_dir.join(format!("{mst}.xml")), xml).unwrap();
    }

    fn write_jemulpo_xml(
        detail_dir: &Path,
        mst: &str,
        promulgation_date: &str,
        promulgation_number: &str,
        amendment: &str,
        article_text: &str,
    ) {
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<법령>
  <기본정보>
    <법령ID>014604</법령ID>
    <공포일자>{promulgation_date}</공포일자>
    <공포번호>{promulgation_number}</공포번호>
    <법종구분>법률</법종구분>
    <법종구분코드>010101</법종구분코드>
    <법령명_한글><![CDATA[인천광역시 제물포구ㆍ영종구 및 검단구 설치 등에 관한 법률]]></법령명_한글>
    <시행일자>20260701</시행일자>
    <제개정구분명>{amendment}</제개정구분명>
    <법령분류명>지방행정</법령분류명>
    <연락부서><부서단위><소관부처명>행정안전부</소관부처명></부서단위></연락부서>
  </기본정보>
  <조문>
    <조문단위>
      <조문번호>1</조문번호>
      <조문제목><![CDATA[목적]]></조문제목>
      <조문내용><![CDATA[{article_text}]]></조문내용>
    </조문단위>
  </조문>
</법령>
"#
        );
        write_sample_xml(detail_dir, mst, &xml);
    }

    #[test]
    fn plan_entries_sorts_and_assigns_paths() {
        let temp = TempDir::new().unwrap();
        let cache_dir = temp.path().join(".cache");
        let detail_dir = cache_dir.join("detail");
        let history_dir = cache_dir.join("history");
        fs::create_dir_all(&detail_dir).unwrap();
        fs::create_dir_all(&history_dir).unwrap();
        write_sample_xml(&detail_dir, "10", SAMPLE_XML_2);
        write_sample_xml(&detail_dir, "1", SAMPLE_XML_1);
        write_sample_xml(&detail_dir, "2", SAMPLE_XML_3);
        write_sample_xml(&detail_dir, "63422", SAMPLE_INVALID_HTML);
        fs::write(
            history_dir.join("테스트법.json"),
            r#"[{"법령일련번호":"1","제개정구분명":"제정"},{"법령일련번호":"2","제개정구분명":"일부개정"},{"법령일련번호":"10","제개정구분명":"일부개정"}]"#,
        )
        .unwrap();

        let (entries, _diagnostics) = plan_and_diagnose(&cache_dir).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].mst, "1");
        assert_eq!(entries[0].path, RepoPathBuf::kr_file("테스트법", "법률.md"));
        assert_eq!(entries[1].mst, "2");
        assert_eq!(entries[1].path, RepoPathBuf::kr_file("테스트법", "법률.md"));
        assert_eq!(entries[2].mst, "10");
        assert_eq!(
            entries[2].path,
            RepoPathBuf::kr_file("테스트법", "시행령.md")
        );
    }

    #[test]
    fn plan_entries_uses_latest_title_path_for_same_law_id() {
        let temp = TempDir::new().unwrap();
        let cache_dir = temp.path().join(".cache");
        let detail_dir = cache_dir.join("detail");
        fs::create_dir_all(&detail_dir).unwrap();
        write_sample_xml(&detail_dir, "1", SAMPLE_XML_1);
        write_sample_xml(&detail_dir, "3", SAMPLE_XML_RENAMED);

        let (entries, _diagnostics) = plan_and_diagnose(&cache_dir).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].mst, "1");
        assert_eq!(entries[1].mst, "3");
        assert_eq!(
            entries[0].path,
            RepoPathBuf::kr_file("새테스트법", "법률.md")
        );
        assert_eq!(
            entries[1].path,
            RepoPathBuf::kr_file("새테스트법", "법률.md")
        );
    }

    #[test]
    fn orphan_only_child_is_reported() {
        let temp = TempDir::new().unwrap();
        let cache_dir = temp.path().join(".cache");
        let detail_dir = cache_dir.join("detail");
        fs::create_dir_all(&detail_dir).unwrap();
        // SAMPLE_XML_2 is a 시행령 whose parent 법률 is absent from the cache.
        write_sample_xml(&detail_dir, "10", SAMPLE_XML_2);

        let (entries, diagnostics) = plan_and_diagnose(&cache_dir).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(diagnostics.orphan_children.len(), 1);
        let orphan = &diagnostics.orphan_children[0];
        assert_eq!(orphan.law_type, "대통령령");
        assert_eq!(orphan.parent_group, "테스트법");
    }

    #[test]
    fn skipped_unparsable_surfaces_in_diagnostics() {
        let temp = TempDir::new().unwrap();
        let cache_dir = temp.path().join(".cache");
        let detail_dir = cache_dir.join("detail");
        fs::create_dir_all(&detail_dir).unwrap();
        write_sample_xml(&detail_dir, "1", SAMPLE_XML_1);
        write_sample_xml(&detail_dir, "63422", SAMPLE_INVALID_HTML);

        let (entries, diagnostics) = plan_and_diagnose(&cache_dir).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(diagnostics.total_xml, 2);
        //
        // Error pages produce valid XML but empty basic metadata, so they surface under
        // `empty_metadata` rather than `unparsable`. Both count as skipped inputs.
        //
        assert_eq!(
            diagnostics.empty_metadata.len() + diagnostics.unparsable.len(),
            1
        );
        if !diagnostics.empty_metadata.is_empty() {
            assert!(diagnostics.empty_metadata[0].contains("63422"));
        }
    }

    #[test]
    fn validate_mode_reports_on_empty_cache() {
        let temp = TempDir::new().unwrap();
        let cache_dir = temp.path().join(".cache");
        let detail_dir = cache_dir.join("detail");
        fs::create_dir_all(&detail_dir).unwrap();

        let output = temp.path().join("output.git");
        run(Cli {
            cache_dir,
            output,
            validate: true,
            on_anomaly: OnAnomaly::Warn,
            strict: false,
            expect_laws: None,
            manifest: None,
            commit_date_basis: CommitDateBasis::Promulgation,
        })
        .unwrap();
    }

    #[test]
    fn manifest_head_sha_is_40_hex_chars() {
        let temp = TempDir::new().unwrap();
        let cache_dir = temp.path().join(".cache");
        let detail_dir = cache_dir.join("detail");
        let history_dir = cache_dir.join("history");
        fs::create_dir_all(&detail_dir).unwrap();
        fs::create_dir_all(&history_dir).unwrap();
        write_sample_xml(&detail_dir, "1", SAMPLE_XML_1);
        write_sample_xml(&detail_dir, "2", SAMPLE_XML_2);

        let output = temp.path().join("output.git");
        let manifest_path = temp.path().join("manifest.json");
        run(Cli {
            cache_dir,
            output,
            validate: false,
            on_anomaly: OnAnomaly::Warn,
            strict: false,
            expect_laws: None,
            manifest: Some(manifest_path.clone()),
            commit_date_basis: CommitDateBasis::Promulgation,
        })
        .unwrap();

        let manifest_json = fs::read_to_string(&manifest_path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&manifest_json).unwrap();
        let head = value["head_commit_sha"].as_str().unwrap();
        assert_eq!(head.len(), 40);
        assert!(
            head.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
    }

    #[test]
    fn end_to_end_builds_bare_repo() {
        let temp = TempDir::new().unwrap();
        let cache_dir = temp.path().join(".cache");
        let detail_dir = cache_dir.join("detail");
        let history_dir = cache_dir.join("history");
        fs::create_dir_all(&detail_dir).unwrap();
        fs::create_dir_all(&history_dir).unwrap();
        write_sample_xml(&detail_dir, "1", SAMPLE_XML_1);
        write_sample_xml(&detail_dir, "2", SAMPLE_XML_2);
        fs::write(
            history_dir.join("테스트법.json"),
            r#"[{"법령일련번호":"1","제개정구분명":"제정"},{"법령일련번호":"2","제개정구분명":"일부개정"}]"#,
        )
        .unwrap();

        let output = temp.path().join("output.git");
        run(Cli {
            cache_dir,
            output: output.clone(),
            validate: false,
            on_anomaly: OnAnomaly::Warn,
            strict: false,
            expect_laws: None,
            manifest: None,
            commit_date_basis: CommitDateBasis::Promulgation,
        })
        .unwrap();

        assert_eq!(
            git_stdout(&output, ["symbolic-ref", "--short", "HEAD"]).trim(),
            "main"
        );
        assert_eq!(
            git_stdout(&output, ["rev-list", "--count", "HEAD"]).trim(),
            "4"
        );

        let commits = git_stdout(&output, ["rev-list", "--reverse", "HEAD"]);
        let commits = commits.lines().collect::<Vec<_>>();
        assert_eq!(commits.len(), 4);

        let contributor_author = git_stdout(
            &output,
            ["show", "-s", "--format=%an%n%ae%n%cn%n%ce", commits[1]],
        );
        assert_eq!(
            contributor_author.lines().collect::<Vec<_>>(),
            vec![
                "Jihyeon Kim",
                "simnalamburt@gmail.com",
                "Jihyeon Kim",
                "simnalamburt@gmail.com"
            ]
        );

        let contributor_tree = git_stdout(&output, ["show", "-s", "--format=%T", commits[1]]);
        let readme_tree = git_stdout(&output, ["show", "-s", "--format=%T", commits[0]]);
        assert_eq!(contributor_tree.trim(), readme_tree.trim());

        let contributor_time = git_stdout(&output, ["show", "-s", "--format=%at %ai", commits[1]]);
        let readme_time = git_stdout(&output, ["show", "-s", "--format=%at %ai", commits[0]]);
        assert_eq!(contributor_time.trim(), readme_time.trim());
    }

    #[test]
    fn end_to_end_includes_jemulpo_lineage_missing_from_incremental_repo() {
        let temp = TempDir::new().unwrap();
        let cache_dir = temp.path().join(".cache");
        let detail_dir = cache_dir.join("detail");
        let history_dir = cache_dir.join("history");
        fs::create_dir_all(&detail_dir).unwrap();
        fs::create_dir_all(&history_dir).unwrap();
        write_jemulpo_xml(
            &detail_dir,
            "259479",
            "20240130",
            "20161",
            "제정",
            "제1조 (목적) 이 법은 인천광역시 제물포구ㆍ영종구 및 검단구를 설치한다.",
        );
        write_jemulpo_xml(
            &detail_dir,
            "281877",
            "20251230",
            "21247",
            "일부개정",
            "제1조 (목적) 이 법은 인천광역시 제물포구ㆍ영종구 및 검단구 설치를 정비한다.",
        );
        fs::write(
            history_dir.join("인천광역시 제물포구ㆍ영종구 및 검단구 설치 등에 관한 법률.json"),
            r#"[{"법령일련번호":"259479","법령명한글":"인천광역시 제물포구ㆍ영종구 및 검단구 설치 등에 관한 법률","제개정구분명":"제정","법령구분":"법률","공포일자":"20240130","공포번호":"20161"},{"법령일련번호":"281877","법령명한글":"인천광역시 제물포구ㆍ영종구 및 검단구 설치 등에 관한 법률","제개정구분명":"일부개정","법령구분":"법률","공포일자":"20251230","공포번호":"21247"}]"#,
        )
        .unwrap();

        let output = temp.path().join("output.git");
        run(Cli {
            cache_dir,
            output: output.clone(),
            validate: false,
            on_anomaly: OnAnomaly::Warn,
            strict: false,
            expect_laws: None,
            manifest: None,
            commit_date_basis: CommitDateBasis::Promulgation,
        })
        .unwrap();

        let initial_commit = git_stdout(&output, ["rev-list", "--max-parents=0", "HEAD"]);
        assert_eq!(initial_commit.lines().count(), 1);
        let mst_259479 = git_stdout(
            &output,
            ["log", "--all", "--format=%H", "--grep=법령MST: 259479"],
        );
        assert_eq!(mst_259479.lines().count(), 1);
        let mst_281877 = git_stdout(
            &output,
            ["log", "--all", "--format=%H", "--grep=법령MST: 281877"],
        );
        assert_eq!(mst_281877.lines().count(), 1);

        let latest = git_stdout(
            &output,
            [
                "show",
                "HEAD:kr/인천광역시제물포구ㆍ영종구및검단구설치등에관한법률/법률.md",
            ],
        );
        assert!(latest.contains("법령MST: 281877"));
        assert!(latest.contains("법령ID: '014604'"));
        assert!(latest.contains("# 인천광역시 제물포구ㆍ영종구 및 검단구 설치 등에 관한 법률"));
    }

    #[test]
    fn end_to_end_keeps_same_day_newest_revision_at_head() {
        let temp = TempDir::new().unwrap();
        let cache_dir = temp.path().join(".cache");
        let detail_dir = cache_dir.join("detail");
        fs::create_dir_all(&detail_dir).unwrap();
        write_jemulpo_xml(
            &detail_dir,
            "100",
            "20240130",
            "8",
            "제정",
            "제1조 (목적) 첫 번째 본문입니다.",
        );
        write_jemulpo_xml(
            &detail_dir,
            "200",
            "20240130",
            "31",
            "일부개정",
            "제1조 (목적) 최신 본문입니다.",
        );

        let output = temp.path().join("output.git");
        run(Cli {
            cache_dir,
            output: output.clone(),
            validate: false,
            on_anomaly: OnAnomaly::Warn,
            strict: false,
            expect_laws: None,
            manifest: None,
            commit_date_basis: CommitDateBasis::Promulgation,
        })
        .unwrap();

        let latest = git_stdout(
            &output,
            [
                "show",
                "HEAD:kr/인천광역시제물포구ㆍ영종구및검단구설치등에관한법률/법률.md",
            ],
        );
        assert!(latest.contains("법령MST: 200"));
        assert!(latest.contains("최신 본문입니다."));
    }

    #[test]
    fn law_body_helpers_keep_path_and_original_cause() {
        let path = Path::new("/nonexistent-law-dir/bad.xml");
        let cause = parse_law_body(b"<law><").unwrap_err();
        let error = parse_law_body_at(b"<law><", path).unwrap_err();
        let text = error.to_string();
        assert!(text.contains(&path.display().to_string()), "{text}");
        assert!(text.contains(&cause.to_string()), "{text}");
        assert!(
            error
                .chain()
                .any(|link| link.to_string() == cause.to_string())
        );

        let missing = read_law_xml(path).unwrap_err();
        assert!(missing.to_string().starts_with("failed to read "));
        assert!(missing.chain().last().unwrap().is::<std::io::Error>());
    }

    fn git_stdout<const N: usize>(repo: &Path, args: [&str; N]) -> String {
        let output = Command::new("git")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
}
