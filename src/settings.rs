use crate::hotkey::HotkeyAction;
use chrono::Datelike;
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// confy が設定ファイルの置き場所を決めるのに使う名前。
// ここがずれると既存の設定ファイルを見失うため、1 箇所にまとめてある。
// ログの出力先も同じデータディレクトリを基準に決めるので、logging から参照する。
pub(crate) const APP_NAME: &str = "capturecard_viewer";

// 各構造体の #[serde(default)] は、項目を追加したあとも古い設定ファイルを
// 読めるようにするためのもの。これが無いと、
//   - Option 以外の項目が欠けた場合はパースが失敗し、全項目が初期化される
//   - Option の項目が欠けた場合は None になり、Default の値が使われない
// という形で既存ユーザーの設定が失われる。新しい項目を足すときも外さないこと。

// 読み込みは RawAppSettings を経由する。旧版の screenshot.hotkey を
// hotkeys へ移す処理（migrate_hotkeys）を、どの経路で読んでも必ず通すため。
// #[serde(from)] を外すと、テストの toml::from_str だけ移行を通らない、
// といった食い違いが生まれる。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "RawAppSettings")]
pub struct AppSettings {
    // 選択中のプリセット名。どれも選んでいなければ `None`。
    //
    // **TOML のテーブルより前に置くこと。** 素の値はテーブルの前にしか
    // 書けないため、`video` などの後ろに宣言すると保存できない形になる。
    pub active_preset: Option<String>,
    pub video: VideoSettings,
    pub audio: AudioSettings,
    pub screenshot: ScreenshotSettings,
    pub ui: UiSettings,
    // アクション → ホットキー文字列。割り当てが無いアクションは入っていない。
    //
    // 設定ファイルでは独立した [hotkeys] セクションになる。**セクションごと
    // 存在しない場合と、空のセクションがある場合は意味が違う。** 前者は
    // 旧版が書いた設定ファイル（既定の F5 を入れる）、後者はすべての
    // 割り当てを外した状態（何も入れない）。
    pub hotkeys: BTreeMap<HotkeyAction, String>,
    // 名前付きのプリセット。設定ファイルでは [[presets]] の並びになる。
    //
    // 中身は `video` と `audio` だけ。線引きの理由は `Preset` のコメントを見ること。
    pub presets: Vec<Preset>,
}

// 設定ファイルから読んだままの形。
//
// AppSettings との違いは hotkeys が Option であることだけ。`None` は
// 「[hotkeys] セクションが無い」を表し、空のマップ（セクションはあるが
// 中身が空）と区別する。この区別が無いと、旧版の設定ファイルを読んだときに
// 既定値の F5 とユーザーが外した状態を見分けられない。
//
// キーを String で受けるのは、知らないアクション名が書かれていても
// ファイル全体のパースを失敗させないため。読めない名前は捨ててログに残す。
//
// **AppSettings に項目を足すときは、ここと From の実装にも足すこと。**
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct RawAppSettings {
    active_preset: Option<String>,
    video: VideoSettings,
    audio: AudioSettings,
    screenshot: ScreenshotSettings,
    ui: UiSettings,
    hotkeys: Option<BTreeMap<String, String>>,
    presets: Vec<Preset>,
}

impl From<RawAppSettings> for AppSettings {
    fn from(raw: RawAppSettings) -> Self {
        let RawAppSettings {
            active_preset,
            video,
            audio,
            mut screenshot,
            ui,
            hotkeys,
            presets,
        } = raw;

        // 旧版の項目はここで読み切って捨てる。保存では書き出さない
        let legacy_hotkey = screenshot.legacy_hotkey.take();
        let hotkeys = migrate_hotkeys(hotkeys, legacy_hotkey);

        // 設定ファイルは手で書き換えられる。名前が無い、あるいは重複した
        // プリセットをそのまま持ち込ませない
        let presets = sanitize_presets(presets);

        let mut settings = Self {
            // 名前の前後の空白は落としてある（sanitize_presets）ので、
            // 選択側も同じ形に揃える。揃えないと空白の有無だけで引けなくなる
            active_preset: active_preset.map(|name| name.trim().to_string()),
            video,
            audio,
            screenshot,
            ui,
            hotkeys,
            presets,
        };
        // 設定ファイルを手で書き換えて、選択中のプリセットと実際の値を
        // 食い違わせることができる。読んだ時点で辻褄を合わせておく
        settings.refresh_active_preset();
        settings
    }
}

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
fn sanitize_presets(presets: Vec<Preset>) -> Vec<Preset> {
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

// 既定のホットキー割り当て。
//
// **スクリーンショット以外は既定で未割り当てにしてある。** グローバル
// ホットキーは他のアプリより先にキーを奪うため、こちらから勝手に
// F11 や Ctrl+↑ のような一般的なキーを押さえるべきではない。
fn default_hotkeys() -> BTreeMap<HotkeyAction, String> {
    BTreeMap::from([(HotkeyAction::Screenshot, "F5".to_string())])
}

// 設定ファイルの [hotkeys] と、旧版の screenshot.hotkey から、実際に使う
// 割り当てを決める。
//
// - [hotkeys] がある（新しい版が書いた）: そのまま使う。旧版の項目は無視する
// - [hotkeys] が無い（旧版が書いた）: 既定値を土台に、screenshot.hotkey が
//   あればスクリーンショットへ移す
//
// 旧版の設定ファイルで screenshot.hotkey が欠けている場合は既定の F5 になる。
// 旧版では「ホットキーを外した状態」を設定ファイルに残せなかった（項目ごと
// 消えるため、欠けた項目と区別できない）ので、そこは従来どおりの挙動に揃えてある。
fn migrate_hotkeys(
    table: Option<BTreeMap<String, String>>,
    legacy_hotkey: Option<String>,
) -> BTreeMap<HotkeyAction, String> {
    let Some(table) = table else {
        let mut hotkeys = default_hotkeys();
        if let Some(hotkey) = legacy_hotkey {
            info!(
                "旧版の設定にあるスクリーンショットのホットキー {} を hotkeys へ移す",
                hotkey
            );
            hotkeys.insert(HotkeyAction::Screenshot, hotkey);
        }
        return hotkeys;
    };

    let mut hotkeys = BTreeMap::new();
    for (key, hotkey) in table {
        match HotkeyAction::from_key(&key) {
            Some(action) => {
                hotkeys.insert(action, hotkey);
            }
            // 新しい版が増やしたアクションを古い版で読んだ場合など。
            // ここでエラーにすると設定ファイル全体が読めなくなる
            None => warn!(
                "設定の hotkeys にある知らないアクション \"{}\" を無視する",
                key
            ),
        }
    }
    hotkeys
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            active_preset: None,
            video: VideoSettings::default(),
            audio: AudioSettings::default(),
            screenshot: ScreenshotSettings::default(),
            ui: UiSettings::default(),
            hotkeys: default_hotkeys(),
            presets: Vec::new(),
        }
    }
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
// `video.auto_reconnect` だけは `video` の中にありながら対象外。理由は
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
    pub fn apply_to(&self, settings: &mut AppSettings) {
        let auto_reconnect = settings.video.auto_reconnect;
        settings.video = self.video.clone();
        settings.video.auto_reconnect = auto_reconnect;
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
            PresetNameError::Empty => "プリセット名を入力してください",
            PresetNameError::Duplicate => "同じ名前のプリセットが既にあります",
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

// PartialEq はプリセットとの一致判定（matches_preset）で使う。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoSettings {
    pub device_name: Option<String>,
    pub resolution: Option<(u32, u32)>,
    pub format: Option<String>,
    pub fps: Option<u32>,
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
            ColorSpace::Auto => "自動（解像度から判断）",
            ColorSpace::Bt601 => "BT.601（SD）",
            ColorSpace::Bt709 => "BT.709（HD）",
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
            ColorRange::Limited => "リミテッド（16〜235）",
            ColorRange::Full => "フル（0〜255）",
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

// PartialEq は VideoSettings と同じくプリセットとの一致判定で使う。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioSettings {
    pub input_device_name: Option<String>,
    pub output_device_name: Option<String>,
    // 以下 2 項目は「希望値」。実際に開く値はデバイスの能力に合わせて
    // `audio::select_best_config` が寄せるため、ここと食い違うことがある
    pub sample_rate: Option<u32>,
    pub channels: Option<u16>,
    pub passthrough_enabled: bool,
    // 入力から出力へ受け渡すリングバッファの長さ（ミリ秒）。
    //
    // 小さいほど遅延が減るが、出力コールバックが間に合わずプチプチという
    // 音（アンダーラン）が出やすくなる。最適な値は環境ごとに違うので
    // 設定ダイアログのスライダーで選ばせる。
    //
    // **同じ [audio] の sample_rate / channels と違い `Option` にしない。**
    // あちらは「希望値」で、デバイスの能力に合わせて寄せられる余地があるが、
    // バッファ長はこちらで好きに決められるので寄せ先が無い。
    //
    // 範囲外の値が書かれていても設定全体を失わせない（deserialize_audio_buffer_ms）
    #[serde(deserialize_with = "deserialize_audio_buffer_ms")]
    pub audio_buffer_ms: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScreenshotSettings {
    // 撮った画をどこへ出すか。ファイル・クリップボード・両方の 3 択。
    // 保存形式と JPEG 品質はファイルへ出すときだけ効く（クリップボードへは
    // 圧縮せずそのまま渡す）
    #[serde(deserialize_with = "deserialize_screenshot_destination")]
    pub destination: ScreenshotDestination,
    pub save_folder: PathBuf,
    // 保存形式と JPEG の品質を別々の項目にしてある。品質を持つ enum を
    // 1 項目として持たせると TOML では [screenshot.format] のテーブルになり、
    // 同じセクションの後続のキー（sound_file など）がテーブルの内側へ
    // 取り込まれてしまう。また項目を分けておくと、PNG に切り替えても
    // 品質の値が残り、JPEG へ戻したときに選び直さずに済む。
    //
    // エンコードへ渡すときは encoding() で ScreenshotEncoding にまとめ、
    // 「PNG なのに品質が付いている」組み合わせを作れないようにする
    #[serde(deserialize_with = "deserialize_screenshot_format")]
    pub format: ScreenshotFormat,
    #[serde(deserialize_with = "deserialize_jpeg_quality")]
    pub jpeg_quality: u8,
    pub sound_file: Option<PathBuf>,
    pub sound_volume: f32,
    // 旧版のホットキー設定。**読むだけで、保存では書き出さない。**
    //
    // ホットキーはアクションごとに持つようになったため、置き場所は
    // `AppSettings::hotkeys` へ移った。ここに残しているのは、既存の
    // 設定ファイルにある値を起動時に移すためだけ。`AppSettings` を作る
    // 時点で `None` に戻るので、この項目を見て動く処理を足さないこと。
    #[serde(rename = "hotkey", skip_serializing)]
    pub legacy_hotkey: Option<String>,
}

// スクリーンショットの出力先。
// 設定ファイルには destination = "file" / "clipboard" / "both" と書かれる。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ScreenshotDestination {
    // 既存ユーザーの設定ファイルには destination が無い。既定をファイルに
    // してあるので、これまでどおりフォルダへ保存されるだけで挙動は変わらない
    #[default]
    File,
    Clipboard,
    Both,
}

