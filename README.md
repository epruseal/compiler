# legalize-kr / compiler

[legalize-kr/legalize-pipeline]이 만드는 `.cache/` 디렉토리를 입력으로 받아
법령(`legalize-kr`), 판례(`precedent-kr`), 행정규칙(`admrule-kr`),
자치법규(`ordinance-kr`) bare Git 저장소를 직접 써내는 Rust 컴파일러
모음입니다. 각 도메인이 한 Cargo workspace 안에 멤버 크레이트로
공존합니다.

[legalize-kr/legalize-pipeline]: https://github.com/legalize-kr/legalize-pipeline

## Workspace 구조

```
compiler/
├── Cargo.toml         # virtual workspace
├── Cargo.lock         # 단일 lockfile
├── admrules/          # 행정규칙 컴파일러 (binary: admrule-kr-compiler)
├── laws/              # 법령 컴파일러 (binary: legalize-kr-compiler)
├── ordinances/        # 자치법규 컴파일러 (binary: ordinance-kr-compiler)
└── precedents/        # 판례 컴파일러 (binary: precedent-kr-compiler)
```

멤버별 README는 역할이 다릅니다. `*/README.md`는 컴파일러 사용·개발 문서이고,
`*/assets/README.md`는 컴파일 결과 저장소의 루트 `README.md`로 포함되는
원본입니다.

| 도메인 | 컴파일러 README | 결과 저장소 README 원본 |
|--------|-----------------|--------------------------|
| 법령 | [`laws/README.md`](laws/README.md) | [`laws/assets/README.md`](laws/assets/README.md) |
| 판례 | [`precedents/README.md`](precedents/README.md) | [`precedents/assets/README.md`](precedents/assets/README.md) |
| 행정규칙 | [`admrules/README.md`](admrules/README.md) | [`admrules/assets/README.md`](admrules/assets/README.md) |
| 자치법규 | [`ordinances/README.md`](ordinances/README.md) | [`ordinances/assets/README.md`](ordinances/assets/README.md) |

## 빠른 시작

```bash
# 워크스페이스 전체 빌드 (네 컴파일러 바이너리 동시 산출)
cargo build --workspace --release

# 법령 컴파일
./target/release/legalize-kr-compiler ../.cache -o ./output.git

# 판례 컴파일
./target/release/precedent-kr-compiler ../.cache/precedent -o ./precedent-output.git

# 행정규칙 컴파일
./target/release/admrule-kr-compiler ../.cache/admrule -o ./admrule-output.git

# 자치법규 컴파일
./target/release/ordinance-kr-compiler ../.cache/ordinance -o ./ordinance-output.git

# 공통 점검/메타데이터 옵션
./target/release/legalize-kr-compiler ../.cache --validate
./target/release/precedent-kr-compiler ../.cache/precedent --manifest ./manifest.json
```

네 컴파일러 모두 기본 출력은 bare Git 저장소이며, `-o/--output`을 생략하면
`output.git`에 씁니다. `--validate`는 저장소를 쓰지 않고 JSON 점검 결과만
표준 출력으로 내보내며, `--manifest`는 빌드 결과의 `HEAD`와 엔트리 수를 JSON으로
기록합니다. `--validate --manifest`가 쓰는 manifest에는 date_index가 없고, 원
컴파일러의 manifest와 같은 형태입니다. enforcement 빌드의 `--validate`는
`--manifest`를 요구하며 date_index 없는 manifest를 씁니다. 행정규칙/자치법규
컴파일러의 기존 Markdown tree 출력은 `--tree`로 사용할 수 있습니다.

## 날짜 기준 옵션의 구현 상태

법령, 행정규칙과 자치법규 컴파일러는
`--commit-date-basis promulgation|enforcement`를 받습니다. 기본값은 `promulgation`입니다.
`enforcement`는 `--manifest <path>`를 요구합니다.
새 빌드는 manifest의 `date_index`에 원천 판본과 occurrence별 날짜 및 순위를 기록합니다.
manifest를 생략한 공포일 빌드는 `<output>.date-index.json`에 index를 기록합니다.
기본 공포일 빌드는 기존 출력을 원래 컴파일러와 같은 방식으로 다룹니다.
저장소는 쓰기를 마친 뒤 기존 출력을 교체하고, 그 전에 실패하면 기존 출력이 남습니다.
index는 출력 쓰기가 성공한 뒤 이전 index를 지우고, 새 index를 임시 파일에 써서 rename으로 게시합니다.
그래서 낡은 index가 새 출력 옆에 남지 않습니다.
`enforcement` 빌드는 기존 출력 저장소나 index가 있으면 실패합니다.
세 컴파일러가 모두 같습니다.
기본 빌드의 `entries_total`은 기존 renderer 또는 계획의 항목 수입니다.
시행일 빌드의 `entries_total`은 날짜 계획에 전달한 원천 occurrence 수이며,
manifest의 `entries_total_basis`는 `source_occurrences`입니다.
이 수에는 계획에 전달하지 않은 파일이나 현행 snapshot 복원 항목이 들어가지 않습니다.
예를 들어 manifest의 count와 occurrence를 다음 명령으로 대조합니다.

```bash
jq '{entries_total, entries_total_basis, occurrences: ([.date_index.revisions[].occurrences | length] | add // 0)}' manifest.json
```
`--tree`는 행정규칙과 자치법규의 기본 공포일 빌드에서만 쓸 수 있습니다.

