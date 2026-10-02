//! 引数を取らない文字列の表。
//!
//! 1 行が「キー { 言語ごとの文言 }」の 1 件。`texts!` がここから
//! `Text` の列挙子、言語ごとの `match`、テスト用の全件の一覧を作る。
//! **キーを打ち間違えると `Text::…` が見つからずコンパイルで落ち、
//! 使われなくなったキーは `dead_code` の警告（CI では `-D warnings` でエラー）になる。**
//! 言語を 1 つでも書き漏らすと `texts!` の形に合わずコンパイルで落ちる。
//!
//! 複数行の文言は `\n` で改行を書く。リテラルの中で改行すると、次の行の
//! ソースの字下げまで文字列に入って画面に出る（Issue #225）。
//!
//! 並びは使う場所ごとにまとめてある。足すときは近い塊の末尾へ置く。

use super::{language, Language};

macro_rules! texts {
    ($($key:ident { ja: $ja:literal, en: $en:literal },)*) => {
        /// 画面に出す文字列のキー。`get()` で現在の言語の文言を返す。
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Text {
            $($key,)*
        }

        impl Text {
            /// 全てのキー。表が揃っているかをテストで確かめるためにある。
            #[cfg(test)]
            const ALL: &'static [Text] = &[$(Text::$key,)*];

            fn ja(self) -> &'static str {
                match self {
                    $(Text::$key => $ja,)*
                }
            }

            fn en(self) -> &'static str {
                match self {
                    $(Text::$key => $en,)*
                }
            }
        }
    };
}

impl Text {
    /// 現在の言語での文言。
    pub fn get(self) -> &'static str {
        self.in_language(language())
    }

    fn in_language(self, language: Language) -> &'static str {
        match language {
            Language::Japanese => self.ja(),
            Language::English => self.en(),
        }
    }
}

