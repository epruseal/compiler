//! Checks source preservation at the real compiler boundary.

use std::fs;
use std::process::Command;

use serde_json::Value;

/// Builds a complete source with exact date text.
fn xml(publication: &str, enforcement: &str, body: &str) -> String {
    format!(
        "<법령><기본정보><법령ID>090001</법령ID><공포일자>{publication}</공포일자><공포번호>503</공포번호><법종구분>법률</법종구분><법령명_한글>검증법</법령명_한글><시행일자>{enforcement}</시행일자><제개정구분명>일부개정</제개정구분명></기본정보><조문><조문단위><조문번호>1</조문번호><조문내용>{body}</조문내용></조문단위></조문></법령>"
    )
}

#[test]
fn invalid_dates_preserve_final_authority_and_exact_raw_text() {
    for (publication, enforcement, body, status, effective) in [
        ("20020101", "broken", "원문", "unresolved", None),
        ("20020101", "99991231", "원문", "unresolved", None),
        ("20020101", "20020230", "원문", "unresolved", None),
        ("  broken  ", " broken ", "원문", "undated", None),
        (
            " broken ",
            " broken ",
            "이 법은 2002년 1월 1일부터 시행한다",
            "confirmed",
            Some("2002-01-01"),
        ),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join("cache");
        fs::create_dir_all(cache.join("detail")).unwrap();
        let source = xml(publication, enforcement, body);
        fs::write(cache.join("detail/503.xml"), &source).unwrap();
        let output = temp.path().join("out.git");
        let manifest = temp.path().join("index.json");
        let result = Command::new(env!("CARGO_BIN_EXE_legalize-kr-compiler"))
            .arg(&cache)
            .arg("-o")
            .arg(&output)
            .args(["--commit-date-basis", "enforcement", "--manifest"])
            .arg(&manifest)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let index: Value = serde_json::from_slice(&fs::read(manifest).unwrap()).unwrap();
        let revision = &index["date_index"]["revisions"][0];
        let occurrence = &revision["occurrences"][0];
        assert_eq!(occurrence["status"], status);
        assert_eq!(occurrence["raw_promulgation_date"], publication);
        assert_eq!(occurrence["raw_enforcement_date"], enforcement);
        assert_eq!(occurrence["enforcement_date"].as_str(), effective);
        if status == "undated" {
            for key in [
                "promulgation_date",
                "enforcement_date",
                "exposure_date",
                "promulgation_rank",
                "enforcement_rank",
            ] {
                assert!(occurrence[key].is_null(), "{key}");
            }
        }
        let reference = &revision["body"];
        assert_eq!(reference["kind"], "git");
        let result = Command::new("git")
            .arg("--git-dir")
            .arg(&output)
            .args([
                "show",
                &format!(
                    "{}:{}",
                    reference["commit_sha"].as_str().unwrap(),
                    reference["path"].as_str().unwrap()
                ),
            ])
            .output()
            .unwrap();
        assert!(result.status.success());
        let text = String::from_utf8(result.stdout).unwrap();
        let yaml = text
            .strip_prefix("---\n")
            .unwrap()
            .split_once("\n---\n")
            .unwrap()
            .0;
        let metadata: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(metadata["시행일자"].as_str(), effective);
        assert_eq!(metadata["공포일자_raw"].as_str(), Some(publication));
        assert_eq!(metadata["시행일자_raw"].as_str(), Some(enforcement));
        assert!(text.contains(body));
    }
}