`enforcement` 빌드는 저장소 출력 경로 `<output>` 옆에 `<output>.label-date-applied.json` 진단 파일을 씁니다.
`--manifest`를 다른 곳에 두어도 이 파일은 manifest 옆이 아니라 `<output>` 옆에 생깁니다.
이 파일은 부칙 라벨 날짜가 공포일보다 늦어 라벨 날짜를 시행일로 적용한 판본의 목록입니다.
각 항목은 MST, 공포일, 라벨 날짜, 차이 일수와 라벨 원문을 담습니다.
목록이 비어도 파일을 쓰고, 기본 공포일 빌드는 이 파일을 쓰지 않습니다.
라벨 날짜 오타 같은 위험을 사람이 점검할 때 쓰는 진단 목록이며 판정에는 쓰지 않습니다.

행정규칙 캐시의 `retired` 폴더에서 같은 이름의 활성 사본이 있는 XML은 원래 컴파일러가 읽지 않던 사본입니다.
날짜 index를 만들 때 이런 사본의 XML을 파싱하지 못하면 빌드를 실패시키지 않고 그 사본을 index에서 뺍니다.
뺀 사본의 경로와 오류는 `date_index.unparsable`에 `{file, error}` 항목으로 남습니다.
활성 사본이 없는 retired 사본이 깨졌으면 원래처럼 빌드가 실패합니다.
두 날짜 기준이 같습니다.
법령과 자치법규의 `date_index.unparsable`은 항상 빈 배열입니다.

Git commit 시각은 1970-01-01보다 앞설 수 없어서, 1970년 이전 시행일의 commit은 모두 1970-01-01로 기록됩니다.
그래서 이런 날짜는 `git rev-list --before`로 찾을 수 없습니다.
`enforcement` 빌드는 날짜별 commit message에 시행일 `Effective-Date: YYYY-MM-DD`를 trailer로 남깁니다.
예를 들어 `git log --grep='^Effective-Date: 1960-08-10$'`로 그 날짜의 commit을 찾습니다.
consumer CLI의 날짜 조회(`--date`와 `--date-index`)는 index의 날짜를 쓰므로 이 제한을 받지 않습니다.
기본 공포일 빌드의 commit message에는 이 trailer가 없습니다.

`enforcement` 저장소의 첫 README commit은 기본 빌드와 같은 고정 날짜(2026-03-30 12:00 KST)를 씁니다.
날짜 조회는 commit 시각이 아니라 index의 snapshot 날짜를 씁니다.
자치법규의 `<부칙>`과 `<조문>`에 시행 문구가 없으면 `<제개정이유내용>` 안의 부칙 글을 같은 부칙 제목 판정과 같은 문장 규칙으로 읽습니다. 제목과 같은 줄에 있는 첫 문장도 읽습니다.
`<조문>`, `<부칙>`과 `<제개정이유내용>`이 모두 비어 있는 판본은 20일 기본값을 적용하지 않고 미해결로 둡니다.
같은 판본의 원 사본과 연혁(history) 사본은 표기와 메타데이터만 다르면(공백, 가운뎃점 문자 코드, 지자체기관명, 별표 첨부파일 키와 URL, 빈 요소) 하나로 합치고 연혁 사본을 씁니다.
내용이나 날짜가 다르면 두 사본 모두 미해결로 두고 reason에 사유를 적습니다.
공포일이 깨지고 시행일이 유효한 판본은 occurrence의 `undated_semantics`에 `공포일자`를 담습니다. 키가 없으면 빈 목록입니다.
기본 공포일 빌드에서는 manifest를 출력 저장소 안에 두어도 됩니다.

부칙 해석 규칙(귀속, 라벨, 단서, 폐지제정 등)과 개별 사례의 정본은 [시행일 예외 기록](docs/enforcement-date-exceptions.md)입니다.
이 README의 규칙 문장과 그 문서가 다르면 그 문서를 따릅니다.

새 법령 후보 renderer는 기존 resolver가 결정한 날짜와 근거를 받습니다.
권위 날짜가 없으면 YAML null을 기록하고 원문 날짜를 별도 필드에 보존합니다.
기본 공포일 renderer는 기존 날짜 형식 검사를 유지합니다.
생성자는 index를 게시하기 전에 occurrence의 날짜, 상태와 rank를 검증합니다.

시행일 모드의 전체 검증은 아직 끝나지 않았습니다.
실제 consumer CLI의 양 기준 교차 조회와 전체 법적 규칙 검증이 남아 있습니다.
결정성, 게시 실패와 독립 검증도 별도로 확인해야 합니다.

## CI 4종 게이트

push 전 다음 4종을 모두 로컬에서 통과시키세요:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --no-deps -- -D warnings
cargo shear
cargo test --workspace
```

Python parity 시험은 `LEGALIZE_PIPELINE_ROOT`로 참조 저장소를 받습니다.
시험은 임시 디렉터리에 입력과 출력을 만듭니다.
참조 저장소의 bytecode 쓰기를 막으려면 다음 환경을 사용하세요.

```bash
LEGALIZE_PIPELINE_ROOT=/path/to/legalize-pipeline PYTHONDONTWRITEBYTECODE=1 cargo test --workspace
```

`cargo-shear`가 없으면 해당 게이트는 미실행으로 기록합니다.
외부 oracle이 없어서 생략한 시험은 oracle 검증 통과를 뜻하지 않습니다.

## pre-commit 훅 활성화

`.githooks/pre-commit`에 `cargo fmt --check` 게이트가 있습니다. clone 또는
worktree 직후 한 번 실행해 활성화하세요:

```bash
git config core.hooksPath .githooks
```

---

*legalize-kr / compiler* is primarily distributed under the terms of both the
[Apache License (Version 2.0)] and the [MIT license]. See [COPYRIGHT] for
details.

[MIT license]: LICENSE-MIT
[Apache License (Version 2.0)]: LICENSE-APACHE
[COPYRIGHT]: COPYRIGHT
