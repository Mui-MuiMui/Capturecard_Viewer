# GUARDRAIL.md

このリポジトリで**してはいけないこと / 必ずすること**だけを集めた一覧。理由はここに書かず、参照先に置いてある。迷ったら参照先を読む。

`CLAUDE.md` から `@GUARDRAIL.md` で取り込まれ、常に読まれる。**短く保つこと。** 理由・経緯・実測値を書き足さない。

## スレッドとデバイス

- 映像フレームを `DeviceEvent` で送らない（理由: `docs/design/device-worker.md`）
- 時間で動くデバイス処理を `update()` から駆動しない（理由: `docs/design/device-worker.md`）
- `update()` の中でデバイスを開く・閉じる・列挙する・能力を問い合わせる処理を書かない（理由: `docs/design/device-worker.md`、`docs/ARCHITECTURE.md`）
- `AudioCapture` はワーカースレッドの中で作る（理由: `docs/design/device-worker.md`）
- ワーカー（`worker_loop` / `worker_connect` / `worker_timers`）から `VideoCapture` / `AudioCapture` を名指しで呼ばない。`app::backend` の trait を通す。コールバックの経路には trait を挟まない（理由: `docs/design/device-worker.md`）
- デバイスに触る使い捨てのスレッドを新しく作らない（理由: `docs/design/threads.md`）
- 新しく `Arc<Mutex<..>>` を足す前に「ワーカーへのコマンドで済まないか」を考える。要る場合もロックを握ったまま重い処理（デバイスの開き直し、画像のエンコード、ファイル I/O）をしない（理由: `docs/design/threads.md`）
- cpal のコールバックスレッドから再接続を始めない。エラーの旗はストリームを開き直すたびに新しい `Arc` へ差し替える（理由: `docs/design/threads.md`）
- ホットキーのリスナースレッドはアプリ全体で 1 本にする。`ListenerState` を別々のロックに分けない（理由: `docs/design/threads.md`）
- キーボードフックのコールバックでロックもアロケーションもしない。受け取ったキーは必ず `CallNextHookEx` で次へ渡す（理由: `docs/design/hotkeys.md`）
- スクリーンショットの保存スレッドの `JoinHandle` を捨てない。`on_exit` で join する（理由: `docs/design/threads.md`）
- クリップボードへのコピーを UI スレッドへ移さない。`arboard::Clipboard` は使うスレッドごとに作る（理由: `docs/design/threads.md`）
- 保存スレッドから直接 `error!` を出さない（理由: `docs/design/threads.md`）
- UI スレッドから `supported_input_configs()` / `supported_output_configs()` を呼ばない（理由: `docs/design/audio.md`）
- フレームコールバックと cpal のコールバックでロックもアロケーションもしない。`SharedColorConversion` に項目を足すときも `Mutex` にしない（理由: `docs/design/video-pipeline.md`、`docs/design/audio.md`）
- 接続を待つために `thread::sleep` を使わない（理由: `docs/design/reconnect.md`）
- 音声が開けないとき Windows の既定デバイスへフォールバックしない（理由: `docs/design/reconnect.md`）
- 切断の監視の中でデバイスを開かない。`ConnectRetry` へ要求を積むところまでにする（理由: `docs/design/reconnect.md`）
- 再接続の前にデバイスを列挙しない（理由: `docs/design/reconnect.md`）
- まだ 1 枚も届いていない状態を切断とみなさない（理由: `docs/design/reconnect.md`）
- 見送った音声のエラーを捨てない（理由: `docs/design/reconnect.md`）
- 映像の有無にかかわらず一定間隔で再描画を予約する形へ戻さない（理由: `docs/design/video-pipeline.md`）
- 16ms で回っている間と最小化中は `RepaintWaker` で起こさない（理由: `docs/design/video-pipeline.md`）
- 映像調整（明るさ / コントラスト / 彩度）を後段のフィルタとして足さない。係数表へ畳み込む（理由: `docs/design/video-pipeline.md`）
- フレームの世代番号をキャプチャ停止時に巻き戻さない（理由: `docs/design/video-pipeline.md`）

## 設定・設定ダイアログ・プリセット

