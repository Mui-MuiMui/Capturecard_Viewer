---
name: naming-conventions
description: このリポジトリのブランチ名・コミットメッセージ・PR タイトル/本文の書き方。ブランチを切るとき、コミットするとき、PR を作るときに必ず参照する。「ブランチ名どうする」「コミットして」「PR 作って」といった依頼で使う。
---

# 命名規則とコミット／PR の書き方

Capturecard_Viewer リポジトリの規約。ブランチ作成・コミット・PR 作成のいずれかを行う前に必ずこれに従う。

## 種別（type）

ブランチ名とコミットメッセージで共通して使う。

| type | 用途 | 対応する Asana セクション |
|---|---|---|
| `feat` | 新機能の追加 | 6. 機能拡充 |
| `fix` | 不具合の修正 | 3. バグ修正 |
| `perf` | 動作を変えない性能改善 | 4. パフォーマンス改善 |
| `refactor` | 動作を変えない内部構造の整理 | 5. リファクタリング |
| `docs` | ドキュメントのみの変更 | 2. ドキュメント整備 |
| `test` | テストの追加・修正 | 1. 開発基盤・CI |
| `ci` | CI 設定・ビルド基盤の変更 | 1. 開発基盤・CI |
| `chore` | 依存更新、skill、設定ファイルなど上記以外 | 1. 開発基盤・CI / 7. リリース・保守 |

迷ったら「ユーザーから見て動作が変わるか」で判断する。変わるなら `feat` か `fix`、変わらないなら `perf` / `refactor` / `chore`。

## ブランチ名

```
<type>/<英語のケバブケース>
```

- 英小文字・数字・ハイフンのみ。日本語や大文字は使わない
- 2〜4 語程度に収める。何を触るかが分かる名前にする
- Asana の gid は入れない（長すぎるため）。タスクとの紐づけは PR 本文で行う

```
fix/audio-passthrough-toggle
perf/frame-clone-removal
refactor/replace-println-with-log
docs/architecture
ci/github-actions
chore/bump-egui
```

避ける例: `fix/bug`（何の不具合か不明）、`update`（type がない）、`fix/音声パススルー`（日本語）

### worktree を使う場合

`EnterWorktree` は自動でブランチ名に `worktree-` を付けるため、作成直後に改名する。

```bash
git branch -m fix/audio-passthrough-toggle
```

## コミットメッセージ

```
<type>: <日本語の要約>

<空行>
<本文：なぜその変更が必要か、何をしたか>

Asana: <タスクの URL>

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
```

- 1 行目は日本語で、50 字程度まで。句点は付けない
- 「修正」「更新」だけで終わらせず、何をどうしたかを書く
- 本文は「何をしたか」より「なぜそうしたか」を優先する
- Asana タスクがある場合は `Asana:` 行に URL を入れる
- Claude が作成したコミットには `Co-Authored-By` を付ける

```
fix: 音声パススルーの無効化がストリームに反映されない問題を修正

audio_passthrough_enabled が出力ストリームのコールバックから
参照されておらず、チェックを外しても音が止まらなかった。
コールバック内でフラグを見て、無効時は無音を書き込むようにする。
リングバッファは消費し続けて溢れを防ぐ。

Asana: https://app.asana.com/1/1218412078016612/project/1218457296782693/task/1218459958806421

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
```

## PR タイトル

コミットメッセージの 1 行目と同じ形式にする。

```
fix: 音声パススルーの無効化がストリームに反映されない問題を修正
```

複数のコミットをまとめる PR では、代表する type を選んで全体を要約する。

## PR 本文

```markdown
## 概要

<この PR が何を解決するかを 1〜3 行で>

## 対応する Asana タスク

[<タスク名>](<タスクの URL>)

## 変更内容

- <変更点を箇条書きで>

## 確認方法

- <レビュワーが動作を確かめる手順。コードのみの変更なら「コード変更なし」等と明記>
```

- Asana タスクがない場合はセクションごと省かず「対応する Asana タスク: なし」と書く
- 末尾に `🤖 Generated with [Claude Code](https://claude.com/claude-code)` を付ける

## PR を出したあとにやること

1. Asana タスクに PR の URL をコメントする（双方向リンクにする）
2. マージされたらローカルを同期し、worktree とブランチを後片付けする

```bash
git checkout main && git pull --ff-only
git worktree remove .claude/worktrees/<name>
git branch -D <branch>
git push origin --delete <branch>
```

3. Asana タスクを完了にする

## 1 つの PR に入れる範囲

- **整形のみの変更は必ず単独の PR にする。** `cargo fmt` はリポジトリ全体に差分を出すため、他の変更と混ぜるとレビューが不可能になる
- 依存クレートのメジャー更新（特に egui）は単独の PR にする。破壊的変更の追跡が必要なため
- バグ修正とリファクタリングは分ける。壊れたときの切り分けができなくなる
