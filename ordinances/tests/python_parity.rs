use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const SAMPLE_XML: &str = r#"<자치법규>
  <자치법규ID>2000111</자치법규ID>
  <자치법규일련번호>12345</자치법규일련번호>
  <자치법규명>Ordinance of Dalseo-gu of Daegu Metropolitan City for Attraction of and Support for Business</자치법규명>
  <자치법규종류>C0001</자치법규종류>
  <지자체기관명>(구)전라남도교육청</지자체기관명>
  <공포일자>20210930</공포일자>
  <공포번호>7825</공포번호>
  <시행일자>202330  </시행일자>
  <제개정구분명>일부개정</제개정구분명>
  <자치법규분야명>일반공공행정</자치법규분야명>
  <담당부서명>4&#8228;3지원과</담당부서명>
  <조문>
    <조>
      <조문번호>000000</조문번호>
      <조문여부>N</조문여부>
      <조제목></조제목>
      <조내용>제1장 총칙</조내용>
    </조>
    <조>
      <조문번호>000001</조문번호>
      <조문여부>N</조문여부>
      <조제목></조제목>
      <조내용>제１장 전각 숫자 구조</조내용>
    </조>
    <조>
      <조문번호>000100</조문번호>
      <조문여부>Y</조문여부>
      <조제목>목적</조제목>
      <조내용>제1조(목적) 이 조례는 테스트를 목적으로 한다.</조내용>
    </조>
    <조>
      <조문번호>000702</조문번호>
      <조문여부>Y</조문여부>
      <조제목>재난안전예산조정위원회</조제목>
      <조내용>제7조의2(재난안전예산조정위원회) 위원회를 둔다.</조내용>
    </조>
    <조>
      <조문번호>000802</조문번호>
      <조문여부>Y</조문여부>
      <조제목></조제목>
      <조내용>제8조2 삭제한다.</조내용>
    </조>
    <조>
      <조문번호>000902</조문번호>
      <조문여부>Y</조문여부>
      <조제목>직무위임</조제목>
      <조내용>제9조의 2(직무위임) 삭제한다.</조내용>
    </조>
    <조>
      <조문번호>001000</조문번호>
      <조문여부>Y</조문여부>
      <조제목>관계기관 등의 협조</조제목>
      <조내용>제10조(관계기관 등의 협조) 관계 기관·단체의 이권(利權)에 관하여 협조를 요청할 수 있다.</조내용>
    </조>
    <조>
      <조문번호>010000</조문번호>
      <조문여부>Y</조문여부>
      <조제목>자료관리</조제목>
      <조내용>제100조(자료관리) 자료를 관리한다.</조내용>
    </조>
  </조문>
  <부칙>
    <부칙공포일자>20210930</부칙공포일자>
    <부칙공포번호>7825</부칙공포번호>
    <부칙내용>이 조례는 공포한 날부터 시행한다.</부칙내용>
    <부칙공포일자>20220101</부칙공포일자>
    <부칙공포번호>8000</부칙공포번호>
    <부칙내용>이 조례는 2022년 1월 1일부터 시행한다.</부칙내용>
  </부칙>
  <별표>
    <별표단위 별표키="1">
      <별표번호>0001</별표번호>
      <별표가지번호>00</별표가지번호>
      <별표구분>서식</별표구분>
      <별표제목><![CDATA[[별표 8] 연구직 및 지도직공무원 경력경쟁임용과 전직을 위한 자격증 구분 및 전직시험이 면제되는 자격증 구분표(서울특별시 테스트 조례)]]></별표제목>
      <별표첨부파일구분>hwp</별표첨부파일구분>
      <별표첨부파일명><![CDATA[http://www.law.go.kr/flDownload.do?gubun=ELIS&flSeq=1&flNm=test]]></별표첨부파일명>
    </별표단위>
  </별표>
</자치법규>"#;

#[test]
fn fixture_matches_python_pipeline_converter() {
    let Some(pipeline) = pipeline_dir() else {
        eprintln!("skipping Python parity test: legalize-pipeline checkout not found");
        return;
    };
    assert!(
        pipeline.join("ordinances").is_dir(),
        "legalize-pipeline checkout does not contain ordinances/: {}",
        pipeline.display()
    );

    let temp = tempfile::tempdir().unwrap();
    let cache_dir = temp.path().join("cache");
    let output_dir = temp.path().join("out");
    fs::create_dir(&cache_dir).unwrap();
    fs::write(cache_dir.join("2000111.xml"), SAMPLE_XML).unwrap();

    let status = Command::new(env!("CARGO_BIN_EXE_ordinance-kr-compiler"))
        .arg(&cache_dir)
        .arg("-o")
        .arg(&output_dir)
        .arg("--tree")
        .status()
        .unwrap();
    assert!(status.success());

    let (expected_path, expected_markdown) = python_reference(SAMPLE_XML);
    assert!(expected_path.starts_with("_미상/(구)전라남도교육청/"));
    let actual_path = output_dir.join(Path::new(&expected_path));
    let actual_markdown = fs::read_to_string(&actual_path)
        .unwrap_or_else(|err| panic!("read {}: {err}", actual_path.display()));

    assert_eq!(actual_markdown, expected_markdown);
    assert!(actual_markdown.contains("자치법규명: 'Ordinance of Dalseo-gu of Daegu Metropolitan City for Attraction of and Support\n  for Business'"));
    assert!(actual_markdown.contains("시행일자: '202330  '"));
    assert!(actual_markdown.contains("담당부서: '4․3지원과'"));
    assert!(!actual_markdown.contains("##### 제0조"));
    assert!(!actual_markdown.contains("##### 제702조"));
    assert!(actual_markdown.contains("## 제１장 전각 숫자 구조"));
    assert!(actual_markdown.contains("##### 제1조 (목적)"));
    assert!(actual_markdown.contains("##### 제7조의2 (재난안전예산조정위원회)"));
    assert!(actual_markdown.contains("##### 제8조의2\n\n2 삭제한다."));
    assert!(actual_markdown.contains("##### 제9조의2 (직무위임)\n\n의 2(직무위임) 삭제한다."));
    assert!(
        actual_markdown.contains("관계 기관ㆍ단체의 이권(利權)에 관하여 협조를 요청할 수 있다.")
    );
    assert!(actual_markdown.contains("##### 제100조 (자료관리)\n\n자료를 관리한다."));
    assert!(actual_markdown.contains("구분표(서울특별시 테스트\n      조례)'"));
    assert_eq!(actual_markdown.matches("## 부칙").count(), 1);
    assert!(actual_markdown.contains("이 조례는 공포한 날부터 시행한다."));
    assert!(actual_markdown.contains("이 조례는 2022년 1월 1일부터 시행한다."));
}

#[test]
fn new_integrated_jurisdiction_matches_python_pipeline_converter() {
    let Some(pipeline) = pipeline_dir() else {
        eprintln!("skipping Python parity test: legalize-pipeline checkout not found");
        return;
    };
    assert!(pipeline.join("ordinances").is_dir());

    let xml = SAMPLE_XML
        .replace("(구)전라남도교육청", "전남광주통합특별시 남구")
        .replace(
            "<자치법규ID>2000111</자치법규ID>",
            "<자치법규ID>2264080</자치법규ID>",
        )
        .replace(
            "<자치법규일련번호>12345</자치법규일련번호>",
            "<자치법규일련번호>2142049</자치법규일련번호>",
        );
    let temp = tempfile::tempdir().unwrap();
    let cache_dir = temp.path().join("cache");
    let output_dir = temp.path().join("out");
    fs::create_dir(&cache_dir).unwrap();
    fs::write(cache_dir.join("2142049.xml"), &xml).unwrap();

    let status = Command::new(env!("CARGO_BIN_EXE_ordinance-kr-compiler"))
        .arg(&cache_dir)
        .arg("-o")
        .arg(&output_dir)
        .arg("--tree")
        .status()
        .unwrap();
    assert!(status.success());

    let (expected_path, expected_markdown) = python_reference(&xml);
    assert!(expected_path.starts_with("전남광주통합특별시/남구/"));
    let actual_path = output_dir.join(&expected_path);
    assert_eq!(fs::read_to_string(actual_path).unwrap(), expected_markdown);
}

fn python_reference(xml: &str) -> (String, String) {
    let pipeline = pipeline_dir().expect("legalize-pipeline checkout is required");
    let script = r#"
import sys
from ordinances import converter
xml = sys.stdin.read()
path, markdown = converter.xml_to_markdown(xml)
print(path)
print("===MARKDOWN===")
print(markdown, end="")
"#;
    let output = Command::new(std::env::var("PYTHON").unwrap_or_else(|_| "python".to_string()))
        .arg("-c")
        .arg(script)
        .env("PYTHONPATH", &pipeline)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child.stdin.as_mut().unwrap().write_all(xml.as_bytes())?;
            child.wait_with_output()
        })
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let (path, markdown) = stdout.split_once("\n===MARKDOWN===\n").unwrap();
    (path.to_string(), markdown.to_string())
}

fn pipeline_dir() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("LEGALIZE_PIPELINE_ROOT") {
        return Some(PathBuf::from(path));
    }
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    let mut candidates = vec![repo_root.join("legalize-pipeline")];
    if let Some(parent) = repo_root.parent() {
        candidates.push(parent.join("legalize-pipeline"));
    }
    candidates
        .into_iter()
        .find(|path| path.join("ordinances").is_dir())
}
