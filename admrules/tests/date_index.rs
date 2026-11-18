//! Checks the date basis options of the real administrative rule compiler binary.

use std::fs;
use std::process::{Command, Output};

fn run(args: &[&std::path::Path], extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_admrule-kr-compiler"))
        .arg(args[0])
        .arg("-o")
        .arg(args[1])
        .args(extra)
        .args(
            args.get(2)
                .map(|manifest| ["--manifest".as_ref(), manifest.as_os_str()])
                .into_iter()
                .flatten(),
        )
        .output()
        .unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn enforcement_needs_a_manifest_bare_output_and_a_new_destination() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    fs::create_dir(&cache).unwrap();
    let output = temp.path().join("out.git");
    let manifest = temp.path().join("index.json");
    let basis = ["--commit-date-basis", "enforcement"];

    let result = run(&[&cache, &output], &basis);
    assert!(
        stderr(&result).contains("requires --manifest"),
        "{}",
        stderr(&result)
    );

    let result = run(
        &[&cache, &output, &manifest],
        &["--commit-date-basis", "enforcement", "--tree"],
    );
    assert!(
        stderr(&result).contains("requires bare repository output"),
        "{}",
        stderr(&result)
    );

    // The destination check runs before any input is read, so an empty cache is enough.
    fs::create_dir(&output).unwrap();
    let result = run(&[&cache, &output, &manifest], &basis);
    assert!(
        stderr(&result).contains("output already exists"),
        "{}",
        stderr(&result)
    );
    fs::remove_dir(&output).unwrap();
    fs::write(&manifest, "keep").unwrap();
    let result = run(&[&cache, &output, &manifest], &basis);
    assert!(
        stderr(&result).contains("date index already exists"),
        "{}",
        stderr(&result)
    );
    assert_eq!(fs::read_to_string(&manifest).unwrap(), "keep");
    assert!(!output.exists());
}

const RULE_XML: &str = "<AdmRulService><행정규칙ID>ABC</행정규칙ID><행정규칙일련번호>123</행정규칙일련번호>\
    <행정규칙명>시험 지침</행정규칙명><행정규칙종류>고시</행정규칙종류><소관부처명>행정안전부</소관부처명>\
    <발령번호>제1호</발령번호><발령일자>20240504</발령일자><시행일자>20240505</시행일자>\
    <제개정구분명>제정</제개정구분명><현행연혁구분>현행</현행연혁구분><조문내용>제1조 목적</조문내용></AdmRulService>";

#[test]
fn default_tree_keeps_the_old_index_when_the_write_fails_and_replaces_it_when_it_succeeds() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    fs::create_dir(&cache).unwrap();
    fs::write(cache.join("123.xml"), RULE_XML).unwrap();
    let output = temp.path().join("out");
    let index_path = temp.path().join("out.date-index.json");
    fs::write(&output, "not a directory").unwrap();
    fs::write(&index_path, "old index").unwrap();
    let failed = run(&[&cache, &output], &["--tree"]);
    assert!(!failed.status.success());
    assert_eq!(fs::read_to_string(&index_path).unwrap(), "old index");
    fs::remove_file(&output).unwrap();
    let built = run(&[&cache, &output], &["--tree"]);
    assert!(built.status.success(), "{}", stderr(&built));
    let index: serde_json::Value = serde_json::from_slice(&fs::read(&index_path).unwrap()).unwrap();
    assert_eq!(index["date_index"]["basis"], "promulgation");
    assert!(output.join("README.md").exists());
}

