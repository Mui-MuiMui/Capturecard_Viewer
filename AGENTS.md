# AGENTS.md

AI エージェント向けの入口です。**このファイルは道案内だけを持ちます。** 規約と手順は下のファイルにあり、ここには再掲しません（片方だけが古くなるのを避けるため）。

作業を始める前に、少なくとも `CLAUDE.md` と `GUARDRAIL.md` の 2 つを読んでください。

| 読むもの | 内容 |
|---|---|
| [CLAUDE.md](CLAUDE.md) | リポジトリの手引き。プロジェクト概要、モジュール構成、タスク管理、開発フロー |
| [GUARDRAIL.md](GUARDRAIL.md) | してはいけないこと / 必ずすること。理由は書かず、参照先だけを添えた一覧 |
| [CONTRIBUTING.md](CONTRIBUTING.md) | 人向けの入口。作業の流れ、ブランチとコミット、CHANGELOG、Issue と PR |
| [docs/design/](docs/design/) | 設計判断の理由と経緯。テーマ別。`GUARDRAIL.md` の各項目の参照先 |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | 目指す構造と現状との差分 |
| [.claude/skills/verify/SKILL.md](.claude/skills/verify/SKILL.md) | ビルドと検証の手順。**コマンドと順番はここが持ちます** |

`CLAUDE.md` は Claude Code が `@GUARDRAIL.md` で `GUARDRAIL.md` を取り込む形になっています。**この記法を解釈しないツールを使う場合は、2 つのファイルを自分で読んでください。**
