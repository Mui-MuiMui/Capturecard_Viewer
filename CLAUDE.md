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
- バージョン番号の出どころは `Cargo.toml` の `version` だけ。`build.rs` が `app.rc` 用のヘッダーを生成するので、他の場所に数値を書かない（`docs/BUILD.md` の「バージョン番号」）
- `cargo clippy --all-targets` は警告ゼロが前提。警告を増やしたままコミットしない
- `cargo fmt --check` は差分ゼロが前提。落ちたら自分の変更を `cargo fmt` で整形してからコミットする
- 整形の基準はリポジトリ直下の `rustfmt.toml`。`edition` だけ指定し、他は rustfmt の既定値に従う

## モジュール構成

| ファイル | 役割 |
|---|---|
| `src/main.rs` | アプリ状態 `CaptureCardViewer`、`eframe::App` 実装、映像描画、コンテキストメニュー、デバイス接続の適用とリトライ、スクリーンショット処理、エントリポイント |
| `src/video.rs` | nokhwa `CallbackCamera` によるキャプチャ、YUY2→RGB 変換、`FrameBuffer`（`Arc` によるフレーム共有と世代番号）、デバイス能力の取得 |
| `src/audio.rs` | cpal による入力→リングバッファ→出力のパススルー、音量制御 |
| `src/screenshot.rs` | global-hotkey によるグローバルホットキー登録とリスナースレッド、rodio による効果音再生 |
| `src/settings.rs` | `AppSettings` とその serde 定義、confy による読み書き、保存パスの決定 |
| `src/ui.rs` | 設定ダイアログとホットキー設定ダイアログの描画 |

### 映像パイプライン

キャプチャーデバイス → nokhwa `Buffer` → フレームコールバックで YUY2→RGB 変換 → `FrameBuffer` → `update_video_texture` で egui テクスチャ化 → 描画

`FrameBuffer` はフレームを `Arc<VideoFrame>` で保持し、取り出し側へは `Arc` の複製を渡す。**画素データを複製しないので、取り出しても 1080p で 6MB の memcpy は発生しない。**

`FrameBuffer` は push のたびに進む世代番号を持つ。`update_video_texture` は `get_frame_if_newer` で前回反映した世代と比較し、新着が無ければテクスチャを更新しない。**新着の有無を問わず最後のフレームが要る用途（スクリーンショット）は `get_latest_frame` を使う。** 世代番号はキャプチャ停止時も巻き戻さない。巻き戻すと再接続後の最初のフレームが呼び出し側の記録と一致し、新着と判別できなくなる。

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

### 設定構造体の `#[serde(default)]` を外さない

`AppSettings` と配下の 4 構造体には、構造体レベルで `#[serde(default)]` が付いている。これが無いと、項目を 1 つ足すだけで既存ユーザーの設定が失われる。`Option` 以外の項目はパース自体が失敗して全項目が初期化され、`Option` の項目は `None` になって `Default` に書いた既定値が効かなくなる。**設定の構造体を新しく足すときも必ず付けること。**

読み込みに失敗した場合、`AppSettings::load()` は壊れたファイルの `.bak` への退避を試みてから既定値で起動する。退避は失敗することがある（保存先のパスが取れない、rename が拒否される）。失敗の理由はまだどこにも残らない（ログ基盤が未導入のため）。

`load()` は `(AppSettings, LoadOutcome)` を返す。`LoadOutcome` は退避まで含めて成功したかを表し、**退避できなかった場合に起動時の書き戻しを止めるためにある。** 退避に失敗すると読めなかったファイルがディスクに残るので、そこへ既定値を `save()` すると証跡ごと潰れる。`CaptureCardViewer::default` の起動時保存は `may_write_defaults_on_startup()` で守ってあるので、**起動経路に `save()` を足すときは同じ判断を通すこと。** 設定ダイアログからの明示的な保存は、ユーザーの意思なので抑止していない。

### 設定の保存はデバウンスされる

ウィンドウの移動・リサイズ、音量スクロール、コンテキストメニューの各操作は、その場ではディスクへ書かない。`mark_settings_dirty()` で変更を記録し、`update()` の末尾の `flush_settings_if_due()` が最後の変更から 2 秒空いたところでまとめて書き出す。終了時は `on_exit` が保留の有無にかかわらず必ず書き出す。

**設定を書き換える処理を足すときは `AppSettings::save()` を直接呼ばず `mark_settings_dirty()` を使うこと。** 直接呼ぶと、ウィンドウをドラッグしている間ずっと毎フレーム TOML を書き出す元の問題に戻る。

例外は 2 つ。起動時の書き戻し（`may_write_defaults_on_startup()` で守られている）と、設定ダイアログの OK ボタン（`main.rs` の `handle_settings_dialog_action`）。後者はユーザーの明示的な保存操作なので即座に書き出す。

### 設定ダイアログはドラフトを編集する

設定ダイアログは共有の `AppSettings` を直接書き換えない。開いたときに複製した `SettingsDialogState` のドラフトを編集し、実行中の設定へ移るのは「適用」と「OK」のときだけ。閉じるときは必ずドラフトを捨てる。

| 操作 | 反映 | ファイルへ保存 | 閉じる |
|---|---|---|---|
| 適用 | する | しない | しない |
| OK | する | する | する |
| キャンセル | しない | しない | する |
| ×（タイトルバー） | しない | しない | する（キャンセルと同じ） |

