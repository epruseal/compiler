//! Checks the date index outputs of the real ordinance compiler binary.

use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use git_writer::date_index::sha256;
use serde_json::Value;

/// Builds one ordinance whose only addendum is `부칙 {label} {clause}`.
fn ordinance(
    identity: &str,
    serial: &str,
    promulgation: &str,
    amendment: &str,
    label: &str,
    clause: &str,
    number: &str,
) -> String {
    format!(
        "<LawService><자치법규기본정보><자치법규ID>{identity}</자치법규ID><자치법규일련번호>{serial}</자치법규일련번호>\
         <공포일자>{promulgation}</공포일자><공포번호>{number}</공포번호><자치법규명><![CDATA[날짜 시험 조례]]></자치법규명>\
         <시행일자>99991231</시행일자><자치법규종류>C0001</자치법규종류><지자체기관명>서울특별시</지자체기관명>\
         <제개정정보>{amendment}</제개정정보></자치법규기본정보>\
         <조문><조 조문번호='000100'><조문번호>000100</조문번호><조문여부>Y</조문여부><조제목><![CDATA[목적]]></조제목>\
         <조내용><![CDATA[제1조(목적) 본문 {serial}.\n]]></조내용></조></조문>\
         <부칙><부칙공포일자>{promulgation}</부칙공포일자><부칙공포번호>{number}</부칙공포번호>\
         <부칙내용><![CDATA[부칙 {label} {clause}\n]]></부칙내용></부칙></LawService>"
    )
}

