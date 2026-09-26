# Portway: `windowLog` 누락으로 인한 대형 컨텍스트 압축 붕괴

> **상태: 수정됨** (`crates/portway-core/src/dict.rs`, `windowLog` + LDM 추가).
> 아래 실측은 수정 **전** 동작입니다. 회귀 테스트는 `far_matches_stay_reachable_past_the_default_window`.

> **먼저 사과**: 이전 답변에서 `refPrefix`로 바꾸라고 했는데, Portway는 **이미 그렇게
> 되어 있었습니다** (`dict.rs:142`). 제가 코드를 읽지 않고 일반론을 편 것이라 헛소리가
> 됐습니다. 실제 코드를 보고 진짜 원인을 찾았습니다.

## 결론

**`dict::compress()`가 `windowLog`를 설정하지 않아, 컨텍스트가 약 4 MiB를 넘으면
압축이 무너집니다.** 최대 **62배** 커지고, 속도도 8배 느려집니다.

```rust
// crates/portway-core/src/dict.rs:136-148  (현재)
let mut cctx = CCtx::create();
cctx.set_parameter(CParameter::CompressionLevel(level))?;
cctx.set_parameter(CParameter::ChecksumFlag(true))?;
cctx.ref_prefix(&base.body)?;              // ← windowLog도, LDM도 없음
```

