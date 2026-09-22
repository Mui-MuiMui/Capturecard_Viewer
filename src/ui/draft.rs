//! ドラフトの反映・読み込み・初期化。
//!
//! どれも設定を組み替えるだけで描画を含まない。呼ぶのは `state` と
//! `app::settings_dialog`（`docs/design/settings-dialog.md`）。

use crate::settings::AppSettings;

/// ドラフトのうち、設定ダイアログが編集する範囲だけを実行中の設定へ反映する。
/// `original` はダイアログを開いた時点の設定。
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
    // ホットキーの割り当てもダイアログの中だけで変わる
    target.hotkeys = draft.hotkeys.clone();
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

    // video / audio を入れ替えたあとなので、ここで選択中のプリセットの
    // 辻褄を合わせる。**この 1 行が無いと、プリセットを読み込んでから
    // 解像度を変えて「適用」したときに選択が残る**
    target.refresh_active_preset();
}

/// 読み込んだ設定からドラフトを作る。
///
/// `current` は差し替える前のドラフト。`imported` の `ui` セクションは
/// **ダイアログが編集する 2 項目（`volume` / `maintain_aspect_ratio`）だけ**を
/// 採り、残りは `current` の値を保つ。
///
/// ウィンドウの位置とサイズを持ち込まないのがいちばんの理由。別の画面構成の
/// PC で書き出したファイルを読むと、画面の外にウィンドウが飛ぶ。
///
/// 他の `ui` の項目（`always_on_top` / `enable_drag_move` / `show_stats_overlay` /
/// `muted`）を持ち込まないのは、**`commit_draft` がそれらを反映しないため。**
/// 右クリックメニューで切り替えるものなので、ドラフトへ入れても「適用」で
/// 実行中の値へ戻る。読めたように見えて反映されない項目を作るより、
/// 最初から触らないほうが分かりやすい。
///
/// `video.auto_reconnect` は同じく右クリックメニューで切り替えるが、
/// `commit_draft` が「ドラフトで変わったときだけ反映する」形になっているので
/// **読み込んだ値をそのまま採る。** 読み込みも初期化もドラフトの編集なので、
/// 反映される側が正しい。
///
/// プリセット（`presets` / `active_preset`）は**読み込んだファイルのものを採る。**
/// 書き出しは設定ファイル丸ごとなのでプリセットも含まれており、読み込みで
/// 落とすと往復にならない。別の PC で作ったプリセットを持ち込むのも、
/// 書き出し・読み込みの主な用途のひとつ。
///
/// **`commit_draft` が反映しない項目を増やすときは、ここでも `current` の値を
/// 保つこと。逆に反映する項目を増やすときは、ここでも `imported` から採ること。**
/// 2 つが食い違うと、読み込んだのに反映されない項目が生まれる。
pub fn draft_from_imported(imported: AppSettings, current: &AppSettings) -> AppSettings {
    let mut draft = imported;
    let volume = draft.ui.volume;
    let maintain_aspect_ratio = draft.ui.maintain_aspect_ratio;

    draft.ui = current.ui.clone();
    draft.ui.volume = volume;
    draft.ui.maintain_aspect_ratio = maintain_aspect_ratio;
    draft
}

