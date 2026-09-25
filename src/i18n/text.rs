//! 引数を取らない文字列の表。
//!
//! 1 行が「キー { 言語ごとの文言 }」の 1 件。`texts!` がここから
//! `Text` の列挙子、言語ごとの `match`、テスト用の全件の一覧を作る。
//! **キーを打ち間違えると `Text::…` が見つからずコンパイルで落ち、
//! 使われなくなったキーは `dead_code` の警告（CI では `-D warnings` でエラー）になる。**
//!
//! 並びは使う場所ごとにまとめてある。足すときは近い塊の末尾へ置く。

use super::{language, Language};

macro_rules! texts {
    ($($key:ident { ja: $ja:literal },)*) => {
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
        }
    };
}

impl Text {
    /// 現在の言語での文言。
    pub fn get(self) -> &'static str {
        match language() {
            Language::Japanese => self.ja(),
        }
    }
}

texts! {
    // ---- 共通 ----
    Input { ja: "入力" },
    Output { ja: "出力" },

    // ---- 失敗の定型文（status::ErrorSource::headline） ----
    HeadlineVideo { ja: "映像デバイスに接続できません" },
    HeadlineAudio { ja: "音声デバイスに接続できません" },
    HeadlineScreenshot { ja: "スクリーンショットを出力できません" },
    HeadlineHotkey { ja: "ホットキーを登録できません" },
    HeadlineSettings { ja: "設定ファイルを読み書きできません" },

    // ---- エラーの文言（各エラー enum の Display） ----
    VideoNoDevices { ja: "映像デバイスが 1 台も見つからない" },
    HotkeyMultipleKeys { ja: "通常キーを 2 つ以上は指定できません" },
    HotkeyMissingKey { ja: "通常キーが指定されていません" },
    KeyboardHookUnsupported { ja: "この OS には対応していません" },
    KeyboardHookListenerStopped { ja: "ホットキーのリスナースレッドが起動しませんでした" },
    PresetNameEmpty { ja: "プリセット名を入力してください" },
    PresetNameDuplicate { ja: "同じ名前のプリセットが既にあります" },

    // ---- ホットキーのアクション名（HotkeyAction::label） ----
    ActionScreenshot { ja: "スクリーンショット" },
    ActionToggleFullscreen { ja: "フルスクリーン切替" },
    ActionToggleAlwaysOnTop { ja: "最前面表示の切替" },
    ActionReconnectDevices { ja: "デバイス再接続" },
    ActionVolumeUp { ja: "音量を上げる" },
    ActionVolumeDown { ja: "音量を下げる" },
    ActionToggleMute { ja: "ミュート切替" },

    // ---- 色空間・色レンジ（settings::ColorSpace / ColorRange の label） ----
    ColorSpaceAuto { ja: "自動（解像度から判断）" },
    ColorSpaceBt601 { ja: "BT.601（SD）" },
    ColorSpaceBt709 { ja: "BT.709（HD）" },
    ColorRangeLimited { ja: "リミテッド（16〜235）" },
    ColorRangeFull { ja: "フル（0〜255）" },

    // ---- 接続状態（status.rs） ----
    VideoActualUnknown { ja: "（取得できない）" },
    ResampleIdentity { ja: "変換なし" },
    WaterLevelUnknown { ja: "不明" },
    UnderrunUnknown { ja: "アンダーラン: -" },
    LinkConnected { ja: "接続中" },
    LinkReconnecting { ja: "未接続（再接続を試しています）" },
    LinkDisconnected { ja: "未接続" },

    // ---- 設定ダイアログの枠（ui/mod.rs） ----
    SettingsTitle { ja: "設定" },
    TabDevice { ja: "デバイス設定" },
    TabScreenshot { ja: "スクリーンショット設定" },
    Hotkeys { ja: "ホットキー" },
    TabOther { ja: "その他" },
    TabStatus { ja: "接続状態" },
    ButtonOk { ja: "OK" },
    ButtonCancel { ja: "キャンセル" },
    ButtonApply { ja: "適用" },
    ButtonRetry { ja: "再取得" },

    // ---- 「デバイス設定」タブ（ui/device_tab.rs / ui/capability.rs） ----
    VideoSettings { ja: "ビデオ設定" },
    VideoDevice { ja: "ビデオデバイス" },
    SelectDevice { ja: "デバイスを選択..." },
    VideoCapabilityPending { ja: "対応形式を取得中..." },
    VideoCapabilityFallback { ja: "下の選択肢は既定値です。" },
    FormatLabel { ja: "フォーマット:" },
    ResolutionLabel { ja: "解像度:" },
    FrameRateLabel { ja: "フレームレート:" },
    ColorSpaceLabel { ja: "色空間:" },
    ColorSpaceHint { ja: "色がずれて見える場合に切り替えます。通常は自動のままで構いません" },
    ColorRangeLabel { ja: "色レンジ:" },
    ColorRangeHint { ja: "黒が灰色に浮く、または黒潰れ・白飛びする場合に切り替えます" },
    VideoAdjustments { ja: "映像調整" },
    ButtonReset { ja: "リセット" },
    VideoAdjustmentsResetHint { ja: "明るさ・コントラスト・彩度を無調整（0）へ戻します" },
    Brightness { ja: "明るさ" },
    BrightnessHint { ja: "映像全体を明るく（＋）または暗く（－）します" },
    Contrast { ja: "コントラスト" },
    ContrastHint { ja: "明暗の差を強く（＋）または弱く（－）します。-100 で中間グレー一色になります" },
    Saturation { ja: "彩度" },
    SaturationHint { ja: "色の濃さを強く（＋）または弱く（－）します。-100 で白黒になります" },
    AudioSettings { ja: "オーディオ設定" },
    AudioInputDevice { ja: "オーディオ入力デバイス" },
    AudioOutputDevice { ja: "オーディオ出力デバイス" },
    DefaultDevice { ja: "デフォルト" },
    SampleRateLabel { ja: "サンプリングレート:" },
    SampleRate { ja: "サンプリングレート" },
    ChannelsLabel { ja: "チャンネル数:" },
    Channels { ja: "チャンネル数" },
    ChannelMono { ja: "1（モノラル）" },
    ChannelStereo { ja: "2（ステレオ）" },
    ChannelsFixedByDevice { ja: "このデバイスの組み合わせでは 1 つしか選べません（Windows の共有モードではデバイスのミックスフォーマットに固定されます）" },
    AudioBufferLabel { ja: "音声バッファ:" },
    AudioBufferHint { ja: "小さいほど低遅延だがノイズが出やすい（既定: 50 ms）" },
    PassthroughLabel { ja: "音声パススルー:" },
    Enabled { ja: "有効" },
    PassthroughDisabledWarning { ja: "音声パススルーが無効です（音は出力されません）" },
    UserInterface { ja: "ユーザーインターフェース" },
    MaintainAspectRatio { ja: "アスペクト比を維持" },
    InitialVolumeLabel { ja: "初期音量:" },

    // ---- 「スクリーンショット設定」タブ（ui/screenshot_tab.rs） ----
    ScreenshotDestination { ja: "出力先" },
    DestinationFile { ja: "ファイルに保存" },
    DestinationClipboard { ja: "クリップボードにコピー" },
    DestinationBoth { ja: "両方" },
    // 2 行目の字下げは、置き換える前のリテラルがソースの字下げごと
    // 文字列に含めていたもの。表示を変えないためにそのまま残してある
    DestinationHint { ja: "クリップボードへは圧縮せずそのままの画をコピーします。
             保存場所と保存形式は、ファイルに保存するときだけ使われます。" },
    SaveLocation { ja: "保存場所" },
    SaveFolderLabel { ja: "保存フォルダ:" },
    ButtonBrowse { ja: "参照..." },
    SaveFormat { ja: "保存形式" },
    JpegQualityLabel { ja: "JPEG 品質:" },
    // DestinationHint と同じ理由で 2 行目の字下げを残してある
    SaveFormatHint { ja: "JPEG はファイルが小さくなりますが、文字や細い線ににじみが出ます。
                 PNG は元の画をそのまま保存できるかわりに、ファイルが数倍の大きさになります。" },
    SoundEffect { ja: "効果音" },
    SoundFileLabel { ja: "サウンドファイル:" },
    ButtonSelectFile { ja: "ファイル選択..." },
    SoundResetDefault { ja: "既定に戻す" },
    SoundResetDefaultHint { ja: "内蔵の効果音を使います" },
    VolumeLabel { ja: "音量:" },
    SoundTest { ja: "テスト再生" },
    SoundDisable { ja: "効果音を鳴らさない" },
    SoundSilent { ja: "なし（効果音を鳴らさない）" },
    SoundDefault { ja: "既定（内蔵）" },

    // ---- 「ホットキー」タブと入力ダイアログ（ui/hotkeys_tab.rs / ui/hotkey_capture.rs） ----
    HotkeySettings { ja: "ホットキー設定" },
    HotkeyTriggerCondition { ja: "反応する条件" },
    HotkeyOnlyWhenFocused { ja: "このアプリにフォーカスがあるときだけ反応する" },
    HotkeyOnlyWhenFocusedHint { ja: "オフのときは、他のアプリを操作している間や最小化している間も反応します。どちらの場合も、押したキーは他のアプリにもそのまま届きます。" },
    HotkeyAssignableHint { ja: "スクリーンショット以外の操作にも割り当てられます。" },
    HotkeyUnassigned { ja: "未設定" },
    ButtonConfigure { ja: "設定..." },
    ButtonClear { ja: "クリア" },
    HotkeyModifiersOnly { ja: "修飾キーだけでは登録できません" },
    HotkeyCaptureWaiting { ja: "キー入力待機中..." },

    // ---- 「その他」タブとプリセット（ui/other_tab.rs / ui/preset.rs） ----
    SettingsFile { ja: "設定ファイル" },
    ExportSettings { ja: "設定を書き出す..." },
    ImportSettings { ja: "設定を読み込む..." },
    ExportHint { ja: "書き出すのは実行中の設定です。編集中の内容を含めたい場合は、先に「適用」を押してください。" },
    ImportHint { ja: "読み込んだ内容は編集中の設定に入ります。「適用」か「OK」を押すまで反映されません。" },
    ImportWindowHint { ja: "ウィンドウの位置とサイズは読み込みません。別の画面構成で書き出したファイルを読んでも、ウィンドウは動きません。" },
    ResetGroup { ja: "初期化" },
    ResetConfirm { ja: "編集中の設定を初期値に戻します。よろしいですか？" },
    ResetConfirmYes { ja: "初期化する" },
    ResetConfirmNo { ja: "やめる" },
    ResetButton { ja: "設定を初期化..." },
    ResetHint { ja: "初期化も編集中の設定に対して行います。「適用」か「OK」を押すまで反映されません。" },
    ResetScopeHint { ja: "戻る範囲は読み込みと同じです。ウィンドウの位置とサイズ、右クリックメニューで切り替える項目は初期化しません。" },
    Preset { ja: "プリセット" },
    PresetNone { ja: "なし" },
    PresetEmpty { ja: "プリセットはまだありません。下の入力欄から作れます。" },
    PresetLoad { ja: "読み込む" },
    PresetLoadHint { ja: "このプリセットのビデオ・オーディオ設定を編集中の設定へ入れます" },
    PresetOverwrite { ja: "上書き保存" },
    PresetOverwriteHint { ja: "編集中のビデオ・オーディオ設定でこのプリセットを置き換えます" },
    PresetDelete { ja: "削除" },
    PresetSaveNewLabel { ja: "現在の設定を新しいプリセットとして保存:" },
    PresetNameHint { ja: "例: 低遅延優先" },
    PresetSave { ja: "保存" },
    PresetScopeHint { ja: "プリセットに入るのは「デバイス設定」タブのビデオとオーディオだけです。スクリーンショット・ホットキー・ウィンドウの設定は含みません。" },
    PresetAutoReconnectHint { ja: "デバイスの自動再接続もプリセットには含みません。右クリックメニューで切り替えた状態がそのまま残ります。" },
    PresetDraftHint { ja: "追加・上書き・削除・読み込みは編集中の設定に対して行います。「適用」か「OK」を押すまで反映されません。" },
    PresetMenuHint { ja: "切り替えは右クリックメニューの「プリセット」からも行えます。" },

    // ---- 「接続状態」タブ（ui/status_tab.rs） ----
    LinkVideo { ja: "映像" },
    LinkAudio { ja: "音声" },
    StatusReadOnlyHint { ja: "この内容は表示だけで、「適用」や「OK」では変わりません。" },
    StatusLogHint { ja: "詳しい経過はログファイルに残っています（%AppData%\\capturecard_viewer\\logs）。" },
    StateLabel { ja: "状態:" },
    NoRecentError { ja: "直近のエラー: なし" },

    // ---- 右クリックメニュー（app/menu/items.rs） ----
    Mute { ja: "ミュート" },
    MenuAlwaysOnTop { ja: "最前面表示" },
    MenuFullscreen { ja: "フルスクリーン表示" },
    MenuHideTitleBar { ja: "タイトルバーを隠す" },
    MenuHideTitleBarHint { ja: "タイトルバーと枠を消します。移動は映像のドラッグ、サイズ変更はウィンドウ端のドラッグ、終了はこのメニューの「終了」か Alt+F4 で行います" },
    MenuHideTitleBarDisabledHint { ja: "フルスクリーン中は元から装飾がないため切り替えられません" },
    MenuDragMove { ja: "画面ドラッグ移動" },
    MenuDragMoveDisabledHint { ja: "タイトルバーを隠している間は、ウィンドウを動かす唯一の手段なので切れません" },
    MenuStats { ja: "情報表示" },
    MenuAutoReconnect { ja: "デバイスの自動再接続" },
    MenuAutoReconnectHint { ja: "映像が途切れたり音声デバイスが消えたときに、自動でデバイスを開き直します" },
    MenuResetWindowSize { ja: "ウィンドウサイズをリセット" },
    MenuResetWindowSizeDisabledHint { ja: "フルスクリーン中は変更できません" },
    MenuAdvancedSettings { ja: "詳細設定..." },
    MenuQuit { ja: "終了" },
    // サブメニューを開く項目。矢印は項目名の一部として訳ごとに持つ
    MenuViewSubmenu { ja: "表示  ⏵" },
    MenuWindowSubmenu { ja: "ウィンドウ  ⏵" },
    MenuPresetSubmenu { ja: "プリセット  ⏵" },

    // ---- 映像の上に出すもの（app/view.rs / app/window.rs） ----
    PlaceholderNoSignal { ja: "映像信号がありません" },
    PlaceholderReconnecting { ja: "デバイスが接続されていません（再接続を試しています）" },
    PlaceholderDisconnected { ja: "デバイスが接続されていません" },
    StatsFpsPending { ja: "FPS - (フレーム間隔の計測待ち)" },
    StatsDecodeUnknown { ja: "デコード -" },
    StatsNoFrame { ja: "映像フレームなし" },
    DragMoveEnabledNotice { ja: "ウィンドウを動かすため、画面ドラッグ移動を有効にしました" },
    FullscreenOn { ja: "フルスクリーン ON" },
    FullscreenOff { ja: "フルスクリーン OFF" },

    // ---- スクリーンショットの結果（app/screenshot.rs） ----
    ScreenshotCopied { ja: "クリップボードへコピーした" },
    ScreenshotNoDestination { ja: "出力先が 1 つも設定されていません" },
    ScreenshotNoFrame { ja: "表示中の映像がありません" },

    // ---- 設定ダイアログの操作の結果（app/settings_dialog.rs） ----
    AudioFileFilter { ja: "音声ファイル" },
    SettingsReadFailed { ja: "設定を読み取れない" },
    SettingsResetDone { ja: "初期値に戻しました。「適用」または「OK」で反映します" },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn text_every_key_has_non_empty_japanese() {
        for key in Text::ALL {
            let text = key.ja();
            assert!(!text.is_empty(), "{key:?} の日本語が空");
            // 前後の空白は並べる側（egui のレイアウト）の仕事。文言に混ぜると
            // 言語ごとに揃え方がばらつく
            assert_eq!(text, text.trim(), "{key:?} の前後に空白がある");
        }
    }

    #[test]
    fn text_japanese_is_not_duplicated_across_keys() {
        // 同じ文言に 2 つのキーがあると、片方だけ直して食い違う。
        // 同じ文言を別の場所で使うときはキーを使い回す
        let mut seen: HashMap<&str, Text> = HashMap::new();
        for key in Text::ALL {
            if let Some(previous) = seen.insert(key.ja(), *key) {
                panic!("{previous:?} と {key:?} が同じ文言「{}」", key.ja());
            }
        }
    }

    #[test]
    fn text_get_returns_japanese_by_default() {
        assert_eq!(Text::LinkConnected.get(), "接続中");
    }
}
