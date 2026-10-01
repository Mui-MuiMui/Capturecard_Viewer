# CLAUDE.md

このファイルは Claude Code がこのリポジトリで作業するときの手引きです。**してはいけないこと / 必ずすることは `GUARDRAIL.md` にまとめてある。** 下の取り込みで常に読まれるので、ここには再掲しない。

@GUARDRAIL.md

## プロジェクト概要

キャプチャーボード（キャプチャーカード）の映像と音声を、低遅延・シンプルな画面で表示する Windows 10/11 専用アプリ。Rust + eframe/egui 製の単一バイナリ。

- キャプチャーデバイスは Windows Media Foundation 経由で Web カメラとして扱う（nokhwa）。Media Foundation に出ないデバイス（OBS の仮想カメラなど）は DirectShow で扱い、名前に「(DirectShow)」を添える
- 音声は WASAPI 経由の入力 → リングバッファ → 出力のパススルー（cpal）
- 設定は `%AppData%\capturecard_viewer\config\default-config.toml`（`toml` で読み書きする。`src/settings/store.rs`）

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
| `src/com.rs` | COM（`ComApartment`、STA / MTA をモデル引数で選ぶ）と Media Foundation（`MfPlatform`）の初期化の RAII。DirectShow のバックエンドがデバイスワーカーで STA、録画スレッドが MTA で使う |
| `src/app/mod.rs` | アプリ状態 `CaptureCardViewer` の定義、`Default`、`eframe::App` 実装（`update` / `on_exit`） |
| `src/app/view.rs` | 映像の描画（ウィンドウ表示とフルスクリーン）、統計 OSD、テクスチャの取り込み |
| `src/app/placeholder.rs` | 映像が出ていないときのプレースホルダー。文言の決め方（`video_placeholder_text`）と映像エリアの中央への配置（`show_video_placeholder`） |
| `src/app/video_overlay.rs` | 映像の上に常設で重ねる表示（統計 OSD・フェイクデバイスの帯・録画中の印）を、映像の上・設定ダイアログの下の層へ寄せて描く `show_video_overlay` |
| `src/app/menu/mod.rs` | 右クリックメニューの置き場所と閉じ方、平らな一覧／サブメニューの出し分け、描画が返した `MenuAction` の処理 |
| `src/app/menu/items.rs` | 右クリックメニューの項目の描画。**状態を持たず、書き換えもしない。** 起きたことは `MenuAction` の列で返す |
| `src/app/window.rs` | 最前面表示、タイトルバーの有無、装飾なしのときの端のドラッグによるリサイズ、大きさのリセット、フルスクリーンの切り替え |
| `src/app/device.rs` | `apply_settings`（設定をワーカーへ渡す）と、ワーカーから届いたイベントの取り込み |
| `src/app/worker.rs` | デバイスワーカーとやり取りする型（コマンド / イベント / `DeviceConfig` / `DeviceSnapshot`）と、UI 側の窓口 `DeviceWorker` |
| `src/app/worker_loop.rs` | デバイスワーカースレッドの本体。`WorkerState` の定義、待ち時間の決定、観測値の書き出し |
| `src/app/worker_commands.rs` | デバイスワーカーのコマンドの受け口（`handle`）。設定の受け取りと開き直しの要求、最小化中のホットキーの代役（音量・ミュート）、即時の再接続 |
| `src/app/worker_timers.rs` | ワーカーがタイマーで回す監視の入口（`tick`）。再試行の期限、フレームの途絶 |
| `src/app/worker_audio_timers.rs` | ワーカーがタイマーで回す監視のうち音声まわり。音声ストリームのエラー、Windows の既定デバイスの切り替え、クロックドリフト補正 |
| `src/app/worker_connect.rs` | ワーカーが行うデバイス操作のうち映像側と列挙。映像を開く・閉じる、列挙、映像の能力の問い合わせ、既定デバイス名の確定。列挙の結果のログと「Windows 側にも見えていない」の判定の適用 |
| `src/app/worker_audio_connect.rs` | ワーカーが行うデバイス操作のうち音声側。音声を開く（`try_connect_audio`）、入力が未指定のときに開かずに待つ、映像の復帰に合わせた開き直し、対応設定の問い合わせ |
| `src/app/backend/mod.rs` | ワーカーがデバイスに触るときの入口の trait（`VideoBackend` / `AudioBackend` / `DeviceBackends`）と、本番かフェイクかを環境変数で選ぶ `backends_from_env`。テスト用のモックもここ（`#[cfg(test)]`） |
| `src/app/backend/system.rs` | 本番のバックエンド `SystemBackends`。映像は `VideoCapture`（Media Foundation）と `DirectShowCapture` を `SystemVideo` で束ね、音声は `AudioCapture` を trait に載せる。一覧の突き合わせ（`merge_video_devices`）とどちらで開くかの判定（`route_for`） |
| `src/app/backend/fake.rs` | フェイクのバックエンド `FakeBackends`。`FakeVideoCapture` / `FakeAudioCapture` を trait に載せる実装と、環境変数（`CAPTURECARD_VIEWER_FAKE_DEVICES` / `CAPTURECARD_VIEWER_FAKE_SCENARIO`）の解釈 |
| `src/app/monitor.rs` | 切断や既定デバイスの切り替え、列挙をログへ出す回と「Windows 側にも見えていない」の**判定**（純粋関数）。ワーカーが使う |
| `src/app/retry.rs` | `ConnectRetry` とバックオフ。「いつ試してよいか」だけを持つ。ワーカーが持つ |
| `src/app/capabilities.rs` | デバイス一覧のキャッシュと、デバイス能力・対応設定の取得要求（ワーカーへ流すところまで） |
| `src/app/screenshot.rs` | 撮影、保存スレッドの管理、結果の取り込み |
| `src/app/screenshot_sound.rs` | 効果音ファイルの読み込みスレッドの管理と結果の取り込み（適用・テスト再生）、効果音の再生と出力先を開けなかったときの報告 |
| `src/app/recording.rs` | 録画の開始・停止（`toggle_recording`、右クリックメニューとホットキーが呼ぶ）、リプレイバッファの設定を録画スレッドへ渡す（`sync_replay_buffer`、`apply_settings` が呼ぶ）、録画スレッドから届いた `RecordingEvent` の取り込み（ログ・トースト・`report_error`）、終了時の停止と `Finalize` の待ち合わせ、録画中の印と統計 OSD の録画の行。フィールド（`recorder`）は `app/mod.rs` |
| `src/app/settings_dialog.rs` | 設定ダイアログの操作の受け止め、インポート / エクスポート / 初期化、プリセットの適用 |
| `src/app/settings_store.rs` | 設定のデバウンス保存と即時保存、保存の失敗が続くときの再試行の間隔（`save_retry_delay`）とログ・トーストの間引き（`SaveFailureStreak`） |
| `src/app/update.rs` | 更新の確認と適用のスレッドの管理と結果の取り込み（`UpdateState`）、通知ダイアログの操作、前回の更新の残りの後片付け、終了時の新しい exe の起動 |
| `src/app/hotkeys.rs` | ホットキーの適用と、押されたときのアクションの実行 |
| `src/app/audio_control.rs` | 音量とミュートの操作、その OSD |
| `src/app/error_report.rs` | 失敗の記録と、トースト・「接続状態」タブへの出し方 |
| `src/video/mod.rs` | `VideoError` とログ用の `elapsed_ms`。外から使う経路（`crate::video::...`）の `pub use` もここ |
| `src/video/capture.rs` | nokhwa `CallbackCamera` によるキャプチャ。開く・閉じる・列挙する、フレームコールバック（nokhwa の `Buffer` から取り出して `FrameSink` へ渡す）、途絶の観測（`VideoLinkState`） |
| `src/video/directshow/mod.rs` | DirectShow の映像デバイス `DirectShowCapture`（列挙・能力・開く・閉じる・観測）と、表示名の「(DirectShow)」の付け外し |
| `src/video/directshow/devices.rs` | DirectShow の列挙（`ICreateDevEnum`）と対応形式（`IAMStreamConfig::GetStreamCaps`）、いまの解像度（`GetFormat`）、開く解像度と形式の選び方（`target_resolution` / `choose_candidate`） |
| `src/video/directshow/graph.rs` | DirectShow のフィルターグラフの組み立て・開始・停止・破棄（`CaptureGraph`） |
| `src/video/directshow/filter.rs` | サンプルを受け取る自前のレンダラーフィルター（`IBaseFilter` / `IPin` / `IMemInputPin`）。`Receive` から `FrameSink` へ渡す |
| `src/video/directshow/media_type.rs` | `AM_MEDIA_TYPE` の読み書きと解放 |
| `src/video/frame_sink.rs` | フレームコールバックの本体 `FrameSink`（YUY2→RGB、`FrameBuffer` へ積む、`RepaintWaker` で UI を起こす）。実機（Media Foundation / DirectShow）とフェイクで共有する。DirectShow の RGB24 / MJPEG / 4:2:0 の YUV（NV12 / I420 / YV12）の受け口もここ |
| `src/video/fake.rs` | 実機なしで動くフェイクの映像デバイス `FakeVideoCapture`。テストパターンを指定 fps で吐く生成スレッド、切断・接続失敗のシナリオ |
| `src/video/test_pattern.rs` | フェイクが吐くテストパターン（カラーバー、ベタ塗り、フレーム番号の焼き込み）の描画。純粋関数 |
| `src/video/capabilities.rs` | `VideoMode` / `FormatCapability` と、デバイス能力の取得 |
| `src/video/color.rs` | YCbCr→RGB の係数表とその選び方、映像調整の畳み込み、設定の共有（`SharedColorConversion`） |
| `src/video/convert.rs` | YUY2→RGB24 の画素変換と、DirectShow の RGB24（BGR）/ MJPEG の展開 |
| `src/video/yuv420.rs` | 4:2:0 の YUV（NV12 / I420 / YV12）→ RGB24 の画素変換（`yuv420_to_rgb`、面の並び `Yuv420Layout`）。1 画素の式と係数表は YUY2 と同じで、違うのは色差の置き方だけ。`FrameSink` が呼ぶ |
| `src/video/frame_buffer.rs` | `FrameBuffer`（`Arc` によるフレーム共有と世代番号）と観測値（`FrameStats`）、画素データの長さの判定（`frame_len_status`）、置き換えたフレームを `Arc` ごと使い回すか（`fill_recycled`） |
| `src/video/tap.rs` | 録画へ映像を回す差し込み口 `VideoTap`。録画中だけ、`FrameSink` が画面へ置いたのと同じ `Arc<VideoFrame>` を容量 3 のリングへ積む（待たない `try_lock`、満杯なら捨てて数える）。Vec の回収に失敗した回数も録画中だけ数える |
| `src/audio/mod.rs` | 音声モジュールの入口。`ActiveAudio` / `AudioDirection` / `AudioError` と能力キャッシュのキー（`cache_key` / `device_name_from_key`）、外から使う経路（`crate::audio::...`）の `pub use` |
| `src/audio/capabilities.rs` | デバイスの対応設定の取得（`query_capabilities`）と、設定画面に出す選択肢の組み立て（`selectable_*` / `ChoiceSource`） |
| `src/audio/stream_config.rs` | 対応設定の中から実際に開く設定を選ぶ（`select_best_config` / `select_aligned_configs`）。扱えるサンプル型の一覧もここ |
| `src/audio/capture.rs` | `AudioCapture`。パススルーの開始と停止、観測値（実際に開いた内容・アンダーラン・リサンプル）の取り出し |
| `src/audio/stream.rs` | cpal の入力ストリームの組み立てと入力のコールバック（本体は `process_input` で、フェイクと共有する）、リングバッファの型、ストリームのエラーの扱い（`handle_stream_error`）、満杯で捨てたフレームの数え方 |
| `src/audio/stream_output.rs` | cpal の出力ストリームの組み立てと出力のコールバック（本体は `process_output` で、フェイクと共有する）、`OutputSignals`、アンダーランの数え方 |
| `src/audio/convert.rs` | レート・チャンネル数が違う場合の変換（`PassthroughConverter`） |
| `src/audio/sample.rs` | サンプル型の変換（f32 ⇄ i16 / u16 / i32）。純粋関数 |
| `src/audio/resample.rs` | クロックドリフト補正の共有状態（`ResampleTelemetry`）と補正係数の決め方（`decide_resample_correction`） |
| `src/audio/controls.rs` | `AudioControls`。音量・パススルー・ミュートの共有状態 |
| `src/audio/fake.rs` | 実機なしで動くフェイクの音声デバイス `FakeAudioCapture`。デバイスの一覧と形、開く・閉じる、観測値、接続失敗・ストリームのエラーのシナリオ |
| `src/audio/fake_stream.rs` | フェイクの音声デバイスが立てる入出力のスレッドの本体。正弦波の入力（`SineInput`）と書き込みを捨てる出力（`DiscardOutput`）、経過時間に合わせて回す `run_paced` |
| `src/audio/tap.rs` | 録画へ音声を回す差し込み口 `AudioTap`。録画中だけ、入力コールバックが f32 へ直した値を入力の形のまま 1 秒ぶんのリングへ積む（待たない `try_lock`、空きが足りなければそのコールバックの分を捨てて数える）。累計のサンプル数・最後に積んだ時刻・入力の形・開き直しの番号・途切れた位置を Atomic で持つ |
| `src/recording/mod.rs` | 録画の入口。`RecordingError`（文言は `Display` から `crate::i18n`）、`EncoderInfo`、経過時間の書式（`format_elapsed`）、外から使う経路（`crate::recording::...`）の `pub use` |
| `src/recording/recorder.rs` | 録画スレッドの窓口 `Recorder`（UI スレッドが 1 つ持つ）。録画かリプレイバッファが ON のときにスレッドを起こし、どちらも無くなったら止めて join する（スレッドは自分から抜けない）。`RecordingCommand` / `RecordingEvent` / `RecordingSummary` / `RecordingTelemetry`（録画中の値は録画を始めたときからの差） |
| `src/recording/recorder_loop.rs` | 録画スレッドの本体。コマンドの受け口と、リプレイバッファを通すかの経路の切り替え（`ReplayState`）。差し込み口を使っている録画の間に変えられたリプレイバッファの設定を、録画が終わってから反映する |
| `src/recording/session.rs` | リプレイバッファを通さない 1 回の録画 `Session`（①②の経路。リングの差し込み、最初のフレームで Sink Writer を作る、それまでの音声を溜めて渡す、止めるときに音声を映像の終わりまで揃える、ハードウェアからソフトウェアへの作り直し、大きさの変化・空き容量・書き込みの失敗で止める）。失敗の扱いの共通部分（`prepare_folder` / `check_disk` / `create_error` / `Finished`） |
| `src/recording/replay.rs` | リプレイバッファ `ReplayPipeline`（③）。差し込み口を差したまま、エンコーダ MFT で H.264 / AAC にしてエンコード済みのリングへ積む。大きさが変わったらエンコーダを作り直してリングを空にする |
| `src/recording/replay_config.rs` | リプレイバッファの設定 `ReplayConfig`（UI スレッドが組み立てて録画スレッドへ渡す）と、エンコーダの作り直しが要るかの判定（`same_encoders`） |
| `src/recording/replay_recording.rs` | リプレイバッファを通す 1 回の録画 `ReplayRecording`。先頭のキーフレームからエンコードなしの Sink Writer へ書く。リングの中身は数 ms ごとに少しずつ書き（`catch_up`）、追いついたらライブのサンプルを直接書く |
| `src/recording/replay_ring.rs` | エンコード済みのリング `EncodedRing` と、書き出すキーフレームの選び方（`replay_start`）・捨てる境界（`keep_from` / `gops_to_drop`）・PTS の付け替え（`Cut`）。判定は純粋関数 |
| `src/recording/encoder.rs` | エンコーダ MFT `EncoderMft`（H.264 はハードウェアの非同期型 → ソフトウェアの同期型の順に試す、AAC は同期型）。非同期型は `METransformNeedInput` / `METransformHaveOutput` を待たずに取る。エンコードなしの Sink Writer へ渡すメディアタイプ（`stream_type`） |
| `src/recording/encoder_setup.rs` | `EncoderMft` を作るときだけ使う補助。エンコーダ MFT の列挙（`enumerate`）、候補を先頭から開く（`open_first`）、H.264 / AAC の入出力の形の組み立て（`configure_video` / `configure_audio`）、ストリームの番号（`stream_ids`） |
| `src/recording/passthrough.rs` | エンコードなしの Sink Writer `PassthroughWriter`（入力 = 出力の H.264 / AAC を MP4 へまとめるだけ） |
| `src/recording/bitstream.rs` | H.264 の Annex B の読み取り（IDR か、SPS / PPS）と、AAC の `MF_MT_USER_DATA` の予備の組み立て。純粋関数 |
| `src/recording/writer.rs` | Media Foundation の Sink Writer（`IMFSinkWriter`）の組み立て（H.264 と AAC の 2 ストリーム）と NV12 / 16bit PCM の書き込み、`Finalize`、エンコーダの遅れ（`backlog`）、使っているエンコーダの名前（`encoder_info`） |
| `src/recording/sample_pool.rs` | Sink Writer とエンコーダ MFT へ渡す NV12 のサンプルの使い回し `SamplePool`。サンプルとバッファの参照が手元の分だけに戻ったものだけを次に使う。判定（`slot_state`）は純粋関数 |
| `src/recording/convert.rs` | RGB → NV12 の画素変換（BT.709 / BT.601 リミテッド、色差は 2x2 の平均）。純粋関数 |
| `src/recording/pts.rs` | 映像の PTS（受け取った時刻 − 録画の開始、単調増加）と、音声の PTS の計算（出力フレーム数 → 100ns、`AudioTap` の時刻と累計からの逆算、途切れたときの揃え方、無音で埋める先、ドリフトの測定）。純粋関数 |
| `src/recording/audio.rs` | 音声トラック `AudioTrack`（録画スレッドの中だけ）。`AudioTap` のリングから取り出し、録画用の `PassthroughConverter` で 48kHz 2ch へ寄せて 16bit PCM にし、PTS を付けた塊にする。開き直し・溢れ・音声が来ない間の揃え方と、停止時のドリフトのログ |
| `src/recording/file_name.rs` | ファイル名の書式の検め（chrono の `Item::Error`、Windows で使えない文字、末尾の空白・ピリオド、予約デバイス名）と、同じ名前があるときの `_2` `_3` … |
| `src/recording/storage.rs` | 保存先の空き容量（`GetDiskFreeSpaceExW`）と、止める境界（500MB） |
| `src/recording/test_support.rs` | 録画のテストの補助（`#[cfg(test)]`）。`#[ignore]` のテストが使う、フェイクの映像と音声を流して `Session` で録画する部分（`record_until_size_changes`）と、書いた MP4 を読み戻す部分 |
| `src/hotkey/mod.rs` | 外から使う経路（`crate::hotkey::...`）の `pub use` だけ |
| `src/hotkey/action.rs` | `HotkeyAction`（ホットキーを割り当てられる操作）と設定ファイル上の名前、溜まった押下の畳み方 |
| `src/hotkey/parse.rs` | `HotkeyError` と、ホットキー文字列のパース |
| `src/hotkey/manager.rs` | `HotkeyManager` の本体（リスナーの起動と停止、ウィンドウ状態の受け渡し）と `BackgroundHotkeyRunner` |
| `src/hotkey/assignments.rs` | `HotkeyAssignmentError`、アクション別の登録（差分適用・一時停止と再開・試し登録）と押下の取り出し |
| `src/hotkey/listener.rs` | リスナースレッドと共有状態 `ListenerState`、押下の照合とデバウンス |
| `src/keyboard_hook.rs` | 低レベルキーボードフック（`WH_KEYBOARD_LL`）。キーを奪わずに押下を観測し、リスナースレッドのメッセージループへ渡す。前面でもフックが呼ばれるよう、winit が登録したキーボードの Raw Input を外す（`stop_raw_keyboard_input`） |
| `src/screenshot.rs` | `ScreenshotError`（クリップボードへのコピーと効果音で共通）と、映像フレームのクリップボードへのコピー（`copy_frame_to_clipboard`） |
| `src/screenshot_sound.rs` | rodio による効果音の読み込みと再生。埋め込みの既定音、設定のパスの解決（`resolve_sound_path`）、読み込み要求の番号の管理（`ScreenshotManager`） |
| `src/settings/mod.rs` | 設定の入口。`AppSettings` と、読み込みで必ず通る `RawAppSettings` → `From`（旧形式からの移行とプリセットの整え）、`SettingsError`、`APP_NAME`。外から使う経路（`crate::settings::...`）の `pub use` もここ |
| `src/settings/video.rs` | `[video]`。`VideoSettings`、色空間・輝度レンジ・開き方の選択肢、映像調整の範囲と、それぞれの serde の補助 |
| `src/settings/audio.rs` | `[audio]`。`AudioSettings`、サンプリングレート・チャンネル数の既定値、リングバッファの長さの範囲 |
| `src/settings/screenshot.rs` | `[screenshot]`。出力先・保存形式・JPEG 品質の選択肢、保存先の既定値、保存するファイルのパス（`AppSettings::get_screenshot_path`） |
| `src/settings/recording.rs` | `[recording]`。ビットレート・リプレイバッファの長さの範囲、保存先の既定値 |
| `src/settings/ui.rs` | `[ui]`（音量・言語・ウィンドウ）と `[update]`。音量の範囲と言語の選択肢（`LanguageSetting`） |
| `src/settings/hotkeys.rs` | `[hotkeys]` / `[hotkey_settings]`。既定の割り当て、旧版の `screenshot.hotkey` からの移行（`migrate_hotkeys`）、`AppSettings::hotkey` / `set_hotkey` |
| `src/settings/preset.rs` | `[[presets]]`。適用と一致の判定（`matches_preset` / `resolved_active_preset`）、名前の検証、読み込んだ一覧の整え方（`sanitize_presets`）、`AppSettings` のプリセット操作 |
| `src/settings/store.rs` | 設定ファイルの読み込みと保存（`AppSettings::load` / `save`）、TOML の読み方と書き方（`parse_settings` / `serialize_settings`）、読めなかったファイル・空のファイルの退避、`LoadOutcome` / `AutoSavePolicy`。置き場所は `config_path` を呼ぶだけ |
| `src/settings/write.rs` | 設定ファイルの書き込み。同じフォルダの一時ファイル（`<名前>.<乱数>.tmp`）へ書いて rename で置き換える（`write_atomically` / `replace_atomically`）。保存と書き出しで共通 |
| `src/settings/transfer.rs` | 設定の書き出し / 読み込み（`export_to` / `import_from`）と、書き出すファイルの既定の名前（`export_file_name`） |
| `src/settings/testing.rs` | テストが複数のファイルから使う設定ファイルの例（`FULL_CONFIG` / `LEGACY_CONFIG`）と `without_key`、一時ファイルが残っているかの確かめ（`has_own_temp_file`）、保存先の候補の例（`#[cfg(test)]`） |
| `src/config_path.rs` | 設定ファイルとログの置き場所（`ConfigLocation`）。既定は `%AppData%` の下（1.2.x まで使っていた confy と同じ場所）で、環境変数 `CAPTURECARD_VIEWER_CONFIG_DIR` で差し替える。解釈（`parse_config_dir` / `resolve`）は純粋関数 |
| `src/logging.rs` | `log` クレートのロガー実装。ログファイルの置き場所・命名・世代管理、レベルの決定 |
| `src/ui/mod.rs` | 設定ダイアログの入口 `show_settings_dialog` と、タブをまたいで使うイベント型・注意書きのヘルパー（`warning_label` / `notice_label` / `status_badge`）。外から使う経路（`crate::ui::...`）の `pub use` もここ |
| `src/ui/state.rs` | `SettingsDialogState`。ドラフトの保持、操作の受け止め、`SettingsDialogView` の切り出し |
| `src/ui/draft.rs` | `commit_draft`（ドラフトを実行中の設定へ反映する）。設定を組み替えるだけで描画を含まない |
| `src/ui/draft_import.rs` | `draft_from_imported` / `draft_from_defaults`（読み込みと初期化でドラフトを作る）。`commit_draft` が反映する項目と揃える。描画を含まない |
| `src/ui/preset.rs` | プリセットの保存・読み込み・削除と「（変更あり）」の判定。描画を含まない |
| `src/ui/capability.rs` | `CapabilityCache`（デバイス能力の取得状態）と、そこから作る選択肢まわりの表示 |
| `src/ui/video_mode.rs` | デバイスを切り替えたときに選び直すビデオの既定値（`select_default_video_mode`） |
| `src/ui/device_tab.rs` | 「デバイス設定」タブの描画 |
| `src/ui/screenshot_tab.rs` | 「スクリーンショット設定」タブの描画 |
| `src/ui/recording_tab.rs` | 「録画」タブの描画（保存先、ファイル名の書式と例、映像のビットレート、ハードウェアエンコーダ、音声の有無とビットレート、リプレイバッファの ON / OFF とさかのぼる長さ） |
| `src/ui/hotkeys_tab.rs` | 「ホットキー」タブの描画と、割り当ての重複判定 |
| `src/ui/hotkey_capture.rs` | ホットキー入力ダイアログ。確定の判定と描画 |
| `src/ui/hotkey_keys.rs` | 入力ダイアログが使うキーの対応表（`hotkey_key_name`、`hotkey::parse` と同じ範囲）、割り当てさせない組み合わせ（`is_clipboard_command_chord`）、ホットキー文字列の組み立て |
| `src/ui/other_tab.rs` | 「その他」タブの描画（プリセット、言語、書き出し / 読み込み / 初期化） |
| `src/ui/status_tab.rs` | 「接続状態」タブの描画 |
| `src/ui/update_dialog.rs` | 新しい版を知らせ、更新の進み具合と結果を出すダイアログの描画（`UpdateDialogView`）。押されたものを `UpdateDialogEvent` で返す |
| `src/status.rs` | 失敗の記録（`ErrorCenter`）とトーストの間引き判定、設定ダイアログへ渡す接続状態（`ConnectionStatus`）、発生源ごとの定型文 |
| `src/update/mod.rs` | 更新の確認。GitHub の Release API への問い合わせ（`check_latest_release`）と、版の比較・通知するかの判定（純粋関数）、`UpdateError` |
| `src/update/apply.rs` | 更新の適用の本体（`run_apply`）。exe を `.new` へ書きながらの SHA-256 の計算と照合、進み具合（`ApplyProgress`）、キャンセルと差し替えの取り合い（`ApplyControl`）、`ApplyError` |
| `src/update/download.rs` | 更新の資産の読み取り。HTTP（ureq）かローカルのファイルを開き（`open_source`）、64 KiB ずつ読みながら合間にキャンセルを見る（`read_in_chunks` / `fetch_text`）。読み取りの失敗の `ApplyError` への変換 |
| `src/update/assets.rs` | 更新の適用のうち資産の選び方（`ApplyPlan`）。版なしの exe 名（`EXE_ASSET_NAME`）→ 1.2.0 の旧名へのフォールバック、Release の資産からの選択、URL の検証 |
| `src/update/swap.rs` | 更新の適用のうちファイルの置き換え。exe の隣の一時名（`ExePaths`）、フォルダに書けるかの確認、`.old` / `.new` を使った差し替えと失敗時の戻し方（`swap_in` / `recovery_for` / `roll_back`）、前回の残りの後片付け |
| `src/update/checksum.rs` | `SHA256SUMS.txt` の行の読み方（`find_checksum`）と、大文字小文字を区別しない照合（`checksum_matches`） |
| `src/update/overrides.rs` | 更新の確認を試すための環境変数（`CAPTURECARD_VIEWER_UPDATE_CURRENT_VERSION` / `CAPTURECARD_VIEWER_UPDATE_API_URL`）の解釈（`CheckOverrides`） |
| `src/overlay.rs` | 操作したときだけ数秒出て消える OSD（フルスクリーンの切り替え、音量、失敗や録画の保存のトーストなど）。期限と描画の `TransientOverlay` と、中身の `OverlayContent`（テキストだけ / バー付き）。常設の統計 OSD は `app/video_overlay.rs` の側 |
| `src/repaint.rs` | 次の再描画までの間隔の判定（`next_repaint_delay`）と、UI スレッド以外から再描画を促す窓口（`RepaintWaker`） |
| `src/i18n/mod.rs` | 画面に出す文字列の入口。現在の言語（`Language` と `static LANGUAGE`）を持ち、`set_language` で切り替える。外から使う経路（`crate::i18n::...`）の `pub use` もここ |
| `src/i18n/text.rs` | 引数を取らない文字列の表（`texts!` が `Text` のキーと言語ごとの `match` を作る） |
| `src/i18n/msg.rs` | 引数を取る文字列。1 関数が 1 件で、言語ごとに文全体を組み立てる |
| `src/i18n/device_msg.rs` | 引数を取る文字列のうち、デバイス（映像・音声）の接続と状態で使うもの（映像・音声のエラー、「Windows 側にも見えていない」、接続状態の観測値、「デバイス設定」タブ、「接続状態」タブ）。書き方は `msg.rs` と同じ |
| `src/i18n/update_msg.rs` | 引数を取る文字列のうち、更新の確認と適用で使うもの。書き方は `msg.rs` と同じ |
| `src/i18n/recording_msg.rs` | 引数を取る文字列のうち、録画で使うもの。書き方は `msg.rs` と同じ |

