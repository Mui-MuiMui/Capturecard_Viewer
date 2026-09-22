//! 設定ダイアログの操作の受け止めと、プリセットの適用。
//!
//! ドラフトの反映・保存・クローズをここで行うのは、UI 側に状態と副作用を
//! 持たせないため（`docs/ARCHITECTURE.md` の「UI は状態を持たない」）。
//! ディスクへの書き出しそのものは `super::settings_store`。

use super::CaptureCardViewer;
use crate::overlay::OverlayContent;
use crate::settings;
use crate::status::{self, ErrorSource};
use crate::ui;
use chrono::Local;
use log::{debug, error, info, warn};
use std::time::{Duration, Instant};

/// プリセットを切り替えたときに OSD を出しておく時間。
///
/// 音量と同じ長さにしてある。どちらも「押した結果がこれで合っているか」を
/// 確かめるための表示で、読み終わる前に消えても困るし、残り続けても邪魔になる
const PRESET_OSD_DURATION: Duration = Duration::from_millis(1500);

impl CaptureCardViewer {
    /// プリセットを実行中の設定へ適用する。
    ///
    /// 変わるのは `video` と `audio` だけ。デバイスを開き直すかどうかは
    /// `apply_settings` の差分判定に任せるので、同じ内容のプリセットを
    /// 選び直しても映像は途切れない。
    pub(super) fn apply_preset_by_name(&mut self, name: &str) {
        // ロックはここで手放す。apply_settings が同じロックを取る
        let applied = match self.settings.lock() {
            Ok(mut settings) => settings.apply_preset(name),
            Err(_) => {
                warn!("プリセットの適用で settings のロックを取得できない");
                return;
            }
        };

        if !applied {
            // 一覧を読んでから選ぶまでの間に消える経路は無いが、
            // 名前で引いている以上は起こりうるものとして扱う
            warn!("プリセット「{}」が見つからない", name);
            return;
        }

        info!("プリセット「{}」へ切り替えた", name);
        self.mark_settings_dirty();
        self.apply_settings(false);
        self.transient_overlay.show(
            OverlayContent::Text(format!("プリセット: {}", name)),
            PRESET_OSD_DURATION,
            Instant::now(),
        );
    }

    /// 設定ダイアログの 1 フレームのイベントを処理する。
    ///
    /// 状態を書き換えるのも副作用を起こすのもここだけ
    /// （`docs/ARCHITECTURE.md` の「UI は状態を持たない」）。
    ///
    /// **受け取った順に処理する。** 名前を打ちながら「保存」を押した場合の
    /// `SetNewPresetName` → `SaveNewPreset` のように、並びが意味を持つ。
    /// `Dialog`（適用 / OK / キャンセル）は必ず最後に来る。
    pub(super) fn handle_settings_events(&mut self, events: Vec<ui::SettingsEvent>) {
        for event in events {
            match event {
                ui::SettingsEvent::Dialog(action) => self.apply_dialog_action(action),
                ui::SettingsEvent::SelectTab(tab) => self.settings_dialog.select_tab(tab),
                ui::SettingsEvent::TestSound => self.play_test_sound(),
                ui::SettingsEvent::ExportSettings => self.export_settings_to_file(),
                ui::SettingsEvent::ImportSettings => self.import_settings_into_draft(),
                ui::SettingsEvent::ResetDraft => self.reset_draft_to_defaults(),
                ui::SettingsEvent::SetResetConfirm(confirming) => {
                    self.settings_dialog.set_reset_confirm(confirming)
                }
                ui::SettingsEvent::SetNewPresetName(name) => {
                    self.settings_dialog.set_new_preset_name(name)
                }
                ui::SettingsEvent::SaveNewPreset => self.settings_dialog.save_new_preset(),
                ui::SettingsEvent::PresetRow(action) => {
                    self.settings_dialog.apply_preset_row(action)
                }
                ui::SettingsEvent::OpenHotkeyCapture(action) => {
                    // どのアクションを編集しているかを入力ダイアログへ渡す。
                    // 実際の描画はこのフレームの後半（`update` のホットキー
                    // ダイアログの節）なので、開くのは今で間に合う
                    self.settings_dialog.hotkey_capture_mut().begin_for(action);
                    self.show_hotkey_dialog = true;
                }
                ui::SettingsEvent::PickScreenshotFolder => self.pick_screenshot_folder(),
                ui::SettingsEvent::PickSoundFile => self.pick_sound_file(),
                ui::SettingsEvent::Capability(event) => self.apply_capability_event(event),
            }
        }
    }

