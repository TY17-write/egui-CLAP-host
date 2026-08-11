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

**実装（採用した形）**: `Vst3Processor` に `u128` のビットマスク（キー0〜127）を持ち、
自分が送った note-on / note-off のたびに更新する。choke 時はビットが立っている
キーすべてに note-off を出す。128ビットなのでオーディオスレッドでも確保なし。

当初は `TrackSequence`（トランスポート側）に置くつもりだったが、そこに置くと
`BlockEvent::Choke` が個別の note-off に展開され、**CLAP が `NoteChoke` を使えなくなる**。
バックエンドに置けば CLAP を巻き込まずに済む。

### (b) スレッド規約が型で守られなくなる

clack は main-thread / audio-thread の区別を**型システムで担保**している。
**この保証があるからこそ**、以下の設計が安全に書けている。

- 処理器をリングバッファでオーディオスレッドへ渡す
- 外した処理器はトラック番号付きで返し、**解放は必ずメインスレッド**
- WAV 書き出しで処理器を一時的に借り出し、必ず戻す

VST3 側のバインディングが同等の保証を持つかは未確認。**持たない場合は
自前の newtype で包んで同じ制約を作る**こと。ここを崩すと、上記3つの仕組みが
静かに壊れる。

**結果（フェーズ2〜3）**: `vst3-host` は同等の保証を持たない（`Plugin` は `Send` なだけ）。
`SharedPlugin` が newtype としてその役を担っている。待つ `lock()` と待たない
`try_lock()` を分け、**待ってよい場面（エディタの開閉・状態の保存・復元・停止）と
待ってはいけない場面（オーディオスレッドとメインスレッドの毎フレーム処理）を
呼び分ける**。停止 (`setProcessing`) を `RetiredProcessor::Vst3` の受け手に
押し付けているのも同じ意図で、リアルタイム安全でない呼び出しがオーディオ経路に
紛れ込まないようにしている。

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

## 音源の保存・復元（VST3 とは独立して先に入れられる）

`.ron` に**音源のパス・種別・状態**を保存し、開いたときに復元する。CLAP でも同じ
扱いにする。読み込みに失敗したトラックは**音源なし（空）**とし、ノートは残す。

### パスだけでは足りない

- **CLAP**: 1つの `.clap` に複数のプラグインが入りうるので、**パス + プラグイン ID**
  が要る（`discovery.rs` が既に `FoundPlugin { id, name }` を列挙している）
- **VST3**: バンドルに複数のクラスが入りうるので、**パス + クラス UID (CID)**

### パラメータではなく「状態」を保存する

「パラメータを保存」の素直な実装は params 拡張で id→値 を列挙することだが、
**これでは足りない**。多くの音源はパラメータに現れない状態を持つ
（読み込んだプリセット、ウェーブテーブル、モジュレーション行列、サンプルの選択など）。
Surge XT のパッチはパラメータ一覧よりずっと大きい。

そこで**状態そのもの**を保存する。どちらの形式にも同等の仕組みがある。

| | 保存 | 復元 |
|---|---|---|
| CLAP | `clap.state` の `save(stream)` | `load(stream)` |
| VST3 | `IComponent::getState(IBStream)` | `setState` + `IEditController::setComponentState` |

いずれも**中身は不透明なバイト列**なので、ホストは解釈せずそのまま持ち回ればよい。
`clap.state` は**未実装**なので新規に足す（`clack_extensions::state`）。
どちらもメインスレッドから呼ぶ。

### `project.rs` の純粋さを保つ

現在の `project::to_string(&MidiEditor)` は純粋な関数だが、音源の状態は
`App.tracks` 側にあり `MidiEditor` には無い。`project.rs` にプラグインの知識を
持ち込まないよう、**main.rs 側で組み立てた値を渡す**形にする。

```rust
/// 保存する音源1つぶん (main.rs が組み立て、project.rs は中身を解釈しない)
pub struct PluginSnapshot {
    pub kind: PluginKind,   // Clap | Vst3
    pub path: PathBuf,
    pub id: String,         // CLAP のプラグイン ID / VST3 のクラス UID
    pub state: Vec<u8>,     // 不透明なバイト列
}

pub fn to_string(editor: &MidiEditor, plugins: &[Option<PluginSnapshot>]) -> Result<String, String>
```