#[test]
fn enforcement_needs_a_manifest_and_refuses_existing_output_and_index() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    fs::create_dir_all(cache.join("detail")).unwrap();
    fs::write(
        cache.join("detail/503.xml"),
        xml("20020101", "20020201", "원문"),
    )
    .unwrap();
    let run = |output: &std::path::Path, extra: &[&std::ffi::OsStr]| {
        let result = Command::new(env!("CARGO_BIN_EXE_legalize-kr-compiler"))
            .arg(&cache)
            .arg("-o")
            .arg(output)
            .args(["--commit-date-basis", "enforcement"])
            .args(extra)
            .output()
            .unwrap();
        (
            result.status.success(),
            String::from_utf8_lossy(&result.stderr).into_owned(),
        )
    };
    let output = temp.path().join("out.git");
    let manifest = temp.path().join("index.json");
    let (ok, message) = run(&output, &[]);
    assert!(!ok && message.contains("requires --manifest"), "{message}");
    let with_manifest = ["--manifest".as_ref(), manifest.as_os_str()];
    let (ok, message) = run(&output, &with_manifest);
    assert!(ok, "{message}");
    // Enforcement also writes the label-date diagnostics next to the repository, even when empty.
    let report: Value = serde_json::from_slice(
        &fs::read(temp.path().join("out.git.label-date-applied.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(report["label_date_applied"], serde_json::json!([]));
    let (ok, message) = run(&output, &with_manifest);
    assert!(
        !ok && message.contains("output already exists"),
        "{message}"
    );
    let (ok, message) = run(&temp.path().join("other.git"), &with_manifest);
    assert!(
        !ok && message.contains("date index already exists"),
        "{message}"
    );
}

#[test]
fn default_build_rewrites_its_index_and_leaves_no_date_files_in_the_tree() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    fs::create_dir_all(cache.join("detail")).unwrap();
    fs::write(
        cache.join("detail/503.xml"),
        xml("20020101", "20020201", "원문"),
    )
    .unwrap();
    let output = temp.path().join("out.git");
    let index_path = temp.path().join("out.git.date-index.json");
    let build = || {
        let result = Command::new(env!("CARGO_BIN_EXE_legalize-kr-compiler"))
            .arg(&cache)
            .arg("-o")
            .arg(&output)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    };
    build();
    fs::write(&index_path, "{\"stale\": true}").unwrap();
    build();
    let index: Value = serde_json::from_slice(&fs::read(&index_path).unwrap()).unwrap();
    assert_eq!(index["date_index"]["basis"], "promulgation");
    // The body is rendered while the sources are read, and its hash must match the commit.
    let body = &index["date_index"]["revisions"][0]["body"];
    assert_eq!(body["kind"], "git");
    let committed = Command::new("git")
        .arg("--git-dir")
        .arg(&output)
        .args([
            "show",
            &format!(
                "{}:{}",
                body["commit_sha"].as_str().unwrap(),
                body["path"].as_str().unwrap()
            ),
        ])
        .output()
        .unwrap();
    assert_eq!(
        git_writer::date_index::sha256(&committed.stdout),
        body["sha256"].as_str().unwrap()
    );
    let listing = Command::new("git")
        .arg("--git-dir")
        .arg(&output)
        .args(["ls-tree", "-r", "--name-only", "HEAD"])
        .output()
        .unwrap();
    assert!(!String::from_utf8_lossy(&listing.stdout).contains(".legalize"));
    assert!(!temp.path().join("out.git.label-date-applied.json").exists());
}

#[test]
fn plain_validate_reports_from_the_metadata_plan_without_reading_bodies() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    fs::create_dir_all(cache.join("detail")).unwrap();
    // The basic information is valid and the body ends with crossed closing tags.
    let broken = xml("20020101", "20020201", "원문")
        .replace("</조문단위></조문></법령>", "</조문></조문단위></법령>");
    fs::write(cache.join("detail/503.xml"), &broken).unwrap();
    let validate = |extra: &[&std::ffi::OsStr]| {
        Command::new(env!("CARGO_BIN_EXE_legalize-kr-compiler"))
            .arg(&cache)
            .arg("--validate")
            .args(extra)
            .output()
            .unwrap()
    };
    let plain = validate(&[]);
    assert!(
        plain.status.success(),
        "{}",
        String::from_utf8_lossy(&plain.stderr)
    );
    let report: Value = serde_json::from_slice(&plain.stdout).unwrap();
    assert_eq!(report["actual_laws"], 1);
    // A validation run builds no repository, so its manifest is the plain build manifest.
    let manifest = temp.path().join("index.json");
    let indexed = validate(&["--manifest".as_ref(), manifest.as_os_str()]);
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    let written: Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    assert!(written.get("date_index").is_none());
    assert_eq!(written["head_commit_sha"], "");
    assert_eq!(written["entries_total"], 1);
    // An enforcement validation checks the dates, which reads every body, but writes no index.
    let checked = temp.path().join("checked.json");
    let enforced = validate(&[
        "--commit-date-basis".as_ref(),
        "enforcement".as_ref(),
        "--manifest".as_ref(),
        checked.as_os_str(),
    ]);
    assert!(!enforced.status.success());
    assert!(!checked.exists());
}