- × は `egui::Window::open()` が `show_settings` を false にするだけでボタンが押されないため、描画後の開閉状態から `ui::resolve_action` が拾ってキャンセルへ倒している
- **キャンセルは「適用」で反映済みの内容を戻さない。** 戻すには反映前の状態をもう 1 つ持つ必要があり、デバイスの開き直しも 2 度走る
- 反映は `ui::commit_draft` が `video` / `audio` / `screenshot` と、ダイアログが編集する `ui` の 2 項目（`maintain_aspect_ratio` / `volume`）だけに限っている。`ui` を丸ごと入れると、ダイアログを開いている間に動かしたウィンドウの位置が巻き戻る。**ダイアログに `ui` の項目を足すときは `commit_draft` にも足すこと**
- ホットキー入力ダイアログと効果音のテスト再生もドラフトを見る。ドラフトへ書いたホットキーはその場で登録しない（2 秒ごとの `apply_settings` が共有設定側の古い値で登録し直してしまうため）

`ui.rs` にはまだタブ選択・デバイス能力キャッシュ・ホットキー入力の `static` が残っている。これらは別タスクで構造体へ移す。

### UI にあるが動作していない設定がある

以下は設定画面から変更できるが実装が追いついていない。README の記述もこれらを前提に書かれているため、修正時は README も合わせて更新すること。

- ビデオフォーマットの MJPEG / RGB24（内部で YUYV に強制される）

オーディオのサンプリングレート／チャンネル数は `select_best_config` でストリームに反映されるようになった。ただし**選択肢はデバイスの能力から生成していないため、デバイスが対応していない値を選べてしまう。** その場合は対応する中で最も近い値が使われる。特に WASAPI はミックスフォーマットのチャンネル数しか列挙しないので、モノラルを選んでもステレオで開かれることが多い。

### アセットは exe に埋め込んである

`icon.ico` と既定の効果音 `sound/SS.mp3` は `include_bytes!` で実行ファイルに埋め込んである。ファイルとして読みに行かないため、カレントディレクトリに左右されない。

設定に保存された効果音のパスだけは外部のファイルを読む。相対パスは `current_exe()` の親ディレクトリ基準で解決し、見つからなければ埋め込みの既定音へ倒す（`screenshot::resolve_sound_path`）。**カレントディレクトリ基準でファイルを解決する処理を新たに足さないこと。**

## コーディング規約

- コードコメント、UI 文字列、コミットメッセージは日本語
- 既存の命名（snake_case、モジュール構成）に合わせる
- 実装とコメントが食い違っている箇所が複数あるので、コメントを鵜呑みにせず実コードを確認すること

ブランチ名・コミットメッセージ・PR の書き方は `.claude/skills/naming-conventions/SKILL.md` にまとめてある。ブランチを切る前、コミットする前、PR を作る前に参照すること。

## テスト

テストの方針は `.claude/skills/testing-conventions/SKILL.md` にまとめてある。デバイス依存が強いため、一般的な 3 層ではなく「CI で自動実行できるか」で区分している。

実機でしか確認できない項目は `docs/MANUAL-TEST.md` のチェックリストで担保する。既知の不具合で現在失敗する項目も同ファイルに明記してあるので、不具合を直したらチェックリスト側も更新すること。

## タスク管理

改善バックログは Asana プロジェクト「Capturecard_Viewer」で管理している。
https://app.asana.com/1/1218412078016612/project/1218457296782693/list

- セクションは 開発基盤・CI / ドキュメント整備 / バグ修正 / パフォーマンス改善 / リファクタリング / 機能拡充 / リリース・保守 の 7 つ
- 各タスクの説明冒頭に `[P1]`〜`[P3]` の優先度、本文に `file:line` 形式で該当箇所を記載
- PR を作るときは説明に対応する Asana タスクの URL を書き、Asana タスク側にも PR の URL をコメントすること

## 開発フロー

計画 → 実装 → レビュー → PR 作成 の 4 段階を slash command にしてある。各段階の終わりに人の判断が入るゲートとして機能する。

| コマンド | 段階 |
|---|---|
| `/cv:plan <Asana タスク URL または説明>` | 計画。コードは書かず、方針を提示して承認を待つ |
| `/cv:implement` | 承認済みの計画に従って worktree を切り実装する。まとまった単位でコミットする |
| `/cv:review` | 検証コマンドを走らせ、観点に沿ってセルフレビューする |
| `/cv:pr` | push、PR 作成、Asana との相互リンク。レビュー指摘への対応にも使う |

`cv:` の名前空間を付けているのは、`/plan` や `/review` が組み込みコマンドや他のプラグインと衝突するのを避けるため。

コマンドは「やること」だけを持ち、書式や基準は skill を参照する。**同じ内容を両方に書かない。** 片方を直したときにもう片方が古くなるため。

### ブランチとコミット

`<type>/<説明>` の作業ブランチ → `dev` → `main`。**PR のマージ先は `dev`。** `main` へ入れるのはリリースのときだけ。

コミットは作業ログとして扱い、まとまった単位でどんどん積む。ただし**各コミットはビルドとテストが通る状態にする**。レビュー指摘への対応は元のコミットを直さず追加のコミットで積み、push 済みの履歴を force push で作り直さない。

詳細は `.claude/skills/naming-conventions/SKILL.md` を参照。

**設計判断や方針は履歴ではなく `CLAUDE.md` か `docs/` に書くこと。** 新しいセッションで読まれるのはこれらであって `git log` ではない。