`src/app/` の子モジュールは**基本どれも `impl CaptureCardViewer` を足す形**で、状態そのものは `app/mod.rs` の構造体 1 つに集めてある。**子モジュール側にフィールドや `static` を持たせないこと。** 他の子モジュールから呼ぶメソッドにだけ `pub(super)` を付け、そのファイルの中だけで使うものは私有のままにする。

**例外はデバイスワーカーの 8 つ**（`worker.rs` / `worker_loop.rs` / `worker_commands.rs` / `worker_timers.rs` / `worker_audio_timers.rs` / `worker_connect.rs` / `worker_audio_connect.rs` / `backend/`）。こちらは UI スレッドとは別のスレッドで動くので、状態を `CaptureCardViewer` に置けない。`worker_loop.rs` の `WorkerState` へ同じやり方で集めてあり、`worker_commands.rs` / `worker_timers.rs` / `worker_audio_timers.rs` / `worker_connect.rs` / `worker_audio_connect.rs` がそこへ `impl` を足す（コマンドの受け口の `worker_commands.rs` も同じ）。`backend/` はアプリの状態（`CaptureCardViewer` / `WorkerState` に属するもの）を持たず、デバイスの入口の trait とその実装だけを持つ。テスト用のモックだけは自分の中に観測用の値を抱える。1 ファイル 800 行以内を目安にし、超えそうなら分け方を見直す。

