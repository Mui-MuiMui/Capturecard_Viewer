//! 設定ダイアログの状態 `SettingsDialogState`。
//!
//! 共有の `AppSettings` を直接書き換えず、開いたときに複製したドラフトを
//! 編集する。ドラフトが実行中の設定へ移るのは「適用」と「OK」のときだけ
//! （`docs/design/settings-dialog.md`）。

use crate::audio::AudioDirection;
use crate::settings::{AppSettings, LanguageSetting};

use super::capability::{AudioCapabilityCache, VideoCapabilityCache};
use super::draft::commit_draft;
use super::hotkey_capture::HotkeyCaptureState;
use super::preset::{apply_preset_row_action, save_new_preset, PresetRowAction};
use super::{AudioCapabilityCaches, ManagementMessage, SettingsDialogAction, SettingsTab};

/// 操作に対して、ダイアログの外側（`CaptureCardViewer`）が行うこと。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingsDialogTransition {
    /// ドラフトを実行中の設定へ反映するか
    pub commit_draft: bool,
    /// 設定ファイルへ保存するか
    pub save_to_file: bool,
    /// ダイアログを閉じるか
    pub close: bool,
}

/// 設定ダイアログの状態。
///
/// ダイアログは共有の `AppSettings` を直接書き換えず、開いたときに複製した
/// ドラフトを編集する。ドラフトが実行中の設定へ移るのは「適用」と「OK」の
/// ときだけで、閉じるときは必ず捨てる。
///
/// こうしないと、未確定の編集が共有設定へ混ざり、ウィンドウ操作や音量変更を
/// きっかけにした保存に巻き込まれてファイルへ書き出されてしまう。
///
/// 選択中のタブ、デバイス能力のキャッシュ、ホットキー入力の状態は
/// ドラフトとは別に持つ。これらは設定の中身ではないので「適用」や「キャンセル」
/// では捨てず、ダイアログを開き直しても引き継ぐ（static だったときと同じ振る舞い）。
#[derive(Default)]
pub struct SettingsDialogState {
    draft: Option<AppSettings>,
    // 開いた時点の設定。ドラフトのどの項目が実際に編集されたかを判別するために持つ
    original: Option<AppSettings>,
    // 選択中のタブ
    selected_tab: SettingsTab,
    // デバイス名 → そのデバイスが扱えるフォーマット・解像度・FPS の取得状態。
    // 取得はデバイスを開く重い処理なのでワーカーへ投げ、一度取ったら保持する
    capabilities: VideoCapabilityCache,
    // 音声デバイスの対応設定。入力と出力で別に持つ。
    // **名前で引くので、入力と出力に同名のデバイスがあっても混ざらないよう分ける。**
    audio_input_capabilities: AudioCapabilityCache,
    audio_output_capabilities: AudioCapabilityCache,
    // ホットキー入力ダイアログの入力状態
    hotkey_capture: HotkeyCaptureState,
    // 「その他」タブに出す直近の結果。ドラフトについての説明なので、
    // ドラフトを作り直すとき・捨てるときに一緒に捨てる
    management_message: Option<ManagementMessage>,
    // 「設定を初期化」の確認待ちか。押し間違いで設定が消えないよう、
    // 1 段目のボタンではこれを立てるだけにして、2 段目で確定させる
    reset_confirm: bool,
    // 「現在の設定を新しいプリセットとして保存」の名前入力欄。
    //
    // 設定の中身ではないのでドラフトには入れない。保存に成功したときだけ
    // 空へ戻す（名前が重複して弾かれたときに入力が消えると直せない）
    new_preset_name: String,
}

impl SettingsDialogState {
    /// 編集中のドラフトを持っているか。
    pub fn has_draft(&self) -> bool {
        self.draft.is_some()
    }

    /// 現在の設定を複製して編集を始める。
    ///
    /// 呼ぶたびに作り直す。以前は `TEMP_SETTINGS` が None のときだけ退避し、
    /// × で閉じたときに消していなかったため、次に開いたときへ古い値が
    /// 持ち越されていた。
    pub fn begin_edit(&mut self, current: &AppSettings) {
        self.draft = Some(current.clone());
        self.original = Some(current.clone());
        self.forget_management_state();
    }

    /// 編集を終える。ドラフトは捨てる。
    pub fn end_edit(&mut self) {
        self.draft = None;
        self.original = None;
        self.forget_management_state();
    }

