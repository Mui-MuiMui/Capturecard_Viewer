//! 画面に出す文字列の置き場所。
//!
//! 設定ダイアログ・右クリックメニュー・OSD・エラーの文言を、呼び出し側へ
//! リテラルで書かずにここへ集める。仕組みと文字列を足すときの手順は
//! `docs/design/i18n.md`。
//!
//! - 引数を取らない文字列は `Text` のキーで引く（`Text::MenuQuit.get()`）。
//!   キーと文言の表は `text.rs`
//! - 引数を取る文字列は `msg.rs` の関数で組み立てる
//!   （`i18n::volume_percent(80)`）。言語ごとに語順が変わるため、呼び出し側で
//!   断片を `format!` でつながない
//!
//! **ログの文言（`log!` 系）はここに入れない。** 不具合報告で読むのは開発側で、
//! 言語を切り替えても同じ文面で残っているほうが追いやすいため。
//!
//! いまは日本語だけを持つ。言語を切り替える口（`set_language`）と英語の表は
//! 第 2 段階（Issue #166）で足す。

use std::sync::atomic::{AtomicU8, Ordering};

mod msg;
mod text;

pub use msg::*;
pub use text::Text;

/// 画面に出す言語。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum Language {
    #[default]
    Japanese = 0,
}

impl Language {
    /// `LANGUAGE` に入っている値から戻すための一覧。
    const ALL: [Language; 1] = [Language::Japanese];
}

/// 現在の言語。**言語の状態を持つのはアプリ全体でこの 1 つだけ。**
///
/// 描画・エラーの `Display`・ワーカースレッドのどこからでも読むので、
/// 引数で配り回さずにここへ置く。`OnceLock` ではなく `AtomicU8` にしてあるのは、
/// 第 2 段階で設定ダイアログの「適用」から再起動なしに切り替えられるようにするため。
static LANGUAGE: AtomicU8 = AtomicU8::new(Language::Japanese as u8);

/// 現在の言語を返す。知らない値が入っていたら既定（日本語）へ倒す。
pub fn language() -> Language {
    let value = LANGUAGE.load(Ordering::Relaxed);
    Language::ALL
        .into_iter()
        .find(|language| *language as u8 == value)
        .unwrap_or_default()
}