#[test]
fn validate_manifests_carry_no_date_index() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    fs::create_dir(&cache).unwrap();
    fs::write(cache.join("123.xml"), RULE_XML).unwrap();
    let output = temp.path().join("unused");
    let manifest = temp.path().join("plain.json");
    let plain = run(&[&cache, &output, &manifest], &["--validate"]);
    assert!(plain.status.success(), "{}", stderr(&plain));
    let written: serde_json::Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    assert!(written.get("date_index").is_none());
    assert_eq!(written["entries_total"], 1);
    let checked = temp.path().join("checked.json");
    let args = ["--validate", "--commit-date-basis", "enforcement"];
    let first = run(&[&cache, &output, &checked], &args);
    assert!(first.status.success(), "{}", stderr(&first));
    let written: serde_json::Value = serde_json::from_slice(&fs::read(&checked).unwrap()).unwrap();
    assert!(written.get("date_index").is_none());
    let again = run(&[&cache, &output, &checked], &args);
    assert!(
        stderr(&again).contains("date index already exists"),
        "{}",
        stderr(&again)
    );
}

#[test]
fn enforcement_counts_source_occurrences_without_running_snapshot() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    fs::create_dir_all(cache.join("retired")).unwrap();
    fs::write(cache.join("123.xml"), RULE_XML).unwrap();
    let retired = RULE_XML
        .replace(
            "<행정규칙일련번호>123</행정규칙일련번호>",
            "<행정규칙일련번호>124</행정규칙일련번호>",
        )
        .replace("제1조 목적", "제1조 다른 본문");
    fs::write(cache.join("retired/124.xml"), retired).unwrap();
    fs::write(
        cache.join("current_snapshot.json"),
        r#"{"schema_version":1,"observed_on":"2025-01-01","rules":{"ABC":"123"}}"#,
    )
    .unwrap();
    let default = temp.path().join("default.git");
    let default_manifest = temp.path().join("default.json");
    let result = run(&[&cache, &default, &default_manifest], &[]);
    assert!(result.status.success(), "{}", stderr(&result));
    let default_index: serde_json::Value =
        serde_json::from_slice(&fs::read(&default_manifest).unwrap()).unwrap();
    assert_eq!(default_index["entries_total"], 3);
    assert!(default_index.get("entries_total_basis").is_none());
    assert_eq!(default_index["current_snapshot_applied"], true);
    let enforcement = temp.path().join("enforcement.git");
    let manifest = temp.path().join("enforcement.json");
    let result = run(
        &[&cache, &enforcement, &manifest],
        &["--commit-date-basis", "enforcement"],
    );
    assert!(result.status.success(), "{}", stderr(&result));
    let index: serde_json::Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    assert_eq!(index["entries_total"], 2);
    assert_eq!(index["entries_total_basis"], "source_occurrences");
    assert_eq!(index["current_snapshot_applied"], false);
    let occurrences: usize = index["date_index"]["revisions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|revision| revision["occurrences"].as_array().unwrap().len())
        .sum();
    assert_eq!(occurrences, 2);
    fs::write(cache.join("current_snapshot.json"), "{invalid").unwrap();
    let bad = run(&[&cache, &temp.path().join("bad.git")], &[]);
    assert!(!bad.status.success());
    let unaffected_manifest = temp.path().join("unaffected.json");
    let unaffected = run(
        &[
            &cache,
            &temp.path().join("unaffected.git"),
            &unaffected_manifest,
        ],
        &["--commit-date-basis", "enforcement"],
    );
    assert!(unaffected.status.success(), "{}", stderr(&unaffected));
    let limited_manifest = temp.path().join("limited.json");
    let limited = run(
        &[&cache, &temp.path().join("limited.git"), &limited_manifest],
        &["--commit-date-basis", "enforcement", "--limit", "1"],
    );
    assert!(limited.status.success(), "{}", stderr(&limited));
    let limited_index: serde_json::Value =
        serde_json::from_slice(&fs::read(limited_manifest).unwrap()).unwrap();
    assert_eq!(limited_index["entries_total"], 1);
}

