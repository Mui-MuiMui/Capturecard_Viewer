use crate::i18n::Text;
use serde::{Serialize, Serializer};

/// ホットキーで実行できるアクション。
///
/// **順序が設定ファイルのキーの並び順と、設定画面の一覧の並び順になる。**
/// `BTreeMap` のキーとして使うため `Ord` を導出しており、その順序は
/// ここでの宣言順で決まる。並べ替えると既存の設定ファイルの見た目が変わる。
///
/// 追加するときは `ALL` と `as_str` / `from_key` / `label` の 4 か所を足す。
/// `as_str` は設定ファイルに書かれる文字列なので、一度出した名前は変えない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HotkeyAction {
    /// スクリーンショットを撮る
    Screenshot,
    /// フルスクリーン表示を切り替える
    ToggleFullscreen,
    /// 最前面表示を切り替える
    ToggleAlwaysOnTop,
    /// デバイスを開き直す
    ReconnectDevices,
    /// 音量を上げる
    VolumeUp,
    /// 音量を下げる
    VolumeDown,
    /// ミュートを切り替える
    ToggleMute,
}

impl HotkeyAction {
    /// 設定画面と一覧の表示順。宣言順（`Ord`）と同じにしておく。
    pub const ALL: [HotkeyAction; 7] = [
        HotkeyAction::Screenshot,
        HotkeyAction::ToggleFullscreen,
        HotkeyAction::ToggleAlwaysOnTop,
        HotkeyAction::ReconnectDevices,
        HotkeyAction::VolumeUp,
        HotkeyAction::VolumeDown,
        HotkeyAction::ToggleMute,
    ];

    /// 設定ファイルに書かれるキー名。**変えると既存の設定を見失う。**
    pub fn as_str(self) -> &'static str {
        match self {
            HotkeyAction::Screenshot => "screenshot",
            HotkeyAction::ToggleFullscreen => "toggle_fullscreen",
            HotkeyAction::ToggleAlwaysOnTop => "toggle_always_on_top",
            HotkeyAction::ReconnectDevices => "reconnect_devices",
            HotkeyAction::VolumeUp => "volume_up",
            HotkeyAction::VolumeDown => "volume_down",
            HotkeyAction::ToggleMute => "toggle_mute",
        }
    }

    /// 設定ファイルのキー名からアクションを引く。知らない名前は `None`。
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|action| action.as_str() == key)
    }

    /// 設定画面に出す名前。
    pub fn label(self) -> &'static str {
        let text = match self {
            HotkeyAction::Screenshot => Text::ActionScreenshot,
            HotkeyAction::ToggleFullscreen => Text::ActionToggleFullscreen,
            HotkeyAction::ToggleAlwaysOnTop => Text::ActionToggleAlwaysOnTop,
            HotkeyAction::ReconnectDevices => Text::ActionReconnectDevices,
            HotkeyAction::VolumeUp => Text::ActionVolumeUp,
            HotkeyAction::VolumeDown => Text::ActionVolumeDown,
            HotkeyAction::ToggleMute => Text::ActionToggleMute,
        };
        text.get()
    }

    /// 最小化している間も、その場で実行してよいか。
    ///
    /// **分かれ目は「画面が要るか」。** 音量・ミュート・再接続は出力
    /// コールバックやデバイスワーカーに届けば効くので、最小化中でも
    /// 意味がある。フルスクリーンや最前面表示、スクリーンショットは
    /// 見えていないウィンドウに対して行っても意味がないうえ、UI スレッド
    /// でしか触れない状態を書き換えるため、復帰するまで保留する（#133）。
    ///
    /// 真を返すものの実行経路は `app::hotkeys::background_hotkey_runner`。
    pub fn runs_while_minimized(self) -> bool {
        match self {
            HotkeyAction::ReconnectDevices
            | HotkeyAction::VolumeUp
            | HotkeyAction::VolumeDown
            | HotkeyAction::ToggleMute => true,
            HotkeyAction::Screenshot
            | HotkeyAction::ToggleFullscreen
            | HotkeyAction::ToggleAlwaysOnTop => false,
        }
    }
}

