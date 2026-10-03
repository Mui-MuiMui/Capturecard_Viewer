//! 映像の設定（[video]）。色空間・輝度レンジ・開き方の選択肢と、
//! 映像調整（明るさ・コントラスト・彩度）の範囲、それぞれの serde の補助。

use crate::i18n::Text;
use log::warn;
use serde::{Deserialize, Serialize};

// PartialEq はプリセットとの一致判定（matches_preset）で使う。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoSettings {
    pub device_name: Option<String>,
    pub resolution: Option<(u32, u32)>,
    pub format: Option<String>,
    pub fps: Option<u32>,
    // 映像デバイスを Media Foundation と DirectShow のどちらで開くか。
    // 既定は自動（名前に「(DirectShow)」があれば DirectShow、無ければ
    // Media Foundation）。両方に出るデバイスを DirectShow で開きたいときの
    // 切り替え（#237）。開き方はデバイスと一体なのでプリセットに含める
    #[serde(deserialize_with = "deserialize_video_backend")]
    pub backend: VideoBackendSetting,
    // 稼働中にフレームが途絶えたとき、自動でデバイスを開き直すか。
    //
    // 映像だけでなく音声のストリームエラーにも効く。右クリックメニューの
    // 「デバイスの自動再接続」が 1 つのスイッチで両方を切り替えるため、
    // 設定の置き場所も 1 か所にまとめてある
    pub auto_reconnect: bool,
    // YUY2 → RGB の変換に使う色空間。既定は解像度からの推定（Auto）。
    //
    // キャプチャーボードは入力信号の色空間を通知してこないため、通常は
    // 解像度から推定するしかない。ただし SD で BT.709、HD で BT.601 を
    // 出す機種があるので、手で固定できるようにしてある
    #[serde(deserialize_with = "deserialize_color_space")]
    pub color_space: ColorSpace,
    // 入力信号の輝度レンジ。既定はリミテッド（Y 16〜235）。
    //
    // フルレンジ（Y 0〜255）で出す機種にリミテッド用の係数を当てると、
    // 黒が潰れ白が飛ぶ。こちらも推定できないので設定で選ばせる
    #[serde(deserialize_with = "deserialize_color_range")]
    pub color_range: ColorRange,
    // 映像の明るさ。-100〜100 で 0 が無調整。
    //
    // 色空間やレンジの選び直しでは追いつかない、機種ごとの「暗い」「薄い」
    // といったクセを手で埋めるためのもの。3 つとも YUY2 → RGB の係数表へ
    // 畳み込むので、変換のコストは調整の有無で変わらない
    #[serde(deserialize_with = "deserialize_brightness")]
    pub brightness: i32,
    // 映像のコントラスト。-100〜100 で 0 が無調整。
    // -100 で中間グレー一色、100 で 2 倍になる
    #[serde(deserialize_with = "deserialize_contrast")]
    pub contrast: i32,
    // 映像の彩度。-100〜100 で 0 が無調整。
    // -100 で白黒、100 で 2 倍になる
    #[serde(deserialize_with = "deserialize_saturation")]
    pub saturation: i32,
}

// YUY2 → RGB の変換に使う色空間。設定ファイルには
// color_space = "auto" / "bt601" / "bt709" と書かれる。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ColorSpace {
    // 既存ユーザーの設定ファイルには color_space が無い。既定を Auto に
    // してあるので、これまでどおり解像度からの推定で動く
    #[default]
    Auto,
    Bt601,
    Bt709,
}

impl ColorSpace {
    // 設定ダイアログのコンボボックスに出す表示名
    pub fn label(self) -> &'static str {
        match self {
            ColorSpace::Auto => Text::ColorSpaceAuto.get(),
            ColorSpace::Bt601 => Text::ColorSpaceBt601.get(),
            ColorSpace::Bt709 => Text::ColorSpaceBt709.get(),
        }
    }

    // コンボボックスに並べる順。ダイアログ側で配列を書き写さずに済ませる
    pub const ALL: [ColorSpace; 3] = [ColorSpace::Auto, ColorSpace::Bt601, ColorSpace::Bt709];
}

