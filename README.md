# clap-host-test

Rust + egui で CLAP (CLever Audio Plug-in) をロードして再生する最小ホスト環境。

## 構成

| クレート | 内容 |
|---|---|
| `host` | ミニホスト本体 (egui GUI + cpal オーディオ出力 + clack-host) |
| `test-plugin` | テスト用のサイン波シンセ CLAP プラグイン (16ボイス、Volume パラメータ、ベロシティ対応) |

主要ライブラリ: [clack](https://github.com/prokopyl/clack) (CLAP の安全な Rust ラッパー、git 依存)、cpal、eframe/egui、rtrb。

## ビルドと実行

```powershell
# 全体をビルド
cargo build

# テストプラグインの .clap ファイルを生成 (DLL のコピー)
Copy-Item target\debug\test_plugin.dll target\debug\test_plugin.clap -Force

# オフライン検証 (オーディオデバイス不要、ロード→ノートオン→波形確認)
cargo run -p clap-host-test --bin smoke -- target\debug\test_plugin.clap

# シーケンサー再生エンジンのオフライン検証
cargo run -p clap-host-test --bin seq_smoke -- target\debug\test_plugin.clap

# GUI ホストを起動
cargo run -p clap-host-test
```

GUI 起動後:

1. 「.clap ファイルを開く…」で `target\debug\test_plugin.clap` (または Surge XT など市販/フリーの CLAP) を選択
2. プラグイン名のボタンをクリックしてロード → 即座にオーディオストリーム開始
3. パラメータがスライダーとして自動生成される (CLAP params 拡張の汎用エディタ)
4. 画面下部の鍵盤ボタンをクリックすると発音する

## アーキテクチャ

```
[メインスレッド: egui]                 [オーディオスレッド: cpal コールバック]
  スライダー/鍵盤操作
    │ rtrb リングバッファ (GuiMsg) ──▶  CLAP イベントに変換して process() に投入
    │                                   プラグイン出力 → ミックス → CPAL バッファ
    ◀── crossbeam チャンネル ────────  request_callback() などのホスト要求
```

- CLAP のスレッドモデル (main-thread / audio-thread) は clack の型システムで担保
- オーディオスレッドではロック・アロケーションなし (リングバッファ + 事前確保バッファ)
- ノートは CLAP ダイアレクト優先、プラグインが MIDI しか受けない場合は生 MIDI にフォールバック

## プラグイン独自 GUI (clap.gui 拡張)

ロードしたプラグインが GUI を持つ場合、「エディタを開く」ボタンが表示される。

- **embedded モード優先**: ホストが Win32 ネイティブウィンドウを作成し (`plugin_window.rs`)、
  HWND を `set_parent` で渡して埋め込む。eframe (winit) のメッセージループが同一スレッドの
  全ウィンドウにメッセージを配送するため、追加のメッセージポンプは不要
- **floating フォールバック**: embedded 非対応プラグインはプラグイン自身にウィンドウを作らせる
- `clap.timer-support` 実装済み (JUCE 系プラグインの GUI 描画に必要)。egui の update ループから tick
- プラグインからのリサイズ要求 / ユーザーのウィンドウリサイズの双方向に対応
- **動作確認**: Surge XT の UI 表示を確認済み
- 既知の問題: clack 同梱の gain-gui サンプル (egui-baseview 製) はウィンドウが白表示になる。
  baseview の WM_TIMER 駆動レンダリングとの相性問題とみられる (実プラグインでは未再現)

検証用 CLI:

```powershell
# 起動時に自動ロードし、GUI も自動で開く
cargo run -p clap-host-test -- "C:\Program Files\Common Files\CLAP\Surge Synth Team\Surge XT.clap" --open-gui
```

## シーケンスエディタ

ウィンドウ下部のパネルでノートを打ち込み、シークバー付きで再生できる。

- **データモデル** (`sequencer.rs`): `Note { start_tick, duration, semitone, octave, velocity, lane }`。
  時間単位は四分音符 = 1.0。`MidiEditor { notes, tempo, beats, beat_type }`
- **段 (レーン) 方式**: デフォルト16段。ノートは置いた段に属する (段は音高と独立)。
  各ノートに "(半音,オクターブ)" 形式のラベルを表示。例: (0,4) = C4 相当
- **操作**: ダブルクリック = その段に配置 (選択中ノートのピッチと音価を引き継ぐ) /
  ドラッグ = 移動 (縦 = 段の移動) / 右端ドラッグ = 音価変更 / 右クリック = 削除 /
  **中クリックドラッグ = 表示のスクロール (縦横とも)** /
  **ノートを掴んだまま画面端に寄せると自動スクロール** (移動は縦横、音価変更は横のみ) /
  ルーラークリック・ドラッグ = シーク (四分音符と選択中ノートの頭にスナップ、Alt 押下で自由) /
  ピッチとベロシティは選択してツールバーで数値編集

> 中ドラッグの実装メモ: egui のドラッグ判定はボタンを問わない (`any_down`) ため、
> パン中は `middle_panning` フラグでノート編集のドラッグ処理を止めている。
> スクロールは `ScrollAnimation::none()` を使い、カーソルに即時追従させる。
>
> 自動スクロールの実装メモ: `DragState.origin` は画面座標ではなく**楽譜座標**
> (拍・段) で保持する。画面座標だと、自動スクロール中にカーソルを止めたときに
> 差分が変化せず、ノートがカーソルから置き去りになるため。グリッド原点は
> スクロールに追従して動くので、楽譜座標なら静止していても追従する。
- **ツールバー**: 再生/停止、ループ、テンポ、拍子 (beats / beat_type)、音階モード、
  スナップ (1/1〜1/32)、選択ノートの半音・オクターブ・ベロシティ

### 音階モード

ホストが変えるのは MIDI ノート番号だけで、実際の音高はプラグイン側の音律設定
(Scala の `.scl` ファイルなど) が決める。ノート番号は
`60 + (オクターブ - 4) × ステップ数 + 半音` で算出し、`(0,4)` が常に 60 (中央ハ) になる。

| モード | ステップ数 | 半音の範囲 | 例 |
|---|---|---|---|
| 12平均律 | 12 | 0..=11 | `(0,4)`→60 (C4) / `(9,4)`→69 (A4) |
| B-P 13音 | 13 | 0..=12 | `(0,4)`→60 / `(3,4)`→63 (平均律 E♭4 と同番号) / `(0,5)`→73 |

ボーレン・ピアースはトライターブ (3:1) を13等分する音階。`.scl` を読み込ませた
CLAP 音源と組み合わせて使う。モードを12平均律に戻すと、半音12のノートは11に丸められる。

### テーマ

[vim-hybrid](https://github.com/w0ng/vim-hybrid) 風のダークテーマを `theme.rs` で定義。
背景 `#1d1f21` / 文字 `#c5c8c6` を基調に、ノートの塗りはオクターブごとに
青→緑→黄→紫→シアン→赤のアクセント色を巡回させる。
- **再生エンジン** (`audio/transport.rs`): オーディオスレッド上でサンプル精度
  (ブロック内オフセット付き) のイベント発行。停止/シーク/シーケンス差し替え時は
  NoteChoke で鳴りっぱなしを防止。再生位置は AtomicU64 で GUI と共有

## 制限事項 (テスト用ホストのため)

- state 拡張 (プリセット保存/復元) 未対応
- シーケンスの保存/読込 (ファイル化) 未対応
- サンプル精度のイベントタイミングなし (全イベントがブロック先頭扱い)
- プラグインのアンロード UI なし (別のプラグインをロードすると前のものは破棄される)
- 出力はモノラル/ステレオのみ

## 既知の注意点

- clack は crates.io に安定版が出ていないため git 依存 (`Cargo.lock` でコミット固定)
- Surge XT など実プラグインは `C:\Program Files\Common Files\CLAP` 配下にインストールされる(Windows)