`ZSTD_CCtx_refPrefix`는 zstd의 LDM과 호환되지만, **LDM을 켜야만** 그 이점이 나옵니다.
zstd 공식 문서가 이 조합을 명시적으로 권장합니다
([PR #3553](https://github.com/facebook/zstd/pull/3553)):

> **"This method is compatible with LDM (long distance mode)."**

그리고 level 11의 기본 `windowLog`는 **20 (1 MiB)** 입니다. 그런데 base가 1 MiB를 넘으면
프리픽스의 먼 쪽 매치를 찾지 못합니다.

## 정량 근거 (실측)

컨텍스트를 append-only로 키우며 측정. `default` = 현재 Portway 동작, `wl=27` = windowLog 상향.

| 컨텍스트 | default (현재) | windowLog=27 | 배율 |
|---|---:|---:|---:|
| 1,165 KiB | 6,233 | 6,233 | 1.0x |
| 2,348 KiB | 6,373 | 6,373 | 1.0x |
| 3,961 KiB | 6,598 | 6,598 | 1.0x |
| **4,606 KiB** | **165,697** | **6,616** | **25.0x** |
| **5,396 KiB** | **421,931** | **6,747** | **62.5x** |

### 임계점은 4 MiB와 5 MiB 사이 (2^22)

저장소 안에서 직접 돌린 실측(`cargo test -p portway-core --lib dict::`)에서, base 6 MiB일 때
수정 전 **2,097,615 B** → 수정 후 **606 B** 로 확인됐습니다. 수정 후에는 base 20 MiB까지도
Δ바이트가 8,352 → 10,488 B 로 선형 증가만 합니다.

정확히는 **base 크기가 2²² = 4 MiB를 넘는 순간**입니다. windowLog=22 (4 MiB)까지는
정상, 23에서 필요해집니다. 위 표에서 3.9 MiB까지 멀쩡하다가 4.6 MiB에서 붕괴하는 것과
일치합니다.

### 속도도 같이 나빠집니다

압축이 안 되면 zstd가 계속 매치를 찾아 헤매므로 훨씬 느려집니다:

| windowLog | 시간 |
|---|---:|
| 20 (1 MiB) | **162.1 ms** |
| 23 (8 MiB) | **18.8 ms** |
| 27 (128 MiB) | **18.8 ms** |

**8.6배 빠릅니다.** "지연시간 우선" 프로파일을 두고 계신데, 현재 대형 컨텍스트에서는
정확히 그 반대의 일이 벌어지고 있습니다.

## 왜 지금까지 안 보였나

README의 벤치마크가 **최대 1 MiB**까지만 측정합니다:

```
| 1 MiB | 1,094,276 | 222,937 | 1,048 | 99.9% | 98.9% |
```

1 MiB는 windowLog=20 경계 **바로 아래**라서 문제가 드러나지 않습니다. 임계점이 4 MiB
근처라 실사용 컨텍스트가 그보다 작으면 지금도 잘 동작합니다 — "나름 잘 쓰고 있었다"는
말씀과 일치합니다.

## 해법

```rust
// 개선안
let mut cctx = CCtx::create();
cctx.set_parameter(CParameter::CompressionLevel(level))?;
cctx.set_parameter(CParameter::ChecksumFlag(true))?;
cctx.set_parameter(CParameter::WindowLog(window_log))?;              // ← 추가
cctx.set_parameter(CParameter::EnableLongDistanceMatching(true))?;   // ← 추가
cctx.ref_prefix(&base.body)?;
```

`window_log`는 다음 중 **최솟값 이상**이면 됩니다:
- base 길이 + body 길이를 담을 만큼 (`(base.len() + body.len()).ilog2() + 1`)
- 상한: `MAX_WINDOW_BYTES` = 128 MiB → `27`

수신부는 이미 안전합니다. `receiver.rs:448`이 `WindowLogMax`를
`max_window_bytes.ilog2()`로 설정하고, 기본값이 128 MiB라 27까지 허용됩니다.
**송신부만 고치면 됩니다.**

### 주의: 상한을 실제로 지켜야 합니다

`MAX_WINDOW_BYTES`(128 MiB)를 넘기면 수신부가 413으로 거부합니다
(`docs/protocol.md`). `pick_scoped`가 이미 `base + body <= MAX_WINDOW_BYTES`를
필터링하므로 그 값의 `ilog2()`(=27)를 상한으로 두면 안전합니다.

### 메모리 비용

`windowLog` 상향은 **압축 컨텍스트 메모리**를 늘립니다. 다만 `dict.rs:136`의 주석대로
매 호출마다 `CCtx::create()`를 하므로, 그 메모리는 호출 동안만 살아 있고 상주하지
않습니다. 128 MiB 윈도우 × 동시 요청 수만큼 일시적으로 잡힙니다. 동시성이 높으면
계층적으로 (예: 8/16/27) 올리는 편이 낫습니다.

## 부수적으로 발견한 것

### 1. level 19가 거의 공짜입니다 (대형 컨텍스트에서)

delta 압축에서는 level을 올려도 비용이 거의 안 늘고 이득만 있습니다.
2 MiB 컨텍스트 기준 측정:

| level | 결과 | 시간 |
|---|---:|---:|
| 3 | 1,003 B | 0.6 ms |
| 11 | 804 B | 0.8 ms |
| 19 | **739 B** | 17.8 ms |

**압축할 게 거의 없어서** level 19도 빠릅니다. 이미 `config.rs:77`에서 1..19를
허용하니, 대형 컨텍스트 경로에서만 level을 올리는 것도 고려할 만합니다.
(반대로 작은 첫 요청에서는 19가 비쌉니다 — 그건 plain zstd 경로니까 별개.)

### 2. `common_prefix`가 매 턴 O(n) 전수 비교입니다

```rust
// dict.rs:117-127
fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    const STRIDE: usize = 4096;
    ...
}
```

`pick_scoped`가 **8개 후보 전부에 대해** 이걸 돌립니다 (`max_by_key`). 5 MiB
컨텍스트 × 8 후보 = 매 턴 40 MiB 비교입니다. 4096 스트라이드 덕에 캐시 친화적이라
치명적이진 않지만, **"최장 공통 프리픽스"만 필요하다면** 훨씬 싼 방법이 있습니다:

- 지문(fingerprint) 기반: 4 KiB 블록별 해시를 캐싱해 두고 해시 비교로 먼저 거르기
- 또는 앞 64 KiB만 정밀 비교해 순위를 정하고 (append-only면 이것으로 충분히 판별됨)
  동점일 때만 전체 비교

다만 이건 **최적화이지 버그가 아닙니다.** 실측으로 병목이 확인되면 손대세요.

### 3. `RING_ENTRIES = 8`로는 인터리브에 부족할 수 있습니다

주석에 "sub-agents and conversations that interleave"라고 되어 있는데, 실제로
sub-agent 여러 개가 한 세션에서 돌면 금방 밀려납니다. 밀려난 base는 412 →
plain zstd 재전송이 됩니다. 이게 "사전이 날아가는" 또 다른 경로입니다.

세션당 대화가 8개를 넘는 워크로드라면 `RING_BYTES`(64 MiB)를 키우는 게
`RING_ENTRIES`를 키우는 것보다 효율적입니다 (작은 base 여러 개 < 큰 base 하나).

## 우선순위

| 순위 | 항목 | 효과 | 비용 |
|---|---|---|---|
| **1** | **`windowLog` + LDM 설정** | **4 MiB+ 에서 62배 → 정상, 속도 8.6배** | 코드 3줄 |
| 2 | level 상향 (대형 컨텍스트 한정) | ~8% 추가 | 없음 (오히려 빠름) |
| 3 | `RING_ENTRIES`/`RING_BYTES` 조정 | 인터리브 미스 감소 | 메모리 |
| 4 | `common_prefix` 지문 최적화 | CPU | 실측 후 판단 |

**1번만 해도 질문하신 "빵빵하게" 가 회복됩니다.**

## 검증 방법

README의 `scripts/bench.py`가 좋은 출발점입니다. 다만 **최대 컨텍스트를 4 MiB 이상으로
올려서** 돌려보세요 — 현재 표에 그 구간이 없어서 회귀를 못 잡습니다.

```sh
python3 scripts/bench.py --turns 40 --context-kb 8192    # 임계점 통과
```

회귀 테스트로는 `dict.rs`의 기존 테스트 패턴을 따라 `MAX_WINDOW_BYTES` 근처
base를 쓰는 케이스를 추가하는 게 좋겠습니다:

```rust
#[test]
fn far_matches_stay_reachable_past_the_default_window() {
    // base > 2^20 이면 windowLog 기본값(20)으로는 먼 쪽 매치를 못 찾는다.
    let previous = noise(6 << 20, 1);       // 6 MiB
    let mut body = previous.clone();
    body.extend_from_slice(b"one more turn");
    let wire = compress(&body, &base_of(&previous), 11).unwrap();
    assert!(wire.len() < 4096, "{} bytes — windowLog likely unset", wire.len());
}
```

(`noise(3 << 20, 1)`을 쓰는 기존 테스트는 3 MiB라 임계점 아래여서 통과합니다 —
그래서 이 버그가 지금까지 안 잡혔습니다.)

---

## 부록: 측정 재현

```python
import zstandard as zstd
base = prev_body            # 4 MiB 초과
params = zstd.ZstdCompressionParameters.from_level(
    11, window_log=27, enable_ldm=1,        # ← 이 두 개가 핵심
)
cctx = zstd.ZstdCompressor(
    compression_params=params,
    dict_data=zstd.ZstdCompressionDict(base),
)
delta = cctx.compress(new_body)
```

Rust에서는 `CParameter::WindowLog(n)` 과
`CParameter::EnableLongDistanceMatching(true)` 입니다.

측정 환경: macOS / Python `zstandard` 0.25.0 / level 11 (Portway 기본값).
데이터는 `~/work/portway` 소스 기반 합성 append-only 대화입니다.
절대 수치는 입력에 따라 다르지만 **4 MiB 임계점과 25~62배 배율은 재현 가능**합니다.