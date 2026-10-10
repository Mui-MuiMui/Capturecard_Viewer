//! 名前付きのプリセット（[[presets]]）。適用と一致の判定、名前の検証、
//! 読み込んだ一覧の整え方（`docs/design/presets.md`）。

use super::{AppSettings, AudioSettings, VideoSettings};
use crate::i18n::Text;
use log::warn;
use serde::{Deserialize, Serialize};

// 設定ファイルから読んだプリセットの一覧を、扱える形に整える。
//
// 名前の前後の空白を落とし、名前が空のものと、既出の名前と重なるものを捨てる。
// **ここで弾かないと、設定ダイアログの `validate_preset_name` を通さずに
// 不正な状態を作れる。** 空の名前はメニューに空の項目として並び、重複した
// 名前は `preset()` も `remove_preset` も先頭しか見ないため、2 つ目以降を
// 選ぶことも消すこともできなくなる。
//
// 捨てるだけでエラーにはしない。他の項目と同じで、読めないところだけを
// 落として残りは活かす。
pub(super) fn sanitize_presets(presets: Vec<Preset>) -> Vec<Preset> {
    let mut kept: Vec<Preset> = Vec::with_capacity(presets.len());
    for preset in presets {
        let name = preset.name.trim();
        if name.is_empty() {
            warn!("設定のプリセットに名前が無いので無視する");
            continue;
        }
        if kept.iter().any(|existing| existing.name == name) {
            warn!(
                "設定に同じ名前のプリセット \"{}\" が複数あるので後のほうを無視する",
                name
            );
            continue;
        }
        kept.push(Preset {
            name: name.to_string(),
            ..preset
        });
    }
    kept
}

// 名前付きプリセット。
//
// **中身は `video` と `audio` だけ。** `screenshot` / `hotkeys` / `ui` は
// 入れていない。プリセットの用途は「複数のキャプチャボードの使い分け」と
// 「低遅延優先 / 画質優先の切替」で、どちらも映像と音声の取り込み方の話。
// 切り替えたらスクリーンショットの保存先やホットキーまで変わるほうが
// 驚きが大きく、「プリセットを切り替えたらホットキーが効かなくなった」
// という迷い方をさせる。
//
// `video.auto_reconnect` と `video.convert`（#456）は `video` の中にありながら対象外。理由は
// `apply_to` のコメントを見ること。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preset {
    // **TOML のテーブルより前に置くこと。** 理由は AppSettings の
    // active_preset と同じ
    pub name: String,
    pub video: VideoSettings,
    pub audio: AudioSettings,
}

impl Preset {
    // いまの設定からプリセットを作る。
    pub fn from_settings(name: String, settings: &AppSettings) -> Self {
        let mut video = settings.video.clone();
        // 対象外の項目は既定値で固定する。現在値を書き込むと、設定ファイルを
        // 読んだ人に「プリセットで自動再接続が切り替わる」と読めてしまう
        video.auto_reconnect = VideoSettings::default().auto_reconnect;
        // 変換の場所（#456）も同じ扱い。PC の GPU で決まる項目で、画は変わらない
        video.convert = VideoSettings::default().convert;
        Self {
            name,
            video,
            audio: settings.audio.clone(),
        }
    }

    // プリセットの内容を設定へ写す。**`video` と `audio` 以外は触らない。**
    //
    // `video.auto_reconnect` も写さない。右クリックメニューだけで切り替える
    // 項目で、設定ダイアログにもプリセットの管理 UI にも出てこない。
    // 含めると次の 2 つが起きる。
    //   - プリセットを読み込んだだけで自動再接続が勝手に切り替わる
    //   - 自動再接続を切り替えただけでプリセットが「（変更あり）」になる
    // どちらもプリセットの目的と関係がない。
    //
    // `video.convert`（YUY2 を GPU と CPU のどちらで変換するか、#456）も写さない。
    // 画は同じで、使える GPU は PC で決まる。キャプチャーボードの使い分けとも
    // 低遅延 / 画質の切替とも関係がなく、含めると読み込んだだけで変換の場所が変わる。
    pub fn apply_to(&self, settings: &mut AppSettings) {
        let auto_reconnect = settings.video.auto_reconnect;
        let convert = settings.video.convert;
        settings.video = self.video.clone();
        settings.video.auto_reconnect = auto_reconnect;
        settings.video.convert = convert;
        settings.audio = self.audio.clone();
    }
}