### 失敗したときの扱い

| 状況 | 扱い |
|---|---|
| ファイルが無い / 読めない | そのトラックは**音源なし**。ノートは残す |
| プラグイン ID / CID が見つからない | 同上 |
| インスタンス化に失敗 | 同上 |
| 音源は載ったが状態の復元に失敗 | **音源は残す**。状態だけ諦め、初期値のまま |

いずれも通知ウィンドウに**どのトラックが失敗したか**を並べる。
`.ron` 本体の検証と同じく、**一部の失敗で全体を捨てない**。

## フェーズ分け

音源の保存・復元は VST3 と独立しているので**先に単独で入れられる**。
先に済ませておくと `.ron` のスキーマが固まり、VST3 追加時は `kind` に
選択肢が増えるだけになる（`#[serde(default)]` があるので旧ファイルも読める）。

| フェーズ | 内容 | 状態 |
|---|---|---|
| **A** | **音源の保存・復元（CLAP のみ）。`clap.state` の実装を含む** | **完了** |
| **0** | **VST3 のスパイク（可否判断）** | **完了 — 続行可** |
| **1** | 抽象化層（CLAP のみ。挙動を変えない） | **完了** |
| **2** | VST3 バックエンド（音のみ。choke 含む） | **完了** |
| **3** | VST3 の GUI（自前の Win32 埋め込みを使う） | **完了** |
| **4** | 仕上げ（CLAP と VST3 の混在確認、ドキュメント） | **完了** |
| **5** | 素の DLL 形式（単体ファイル）の `.vst3` を選べるようにする | **完了** |

### フェーズ5 で分かったこと（実装後）

**読み込む側は最初から両対応だった。足りなかったのはダイアログだけ。**
`discovery::load_vst3_file` も `audio::activate_vst3_track` もパスを
`vst3-host` にそのまま渡しており、`get_vst3_binary_path` が
「`is_file()` ならそのパスを返す」形で単体ファイルを既に扱っている
（`module_loader/windows.rs` がバンドルの解決に失敗しても元のパスへ落ちる）。
`moduleinfo.json` も、見つからなければ `Ok(None)` になるだけで失敗しない。
したがって変更は `open_vst3_dialog` に `bundle: bool` を足し、形式選択の行に
ボタンを1つ増やすだけで済んだ（`LoadChoice`）。

**ここは想像以上に効く。この環境の VST3 は大半が単体ファイルだった。**
`C:\Program Files\Common Files\VST3` の直下 90 個のうち、バンドルディレクトリは
**4個だけ**で、残りはすべて素の DLL。「非推奨の古い形式」という前提で後回しに
していたが、実際には**そちらが多数派**だった。