/// 初期化でドラフトを作る。
///
/// 既定値を読み込んだのと同じ扱いにしてある。ウィンドウの位置とサイズが
/// 保たれるのも、`ui` の他の項目が現状のまま残るのも読み込みと同じ。
///
/// 初期化でウィンドウが既定の大きさに戻らないのは意図した動作。位置と
/// サイズは設定ダイアログで触れる項目ではなく、初期化したい対象でもない。
///
/// **プリセットも消さない。** 初期化は「いまの設定を既定へ戻す」操作で、
/// ユーザーが作り溜めた名前付きの設定を捨てる操作ではない。捨ててしまうと
/// 復旧の手段が書き出したファイルしかなく、初期化を押しにくくなる。
/// 消したいときは「その他」タブのプリセット一覧から 1 つずつ削除する。
pub fn draft_from_defaults(current: &AppSettings) -> AppSettings {
    let mut draft = draft_from_imported(AppSettings::default(), current);
    draft.presets = current.presets.clone();
    draft.active_preset = current.active_preset.clone();
    // video / audio は既定値へ戻っているので、たいていここで選択が外れる
    draft.refresh_active_preset();
    draft
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkey::HotkeyAction;
    use crate::settings::{AppSettings, Preset, ScreenshotDestination, ScreenshotFormat};

    use crate::ui::tests::{defaults_with_presets, preset_named, sample_settings};

    #[test]
    fn draft_from_imported_takes_device_and_screenshot_sections() {
        let imported = sample_settings();
        let current = AppSettings::default();

        let draft = draft_from_imported(imported.clone(), &current);

        assert_eq!(draft.video.device_name, imported.video.device_name);
        assert_eq!(draft.video.resolution, imported.video.resolution);
        assert_eq!(draft.video.auto_reconnect, imported.video.auto_reconnect);
        assert_eq!(draft.audio.sample_rate, imported.audio.sample_rate);
        assert_eq!(draft.screenshot.format, imported.screenshot.format);
        assert_eq!(draft.hotkeys, imported.hotkeys);
    }

    #[test]
    fn draft_from_imported_takes_video_adjustments() {
        // 映像調整は video セクションごと差し替わる。commit_draft も
        // video を丸ごと入れるので、読み込んだ値がそのまま反映される
        let imported = sample_settings();
        let current = AppSettings::default();

        let draft = draft_from_imported(imported.clone(), &current);

        assert_eq!(draft.video.brightness, imported.video.brightness);
        assert_eq!(draft.video.contrast, imported.video.contrast);
        assert_eq!(draft.video.saturation, imported.video.saturation);
    }

    #[test]
    fn draft_from_defaults_resets_video_adjustments() {
        // 初期化で 3 つとも無調整へ戻ること
        let current = sample_settings();

        let draft = draft_from_defaults(&current);

        assert_eq!(draft.video.brightness, 0);
        assert_eq!(draft.video.contrast, 0);
        assert_eq!(draft.video.saturation, 0);
    }

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
    fn draft_from_imported_takes_auto_reconnect() {
        // auto_reconnect は右クリックメニューで切り替えるが、commit_draft が
        // 「ドラフトで変わったときだけ反映する」形なので読み込める
        let imported = sample_settings();
        let current = AppSettings::default();
        assert_ne!(imported.video.auto_reconnect, current.video.auto_reconnect);

        let draft = draft_from_imported(imported.clone(), &current);

        assert_eq!(draft.video.auto_reconnect, imported.video.auto_reconnect);
    }

    #[test]
    fn draft_from_defaults_takes_the_default_auto_reconnect() {
        // 初期化も同じ。自動再接続を切っている状態から初期化すれば、
        // 既定値（オン）へ戻る
        let mut current = sample_settings();
        current.video.auto_reconnect = false;

        let draft = draft_from_defaults(&current);

        assert!(draft.video.auto_reconnect);
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
    fn draft_from_imported_keeps_the_window_geometry() {
        // 別の画面構成で書き出したファイルを読んでも、ウィンドウが
        // 画面の外へ飛ばないこと
        let imported = sample_settings();
        let mut current = AppSettings::default();
        current.ui.last_window_size = Some((1280.0, 720.0));
        current.ui.last_window_pos = Some((100.0, 50.0));

        let draft = draft_from_imported(imported, &current);

        assert_eq!(draft.ui.last_window_size, Some((1280.0, 720.0)));
        assert_eq!(draft.ui.last_window_pos, Some((100.0, 50.0)));
    }

    #[test]
    fn draft_from_imported_takes_the_two_ui_items_the_dialog_edits() {
        // commit_draft が反映する 2 項目だけは読み込む。
        // 読み込んでも反映されない項目を作らないため
        let imported = sample_settings();
        let current = AppSettings::default();
        assert_ne!(imported.ui.volume, current.ui.volume);
        assert_ne!(
            imported.ui.maintain_aspect_ratio,
            current.ui.maintain_aspect_ratio
        );

        let draft = draft_from_imported(imported.clone(), &current);

        assert_eq!(draft.ui.volume, imported.ui.volume);
        assert_eq!(
            draft.ui.maintain_aspect_ratio,
            imported.ui.maintain_aspect_ratio
        );
    }

    #[test]
    fn draft_from_imported_keeps_ui_items_the_dialog_cannot_apply() {
        // always_on_top などは右クリックメニューで切り替えるもので、
        // commit_draft が反映しない。読み込んでも「適用」で消えるだけなので、
        // 最初からドラフトへ入れない
        let mut imported = sample_settings();
        imported.ui.borderless = true;
        let current = AppSettings::default();
        assert_ne!(imported.ui.always_on_top, current.ui.always_on_top);
        assert_ne!(imported.ui.borderless, current.ui.borderless);

        let draft = draft_from_imported(imported, &current);

        assert_eq!(draft.ui.always_on_top, current.ui.always_on_top);
        assert_eq!(draft.ui.enable_drag_move, current.ui.enable_drag_move);
        assert_eq!(draft.ui.show_stats_overlay, current.ui.show_stats_overlay);
        assert_eq!(draft.ui.muted, current.ui.muted);
        assert_eq!(draft.ui.borderless, current.ui.borderless);
    }

    #[test]
    fn draft_from_defaults_resets_the_settings_but_not_the_window_geometry() {
        let mut current = sample_settings();
        current.ui.last_window_size = Some((640.0, 480.0));
        current.ui.last_window_pos = Some((5.0, 6.0));
        let defaults = AppSettings::default();

        let draft = draft_from_defaults(&current);

        assert_eq!(draft.video.resolution, defaults.video.resolution);
        assert_eq!(draft.audio.sample_rate, defaults.audio.sample_rate);
        assert_eq!(draft.screenshot.format, defaults.screenshot.format);
        assert_eq!(draft.hotkeys, defaults.hotkeys);
        assert_eq!(draft.ui.volume, defaults.ui.volume);
        assert_eq!(
            draft.ui.maintain_aspect_ratio,
            defaults.ui.maintain_aspect_ratio
        );
        assert_eq!(draft.ui.last_window_size, Some((640.0, 480.0)));
        assert_eq!(draft.ui.last_window_pos, Some((5.0, 6.0)));
    }

    #[test]
    fn imported_draft_reaches_the_shared_settings_on_commit() {
        // 読み込み → 「適用」の一連。commit_draft が拾う範囲と
        // draft_from_imported が読む範囲が噛み合っていることを見る
        let mut target = AppSettings::default();
        let original = target.clone();
        let imported = sample_settings();

        let draft = draft_from_imported(imported.clone(), &original);
        commit_draft(&mut target, &draft, &original);

        assert_eq!(target.video.device_name, imported.video.device_name);
        assert_eq!(target.screenshot.format, imported.screenshot.format);
        assert_eq!(target.hotkeys, imported.hotkeys);
        assert_eq!(target.ui.volume, imported.ui.volume);
        assert_eq!(
            target.ui.maintain_aspect_ratio,
            imported.ui.maintain_aspect_ratio
        );
        // ウィンドウの位置とサイズは動かない
        assert_eq!(target.ui.last_window_size, original.ui.last_window_size);
        assert_eq!(target.ui.last_window_pos, original.ui.last_window_pos);
        // 自動再接続は読み込んだ値が届く
        assert_eq!(target.video.auto_reconnect, imported.video.auto_reconnect);
        // commit_draft が反映しない項目は、ドラフトにも実行中の値が入っている。
        // 「読み込んだのに反映されない」項目が生まれていないこと
        assert_eq!(target.ui.always_on_top, original.ui.always_on_top);
        assert_eq!(draft.ui.always_on_top, original.ui.always_on_top);
    }

    #[test]
    fn reset_draft_reaches_the_shared_settings_on_commit() {
        // 初期化 →「適用」の一連。自動再接続を切っている状態から初期化すれば
        // 既定値（オン）へ戻り、ドラフトと実行中の設定が食い違わないこと
        let mut target = sample_settings();
        target.video.auto_reconnect = false;
        let original = target.clone();
        let defaults = AppSettings::default();

        let draft = draft_from_defaults(&original);
        commit_draft(&mut target, &draft, &original);

        assert_eq!(target.video.resolution, defaults.video.resolution);
        assert_eq!(target.hotkeys, defaults.hotkeys);
        assert_eq!(target.ui.volume, defaults.ui.volume);
        assert_eq!(target.video.auto_reconnect, defaults.video.auto_reconnect);
        assert_eq!(draft.video.auto_reconnect, defaults.video.auto_reconnect);
        assert_eq!(target.ui.last_window_size, original.ui.last_window_size);
        assert_eq!(target.ui.last_window_pos, original.ui.last_window_pos);
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
    fn draft_from_defaults_keeps_presets() {
        // **初期化でプリセットを消さない。** 消すと復旧の手段が
        // 書き出したファイルしかなくなり、初期化を押しにくくなる
        let mut current = sample_settings();
        current.presets = vec![
            preset_named("低遅延優先", 1280, 720, 60),
            preset_named("画質優先", 1920, 1080, 30),
        ];

        let draft = draft_from_defaults(&current);

        assert_eq!(draft.presets, current.presets);
        // 他の項目は既定値へ戻る
        assert_eq!(
            draft.video.resolution,
            AppSettings::default().video.resolution
        );
    }

    #[test]
    fn draft_from_defaults_clears_the_active_preset_that_no_longer_matches() {
        // 初期化で video / audio が既定値へ戻るので、選択中のままにはできない
        let mut current = sample_settings();
        current.presets = vec![preset_named("画質優先", 1920, 1080, 30)];
        current.active_preset = Some("画質優先".to_string());

        let draft = draft_from_defaults(&current);

        assert_eq!(draft.active_preset, None);
        assert_eq!(draft.presets.len(), 1);
    }

    #[test]
    fn draft_from_defaults_keeps_an_active_preset_that_still_matches() {
        // 既定値と同じ中身のプリセットを選んでいた場合は選択が残る
        let mut current = sample_settings();
        current.presets = vec![Preset::from_settings(
            "既定と同じ".to_string(),
            &AppSettings::default(),
        )];
        current.active_preset = Some("既定と同じ".to_string());

        let draft = draft_from_defaults(&current);

        assert_eq!(draft.active_preset.as_deref(), Some("既定と同じ"));
    }

    #[test]
    fn draft_from_imported_takes_presets_from_the_file() {
        // 書き出しは設定ファイル丸ごとなのでプリセットも含まれる。
        // 読み込みで落とすと往復にならない
        let mut imported = sample_settings();
        imported.presets = vec![preset_named("持ち込み", 1920, 1080, 30)];
        imported.active_preset = Some("持ち込み".to_string());
        let current = defaults_with_presets(vec![preset_named("手元", 1280, 720, 60)]);

        let draft = draft_from_imported(imported.clone(), &current);

        assert_eq!(draft.presets, imported.presets);
        assert_eq!(draft.active_preset.as_deref(), Some("持ち込み"));
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
}