impl ScreenshotDestination {
    // ファイルへ書き出すか。false のときは保存先のファイル名も作らない
    pub fn saves_file(self) -> bool {
        matches!(
            self,
            ScreenshotDestination::File | ScreenshotDestination::Both
        )
    }

    // クリップボードへコピーするか
    pub fn copies_to_clipboard(self) -> bool {
        matches!(
            self,
            ScreenshotDestination::Clipboard | ScreenshotDestination::Both
        )
    }
}

// スクリーンショットの保存形式。設定ファイルには format = "jpeg" / "png" と書かれる。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ScreenshotFormat {
    // 既存ユーザーの設定ファイルには format が無い。既定を JPEG にしてあるので
    // 従来どおり JPEG で保存され、拡張子も .jpg のまま変わらない
    #[default]
    Jpeg,
    Png,
}

impl ScreenshotFormat {
    // 保存するファイルの拡張子。先頭のドットは含まない
    pub fn extension(self) -> &'static str {
        match self {
            ScreenshotFormat::Jpeg => "jpg",
            ScreenshotFormat::Png => "png",
        }
    }
}

impl ScreenshotSettings {
    // 設定からエンコードの指定を組み立てる。
    //
    // 品質は設定ファイルを手で書き換えられる前提で、ここで範囲に収める。
    // image 0.24 の JpegEncoder も内部で 1〜100 に丸めるが、そこに寄りかかると
    // クレートの版が変わったときに振る舞いが変わる。渡す前に確定させておく
    pub fn encoding(&self) -> ScreenshotEncoding {
        match self.format {
            ScreenshotFormat::Jpeg => ScreenshotEncoding::Jpeg {
                quality: self.jpeg_quality.clamp(MIN_JPEG_QUALITY, MAX_JPEG_QUALITY),
            },
            ScreenshotFormat::Png => ScreenshotEncoding::Png,
        }
    }
}

// 実際にエンコードするときの形式とパラメータ。
//
// 設定の保存形式（ScreenshotFormat）と分けてあるのは、保存関数へ
// 「PNG なのに品質が付いている」ような組み合わせを渡せなくするため。
// 設定ファイルには書かれないので、TOML の都合に縛られない
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenshotEncoding {
    Jpeg { quality: u8 },
    Png,
}

// オーディオのサンプリングレートとチャンネル数の既定値。
// 設定に値が入っていないときの表示にも使うので、設定画面側と揃うよう定数にしてある
pub const DEFAULT_SAMPLE_RATE: u32 = 48_000;
pub const DEFAULT_CHANNELS: u16 = 2;

// 音声のリングバッファの長さ（ミリ秒）の下限・上限と既定値。
//
// 下限を 20ms にしてあるのは、WASAPI 共有モードの出力コールバックが
// 10ms 前後の周期で呼ばれるため。それを下回る長さにすると、1 回の
// コールバックで使い切ってしまい常に音が途切れる。
//
// 上限の 200ms は、遅延として体感できる上限の目安。これ以上を選べても
// 「映像より音が遅れている」状態を積むだけで、低遅延という目的から外れる。
//
// 既定の 50ms は設定項目になる前にハードコードされていた長さ。
// 更新しても既存ユーザーの音の出かたが変わらないようにしてある
pub const MIN_AUDIO_BUFFER_MS: u32 = 20;
pub const MAX_AUDIO_BUFFER_MS: u32 = 200;
pub const DEFAULT_AUDIO_BUFFER_MS: u32 = 50;

// JPEG 品質の下限と上限。image クレートの JpegEncoder が受け付ける範囲に合わせてある
pub const MIN_JPEG_QUALITY: u8 = 1;
pub const MAX_JPEG_QUALITY: u8 = 100;

// 映像調整（明るさ・コントラスト・彩度）の下限と上限。0 が無調整。
//
// 3 つで範囲を揃えてあるのは、スライダーの中央が常に「無調整」になり、
// 「リセット」が 3 つとも 0 を書くだけで済むため。実際の倍率への変換は
// video::VideoAdjustments が受け持つ
pub const MIN_VIDEO_ADJUSTMENT: i32 = -100;
pub const MAX_VIDEO_ADJUSTMENT: i32 = 100;

// 音量の下限と上限。100% が等倍で、そこから先は増幅になる。
// UI（スライダー・ホイール）と OSD の表示もこの範囲を前提にしている
pub const MIN_VOLUME: f32 = 0.0;
pub const MAX_VOLUME: f32 = 200.0;

// 音量の既定値。等倍
pub const DEFAULT_VOLUME: f32 = 100.0;

// 設定ファイルの format に知らない値が書かれていても、設定全体を失わせない。
// ここでエラーを返すと TOML のパースがファイル単位で失敗し、保存形式と
// 無関係な項目まで既定値へ戻ってしまう。
//
// 値が文字列ですらない場合（format = 3 など）はここでも落ちる。手で書き換えた
// ときに起きやすいのは綴りの誤りなので、拾うのはそこまでにしてある。
fn deserialize_screenshot_format<'de, D>(deserializer: D) -> Result<ScreenshotFormat, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    Ok(screenshot_format_from_str(&raw).unwrap_or_else(|| {
        warn!(
            "設定の保存形式 \"{}\" を解釈できないので JPEG として扱う",
            raw
        );
        ScreenshotFormat::default()
    }))
}

// 設定ファイルの destination に知らない値が書かれていても、設定全体を失わせない。
// 理由は deserialize_screenshot_format と同じ。
fn deserialize_screenshot_destination<'de, D>(
    deserializer: D,
) -> Result<ScreenshotDestination, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    Ok(screenshot_destination_from_str(&raw).unwrap_or_else(|| {
        warn!(
            "設定の出力先 \"{}\" を解釈できないのでファイルへの保存として扱う",
            raw
        );
        ScreenshotDestination::default()
    }))
}

// 範囲外の品質が書かれていても、設定全体を失わせない。u8 のまま読むと
// jpeg_quality = 256 のような値でパースがファイル単位で失敗し、品質と
// 無関係な項目まで既定値へ戻ってしまう。TOML の整数は i64 なので、
// 広いほうで受けてから 1〜100 に丸める。
//
// 値が整数ですらない場合（jpeg_quality = 90.5 など）はここでも落ちる。
// format と同じく、手で書き換えたときに起きやすいところだけを拾う。
fn deserialize_jpeg_quality<'de, D>(deserializer: D) -> Result<u8, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = i64::deserialize(deserializer)?;
    let clamped = raw.clamp(i64::from(MIN_JPEG_QUALITY), i64::from(MAX_JPEG_QUALITY));
    if clamped != raw {
        warn!(
            "設定の JPEG 品質 {} は範囲外なので {} として扱う",
            raw, clamped
        );
    }
    // clamp 済みなので u8 に収まる
    Ok(clamped as u8)
}

// 範囲外の音声バッファ長が書かれていても、設定全体を失わせない。
// 考え方は deserialize_jpeg_quality と同じで、TOML の整数である i64 で
// 受けてから 20〜200ms へ丸める。u32 のまま読むと、手で書き換えられた
// 負の値でパースがファイル単位で失敗し、無関係な項目まで既定値へ戻る。
fn deserialize_audio_buffer_ms<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = i64::deserialize(deserializer)?;
    let clamped = raw.clamp(
        i64::from(MIN_AUDIO_BUFFER_MS),
        i64::from(MAX_AUDIO_BUFFER_MS),
    );
    if clamped != raw {
        warn!(
            "設定の音声バッファ長 {} ms は範囲外なので {} ms として扱う",
            raw, clamped
        );
    }
    // clamp 済みなので u32 に収まる
    Ok(clamped as u32)
}

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

// 範囲外の音量が書かれていても、そのまま受け取らない。
// UI からは 0〜200% しか作れないが、設定ファイルは手で書き換えられる。
// 1000% が書かれていると OSD に「音量: 1000%」と出てしまう
// （音声側は AudioCapture::set_volume が別途 0〜2.0 に丸めている）。
//
// jpeg_quality と違い、範囲外でもパース自体は成功するので設定が失われる
// わけではない。表示と実際の音量を食い違わせないために丸めている。
//
// NaN と無限大は clamp では落ちない（NaN.clamp(..) は NaN を返す）ため、
// 先に既定値へ倒す。
fn deserialize_volume<'de, D>(deserializer: D) -> Result<f32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = f32::deserialize(deserializer)?;
    if !raw.is_finite() {
        warn!(
            "設定の音量 {} は数値として扱えないので {}% として扱う",
            raw, DEFAULT_VOLUME
        );
        return Ok(DEFAULT_VOLUME);
    }

    let clamped = raw.clamp(MIN_VOLUME, MAX_VOLUME);
    if clamped != raw {
        warn!("設定の音量 {} は範囲外なので {} として扱う", raw, clamped);
    }
    Ok(clamped)
}

// 設定ファイルに書かれた文字列から出力先を決める。
// 解釈できない場合は None を返す。
fn screenshot_destination_from_str(raw: &str) -> Option<ScreenshotDestination> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "file" => Some(ScreenshotDestination::File),
        "clipboard" => Some(ScreenshotDestination::Clipboard),
        "both" => Some(ScreenshotDestination::Both),
        _ => None,
    }
}

// 設定ファイルに書かれた文字列から保存形式を決める。
// 解釈できない場合は None を返す。
fn screenshot_format_from_str(raw: &str) -> Option<ScreenshotFormat> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "jpeg" | "jpg" => Some(ScreenshotFormat::Jpeg),
        "png" => Some(ScreenshotFormat::Png),
        _ => None,
    }
}

// スクリーンショットの保存先の既定値。
//
// デスクトップ → %USERPROFILE% → 実行ファイルの置き場所 → 一時フォルダ の順に倒す。
// **カレントディレクトリは使わない。** どこから起動したかで保存先が変わるうえ、
// ショートカットやタスクスケジューラから起動すると C:\Windows\System32 のような
// 書き込めない場所を指しうる。保存に失敗する理由が設定画面から見て分からない。
fn default_screenshot_folder() -> PathBuf {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));

    screenshot_folder_from(
        dirs::desktop_dir(),
        dirs::home_dir(),
        exe_dir,
        std::env::temp_dir(),
    )
}

