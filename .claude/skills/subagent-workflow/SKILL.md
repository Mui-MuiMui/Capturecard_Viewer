---
name: subagent-workflow
description: 指示役から Issue を渡されて並行開発するサブエージェントの共通手順。前提ファイルの読み方、ブランチの切り方、コミットと PR、CodeRabbit 対応、マージ前の確認、してはいけないこと、最終報告の書式（20 行以内）。サブエージェントとして作業を始めるとき、依頼文に「この skill に従う」と書かれているときに参照する。末尾に指示役が使う依頼文の雛形がある。
---

# サブエージェントの共通手順

指示役（オーケストレーター）から Issue を 1 つ渡され、worktree の中で独立して作業するサブエージェント向けの手順。**依頼文に書かれるのはタスク固有の部分だけで、共通の手順はこのファイルが持つ。**

書式と基準は既にある skill が持っている。**ここには再掲せず参照する。**

| 何を知りたいか | どこを読むか |
|---|---|
| ブランチ名、コミットの粒度と書式、PR タイトル / 本文の決まり | `.claude/skills/naming-conventions/SKILL.md` |
| 検証コマンドと順番、落ちたときの扱い | `.claude/skills/verify/SKILL.md` |
| テストを書く場所と方針 | `.claude/skills/testing-conventions/SKILL.md` |
| 実装中の判断（範囲、コミットの積み方） | `.claude/commands/cv/implement.md` |
| セルフレビューの観点 | `.claude/commands/cv/review.md` |
| PR 作成とレビュー対応の手順 | `.claude/commands/cv/pr.md` |

このファイルが持つのは、**サブエージェントとして動くときにだけ要る話**（前提の読み方、他エージェントとの棲み分け、してはいけないこと、報告の書式）に限る。

## 1. 前提を読む

作業を始める前に、この 4 つを読む。

1. `CLAUDE.md`（`@GUARDRAIL.md` で `GUARDRAIL.md` を取り込んでいる。**取り込みを解釈しないなら 2 つとも自分で読む**）
2. `.claude/skills/naming-conventions/SKILL.md`
3. `.claude/skills/verify/SKILL.md`
4. `.github/pull_request_template.md`

テストを書く担当なら `.claude/skills/testing-conventions/SKILL.md` も先に読む。テストの置き場所・境界値・異常系の決まりはそちらが持っている。**ドキュメントだけを触る担当では読まなくてよい。**

`docs/design/*.md` は**担当領域に関係するものだけ**読む。どれを読むかは `CLAUDE.md` の「設計の理由はどこにあるか」の表から選ぶ。14 本あるので全部読まない。

Issue の本文は `gh issue view <番号>` で読む。**本文の `file:line` は起票時点のスナップショットなので、着手前に実コードで裏を取る。**

## 2. ブランチを切る

worktree の中で行う。**worktree は指示役が用意しているので、自分で作らない。`cd` でリポジトリ本体へ移動しない。**

```bash
git fetch origin
git checkout -b <type>/<説明> origin/dev
```

- **起点は `origin/dev`。** worktree が古いコミットを指していることがあるので、`git log --oneline origin/dev -1` で確認してから切る
- ブランチ名の規則は `naming-conventions` skill の「ブランチ名」。依頼文でブランチ名を指定されていればそれに従う
- worktree のブランチ名に `worktree-` が付いている場合は `git branch -m` で改名する

## 3. 実装中の決まり

`.claude/commands/cv/implement.md` の「実装中に守ること」と `GUARDRAIL.md` に従う。それに加えて、並行開発だから要るものを挙げる。

