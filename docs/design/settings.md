# 設定の永続化

設定ファイルを壊さないための serde の決まり、書きすぎないためのデバウンス、
読み込みに失敗したファイルを上書きしないための `AutoSavePolicy`。
設定ダイアログ側の決まりは `docs/design/settings-dialog.md` にある。

## 設定構造体の `#[serde(default)]` を外さない

`AppSettings` と配下の 4 構造体には、構造体レベルで `#[serde(default)]` が付いている。これが無いと、項目を 1 つ足すだけで既存ユーザーの設定が失われる。`Option` 以外の項目はパース自体が失敗して全項目が初期化され、`Option` の項目は `None` になって `Default` に書いた既定値が効かなくなる。**設定の構造体を新しく足すときも必ず付けること。**

読み込みに失敗した場合、`AppSettings::load()` は壊れたファイルの `.bak` への退避を試みてから既定値で起動する。退避は失敗することがある（保存先のパスが取れない、rename が拒否される）。読み込みの失敗・退避の成否・退避先のパスはログに残る。

**空のファイル（0 バイトか空白だけ）も読めなかったものとして退避する**（Issue #317）。全ての構造体に `#[serde(default)]` があるため空の文字列は TOML として読めてしまい、退避されないまま全設定（プリセット・ホットキー・デバイス名）が黙って既定値に戻っていた。保存の途中で止まったファイルがこの形になる。

`load()` は `(AppSettings, LoadOutcome)` を返す。`LoadOutcome` は退避まで含めて成功したかを表し、**退避できなかった場合に起動時の書き戻しを止めるためにある。** 退避に失敗すると読めなかったファイルがディスクに残るので、そこへ既定値を `save()` すると証跡ごと潰れる。`CaptureCardViewer::new` の起動時保存は `may_write_defaults_on_startup()` で守ってあるので、**起動経路に `save()` を足すときは同じ判断を通すこと。** 設定ダイアログからの明示的な保存は、ユーザーの意思なので抑止していない。

### 読めなかったことを利用者へ知らせる（Issue #505）

以前は読めなかったこと・退避したこと・自動保存を止めたことをログにしか残さず、利用者は設定が初期化されたことにも、ウィンドウ位置や音量が保存されなくなったことにも気づけなかった。いまは起動後に `report_error(ErrorSource::Settings, ..)` でトーストと設定ダイアログの「接続状態」タブ（「設定ファイル」の枠）へ出す。

- **設定を読むのは `main.rs` の 1 回だけ。** 読んだ設定と `LoadOutcome` を `CaptureCardViewer::new` へ渡す。アプリを作るときにもう一度読むと、1 回目で退避したあとは「ファイルが無い」を読んで `Loaded` になり、知らせる材料が消える（以前はそうなっていた）
- `LoadOutcome` は理由（`LoadFailure`）と、退避先（`FellBackToDefaults`）または残したファイル（`BrokenFileLeftBehind`）を持つ。`LoadOutcome::failure` が `SettingsError::LoadFellBackToDefaults` / `LoadFailedFileKept` にし、文言はその `Display` が `crate::i18n` から引く。理由も種類で持つ（空のファイルは `LoadFailure::Blank`）。読むのは画面の言語を決める前なので、文言にするのは知らせるときにする
- 読むのを 1 回にしたので、退避できなかったときにアプリを作るところでもう一度退避を試みることは無くなった。そのまま `BrokenFileLeftBehind` で自動保存を止め、次の起動で退避を試みる
- 知らせるのは `main.rs` が `apply_language` を呼んだ直後の `report_startup_settings`。**`new()` の中では文言を作らない**（言語を決める前なので、英語の設定でも既定の日本語で出る。`new()` で決めないのは #256）。起動時の書き戻しも `new()` で保存するが、結果は型のまま `pending_startup_save` に預け、取り込む（失敗のログとトースト）のは `report_startup_settings` の中
- 書き戻しの失敗と読み込みの知らせは同じ `ErrorSource::Settings` で、`ErrorCenter` は直近の 1 件しか持たない。後から出す読み込みの知らせが書き戻しの失敗の記録を上書きする（書き戻しの失敗はログに残り、保留として再試行される）
- **読み込みの知らせは `ErrorCenter` とは別に `CaptureCardViewer::settings_load_notice` にも残す。** `ErrorCenter` の記録は、あとの保存の失敗に上書きされ、保存の成功（`note_settings_save_result` の `errors.clear(ErrorSource::Settings)`）で消える。そのままだと、起動時の書き戻しに失敗したあとで保存できたときに「初期化した」の知らせまで消える。「接続状態」タブの「設定ファイル」の枠には、この知らせと直近の失敗を並べて出す（同じものなら 1 度だけ）。利用者が「設定が初期化された」ことを後から読めるほうを優先した
- **トーストには優先度を付けない。** 後勝ちなので、直後に接続の失敗が届くと上書きされる（`docs/design/error-reporting.md`）。「接続状態」タブに残るのでそこで読み返せる。トーストは定型文と合わせて 60 文字で切れ、英語は定型文が長いので残りは 20 文字ほどしかない。読めなかったことは定型文が言うので繰り返さず、起きたこと（「既定値に初期化した」「自動保存を止めた」、英語は "Reset to defaults" / "Auto-save stopped"）を先頭の短い 1 文にし、退避先と理由は後ろに置く（タブには全文が出る）
- 退避できずに自動保存を止めた知らせは、「適用」「OK」で保存して自動保存を再開したときに取り下げる（`errors.clear(ErrorSource::Settings)` と `settings_load_notice` を消す）。退避して既定値で起動した知らせは、保存しても事実は変わらないので、終了するまで起きた時刻と一緒に残す
- `AutoSavePolicy` と `may_write_defaults_on_startup` の判定は変えていない