// 保存先の候補から実際に使うものを選ぶ。
//
// 候補の取得は環境に依存するため、選ぶ部分だけを切り出してテストする。
// last_resort は常に値がある候補（一時フォルダ）を想定している。
fn screenshot_folder_from(
    desktop: Option<PathBuf>,
    home: Option<PathBuf>,
    exe_dir: Option<PathBuf>,
    last_resort: PathBuf,
) -> PathBuf {
    if let Some(desktop) = desktop {
        return desktop;
    }

    if let Some(home) = home {
        warn!(
            "デスクトップの場所が分からないので、スクリーンショットの保存先を {} にする",
            home.display()
        );
        return home;
    }

    if let Some(exe_dir) = exe_dir {
        warn!(
            "ユーザーフォルダの場所も分からないので、スクリーンショットの保存先を実行ファイルの場所 {} にする",
            exe_dir.display()
        );
        return exe_dir;
    }

    warn!(
        "実行ファイルの場所も分からないので、スクリーンショットの保存先を一時フォルダ {} にする",
        last_resort.display()
    );
    last_resort
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiSettings {
    #[serde(deserialize_with = "deserialize_volume")]
    pub volume: f32,
    // ミュート中か。**音量とは別に持つ。** 音量 0% で代用すると、解除したときに
    // 戻すべき値が残らない。設定ダイアログには出さず、右クリックメニュー・
    // ミドルクリック・ホットキーだけで切り替える
    pub muted: bool,
    pub maintain_aspect_ratio: bool,
    pub last_window_size: Option<(f32, f32)>,
    pub last_window_pos: Option<(f32, f32)>,
    pub always_on_top: bool,
    pub enable_drag_move: bool,
    // 映像の上に FPS などの統計を重ねて出すか
    pub show_stats_overlay: bool,
    // タイトルバーと枠を消すか。デュアルモニタでサブウィンドウとして置くときに
    // 装飾が邪魔になるため。設定ダイアログには出さず、右クリックメニューだけで
    // 切り替える。**有効にすると × が無くなる** ので、右クリックメニューの
    // 「終了」と Alt+F4 が閉じる手段になる
    pub borderless: bool,
}

impl Default for VideoSettings {
    fn default() -> Self {
        Self {
            device_name: None,
            resolution: Some((1280, 720)),    // 720pで安定性を優先
            format: Some("YUY2".to_string()), // YUY2フォーマット
            fps: Some(60),                    // 60fps目標
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

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            input_device_name: None,
            output_device_name: None,
            sample_rate: Some(DEFAULT_SAMPLE_RATE),
            channels: Some(DEFAULT_CHANNELS),
            passthrough_enabled: true,
            // 既定は 50ms。この値が設定項目になる前にハードコードされていた
            // 長さと同じで、更新しても音の出かたが変わらない
            audio_buffer_ms: DEFAULT_AUDIO_BUFFER_MS,
        }
    }
}

impl Default for ScreenshotSettings {
    fn default() -> Self {
        Self {
            // 既定はファイルへの保存だけ。これまでの挙動をそのまま既定にする
            destination: ScreenshotDestination::File,
            save_folder: default_screenshot_folder(),
            format: ScreenshotFormat::Jpeg,
            // image クレートの save() は JpegEncoder::new を通るため、
            // これまでの保存は品質 75 固定だった。ゲーム画面のように
            // 文字や細い線が多い画には 75 では圧縮の跡が見えるので、
            // 既定をひとつ上の 90 にしてある。ファイルは 75 のおよそ 2 倍に
            // なるが、それでも PNG よりはずっと小さい。
            // 品質を気にしない用途は既定のまま、跡を残したくない用途は
            // PNG を選ぶ、という切り分けにする
            jpeg_quality: 90,
            // 相対パスのまま既定値にしてある。既存ユーザーの設定ファイルにも
            // この値が保存されているため、変えると移行の前提が崩れる。
            // 解決は screenshot::resolve_sound_path が exe の置き場所を基準に行い、
            // 見つからなければ埋め込みの既定音へ倒す。
            // None は「効果音を鳴らさない」の意味なので、既定値には使えない
            sound_file: Some(PathBuf::from("sound/SS.mp3")),
            sound_volume: 100.0,
            // 既定は「旧版の項目が無い」。既定のホットキーは
            // default_hotkeys() が持つ
            legacy_hotkey: None,
        }
    }
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            volume: DEFAULT_VOLUME,
            // 既定は音が出る状態にする
            muted: false,
            maintain_aspect_ratio: true,
            last_window_size: None,
            last_window_pos: None,
            always_on_top: false,
            enable_drag_move: true,
            // 常時出しているものではないので、既定は非表示にする
            show_stats_overlay: false,
            // 既定はタイトルバーありにする。装飾なしは閉じ方・動かし方が
            // 通常のウィンドウと変わるので、知らずにその状態で起動させない
            borderless: false,
        }
    }
}

// 設定ファイルをどう読めたか。起動時に既定値を書き戻してよいかの判断に使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadOutcome {
    // 読み込めた。初回起動で confy が既定値のファイルを作った場合も含む
    Loaded,
    // 読み込めなかったので既定値で起動した。読めなかったファイルは退避済みか、
    // そもそも存在しなかった。どちらもディスクに壊れたファイルは残っていない
    FellBackToDefaults,
    // 読み込めず、退避もできなかった。読めなかったファイルがそのまま残っている
    BrokenFileLeftBehind,
}

impl LoadOutcome {
    // 起動時に既定値を設定ファイルへ書き戻してよいか。
    //
    // 退避できなかった場合だけ false になる。読めなかったファイルがディスクに
    // 残っているため、ここで書き戻すとユーザーが設定を取り戻す最後の手段が消える。
    // 書き戻さなければ壊れたファイルは手元に残り、次回以降も退避を試みられる。
    pub fn may_write_defaults_on_startup(self) -> bool {
        !matches!(self, LoadOutcome::BrokenFileLeftBehind)
    }
}

// 設定の自動保存（デバウンス保存と終了時保存）を許してよいかを持つ。
//
// 読めなかった設定ファイルを退避できなかった場合、ディスクには壊れたファイルが
// そのまま残っている。起動時の書き戻しだけを止めても、ウィンドウを動かせば
// 2 秒後のデバウンス保存が、何もしなくても終了時の保存が、同じファイルを
// 既定値で上書きしてしまう。そのため壊れたファイルが残っている間は
// 自動保存そのものを止める。
//
// 止めている間はウィンドウの位置・サイズや音量も永続化されない。設定を
// 取り戻す手段を残すほうを優先する、という判断。
//
// 設定ダイアログの「適用」「OK」による保存はユーザーの明示的な操作なので
// 止めない。それが成功した時点で壊れたファイルはユーザーの意思で置き換わって
// いるため、以降の自動保存も解禁する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoSavePolicy {
    allowed: bool,
}

impl AutoSavePolicy {
    // 設定の読み込み結果から初期状態を決める。
    pub fn from_load_outcome(outcome: LoadOutcome) -> Self {
        Self {
            allowed: !matches!(outcome, LoadOutcome::BrokenFileLeftBehind),
        }
    }

    // 自動保存してよいか。
    pub fn is_allowed(self) -> bool {
        self.allowed
    }

    // 明示的な保存操作の結果を反映する。`saved` は実際に書き出せたか。
    //
    // 失敗した場合に解禁しないのは、壊れたファイルがまだ残っているため。
    // 解禁してしまうと、次のウィンドウ操作で自動保存が走って上書きしうる。
    pub fn note_explicit_save(&mut self, saved: bool) {
        if saved {
            self.allowed = true;
        }
    }
}

// 退避の結果から読み込み結果を決める。
//
// load() 自体は confy が %AppData% を直接読み書きするためテストできない。
// 判断の部分だけをこの関数に切り出して、退避が失敗した場合を含めて検証する。
fn outcome_from_backup(backup: std::io::Result<Option<PathBuf>>) -> LoadOutcome {
    match backup {
        Ok(_) => LoadOutcome::FellBackToDefaults,
        Err(_) => LoadOutcome::BrokenFileLeftBehind,
    }
}

// 読み込めなかった設定ファイルを退避する。
// 退避できた場合は退避先のパスを返す。元のファイルが無い場合は None を返す。
//
// コピーではなく rename にしているのは、退避したあとに既定値が書き戻されて
// 元のファイルが上書きされ、内容が失われるのを避けるため。
fn backup_broken_config(path: &Path) -> std::io::Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }

    let backup_path = next_backup_path(path);
    std::fs::rename(path, &backup_path)?;
    Ok(Some(backup_path))
}

// 退避先のパスを決める。<元のファイル名>.bak を基本とし、
// 既に存在する場合は .bak.1、.bak.2 と連番を足して過去の退避を上書きしない。
fn next_backup_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();

    let mut candidate = path.with_file_name(format!("{}.bak", file_name));
    let mut counter = 1;
    while candidate.exists() {
        candidate = path.with_file_name(format!("{}.bak.{}", file_name, counter));
        counter += 1;
    }

    candidate
}

impl AppSettings {
    // 設定と、その読み込み結果を返す。
    //
    // 結果を返しているのは、起動時に既定値を書き戻してよいかを呼び出し側が
    // 判断できるようにするため。退避に失敗したまま書き戻すと、読めなかった
    // ファイルを既定値で上書きしてしまい、証跡ごと消える。
    pub fn load() -> (Self, LoadOutcome) {
        match confy::load(APP_NAME, None) {
            Ok(settings) => (settings, LoadOutcome::Loaded),
            Err(e) => {
                error!("設定ファイルを読み込めないため既定値で起動する: {}", e);

                // 読み込みに失敗した設定ファイルは、既定値で起動する前に退避する。
                // 黙って上書きすると、ユーザーが自分の設定を取り戻す手段が無くなる。
                //
                // 失敗したという事実は LoadOutcome として呼び出し側へ渡し、
                // 理由はログに残す。ここは起動直後で UI がまだ無いため、
                // ユーザーへ伝える手段がログしかない。
                let outcome = match confy::get_configuration_file_path(APP_NAME, None) {
                    Ok(path) => {
                        let backup = backup_broken_config(&path);
                        match &backup {
                            Ok(Some(backup_path)) => warn!(
                                "読み込めなかった設定ファイルを {} へ退避した",
                                backup_path.display()
                            ),
                            // 元のファイルが無い。退避するものが無いだけなので何も言わない
                            Ok(None) => {}
                            Err(e) => error!(
                                "読み込めなかった設定ファイル {} を退避できない: {}",
                                path.display(),
                                e
                            ),
                        }
                        outcome_from_backup(backup)
                    }
                    // 設定ファイルの置き場所が分からず、退避を試みることすらできない。
                    // 読めなかったファイルが残っている可能性があるため、
                    // 書き戻さない側に倒す。
                    Err(e) => {
                        error!("設定ファイルの置き場所が分からず退避できない: {}", e);
                        LoadOutcome::BrokenFileLeftBehind
                    }
                };
                (Self::default(), outcome)
            }
        }
    }

