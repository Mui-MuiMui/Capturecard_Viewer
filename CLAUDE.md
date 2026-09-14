# CLAUDE.md

このファイルは Claude Code がこのリポジトリで作業するときの手引きです。

## プロジェクト概要

キャプチャーボード（キャプチャーカード）の映像と音声を、低遅延・シンプルな画面で表示する Windows 10/11 専用アプリ。Rust + eframe/egui 製の単一バイナリ。

- キャプチャーデバイスは Windows Media Foundation 経由で Web カメラとして扱う（nokhwa）
- 音声は WASAPI 経由の入力 → リングバッファ → 出力のパススルー（cpal）
- 設定は `%AppData%\capturecard_viewer\config\default-config.toml`（confy）

## ビルドと検証

```bash
cargo build --release
```

```bash
cargo fmt --check && cargo clippy --all-targets && cargo test
```

- ビルドには MSVC ツールチェインと Windows SDK が必要（`build.rs` が `embed_resource` で `app.rc` をコンパイルするため）
- `cargo clippy --all-targets` はクリーンではなく、既知の警告が残っている（詳細は Asana のタスク参照）
- `cargo fmt --check` は現状リポジトリ全体で差分を出す。整形は独立したコミットで行うこと

## モジュール構成

| ファイル | 役割 |
|---|---|
| `src/main.rs` | アプリ状態 `CaptureCardViewer`、`eframe::App` 実装、映像描画、コンテキストメニュー、デバイス接続の適用とリトライ、スクリーンショット処理、エントリポイント |
| `src/video.rs` | nokhwa `CallbackCamera` によるキャプチャ、YUY2→RGB 変換、`FrameBuffer`（ダブルバッファ）、デバイス能力の取得 |
| `src/audio.rs` | cpal による入力→リングバッファ→出力のパススルー、音量制御 |
| `src/screenshot.rs` | global-hotkey によるグローバルホットキー登録とリスナースレッド、rodio による効果音再生 |
| `src/settings.rs` | `AppSettings` とその serde 定義、confy による読み書き、保存パスの決定 |
| `src/ui.rs` | 設定ダイアログとホットキー設定ダイアログの描画 |

### 映像パイプライン

キャプチャーデバイス → nokhwa `Buffer` → フレームコールバックで YUY2→RGB 変換 → `FrameBuffer` → `update_video_texture` で egui テクスチャ化 → 描画

### スレッド構成

- egui/eframe の UI スレッド（`update()` が毎フレーム呼ばれる。ここが全ての起点）
- nokhwa のフレームコールバックスレッド
- cpal の入力コールバック／出力コールバックスレッド
- ホットキーリスナースレッド（`set_hotkey` のたびに再生成される）
- 効果音再生スレッド（再生ごとに spawn）

### ロック順序

`Arc<Mutex<..>>` を 4 つ持つ（`settings` / `video_capture` / `audio_capture` / `screenshot_manager`）。
現状コード内でロック順序が統一されておらず、`apply_settings` は settings → video → audio → screenshot の順、`take_screenshot` は video → settings → screenshot の順になっている。いまは全て UI スレッドからのみ呼ばれるため顕在化しないが、**処理を別スレッドへ逃がす変更を入れるときは必ずロック順序を settings → video → audio → screenshot に揃えること**。

## 作業時の注意点

### ログが見えない

`src/main.rs` 冒頭に `#![windows_subsystem = "windows"]` があるためコンソールが存在せず、コード中の `println!` / `eprintln!`（86 箇所）の出力はどこにも届かない。デバッグ目的で `println!` を足しても無意味なので、ログが必要な場合はファイル出力の仕組みを入れること。

### catch_unwind は機能しない

`Cargo.toml` の `[profile.release]` に `panic = "abort"` があるため、`main.rs` 内の `std::panic::catch_unwind` は release ビルドで一切機能しない。

### 設定ファイルの読み込みは全か無か

`AppSettings::load()` は `confy::load(..).unwrap_or_default()` なので、**構造体にフィールドを 1 つ足すと既存ユーザーの設定が丸ごと初期化される**。設定構造体を変更する場合は `#[serde(default)]` を必ず付けること。

### UI にあるが動作していない設定がある

以下は設定画面から変更できるが実装が追いついていない。README の記述もこれらを前提に書かれているため、修正時は README も合わせて更新すること。

- 音声パススルーの有効/無効（フラグがストリーム側から参照されていない）
- オーディオのサンプリングレート／チャンネル数（`start_passthrough_with_settings` の引数が未使用）
- ビデオフォーマットの MJPEG / RGB24（内部で YUYV に強制される）

### 相対パス依存

`icon.ico` と既定の効果音 `sound/SS.mp3` はカレントディレクトリ基準で解決される。exe の場所と CWD が異なると読み込みに失敗する。

## コーディング規約

- コードコメント、UI 文字列、コミットメッセージは日本語
- 既存の命名（snake_case、モジュール構成）に合わせる
- 実装とコメントが食い違っている箇所が複数あるので、コメントを鵜呑みにせず実コードを確認すること

## タスク管理

改善バックログは Asana プロジェクト「Capturecard_Viewer」で管理している。
https://app.asana.com/1/1218412078016612/project/1218457296782693/list

- セクションは 開発基盤・CI / ドキュメント整備 / バグ修正 / パフォーマンス改善 / リファクタリング / 機能拡充 / リリース・保守 の 7 つ
- 各タスクの説明冒頭に `[P1]`〜`[P3]` の優先度、本文に `file:line` 形式で該当箇所を記載
- PR を作るときは説明に対応する Asana タスクの URL を書き、Asana タスク側にも PR の URL をコメントすること