    /// 「その他」タブの表示状態を捨てる。
    ///
    /// メッセージも確認待ちもドラフトについてのものなので、ドラフトを
    /// 作り直すとき・捨てるときに残さない。残すと、開き直したダイアログに
    /// 「読み込みました」が出たままになる。
    fn forget_management_state(&mut self) {
        self.management_message = None;
        self.reset_confirm = false;
        self.new_preset_name.clear();
    }

    /// 「その他」タブに出すメッセージを差し替える。
    pub fn set_management_message(&mut self, text: String, is_error: bool) {
        self.management_message = Some(ManagementMessage { text, is_error });
    }

    /// 「その他」タブのメッセージを消す。
    ///
    /// 「適用」で反映したあとに呼ぶ。「適用」か「OK」で反映してください、と
    /// 促す文言が、反映したあとも残ると読み手を迷わせる。
    pub fn clear_management_message(&mut self) {
        self.management_message = None;
    }

    pub fn draft(&self) -> Option<&AppSettings> {
        self.draft.as_ref()
    }

    pub fn draft_mut(&mut self) -> Option<&mut AppSettings> {
        self.draft.as_mut()
    }

    /// ホットキー入力ダイアログの入力状態。
    ///
    /// ホットキー入力ダイアログは設定ダイアログから開くが、設定ダイアログを
    /// × で先に閉じても入力中の状態を失わないよう、`end_edit` では触らない。
    pub fn hotkey_capture_mut(&mut self) -> &mut HotkeyCaptureState {
        &mut self.hotkey_capture
    }

    /// ホットキー入力ダイアログの入力状態（読み取り）。
    pub fn hotkey_capture(&self) -> &HotkeyCaptureState {
        &self.hotkey_capture
    }

    /// デバイス能力の取得状態。
    ///
    /// 取得要求の取り出しと結果の反映は `CaptureCardViewer` が行うため、
    /// ダイアログを開いていない間（起動時の先読み）も触られる。
    pub fn capabilities_mut(&mut self) -> &mut VideoCapabilityCache {
        &mut self.capabilities
    }

    /// オーディオ入力デバイスの対応設定。
    ///
    /// ビデオ側と同じく、取得要求の取り出しと結果の反映は `CaptureCardViewer`
    /// が行う。**ここにあるのは設定ダイアログの選択肢のため。** 音声を開く
    /// ときに使う一覧はデバイスワーカーが別に持っている（`app::worker_loop`）。
    pub fn audio_input_capabilities_mut(&mut self) -> &mut AudioCapabilityCache {
        &mut self.audio_input_capabilities
    }

    /// オーディオ出力デバイスの対応設定。
    pub fn audio_output_capabilities_mut(&mut self) -> &mut AudioCapabilityCache {
        &mut self.audio_output_capabilities
    }

    /// 入出力を指定してオーディオの対応設定を取り出す。
    ///
    /// `CapabilityEvent` が `AudioDirection` を持って届くので、入口を 1 つに
    /// してある。入力と出力で同じ処理を 2 回書かないため。
    pub fn audio_capabilities_mut(
        &mut self,
        direction: AudioDirection,
    ) -> &mut AudioCapabilityCache {
        match direction {
            AudioDirection::Input => &mut self.audio_input_capabilities,
            AudioDirection::Output => &mut self.audio_output_capabilities,
        }
    }

    /// 選択中のタブを切り替える。
    pub fn select_tab(&mut self, tab: SettingsTab) {
        self.selected_tab = tab;
    }

    /// 「設定を初期化」の確認待ちにするかを切り替える。
    pub fn set_reset_confirm(&mut self, confirming: bool) {
        self.reset_confirm = confirming;
    }

    /// 新しいプリセットの名前入力欄を差し替える。
    pub fn set_new_preset_name(&mut self, name: String) {
        self.new_preset_name = name;
    }

    /// ドラフトの言語を差し替える。画面の言語はまだ変えない。
    /// 切り替わるのは「適用」「OK」で実行中の設定へ反映したとき。
    pub fn set_draft_language(&mut self, language: LanguageSetting) {
        if let Some(draft) = self.draft.as_mut() {
            draft.ui.language = language;
        }
    }

    /// 入力欄の名前で、ドラフトを新しいプリセットとして保存する。
    ///
    /// 成功したときだけ入力欄を空へ戻す。名前が重複して弾かれたときに
    /// 入力が消えると直せないため（`save_new_preset`）。
    pub fn save_new_preset(&mut self) {
        let Some(draft) = self.draft.as_mut() else {
            return;
        };
        save_new_preset(
            draft,
            &mut self.new_preset_name,
            &mut self.management_message,
        );
    }