texts! {
    // ---- 共通 ----
    // 英語では文頭に来る形。文の途中に入れるときは msg.rs 側で小文字にする
    Input { ja: "入力", en: "Input" },
    Output { ja: "出力", en: "Output" },

    // ---- 失敗の定型文（status::ErrorSource::headline） ----
    HeadlineVideo { ja: "映像デバイスに接続できません", en: "Cannot connect to the video device" },
    HeadlineAudio { ja: "音声デバイスに接続できません", en: "Cannot connect to the audio device" },
    HeadlineScreenshot { ja: "スクリーンショットを出力できません", en: "Cannot output the screenshot" },
    HeadlineScreenshotSound { ja: "効果音を再生できません", en: "Cannot play the screenshot sound" },
    HeadlineHotkey { ja: "ホットキーを登録できません", en: "Cannot register the hotkey" },
    HeadlineSettings { ja: "設定ファイルを読み書きできません", en: "Cannot read or write the settings file" },
    HeadlineUpdate { ja: "更新できません", en: "Cannot update" },
    HeadlineRecording { ja: "録画できません", en: "Cannot record" },

    // ---- エラーの文言（各エラー enum の Display） ----
    VideoNoDevices { ja: "映像デバイスが 1 台も見つからない", en: "No video devices found" },
    // ワーカーが映像デバイスの未指定でストリームを閉じたときの理由（app/worker_connect.rs、#334）
    VideoDeviceNotSelected { ja: "映像デバイスが選ばれていません。「デバイス設定」タブで選んでください", en: "No video device is selected. Choose one in the Devices tab" },
    // ワーカーが入力の未指定で音声を開かないときの理由（app/worker_connect.rs、#304）
    AudioInputNotSelected { ja: "オーディオ入力デバイスが選ばれていません。「デバイス設定」タブで選んでください", en: "No audio input device is selected. Choose one in the Devices tab" },
    // 入力が映像デバイスの音声ピンなのに使えず、音声を開かずに待つ理由（app/monitor_audio_pin.rs、#388）。
    // 設定ダイアログの選べない理由にも出るので「DirectShow」「音声ピン」の語を使わない（#409）
    AudioPinVideoNotOpen { ja: "映像デバイスが開いていないので、映像デバイスの音声は使えません", en: "The video device is not open, so its audio cannot be used" },
    AudioPinMediaFoundation { ja: "いまの映像の開き方では、映像デバイスの音声を使えません。「デバイス設定」タブの「映像の開き方」を「DirectShow」にすると使えます", en: "The video device's audio is not available with the current way of opening the video. Set \"Open video with\" to \"DirectShow\" in the Devices tab to use it" },
    AudioPinMissing { ja: "この映像デバイスからは音声を受け取れません", en: "This video device does not provide audio" },
    AudioPinNotConnected { ja: "映像デバイスの音声を使うために、映像デバイスを開き直すのを待っています", en: "Waiting for the video device to reopen so that its audio can be used" },
    // 音声ピンの入力で開こうとしたが、繋いだ音声ピンが無かった（audio::AudioError::VideoPinUnavailable）
    AudioPinUnavailable { ja: "映像デバイスの音声がつながっていないので、音声を開けません", en: "The video device's audio is not connected, so audio cannot be opened" },
    // 自動再接続が無効な間に音声ストリームがエラーで止まったときの理由（app/worker_timers.rs、#310）
    AudioStreamStoppedWithoutReconnect { ja: "音声ストリームがエラーで止まりました。自動再接続が無効なので、右クリックメニューの「デバイス再接続」で開き直してください", en: "The audio stream stopped with an error. Automatic reconnection is off, so use Reconnect devices in the right-click menu" },
    DeviceNotVisibleNoDevices { ja: "Windows 側にもデバイスが見えていない可能性があります（1 台も見えていません）。デバイスマネージャーで接続を確認してください", en: "Windows may not see the device either (no devices are visible). Check the connection in Device Manager" },
    HotkeyMultipleKeys { ja: "通常キーを 2 つ以上は指定できません", en: "Only one non-modifier key can be specified" },
    HotkeyMissingKey { ja: "通常キーが指定されていません", en: "No non-modifier key is specified" },
    KeyboardHookUnsupported { ja: "この OS には対応していません", en: "This OS is not supported" },
    KeyboardHookListenerStopped { ja: "ホットキーのリスナースレッドが起動しませんでした", en: "The hotkey listener thread did not start" },
    PresetNameEmpty { ja: "プリセット名を入力してください", en: "Enter a preset name" },
    PresetNameDuplicate { ja: "同じ名前のプリセットが既にあります", en: "A preset with the same name already exists" },
    UpdateTimedOut { ja: "GitHub から 5 秒以内に応答がない", en: "No response from GitHub within 5 seconds" },
    UpdateNoRelease { ja: "公開されたリリースが無い", en: "No published release was found" },
    UpdateRateLimited { ja: "GitHub への問い合わせ回数の上限に達した。しばらくしてから確かめてください", en: "Reached the GitHub request limit. Try again later" },
    UpdateNoAssets { ja: "このバージョンには自動更新用のファイルがありません。リリースページから手動で更新してください", en: "This release has no files for automatic updates. Update manually from the release page" },
    UpdateUnexpectedAssetUrl { ja: "自動更新用のファイルの URL がこのリポジトリのものではない", en: "The download URL of the update does not belong to this repository" },
    UpdateNotWritable { ja: "このフォルダには書き込めないため自動更新できません。リリースページから手動で更新してください", en: "Cannot update automatically because this folder is not writable. Update manually from the release page" },
    UpdateDownloadTimedOut { ja: "ダウンロードが時間内に終わらない", en: "The download did not finish in time" },
    UpdateDownloadTooLarge { ja: "ダウンロードするファイルが大きすぎる", en: "The file to download is too large" },
    UpdateChecksumMissing { ja: "SHA256SUMS.txt に exe の行が無い", en: "SHA256SUMS.txt has no line for the exe" },
    UpdateChecksumMismatch { ja: "ダウンロードした exe の SHA-256 が SHA256SUMS.txt と合わない", en: "The SHA-256 of the downloaded exe does not match SHA256SUMS.txt" },
    UpdateCancelled { ja: "キャンセルした", en: "Cancelled" },
    UpdateThreadEnded { ja: "更新のスレッドが結果を返さずに終わった", en: "The update thread ended without a result" },

    // ---- ホットキーのアクション名（HotkeyAction::label） ----
    ActionScreenshot { ja: "スクリーンショット", en: "Screenshot" },
    ActionToggleFullscreen { ja: "フルスクリーン切替", en: "Toggle fullscreen" },
    ActionToggleAlwaysOnTop { ja: "最前面表示の切替", en: "Toggle always on top" },
    ActionReconnectDevices { ja: "デバイス再接続", en: "Reconnect devices" },
    ActionVolumeUp { ja: "音量を上げる", en: "Volume up" },
    ActionVolumeDown { ja: "音量を下げる", en: "Volume down" },
    ActionToggleMute { ja: "ミュート切替", en: "Toggle mute" },
    ActionToggleRecording { ja: "録画の開始・停止", en: "Start / stop recording" },

    // ---- 色空間・色レンジ（settings::ColorSpace / ColorRange の label） ----
    ColorSpaceAuto { ja: "自動（解像度から判断）", en: "Auto (based on resolution)" },
    ColorSpaceBt601 { ja: "BT.601（SD）", en: "BT.601 (SD)" },
    ColorSpaceBt709 { ja: "BT.709（HD）", en: "BT.709 (HD)" },
    ColorRangeLimited { ja: "リミテッド（16〜235）", en: "Limited (16-235)" },
    ColorRangeFull { ja: "フル（0〜255）", en: "Full (0-255)" },

    // ---- 映像の開き方（settings::VideoBackendSetting / video::CaptureApi の label） ----
    // Media Foundation / DirectShow は API の名前なので訳さない
    VideoBackendAuto { ja: "自動", en: "Auto" },
    VideoBackendMediaFoundation { ja: "Media Foundation", en: "Media Foundation" },
    VideoBackendDirectShow { ja: "DirectShow", en: "DirectShow" },
    CaptureApiFake { ja: "フェイク", en: "Fake" },

    // ---- 言語（settings::LanguageSetting の label） ----
    // 言語名はどの言語で表示していても、その言語自身の表記で出す。
    // 読めない言語へ切り替えてしまっても、戻す先を見つけられるようにするため
    LanguageAuto { ja: "自動（OS の言語に合わせる）", en: "Auto (follow the OS language)" },
    LanguageJapanese { ja: "日本語", en: "日本語" },
    LanguageEnglish { ja: "English", en: "English" },

    // ---- 接続状態（status.rs） ----
    VideoActualUnknown { ja: "（取得できない）", en: "(unavailable)" },
    ResampleIdentity { ja: "変換なし", en: "No conversion" },
    WaterLevelUnknown { ja: "不明", en: "unknown" },
    UnderrunUnknown { ja: "アンダーラン: -", en: "Underruns: -" },
    DroppedFramesUnknown { ja: "満杯で捨てた: -", en: "Dropped (buffer full): -" },
    LinkConnected { ja: "接続中", en: "Connected" },
    LinkReconnecting { ja: "未接続（再接続を試しています）", en: "Disconnected (trying to reconnect)" },
    LinkDisconnected { ja: "未接続", en: "Disconnected" },

    // ---- 設定ダイアログの枠（ui/mod.rs） ----
    SettingsTitle { ja: "設定", en: "Settings" },
    TabDevice { ja: "デバイス設定", en: "Devices" },
    TabScreenshot { ja: "スクリーンショット設定", en: "Screenshots" },
    TabRecording { ja: "録画", en: "Recording" },
    Hotkeys { ja: "ホットキー", en: "Hotkeys" },
    TabOther { ja: "その他", en: "Other" },
    TabStatus { ja: "接続状態", en: "Connection status" },
    ButtonOk { ja: "OK", en: "OK" },
    ButtonCancel { ja: "キャンセル", en: "Cancel" },
    ButtonApply { ja: "適用", en: "Apply" },
    ButtonRetry { ja: "再取得", en: "Retry" },

    // ---- 「デバイス設定」タブ（ui/device_tab.rs / ui/capability.rs） ----
    VideoSettings { ja: "ビデオ設定", en: "Video" },
    VideoDevice { ja: "ビデオデバイス", en: "Video device" },
    SelectDevice { ja: "デバイスを選択...", en: "Select a device..." },
    VideoBackendLabel { ja: "映像の開き方:", en: "Open video with:" },
    VideoBackendHint { ja: "自動では、名前に「(DirectShow)」が付いたデバイスだけを DirectShow で、それ以外を Media Foundation で開きます", en: "Auto opens devices whose name ends with \"(DirectShow)\" with DirectShow, and all others with Media Foundation" },
    VideoBackendNotice { ja: "選んだ方法の一覧に無いデバイスには接続できません。映像が出ない場合は「自動」に戻してください。", en: "Devices that the selected method does not list cannot be connected. If no picture appears, switch back to Auto." },
    VideoCapabilityPending { ja: "対応形式を取得中...", en: "Querying supported formats..." },
    VideoCapabilityFallback { ja: "下の選択肢は既定値です。", en: "The choices below are defaults." },
    FormatLabel { ja: "フォーマット:", en: "Format:" },
    ResolutionLabel { ja: "解像度:", en: "Resolution:" },
    FrameRateLabel { ja: "フレームレート:", en: "Frame rate:" },
    ColorSpaceLabel { ja: "色空間:", en: "Color space:" },
    ColorSpaceHint { ja: "色がずれて見える場合に切り替えます。通常は自動のままで構いません", en: "Change this if colors look off. Auto is usually fine" },
    ColorRangeLabel { ja: "色レンジ:", en: "Color range:" },
    ColorRangeHint { ja: "黒が灰色に浮く、または黒潰れ・白飛びする場合に切り替えます", en: "Change this if blacks look gray, or if shadows are crushed or highlights are blown out" },
    VideoAdjustments { ja: "映像調整", en: "Picture adjustments" },
    ButtonReset { ja: "リセット", en: "Reset" },
    VideoAdjustmentsResetHint { ja: "明るさ・コントラスト・彩度を無調整（0）へ戻します", en: "Resets brightness, contrast and saturation to no adjustment (0)" },
    Brightness { ja: "明るさ", en: "Brightness" },
    BrightnessHint { ja: "映像全体を明るく（＋）または暗く（－）します", en: "Makes the whole picture brighter (+) or darker (-)" },
    Contrast { ja: "コントラスト", en: "Contrast" },
    ContrastHint { ja: "明暗の差を強く（＋）または弱く（－）します。-100 で中間グレー一色になります", en: "Increases (+) or decreases (-) the difference between light and dark. At -100 the picture becomes flat mid-gray" },
    Saturation { ja: "彩度", en: "Saturation" },
    SaturationHint { ja: "色の濃さを強く（＋）または弱く（－）します。-100 で白黒になります", en: "Makes colors more (+) or less (-) vivid. At -100 the picture becomes black and white" },
    AudioSettings { ja: "オーディオ設定", en: "Audio" },
    AudioInputDevice { ja: "オーディオ入力デバイス", en: "Audio input device" },
    // 「オーディオ入力デバイス」の先頭の項目（ui/audio_input.rs、#394）。ふだんは映像デバイスの
    // 名前を項目名にし、これは名前が無いときだけ出す（#409）。設定に残るのは `video_pin` で、
    // これは表示だけの文言なので翻訳してよい（docs/design/directshow-audio.md の (5)）
    AudioInputVideoPin { ja: "映像デバイスの音声", en: "Video device audio" },
    AudioInputVideoPinHint { ja: "この映像デバイスが受け取っている音声（HDMI などの音声）をそのまま使います", en: "Uses the audio that this video device receives (such as HDMI audio)" },
    AudioOutputDevice { ja: "オーディオ出力デバイス", en: "Audio output device" },
    DefaultDevice { ja: "デフォルト", en: "Default" },
    SampleRateLabel { ja: "サンプリングレート:", en: "Sample rate:" },
    // 英語では文の途中に入れるときに msg.rs 側で小文字にする
    SampleRate { ja: "サンプリングレート", en: "Sample rate" },
    ChannelsLabel { ja: "チャンネル数:", en: "Channels:" },
    Channels { ja: "チャンネル数", en: "Channel count" },
    ChannelMono { ja: "1（モノラル）", en: "1 (mono)" },
    ChannelStereo { ja: "2（ステレオ）", en: "2 (stereo)" },
    ChannelsFixedByDevice { ja: "このデバイスの組み合わせでは 1 つしか選べません（Windows の共有モードではデバイスのミックスフォーマットに固定されます）", en: "Only one choice is available for this device combination (Windows shared mode fixes it to the device's mix format)" },
    AudioBufferLabel { ja: "音声バッファ:", en: "Audio buffer:" },
    AudioBufferHint { ja: "この長さがそのまま音声の遅延になる。小さいほど低遅延だがノイズが出やすい（既定: 50 ms）", en: "This length becomes the audio delay. Smaller means lower latency but more likely to crackle (default: 50 ms)" },
    PassthroughLabel { ja: "音声パススルー:", en: "Audio passthrough:" },
    Enabled { ja: "有効", en: "Enabled" },
    PassthroughDisabledWarning { ja: "音声パススルーが無効です（音は出力されません）", en: "Audio passthrough is disabled (no sound will be output)" },
    UserInterface { ja: "ユーザーインターフェース", en: "User interface" },
    MaintainAspectRatio { ja: "アスペクト比を維持", en: "Keep aspect ratio" },
    InitialVolumeLabel { ja: "初期音量:", en: "Initial volume:" },

    // ---- 「スクリーンショット設定」タブ（ui/screenshot_tab.rs） ----
    ScreenshotDestination { ja: "出力先", en: "Destination" },
    DestinationFile { ja: "ファイルに保存", en: "Save to file" },
    DestinationClipboard { ja: "クリップボードにコピー", en: "Copy to clipboard" },
    DestinationBoth { ja: "両方", en: "Both" },
    DestinationHint { ja: "クリップボードへは圧縮せずそのままの画をコピーします。\n保存場所と保存形式は、ファイルに保存するときだけ使われます。", en: "The clipboard receives the image as is, without compression.\nThe save location and format are used only when saving to a file." },
    SaveLocation { ja: "保存場所", en: "Save location" },
    SaveFolderLabel { ja: "保存フォルダ:", en: "Folder:" },
    ButtonBrowse { ja: "参照...", en: "Browse..." },
    SaveFormat { ja: "保存形式", en: "File format" },
    JpegQualityLabel { ja: "JPEG 品質:", en: "JPEG quality:" },
    SaveFormatHint { ja: "JPEG はファイルが小さくなりますが、文字や細い線ににじみが出ます。\nPNG は元の画をそのまま保存できるかわりに、ファイルが数倍の大きさになります。", en: "JPEG makes smaller files, but text and thin lines get blurry.\nPNG keeps the original image as is, but files are several times larger." },
    SoundEffect { ja: "効果音", en: "Sound effect" },
    SoundFileLabel { ja: "サウンドファイル:", en: "Sound file:" },
    ButtonSelectFile { ja: "ファイル選択...", en: "Choose file..." },
    SoundResetDefault { ja: "既定に戻す", en: "Use default" },
    SoundResetDefaultHint { ja: "内蔵の効果音を使います", en: "Uses the built-in sound" },
    VolumeLabel { ja: "音量:", en: "Volume:" },
    SoundTest { ja: "テスト再生", en: "Test" },
    SoundDisable { ja: "効果音を鳴らさない", en: "No sound" },
    SoundSilent { ja: "なし（効果音を鳴らさない）", en: "None (no sound)" },
    SoundDefault { ja: "既定（内蔵）", en: "Default (built-in)" },

    // ---- 「ホットキー」タブと入力ダイアログ（ui/hotkeys_tab.rs / ui/hotkey_capture.rs） ----
    HotkeySettings { ja: "ホットキー設定", en: "Hotkey settings" },
    HotkeyTriggerCondition { ja: "反応する条件", en: "When hotkeys work" },
    HotkeyOnlyWhenFocused { ja: "このアプリにフォーカスがあるときだけ反応する", en: "Only when this app has focus" },
    HotkeyOnlyWhenFocusedHint { ja: "オフのときは、他のアプリを操作している間や最小化している間も反応します。どちらの場合も、押したキーは他のアプリにもそのまま届きます。", en: "When off, hotkeys also work while you use other apps or while this app is minimized. Either way, the keys you press still reach other apps." },
    HotkeyAssignableHint { ja: "スクリーンショット以外の操作にも割り当てられます。", en: "Hotkeys can be assigned to actions other than screenshots too." },
    HotkeyUnassigned { ja: "未設定", en: "Not set" },
    ButtonConfigure { ja: "設定...", en: "Set..." },
    ButtonClear { ja: "クリア", en: "Clear" },
    HotkeyModifiersOnly { ja: "修飾キーだけでは登録できません", en: "Modifier keys alone cannot be registered" },
    HotkeyCaptureWaiting { ja: "キー入力待機中...", en: "Waiting for keys..." },
    HotkeyClipboardCommand { ja: "コピー・切り取り・貼り付けになるキー（Ctrl+C / Ctrl+X / Ctrl+V / Ctrl+Insert / Shift+Insert / Shift+Delete）はここでは割り当てられません", en: "Keys that act as copy, cut, or paste (Ctrl+C / Ctrl+X / Ctrl+V / Ctrl+Insert / Shift+Insert / Shift+Delete) cannot be assigned here" },
    HotkeyNavigationKeyHint { ja: "Tab・矢印・Home・End・PageUp・PageDown を修飾キーなしで割り当てると、このアプリが前面にある間はそのキーを画面のキー操作（フォーカスの移動など）に使えなくなります。Ctrl などとの組み合わせをおすすめします。", en: "If you assign Tab, an arrow key, Home, End, PageUp, or PageDown without a modifier, that key can no longer be used for keyboard navigation in this app (such as moving focus) while the app is in front. Combining it with Ctrl or another modifier is recommended." },

    // ---- 「その他」タブとプリセット（ui/other_tab.rs / ui/preset.rs） ----
    // 日本語の見出しにも英語を添える。読めない言語へ切り替えてしまっても、
    // どこで戻せばよいかを見つけられるようにするため
    LanguageGroup { ja: "言語（Language）", en: "Language" },
    LanguageHint { ja: "「適用」か「OK」で切り替わります。自動のときは、OS の表示言語が日本語なら日本語、それ以外なら英語になります。", en: "Takes effect when you press Apply or OK. With Auto, Japanese is used if the OS display language is Japanese, and English otherwise." },
    SettingsFile { ja: "設定ファイル", en: "Settings file" },
    ExportSettings { ja: "設定を書き出す...", en: "Export settings..." },
    ImportSettings { ja: "設定を読み込む...", en: "Import settings..." },
    ExportHint { ja: "書き出すのは実行中の設定です。編集中の内容を含めたい場合は、先に「適用」を押してください。", en: "Exports the settings currently in use. To include your pending edits, press Apply first." },
    ImportHint { ja: "読み込んだ内容は編集中の設定に入ります。「適用」か「OK」を押すまで反映されません。", en: "Imported settings go into the settings being edited. They take effect when you press Apply or OK." },
    ImportWindowHint { ja: "ウィンドウの位置とサイズは読み込みません。別の画面構成で書き出したファイルを読んでも、ウィンドウは動きません。", en: "Window position and size are not imported, so the window stays put even if the file was exported on a different display setup." },
    ResetGroup { ja: "初期化", en: "Reset" },
    ResetConfirm { ja: "編集中の設定を初期値に戻します。よろしいですか？", en: "Reset the settings being edited to their defaults?" },
    ResetConfirmYes { ja: "初期化する", en: "Reset" },
    ResetConfirmNo { ja: "やめる", en: "Don't reset" },
    ResetButton { ja: "設定を初期化...", en: "Reset settings..." },
    ResetHint { ja: "初期化も編集中の設定に対して行います。「適用」か「OK」を押すまで反映されません。", en: "Resetting also applies to the settings being edited. It takes effect when you press Apply or OK." },
    ResetScopeHint { ja: "戻る範囲は読み込みと同じです。ウィンドウの位置とサイズ、右クリックメニューで切り替える項目は初期化しません。", en: "Resets the same items that import covers. Window position and size, and items toggled from the right-click menu, are not reset." },
    Preset { ja: "プリセット", en: "Presets" },
    PresetNone { ja: "なし", en: "None" },
    PresetEmpty { ja: "プリセットはまだありません。下の入力欄から作れます。", en: "No presets yet. You can create one with the field below." },
    PresetLoad { ja: "読み込む", en: "Load" },
    PresetLoadHint { ja: "このプリセットのビデオ・オーディオ設定を編集中の設定へ入れます", en: "Loads this preset's video and audio settings into the settings being edited" },
    PresetOverwrite { ja: "上書き保存", en: "Overwrite" },
    PresetOverwriteHint { ja: "編集中のビデオ・オーディオ設定でこのプリセットを置き換えます", en: "Replaces this preset with the video and audio settings being edited" },
    PresetDelete { ja: "削除", en: "Delete" },
    PresetSaveNewLabel { ja: "現在の設定を新しいプリセットとして保存:", en: "Save the current settings as a new preset:" },
    PresetNameHint { ja: "例: 低遅延優先", en: "e.g. Low latency" },
    PresetSave { ja: "保存", en: "Save" },
    PresetScopeHint { ja: "プリセットに入るのは「デバイス設定」タブのビデオとオーディオだけです。スクリーンショット・ホットキー・ウィンドウの設定は含みません。", en: "Presets contain only the video and audio settings from the Devices tab. Screenshot, hotkey and window settings are not included." },
    PresetAutoReconnectHint { ja: "デバイスの自動再接続もプリセットには含みません。右クリックメニューで切り替えた状態がそのまま残ります。", en: "Automatic device reconnection is not included either. Whatever you set from the right-click menu stays as is." },
    PresetDraftHint { ja: "追加・上書き・削除・読み込みは編集中の設定に対して行います。「適用」か「OK」を押すまで反映されません。", en: "Adding, overwriting, deleting and loading apply to the settings being edited. They take effect when you press Apply or OK." },
    PresetMenuHint { ja: "切り替えは右クリックメニューの「プリセット」からも行えます。", en: "You can also switch presets from Presets in the right-click menu." },

    // ---- 「接続状態」タブ（ui/status_tab.rs） ----
    LinkVideo { ja: "映像", en: "Video" },
    LinkAudio { ja: "音声", en: "Audio" },
    // 直近の失敗があるときだけ出す枠の見出し（Issue #356）
    LinkScreenshotSound { ja: "スクリーンショットの効果音", en: "Screenshot sound" },
    StatusReadOnlyHint { ja: "この内容は表示だけで、「適用」や「OK」では変わりません。", en: "This tab is for information only. Apply and OK do not change it." },
    StatusLogHint { ja: "詳しい経過はログファイルに残っています（%AppData%\\capturecard_viewer\\logs）。", en: "Details are recorded in the log files (%AppData%\\capturecard_viewer\\logs)." },
    FakeDevicesNotice { ja: "テスト用のフェイクデバイスで動いています（環境変数 CAPTURECARD_VIEWER_FAKE_DEVICES）", en: "Running with fake test devices (environment variable CAPTURECARD_VIEWER_FAKE_DEVICES)" },
    StateLabel { ja: "状態:", en: "Status:" },
    NoRecentError { ja: "直近のエラー: なし", en: "Recent error: none" },

    // ---- 右クリックメニュー（app/menu/items.rs） ----
    Mute { ja: "ミュート", en: "Mute" },
    MenuAlwaysOnTop { ja: "最前面表示", en: "Always on top" },
    MenuFullscreen { ja: "フルスクリーン表示", en: "Fullscreen" },
    MenuHideTitleBar { ja: "タイトルバーを隠す", en: "Hide title bar" },
    MenuHideTitleBarHint { ja: "タイトルバーと枠を消します。移動は映像のドラッグ、サイズ変更はウィンドウ端のドラッグ、終了はこのメニューの「終了」か Alt+F4 で行います", en: "Removes the title bar and border. Drag the video to move the window, drag its edges to resize, and use Quit in this menu or Alt+F4 to exit" },
    MenuHideTitleBarDisabledHint { ja: "フルスクリーン中は元から装飾がないため切り替えられません", en: "Not available in fullscreen, which has no title bar anyway" },
    MenuDragMove { ja: "画面ドラッグ移動", en: "Drag video to move window" },
    MenuDragMoveDisabledHint { ja: "タイトルバーを隠している間は、ウィンドウを動かす唯一の手段なので切れません", en: "Cannot be turned off while the title bar is hidden, because it is the only way to move the window" },
    MenuStats { ja: "情報表示", en: "Show stats" },
    MenuAutoReconnect { ja: "デバイスの自動再接続", en: "Reconnect devices automatically" },
    MenuAutoReconnectHint { ja: "映像が途切れたり音声デバイスが消えたときに、自動でデバイスを開き直します", en: "Reopens the devices automatically when the video stops or an audio device disappears" },
    MenuResetWindowSize { ja: "ウィンドウサイズをリセット", en: "Reset window size" },
    MenuResetWindowSizeDisabledHint { ja: "フルスクリーン中は変更できません", en: "Not available in fullscreen" },
    MenuAdvancedSettings { ja: "詳細設定...", en: "Settings..." },
    MenuQuit { ja: "終了", en: "Quit" },
    MenuStartRecording { ja: "録画を開始", en: "Start recording" },
    MenuRecordingFinishing { ja: "録画を保存しています...", en: "Saving the recording..." },
    // サブメニューを開く項目。矢印は項目名の一部として訳ごとに持つ
    MenuViewSubmenu { ja: "表示  ⏵", en: "View  ⏵" },
    MenuWindowSubmenu { ja: "ウィンドウ  ⏵", en: "Window  ⏵" },
    MenuPresetSubmenu { ja: "プリセット  ⏵", en: "Presets  ⏵" },

    // ---- 映像の上に出すもの（app/view.rs / app/window.rs） ----
    PlaceholderNoSignal { ja: "映像信号がありません", en: "No video signal" },
    PlaceholderReconnecting { ja: "デバイスが接続されていません（再接続を試しています）", en: "No device connected (trying to reconnect)" },
    PlaceholderDisconnected { ja: "デバイスが接続されていません", en: "No device connected" },
    StatsFpsPending { ja: "FPS - (フレーム間隔の計測待ち)", en: "FPS - (measuring frame interval)" },
    StatsDecodeUnknown { ja: "デコード -", en: "Decode -" },
    StatsNoFrame { ja: "映像フレームなし", en: "No video frames" },
    DragMoveEnabledNotice { ja: "ウィンドウを動かすため、画面ドラッグ移動を有効にしました", en: "Turned on dragging the video to move the window, so the window can still be moved" },
    FullscreenOn { ja: "フルスクリーン ON", en: "Fullscreen ON" },
    FullscreenOff { ja: "フルスクリーン OFF", en: "Fullscreen OFF" },

    // ---- スクリーンショットの結果（app/screenshot.rs） ----
    ScreenshotCopied { ja: "クリップボードへコピーした", en: "Copied to the clipboard" },
    ScreenshotNoDestination { ja: "出力先が 1 つも設定されていません", en: "No destination is selected" },
    ScreenshotNoFrame { ja: "表示中の映像がありません", en: "No video is being shown" },

    // ---- 設定ダイアログの操作の結果（app/settings_dialog.rs） ----
    AudioFileFilter { ja: "音声ファイル", en: "Audio files" },
    SettingsReadFailed { ja: "設定を読み取れない", en: "Cannot read the settings" },
    SettingsResetDone { ja: "初期値に戻しました。「適用」または「OK」で反映します", en: "Reset to defaults. Press Apply or OK to use them" },

    // ---- 更新の確認（ui/update_dialog.rs / ui/other_tab.rs） ----
    UpdateDialogTitle { ja: "新しいバージョンがあります", en: "Update available" },
    UpdateViewReleaseNotes { ja: "リリースノートを見る", en: "View release notes" },
    UpdateNow { ja: "更新する", en: "Update" },
    UpdateNowHint { ja: "新しいバージョンをダウンロードして照合し、いまの exe と置き換えて再起動します。", en: "Downloads and verifies the new version, replaces the current exe, and restarts." },
    UpdateLater { ja: "後で", en: "Later" },
    UpdateSkipVersion { ja: "このバージョンは通知しない", en: "Don't notify me about this version" },
    UpdateApplyingTitle { ja: "更新しています", en: "Updating" },
    UpdateFailedTitle { ja: "更新できませんでした", en: "Update failed" },
    UpdatePreparing { ja: "準備しています...", en: "Preparing..." },
    UpdateInstalling { ja: "照合が済みました。置き換えています...", en: "Verified. Replacing the exe..." },
    UpdateRestarting { ja: "新しいバージョンを起動します...", en: "Starting the new version..." },
    UpdateClose { ja: "閉じる", en: "Close" },
    UpdateGroup { ja: "更新", en: "Updates" },
    UpdateCheckNow { ja: "更新を確認", en: "Check for updates" },
    UpdateNotChecked { ja: "まだ確認していません", en: "Not checked yet" },
    UpdateChecking { ja: "確認中...", en: "Checking..." },
    UpdateUpToDate { ja: "最新です", en: "You have the latest version" },
    UpdateOpenReleasePage { ja: "リリースページを開く", en: "Open the release page" },
    UpdateCheckOnStartup { ja: "起動時に更新を確認する", en: "Check for updates at startup" },
    UpdateNotifyOnStartup { ja: "新しいバージョンがあれば起動時にダイアログで知らせる", en: "Show a dialog at startup when a new version is available" },
    UpdateClearSkipped { ja: "解除", en: "Clear" },
    UpdateHint { ja: "確認は GitHub の Release へ問い合わせるだけです。「更新する」で新しいバージョンをダウンロードし、exe と同じフォルダで置き換えて再起動します。", en: "Checking only asks GitHub Releases. Update downloads the new version, replaces the exe in its folder, and restarts." },
    UpdateDraftHint { ja: "チェックと「解除」は「適用」か「OK」で反映します。", en: "The checkboxes and Clear take effect when you press Apply or OK." },

    // ---- 録画（recording/ / app/recording.rs） ----
    RecordingNoVideo { ja: "映像が届かなかったので、ファイルを作りませんでした", en: "No video arrived, so no file was created" },
    RecordingEncoderNotFound { ja: "エンコーダが登録されていない", en: "No encoder is registered" },
    RecordingThreadStopped { ja: "録画スレッドが止まっている", en: "The recording thread has stopped" },
    RecordingFileNameEmpty { ja: "ファイル名の書式が空です", en: "The file name format is empty" },
    RecordingFileNameInvalidSpecifier { ja: "ファイル名の書式に使えない指定子（% で始まるもの）が含まれています", en: "The file name format contains an unknown % specifier" },
    RecordingFileNameTrailingDot { ja: "ファイル名の末尾を空白や「.」にはできません", en: "A file name cannot end with a space or a period" },
    RecordingFileNameGroup { ja: "ファイル名", en: "File name" },
    RecordingFileNameFormatLabel { ja: "書式:", en: "Format:" },
    RecordingFileNameHint { ja: "日時は chrono の書式（%Y 年、%m 月、%d 日、%H 時、%M 分、%S 秒）で書きます。拡張子（.mp4）は自動で付きます。\n同じ名前のファイルがあれば _2、_3 … を付けます。使えない書式なら既定の書式で保存します。", en: "Write the date and time with chrono specifiers (%Y year, %m month, %d day, %H hour, %M minute, %S second). The .mp4 extension is added automatically.\nIf a file with the same name exists, _2, _3, ... is appended. An invalid format falls back to the default." },
    RecordingBitrateLabel { ja: "ビットレート:", en: "Bitrate:" },
    RecordingHardwareEncoder { ja: "ハードウェアエンコーダを使う", en: "Use a hardware encoder" },
    RecordingVideoHint { ja: "H.264 の MP4 で保存します。ハードウェアエンコーダ（GPU）が使えなければ、ソフトウェアのエンコーダで保存します。\n使ったエンコーダの名前は、録画中の情報表示に出ます。", en: "Recordings are saved as H.264 MP4. If no hardware (GPU) encoder is available, the software encoder is used.\nThe encoder in use is shown in the stats overlay while recording." },
    RecordingAudioEnabled { ja: "音声も録画する", en: "Record audio" },
    RecordingAudioHint { ja: "音声入力の音を AAC（48kHz 2ch）で保存します。音量・ミュート・パススルーの設定は録画に効きません（入力の音をそのまま録ります）。\n音声デバイスが無い・開けない間は無音が入ります。", en: "Audio from the audio input is saved as AAC (48 kHz, stereo). Volume, mute, and passthrough do not affect the recording (the input is recorded as is).\nWhile no audio device is available, silence is recorded." },
    RecordingAudioOffsetLabel { ja: "映像と音声のずれの補正:", en: "Audio offset:" },
    RecordingAudioOffsetHint { ja: "正の値で音声を遅らせ、負の値で早めます（±200ms、既定 0）。録画の音声が映像より遅れて聞こえるなら負の値にします。\n負の値では録画の先頭の音声がその分削られ、正の値では先頭にその分の無音が入ります。リプレイバッファにも効きます（変えると溜めた分は捨てて溜め直します）。録画中に変えたときは次の録画から効きます。", en: "Positive values delay the audio and negative values advance it (±200 ms, default 0). If the recorded audio is heard later than the video, use a negative value.\nA negative value trims that much audio from the start of the recording; a positive value adds that much silence at the start. It also applies to the replay buffer (changing it discards the buffered footage). Changes made while recording take effect from the next recording." },
    RecordingTabHint { ja: "録画の開始と停止は、右クリックメニューかホットキー（「録画の開始・停止」）で行います。\n変更は次の録画から効きます（リプレイバッファの ON / OFF と長さはすぐ効きます。ただし録画中に ON にしたときは、その録画が終わってから溜め始めます）。", en: "Start and stop recording from the right-click menu or with the Start / stop recording hotkey.\nChanges take effect from the next recording (the replay buffer switch and length take effect immediately, except that turning it on during a recording starts buffering after that recording ends)." },
    RecordingReplayGroup { ja: "リプレイバッファ（さかのぼり録画）", en: "Replay buffer" },
    RecordingReplayEnabled { ja: "録画の開始時に、直前の映像と音声を含める", en: "Include the footage just before the recording starts" },
    RecordingReplaySecondsLabel { ja: "さかのぼる長さ:", en: "Length:" },
    RecordingReplayMemoryNotice { ja: "長くするほどメモリを使います（映像 8000kbps で 5 分なら約 300MB。ビットレートに比例して増えます）。", en: "Longer lengths use more memory (about 300 MB for 5 minutes at 8000 kbps video; it grows with the bitrate)." },
    RecordingReplayHint { ja: "ON のあいだは録画していなくてもエンコーダが動き、直近の映像と音声をメモリに持ちます。録画を始めると、その分をファイルの先頭に入れます（2 秒ごとのキーフレームの位置から）。\nON にしたときから溜め始めます。映像のビットレートやエンコーダ、音声の設定を変えると、溜めた分は捨てて溜め直します。", en: "While on, the encoder runs even when not recording and keeps the latest video and audio in memory. When you start recording, that footage is placed at the beginning of the file (from a keyframe, every 2 seconds).\nBuffering starts when you turn it on. Changing the video bitrate, encoder, or audio settings discards the buffered footage and starts over." },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::with_language;
    use std::collections::HashMap;

    const LANGUAGES: [Language; 2] = [Language::Japanese, Language::English];

    // 日本語の文字（ひらがな・カタカナ・漢字・全角の記号）を含むか。
    // 英語の表に訳し忘れた日本語が残っていないかを見るために使う
    fn contains_japanese(text: &str) -> bool {
        text.chars().any(|c| {
            matches!(c,
                '\u{3000}'..='\u{30FF}' // 全角の句読点・かっこ、ひらがな、カタカナ
                | '\u{4E00}'..='\u{9FFF}' // 漢字
                | '\u{FF00}'..='\u{FFEF}' // 全角英数と全角記号
            )
        })
    }

    #[test]
    fn text_every_key_has_non_empty_text_in_every_language() {
        for language in LANGUAGES {
            for key in Text::ALL {
                let text = key.in_language(language);
                assert!(!text.is_empty(), "{key:?} の {language:?} が空");
                // 前後の空白は並べる側（egui のレイアウト）の仕事。文言に混ぜると
                // 言語ごとに揃え方がばらつく
                assert_eq!(
                    text,
                    text.trim(),
                    "{key:?} の {language:?} の前後に空白がある"
                );
            }
        }
    }

    #[test]
    fn text_japanese_is_not_duplicated_across_keys() {
        // 同じ文言に 2 つのキーがあると、片方だけ直して食い違う。
        // 同じ文言を別の場所で使うときはキーを使い回す。
        //
        // 英語では確かめない。日本語で言い分けている語が英語では同じ語になる
        // ことがある（「ビデオ設定」の見出しと「接続状態」タブの「映像」が
        // どちらも Video、など）。キーの使い回しを決めるのは日本語の側
        let mut seen: HashMap<&str, Text> = HashMap::new();
        for key in Text::ALL {
            if let Some(previous) = seen.insert(key.ja(), *key) {
                panic!("{previous:?} と {key:?} が同じ文言「{}」", key.ja());
            }
        }
    }

    #[test]
    fn text_english_has_no_japanese_except_language_names() {
        for key in Text::ALL {
            // 言語名は、その言語自身の表記で出すと決めてある
            if *key == Text::LanguageJapanese {
                continue;
            }
            assert!(
                !contains_japanese(key.en()),
                "{key:?} の英語に日本語が残っている: {}",
                key.en()
            );
        }
    }

    #[test]
    fn text_has_no_source_indentation() {
        // 複数行の文言の 2 行目以降に、ソースの字下げが入っていないこと（Issue #225）
        for language in LANGUAGES {
            for key in Text::ALL {
                for line in key.in_language(language).lines().skip(1) {
                    assert_eq!(
                        line,
                        line.trim_start(),
                        "{key:?} の {language:?} の 2 行目以降に字下げがある"
                    );
                }
            }
        }
    }

    #[test]
    fn text_get_returns_japanese_by_default() {
        assert_eq!(Text::LinkConnected.get(), "接続中");
    }

    #[test]
    fn text_get_follows_the_current_language() {
        assert_eq!(
            with_language(Language::English, || Text::LinkConnected.get()),
            "Connected"
        );
    }
}