- 設定構造体には**構造体レベル**の `#[serde(default)]` を付ける。既にあるものを外さない（理由: `docs/design/settings.md`）
- `AppSettings` に項目を足したら `RawAppSettings` と `From` にも足す（理由: `docs/design/hotkeys.md`）
- 設定を書き換える処理では `AppSettings::save()` を直接呼ばず `mark_settings_dirty()` を使う（理由: `docs/design/settings.md`）
- 自動保存の経路を足すときは `AutoSavePolicy::is_allowed()` を通す。起動経路の保存は `may_write_defaults_on_startup()` を通す（理由: `docs/design/settings.md`）
- `active_preset` は `AppSettings` の先頭、`Preset::name` は `video` / `audio` より前に置く（理由: `docs/design/presets.md`）
- プリセットに `screenshot` / `hotkeys` / `ui` を入れない。適用する項目と比較する項目を必ず揃える（理由: `docs/design/presets.md`）
- `commit_draft` の末尾の `refresh_active_preset()` を外さない（理由: `docs/design/presets.md`）
- `src/ui/` に `static` を追加しない。新しい状態は `SettingsDialogState` へ足し、描画側へは `SettingsDialogView` の読み取り専用の借用で渡す（理由: `docs/design/settings-dialog.md`）
- 描画関数へ共有設定の `Arc<Mutex<AppSettings>>` を渡さない。描画が書き換えてよいのはドラフトだけ（理由: `docs/design/settings-dialog.md`）
- `SettingsEvent` は受け取った順に処理する。`SettingsEvent::Dialog` は必ず列の最後（理由: `docs/design/settings-dialog.md`）
- `rfd` のファイルダイアログを描画の中から開かない。`settings` のロックを握ったまま出さない（理由: `docs/design/settings-dialog.md`）
- `Color32::YELLOW` のような固定色を直接書かない。`warning_label` / `notice_label` / `status_badge` を使う（理由: `docs/design/settings-dialog.md`）
- `awaiting_defaults` を読んだら必ず落とす要求を返す（理由: `docs/design/settings-dialog.md`）
- `commit_draft` が反映する項目を増減させたら `draft_from_imported` も合わせる（理由: `docs/design/settings-dialog.md`）
- 描画中にデバイスへ問い合わせない（理由: `docs/design/error-reporting.md`）

## ホットキーとウィンドウ

- `HotkeyManager::apply` を直接呼ばない。呼ぶのは `apply_hotkey_assignments` だけ（理由: `docs/design/hotkeys.md`）
- `HotkeyAction::as_str()` の文字列は、一度出した名前を変えない（理由: `docs/design/hotkeys.md`）
- ホットキーのアクションは右クリックメニューと同じメソッドを呼ぶ。独自に書かない（理由: `docs/design/hotkeys.md`）
- ホットキーのリスナースレッドから `CaptureCardViewer` の状態を触らない。最小化中に実行するのは `HotkeyAction::runs_while_minimized()` が真のものだけで、経路はワーカーへの `DeviceCommand`（理由: `docs/design/hotkeys.md`）
- `ui.enable_drag_move` が切れている状態で装飾（`ui.borderless`）を外させない（理由: `docs/design/window.md`）
- 生成後に `ViewportCommand::Decorations` で装飾を外さない。起動時は `ViewportBuilder::with_decorations` で決める（理由: `docs/design/window.md`）

## ログ・失敗の扱い・コードの置き場所

- `println!` / `eprintln!` を新たに足さない（例外は `#[cfg(test)]` の中）（理由: `docs/design/logging.md`）
- `catch_unwind` を使わない（理由: `docs/design/logging.md`）
- 失敗はログだけで終わらせず `report_error(ErrorSource::_, 理由)` を呼ぶ。接続に成功したら `errors.clear(..)` を呼ぶ（理由: `docs/design/error-reporting.md`）
- `video/` / `audio/` / `screenshot.rs` / `hotkey/` / `settings.rs` の公開 API は `String` ではなく自分のエラー enum を返す（理由: `docs/design/error-reporting.md`）
- エラー enum の日本語の文言はその型の `Display` に書く。`status.rs` に発生源ごとの `match` を足さない（理由: `docs/design/error-reporting.md`）
- カレントディレクトリ基準でファイルを解決する処理を新たに足さない（理由: `docs/design/assets.md`）
- `src/app/` の子モジュールにフィールドや `static` を持たせない（理由: `CLAUDE.md` の「モジュール構成」）
- `src/app/menu/items.rs` の描画関数から状態を書き換えない。起きたことは `MenuAction` で返す（理由: `CLAUDE.md` の「モジュール構成」）
- 1 ファイル 800 行以内を目安にする（理由: `CLAUDE.md` の「モジュール構成」）
- テストは対象と同じファイルの末尾の `#[cfg(test)] mod tests` に置く。**1 ファイルに `mod tests` は 1 つだけ**（理由: `.claude/skills/testing-conventions/SKILL.md`）
- コードコメント・UI 文字列・コミットメッセージは日本語で書く（理由: `CLAUDE.md` の「コーディング規約」）

## Git と Issue

- `Closes` / `Fixes` などのクローズ用キーワードを使わない。Issue 番号の前に置いてよいのは `Refs` だけ（理由: `.claude/skills/naming-conventions/SKILL.md` の「`Closes` ではなく `Refs` を使う」）
- Issue を閉じるのは人が実機で確認したとき。Claude は閉じない（理由: 同上）
- 進行状況は Project の Status だけで管理する。ラベルでは表さない（理由: `CLAUDE.md` の「タスク管理」）
- 各コミットはビルドとテストが通る状態にする。push 済みの履歴を force push で作り直さない（理由: `.claude/skills/naming-conventions/SKILL.md`）
- 設計判断や方針は `git log` ではなく `CLAUDE.md` / `GUARDRAIL.md` / `docs/design/` に書く（理由: `.claude/skills/naming-conventions/SKILL.md`）
- `CHANGELOG.md` に書くのはユーザーから見える変更だけ（新しいドキュメントは「増えるもの」として書く）（理由: `CONTRIBUTING.md` の「CHANGELOG」）
- バージョン番号を `Cargo.toml` の `version` 以外に書かない。通常の PR で `version` を触らない（理由: `docs/BUILD.md`、`docs/RELEASE.md`）