// 入力信号の輝度レンジ。設定ファイルには
// color_range = "limited" / "full" と書かれる。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ColorRange {
    // 放送・HDMI の既定はリミテッドレンジ。従来の係数表もこちらなので、
    // 設定が無い既存ユーザーの見え方は変わらない
    #[default]
    Limited,
    Full,
}

impl ColorRange {
    // 設定ダイアログのコンボボックスに出す表示名
    pub fn label(self) -> &'static str {
        match self {
            ColorRange::Limited => Text::ColorRangeLimited.get(),
            ColorRange::Full => Text::ColorRangeFull.get(),
        }
    }

    pub const ALL: [ColorRange; 2] = [ColorRange::Limited, ColorRange::Full];
}

// 設定ファイルの color_space に知らない値が書かれていても、設定全体を
// 失わせない。ScreenshotFormat と同じ考え方で、ここでエラーを返すと
// TOML のパースがファイル単位で失敗し、色空間と無関係な項目まで既定値へ戻る。
fn deserialize_color_space<'de, D>(deserializer: D) -> Result<ColorSpace, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    Ok(color_space_from_str(&raw).unwrap_or_else(|| {
        warn!("設定の色空間 \"{}\" を解釈できないので自動として扱う", raw);
        ColorSpace::default()
    }))
}

// 設定ファイルの color_range も同じ扱いにする。
fn deserialize_color_range<'de, D>(deserializer: D) -> Result<ColorRange, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    Ok(color_range_from_str(&raw).unwrap_or_else(|| {
        warn!(
            "設定の色レンジ \"{}\" を解釈できないのでリミテッドとして扱う",
            raw
        );
        ColorRange::default()
    }))
}

// 設定ファイルに書かれた文字列から色空間を決める。解釈できない場合は None。
fn color_space_from_str(raw: &str) -> Option<ColorSpace> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "auto" => Some(ColorSpace::Auto),
        // ドットや空白入りで手書きされることを見込んで、区切りを落とした形も拾う
        "bt601" | "bt.601" | "601" => Some(ColorSpace::Bt601),
        "bt709" | "bt.709" | "709" => Some(ColorSpace::Bt709),
        _ => None,
    }
}

// 設定ファイルに書かれた文字列から輝度レンジを決める。解釈できない場合は None。
fn color_range_from_str(raw: &str) -> Option<ColorRange> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "limited" | "tv" => Some(ColorRange::Limited),
        "full" | "pc" => Some(ColorRange::Full),
        _ => None,
    }
}

// 映像デバイスを開く経路の設定。設定ファイルには
// backend = "auto" / "media_foundation" / "direct_show" と書かれる。
//
// 実際に開いた経路（`video::CaptureApi`）とは別の型にしてある。こちらは
// 「自動」を持ち、デバイス名と合わせて初めて 1 つに決まるため
// （`app::backend::system` の `route_for`）。
// `Hash` は設定ダイアログの能力キャッシュのキー（`ui::VideoCapabilityKey`）に使う
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum VideoBackendSetting {
    // 名前に「(DirectShow)」があれば DirectShow、無ければ Media Foundation
    #[default]
    #[serde(rename = "auto")]
    Auto,
    // 「(DirectShow)」付きの名前でも Media Foundation の一覧から探す
    #[serde(rename = "media_foundation")]
    MediaFoundation,
    // 同じ表示名を DirectShow の一覧から探す
    #[serde(rename = "direct_show")]
    DirectShow,
}

