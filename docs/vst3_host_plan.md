# VST3 対応の計画

CLAP に加えて VST3 の音源も読み込み、**トラックごとにどちらでも載せられる**ようにする。

## ライセンス（解決済み）

**2025年10月20日に VST3 SDK 3.8.0 が MIT ライセンスで公開された。** それ以前の
「GPLv3 か Steinberg 独自ライセンスの二択」は解消され、独自ライセンス契約も不要。
このプロジェクトは **MIT のまま** VST3 対応を書ける。

依存の連鎖もきれいに繋がる。

| 層 | ライセンス | 備考 |
|---|---|---|
| VST3 SDK 3.8.0 | MIT | 2025-10-20 に再ライセンス |
| `vst3` クレート | MIT OR Apache-2.0 | SDK ヘッダから libclang で生成。MIT 化されたから公開できるようになった |
| `vst3-host` クレート | MIT | 上記の生成済みバインディングを同梱。SDK の別途入手が不要 |

残るのは**商標**のみ。「VST」は Steinberg の商標なので、名称・ロゴの表示は
ガイドラインの確認が要る（コードのライセンスとは別の話）。

参考:
- <https://forums.steinberg.net/t/vst-3-8-0-sdk-released/1011988>
- <https://steinbergmedia.github.io/vst3_dev_portal/pages/VST+3+Licensing/Index.html>
- <https://micahjohnston.com/posts/vst3/>

## 現状のコード構造

### 良い知らせ: 編集・データ側は既に形式中立

以下のファイルは CLAP を1箇所も参照していない。**一切触らずに済む。**

```
sequencer.rs / editor_ui.rs / midi.rs / ccs.rs / project.rs / swing.rs / wav.rs / theme.rs
```

`SeqEvent`（`{ sample_time, key, velocity, is_on }`）が形式中立なので、
**抽象化の境界を引く場所は既に決まっている**。

### 悪い知らせ: ホスティング側は全層が CLAP に密着

| ファイル | 行数 | CLAP への依存 |
|---|---|---|
| `transport.rs` | 404 | CLAP イベントを直接組み立てている |
| `config.rs` | 367 | audio-ports 拡張でポート構成を取得 |
| `audio/mod.rs` | 352 | `TrackProcessor` が CLAP の処理器を直接保持 |
| `offline.rs` | 214 | 処理器を直接叩く |
| `buffers.rs` | 202 | CLAP のバッファ構造を組み立て |
| `plugin_window.rs` | 192 | Win32 の窓を作って `clap.gui` に渡す |
| `gui.rs` | 153 | gui 拡張の開閉・リサイズ |
| `host.rs` | 133 | clack のホスト trait 実装 |
| `timers.rs` | 95 | timer-support 拡張 |
| `params.rs` | 48 | params 拡張（UI は現在無効化中） |
| `discovery.rs` | 33 | `.clap` の走査 |

プラグインに触れる層で **約 2,200 行**（全体 9,475 行）。

### 現在のデータの流れと、切るべき場所

```
MidiEditor::performed_notes()
  → collect_events() → Vec<SeqEvent>        ← 形式中立
  → TransportMsg::SetSequence               ← 形式中立
  → Transport::plan_block() → BlockPlan     ← 形式中立
  ────────────────────────────────────────  ← ここで切る
  → Transport::emit_track(..., &mut EventBuffer, note_port)   ← CLAP
  → TrackProcessor { audio_processor, buffers, events, .. }   ← CLAP
  → audio_processor.process(&ins, &mut outs, &events, ..)     ← CLAP
```

## 設計方針

### 1. trait ではなく enum で分岐する

バックエンドは2つしかないので、**enum によるディスパッチ**を採る。

```rust
enum TrackProcessor {          // オーディオスレッド側
    Clap(ClapProcessor),
    Vst3(Vst3Processor),
}

enum TrackPlugin {             // メインスレッド側 (現在の TrackAudio 相当)
    Clap(ClapPlugin),
    Vst3(Vst3Plugin),
}
```

trait オブジェクトにしない理由:

- **停止・解放の経路が型安全に書けない。** いまは `into_stopped()` が
  `StoppedPluginAudioProcessor<MiniHost>` を返し、それを
  `instance.deactivate(stopped)` に渡している。`dyn` にすると `Any` への
  ダウンキャストが必要になり、リングバッファ越しの受け渡しが型で守られなくなる
- **オーディオスレッドで動的ディスパッチを避けられる**
- バックエンドが増える見込みが薄い（2つで打ち止め）

### 2. 形式中立なブロックイベント

