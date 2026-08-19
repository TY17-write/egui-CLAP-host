# egui-CLAP-host

Rust + egui で CLAP (CLever Audio Plug-in) と VST3 をロードして鳴らす、マルチトラックのホスト。
オーディオトラックごとに音源とエフェクトを重ね、トラック同士を自由に繋いで鳴らせる。
CLAP と VST3 は同じチェーンの中で混在でき、どちらも音源独自のエディタを開ける。

> **このリポジトリのコードは Claude Opus 5 (Anthropic) が作成しました。**
> 設計方針の決定・仕様の判断・動作確認は人間が行い、実装とドキュメントは対話しながら生成しています。
> 学習・検証を目的としたテスト用ホストであり、製品版 DAW のような堅牢さはありません。

## できること

- **CLAP / VST3 の音源をトラックごとに読み込む** (混在可・音源独自のエディタも開ける)
- **ピアノロールでの打ち込み** — 範囲選択・移動・伸縮・移調・連符、縦横のズーム
- **CC 段** — ペダルなどを「書いた区間だけ効く」形で書ける
- **スウィング** — 研究に基づく裏拍の比と表拍の遅れを、再生と書き出しにだけ乗せる
- **不均等な拍** — 3/5/7拍子で拍の長さ自体を変える (ウィンナ・ワルツ風。スウィングと併用可)
- **12平均律 / ボーレン・ピアース13音**の切り替え
- **マスターメーター** — スペクトルと LUFS (BS.1770-4 の M / S / Integrated。基準 −14 LUFS)
- **保存と書き出し** — `.ron` (音源の音作りごと) / MIDI (入出力) / WAV / Ogg-Opus / CeVIO (.ccs)

**操作と機能の詳細、内部の作り、できないことは [docs/guide.md](docs/guide.md) にまとめてあります。**

## 使っているもの

CLAP の扱いには **[clack](https://github.com/prokopyl/clack)** (MIT OR Apache-2.0) を使っています。
CLAP の C API を安全な Rust API で包み、**main-thread / audio-thread のスレッド規約を型システムで
担保**してくれるクレートです。crates.io に安定版が出ていないため git 依存とし、`Cargo.lock` で
コミットを固定しています。

**`vst3-host` も crates.io 版ではなくフォークを使っています。** エディタを開く経路と
単体ファイル形式の読み込みに、手元の音源の多くが通らない箇所があったためです
(内訳は [docs/vst3_host_plan.md](docs/vst3_host_plan.md) のフェーズ6)。

| クレート | 用途 | ライセンス |
|---|---|---|
| [clack](https://github.com/prokopyl/clack) | CLAP ホスト/プラグイン実装 | MIT OR Apache-2.0 |
| [vst3-host](https://github.com/TY17-write/rust-vst3-host) (フォーク) | VST3 ホスト実装 | MIT |
| cpal | オーディオ出力 | Apache-2.0 |
| eframe / egui | GUI | MIT OR Apache-2.0 |
| midly | 標準 MIDI ファイルの読み書き | Unlicense |
| quick-xml | CeVIO (.ccs) の書き出し | MIT |
| uuid | .ccs のパート識別子 | MIT OR Apache-2.0 |
| [opus-rs](https://crates.io/crates/opus_rs) | Opus の符号化 (純 Rust) | BSD-3-Clause |
| [ogg](https://crates.io/crates/ogg) | Ogg 容器 (純 Rust) | BSD-3-Clause |
| serde / ron | プロジェクトファイル (.ron) の読み書き | MIT OR Apache-2.0 |
| rtrb | オーディオスレッドとのリングバッファ | MIT OR Apache-2.0 |
| crossbeam-channel | プラグインからのメインスレッド要求 | MIT OR Apache-2.0 |
| rfd | ファイルダイアログ | MIT |
| windows-sys | プラグイン GUI の埋め込み (Win32) | MIT OR Apache-2.0 |

`host/src/audio/buffers.rs` は clack リポジトリの cpal サンプルをほぼそのまま利用しています
(MIT OR Apache-2.0)。

VST3 は **2025年10月20日に SDK 3.8.0 が MIT ライセンスで公開された**ため、このプロジェクトを
MIT のまま VST3 対応にできています (`vst3-host` → `vst3` → SDK の連鎖がすべて MIT 系)。
残るのは商標の扱いのみで、コードのライセンスとは別の話です。

## 構成

| クレート | 内容 |
|---|---|
| `host` | ホスト本体 (egui GUI + cpal オーディオ出力 + clack-host + vst3-host)。パッケージ名は `egui-clap-host` |
| `test-plugin` | テスト用のサイン波シンセ CLAP プラグイン (16ボイス、Volume パラメータ、ベロシティ対応) |
| `spike/vst3` | VST3 に進めるかを確かめた実験用クレート (親のワークスペースからは切り離してある) |
| `spike/opus` | Ogg/Opus の書き出しを確かめた実験用クレート (同上)。`opus-rs` の不具合を測って上流へ報告した検証コードも置いてある |

`host/src/bin/` にはオーディオデバイス不要の検証バイナリが並びます。
`test-plugin` は **CLAP 専用**なので、VST3 側の検証には実物の `.vst3` が要ります
([docs/guide.md](docs/guide.md#起動する) に一覧と、CLAP 版から VST3 のテスト治具を作る手順)。

## ビルドと実行

Windows で開発・確認しています (他プラットフォームは未検証)。

```powershell
cargo build --workspace

# .clap は DLL のリネーム (先に消してからコピーすること)。
# 置き場は target\ 直下 — target\debug\ だと cargo clean -p で消える
Remove-Item target\test_plugin.clap -ErrorAction SilentlyContinue
Copy-Item target\debug\test_plugin.dll target\test_plugin.clap

# GUI ホストを起動 (--bin の指定が必要。検証用バイナリも同居しているため)
cargo run -p egui-clap-host --bin egui-clap-host -- target\test_plugin.clap

# ユニットテスト
cargo test -p egui-clap-host --lib
```

引数なしでも起動でき、あとから画面上で音源を読み込めます。

## ドキュメント

| | 内容 |
|---|---|
| [docs/guide.md](docs/guide.md) | **使い方・機能の詳細・アーキテクチャ・制限事項** |
| [docs/vst3_host_plan.md](docs/vst3_host_plan.md) | VST3 対応の設計と実測 (フォークした理由、エディタまわりの調査) |
| [docs/export_rate_plan.md](docs/export_rate_plan.md) | Opus 書き出しの設計と、ビットレートを絞った理由 |
| [docs/routing_plan.md](docs/routing_plan.md) | 音声ルーティングの設計 (オーディオトラックと繋ぎ方) |
| [docs/library_plan.md](docs/library_plan.md) | プラグイン一覧 (フォルダ走査・分類・お気に入り) の設計 |
| [docs/swing-plan.md](docs/swing-plan.md) | スウィングの実装計画 |
| [docs/waltz-plan.md](docs/waltz-plan.md) | 不均等な拍 (ウィンナ・ワルツ風) の実装計画 |

## ライセンス

[MIT License](LICENSE)

依存クレートはそれぞれのライセンスに従います (上表を参照)。CLAP 規格自体は MIT です。
