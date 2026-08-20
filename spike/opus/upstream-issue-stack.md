# Upstream issue 2 (restsend/opus-rs) — スタック使用量

`opus-rs` 0.1.27 以降のスタック使用量について報告した本文。

- **投稿先**: <https://github.com/restsend/opus-rs/issues/12> (2026-08-17)
- **0.1.29 で修正され、同日クローズ済み** (末尾のコメントを参照)

符号化の破綻 (issue #11 / `upstream-issue.md`) とは別件なので分けてある。

**強い提言としては出さない。** これは **Windows のメインスレッドが既定で
1MiB しか無い**という環境側の事情で顕在化したもので、Linux では見えない。
こちらは専用スレッドを立てるだけで解決しており、困ってもいない。
**「遅くていいのでヒープに置く経路も欲しい」程度のお願い**として出す。

再現コードは `src/bin/stack_probe.rs`。本体側の対処は `host/src/opus.rs` の
`ENCODE_STACK`、経緯は `docs/archive/export_rate_plan.md`。

## 0.1.29 で入った (2026-08-17)

**投稿当日に対応された。** 既定で有効な `heap` フィーチャが足され、
`OpusEncoder` / `OpusDecoder` の大きなフィールドが `Box` 越しになった。
`heap` を切ると従来のインライン (`no_std` / `static` 配置向け) のまま。

| 版 | `size_of::<OpusEncoder>()` | release | debug |
|---|---|---|---|
| 0.1.26 | 1,288 バイト | 64KiB でも足りる | — |
| 0.1.27 / 0.1.28 | 254,608 バイト | 832KiB は落ち、864KiB で足りる | 1MiB は落ち、2MiB で足りる |
| **0.1.29** | **2,416 バイト** | **384KiB は落ち、386KiB で足りる** | **512KiB は落ち、544KiB で足りる** |

**debug でも Windows の 1MiB に収まるようになった**ので、こちらの当初の困り事
(メインスレッドで書き出すと落ちる) は解消している。

**出力は変わらない。** 試験信号2種 × 12ビットレートの `.opus` 24本を
0.1.28 と 0.1.29 で作り直して SHA-256 を突き合わせ、**全て一致**。
`heap` は配置を変えるだけで符号化には影響しない、と実測で言える。

### 「包むのは手遅れ」は残っている

`CeltEncoder` と `SilkEncoderState` の**中身は今もインラインの固定長**で、
`new` は `Box::new(CeltEncoder::new(mode, channels))` と書いている。
**`Box::new` に渡す時点で一度スタックに載る**ので、2.4KB の構造体に対して
release で 386KiB を要求する。**構造体の大きさから予想される量とは2桁違う。**

上流の `tests/stack_usage_test.rs` は、コメントで「256KiB に余裕で収まる」と
書きつつ実際のテストは 768KiB で回している。**手元の実測では 256KiB は落ちる**
(release / debug とも)。コメントのほうが実態とずれているだけで、テスト自体は通る。

**報告はしていない。** こちらは困っておらず、直してもらった側から粗探しを
する話でもない。**聞かれたら出せるように測っておく**という位置づけ。

### 上流に出したクローズ用コメント (2026-08-17・投稿済み)

**現状の報告だけにして閉じた。** 残っている `Box::new(CeltEncoder::new(..))` の
件には触れていない (困っていないので、指摘は野暮)。

以下は**実際に投稿した本文**。

---

Verified on my side and it resolves what I ran into. Closing.

| version | `size_of::<OpusEncoder>()` | stack needed (release) | stack needed (debug) |
|---|---|---|---|
| 0.1.26 | 1,288 B | fits in 64 KiB | — |
| 0.1.27 / 0.1.28 | 254,608 B | 832 KiB overflows, 864 KiB is enough | 1 MiB overflows, 2 MiB is enough |
| **0.1.29** | **2,416 B** | **384 KiB overflows, 386 KiB is enough** | **512 KiB overflows, 544 KiB is enough** |

**Output is unchanged.** I re-encoded my two test signals at 12 bitrates each
(24 `.opus` files) with 0.1.28 and 0.1.29 and compared SHA-256 — all 24 match
byte for byte. So the `heap` feature is purely a placement change as far as the
bitstream is concerned.

I also like that `heap` is opt-out rather than forced, so the inline layout is
still there for `no_std` / static placement.

Same environment as before: Windows 10 Pro 22H2, x86_64, rustc/cargo 1.97.1,
default profiles. Measured with the same `stack_probe` binary from the original
report. Thanks for turning this around so quickly.

## 投稿後に手元で確かめたこと (報告はしない)

**`Box` で包んでも逃げられない。** `Box::new(OpusEncoder::new(..))` と書いても
必要なスタックは1バイトも変わらない (release / debug とも実測。
`src/bin/stack_probe.rs` に第2引数 `box` で入る)。`new` が `Self` を値で返す
以上、**`Box::new` に渡す時点で既にスタック上に出来ている**。

**#12 に反応があったときのために残しておく。** 本文では「`Box` を返す構築子」
と書いたが、クレート側で `Box::new(Self { .. })` と書くだけでは同じことで、
効果が無い。避けるには `Box<MaybeUninit<Self>>` を確保して**その場で埋める**
必要があり、**こちらが示唆したよりずっと侵襲的**になる。

つまり「入れば専用スレッドを畳める」という見込みは、**入り方次第**。
`Box` を返すだけの対応が入っても、こちらの状況は変わらない。

**この見立ては当たった。** 0.1.29 はフィールド単位で `Box` に移す形で入り、
必要量は 0.85MiB → 0.38MiB (release) まで落ちたが、`Box::new(CeltEncoder::new(..))`
の中身が一度スタックに載る点はそのまま。**畳めるほど小さくはなっていない**ので、
本体の専用スレッドは残す。

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