- **各コミットで verify skill の 4 段が通る状態にする。** 壊れた状態を積むと、他エージェントが `dev` を取り込んだときに巻き添えになる
- **`#[cfg(test)] mod tests` は 1 ファイルに 1 つだけ。** 並行 PR がそれぞれ同じファイルの末尾へ `mod tests` を足すと、マージ後に二重定義でビルドが落ちる。判定ロジックは純粋関数に切り出し、既存の `mod tests` の中へテストを足す
- **依頼文に書かれた「他エージェントの領域」には触らない。** ついでの整形や、気になった箇所の修正も含めて触らない
- **範囲外の発見には手を出さない。** 最終報告の「起票を提案する Issue」に 1 行で書く
- 一時ファイルは worktree の中か `/tmp` に置き、終わったら消す。リポジトリ本体や `%AppData%` に置かない
- **force push しない。既存のコミットを書き換えない**（amend / rebase を含む）。理由は `naming-conventions` skill の「履歴を作り直さない」
- **アプリを実行するときは設定ファイルを退避し、終了後に戻す。** 起動しただけで上書きされる

```bash
mv "$APPDATA/capturecard_viewer/config/default-config.toml" "$APPDATA/capturecard_viewer/config/default-config.toml.agent-bak"
# 実行と確認
mv "$APPDATA/capturecard_viewer/config/default-config.toml.agent-bak" "$APPDATA/capturecard_viewer/config/default-config.toml"
```

## 4. コミットする

書式は `naming-conventions` skill の「コミットメッセージ」。本文に次の 2 行を入れる。

```text
Refs: #<Issue 番号>

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
```

`Closes` / `Fixes` などのクローズ用キーワードは使わない（`GUARDRAIL.md`、理由は `naming-conventions` skill の「`Closes` ではなく `Refs` を使う」）。

## 5. PR を出す

**手順は `.claude/commands/cv/pr.md` の「初回」をそのまま踏む。** マージ先が `dev` であること、本文を `.github/pull_request_template.md` の見出しに沿って自分で並べること、`Refs #<番号>` を使うこと、末尾に `🤖 Generated with [Claude Code](https://claude.com/claude-code)` を付けることは、すべてそちらと `naming-conventions` skill にある。ここには再掲しない。

サブエージェントとして足すのは 1 点だけ。

**PR 本文と Issue コメントが詳細の置き場所になる。** 最終報告は 20 行に絞るので（9 節）、変更の要点・確認手順・CodeRabbit 対応の経緯はここへ書き切る。特に Issue コメントの「人間が dev で確認すること」は、マージ後に人がなぞる唯一の手順になるため、**操作手順と期待結果の形**で書く。

## 6. CI と CodeRabbit を確認する

```bash
gh pr checks <番号> --watch
```

CI が起動しないときは PR を閉じて開き直すと発火する。

```bash
gh pr close <番号> && gh pr reopen <番号>
```

CodeRabbit のレビューは PR 作成から 2〜3 分後に届く。**読み方と対応の仕方は `.claude/commands/cv/pr.md` の「レビュー対応」に従う**（取得するコマンド、追加のコミットで積むこと、採用しない指摘に理由を返信すること）。

サブエージェントとして足すのは、**判断が分かれる指摘は直さずに最終報告の「判断を仰ぐ点」へ回す**こと。指示役に確認せず方針を決めない。

既知の誤検知。出ても採用しない。

| 指摘 | 採らない理由 |
|---|---|
| Docstring Coverage | `///` ではなく `//` の日本語コメントで「なぜ」を書く方針。`.coderabbit.yaml` で `pre_merge_checks.docstrings.mode: "off"` にしてあるが、それでも出ることがある |
| フィールド単位の `#[serde(default)]` の追加 | 設定構造体は**構造体レベル**で付けてある。フィールドに付けるとその型の `Default`（bool なら false）へ倒れ、構造体の `Default` に書いた既定値が効かなくなる（`docs/design/settings.md`） |
| ファイルの移動だけの PR に対する挙動変更の提案 | 移動と挙動の変更を混ぜると、差分が「移動のみ」として読めなくなる。別 Issue として提案する |

## 7. マージ前に確認する

