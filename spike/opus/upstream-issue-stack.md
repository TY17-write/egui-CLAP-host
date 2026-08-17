# Upstream issue 2 (restsend/opus-rs) — スタック使用量

`opus-rs` 0.1.27 以降のスタック使用量について出す想定の本文。**まだ投稿して
いない。**

符号化の破綻 (issue #11 / `upstream-issue.md`) とは別件なので分けてある。

**強い提言としては出さない。** これは **Windows のメインスレッドが既定で
1MiB しか無い**という環境側の事情で顕在化したもので、Linux では見えない。
こちらは専用スレッドを立てるだけで解決しており、困ってもいない。
**「遅くていいのでヒープに置く経路も欲しい」程度のお願い**として出す。

再現コードは `src/bin/stack_probe.rs`。本体側の対処は `host/src/opus.rs` の
`ENCODE_STACK`、経緯は `docs/export_rate_plan.md`。

## 手元で分かっていること

- `size_of::<OpusEncoder>()` が **1,288 → 254,608 バイト**になった (0.1.27)
- `OpusEncoder::new` が **約 0.85MiB のスタック**を要求する (0.1.26 は 64KiB 未満)
- 0.1.27 で状態がヒープからインラインの固定長へ移った設計変更によるもの。
  `no_std` 向けと見えるので、**その意図自体には触れない**

---

### A note on stack usage on Windows, and a small request

Not really a bug report — more a note about something I ran into on Windows when
upgrading, in case it is useful. I have worked around it easily enough, so please
treat this as low priority.

Written with AI assistance, as with #11.

#### What I ran into

`OpusEncoder::new` overflows a small thread stack from 0.1.27 onwards, where
0.1.26 was comfortable. Encoding is not involved — it happens during
construction.

```rust
use opus_rs::{Application, OpusEncoder};

std::thread::Builder::new()
    .stack_size(832 * 1024)
    .spawn(|| {
        // 0.1.26: fine. 0.1.27 and 0.1.28: stack overflow here.
        let _ = OpusEncoder::new(48_000, 2, Application::Audio).unwrap();
    })
    .unwrap()
    .join()
    .unwrap();
```

Bisecting the thread stack size, with the struct size alongside:

| version | `size_of::<OpusEncoder>()` | stack needed (release) | stack needed (debug) |
|---|---|---|---|
| 0.1.26 | 1,288 B | fits in 64 KiB (smallest I tried) | — |
| 0.1.27 | 254,608 B | 832 KiB overflows, 864 KiB is enough | — |
| 0.1.28 | 254,608 B | 832 KiB overflows, 864 KiB is enough | 1 MiB overflows, 2 MiB is enough |

So this arrived in 0.1.27, and 0.1.28 is unchanged in this respect.

Environment: Windows 10 Pro 22H2, x86_64, rustc/cargo 1.97.1, default profiles.

#### Why it showed up for me and probably not for you

**On Windows the main thread gets 1 MiB by default** — the MSVC linker default,
which Rust does not raise. My application encodes on its main thread (a GUI host
writing an export from the UI thread), so it lands right around the requirement.

On Linux the main thread typically gets 8 MiB, so I would not expect this to be
visible there at all. It really is a platform-specific corner rather than
something wrong with the crate.

#### I did look at why, and it seems deliberate

The encoder state moved from the heap into the struct:

```rust
// 0.1.26
silk_enc: Box<SilkEncoderState>,
hp_mem: Vec<i32>,
buf_filtered: Vec<i16>,
// ...

// 0.1.27 / 0.1.28
silk_enc: SilkEncoderState,
hp_mem: FixedVec<i32, OPUS_HP_MEM>,
buf_filtered: FixedVec<i16, OPUS_MAX_FRAME>,
// ...
```

with `FixedVec<T, N>` holding `[MaybeUninit<T>; N]` inline.

An allocation-free encoder is exactly what you want for `no_std` and embedded
use, and the feature list points that way, so this reads as intentional and I am
not suggesting changing it. Since `new` returns `Result<Self, _>` by value, the
254 KB struct gets built and moved into the caller's slot, which is presumably
where the ~850 KiB comes from — though that last step is my inference, not
something I measured.

#### The request

If it is ever convenient: **an optional way to get the encoder on the heap** —
something like a `Box`-returning constructor alongside the current one, or a
feature flag. **I do not mind if it is slower**; for offline file export the
difference would not matter to me, and it would let callers on small-stack
platforms use the crate without arranging their own thread.

Entirely understand if this is not worth the API surface. My workaround —
encoding on a thread with an explicit 8 MiB stack — costs me nothing, so I am
fine either way. Mostly I wanted the measurements on record in case someone else
hits the same thing on Windows.

#### What I did not check

- Which field dominates the 254 KB (I only read the struct definition).
- Whether `OpusDecoder` is shaped the same way.
- Anything outside Windows/x86_64.
- Whether the requirement varies with channels, sampling rate, or `Application`;
  I measured 48 kHz stereo `Audio` only.
