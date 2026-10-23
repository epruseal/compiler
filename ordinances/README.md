# ordinance-kr-compiler

[legalize-kr/legalize-pipeline]으로 만들어진 `.cache/ordinance` 디렉토리를 git으로
바꿔주는 컴파일러입니다. 이 프로그램은 국가법령정보센터 API를 직접 호출하지
않고, 이미 존재하는 캐시만 입력으로 받습니다.

[legalize-kr/legalize-pipeline]: https://github.com/legalize-kr/legalize-pipeline

## 사용법

```bash
ordinance-kr-compiler <input_cache_dir> [-o <output_git_dir>] [--validate] [--manifest <path>]
```

기본 출력 경로는 `./output.git`입니다. 결과물은 bare repo이므로 내용을 보려면
clone해서 확인하면 됩니다.

```bash
ordinance-kr-compiler ../.cache/ordinance
git clone ./output.git ./ordinance-kr
cd ordinance-kr
```

출력 bare repo 경로를 직접 지정할 수도 있습니다.

```bash
ordinance-kr-compiler ../.cache/ordinance -o ./another.git
```

저장소를 쓰기 전에 캐시 상태만 JSON으로 확인하려면 `--validate`를 사용합니다.
빌드 결과의 `HEAD`와 엔트리 수는 `--manifest <path>`로 기록할 수 있습니다.
기존 Markdown tree 디렉토리 출력이 필요하면 `--tree`를 사용합니다.

```bash
ordinance-kr-compiler ../.cache/ordinance --tree -o ./ordinance-tree
```

## 동작 방식

2-pass로 동작합니다.

1. 기존 현행 캐시인 `{cache_dir}/*.xml`과 연혁 캐시인
   `{cache_dir}/history/*.xml`의 자치법규 메타데이터와 본문을 읽어 entry를 만듭니다.
   같은 `자치법규일련번호`가 두 위치에 있으면 `history/`의 serial-key 파일을
   우선하여 한 개정만 처리합니다.
   `history/{MST}.xml` 내부 일련번호가 없거나 파일명과 다르면 세 실행 모드 모두 실패하며 기존 출력을 보존합니다. 기존 ID 기반 현행 캐시 형식은 유지합니다.
   - `조례`, `규칙`, `훈령`, `예규`, `고시`, `의회규칙`이 아닌 원천 분류는
     본문 트리에 쓰지 않고 건너뜁니다.
   - XML 문법 오류는 출력 저장소를 쓰기 전에 실패합니다. 오류 파일을 건너뛴 불완전한 저장소는 생성하지 않습니다.
   - 중복 조문 번호나 반복된 조문 시작 표기는 `원문오류` 목록과 본문 경고로 표시합니다.
     조문과 부칙의 원문 필드는 등장 순서대로 코드 블록에 보존합니다. 공백, 중복, 제목과 내용을 수정하지 않습니다.
     XML 표준에 따른 엔티티 해석과 줄바꿈 처리는 적용됩니다. 원본 XML 캐시는 바꾸지 않습니다.
     Python 변환기와 세 실행 모드가 같은 경로와 Markdown을 생성합니다. 정상 판본의 출력 형식은 유지합니다.
     일일 갱신은 처음 게시할 선택 판본의 기존 XML도 재조회합니다. 재조회 실패 시 파일 작성과 커밋을 중단합니다.
     이 컴파일러는 제공된 캐시를 읽습니다. XML이 유효해도 현재 공식 원문과 같은지는 원천 재조회로 확인해야 합니다.
2. `지자체기관명`을 광역·기초 단위로 나누어 저장소 경로를 결정합니다.
3. 경로 충돌 규칙을 적용해 출력 파일 경로를 확정합니다.
   - 기본 경로: `{광역}/{기초 또는 _본청 또는 _교육청}/{자치법규종류}/{자치법규명}/본문.md`
   - 충돌 시: 자치법규명에 `공포번호`, `자치법규ID` 또는 두 값을 조합한
     접미사를 붙입니다.
4. entry를 다음 순서로 정렬합니다.
   - `공포일자 asc`
   - `자치법규일련번호 asc (numeric)`
   - `출력 경로 asc`
5. 정렬된 순서대로 Markdown과 commit message를 만들고 commit을 작성합니다.
   같은 `자치법규ID`의 개정으로 경로가 바뀌면 이전 경로의 파일을 함께
   삭제합니다.
   `제개정구분`이 `폐지`인 개정은 새 본문을 쓰지 않고 직전 경로의 파일을
   삭제하는 commit으로 남깁니다. 현재 캐시에 직전 경로가 없더라도 해당
   일련번호를 유실하지 않도록 동일 tree를 가리키는 빈 폐지 commit을 남깁니다.

## 출력 특성

- 매 실행마다 fresh bare repo를 새로 만듭니다.
- branch는 `main`입니다.
- 결과 저장소의 루트 `README.md`는 [`assets/README.md`](assets/README.md)를 사용합니다.
- commit author/committer는 `legalize-kr-bot <bot@legalize.kr>`입니다.
- commit timestamp는 공포일자 기준 KST `12:00:00`입니다.
- `1970-01-01` 이전 날짜 또는 잘못된 날짜는 epoch 이전 commit을 피하기 위해 clamp합니다.

## 개발

```bash
# test
cargo test

# format
cargo fmt

# lint
cargo clippy

# release build
cargo build --release
```

&nbsp;

---

*ordinance-kr-compiler* is primarily distributed under the terms of both the
[Apache License (Version 2.0)] and the [MIT license].

[MIT license]: ../LICENSE-MIT
[Apache License (Version 2.0)]: ../LICENSE-APACHE

폐지 판본의 조문 구조에 오류가 있으면 현행 파일은 삭제하되, 삭제 커밋 본문에 경고와 원문 필드를 보존합니다. 파일이 없는 폐지 판본도 Git 이력에서 원문을 확인할 수 있습니다.
