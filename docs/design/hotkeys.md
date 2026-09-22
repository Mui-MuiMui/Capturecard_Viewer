# ホットキー

割り当てをアクションごとに持つ設定ファイルの形と、旧形式からの移行。
アクションを増やすときに埋める場所と、登録を差分で行う理由。
リスナースレッドの扱いは `docs/design/threads.md` にある。

## ホットキーはアクションごとに持つ

割り当ては `AppSettings::hotkeys`（`BTreeMap<HotkeyAction, String>`）にあり、設定ファイルでは独立した `[hotkeys]` セクションになる。キーは `HotkeyAction::as_str()` の文字列。

```toml
[hotkeys]
screenshot = "F5"
toggle_fullscreen = "Ctrl+F11"
```

- **`[hotkeys]` セクションが無い場合と、空のセクションがある場合は意味が違う。** 前者は旧版が書いた設定ファイル（既定の F5 を入れる）、後者はすべての割り当てを外した状態（何も入れない）。`RawAppSettings::hotkeys` を `Option` で受けているのはこの区別のため
- **既定で割り当てるのはスクリーンショットの F5 だけ。** グローバルホットキーは他のアプリより先にキーを奪うので、こちらから F11 のような一般的なキーを押さえない
- **旧版の `screenshot.hotkey` は読むだけで書き戻さない。** `ScreenshotSettings::legacy_hotkey` が `#[serde(rename = "hotkey", skip_serializing)]` で受け、`[hotkeys]` が無いときだけスクリーンショットへ移す。移行後の最初の保存で旧項目は設定ファイルから消える（新しい版で一度起動すると、古い版へ戻したときはホットキーが既定の F5 に戻る）
- 知らないアクション名は読み飛ばしてログに残す。エラーにすると設定ファイル全体が読めなくなる
- **読み込みは `#[serde(from = "RawAppSettings")]` を通る。** どの経路で読んでも移行が走るようにするためで、`AppSettings` に項目を足すときは `RawAppSettings` と `From` にも足すこと

アクションを増やすときは `HotkeyAction` に variant を足し、`ALL` / `as_str` / `label` / `runs_while_minimized` の 4 か所と、`folded_repeats`、`src/app/hotkeys.rs` の `run_hotkey_action` と `background_hotkey_runner` を埋める（後ろの 4 つは「最小化中の扱い」を参照）。**`as_str` の文字列は設定ファイルに書かれるので、一度出した名前は変えない。** 実行は右クリックメニューや映像上の操作と同じメソッドを呼ぶこと（`set_always_on_top` / `adjust_volume` / `reconnect_devices` / `toggle_fullscreen`）。独自に書くと、同じ操作なのに設定の保存やオーバーレイ表示の有無が経路で変わる。

登録は `HotkeyManager::apply` が差分だけ行う。2 秒ごとに呼ばれるため、無条件に登録し直すとその瞬間の入力を取りこぼす。登録できなかったものは `errors()` に残り、設定画面に理由が出る。**直るまで毎回試し直すが、ログに出すのは理由が変わったときだけ**（同じ失敗が 2 秒ごとに積もらないように）。

**`HotkeyManager::apply` を直接呼ばないこと。** 呼ぶのは `CaptureCardViewer::apply_hotkey_assignments` だけで、そこで `report_error(ErrorSource::Hotkey, ..)` によるトースト通知と、全て登録できたときの `ErrorCenter::clear` を行っている。直接呼ぶとこれらが抜ける。設定画面の一覧に出す見出しも `ErrorSource::Hotkey.headline()` を使い、トーストと同じ文言に揃えてある。

## 最小化中の扱い

最小化すると eframe が再描画要求を捨てるため `update()` が呼ばれなくなる（`docs/design/video-pipeline.md` の「再描画をいつ要求するか」）。押下を検出するのはリスナースレッドなので最小化中も止まらないが、**実行は `update()` の `handle_hotkeys` が行っていたため、復帰するまで持ち越されていた**（#133）。接続の再試行と切断の監視はデバイスワーカーへ移して解決済みで（`docs/design/device-worker.md`）、残っていたのがこのアクション実行。

