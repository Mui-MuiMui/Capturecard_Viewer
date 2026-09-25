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
//! 持っている言語は日本語と英語。どちらを出すかは設定の `ui.language` から
//! `CaptureCardViewer` が決め、`set_language` で書き換える。

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
    English = 1,
}

impl Language {
    /// `LANGUAGE` に入っている値から戻すための一覧。
    const ALL: [Language; 2] = [Language::Japanese, Language::English];
}

/// 現在の言語。**言語の状態を持つのはアプリ全体でこの 1 つだけ。**
///
/// 描画・エラーの `Display`・ワーカースレッドのどこからでも読むので、
/// 引数で配り回さずにここへ置く。`OnceLock` ではなく `AtomicU8` にしてあるのは、
/// 設定ダイアログの「適用」から再起動なしに切り替えるため。
static LANGUAGE: AtomicU8 = AtomicU8::new(Language::Japanese as u8);

// テストの中だけ、そのスレッドで読む言語を差し替える口。
//
// `LANGUAGE` はプロセス全体で共有されるので、テストから書き換えると
// 並列に走る他のテスト（`Display` が日本語であることを見るもの）の結果が
// 変わる。スレッドに閉じた上書きなら他のテストへ漏れない
#[cfg(test)]
thread_local! {
    static TEST_LANGUAGE: std::cell::Cell<Option<Language>> =
        const { std::cell::Cell::new(None) };
}

/// 現在の言語を返す。知らない値が入っていたら既定（日本語）へ倒す。
pub fn language() -> Language {
    #[cfg(test)]
    if let Some(language) = TEST_LANGUAGE.with(|cell| cell.get()) {
        return language;
    }

    let value = LANGUAGE.load(Ordering::Relaxed);
    Language::ALL
        .into_iter()
        .find(|language| *language as u8 == value)
        .unwrap_or_default()
}

/// 画面に出す言語を切り替える。次に文字列を引いたところから切り替わる。
///
/// 呼ぶのは起動時と、設定ダイアログの「適用」「OK」だけ
/// （`CaptureCardViewer::apply_language`）。
pub fn set_language(language: Language) {
    LANGUAGE.store(language as u8, Ordering::Relaxed);
}

/// テストの中で、このスレッドだけ言語を `language` にして `f` を呼ぶ。
#[cfg(test)]
pub fn with_language<R>(language: Language, f: impl FnOnce() -> R) -> R {
    // 抜けるときに上書きを戻す。戻さないと、同じスレッドで次に走る
    // テストが英語のまま始まる
    struct Restore(Option<Language>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_LANGUAGE.with(|cell| cell.set(self.0));
        }
    }

    let _restore = Restore(TEST_LANGUAGE.with(|cell| cell.replace(Some(language))));
    f()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_language_overrides_only_inside_the_closure() {
        let inside = with_language(Language::English, language);
        assert_eq!(inside, Language::English);
        // 抜けたら元へ戻る。`LANGUAGE` 自体は触っていない
        assert_eq!(language(), Language::Japanese);
    }
}