#[test]
fn enforcement_publication_refuses_collisions_and_records_the_semantic_fields() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    fs::create_dir(&cache).unwrap();
    fs::write(cache.join("123.xml"), RULE_XML).unwrap();
    let output = temp.path().join("out.git");
    let diagnostic = temp.path().join("out.git.label-date-applied.json");
    let basis = ["--commit-date-basis", "enforcement"];
    let alias = run(&[&cache, &output, &diagnostic], &basis);
    assert!(!alias.status.success());
    let missing = temp.path().join("missing/index.json");
    assert!(!run(&[&cache, &output, &missing], &basis).status.success());
    assert!(!output.exists() && !diagnostic.exists());
    fs::write(&diagnostic, b"existing").unwrap();
    let manifest = temp.path().join("index.json");
    assert!(!run(&[&cache, &output, &manifest], &basis).status.success());
    assert_eq!(fs::read(&diagnostic).unwrap(), b"existing");
    assert!(!output.exists() && !manifest.exists());
    fs::remove_file(&diagnostic).unwrap();
    let built = run(&[&cache, &output, &manifest], &basis);
    assert!(built.status.success(), "{}", stderr(&built));
    let index: serde_json::Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    assert_eq!(index["source_date_field"], "시행일자");
    assert_eq!(
        index["date_policy"]["clamped_dates_keep_original_order"],
        true
    );
    assert_eq!(index["final_state_deletions_marked"], false);
    assert_eq!(index["current_snapshot_applied"], false);
    let default_manifest = temp.path().join("default.json");
    let default = run(
        &[&cache, &temp.path().join("default.git"), &default_manifest],
        &[],
    );
    assert!(default.status.success(), "{}", stderr(&default));
    let old: serde_json::Value =
        serde_json::from_slice(&fs::read(default_manifest).unwrap()).unwrap();
    assert_eq!(old["source_date_field"], "발령일자");
    assert_eq!(
        old["date_policy"]["clamped_dates_keep_original_order"],
        false
    );
    assert_eq!(old["final_state_deletions_marked"], true);
    // No snapshot file: the default build applies nothing.
    assert_eq!(old["current_snapshot_applied"], false);
}

/// Builds a cache with one active rule and an archived copy with the same name.
fn cache_with_archived_copy(temp: &std::path::Path, archived: &str) -> std::path::PathBuf {
    let cache = temp.join("cache");
    fs::create_dir_all(cache.join("retired")).unwrap();
    fs::write(cache.join("123.xml"), RULE_XML).unwrap();
    fs::write(cache.join("retired/123.xml"), archived).unwrap();
    cache
}

fn read_json(path: &std::path::Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

#[test]
fn an_unparsable_archived_copy_with_an_active_namesake_is_left_out_and_reported() {
    let temp = tempfile::tempdir().unwrap();
    let broken = cache_with_archived_copy(temp.path(), "<AdmRulService><broken");
    let clean = temp.path().join("clean");
    fs::create_dir_all(&clean).unwrap();
    fs::write(clean.join("123.xml"), RULE_XML).unwrap();
    // Default build: the original build never read the archived namesake, so it succeeds,
    // writes the same repository and reports the copy inside the index object.
    let with_copy = temp.path().join("with.git");
    let without = temp.path().join("without.git");
    let manifest = temp.path().join("with.json");
    let result = run(&[&broken, &with_copy, &manifest], &[]);
    assert!(result.status.success(), "{}", stderr(&result));
    let reference = run(&[&clean, &without], &[]);
    assert!(reference.status.success(), "{}", stderr(&reference));
    let head = |path: &std::path::Path| {
        let out = std::process::Command::new("git")
            .args(["--git-dir"])
            .arg(path)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap()
    };
    assert_eq!(head(&with_copy), head(&without));
    let index = read_json(&manifest);
    let skipped = index["date_index"]["unparsable"].as_array().unwrap();
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0]["file"], "retired/123.xml");
    let error = skipped[0]["error"].as_str().unwrap();
    // The cause stands once and carries no path of the cache.
    assert!(error.len() > 3 && !error.contains('/'), "{error}");
    assert_eq!(error.matches("tag not closed").count(), 1, "{error}");
    let sources: Vec<&str> = index["date_index"]["revisions"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|revision| revision["occurrences"].as_array().unwrap())
        .map(|occurrence| occurrence["source_path"].as_str().unwrap())
        .collect();
    assert_eq!(sources, vec!["123.xml"]);
    // The report stays out of the top level of the build manifest.
    assert!(index.get("unparsable").is_none());
    // Without --manifest the sidecar index holds the same report.
    let sidecar_output = temp.path().join("sidecar.git");
    let built = run(&[&broken, &sidecar_output], &[]);
    assert!(built.status.success(), "{}", stderr(&built));
    let sidecar = read_json(&temp.path().join("sidecar.git.date-index.json"));
    assert_eq!(
        sidecar["date_index"]["unparsable"],
        index["date_index"]["unparsable"]
    );
    // The enforcement build reports it as well.
    let enforcement = temp.path().join("enforcement.git");
    let enforcement_manifest = temp.path().join("enforcement.json");
    let built = run(
        &[&broken, &enforcement, &enforcement_manifest],
        &["--commit-date-basis", "enforcement"],
    );
    assert!(built.status.success(), "{}", stderr(&built));
    let enforced = read_json(&enforcement_manifest);
    assert_eq!(
        enforced["date_index"]["unparsable"],
        index["date_index"]["unparsable"]
    );
    assert!(enforced.get("unparsable").is_none());
}