    /// プリセット一覧の行のボタンをドラフトへ反映する。
    pub fn apply_preset_row(&mut self, action: PresetRowAction) {
        let Some(draft) = self.draft.as_mut() else {
            return;
        };
        apply_preset_row_action(draft, action, &mut self.management_message);
    }

    /// 描画のために、ドラフトの `&mut` とそれ以外の読み取り専用の借用へ分ける。
    ///
    /// ドラフトを編集しながら能力キャッシュやメッセージも読むため、
    /// `&mut SettingsDialogState` のままでは二重の借用になる。
    /// **ドラフト以外を `&mut` で渡さないのがこの分け方の目的。**
    /// 描画側が書き換えてよいのはドラフトだけで、他は
    /// `SettingsEvent` を通して `app` が動かす。
    ///
    /// ドラフトがまだ無ければ `None`。呼び出し側は描画を見送る。
    pub fn split_for_draw(&mut self) -> Option<(&mut AppSettings, SettingsDialogView<'_>)> {
        let Self {
            draft,
            selected_tab,
            capabilities,
            audio_input_capabilities,
            audio_output_capabilities,
            management_message,
            reset_confirm,
            new_preset_name,
            ..
        } = self;
        let draft = draft.as_mut()?;
        let view = SettingsDialogView {
            selected_tab: *selected_tab,
            video_capabilities: capabilities,
            audio_capabilities: AudioCapabilityCaches {
                input: audio_input_capabilities,
                output: audio_output_capabilities,
            },
            management_message: management_message.as_ref(),
            reset_confirm: *reset_confirm,
            new_preset_name,
        };
        Some((draft, view))
    }

    /// ドラフトを実行中の設定へ反映する。ドラフトを持っていなければ何もしない。
    pub fn commit_into(&self, target: &mut AppSettings) {
        if let (Some(draft), Some(original)) = (&self.draft, &self.original) {
            commit_draft(target, draft, original);
        }
    }

    /// 操作に対して、ダイアログの外側が行うことを決める。
    pub fn transition_for(action: SettingsDialogAction) -> SettingsDialogTransition {
        match action {
            SettingsDialogAction::None => SettingsDialogTransition {
                commit_draft: false,
                save_to_file: false,
                close: false,
            },
            SettingsDialogAction::Apply => SettingsDialogTransition {
                commit_draft: true,
                save_to_file: true,
                close: false,
            },
            SettingsDialogAction::Ok => SettingsDialogTransition {
                commit_draft: true,
                save_to_file: true,
                close: true,
            },
            SettingsDialogAction::Cancel => SettingsDialogTransition {
                commit_draft: false,
                save_to_file: false,
                close: true,
            },
        }
    }
}

/// 設定ダイアログの描画に要る、ドラフト以外の状態。
///
/// **すべて読み取り専用。** ここを `&mut` にすると、描画の途中で状態が
/// 変わる経路が復活する。書き換えは `SettingsEvent` を返して `app` に任せる。
///
/// 中身は `SettingsDialogState` の一部で、`split_for_draw` が作る。
pub struct SettingsDialogView<'a> {
    /// 選択中のタブ
    pub selected_tab: SettingsTab,
    /// ビデオデバイスの能力
    pub video_capabilities: &'a VideoCapabilityCache,
    /// オーディオデバイスの対応設定（入力・出力）
    pub audio_capabilities: AudioCapabilityCaches<'a>,
    /// 「その他」タブに出す直近の結果
    pub management_message: Option<&'a ManagementMessage>,
    /// 「設定を初期化」の確認待ちか
    pub reset_confirm: bool,
    /// 新しいプリセットの名前入力欄の内容
    pub new_preset_name: &'a str,
}