// プリセット名として受け付けられない理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresetNameError {
    // 空、または空白だけ
    Empty,
    // 同じ名前のプリセットが既にある
    Duplicate,
}

impl PresetNameError {
    // 設定ダイアログに出す文言。
    pub fn message(self) -> &'static str {
        match self {
            PresetNameError::Empty => Text::PresetNameEmpty.get(),
            PresetNameError::Duplicate => Text::PresetNameDuplicate.get(),
        }
    }
}

// プリセット名として使えるかを調べる。使えるなら前後の空白を落とした名前を返す。
//
// **名前の重複を許さない。** 切替は名前で行うので、同じ名前が 2 つあると
// どちらが選ばれるかがリストの並び順に依存する。
//
// `replacing` には上書き対象の名前を渡す。同じ名前のまま上書き保存するときに、
// 自分自身との重複で弾かないため。新規保存では `None` を渡す。
pub fn validate_preset_name(
    name: &str,
    presets: &[Preset],
    replacing: Option<&str>,
) -> Result<String, PresetNameError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(PresetNameError::Empty);
    }

    let taken = presets
        .iter()
        .any(|preset| preset.name == trimmed && Some(preset.name.as_str()) != replacing);
    if taken {
        return Err(PresetNameError::Duplicate);
    }

    Ok(trimmed.to_string())
}

// 設定の `video` / `audio` がプリセットと一致するか。
//
// 一致の判定と適用（`Preset::apply_to`）は同じ項目を見ていなければならない。
// 適用しない `auto_reconnect` をここで比べると、読み込んだ直後から
// 「（変更あり）」になる。
pub fn matches_preset(preset: &Preset, settings: &AppSettings) -> bool {
    let mut video = settings.video.clone();
    video.auto_reconnect = preset.video.auto_reconnect;
    video.convert = preset.video.convert;
    video == preset.video && settings.audio == preset.audio
}

// いま実際に効いているプリセットの名前。
//
// `active_preset` に名前が入っていても、そのプリセットが消えていたり、
// 読み込んだあとで手作業で値を変えていれば `None` を返す。**この関数が
// 「（変更あり）」表示の判定そのもの。**
pub fn resolved_active_preset(settings: &AppSettings) -> Option<&str> {
    let name = settings.active_preset.as_deref()?;
    let preset = settings.presets.iter().find(|preset| preset.name == name)?;
    if matches_preset(preset, settings) {
        Some(preset.name.as_str())
    } else {
        None
    }
}

impl AppSettings {
    // 名前でプリセットを引く。
    pub fn preset(&self, name: &str) -> Option<&Preset> {
        self.presets.iter().find(|preset| preset.name == name)
    }

    // プリセットを適用する。**`video` と `audio` だけが変わる。**
    // 見つからなければ何もせず `false` を返す。
    pub fn apply_preset(&mut self, name: &str) -> bool {
        let Some(preset) = self.preset(name).cloned() else {
            return false;
        };
        preset.apply_to(self);
        self.active_preset = Some(preset.name);
        true
    }

    // プリセットを追加、または同じ名前のものを上書きする。
    //
    // 上書きしたものが選択中なら、そのまま選択中のままにする。中身は
    // いまの設定から作ったものなので、一致したままになる。
    pub fn upsert_preset(&mut self, preset: Preset) {
        match self
            .presets
            .iter_mut()
            .find(|existing| existing.name == preset.name)
        {
            Some(existing) => *existing = preset,
            None => self.presets.push(preset),
        }
    }

    // プリセットを削除する。消せたら `true`。
    //
    // 選択中のものを消したときは選択も外す。名前だけが残っても、引ける
    // プリセットが無いので「なし」と同じ意味になる。
    pub fn remove_preset(&mut self, name: &str) -> bool {
        let Some(index) = self.presets.iter().position(|preset| preset.name == name) else {
            return false;
        };
        self.presets.remove(index);
        if self.active_preset.as_deref() == Some(name) {
            self.active_preset = None;
        }
        true
    }

