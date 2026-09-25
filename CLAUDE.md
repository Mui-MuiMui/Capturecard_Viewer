# CLAUDE.md

このファイルは Claude Code がこのリポジトリで作業するときの手引きです。**してはいけないこと / 必ずすることは `GUARDRAIL.md` にまとめてある。** 下の取り込みで常に読まれるので、ここには再掲しない。

@GUARDRAIL.md

## プロジェクト概要

キャプチャーボード（キャプチャーカード）の映像と音声を、低遅延・シンプルな画面で表示する Windows 10/11 専用アプリ。Rust + eframe/egui 製の単一バイナリ。

- キャプチャーデバイスは Windows Media Foundation 経由で Web カメラとして扱う（nokhwa）
- 音声は WASAPI 経由の入力 → リングバッファ → 出力のパススルー（cpal）
- 設定は `%AppData%\capturecard_viewer\config\default-config.toml`（confy）

## ビルドと検証

```bash
cargo build --release
```

- ビルドには MSVC ツールチェインと Windows SDK が必要（`build.rs` が `embed_resource` で `app.rc` をコンパイルするため）
- バージョン番号の出どころは `Cargo.toml` の `version` だけ。`build.rs` が `app.rc` 用のヘッダーを生成するので、他の場所に数値を書かない（`docs/BUILD.md` の「バージョン番号」）
- **検証は `.claude/skills/verify/SKILL.md` の手順で回す。** fmt → clippy → release ビルド → test を CI と同じ引数で通す。ここにコマンドを再掲しない
- 整形の基準はリポジトリ直下の `rustfmt.toml`。`edition` だけ指定し、他は rustfmt の既定値に従う

## モジュール構成