fn run<S: AsRef<OsStr>>(args: &[S]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ordinance-kr-compiler"))
        .args(args)
        .output()
        .unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{args:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn write_cache(root: &Path, files: &[(&str, String)]) -> std::path::PathBuf {
    let cache = root.join("cache");
    fs::create_dir_all(&cache).unwrap();
    for (name, xml) in files {
        fs::write(cache.join(format!("{name}.xml")), xml).unwrap();
    }
    cache
}

const PLAIN: &str = "제1조(시행일) 이 조례는 공포한 날부터 시행한다.";

#[test]
fn enforcement_needs_a_manifest_and_refuses_existing_output_and_index() {
    let temp = tempfile::tempdir().unwrap();
    let cache = write_cache(
        temp.path(),
        &[(
            "1",
            ordinance(
                "9500001",
                "1",
                "20200101",
                "제정",
                "(2020.1.1)",
                PLAIN,
                "11",
            ),
        )],
    );
    let output = temp.path().join("out.git");
    let manifest = temp.path().join("index.json");
    let basis = ["--commit-date-basis", "enforcement"];

    let failed = run(&[
        cache.as_os_str(),
        "-o".as_ref(),
        output.as_os_str(),
        basis[0].as_ref(),
        basis[1].as_ref(),
    ]);
    assert!(!failed.status.success());
    assert!(
        stderr(&failed).contains("requires --manifest"),
        "{}",
        stderr(&failed)
    );
    assert!(!output.exists());

    let tree = run(&[
        cache.as_os_str(),
        "-o".as_ref(),
        output.as_os_str(),
        basis[0].as_ref(),
        basis[1].as_ref(),
        "--manifest".as_ref(),
        manifest.as_os_str(),
        "--tree".as_ref(),
    ]);
    assert!(
        stderr(&tree).contains("requires bare repository output"),
        "{}",
        stderr(&tree)
    );

    let build = |out: &Path, index: &Path| {
        run(&[
            cache.as_os_str(),
            "-o".as_ref(),
            out.as_os_str(),
            basis[0].as_ref(),
            basis[1].as_ref(),
            "--manifest".as_ref(),
            index.as_os_str(),
        ])
    };
    let built = build(&output, &manifest);
    assert!(built.status.success(), "{}", stderr(&built));
    let index: Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    assert_eq!(index["date_index"]["basis"], "enforcement");
    // The report sits next to the repository, not next to the manifest, and is empty here.
    let report: Value = serde_json::from_slice(
        &fs::read(temp.path().join("out.git.label-date-applied.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(report["label_date_applied"], serde_json::json!([]));
    assert!(
        !temp
            .path()
            .join("index.json.label-date-applied.json")
            .exists()
    );

    let head = git(&output, &["rev-parse", "HEAD"]);
    let manifest_bytes = fs::read(&manifest).unwrap();
    let again = build(&output, &manifest);
    assert!(
        stderr(&again).contains("output already exists"),
        "{}",
        stderr(&again)
    );
    let other = temp.path().join("other.git");
    let indexed = build(&other, &manifest);
    assert!(
        stderr(&indexed).contains("date index already exists"),
        "{}",
        stderr(&indexed)
    );
    assert!(!other.exists());
    assert_eq!(git(&output, &["rev-parse", "HEAD"]), head);
    assert_eq!(fs::read(&manifest).unwrap(), manifest_bytes);
}

#[test]
fn default_build_replaces_output_and_stale_index_and_writes_no_date_files_in_the_tree() {
    let temp = tempfile::tempdir().unwrap();
    let cache = write_cache(
        temp.path(),
        &[
            (
                "1",
                ordinance(
                    "9500001",
                    "1",
                    "20200101",
                    "제정",
                    "(2020.1.1)",
                    PLAIN,
                    "11",
                ),
            ),
            (
                "2",
                ordinance(
                    "9500001",
                    "2",
                    "20210601",
                    "일부개정",
                    "(2021.7.1)",
                    PLAIN,
                    "22",
                ),
            ),
        ],
    );
    let output = temp.path().join("out.git");
    let index_path = temp.path().join("out.git.date-index.json");
    let first = run(&[cache.as_os_str(), "-o".as_ref(), output.as_os_str()]);
    assert!(first.status.success(), "{}", stderr(&first));
    let head = git(&output, &["rev-parse", "HEAD"]);
    // A stale index from an earlier run must not survive a rebuild.
    fs::write(&index_path, "{\"stale\": true}").unwrap();
    let second = run(&[cache.as_os_str(), "-o".as_ref(), output.as_os_str()]);
    assert!(second.status.success(), "{}", stderr(&second));
    assert_eq!(
        git(&output, &["rev-parse", "HEAD"]),
        head,
        "same input, same objects"
    );
    let index: Value = serde_json::from_slice(&fs::read(&index_path).unwrap()).unwrap();
    assert_eq!(index["date_index"]["basis"], "promulgation");
    assert_eq!(index["date_index"]["head_commit_sha"], head);
    // The promulgation tree has no date files, and no staging or report file is left behind.
    assert!(!git(&output, &["ls-tree", "-r", "--name-only", "HEAD"]).contains(".legalize"));
    assert!(!temp.path().join("out.git.label-date-applied.json").exists());
    let leftovers: Vec<_> = fs::read_dir(temp.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn long_identity_and_label_date_files_appear_in_the_enforcement_tree() {
    // 33 Hangul characters: 99 bytes, so the hex name plus ".json" passes 200 bytes.
    let long = "가나다라마바사아자차카타파하".repeat(2) + "가나다라마";
    let temp = tempfile::tempdir().unwrap();
    let cache = write_cache(
        temp.path(),
        &[
            (
                "1",
                ordinance(&long, "1", "20200301", "제정", "(2020.3.1)", PLAIN, "11"),
            ),
            (
                "2",
                ordinance(
                    &long,
                    "2",
                    "20210601",
                    "일부개정",
                    "(2021.6.1)",
                    "제1조(시행일) 이 조례는 규칙이 정하는 날부터 시행한다.",
                    "22",
                ),
            ),
            (
                "3",
                ordinance(
                    "9500002",
                    "3",
                    "20100601",
                    "일부개정",
                    "(2010.7.1)",
                    PLAIN,
                    "33",
                ),
            ),
        ],
    );
    let output = temp.path().join("out.git");
    let manifest = temp.path().join("index.json");
    let built = run(&[
        cache.as_os_str(),
        "-o".as_ref(),
        output.as_os_str(),
        "--commit-date-basis".as_ref(),
        "enforcement".as_ref(),
        "--manifest".as_ref(),
        manifest.as_os_str(),
    ]);
    assert!(built.status.success(), "{}", stderr(&built));

    let hash = sha256(long.as_bytes());
    let directory = format!("{}/{}/sha256-{hash}", &hash[..2], &hash[2..4]);
    let tree = git(&output, &["ls-tree", "-r", "--name-only", "HEAD"]);
    let index_path = format!(".legalize/date-index/ordinances/{directory}.json");
    assert!(tree.lines().any(|path| path == index_path), "{tree}");
    let candidate_prefix = format!(".legalize/candidates/ordinances/{directory}/");
    assert!(
        tree.lines().any(|path| path.starts_with(&candidate_prefix)),
        "{tree}"
    );
    // A short identity uses its plain hex name.
    let short = sha256(b"9500002");
    let short_path = format!(
        ".legalize/date-index/ordinances/{}/{}/{}.json",
        &short[..2],
        &short[2..4],
        "9500002"
            .bytes()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    assert!(tree.lines().any(|path| path == short_path), "{tree}");

    // The external index points at the candidate path, and Git can read that body.
    let index: Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    let revisions = index["date_index"]["revisions"].as_array().unwrap();
    let unresolved = revisions.iter().find(|r| r["revision"] == "2").unwrap();
    assert_eq!(unresolved["status"], "unresolved");
    let body = &unresolved["body"];
    assert!(
        body["path"]
            .as_str()
            .unwrap()
            .starts_with(&candidate_prefix)
    );
    let text = git(
        &output,
        &[
            "show",
            &format!(
                "{}:{}",
                body["commit_sha"].as_str().unwrap(),
                body["path"].as_str().unwrap()
            ),
        ],
    );
    assert!(text.contains("본문 2."), "{text}");
    let file: Value =
        serde_json::from_str(&git(&output, &["show", &format!("HEAD:{index_path}")])).unwrap();
    assert_eq!(file["identity"], long);
    assert_eq!(file["revisions"].as_array().unwrap().len(), 2);

    // The label later than the promulgation date is applied and listed.
    let report: Value = serde_json::from_slice(
        &fs::read(temp.path().join("out.git.label-date-applied.json")).unwrap(),
    )
    .unwrap();
    let listed = report["label_date_applied"].as_array().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["identity"], "9500002");
    assert_eq!(listed[0]["revision"], "3");
    assert_eq!(listed[0]["promulgation_date"], "2010-06-01");
    assert_eq!(listed[0]["label_date"], "2010-07-01");
    assert_eq!(listed[0]["days_after_promulgation"], 30);
    assert_eq!(listed[0]["attributed_label"], "(2010.7.1)");
    let applied = revisions.iter().find(|r| r["revision"] == "3").unwrap();
    assert_eq!(applied["enforcement_date"], "2010-07-01");
}

#[test]
fn default_tree_keeps_the_old_index_when_the_write_fails_and_replaces_it_when_it_succeeds() {
    let temp = tempfile::tempdir().unwrap();
    let cache = write_cache(
        temp.path(),
        &[(
            "1",
            ordinance(
                "9500001",
                "1",
                "20200101",
                "제정",
                "(2020.1.1)",
                PLAIN,
                "11",
            ),
        )],
    );
    let output = temp.path().join("out");
    let index_path = temp.path().join("out.date-index.json");
    // An output path that is a regular file makes the tree write fail.
    fs::write(&output, "not a directory").unwrap();
    fs::write(&index_path, "old index").unwrap();
    let failed = run(&[
        cache.as_os_str(),
        "-o".as_ref(),
        output.as_os_str(),
        "--tree".as_ref(),
    ]);
    assert!(!failed.status.success());
    assert_eq!(fs::read_to_string(&index_path).unwrap(), "old index");
    // A successful tree write replaces the old index.
    fs::remove_file(&output).unwrap();
    let built = run(&[
        cache.as_os_str(),
        "-o".as_ref(),
        output.as_os_str(),
        "--tree".as_ref(),
    ]);
    assert!(built.status.success(), "{}", stderr(&built));
    let index: Value = serde_json::from_slice(&fs::read(&index_path).unwrap()).unwrap();
    assert_eq!(index["date_index"]["basis"], "promulgation");
    assert!(output.join("README.md").exists());
}

#[test]
fn default_commit_messages_have_no_effective_date_trailer_and_enforcement_ones_do() {
    let temp = tempfile::tempdir().unwrap();
    let cache = write_cache(
        temp.path(),
        &[(
            "1",
            ordinance(
                "9500001",
                "1",
                "20200101",
                "제정",
                "(2020.1.1)",
                PLAIN,
                "11",
            ),
        )],
    );
    let default = temp.path().join("default.git");
    assert!(
        run(&[cache.as_os_str(), "-o".as_ref(), default.as_os_str()])
            .status
            .success()
    );
    assert!(!git(&default, &["log", "--format=%B"]).contains("Effective-Date"));
    let enforced = temp.path().join("enforced.git");
    let manifest = temp.path().join("index.json");
    let built = run(&[
        cache.as_os_str(),
        "-o".as_ref(),
        enforced.as_os_str(),
        "--commit-date-basis".as_ref(),
        "enforcement".as_ref(),
        "--manifest".as_ref(),
        manifest.as_os_str(),
    ]);
    assert!(built.status.success(), "{}", stderr(&built));
    let found = git(
        &enforced,
        &["log", "--format=%s", "--grep=^Effective-Date: 2020-01-01$"],
    );
    assert!(found.contains("Record date snapshot 2020-01-01"), "{found}");
}

#[test]
fn history_copies_merge_when_only_notation_differs_and_stay_unresolved_otherwise() {
    let temp = tempfile::tempdir().unwrap();
    let a_root = ordinance(
        "9500001",
        "1",
        "20200101",
        "일부개정",
        "(2020.1.1)",
        PLAIN,
        "11",
    );
    let a_history = a_root
        .replace("서울특별시", "다른 지자체")
        .replace("제1조(목적) 본문 1.", "제1조(목적)\n  본문   1. ");
    let b_root = ordinance(
        "9500001",
        "2",
        "20210101",
        "일부개정",
        "(2021.1.1)",
        PLAIN,
        "22",
    );
    let b_history = b_root.replace("본문 2.", "다른 본문 2.");
    let cache = write_cache(temp.path(), &[("1", a_root), ("2", b_root)]);
    fs::create_dir_all(cache.join("history")).unwrap();
    fs::write(cache.join("history/1.xml"), a_history).unwrap();
    fs::write(cache.join("history/2.xml"), b_history).unwrap();
    let output = temp.path().join("out.git");
    let manifest = temp.path().join("index.json");
    let built = run(&[
        cache.as_os_str(),
        "-o".as_ref(),
        output.as_os_str(),
        "--commit-date-basis".as_ref(),
        "enforcement".as_ref(),
        "--manifest".as_ref(),
        manifest.as_os_str(),
    ]);
    assert!(built.status.success(), "{}", stderr(&built));
    let index: Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    let revisions = index["date_index"]["revisions"].as_array().unwrap();
    let copies = |serial: &str| -> Vec<&Value> {
        revisions
            .iter()
            .filter(|r| r["revision"] == serial)
            .collect()
    };
    // Notation only: one revision, the history copy.
    let merged = copies("1");
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0]["occurrences"][0]["source_path"], "history/1.xml");
    assert_eq!(merged[0]["status"], "confirmed");
    // Different content: both copies stay and neither gets a date.
    let conflict = copies("2");
    assert_eq!(conflict.len(), 2);
    for copy in &conflict {
        assert_eq!(copy["status"], "unresolved");
        assert_eq!(
            copy["occurrences"][0]["date_evidence"]["decision"],
            "duplicate-copy-conflict"
        );
    }
    // The history copy, which the default build commits, ranks last on its promulgation date.
    let top = conflict
        .iter()
        .max_by_key(|copy| {
            copy["occurrences"][0]["promulgation_rank"]
                .as_u64()
                .unwrap()
        })
        .unwrap();
    assert_eq!(top["occurrences"][0]["source_path"], "history/2.xml");
    // The first commit keeps the fixed date of the default build.
    let first = git(&output, &["log", "--reverse", "--format=%aI %s"]);
    assert_eq!(
        first.lines().next().unwrap(),
        "2026-03-30T12:00:00+09:00 initial commit"
    );
}

#[test]
fn default_build_accepts_an_index_inside_the_output_repository() {
    let temp = tempfile::tempdir().unwrap();
    let cache = write_cache(
        temp.path(),
        &[(
            "1",
            ordinance(
                "9500001",
                "1",
                "20200101",
                "제정",
                "(2020.1.1)",
                PLAIN,
                "11",
            ),
        )],
    );
    let output = temp.path().join("out.git");
    let manifest = output.join("index.json");
    let built = run(&[
        cache.as_os_str(),
        "-o".as_ref(),
        output.as_os_str(),
        "--manifest".as_ref(),
        manifest.as_os_str(),
    ]);
    assert!(built.status.success(), "{}", stderr(&built));
    assert!(manifest.exists());
    // Enforcement still refuses it.
    let other = temp.path().join("other.git");
    let inside = other.join("index.json");
    let refused = run(&[
        cache.as_os_str(),
        "-o".as_ref(),
        other.as_os_str(),
        "--commit-date-basis".as_ref(),
        "enforcement".as_ref(),
        "--manifest".as_ref(),
        inside.as_os_str(),
    ]);
    assert!(
        stderr(&refused).contains("outside the output repository"),
        "{}",
        stderr(&refused)
    );
}

#[test]
fn validate_manifests_carry_no_date_index_and_enforcement_validate_keeps_existing_ones() {
    let temp = tempfile::tempdir().unwrap();
    let cache = write_cache(
        temp.path(),
        &[(
            "1",
            ordinance(
                "9500001",
                "1",
                "20200101",
                "제정",
                "(2020.1.1)",
                PLAIN,
                "11",
            ),
        )],
    );
    let manifest = temp.path().join("plain.json");
    let plain = run(&[
        cache.as_os_str(),
        "--validate".as_ref(),
        "--manifest".as_ref(),
        manifest.as_os_str(),
    ]);
    assert!(plain.status.success(), "{}", stderr(&plain));
    let written: Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    assert!(written.get("date_index").is_none());
    assert_eq!(written["entries_total"], 1);
    let checked = temp.path().join("checked.json");
    let args = [
        cache.as_os_str(),
        "--validate".as_ref(),
        "--commit-date-basis".as_ref(),
        "enforcement".as_ref(),
        "--manifest".as_ref(),
        checked.as_os_str(),
    ];
    let first = run(&args);
    assert!(first.status.success(), "{}", stderr(&first));
    let written: Value = serde_json::from_slice(&fs::read(&checked).unwrap()).unwrap();
    assert!(written.get("date_index").is_none());
    let again = run(&args);
    assert!(
        stderr(&again).contains("date index already exists"),
        "{}",
        stderr(&again)
    );
}

#[test]
fn copies_of_one_revision_share_a_state_when_any_copy_conflicts() {
    let temp = tempfile::tempdir().unwrap();
    let valid = ordinance(
        "9500001",
        "1",
        "20200101",
        "일부개정",
        "(2020.1.1)",
        PLAIN,
        "11",
    )
    .replace("<시행일자>99991231", "<시행일자>20200101");
    // `2000111.xml` differs in content. `1.xml` and `history/1.xml` have the same bytes.
    let other = valid.replace("본문 1.", "다른 본문 1.");
    let cache = write_cache(temp.path(), &[("2000111", other), ("1", valid.clone())]);
    fs::create_dir_all(cache.join("history")).unwrap();
    fs::write(cache.join("history/1.xml"), &valid).unwrap();
    let output = temp.path().join("out.git");
    let manifest = temp.path().join("index.json");
    let built = run(&[
        cache.as_os_str(),
        "-o".as_ref(),
        output.as_os_str(),
        "--commit-date-basis".as_ref(),
        "enforcement".as_ref(),
        "--manifest".as_ref(),
        manifest.as_os_str(),
    ]);
    assert!(built.status.success(), "{}", stderr(&built));
    let index: Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    assert_eq!(index["entries_total"], 3);
    assert_eq!(index["entries_total_basis"], "source_occurrences");
    let revisions = index["date_index"]["revisions"].as_array().unwrap();
    assert_eq!(revisions.len(), 2, "two distinct XML copies");
    let mut sources = 0;
    for copy in revisions {
        assert_eq!(copy["status"], "unresolved");
        for occurrence in copy["occurrences"].as_array().unwrap() {
            sources += 1;
            assert_eq!(occurrence["status"], "unresolved");
            assert_eq!(
                occurrence["date_evidence"]["decision"],
                "duplicate-copy-conflict"
            );
        }
    }
    assert_eq!(sources, 3, "all three source files are indexed");
    let default_output = temp.path().join("default.git");
    let default_manifest = temp.path().join("default.json");
    let default = run(&[
        cache.as_os_str(),
        "-o".as_ref(),
        default_output.as_os_str(),
        "--manifest".as_ref(),
        default_manifest.as_os_str(),
    ]);
    assert!(default.status.success(), "{}", stderr(&default));
    let old: Value = serde_json::from_slice(&fs::read(default_manifest).unwrap()).unwrap();
    assert_eq!(old["entries_total"], 1);
    assert!(old.get("entries_total_basis").is_none());
}

#[test]
fn a_source_without_a_serial_builds_in_every_mode_and_is_keyed_by_its_path() {
    let temp = tempfile::tempdir().unwrap();
    let without_serial = ordinance(
        "9500001",
        "1",
        "20200101",
        "제정",
        "(2020.1.1)",
        PLAIN,
        "11",
    )
    .replace("<자치법규일련번호>1</자치법규일련번호>", "");
    let cache = write_cache(temp.path(), &[("2000111", without_serial)]);
    let default = temp.path().join("default.git");
    let built = run(&[cache.as_os_str(), "-o".as_ref(), default.as_os_str()]);
    assert!(built.status.success(), "{}", stderr(&built));
    let tree = temp.path().join("tree");
    let built = run(&[
        cache.as_os_str(),
        "-o".as_ref(),
        tree.as_os_str(),
        "--tree".as_ref(),
    ]);
    assert!(built.status.success(), "{}", stderr(&built));
    let output = temp.path().join("out.git");
    let manifest = temp.path().join("index.json");
    let built = run(&[
        cache.as_os_str(),
        "-o".as_ref(),
        output.as_os_str(),
        "--commit-date-basis".as_ref(),
        "enforcement".as_ref(),
        "--manifest".as_ref(),
        manifest.as_os_str(),
    ]);
    assert!(built.status.success(), "{}", stderr(&built));
    let index: Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    let revisions = index["date_index"]["revisions"].as_array().unwrap();
    assert_eq!(revisions.len(), 1);
    let revision = revisions[0]["revision"].as_str().unwrap();
    assert!(
        revision.starts_with("path:") && revision.ends_with("2000111.xml"),
        "{revision}"
    );
    assert_eq!(revisions[0]["status"], "confirmed");
    assert_eq!(
        revisions[0]["occurrences"][0]["date_evidence"]["revision_key"],
        "source-path"
    );
    git(&output, &["fsck", "--full"]);
}

#[test]
fn a_repeal_and_enact_revision_replaces_the_text_instead_of_deleting_it() {
    let temp = tempfile::tempdir().unwrap();
    let first = ordinance(
        "9500001",
        "1",
        "20200101",
        "제정",
        "(2020.1.1)",
        PLAIN,
        "11",
    );
    let second = ordinance(
        "9500001",
        "2",
        "20210101",
        "폐지제정",
        "(2021.1.1)",
        PLAIN,
        "22",
    );
    let cache = write_cache(temp.path(), &[("1", first), ("2", second)]);
    let output = temp.path().join("out.git");
    let manifest = temp.path().join("index.json");
    let built = run(&[
        cache.as_os_str(),
        "-o".as_ref(),
        output.as_os_str(),
        "--commit-date-basis".as_ref(),
        "enforcement".as_ref(),
        "--manifest".as_ref(),
        manifest.as_os_str(),
    ]);
    assert!(built.status.success(), "{}", stderr(&built));
    let index: Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    let second = index["date_index"]["revisions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|revision| revision["revision"] == "2")
        .unwrap();
    assert_eq!(second["status"], "confirmed");
    assert_eq!(second["repeal"], false);
    assert_eq!(
        second["occurrences"][0]["date_evidence"]["amendment_treated_as"],
        "제정"
    );
    let files = git(
        &output,
        &[
            "-c",
            "core.quotepath=false",
            "ls-tree",
            "-r",
            "--name-only",
            "HEAD",
        ],
    );
    let law = files
        .lines()
        .find(|path| path.ends_with("본문.md") && !path.starts_with(".legalize"))
        .unwrap_or_else(|| panic!("the new text is missing: {files}"));
    let text = git(&output, &["show", &format!("HEAD:{law}")]);
    assert!(text.contains("본문 2."), "{text}");
}

#[test]
fn a_repeal_and_enact_revision_with_only_a_repeal_sentence_deletes_the_text() {
    let temp = tempfile::tempdir().unwrap();
    let first = ordinance(
        "9500001",
        "1",
        "20200101",
        "제정",
        "(2020.1.1)",
        PLAIN,
        "11",
    );
    let second = ordinance(
        "9500001",
        "2",
        "20210101",
        "폐지제정",
        "(2021.1.1)",
        PLAIN,
        "22",
    )
    .replace("제1조(목적) 본문 2.", "시험 조례를 폐지한다.");
    let cache = write_cache(temp.path(), &[("1", first), ("2", second)]);
    let output = temp.path().join("out.git");
    let manifest = temp.path().join("index.json");
    let built = run(&[
        cache.as_os_str(),
        "-o".as_ref(),
        output.as_os_str(),
        "--commit-date-basis".as_ref(),
        "enforcement".as_ref(),
        "--manifest".as_ref(),
        manifest.as_os_str(),
    ]);
    assert!(built.status.success(), "{}", stderr(&built));
    let index: Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    let second = index["date_index"]["revisions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|revision| revision["revision"] == "2")
        .unwrap();
    assert_eq!(second["status"], "confirmed");
    assert_eq!(second["repeal"], true);
    assert_eq!(
        second["occurrences"][0]["date_evidence"]["amendment_treated_as"],
        "폐지"
    );
    let files = git(
        &output,
        &[
            "-c",
            "core.quotepath=false",
            "ls-tree",
            "-r",
            "--name-only",
            "HEAD",
        ],
    );
    assert!(
        !files
            .lines()
            .any(|path| path.ends_with("본문.md") && !path.starts_with(".legalize")),
        "the text must be deleted: {files}"
    );
}

#[test]
fn enforcement_rejects_manifest_collisions_and_missing_parents() {
    let temp = tempfile::tempdir().unwrap();
    let cache = write_cache(
        temp.path(),
        &[(
            "1",
            ordinance(
                "9500001",
                "1",
                "20200101",
                "제정",
                "(2020.1.1)",
                PLAIN,
                "11",
            ),
        )],
    );
    let output = temp.path().join("output.git");
    let diagnostic = temp.path().join("output.git.label-date-applied.json");
    let build = |manifest: &Path| {
        run(&[
            cache.as_os_str(),
            "-o".as_ref(),
            output.as_os_str(),
            "--commit-date-basis".as_ref(),
            "enforcement".as_ref(),
            "--manifest".as_ref(),
            manifest.as_os_str(),
        ])
    };
    let alias = build(&diagnostic);
    assert!(!alias.status.success());
    assert!(!output.exists());
    assert!(!diagnostic.exists());
    let missing = build(&temp.path().join("missing/index.json"));
    assert!(!missing.status.success());
    assert!(!output.exists());
    assert!(!diagnostic.exists());
    fs::write(&diagnostic, b"existing diagnostic").unwrap();
    let occupied = build(&temp.path().join("index.json"));
    assert!(!occupied.status.success());
    assert!(
        stderr(&occupied).contains("diagnostic already exists"),
        "{}",
        stderr(&occupied)
    );
    assert_eq!(fs::read(&diagnostic).unwrap(), b"existing diagnostic");
    assert!(!output.exists());
}

#[test]
fn enforcement_revision_commits_carry_the_identity_lines_of_the_default_mode() {
    // user-enforcement-mst-trailer-20261117: a consumer finds a past copy by these lines.
    let temp = tempfile::tempdir().unwrap();
    let cache = write_cache(
        temp.path(),
        &[
            (
                "1",
                ordinance(
                    "9500001",
                    "1",
                    "20200101",
                    "제정",
                    "(2020.1.1)",
                    PLAIN,
                    "11",
                ),
            ),
            (
                "2",
                ordinance(
                    "9500001",
                    "2",
                    "20210101",
                    "일부개정",
                    "(2021.1.1)",
                    PLAIN,
                    "22",
                ),
            ),
        ],
    );
    let output = temp.path().join("out.git");
    let manifest = temp.path().join("index.json");
    let built = run(&[
        cache.as_os_str(),
        "-o".as_ref(),
        output.as_os_str(),
        "--commit-date-basis".as_ref(),
        "enforcement".as_ref(),
        "--manifest".as_ref(),
        manifest.as_os_str(),
    ]);
    assert!(built.status.success(), "{}", stderr(&built));
    let log = git(&output, &["log", "--reverse", "--format=%x01%B"]);
    let revisions: Vec<&str> = log
        .split('\u{1}')
        .filter(|message| message.contains("\nrevision: "))
        .collect();
    assert_eq!(revisions.len(), 2, "{log}");
    for (message, serial) in revisions.iter().zip(["1", "2"]) {
        let lines =
            format!("\n\n자치법규ID: 9500001\n자치법규일련번호: {serial}\n\nEffective-Date: ");
        assert!(message.contains(&lines), "{message}");
    }
    // Date snapshot commits stay as they were.
    for message in log
        .split('\u{1}')
        .filter(|m| m.starts_with("Record date snapshot"))
    {
        assert!(!message.contains("자치법규"), "{message}");
    }
}