指示役へ報告する直前に行う。

```bash
git merge origin/dev
```

1. 最新の `origin/dev` を取り込む（`git fetch origin` を先に）
2. `mod tests` の二重定義が無いことを確認する。**各ファイル 1 以下**

```bash
grep -c "mod tests" src/*.rs src/app/*.rs
```

3. verify skill の 4 段を通す
4. push する

衝突したときは**両方を残す形で解決する。** 他エージェントの変更を消さない。解決の仕方に迷ったら、消さずに残したうえで最終報告の「判断を仰ぐ点」に書く。

## 8. しないこと

指示役の仕事なので、サブエージェントは行わない。

- **PR のマージ**（auto-merge の有効化も含む）
- **Issue のクローズ**（PR 本文にクローズ用キーワードを書いて自動で閉じさせることも含む）
- **Project の Status 変更**
- **Issue の新規起票。** 必要だと思ったら最終報告の「起票を提案する Issue」に書く
- **`CHANGELOG.md` への内部リファクタの記載。** 書くのはユーザーから見える変更だけ（`GUARDRAIL.md`）
- **`main` へ向けた PR**
- **依頼文で指定された他エージェントの領域への変更**

## 9. 最終報告の書式

**20 行以内。厳守。日本語で書く。** この 5 項目だけを書く。

```text
PR: #<番号> <URL>
判断を仰ぐ点: （無ければ「なし」）
未確認事項: （実機が要るものなど。無ければ「なし」）
触っていない領域: （依頼文で指定されたもの）
起票を提案する Issue: （あれば 1 行ずつ。無ければ「なし」）
```

**変更の要点・確認手順・CodeRabbit 対応の詳細を報告に再掲しない。** それらは PR 本文と Issue コメントに書いてある。指示役は PR と Issue を読めるので、報告に写すとコンテキストを二重に使うだけになる。

項目ごとの粒度。

- **判断を仰ぐ点** — 方針が分かれてどちらも選べた箇所、指示役の判断が要る積み残し。PR 本文の「判断を仰ぐ点」と同じ内容を 1 行に圧縮する
- **未確認事項** — CI では確かめられないもの。実機が要るなら `docs/MANUAL-TEST.md` のどの項目かを名指しする
- **触っていない領域** — 依頼文で指定された他エージェントの領域を、そのまま書き戻す。触っていないことの確認になる
- **起票を提案する Issue** — 範囲外として見送った発見。1 行 1 件で、何をどうするかが分かる粒度にする

失敗して PR まで到達できなかった場合も同じ書式で、`PR:` に「未作成（理由）」と書く。

## 10. 依頼文の雛形

指示役がサブエージェントへ渡す依頼文。**共通の手順はこの skill が持つので、書くのはタスク固有の部分だけ。** 10〜20 行に収める。

```text
あなたは Capturecard_Viewer（Rust / eframe）の <担当領域> 担当です。
この skill（.claude/skills/subagent-workflow/SKILL.md）に従ってください。

担当: Issue #<番号> <タイトル>
まず `gh issue view <番号>` を読むこと。

実装の指針:
- <タスク固有の方針。どう直すか、どこを変えるか>
- <採ってほしくない案があればその理由も>

ブランチ: <type>/<説明>

同時進行の他エージェントの領域（触らないこと）:
- <エージェント名 / Issue 番号 / 対象ファイル>

<モデル固有の注意があれば 1〜2 行>
```

- **「実装の指針」以外はほぼ定型。** 手順・報告の書式・してはいけないことを依頼文に書き足さない。書き足すとこの skill と二重管理になり、片方が古くなる
- **「同時進行の他エージェントの領域」は必ず書く。** 空なら「なし」と書く。書き忘れるとサブエージェントは衝突を避けられない
- **モデル固有の注意**は、Sonnet / Haiku に渡すときに範囲を狭める指示などに使う。無ければ行ごと省く