`src/video/` の子モジュールは**役割で分けてあるだけで、状態はそれぞれのファイルが定義する型が持つ。** 他のファイルから呼ぶ項目にだけ `pub(super)` を付け、そのファイルの中だけで使うものは私有のままにする。**外から使う経路（`crate::video::...`）は `video/mod.rs` の `pub use` に集める。** ただし**呼び出し側のテストからしか参照されない項目は再輸出しない。** テストを含まないビルドで誰も使わない `pub use` が残り、`unused_imports` の警告になるため。そういう項目（`FormatCapability` / `IntervalStats`）は置いてある子モジュールを `pub(crate) mod` にして、`crate::video::capabilities::FormatCapability` のように子モジュールの経路で参照する。`src/ui/` と同じ考え方。

`src/ui/` の子モジュールは**どれも状態を持たず、書き換えるのもドラフトだけ。** 起きたことは `SettingsEvent` / `HotkeyDialogEvent` の列で返す。ダイアログの状態は `state.rs` の `SettingsDialogState` 1 つに集めてある。**外から使う経路（`crate::ui::...`）は `ui/mod.rs` の `pub use` に集める。** `ui` の中だけで使う項目は再輸出せず、子モジュールの経路で参照する（`mod ui;` 自体が私有なので、誰も使わない再輸出は `unused_imports` の警告になる）。

