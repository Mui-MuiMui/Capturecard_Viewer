//! 引数を取らない文字列の表。
//!
//! 1 行が「キー { 言語ごとの文言 }」の 1 件。`texts!` がここから
//! `Text` の列挙子、言語ごとの `match`、テスト用の全件の一覧を作る。
//! **キーを打ち間違えると `Text::…` が見つからずコンパイルで落ち、
//! 使われなくなったキーは `dead_code` の警告（CI では `-D warnings` でエラー）になる。**
//!
//! 並びは使う場所ごとにまとめてある。足すときは近い塊の末尾へ置く。

use super::{language, Language};

macro_rules! texts {
    ($($key:ident { ja: $ja:literal },)*) => {
        /// 画面に出す文字列のキー。`get()` で現在の言語の文言を返す。
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Text {
            $($key,)*
        }

        impl Text {
            /// 全てのキー。表が揃っているかをテストで確かめるためにある。
            #[cfg(test)]
            const ALL: &'static [Text] = &[$(Text::$key,)*];

            fn ja(self) -> &'static str {
                match self {
                    $(Text::$key => $ja,)*
                }
            }
        }
    };
}

impl Text {
    /// 現在の言語での文言。
    pub fn get(self) -> &'static str {
        match language() {
            Language::Japanese => self.ja(),
        }
    }
}

texts! {
    // ---- 共通 ----
    Input { ja: "入力" },
    Output { ja: "出力" },

    // ---- 失敗の定型文（status::ErrorSource::headline） ----
    HeadlineVideo { ja: "映像デバイスに接続できません" },
    HeadlineAudio { ja: "音声デバイスに接続できません" },
    HeadlineScreenshot { ja: "スクリーンショットを出力できません" },
    HeadlineHotkey { ja: "ホットキーを登録できません" },
    HeadlineSettings { ja: "設定ファイルを読み書きできません" },

    // ---- エラーの文言（各エラー enum の Display） ----
    VideoNoDevices { ja: "映像デバイスが 1 台も見つからない" },
    HotkeyMultipleKeys { ja: "通常キーを 2 つ以上は指定できません" },
    HotkeyMissingKey { ja: "通常キーが指定されていません" },
    KeyboardHookUnsupported { ja: "この OS には対応していません" },
    KeyboardHookListenerStopped { ja: "ホットキーのリスナースレッドが起動しませんでした" },
    PresetNameEmpty { ja: "プリセット名を入力してください" },
    PresetNameDuplicate { ja: "同じ名前のプリセットが既にあります" },

    // ---- ホットキーのアクション名（HotkeyAction::label） ----
    ActionScreenshot { ja: "スクリーンショット" },
    ActionToggleFullscreen { ja: "フルスクリーン切替" },
    ActionToggleAlwaysOnTop { ja: "最前面表示の切替" },
    ActionReconnectDevices { ja: "デバイス再接続" },
    ActionVolumeUp { ja: "音量を上げる" },
    ActionVolumeDown { ja: "音量を下げる" },
    ActionToggleMute { ja: "ミュート切替" },

    // ---- 色空間・色レンジ（settings::ColorSpace / ColorRange の label） ----
    ColorSpaceAuto { ja: "自動（解像度から判断）" },
    ColorSpaceBt601 { ja: "BT.601（SD）" },
    ColorSpaceBt709 { ja: "BT.709（HD）" },
    ColorRangeLimited { ja: "リミテッド（16〜235）" },
    ColorRangeFull { ja: "フル（0〜255）" },

    // ---- 接続状態（status.rs） ----
    VideoActualUnknown { ja: "（取得できない）" },
    ResampleIdentity { ja: "変換なし" },
    WaterLevelUnknown { ja: "不明" },
    UnderrunUnknown { ja: "アンダーラン: -" },
    LinkConnected { ja: "接続中" },
    LinkReconnecting { ja: "未接続（再接続を試しています）" },
    LinkDisconnected { ja: "未接続" },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn text_every_key_has_non_empty_japanese() {
        for key in Text::ALL {
            let text = key.ja();
            assert!(!text.is_empty(), "{key:?} の日本語が空");
            // 前後の空白は並べる側（egui のレイアウト）の仕事。文言に混ぜると
            // 言語ごとに揃え方がばらつく
            assert_eq!(text, text.trim(), "{key:?} の前後に空白がある");
        }
    }

    #[test]
    fn text_japanese_is_not_duplicated_across_keys() {
        // 同じ文言に 2 つのキーがあると、片方だけ直して食い違う。
        // 同じ文言を別の場所で使うときはキーを使い回す
        let mut seen: HashMap<&str, Text> = HashMap::new();
        for key in Text::ALL {
            if let Some(previous) = seen.insert(key.ja(), *key) {
                panic!("{previous:?} と {key:?} が同じ文言「{}」", key.ja());
            }
        }
    }

    #[test]
    fn text_get_returns_japanese_by_default() {
        assert_eq!(Text::LinkConnected.get(), "接続中");
    }
}
