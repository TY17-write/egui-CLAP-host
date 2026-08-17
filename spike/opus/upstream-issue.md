# Upstream issue (restsend/opus-rs)

`opus-rs` 0.1.26 の不具合として報告した本文。

- **投稿先**: <https://github.com/restsend/opus-rs/issues/11> (2026-08-12)
- 投稿時のタイトルは
  *"Encoded stream decodes to out-of-range samples in libopus at higher bitrates
  with tonal input; `celt_pvq_u`/`celt_pvq_v` also panic for large K"*

再現コードは同じディレクトリの `main.rs` / `pvq_check.rs` / `cwrs_bound.rs` /
`cwrs_check.rs` / `spectral_match.rs`。測定の経緯と、こちらの対処
(48 / 96kbps に絞った理由) は `docs/export_rate_plan.md` の
「高ビットレートは出さない」を参照。

## 0.1.28 で直った (2026-08-17 に確認)

**符号化の破綻は解消している。** 全ビットレートで「上げるほど元に近づく」に
戻った。以下は同じ物差し (`spectral_match`) での実測。ffmpeg は 9.0
(報告時は 8.1.1)。数値が大きいほど元に近い。

| 信号 | 版 | 48 | 96 | 128 | 160 | 176 | 192 | 224 | 256 | 320 | 384 | 448 | 510 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| サイン波 | 0.1.26 | 26.3 | 30.9 | 34.0 | 36.9 | **4.5** | **2.5** | **2.3** | **2.3** | **2.5** | **2.5** | **2.7** | 51.8 |
| サイン波 | 0.1.28 | 26.3 | 30.9 | 34.0 | 36.9 | 35.4 | 37.5 | 38.0 | 37.8 | 37.9 | 43.4 | 48.8 | 51.8 |
| 和音+ノイズ | 0.1.26 | 10.5 | 11.9 | 12.3 | **5.1** | **5.0** | **4.8** | **4.5** | **4.4** | **4.4** | **4.4** | **4.5** | 11.7 |
| 和音+ノイズ | 0.1.28 | 10.5 | 11.9 | 12.3 | 12.5 | 12.6 | 12.6 | 12.7 | 12.7 | 12.8 | 12.9 | 12.9 | 12.9 |

**0.1.26 の値は報告時の表と一致する** (測り直しても同じ) ので、物差しは同じもの。
壊れていた 48〜192kbps の範囲は上の表の左半分がそれで、0.1.28 では崩れが消え、
**510kbps まで単調に良くなる**。

**0.1.26 / 510kbps の値 (51.8 / 11.7) は信用してはいけない。** 聴くと明らかに
壊れているのに高く出る。**この指標は「低ければ壊れている」は言えるが、
「高ければ正常」は言えない。** 詳しくは `src/bin/spectral_match.rs` の冒頭。

PVQ 側も変わった。

| 見たもの | 0.1.26 | 0.1.28 |
|---|---|---|
| `celt_pvq_u/v` のパニック (N>=8) | K>=129 でパニック | パニックしない |
| 表外の `V(N,K)` | `wrapping_add` で巻き戻る | u32 の上限で飽和する |
| 索引が符号帳の外 (600件中) | 145件 | 0件 |
| 索引の往復 (600件中) | 124件が失敗 | 失敗なし |
| `pvq_search` の `sum(|y|)==K` (2280件) | 全て一致 | 全て一致 |

### 残っている点: `cwrsi` が戻ってこない

**パニックが無限ループに変わった箇所がある。** `cwrsi(N=200, K=2)` と
`(N=200, K=4)` が返らない (0.1.26 では同じ入力でパニックしていた)。
段階を切り分けたのは `cwrs_hang.rs`。

**符号化から到達する経路ではない。** `N=200` は本家 libopus が帯域分割で作る
`N` の集合 (176, 144, 96, ...) に無く、実際 `N=176` 以下は 0.1.28 で全て素通り
する。**書き出しには影響しない**が、失敗の仕方としてはパニックより悪いので、
上流へ追記するなら**この一点**。

### 本体側 (対応済み)

`host/src/opus.rs` の `BITRATES_KBPS` を **48 / 96 → 48 / 96 / 128 / 192** に
広げ、**実際の曲を 192kbps で書き出して聴いて確かめた** (ノイズにならない)。
経緯は `docs/export_rate_plan.md` の「0.1.28 で直った」。

