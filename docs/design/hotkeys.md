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

アクションを増やすときは `HotkeyAction` に variant を足し、`ALL` / `as_str` / `label` の 3 か所と、`src/app/hotkeys.rs` の `run_hotkey_action` を埋める。**`as_str` の文字列は設定ファイルに書かれるので、一度出した名前は変えない。** 実行は右クリックメニューや映像上の操作と同じメソッドを呼ぶこと（`set_always_on_top` / `adjust_volume` / `reconnect_devices` / `toggle_fullscreen`）。独自に書くと、同じ操作なのに設定の保存やオーバーレイ表示の有無が経路で変わる。

登録は `HotkeyManager::apply` が差分だけ行う。2 秒ごとに呼ばれるため、無条件に登録し直すとその瞬間の入力を取りこぼす。登録できなかったものは `errors()` に残り、設定画面に理由が出る。**直るまで毎回試し直すが、ログに出すのは理由が変わったときだけ**（同じ失敗が 2 秒ごとに積もらないように）。

**`HotkeyManager::apply` を直接呼ばないこと。** 呼ぶのは `CaptureCardViewer::apply_hotkey_assignments` だけで、そこで `report_error(ErrorSource::Hotkey, ..)` によるトースト通知と、全て登録できたときの `ErrorCenter::clear` を行っている。直接呼ぶとこれらが抜ける。設定画面の一覧に出す見出しも `ErrorSource::Hotkey.headline()` を使い、トーストと同じ文言に揃えてある。
