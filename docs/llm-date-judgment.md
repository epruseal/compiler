# LLM 시행일 판단 정책

이 문서는 compiler의 enforcement 모드에서 LLM이 정하거나 교차 확인하는 시행일의 정책을 적는다. 판단 결과는 원장 파일에 따로 저장하고, 이 문서는 판단 정책, 프롬프트, 대상 범주, 사용자 결정 보호 목록을 정한다. 정본 이슈는 epruseal/compiler#2다.

## 결정 근거

- `user-llm-remaining-judgment-20261116`(compiler#1 댓글 6908338952): 파서 규칙은 compiler#1 r25c에서 멈춘다. 파서가 정하지 못한 판본은 LLM이 판단한다. XML 시행일자 없이 파서가 정한 판본은 LLM이 교차 확인하고 날짜가 다르면 미해결로 돌린다. 사용자 결정으로 미해결로 둔 범주도 LLM이 판단한다. LLM 판단은 구별해 표시하고 따로 저장한다.
- `lead-user-decision-protection-20261116`: 판본을 지목해 날짜나 결과를 정한 사용자 결정의 판본은 보호한다(아래 보호 목록).
- `lead-compiler2-check-contract-20261116`: 기계 검사는 출처를 확인한다. 인용 문장이 이 판본의 시행일 문장인지는 LLM이 판단하고 정확도로 측정한다.
- `lead-compiler2-design-freeze-20261116`: 설계 확정.
- `user-compiler2-phase1-20261116`: 1단계(파서가 정하지 못한 판본의 판단)만 Sonnet 5.5 기본 effort로 실행한다. 교차 확인은 1단계 결과를 보인 뒤 따로 정한다. LLM은 문장에 적힌 날짜만 채택하고 기간 계산과 사본 선택을 하지 않는다. 교차 확인에서 날짜 없는 시행 문장만 찾으면 불성립으로 기록하고 파서 날짜를 둔다. 일일 수집 스크립트는 고치지 않는다.
- `user-compiler2-shunt-path-20261116`: 이 머신의 사용자 구독 게이트웨이 경유를 인정한다. 유료 API 키(ANTHROPIC_API_KEY)는 쓰지 않는다.
- `lead-compiler2-f3-20261116`: 실행 policy가 원장 policy와 다르면 명시 인자 없이는 실행하지 않는다.
- `lead-compiler2-line-end-20261116`: 줄 끝은 문장 끝이 아니다(2026-11-09 사용자 결정과 같다).

## 대상과 우선순위

판본 사본 하나마다 아래 순서로 처리한다. 앞 항목에 걸리면 뒤 항목을 보지 않는다.

1. 보호 목록 판본: LLM 판단도 교차 확인도 하지 않는다. 사본 SHA-256이 목록에 없으면 사용자 확인 대기열로 보낸다.
2. 사본 내용이 다른 판본: 날짜를 채택하지 않고 사본별 후보만 남긴다. 채택 여부는 사용자 확인 항목이다.
3. 유효한 XML 시행일자: 그 날짜를 쓴다. 교차 확인하지 않는다.
4. 원문(조문, 부칙, 제개정이유, 개정문)이 모두 빈 판본: 대상이 아니다.
5. 원장 항목이 policy(model, effort, prompt_version, runner_version)와 다르거나 원천 SHA-256이 다르면 항목이 없는 것으로 본다.
6. 파서가 정하지 못한 판본: 기계 검사를 통과한 LLM 날짜를 status `llm`으로 쓴다.
7. 파서가 XML 시행일자 없이 정한 판본: 검사를 통과한 LLM 날짜가 같으면 일치, 다르면 미해결로 돌리고 두 후보를 남긴다. LLM이 날짜를 내지 못하면 불성립으로 기록하고 파서 결과를 둔다.
8. 원장 항목이 없는 판본: 파서 결과를 둔다. 대기열에 남는다.

LLM은 기간을 계산하지 않는다. "공포 후 1개월이 경과한 날" 같은 상대기간과 부칙이 빈 판본의 기본 기간(21일, 6일)은 LLM 날짜가 되지 못한다.

## 기계 검사 계약

`tools/llm-date-judgment/checks.py`가 판단 직후에 검사하고 결과를 원장에 쓴다. 날짜가 있는 판단은 아래를 모두 통과해야 채택할 수 있다.

- 인용이 지정한 칸 원문(공백 제거)에 그대로 있다.
- 인용이 같은 줄의 인용 부호 쌍 안에 있지 않다. 낫표는 법령 제목에 쓰이므로 세지 않는다.
- 날짜 문자열이 인용 안에 있고 달력상 유효하다.
- 시행 동사가 서술어 위치에 있다. 동사 어간은 인용 안에 있고 어미는 칸 원문에서 읽는다. 종결형이나 연결형(하되, 하고, 하며, 하나, 되며, 되고, 되되)만 받고 관형형과 명사형은 받지 않는다.
- 종결형(다, 함, 됨, 킴으로 끝나는 어미)은 칸 원문에서 바로 뒤 글자가 마침표, 쉼표, 세미콜론, 닫는 부호이거나 칸 끝일 때만 받는다. 그 밖의 글자(는, 고, 면, 던, 며, 지, 니, 에, 과 등)가 오면 인용형, 연결형, 명사형 쓰임이므로 받지 않는다. 뒤에 올 수 있는 글자가 열린 집합이므로 받는 글자를 나열한다. 줄 끝은 문장 끝이 아니다. 마침표 없이 줄이 끝나는 시행 문장은 채택하지 않고 미해결로 남는다.
- 공포일 날짜는 "공포한 날부터 시행" 꼴과 원천 공포일자가 맞아야 한다. 라벨 날짜는 자치법규의 유일한 부칙에서만 받는다.
- 부칙 제목이 둘 이상인 칸에서는 인용이 밝힌 제목의 구역 안에 있다. 다음 판본 공포일과 같거나 늦은 라벨의 구역은 "공포한 날"과 라벨 날짜의 근거가 되지 못한다(user-label-not-on-or-after-next-revision-20261103).

인용 문장이 적용례인지, 다른 법령의 시행 서술인지 같은 의미 판단은 검사하지 않는다. 그 정확도는 정답 표본으로 측정한다. 결과는 "LLM 판단" 표시로 사용자에게 보인다.

## 프롬프트

프롬프트 본문은 `tools/llm-date-judgment/prompts/v2.md`이고 출력 형식은 `schema/judgment.schema.json`이다. 판단 기준은 이슈에 기록된 사용자 결정과 리드 기술 재정만 옮기고 근거를 붙인다. 프롬프트 판은 시스템 문장, 프롬프트 본문, 출력 형식의 SHA-256으로 정한다(`judge.py prompt_version`).

호출은 지역 `claude -p`를 도구 없이 쓰고 사용자 설정을 읽지 않는다(`--setting-sources project`, 빈 작업 폴더). 실행 환경의 게이트웨이 변수는 그대로 물려준다. 유료 API 키는 쓰지 않는다. 실행기는 환경에 `ANTHROPIC_API_KEY`가 있으면 시작하지 않는다.

## 원장

운영 폴더 아래에 둔다.

- `ledger/<dataset>.json`: 정본. 최상위는 schema_version, dataset, policy, protected_list_sha256, entries다. 항목 키는 (dataset, identity, revision, source_sha256)이다.
- `ledger/<dataset>.journal.jsonl`: 판단마다 한 줄. compaction이 정본에 합친다.
- `ledger-history/<dataset>.jsonl`: 바뀐 항목.
- `raw/<prompt_version>/<model>/<effort>/<sha256>-<시각>-<임의값>/`: CLI 원 응답과 호출 인자. 덮어쓰지 않는다.
- `failures/<prompt_version>/<model>/<effort>/failures-<dataset>.json`: policy별 대상 실패 수. 프롬프트나 모델을 바꾸면 새로 센다.
- `queue/`: 대기열 생성기의 `new-<날짜>.jsonl`, `seen.jsonl`, `pending.json`.

판단 항목은 판단, 검사 결과, 채택 여부, 날짜, 모델, effort, 프롬프트 판, 실행기 판, 판단 시각, 입력 조건, 판단 시점의 파서 결과(manifest SHA-256 포함)와 교차 확인 결과, 사용량과 표시 가격을 담는다. 실행기 판이 바뀌면 compaction이 저장된 판단을 새 검사로 다시 검사하고 교차 확인 값도 다시 정한다. 보호 판본은 사유 "사용자 결정 보호"의 제외 항목으로 남는다. 원문이 바뀌면 SHA-256이 바뀌어 대기열로 돌아간다.

## 실행

```sh
# 대기열 생성(원자료 머리 스캔, LLM 호출 없음). 수집 뒤 사람이 직접 실행한다.
python3 -I tools/llm-date-judgment/queue_build.py --cache <cache> --ops <ops> \
  --protected git-writer/data/llm-protected-revisions.json [--manifest <enforcement manifest>]
# 판단(사용자가 승인한 호출 수를 --max-calls로 준다)
python3 -I tools/llm-date-judgment/judge.py run --dataset ordinances --model <model> --ops <ops> \
  --cache <cache> --targets <ops>/queue/pending.json \
  --protected git-writer/data/llm-protected-revisions.json --max-calls <n>
# 모델, effort, 프롬프트를 바꿀 때만: 재판단 대상 수를 먼저 출력하고 --max-calls 안에서 부른다
#   ... --replace-policy
# 시험
python3 -m unittest discover tools/llm-date-judgment/tests
```

대기열은 일일 수집에 연결하지 않는다(사용자 결정 Q4). 수집이 끝난 뒤 위 명령으로 직접 만든다. 대기열 생성기는 자기 잠금(`queue/lock`)만 잡고, 다른 생성기가 잡고 있으면 기다리지 않고 건너뛴다. 실행기의 운영 잠금은 잡지 않으므로 판단 중에도 만들 수 있다. 생성기는 처음 본 키를 `new-<날짜>.jsonl`에 적고, 바이트가 같은 사본은 대상 하나로 묶으며(`other_paths`), 원자료에서 더 찾지 못한 원장 항목 수(`stale_entries`)를 보고한다.

실행기는 운영 폴더의 잠금을 잡고 대상을 순서대로 하나씩 부른다. 원장 policy와 다른 모델, effort, 프롬프트로 실행하면 `--replace-policy` 없이 거부한다. 판단 없음이 연속 5번이면 멈추고 마지막 오류 요약을 보고한다. 이 연속 실패는 공통 원인(사용 한도, 로그인)으로 보아 대상별 실패 수에 넣지 않는다. 그 밖에 3번 실패한 대상은 건너뛰고 보고에 키를 적는다. 원천 파일이 없거나 바뀐 대상도 건너뛰고 키를 적는다. 원 응답만 있고 원장 항목이 없는 키는 같은 policy의 원 응답에서 항목을 복원한다.

## 사용자 결정 보호 목록

정본은 `git-writer/data/llm-protected-revisions.json`이다. 결정 기록(compiler#1 이슈 본문과 댓글 전량, compiler#2 본문, 시행일 예외 문서)에서 판본 번호를 기계적으로 뽑고 분류해 만들었다(`tools/llm-date-judgment/measure/protect_candidates.py`, `protect_classify.py`). 보호 기준은 다음과 같다.

- 승인 등록표의 판본이다.
- 사용자 결정 기록이 판본을 지목하고 결과를 적은 판본이다. 결정 질문의 예시가 고른 안 아래에서 결과를 하나로 정하는 판본도 든다.
- 예외 문서가 같은 문장에서 결과를 사용자 결정 id에 귀속한 판본이다. 리드 확인이 필요하다.

데이터 파일의 `record_result`는 결정 기록이 적은 결과 문자열이고, `expected`는 compiler#1 r25c 파서 결과(기록에서 온 509050과 2120871은 그 기록)에서 옮긴 기대값이며 출처는 `expected_source`에 있다. compiler 통합 때 이 필드들을 알려진 필드로 등록한다.

판본을 지목해 미해결로 둔 판본은 보호하지 않는다. 사용자가 판본을 지목해 새로 결정하면 이 표와 데이터 파일에 함께 더한다. 시험이 두 목록의 일치를 확인한다.

<!-- protected-revisions:start -->
| 판본 | 데이터셋 | 기록이 정한 결과 | 결정 id | 근거 |
|---|---|---|---|---|
| 2200000088451 | admrules | 20090627 | user-amendment-explicit-dates-20261113 | approval registry; user-amendment-explicit-dates-20261113 (comment 6841451678) |
| 2200000097543 | admrules | 19950825 | id 없음(2026-10-26 사용자 선택 "부칙처럼 해석") | issue body item: user selection (2026-10-26) "부칙처럼 해석" names this example and its sentence 본 지침은 1995년 8월 25일부터 시행한다 fixes one date under the chosen option |
| 2200000104677 | admrules | 19981007 | user-amendment-explicit-dates-20261113 | approval registry; user-amendment-explicit-dates-20261113 (comment 6841451678) |
| 496220 | ordinances | 19990101 (valid source effective date, replayed on it) | id 없음(2026-10-26 사용자 선택 "시행일로 재생, 정렬은 다음 키로") | issue body item: example of the broken promulgation date question; user selection "시행일로 재생, 정렬은 다음 키로" fixes the result |
| 509050 | ordinances | enactment, not repeal | user-repeal-and-enact-as-enactment-20261103 | comment 6620840356 user decision 3 names the result (repeal-and-enact with articles is an enactment) |
| 806099 | ordinances | 20080321 | single-label-date-priority-20261103 | comment 6593104875 (user decision lists the result) |
| 847263 | ordinances | 20060628 | single-label-date-priority-20261103 | comment 6593104875 (user decision lists the result) |
| 873698 | ordinances | 19990101 | user-explicit-date-kept-20261104 | user-explicit-date-kept-20261104 (comment 6623245364), comments 6624108041 and 6731875583 |
| 879919 | ordinances | keep current behaviour (21-day default) | user-879919-twenty-day-kept-20261104 | user-879919-twenty-day-kept-20261104 (comment 6624108041 item 5) |
| 931559 | ordinances | promulgation day (body clause first) | id 없음(2026-10-26 사용자 선택 "본문 시행 문구 우선") | issue body item: the chosen option "본문 시행 문구 우선" gives the promulgation day for this example |
| 1000238 | ordinances | 20071201 | id 없음(2026-10-27 사용자 결정, 댓글 6437632923) | approval registry; comment 6437632923 (user decision, one-month calculation) |
| 1006223 | ordinances | 20020821 | single-label-date-priority-20261103(2026-10-27 사용자 정정 포함) | comment 6437566094 (2026-10-27 user correction), comment 6593104875 (user decision lists the result) |
| 1011016 | ordinances | 20110101 | id 없음(2026-10-26 사용자 선택 "적힌 날짜 그대로", compiler#1 본문) | approval registry; issue body (explicit date earlier than promulgation kept as written), comment 6571062276 |
| 1018380 | ordinances | 20100101 | user-explicit-date-kept-20261104 | user-explicit-date-kept-20261104 (comment 6623245364 names the result that the chosen option keeps) |
| 1025947 | ordinances | 20110101 | user-proviso-scope-enforcement-sentence-20261103 | enforcement-date-exceptions.md E-CASES: the sentence gives 2011-01-01 and names user-proviso-scope-enforcement-sentence-20261103 |
| 1063983 | ordinances | 20070601 | user-1063983-explicit-clause-date-20261113 | approval registry; user-1063983-explicit-clause-date-20261113 (comment 6841521863) |
| 1079521 | ordinances | 19991231 | id 없음(2026-10-27 사용자 확인, 댓글 6437602564) | comment 6437602564 (2026-10-27 user confirmation), comment 6571062276 |
| 1081693 | ordinances | 20141021 | id 없음(2026-10-27 사용자 확인, 댓글 6437602564) | comment 6437602564 (2026-10-27 user confirmation), comment 6571062276 |
| 2113963 | ordinances | 19960701 | user-explicit-date-without-buteo-20261104 | user-explicit-date-without-buteo-20261104 (comment 6730055022 names the result) |
| 2120871 | ordinances | repealed from 2026-04-10 | user-repeal-and-enact-repeal-only-is-repeal-20261103 | comment 6620840356 user decision 3 names the result (user-repeal-and-enact-repeal-only-is-repeal-20261103) |
<!-- protected-revisions:end -->

## 정확도 (2026-11-16 측정, 기준값 아님)

정답 표본 90건은 사용자 결정과 리드 기록이 결과를 정한 판본이며 LLM 호출 전에 고정했다. 모델별로 한 번씩 판단했다. 측정 판은 프롬프트 `p2-706ea70aa0896c60`, 측정용 실행기 proto-2다. 저장된 판단을 실행기 r2의 검사로 다시 채점해도 아래 수는 같다.

날짜 결과 80건(등급 A, B, B2):

| 모델 | 정답 일치 | 잘못 채택 | 채택 안 함(판정 보류 또는 검사 탈락) |
|---|---|---|---|
| Opus 5.5 | 79 | 1 | 0 |
| Sonnet 5.5 | 79 | 1 | 0 |
| Haiku 5.5 | 74 | 3 | 3 |

구조 결과 10건(등급 S, 단서 찾기): Opus 10, Sonnet 10, Haiku 9.

Opus와 Sonnet의 잘못 채택 1건(2200000095587)은 자기 부칙이 없는 일부개정 판본에서 이전 판본 부칙의 날짜를 채택한 경우다. 기계 검사로 막지 못하는 알려진 한계이며 후속 과제다(stale-addendum-attribution). Haiku의 잘못 채택은 1017994, 819124, 2200000095587이다. 재현 명령은 `tools/llm-date-judgment/measure/gold_run.py`(`run`, `score`)에 있다.

## 알려진 한계

- 이전 판본 부칙 승계: 부칙 제목이 하나뿐인 칸에서는 인용한 명시 날짜가 이전 판본의 것인지 검사가 가리지 못한다(시험 `test_known_limit_previous_revision_addendum`).
- 마침표 없는 줄 끝 시행 문장은 채택하지 않는다. 운영 보고에서 건수를 센다.
- 연결 어미 뒤의 이어짐("-고 나서", "-고도", "-고는")은 검사가 막지 않는다. 운영 보고에서 건수를 센다.
- 여러 줄에 걸친 인용 부호와 전각 따옴표는 인용 깊이에 넣지 않는다. 실데이터 건수를 따로 잰다.

## 표시

LLM이 정한 날짜는 date index의 status `llm`, frontmatter의 `날짜판단상태: "llm"`과 `시행일근거문장`, 커밋 제목의 " (LLM 판단)"으로 파서 결과와 구별된다. 이 표시는 compiler#1 제품 이식 뒤 compiler 통합에서 구현한다. cli-tools는 epruseal/cli-tools#2에서 "LLM 판단"과 근거 문장을 보인다.

## 사용자 확인 항목

- 사본 내용이 다른 판본의 날짜 채택 여부(사용자 결정: 채택하지 않고 후보만 남긴다).
- 2단계 교차 확인 실행은 1단계 결과를 사용자에게 보인 뒤 정한다.
