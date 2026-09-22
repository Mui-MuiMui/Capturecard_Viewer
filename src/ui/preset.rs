//! プリセットの保存・読み込み・削除。
//!
//! 描画からは切り離してあり、操作はすべてドラフトに対して行う
//! （`docs/design/presets.md`）。

use crate::settings::{resolved_active_preset, validate_preset_name, AppSettings, Preset};
use log::debug;

use super::ManagementMessage;

/// プリセット一覧の 1 行で押されたボタン。
///
/// ループの中でドラフトを書き換えると、一覧を借りたまま変更することになる。
/// 押されたことだけを持ち帰り、実際の変更は `app` が
/// `SettingsDialogState::apply_preset_row` で行う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresetRowAction {
    /// ドラフトの video / audio をこのプリセットで置き換える
    Load(usize),
    /// このプリセットの中身を、いまのドラフトの video / audio で置き換える
    Overwrite(usize),
    /// 一覧から消す
    Delete(usize),
}

/// 「現在:」の右に出す文言。
///
/// プリセットを読み込んだあとに手で値を変えた場合は「（変更あり）」を添える。
/// 選択そのものを外さないのは、どれを土台にしているかが分かるほうが
/// 「上書き保存」を押すときに迷わないため。
pub(super) fn active_preset_label(settings: &AppSettings) -> String {
    match (
        settings.active_preset.as_deref(),
        resolved_active_preset(settings),
    ) {
        (_, Some(name)) => name.to_string(),
        (Some(name), None) if settings.preset(name).is_some() => {
            format!("{}（変更あり）", name)
        }
        _ => "なし".to_string(),
    }
}

/// 一覧の行で押されたボタンをドラフトへ反映する。
///
/// 描画からは切り離してあり、呼ぶのは `SettingsDialogState::apply_preset_row`
/// （`commit_draft` などと同じく、設定を組み替えるだけの関数）。
pub(super) fn apply_preset_row_action(
    draft: &mut AppSettings,
    action: PresetRowAction,
    message: &mut Option<ManagementMessage>,
) {
    match action {
        PresetRowAction::Load(index) => {
            let Some(name) = draft.presets.get(index).map(|preset| preset.name.clone()) else {
                return;
            };
            draft.apply_preset(&name);
            debug!("プリセット「{}」を編集中の設定へ読み込んだ", name);
            *message = Some(ManagementMessage {
                text: format!(
                    "プリセット「{}」を読み込みました。「適用」または「OK」で反映します",
                    name
                ),
                is_error: false,
            });
        }
        PresetRowAction::Overwrite(index) => {
            let Some(name) = draft.presets.get(index).map(|preset| preset.name.clone()) else {
                return;
            };
            draft.upsert_preset(Preset::from_settings(name.clone(), draft));
            // 中身をいまの設定から作ったので、このプリセットが選択中になる
            draft.active_preset = Some(name.clone());
            debug!("プリセット「{}」を編集中の設定で上書きした", name);
            *message = Some(ManagementMessage {
                text: format!(
                    "プリセット「{}」を上書きしました。「適用」または「OK」で反映します",
                    name
                ),
                is_error: false,
            });
        }
        PresetRowAction::Delete(index) => {
            let Some(name) = draft.presets.get(index).map(|preset| preset.name.clone()) else {
                return;
            };
            draft.remove_preset(&name);
            debug!("プリセット「{}」を削除した", name);
            *message = Some(ManagementMessage {
                text: format!(
                    "プリセット「{}」を削除しました。取り消すには「キャンセル」を押してください",
                    name
                ),
                is_error: false,
            });
        }
    }
}

