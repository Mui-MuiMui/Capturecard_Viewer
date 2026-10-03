//! ドラフトを実行中の設定へ反映する（`commit_draft`）。
//!
//! 設定を組み替えるだけで描画を含まない。呼ぶのは `state`
//! （`docs/design/settings-dialog.md`）。読み込みと初期化でドラフトを作るのは `draft_import.rs`。

use crate::settings::AppSettings;

/// ドラフトのうち、設定ダイアログが編集する範囲だけを実行中の設定へ反映する。
/// `original` は開いた時点（「適用」のあとは直前に反映した時点、#316）の設定。
///
/// `ui` セクションを丸ごと上書きしないのは、ウィンドウのサイズ・位置、
/// 最前面表示、画面ドラッグ移動がダイアログの外で変わるため。丸ごと入れると、
/// ダイアログを開いている間に動かしたウィンドウの位置が、開いた時点の
/// スナップショットで巻き戻る。
///
/// ダイアログでも外でも変えられる `maintain_aspect_ratio` と `volume` は、
/// **ダイアログで実際に編集されたときだけ**反映する。開いた時点の値と同じなら
/// 外側の変更（映像上でのホイール操作、コンテキストメニュー）を残す。無条件に
/// 入れると、ダイアログを開いたままホイールで音量を変えて「適用」を押したときに
/// 音量が巻き戻る。
///
/// `audio` / `screenshot` にこの比較が要らないのは、ダイアログの外から
/// 書き換わらないため。ホットキー入力ダイアログもドラフトへ書く。
///
/// `video` の `auto_reconnect` だけは例外で、ダイアログに無く右クリックメニューで
/// 切り替える。丸ごと上書きすると、ダイアログを開いている間の切り替えが
/// 開いた時点のスナップショットで巻き戻るため、`ui` の 2 項目と同じ比較を使い、
/// **ドラフトで実際に変わったときだけ**反映する。通常の編集ではドラフトの値が
/// 動かないので、これまでどおり実行中の値が残る。動くのは設定の読み込みと
/// 初期化だけで、そのときはユーザーが選んだ内容を反映する側が正しい。
///
/// `ui` の `muted` もダイアログに無い（右クリックメニュー・ミドルクリック・
/// ホットキーで切り替える）。ここで触らないので、ダイアログを開いている間の
/// 切り替えはそのまま残る。
///
/// `ui` の `language` は「その他」タブだけで変える項目なので、ホットキーと
/// 同じく無条件に反映する。画面の言語を切り替えるのは呼び出し側
/// （`app::settings_dialog` の `apply_language`）。
///
/// `update` の 2 つのチェックも「その他」タブだけで変わるので無条件に反映する。
/// `skipped_version` だけは起動時の通知ダイアログの「この版は通知しない」でも
/// 変わるため、`auto_reconnect` と同じく**ドラフトで変わったときだけ**反映する。
/// 無条件に入れると、設定ダイアログを開いたまま通知ダイアログで飛ばした版が
/// 「適用」で消える。
///
/// **ダイアログに `ui` セクションの項目を足すときは、ここにも足すこと。**
/// **逆に、ダイアログの外だけで変える項目を足すときは、ここで残すこと。**
pub fn commit_draft(target: &mut AppSettings, draft: &AppSettings, original: &AppSettings) {
    let auto_reconnect = target.video.auto_reconnect;
    target.video = draft.video.clone();
    // ドラフトが開いた時点のままなら、ダイアログの外（右クリックメニュー）で
    // 切り替えた値を残す。変わっているのは読み込みと初期化のときだけ
    if draft.video.auto_reconnect == original.video.auto_reconnect {
        target.video.auto_reconnect = auto_reconnect;
    }
    target.audio = draft.audio.clone();
    target.screenshot = draft.screenshot.clone();
    // 録画もダイアログ（「録画」タブ）の中だけで変わる。録画中に変えた値は次の録画から効く
    target.recording = draft.recording.clone();
    // ホットキーの割り当ても、反応する条件も、ダイアログの中だけで変わる
    target.hotkeys = draft.hotkeys.clone();
    target.hotkey_settings = draft.hotkey_settings.clone();
    // プリセットの追加・上書き・削除もダイアログの中だけで行う。
    // 右クリックメニューからは選ぶだけで一覧を触らない
    target.presets = draft.presets.clone();
    target.active_preset = draft.active_preset.clone();

    // ダイアログの「ユーザーインターフェース」グループが編集する 2 項目
    if draft.ui.maintain_aspect_ratio != original.ui.maintain_aspect_ratio {
        target.ui.maintain_aspect_ratio = draft.ui.maintain_aspect_ratio;
    }
    if draft.ui.volume != original.ui.volume {
        target.ui.volume = draft.ui.volume;
    }
    // 「その他」タブの言語。ダイアログの外からは変わらない
    target.ui.language = draft.ui.language;

    // 「その他」タブの更新の節。飛ばした版だけは通知ダイアログからも変わる
    target.update.check_on_startup = draft.update.check_on_startup;
    target.update.notify_on_startup = draft.update.notify_on_startup;
    if draft.update.skipped_version != original.update.skipped_version {
        target.update.skipped_version = draft.update.skipped_version.clone();
    }

    // video / audio を入れ替えたあとなので、ここで選択中のプリセットの
    // 辻褄を合わせる。**この 1 行が無いと、プリセットを読み込んでから
    // 解像度を変えて「適用」したときに選択が残る**
    target.refresh_active_preset();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkey::HotkeyAction;
    use crate::settings::{AppSettings, LanguageSetting, ScreenshotDestination, ScreenshotFormat};

    use crate::ui::tests::{preset_named, sample_settings};

    #[test]
    fn commit_draft_applies_video_adjustments() {
        // ダイアログのスライダーで編集した値が共有の設定へ移ること
        let mut shared = AppSettings::default();
        let original = AppSettings::default();
        let draft = sample_settings();

        commit_draft(&mut shared, &draft, &original);

        assert_eq!(shared.video.brightness, 10);
        assert_eq!(shared.video.contrast, -20);
        assert_eq!(shared.video.saturation, 30);
    }

    #[test]
    fn commit_draft_applies_auto_reconnect_changed_by_import() {
        // 読み込みで変わった auto_reconnect は実行中の設定へ届くこと。
        // commit_draft_keeps_auto_reconnect_changed_outside_dialog と対になる
        let mut target = AppSettings::default();
        let original = target.clone();
        let mut draft = target.clone();
        draft.video.auto_reconnect = !original.video.auto_reconnect;

        commit_draft(&mut target, &draft, &original);

        assert_eq!(target.video.auto_reconnect, draft.video.auto_reconnect);
    }

    #[test]
    fn commit_draft_replaces_device_and_screenshot_sections() {
        let mut shared = AppSettings::default();
        let original = AppSettings::default();
        let draft = sample_settings();

        commit_draft(&mut shared, &draft, &original);

        assert_eq!(shared.video.format, Some("MJPEG".to_string()));
        assert_eq!(shared.video.resolution, Some((1920, 1080)));
        assert_eq!(shared.video.fps, Some(30));
        assert_eq!(shared.audio.sample_rate, Some(44100));
        assert_eq!(shared.audio.channels, Some(1));
        assert!(!shared.audio.passthrough_enabled);
        assert_eq!(shared.screenshot.sound_volume, 50.0);
        // 保存形式・品質・出力先も screenshot セクションごと差し替わる
        assert_eq!(shared.screenshot.format, ScreenshotFormat::Png);
        assert_eq!(shared.screenshot.jpeg_quality, 60);
        assert_eq!(shared.screenshot.destination, ScreenshotDestination::Both);
    }

    #[test]
    fn commit_draft_replaces_recording_section() {
        // 録画はダイアログの中だけで変わるので、セクションごと差し替える
        let mut shared = AppSettings::default();
        let original = AppSettings::default();
        let draft = sample_settings();

        commit_draft(&mut shared, &draft, &original);

        assert_eq!(shared.recording, draft.recording);
        // リプレイバッファの ON / OFF とさかのぼる長さも同じセクションで反映される
        assert!(shared.recording.replay_enabled);
        assert_eq!(shared.recording.replay_seconds, 90);
    }

    #[test]
    fn commit_draft_replaces_hotkey_assignments() {
        // ホットキーの割り当てはダイアログの中だけで変わるので、
        // ドラフトの内容でまるごと差し替える
        let mut shared = AppSettings::default();
        let original = AppSettings::default();
        let draft = sample_settings();

        commit_draft(&mut shared, &draft, &original);

        assert_eq!(shared.hotkey(HotkeyAction::Screenshot), Some("Ctrl+S"));
        assert_eq!(shared.hotkey(HotkeyAction::ToggleFullscreen), Some("F11"));
    }

    #[test]
    fn commit_draft_applies_hotkey_settings() {
        // 「フォーカスがあるときだけ反応する」もダイアログの中だけで変わる
        let mut shared = AppSettings::default();
        let original = AppSettings::default();
        let draft = sample_settings();
        assert!(draft.hotkey_settings.only_when_focused);
        assert!(draft.hotkey_settings.use_register_hotkey);

        commit_draft(&mut shared, &draft, &original);

        assert!(shared.hotkey_settings.only_when_focused);
        // キーを奪う方式の切り替えも同じセクションなので一緒に反映される
        assert!(shared.hotkey_settings.use_register_hotkey);
    }

    #[test]
    fn commit_draft_clearing_every_hotkey_reaches_the_shared_settings() {
        // すべての割り当てを外した状態を、空のマップとして反映できること。
        // 「ドラフトに何も無い＝変更なし」と扱うと、解除が反映されない
        let mut shared = AppSettings::default();
        let original = AppSettings::default();
        let mut draft = AppSettings::default();
        draft.hotkeys.clear();

        commit_draft(&mut shared, &draft, &original);

        assert!(shared.hotkeys.is_empty());
    }

    #[test]
    fn commit_draft_applies_ui_items_the_dialog_edits() {
        // 「ユーザーインターフェース」グループの 2 項目は、ダイアログで
        // 編集されていれば反映する
        let mut shared = AppSettings::default();
        let original = AppSettings::default();
        let draft = sample_settings();

        commit_draft(&mut shared, &draft, &original);

        assert_eq!(shared.ui.volume, 80.0);
        assert!(!shared.ui.maintain_aspect_ratio);
        assert_eq!(shared.ui.language, LanguageSetting::English);
    }

    #[test]
    fn commit_draft_keeps_auto_reconnect_changed_outside_dialog() {
        // 自動再接続は右クリックメニューだけで切り替える。ダイアログを開いたまま
        // 切り替えて「適用」を押しても、開いた時点の値へ巻き戻ってはいけない
        let original = sample_settings(); // auto_reconnect = false
        let draft = original.clone(); // ダイアログでは触れない項目
        let mut shared = original.clone();

        shared.video.auto_reconnect = true;

        commit_draft(&mut shared, &draft, &original);

        assert!(shared.video.auto_reconnect);
        // 同じ video セクションの他の項目はドラフトで差し替わる
        assert_eq!(shared.video.fps, Some(30));
    }

    #[test]
    fn commit_draft_keeps_window_state_changed_while_dialog_is_open() {
        // ダイアログを開いている間にウィンドウを動かす・最前面表示を切り替える
        // といった操作をしても、OK でその変更が巻き戻ってはいけない。
        // ドラフトは開いた時点のスナップショットなので、これらを丸ごと
        // 書き戻すと位置が飛ぶ
        let mut shared = sample_settings();
        let draft = shared.clone();
        let original = shared.clone();

        shared.ui.last_window_size = Some((1280.0, 720.0));
        shared.ui.last_window_pos = Some((100.0, 200.0));
        shared.ui.always_on_top = false;
        shared.ui.enable_drag_move = true;

        commit_draft(&mut shared, &draft, &original);

        assert_eq!(shared.ui.last_window_size, Some((1280.0, 720.0)));
        assert_eq!(shared.ui.last_window_pos, Some((100.0, 200.0)));
        assert!(!shared.ui.always_on_top);
        assert!(shared.ui.enable_drag_move);
    }

    #[test]
    fn commit_draft_keeps_mute_changed_outside_dialog() {
        // ミュートはダイアログに無い。ダイアログを開いたまま切り替えて
        // 「適用」を押しても、開いた時点の値へ巻き戻ってはいけない
        let original = sample_settings();
        let draft = original.clone();
        let mut shared = original.clone();

        shared.ui.muted = !original.ui.muted;

        commit_draft(&mut shared, &draft, &original);

        assert_eq!(shared.ui.muted, !original.ui.muted);
    }

    #[test]
    fn commit_draft_keeps_borderless_changed_outside_dialog() {
        // タイトルバーの表示もダイアログに無い。ダイアログを開いたまま
        // 右クリックメニューで隠して「適用」を押しても、装飾が戻ってはいけない
        let original = sample_settings();
        let draft = original.clone();
        let mut shared = original.clone();

        shared.ui.borderless = !original.ui.borderless;

        commit_draft(&mut shared, &draft, &original);

        assert_eq!(shared.ui.borderless, !original.ui.borderless);
    }

    #[test]
    fn commit_draft_keeps_drag_move_enabled_by_the_borderless_guard() {
        // 装飾を外すときのガードが有効にした「画面ドラッグ移動」も、
        // ダイアログの「適用」で切られてはいけない。切られると
        // タイトルバーもドラッグ移動も無い状態になり、ウィンドウを動かせなくなる
        let mut original = sample_settings();
        original.ui.enable_drag_move = false;
        let draft = original.clone();
        let mut shared = original.clone();

        // 右クリックメニューで「タイトルバーを隠す」を押した状態
        shared.ui.borderless = true;
        shared.ui.enable_drag_move = true;

        commit_draft(&mut shared, &draft, &original);

        assert!(shared.ui.borderless);
        assert!(shared.ui.enable_drag_move);
    }

    #[test]
    fn commit_draft_keeps_ui_items_changed_outside_dialog() {
        // ダイアログを開いたまま映像上でホイール操作をして音量を変え、
        // コンテキストメニューでアスペクト比を切り替えたあとに「適用」を
        // 押しても、それらが巻き戻ってはいけない
        let original = sample_settings();
        let draft = original.clone(); // ダイアログでは何も編集していない
        let mut shared = original.clone();

        shared.ui.volume = 150.0;
        shared.ui.maintain_aspect_ratio = true;

        commit_draft(&mut shared, &draft, &original);

        assert_eq!(shared.ui.volume, 150.0);
        assert!(shared.ui.maintain_aspect_ratio);
    }

    #[test]
    fn commit_draft_applies_ui_items_edited_in_dialog_over_outside_changes() {
        // ダイアログ側で編集していれば、外側の変更より優先する
        let original = sample_settings();
        let mut draft = original.clone();
        let mut shared = original.clone();

        draft.ui.volume = 120.0;
        draft.ui.maintain_aspect_ratio = true;
        shared.ui.volume = 150.0;

        commit_draft(&mut shared, &draft, &original);

        assert_eq!(shared.ui.volume, 120.0);
        assert!(shared.ui.maintain_aspect_ratio);
    }

    #[test]
    fn commit_draft_replaces_the_preset_list() {
        let mut target = sample_settings();
        target.presets = vec![preset_named("消えるはず", 1280, 720, 60)];
        let original = target.clone();
        let mut draft = target.clone();
        draft.presets = vec![preset_named("残るはず", 1920, 1080, 30)];

        commit_draft(&mut target, &draft, &original);

        assert_eq!(target.presets.len(), 1);
        assert_eq!(target.presets[0].name, "残るはず");
    }

    #[test]
    fn commit_draft_keeps_the_active_preset_when_the_values_match() {
        let mut target = AppSettings::default();
        let original = target.clone();
        let mut draft = target.clone();
        draft.presets = vec![preset_named("画質優先", 1920, 1080, 30)];
        assert!(draft.apply_preset("画質優先"));

        commit_draft(&mut target, &draft, &original);

        assert_eq!(target.active_preset.as_deref(), Some("画質優先"));
        assert_eq!(target.video.resolution, Some((1920, 1080)));
    }

    #[test]
    fn commit_draft_clears_the_active_preset_when_the_values_were_edited() {
        // プリセットを読み込んだあと「デバイス設定」タブで解像度を変えて
        // 「適用」した場合。選択が残ると、右クリックメニューのチェックが
        // 実際の設定と食い違う
        let mut target = AppSettings::default();
        let original = target.clone();
        let mut draft = target.clone();
        draft.presets = vec![preset_named("画質優先", 1920, 1080, 30)];
        assert!(draft.apply_preset("画質優先"));
        draft.video.fps = Some(24);

        commit_draft(&mut target, &draft, &original);

        assert_eq!(target.active_preset, None);
        assert_eq!(target.video.fps, Some(24));
    }

    #[test]
    fn commit_draft_applies_update_settings_edited_in_dialog() {
        // 「その他」タブの更新の節で変えた 3 項目が共有の設定へ移ること
        let mut shared = AppSettings::default();
        let original = AppSettings::default();
        let draft = sample_settings();

        commit_draft(&mut shared, &draft, &original);

        assert!(!shared.update.check_on_startup);
        assert!(!shared.update.notify_on_startup);
        assert_eq!(shared.update.skipped_version.as_deref(), Some("1.2.0"));
    }

    #[test]
    fn commit_draft_keeps_skipped_version_set_outside_dialog() {
        // 設定ダイアログを開いたまま、起動時の通知ダイアログで
        // 「この版は通知しない」を押した場合。「適用」で消えてはいけない
        let original = AppSettings::default();
        let draft = original.clone();
        let mut shared = original.clone();

        shared.update.skipped_version = Some("1.3.0".to_string());

        commit_draft(&mut shared, &draft, &original);

        assert_eq!(shared.update.skipped_version.as_deref(), Some("1.3.0"));
    }

    #[test]
    fn commit_draft_clears_skipped_version_released_in_dialog() {
        // 「解除」を押して「適用」した場合
        let original = sample_settings(); // skipped_version = 1.2.0
        let mut draft = original.clone();
        draft.update.skipped_version = None;
        let mut shared = original.clone();

        commit_draft(&mut shared, &draft, &original);

        assert_eq!(shared.update.skipped_version, None);
    }
}
