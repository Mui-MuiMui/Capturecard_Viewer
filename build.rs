fn main() {
    #[cfg(target_os = "windows")]
    {
        // 監視対象を明示する。これを出さないと Cargo はパッケージ全体を走査する
        println!("cargo:rerun-if-changed=app.rc");
        println!("cargo:rerun-if-changed=icon.ico");
        // version.h の元になるので、バージョンを書き換えたら再生成させる
        println!("cargo:rerun-if-changed=Cargo.toml");

        generate_version_header();
        embed_resource::compile("app.rc", embed_resource::NONE);
    }
}

/// Cargo.toml の version からリソーススクリプト用のヘッダーを OUT_DIR に生成する。
///
/// バージョン番号の出どころを Cargo.toml だけにするための仕組み。`app.rc` は
/// `#include "version.h"` でここで定義したマクロを参照する。
/// embed-resource は rc.exe に `/I <OUT_DIR>` を渡すため、OUT_DIR に置けば見つかる。
#[cfg(target_os = "windows")]
fn generate_version_header() {
    use std::path::PathBuf;

    // FILEVERSION / PRODUCTVERSION は 16 bit 整数 4 つしか取れない。Cargo のバージョンは
    // 数値を 3 つしか持たないため第 4 フィールドは 0 で固定する。
    // `1.0.7-rc1` のようなプレリリース表記が来てもビルドが落ちないよう、数値部分だけを
    // Cargo が分解した環境変数から読む。プレリリース識別子は文字列側にだけ残る。
    let major = version_field("CARGO_PKG_VERSION_MAJOR");
    let minor = version_field("CARGO_PKG_VERSION_MINOR");
    let patch = version_field("CARGO_PKG_VERSION_PATCH");
    let version = std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION が設定されていない");

    let header = format!(
        "// build.rs が Cargo.toml の version から生成したファイル。直接編集しない\n\
         #define CV_VERSION_NUM {major},{minor},{patch},0\n\
         #define CV_VERSION_STR \"{version}\"\n"
    );

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR が設定されていない");
    let out_path = PathBuf::from(out_dir).join("version.h");
    std::fs::write(&out_path, header)
        .unwrap_or_else(|e| panic!("{} の書き出しに失敗した: {e}", out_path.display()));
}

/// Cargo が分解したバージョンの数値をひとつ読む。
/// rc.exe が受け付ける範囲（0〜65535）から外れていればビルドを止める。
#[cfg(target_os = "windows")]
fn version_field(name: &str) -> u16 {
    let raw = std::env::var(name).unwrap_or_else(|_| panic!("{name} が設定されていない"));
    raw.parse()
        .unwrap_or_else(|_| panic!("{name}={raw} を 0〜65535 の整数として解釈できない"))
}