| ファイル | 役割 |
|---|---|
| `src/main.rs` | エントリポイント。ロガーの初期化、`NativeOptions` の組み立て、`run_native` だけ |
| `src/platform.rs` | Windows 固有処理。日本語フォントの探索、埋め込みアイコンの読み込み、モニタの作業領域の列挙、保存されたウィンドウの大きさ・位置が使えるかの判定、OS の表示言語からの言語の推定 |
| `src/app/mod.rs` | アプリ状態 `CaptureCardViewer` の定義、`Default`、`eframe::App` 実装（`update` / `on_exit`） |
| `src/app/view.rs` | 映像の描画（ウィンドウ表示とフルスクリーン）、プレースホルダーの文言、統計 OSD、テクスチャの取り込み |
| `src/app/menu/mod.rs` | 右クリックメニューの置き場所と閉じ方、平らな一覧／サブメニューの出し分け、描画が返した `MenuAction` の処理 |
| `src/app/menu/items.rs` | 右クリックメニューの項目の描画。**状態を持たず、書き換えもしない。** 起きたことは `MenuAction` の列で返す |
| `src/app/window.rs` | 最前面表示、タイトルバーの有無、装飾なしのときの端のドラッグによるリサイズ、大きさのリセット、フルスクリーンの切り替え |
| `src/app/device.rs` | `apply_settings`（設定をワーカーへ渡す）と、ワーカーから届いたイベントの取り込み |
| `src/app/worker.rs` | デバイスワーカーとやり取りする型（コマンド / イベント / `DeviceConfig` / `DeviceSnapshot`）と、UI 側の窓口 `DeviceWorker` |
| `src/app/worker_loop.rs` | デバイスワーカースレッドの本体。`WorkerState` の定義、コマンドの受け口、待ち時間の決定、観測値の書き出し |
| `src/app/worker_timers.rs` | ワーカーがタイマーで回す監視。再試行の期限、フレームの途絶、音声ストリームのエラー、既定デバイスの切り替え、クロックドリフト補正 |
| `src/app/worker_connect.rs` | ワーカーが行うデバイス操作。開く・閉じる・列挙する・能力を問い合わせる |
| `src/app/backend/mod.rs` | ワーカーがデバイスに触るときの入口の trait（`VideoBackend` / `AudioBackend` / `DeviceBackends`）と、本番かフェイクかを環境変数で選ぶ `backends_from_env`。テスト用のモックもここ（`#[cfg(test)]`） |
| `src/app/backend/system.rs` | 本番のバックエンド `SystemBackends`。`VideoCapture` / `AudioCapture` を trait に載せる |
| `src/app/backend/fake.rs` | フェイクのバックエンド `FakeBackends`。`FakeVideoCapture` / `FakeAudioCapture` を trait に載せる実装と、環境変数（`CAPTURECARD_VIEWER_FAKE_DEVICES` / `CAPTURECARD_VIEWER_FAKE_SCENARIO`）の解釈 |
| `src/app/monitor.rs` | 切断や既定デバイスの切り替えの**判定**（純粋関数）。ワーカーが使う |
| `src/app/retry.rs` | `ConnectRetry` とバックオフ。「いつ試してよいか」だけを持つ。ワーカーが持つ |
| `src/app/capabilities.rs` | デバイス一覧のキャッシュと、デバイス能力・対応設定の取得要求（ワーカーへ流すところまで） |
| `src/app/screenshot.rs` | 撮影、保存スレッドの管理、結果の取り込み |
| `src/app/screenshot_sound.rs` | 効果音ファイルの読み込みスレッドの管理と結果の取り込み（適用・テスト再生） |
| `src/app/settings_dialog.rs` | 設定ダイアログの操作の受け止め、インポート / エクスポート / 初期化、プリセットの適用 |
| `src/app/settings_store.rs` | 設定のデバウンス保存と即時保存 |
| `src/app/hotkeys.rs` | ホットキーの適用と、押されたときのアクションの実行 |
| `src/app/audio_control.rs` | 音量とミュートの操作、その OSD |
| `src/app/error_report.rs` | 失敗の記録と、トースト・「接続状態」タブへの出し方 |
| `src/video/mod.rs` | `VideoError` とログ用の `elapsed_ms`。外から使う経路（`crate::video::...`）の `pub use` もここ |
| `src/video/capture.rs` | nokhwa `CallbackCamera` によるキャプチャ。開く・閉じる・列挙する、フレームコールバック（nokhwa の `Buffer` から取り出して `FrameSink` へ渡す）、途絶の観測（`VideoLinkState`） |
| `src/video/frame_sink.rs` | フレームコールバックの本体 `FrameSink`（YUY2→RGB、`FrameBuffer` へ積む、`RepaintWaker` で UI を起こす）。実機とフェイクで共有する |
| `src/video/fake.rs` | 実機なしで動くフェイクの映像デバイス `FakeVideoCapture`。テストパターンを指定 fps で吐く生成スレッド、切断・接続失敗のシナリオ |
| `src/video/test_pattern.rs` | フェイクが吐くテストパターン（カラーバー、ベタ塗り、フレーム番号の焼き込み）の描画。純粋関数 |
| `src/video/capabilities.rs` | `VideoMode` / `FormatCapability` と、デバイス能力の取得 |
| `src/video/color.rs` | YCbCr→RGB の係数表とその選び方、映像調整の畳み込み、設定の共有（`SharedColorConversion`） |
| `src/video/convert.rs` | YUY2→RGB24 の画素変換 |
| `src/video/frame_buffer.rs` | `FrameBuffer`（`Arc` によるフレーム共有と世代番号）と観測値（`FrameStats`） |
| `src/audio/mod.rs` | 音声モジュールの入口。`ActiveAudio` / `AudioDirection` / `AudioError` と能力キャッシュのキー（`cache_key` / `device_name_from_key`）、外から使う経路（`crate::audio::...`）の `pub use` |
| `src/audio/capabilities.rs` | デバイスの対応設定の取得（`query_capabilities`）と、設定画面に出す選択肢の組み立て（`selectable_*` / `ChoiceSource`） |
| `src/audio/stream_config.rs` | 対応設定の中から実際に開く設定を選ぶ（`select_best_config` / `select_aligned_configs`）。扱えるサンプル型の一覧もここ |
| `src/audio/capture.rs` | `AudioCapture`。パススルーの開始と停止、観測値（実際に開いた内容・アンダーラン・リサンプル）の取り出し |
| `src/audio/stream.rs` | cpal のストリームの組み立てと入出力のコールバック（本体は `process_input` / `process_output` で、フェイクと共有する）、リングバッファの型、アンダーランの数え方 |
| `src/audio/convert.rs` | サンプル型の変換（f32 ⇄ i16 / u16 / i32）と、レート・チャンネル数が違う場合の変換（`PassthroughConverter`） |
| `src/audio/resample.rs` | クロックドリフト補正の共有状態（`ResampleTelemetry`）と補正係数の決め方（`decide_resample_correction`） |
| `src/audio/controls.rs` | `AudioControls`。音量・パススルー・ミュートの共有状態 |
| `src/audio/fake.rs` | 実機なしで動くフェイクの音声デバイス `FakeAudioCapture`。正弦波の入力と書き込みを捨てる出力のスレッド |
| `src/hotkey/mod.rs` | 外から使う経路（`crate::hotkey::...`）の `pub use` だけ |
| `src/hotkey/action.rs` | `HotkeyAction`（ホットキーを割り当てられる操作）と設定ファイル上の名前、溜まった押下の畳み方 |
| `src/hotkey/parse.rs` | `HotkeyError` と、ホットキー文字列のパース |
| `src/hotkey/manager.rs` | `HotkeyManager` の本体（リスナーの起動と停止、ウィンドウ状態の受け渡し）と `BackgroundHotkeyRunner` |
| `src/hotkey/assignments.rs` | `HotkeyAssignmentError`、アクション別の登録（差分適用・一時停止と再開・試し登録）と押下の取り出し |
| `src/hotkey/listener.rs` | リスナースレッドと共有状態 `ListenerState`、押下の照合とデバウンス |
| `src/keyboard_hook.rs` | 低レベルキーボードフック（`WH_KEYBOARD_LL`）。キーを奪わずに押下を観測し、リスナースレッドのメッセージループへ渡す |
| `src/screenshot.rs` | rodio による効果音の読み込みと再生 |
| `src/settings.rs` | `AppSettings` とその serde 定義、confy による読み書き、保存パスの決定、旧形式からの移行 |
| `src/logging.rs` | `log` クレートのロガー実装。ログファイルの置き場所・命名・世代管理、レベルの決定 |
| `src/ui/mod.rs` | 設定ダイアログの入口 `show_settings_dialog` と、タブをまたいで使うイベント型・注意書きのヘルパー（`warning_label` / `notice_label` / `status_badge`）。外から使う経路（`crate::ui::...`）の `pub use` もここ |
| `src/ui/state.rs` | `SettingsDialogState`。ドラフトの保持、操作の受け止め、`SettingsDialogView` の切り出し |
| `src/ui/draft.rs` | `commit_draft` / `draft_from_imported` / `draft_from_defaults`。設定を組み替えるだけで描画を含まない |
| `src/ui/preset.rs` | プリセットの保存・読み込み・削除と「（変更あり）」の判定。描画を含まない |
| `src/ui/capability.rs` | `CapabilityCache`（デバイス能力の取得状態）と、そこから作る選択肢まわりの表示 |
| `src/ui/video_mode.rs` | デバイスを切り替えたときに選び直すビデオの既定値（`select_default_video_mode`） |
| `src/ui/device_tab.rs` | 「デバイス設定」タブの描画 |
| `src/ui/screenshot_tab.rs` | 「スクリーンショット設定」タブの描画 |
| `src/ui/hotkeys_tab.rs` | 「ホットキー」タブの描画と、割り当ての重複判定 |
| `src/ui/hotkey_capture.rs` | ホットキー入力ダイアログ。キー入力の組み立てと確定の判定 |
| `src/ui/other_tab.rs` | 「その他」タブの描画（プリセット、言語、書き出し / 読み込み / 初期化） |
| `src/ui/status_tab.rs` | 「接続状態」タブの描画 |
| `src/status.rs` | 失敗の記録（`ErrorCenter`）とトーストの間引き判定、設定ダイアログへ渡す接続状態（`ConnectionStatus`）、発生源ごとの定型文 |
| `src/repaint.rs` | 次の再描画までの間隔の判定（`next_repaint_delay`）と、UI スレッド以外から再描画を促す窓口（`RepaintWaker`） |
| `src/i18n/mod.rs` | 画面に出す文字列の入口。現在の言語（`Language` と `static LANGUAGE`）を持ち、`set_language` で切り替える。外から使う経路（`crate::i18n::...`）の `pub use` もここ |
| `src/i18n/text.rs` | 引数を取らない文字列の表（`texts!` が `Text` のキーと言語ごとの `match` を作る） |
| `src/i18n/msg.rs` | 引数を取る文字列。1 関数が 1 件で、言語ごとに文全体を組み立てる |

