//! 設定の読み込みと初期化でドラフトを作る。
//!
//! 描画を含まない。呼ぶのは `app::settings_dialog`（`docs/design/settings-dialog.md`）。
//! 作ったドラフトを実行中の設定へ反映するのは `draft.rs` の `commit_draft`。
//! **`commit_draft` が反映する項目を増減させたら、ここも合わせること**（`GUARDRAIL.md`）。

use crate::settings::AppSettings;

/// 読み込んだ設定からドラフトを作る。
///
/// `current` は差し替える前のドラフト。`imported` の `ui` セクションは
/// **ダイアログが編集する 3 項目（`volume` / `maintain_aspect_ratio` / `language`）
/// だけ**を採り、残りは `current` の値を保つ。
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
/// 更新の設定（`update`）も読み込んだファイルのものを採る。`commit_draft` は
/// `skipped_version` を「ドラフトで変わったときだけ」反映するので、読み込んだ
/// 値が開いた時点と違えば反映される。
///
/// **`commit_draft` が反映しない項目を増やすときは、ここでも `current` の値を
/// 保つこと。逆に反映する項目を増やすときは、ここでも `imported` から採ること。**
/// 2 つが食い違うと、読み込んだのに反映されない項目が生まれる。
pub fn draft_from_imported(imported: AppSettings, current: &AppSettings) -> AppSettings {
    let mut draft = imported;
    let volume = draft.ui.volume;
    let maintain_aspect_ratio = draft.ui.maintain_aspect_ratio;
    let language = draft.ui.language;

    draft.ui = current.ui.clone();
    draft.ui.volume = volume;
    draft.ui.maintain_aspect_ratio = maintain_aspect_ratio;
    draft.ui.language = language;
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
    use crate::settings::{AppSettings, Preset};
    use crate::ui::draft::commit_draft;

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
        // commit_draft が反映する項目なので、読み込んだ値を採る
        assert_eq!(draft.hotkey_settings, imported.hotkey_settings);
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
    fn draft_from_imported_takes_the_ui_items_the_dialog_edits() {
        // commit_draft が反映する 3 項目だけは読み込む。
        // 読み込んでも反映されない項目を作らないため
        let imported = sample_settings();
        let current = AppSettings::default();
        assert_ne!(imported.ui.volume, current.ui.volume);
        assert_ne!(
            imported.ui.maintain_aspect_ratio,
            current.ui.maintain_aspect_ratio
        );
        assert_ne!(imported.ui.language, current.ui.language);

        let draft = draft_from_imported(imported.clone(), &current);

        assert_eq!(draft.ui.volume, imported.ui.volume);
        assert_eq!(
            draft.ui.maintain_aspect_ratio,
            imported.ui.maintain_aspect_ratio
        );
        assert_eq!(draft.ui.language, imported.ui.language);
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
        // 言語も既定（自動）へ戻る
        assert_eq!(draft.ui.language, defaults.ui.language);
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
        assert_eq!(target.ui.language, imported.ui.language);
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
    fn draft_from_imported_takes_recording_section() {
        // commit_draft が反映する項目なので、読み込んだ値を採る
        let imported = sample_settings();
        let draft = draft_from_imported(imported.clone(), &AppSettings::default());

        assert_eq!(draft.recording, imported.recording);
        assert!(draft.recording.replay_enabled);
        assert_eq!(draft.recording.replay_seconds, 90);
    }

    #[test]
    fn draft_from_defaults_resets_recording_section() {
        let draft = draft_from_defaults(&sample_settings());

        assert_eq!(draft.recording, AppSettings::default().recording);
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
    fn draft_from_imported_takes_update_settings() {
        let imported = sample_settings();
        let current = AppSettings::default();

        let draft = draft_from_imported(imported.clone(), &current);

        assert_eq!(draft.update, imported.update);
    }

    #[test]
    fn draft_from_defaults_resets_update_settings() {
        let current = sample_settings();

        let draft = draft_from_defaults(&current);

        assert!(draft.update.check_on_startup);
        assert!(draft.update.notify_on_startup);
        assert_eq!(draft.update.skipped_version, None);
    }
}
