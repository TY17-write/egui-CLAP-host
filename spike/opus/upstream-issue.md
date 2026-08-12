# Upstream issue (restsend/opus-rs)

`opus-rs` 0.1.26 の不具合として報告した内容。**以下は実際に投稿した本文**
(草稿から、単なる感想にあたる部分を削ってある)。

再現コードは同じディレクトリの `main.rs` / `pvq_check.rs` / `cwrs_bound.rs`。
測定の経緯と、こちらの対処 (48 / 96kbps に絞った理由) は
`docs/export_rate_plan.md` の「高ビットレートは出さない」を参照。

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

As far as I could tell, the default path in libopus does not have an equivalent
fallback — `CELT_PVQ_U` there looks like a plain table lookup:

```c
/* celt/cwrs.c */
# define CELT_PVQ_U(_n,_k) (CELT_PVQ_U_ROW[IMIN(_n,_k)][IMAX(_n,_k)])
# define CELT_PVQ_V(_n,_k) (CELT_PVQ_U(_n,_k)+CELT_PVQ_U(_n,(_k)+1))
```

and the table is described as covering *"K=128, or however many fit in 32 bits,
whichever is smaller"* (comment in `celt/cwrs.c`, above `CELT_PVQ_U_ROW[15]`).
I read that as upstream keeping the arguments inside the table, and inside 32
bits, by construction rather than handling out-of-table cases at runtime. The
only computing path I found upstream is `ncwrs_urow()` under `SMALL_FOOTPRINT`.

The fallback here uses `wrapping_add`, so if it is reached with arguments outside
that range the result can wrap silently. `V(N,K)` should come out larger than
`U(N,K)`, but for some pairs it comes out smaller:

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
