# admrule-kr-compiler

[legalize-kr/legalize-pipeline]으로 만들어진 `.cache/admrule` 디렉토리를 git으로
바꿔주는 컴파일러입니다. 이 프로그램은 국가법령정보센터 API를 직접 호출하지
않고, 이미 존재하는 캐시만 입력으로 받습니다.

[legalize-kr/legalize-pipeline]: https://github.com/legalize-kr/legalize-pipeline

## 사용법

```bash
admrule-kr-compiler <input_cache_dir> [-o <output_git_dir>] [--validate] [--manifest <path>]
```

기본 출력 경로는 `./output.git`입니다. 결과물은 bare repo이므로 내용을 보려면
clone해서 확인하면 됩니다.

```bash
admrule-kr-compiler ../.cache/admrule
git clone ./output.git ./admrule-kr
cd admrule-kr
```

출력 bare repo 경로를 직접 지정할 수도 있습니다.

```bash
admrule-kr-compiler ../.cache/admrule -o ./another.git
```

저장소를 쓰기 전에 캐시 상태만 JSON으로 확인하려면 `--validate`를 사용합니다.
빌드 결과의 `HEAD`와 엔트리 수는 `--manifest <path>`로 기록할 수 있습니다.
기존 Markdown tree 디렉토리 출력이 필요하면 `--tree`를 사용합니다.

```bash
admrule-kr-compiler ../.cache/admrule --tree -o ./admrule-tree
```

## 동작 방식

2-pass로 동작합니다.

1. `{cache_dir}/*.xml`의 행정규칙 메타데이터와 본문을 읽어 entry를 만듭니다.
   캐시 파일은 **개정 버전(serial, `행정규칙일련번호`)마다 1개**입니다. 같은
   `행정규칙ID`의 여러 개정본이 각각 별도 XML로 존재하며, entry는 파일명이 아니라
   XML 내용의 `행정규칙ID`로 동일 행정규칙임을 식별합니다.
   (`행정규칙ID`가 없는 구형 레코드는 `행정규칙일련번호`를 fallback 키로 사용합니다.)
2. 원천의 `상위부처명`, `소관부처명`, `담당부서기관명`을 정규화해 저장소
   기관 경로를 결정합니다.
3. 경로 충돌 규칙을 적용해 출력 파일 경로를 확정합니다.
   - 기본 경로: `{기관경로...}/{행정규칙종류}/{행정규칙명}/본문.md`
   - 충돌 시: 행정규칙명에 `발령번호`, `행정규칙일련번호` 또는 두 값을 조합한
     접미사를 붙입니다.
4. entry를 다음 순서로 정렬합니다.
   - `발령일자 asc`
   - `행정규칙일련번호 asc (numeric)`
   - `출력 경로 asc`
5. 정렬된 순서대로 Markdown과 commit message를 만들고 commit을 작성합니다.
   **개정 버전마다 1개의 commit**이 생성되어 연혁이 보존됩니다(commit timestamp =
   발령일자). 같은 `행정규칙ID`의 개정으로 경로가 바뀌면 이전 경로의 파일을 함께
   삭제해 최신 경로로 이어집니다. 경로가 그대로면 동일 `본문.md`에 개정 이력이
   시간순으로 쌓입니다.

## 출력 Markdown 구조

본문 Markdown은 다음 섹션 순서로 구성됩니다.

```
---
(frontmatter)
---

(본문 조문)

## 제개정이유

(제개정이유내용 — 없으면 섹션 생략)

## 개정문

(개정문내용 — 없으면 섹션 생략)
```

`제개정이유내용`, `개정문내용`은 law.go.kr API의 MST 파라미터 응답에 포함되며,
detail XML에 CDATA로 제공된다.

## 출력 특성

- 매 실행마다 fresh bare repo를 새로 만듭니다.
- branch는 `main`입니다.
- 결과 저장소의 루트 `README.md`는 [`assets/README.md`](assets/README.md)를 사용합니다.
- commit author/committer는 `legalize-kr-bot <bot@legalize.kr>`입니다.
- commit timestamp는 발령일자 기준 KST `12:00:00`입니다.
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

*admrule-kr-compiler* is primarily distributed under the terms of both the
[Apache License (Version 2.0)] and the [MIT license].

[MIT license]: ../LICENSE-MIT
[Apache License (Version 2.0)]: ../LICENSE-APACHE