impl VideoBackendSetting {
    // 設定ダイアログのコンボボックスに出す表示名
    pub fn label(self) -> &'static str {
        match self {
            VideoBackendSetting::Auto => Text::VideoBackendAuto.get(),
            VideoBackendSetting::MediaFoundation => Text::VideoBackendMediaFoundation.get(),
            VideoBackendSetting::DirectShow => Text::VideoBackendDirectShow.get(),
        }
    }

    pub const ALL: [VideoBackendSetting; 3] = [
        VideoBackendSetting::Auto,
        VideoBackendSetting::MediaFoundation,
        VideoBackendSetting::DirectShow,
    ];
}

// 設定ファイルの backend に知らない値が書かれていても、設定全体を
// 失わせない。色空間と同じ考え方で、自動として扱う
fn deserialize_video_backend<'de, D>(deserializer: D) -> Result<VideoBackendSetting, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    Ok(video_backend_from_str(&raw).unwrap_or_else(|| {
        warn!(
            "設定の映像の開き方 \"{}\" を解釈できないので自動として扱う",
            raw
        );
        VideoBackendSetting::default()
    }))
}

// 設定ファイルに書かれた文字列から開き方を決める。解釈できない場合は None。
fn video_backend_from_str(raw: &str) -> Option<VideoBackendSetting> {
    // 手書きされることを見込んで、区切り（`_` / `-` / 空白）の有無は問わない
    let normalized: String = raw
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|c| !matches!(c, '_' | '-' | ' '))
        .collect();
    match normalized.as_str() {
        "auto" => Some(VideoBackendSetting::Auto),
        "mediafoundation" | "mf" => Some(VideoBackendSetting::MediaFoundation),
        "directshow" | "dshow" => Some(VideoBackendSetting::DirectShow),
        _ => None,
    }
}

// 映像調整（明るさ・コントラスト・彩度）の下限と上限。0 が無調整。
//
// 3 つで範囲を揃えてあるのは、スライダーの中央が常に「無調整」になり、
// 「リセット」が 3 つとも 0 を書くだけで済むため。実際の倍率への変換は
// video::VideoAdjustments が受け持つ
pub const MIN_VIDEO_ADJUSTMENT: i32 = -100;

pub const MAX_VIDEO_ADJUSTMENT: i32 = 100;

// 範囲外の映像調整の値が書かれていても、設定全体を失わせない。
// 考え方は deserialize_jpeg_quality と同じで、TOML の整数である i64 で
// 受けてから -100〜100 へ丸める。i32 のまま読むと、手で書き換えられた
// 巨大な値でパースがファイル単位で失敗し、無関係な項目まで既定値へ戻る。
//
// 項目名を引数で受けるのは、ログだけを見て「どのスライダーの値が
// 丸められたか」を判別できるようにするため。
fn clamp_video_adjustment(name: &str, raw: i64) -> i32 {
    let clamped = raw.clamp(
        i64::from(MIN_VIDEO_ADJUSTMENT),
        i64::from(MAX_VIDEO_ADJUSTMENT),
    );
    if clamped != raw {
        warn!(
            "設定の映像調整（{}）{} は範囲外なので {} として扱う",
            name, raw, clamped
        );
    }
    // clamp 済みなので i32 に収まる
    clamped as i32
}

fn deserialize_brightness<'de, D>(deserializer: D) -> Result<i32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(clamp_video_adjustment(
        "明るさ",
        i64::deserialize(deserializer)?,
    ))
}

fn deserialize_contrast<'de, D>(deserializer: D) -> Result<i32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(clamp_video_adjustment(
        "コントラスト",
        i64::deserialize(deserializer)?,
    ))
}

fn deserialize_saturation<'de, D>(deserializer: D) -> Result<i32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(clamp_video_adjustment(
        "彩度",
        i64::deserialize(deserializer)?,
    ))
}

