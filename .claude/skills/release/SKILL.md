---
name: release
description: 版を切って GitHub Release を出すときの進め方。バージョンの更新、CHANGELOG の切り出し、dev → main の PR、タグ push、公開後の確認。「リリースして」「版を切って」「バージョンを上げて」「タグを打って」といった依頼で使う。
---

# リリースの進め方

**手順とコマンドの実体は `docs/RELEASE.md`。このファイルに再掲しない。** ここに書くのは、Claude が進めるときの順番と、各段で確認すること、やらないこと。

タグを push したあとは `.github/workflows/release.yml` が引き取る。人が判断するのはタグを打つまで。

## 進め方

### 1. 前提を確認する

`docs/RELEASE.md` の「前提」を 1 つずつ突き合わせる。**満たしていないものがあればそこで止めてユーザーに確認する。**

- `dev` の CI が緑か（`gh run list --branch dev --limit 1`）
- 手元の検証（`.claude/skills/verify/SKILL.md`）
- **`docs/MANUAL-TEST.md` の実機テストを実施したか。** これは Claude には実施できないため、必ず聞く
- `CHANGELOG.md` の「未リリース」節に、この版に入る変更が書かれているか。`git log --first-parent origin/main..origin/dev` と突き合わせて、書き漏れがないか見る

### 2. 上げる版を決める

`docs/RELEASE.md` の「1. バージョンを上げる」の表で major / minor / patch を判断し、**どれにするかを理由とともに提示して承認を得てから**書き換える。勝手に決めない。

### 3. バージョンと CHANGELOG のコミットを作る

`dev` から作業ブランチを切り、`docs/RELEASE.md` の手順 1 と 2 を行う。ここまでは通常の PR と同じで、マージ先は `dev`。

確認すること:

- `Cargo.toml` と `Cargo.lock` の両方が更新され、同じコミットに入っているか
- `app.rc` を触っていないか
- CHANGELOG の見出しが `## [1.0.7] - YYYY-MM-DD` の形か。**日付は `<env>` の今日の日付を見る。記憶で書かない**
- 空の `## [未リリース]` を作り直したか
- 節の中身がユーザーから見える変更になっているか（内部のリファクタリングだけの行を混ぜない）

### 4. dev → main の PR を作る

`docs/RELEASE.md` の手順 3。**`--base main` を明示する。** 通常の PR は `dev` 向けなので、ここだけが例外。

**この PR にだけは `Closes` / `Fixes` が実際に効く。絶対に書かないこと。** 通常の `dev` 向け PR ではキーワードが無視されるのに対し、ここは既定ブランチ向けなのでそのまま発火し、実機確認の済んでいない Issue まで閉じる。含まれる変更は `Refs #<番号>` で並べるか、`CHANGELOG.md` の該当節を指すに留める。

同じ理由で、**`dev` に積まれたコミットメッセージに `Closes` が紛れていないか**も確認する。コミット側のキーワードは `main` に載った時点で発火する。

### 5. タグを打つ

**PR がマージされたことを確認してから。** `main` の最新を取得して打つ。

タグ push は Release の公開を起動する。**打つ直前にタグ名と `Cargo.toml` の version を読み上げ、承認を得てから push する。**

### 6. 結果を確認する

`docs/RELEASE.md` の手順 5。Release ができたら URL を報告する。**zip を展開して exe が起動するかの確認はユーザーに依頼する。** ビルドが通ったことと、配った物が動くことは別。

失敗していたら「ワークフローが失敗したとき」の表で切り分ける。**手動でリリースを出す前に、タグを打ち直せる状況か（Release がまだ無いか）を確認する。**

### 7. 後片付け

`docs/RELEASE.md` の手順 6（`main` を `dev` へ戻す）と、`area:release` ラベルの付いた Issue の更新（`~/.claude/skills/github-issues/SKILL.md`）。

## やらないこと

- **PR のマージ**（ユーザーが行う）
- **承認なしのタグ push。** Release の公開はやり直しが効かない
- 公開済み Release の削除、公開済みタグの付け替え
- `Cargo.toml` 以外の場所にバージョンを書く（`app.rc` に数値を直接書かない）
- CHANGELOG に無い変更を Release の説明に書き足す。説明は CHANGELOG の節をそのまま使う
- 前提が満たせていない状態で先へ進む
