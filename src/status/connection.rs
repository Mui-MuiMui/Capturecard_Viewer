//! 「接続状態」タブと統計 OSD へ渡す観測値（`ConnectionStatus` / `LinkStatus`）と、
//! その行の組み立て（リサンプル・アンダーラン・捨てたフレーム数）。
//!
//! 失敗の記録（`ErrorCenter`）は `status/mod.rs` の側。外から使う経路
//! （`crate::status::...`）は `status/mod.rs` の `pub use` に集める。

use crate::audio::ResampleStatus;
use crate::i18n::{self, Text};

/// 音声のリサンプル状態を「接続状態」タブに出す行に組み立てる。
///
/// `DeviceSnapshot.audio_resample` をそのまま渡す。補正の共有状態が無い
/// `None` のときは「変換なし」の 1 行、あるときは比率と水位の 2 行を返す。
/// 音声を開いていれば入出力の形が揃っていても共有状態を作る（Issue #308）ので、
/// 開いている間は比率と水位が出る。
///
/// **目標水位が 0 のときは水位の割合を出さない。** `ResampleTelemetry` を
/// 作った時点で 0 になることは無いはずだが、割ってしまうと `NaN` になり
/// 表示が崩れるため、そのまま「バッファ水位: 不明」で逃がす。
pub fn format_resample_status(status: Option<ResampleStatus>) -> Vec<String> {
    let Some(status) = status else {
        return vec![Text::ResampleIdentity.get().to_string()];
    };

    let water_level_text = if status.target_level == 0 {
        Text::WaterLevelUnknown.get().to_string()
    } else {
        let percent = status.water_level as f64 / status.target_level as f64 * 100.0;
        format!("{:.0}%", percent)
    };

    vec![
        i18n::resample_ratio(status.ratio),
        i18n::buffer_water_level(water_level_text),
    ]
}

/// 音声のアンダーランの回数を 1 行に組み立てる。
///
/// **統計 OSD（`app::view`）と「接続状態」タブの両方がこれを呼ぶ。**
/// 同じ数を別の文言で出すと、どちらを見ているのか分からなくなるため。
///
/// `DeviceSnapshot.audio_underruns` をそのまま渡す。音声を開いていない
/// `None` のときは数を出さずに「-」にする。**0 と書かない。** 開いていて
/// 一度も途切れていない状態と区別が付かなくなるため。
pub fn format_underrun_count(count: Option<u32>) -> String {
    match count {
        Some(count) => i18n::underrun_count(count),
        None => Text::UnderrunUnknown.get().to_string(),
    }
}

/// 統計 OSD の音声の行。アンダーランの行（`format_underrun_count`）に、入力が
/// 映像デバイスの音声ピンなら「（映像デバイスの音声）」を添える（#394、#409）。経路が違うと
/// アンダーランの出方も変わりうるので、OSD だけを見て比べるときに取り違えないため。
/// 詳しい経路は「接続状態」タブに出す
pub fn format_osd_audio_line(underruns: Option<u32>, via_audio_pin: bool) -> String {
    let line = format_underrun_count(underruns);
    if via_audio_pin {
        i18n::via_audio_pin(&line)
    } else {
        line
    }
}

/// 「接続状態」タブへ出す、入力がリングバッファの満杯で捨てたフレーム数の行。
///
/// `DeviceSnapshot.audio_dropped_frames` をそのまま渡す。音声を開いていない
/// （`None`）ときは `format_underrun_count` と同じく「-」を出す（Issue #350）。
pub fn format_dropped_frame_count(count: Option<u32>) -> String {
    match count {
        Some(count) => i18n::dropped_frame_count(count),
        None => Text::DroppedFramesUnknown.get().to_string(),
    }
}

/// 映像か音声、片方の接続状態。設定ダイアログの「接続状態」タブへ渡す。
///
/// **デバイスワーカーが書き出した観測値（`DeviceSnapshot`）から作る。**
/// 描画中にデバイスへ問い合わせないようにするため。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkStatus {
    /// ストリームを開けているか
    pub connected: bool,
    /// 接続を追いかけている最中か（`ConnectRetry::is_active`）
    pub reconnecting: bool,
    /// 連続して失敗した回数
    pub attempts: u32,
    /// 実際に開いた内容の説明。行ごとに分けて渡す
    pub details: Vec<String>,
    /// 直近の失敗。`(整形済みの文言, 発生時刻)`
    pub error: Option<(String, String)>,
}

