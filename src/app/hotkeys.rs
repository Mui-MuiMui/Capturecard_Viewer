//! ホットキーの適用と、押されたときの実行。
//!
//! **実処理は右クリックメニューや映像上の操作と同じ経路を通す。**
//! 登録そのものは `crate::hotkey::HotkeyManager` が持つ。

use super::audio_control::VOLUME_SCROLL_STEP;
use super::worker::DeviceCommand;
use super::CaptureCardViewer;
use crate::hotkey::{BackgroundHotkeyRunner, HotkeyAction, HotkeyAssignmentError};
use crate::status::ErrorSource;
use eframe::egui;
use log::{debug, trace, warn};
use std::collections::BTreeMap;
use std::sync::mpsc::Sender;

/// 最小化中のアクションを、UI スレッドを介さずに実行する窓口を組み立てる。
///
/// ホットキーのリスナースレッドから呼ばれる。**やってよいのはデバイス
/// ワーカーへコマンドを積むところまで。** `CaptureCardViewer` の状態は
/// UI スレッドのものなので、ここからは触れない。
///
/// 音量とミュートをワーカーへ回しているのは、ワーカーがウィンドウの状態に
/// 関係なく動く唯一のスレッドだから。**結果は `DeviceEvent` で UI へ戻り、
/// 復帰したフレームで `adjust_volume` / `toggle_mute` を通る**ので、
/// 設定への反映と OSD は右クリックメニューから操作したときと同じになる。
///
/// `HotkeyAction::runs_while_minimized` が偽のものはここへ届かない。
/// 届いても何もしないので、分類を増やしたときに勝手に実行されることはない。
pub(super) fn background_hotkey_runner(commands: Sender<DeviceCommand>) -> BackgroundHotkeyRunner {
    BackgroundHotkeyRunner::new(move |action| {
        let command = match action {
            HotkeyAction::ReconnectDevices => DeviceCommand::ReconnectNow,
            HotkeyAction::VolumeUp => DeviceCommand::AdjustVolume(VOLUME_SCROLL_STEP),
            HotkeyAction::VolumeDown => DeviceCommand::AdjustVolume(-VOLUME_SCROLL_STEP),
            HotkeyAction::ToggleMute => DeviceCommand::ToggleMute,
            HotkeyAction::Screenshot
            | HotkeyAction::ToggleFullscreen
            | HotkeyAction::ToggleAlwaysOnTop => return,
        };
        if let Err(e) = commands.send(command) {
            // ワーカーが終わっているときだけ。復帰後に UI 側で実行される
            // わけでもないので、押下が 1 回落ちる
            warn!("最小化中のホットキーをデバイスワーカーへ送れない: {}", e);
        }
    })
}

/// 登録できなかったホットキーを、通知 1 件ぶんの文字列にまとめる。
/// すべて登録できていれば `None`。
///
/// 定型文（「ホットキーを登録できません」）は `status::format_message` が
/// 前に付けるので、ここでは付けない。どのアクションのどのキーが駄目だったかを
/// 並べるところまでを受け持つ。
fn hotkey_error_summary(errors: &BTreeMap<HotkeyAction, HotkeyAssignmentError>) -> Option<String> {
    if errors.is_empty() {
        return None;
    }

    let detail = errors
        .iter()
        .map(|(action, error)| format!("{}（{}）: {}", action.label(), error.hotkey, error.reason))
        .collect::<Vec<_>>()
        .join(" / ");
    Some(detail)
}

impl CaptureCardViewer {
    /// 押されたホットキーのアクションを実行する。
    ///
    /// 1 フレームに複数のアクションが押されていた場合は、`HotkeyAction` の
    /// 宣言順に実行する。
    pub(super) fn handle_hotkeys(&mut self, ctx: &egui::Context) {
        for action in self.hotkey_manager.take_pressed() {
            trace!("ホットキーの押下を受け取った: {}", action.label());
            self.run_hotkey_action(ctx, action);
        }
    }

    /// このフレームで egui へ渡るキー入力から、ホットキーに割り当てたキーの
    /// 押下を取り除く（#217）。
    ///
    /// キーを奪わないフックにしたので、前面にいる間は割り当てたキーが egui にも
    /// 届き、Escape を割り当てると右クリックメニューも同時に閉じていた。
    /// **描画より前に呼ぶこと。** 描画の中で `key_pressed` を見る処理
    /// （右クリックメニューの Escape など）より後だと取り除いても間に合わない。
    ///
    /// 入力中かはフレームの先頭の値で見る。リスナーへ渡している旗（`update()` の
    /// 末尾で書く）と同じく、前のフレームの描画を終えた時点の状態になる。
    pub(super) fn remove_hotkey_key_events(&self, ctx: &egui::Context) {
        let typing = ctx.wants_keyboard_input();
        let removed = ctx.input_mut(|input| {
            self.hotkey_manager
                .remove_hotkey_key_events(&mut input.events, typing)
        });
        if removed > 0 {
            trace!("ホットキーのキー入力 {removed} 件を egui へ渡さずに捨てた");
        }
    }