## 1 項目が読めなくても設定全体を失わせない

設定ファイルは手で書き換えられる。serde は 1 項目でも読めないとパース全体を失敗にするので、そのままでは綴りを 1 つ誤っただけで、ファイルが `.bak` へ退避されて全設定が既定値に戻る。そこで、手で書き換えやすい項目は `deserialize_with` の補助を通し、**その項目だけ**既定値か範囲の端にして WARN を残す。共通の読み方は `src/settings/lenient.rs`、各項目の `deserialize_*` はその設定と同じファイルに置く。

| 補助 | 項目 | 拾うもの |
|---|---|---|
| `choice` | `[video]` `color_space` / `color_range` / `backend` / `convert`、`[audio]` `input_source`、`[screenshot]` `destination` / `format`、`[ui]` `language` | 知らない文字列、文字列でない値 → 既定値 |
| `integer` + `clamp_integer` | `[video]` `brightness` / `contrast` / `saturation`、`[audio]` `buffer_ms`、`[recording]` `video_bitrate_kbps` / `replay_seconds` / `audio_offset_ms`、`[screenshot]` `jpeg_quality` | 範囲外 → 範囲の端、整数でない値（`90.0` も） → 既定値 |
| `integer` + 近い選択肢 | `[recording]` `audio_bitrate_kbps` | 選択肢に無い値 → 近い選択肢、整数でない値 → 既定値 |
| `number` + `clamp_finite` | `[ui]` `volume`、`[screenshot]` `sound_volume` | 範囲外 → 範囲の端、NaN・無限大と数値でない値 → 既定値（整数は数値として読む） |

**値の型の違い（`color_space = 5`、`jpeg_quality = 90.0`、`volume = "loud"` など）も、2026-10 から（#531）その項目だけ既定値にする。** それまでの補助は値を `String` / `i64` / `f32` で受けていたので、型が違うと補助へ届く前に失敗し、ファイルごと退避されていた。いまは `toml::Value` で受けてから型を確かめる。

**構造体レベルの `#[serde(default)]` との関係。** `#[serde(default)]` が拾うのは**キーが無い**場合だけで、キーがあって値が読めない場合は拾わない。上の補助は後者を拾う。両方を合わせて、表の項目は「無い・読めない・範囲外」のどれでも、その項目だけが既定値になる。補助を足したからといって構造体レベルの `#[serde(default)]` を外さないこと（キーが無い場合が拾えなくなる）。