    // 選択中のプリセットが実際の値と食い違っていれば選択を外す。
    //
    // **`video` / `audio` を書き換えたあとに呼ぶこと。** 呼ばないと、
    // プリセットを読み込んだあと設定ダイアログで解像度を変えても
    // 「プリセット: 低遅延優先」のままになる。
    pub fn refresh_active_preset(&mut self) {
        if resolved_active_preset(self).is_none() {
            self.active_preset = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkey::HotkeyAction;
    use crate::settings::testing::FULL_CONFIG;
    use crate::settings::{export_to, import_from, VideoBackendSetting, VideoConvertSetting};
    use tempfile::tempdir;

    #[test]
    fn video_backend_is_part_of_the_preset() {
        // 開き方はデバイスと一体なので、プリセットで適用も比較もする
        let mut settings = AppSettings::default();
        settings.video.backend = VideoBackendSetting::DirectShow;
        let preset = Preset::from_settings("DS".to_string(), &settings);
        assert_eq!(preset.video.backend, VideoBackendSetting::DirectShow);

        let mut target = AppSettings::default();
        assert!(!matches_preset(&preset, &target));
        preset.apply_to(&mut target);
        assert_eq!(target.video.backend, VideoBackendSetting::DirectShow);
        assert!(matches_preset(&preset, &target));
    }

    #[test]
    fn video_convert_is_not_part_of_the_preset() {
        // 変換の場所（#456）は PC で決まる。プリセットで変わらず、比べもしない
        let mut settings = AppSettings::default();
        settings.video.convert = VideoConvertSetting::Cpu;
        let preset = Preset::from_settings("CPU".to_string(), &settings);
        assert_eq!(preset.video.convert, VideoConvertSetting::Auto);
        assert!(
            matches_preset(&preset, &settings),
            "変換の場所だけ違っても一致"
        );

        let mut target = AppSettings::default();
        target.video.convert = VideoConvertSetting::Gpu;
        preset.apply_to(&mut target);
        assert_eq!(target.video.convert, VideoConvertSetting::Gpu);
        assert!(matches_preset(&preset, &target));
    }

    // プリセットのテストで使う設定ファイル。
    //
    // 選択中のプリセットと [video] / [audio] の中身が一致している状態。
    // 省略した項目はどちらも既定値になるので、実際に一致する。
    const PRESET_CONFIG: &str = r#"
active_preset = "低遅延優先"

[video]
device_name = "Capture Device"
resolution = [1280, 720]
fps = 60

[audio]
input_device_name = "Line In"

[[presets]]
name = "低遅延優先"

[presets.video]
device_name = "Capture Device"
resolution = [1280, 720]
fps = 60

[presets.audio]
input_device_name = "Line In"

[[presets]]
name = "画質優先"

[presets.video]
device_name = "Capture Device"
resolution = [1920, 1080]
fps = 30

[presets.audio]
input_device_name = "Line In"
"#;

    // 解像度と fps だけが違うプリセットを作る。
    fn preset_named(name: &str, width: u32, height: u32, fps: u32) -> Preset {
        let mut settings = AppSettings::default();
        settings.video.resolution = Some((width, height));
        settings.video.fps = Some(fps);
        Preset::from_settings(name.to_string(), &settings)
    }

    #[test]
    fn validate_preset_name_empty_returns_error() {
        assert_eq!(
            validate_preset_name("", &[], None),
            Err(PresetNameError::Empty)
        );
    }

    #[test]
    fn validate_preset_name_whitespace_only_returns_error() {
        // 空白だけの名前はメニューに出しても読めない
        assert_eq!(
            validate_preset_name("   ", &[], None),
            Err(PresetNameError::Empty)
        );
    }

    #[test]
    fn validate_preset_name_trims_surrounding_whitespace() {
        assert_eq!(
            validate_preset_name("  低遅延優先  ", &[], None),
            Ok("低遅延優先".to_string())
        );
    }

    #[test]
    fn validate_preset_name_duplicate_returns_error() {
        let presets = vec![preset_named("低遅延優先", 1280, 720, 60)];

        assert_eq!(
            validate_preset_name("低遅延優先", &presets, None),
            Err(PresetNameError::Duplicate)
        );
    }

    #[test]
    fn validate_preset_name_trimmed_duplicate_returns_error() {
        // 前後の空白を落としたあとで比べる。落とさないと
        // 「低遅延優先 」と「低遅延優先」が並んで見分けられなくなる
        let presets = vec![preset_named("低遅延優先", 1280, 720, 60)];

        assert_eq!(
            validate_preset_name(" 低遅延優先 ", &presets, None),
            Err(PresetNameError::Duplicate)
        );
    }

    #[test]
    fn validate_preset_name_duplicate_is_allowed_when_replacing_itself() {
        // 同じ名前のまま上書き保存する場合
        let presets = vec![preset_named("低遅延優先", 1280, 720, 60)];

        assert_eq!(
            validate_preset_name("低遅延優先", &presets, Some("低遅延優先")),
            Ok("低遅延優先".to_string())
        );
    }

    #[test]
    fn validate_preset_name_duplicate_of_another_preset_is_rejected_when_replacing() {
        // 上書き対象以外との衝突は弾く
        let presets = vec![
            preset_named("低遅延優先", 1280, 720, 60),
            preset_named("画質優先", 1920, 1080, 30),
        ];

        assert_eq!(
            validate_preset_name("画質優先", &presets, Some("低遅延優先")),
            Err(PresetNameError::Duplicate)
        );
    }

    #[test]
    fn preset_from_settings_normalizes_auto_reconnect() {
        // 自動再接続はプリセットの対象外。作った時点の値を拾わず、
        // 既定値で固定されること
        let mut settings = AppSettings::default();
        settings.video.auto_reconnect = false;

        let preset = Preset::from_settings("低遅延優先".to_string(), &settings);

        assert!(preset.video.auto_reconnect);
    }

    #[test]
    fn apply_preset_changes_only_video_and_audio() {
        let mut settings = AppSettings::default();
        settings.screenshot.jpeg_quality = 55;
        settings.ui.volume = 33.0;
        settings.set_hotkey(HotkeyAction::Screenshot, Some("Ctrl+S".to_string()));
        settings
            .presets
            .push(preset_named("画質優先", 1920, 1080, 30));

        assert!(settings.apply_preset("画質優先"));

        assert_eq!(settings.video.resolution, Some((1920, 1080)));
        assert_eq!(settings.video.fps, Some(30));
        assert_eq!(settings.active_preset.as_deref(), Some("画質優先"));
        // プリセットが持たないセクションは 1 つも動かない
        assert_eq!(settings.screenshot.jpeg_quality, 55);
        assert_eq!(settings.ui.volume, 33.0);
        assert_eq!(settings.hotkey(HotkeyAction::Screenshot), Some("Ctrl+S"));
    }

    #[test]
    fn apply_preset_keeps_auto_reconnect() {
        // 右クリックメニューで切った自動再接続が、プリセットの
        // 切替で勝手に戻らないこと
        let mut settings = AppSettings::default();
        settings.video.auto_reconnect = false;
        settings
            .presets
            .push(preset_named("画質優先", 1920, 1080, 30));

        assert!(settings.apply_preset("画質優先"));

        assert!(!settings.video.auto_reconnect);
    }

    #[test]
    fn apply_preset_unknown_name_changes_nothing() {
        let mut settings = AppSettings::default();
        settings
            .presets
            .push(preset_named("画質優先", 1920, 1080, 30));

        assert!(!settings.apply_preset("無い名前"));

        assert_eq!(settings.video.resolution, Some((1280, 720)));
        assert_eq!(settings.active_preset, None);
    }

    #[test]
    fn matches_preset_after_apply_returns_true() {
        let mut settings = AppSettings::default();
        let preset = preset_named("画質優先", 1920, 1080, 30);
        preset.apply_to(&mut settings);

        assert!(matches_preset(&preset, &settings));
    }

    #[test]
    fn matches_preset_different_resolution_returns_false() {
        let mut settings = AppSettings::default();
        let preset = preset_named("画質優先", 1920, 1080, 30);
        preset.apply_to(&mut settings);
        settings.video.resolution = Some((1280, 720));

        assert!(!matches_preset(&preset, &settings));
    }

    #[test]
    fn matches_preset_different_audio_returns_false() {
        let mut settings = AppSettings::default();
        let preset = preset_named("画質優先", 1920, 1080, 30);
        preset.apply_to(&mut settings);
        settings.audio.passthrough_enabled = !settings.audio.passthrough_enabled;

        assert!(!matches_preset(&preset, &settings));
    }

    #[test]
    fn matches_preset_different_audio_input_source_returns_false() {
        // 入力の種類（#388）も `audio` の一部として比べる。適用すると入力の種類も
        // 切り替わる（`docs/design/presets.md`）
        let mut settings = AppSettings::default();
        let preset = preset_named("画質優先", 1920, 1080, 30);
        preset.apply_to(&mut settings);
        settings.audio.input_source = crate::settings::AudioInputSource::VideoPin;
        assert!(!matches_preset(&preset, &settings));

        preset.apply_to(&mut settings);
        assert_eq!(
            settings.audio.input_source,
            crate::settings::AudioInputSource::Device
        );
    }

    #[test]
    fn matches_preset_ignores_auto_reconnect() {
        // 適用しない項目を比べないこと。比べると、自動再接続を
        // 切り替えただけで「（変更あり）」になる
        let mut settings = AppSettings::default();
        let preset = preset_named("画質優先", 1920, 1080, 30);
        preset.apply_to(&mut settings);
        settings.video.auto_reconnect = !preset.video.auto_reconnect;

        assert!(matches_preset(&preset, &settings));
    }

    #[test]
    fn resolved_active_preset_returns_the_name_when_values_match() {
        let settings: AppSettings =
            toml::from_str(PRESET_CONFIG).expect("プリセット付きの設定を読めること");

        assert_eq!(resolved_active_preset(&settings), Some("低遅延優先"));
        assert_eq!(settings.presets.len(), 2);
    }

    #[test]
    fn resolved_active_preset_returns_none_after_editing_video() {
        let mut settings: AppSettings =
            toml::from_str(PRESET_CONFIG).expect("プリセット付きの設定を読めること");
        settings.video.fps = Some(30);

        assert_eq!(resolved_active_preset(&settings), None);
        // 判定だけでは active_preset を書き換えない
        assert_eq!(settings.active_preset.as_deref(), Some("低遅延優先"));
    }

    #[test]
    fn resolved_active_preset_returns_none_when_the_preset_was_removed() {
        let mut settings: AppSettings =
            toml::from_str(PRESET_CONFIG).expect("プリセット付きの設定を読めること");
        settings.presets.clear();

        assert_eq!(resolved_active_preset(&settings), None);
    }

    #[test]
    fn refresh_active_preset_clears_a_stale_selection() {
        let mut settings: AppSettings =
            toml::from_str(PRESET_CONFIG).expect("プリセット付きの設定を読めること");
        settings.video.fps = Some(24);
        settings.refresh_active_preset();

        assert_eq!(settings.active_preset, None);
    }

    #[test]
    fn refresh_active_preset_keeps_a_matching_selection() {
        let mut settings: AppSettings =
            toml::from_str(PRESET_CONFIG).expect("プリセット付きの設定を読めること");
        settings.refresh_active_preset();

        assert_eq!(settings.active_preset.as_deref(), Some("低遅延優先"));
    }

    #[test]
    fn load_clears_an_active_preset_that_does_not_match() {
        // 設定ファイルを手で書き換えて食い違わせた場合。
        // 読んだ時点で選択が外れていること
        let config = PRESET_CONFIG.replacen("fps = 60", "fps = 24", 1);
        let settings: AppSettings = toml::from_str(&config).expect("読めること");

        assert_eq!(settings.active_preset, None);
        assert_eq!(settings.presets.len(), 2);
    }

    #[test]
    fn upsert_preset_replaces_the_same_name() {
        let mut settings = AppSettings::default();
        settings
            .presets
            .push(preset_named("画質優先", 1920, 1080, 30));
        settings.upsert_preset(preset_named("画質優先", 1280, 720, 60));

        assert_eq!(settings.presets.len(), 1);
        assert_eq!(settings.presets[0].video.resolution, Some((1280, 720)));
    }

    #[test]
    fn upsert_preset_appends_a_new_name() {
        let mut settings = AppSettings::default();
        settings
            .presets
            .push(preset_named("画質優先", 1920, 1080, 30));
        settings.upsert_preset(preset_named("低遅延優先", 1280, 720, 60));

        assert_eq!(settings.presets.len(), 2);
        assert_eq!(settings.presets[1].name, "低遅延優先");
    }

    #[test]
    fn remove_preset_clears_the_active_selection() {
        let mut settings = AppSettings::default();
        settings
            .presets
            .push(preset_named("画質優先", 1920, 1080, 30));
        assert!(settings.apply_preset("画質優先"));

        assert!(settings.remove_preset("画質優先"));

        assert!(settings.presets.is_empty());
        assert_eq!(settings.active_preset, None);
    }

    #[test]
    fn remove_preset_keeps_the_selection_of_another_preset() {
        let mut settings = AppSettings::default();
        settings
            .presets
            .push(preset_named("画質優先", 1920, 1080, 30));
        settings
            .presets
            .push(preset_named("低遅延優先", 1280, 720, 60));
        assert!(settings.apply_preset("画質優先"));

        assert!(settings.remove_preset("低遅延優先"));

        assert_eq!(settings.active_preset.as_deref(), Some("画質優先"));
    }

    #[test]
    fn remove_preset_unknown_name_returns_false() {
        let mut settings = AppSettings::default();
        settings
            .presets
            .push(preset_named("画質優先", 1920, 1080, 30));

        assert!(!settings.remove_preset("無い名前"));
        assert_eq!(settings.presets.len(), 1);
    }

    #[test]
    fn load_drops_a_preset_without_a_name() {
        // 手で書き換えた設定ファイル。名前が無いプリセットをそのまま持つと、
        // 右クリックメニューと一覧に空の項目が並ぶ
        let config = r#"
[[presets]]
name = "   "

[presets.video]
fps = 30

[[presets]]
name = "画質優先"

[presets.video]
fps = 30
"#;

        let settings: AppSettings = toml::from_str(config).expect("読めること");

        assert_eq!(settings.presets.len(), 1);
        assert_eq!(settings.presets[0].name, "画質優先");
    }

    #[test]
    fn load_drops_a_duplicated_preset_name_keeping_the_first() {
        // 重複した名前を残すと、preset() も remove_preset() も先頭しか
        // 見ないため 2 つ目以降を選ぶことも消すこともできない
        let config = r#"
[[presets]]
name = "画質優先"

[presets.video]
fps = 30

[[presets]]
name = " 画質優先 "

[presets.video]
fps = 24
"#;

        let settings: AppSettings = toml::from_str(config).expect("読めること");

        assert_eq!(settings.presets.len(), 1);
        assert_eq!(settings.presets[0].video.fps, Some(30));
    }

    #[test]
    fn load_trims_preset_names_and_the_active_selection() {
        // 名前と選択の両方から空白を落とす。片方だけだと空白の有無で引けなくなる
        let config = r#"
active_preset = "  画質優先  "

[video]
resolution = [1920, 1080]
fps = 30

[[presets]]
name = "  画質優先  "

[presets.video]
resolution = [1920, 1080]
fps = 30
"#;

        let settings: AppSettings = toml::from_str(config).expect("読めること");

        assert_eq!(settings.presets[0].name, "画質優先");
        assert_eq!(settings.active_preset.as_deref(), Some("画質優先"));
        assert_eq!(resolved_active_preset(&settings), Some("画質優先"));
    }

    #[test]
    fn app_settings_without_presets_section_has_no_presets() {
        // プリセットを足した版へ上げた直後。既存ユーザーの設定には
        // [[presets]] も active_preset も無いが、他の項目は保たれること
        let settings: AppSettings =
            toml::from_str(FULL_CONFIG).expect("設定ファイルを読めなければならない");

        assert!(settings.presets.is_empty());
        assert_eq!(settings.active_preset, None);
        assert_eq!(settings.video.resolution, Some((1920, 1080)));
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn presets_survive_a_save_and_load_roundtrip() {
        // 保存で書き出せる形になっていることを確かめる。
        // **TOML はテーブルのあとに素の値を書けない。** active_preset の
        // 宣言位置を後ろへ移すとここで落ちる
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("settings.toml");

        let mut settings = AppSettings::default();
        settings
            .presets
            .push(preset_named("画質優先", 1920, 1080, 30));
        settings
            .presets
            .push(preset_named("低遅延優先", 1280, 720, 60));
        assert!(settings.apply_preset("画質優先"));

        export_to(&path, &settings).expect("書き出せること");
        let loaded = import_from(&path).expect("読めること");

        assert_eq!(loaded.presets, settings.presets);
        assert_eq!(loaded.active_preset.as_deref(), Some("画質優先"));
        assert_eq!(loaded.video.resolution, Some((1920, 1080)));
    }
}
