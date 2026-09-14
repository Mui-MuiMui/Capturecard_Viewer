# 開発に参加する

不具合報告、機能の提案、Pull Request を歓迎します。

## 不具合を報告する

[Issue](https://github.com/Mui-MuiMui/Capturecard_Viewer/issues) から報告してください。

**このアプリはキャプチャーボードとオーディオデバイスに強く依存します。** 開発者の手元にあるデバイスは限られているため、環境情報がないと再現も原因の特定もできません。Issue テンプレートの項目をできるだけ埋めてください。

報告の前に [docs/TROUBLESHOOTING.md](docs/TROUBLESHOOTING.md) を確認してください。既知の不具合と回避策が載っています。

現状このアプリはログを出力しないため、**再現手順の具体性が原因究明の頼り**になります。

## 機能を提案する

Issue に用途と併せて書いてください。「何ができるようになりたいか」が分かると判断しやすくなります。

判断の軸は [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) の「設計の前提」に書いてあります。優先順位は **低遅延 > 単体で動く > 原因が追える > 画質** です。遅延を増やす変更や、外部ランタイムを必要とする変更は慎重に検討します。

## Pull Request を送る

### 事前に

大きな変更は、先に Issue で方針を相談してもらえると手戻りが減ります。誤字修正や小さな不具合修正は、直接 PR で構いません。

### 開発環境

[docs/BUILD.md](docs/BUILD.md) を参照してください。Windows と MSVC ツールチェイン、`rc.exe` のための Windows SDK が必要です。

### 守ってほしいこと

**ブランチ名とコミットメッセージの書式**

```
<type>/<英語のケバブケース>        ブランチ名の例: fix/audio-passthrough-toggle
<type>: <日本語の要約>            コミットの例:   fix: 音声パススルーの無効化を反映する
```

`type` は `feat` / `fix` / `perf` / `refactor` / `docs` / `test` / `ci` / `chore` から選びます。詳細は [.claude/skills/naming-conventions/SKILL.md](.claude/skills/naming-conventions/SKILL.md) にあります。

**送る前に通すこと**

```bash
cargo fmt --check
```

```bash
cargo clippy --all-targets
```

```bash
cargo build --release
```

```bash
cargo test
```

`cargo fmt --check` と `cargo clippy` は、リポジトリ全体では既知の差分と警告が残っています。**自分が触った箇所について、新たな差分や警告を増やしていないこと**を確認してください。

**テスト**

方針は [.claude/skills/testing-conventions/SKILL.md](.claude/skills/testing-conventions/SKILL.md) にあります。

不具合を直すときは、**再現するテストを先に書いて落ちることを確認してから**直してください。後から書くと、そのテストが本当に不具合を捕まえていたか分かりません。

デバイスを必要とするテストは `#[ignore]` を付けて、理由に必要な機材を書いてください。

**実機での確認**

映像・音声・デバイス接続・ホットキー・ウィンドウ操作に関わる変更は、コードを読むだけでは確認できません。[docs/MANUAL-TEST.md](docs/MANUAL-TEST.md) の該当項目を実機で確認し、確認した項目と環境を PR に書いてください。

**1 つの PR に入れる範囲**

- 整形のみの変更は単独の PR にしてください。全体にかけると差分が大きすぎてレビューできません
- 依存クレートのメジャー更新、特に egui は単独の PR にしてください
- 不具合修正とリファクタリングは分けてください。壊れたときの切り分けができなくなります

**ドキュメント**

README に書かれている挙動を変えたら、README も合わせて直してください。`docs/MANUAL-TEST.md` の「既知の不具合により失敗する項目」を解消したら、その節から外して上のチェックリストへ移してください。

### 言語について

コードのコメント、UI の文字列、コミットメッセージは日本語で書いています。Issue と PR はどちらでも構いません。

## AI の利用について

このプロジェクトは開発に生成 AI を活用しています。README にも明記しています。

リポジトリには [Claude Code](https://claude.com/claude-code) 向けの設定が含まれます。

- `CLAUDE.md` — リポジトリの手引き
- `.claude/skills/` — 命名規則とテスト方針
- `.claude/commands/cv/` — 計画・実装・レビュー・PR 作成の各段階

AI を使うかどうかは自由です。使わない場合でも、上記のファイルは規約をまとめた文書として参照できます。

## ライセンス

このプロジェクトは [LICENSE](LICENSE) の条件で公開されています。Pull Request を送った時点で、その内容が同じライセンスで公開されることに同意したものとみなします。
