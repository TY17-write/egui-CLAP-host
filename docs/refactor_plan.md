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

## フェーズ1: `editor_ui.rs` → `editor_ui/` — 済

外から使われているのは **`EditorState` / `EditorCommand` / `editor_panel` の3つだけ**
(`main.rs` から)。**公開面が極端に小さいので、割っても外への影響が無い。**

4545行の1ファイルを11ファイルに割った (テストを含む実測):

| ファイル | 内容 | 行 | テスト |
|---|---|---:|---:|
| `metrics.rs` | 複数箇所で使う画面寸法とズームの上下限 | 34 | – |
| `history.rs` | `Snapshot`・`EditGroup`・`History` | 135 | – |
| `help.rs` | 操作ガイド | 143 | – |
| `mod.rs` | `EditorCommand`・`editor_panel`・`pub use` | 243 | – |
| `shortcuts.rs` | ショートカット・再生の開始停止・クリップボード | 268 | – |
| `color.rs` | `note_fill` ほか Oklch 変換 | 331 | 7 |
| `toolbar.rs` | 上部ツールバーと揺らぎの行 | 371 | – |
| `gutter.rs` | 左のトラック欄・段の帯・CC 段の一覧 | 542 | – |
| `grid.rs` | `grid` と `take_wheel_notches` | 771 | – |
| `geometry.rs` | 座標変換・当たり判定・移動量の算出 | 835 | 19 |
| `state.rs` | `EditorState`・`NoteDefaults`・選択と編集 | 1014 | 20 |

テストは 46 件のまま、`cargo test --lib` は 229 件のまま。

**アンドゥのテスト3件だけは `state.rs` に置いた。** `EditorState::history` は
private のままにしたかったので、フィールドを持つ側に寄せている
(`history.rs` を機構だけのファイルに保てる)。

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

### 割ったら `cargo coupling` の点が下がった (指標の限界)

分割後にもう一度測ると、**God Module は消えたが総合点は 0.89 → 0.85 に落ちた**。

| 分割前 | 分割後 |
|---|---|
| God Module: `editor_ui` (48関数/30) | **消えた** |
| — | High Efferent: `editor_ui::grid` → 32依存 |
| — | High Afferent: `editor_ui::metrics` ← 27依存 |

**これは指標の作りによるもので、コードが悪くなったのではない。** 1ファイルに
入っていたときは関数どうしの参照が `use` として現れないので、依存の辺が0本に
見えていた。割れば必ず辺が生える。

- `metrics.rs` は**定数を置いただけのファイル**で、全員が使うのは当然
- `grid.rs` の32依存は、その大半が `geometry` から取る算出関数

助言どおり `MetricsInterface` トレイトを作るのも、`grid` をさらに割るのも、
**辺を隠して点を上げるだけ**で読みやすさには寄与しない。採らない。

### `grid` (771行) をさらに割るかは保留 → **割らないと決めた**

`grid` の中は「背景 → 地色 → 区切り線 → ノート → 再生線 →
操作 → ドラッグ」と節に分かれている。描画部と操作部で割れそうに見える。

**が、これは移動ではなく分解**になる。`grid` の中は `ui`・`state`・スクロール位置を
共有しながら上から下へ流れており、割るには引数の設計が要る。

**ユーザーの判断で、割らないことにした。** 1ファイルに収まった今の形を維持する。

---

## フェーズ2: `main.rs` → `app/` (ライブラリへ移す) — 済

### なぜバイナリの下ではなくライブラリへ入れたか

`main.rs` が `mod app;` と書くと `host/src/app/` を探しに行く。これは
**`lib.rs` の隣**で、モジュールの木が2つ並ぶ形になり紛らわしい。

`lib.rs` に `pub mod app;` を足して**ライブラリ側の一員にした**。
副作用として、`main.rs` にあった6つのテスト (`db_to_linear` 周り) が
`cargo test --lib` に合流し、**`--bin` を別に走らせる必要が無くなった**
(229 + 6 = 235件)。

2854行の1ファイルを、`main.rs` 70行 + 8ファイルに割った (実測):

| ファイル | 内容 | 行 | テスト |
|---|---|---:|---:|
| `main.rs` | `fn main()` と日本語フォントの読み込みだけ | 70 | – |
| `notice.rs` | `Notice`・`notice_window` | 69 | – |
| `project_io.rs` | MIDI 入出力・`.ron` の保存と読み込み・スナップショット | 328 | – |
| `plugins.rs` | 読み込みダイアログ・`load_plugin`・サンプルレート切替 | 339 | – |
| `track.rs` | `TrackAudio`・`AudioTrackUi`・`Engine`・音源の生成 | 357 | – |
| `routing.rs` | ルーティングとミキサの反映、dB 変換 | 358 | 6 |
| `render.rs` | WAV / Opus / CeVIO の書き出し | 439 | – |
| `mod.rs` | `App` の定義と `impl eframe::App`(毎フレームの処理) | 479 | – |
| `mixer_ui.rs` | オーディオトラックの一覧と詳細・音源選択ポップアップ | 539 | – |

### `App` のフィールドは private のままにできた

**Rust の可視性は「そのモジュールとその子孫」まで届く。** `App` を `app/mod.rs`
に置くと、`app::routing` などの子モジュールは**兄弟ではなく子孫**なので、
private のフィールドにそのまま触れる。

そのため `pub(super)` を足したのは**子モジュール側の項目だけ**で、
`App` の内部は1つも公開範囲が広がっていない。外に出したのは
`App` 本体と、`main.rs` から呼ぶ `App::with_autoload` の2つだけ。

### ついでに消したもの

- `KEYS` と `TrackAudio::pressed_keys` — 無効化したままの鍵盤 UI (別コミット)

**パラメータ汎用エディタのコメントアウトは残してある。** 同じく無効化中だが、
消す指示が出ていないので触っていない。

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

## 分割後の `cargo coupling`

| | 分割前 | フェーズ1後 | フェーズ2後 |
|---|---|---|---|
| 点 (`--no-git --exclude-tests`) | 0.89 | 0.85 | 0.87 |
| God Module | `editor_ui` | なし | なし |
| High Afferent | `sequencer` | `sequencer`・`metrics` | + `audio` |
| High Efferent | なし | `grid` | `grid` |

**God Module は消えたが、点は戻っていない。** 割れば依存の辺が生えるためで、
残っているのは「土台のモジュールが広く使われている」形ばかり
(`sequencer` = 型の置き場、`audio` = エンジン、`metrics` = 定数)。
**被参照が多く自分からの参照が少ないのは安定した核**であって、欠陥ではない。

この指標を上げにいくと、データ型や定数を動的分配の裏に隠すことになる。
**行数で測った読みやすさとは別の話**なので、ここから先は追わない。

## 着手前の確認

`CLAUDE.md` のとおり、**始める前に作業ツリーがコミット済みであること**を確認する。
広範囲にファイルを動かすため、`git` で戻せない状態では入らない。
