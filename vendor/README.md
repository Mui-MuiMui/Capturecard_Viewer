# vendor

アップストリームのクレートに手を入れて使うものを置く。`Cargo.toml` の `[patch.crates-io]` で crates.io の版と差し替える。

**差分は最小にする。** アップストリームの版を上げるときに追えるよう、変えた箇所には `[capturecard_viewer]` を含むコメントを付ける。`grep -n "\[capturecard_viewer\]" -r vendor/` で全部が引ける。

## nokhwa-bindings-windows

| 項目 | 内容 |
|---|---|
| 元の版 | crates.io の `nokhwa-bindings-windows` 0.4.6（アップストリームのコミット `fa5a19208ade2b22113bfaa48f20dfdd3758b751`、`https://github.com/l1npengtul/nokhwa` の `nokhwa-bindings-windows/`） |
| ライセンス | Apache-2.0。`LICENSE` は同じリポジトリの `nokhwa` 0.10.11 の同梱品（crates.io の 0.4.6 にはライセンスのファイルが入っていない） |
| 置いたファイル | `Cargo.toml`（crates.io が正規化したもの）・`README.md`・`src/lib.rs` は 0.4.6 のまま。変えたのは `src/lib.rs` だけ |
| 理由 | Media Foundation のソースリーダーに `MF_LOW_LATENCY` を付けるため（#456）。属性はクレートの中で組み立てていて、外から渡せない。詳しくは `docs/design/video-pipeline.md` の「Media Foundation のソースリーダーの低遅延モード（#456）」 |

### 変えた箇所

- `src/lib.rs`
  - `MF_LOW_LATENCY` の import
  - 付けるかどうかの旗 `LOW_LATENCY`（既定 `true`）と、アプリから切り替える `set_low_latency`
  - `MediaFoundationDevice::new` のソースリーダーの属性に、旗が立っていれば `MF_LOW_LATENCY = TRUE` を足す

### アップストリームの版を上げるとき

1. 新しい版を crates.io から取り、`Cargo.toml`・`README.md`・`src/lib.rs` を置き換える
2. 上の「変えた箇所」を当て直す（`[capturecard_viewer]` のコメントごと）
3. `nokhwa` が新しい版を要求していれば `Cargo.toml` の `[patch.crates-io]` が効いているか（`Cargo.lock` の `nokhwa-bindings-windows` に `source` が無いこと）を確かめる。版が合わないと差し替えが外れ、`cargo` は警告（`Patch ... was not used in the crate graph`）を出すだけで止まらない
4. この表の「元の版」を書き換え、`THIRD-PARTY-LICENSES.txt` を生成し直す（`docs/DEPENDENCIES.md` の「ライセンス」）