`src/audio/` の子モジュールで**状態を持つのは `capture.rs` の `AudioCapture`、`fake.rs` の `FakeAudioCapture`（とその入出力のスレッドだけが持つ `fake_stream.rs` の `SineInput` / `DiscardOutput`）と、スレッドをまたいで共有する `AudioControls` / `ResampleTelemetry` / `AudioTap` だけ。** 残りは純粋関数か、cpal のストリームを組み立てて返すだけにする。**外から使う経路（`crate::audio::...`）は `audio/mod.rs` の `pub use` に集める**（`ui/mod.rs` と同じ理由で、誰も使わない再輸出は警告になる）。子モジュール同士で使うものには `pub(super)` を付け、そのファイルの中だけで使うものは私有のままにする。

`src/recording/` の子モジュールで**状態を持つのは `recorder.rs` の `Recorder`（UI スレッドの窓口）と、録画スレッドの中だけにあるもの（`recorder_loop.rs` の `Worker`、1 回の録画 `Session` / `ReplayRecording`、リプレイバッファ `ReplayPipeline` とそのリング `EncodedRing`、音声トラック `AudioTrack`、`SinkWriter` / `PassthroughWriter` / `EncoderMft`）だけ。** 変換・PTS・ファイル名・空き容量の判定、リングのどこから書くか・どこで捨てるかは純粋関数にする。**録画スレッドから `error!` を出さず、失敗は `RecordingEvent` で UI スレッドへ返す**（`docs/design/threads.md`）。外から使う経路は `recording/mod.rs` の `pub use` に集める。