#[test]
fn a_sound_archived_copy_leaves_the_report_empty() {
    let temp = tempfile::tempdir().unwrap();
    let cache = cache_with_archived_copy(temp.path(), RULE_XML);
    let manifest = temp.path().join("index.json");
    let result = run(&[&cache, &temp.path().join("out.git"), &manifest], &[]);
    assert!(result.status.success(), "{}", stderr(&result));
    assert_eq!(
        read_json(&manifest)["date_index"]["unparsable"],
        serde_json::json!([])
    );
}

#[test]
fn an_unparsable_archived_copy_without_an_active_namesake_still_stops_both_modes() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    fs::create_dir_all(cache.join("retired")).unwrap();
    fs::write(cache.join("123.xml"), RULE_XML).unwrap();
    fs::write(cache.join("retired/124.xml"), "<AdmRulService><broken").unwrap();
    let default = run(&[&cache, &temp.path().join("default.git")], &[]);
    assert!(!default.status.success());
    let enforcement = run(
        &[
            &cache,
            &temp.path().join("enforcement.git"),
            &temp.path().join("e.json"),
        ],
        &["--commit-date-basis", "enforcement"],
    );
    assert!(!enforcement.status.success());
    assert!(!temp.path().join("e.json").exists());
}

#[test]
fn the_plain_validate_manifest_keeps_its_original_shape() {
    let temp = tempfile::tempdir().unwrap();
    let broken = cache_with_archived_copy(temp.path(), "<AdmRulService><broken");
    let manifest = temp.path().join("plain.json");
    let result = run(
        &[&broken, &temp.path().join("unused"), &manifest],
        &["--validate"],
    );
    assert!(result.status.success(), "{}", stderr(&result));
    let written = read_json(&manifest);
    assert!(written.get("date_index").is_none());
    assert!(written.get("unparsable").is_none());
}

#[test]
fn enforcement_revision_commits_carry_the_identity_lines_of_the_default_mode() {
    // user-enforcement-mst-trailer-20261117: the keys and order of the default-mode commit.
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    fs::create_dir(&cache).unwrap();
    fs::write(cache.join("123.xml"), RULE_XML).unwrap();
    let output = temp.path().join("out.git");
    let manifest = temp.path().join("index.json");
    let built = run(
        &[&cache, &output, &manifest],
        &["--commit-date-basis", "enforcement"],
    );
    assert!(built.status.success(), "{}", stderr(&built));
    let log = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(&output)
        .args(["log", "--format=%x01%B"])
        .output()
        .unwrap();
    let log = String::from_utf8(log.stdout).unwrap();
    let revisions: Vec<&str> = log
        .split('\u{1}')
        .filter(|message| message.contains("\nrevision: "))
        .collect();
    assert_eq!(revisions.len(), 1, "{log}");
    assert!(
        revisions[0].contains("\n\n행정규칙일련번호: 123\n행정규칙ID: ABC\n\nEffective-Date: "),
        "{}",
        revisions[0]
    );
}
