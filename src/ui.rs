use crate::settings::AppSettings;
use crate::video::DeviceCapabilities;
use eframe::egui;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

static TEST_SOUND_FLAG: AtomicBool = AtomicBool::new(false);

// デバイス能力のキャッシュ
static DEVICE_CAPABILITIES_CACHE: std::sync::OnceLock<Mutex<HashMap<String, DeviceCapabilities>>> =
    std::sync::OnceLock::new();

pub fn should_play_test_sound() -> bool {
    TEST_SOUND_FLAG.swap(false, Ordering::SeqCst)
}

/// 設定ダイアログで行われた操作。
///
/// 各ボタンの意味は `README.md` の「設定」と
/// `docs/ARCHITECTURE.md` の「適用の境界を明確にする」に合わせている。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsDialogAction {
    /// まだ何も押されていない（編集中）
    None,
    /// 適用: ドラフトを実行中の設定へ反映してファイルへ保存する。ダイアログは閉じない
    Apply,
    /// OK: 適用と同じことをしたうえで閉じる
    Ok,
    /// キャンセル: ドラフトを捨てて閉じる。タイトルバーの × も同じ扱い。
    /// 「適用」で既に反映したぶんは元に戻さない
    Cancel,
}

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
#[derive(Default)]
pub struct SettingsDialogState {
    draft: Option<AppSettings>,
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
    }

    /// 編集を終える。ドラフトは捨てる。
    pub fn end_edit(&mut self) {
        self.draft = None;
    }

    pub fn draft(&self) -> Option<&AppSettings> {
        self.draft.as_ref()
    }

    pub fn draft_mut(&mut self) -> Option<&mut AppSettings> {
        self.draft.as_mut()
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

/// ドラフトのうち、設定ダイアログが編集する範囲だけを実行中の設定へ反映する。
///
/// `ui` セクションを丸ごと上書きしないのは、ウィンドウのサイズ・位置、
/// 最前面表示、画面ドラッグ移動がダイアログの外で変わるため。丸ごと入れると、
/// ダイアログを開いている間に動かしたウィンドウの位置が、開いた時点の
/// スナップショットで巻き戻る。
///
/// **ダイアログに `ui` セクションの項目を足すときは、ここにも足すこと。**
pub fn commit_draft(target: &mut AppSettings, draft: &AppSettings) {
    target.video = draft.video.clone();
    target.audio = draft.audio.clone();
    target.screenshot = draft.screenshot.clone();
    // ダイアログの「ユーザーインターフェース」グループが編集する 2 項目だけ
    target.ui.maintain_aspect_ratio = draft.ui.maintain_aspect_ratio;
    target.ui.volume = draft.ui.volume;
}

/// 設定ダイアログを描画し、行われた操作を返す。
///
/// 編集対象は `dialog` が持つドラフトで、実行中の設定はここでは触らない。
/// ドラフトの反映・保存・クローズは呼び出し側が `transition_for` の結果に
/// 従って行う。
pub fn show_settings_dialog(
    ctx: &egui::Context,
    show_settings: &mut bool,
    dialog: &mut SettingsDialogState,
    show_hotkey_dialog: &mut bool,
    video_devices: &[(String, String)],
    input_devices: &[String],
    output_devices: &[String],
) -> SettingsDialogAction {
    use std::sync::OnceLock;
    static SELECTED_TAB: OnceLock<Mutex<i32>> = OnceLock::new();
    let selected_tab = SELECTED_TAB.get_or_init(|| Mutex::new(0));

    // ドラフトが用意できていなければ描画しない。呼び出し側が begin_edit を
    // 呼ぶまで待つ
    let Some(draft) = dialog.draft_mut() else {
        return SettingsDialogAction::None;
    };

    let mut button = SettingsDialogAction::None;

    egui::Window::new("設定")
        .open(show_settings)
        .default_size([650.0, 500.0])
        .resizable(true)
        .show(ctx, |ui| {
            // タブ選択
            ui.horizontal(|ui| {
                if let Ok(mut tab) = selected_tab.lock() {
                    ui.selectable_value(&mut *tab, 0, "デバイス設定");
                    ui.selectable_value(&mut *tab, 1, "スクリーンショット設定");
                }
            });

            ui.separator();

            egui::ScrollArea::vertical().show(ui, |ui| {
                if let Ok(tab) = selected_tab.lock() {
                    match *tab {
                        0 => show_device_settings_tab(
                            ui,
                            draft,
                            video_devices,
                            input_devices,
                            output_devices,
                        ),
                        1 => show_screenshot_settings_tab(ui, draft, show_hotkey_dialog),
                        _ => {}
                    }
                }
            });

            ui.separator();

            // OK、キャンセル、適用ボタン
            ui.horizontal(|ui| {
                if ui.button("OK").clicked() {
                    button = SettingsDialogAction::Ok;
                }

                if ui.button("キャンセル").clicked() {
                    button = SettingsDialogAction::Cancel;
                }

                if ui.button("適用").clicked() {
                    button = SettingsDialogAction::Apply;
                }
            });
        });

    resolve_action(button, *show_settings)
}

fn show_device_settings_tab(
    ui: &mut egui::Ui,
    settings: &mut AppSettings,
    video_devices: &[(String, String)],
    input_devices: &[String],
    output_devices: &[String],
) {
    ui.heading("デバイス設定");
    ui.add_space(10.0);

    // キャッシュの初期化
    let capabilities_cache = DEVICE_CAPABILITIES_CACHE.get_or_init(|| Mutex::new(HashMap::new()));

    // ビデオ設定
    ui.group(|ui| {
        ui.strong("ビデオ設定");
        ui.add_space(5.0);

        // ビデオデバイス選択
        // 一覧は main.rs 側でキャッシュ済みのものを受け取る（毎フレームの列挙を避けるため）
        let selected_device = settings.video.device_name.clone().unwrap_or_default();

        let mut device_changed = false;
        egui::ComboBox::from_label("ビデオデバイス")
            .selected_text(if selected_device.is_empty() {
                "デバイスを選択..."
            } else {
                &selected_device
            })
            .show_ui(ui, |ui| {
                for (name, description) in video_devices {
                    let display_text = if description.is_empty() {
                        name.clone()
                    } else {
                        format!("{} ({})", name, description)
                    };
                    if ui
                        .selectable_label(
                            settings.video.device_name.as_ref() == Some(name),
                            display_text,
                        )
                        .clicked()
                        && settings.video.device_name.as_ref() != Some(name)
                    {
                        settings.video.device_name = Some(name.clone());
                        device_changed = true;
                    }
                }
            });

        // デバイス変更時の処理
        if device_changed {
            // 新しく選択されたデバイス名を取得
            let new_device = settings.video.device_name.clone().unwrap_or_default();

            // デバイス能力を取得（キャッシュ確認）
            if let Ok(mut cache) = capabilities_cache.lock() {
                if !cache.contains_key(&new_device) && !new_device.is_empty() {
                    // キャッシュにない場合は取得
                    ui.spinner(); // 読み込み中表示
                    if let Ok(caps) =
                        crate::video::VideoCapture::get_device_capabilities(Some(&new_device))
                    {
                        cache.insert(new_device.clone(), caps);
                    }
                }
            }

            // デフォルトのフォーマットを設定
            if let Ok(cache) = capabilities_cache.lock() {
                if let Some(caps) = cache.get(&new_device) {
                    // 最初のフォーマットを選択
                    if let Some((format, _)) = caps.first() {
                        settings.video.format = Some(format.clone());
                    }
                }
            }
        }

        // フォーマット選択（フォーマットが起点）
        let mut format_changed = false;
        ui.horizontal(|ui| {
            ui.label("フォーマット:");
            let current_format = settings
                .video
                .format
                .clone()
                .unwrap_or_else(|| "YUY2".to_string());

            egui::ComboBox::from_id_source("format_combo")
                .selected_text(&current_format)
                .show_ui(ui, |ui| {
                    // キャッシュからフォーマット一覧を取得
                    if let Ok(cache) = capabilities_cache.lock() {
                        if let Some(caps) = cache.get(&selected_device) {
                            for (format, _) in caps {
                                if ui
                                    .selectable_value(
                                        &mut settings.video.format,
                                        Some(format.clone()),
                                        format,
                                    )
                                    .clicked()
                                {
                                    format_changed = true;
                                }
                            }
                        } else {
                            // キャッシュがない場合はデフォルト
                            ui.selectable_value(
                                &mut settings.video.format,
                                Some("YUY2".to_string()),
                                "YUY2",
                            );
                            ui.selectable_value(
                                &mut settings.video.format,
                                Some("MJPEG".to_string()),
                                "MJPEG",
                            );
                            ui.selectable_value(
                                &mut settings.video.format,
                                Some("RGB24".to_string()),
                                "RGB24",
                            );
                        }
                    }
                });
        });

        // フォーマット変更時に解像度をリセット
        if format_changed {
            if let Ok(cache) = capabilities_cache.lock() {
                if let Some(caps) = cache.get(&selected_device) {
                    if let Some(current_format) = &settings.video.format {
                        // 現在のフォーマットに対応する最初の解像度を選択
                        for (format, resolutions) in caps {
                            if format == current_format {
                                if let Some((w, h, fps)) = resolutions.first() {
                                    settings.video.resolution = Some((*w, *h));
                                    settings.video.fps = Some(*fps);
                                }
                                break;
                            }
                        }
                    }
                }
            }
        }

        // 解像度選択（フォーマットに応じて動的に変更）
        let mut resolution_changed = false;
        ui.horizontal(|ui| {
            ui.label("解像度:");
            let current_resolution = settings.video.resolution.unwrap_or((1280, 720));

            egui::ComboBox::from_id_source("resolution_combo")
                .selected_text(format!("{}x{}", current_resolution.0, current_resolution.1))
                .show_ui(ui, |ui| {
                    if let Ok(cache) = capabilities_cache.lock() {
                        if let Some(caps) = cache.get(&selected_device) {
                            if let Some(current_format) = &settings.video.format {
                                // 現在のフォーマットに対応する解像度一覧
                                let mut unique_resolutions =
                                    std::collections::HashSet::<(u32, u32)>::new();
                                for (format, resolutions) in caps {
                                    if format == current_format {
                                        for (w, h, _) in resolutions {
                                            unique_resolutions.insert((*w, *h));
                                        }
                                    }
                                }

                                // ソートして表示
                                let mut sorted_resolutions: Vec<_> =
                                    unique_resolutions.into_iter().collect();
                                sorted_resolutions.sort_by(|a, b| {
                                    let size_a = a.0 * a.1;
                                    let size_b = b.0 * b.1;
                                    size_b.cmp(&size_a)
                                });

                                for (w, h) in sorted_resolutions {
                                    if ui
                                        .selectable_value(
                                            &mut settings.video.resolution,
                                            Some((w, h)),
                                            format!("{}x{}", w, h),
                                        )
                                        .clicked()
                                    {
                                        resolution_changed = true;
                                    }
                                }
                            }
                        } else {
                            // デフォルトの解像度
                            ui.selectable_value(
                                &mut settings.video.resolution,
                                Some((1920, 1080)),
                                "1920x1080",
                            );
                            ui.selectable_value(
                                &mut settings.video.resolution,
                                Some((1280, 720)),
                                "1280x720",
                            );
                            ui.selectable_value(
                                &mut settings.video.resolution,
                                Some((640, 480)),
                                "640x480",
                            );
                        }
                    }
                });
        });

        // 解像度変更時にFPSをリセット
        if resolution_changed {
            if let Ok(cache) = capabilities_cache.lock() {
                if let Some(caps) = cache.get(&selected_device) {
                    if let Some(current_format) = &settings.video.format {
                        if let Some((w, h)) = settings.video.resolution {
                            // 現在のフォーマットと解像度に対応する最初のFPSを選択
                            for (format, resolutions) in caps {
                                if format == current_format {
                                    for (res_w, res_h, fps) in resolutions {
                                        if *res_w == w && *res_h == h {
                                            settings.video.fps = Some(*fps);
                                            break;
                                        }
                                    }
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }

        // FPS選択（フォーマットと解像度に応じて動的に変更）
        ui.horizontal(|ui| {
            ui.label("フレームレート:");
            let current_fps = settings.video.fps.unwrap_or(30);

            egui::ComboBox::from_id_source("fps_combo")
                .selected_text(format!("{} fps", current_fps))
                .show_ui(ui, |ui| {
                    if let Ok(cache) = capabilities_cache.lock() {
                        if let Some(caps) = cache.get(&selected_device) {
                            if let Some(current_format) = &settings.video.format {
                                if let Some((w, h)) = settings.video.resolution {
                                    // 現在のフォーマットと解像度に対応するFPS一覧
                                    let mut available_fps = Vec::new();
                                    for (format, resolutions) in caps {
                                        if format == current_format {
                                            for (res_w, res_h, fps) in resolutions {
                                                if *res_w == w && *res_h == h {
                                                    available_fps.push(*fps);
                                                }
                                            }
                                        }
                                    }

                                    // 重複を削除してソート
                                    available_fps.sort();
                                    available_fps.dedup();
                                    available_fps.reverse(); // 大きい順

                                    for fps in available_fps {
                                        ui.selectable_value(
                                            &mut settings.video.fps,
                                            Some(fps),
                                            format!("{} fps", fps),
                                        );
                                    }
                                }
                            }
                        } else {
                            // デフォルトのFPS
                            ui.selectable_value(&mut settings.video.fps, Some(30), "30 fps");
                            ui.selectable_value(&mut settings.video.fps, Some(60), "60 fps");
                        }
                    }
                });
        });
    });

    ui.add_space(15.0);

    // オーディオ設定
    ui.group(|ui| {
        ui.strong("オーディオ設定");
        ui.add_space(5.0);

        // オーディオ入力デバイス選択 - キャッシュリストを使用
        let current_input_device = settings.audio.input_device_name.clone().unwrap_or_default();

        egui::ComboBox::from_label("オーディオ入力デバイス")
            .selected_text(if current_input_device.is_empty() {
                "デバイスを選択..."
            } else {
                &current_input_device
            })
            .show_ui(ui, |ui| {
                for device_name in input_devices {
                    ui.selectable_value(
                        &mut settings.audio.input_device_name,
                        Some(device_name.clone()),
                        device_name,
                    );
                }
            });

        // オーディオ出力デバイス選択 - キャッシュリストを使用
        let current_output_device = settings
            .audio
            .output_device_name
            .clone()
            .unwrap_or_default();

        egui::ComboBox::from_label("オーディオ出力デバイス")
            .selected_text(if current_output_device.is_empty() {
                "デフォルト"
            } else {
                &current_output_device
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut settings.audio.output_device_name, None, "デフォルト");
                for device_name in output_devices {
                    ui.selectable_value(
                        &mut settings.audio.output_device_name,
                        Some(device_name.clone()),
                        device_name,
                    );
                }
            });

        // サンプルレート
        ui.horizontal(|ui| {
            ui.label("サンプリングレート:");
            let sample_rates = vec![8000, 16000, 22050, 32000, 44100, 48000, 96000];
            let current_rate = settings.audio.sample_rate.unwrap_or(44100);
            egui::ComboBox::from_id_source("sample_rate_combo")
                .selected_text(format!("{} Hz", current_rate))
                .show_ui(ui, |ui| {
                    for rate in sample_rates {
                        ui.selectable_value(
                            &mut settings.audio.sample_rate,
                            Some(rate),
                            format!("{} Hz", rate),
                        );
                    }
                });
        });

        // チャンネル数
        ui.horizontal(|ui| {
            ui.label("チャンネル数:");
            let current_channels = settings.audio.channels.unwrap_or(2);
            egui::ComboBox::from_id_source("channels_combo")
                .selected_text(format!("{}", current_channels))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut settings.audio.channels, Some(1), "1 (Mono)");
                    ui.selectable_value(&mut settings.audio.channels, Some(2), "2 (Stereo)");
                });
        });

        ui.add_space(10.0);

        // オーディオパススルー制御
        ui.horizontal(|ui| {
            ui.label("音声パススルー:");
            if ui
                .checkbox(&mut settings.audio.passthrough_enabled, "有効")
                .changed()
            {
                println!(
                    "Audio passthrough changed to: {}",
                    settings.audio.passthrough_enabled
                );
            }
        });

        if !settings.audio.passthrough_enabled {
            ui.colored_label(
                egui::Color32::YELLOW,
                "⚠ 音声パススルーが無効です（ノイズ軽減のため）",
            );
        }
    });

    ui.add_space(15.0);

    // UI設定
    ui.group(|ui| {
        ui.strong("ユーザーインターフェース");
        ui.add_space(5.0);

        ui.checkbox(&mut settings.ui.maintain_aspect_ratio, "アスペクト比を維持");

        ui.horizontal(|ui| {
            ui.label("初期音量:");
            ui.add(egui::Slider::new(&mut settings.ui.volume, 0.0..=200.0).suffix("%"));
        });
    });
}

fn show_screenshot_settings_tab(
    ui: &mut egui::Ui,
    settings: &mut AppSettings,
    show_hotkey_dialog: &mut bool,
) {
    ui.heading("スクリーンショット設定");
    ui.add_space(10.0);

    // 保存フォルダー
    ui.group(|ui| {
        ui.strong("保存場所");
        ui.add_space(5.0);

        ui.horizontal(|ui| {
            ui.label("保存フォルダ:");
            let mut folder_str = settings
                .screenshot
                .save_folder
                .to_string_lossy()
                .to_string();
            ui.text_edit_singleline(&mut folder_str);
            settings.screenshot.save_folder = std::path::PathBuf::from(folder_str);

            if ui.button("参照...").clicked() {
                if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                    settings.screenshot.save_folder = folder;
                }
            }
        });
    });

    ui.add_space(15.0);

    // サウンド設定
    ui.group(|ui| {
        ui.strong("効果音");
        ui.add_space(5.0);

        ui.horizontal(|ui| {
            ui.label("サウンドファイル:");
            let sound_file_str = settings
                .screenshot
                .sound_file
                .as_ref()
                .map(|p| {
                    p.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string()
                })
                .unwrap_or_else(|| "未選択".to_string());
            ui.label(&sound_file_str);

            if ui.button("ファイル選択...").clicked() {
                if let Some(file) = rfd::FileDialog::new()
                    .add_filter("音声ファイル", &["mp3", "wav", "ogg"])
                    .pick_file()
                {
                    settings.screenshot.sound_file = Some(file);
                }
            }
        });

        if settings.screenshot.sound_file.is_some() {
            ui.horizontal(|ui| {
                ui.label("音量:");
                ui.add(
                    egui::Slider::new(&mut settings.screenshot.sound_volume, 0.0..=200.0)
                        .suffix("%"),
                );
            });

            ui.horizontal(|ui| {
                if ui.button("テスト再生").clicked() {
                    TEST_SOUND_FLAG.store(true, Ordering::SeqCst);
                }
                if ui.button("クリア").clicked() {
                    settings.screenshot.sound_file = None;
                }
            });
        }
    });

    ui.add_space(15.0);

    // ホットキー設定
    ui.group(|ui| {
        ui.strong("ホットキー設定");
        ui.add_space(5.0);

        ui.horizontal(|ui| {
            ui.label("スクリーンショットホットキー:");
            let hotkey_str = settings
                .screenshot
                .hotkey
                .clone()
                .unwrap_or_else(|| "未設定".to_string());

            ui.label(&hotkey_str);

            if ui.button("ホットキー設定...").clicked() {
                *show_hotkey_dialog = true;
            }
        });

        if settings.screenshot.hotkey.is_some() {
            ui.horizontal(|ui| {
                if ui.button("ホットキー解除").clicked() {
                    settings.screenshot.hotkey = None;
                }
            });
        }

        ui.add_space(5.0);
        ui.small("『ホットキー設定...』を押して希望のキーコンビネーションを入力してください。");
    });
}

/// egui のキーを、ホットキー文字列で使う名前に変換する。
/// ホットキーとして扱わないキーは `None` を返す。
fn hotkey_key_name(key: egui::Key) -> Option<&'static str> {
    let name = match key {
        egui::Key::A => "A",
        egui::Key::B => "B",
        egui::Key::C => "C",
        egui::Key::D => "D",
        egui::Key::E => "E",
        egui::Key::F => "F",
        egui::Key::G => "G",
        egui::Key::H => "H",
        egui::Key::I => "I",
        egui::Key::J => "J",
        egui::Key::K => "K",
        egui::Key::L => "L",
        egui::Key::M => "M",
        egui::Key::N => "N",
        egui::Key::O => "O",
        egui::Key::P => "P",
        egui::Key::Q => "Q",
        egui::Key::R => "R",
        egui::Key::S => "S",
        egui::Key::T => "T",
        egui::Key::U => "U",
        egui::Key::V => "V",
        egui::Key::W => "W",
        egui::Key::X => "X",
        egui::Key::Y => "Y",
        egui::Key::Z => "Z",
        egui::Key::F1 => "F1",
        egui::Key::F2 => "F2",
        egui::Key::F3 => "F3",
        egui::Key::F4 => "F4",
        egui::Key::F5 => "F5",
        egui::Key::F6 => "F6",
        egui::Key::F7 => "F7",
        egui::Key::F8 => "F8",
        egui::Key::F9 => "F9",
        egui::Key::F10 => "F10",
        egui::Key::F11 => "F11",
        egui::Key::F12 => "F12",
        egui::Key::Num0 => "0",
        egui::Key::Num1 => "1",
        egui::Key::Num2 => "2",
        egui::Key::Num3 => "3",
        egui::Key::Num4 => "4",
        egui::Key::Num5 => "5",
        egui::Key::Num6 => "6",
        egui::Key::Num7 => "7",
        egui::Key::Num8 => "8",
        egui::Key::Num9 => "9",
        egui::Key::Space => "Space",
        egui::Key::Enter => "Enter",
        _ => return None,
    };
    Some(name)
}

/// 押されている修飾キーと通常キーから、`screenshot::parse_hotkey` が解釈できる
/// ホットキー文字列を組み立てる。
///
/// 通常キーが 1 つも押されていない（修飾キーだけの）場合は `None` を返す。
fn build_hotkey_string(modifiers: &egui::Modifiers, keys_down: &[egui::Key]) -> Option<String> {
    // 通常キーが 1 つも無いうちは確定させない。修飾キーだけの文字列を確定させると
    // screenshot::parse_hotkey が "No key code specified" で弾き、登録に失敗する。
    // 押されているキーのうち対応している最初の 1 つだけを使う（ホットキーに含められる
    // 通常キーは 1 つだけのため）。
    let key_name = keys_down.iter().copied().find_map(hotkey_key_name)?;

    let mut parts = Vec::new();

    if modifiers.ctrl {
        parts.push("Ctrl");
    }
    if modifiers.shift {
        parts.push("Shift");
    }
    if modifiers.alt {
        parts.push("Alt");
    }
    parts.push(key_name);

    Some(parts.join("+"))
}

#[allow(static_mut_refs)]
pub fn show_hotkey_capture_dialog(
    ctx: &egui::Context,
    show_dialog: &mut bool,
    captured_hotkey: &mut String,
) -> bool {
    static mut CAPTURING: bool = false;
    static mut TEMP_HOTKEY: String = String::new();

    let mut close_dialog = false;

    egui::Window::new("ホットキー設定")
        .open(show_dialog)
        .fixed_size([350.0, 200.0])
        .collapsible(false)
        .show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.heading("ホットキー設定");
                ui.add_space(10.0);

                if unsafe { !CAPTURING } {
                    ui.label(
                        "『キャプチャ開始』を押してスクリーンショット用のキーを入力してください",
                    );

                    ui.add_space(10.0);

                    ui.horizontal(|ui| {
                        ui.label("現在のホットキー:");
                        let hotkey_text = if captured_hotkey.is_empty() {
                            "未設定"
                        } else {
                            captured_hotkey.as_str()
                        };
                        ui.monospace(hotkey_text);
                    });

                    ui.add_space(15.0);

                    if ui.button("キャプチャ開始").clicked() {
                        unsafe {
                            CAPTURING = true;
                            TEMP_HOTKEY.clear();
                        }
                    }
                } else {
                    ui.colored_label(egui::Color32::YELLOW, "キー入力待機中...");
                    ui.label("任意のキーコンビネーションを押してください");

                    // キーボード入力をキャプチャ
                    ctx.input(|i| {
                        // HashSet の反復順は不定なので、同じ組み合わせから常に同じ
                        // ホットキー文字列が得られるよう並べてから渡す
                        let mut keys_down: Vec<egui::Key> = i.keys_down.iter().copied().collect();
                        keys_down.sort();

                        if let Some(hotkey) = build_hotkey_string(&i.modifiers, &keys_down) {
                            unsafe {
                                TEMP_HOTKEY = hotkey;
                                CAPTURING = false;
                            }
                        }
                    });

                    unsafe {
                        if !TEMP_HOTKEY.is_empty() {
                            ui.add_space(10.0);
                            ui.horizontal(|ui| {
                                ui.label("取得:");
                                ui.monospace(&TEMP_HOTKEY);
                            });
                        }
                    }

                    ui.add_space(10.0);

                    if ui.button("停止").clicked() {
                        unsafe {
                            CAPTURING = false;
                        }
                    }
                }

                ui.add_space(20.0);

                ui.horizontal(|ui| {
                    if ui.button("OK").clicked() {
                        unsafe {
                            if !TEMP_HOTKEY.is_empty() {
                                *captured_hotkey = TEMP_HOTKEY.clone();
                                TEMP_HOTKEY.clear();
                            }
                            CAPTURING = false;
                        }
                        close_dialog = true;
                    }

                    if ui.button("キャンセル").clicked() {
                        unsafe {
                            CAPTURING = false;
                            TEMP_HOTKEY.clear();
                        }
                        close_dialog = true;
                    }

                    if ui.button("クリア").clicked() {
                        captured_hotkey.clear();
                        unsafe {
                            CAPTURING = false;
                            TEMP_HOTKEY.clear();
                        }
                        close_dialog = true;
                    }
                });
            });
        });

    let hotkey_captured = !captured_hotkey.is_empty() && close_dialog;

    if close_dialog {
        *show_dialog = false;
    }

    hotkey_captured
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{AudioSettings, ScreenshotSettings, UiSettings, VideoSettings};
    use std::path::PathBuf;

    /// 既定値と全項目が異なる設定。どの項目が反映され、どの項目が
    /// 据え置かれるかを区別できるようにするためのもの。
    fn sample_settings() -> AppSettings {
        AppSettings {
            video: VideoSettings {
                device_name: Some("Capture Device".to_string()),
                resolution: Some((1920, 1080)),
                format: Some("MJPEG".to_string()),
                fps: Some(30),
            },
            audio: AudioSettings {
                input_device_name: Some("Line In".to_string()),
                output_device_name: Some("Speakers".to_string()),
                sample_rate: Some(44100),
                channels: Some(1),
                passthrough_enabled: false,
            },
            screenshot: ScreenshotSettings {
                save_folder: PathBuf::from("C:/shots"),
                sound_file: Some(PathBuf::from("sound/custom.mp3")),
                sound_volume: 50.0,
                hotkey: Some("Ctrl+S".to_string()),
            },
            ui: UiSettings {
                volume: 80.0,
                maintain_aspect_ratio: false,
                last_window_size: Some((800.0, 600.0)),
                last_window_pos: Some((10.0, 20.0)),
                always_on_top: true,
                enable_drag_move: false,
            },
        }
    }

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
    fn commit_draft_replaces_device_and_screenshot_sections() {
        let mut shared = AppSettings::default();
        let draft = sample_settings();

        commit_draft(&mut shared, &draft);

        assert_eq!(shared.video.format, Some("MJPEG".to_string()));
        assert_eq!(shared.video.resolution, Some((1920, 1080)));
        assert_eq!(shared.video.fps, Some(30));
        assert_eq!(shared.audio.sample_rate, Some(44100));
        assert_eq!(shared.audio.channels, Some(1));
        assert!(!shared.audio.passthrough_enabled);
        assert_eq!(shared.screenshot.hotkey, Some("Ctrl+S".to_string()));
        assert_eq!(shared.screenshot.sound_volume, 50.0);
    }

    #[test]
    fn commit_draft_applies_ui_items_the_dialog_edits() {
        // 「ユーザーインターフェース」グループの 2 項目は反映する
        let mut shared = AppSettings::default();
        let draft = sample_settings();

        commit_draft(&mut shared, &draft);

        assert_eq!(shared.ui.volume, 80.0);
        assert!(!shared.ui.maintain_aspect_ratio);
    }

    #[test]
    fn commit_draft_keeps_window_state_changed_while_dialog_is_open() {
        // ダイアログを開いている間にウィンドウを動かす・最前面表示を切り替える
        // といった操作をしても、OK でその変更が巻き戻ってはいけない。
        // ドラフトは開いた時点のスナップショットなので、これらを丸ごと
        // 書き戻すと位置が飛ぶ
        let mut shared = sample_settings();
        let draft = shared.clone();

        shared.ui.last_window_size = Some((1280.0, 720.0));
        shared.ui.last_window_pos = Some((100.0, 200.0));
        shared.ui.always_on_top = false;
        shared.ui.enable_drag_move = true;

        commit_draft(&mut shared, &draft);

        assert_eq!(shared.ui.last_window_size, Some((1280.0, 720.0)));
        assert_eq!(shared.ui.last_window_pos, Some((100.0, 200.0)));
        assert!(!shared.ui.always_on_top);
        assert!(shared.ui.enable_drag_move);
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
        commit_draft(&mut shared, state.draft().expect("ドラフトがある"));

        let cancelled = SettingsDialogState::transition_for(SettingsDialogAction::Cancel);
        assert!(!cancelled.commit_draft);
        assert!(cancelled.close);
        state.end_edit();

        assert_eq!(shared.video.fps, Some(24));
        assert!(!state.has_draft());
    }

    fn modifiers(ctrl: bool, shift: bool, alt: bool) -> egui::Modifiers {
        egui::Modifiers {
            alt,
            ctrl,
            shift,
            mac_cmd: false,
            // Windows では command は ctrl と同じ値にする決まりになっている
            command: ctrl,
        }
    }

    #[test]
    fn build_hotkey_string_no_input_returns_none() {
        assert_eq!(
            build_hotkey_string(&modifiers(false, false, false), &[]),
            None
        );
    }

    #[test]
    fn build_hotkey_string_one_modifier_only_returns_none() {
        assert_eq!(
            build_hotkey_string(&modifiers(true, false, false), &[]),
            None
        );
        assert_eq!(
            build_hotkey_string(&modifiers(false, true, false), &[]),
            None
        );
        assert_eq!(
            build_hotkey_string(&modifiers(false, false, true), &[]),
            None
        );
    }

    #[test]
    fn build_hotkey_string_two_modifiers_only_returns_none() {
        // 修飾キーが 2 つ押されただけで確定してしまう不具合の再現
        assert_eq!(
            build_hotkey_string(&modifiers(true, true, false), &[]),
            None
        );
        assert_eq!(
            build_hotkey_string(&modifiers(true, false, true), &[]),
            None
        );
        assert_eq!(
            build_hotkey_string(&modifiers(false, true, true), &[]),
            None
        );
    }

    #[test]
    fn build_hotkey_string_three_modifiers_only_returns_none() {
        assert_eq!(build_hotkey_string(&modifiers(true, true, true), &[]), None);
    }

    #[test]
    fn build_hotkey_string_unsupported_key_only_returns_none() {
        // 対応していないキーは通常キーとして数えない
        assert_eq!(
            build_hotkey_string(&modifiers(true, true, false), &[egui::Key::Tab]),
            None
        );
    }

    #[test]
    fn build_hotkey_string_single_key_returns_key_only() {
        assert_eq!(
            build_hotkey_string(&modifiers(false, false, false), &[egui::Key::F5]),
            Some("F5".to_string())
        );
        assert_eq!(
            build_hotkey_string(&modifiers(false, false, false), &[egui::Key::A]),
            Some("A".to_string())
        );
    }

    #[test]
    fn build_hotkey_string_one_modifier_with_key_returns_combination() {
        assert_eq!(
            build_hotkey_string(&modifiers(true, false, false), &[egui::Key::S]),
            Some("Ctrl+S".to_string())
        );
    }

    #[test]
    fn build_hotkey_string_three_modifiers_with_key_keeps_fixed_order() {
        assert_eq!(
            build_hotkey_string(&modifiers(true, true, true), &[egui::Key::A]),
            Some("Ctrl+Shift+Alt+A".to_string())
        );
    }

    #[test]
    fn build_hotkey_string_digit_keys_are_supported() {
        assert_eq!(
            build_hotkey_string(&modifiers(false, false, false), &[egui::Key::Num0]),
            Some("0".to_string())
        );
        assert_eq!(
            build_hotkey_string(&modifiers(true, true, false), &[egui::Key::Num9]),
            Some("Ctrl+Shift+9".to_string())
        );
    }

    #[test]
    fn build_hotkey_string_ignores_unsupported_keys_when_key_is_present() {
        assert_eq!(
            build_hotkey_string(
                &modifiers(true, false, false),
                &[egui::Key::Tab, egui::Key::S]
            ),
            Some("Ctrl+S".to_string())
        );
    }
}