`src/settings/` の子モジュールは**セクションごとに分けてあるだけで、`AppSettings` の定義と `RawAppSettings` / `From` は `mod.rs` に置く。** セクションに項目を足すときは型を置いたファイルを、`AppSettings` に項目を足すときは `mod.rs` の 3 か所（`AppSettings` / `RawAppSettings` / `From`）を触る。serde の補助（`deserialize_*` / `*_from_str`）はそれを使う設定と同じファイルに置く。`AppSettings` のメソッドは関係するファイルがそれぞれ `impl AppSettings` を足す。外から使う経路（`crate::settings::...`）は `settings/mod.rs` の `pub use` に集め、外からテストでしか使わない項目は再輸出しない（`ui/mod.rs` と同じ理由。例外は `app::worker_loop` のテストが使う `DEFAULT_BUFFER_MS` で、`#[cfg(test)]` の `pub use` にしてある）。複数のファイルのテストが使う設定ファイルの例は `settings/testing.rs` に置く。

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
| `docs/design/recording.md` | 録画（#120）とリプレイバッファ（#182）の設計。**第 1 段（映像、#281）、第 2 段（音声、#282）、リプレイバッファ（③、#182）を実装済み。** 録画スレッドとその寿命、コールバックからロックなしで渡すリング、Media Foundation の Sink Writer、エンコーダ MFT とエンコード済みのリング、PTS とドリフト、失敗の扱い、`[recording]`、段階分け |
| `docs/design/update.md` | 更新の確認（GitHub の Release API、native-tls、確認のスレッド、`[update]`、通知ダイアログ）と適用（資産、書き込みの確認、SHA-256 の照合、`.old` / `.new` での差し替えと戻し方、再起動） |

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