/// 入力された名前で、いまのドラフトをプリセットとして保存する。
///
/// 名前が使えない場合はドラフトも入力欄も動かさない。入力欄を消すと
/// 打ち直しになるので、直せる状態のまま理由だけを出す。
pub(super) fn save_new_preset(
    draft: &mut AppSettings,
    new_preset_name: &mut String,
    message: &mut Option<ManagementMessage>,
) {
    let name = match validate_preset_name(new_preset_name, &draft.presets, None) {
        Ok(name) => name,
        Err(e) => {
            *message = Some(ManagementMessage {
                text: e.message().to_string(),
                is_error: true,
            });
            return;
        }
    };

    draft.upsert_preset(Preset::from_settings(name.clone(), draft));
    draft.active_preset = Some(name.clone());
    new_preset_name.clear();

    debug!("プリセット「{}」を編集中の設定へ追加した", name);
    *message = Some(ManagementMessage {
        text: format!(
            "プリセット「{}」を追加しました。「適用」または「OK」で反映します",
            name
        ),
        is_error: false,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::settings::AppSettings;

    use crate::ui::tests::{defaults_with_presets, preset_named, sample_settings};

    #[test]
    fn active_preset_label_without_selection_reads_none() {
        let settings = AppSettings::default();

        assert_eq!(active_preset_label(&settings), "なし");
    }

    #[test]
    fn active_preset_label_matching_selection_is_the_name() {
        let mut settings = defaults_with_presets(vec![preset_named("画質優先", 1920, 1080, 30)]);
        assert!(settings.apply_preset("画質優先"));

        assert_eq!(active_preset_label(&settings), "画質優先");
    }

    #[test]
    fn active_preset_label_after_editing_says_modified() {
        let mut settings = defaults_with_presets(vec![preset_named("画質優先", 1920, 1080, 30)]);
        assert!(settings.apply_preset("画質優先"));
        settings.video.fps = Some(24);

        assert_eq!(active_preset_label(&settings), "画質優先（変更あり）");
    }

    #[test]
    fn active_preset_label_for_a_removed_preset_reads_none() {
        // 名前だけが残っている状態。「（変更あり）」ではなく「なし」にする。
        // 戻す先のプリセットが無いので、変更という言い方が合わない
        let settings = AppSettings {
            active_preset: Some("もう無い".to_string()),
            ..AppSettings::default()
        };

        assert_eq!(active_preset_label(&settings), "なし");
    }

    #[test]
    fn save_new_preset_adds_the_current_video_and_audio() {
        let mut draft = sample_settings();
        let mut name = "  低遅延優先 ".to_string();
        let mut message = None;

        save_new_preset(&mut draft, &mut name, &mut message);

        assert_eq!(draft.presets.len(), 1);
        // 前後の空白は落とす
        assert_eq!(draft.presets[0].name, "低遅延優先");
        assert_eq!(draft.presets[0].video.resolution, draft.video.resolution);
        assert_eq!(draft.presets[0].audio.sample_rate, draft.audio.sample_rate);
        assert_eq!(draft.active_preset.as_deref(), Some("低遅延優先"));
        // 成功したら入力欄を空にする
        assert!(name.is_empty());
        assert_eq!(message.map(|m| m.is_error), Some(false));
    }

    #[test]
    fn save_new_preset_with_a_duplicate_name_changes_nothing() {
        let mut draft = sample_settings();
        draft.presets = vec![preset_named("低遅延優先", 1280, 720, 60)];
        let mut name = "低遅延優先".to_string();
        let mut message = None;

        save_new_preset(&mut draft, &mut name, &mut message);

        assert_eq!(draft.presets.len(), 1);
        assert_eq!(draft.presets[0].video.resolution, Some((1280, 720)));
        // 打ち直しにならないよう入力は残す
        assert_eq!(name, "低遅延優先");
        assert_eq!(message.map(|m| m.is_error), Some(true));
    }

    #[test]
    fn save_new_preset_with_an_empty_name_changes_nothing() {
        let mut draft = sample_settings();
        let mut name = "   ".to_string();
        let mut message = None;

        save_new_preset(&mut draft, &mut name, &mut message);

        assert!(draft.presets.is_empty());
        assert_eq!(message.map(|m| m.is_error), Some(true));
    }

    #[test]
    fn preset_row_load_replaces_only_video_and_audio() {
        let mut draft = sample_settings();
        draft.presets = vec![preset_named("低遅延優先", 1280, 720, 60)];
        let screenshot_before = draft.screenshot.clone();
        let hotkeys_before = draft.hotkeys.clone();
        let mut message = None;

        apply_preset_row_action(&mut draft, PresetRowAction::Load(0), &mut message);

        assert_eq!(draft.video.resolution, Some((1280, 720)));
        assert_eq!(draft.active_preset.as_deref(), Some("低遅延優先"));
        assert_eq!(draft.screenshot.format, screenshot_before.format);
        assert_eq!(draft.hotkeys, hotkeys_before);
    }

    #[test]
    fn preset_row_overwrite_stores_the_current_values() {
        let mut draft = sample_settings();
        draft.presets = vec![preset_named("低遅延優先", 1280, 720, 60)];
        let mut message = None;

        apply_preset_row_action(&mut draft, PresetRowAction::Overwrite(0), &mut message);

        assert_eq!(draft.presets.len(), 1);
        assert_eq!(draft.presets[0].name, "低遅延優先");
        assert_eq!(draft.presets[0].video.resolution, draft.video.resolution);
        assert_eq!(draft.active_preset.as_deref(), Some("低遅延優先"));
    }

    #[test]
    fn preset_row_delete_removes_the_entry_and_the_selection() {
        let mut draft = sample_settings();
        draft.presets = vec![preset_named("低遅延優先", 1280, 720, 60)];
        assert!(draft.apply_preset("低遅延優先"));
        let mut message = None;

        apply_preset_row_action(&mut draft, PresetRowAction::Delete(0), &mut message);

        assert!(draft.presets.is_empty());
        assert_eq!(draft.active_preset, None);
    }

    #[test]
    fn preset_row_action_with_a_stale_index_changes_nothing() {
        // 一覧を描いてから反映するまでの間は 1 フレームも空かないが、
        // 添字で触る以上は範囲外でも落ちないようにしておく
        let mut draft = sample_settings();
        draft.presets = vec![preset_named("低遅延優先", 1280, 720, 60)];
        let mut message = None;

        apply_preset_row_action(&mut draft, PresetRowAction::Delete(5), &mut message);

        assert_eq!(draft.presets.len(), 1);
        assert!(message.is_none());
    }
}