#[test]
fn enforcement_validate_writes_a_manifest_without_a_date_index_and_keeps_existing_ones() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    fs::create_dir_all(cache.join("detail")).unwrap();
    fs::write(
        cache.join("detail/503.xml"),
        xml("20020101", "20020201", "원문"),
    )
    .unwrap();
    let manifest = temp.path().join("checked.json");
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_legalize-kr-compiler"))
            .arg(&cache)
            .arg("--validate")
            .args(["--commit-date-basis", "enforcement", "--manifest"])
            .arg(&manifest)
            .output()
            .unwrap()
    };
    let first = run();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let written: Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    assert!(written.get("date_index").is_none());
    let again = run();
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("date index already exists"));
}

#[test]
fn default_build_keeps_the_text_of_bodies_the_date_renderer_changes() {
    // A sentinel effective date is written differently in the date index body, so that body
    // cannot be matched to the committed one by hash and its text must stay in the index.
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    fs::create_dir_all(cache.join("detail")).unwrap();
    fs::write(
        cache.join("detail/503.xml"),
        xml("20020101", "99991231", "원문"),
    )
    .unwrap();
    let output = temp.path().join("out.git");
    let result = Command::new(env!("CARGO_BIN_EXE_legalize-kr-compiler"))
        .arg(&cache)
        .arg("-o")
        .arg(&output)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let index: Value =
        serde_json::from_slice(&fs::read(temp.path().join("out.git.date-index.json")).unwrap())
            .unwrap();
    let revision = &index["date_index"]["revisions"][0];
    assert_eq!(revision["status"], "unresolved");
    let body = &revision["body"];
    assert_eq!(body["kind"], "inline");
    assert_eq!(
        git_writer::date_index::sha256(body["text"].as_str().unwrap().as_bytes()),
        body["sha256"].as_str().unwrap()
    );
}

#[test]
fn default_build_checks_the_expected_count_before_it_reads_any_body() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    fs::create_dir_all(cache.join("detail")).unwrap();
    // The basic information is valid and the body ends with crossed closing tags.
    let broken = xml("20020101", "20020201", "원문")
        .replace("</조문단위></조문></법령>", "</조문></조문단위></법령>");
    fs::write(cache.join("detail/503.xml"), &broken).unwrap();
    let build = |expected: &str, output: &str| {
        Command::new(env!("CARGO_BIN_EXE_legalize-kr-compiler"))
            .arg(&cache)
            .arg("-o")
            .arg(temp.path().join(output))
            .args(["--expect-laws", expected])
            .output()
            .unwrap()
    };
    // The original order: the count guard (exit 3) comes before the body is parsed.
    let mismatch = build("2", "mismatch.git");
    assert_eq!(mismatch.status.code(), Some(3));
    assert!(
        String::from_utf8_lossy(&mismatch.stderr).contains("expected 2 entries, got 1"),
        "{}",
        String::from_utf8_lossy(&mismatch.stderr)
    );
    assert!(!temp.path().join("mismatch.git").exists());
    // With the right count the broken body is reported as before.
    let matching = build("1", "match.git");
    assert_eq!(matching.status.code(), Some(1));
}

/// The error page the law service returned instead of an XML body (real files 63422 and 67867).
const ERROR_PAGE: &str = "\r\n\r\n<!DOCTYPE html PUBLIC \"-//W3C//DTD XHTML 1.0 Transitional//EN\" \"http://www.w3.org/TR/xhtml1/DTD/xhtml1-transitional.dtd\">\r\n<html xmlns=\"http://www.w3.org/1999/xhtml\" lang=\"ko\" xml:lang=\"ko\">\r\n\r\n<head>\r\n<meta http-equiv=\"Content-Type\" content=\"text/html; charset=utf-8\" />\r\n    <title>Error</title>\r\n    <script type=\"text/javascript\">\r\n        alert(\"XML 파싱중 오류 발생\");\r\n        history.back();\r\n    </script>\r\n</head>\r\n\r\n<body>\r\n</body>\r\n</html>";