`src/app/` の子モジュールは**基本どれも `impl CaptureCardViewer` を足す形**で、状態そのものは `app/mod.rs` の構造体 1 つに集めてある。**子モジュール側にフィールドや `static` を持たせないこと。** 他の子モジュールから呼ぶメソッドにだけ `pub(super)` を付け、そのファイルの中だけで使うものは私有のままにする。

**例外はデバイスワーカーの 5 つ**（`worker.rs` / `worker_loop.rs` / `worker_timers.rs` / `worker_connect.rs` / `backend/`）。こちらは UI スレッドとは別のスレッドで動くので、状態を `CaptureCardViewer` に置けない。`worker_loop.rs` の `WorkerState` へ同じやり方で集めてあり、`worker_timers.rs` と `worker_connect.rs` がそこへ `impl` を足す。`backend/` はアプリの状態（`CaptureCardViewer` / `WorkerState` に属するもの）を持たず、デバイスの入口の trait とその実装だけを持つ。テスト用のモックだけは自分の中に観測用の値を抱える。1 ファイル 800 行以内を目安にし、超えそうなら分け方を見直す。

`src/video/` の子モジュールは**役割で分けてあるだけで、状態はそれぞれのファイルが定義する型が持つ。** 他のファイルから呼ぶ項目にだけ `pub(super)` を付け、そのファイルの中だけで使うものは私有のままにする。**外から使う経路（`crate::video::...`）は `video/mod.rs` の `pub use` に集める。** ただし**呼び出し側のテストからしか参照されない項目は再輸出しない。** テストを含まないビルドで誰も使わない `pub use` が残り、`unused_imports` の警告になるため。そういう項目（`FormatCapability` / `IntervalStats`）は置いてある子モジュールを `pub(crate) mod` にして、`crate::video::capabilities::FormatCapability` のように子モジュールの経路で参照する。`src/ui/` と同じ考え方。