**版を上げるとスタックが足りなくなる。** これは別件なので
`upstream-issue-stack.md` に分けた (原因は 0.1.27 の設計変更)。
`opus::to_bytes` が専用スレッドを立てるようにして塞いである。

---

### Up front

- **This report was written by an AI (Claude Opus 5)**, working with me. The
  investigation and the measurements below were also carried out AI-assisted,
  while verifying a small hobby audio host of mine that uses this crate. I have
  read it through and I am posting it, so any mistakes are mine to answer for —
  but if you would rather not engage with AI-written reports, please just say so
  or close this.
- I am not a codec expert. I have tried to keep what was actually measured
  separate from what is only a guess, and I would suggest treating the reading of
  the cause with some caution.

Thanks for making a pure-Rust Opus encoder; not needing a C toolchain is exactly
why I picked it.

### Environment

- `opus-rs` 0.1.26
- Windows 11, x86_64 (AVX2 available)
- Encoder: `OpusEncoder::new(48000, 2, Application::Audio)`, 20 ms frames
  (`frame_size = 960`), only `bitrate_bps` changed between runs
- Packets muxed into Ogg (`ogg` 0.9) and decoded with ffmpeg 8.1.1 (libopus)

### Symptom

Above a certain bitrate the encoded stream decodes (in libopus) to something that
no longer resembles the input. Raising the bitrate further does not help.

I first hit this on real musical material (output of a software instrument) at
192 kbps, where it sounds like noise. I then reduced it to two 2-second stereo
test signals:

- **sine** — 440 Hz left, 660 Hz right, amplitude 0.5
- **complex** — a three-tone chord per channel plus a little noise

Metric: encode, decode with libopus, then compare magnitude spectra against the
original input per 20 ms frame, reported as
`10*log10( sum(A²) / sum((A-B)²) )` averaged over frames — higher is better. The
encoder also writes the input as raw f32 so the comparison is against the actual
input rather than a re-derived reference.

| signal | bitrate | spectral match (dB) | worst frame (dB) |
|---|---|---|---|
| sine | 48 | 26.3 | 18.6 |
| sine | 96 | 30.6 | 13.1 |
| sine | 128 | 34.1 | 18.4 |
| sine | 160 | 35.9 | 18.1 |
| sine | **176** | **5.0** | −2.5 |
| sine | **192** | **2.7** | −2.1 |
| complex | 48 | 10.5 | −2.3 |
| complex | 96 | 11.9 | −1.4 |
| complex | 128 | 12.3 | −1.5 |
| complex | **160** | **5.1** | −4.7 |
| complex | **176** | 5.0 | −4.2 |
| complex | **192** | 4.8 | −6.9 |

The absolute values are not comparable between the two signals (the noisy one is
inherently harder to match). What stands out is that **raising the bitrate makes
the match worse**, which should not happen for a working encoder:

- sine: 35.9 dB at 160 kbps → 5.0 dB at 176 kbps
- complex: 12.3 dB at 128 kbps → 5.1 dB at 160 kbps