#[test]
fn enforcement_build_skips_and_reports_files_without_metadata_like_the_default_build() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    fs::create_dir_all(cache.join("detail")).unwrap();
    fs::write(
        cache.join("detail/503.xml"),
        xml("20020101", "20020201", "원문"),
    )
    .unwrap();
    for name in ["63422", "67867"] {
        fs::write(cache.join(format!("detail/{name}.xml")), ERROR_PAGE).unwrap();
    }
    let run = |args: &[&std::ffi::OsStr]| {
        Command::new(env!("CARGO_BIN_EXE_legalize-kr-compiler"))
            .arg(&cache)
            .args(args)
            .output()
            .unwrap()
    };
    let default_out = temp.path().join("default.git");
    let default_manifest = temp.path().join("default.json");
    let built = run(&[
        "-o".as_ref(),
        default_out.as_os_str(),
        "--manifest".as_ref(),
        default_manifest.as_os_str(),
    ]);
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let enforced_out = temp.path().join("enforced.git");
    let enforced_manifest = temp.path().join("enforced.json");
    let args = [
        "-o".as_ref(),
        enforced_out.as_os_str(),
        "--commit-date-basis".as_ref(),
        "enforcement".as_ref(),
        "--manifest".as_ref(),
        enforced_manifest.as_os_str(),
    ];
    let enforced = run(&args);
    assert!(
        enforced.status.success(),
        "{}",
        String::from_utf8_lossy(&enforced.stderr)
    );
    // Both manifests report the same skipped files, and the enforcement one adds the index.
    let default: Value = serde_json::from_slice(&fs::read(&default_manifest).unwrap()).unwrap();
    let enforced: Value = serde_json::from_slice(&fs::read(&enforced_manifest).unwrap()).unwrap();
    for key in ["unparsable", "empty_metadata", "orphan_children", "by_type"] {
        assert_eq!(default[key], enforced[key], "{key}");
    }
    assert_eq!(enforced["entries_total"], 1);
    assert_eq!(enforced["entries_total_basis"], "source_occurrences");
    let occurrences: usize = enforced["date_index"]["revisions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|revision| revision["occurrences"].as_array().unwrap().len())
        .sum();
    assert_eq!(occurrences, 1);
    let skipped = enforced["unparsable"].as_array().unwrap().len()
        + enforced["empty_metadata"].as_array().unwrap().len();
    assert_eq!(skipped, 2);
    // `--strict` turns the skipped files into exit code 2 after the build, as the default does.
    let strict_out = temp.path().join("strict.git");
    let strict_manifest = temp.path().join("strict.json");
    let strict = run(&[
        "-o".as_ref(),
        strict_out.as_os_str(),
        "--commit-date-basis".as_ref(),
        "enforcement".as_ref(),
        "--manifest".as_ref(),
        strict_manifest.as_os_str(),
        "--strict".as_ref(),
    ]);
    assert_eq!(strict.status.code(), Some(2));
    assert!(strict_manifest.exists());
}

#[test]
fn enforcement_publication_refuses_collisions_and_leaves_existing_files_untouched() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    fs::create_dir_all(cache.join("detail")).unwrap();
    fs::write(
        cache.join("detail/503.xml"),
        xml("20020101", "20020201", "원문"),
    )
    .unwrap();
    let output = temp.path().join("out.git");
    let diagnostic = temp.path().join("out.git.label-date-applied.json");
    let build = |manifest: &std::path::Path| {
        Command::new(env!("CARGO_BIN_EXE_legalize-kr-compiler"))
            .arg(&cache)
            .arg("-o")
            .arg(&output)
            .args(["--commit-date-basis", "enforcement", "--manifest"])
            .arg(manifest)
            .output()
            .unwrap()
    };
    assert!(!build(&diagnostic).status.success());
    assert!(
        !build(&temp.path().join("missing/index.json"))
            .status
            .success()
    );
    assert!(!output.exists() && !diagnostic.exists());
    fs::write(&diagnostic, b"existing").unwrap();
    assert!(!build(&temp.path().join("index.json")).status.success());
    assert_eq!(fs::read(&diagnostic).unwrap(), b"existing");
    assert!(!output.exists());
    fs::remove_file(&diagnostic).unwrap();
    let manifest = temp.path().join("index.json");
    let built = build(&manifest);
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    assert!(output.exists() && diagnostic.exists());
    let index: Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    assert_eq!(index["commit_date_basis"], "enforcement");
    assert_eq!(index["output_kind"], "bare");
    assert_eq!(index["current_snapshot_applied"], false);
    assert_eq!(index["final_state_deletions_marked"], false);
    assert_eq!(index["entries_total_basis"], "source_occurrences");
}