    /// デバイス能力のキャッシュに対する要求を反映する。
    ///
    /// キャッシュを触るのは UI スレッドだけなのでロックは要らない。実際の
    /// 問い合わせは、溜まった要求を `dispatch_capability_requests` が
    /// ワーカーへ流したときに始まる。
    fn apply_capability_event(&mut self, event: ui::CapabilityEvent) {
        let dialog = &mut self.settings_dialog;
        match event {
            ui::CapabilityEvent::RequestVideo(device) => {
                dialog.capabilities_mut().request(&device);
            }
            ui::CapabilityEvent::RetryVideo(device) => {
                dialog.capabilities_mut().retry(&device);
            }
            ui::CapabilityEvent::ExpectVideoDefaults(device) => {
                dialog.capabilities_mut().expect_defaults(&device)
            }
            ui::CapabilityEvent::ClearVideoDefaults(device) => {
                dialog.capabilities_mut().clear_awaiting_defaults(&device)
            }
            ui::CapabilityEvent::RequestAudio(direction, key) => {
                dialog.audio_capabilities_mut(direction).request(&key);
            }
            ui::CapabilityEvent::RetryAudio(direction, key) => {
                dialog.audio_capabilities_mut(direction).retry(&key);
            }
            ui::CapabilityEvent::ExpectAudioDefaults(direction, key) => dialog
                .audio_capabilities_mut(direction)
                .expect_defaults(&key),
            ui::CapabilityEvent::ClearAudioDefaults(direction, key) => dialog
                .audio_capabilities_mut(direction)
                .clear_awaiting_defaults(&key),
        }
    }

    /// スクリーンショットの保存フォルダーをファイルダイアログで選ぶ。
    ///
    /// 入れるのはドラフトなので、反映は「適用」か「OK」のとき。
    /// `rfd` は UI スレッドを止めるモーダルだが、描画を終えたあとに開くので
    /// 止まった途中のフレームが画面に残ることはない。
    fn pick_screenshot_folder(&mut self) {
        let Some(folder) = rfd::FileDialog::new().pick_folder() else {
            debug!("スクリーンショットの保存先の選択がキャンセルされた");
            return;
        };
        let Some(draft) = self.settings_dialog.draft_mut() else {
            warn!("ドラフトが無い状態で保存先の選択が要求された");
            return;
        };
        draft.screenshot.save_folder = folder;
    }

    /// スクリーンショットの効果音をファイルダイアログで選ぶ。
    fn pick_sound_file(&mut self) {
        let Some(file) = rfd::FileDialog::new()
            .add_filter("音声ファイル", &["mp3", "wav", "ogg"])
            .pick_file()
        else {
            debug!("効果音ファイルの選択がキャンセルされた");
            return;
        };
        let Some(draft) = self.settings_dialog.draft_mut() else {
            warn!("ドラフトが無い状態で効果音ファイルの選択が要求された");
            return;
        };
        draft.screenshot.sound_file = Some(file);
    }

    /// 「適用」「OK」「キャンセル」を処理する。
    ///
    /// ドラフトの反映・保存・クローズをここで行うのは、UI 側に状態と副作用を
    /// 持たせないため（`docs/ARCHITECTURE.md` の「UI は状態を持たない」）。
    fn apply_dialog_action(&mut self, action: ui::SettingsDialogAction) {
        let transition = ui::SettingsDialogState::transition_for(action);

        if transition.commit_draft {
            if let Ok(mut settings) = self.settings.lock() {
                self.settings_dialog.commit_into(&mut settings);
            } else {
                warn!("設定ダイアログの反映に失敗した: settings のロックを取れない");
            }
            // 反映した内容でデバイスを開き直す
            self.apply_settings(false);
            // 「読み込みました。適用してください」の類の案内は役目を終えている。
            // 残すと、反映済みなのにまだ何かする必要があるように読める
            self.settings_dialog.clear_management_message();
        }

        if transition.save_to_file {
            // 「適用」と「OK」はユーザーの明示的な保存操作なので、
            // デバウンスを待たずに書き出す。読めなかった設定ファイルが
            // 残っている場合も、上書きするかはユーザーが決めることなので止めない
            let saved = self.save_settings_now();
            // 明示的な保存が通ったなら、守るべき壊れたファイルはもう無い。
            // 以降はウィンドウ位置や音量の自動保存も通常どおり行う
            self.autosave.note_explicit_save(saved);
        }

        if transition.close {
            self.settings_dialog.end_edit();
            self.show_settings = false;
        }
    }

    /// 設定ダイアログの「テスト再生」で効果音を鳴らす。
    ///
    /// ダイアログを開いている間はドラフトの音量で鳴らす。スライダーを
    /// 動かした結果をその場で確かめられるようにするため。
    /// 効果音のファイル自体は「適用」か「OK」まで差し替わらない。
    fn play_test_sound(&self) {
        // この操作が返るのはダイアログを描画しているときだけなので、ドラフトは必ずある
        let Some(volume) = self
            .settings_dialog
            .draft()
            .map(|draft| draft.screenshot.sound_volume)
        else {
            return;
        };

        if let Ok(ss) = self.screenshot_manager.lock() {
            ss.play_screenshot_sound(volume);
        } else {
            warn!("テスト再生で screenshot_manager のロックを取得できない");
        }
    }

