# モジュール分割の計画

`editor_ui.rs` (4545行) と `main.rs` (2921行) を、責務ごとのディレクトリに割る計画。

**目的は「読める大きさに戻す」ことだけ**で、動作は1バイトも変えない。
新しい機能も、既存の挙動の変更も、この計画には含めない。

## まず: `cargo coupling` の C 評価はほぼ無視してよい

`cargo coupling --ai ./host/src/` は **Grade C (0.57) / High 1件・Medium 29件**を出す。
しかし**git 履歴とテストを外すと Grade S (0.89) / Medium 2件**まで落ちる。

```
cargo coupling --ai --no-git --exclude-tests ./host/src/
```

つまり C の中身は、ほとんどが**構造ではなく履歴**由来だった。内訳を分けておく。

### 直さないもの (24件: Hidden Coupling)

`audio::offline ↔ bin::wav_smoke` が「100%・6回の共変更」で挙がるが、これは
**wav_smoke が offline を検証するために存在する**からで、一緒に変わるのが正しい。
`audio::transport ↔ bin::seq_smoke`、`audio::clap ↔ bin::choke_smoke` も同じ。

**テストがテスト対象を追いかけているだけ**なので、「共有の抽象を作れ」という
助言に従うと、検証と実装の間に無駄な層が挟まって逆に悪くなる。
ツール自身も blind spot として「共変更は隠れた結合を*示唆し得る*」と留保している。

### 直さないもの (1件: High Afferent Coupling — `sequencer`)

33個から参照されている。しかし `sequencer` は `Note` / `MidiEditor` / `ScaleMode` /
`TrackInfo` を持つ**土台の型の置き場**で、被参照が多いのは設計どおり。
被参照が多く・自分からの参照が少ない形は**安定した核**であって、欠陥ではない。

助言どおり `SequencerInterface` トレイトを挟むと、
**単なるデータ型を動的分配の裏に隠す**ことになり、何も得られない。

### 直すもの (1件: God Module — `editor_ui`)

これは本物。48関数 / 閾値30。行数でいえば 4545 行。

### ツールが挙げていないが直すもの — `main.rs`

**`main.rs` (2921行) は God Module に挙がっていない。**
判定が「行数」ではなく「項目数」で、`main.rs` の関数の大半が
`impl App` の中にいるため、`impls: 1` として数えられて閾値を抜けてしまう。

**指標を抜けていることは、読みやすいことを意味しない。**
`impl App` ひとつが 483〜2288 行 (約1800行・41メソッド) を占めており、
ここが実際に一番読みにくい。ツールではなく実感を採る。

### ツールが「見えない」と言っている本物の重複

blind spot に挙がっている implicit functional coupling が1件実在する。

- `sequencer.rs:632` `suppress_redundant_cc_releases(&mut Vec<SeqEvent>)`
- `midi.rs:219` `suppress_redundant_cc_releases(&mut Vec<(u32, u8, TrackEventKind)>)`

**同じアルゴリズムが型違いで2つある** (同時刻に後続の同番号 CC があれば解除を捨てる)。
`editor_ui ↔ midi` の共変更が挙がっていた本当の理由はおそらくこれ。

---

## 方針

1. **純粋な移動しかしない。** 関数の中身は触らない。改名もしない
2. **1フェーズ = 1ファイル。** 終わるたびに検証してコミットする
3. **可視性は `pub(super)` を基本にする。** 同じディレクトリの兄弟からは見えるが、
   `editor_ui` / `app` の外へは漏れない。今 private なものが `pub` に格上げされない
4. **テストは移動先へ一緒に連れて行く。** 対象と同じファイルに置く

### 移動は必ず Edit / Write ツールで行う

対象は**日本語のコメントが密なファイル**で、`CLAUDE.md` の禁止事項に正面から当たる。

> Windows PowerShell 5.1 の `Get-Content` は UTF-8 を ANSI (CP932) として読むため、
> 読んだ時点で日本語が不可逆に壊れる