**ファイル選択はバンドルの中にも入れるので、逃げ道にもなる。** フォルダ選択が
うまくいかないときは `Contents\x86_64-win\` の DLL を直接指せばよく、
`get_vst3_binary_path` の `is_file()` 分岐でそのまま通る。Pianoteq 9 で
バンドルと内側の DLL の両方に `vst3_smoke` をかけ、出力が1桁まで一致することを
確かめた。

**`vst3-host` 側に、単体ファイルでだけ表面化する不具合がある。**

```
Error: PluginLoadFailed("IPluginCompatibility::getCompatibilityJSON failed: 0x1")
```

`Plugin Compatibility Class` を名乗りながら `getCompatibilityJSON` が
`kResultFalse` (=1) を返す音源があり、`module_info.rs` の
`read_factory_compatibility` がこれを**致命的なエラーとして返す**
（`0x1` は「互換情報なし」の意味なので、`Ok(Vec::new())` が正しい）。
`moduleinfo.json` があればこの経路は通らないため、**バンドル形式では隠れていて、
単体ファイルでのみ必ず表面化する**。読み込み経路
（`plugin_impl.rs:1265`）も同じ関数を通るので、列挙だけでなく装填も失敗する。

手元の 17 個で当たったのは `PapyrusKeys` の1つだけ。クレートの中で起きるので
**公開 API では回避できない**。直すならフォークして `[patch.crates-io]` を張るか、
上流へ報告するかになる。今は入れていない。

### フェーズ4 で分かったこと（実装後）

**混在の検証は `mixed_smoke` に置いた。** 形式ごとの smoke はそれぞれ1形式しか見ていない。
1本のトランスポートで CLAP と VST3 を同時に回し、「片方だけ鳴る区間」「両方鳴る区間」
「休みの区間」を作って測る。実測:

```
実時間  : CLAPのみ=0.2000 両方=0.3624 VST3のみ=0.2022 休み=0.0000
書き出し: CLAPのみ=0.2000 両方=0.3893 VST3のみ=0.2021 休み=0.0000
```

重なった区間が片方だけの区間より大きいことを判定に入れてある。**足し合わせではなく
上書きになっていると、ここが `max(片方)` で止まる**ので、混ぜ方の壊れが出る。
実時間とオフラインの両方を同じ判定にかけているので、書き出し経路だけずれる壊れ方も拾える。

**返し方の分岐も同時に通る。** `RetiredProcessor` は形式ごとに始末が違い
（CLAP はインスタンスへ返す、VST3 はメインスレッドで止める）、混在させて初めて
両方の腕が1回の実行で通る。

**テスト治具（clap-wrapper で test-plugin を VST3 化）は決定どおり用意した。**
同じ音源を両形式で鳴らして突き合わせた結果:

```
実時間  : CLAPのみ=0.2000 両方=0.4000 VST3のみ=0.2000 休み=0.0000
書き出し: CLAPのみ=0.2000 両方=0.4000 VST3のみ=0.2000 休み=0.0000
```

**サイズの見積もりを大きく外した（0.5〜1 GB と言ったが、実際は 80 MB）。**
clap-wrapper が CPM で取得するのは VST3 SDK 3.8.0 の `base` / `public.sdk` /
`pluginterfaces` / `cmake` だけで、**いちばん重い vstgui を含まない**。
ラッパーはエディタを VSTGUI で描かないので要らない。

**VST3 版はシステムに入れなくてよい。** ラッパーは同名の `.clap` を CLAP の探索パス
から探す作りだが、Windows では `CLAP_PATH` 環境変数も見る。`target\debug` を指せば、
共通フォルダに何も置かずに検証できる。

**`--same-plugin` が見ているのは音量であって波形ではない。** 2つの区間は別の音程を
鳴らしているので、波形どうしは比べられない。ベロシティの換算やゲインの取り違えの
ように「片方の形式だけ音量が変わる」壊れ方を捕まえるためのもの。
**別の音源を渡しても、音量がたまたま近ければ通ってしまう**
（Surge XT と test-plugin は 1.1% しか違わず、2% の許容に収まった）。
波形まで比べるには、両トラックに同じ音程を同じ位相で鳴らす専用のシーケンスが要る。
そこまではやっていない。

**フェーズ A は単体で価値がある。** VST3 に進まない判断をしても無駄にならない
（「開いたら音源も音作りも戻る」は現状いちばん欠けている機能）。

### フェーズ3 で分かったこと（実装後）

**`plugin_window.rs` はそのまま使えた。** 計画どおり、窓はホストが作り、
`vst3-host` の `PluginWindow` / `EmbeddedEditor` には頼っていない。`open_editor` の中身は
`createView` → `setFrame` → `attached(hwnd, kPlatformTypeHWND)` なので、
計画で書いた「自前で `IPlugView::attached` に渡す」と実質同じものになっている。
`IPlugFrame` も `vst3-host` が用意していて、`resizeView` の要求は
`take_editor_resize_request()` で取りに行ける（CLAP はコールバックで届くので、
ここだけ向きが逆）。`resizeView` を受けた時点で `onSize` は済んでいるため、
ホスト側は窓を合わせるだけでよい。

**取り合いの原因を測って突き止めた（推測は2回とも外れた）。**

エディタを開いた状態で走らせると、**ブロックの2割強が `Busy` で無音**になった。
最初は「メインスレッドが毎フレーム `lock()` で待つせいで、オーディオスレッドが
手放すたびに横取りしている」と考えて `try_lock` に変えたが、**件数は変わらなかった**。
次に「リサイズの往復がループしている」と考えて計測したが、リサイズは4回で収束していた。

実際に測ると、メインスレッドの保持は **15秒で合計 11.5ms（0.077%）** しかなく、
毎フレームのポーリングを丸ごと止めても件数はほぼ同じだった。ログの並びを見ると
**`Busy` はすべてエディタが開ききる前に出ており、開いたあとは1件も出ていない**。

つまり原因は**エディタの生成そのもの**。`open_editor` は `&mut Plugin` を要求するので、
その間ずっと音源を握る。Surge XT ではここが約2.9秒かかっていた。

**半分は削れた。** 「リサイズできるか」を開く前に聞いていたのが効いていた
（`editor_can_resize()` はエディタが無いと**そのためだけに使い捨ての view を作る**）。
窓は仮の大きさ・リサイズ可で作って先に貼り付け、**大きさも可否も生きている view に
聞き直して後から直す**形にしたところ、2.9秒 → 1.7秒になった
（`PluginWindow::set_resizable` を追加）。

残りの約1.7秒は音源自身のエディタ生成なので、このクレートを使う限り避けられない。
実 DAW でも重いエディタを開くと音が途切れるので、仕様として受け入れる。
持ち越しの仕組みがあるので、**開き終わったあとに音がずれたり鳴りっぱなしになったりはしない**
（押しっぱなしのノートはそのまま続き、その間の note-off も落ちない）。
ただし溜まったイベントは1ブロックにまとめて出るため、密なシーケンスだと
開いた直後に短い雑音が出る。

**`Busy` はオーディオスレッドから出力しない。** 想定内で自然に直る状態なのに、
`eprintln!` はオーディオスレッドで標準エラーのロックと I/O を取る。
状態そのものより出力のほうが害が大きい。

**メインスレッドの毎フレーム処理も `try_lock` にした（原因ではなかったが、こちらが正しい）。**
毎フレーム待ちに行くと、オーディオスレッドが手放した瞬間に横取りする形になり、
理屈のうえでは `Busy` を誘発する。取れなければ次のフレームでよいものは待たない。

### フェーズ2 で分かったこと（実装後）

**設計調査の結論を1つ覆した。`Plugin` は値ではなく `Arc<Mutex<Plugin>>` で共有する。**

調査の時点では「`Plugin` を処理器が値で持つ」と決めていたが、それだと**エディタ
（フェーズ3）と状態の保存・復元がメインスレッドから届かなくなる**。`vst3-host` の
`Plugin` は `IAudioProcessor` と `IEditController` を1つの型にまとめており、
`open_editor` / `save_state` / `load_state` がいずれも `&mut Plugin` を要求するため。
本物の VST3 ホストでは editController は別の COM オブジェクトなので、この制約は
VST3 由来ではなくクレートの作り由来である。

`vst3-host` 自身もこの組み合わせを解いていない。実時間経路 (`RtAudioHandle` /
`RtControl`) にはエディタへの入口が無く、エディタを扱う経路
（`AudioHandle::plugin()` と `EmbeddedEditor`）はどちらも `Arc<Mutex<Plugin>>` を要求する。

そこで `audio/vst3.rs` に `SharedPlugin` を置き、**オーディオスレッドは必ず
`try_lock`** で触る。待たないので締め切りは落とさない。取れなかったブロックは
無音になるが、取り合いが起きるのはエディタの開閉と状態の保存・復元のときだけで、
どれもユーザー操作の頻度でしか起きない。

**取れなかったときイベントを捨ててはいけない。** そのブロックの note-off を落とすと
音が鳴りっぱなしになる。`Vst3Processor` は送れなかったイベントを持ち越し、
次に取れたブロックの先頭で送り直す（位置は失われるが、鳴り続けるよりましと判断）。
`vst3_smoke` の第1段で、音源を握った状態のブロックが `ProcessError::Busy` を返して
無音になり、次のブロックで発音が届くことを確かめている。

**choke のアクティブノート追跡はトランスポートではなくバックエンドに置いた。**
計画では `TrackSequence` に `u128` を持たせるつもりだったが、そこに置くと
`BlockEvent::Choke` が個別の note-off に展開されてしまい、**CLAP 側が `NoteChoke` を
使えなくなる**。バックエンドは自分が送った note-on / note-off をすべて見ているので、
`Vst3Processor` 側に `u128` を持たせれば同じことが CLAP を巻き込まずにできる。

**ブロック長は `AudioBuffers` の `Vec` の長さで決まる。** 実装前に残していた
「`block_size` を毎回書き換えてよいか」は、`internal.process` を読んで解決した。
フレーム数は `buffers.outputs[0].len()` から取られ、`block_size` は**分割の刻み**として
しか使われない。したがって短いブロックはそのまま通り、長いブロックは自動で分割される。
生成時に上限ぶん確保しておけば、毎ブロックの `resize` で確保は起きない。

**その他**

- 処理エラーは形式中立の `audio::ProcessError` にまとめた。`vst3-host` の
  `ProcessFailed` / `NotProcessing` は確保しない形で作られており（クレート側が
  オーディオスレッド用に意図している）、CLAP 側と同じ扱いにできる
- `Vst3Host` はインスタンス化の入口でしかない。`Plugin` がモジュールを自分で
  抱えるので、読み込みのたびに作って捨ててよい（CLAP の `PluginEntry` と同じ整理）
- **Windows の `.vst3` はバンドルディレクトリなので、ファイル選択ダイアログでは掴めない。**
  「♪」を押したら先に形式を聞き、CLAP はファイル選択、VST3 はフォルダ選択を開く形にした。
  素の DLL 形式の `.vst3`（VST 3.6.10 以降は非推奨）は当時選べなかった
  → **フェーズ5 で選べるようにした**（下記）
- 停止 (`setProcessing(false)`) はリアルタイム安全でないので、`RetiredProcessor::Vst3` は
  **止まっていない状態**でメインスレッドへ渡り、受け取った側が止める。CLAP は
  オーディオ処理器のまま止められるので、ここだけ非対称になっている

### フェーズ2 の設計調査（実装前に判明したこと）

**`Plugin` も `RealtimePluginRunner` も `Send` だった**（`spike/vst3/src/bin/sendness.rs`
でコンパイルにより確定）。処理器をリングバッファでオーディオスレッドへ渡す本体の
設計がそのまま使える。

**`RealtimePluginRunner` は使わず、`Plugin` を直接持つ。**
（**所有の形だけは実装時に変えた。**上の「フェーズ2 で分かったこと」を参照）

runner は「音源を所有し、指示を SPSC リングで受ける」ものだが、**本体には既に
同じ役割のリングバッファ (`GuiMsg`) がある**。runner を挟むと
`BlockEvents` → `RtControl` のリング → runner の drain という二重の受け渡しになり、
リングが溢れると指示を落とす。`Plugin::send_midi_event_at` と
`Plugin::process_audio` はどちらも公開されているので、自前の処理器から直接呼べる。

破棄の安全性も確保できる。VST3 は音源を作ったスレッドで破棄する必要があるが、
本体は「音源はメインスレッドで作り、処理器はメインスレッドへ返して解放する」
設計なので、構造的に満たされる。

**`process_audio` はオーディオスレッドで Mutex を取る。**

```rust
if let Ok(mut levels) = self.audio_levels.lock() { ... }   // 毎ブロック
```

`process_bus_audio` も同じ。この Mutex を取る相手は**メータ取得 API
(`get_output_levels`) だけ**なので、**それを使わなければ競合しない**
(取得者がいなければロックは常に空いており、実質ノーコスト)。
本体はメータを使っていないので、このまま進める。使いたくなったら
別途アトミックで持つこと。

### フェーズ1 で分かったこと

- **`buffers.rs` の共通化は見送った。** チャンネル変換 (`mux` / `mix_mono` /
  `mono_to_multi`) は素のスライスを扱うので共有できそうに見えるが、元の形が違う
  （CLAP はポートごとに連続、VST3 はチャンネルごとの `Vec`）。VST3 側の要求が
  はっきりする前に共通化すると引き直しになるので、フェーズ2 で判断する
- **`seq_smoke` と `choke_smoke` が手組みの並行実装を持っていた**（自前で
  `AudioPorts` を組み立てていた）。本体と同じ経路 (`activate_track` →
  `TrackProcessor`) に載せ替えた。並行実装のままだと、抽象化を変えたときに
  そこだけ古い形で通ってしまう
- **単一バリアントの enum は `match` ではなく `let` で分解する。** どちらも
  VST3 を足せばコンパイルエラーになるが、`let` のほうが clippy に怒られない

### フェーズ0 の結果（続行可）

`spike/vst3/` に独立したクレートを置いて確かめた（親のワークスペースからは
切り離してあるので、本体のビルドには影響しない）。Surge XT の VST3 で実行。

```
ピーク: 発音前=0.0000 発音中=0.2126 消音後=0.0000
取りこぼした指示: 0 / 破棄完了: true
✅ VST3 スパイク成功: 読み込み・発音・消音・後始末が通った
```

**3つの関門はすべて通った。**

| 関門 | 結果 |
|---|---|
| Windows でビルドできるか | ✅ `default-features = false` でコアのみ。**C のツールチェーン不要** (`vst3` 0.3.0 は生成済みバインディング同梱) |
| 読み込んで音を出せるか | ✅ バンドルディレクトリをそのまま読める。発音前・消音後とも無音 |
| 設計に載る形か | ✅ 下記 |

**リアルタイム経路は懸念より良かった。** README の「RT 監査済みでない」は主に
`Arc<Mutex<Plugin>>` を使う簡易経路 (`play` / `simple`) の話で、
`RealtimePluginRunner` は別物だった。

- **音源を runner が所有する**（= オーディオスレッド側）。指示は SPSC リングで渡す。
  本体の「処理器をリングバッファでオーディオスレッドへ渡す」設計とそのまま同じ形
- **コマンドの drain に上限がある。** 制御スレッドが押し続けてもコールバックが
  占有されない、という意図がドキュメントに明記されている
- `process()` はリングの drain とプラグインの処理だけ。目立つ確保は見当たらない
- **サンプル精度でノートを置ける** (`send_midi_at(event, offset)`)

**設計に効く差分**

- `AudioBuffers` は `Vec<Vec<f32>>`（チャンネルごとの Vec）。本体の `buffers.rs` は
  平坦な配置なので、変換が要る。確保は生成時の一度きりなので実害はない
- **後始末がスレッド固定。** `runner.stop()` → `drop(runner)` → `control.service_teardown()`
  の順で、COM の破棄を特定のスレッドで行う必要がある。本体の
  「解放は必ずメインスレッド」という retire 経路と噛み合わせる設計が要る
- `simple::load_plugin*` は呼ぶたびに `Vst3Host` を作る。複数トラックでは
  ホストを1つにまとめる形にしたい

### フェーズ A で分かったこと

- **`test-plugin` が `clap.state` を持っていなかった。** そのままでは保存経路の検証が
  「空を保存して空を戻す」になるため、テスト用プラグイン側にも実装した。
  `state_smoke` で往復を実プラグインに対して確かめている
- **`PluginEntry` をホスト側で持ち続ける必要はない。** clack は
  `PluginInstanceInner` に entry を保持しており（DLL を生かすため）、
  インスタンス化のたびに開き直してよい。`Candidates` から entry を外した
- clack の更新で拡張のコールバックが `&mut self` から `&self` になったため、
  ホストのメインスレッド側は内部可変性 (`Cell` / `RefCell`) へ移した

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

**自前で実装する。** `plugin_window.rs` の Win32 窓を共有し、
`IPlugView::attached(hwnd, kPlatformTypeHWND)` に渡す。プラグイン発のリサイズは
`IPlugFrame::resizeView`。`vst3-host` の GUI 機能には頼らない
（Windows での実行時検証がされていないうえ、こちらは既に Surge XT で
埋め込みの動作を確認できているため）。

`gui.rs`（153行）をバックエンドで分岐させ、CLAP 側の実装はそのまま残す。

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

**clap-wrapper を使う（決定）。** 同一の音源で CLAP 版と VST3 版の出力を
突き合わせられるので、抽象化が壊れていないことを直接確かめられる。CMake は
Opus のときに避けた経緯があるが、あれは**配布物のビルドに必要**だったのに対し、
今回は**テスト治具を一度作るだけ**なので事情が違う。

生成した `.vst3` は `target/` 配下に置き、`.gitignore` の対象とする
（`test_plugin.clap` と同じ扱い）。

## リスク

- **`vst3-host` が早期段階**（スター 26、macOS で検証、Windows GUI は実行時未検証、
  RT 経路が未監査）。フェーズ0 はこのリスクを最初に潰すためにある
- **早すぎる抽象化**。フェーズ0 を先に置くことで緩和する
- **保守範囲が広がる**。README は「テスト用ホスト」と位置づけているので、
  2つのプラグイン形式を抱える価値があるかは別途の判断

## 決定事項

1. **音源のパス・種別・状態を `.ron` に保存する。** CLAP も同様。
   読み込みに失敗したトラックは音源なし（空）とし、ノートは残す
2. **テスト用 VST3 は clap-wrapper で用意する。** 既存の `test-plugin` を VST3 化し、
   同じ音源で両バックエンドの出力を突き合わせる。CMake はテスト治具のビルドにのみ
   必要で、配布物には含まれない
3. **GUI は自前で実装する。** `plugin_window.rs` の Win32 埋め込みを共有し、
   CLAP 側の実装はそのまま残す。`vst3-host` の GUI 機能には頼らない
   （Windows での実行時検証がされていないため）
4. **音源の状態は `.ron` に base64 で埋め込む。** サイドカーにはしない
   （下記「状態のバイト列の置き場所」を参照）

### 状態のバイト列の置き場所（決定: `.ron` に埋め込む）

音源の状態は不透明なバイト列で**大きくなりうる**。Surge XT のパッチは数十 KB あり、
base64 にすると 1.33 倍になる。トラックが増えれば `.ron` の大半が読めない文字列で
埋まる。サイドカー方式（`song.ron` + `song.plugins/track0.bin`）と比較したうえで、
**埋め込みを採る**。

`.ron` を選んだ動機のひとつは可読性だが、それは主にテンポ・拍子・トラック構成・
ノートを目で確認できることにある。その部分は**構造体の並び順で `plugins` を末尾に
置けばファイル先頭に残る**。一方「1ファイル = 1プロジェクト」が崩れる代償
（移動・複製で片方を落とす事故）は日常的に効いてくる。

### `.ron` の形（フェーズ A 後）

```ron
(
    version: 2,
    tempo: 120,
    beats: 4,
    beat_type: 4,
    scale: Equal12,
    swing_peak_ratio: 1.5,
    tracks: [
        (name: "トラック 1", lanes: 1, muted: false, soloed: false, swing: false),
    ],
    notes: [
        (start: 0.0, duration: 0.5, semitone: 0, octave: 4, velocity: 100, track: 0, lane: 0),
    ],
    // 読める部分を先頭に残すため、巨大になりうる plugins は末尾に置く
    plugins: [
        Some((
            kind: Clap,
            path: "C:\\Program Files\\Common Files\\CLAP\\Surge XT.clap",
            id: "org.surge-synth-team.surge-xt",
            state: "U3VyZ2VQYXRjaAAAAA...",   // base64
        )),
    ],
)
```

`version` は 2 に上げる。`plugins` に `#[serde(default)]` を付けておけば、
**バージョン1 のファイル（音源情報なし）もそのまま読める**。

base64 化には `base64` クレート（MIT OR Apache-2.0、純 Rust）を使う。自前でも
40行ほどで書けるが、**ここの誤りは音源の状態を静かに壊す**（読み込めてしまうが
音が違う）ので、枯れた実装に任せる。`serde` に `Vec<u8>` をそのまま渡すと
RON では数値の配列になってしまうため、文字列への変換は必須。
