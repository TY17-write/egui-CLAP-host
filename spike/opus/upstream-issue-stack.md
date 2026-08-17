# Upstream issue 2 (restsend/opus-rs) — スタック使用量

`opus-rs` **0.1.27 で入った退行**として出す想定の本文。**まだ投稿していない。**

符号化の破綻 (issue #11 / `upstream-issue.md`) とは別件なので分けてある。
**あちらの検証中に見つかったもので、こちらのほうが実害があった** —
本体はこれで書き出しが落ちるようになった。

再現コードは `src/bin/stack_probe.rs`。本体側の対処 (専用スレッドを立てる) は
`host/src/opus.rs` の `ENCODE_STACK`、経緯は `docs/export_rate_plan.md`。

## 要点

- `size_of::<OpusEncoder>()` が **1,288 → 254,608 バイト**になった (0.1.27)
- `OpusEncoder::new` が **約 0.85MiB のスタック**を要求する (0.1.26 は 64KiB 未満)
- **Windows のメインスレッドは既定で 1MiB** なので、そこで符号化する
  アプリケーションは**線の際に立たされる**
- 原因は状態をヒープからインラインの固定長へ移した設計変更。
  `new` が `Self` を値で返すため、呼ぶだけで複製が数回積まれる

---

### `OpusEncoder::new` needs ~0.85 MiB of stack since 0.1.27

Splitting this out of #11 — it turned up while verifying the encoder fix there,
but it is a separate problem and, for me, the more disruptive one: upgrading made
my exports crash outright.

Same caveats as before: written with AI assistance, and I am not a codec expert.
I have kept the measurements separate from my reading of the cause.

#### Symptom

`OpusEncoder::new` overflows a small thread stack. Encoding is not involved — it
dies during construction.

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

#### Measurements

Bisecting the thread stack size, and printing the struct size alongside:

| version | `size_of::<OpusEncoder>()` | stack needed (release) | stack needed (debug) |
|---|---|---|---|
| 0.1.26 | 1,288 B | fits in 64 KiB (smallest I tried) | — |
| 0.1.27 | 254,608 B | 832 KiB overflows, 864 KiB is enough | — |
| 0.1.28 | 254,608 B | 832 KiB overflows, 864 KiB is enough | 1 MiB overflows, 2 MiB is enough |

So the change arrived in **0.1.27** and 0.1.28 is unchanged in this respect.

Environment: Windows 10 Pro 22H2, x86_64, rustc/cargo 1.97.1, default profiles.

#### Why this hurts

**On Windows the main thread gets 1 MiB by default** — the MSVC linker default,
which Rust does not raise. An application that encodes on its main thread (mine
did: a GUI host writing an export from the UI thread) therefore sits right on the
edge of the requirement.

That edge is the unpleasant part. The same logic crashed when called from my
application and survived from a small standalone binary doing the same thing,
presumably because inlining moved the frame size slightly either side of the
line. It looks like it works until it does not.

I expect this is much less visible on Linux, where the main thread typically gets
8 MiB — which may be why it has not come up.

#### What changed, as far as I can see

The encoder state moved from the heap into the struct:

```rust
// 0.1.26
silk_enc: Box<SilkEncoderState>,
hp_mem: Vec<i32>,
buf_filtered: Vec<i16>,
buf_silk_input: Vec<i16>,
// ...

// 0.1.27 / 0.1.28
silk_enc: SilkEncoderState,
hp_mem: FixedVec<i32, OPUS_HP_MEM>,
buf_filtered: FixedVec<i16, OPUS_MAX_FRAME>,
buf_silk_input: FixedVec<i16, OPUS_MAX_FRAME>,
// ...
```

with `FixedVec<T, N>` holding `[MaybeUninit<T>; N]` inline and
`OPUS_MAX_FRAME = 5760`.

I take this to be deliberate — an allocation-free encoder is exactly what you
want for `no_std` and embedded use, and the crate has `std` / `libm` features
pointing that way. **So I am reporting the consequence, not asking for a
revert.**

The consequence is that `new` returns `Result<Self, &'static str>` **by value**,
so a 254 KB struct is built in a temporary and moved into the caller's slot. The
~850 KiB requirement is roughly three times the struct size, which is what I
would expect from a couple of such moves — though that last step is inference on
my part, not something I measured.

If that reading is right, then the cost is not the inline storage itself but
returning it by value. Something like a `Box`-returning constructor, or an
`init(&mut MaybeUninit<Self>)`-style entry point, would let callers place the
state where they want it without giving up the allocation-free design. A note in
the README about the stack requirement would already have saved me the crash.

#### What I did not verify

- Which field dominates the 254 KB (I only read the struct definition).
- Whether `OpusDecoder` has the same shape — its `new` has the same signature
  style, but I did not measure it.
- Anything outside Windows/x86_64.
- Whether the requirement changes with `channels`, sampling rate, or
  `Application`; I measured 48 kHz stereo `Audio` only.