    /// ホットキーに割り当てられたアクションを 1 つ実行する。
    ///
    /// **実処理は右クリックメニューや映像上の操作と同じ経路を通す。**
    /// ここに独自の処理を書くと、同じ操作なのに設定の保存やオーバーレイ表示の
    /// 有無が経路によって変わってしまう。
    fn run_hotkey_action(&mut self, ctx: &egui::Context, action: HotkeyAction) {
        match action {
            HotkeyAction::Screenshot => {
                debug!("スクリーンショットの処理に入る");
                self.take_screenshot();
            }
            HotkeyAction::ToggleFullscreen => {
                let to_full = !self.is_fullscreen;
                self.toggle_fullscreen(ctx, to_full);
            }
            HotkeyAction::ToggleAlwaysOnTop => {
                let enabled = !self.always_on_top;
                self.set_always_on_top(ctx, enabled);
            }
            HotkeyAction::ReconnectDevices => self.reconnect_devices(),
            HotkeyAction::VolumeUp => self.adjust_volume(VOLUME_SCROLL_STEP),
            HotkeyAction::VolumeDown => self.adjust_volume(-VOLUME_SCROLL_STEP),
            HotkeyAction::ToggleMute => self.toggle_mute(),
        }
    }

    /// ホットキーの割り当てを登録し直し、失敗を画面へ出す。
    ///
    /// **`HotkeyManager::apply` を直接呼ばないこと。** 直接呼ぶと、失敗の
    /// 通知と、直ったときのエラー表示の取り下げが抜ける。
    pub(super) fn apply_hotkey_assignments(&mut self, desired: &BTreeMap<HotkeyAction, String>) {
        self.hotkey_manager.apply(desired);
        self.report_hotkey_errors();
    }

    /// ホットキー入力ダイアログを閉じたときに、一時解除していたホットキーを
    /// 共有設定の内容で登録し直す。
    ///
    /// ダイアログを開いている間に確定した分は既に共有設定（またはドラフト）へ
    /// 書き込まれているので、ここでは常に**共有設定**を見る。ドラフトへ
    /// 書いた分（設定ダイアログが開いたままの場合）はまだ「適用」されていない
    /// ので、共有設定には反映されておらず、ここでも登録し直されない。
    /// 「適用」「OK」を押すまで効かない、という既存の約束どおりの挙動になる
    pub(super) fn resume_hotkeys_after_capture(&mut self) {
        let desired = match self.settings.lock() {
            Ok(settings) => settings.hotkeys.clone(),
            Err(_) => {
                warn!("ホットキーの再開で settings のロックを取得できない");
                return;
            }
        };
        self.hotkey_manager.resume(&desired);
        self.report_hotkey_errors();
    }

    /// 登録できないものが残っているかを、いまの `hotkey_manager` の状態から
    /// まとめて画面へ反映する。1 件ずつ通知すると、複数まとめて失敗したときに
    /// トーストが上書きされて最後の 1 件しか読めない
    fn report_hotkey_errors(&mut self) {
        let summary = hotkey_error_summary(self.hotkey_manager.errors());
        match summary {
            Some(reason) => self.report_error(ErrorSource::Hotkey, reason),
            // 他のアプリがキーを離して登録できるようになった場合も通る。
            // 残しておくと、直ったのに接続状態の表示が古いままになる
            None => self.errors.clear(ErrorSource::Hotkey),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkey::HotkeyError;
    use crate::keyboard_hook::KeyboardHookError;

    #[test]
    fn hotkey_error_summary_without_errors_is_none() {
        // すべて登録できている状態。通知を出さないだけでなく、
        // 呼び出し側が既存の通知を取り下げる合図にもなる
        assert_eq!(hotkey_error_summary(&BTreeMap::new()), None);
    }

    #[test]
    fn hotkey_error_summary_one_error_names_the_action_and_key() {
        let errors = BTreeMap::from([(
            HotkeyAction::Screenshot,
            HotkeyAssignmentError {
                hotkey: "F12".to_string(),
                reason: HotkeyError::UnsupportedKey("F13".to_string()),
            },
        )]);

        assert_eq!(
            hotkey_error_summary(&errors),
            Some("スクリーンショット（F12）: 未対応のキー: F13".to_string())
        );
    }

    #[test]
    fn hotkey_error_summary_multiple_errors_are_joined() {
        // 1 件ずつ通知するとトーストが上書きされて最後の 1 件しか読めない。
        // 並び順はアクションの宣言順（BTreeMap）で安定する
        let errors = BTreeMap::from([
            (
                HotkeyAction::VolumeUp,
                HotkeyAssignmentError {
                    hotkey: "F8".to_string(),
                    reason: HotkeyError::MissingKey,
                },
            ),
            (
                HotkeyAction::Screenshot,
                HotkeyAssignmentError {
                    hotkey: "F5".to_string(),
                    reason: HotkeyError::MultipleKeys,
                },
            ),
        ]);

        assert_eq!(
            hotkey_error_summary(&errors),
            Some(
                "スクリーンショット（F5）: 通常キーを 2 つ以上は指定できません / 音量を上げる（F8）: 通常キーが指定されていません"
                    .to_string()
            )
        );
    }

    #[test]
    fn hotkey_error_summary_does_not_repeat_the_headline() {
        // 定型文は status::format_message が前に付ける。ここで付けると
        // 「ホットキーを登録できません: ホットキーを登録できません: ...」になる
        let errors = BTreeMap::from([(
            HotkeyAction::Screenshot,
            HotkeyAssignmentError {
                hotkey: "F5".to_string(),
                reason: HotkeyError::HookUnavailable(KeyboardHookError::Unsupported),
            },
        )]);

        let summary = hotkey_error_summary(&errors).expect("理由があること");

        assert!(!summary.contains(ErrorSource::Hotkey.headline()));
    }
}