    // 保存できたかを返す。
    //
    // 結果を捨てないのは、デバウンスして書き出す側が失敗を検知して
    // 再試行できるようにするため。失敗を握り潰すと、書けなかった変更が
    // 保存済みとして扱われて消える。
    pub fn save(&self) -> bool {
        match confy::store(APP_NAME, None, self) {
            Ok(()) => true,
            Err(e) => {
                error!("設定の保存に失敗した: {}", e);
                false
            }
        }
    }

    // アクションに割り当てられたホットキー。未割り当てなら None。
    pub fn hotkey(&self, action: HotkeyAction) -> Option<&str> {
        self.hotkeys.get(&action).map(String::as_str)
    }

    // アクションのホットキーを差し替える。`None` は割り当ての解除。
    //
    // 解除をキーの削除で表すのは、空文字と「未割り当て」を混ぜないため。
    // 空文字を入れるとパースに失敗して、毎回ログへ理由が出ることになる。
    pub fn set_hotkey(&mut self, action: HotkeyAction, hotkey: Option<String>) {
        match hotkey {
            Some(hotkey) => {
                self.hotkeys.insert(action, hotkey);
            }
            None => {
                self.hotkeys.remove(&action);
            }
        }
    }

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

    pub fn get_screenshot_path(&self, timestamp: &str) -> PathBuf {
        let extension = self.screenshot.format.extension();
        let mut path = self.screenshot.save_folder.clone();
        path.push(format!("{}.{}", timestamp, extension));

        // ファイル名の競合を処理
        let mut counter = 1;
        while path.exists() {
            let stem = format!("{}({})", timestamp, counter);
            path.set_file_name(format!("{}.{}", stem, extension));
            counter += 1;
        }

        path
    }
}

// 書き出す設定ファイルの既定のファイル名。
//
// 日付を入れるのは、同じフォルダへ何度も書き出したときに前回のものを
// 黙って上書きしないため。同じ日に 2 度書き出した場合は、保存ダイアログが
// 上書きの確認を出す。
//
// 時刻を入れないのは、不具合報告に添える用途で名前が長くなりすぎるため。
// 日が変わらないうちの 2 度目は、ユーザーが名前を変えればよい。
pub fn export_file_name(date: &impl Datelike) -> String {
    format!(
        "{}-settings-{:04}{:02}{:02}.toml",
        APP_NAME,
        date.year(),
        date.month(),
        date.day()
    )
}

// 設定を、指定した場所へ TOML として書き出す。
//
// **confy の `store_path` をそのまま使う。** 書式を `%AppData%` の設定ファイルと
// 揃えたいためで、ここだけ別の toml 実装で書くと、confy が書式を変えたときに
// 書き出したファイルを読み戻せない組み合わせが生まれる。
pub fn export_to(path: &Path, settings: &AppSettings) -> Result<(), String> {
    confy::store_path(path, settings).map_err(|e| e.to_string())
}

