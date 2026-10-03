# リリース手順

`dev` に溜まった変更を 1 つの版として切り出し、GitHub Release として公開するまでの手順。

タグを push すると `.github/workflows/release.yml` が動き、release ビルド → 配布する exe と `SHA256SUMS.txt` の作成 → Release の作成までを自動で行う。**人がやるのはバージョンの更新、`CHANGELOG.md` の整理、`dev` → `main` の PR、タグの push まで。**

Claude に手順をなぞらせる場合は `.claude/skills/release/SKILL.md` を使う。

## 全体像

```mermaid
flowchart TD
    check["前提の確認<br/>CI 緑 / 実機テスト / CHANGELOG"]
    bump["1. Cargo.toml の version を上げる<br/>Cargo.lock も更新する"]
    log["2. CHANGELOG の 未リリース を版に切る"]
    pr["3. dev → main の PR を作りマージする"]
    tag["4. main で v1.0.7 を打って push する"]
    action["5. release.yml が Release を作る"]
    back["6. main を dev へ戻す"]

    check --> bump --> log --> pr --> tag --> action --> back
```

## 前提

タグを打つ前に以下を満たしていること。

- **`dev` の CI が緑**（`.github/workflows/ci.yml`）。手元でも `.claude/skills/verify/SKILL.md` の検証を一通り通す
- **`docs/MANUAL-TEST.md` のチェックリストを実機で実施した。** 落ちてよいのは同ファイルの「既知の不具合により失敗する項目」に載っているものだけ
- **`CHANGELOG.md` の「未リリース」節に、この版に入る変更が書かれている**

## 1. バージョンを上げる

バージョンの出どころは `Cargo.toml` の `version` だけ（`docs/BUILD.md` の「バージョン番号」）。**`app.rc` は触らない。** `build.rs` が `version.h` を生成して exe のバージョンリソースへ流し込む。