`Transport::emit_track` の出力先を CLAP の `EventBuffer` から中立の入れ物に変える。

```rust
/// 1ブロック分の発行イベント (バックエンド非依存)
pub enum BlockEvent {
    NoteOn  { offset: u32, key: u8, velocity: f64 },
    NoteOff { offset: u32, key: u8 },
    Choke   { offset: u32 },
}

pub struct BlockEvents {
    events: Vec<BlockEvent>,   // 容量は事前確保し、毎ブロック clear する
}
```

各バックエンドがこれを自分の形式へ変換する。ブロックあたり数十件なので
変換のコストは無視できる。容量を事前確保して `clear()` する方針は
現在の `EventBuffer::with_capacity(128)` と同じ。

### 3. バッファはチャンネル変換だけ共有する

`buffers.rs` の `mux` / `mix_mono` / `mono_to_multi` は素の `&[f32]` を扱うので
**そのまま使い回せる**。CLAP の `AudioPorts` を組み立てる部分だけをバックエンド側へ移す。

### 4. Win32 の窓は使い回せる

`plugin_window.rs`（192行）は Win32 のネイティブウィンドウを作るだけで、
CLAP に触れているのは1箇所。VST3 では `IPlugView::attached(hwnd, kPlatformTypeHWND)`
に渡す先が変わるだけ。**GUI 埋め込みの土台は再利用できる。**

## 特に厄介な3点

### (a) VST3 には choke に相当するものが無い

停止・シーク・シーケンス差し替えで音を止める仕組み。CLAP は
`NoteChokeEvent::new(time, Pckn::match_all())` を1つ送れば済み、
いまの `push_choke` は数行で終わっている。

VST3 には `IAudioProcessor` レベルでの「全ノートオフ」が無い。

| 案 | 評価 |
|---|---|
| 鳴っているノートを覚えて個別に note-off | **採用。** 確実 |
| MIDI CC 123 (All Notes Off) | VST3 は CC を `IMidiMapping` 経由でパラメータに割り当てるため、対応が不揃い |
| `setProcessing(false/true)` | リアルタイム安全でない。オーディオスレッドから呼べない |

**実装**: `TrackSequence` に `u128` のビットマスク（キー0〜127）を持ち、
`emit_track` で note-on / note-off のたびに更新する。choke 時はビットが立って
いるキーすべてに note-off を出す。128ビットなのでオーディオスレッドでも確保なし。

なお、これは CLAP 側にも入れておくと堅くなる（現状は NoteChoke 頼み）が、
必須ではない。

### (b) スレッド規約が型で守られなくなる

clack は main-thread / audio-thread の区別を**型システムで担保**している。
**この保証があるからこそ**、以下の設計が安全に書けている。

- 処理器をリングバッファでオーディオスレッドへ渡す
- 外した処理器はトラック番号付きで返し、**解放は必ずメインスレッド**
- WAV 書き出しで処理器を一時的に借り出し、必ず戻す

VST3 側のバインディングが同等の保証を持つかは未確認。**持たない場合は
自前の newtype で包んで同じ制約を作る**こと。ここを崩すと、上記3つの仕組みが
静かに壊れる。

### (c) パラメータモデルが別物

| | CLAP | VST3 |
|---|---|---|
| 値域 | 実単位 | 0.0〜1.0 に正規化 |
| 置き場所 | プラグインインスタンス直結 | `IEditController` が別オブジェクト |
| 接続 | 不要 | `IConnectionPoint` で component と繋ぐ |
| 変更通知 | イベント | `IComponentHandler` をホストが実装 |

`params.rs` の UI は現在無効化中なので**後回しにできる**。ただし GUI を出すなら
`IComponentHandler` の実装は必須。

## その他の差分

- **探索**: VST3 は Windows では**バンドルディレクトリ**
  （`Foo.vst3\Contents\x86_64-win\Foo.vst3`）。素の DLL 形式もあるので両対応が要る。
  標準の置き場は `C:\Program Files\Common Files\VST3`
- **時刻**: CLAP の `steady_time` に対し VST3 は `ProcessContext`。意味が違うので対応付けが要る
- **ノートの識別**: CLAP は `(port, channel, key, note_id)` の Pckn、VST3 は
  `noteId + channel + pitch`。対応付け可能

## フェーズ分け

| フェーズ | 内容 | 状態 |
|---|---|---|
| **0** | **スパイク（可否判断）** | |
| **1** | 抽象化層（CLAP のみ。挙動を変えない） | |
| **2** | VST3 バックエンド（音のみ。choke 含む） | |
| **3** | VST3 の GUI | |
| **4** | 仕上げ（CLAP と VST3 の混在確認、ドキュメント） | |

