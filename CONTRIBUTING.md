# コントリビューションについて

**現時点で外部のコントリビューターは募集していません。** コードの変更を伴う提案は、作者の手が回らず取り込めないことがあります（README.en.md の "This project is not currently looking for contributors." と同じ立場です）。

一方で**不具合の報告は歓迎します。** このアプリはお使いのキャプチャーボードとオーディオデバイスに強く依存し、作者が試せる機材は限られています。手元でしか起きない現象の報告は、それ自体が貴重な情報です。日本語・英語のどちらでも構いません。

- 不具合の報告 — [Issues](https://github.com/Mui-MuiMui/Capturecard_Viewer/issues) の「不具合の報告」テンプレート
- 機能の提案 — 同じく「機能の提案」テンプレート
- 報告の前に [docs/TROUBLESHOOTING.md](docs/TROUBLESHOOTING.md) を確認してください。既知の不具合と回避策をまとめてあります

このファイルは**人が読む入口**です。「どこに何が書いてあるか」と、迷いやすい決まりだけを扱います。手順そのものは各ドキュメントにあり、ここには再掲しません。

## ドキュメントの地図

| 置き場所 | 内容 |
|---|---|
| [README.md](README.md) / [README.en.md](README.en.md) | 使い方と設定項目の説明。利用者向け |
| [CHANGELOG.md](CHANGELOG.md) | 変更履歴。**リポジトリ直下**にあります |
| [docs/BUILD.md](docs/BUILD.md) | ビルド手順、必要なツール、バージョン番号の扱い、プロファイル設定 |
| [docs/RELEASE.md](docs/RELEASE.md) | 版を切って GitHub Release を出すまでの手順 |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | 目指すアーキテクチャと現状との差分。設計の前提（低遅延 > 単体で動く > 原因が追える > 画質） |
| [docs/DEPENDENCIES.md](docs/DEPENDENCIES.md) | 依存クレートの状況と更新方針、ライセンス一覧の生成手順 |
| [docs/MANUAL-TEST.md](docs/MANUAL-TEST.md) | 実機での手動テストチェックリスト |
| [docs/TROUBLESHOOTING.md](docs/TROUBLESHOOTING.md) | 既知の不具合と回避策。利用者向け |
| `CLAUDE.md` / `.claude/` | コードの構造と作業時の注意点、検証・命名・テスト・リリースの各手順 |

## ビルドと検証

Windows 10/11 専用です。他の OS では Media Foundation と WASAPI が使えないためビルドも実行もできません。必要なツール（MSVC ターゲットの Rust ツールチェインと Windows SDK）と手順は [docs/BUILD.md](docs/BUILD.md) にあります。

| コマンド | 何をするか |
|---|---|
| `cargo build --release` | 配布と同じ最適化で実行ファイルを作る。成果物は `target/release/capturecard_viewer.exe` |
| `cargo fmt --check` | 整形漏れを検出する。基準はリポジトリ直下の `rustfmt.toml` |
| `cargo clippy --locked --all-targets -- -D warnings` | 静的解析。警告が 1 件でも失敗する。`println!` の混入もここで落ちる |
| `cargo build --locked --release` | 上と同じ release ビルドを、コミット済みの `Cargo.lock` に固定して行う |
| `cargo test --locked` | 自動テスト。実機が必要なテストには `#[ignore]` が付いており走らない |

`--locked` はコミット済みの `Cargo.lock` をそのまま使わせるためのものです。付けないと `Cargo.toml` と食い違っていても勝手に再解決され、手元と違う依存で通ってしまいます。

CI（`.github/workflows/ci.yml`）は上の表の下 4 行（fmt → clippy → release ビルド → test）を**この順で**回します。手元でも同じ順で通してから PR を出してください。**順番の理由と、落ちたときの扱いは [.claude/skills/verify/SKILL.md](.claude/skills/verify/SKILL.md) が持ちます。**

キャプチャーデバイスが必要な確認（映像・音声・デバイス接続・ホットキー・ウィンドウ操作）は CI では担保できません。該当する変更をしたときは [docs/MANUAL-TEST.md](docs/MANUAL-TEST.md) のチェックリストを実機でなぞります。テストを書くかどうかの線引きは [.claude/skills/testing-conventions/SKILL.md](.claude/skills/testing-conventions/SKILL.md) にあります。

## ブランチとコミット

作業ブランチ（`<type>/<説明>`）→ `dev` → `main` の 3 段です。

- **作業ブランチは `dev` から切り、PR も `dev` へ向けます。** `main` へ入れるのはリリースのときだけです
- ブランチ名は `fix/audio-passthrough-toggle` のように `<type>/<英小文字のケバブケース>`。日本語や Issue 番号は入れません
- **コミットメッセージは日本語**で、1 行目は `<type>: <要約>`。「修正」「更新」で終わらせず、何をどうしたかを書きます
- 対応する Issue があれば本文に `Refs: #<番号>` を入れます。**`Closes` / `Fixes` などのクローズ用キーワードは使いません**
- 各コミットはビルドとテストが通る状態にします。レビュー指摘への対応は元のコミットを直さず追加のコミットで積み、push 済みの履歴を force push で作り直しません

クローズ用キーワードを禁じているのは、GitHub の自動クローズが**既定ブランチ（`main`）に入った瞬間**に働くためです。コミットメッセージに紛れていると、リリースで `dev` → `main` を入れたときに、作者がまだ実機で確認していない Issue までまとめて閉じてしまいます。**Issue 番号の前に置いてよいのは `Refs` だけ**と覚えてください。

**種別（type）の一覧、マージ戦略、1 つの PR に入れる範囲は [.claude/skills/naming-conventions/SKILL.md](.claude/skills/naming-conventions/SKILL.md) にあります。**

## バージョン番号

- **出どころは `Cargo.toml` の `version` だけです。** `build.rs` がそこからヘッダーを生成し、`app.rc` を通して exe のバージョンリソースへ流し込みます。**他のファイルに数値を書かないでください**（[docs/BUILD.md](docs/BUILD.md) の「バージョン番号」）
- **バージョンを上げるのはリリースのときだけです。** 通常の PR で `Cargo.toml` の `version` を触らないでください。上げ方・`CHANGELOG.md` の切り出し・タグの打ち方は [docs/RELEASE.md](docs/RELEASE.md) にまとまっています
- `Cargo.lock` にはこのパッケージ自身の version も入っています。依存やバージョンを触ったら `cargo check` で `Cargo.lock` を更新し、同じコミットに含めてください。忘れると `--locked` を付けた CI が落ちます

## CHANGELOG

**変更履歴は[リポジトリ直下の `CHANGELOG.md`](CHANGELOG.md)** です。`docs/` の下ではありません。書式は [Keep a Changelog](https://keepachangelog.com/ja/1.1.0/)、バージョン番号は[セマンティック バージョニング](https://semver.org/lang/ja/)に従います。

- **機能追加・不具合修正・挙動が変わる変更の PR では、`## [未リリース]` 節に 1 行足します。** 分類は 追加 / 変更 / 非推奨 / 削除 / 修正 / セキュリティ
- **ユーザーから見える変更だけを書きます。** 内部のリファクタリングは、挙動が変わらないなら書きません。ドキュメントのみの変更も同じです
- 版の節（`## [1.0.7] - YYYY-MM-DD`）へ切り出すのはリリースのときだけです。リリースワークフローがこの見出しを目印に本文を抜き出して Release の説明にするため、**見出しの形を崩さないでください**
- 細かい方針は `CHANGELOG.md` 末尾の「記入の方針」にあります

## Issue と PR

- Issue は `.github/ISSUE_TEMPLATE/` のテンプレートから起票します。キャプチャーボードの製品名・Windows の版数・アプリの版数が無いと再現できないことがほとんどなので、埋められる欄は埋めてください。設定ファイルのパスにはユーザー名が含まれるので、伏せて構いません
- PR の本文は [.github/pull_request_template.md](.github/pull_request_template.md) の見出しに沿って書きます。**マージ先は `dev`** です。既定の候補が `main` になっていることがあるので毎回確認してください
- 改善のバックログは GitHub Issues で管理し、進行状況は GitHub Project の Status（未着手 / 作業中 / レビュー待ち / 人間確認待ち / 完了）で見ます。分野は `area:` ラベル、優先度は `P1`〜`P3` ラベルです（`CLAUDE.md` の「タスク管理」）
- **Issue を閉じるのは、作者が実機で確認したときです。** PR がマージされた時点では閉じません

優先度の目安。

| ラベル | 目安 |
|---|---|
| `P1` | ユーザーに実害がある、または他の作業の前提になる |
| `P2` | 直すべきだが回避策がある、影響が限定的 |
| `P3` | あると良い、余裕があれば |

## AI を使った開発について

このリポジトリは [Claude Code](https://claude.com/claude-code) を使って開発しています。リポジトリの構造・設計判断・作業時の注意点は `CLAUDE.md` に、検証・命名・テスト・リリースの各手順は `.claude/` 以下にまとめてあります。

これらは AI 向けに書いてありますが、**内容は人が読んでも通る規約文書**です。この `CONTRIBUTING.md` は人向けの入口として要点と道筋だけを示し、詳細は重複させずにそちらを参照しています。