[セマンティック バージョニング](https://semver.org/lang/ja/)に従って上げる。

| 変更の内容 | 上げる位置 |
|---|---|
| 設定ファイルの互換性が切れるなど、ユーザーの移行作業が要る変更 | major |
| 後方互換のある機能追加 | minor |
| 後方互換のある修正のみ | patch |

書き換えたら **`Cargo.lock` も更新する。** `Cargo.lock` には自分自身のパッケージの version が入っているため、更新せずに push すると CI と release ワークフローの `--locked` が「lock ファイルの更新が必要」で落ちる。

```bash
cargo check
```

`Cargo.toml` と `Cargo.lock` を同じコミットに入れる。

```
chore: バージョンを 1.0.7 に上げる
```

## 2. CHANGELOG を版に切る

`CHANGELOG.md` の `## [未リリース]` を、この版の見出しに書き換える。

```markdown
## [1.0.7] - 2026-09-19
```

日付はリリース日（JST）。書き換えたあと、その上に空の `## [未リリース]` 節を作り直す。

release ワークフローはこの見出しを目印に本文を抜き出して Release の説明にするため、**見出しの形（`## [1.0.7] - YYYY-MM-DD`）を崩さない。** 節が見つからないとワークフローはビルドの前に失敗する。

中身の書き方はファイル末尾の「記入の方針」に従う。ユーザーから見える変更だけを書き、内部のリファクタリングは挙動が変わらないなら書かない。

見出しの直後、最初の `### ` より前に**版の要約を 1〜3 行**書く。 要約の中で水平線を引くときは `---` ではなく `<hr>` を使う（ワークフローは `---` の行を節の終わりとみなす）。日本語と英語を併記する場合は `<hr>` で区切る。release ワークフローはこの要約だけを Release の説明の先頭に出し、`### 追加` 以降の一覧は `<details>` で折りたたむ（節が長いため）。要約が無い場合は全文をそのまま出す。

## 3. dev → main の PR を作る

```bash
gh pr create --base main --head dev --title "chore: 1.0.7 をリリースする"
```

- 作成直後に `gh pr comment <番号> --body "@coderabbitai review"` を投稿して CodeRabbit を手動で起動する（「fewer than 10 stars」で自動レビューが飛ばされるため）
- **`main` へ PR を出してよいのはこのときだけ。** 通常の PR は `dev` へ向ける（`.claude/skills/naming-conventions/SKILL.md`）
- マージは merge commit。squash も rebase も使わない
- CI が緑で、レビュースレッド（CodeRabbit の指摘も人のレビューも含めて全て）が解決されてからマージする。ルールセットが両方をマージの条件にしているので、未解決があるとマージできない。リリース PR で初めて出た指摘は、このリリースに含めるか別 Issue に回すかを決め、返信して Resolve する

## 4. タグを打つ

マージ後の `main` を取得してタグを打つ。

```bash
git checkout main && git pull --ff-only
git tag v1.0.7
git push origin v1.0.7
```

- **タグ名は `v<バージョン>`。** `Cargo.toml` の version と一致していないとワークフローが失敗する（`v1.0.7` ↔ `1.0.7`）
- ワークフローが受け付けるタグの形は `^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?\z`（大文字小文字を区別する）。`-` の後ろが付いたタグは pre-release として出る（「pre-release を出す」）。`v1.0.7+build` や `V1.0.7` はビルドの前に落ちる
- 1.0.6 以前のタグは `Ver1.0.6` の形式だったが、今後は小文字の `v` を使う。ワークフローの起動条件が `v*` なので、`Ver1.0.7` では何も動かない
- **タグは `dev` ではなく `main` で打つ**
- Release が作られる前に間違いに気付いたら、タグを消して打ち直してよい。公開後は打ち直さず、次の版で直す

**打ち直す前に、そのタグで動き出したワークフローが終わっているか止まっているかを確認する。** ワークフローは `cancel-in-progress: false` なので、タグを消しても走っている run は止まらない。走らせたまま打ち直すと、古いコミットからビルドした exe が、新しいコミットを指すタグの Release に添付されることがある。

```bash
gh run list --workflow release.yml --limit 3
gh run cancel <run-id>
```

```bash
git push origin :refs/tags/v1.0.7 && git tag -d v1.0.7
```

## 5. ワークフローの結果を確認する

`.github/workflows/release.yml` が以下を順に行う。

1. タグの形を確かめ、pre-release かどうかを決める。タグ名と `Cargo.toml` の version が一致するか確認する（どちらかが駄目ならここで失敗）
2. `CHANGELOG.md` から該当する版の節を抜き出す（無ければここで失敗）
3. `cargo build --locked --release`
4. `target/release/capturecard_viewer.exe` の SHA-256 を `SHA256SUMS.txt` に書く（exe は写さず、そのまま添付する）
5. `gh release create` で Release を作り、2 つの資産を添付して CHANGELOG の節を説明にする。pre-release のタグなら `--prerelease --latest=false` を付ける

**リリースのジョブでは cargo のキャッシュ（`Swatinem/rust-cache`）を使わず、毎回クリーンビルドする。** 配布して自動更新で全員へ届く exe に、復元した `target/` の依存物がそのままリンクされるのを避けるため。`SHA256SUMS.txt` は同じジョブでその exe から作るので、照合では混入を検出できない。その分ビルドに時間がかかるが、リリースは頻度が低いので受け入れる。CI（`ci.yml`）のキャッシュはそのまま使う。

```bash
gh run list --workflow release.yml --limit 1
gh release view v1.0.7
```

**Release に資産が 2 つ（exe・`SHA256SUMS.txt`）付いていることを確認する。** そのうえで **exe を実際に落として起動することを確認する。** ビルドが通ったことと、配った物が動くことは別。

## pre-release を出す

`v1.4.0-rc.1` のように `-` の後ろが付いたタグは、**拒否せず pre-release として出す。** ワークフローはタグに `-` が含まれるかだけで判定し、`gh release create` に `--prerelease --latest=false` を付ける。

- pre-release の印を付けた Release は GitHub の `/releases/latest` から外れる。**印を付けずに Latest で出すと、`/releases/latest` がその rc を返す。** アプリは rc を勧めない（`update::is_newer_stable`、`docs/design/update.md` の「検知」）ので、そのあいだは公開済みの正式版の更新も誰にも知らされなくなる。印はワークフローが付けるので、手で付け忘れることは無い
- 手順は正式版と同じ。`Cargo.toml` の version をタグから `v` を外した文字列（`1.4.0-rc.1`）にし、`CHANGELOG.md` に `## [1.4.0-rc.1] - YYYY-MM-DD` の節を作る。どちらかが無ければワークフローはビルドの前に落ちる
- 自動アップデートは pre-release を落とさない。試してもらう人には Release のページから exe を手で落としてもらう
- 手動で出す場合（「ワークフローが失敗したとき」）も、pre-release なら `gh release create` に `--prerelease --latest=false` を足す

## 配布物

**実行ファイル単体。** `icon.ico` と既定の効果音 `sound/SS.mp3` は exe に埋め込んであるため同梱しない（`docs/BUILD.md` の「配布時に同梱するもの」）。

Release には次の 2 つを添付する。1.2.0 までは zip も添付していたが、中身が exe と同じで二重に見えるため 1.2.0 の次の版からやめた。

```
capturecard_viewer.exe   人が落とす exe。自動アップデートもこれを落とす
SHA256SUMS.txt           exe の SHA-256
```

`SHA256SUMS.txt` は `sha256sum` と同じ形式で、1 行 1 ファイル（BOM なしの UTF-8、改行は LF）。

```
<64 桁の小文字の 16 進>  capturecard_viewer.exe
```

**exe の資産名にバージョンを入れない。** 自動アップデートは実行中の exe の名前を保ったまま差し替えるので（`docs/design/update.md` の「適用」）、資産名にバージョンがあると、ダウンロードした名前のまま使う人は更新のあとも古いバージョンの名前で新しいバージョンを動かすことになる。1.2.0 だけは `capturecard_viewer-v1.2.0-windows-x64.exe` の名前で出しており、自動アップデートは `capturecard_viewer.exe` が無ければこの旧名も探す。

**資産名（`capturecard_viewer.exe` と `SHA256SUMS.txt`）と `SHA256SUMS.txt` の形式は変えない。** 自動アップデート（Issue #240）が既に出た版からこの名前で読みにいくため、変えると古い版が更新できなくなる。1.2.0 の更新機能は旧名しか探さないので、1.2.0 から 1.2.1 への更新は手動になった（Issue #267）。

## 6. リリース後に main を dev へ戻す

`dev` → `main` をマージコミットで取り込むと、`main` に `dev` が持たないコミット（マージコミットそのもの）ができる。そのままにすると次のリリース PR の差分が読みにくくなるため、`main` を `dev` へマージして揃える。

`dev` はルールセットで PR 経由のマージしか受け付けないため、直接 push ではなく `main` → `dev` の PR を作ってマージする。差分はリリース PR のマージコミットだけなので、CI はそのまま通る。

```bash
gh pr create --base dev --head main --title "chore: 1.0.7 のリリース後に main を dev へ戻す" --body "Refs #<リリース PR の番号>"
```

早送りで済む場合はこの操作自体が不要になる。`git log --oneline dev..main` が空なら何もしなくてよい。

あわせて `area:release` ラベルの付いた Issue の状態を更新する。

https://github.com/Mui-MuiMui/Capturecard_Viewer/labels/area%3Arelease

## ワークフローが失敗したとき

| 症状 | 原因 | 対処 |
|---|---|---|
| タグ名が `v<バージョン>` の形式ではないと言われて落ちる | `+build` や大文字の `V` を付けた、`-` の後ろが空、など | タグを消し、「4. タグを打つ」の形で打ち直す |
| タグ名と version の不一致で落ちる | `Cargo.toml` の version を上げ忘れた、またはタグを打ち間違えた | タグを消し、`Cargo.toml` を直してから打ち直す |
| CHANGELOG に節が無いと言われて落ちる | 「未リリース」を版に切り忘れた | CHANGELOG を直してタグを打ち直す |
| `--locked` で lock の更新が必要と言われる | `Cargo.lock` を更新せずにコミットした | `cargo check` で更新してコミットし、タグを打ち直す |

タグを打ち直すときは、手順 4 の注意（走っているワークフローを先に止める）に従う。

Release がまだ作られていなければ、原因を直してタグを打ち直すのが一番簡単。**Release が作られたあとや、Actions 側の障害で通らない場合は手動で出す。**

```bash
cargo build --locked --release
```

`SHA256SUMS.txt` を作る（形式は「配布物」を参照）。exe は `target/release/capturecard_viewer.exe` をそのまま添付するので写さない。

```powershell
$hash = (Get-FileHash target/release/capturecard_viewer.exe -Algorithm SHA256).Hash.ToLowerInvariant()
[System.IO.File]::WriteAllText("$PWD\SHA256SUMS.txt", "$hash  capturecard_viewer.exe`n", (New-Object System.Text.UTF8Encoding $false))
```

`CHANGELOG.md` の該当する節を `release-notes.md` に書き出してから Release を作る。以下の `v1.0.7` は例なので、`Cargo.toml` の version に対応するタグに置き換える。`gh` はパスの末尾（`capturecard_viewer.exe`）をそのまま資産名にする。

```bash
gh release create v1.0.7 target/release/capturecard_viewer.exe SHA256SUMS.txt --title v1.0.7 --notes-file release-notes.md --verify-tag
```

既に Release がある状態で資産を差し替える場合は以下。exe を差し替えたら `SHA256SUMS.txt` も作り直して一緒に上げる。

```bash
gh release upload v1.0.7 target/release/capturecard_viewer.exe SHA256SUMS.txt --clobber
```

手動で出したあとは `release-notes.md` と `SHA256SUMS.txt` を消して、作業ディレクトリに残さない。
