# ログと panic

コンソールが無いこのアプリでデバッグ出力をどこへ出すか、レベルをどう使い分けるか。
`panic = "abort"` の結果として `catch_unwind` が効かないことと、その代わりに何で担保するか。
目指す姿は `docs/ARCHITECTURE.md` の「エラーとログ」にある。

## 標準出力は届かない。ログは log クレートを使う

`src/main.rs` 冒頭に `#![windows_subsystem = "windows"]` があるためコンソールが存在せず、`println!` / `eprintln!` の出力はどこにも届かない。**デバッグ目的で `println!` を足さないこと。**

代わりに `log` クレートのマクロ（`error!` / `warn!` / `info!` / `debug!` / `trace!`）を使う。`main()` の先頭で `logging::init()` を呼んでおり、出力先は設定ファイルの隣。

```
%AppData%\capturecard_viewer\logs\capturecard_viewer-YYYYMMDD-HHMMSS.log
```

- **1 回の起動につき 1 ファイル。** 起動時に新しいものから 10 個だけ残して古い世代を削除する。同じ秒に 2 つ起動した場合は `_1` から始まる連番が付き、互いのログが混ざらない
- レベルは環境変数 `CAPTURECARD_VIEWER_LOG`（`error` / `warn` / `info` / `debug` / `trace`、既定 `info`）。解釈できない値は `info` に倒れる。設定ファイルには持たせていない
- `panic = "abort"` で終了時のフラッシュが走らないため、1 行ごとにフラッシュしている
- `logging::init()` が失敗してもアプリは起動する。ログが無いだけで機能には影響しない

レベルの使い分け。

| レベル | 使う場面 |
|---|---|
| `error` | 復旧できない失敗。ストリームのエラー、設定の保存失敗、ホットキーの登録失敗 |
| `warn` | 続行できるが想定外。フォールバックした、ロックを取れなかった |
| `info` | 状態の遷移。デバイスの接続、キャプチャの開始と停止、確定した設定値 |
| `debug` | 経過の詳細。デバイスの探索、スレッドの起動と終了 |
| `trace` | 毎フレーム・毎イベント流れるもの。ホットキーイベントの受信、2 秒ごとの再適用、フレームの到着 |

**アプリ本体に `println!` / `eprintln!` は 1 つも残っていない。** 足し直すと CI で落ちる。`Cargo.toml` の `[lints.clippy]` で `print_stdout` / `print_stderr` を `warn` にしてあり、CI は `-D warnings` で clippy を回すため。

**例外はテストコードの中。** テストバイナリの標準出力は `cargo test -- --nocapture` で読めるため、`println!` を使ってよい（`src/video/convert.rs` の計測用テストがその例）。`src/main.rs` 冒頭の `#![cfg_attr(test, allow(clippy::print_stdout))]` がこれを許している。**クレートルートに置いてあるのは、テストを持つモジュール側に `#[allow]` を散らかさないため。**

**デバイス起因の不具合を調べるときは `.claude/skills/device-debug/SKILL.md` の手順に従う。** ログの読み方、正常時の所要時間の目安、症状ごとの確認順をまとめてある。

## catch_unwind は使わない

`Cargo.toml` の `[profile.release]` に `panic = "abort"` があるため、`std::panic::catch_unwind` は release ビルドで一切機能しない。パニックが起きればプロセスごと落ちる。

以前は `update()` の中で `ViewportCommand` の送出と `apply_settings` を `catch_unwind` で囲み、失敗を警告に落としているように見えていた。**実際には守っておらず、読む側に「ここはパニックしても続く」と誤解させるだけなので削除した。** 代わりに、囲んでいた処理がパニックしないことを確認してある（`unwrap` / `expect` / 添字が無く、`Mutex::lock()` の失敗も `if let Ok` で受けている）。

**パニックしうる処理を書かない側で担保すること。** `Option` と `Result` は `unwrap` せずに分岐し、ロックの失敗はログに残して諦める。`catch_unwind` を足しても release では効かない。
