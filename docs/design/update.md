# 更新の確認（と、次の段階の適用）

GitHub の Release から新しい版を見つけて知らせる仕組み。Issue #240 を 3 本の PR に分けて進めている。

| 段階 | 中身 | 状態 |
|---|---|---|
| 1 | Release に単体の exe と `SHA256SUMS.txt` を添付する（Issue #239） | 済み（PR #241） |
| 2 | 検知、通知ダイアログ、設定 `[update]`、「その他」タブの「更新」の欄。「更新する」はリリースページを開くまで | この文書の本体 |
| 3 | ダウンロード、照合、差し替え、再起動 | 未着手。末尾の「次の段階への申し送り」 |

置き場所は `src/update/mod.rs`（問い合わせと判断の純粋関数）、`src/update/overrides.rs`（試すための環境変数）、`src/app/update.rs`（スレッドと結果の取り込み、通知ダイアログの操作）、`src/ui/update_dialog.rs`（通知ダイアログの描画）、`src/ui/other_tab.rs`（「更新」の欄）。

## 検知

`GET https://api.github.com/repos/Mui-MuiMui/Capturecard_Viewer/releases/latest` を認証なしで 1 回呼ぶ。

- **`User-Agent` を必ず付ける。** GitHub の API は無い要求を拒否する。`capturecard_viewer/<版>` にしてある。`Accept: application/vnd.github+json` と `X-GitHub-Api-Version` も付ける
- 使うのは `tag_name` / `html_url` / `body` / `draft` / `prerelease` / `assets[].name` / `assets[].browser_download_url` だけ。知らない項目は読み飛ばす
- タグ（`v1.2.0`）から先頭の `v` を外して `semver` で読み、`CARGO_PKG_VERSION` と比べる。**新しい正式版のときだけ「更新あり」。** 同じ版・古い版（ダウングレード）・`1.2.0-rc.1` のような pre-release のタグは「最新」として扱う（`update::is_newer_stable`）。`/latest` は draft と pre-release の印を付けた Release を元から除くが、印を付け忘れたものまで勧めないよう、タグの形と `draft` / `prerelease` の値でも弾く
- タグが版として読めなければ失敗として扱う（`UpdateError::InvalidTag`）
- **`html_url` はそのままブラウザへ渡さない。** `https://github.com/Mui-MuiMui/Capturecard_Viewer/releases/` で始まるときだけ使い、それ以外は最新のリリースページにする（`release_page_url`）
- 失敗の理由は `UpdateError` の変種で分ける。404 は「公開されたリリースが無い」、403 / 429 は認証なしの問い合わせ回数の上限（1 時間 60 回）、タイムアウト、その他の HTTP、接続の失敗、JSON を読めない、の 7 つ。文言は `Display` から `crate::i18n` を呼んで出す（`docs/design/error-reporting.md`）

### HTTP と TLS

`ureq` を `native-tls` で使う。**TLS は Windows の schannel で、証明書は OS の証明書ストアで確かめる**（`RootCerts::PlatformVerifier`）。rustls と ring は入れない。社内のプロキシのように OS 側で信頼している証明書にも従うため。

- **フィーチャは `native-tls` にすること。** `native-tls-no-default` だけでは ureq が native-tls を無効とみなし、問い合わせの時点でパニックする。release ビルドは `panic = "abort"` なのでアプリごと落ちる。コンパイルは通ってしまうので、ureq のフィーチャや TLS の設定を触ったら `#[ignore]` のネットワークのテスト（`cargo test check_latest_release_reaches_github -- --ignored`）を一度は通すこと
- `native-tls` は同梱のルート証明書（`webpki-root-certs`、CDLA-Permissive-2.0）も引き込む。使わないが外せない（`docs/DEPENDENCIES.md`）
- 上限は問い合わせ全体（名前解決・接続・応答の読み取り）で 5 秒（`REQUEST_TIMEOUT`）。失敗の文言にも秒数を書いてあるので、変えたら合わせる

## スレッド

**起動時に 1 回だけ**、最初の `update()` の「起動直後に 1 度だけ行う処理」から始める。「その他」タブの「更新を確認」も同じ入口（`start_update_check`）を通る。

