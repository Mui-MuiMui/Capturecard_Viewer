---
name: verify
description: このリポジトリの検証手順。fmt / clippy / build / test をどの順で回し、落ちたときに何をするか。コミット前、PR を出す前、レビュー時に使う。「検証して」「ビルド通る」「テスト回して」「CI 通りそう」といった依頼で使う。
---

# 検証手順

Capturecard_Viewer の変更を手元で検証する手順。**`.github/workflows/ci.yml` と同じコマンドを同じ順で回す。** 手元で通ったものが CI で落ちる状態を作らないため。

## 順番

上から順に実行する。**途中で失敗したらそこで止めて報告する。** 後続を飛ばして「だいたい通った」と報告しない。

```bash
cargo fmt --check
```

```bash
cargo clippy --locked --all-targets -- -D warnings
```

```bash
cargo build --locked --release
```

```bash
cargo test --locked
```

fmt と clippy を先に置いてあるのは、release ビルドが `lto` と `codegen-units = 1` の影響で時間がかかるため。整形漏れや警告で落ちるなら、待つ前に分かったほうがよい。

### `--locked` を付ける理由

コミット済みの `Cargo.lock` をそのまま使わせる。付けないと `Cargo.toml` と食い違っていても勝手に再解決され、手元と違う依存で通ってしまう。CI も release ワークフローも同じ理由で付けている。

依存やバージョンを触って `Cargo.lock` を更新していないと、ここで「lock ファイルの更新が必要」で落ちる。**これは検出であって不具合ではない。** `cargo check` で lock を更新し、`Cargo.toml` と同じコミットに入れる。

## 落ちたときの扱い

| 段 | 前提 | 落ちたら |
|---|---|---|
| `cargo fmt --check` | 差分ゼロ | 自分の変更を `cargo fmt` で整形し、**追加のコミット**として積む。無視してよい差分はない |
| `cargo clippy` | 警告ゼロ | 直す。直さないなら理由を報告する。`#[allow(..)]` で黙らせるなら、なぜ許容するかをコメントに書く |
| `cargo build --release` | 成功 | dev ビルドで通ったからと飛ばさない。`panic = "abort"` や `lto` の影響で release でしか出ない失敗がある |
| `cargo test` | 全て成功 | **テストを消す、`#[ignore]` を付ける、アサーションを緩める、で通さない。** 原因を直す |

整形のみの差分が広い範囲に出た場合は、本筋の変更と混ぜずに単独の PR にする（`.claude/skills/naming-conventions/SKILL.md` の「1 つの PR に入れる範囲」）。

## ここに含まれないもの

- **`cargo test -- --ignored`** — キャプチャーデバイスが必要で、CI でも走らない。実機必須のテストを足したなら、その旨を報告してユーザーに実行を依頼する
- **実機での動作確認** — `docs/MANUAL-TEST.md` のチェックリスト。映像・音声・デバイス接続・ホットキー・ウィンドウ操作に触れる変更は、原則としてコードの検証だけでは足りない

## 報告

- 4 つそれぞれの結果。通ったものも通ったと書く
- 落ちた段があれば、出力と、直したか直さない理由か
- ユーザーに実機確認を依頼する項目（`docs/MANUAL-TEST.md` のどの項目かを名指しする）