Listening agrees with the numbers. For the sine, 48–160 kbps are clean tones and
176/192 kbps are noise. For the complex signal, 48–128 kbps are noisy but clearly
pitched, while 160 kbps and above become pitchless noise ("like rain or running
water").

So the threshold appears to depend on the content: 176 kbps for the sine,
160 kbps for the complex signal, and my real material was already broken at
192 kbps (I did not test it lower). The highest bitrate that was clean for both
test signals was 128 kbps.

Reproduction: encode both signals at each bitrate, then

```sh
ffmpeg -v error -y -i sine_$k.opus -f f32le -ac 2 -ar 48000 dec_sine_$k.raw
```

and compare `dec_sine_$k.raw` against the dumped input as above.

### Background: what the table holds

Spelling this out because the rest of my reasoning depends on it, and because I
had to work it out from scratch — you obviously know this already, but it makes
the next section readable for anyone else who ends up here.

`V(N,K)` is the size of the PVQ codebook for a band of `N` samples carrying `K`
pulses: the number of integer vectors of length `N` whose absolute values sum to
`K`. Upstream describes it as

> the number of combinations, with replacement, of N items, taken K at a time,
> when a sign bit is added to each item taken at least once

I checked the ported table against a brute-force count of those vectors for small
`N`/`K`, and they agree exactly:

| N | K | V(N,K) from table | brute force |
|---|---|---|---|
| 2 | 1 | 4 | 4 |
| 2 | 2 | 8 | 8 |
| 3 | 3 | 38 | 38 |
| 4 | 3 | 88 | 88 |
| 5 | 4 | 450 | 450 |

`U(N,K)` is the helper the index arithmetic needs. `V(N,K) = U(N,K) + U(N,K+1)`,
where (per the upstream comment) `U(N,K+1)` counts the combinations whose first
element is non-negative and `U(N,K)` those where it is negative — which is what
lets `icwrs`/`cwrsi` narrow the index down element by element instead of just
knowing the total. Both satisfy
`U(N,K) = U(N-1,K) + U(N,K-1) + U(N-1,K-1)`.

The part that matters for this issue: these are counting numbers, so they grow
very quickly and run past 32 bits at fairly modest `N` and `K`.

### A separate, smaller thing: panics for large K

`celt_pvq_u(n, k)` and `celt_pvq_v(n, k)` panic with an out-of-bounds index for
large `k`, for any `n >= 8`:

```
thread 'main' panicked at src/pvq.rs:156:9:
index out of bounds: the len is 130 but the index is 130
```

The boundaries I measured (N >= 8):

| function | highest K that does not panic |
|---|---|
| `celt_pvq_u(N, K)` | 128 |
| `celt_pvq_v(N, K)` | 127 |

`celt_pvq_v` stops one step earlier, presumably because it evaluates
`celt_pvq_u(n, k + 1)`.

This one looks fairly clear-cut: `compute_u` fills a fixed-size array

```rust
const MAX_PVQ_K: usize = 128;
const MAX_PVQ_U: usize = MAX_PVQ_K + 2;      // 130
...
let mut u = [0u32; MAX_PVQ_U];
for ki in 2..=(k + 1) as usize {             // k = 129 -> index 130 -> panic
```

### What I think might be going on

This part is speculation on my side, so it may be worth checking rather than
taking at face value.

`celt_pvq_u_lookup` falls back to `compute_u` when the ported table does not
cover `(n, k)`:

```rust
if r >= CELT_PVQ_U_ROW.len() { return compute_u(n, k); }
...
if idx >= CELT_PVQ_U_DATA.len() { return compute_u(n, k); }
```

Here is what I found on the reference side, for comparison.

In libopus the default path has no fallback at all: `CELT_PVQ_U` is a bare table
lookup, with no bounds check and nothing to compute a missing entry.

```c
/* celt/cwrs.c */
# define CELT_PVQ_U(_n,_k) (CELT_PVQ_U_ROW[IMIN(_n,_k)][IMAX(_n,_k)])
# define CELT_PVQ_V(_n,_k) (CELT_PVQ_U(_n,_k)+CELT_PVQ_U(_n,(_k)+1))
```

The only computing path I found there is `ncwrs_urow()`, which is compiled only
under `SMALL_FOOTPRINT`.

The table it indexes is `CELT_PVQ_U_ROW[15]`, so the row index `IMIN(n,k)` can
only be 0..14 — an out-of-table argument would read out of bounds rather than
fall back to anything. The accompanying comments say the table is built for the
`N` values that can actually occur — *"the set of N which can be achieved by
splitting a band from a standard Opus mode: 176, 144, 96, 88, 72, 64, 48, 44, 36,
32, 24, 22, 18, 16, 8, 4, 2"* — and covers *"K=128, or however many fit in 32
bits, whichever is smaller"*.

That bound on K lines up with `celt/rate.h`:

```c
#define MAX_PSEUDO 40
#define LOG_MAX_PSEUDO 6
#define CELT_MAX_PULSES 128
```

so the pulse count per band is not free: it comes out of the precomputed bit
cache (`bits2pulses` binary-searches `LOG_MAX_PSEUDO` steps over at most
`MAX_PSEUDO` entries), capped at `CELT_MAX_PULSES`, while band splitting keeps
`N` to the listed set. Between the two, the `(N, K)` pairs the encoder can
produce stay inside the table by construction — which is presumably why a
fallback was never needed.

This crate ports the same table (15 rows, 1272 entries) and the same
`CELT_MAX_PULSES`-equivalent (`MAX_PVQ_K = 128`), but adds the runtime fallback
on top, so out-of-table arguments produce a computed value instead of being
impossible.

The fallback here uses `wrapping_add`, so if it is reached with arguments outside
that range the result can wrap silently. Since `V(N,K) = U(N,K) + U(N,K+1)` and
both terms are non-negative, `V` can never legitimately be smaller than `U` — but
for some pairs it is, which means the addition wrapped:

```
N= 64 K= 16 : V=955449344  <= U=4033863679
N=128 K= 16 : V=2211184640 <= U=2911027199
N= 32 K= 64 : V=3279093760 <= U=4102848511
N= 64 K= 32 : V=2263220224 <= U=4102848511
```

If a codebook size like that were used, I would expect the encoder to emit an
index that the reference decoder reads differently, which would fit what I see:
the damage is not an occasional glitch but affects essentially every frame once
the threshold is crossed, and K per band grows with bitrate, which would fit the
bitrate dependence. A content-dependent threshold would also be consistent with
this, since how many pulses land in a given band depends on the signal — but that
is hand-waving on my part.

### What I did not verify

I have not confirmed that the encoder actually reaches these `(N, K)` values while
encoding. What I showed is only that

1. `V(N,K)` does wrap for some `(N, K)`, and
2. the conditions under which it would wrap line up with the conditions that
   produce the corruption.

Checking the link properly would mean instrumenting the band loop to log the
`(N, K)` pairs actually used and correlating those with the corrupted frames, and
I stopped short of that. It is quite possible the real cause is elsewhere and the
overflow I found is unreachable in practice — in which case the panics would still
seem worth fixing on their own.

I also did not try other sample rates, mono, or `Application` values other than
`Audio`.

### Things that looked fine when I checked them

- PVQ search returning the wrong number of pulses: I checked `sum(|y|) == K` for
  2280 combinations of `N` (2..200) and `K` (1..256) across several input shapes,
  and they all matched.
- My container/muxing: the same muxer produces files that ffmpeg reports as valid
  and decodes to exactly the expected length at the bitrates that work.

---

## 追記用の本文 (2026-08-17)

以下は**まだ投稿していない**。0.1.28 の検証結果として issue に足す想定の本文。
日本語版の要約は上の「0.1.28 で直った」を参照。

---

### Follow-up: verified on 0.1.28

**The encoding corruption is fixed.** Thank you. I re-ran the same measurements
against both versions; below is what I get. There is one thing left that I would
not have found without the fix, so I am adding it at the end rather than opening
a new issue — please split it out if you would rather track it separately.

Same caveats as the original report: this was written with AI assistance and I am
not a codec expert. I have kept what was measured separate from what I am
guessing at.

#### What I ran

- `opus-rs` 0.1.26 and 0.1.28, selected with `cargo update -p opus-rs --precise`
- Windows 10 Pro 22H2, x86_64 (AVX2 available), rustc/cargo 1.97.1 — **a
  different machine from the original report**, which is why the 0.1.26 numbers
  below are a reproduction on new hardware rather than the same run
- Encoder settings unchanged: `OpusEncoder::new(48000, 2, Application::Audio)`,
  20 ms frames (`frame_size = 960`), only `bitrate_bps` varied
- Decoded with ffmpeg 9.0 (the original report used 8.1.1)
- Same two 2-second stereo signals as before (**sine**: 440 Hz left / 660 Hz
  right at 0.5; **complex**: a three-tone chord per channel plus a little noise)

Metric as before: per 20 ms frame, compare magnitude spectra of input and decoded
output, `10*log10( sum(A²) / sum((A-B)²) )`, averaged over frames and channels.
Higher is better. I did not keep the original measurement script, so I rewrote it
(960-point DFT, Hann window). **The 0.1.26 column below reproduces the table in
the original report** — exactly for the complex signal, and within about 1 dB for
the sine — so I believe it is the same yardstick. The small differences are
presumably the ffmpeg version and the windowing detail.

#### 1. The bitrate-dependent corruption is gone

I extended the sweep up to 510 kbps (the stereo maximum) since the point of the
fix is being able to use those rates.

| bitrate (kbps) | sine 0.1.26 | sine 0.1.28 | complex 0.1.26 | complex 0.1.28 |
|---|---|---|---|---|
| 48 | 26.3 | 26.3 | 10.5 | 10.5 |
| 96 | 30.9 | 30.9 | 11.9 | 11.9 |
| 128 | 34.0 | 34.0 | 12.3 | 12.3 |
| 160 | 36.9 | 36.9 | **5.1** | 12.5 |
| 176 | **4.5** | 35.4 | **5.0** | 12.6 |
| 192 | **2.5** | 37.5 | **4.8** | 12.6 |
| 224 | **2.3** | 38.0 | **4.5** | 12.7 |
| 256 | **2.3** | 37.8 | **4.4** | 12.7 |
| 320 | **2.5** | 37.9 | **4.4** | 12.8 |
| 384 | **2.5** | 43.4 | **4.4** | 12.9 |
| 448 | **2.7** | 48.8 | **4.5** | 12.9 |
| 510 | 51.8 | 51.8 | 11.7 | 12.9 |

On 0.1.28 the match is non-decreasing across the whole range for both signals,
which is what I would expect from a working encoder. On 0.1.26 it collapses above
the content-dependent threshold and stays collapsed.

#### 2. The PVQ-level checks

Over 600 combinations of `N` (2..200) and `K` (1..127) with four input shapes
each:

| check | 0.1.26 | 0.1.28 |
|---|---|---|
| `celt_pvq_u`/`celt_pvq_v` panic (`N >= 8`) | panics for `K >= 129` | no panic for any `K` I tried (up to 299) |
| out-of-table `V(N,K)` | wraps (`wrapping_add`) | saturates at `u32::MAX` |
| index outside the codebook (`i >= V`) | 145 | 0 |
| `icwrs` → `cwrsi` round-trip mismatches | 124 | 0 |
| `pvq_search` giving `sum(\|y\|) == K` (2280 cases) | all pass | all pass |

A caveat on the two zeros: on 0.1.28, 292 of the 600 pairs now return
`V == u32::MAX`, and my harness skips the index checks for those, on the grounds
that a `u32` index cannot address a codebook that large anyway. Another 8 do not
return (see below). The zeros cover the remaining 300.

The value itself also looks better where I can check it by hand. `V(200,2)`
should be `2N + 2N(N-1) = 80000`; 0.1.26 returns `26158`, 0.1.28 returns `80000`.

#### 3. Left over: `cwrsi` does not return for `N = 200`

The 8 cases above are `N=200, K=2` and `N=200, K=4`. On 0.1.26 the same calls
panicked with the out-of-bounds index; on 0.1.28 they do not come back.

```rust
use opus_rs::pvq::cwrsi;

let mut y = vec![0i32; 200];
cwrsi(200, 2, 512, &mut y); // does not return
```

It is not just slow — I let one call run for 120 seconds. Scanning the index
directly, with `V(200,2) = 80000`:

| `i` | `N = 176` | `N = 200` |
|---|---|---|
| 0, 1, 63, 64, 128, 256 | returns | returns |
| 512, 1024, 2048, 4096, 8192, 16384 | returns | **does not return** |

Note this is not an overflow case: `V(200,2)` is 80000, nowhere near 32 bits.

**I do not think this is reachable from the encoder.** `N = 200` is not in the
set of `N` that band splitting produces (176, 144, 96, 88, ...); I only had it in
my harness to see what happens outside the table. `N = 176` is fine at every
index I tried, and encoding is clean at every bitrate now. So this does not
affect me. I am mentioning it only because a hang is a worse failure mode than a
panic for a library — a caller can catch the panic.

#### A separate problem, filed on its own

While measuring the above I also found that `OpusEncoder::new` needs about
0.85 MiB of stack since 0.1.27 (0.1.26 fit in 64 KiB), which is enough to crash
an application that encodes on the Windows main thread. That is unrelated to the
codec behaviour discussed here, so I have raised it separately rather than
lengthening this thread.

#### What I did not verify

- Other sample rates, mono, or `Application` values other than `Audio`.
- That `N = 200` really is unreachable from the encoder — I only checked that
  `N <= 176` behaves, and that encoding is clean end to end.

The bitrate fix does hold up on real material, though: I exported the music that
originally showed the problem at 192 kbps and it is clean by ear, with no crash.