/// 描画後の状態から、実際に行われた操作を決める。
///
/// `window_still_open` は `egui::Window::open()` に渡した値の描画後の状態。
/// タイトルバーの × で閉じられるとボタンを押さずに false になるため、
/// キャンセルと同じ扱いにする。これを拾わないと、ドラフトを捨てる処理が
/// 走らずに次へ持ち越される。
pub fn resolve_action(
    button: SettingsDialogAction,
    window_still_open: bool,
) -> SettingsDialogAction {
    if !window_still_open && button == SettingsDialogAction::None {
        SettingsDialogAction::Cancel
    } else {
        button
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkey::HotkeyAction;
    use crate::settings::AppSettings;

    use crate::ui::tests::sample_settings;

    #[test]
    fn transition_for_none_does_nothing() {
        let transition = SettingsDialogState::transition_for(SettingsDialogAction::None);
        assert!(!transition.commit_draft);
        assert!(!transition.save_to_file);
        assert!(!transition.close);
    }

    #[test]
    fn transition_for_apply_commits_and_saves_without_closing() {
        // 適用 = 反映してファイルへ保存する。閉じないところだけが OK と違う
        let transition = SettingsDialogState::transition_for(SettingsDialogAction::Apply);
        assert!(transition.commit_draft);
        assert!(transition.save_to_file);
        assert!(!transition.close);
    }

    #[test]
    fn transition_for_apply_and_ok_differ_only_in_closing() {
        // 「適用」と「OK」の違いは閉じるかどうかだけにする。
        // 保存の有無で分けると「適用したのに再起動で戻る」が起きる
        let apply = SettingsDialogState::transition_for(SettingsDialogAction::Apply);
        let ok = SettingsDialogState::transition_for(SettingsDialogAction::Ok);
        assert_eq!(apply.commit_draft, ok.commit_draft);
        assert_eq!(apply.save_to_file, ok.save_to_file);
        assert!(!apply.close);
        assert!(ok.close);
    }

    #[test]
    fn transition_for_ok_commits_saves_and_closes() {
        let transition = SettingsDialogState::transition_for(SettingsDialogAction::Ok);
        assert!(transition.commit_draft);
        assert!(transition.save_to_file);
        assert!(transition.close);
    }

    #[test]
    fn transition_for_cancel_discards_without_committing() {
        // キャンセル = ドラフトを捨てて閉じるだけ。適用済みの分は戻さない
        let transition = SettingsDialogState::transition_for(SettingsDialogAction::Cancel);
        assert!(!transition.commit_draft);
        assert!(!transition.save_to_file);
        assert!(transition.close);
    }

    // テスト再生・書き出し・読み込み・初期化が反映や保存を起こさないことは、
    // `SettingsDialogAction` にその変種が無い（`SettingsEvent` の別の変種で
    // 表す）ことで構造的に保証されている。以前はここに
    // `transition_for` が何もしないことを確かめるテストがあった

    #[test]
    fn settings_dialog_state_end_edit_drops_the_management_message() {
        // 開き直したダイアログに「読み込みました」が残らないこと
        let mut state = SettingsDialogState::default();
        state.begin_edit(&AppSettings::default());
        state.set_management_message("読み込みました".to_string(), false);

        state.end_edit();

        assert!(state.management_message.is_none());
    }

    #[test]
    fn settings_dialog_state_begin_edit_drops_the_management_message() {
        let mut state = SettingsDialogState::default();
        state.set_management_message("失敗しました".to_string(), true);

        state.begin_edit(&AppSettings::default());

        assert!(state.management_message.is_none());
    }

    #[test]
    fn settings_dialog_state_management_message_keeps_the_error_flag() {
        let mut state = SettingsDialogState::default();
        state.begin_edit(&AppSettings::default());

        state.set_management_message("読み込めません".to_string(), true);

        let message = state
            .management_message
            .as_ref()
            .expect("メッセージがあること");
        assert_eq!(message.text, "読み込めません");
        assert!(message.is_error);
    }

    #[test]
    fn settings_dialog_state_clear_management_message_removes_it() {
        let mut state = SettingsDialogState::default();
        state.begin_edit(&AppSettings::default());
        state.set_management_message("読み込みました".to_string(), false);

        state.clear_management_message();

        assert!(state.management_message.is_none());
    }

    #[test]
    fn resolve_action_window_closed_without_button_returns_cancel() {
        // タイトルバーの × で閉じた場合。ボタンは押されていないが、
        // ドラフトを捨てるためにキャンセルとして扱う
        assert_eq!(
            resolve_action(SettingsDialogAction::None, false),
            SettingsDialogAction::Cancel
        );
    }

    #[test]
    fn resolve_action_window_open_without_button_returns_none() {
        assert_eq!(
            resolve_action(SettingsDialogAction::None, true),
            SettingsDialogAction::None
        );
    }

    #[test]
    fn resolve_action_keeps_pressed_button() {
        for action in [
            SettingsDialogAction::Ok,
            SettingsDialogAction::Cancel,
            SettingsDialogAction::Apply,
        ] {
            assert_eq!(resolve_action(action, true), action);
            assert_eq!(resolve_action(action, false), action);
        }
    }

    #[test]
    fn settings_dialog_state_commit_into_without_draft_does_nothing() {
        // ドラフトを持っていない状態で反映しても何も起きない
        let state = SettingsDialogState::default();
        let mut shared = sample_settings();
        let before = shared.clone();

        state.commit_into(&mut shared);

        assert_eq!(shared.video.fps, before.video.fps);
        assert_eq!(shared.ui.volume, before.ui.volume);
    }

    #[test]
    fn settings_dialog_state_begin_edit_snapshots_current_settings() {
        let settings = sample_settings();
        let mut state = SettingsDialogState::default();
        assert!(!state.has_draft());

        state.begin_edit(&settings);

        assert!(state.has_draft());
        assert_eq!(state.draft().expect("ドラフトがある").video.fps, Some(30));
    }

    #[test]
    fn settings_dialog_state_draft_edit_does_not_reach_source() {
        // ドラフトの編集が共有設定へ漏れないこと。
        // 漏れると、未確定の編集がウィンドウ操作などをきっかけに保存される
        let settings = sample_settings();
        let mut state = SettingsDialogState::default();
        state.begin_edit(&settings);

        state.draft_mut().expect("ドラフトがある").video.fps = Some(24);

        assert_eq!(settings.video.fps, Some(30));
    }

    #[test]
    fn settings_dialog_state_end_edit_drops_draft() {
        let mut state = SettingsDialogState::default();
        state.begin_edit(&sample_settings());

        state.end_edit();

        assert!(!state.has_draft());
        assert!(state.draft().is_none());
    }

    #[test]
    fn settings_dialog_state_reopen_after_close_by_window_button_uses_latest_settings() {
        // × で閉じたあと、別の手段で設定を変えてから開き直したとき、
        // 閉じる前のスナップショットが復活してはいけない
        let mut settings = sample_settings();
        let mut state = SettingsDialogState::default();

        state.begin_edit(&settings);
        state.draft_mut().expect("ドラフトがある").video.fps = Some(24);

        // タイトルバーの × で閉じる
        let closed =
            SettingsDialogState::transition_for(resolve_action(SettingsDialogAction::None, false));
        assert!(closed.close);
        assert!(!closed.commit_draft);
        state.end_edit();

        // 別の手段で設定が変わる
        settings.video.fps = Some(60);

        state.begin_edit(&settings);

        assert_eq!(state.draft().expect("ドラフトがある").video.fps, Some(60));
    }

    #[test]
    fn apply_then_cancel_keeps_applied_values() {
        // 「適用」で反映した内容は、そのあと「キャンセル」しても戻さない
        let mut shared = sample_settings();
        let mut state = SettingsDialogState::default();
        state.begin_edit(&shared);
        state.draft_mut().expect("ドラフトがある").video.fps = Some(24);

        let applied = SettingsDialogState::transition_for(SettingsDialogAction::Apply);
        assert!(applied.commit_draft);
        assert!(applied.save_to_file);
        assert!(!applied.close);
        state.commit_into(&mut shared);

        let cancelled = SettingsDialogState::transition_for(SettingsDialogAction::Cancel);
        assert!(!cancelled.commit_draft);
        assert!(cancelled.close);
        state.end_edit();

        assert_eq!(shared.video.fps, Some(24));
        assert!(!state.has_draft());
    }

    #[test]
    fn settings_dialog_state_end_edit_keeps_hotkey_capture_state() {
        // ホットキー入力ダイアログを開いたまま設定ダイアログを × で閉じても、
        // 入力中の状態を失わない（static だったときと同じ振る舞い）
        let mut state = SettingsDialogState::default();
        state.begin_edit(&sample_settings());
        state
            .hotkey_capture_mut()
            .begin_for(HotkeyAction::Screenshot);
        state
            .hotkey_capture_mut()
            .set_rejection("修飾キーだけでは登録できません".to_string());

        state.end_edit();

        assert!(!state.has_draft());
        assert_eq!(
            state.hotkey_capture_mut().editing(),
            Some(HotkeyAction::Screenshot)
        );
        assert_eq!(
            state.hotkey_capture_mut().rejection(),
            Some("修飾キーだけでは登録できません")
        );
    }

    #[test]
    fn settings_dialog_state_default_tab_is_device() {
        let mut state = SettingsDialogState::default();
        assert_eq!(state.selected_tab, SettingsTab::Device);

        // タブの選択はドラフトとは別なので、開いて閉じても引き継がれる
        state.begin_edit(&sample_settings());
        state.selected_tab = SettingsTab::Screenshot;
        state.end_edit();

        assert_eq!(state.selected_tab, SettingsTab::Screenshot);
    }
}