- 確認ごとにスレッドを 1 本起こし（名前は `update-check`）、結果を mpsc で返す。`update()` の先頭の `drain_update_results` が取り込む。ログ・トースト・状態の更新は UI スレッドで行う。効果音の読み込み（`app::screenshot_sound`）と同じ形で、届いたら `RepaintWaker` で UI スレッドを起こす
- **ネットワークだけを触るスレッドなので、「デバイスに触る使い捨てのスレッド」の禁止（`GUARDRAIL.md`）には当たらない。** デバイスワーカーへ載せないのは、ワーカーのコマンドの列が 5 秒のネットワーク待ちで塞がるため
- `JoinHandle` は捨てず `on_exit` で join する。問い合わせは上限の 5 秒で必ず終わるので、**起動直後に閉じたときに待たされるのも最大 5 秒**。切り離すとプロセスの終了と競合するので、待つほうを取った
- **確認中は重ねて始めない。** 「更新を確認」は確認中は押せず、入口でも弾く。起動時の確認と手動の確認がぶつかることも無いので、要求に番号は振っていない
- 誰が始めたか（`CheckOrigin::Startup` / `Manual`）を結果に載せて返す。ダイアログを出すのは起動時の確認だけ

## 失敗の扱い

**起動を止めない。** 失敗は WARN のログと `report_error(ErrorSource::Update, 理由)`（トーストと「接続状態」タブ）、「更新」の欄の「確認できない: 理由」に出すだけ。確認できたら `errors.clear(ErrorSource::Update)` で取り下げる。

ネットワークの無い環境では、起動のたびに「更新を確認できません」のトーストが 1 回出る。邪魔なら「起動時に更新を確認する」を外してもらう。

## 設定 `[update]`

`settings::UpdateSettings`。構造体レベルの `#[serde(default)]` で、`[update]` の無い設定ファイルでは既定値になる（`docs/design/settings.md`）。

| 項目 | 既定 | 意味 |
|---|---|---|
| `check_on_startup` | true | 起動時に確認するか。偽なら起動時は何もせず、「更新を確認」だけが使える |
| `notify_on_startup` | true | 起動時の確認で見つかったときダイアログを出すか。偽なら「更新」の欄に出すだけ |
| `skipped_version` | なし | 「この版は通知しない」を選んだ版（`1.2.0` の形）。同じ版のあいだはダイアログを出さない |

- **プリセットには入れない**（`docs/design/presets.md`）
- 2 つのチェックは「その他」タブだけで変わるので、`commit_draft` は無条件に反映する
- **`skipped_version` は通知ダイアログからも変わる**ので、`auto_reconnect` と同じく**ドラフトで変わったときだけ**反映する。無条件に入れると、設定ダイアログを開いたまま通知ダイアログで飛ばした版が「適用」で消える（`docs/design/settings-dialog.md`）
- 通知ダイアログの「この版は通知しない」は共有の設定へ書いて `mark_settings_dirty()`。ドラフトには書かないので、設定ダイアログを開いたままなら、欄の表示は開き直すまで古い
- `skipped_version` の比べ方は `update::is_skipped`。手で書かれた `v1.2.0` や前後の空白も同じ版として扱い、読めない値はどの版も指さない

## 通知ダイアログ

`ui::show_update_dialog`。映像の上に出す `egui::Window` で、Id は `update_dialog` に固定（`docs/design/i18n.md`）。**状態を持たず、押されたものを `UpdateDialogEvent` で返す。** 出すかどうかと、出す版（`UpdateCheck`）は `app::update` の `UpdateState` が持つ。設定ダイアログの `SettingsDialogState` に置かないのは、設定ダイアログを開いていなくても出るため。

出すのは次の 3 つが揃ったとき（`update::should_notify_on_startup`）。

1. 起動時の確認で新しい版が見つかった
2. `notify_on_startup` が真
3. `skipped_version` がその版ではない

| ボタン | 動作 |
|---|---|
| 更新する | その版のリリースページを開いて閉じる。ブラウザの起動は `egui::Context::open_url`（eframe が `webbrowser` で開く） |
| 後で（× も同じ） | 閉じるだけ。次の起動でまた出る |
| この版は通知しない | `skipped_version` に入れて閉じる |

リリースノートの要約は `update::summarize_notes`。**本文の最初の `### ` の見出しより前**を採る。このリポジトリの Release の本文は「概要の段落 → 折りたたみの中に `### 追加` などの一覧」の形なので、概要だけが出る。HTML のタグだけの行（`<details>` / `<summary>`）と空行は落とす。見出しが無い、または見出しより前が空なら先頭の 5 行。長いときはダイアログの中でスクロールする。

## 「その他」タブの「更新」の欄

現在の版、「更新を確認」、結果（まだ確認していない / 確認中 / 最新 / 新しい版あり / 確認できない）、「リリースページを開く」、2 つのチェック、飛ばした版と「解除」。

- 結果はドラフトではなく実行中のアプリの状態（`UpdateStatus`）で、`show_settings_dialog` の引数で読み取り専用に渡す
- 「更新を確認」は `SettingsEvent::CheckForUpdates` を返す。チェックと「解除」は差し替えたあとのドラフトの `update` を `SettingsEvent::SetUpdateSettings` で返し、反映は「適用」「OK」
- 「リリースページを開く」は egui のリンク（`hyperlink_to`）。見つかった版があればそのページ、無ければ最新のリリースページ