`src/ui/` の子モジュールは**どれも状態を持たず、書き換えるのもドラフトだけ。** 起きたことは `SettingsEvent` / `HotkeyDialogEvent` の列で返す。ダイアログの状態は `state.rs` の `SettingsDialogState` 1 つに集めてある。**外から使う経路（`crate::ui::...`）は `ui/mod.rs` の `pub use` に集める。** `ui` の中だけで使う項目は再輸出せず、子モジュールの経路で参照する（`mod ui;` 自体が私有なので、誰も使わない再輸出は `unused_imports` の警告になる）。

`src/audio/` の子モジュールで**状態を持つのは `capture.rs` の `AudioCapture`、`fake.rs` の `FakeAudioCapture` と、スレッドをまたいで共有する `AudioControls` / `ResampleTelemetry` だけ。** 残りは純粋関数か、cpal のストリームを組み立てて返すだけにする。**外から使う経路（`crate::audio::...`）は `audio/mod.rs` の `pub use` に集める**（`ui/mod.rs` と同じ理由で、誰も使わない再輸出は警告になる）。子モジュール同士で使うものには `pub(super)` を付け、そのファイルの中だけで使うものは私有のままにする。

## 設計の理由はどこにあるか

**「なぜそうなっているか」は `docs/design/` にテーマ別に置いてある。** 作業の前に、触る範囲のものだけ読む。`GUARDRAIL.md` の各項目も、この一覧のどれかを参照している。