/// 保留していた押下を、復帰したときに何回実行するかへ畳む。
///
/// 最小化している間 `update()` は呼ばれないので、押下は復帰するまで溜まる。
/// 溜まった数をそのまま実行すると、フルスクリーンを 2 回押して戻したはずが
/// 復帰後にフルスクリーンになる、といった食い違いが出る。
///
/// **`match` に `_` を置かないこと。** アクションを増やしたときに、
/// 溜まった押下をどう畳むかをここで必ず決めさせるため。
pub(super) fn folded_repeats(action: HotkeyAction, presses: u32) -> u32 {
    match action {
        // トグルは偶数回なら元の状態へ戻る。押した回数ぶん切り替えても
        // 結果は同じなので、奇数回のときだけ 1 回実行する
        HotkeyAction::ToggleFullscreen
        | HotkeyAction::ToggleAlwaysOnTop
        | HotkeyAction::ToggleMute => presses % 2,
        // 復帰してから撮るので、何回押されていても同じ 1 枚にしかならない
        HotkeyAction::Screenshot => presses.min(1),
        // 開き直しは何回要求しても結果が同じ
        HotkeyAction::ReconnectDevices => presses.min(1),
        // 増減は押した回数ぶん効かせる。畳むと「10 段上げたのに 1 段」になる
        HotkeyAction::VolumeUp | HotkeyAction::VolumeDown => presses,
    }
}

// 設定では BTreeMap<HotkeyAction, String> のキーとして使う。TOML のキーは
// 文字列でなければならないため、derive ではなく文字列として書き出す。
// derive の単位バリアントはシリアライザによってキーとして受け付けられない
// ことがあり、そこに寄りかかると TOML 側の都合で保存できなくなる。
//
// 読むほうは `Deserialize` を実装していない。知らないアクション名が書かれて
// いても設定ファイル全体を失わないよう、`settings::migrate_hotkeys` が
// 文字列のまま受けて `from_key` で振り分ける。
impl Serialize for HotkeyAction {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- アクションの名前 ----

    #[test]
    fn hotkey_action_all_contains_every_variant_once() {
        // ALL から漏れると、設定画面に出ないアクションができる
        let mut seen: Vec<&str> = HotkeyAction::ALL.iter().map(|a| a.as_str()).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), HotkeyAction::ALL.len());
    }

    #[test]
    fn hotkey_action_from_key_roundtrips() {
        for action in HotkeyAction::ALL {
            assert_eq!(HotkeyAction::from_key(action.as_str()), Some(action));
        }
    }

    #[test]
    fn hotkey_action_from_key_unknown_returns_none() {
        // 設定ファイルに知らないアクションが書かれている場合
        assert_eq!(HotkeyAction::from_key("mute"), None);
        assert_eq!(HotkeyAction::from_key(""), None);
        assert_eq!(HotkeyAction::from_key("Screenshot"), None);
    }

    #[test]
    fn hotkey_action_order_is_declaration_order() {
        // BTreeMap の並び順＝設定ファイルのキーの並び順。変わると見た目が変わる
        let mut sorted = HotkeyAction::ALL;
        sorted.sort();
        assert_eq!(sorted, HotkeyAction::ALL);
    }

    // ---- 溜まった押下の畳み方 ----

    #[test]
    fn folded_repeats_toggles_cancel_out_in_pairs() {
        // 最小化中に 2 回押して戻したなら、復帰しても切り替えない
        assert_eq!(folded_repeats(HotkeyAction::ToggleFullscreen, 2), 0);
        assert_eq!(folded_repeats(HotkeyAction::ToggleFullscreen, 3), 1);
        assert_eq!(folded_repeats(HotkeyAction::ToggleAlwaysOnTop, 4), 0);
        assert_eq!(folded_repeats(HotkeyAction::ToggleMute, 1), 1);
    }

    #[test]
    fn folded_repeats_screenshot_is_taken_once() {
        // 復帰してから撮るので、何回押されていても同じ 1 枚にしかならない
        assert_eq!(folded_repeats(HotkeyAction::Screenshot, 5), 1);
        assert_eq!(folded_repeats(HotkeyAction::Screenshot, 0), 0);
    }

    #[test]
    fn folded_repeats_volume_keeps_every_press() {
        // 増減は押した回数ぶん効かせる（最小化中はワーカーが実行するので、
        // ここへ来るのは通常のフレームで溜まった分だけ）
        assert_eq!(folded_repeats(HotkeyAction::VolumeUp, 3), 3);
        assert_eq!(folded_repeats(HotkeyAction::VolumeDown, 1), 1);
    }
}