impl Default for VideoSettings {
    fn default() -> Self {
        Self {
            device_name: None,
            resolution: Some((1280, 720)),    // 720pで安定性を優先
            format: Some("YUY2".to_string()), // YUY2フォーマット
            fps: Some(60),                    // 60fps目標
            // 既存ユーザーの設定ファイルには backend が無い。自動にしておけば
            // これまでどおり名前で経路が決まる
            backend: VideoBackendSetting::Auto,
            // 既定は有効。USB を挿し直したときに何もしなくても復帰するほうが、
            // 「映像が止まったまま気付かない」よりも害が少ない
            auto_reconnect: true,
            // 既定は従来どおりの振る舞い。解像度から BT.601 / BT.709 を選び、
            // リミテッドレンジの係数で変換する
            color_space: ColorSpace::Auto,
            color_range: ColorRange::Limited,
            // 既定は無調整。係数表がそのまま使われ、変換結果は
            // 映像調整を入れる前と 1 ビットも変わらない
            brightness: 0,
            contrast: 0,
            saturation: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::testing::{without_key, FULL_CONFIG};
    use crate::settings::AppSettings;

    #[test]
    fn app_settings_missing_auto_reconnect_defaults_to_enabled() {
        // 自動再接続の項目を足した版へ上げた直後、既存ユーザーの設定ファイルには
        // このキーが無い。欠けていても他の項目が保持され、既定の有効になること。
        let config = without_key(FULL_CONFIG, "auto_reconnect");
        assert!(
            !config.contains("auto_reconnect ="),
            "テスト用の設定から auto_reconnect が消えていない"
        );

        let settings: AppSettings =
            toml::from_str(&config).expect("auto_reconnect が欠けていても読めなければならない");

        assert!(settings.video.auto_reconnect); // 既定値は true
        assert_eq!(settings.video.fps, Some(30));
        assert_eq!(
            settings.video.device_name,
            Some("Capture Device".to_string())
        );
    }

    #[test]
    fn app_settings_auto_reconnect_false_is_kept() {
        // 明示的に無効にした設定が、既定値（true）で上書きされないこと。
        let settings: AppSettings =
            toml::from_str(FULL_CONFIG).expect("全項目そろった設定は読めなければならない");

        assert!(!settings.video.auto_reconnect);
    }

    #[test]
    fn app_settings_missing_color_keys_use_auto_and_limited() {
        // 色空間の設定を足す前の版が書いた設定ファイル。
        // 2 つのキーだけが既定へ倒れ、他の項目は保持されなければならない
        let config = without_key(&without_key(FULL_CONFIG, "color_space"), "color_range");
        assert!(
            !config.contains("color_space =") && !config.contains("color_range ="),
            "テスト用の設定から色空間のキーが消えていない"
        );

        let settings: AppSettings =
            toml::from_str(&config).expect("色空間のキーが欠けていても読めなければならない");

        assert_eq!(settings.video.color_space, ColorSpace::Auto);
        assert_eq!(settings.video.color_range, ColorRange::Limited);
        assert_eq!(settings.video.fps, Some(30));
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn app_settings_unknown_color_space_falls_back_without_losing_settings() {
        // 手で書き換えて綴りを誤った場合。色空間だけが自動へ倒れ、
        // 無関係な項目は保持されなければならない
        let config = FULL_CONFIG.replace(r#"color_space = "bt601""#, r#"color_space = "bt2020""#);
        assert!(config.contains(r#"color_space = "bt2020""#));

        let settings: AppSettings =
            toml::from_str(&config).expect("知らない色空間でも読めなければならない");

        assert_eq!(settings.video.color_space, ColorSpace::Auto);
        // 同じセクションの他の項目が巻き添えになっていないこと
        assert_eq!(settings.video.color_range, ColorRange::Full);
        assert_eq!(settings.video.fps, Some(30));
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn app_settings_unknown_color_range_falls_back_without_losing_settings() {
        let config = FULL_CONFIG.replace(r#"color_range = "full""#, r#"color_range = "wide""#);
        assert!(config.contains(r#"color_range = "wide""#));

        let settings: AppSettings =
            toml::from_str(&config).expect("知らない色レンジでも読めなければならない");

        assert_eq!(settings.video.color_range, ColorRange::Limited);
        assert_eq!(settings.video.color_space, ColorSpace::Bt601);
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn app_settings_missing_video_adjustment_keys_use_zero() {
        // 映像調整を足す前の版が書いた設定ファイル。
        // 3 つのキーだけが無調整へ倒れ、他の項目は保持されなければならない
        let config = without_key(
            &without_key(&without_key(FULL_CONFIG, "brightness"), "contrast"),
            "saturation",
        );
        assert!(
            !config.contains("brightness =")
                && !config.contains("contrast =")
                && !config.contains("saturation ="),
            "テスト用の設定から映像調整のキーが消えていない"
        );

        let settings: AppSettings =
            toml::from_str(&config).expect("映像調整のキーが欠けていても読めなければならない");

        assert_eq!(settings.video.brightness, 0);
        assert_eq!(settings.video.contrast, 0);
        assert_eq!(settings.video.saturation, 0);
        assert_eq!(settings.video.color_range, ColorRange::Full);
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn app_settings_video_adjustment_keys_are_read() {
        let settings: AppSettings =
            toml::from_str(FULL_CONFIG).expect("全項目を書いた設定は読めなければならない");

        assert_eq!(settings.video.brightness, 10);
        assert_eq!(settings.video.contrast, -20);
        assert_eq!(settings.video.saturation, 30);
    }

    #[test]
    fn app_settings_out_of_range_video_adjustment_is_clamped_without_losing_settings() {
        // 手で書き換えた場合。i32 に収まらない値でも設定全体を失わせない
        let config = FULL_CONFIG
            .replace("brightness = 10", "brightness = 5000000000")
            .replace("contrast = -20", "contrast = -300");

        let settings: AppSettings =
            toml::from_str(&config).expect("範囲外の映像調整でも読めなければならない");

        assert_eq!(settings.video.brightness, MAX_VIDEO_ADJUSTMENT);
        assert_eq!(settings.video.contrast, MIN_VIDEO_ADJUSTMENT);
        // 巻き添えになっていないこと
        assert_eq!(settings.video.saturation, 30);
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn clamp_video_adjustment_keeps_values_in_range() {
        assert_eq!(clamp_video_adjustment("明るさ", 0), 0);
        assert_eq!(clamp_video_adjustment("明るさ", 100), 100);
        assert_eq!(clamp_video_adjustment("明るさ", -100), -100);
        assert_eq!(clamp_video_adjustment("明るさ", 101), 100);
        assert_eq!(clamp_video_adjustment("明るさ", -101), -100);
        assert_eq!(clamp_video_adjustment("明るさ", i64::MAX), 100);
        assert_eq!(clamp_video_adjustment("明るさ", i64::MIN), -100);
    }

    #[test]
    fn color_space_from_str_accepts_known_spellings() {
        assert_eq!(color_space_from_str("auto"), Some(ColorSpace::Auto));
        assert_eq!(color_space_from_str(" AUTO "), Some(ColorSpace::Auto));
        assert_eq!(color_space_from_str("bt601"), Some(ColorSpace::Bt601));
        assert_eq!(color_space_from_str("BT.709"), Some(ColorSpace::Bt709));
        assert_eq!(color_space_from_str("601"), Some(ColorSpace::Bt601));
        assert_eq!(color_space_from_str(""), None);
        assert_eq!(color_space_from_str("bt2020"), None);
    }

    #[test]
    fn color_range_from_str_accepts_known_spellings() {
        assert_eq!(color_range_from_str("limited"), Some(ColorRange::Limited));
        assert_eq!(color_range_from_str(" TV "), Some(ColorRange::Limited));
        assert_eq!(color_range_from_str("full"), Some(ColorRange::Full));
        assert_eq!(color_range_from_str("pc"), Some(ColorRange::Full));
        assert_eq!(color_range_from_str(""), None);
        assert_eq!(color_range_from_str("wide"), None);
    }

    #[test]
    fn color_space_and_range_serialize_as_lowercase_strings() {
        // 設定ファイルに書き出される綴り。ここが変わると、既に配布した版が
        // 書いた設定ファイルを読めなくなる
        let mut settings = AppSettings::default();
        settings.video.color_space = ColorSpace::Bt709;
        settings.video.color_range = ColorRange::Full;

        let serialized = toml::to_string(&settings).expect("設定を書き出せること");

        assert!(
            serialized.contains(r#"color_space = "bt709""#),
            "{}",
            serialized
        );
        assert!(
            serialized.contains(r#"color_range = "full""#),
            "{}",
            serialized
        );
    }

    #[test]
    fn video_backend_is_read_from_the_full_config() {
        let settings: AppSettings =
            toml::from_str(FULL_CONFIG).expect("全項目そろった設定は読めなければならない");
        assert_eq!(settings.video.backend, VideoBackendSetting::DirectShow);
    }

    #[test]
    fn video_backend_missing_defaults_to_auto_and_keeps_other_items() {
        // 開き方の項目ができる前の設定ファイル
        let config = without_key(FULL_CONFIG, "backend");
        assert!(!config.contains("backend ="));

        let settings: AppSettings = toml::from_str(&config).expect("読めること");

        assert_eq!(settings.video.backend, VideoBackendSetting::Auto);
        assert_eq!(
            settings.video.device_name,
            Some("Capture Device".to_string())
        );
        assert_eq!(settings.video.fps, Some(30));
        assert!(!settings.video.auto_reconnect);
    }

    #[test]
    fn video_backend_unknown_value_falls_back_to_auto_and_keeps_other_items() {
        let config = FULL_CONFIG.replace(r#"backend = "direct_show""#, r#"backend = "vfw""#);
        assert!(config.contains(r#"backend = "vfw""#));

        let settings: AppSettings =
            toml::from_str(&config).expect("知らない開き方でも読めなければならない");

        assert_eq!(settings.video.backend, VideoBackendSetting::Auto);
        // 同じセクションの他の項目が巻き添えになっていないこと
        assert_eq!(settings.video.format, Some("MJPEG".to_string()));
        assert_eq!(settings.video.color_space, ColorSpace::Bt601);
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn video_backend_serializes_as_snake_case_and_reads_back() {
        // 設定ファイルに書き出される綴り。ここが変わると、既に配布した版が
        // 書いた設定ファイルを読めなくなる
        for (setting, expected) in [
            (VideoBackendSetting::Auto, r#"backend = "auto""#),
            (
                VideoBackendSetting::MediaFoundation,
                r#"backend = "media_foundation""#,
            ),
            (
                VideoBackendSetting::DirectShow,
                r#"backend = "direct_show""#,
            ),
        ] {
            let mut settings = AppSettings::default();
            settings.video.backend = setting;
            let serialized = toml::to_string(&settings).expect("設定を書き出せること");
            assert!(serialized.contains(expected), "{}", serialized);

            let restored: AppSettings = toml::from_str(&serialized).expect("読み戻せること");
            assert_eq!(restored.video.backend, setting);
        }
    }

    #[test]
    fn video_backend_from_str_accepts_known_spellings() {
        assert_eq!(
            video_backend_from_str("auto"),
            Some(VideoBackendSetting::Auto)
        );
        assert_eq!(
            video_backend_from_str(" Media Foundation "),
            Some(VideoBackendSetting::MediaFoundation)
        );
        assert_eq!(
            video_backend_from_str("MF"),
            Some(VideoBackendSetting::MediaFoundation)
        );
        assert_eq!(
            video_backend_from_str("DirectShow"),
            Some(VideoBackendSetting::DirectShow)
        );
        assert_eq!(
            video_backend_from_str("direct-show"),
            Some(VideoBackendSetting::DirectShow)
        );
        assert_eq!(video_backend_from_str(""), None);
        assert_eq!(video_backend_from_str("vfw"), None);
    }
}