| ファイル | 扱う話題 |
|---|---|
| `docs/design/device-worker.md` | デバイス操作をワーカースレッド 1 本へ隔離した理由、チャネルを通さない共有、開き直しの差分判定、最小化中の扱い |
| `docs/design/threads.md` | スレッドの一覧と役割、ロック順序、ホットキーのリスナー、スクリーンショットの保存とクリップボード |
| `docs/design/reconnect.md` | 切断の検出、バックオフでの再試行、音声のフォールバックを外した経緯、Windows の既定デバイスの追従 |
| `docs/design/video-pipeline.md` | `FrameBuffer` と世代番号、色変換への映像調整の畳み込み、再描画の間隔と `RepaintWaker`、UI にあるが効かない設定 |
| `docs/design/audio.md` | 入出力の形が違う場合の変換、クロックドリフト補正、対応設定の取得、ミュート |
| `docs/design/settings.md` | `#[serde(default)]`、デバウンス保存、壊れた設定ファイルと `AutoSavePolicy` |
| `docs/design/settings-dialog.md` | ドラフトの編集、イベントで返す形、`commit_draft` の決まり、「その他」タブの書き出し / 読み込み / 初期化 |
| `docs/design/hotkeys.md` | アクションごとの割り当て、旧形式からの移行、差分での登録 |
| `docs/design/presets.md` | プリセットに入れる項目、「（変更あり）」の判定、名前の検証 |
| `docs/design/window.md` | 装飾なし（ボーダーレス）と、動かす / 大きさを変える / 閉じる手段の代替 |
| `docs/design/error-reporting.md` | 失敗の通知と間引き、「接続状態」タブ |
| `docs/design/logging.md` | ログの出力先とレベル、`catch_unwind` が効かないこと |
| `docs/design/assets.md` | アイコンと効果音の埋め込み、パスの解決 |
| `docs/design/i18n.md` | 画面に出す文字列を `src/i18n/` に集める仕組み、入れるもの・入れないもの、文字列を足すときの手順 |

目指す構造と現状との差分は `docs/ARCHITECTURE.md`。**同じ話が両方にある場合は `docs/ARCHITECTURE.md` を正とする。** デバイス起因の不具合を調べるときは `.claude/skills/device-debug/SKILL.md` の手順（ログの読み方、正常時の所要時間の目安、症状ごとの確認順）に従う。

## コーディング規約

- コードコメント、UI 文字列、コミットメッセージは日本語
- 既存の命名（snake_case、モジュール構成）に合わせる
- コメントは Issue #73 で一巡整理済み（ワーカースレッド化と競合するため後回しにしていた `src/app/device.rs` / `monitor.rs` / `retry.rs` / `capabilities.rs`、`src/video/`、`src/audio/` も含む）。とはいえ実装とコメントが食い違っている箇所が今後また出うるので、コメントを鵜呑みにせず実コードを確認すること

ブランチ名・コミットメッセージ・PR の書き方は `.claude/skills/naming-conventions/SKILL.md` にまとめてある。ブランチを切る前、コミットする前、PR を作る前に参照すること。

## テスト

テストの方針は `.claude/skills/testing-conventions/SKILL.md` にまとめてある。デバイス依存が強いため、一般的な 3 層ではなく「CI で自動実行できるか」で区分している。実機でしか確認できない項目は `docs/MANUAL-TEST.md` のチェックリストで担保する。既知の不具合で現在失敗する項目も同ファイルに明記してあるので、不具合を直したらチェックリスト側も更新すること。

## タスク管理

改善バックログは **GitHub Issues** で管理している。進行状況は GitHub Project「Capturecard_Viewer」で見る。

- Issues: https://github.com/Mui-MuiMui/Capturecard_Viewer/issues
- Project: https://github.com/users/Mui-MuiMui/projects/2

2026-09-20 に Asana から移行した。移行済みの Issue には本文末尾に移行元の Asana タスクの URL が残っている。**過去の PR 本文や `CHANGELOG.md` に残る Asana の URL は書き換えていない。** 当時の記録なので、そのまま読めばよい。

| 軸 | 表し方 |
|---|---|
| 分野 | `area:` ラベル（`ci` / `docs` / `bug` / `perf` / `refactor` / `feature` / `release`）。Asana のセクションに 1 対 1 で対応する |
| 優先度 | `P1` / `P2` / `P3` ラベル。本文冒頭の `[P1]` 表記も残してある |
| 進行状況 | Project の Status（未着手 / 作業中 / レビュー待ち / 人間確認待ち / 完了） |

**進行状況は Project の Status だけで管理する。ラベルでは表さない。** ラベルは分野（`area:`）と優先度（`P1`〜`P3`）のフィルタ用途に限る。2026-09-21 まで併用していた `status:人間確認待ち` ラベルは廃止し、全 Issue から外して削除済み。**二重管理は片方の更新漏れで必ず食い違うので、復活させないこと。**

Status で絞った一覧は Project から引く。着手候補は **Status が「未着手」のもの**から選ぶ。open な Issue には実装済みで人の確認を待っているだけのものが混ざっているため、`gh issue list` だけで選ばない。

```bash
gh project item-list 2 --owner Mui-MuiMui --format json --limit 300 --jq '.items[]|select(.status=="未着手" and .content.type=="Issue")|"#\(.content.number) \(.content.title)"'
```

