# ビルド手順

Windows 10/11 専用。他の OS では Media Foundation と WASAPI が使えないためビルドも実行もできない。

## 必要なもの

| | 用途 |
|---|---|
| Rust ツールチェイン（MSVC ターゲット） | 本体のビルド |
| Visual Studio Build Tools + Windows SDK | `build.rs` が `embed_resource` で `app.rc` をコンパイルするために `rc.exe` を使う |

`rustup` の既定ターゲットが `x86_64-pc-windows-msvc` であることを確認する。

```bash
rustup show
```

GNU ツールチェインでは `embed_resource` のリソース埋め込みが期待どおりに動かない。MSVC を使う。

## ビルド

```bash
cargo build --release
```

成果物は `target/release/capturecard_viewer.exe`。

開発中は以下でよい。`[profile.dev]` に `opt-level = 1` を指定してあるので、最適化なしよりは映像が滑らかに動く。

```bash
cargo build
```

## バージョン番号

**出どころは `Cargo.toml` の `version` だけ。** バージョンを上げるときはここだけを書き換える。

`build.rs` が `CARGO_PKG_VERSION_*` から `OUT_DIR/version.h` を生成し、`app.rc` がそれを `#include` して exe のバージョンリソースに流し込む。**`app.rc` に数値を直接書かない。**

埋め込まれた値は PowerShell で確認できる。

```powershell
(Get-Item target/release/capturecard_viewer.exe).VersionInfo | Format-List FileVersion,ProductVersion,FileVersionRaw,ProductVersionRaw
```

`FileVersionRaw` と `ProductVersionRaw` は PowerShell が `FileVersionInfo` に足すプロパティで、`FileMajorPart` などの数値から組み立てられている。.NET の型そのものには無いため、API リファレンスを見ても載っていない。

値の入り方は以下のようになる。**現行バージョンとは無関係な例**であり、ここを実際のバージョンに合わせて更新する必要はない。

| `Cargo.toml` の `version` | `FileVersion`（文字列） | `FileVersionRaw`（数値） |
|---|---|---|
| `2.3.4` | `2.3.4` | `2.3.4.0` |
| `2.3.4-rc1` | `2.3.4-rc1` | `2.3.4.0` |

数値のバージョンは 16 bit 整数 4 つに限られるため、プレリリース識別子は文字列側にだけ入る。第 4 フィールドは常に 0。

`app.rc` を編集するときは、先頭の `#pragma code_page(65001)` を消さないこと。rc.exe は既定でシステムのコードページ（日本語環境では 932）としてファイルを読むため、これがないと UTF-8 の日本語コメントが 2 バイト文字と解釈されて改行を食い、直後の行まで巻き込む。

exe に埋め込まれる値の話はここまで。`CHANGELOG.md` の見出しとリリースのタグは別途更新する。バージョンを上げてから Release を出すまでの手順は `docs/RELEASE.md` にまとめてある。

## 検証

```bash
cargo fmt --check
```

```bash
cargo clippy --all-targets
```

```bash
cargo test
```

```bash
cargo test -- --ignored
```

- `cargo fmt --check` は差分ゼロが前提。落ちたら自分の変更を `cargo fmt` で整形する。整形の基準はリポジトリ直下の `rustfmt.toml`
- `cargo clippy --all-targets` は警告ゼロが前提。`-- -D warnings` を付けて実行すれば警告の混入を検出できる
- `--ignored` 付きのテストはキャプチャーデバイスを接続した状態で実行する

開発フローに沿って進める場合は `/cv:review` がこれらをまとめて実行する。

これらは GitHub Actions でも回る（`.github/workflows/ci.yml`）。`dev` / `main` への PR と push が対象で、ランナーは windows-latest。
CI が実行するのは `cargo fmt --check` → `cargo clippy --locked --all-targets -- -D warnings` → `cargo build --locked --release` → `cargo test --locked` の 4 つで、`--ignored` 付きの実機テストは走らせない。
加えて `THIRD-PARTY-LICENSES.txt` を生成し直し、コミット済みのものと一致するかを確認する。再生成の手順は `docs/DEPENDENCIES.md` の「ライセンス」にある。
clippy は `-D warnings` 付きで回すため、警告が 1 件でも増えると CI が落ちる。
`--locked` はコミット済みの `Cargo.lock` をそのまま使わせるため。付けないと `Cargo.toml` と食い違っていても勝手に再解決され、手元と違う依存で CI が通ってしまう。

## 配布時に同梱するもの

実行ファイル単体で動作する。

```
capturecard_viewer.exe
```

ウィンドウアイコン（`icon.ico`）と既定の効果音（`sound/SS.mp3`）は `include_bytes!` で実行ファイルに埋め込んでいるため、隣に置く必要はない。置いてあっても構わない。

設定画面でユーザーが選んだ効果音ファイルだけは外部のファイルを読む。設定に相対パスが保存されている場合は実行ファイルの置き場所を基準に解決し、見つからなければ埋め込みの既定音を鳴らす。

## プロファイル設定

`Cargo.toml` の `[profile.release]`。

| 設定 | 値 | 影響 |
|---|---|---|
| `opt-level` | `3` | 速度優先 |
| `lto` | `true` | リンク時最適化。ビルドは遅くなるがバイナリが小さく速くなる |
| `codegen-units` | `1` | 最適化の質を上げる。ビルドは遅くなる |
| `panic` | `"abort"` | 巻き戻しコードを省く。**`catch_unwind` が機能しなくなる** |

release ビルドは `lto` と `codegen-units = 1` の影響で時間がかかる。反復作業には dev ビルドを使う。

## デバッグ時の注意

`src/main.rs` 冒頭の `#![windows_subsystem = "windows"]` によってコンソールが割り当てられないため、**標準出力と標準エラーはどこにも表示されない。** `println!` を足してもデバッグの役に立たない。

代わりに `log` クレートのマクロを使う。出力先は `src/logging.rs` が用意するファイルで、置き場所は設定ファイルの隣。

```
%AppData%\capturecard_viewer\logs\capturecard_viewer-YYYYMMDD-HHMMSS.log
```

1 回の起動につき 1 ファイル作られ、起動時に新しいものから 10 個だけ残して削除される。

既定のレベルは `info`。詳細を見たいときは環境変数で上げる。

```
set CAPTURECARD_VIEWER_LOG=trace
capturecard_viewer.exe
```

受け付ける値は `error` / `warn` / `info` / `debug` / `trace`。解釈できない値を渡した場合は `info` に戻る。

テストコードの中は例外で、テストバイナリの標準出力は `cargo test -- --nocapture` で読めるため `println!` を使ってよい（`src/video.rs` の計測用テストがその例）。

## Cargo.lock

`Cargo.lock` はリポジトリにコミットしてある。配布バイナリを持つプロジェクトでは `Cargo.lock` をコミットするのが Cargo の推奨で、これにより誰がいつビルドしても同じ版の依存が解決される。

依存を更新したときは `Cargo.lock` の差分も同じコミットに含めること。`.gitignore` には入れない。

## 実機確認

コードの検証だけでは足りない変更（映像、音声、デバイス接続、ホットキー、ウィンドウ操作）は、`docs/MANUAL-TEST.md` のチェックリストで確認する。