`sed` で行範囲を切り出して繋ぐ、という一番速いやり方は**使えない**。
1ファイルずつ手で運ぶ前提で、フェーズを細かく切ってある。

### 各フェーズの受け入れ条件

移動しかしないので、**数値が1つでも動いたら移動ミス**である。

| 確認 | 期待 |
|---|---|
| `cargo fmt --all --check` | 差分なし |
| `cargo build --workspace` | warning 0 |
| `cargo test --lib` | 229 → フェーズ2で 235 (`main.rs` の6件が lib に移るため) |
| `cargo test --bin egui-clap-host` | 6 → フェーズ2で 0 |
| smoke 10本 | **数値が完全に一致** (`mixed_smoke` = 0.2000 / 0.4000 など) |

smoke の数値一致が一番効く。過去4件の実害 (上書き/足し込みの取り違え、パンの -3dB、
複数トラックへの重ね、6ch の R 取り違え) は**すべて型では防げず、数値でだけ出た**。

---

## フェーズ1: `editor_ui.rs` → `editor_ui/`

外から使われているのは **`EditorState` / `EditorCommand` / `editor_panel` の3つだけ**
(`main.rs` から)。**公開面が極端に小さいので、割っても外への影響が無い。**

| 新ファイル | 移すもの (現在の行) | 目安 |
|---|---|---|
| `mod.rs` | `EditorCommand` (984-1011)、`editor_panel` (1012-1205)、`pub use` | 250 |
| `metrics.rs` | 画面寸法の定数 (16-65) のうち複数箇所で使うもの | 60 |
| `color.rs` | `note_fill` ほか OKLCH 変換 (80-206) | 130 |
| `state.rs` | `NoteDefaults`・`EditorState`・`MiddleDrag`・`impl EditorState` (207-366, 500-813) | 480 |
| `history.rs` | `Snapshot`・`EditGroup`・`History` (814-938) | 125 |
| `geometry.rs` | 座標・当たり判定の純関数 (939-983, 2681-2861, 3268-3379) | 340 |
| `grid.rs` | `grid` (1965-2680) | 720 |
| `gutter.rs` | 左のトラック欄と段の帯 (367-467, 2862-3267) | 500 |
| `toolbar.rs` | `toolbar`・`groove_toolbar`・ラベル (1588-1964) | 375 |
| `shortcuts.rs` | `Shortcuts`・`shortcuts`・再生の開始停止・クリップボード (468-499, 1344-1587) | 275 |
| `help.rs` | 操作ガイド (1206-1343) | 140 |

定数は**使う場所が1箇所ならそのファイルへ、複数なら `metrics.rs` へ**。確認済み:

- `LANE_BUTTON_ROW_Y/H`・`LANE_STRIP_W` → `gutter.rs` のみ
- `EDGE_W` → `geometry.rs` (`hit_note`) のみ
- `VELOCITY_GHOST_ALPHA`・`NOTE_LABEL_SIZE`・`VELOCITY_WHEEL_STEP` → `grid.rs` のみ
- `RULER_H`・`GUTTER_W`・`MIN_OCTAVE`/`MAX_OCTAVE` → 広く使う → `metrics.rs`

### `geometry.rs` を最初に出す

**46あるテストの半分以上がここに掛かっている** (`hit_note_*`・`note_rect_*`・
`edge_scroll_*`・`resize_*`・`seek_*`・`horizontal_zoom_*`・`velocity_fill_*`)。
egui の描画に触らない純関数の塊なので、**一番安全で、一番効果が大きい**。

順序は `color.rs` → `geometry.rs` → `history.rs` → `state.rs` → `help.rs` →
`shortcuts.rs` → `toolbar.rs` → `gutter.rs` → `grid.rs`。
**依存の葉から順に出す**ので、途中で `mod.rs` が壊れない。

### `grid` (716行) をさらに割るかは保留