### フェーズ0: スパイク（最重要）

**本体には一切触らず**、独立した実験用バイナリを1つ作る。VST3 を1つ読み込み、
数音をオフラインで WAV に書き出すだけ。

確かめること:

- **`vst3-host` のリアルタイム経路が使い物になるか。** README に
  「`play_realtime` はあるが、どちらの経路もまだ RT 監査済みではない」とある。
  このプロジェクトはオーディオスレッドでの確保・ロックを排除した設計なので、
  ここが通らなければ話が始まらない
- **Windows で動くか。** 実行時に検証されているのは macOS のみ
- API の形が enum 分岐の設計に合うか

**ここで打ち切る判断もありうる。** 逆に、通れば残りは見通しが立つ。

**この順序には理由がある。** CLAP しか裏に無い状態で先に抽象化を切ると、
CLAP の都合に寄った形になり、いざ VST3 を足すときに引き直すことになる。
先に VST3 側の要求を知ってから境界を決める。

### フェーズ1: 抽象化層

CLAP を enum の裏へ移す。**挙動は一切変えない。**
既存のテスト全件と smoke 4本がそのまま通ることが完了条件。

- `BlockEvent` / `BlockEvents` の新設、`Transport::emit_track` の出力先変更
- `buffers.rs` をチャンネル変換（共有）とポート組み立て（CLAP 固有）に分割
- `TrackProcessor` / `TrackPlugin` を enum 化
- `offline.rs` を enum 経由に

規模: **600〜900 行の改修**。ここが支配的なコスト。

### フェーズ2: VST3 バックエンド（音のみ）

- 探索（バンドルディレクトリ + 素の DLL）
- 読み込み、バス構成、`setupProcessing` / `setActive` / `setProcessing`
- ノートの発行、オーディオバッファの受け渡し
- **choke のためのアクティブノート追跡**

規模: `vst3-host` が使えるなら **400〜800 行**。自前で COM を叩くなら 1,500〜3,000 行。

### フェーズ3: VST3 の GUI

`plugin_window.rs` の Win32 窓を再利用し、`IPlugView::attached` に渡す。
プラグイン発のリサイズは `IPlugFrame::resizeView`。

### フェーズ4: 仕上げ

- CLAP と VST3 を同じプロジェクトの別トラックに載せて同時再生
- WAV 書き出しでの借り出し・返却が両バックエンドで正しく動くこと
- README・操作ガイド

## 検証環境の問題

`test-plugin` は clack-plugin 製で **CLAP 専用**。smoke 4本
（`smoke` / `seq_smoke` / `wav_smoke` / `choke_smoke`）は VST3 では回せない。

| 案 | 評価 |
|---|---|
| **clap-wrapper で test-plugin を VST3 化** | 既存のサイン波シンセをそのまま流用でき、**同じ音源で両バックエンドを比較**できる。ただし CMake が要る（成果物には含まれず、テスト用の一度きりのビルド） |
| VST3 のテストプラグインを自作 | 確実だが工数が大きい |
| Surge XT の VST3 で手動確認 | 手軽だが自動テストにならない |

**clap-wrapper 案を推す。** 同一の音源で CLAP 版と VST3 版の出力を突き合わせられる
ので、抽象化が壊れていないことを直接確かめられる。CMake は Opus のときに避けた
経緯があるが、あれは**配布物のビルドに必要**だったのに対し、今回は**テスト治具を
一度作るだけ**なので事情が違う。

## リスク

- **`vst3-host` が早期段階**（スター 26、macOS で検証、Windows GUI は実行時未検証、
  RT 経路が未監査）。フェーズ0 はこのリスクを最初に潰すためにある
- **早すぎる抽象化**。フェーズ0 を先に置くことで緩和する
- **保守範囲が広がる**。README は「テスト用ホスト」と位置づけているので、
  2つのプラグイン形式を抱える価値があるかは別途の判断

## 着手前に決めること

1. **`.ron` に音源のパスと種別を保存するか。** 現状は音源のパスを保存していない
   （`clap.state` 未対応でパラメータが復元できないため）。VST3 も同じ事情なので
   方針は変えなくてよいが、形式が2つになると「どちらの形式だったか」の情報が要る
   場面が出てくる
2. **テスト用 VST3 の用意方法**（clap-wrapper / 自作 / 手動確認）
3. **GUI は自前の Win32 埋め込みを使うか、クレートの機能に任せるか**
   （こちらは既に Win32 埋め込みが動いているので、自前のほうが確実かもしれない）