/// 映像と音声の接続状態。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectionStatus {
    /// 実機ではなくテスト用のフェイクデバイスで動いているか（#252）
    pub fake_devices: bool,
    pub video: LinkStatus,
    pub audio: LinkStatus,
    /// スクリーンショットの効果音の直近の失敗。`(整形済みの文言, 発生時刻)`。
    ///
    /// 繋ぎっぱなしのデバイスではないので接続中・未接続の見出しは持たない。
    /// 失敗が無ければ `None` で、タブにも枠を出さない（Issue #356）
    pub screenshot_sound_error: Option<(String, String)>,
    /// 起動時に設定ファイルを読めなかった知らせ（初期化した / 自動保存を止めた、
    /// Issue #505）。`(整形済みの文言, 発生時刻)`。
    ///
    /// `ErrorCenter` とは別に持つ。`ErrorSource::Settings` は直近の 1 件しか持たず、
    /// あとの保存の失敗に上書きされ、保存の成功で消えるため。「設定が初期化された」
    /// ことを後から読めるよう、自動保存を再開するまで残す
    pub settings_load_notice: Option<(String, String)>,
    /// 設定ファイルの直近の失敗（保存できない・書き出せないなど）。
    /// `(整形済みの文言, 発生時刻)`。`settings_load_notice` と同じものなら `None`。
    ///
    /// トーストは接続の失敗に上書きされうるので、ここで読み返せるようにする。
    /// 失敗も知らせも無ければ枠を出さない
    pub settings_error: Option<(String, String)>,
}

/// フェイクデバイスで動いているときに出す知らせ。実機なら `None`。
///
/// 起動時のトーストと「接続状態」タブの先頭の両方がこれを使う。
pub fn fake_devices_notice(fake_devices: bool) -> Option<&'static str> {
    fake_devices.then(|| Text::FakeDevicesNotice.get())
}

impl LinkStatus {
    /// 状態を 1 行で表す見出し。
    pub fn headline(&self) -> &'static str {
        let text = match (self.connected, self.reconnecting) {
            (true, _) => Text::LinkConnected,
            (false, true) => Text::LinkReconnecting,
            (false, false) => Text::LinkDisconnected,
        };
        text.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_devices_notice_only_when_fake() {
        assert_eq!(super::fake_devices_notice(false), None);
        let notice = super::fake_devices_notice(true).expect("フェイクなら出る");
        assert!(notice.contains("CAPTURECARD_VIEWER_FAKE_DEVICES"));
    }
    #[test]
    fn link_status_headline_reflects_the_three_states() {
        let connected = LinkStatus {
            connected: true,
            ..LinkStatus::default()
        };
        let reconnecting = LinkStatus {
            connected: false,
            reconnecting: true,
            ..LinkStatus::default()
        };
        let idle = LinkStatus::default();

        assert_eq!(connected.headline(), "接続中");
        assert_eq!(reconnecting.headline(), "未接続（再接続を試しています）");
        assert_eq!(idle.headline(), "未接続");
    }

    #[test]
    fn format_resample_status_with_none_returns_no_conversion() {
        assert_eq!(format_resample_status(None), vec!["変換なし".to_string()]);
    }

    #[test]
    fn format_resample_status_with_a_correction_returns_ratio_and_water_level() {
        let status = ResampleStatus {
            ratio: 1.0003,
            water_level: 52,
            target_level: 100,
        };
        assert_eq!(
            format_resample_status(Some(status)),
            vec![
                "リサンプル比: 1.0003".to_string(),
                "バッファ水位: 52%".to_string(),
            ]
        );
    }

    #[test]
    fn format_resample_status_rounds_the_water_level_percentage() {
        let status = ResampleStatus {
            ratio: 0.9998,
            water_level: 1,
            target_level: 3,
        };
        assert_eq!(
            format_resample_status(Some(status)),
            vec![
                "リサンプル比: 0.9998".to_string(),
                "バッファ水位: 33%".to_string(),
            ]
        );
    }

    #[test]
    fn format_resample_status_with_a_zero_target_level_shows_unknown() {
        // ResampleTelemetry を作った時点で 0 になることは無いはずだが、
        // 割り算で NaN になるのを避けて安全に倒す
        let status = ResampleStatus {
            ratio: 1.0,
            water_level: 0,
            target_level: 0,
        };
        assert_eq!(
            format_resample_status(Some(status)),
            vec![
                "リサンプル比: 1.0000".to_string(),
                "バッファ水位: 不明".to_string(),
            ]
        );
    }

    #[test]
    fn format_underrun_count_with_a_count_shows_the_number() {
        assert_eq!(format_underrun_count(Some(0)), "アンダーラン: 0 回");
        assert_eq!(format_underrun_count(Some(12)), "アンダーラン: 12 回");
    }

    #[test]
    fn format_underrun_count_without_audio_shows_a_dash() {
        // 音声を開いていない間に 0 と出すと、開いていて一度も途切れて
        // いない状態と読み分けられない
        assert_eq!(format_underrun_count(None), "アンダーラン: -");
    }

    #[test]
    fn format_osd_audio_line_marks_the_audio_pin() {
        assert_eq!(
            format_osd_audio_line(Some(3), true),
            "アンダーラン: 3 回（映像デバイスの音声）"
        );
        assert_eq!(format_osd_audio_line(Some(3), false), "アンダーラン: 3 回");
    }

    #[test]
    fn format_dropped_frame_count_shows_the_number_or_a_dash() {
        assert_eq!(
            format_dropped_frame_count(Some(0)),
            "満杯で捨てた: 0 フレーム"
        );
        assert_eq!(
            format_dropped_frame_count(Some(480)),
            "満杯で捨てた: 480 フレーム"
        );
        assert_eq!(format_dropped_frame_count(None), "満杯で捨てた: -");
    }
}