アクションを 2 種に分けて扱う。分かれ目は `HotkeyAction::runs_while_minimized()`。

| 種別 | アクション | 最小化中の扱い |
|---|---|---|
| 画面が要らない | デバイス再接続 / 音量を上げる・下げる / ミュート切替 | その場で実行する |
| 画面が要る | スクリーンショット / フルスクリーン切替 / 最前面表示の切替 | 復帰するまで溜める |

### 画面が要らないものはリスナーからワーカーへ流す

リスナースレッドは `BackgroundHotkeyRunner`（`DeviceCommand` を送る閉包）を持っており、最小化中はそこへ渡す。組み立ては `app::hotkeys::background_hotkey_runner` で、送るのは `ReconnectNow` / `AdjustVolume` / `ToggleMute` の 3 つ。**デバイスワーカーはウィンドウの状態に関係なく動く唯一のスレッドなので、UI スレッドの代役をそこに置いた。**

- **新しい `Arc<Mutex<..>>` は足していない。** 音量とミュートの実体は既に `AudioControls`（Atomic）で、ワーカーはその複製を持っている。最小化しているかは既存の `ListenerState`（リスナーと共有している唯一のロック）へ `minimized` として書く
- **最小化しているかは `update()` の末尾で毎フレーム書く。** 最小化すると `update()` が呼ばれなくなるので、最後に書いた `true` がそのまま残る。これが「最小化中である」ことの表現になっている
- **ワーカーが実行したことは `DeviceEvent::VolumeAdjusted` / `MuteToggled` で UI へ返す。** 復帰した最初のフレームで `adjust_volume` / `toggle_mute` を通るので、設定への反映・OSD・右クリックメニューへの反映は、メニューから操作したときと同じになる（GUARDRAIL の「ホットキーのアクションは右クリックメニューと同じメソッドを呼ぶ」）
- **音量は絶対値ではなく差分（`delta`）で返す。** `AudioControls` が持つのは 0.0〜2.0 の倍率で、パーセントへ戻すと端数が動く（60% が 59.999996% になりうる）。差分なら UI 側の値を正のまま同じ経路へ通せる
- 上下限とミュート解除の判断は UI と同じ `app::audio_control::volume_change_result` を通す。ワーカー側で書き直すと、経路によって上限や解除の有無が変わる
- **ワーカーはコマンドを 1 つずつ処理するので、デバイスを開いている最中（数百 ms）は待たされる。** 最小化中の操作なので許容している。ここが気になるなら、リスナーから `AudioControls` を直接書く形もありうるが、そのときも「UI へ何を返すか」は同じ問題が残る

### 画面が要るものは復帰したときに畳む

保留した押下は `ListenerState::pressed`（`BTreeMap<HotkeyAction, u32>`）に**回数で**溜まる。復帰したときに `take_pressed` が `folded_repeats` で畳む。

- トグル（フルスクリーン / 最前面表示 / ミュート）は**奇数回なら 1 回、偶数回なら実行しない**。2 回押して戻したつもりが復帰時に切り替わる、という食い違いを避ける
- スクリーンショットは何回押されていても 1 枚。撮るのは復帰後の映像なので、何枚撮っても同じ絵になる
- 音量の上げ下げは押した回数ぶん効かせる（最小化中はワーカーが実行するので、ここへ来るのは通常のフレームで溜まった分だけ）

**デバウンス（200ms）はリスナー側で行う**（`ListenerState::record_press`）。以前は `take_pressed`、つまり実行の時点で計っていたが、最小化中は実行が UI スレッドを通らないため、押しっぱなしのキーリピートを落とせない。記録の時点で計れば、どちらの経路でも同じ間引きが効く。