## 次の段階への申し送り

第 3 段階（ダウンロード・照合・差し替え・再起動）で守ること。Issue #240 のコメントでユーザーが決めた内容を含む。

- **資産名は `capturecard_viewer-<tag>-windows-x64.exe` と `SHA256SUMS.txt`**（`docs/RELEASE.md` の「配布物」）。`UpdateCheck::assets` から名前で引く。この段階では使わず、見つかったときにログへ名前を出すだけにしてある
- `SHA256SUMS.txt` は `sha256sum` の形式（`<64 桁の小文字の 16 進>  <ファイル名>`、BOM なし、LF）。**照合は大文字小文字を区別せずに比べる**（生成側は小文字だが、手で作り直した版が大文字でも弾かない）。SHA-256 は `sha2` を足して計算する
- **ダウンロード先は exe と同じフォルダ**（`%TEMP%` ではない）。`capturecard_viewer.exe.new` のような一時名で落とし、照合が済んでから改名する
- **ダウンロードを始める前に、exe のフォルダに書けるかを確かめる**（一時ファイルを作って消す）。書けなければダウンロードせず、「このフォルダには書き込めないため自動更新できません。リリースページから手動で更新してください」を出して「リリースページを開く」に倒す（Program Files など）
- **途中で失敗したら（ディスク満杯、照合の不一致、改名の失敗）`.new` を消し、元の exe をそのまま残す。** 理由はトーストとログに出す。**元の exe を壊す経路を作らない**
- 差し替えは、実行中の exe を `.old` へ改名してから（Windows は実行中でも改名できる）`.new` を元の名前へ改名し、新しい exe を起動して自分は通常の `on_exit` を通って終わる。次の起動で `.old` を消す
- ダウンロードは UI とデバイスワーカーのどちらとも別のスレッドで行い、進捗はチャネルで返す。更新中もデバイスワーカーは止めない
- 通知ダイアログの「更新する」と「その他」タブに「更新する」を足すのはこの段階。いまの「更新する」（リリースページを開く）は、書き込めないときの逃げ道として残す

## 試すための環境変数

実際に新しい Release が無くても通知の流れを確かめられるよう、フェイクデバイス（`CAPTURECARD_VIEWER_FAKE_DEVICES`）と同じ流儀で 2 つの環境変数を読む（`update::CheckOverrides`）。**起動するときだけ指定する開発者向けのもので、設定ファイルには保存しない。**

| 環境変数 | 値 | 意味 |
|---|---|---|
| `CAPTURECARD_VIEWER_UPDATE_CURRENT_VERSION` | `1.0.0` / `v1.0.0` | 比較に使う「いまの版」を差し替える。公開済みの最新より古い版を入れれば、その最新が「新しい版」として知らされる。通知ダイアログの「いまは vA.B.C」と「その他」タブの「現在の版」もこの版になる |
| `CAPTURECARD_VIEWER_UPDATE_API_URL` | `http://` / `https://` の URL、または `file://` の URL | Release API の代わりに問い合わせる先。`file://` なら Release の JSON（`releases/latest` の応答と同じ形）のファイルをそのまま読む。`file:///C:/work/latest.json` と `file://C:/work/latest.json` は同じ。URL の符号化（`%20`）は解かない |

- 読むのは起動時に 1 回だけ（`UpdateState::new`）。どちらかが効いていれば「更新の確認のテスト用のオーバーライドが有効」を WARN でログに残す
- 解釈は純粋関数（`CheckOverrides::from_env_values`）。空や空白だけの値は指定していないのと同じ。読めない値（版として読めない、`http://` / `https://` / `file://` のどれでもない）は WARN を残して使わず、通常の確認に倒す
- 問い合わせ先を差し替えても、判断（版の比較、pre-release の扱い、`html_url` の確認）は通常と同じ関数を通る。**よそのリポジトリの Release を指したときは `html_url` が弾かれ、「リリースページを開く」は本物の最新のリリースページになる**
- 次の段階では、ローカルの HTTP サーバーやテスト用のリポジトリの Release に向けて、ダウンロードと照合を試すのに使う

## 選ばなかった案

| 案 | 採らなかった理由 |
|---|---|
| 定期的な確認（起動中に何時間おきなど） | 起動時 1 回と手動で足りる。認証なしの API は 1 時間 60 回までなので、回数も増やしたくない |
| rustls（ureq の既定） | ring と同梱のルート証明書を増やす。Windows 専用アプリなので schannel と OS の証明書ストアで足りる |
| デバイスワーカーで問い合わせる | 5 秒のネットワーク待ちでデバイスのコマンドの列が塞がる |
| 起動を待って確認する | ネットワークの無い環境で起動が遅れる。確認は裏で行い、結果が来たら知らせる |