**補助を通していない項目は、型が違うと今までどおりパース全体が失敗して退避される**（`LoadOutcome::FellBackToDefaults`、退避できなければ `BrokenFileLeftBehind`）。真偽値（`always_on_top` など）、文字列とパス（`device_name`、`save_folder` など）、配列（`resolution`、`last_window_size` など）、`[hotkeys]` の表、`[[presets]]` の `name`、TOML として読めないファイルがそう（プリセットの中の `video` / `audio` は同じ型なので、表の項目は寛容に読む）。真偽値や名前は手で書き換えても型を誤りにくく、配列や表は型だけ直しても意味を取り違えやすいので、退避して利用者に知らせる（「読めなかったことを利用者へ知らせる」）ほうを残した。項目を寛容にしたくなったら、表に足して `lenient` を呼ぶ。両方の場合は `lenient.rs` のテストが押さえている。

## 置き場所の差し替え

設定ファイルの置き場所は既定で `%AppData%\capturecard_viewer\config\default-config.toml`（`dirs::config_dir()` の下。1.2.x まで使っていた confy 0.6 と同じ場所で、ずらすと既存の設定ファイルを見失う。`docs/DEPENDENCIES.md` の第 3 段）。環境変数 `CAPTURECARD_VIEWER_CONFIG_DIR` にフォルダを指定すると、`<フォルダ>\default-config.toml` を使い、ログもそのフォルダの `logs\` に出す（`src/config_path.rs`、Issue #290）。複数のエージェントが並行してアプリを起動したとき、全員が同じ `%AppData%` の設定ファイルを読み書きして退避と復元が交差し、設定が壊れたため足した。開発者向けで、設定ファイルには保存しない。

- **絶対パスだけを使う。** 相対パスはカレントディレクトリ基準にも exe の置き場所基準にもせず、WARN を出して既定の置き場所へ倒す（`docs/design/assets.md`）
- フォルダが無ければ作る。作れない（同じ名前のファイルがあるなど）ときも既定の置き場所へ倒して WARN を出す
- 環境変数を読むのは最初の問い合わせの 1 回だけで、以後は同じ結果を使う。設定とログで置き場所が食い違わないようにするため
- 使っている設定ファイルのパスはログの先頭近くに出る。差し替えたときと差し替えを使えなかったときは WARN

**設定ファイルのパスを `config_path` 以外から引かないこと。** `AppSettings::load` / `save` とログの置き場所は `config_path` を通しているので、`dirs::config_dir()` から直接組み立てるような経路を足すと差し替えが効かなくなる。

## 設定の保存はデバウンスされる

ウィンドウの移動・リサイズ、音量スクロール、コンテキストメニューの各操作は、その場ではディスクへ書かない。`mark_settings_dirty()` で変更を記録し、`update()` の末尾の `flush_settings_if_due()` が最後の変更から 2 秒空いたところでまとめて書き出す。終了時は `on_exit` が保留の有無にかかわらず書き出す。

**設定を書き換える処理を足すときは `AppSettings::save()` を直接呼ばず `mark_settings_dirty()` を使うこと。** 直接呼ぶと、ウィンドウをドラッグしている間ずっと毎フレーム TOML を書き出す元の問題に戻る。

例外は 2 つ。起動時の書き戻し（`may_write_defaults_on_startup()` で守られている）と、設定ダイアログの「適用」「OK」（`src/app/settings_dialog.rs` の `apply_dialog_action`）。後者はユーザーの明示的な保存操作なので即座に書き出す。

## 保存は一時ファイルを経由して置き換える

`AppSettings::save()` は、同じフォルダの一時ファイル（`default-config.toml.<乱数>.tmp`）へ書いて `sync_all` でディスクへ書き切り、`rename` で本来のファイルと置き換える（`src/settings/write.rs` の `write_atomically`、Issue #317）。confy 0.6 の `store_path` で本来のファイルへ直接書いていたころは、`truncate` で開いてから書くため、書き込み中の強制終了や電源断で 0 バイトか書きかけのファイルが残った。rename なら、ディスクに残るのは古い内容か新しい内容のどちらかになる。

- 「その他」タブの「設定を書き出す」（`src/settings/transfer.rs` の `export_to`）も同じ処理を通す。一時ファイルはユーザーが選んだ書き出し先と同じフォルダに置く（Issue #361）
- 一時ファイルを同じフォルダに置くのは、rename が同じボリュームの中でだけ置き換えとして働くため
- 書く内容は `toml` 1.x の `to_string_pretty`（`serialize_settings`）。読み込みも同じ `toml` 1.x（`parse_settings`）。1.2.x まで使っていた confy 0.6（内部は `toml` 0.8）と書き出す内容はバイト単位で同じで、互いに読める（`docs/DEPENDENCIES.md` の第 3 段、Issue #302）
- 開いた一時ファイルのハンドルへそのまま書き、`sync_all` してから閉じて rename する。書き込みのためにパスから開き直さない
- 保存（`save`）は設定ファイルのフォルダが無ければ作る。初回起動ではまだ無いため。書き出し（`export_to`）は作らない（ユーザーが選んだフォルダ）
- 失敗したら一時ファイルを消す。元のファイルは手付かずで残る
- 一時ファイルは乱数入りの名前で `create_new`（排他的に作る）で作り、消すのは自分で作ったものだけにする。もともと同じ名前の `.tmp` があっても上書きも削除もしない（Issue #369）。起動時に前回の残りを消す処理は無い
- 読み込み（`load`）はファイルが無ければ既定値を返すだけで、ファイルは作らない。起動時の書き戻しが作る（confy 0.6 の `load_path` は読み込みの中で既定値を書いていた）。読めなかった理由は `toml` の位置（行と列）と理由を 1 行にまとめる

### 保存の失敗が続く間は間隔を伸ばし、知らせるのは初回だけ

ディスク満杯・権限なし・他のプロセスのロックでは、再試行しても書けないことが多い。以前は 2 秒ごとに再試行して毎回 `error!` を出し、画面には何も出さなかった。いまは `src/app/settings_store.rs` の `SaveFailureStreak` が続けて失敗した回数と直前の理由を持つ（フィールドは `app/mod.rs`）。

- **再試行の間隔は 2 → 4 → 8 → 16 → 32 → 60 秒で頭打ち**（`save_retry_delay`）。直ったあとも 1 分以内に書ける
- **同じ理由の失敗は初回だけ `error!` と `report_error(ErrorSource::Settings, ..)`。** 以降は `debug!` に落とす。理由が変わったら知らせ直す（手の打ち方が変わるため）
- **間引くのは自動保存（デバウンス保存・終了時の保存・起動時の書き戻し）だけ。** 設定ダイアログの「適用」「OK」（`save_settings_now`）の失敗は、同じ理由でも毎回 `error!` とトーストで知らせる。押した結果が何も出ないと保存できたのか分からないため。`ErrorCenter` の 60 秒の間引きも、記録を消してから記録し直して外す。自動保存は `save_settings_automatically` を通す
- 保存できたら回数を戻し、失敗を知らせていたなら `errors.clear(ErrorSource::Settings)` で取り下げる。書き出し・読み込みの失敗も同じ発生源だが、直近の 1 件しか持たないので区別しない
- `AppSettings::save()` 自身はログを出さず `SettingsError` を返す。起動時の書き戻しも `note_settings_save_result` を通す

### 壊れた設定ファイルが残っている間は自動保存しない

読み込みに失敗した設定ファイルを `.bak` へ退避できなかった場合（`LoadOutcome::BrokenFileLeftBehind`）、読めなかったファイルがディスクに残る。**起動時の書き戻しだけを止めても足りない。** ウィンドウを動かせば 2 秒後のデバウンス保存が、何もしなくても `on_exit` が、同じファイルを既定値で上書きしてしまう。

そのため `CaptureCardViewer` は `settings::AutoSavePolicy` を持ち、`mark_settings_dirty` / `flush_settings_if_due` / `on_exit` をこれで守っている。止めている間は**ウィンドウの位置・サイズや音量も永続化されない。** 設定を取り戻す手段を残すほうを優先している。

解除するのは設定ダイアログの「適用」「OK」で保存に成功したときだけ（`AutoSavePolicy::note_explicit_save`）。その時点で壊れたファイルはユーザーの意思で置き換わっているため、守る対象がもう無い。**自動保存の経路を足すときは `AutoSavePolicy::is_allowed()` を通すこと。**