Status の変更はユーザーレベルの `github-issues` skill にあるヘルパーを使う。**このスクリプトは個人環境の手順なのでリポジトリには置かない。**

```bash
bash ~/.claude/skills/github-issues/set-status.sh <番号...> -- <Status>
```

Project の Workflows（Item closed → 完了、Item reopened → 未着手、auto-add → 未着手）はユーザーがブラウザで設定するもの。**有効なら Claude は Issue のクローズと起票だけでよく、Status の手当ては要らない。**

各 Issue の本文には `file:line` 形式で該当箇所を書く。**この記述は起票時点のスナップショットなので、着手前に実コードで裏を取ること。**

### PR と Issue のリンク

**PR 本文には `Refs #<番号>`、コミットメッセージには `Refs: #<番号>` を書く。`Closes` / `Fixes` は使わない。** 場所ごとに GitHub の自動クローズがどう働くか、なぜ全ての場所で `Refs` に揃えるかは `.claude/skills/naming-conventions/SKILL.md` の「`Closes` ではなく `Refs` を使う」にある。

PR 本文の雛形は `.github/pull_request_template.md`。**GitHub が自動で差し込むのは既定ブランチ（`main`）にある版なので、この仕組みが効くのは次のリリースで `main` に入ってから。** それまでは見出しを自分で並べる。`gh pr create --body-file` で本文を渡す経路では、いずれにせよテンプレートは差し込まれない。

したがって流れはこうなる。

1. PR を作るとき本文の「対応する Issue」に `Refs #<番号>` を書く
2. Issue 側にも PR の URL と「人間が確認すること」をコメントする
3. マージされたら Status を「人間確認待ち」にする
4. **Issue を閉じるのは人が実機で確認したとき。** Claude は閉じない

部分実装の PR なら、Issue にその旨と残りのスコープを書いて Status は「作業中」のままにする。

## 開発フロー

計画 → 実装 → レビュー → PR 作成 の 4 段階を slash command にしてある。各段階の終わりに人の判断が入るゲートとして機能する。

| コマンド | 段階 |
|---|---|
| `/cv:plan <Issue 番号・URL または説明>` | 計画。コードは書かず、方針を提示して承認を待つ |
| `/cv:implement` | 承認済みの計画に従って worktree を切り実装する。まとまった単位でコミットする |
| `/cv:review` | 検証コマンドを走らせ、観点に沿ってセルフレビューする |
| `/cv:pr` | push、PR 作成、Issue との相互リンク。レビュー指摘への対応にも使う |

`cv:` の名前空間を付けているのは、`/plan` や `/review` が組み込みコマンドや他のプラグインと衝突するのを避けるため。コマンドは「やること」だけを持ち、書式や基準は skill を参照する。**同じ内容を両方に書かない。** 片方を直したときにもう片方が古くなるため。

リリースはこの 4 段階の外側にある。手順は `docs/RELEASE.md`、Claude がなぞる場合は `.claude/skills/release/SKILL.md` を使う。

指示役から Issue を渡されて並行開発するサブエージェントは `.claude/skills/subagent-workflow/SKILL.md` に従う。共通手順・してはいけないこと・最終報告の書式（20 行以内）と、指示役が使う依頼文の雛形をまとめてある。

**`CONTRIBUTING.md` は人向けの入口。** 規約の要約と各ドキュメントへの道案内だけを持ち、`CLAUDE.md` や skill と同じ内容を重複させない。 Claude Code 以外の AI エージェント向けの入口は `AGENTS.md` で、こちらも道案内だけを持つ。

### ブランチとコミット

`<type>/<説明>` の作業ブランチ → `dev` → `main`。**PR のマージ先は `dev`。** `main` へ入れるのはリリースのときだけ。

コミットは作業ログとして扱い、まとまった単位でどんどん積む。ただし**各コミットはビルドとテストが通る状態にする**。レビュー指摘への対応は元のコミットを直さず追加のコミットで積み、push 済みの履歴を force push で作り直さない。詳細は `.claude/skills/naming-conventions/SKILL.md` を参照。

**設計判断や方針は履歴ではなく `docs/design/` に書くこと。** 新しいセッションで読まれるのはこれらであって `git log` ではない。