// 書き出した設定ファイルを読む。
//
// 読めた場合は `AppSettings` へのパースを通っているので、知らない値は
// 既定へ倒れ、旧版のホットキーも移行済みになっている（`RawAppSettings`）。
//
// **`confy::load_path` はファイルが無いと既定値で新しく作る。** 読み込みの
// つもりで呼んだ結果、選んだ場所に既定値のファイルが増えるのは意図と違うので、
// 先に存在を確かめてから渡す。
pub fn import_from(path: &Path) -> Result<AppSettings, String> {
    if !path.is_file() {
        return Err(format!("{} が見つからない", path.display()));
    }
    confy::load_path(path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use std::fs;
    use tempfile::tempdir;

    // 全項目を明示した設定ファイル。値はすべて既定値と異なるものにしてある。
    // 各テストはここから一部を削り、「古い版が書いた設定ファイル」を再現する。
    const FULL_CONFIG: &str = r#"
[video]
device_name = "Capture Device"
resolution = [1920, 1080]
format = "MJPEG"
fps = 30
auto_reconnect = false
color_space = "bt601"
color_range = "full"
brightness = 10
contrast = -20
saturation = 30

[audio]
input_device_name = "Line In"
output_device_name = "Speakers"
sample_rate = 44100
channels = 1
passthrough_enabled = false
audio_buffer_ms = 120

[screenshot]
destination = "both"
save_folder = 'C:\shots'
format = "png"
jpeg_quality = 60
sound_file = 'sound/custom.mp3'
sound_volume = 50.0

[ui]
volume = 80.0
muted = true
maintain_aspect_ratio = false
last_window_size = [800.0, 600.0]
last_window_pos = [10.0, 20.0]
always_on_top = true
enable_drag_move = false
show_stats_overlay = true
borderless = true

[hotkeys]
screenshot = "Ctrl+S"
toggle_fullscreen = "F11"
"#;

    // ホットキーをアクション別にする前の版が書いた設定ファイル。
    // [hotkeys] が無く、screenshot セクションに hotkey がある。
    const LEGACY_CONFIG: &str = r#"
[video]
device_name = "Capture Device"
fps = 30

[audio]
sample_rate = 44100

[screenshot]
save_folder = 'C:\shots'
sound_volume = 50.0
hotkey = "Ctrl+S"

[ui]
volume = 80.0
"#;

    // 指定したキーの行を取り除く。項目を 1 つ追加した直後の、
    // そのキーだけが存在しない設定ファイルを作るために使う。
    fn without_key(config: &str, key: &str) -> String {
        let prefix = format!("{} =", key);
        config
            .lines()
            .filter(|line| !line.trim_start().starts_with(&prefix))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn app_settings_missing_one_key_keeps_other_values() {
        // 項目を 1 つ足してリリースした直後に起きる状況。
        // 欠けたキーだけが既定値になり、他の値は保持されなければならない。
        let config = without_key(FULL_CONFIG, "fps");
        assert!(
            !config.contains("fps ="),
            "テスト用の設定から fps が消えていない"
        );

        let settings: AppSettings =
            toml::from_str(&config).expect("fps が欠けていても読めなければならない");

        assert_eq!(settings.video.fps, Some(60)); // 欠けた項目だけ既定値
        assert_eq!(
            settings.video.device_name,
            Some("Capture Device".to_string())
        );
        assert_eq!(settings.video.resolution, Some((1920, 1080)));
        assert_eq!(settings.video.format, Some("MJPEG".to_string()));
        assert_eq!(settings.audio.sample_rate, Some(44100));
        assert_eq!(settings.hotkey(HotkeyAction::Screenshot), Some("Ctrl+S"));
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn app_settings_missing_bool_key_keeps_other_values() {
        // bool のように「値が無い＝false」と誤解されやすい型でも、
        // 欠けたときは Default の値（true）に戻ることを確かめる。
        let config = without_key(FULL_CONFIG, "enable_drag_move");
        assert!(
            !config.contains("enable_drag_move ="),
            "テスト用の設定から enable_drag_move が消えていない"
        );

        let settings: AppSettings =
            toml::from_str(&config).expect("enable_drag_move が欠けていても読めなければならない");

        assert!(settings.ui.enable_drag_move); // 既定値は true
        assert!(settings.ui.always_on_top);
        assert!(!settings.ui.maintain_aspect_ratio);
        assert_eq!(settings.ui.last_window_size, Some((800.0, 600.0)));
    }

    #[test]
    fn app_settings_missing_muted_defaults_to_unmuted() {
        // ミュートの項目を足した版へ上げた直後、既存ユーザーの設定ファイルには
        // このキーが無い。欠けていても他の項目が保持され、音が出る状態で起動すること
        let config = without_key(FULL_CONFIG, "muted");
        assert!(
            !config.contains("muted ="),
            "テスト用の設定から muted が消えていない"
        );

        let settings: AppSettings =
            toml::from_str(&config).expect("muted が欠けていても読めなければならない");

        assert!(!settings.ui.muted); // 既定値は false
        assert_eq!(settings.ui.volume, 80.0);
        assert!(settings.ui.always_on_top);
    }

    #[test]
    fn app_settings_muted_is_read_and_kept_apart_from_volume() {
        // ミュートは音量とは別の項目。読み込みで音量へ潰れてはいけない
        let settings: AppSettings =
            toml::from_str(FULL_CONFIG).expect("設定ファイルを読めなければならない");

        assert!(settings.ui.muted);
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn app_settings_missing_show_stats_overlay_defaults_to_hidden() {
        // 情報表示の項目を足した版へ上げた直後、既存ユーザーの設定ファイルには
        // このキーが無い。欠けていても他の項目が保持され、既定の非表示になること。
        let config = without_key(FULL_CONFIG, "show_stats_overlay");
        assert!(
            !config.contains("show_stats_overlay ="),
            "テスト用の設定から show_stats_overlay が消えていない"
        );

        let settings: AppSettings =
            toml::from_str(&config).expect("show_stats_overlay が欠けていても読めなければならない");

        assert!(!settings.ui.show_stats_overlay); // 既定値は false
        assert_eq!(settings.ui.volume, 80.0);
        assert!(settings.ui.always_on_top);
        assert!(!settings.ui.enable_drag_move);
    }

    #[test]
    fn app_settings_missing_borderless_defaults_to_decorated() {
        // 装飾なしの項目を足した版へ上げた直後、既存ユーザーの設定ファイルには
        // このキーが無い。欠けていても他の項目が保持され、タイトルバーありで起動すること。
        // **既定が true になると、更新しただけで × が消えたウィンドウが出てくる。**
        let config = without_key(FULL_CONFIG, "borderless");
        assert!(
            !config.contains("borderless ="),
            "テスト用の設定から borderless が消えていない"
        );

        let settings: AppSettings =
            toml::from_str(&config).expect("borderless が欠けていても読めなければならない");

        assert!(!settings.ui.borderless); // 既定値は false
        assert_eq!(settings.ui.volume, 80.0);
        assert!(settings.ui.always_on_top);
        assert!(settings.ui.show_stats_overlay);
    }

    #[test]
    fn app_settings_borderless_is_read_from_the_file() {
        let settings: AppSettings =
            toml::from_str(FULL_CONFIG).expect("設定ファイルを読めなければならない");

        assert!(settings.ui.borderless);
    }

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
    fn app_settings_missing_section_keeps_other_sections() {
        // 設定の構造体をまるごと 1 つ足した状況。
        // セクションごと存在しなくても、他のセクションは読めなければならない。
        let config = FULL_CONFIG
            .split("[ui]")
            .next()
            .expect("FULL_CONFIG に [ui] セクションがある")
            .to_string();
        assert!(
            !config.contains("[ui]"),
            "テスト用の設定から [ui] が消えていない"
        );

        let settings: AppSettings =
            toml::from_str(&config).expect("[ui] セクションが欠けていても読めなければならない");

        assert_eq!(settings.ui.volume, 100.0); // UiSettings ごと既定値
        assert!(settings.ui.maintain_aspect_ratio);
        assert_eq!(settings.ui.last_window_size, None);
        assert_eq!(
            settings.video.device_name,
            Some("Capture Device".to_string())
        );
        assert_eq!(settings.audio.channels, Some(1));
    }

    #[test]
    fn app_settings_empty_config_uses_all_defaults() {
        // 設定ファイルが空でも既定値で起動できること。
        let settings: AppSettings = toml::from_str("").expect("空の設定でも読めなければならない");

        assert_eq!(settings.video.device_name, None);
        assert_eq!(settings.video.resolution, Some((1280, 720)));
        assert_eq!(settings.video.format, Some("YUY2".to_string()));
        assert_eq!(settings.video.fps, Some(60));
        assert_eq!(settings.audio.sample_rate, Some(48000));
        assert_eq!(settings.audio.channels, Some(2));
        assert!(settings.audio.passthrough_enabled);
        assert_eq!(settings.screenshot.sound_volume, 100.0);
        assert_eq!(settings.screenshot.format, ScreenshotFormat::Jpeg);
        assert_eq!(settings.screenshot.jpeg_quality, 90);
        // 既存ユーザーの設定にも保存されている値。screenshot::resolve_sound_path が
        // exe の置き場所を基準に解決する前提になっている
        assert_eq!(
            settings.screenshot.sound_file,
            Some(PathBuf::from("sound/SS.mp3"))
        );
        assert_eq!(settings.hotkey(HotkeyAction::Screenshot), Some("F5"));
        assert_eq!(settings.ui.volume, 100.0);
        assert!(settings.ui.maintain_aspect_ratio);
        assert!(!settings.ui.always_on_top);
        assert!(settings.ui.enable_drag_move);
    }

    #[test]
    fn app_settings_volume_above_maximum_is_clamped() {
        // 手で書き換えた設定ファイル。UI からは作れない値でも読めてしまうので、
        // 表示と実際の音量が食い違わないよう上限で止める
        let settings: AppSettings =
            toml::from_str("[ui]\nvolume = 1000.0\n").expect("範囲外でも読めなければならない");

        assert_eq!(settings.ui.volume, 200.0);
    }

    #[test]
    fn app_settings_volume_below_minimum_is_clamped() {
        let settings: AppSettings =
            toml::from_str("[ui]\nvolume = -50.0\n").expect("範囲外でも読めなければならない");

        assert_eq!(settings.ui.volume, 0.0);
    }

    #[test]
    fn app_settings_volume_not_a_number_falls_back_to_default() {
        // TOML は nan / inf をそのまま書ける。clamp では落とせないので
        // 既定値へ倒していること
        let settings: AppSettings =
            toml::from_str("[ui]\nvolume = nan\n").expect("nan でも読めなければならない");
        assert_eq!(settings.ui.volume, 100.0);

        let settings: AppSettings =
            toml::from_str("[ui]\nvolume = inf\n").expect("inf でも読めなければならない");
        assert_eq!(settings.ui.volume, 100.0);
    }

    #[test]
    fn app_settings_volume_at_bounds_is_kept() {
        // 境界。丸めが 1 段ずれて端の値が使えなくなっていないこと
        let settings: AppSettings =
            toml::from_str("[ui]\nvolume = 200.0\n").expect("上限が読めなければならない");
        assert_eq!(settings.ui.volume, 200.0);

        let settings: AppSettings =
            toml::from_str("[ui]\nvolume = 0.0\n").expect("下限が読めなければならない");
        assert_eq!(settings.ui.volume, 0.0);
    }

    #[test]
    fn app_settings_unknown_key_is_ignored() {
        // 新しい版で増えた項目が残った設定ファイルを、古い版で読む場合。
        // 知らないキーで失敗せず、既知の項目が保持されなければならない。
        // [hotkeys] は値が文字列でなければ読めないため、末尾ではなく
        // [ui] の中へ入れる
        let config = FULL_CONFIG.replace(
            "show_stats_overlay = true",
            "show_stats_overlay = true\nfuture_option = true",
        );
        assert!(config.contains("future_option = true"));

        let settings: AppSettings =
            toml::from_str(&config).expect("知らないキーがあっても読めなければならない");

        assert_eq!(settings.ui.volume, 80.0);
        assert!(!settings.ui.enable_drag_move);
    }

    #[test]
    fn app_settings_roundtrip_preserves_all_values() {
        // 書き出して読み直したときに全項目が保たれること。
        let original: AppSettings =
            toml::from_str(FULL_CONFIG).expect("FULL_CONFIG が読めなければならない");
        let serialized = toml::to_string(&original).expect("設定を書き出せなければならない");
        let restored: AppSettings =
            toml::from_str(&serialized).expect("書き出した設定を読み直せなければならない");

        assert_eq!(
            restored.video.device_name,
            Some("Capture Device".to_string())
        );
        assert_eq!(restored.video.resolution, Some((1920, 1080)));
        assert_eq!(restored.video.format, Some("MJPEG".to_string()));
        assert_eq!(restored.video.fps, Some(30));
        assert_eq!(
            restored.audio.input_device_name,
            Some("Line In".to_string())
        );
        assert_eq!(
            restored.audio.output_device_name,
            Some("Speakers".to_string())
        );
        assert_eq!(restored.audio.sample_rate, Some(44100));
        assert_eq!(restored.audio.channels, Some(1));
        assert!(!restored.audio.passthrough_enabled);
        assert_eq!(restored.screenshot.destination, ScreenshotDestination::Both);
        assert_eq!(restored.screenshot.save_folder, PathBuf::from(r"C:\shots"));
        assert_eq!(restored.screenshot.format, ScreenshotFormat::Png);
        assert_eq!(restored.screenshot.jpeg_quality, 60);
        assert_eq!(
            restored.screenshot.sound_file,
            Some(PathBuf::from("sound/custom.mp3"))
        );
        assert_eq!(restored.screenshot.sound_volume, 50.0);
        assert_eq!(restored.hotkey(HotkeyAction::Screenshot), Some("Ctrl+S"));
        assert_eq!(restored.hotkey(HotkeyAction::ToggleFullscreen), Some("F11"));
        assert_eq!(restored.ui.volume, 80.0);
        assert!(!restored.ui.maintain_aspect_ratio);
        assert_eq!(restored.ui.last_window_size, Some((800.0, 600.0)));
        assert_eq!(restored.ui.last_window_pos, Some((10.0, 20.0)));
        assert!(restored.ui.always_on_top);
        assert!(!restored.ui.enable_drag_move);
    }

    #[test]
    fn backup_broken_config_moves_file_and_returns_path() {
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("default-config.toml");
        fs::write(&path, "[video] 壊れている").expect("テスト用の設定を書けること");

        let backup = backup_broken_config(&path)
            .expect("退避に成功すること")
            .expect("退避先のパスが返ること");

        assert_eq!(backup, dir.path().join("default-config.toml.bak"));
        assert!(!path.exists(), "退避後に元のファイルが残っている");
        assert_eq!(
            fs::read_to_string(&backup).expect("退避先を読めること"),
            "[video] 壊れている"
        );
    }

    #[test]
    fn backup_broken_config_existing_backup_gets_numbered_suffix() {
        // 続けて壊れた場合に、前回退避した内容を上書きしないこと。
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("default-config.toml");

        fs::write(&path, "1 回目").expect("テスト用の設定を書けること");
        let first = backup_broken_config(&path).unwrap().unwrap();
        fs::write(&path, "2 回目").expect("テスト用の設定を書けること");
        let second = backup_broken_config(&path).unwrap().unwrap();
        fs::write(&path, "3 回目").expect("テスト用の設定を書けること");
        let third = backup_broken_config(&path).unwrap().unwrap();

        assert_eq!(first, dir.path().join("default-config.toml.bak"));
        assert_eq!(second, dir.path().join("default-config.toml.bak.1"));
        assert_eq!(third, dir.path().join("default-config.toml.bak.2"));
        assert_eq!(fs::read_to_string(&first).unwrap(), "1 回目");
        assert_eq!(fs::read_to_string(&second).unwrap(), "2 回目");
        assert_eq!(fs::read_to_string(&third).unwrap(), "3 回目");
    }

    #[test]
    fn outcome_from_backup_backup_failed_forbids_writing_defaults() {
        // 退避に失敗した場合。読めなかったファイルがディスクに残っているため、
        // 起動時に既定値を書き戻してはならない。書き戻すとユーザーが設定を
        // 取り戻す最後の手段が消える。
        let failed = Err(std::io::Error::other("退避に失敗した"));

        let outcome = outcome_from_backup(failed);

        assert_eq!(outcome, LoadOutcome::BrokenFileLeftBehind);
        assert!(!outcome.may_write_defaults_on_startup());
    }

    #[test]
    fn outcome_from_backup_backup_succeeded_allows_writing_defaults() {
        // 退避できた場合。元のファイルは .bak として残っているため、
        // 既定値を書き戻してよい。
        let backed_up = Ok(Some(PathBuf::from("default-config.toml.bak")));

        let outcome = outcome_from_backup(backed_up);

        assert_eq!(outcome, LoadOutcome::FellBackToDefaults);
        assert!(outcome.may_write_defaults_on_startup());
    }

    #[test]
    fn outcome_from_backup_nothing_to_back_up_allows_writing_defaults() {
        // 退避するファイルがそもそも無かった場合。
        // 潰す相手がいないので、既定値を書き戻してよい。
        let nothing = Ok(None);

        let outcome = outcome_from_backup(nothing);

        assert_eq!(outcome, LoadOutcome::FellBackToDefaults);
        assert!(outcome.may_write_defaults_on_startup());
    }

    // 保存先の候補。実在しないパスでよい。screenshot_folder_from は
    // 候補の存在を確かめず、取れた順に選ぶだけ
    const DESKTOP: &str = r"C:\Users\tester\Desktop";
    const HOME: &str = r"C:\Users\tester";
    const EXE_DIR: &str = r"C:\Program Files\capturecard_viewer";
    const TEMP: &str = r"C:\Users\tester\AppData\Local\Temp";

    #[test]
    fn screenshot_folder_from_desktop_available_uses_desktop() {
        let folder = screenshot_folder_from(
            Some(PathBuf::from(DESKTOP)),
            Some(PathBuf::from(HOME)),
            Some(PathBuf::from(EXE_DIR)),
            PathBuf::from(TEMP),
        );

        assert_eq!(folder, PathBuf::from(DESKTOP));
    }

    #[test]
    fn screenshot_folder_from_no_desktop_falls_back_to_home() {
        // デスクトップをリダイレクトしている環境などで desktop_dir() が None になる場合
        let folder = screenshot_folder_from(
            None,
            Some(PathBuf::from(HOME)),
            Some(PathBuf::from(EXE_DIR)),
            PathBuf::from(TEMP),
        );

        assert_eq!(folder, PathBuf::from(HOME));
    }

    #[test]
    fn screenshot_folder_from_no_user_folders_falls_back_to_exe_dir() {
        let folder = screenshot_folder_from(
            None,
            None,
            Some(PathBuf::from(EXE_DIR)),
            PathBuf::from(TEMP),
        );

        assert_eq!(folder, PathBuf::from(EXE_DIR));
    }

    #[test]
    fn screenshot_folder_from_nothing_available_falls_back_to_last_resort() {
        let folder = screenshot_folder_from(None, None, None, PathBuf::from(TEMP));

        assert_eq!(folder, PathBuf::from(TEMP));
    }

    #[test]
    fn screenshot_folder_from_never_returns_current_dir() {
        // 修正前の挙動の再現防止。どの候補も取れなくてもカレントディレクトリを
        // 指さないこと。相対パスだと起動元によって保存先が変わる
        let folder = screenshot_folder_from(None, None, None, PathBuf::from(TEMP));

        assert_ne!(folder, PathBuf::from("."));
        assert!(folder.is_absolute(), "保存先の既定値は絶対パスであること");
    }

    #[test]
    fn auto_save_policy_broken_file_left_behind_blocks_autosave() {
        // 退避できなかった場合。ウィンドウを動かすか終了するだけで
        // 壊れたファイルが既定値で潰れるのを防ぐため、自動保存を止める
        let policy = AutoSavePolicy::from_load_outcome(LoadOutcome::BrokenFileLeftBehind);

        assert!(!policy.is_allowed());
    }

    #[test]
    fn auto_save_policy_loaded_allows_autosave() {
        assert!(AutoSavePolicy::from_load_outcome(LoadOutcome::Loaded).is_allowed());
    }

    #[test]
    fn auto_save_policy_fell_back_to_defaults_allows_autosave() {
        // 退避できていれば元の内容は .bak に残っている。守る相手がいないので
        // ウィンドウ位置や音量を通常どおり保存してよい
        assert!(AutoSavePolicy::from_load_outcome(LoadOutcome::FellBackToDefaults).is_allowed());
    }

    #[test]
    fn auto_save_policy_successful_explicit_save_unblocks_autosave() {
        // 設定画面の「適用」「OK」で保存できた時点で、壊れたファイルは
        // ユーザーの意思で置き換わっている。以降は自動保存を止めない
        let mut policy = AutoSavePolicy::from_load_outcome(LoadOutcome::BrokenFileLeftBehind);

        policy.note_explicit_save(true);

        assert!(policy.is_allowed());
    }

    #[test]
    fn auto_save_policy_failed_explicit_save_keeps_autosave_blocked() {
        // 保存に失敗した場合は壊れたファイルがまだ残っているため、
        // 止めたままにする
        let mut policy = AutoSavePolicy::from_load_outcome(LoadOutcome::BrokenFileLeftBehind);

        policy.note_explicit_save(false);

        assert!(!policy.is_allowed());
    }

    #[test]
    fn auto_save_policy_failed_explicit_save_does_not_block_allowed_policy() {
        // もともと許可されている状態は、保存の失敗で止まらない。
        // 一時的な書き込み失敗で以降の保存が全部止まると、
        // 復旧したあとも設定が残らなくなる
        let mut policy = AutoSavePolicy::from_load_outcome(LoadOutcome::Loaded);

        policy.note_explicit_save(false);

        assert!(policy.is_allowed());
    }

    #[test]
    fn load_outcome_loaded_allows_writing_defaults() {
        // 正常に読めた場合。通常どおり保存してよい。
        assert!(LoadOutcome::Loaded.may_write_defaults_on_startup());
    }

    // get_screenshot_path は save_folder しか見ないため、
    // 一時ディレクトリを指した設定を組み立てれば %AppData% にもデスクトップにも触れない。
    fn settings_saving_into(dir: &Path) -> AppSettings {
        let mut settings = AppSettings::default();
        settings.screenshot.save_folder = dir.to_path_buf();
        settings
    }

    #[test]
    fn get_screenshot_path_no_conflict_uses_timestamp_as_is() {
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let settings = settings_saving_into(dir.path());

        let path = settings.get_screenshot_path("2026-09-19_12-00-00-000");

        assert_eq!(path, dir.path().join("2026-09-19_12-00-00-000.jpg"));
    }

    #[test]
    fn get_screenshot_path_one_conflict_appends_1() {
        let dir = tempdir().expect("一時ディレクトリを作れること");
        fs::write(dir.path().join("2026-09-19_12-00-00-000.jpg"), b"")
            .expect("先客のファイルを置けること");
        let settings = settings_saving_into(dir.path());

        let path = settings.get_screenshot_path("2026-09-19_12-00-00-000");

        assert_eq!(path, dir.path().join("2026-09-19_12-00-00-000(1).jpg"));
    }

    #[test]
    fn get_screenshot_path_two_conflicts_appends_2() {
        // 連番付きのファイルも競合の判定に含めること。
        // 「(1) を作ったら (1) を上書きした」を防ぐための確認
        let dir = tempdir().expect("一時ディレクトリを作れること");
        for name in [
            "2026-09-19_12-00-00-000.jpg",
            "2026-09-19_12-00-00-000(1).jpg",
        ] {
            fs::write(dir.path().join(name), b"").expect("先客のファイルを置けること");
        }
        let settings = settings_saving_into(dir.path());

        let path = settings.get_screenshot_path("2026-09-19_12-00-00-000");

        assert_eq!(path, dir.path().join("2026-09-19_12-00-00-000(2).jpg"));
    }

    #[test]
    fn get_screenshot_path_three_conflicts_appends_3() {
        let dir = tempdir().expect("一時ディレクトリを作れること");
        for name in [
            "2026-09-19_12-00-00-000.jpg",
            "2026-09-19_12-00-00-000(1).jpg",
            "2026-09-19_12-00-00-000(2).jpg",
        ] {
            fs::write(dir.path().join(name), b"").expect("先客のファイルを置けること");
        }
        let settings = settings_saving_into(dir.path());

        let path = settings.get_screenshot_path("2026-09-19_12-00-00-000");

        assert_eq!(path, dir.path().join("2026-09-19_12-00-00-000(3).jpg"));
    }

    #[test]
    fn get_screenshot_path_gap_in_numbering_fills_the_gap() {
        // (1) だけ消された状態。連番は「空いている最小の番号」であり、
        // 既存の最大値 + 1 ではない
        let dir = tempdir().expect("一時ディレクトリを作れること");
        for name in [
            "2026-09-19_12-00-00-000.jpg",
            "2026-09-19_12-00-00-000(2).jpg",
        ] {
            fs::write(dir.path().join(name), b"").expect("先客のファイルを置けること");
        }
        let settings = settings_saving_into(dir.path());

        let path = settings.get_screenshot_path("2026-09-19_12-00-00-000");

        assert_eq!(path, dir.path().join("2026-09-19_12-00-00-000(1).jpg"));
    }

    #[test]
    fn get_screenshot_path_timestamp_with_dots_keeps_jpg_extension() {
        // タイムスタンプ自体にドットが含まれる場合。連番を付けるときに
        // ドット以降を拡張子と見なして削ってしまうと ".jpg" を失う
        let dir = tempdir().expect("一時ディレクトリを作れること");
        fs::write(dir.path().join("2026.09.19_12.00.00.jpg"), b"")
            .expect("先客のファイルを置けること");
        let settings = settings_saving_into(dir.path());

        let path = settings.get_screenshot_path("2026.09.19_12.00.00");

        assert_eq!(path, dir.path().join("2026.09.19_12.00.00(1).jpg"));
    }

    #[test]
    fn get_screenshot_path_only_numbered_file_exists_uses_timestamp_as_is() {
        // 連番だけがあって本体が無い場合は、連番を付けずに本体の名前を使う
        let dir = tempdir().expect("一時ディレクトリを作れること");
        fs::write(dir.path().join("2026-09-19_12-00-00-000(1).jpg"), b"")
            .expect("先客のファイルを置けること");
        let settings = settings_saving_into(dir.path());

        let path = settings.get_screenshot_path("2026-09-19_12-00-00-000");

        assert_eq!(path, dir.path().join("2026-09-19_12-00-00-000.jpg"));
    }

    #[test]
    fn get_screenshot_path_png_format_uses_png_extension() {
        // 保存形式を PNG にしたら拡張子も追従すること
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let mut settings = settings_saving_into(dir.path());
        settings.screenshot.format = ScreenshotFormat::Png;

        let path = settings.get_screenshot_path("2026-09-19_12-00-00-000");

        assert_eq!(path, dir.path().join("2026-09-19_12-00-00-000.png"));
    }

    #[test]
    fn get_screenshot_path_png_conflict_keeps_png_extension() {
        // 連番を付けるときに拡張子を .jpg へ戻してしまわないこと
        let dir = tempdir().expect("一時ディレクトリを作れること");
        fs::write(dir.path().join("2026-09-19_12-00-00-000.png"), b"")
            .expect("先客のファイルを置けること");
        let mut settings = settings_saving_into(dir.path());
        settings.screenshot.format = ScreenshotFormat::Png;

        let path = settings.get_screenshot_path("2026-09-19_12-00-00-000");

        assert_eq!(path, dir.path().join("2026-09-19_12-00-00-000(1).png"));
    }

    #[test]
    fn get_screenshot_path_png_ignores_jpg_with_same_name() {
        // 形式が違えばファイル名は衝突しない。同名の .jpg があっても
        // .png 側は連番を付けずに撮影時刻そのままを使う
        let dir = tempdir().expect("一時ディレクトリを作れること");
        fs::write(dir.path().join("2026-09-19_12-00-00-000.jpg"), b"")
            .expect("先客のファイルを置けること");
        let mut settings = settings_saving_into(dir.path());
        settings.screenshot.format = ScreenshotFormat::Png;

        let path = settings.get_screenshot_path("2026-09-19_12-00-00-000");

        assert_eq!(path, dir.path().join("2026-09-19_12-00-00-000.png"));
    }

    #[test]
    fn screenshot_format_extension_matches_format() {
        assert_eq!(ScreenshotFormat::Jpeg.extension(), "jpg");
        assert_eq!(ScreenshotFormat::Png.extension(), "png");
    }

    #[test]
    fn encoding_jpeg_passes_quality_through() {
        let mut settings = AppSettings::default();
        settings.screenshot.format = ScreenshotFormat::Jpeg;
        settings.screenshot.jpeg_quality = 55;

        assert_eq!(
            settings.screenshot.encoding(),
            ScreenshotEncoding::Jpeg { quality: 55 }
        );
    }

    #[test]
    fn encoding_jpeg_clamps_quality_into_range() {
        // 設定ファイルを手で書き換えられた場合。エンコーダへ渡す前に丸める
        let mut settings = AppSettings::default();
        settings.screenshot.format = ScreenshotFormat::Jpeg;

        settings.screenshot.jpeg_quality = 0;
        assert_eq!(
            settings.screenshot.encoding(),
            ScreenshotEncoding::Jpeg { quality: 1 }
        );

        settings.screenshot.jpeg_quality = 255;
        assert_eq!(
            settings.screenshot.encoding(),
            ScreenshotEncoding::Jpeg { quality: 100 }
        );
    }

    #[test]
    fn encoding_png_ignores_jpeg_quality() {
        // PNG は可逆なので品質の値を持ち込まない
        let mut settings = AppSettings::default();
        settings.screenshot.format = ScreenshotFormat::Png;
        settings.screenshot.jpeg_quality = 10;

        assert_eq!(settings.screenshot.encoding(), ScreenshotEncoding::Png);
    }

    #[test]
    fn app_settings_missing_format_key_defaults_to_jpeg() {
        // format を足す前の版が書いた設定ファイル。これまでと同じ JPEG で
        // 保存され、他の項目も保持されなければならない。
        // without_key を使わないのは [video] にも format があるため
        let config = FULL_CONFIG.replace(
            "format = \"png\"
",
            "",
        );
        assert!(
            !config.contains("format = \"png\""),
            "テスト用の設定から screenshot の format が消えていない"
        );

        let settings: AppSettings =
            toml::from_str(&config).expect("format が欠けていても読めなければならない");

        assert_eq!(settings.screenshot.format, ScreenshotFormat::Jpeg);
        assert_eq!(settings.screenshot.jpeg_quality, 60);
        assert_eq!(settings.hotkey(HotkeyAction::Screenshot), Some("Ctrl+S"));
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn app_settings_missing_jpeg_quality_key_uses_default_quality() {
        let config = without_key(FULL_CONFIG, "jpeg_quality");
        assert!(
            !config.contains("jpeg_quality ="),
            "テスト用の設定から jpeg_quality が消えていない"
        );

        let settings: AppSettings =
            toml::from_str(&config).expect("jpeg_quality が欠けていても読めなければならない");

        assert_eq!(settings.screenshot.jpeg_quality, 90);
        assert_eq!(settings.screenshot.format, ScreenshotFormat::Png);
    }

    #[test]
    fn app_settings_missing_destination_key_defaults_to_file() {
        // 出力先を足す前の版が書いた設定ファイル。これまでどおりファイルへ
        // 保存され、他の項目も保持されなければならない
        let config = without_key(FULL_CONFIG, "destination");
        assert!(
            !config.contains("destination ="),
            "テスト用の設定から destination が消えていない"
        );

        let settings: AppSettings =
            toml::from_str(&config).expect("destination が欠けていても読めなければならない");

        assert_eq!(settings.screenshot.destination, ScreenshotDestination::File);
        assert_eq!(settings.screenshot.format, ScreenshotFormat::Png);
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn app_settings_unknown_destination_value_falls_back_to_file_without_losing_settings() {
        // 手で書き換えて綴りを誤った場合。出力先だけが既定へ倒れ、
        // 無関係な項目は保持されなければならない
        let config = FULL_CONFIG.replace(r#"destination = "both""#, r#"destination = "printer""#);
        assert!(config.contains(r#"destination = "printer""#));

        let settings: AppSettings =
            toml::from_str(&config).expect("知らない出力先でも読めなければならない");

        assert_eq!(settings.screenshot.destination, ScreenshotDestination::File);
        assert_eq!(settings.screenshot.jpeg_quality, 60);
        assert_eq!(settings.hotkey(HotkeyAction::Screenshot), Some("Ctrl+S"));
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn screenshot_destination_from_str_accepts_known_spellings() {
        assert_eq!(
            screenshot_destination_from_str("file"),
            Some(ScreenshotDestination::File)
        );
        assert_eq!(
            screenshot_destination_from_str("CLIPBOARD"),
            Some(ScreenshotDestination::Clipboard)
        );
        assert_eq!(
            screenshot_destination_from_str(" both "),
            Some(ScreenshotDestination::Both)
        );
        assert_eq!(screenshot_destination_from_str(""), None);
        assert_eq!(screenshot_destination_from_str("files"), None);
    }

    #[test]
    fn screenshot_destination_flags_match_each_variant() {
        // 出力先ごとに「何をするか」の判定。両方のときは 2 つとも true になる
        assert!(ScreenshotDestination::File.saves_file());
        assert!(!ScreenshotDestination::File.copies_to_clipboard());

        assert!(!ScreenshotDestination::Clipboard.saves_file());
        assert!(ScreenshotDestination::Clipboard.copies_to_clipboard());

        assert!(ScreenshotDestination::Both.saves_file());
        assert!(ScreenshotDestination::Both.copies_to_clipboard());
    }

    #[test]
    fn app_settings_unknown_format_value_falls_back_to_jpeg_without_losing_settings() {
        // 手で書き換えて綴りを誤った場合。保存形式だけが既定へ倒れ、
        // 無関係な項目は保持されなければならない
        let config = FULL_CONFIG.replace(r#"format = "png""#, r#"format = "webp""#);
        assert!(config.contains(r#"format = "webp""#));

        let settings: AppSettings =
            toml::from_str(&config).expect("知らない保存形式でも読めなければならない");

        assert_eq!(settings.screenshot.format, ScreenshotFormat::Jpeg);
        assert_eq!(settings.screenshot.jpeg_quality, 60);
        assert_eq!(settings.hotkey(HotkeyAction::Screenshot), Some("Ctrl+S"));
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn app_settings_out_of_range_jpeg_quality_is_clamped_without_losing_settings() {
        // 手で書き換えて u8 に収まらない値を入れた場合。品質だけが範囲に
        // 収まり、無関係な項目は保持されなければならない
        let config = FULL_CONFIG.replace("jpeg_quality = 60", "jpeg_quality = 256");
        assert!(config.contains("jpeg_quality = 256"));

        let settings: AppSettings =
            toml::from_str(&config).expect("範囲外の品質でも読めなければならない");

        assert_eq!(settings.screenshot.jpeg_quality, 100);
        assert_eq!(settings.screenshot.format, ScreenshotFormat::Png);
        assert_eq!(settings.hotkey(HotkeyAction::Screenshot), Some("Ctrl+S"));
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn app_settings_missing_audio_buffer_key_uses_the_previous_hardcoded_length() {
        // 音声バッファを設定項目にする前の版が書いた設定ファイル。
        // 既定は当時ハードコードされていた 50ms で、音の出かたが変わらない
        let config = without_key(FULL_CONFIG, "audio_buffer_ms");
        assert!(
            !config.contains("audio_buffer_ms ="),
            "テスト用の設定から audio_buffer_ms が消えていない"
        );

        let settings: AppSettings =
            toml::from_str(&config).expect("audio_buffer_ms が欠けていても読めなければならない");

        assert_eq!(settings.audio.audio_buffer_ms, DEFAULT_AUDIO_BUFFER_MS);
        assert_eq!(settings.audio.sample_rate, Some(44100));
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn app_settings_out_of_range_audio_buffer_is_clamped_without_losing_settings() {
        // 手で書き換えて桁を間違えた場合。バッファ長だけが範囲に収まり、
        // 無関係な項目は保持されなければならない
        let config = FULL_CONFIG.replace("audio_buffer_ms = 120", "audio_buffer_ms = 5000");

        let settings: AppSettings =
            toml::from_str(&config).expect("範囲外のバッファ長でも読めなければならない");

        assert_eq!(settings.audio.audio_buffer_ms, MAX_AUDIO_BUFFER_MS);
        assert_eq!(settings.audio.channels, Some(1));
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn app_settings_negative_audio_buffer_is_clamped_to_minimum() {
        // u32 のまま読むと負の値でファイル単位のパースが落ちる。
        // i64 で受けてから丸めているので、他の項目まで失わない
        let config = FULL_CONFIG.replace("audio_buffer_ms = 120", "audio_buffer_ms = -1");

        let settings: AppSettings =
            toml::from_str(&config).expect("負のバッファ長でも読めなければならない");

        assert_eq!(settings.audio.audio_buffer_ms, MIN_AUDIO_BUFFER_MS);
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn app_settings_audio_buffer_survives_a_save_and_load_roundtrip() {
        // 既定値と違う値が TOML を往復しても保たれること。
        // [audio] へ書き出されなければ、次の起動で 50ms へ戻ってしまう
        let settings: AppSettings =
            toml::from_str(FULL_CONFIG).expect("テスト用の設定を読めること");
        assert_eq!(settings.audio.audio_buffer_ms, 120);

        let written = toml::to_string(&settings).expect("設定を書き出せること");
        let restored: AppSettings = toml::from_str(&written).expect("書き出した設定を読めること");

        assert_eq!(restored.audio.audio_buffer_ms, 120);
    }

    #[test]
    fn app_settings_negative_jpeg_quality_is_clamped_to_minimum() {
        let config = FULL_CONFIG.replace("jpeg_quality = 60", "jpeg_quality = -5");

        let settings: AppSettings =
            toml::from_str(&config).expect("負の品質でも読めなければならない");

        assert_eq!(settings.screenshot.jpeg_quality, 1);
        assert_eq!(settings.ui.volume, 80.0);
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
    fn screenshot_format_from_str_accepts_known_spellings() {
        assert_eq!(
            screenshot_format_from_str("jpeg"),
            Some(ScreenshotFormat::Jpeg)
        );
        assert_eq!(
            screenshot_format_from_str("JPG"),
            Some(ScreenshotFormat::Jpeg)
        );
        assert_eq!(
            screenshot_format_from_str(" png "),
            Some(ScreenshotFormat::Png)
        );
        assert_eq!(screenshot_format_from_str(""), None);
        assert_eq!(screenshot_format_from_str("bmp"), None);
    }

    // ---- ホットキーの移行 ----

    #[test]
    fn legacy_config_moves_screenshot_hotkey_into_hotkeys() {
        // アクション別にする前の版が書いた設定ファイル。設定していた
        // ホットキーがスクリーンショットへ移り、失われないこと
        let settings: AppSettings =
            toml::from_str(LEGACY_CONFIG).expect("旧版の設定ファイルが読めなければならない");

        assert_eq!(settings.hotkey(HotkeyAction::Screenshot), Some("Ctrl+S"));
        // 他のアクションは未割り当てのまま
        assert_eq!(settings.hotkeys.len(), 1);
        // 無関係な項目も保持される
        assert_eq!(settings.ui.volume, 80.0);
        assert_eq!(settings.video.fps, Some(30));
    }

    #[test]
    fn legacy_config_without_hotkey_keeps_the_default_f5() {
        // 旧版では「ホットキーを外した状態」を設定ファイルに残せなかった
        // （項目ごと消えるため、欠けた項目と区別できない）。移行後も
        // 従来と同じく既定の F5 になること
        let config = without_key(LEGACY_CONFIG, "hotkey");
        assert!(
            !config.contains("hotkey ="),
            "テスト用の設定に hotkey が残っている"
        );

        let settings: AppSettings =
            toml::from_str(&config).expect("hotkey が無い旧版の設定も読めなければならない");

        assert_eq!(settings.hotkey(HotkeyAction::Screenshot), Some("F5"));
    }

    #[test]
    fn hotkeys_section_wins_over_the_legacy_key() {
        // 手で書き換えて両方が書かれている場合。新しい形式を正とする
        let config = format!("{}\n[hotkeys]\nscreenshot = \"F8\"\n", LEGACY_CONFIG);

        let settings: AppSettings =
            toml::from_str(&config).expect("両方あっても読めなければならない");

        assert_eq!(settings.hotkey(HotkeyAction::Screenshot), Some("F8"));
    }

    #[test]
    fn empty_hotkeys_section_means_no_assignment() {
        // すべての割り当てを外した状態。セクションはあるが中身が無い。
        // 既定の F5 を入れ直してはならない
        let config = format!("{}\n[hotkeys]\n", LEGACY_CONFIG);

        let settings: AppSettings =
            toml::from_str(&config).expect("空の [hotkeys] でも読めなければならない");

        assert!(settings.hotkeys.is_empty());
        assert_eq!(settings.hotkey(HotkeyAction::Screenshot), None);
    }

    #[test]
    fn empty_hotkeys_survives_a_save_and_load_roundtrip() {
        // 「割り当て無し」を書き出して読み直しても、既定の F5 に戻らないこと。
        // [hotkeys] セクションごと書き出されないと、旧版の設定ファイルと
        // 区別が付かなくなる
        let mut original = AppSettings::default();
        original.hotkeys.clear();

        let serialized = toml::to_string(&original).expect("設定を書き出せなければならない");
        let restored: AppSettings =
            toml::from_str(&serialized).expect("書き出した設定を読み直せなければならない");

        assert!(
            serialized.contains("[hotkeys]"),
            "空でも [hotkeys] セクションが書き出されること: {}",
            serialized
        );
        assert!(restored.hotkeys.is_empty());
    }

    #[test]
    fn saved_config_does_not_keep_the_legacy_hotkey_key() {
        // 移行したあとは旧版の項目を書き戻さない。残すと 2 つの置き場所が
        // 食い違ったときにどちらが正か決まらなくなる
        let settings: AppSettings =
            toml::from_str(LEGACY_CONFIG).expect("旧版の設定ファイルが読めなければならない");

        let serialized = toml::to_string(&settings).expect("設定を書き出せなければならない");

        assert!(
            !serialized.contains("hotkey = "),
            "screenshot.hotkey が書き戻されている: {}",
            serialized
        );
        assert!(serialized.contains("screenshot = \"Ctrl+S\""));
    }

    #[test]
    fn migrated_settings_drop_the_legacy_field() {
        // 読み込んだ時点で旧版の項目は空になる。残っていると、そこを見て
        // 動く処理をうっかり足せてしまう
        let settings: AppSettings =
            toml::from_str(LEGACY_CONFIG).expect("旧版の設定ファイルが読めなければならない");

        assert_eq!(settings.screenshot.legacy_hotkey, None);
    }

    #[test]
    fn unknown_hotkey_action_is_ignored_without_losing_settings() {
        // 新しい版が増やしたアクションを古い版で読んだ場合。知らない名前で
        // ファイル全体のパースを失敗させない
        let config = format!("{}mute = \"Ctrl+M\"\n", FULL_CONFIG);

        let settings: AppSettings =
            toml::from_str(&config).expect("知らないアクションがあっても読めなければならない");

        assert_eq!(settings.hotkey(HotkeyAction::Screenshot), Some("Ctrl+S"));
        assert_eq!(settings.hotkey(HotkeyAction::ToggleFullscreen), Some("F11"));
        assert_eq!(settings.hotkeys.len(), 2);
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn hotkeys_are_readable_for_every_action() {
        // アクションを足したときに、設定ファイル側のキー名が読めなくなって
        // いないことを全アクションで確かめる
        let lines: String = HotkeyAction::ALL
            .iter()
            .map(|action| format!("{} = \"F5\"\n", action.as_str()))
            .collect();
        let config = format!("[hotkeys]\n{}", lines);

        let settings: AppSettings =
            toml::from_str(&config).expect("全アクションぶんの割り当てが読めなければならない");

        assert_eq!(settings.hotkeys.len(), HotkeyAction::ALL.len());
        for action in HotkeyAction::ALL {
            assert_eq!(settings.hotkey(action), Some("F5"), "{:?}", action);
        }
    }

    #[test]
    fn set_hotkey_none_removes_the_assignment() {
        let mut settings = AppSettings::default();

        settings.set_hotkey(HotkeyAction::Screenshot, None);

        assert_eq!(settings.hotkey(HotkeyAction::Screenshot), None);
        assert!(settings.hotkeys.is_empty());
    }

    #[test]
    fn set_hotkey_replaces_the_existing_assignment() {
        let mut settings = AppSettings::default();

        settings.set_hotkey(HotkeyAction::Screenshot, Some("Ctrl+S".to_string()));
        settings.set_hotkey(HotkeyAction::VolumeUp, Some("Ctrl+Shift+1".to_string()));

        assert_eq!(settings.hotkey(HotkeyAction::Screenshot), Some("Ctrl+S"));
        assert_eq!(
            settings.hotkey(HotkeyAction::VolumeUp),
            Some("Ctrl+Shift+1")
        );
    }

    #[test]
    fn default_hotkeys_assign_only_the_screenshot() {
        // 他のアクションを既定で割り当てない。グローバルホットキーは
        // 他のアプリより先にキーを奪うため、こちらから押さえない
        let settings = AppSettings::default();

        assert_eq!(settings.hotkey(HotkeyAction::Screenshot), Some("F5"));
        assert_eq!(settings.hotkeys.len(), 1);
    }

    #[test]
    fn backup_broken_config_missing_file_returns_none() {
        // 初回起動のように設定ファイルがまだ無い場合。退避するものが無いだけで、
        // エラーとして扱わない。
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("default-config.toml");

        let backup = backup_broken_config(&path).expect("失敗しないこと");

        assert!(backup.is_none());
    }

    #[test]
    fn export_file_name_uses_the_given_date() {
        let date = NaiveDate::from_ymd_opt(2026, 9, 21).expect("日付として正しいこと");

        assert_eq!(
            export_file_name(&date),
            "capturecard_viewer-settings-20260921.toml"
        );
    }

    #[test]
    fn export_file_name_pads_single_digit_month_and_day() {
        // 境界。0 埋めを忘れると 2026-1-5 が 202615 になり、並べ替えが崩れる
        let date = NaiveDate::from_ymd_opt(2026, 1, 5).expect("日付として正しいこと");

        assert_eq!(
            export_file_name(&date),
            "capturecard_viewer-settings-20260105.toml"
        );
    }

    #[test]
    fn export_to_then_import_from_restores_the_values() {
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("exported.toml");
        let mut settings: AppSettings = toml::from_str(FULL_CONFIG).expect("読めること");
        settings.set_hotkey(HotkeyAction::VolumeUp, Some("Ctrl+Up".to_string()));

        export_to(&path, &settings).expect("書き出せること");
        let imported = import_from(&path).expect("読み戻せること");

        assert_eq!(imported.video.device_name, settings.video.device_name);
        assert_eq!(imported.video.resolution, settings.video.resolution);
        assert_eq!(imported.audio.sample_rate, settings.audio.sample_rate);
        assert_eq!(imported.screenshot.format, settings.screenshot.format);
        assert_eq!(
            imported.screenshot.jpeg_quality,
            settings.screenshot.jpeg_quality
        );
        assert_eq!(imported.ui.volume, settings.ui.volume);
        assert_eq!(imported.hotkeys, settings.hotkeys);
    }

    #[test]
    fn import_from_missing_file_returns_error_without_creating_it() {
        // confy の load_path はファイルが無いと既定値で作ってしまう。
        // 読み込みのつもりで選んだ場所にファイルが増えないこと
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("missing.toml");

        assert!(import_from(&path).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn import_from_broken_toml_returns_error() {
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("broken.toml");
        fs::write(&path, "これは TOML ではない [[[").expect("書けること");

        assert!(import_from(&path).is_err());
    }

    #[test]
    fn import_from_unknown_values_falls_back_to_defaults() {
        // 手で書き換えたファイルを読み込んだ場合。解釈できない値だけが
        // 既定へ倒れ、他の項目は読めていること
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("odd.toml");
        fs::write(
            &path,
            r#"
[video]
fps = 30
color_space = "bt2020"

[screenshot]
format = "webp"
jpeg_quality = 500
"#,
        )
        .expect("書けること");

        let imported = import_from(&path).expect("読めること");

        assert_eq!(imported.video.fps, Some(30));
        assert_eq!(imported.video.color_space, ColorSpace::Auto);
        assert_eq!(imported.screenshot.format, ScreenshotFormat::Jpeg);
        assert_eq!(imported.screenshot.jpeg_quality, MAX_JPEG_QUALITY);
    }

    #[test]
    fn import_from_legacy_file_migrates_the_hotkey() {
        // 旧版が書き出したファイルを読み込んだ場合も、通常の起動と同じく
        // screenshot.hotkey が hotkeys へ移ること
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("legacy.toml");
        fs::write(&path, LEGACY_CONFIG).expect("書けること");

        let imported = import_from(&path).expect("読めること");

        assert_eq!(imported.hotkey(HotkeyAction::Screenshot), Some("Ctrl+S"));
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