`grid` 1つで 716 行あり、中は「背景 → 地色 → 区切り線 → ノート → 再生線 →
操作 → ドラッグ」と節に分かれている。描画部と操作部で割れそうに見える。

**が、これは移動ではなく分解**になる。`grid` の中は `ui`・`state`・スクロール位置を
共有しながら上から下へ流れており、割るには引数の設計が要る。
**フェーズ1では触らず、1ファイルに収まった状態を見てから判断する。**

---

## フェーズ2: `main.rs` → `app/` (ライブラリへ移す)

### なぜバイナリの下ではなくライブラリへ入れるのか

`main.rs` が `mod app;` と書くと `host/src/app/` を探しに行く。これは
**`lib.rs` の隣**で、モジュールの木が2つ並ぶ形になり紛らわしい。

`lib.rs` に `pub mod app;` を足して**ライブラリ側の一員にする**。
副作用として、`main.rs` にある6つのテスト (`db_to_linear` 周り) が
`cargo test --lib` に合流し、**`--bin` を別に走らせる必要が無くなる**。

`main.rs` に残すのは `fn main()` と `setup_japanese_fonts` だけ (約80行)。

| 新ファイル | 移すもの (現在の行) | 目安 |
|---|---|---|
| `mod.rs` | `App` の定義 (440-482)、`impl eframe::App for App` (2430-2868) | 500 |
| `track.rs` | `TrackAudio`・`TrackPlugin`・`AudioTrackUi`・`Engine`・`Candidates`・生成 (110-301, 2289-2429) | 330 |
| `routing.rs` | ルーティングとミキサの反映、dB 変換とその6テスト (396-420, 777-932, 1447-1492, 1615-1680) | 400 |
| `plugins.rs` | 読み込みダイアログ・`load_plugin`・サンプルレート切替 (379-438, 487-576, 1387-1446, 1502-1614, 2217-2233) | 450 |
| `project_io.rs` | MIDI 入出力・`.ron` の保存と読み込み・スナップショット (302-316, 577-767, 2186-2288) | 350 |
| `render.rs` | WAV / Opus / CeVIO の書き出しと `render_setup` (933-1352) | 420 |
| `mixer_ui.rs` | オーディオトラックの一覧と詳細・音源選択ポップアップ (386-395, 1681-2185) | 520 |
| `notice.rs` | `Notice`・`notice_window` (317-378) | 70 |

`impl App` は分割後も**1つの型に対する複数の `impl` ブロック**になる。
Rust では同じクレート内であれば何箇所に分けてもよい。

### ついでに消すもの

- `main.rs:37` の `KEYS` — `#[allow(dead_code)]` 付きで「鍵盤 UI 無効化中のため未使用」。
  鍵盤 UI を戻す予定が無いなら**この機に消す**。残すなら理由を書き足す

---

## フェーズ3: 重複した `suppress_redundant_cc_releases` (任意)

`sequencer.rs` と `midi.rs` に同じアルゴリズムが型違いで置かれている。
**片方を直してもう片方を忘れると、書き出した MIDI と鳴る音がずれる**種類の重複。

まとめるなら「`(時刻, 番号, 値)` を取り出す関数」を渡す形にして、片側へ寄せる。

**ただし優先度は低い。** 現状どちらも動いており、テストもある。
フェーズ1・2と混ぜると「移動しかしない」という約束が崩れるので、
**やるなら完全に別のコミットで**。

---

## やらないこと

- **`audio/` はそのまま。** `graph.rs` (テストを除くと740行)・`mod.rs` (741行) は
  まだ読める大きさで、責務も割れている。ここを触るのは今のところ churn にしかならない
- **`sequencer.rs` にトレイトを挟まない** (上述)
- **smoke バイナリの共通化をしない** (上述)
- **改名・API の整理をしない。** 移動と混ぜると差分が読めなくなる

## 着手前の確認

`CLAUDE.md` のとおり、**始める前に作業ツリーがコミット済みであること**を確認する。
広範囲にファイルを動かすため、`git` で戻せない状態では入らない。