    /// 設定ダイアログの「設定を書き出す」。
    ///
    /// 書き出すのは**実行中の設定**で、編集中のドラフトではない。ドラフトは
    /// まだ「適用」されていない下書きなので、それをファイルとして配ると、
    /// 手元で動いている設定と中身が食い違う。
    ///
    /// `rfd` の保存ダイアログは UI スレッドを止めるモーダルで、出している間は
    /// 映像の更新も止まる。既存の効果音ファイル選択と同じ割り切り。
    /// **設定のロックは先に手放す。** 握ったままダイアログを出すと、
    /// ユーザーが閉じるまで設定に触る全ての経路が止まる。
    fn export_settings_to_file(&mut self) {
        // ロックの結果を先に畳んでから self を可変で借りる。match の中で
        // 失敗を報告しようとすると、MutexGuard の一時値が生きたままになる
        let settings = self.settings.lock().ok().map(|settings| settings.clone());
        let Some(settings) = settings else {
            warn!("設定の書き出しで settings のロックを取得できない");
            self.report_settings_error("設定を読み取れない".to_string());
            return;
        };

        let Some(path) = rfd::FileDialog::new()
            .set_file_name(&settings::export_file_name(&Local::now()))
            .add_filter("設定ファイル", &["toml"])
            .save_file()
        else {
            debug!("設定の書き出しがキャンセルされた");
            return;
        };

        match settings::export_to(&path, &settings) {
            Ok(()) => {
                info!("設定を {} へ書き出した", path.display());
                self.settings_dialog
                    .set_management_message(format!("{} へ書き出しました", path.display()), false);
            }
            Err(e) => {
                // 書き出し先は SettingsError が持っているので、ここでは足さない
                error!("{e}");
                self.report_settings_error(e.to_string());
            }
        }
    }

    /// 設定ダイアログの「設定を読み込む」。
    ///
    /// 読めた内容は**ドラフトへ入れるだけ**で、実行中の設定には触らない。
    /// 読み込んだ瞬間に反映すると「キャンセル」で取り消せないため、
    /// 他の編集と同じく「適用」「OK」を通す。
    ///
    /// 読めなかった場合はドラフトを一切動かさない。半分だけ読み込んだ状態を
    /// 作ると、どこまでが元の値か分からなくなる。
    fn import_settings_into_draft(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("設定ファイル", &["toml"])
            .pick_file()
        else {
            debug!("設定の読み込みがキャンセルされた");
            return;
        };

        let imported = match settings::import_from(&path) {
            Ok(imported) => imported,
            Err(e) => {
                // 読み込み元は SettingsError が持っているので、ここでは足さない
                error!("{e}");
                self.report_settings_error(e.to_string());
                return;
            }
        };

        // この操作が返るのはダイアログを描画しているときだけなので、ドラフトは必ずある
        let Some(draft) = self.settings_dialog.draft_mut() else {
            warn!("ドラフトが無い状態で設定の読み込みが要求された");
            return;
        };
        let merged = ui::draft_from_imported(imported, draft);
        *draft = merged;

        info!("設定ファイル {} を編集中の設定へ読み込んだ", path.display());
        self.settings_dialog.set_management_message(
            format!(
                "{} を読み込みました。「適用」または「OK」で反映します",
                path.display()
            ),
            false,
        );
    }

    /// 設定ダイアログの「設定を初期化」。
    ///
    /// 読み込みと同じくドラフトを差し替えるだけ。確認は `show_other_tab` の
    /// 2 段階ボタンで済んでいるので、ここでは聞き直さない。
    fn reset_draft_to_defaults(&mut self) {
        // 読み込みと同じく、返るのはダイアログを描画しているときだけ
        let Some(draft) = self.settings_dialog.draft_mut() else {
            warn!("ドラフトが無い状態で設定の初期化が要求された");
            return;
        };
        let defaults = ui::draft_from_defaults(draft);
        *draft = defaults;

        info!("編集中の設定を初期値へ戻した");
        self.settings_dialog.set_management_message(
            "初期値に戻しました。「適用」または「OK」で反映します".to_string(),
            false,
        );
    }

    /// 設定ファイルの読み書きの失敗を、トーストとダイアログの両方へ出す。
    ///
    /// トーストは画面下部に出るため、設定ダイアログの位置によっては隠れる。
    /// 操作したその場にも理由が残るようにする。
    fn report_settings_error(&mut self, reason: String) {
        self.settings_dialog
            .set_management_message(status::format_message(ErrorSource::Settings, &reason), true);
        self.report_error(ErrorSource::Settings, reason);
    }
}
