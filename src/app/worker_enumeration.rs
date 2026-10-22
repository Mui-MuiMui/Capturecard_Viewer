//! デバイスワーカーが行う列挙のログと、「Windows 側にも見えていない」の判定の適用（#236、#506）。
//!
//! どれもデバイスワーカースレッド（`super::worker_loop`）の上でだけ走り、`WorkerState` に
//! 生やす形にしてある（`super::worker_connect` と同じ）。出すかどうか・見えていないかの
//! 判定そのものは `super::monitor` の純粋関数で、ここはそれを呼んでログと UI への知らせに
//! 直す。ログの文言と出すレベルの決め方（`enumeration_text` / `enumeration_log_level`）もここ。
//!
//! **接続の試行とは切り離してある。** 再接続の前に列挙しない決まり
//! （`docs/design/reconnect.md`）はそのままで、列挙は試行のあとに間引いて行う。

use super::monitor::{decide_device_not_visible, should_log_enumeration, DeviceNotVisible};
use super::retry::is_retry_log_milestone;
use super::worker::DeviceEvent;
use super::worker_loop::WorkerState;
use crate::audio::AudioDirection;
use crate::i18n;
use crate::settings::AudioInputSource;
use log::{debug, log, warn, Level};
use std::fmt::Display;

/// 接続の失敗を UI へ渡す 1 行。「Windows 側にも見えていない」と判定済みなら
/// 案内を添える。
pub(super) fn failure_message(not_visible: Option<&DeviceNotVisible>, reason: &str) -> String {
    match not_visible {
        Some(notice) => i18n::failure_with_device_not_visible(notice, reason),
        None => reason.to_string(),
    }
}

/// 列挙の結果 1 経路ぶんを、ログに出す文言と、列挙できたかに直す。
/// **台数と名前を必ず並べる。** 0 台なのか、名前が食い違っているのかを
/// ログだけで見分けるため。前回出したものと比べるのにもこの文言を使う。
pub(super) fn enumeration_text<E: Display>(result: &Result<Vec<String>, E>) -> (String, bool) {
    match result {
        Ok(names) => (format!("{} 台 {:?}", names.len(), names), true),
        Err(e) => (e.to_string(), false),
    }
}

/// 接続の失敗が続く間の列挙のログ 1 経路ぶんを出すレベル（#506）。再試行のログの
/// 節目の回か、前回出したものから変わったときは INFO（列挙できなければ WARN）、
/// それ以外は DEBUG。
///
/// **変わったら節目を待たずに出す。** 長く抜けていたボードを挿し直して OS 側に
/// 戻ったのが、100 回ごとの節目まで（8 分強）ログから読めないと困るため。
pub(super) fn enumeration_log_level(listed: bool, milestone: bool, changed: bool) -> Level {
    match (milestone || changed, listed) {
        (false, _) => Level::Debug,
        (true, true) => Level::Info,
        (true, false) => Level::Warn,
    }
}

impl WorkerState {
    /// 列挙の結果を 1 経路ぶんだけログへ出し、出したものを覚えておく。
    fn log_listing<E: Display>(
        &mut self,
        milestone: bool,
        trigger: &str,
        source: &'static str,
        result: &Result<Vec<String>, E>,
    ) {
        let (text, listed) = enumeration_text(result);
        let changed = self.last_enumeration_listing.get(source) != Some(&text);
        let level = enumeration_log_level(listed, milestone, changed);
        let what = if listed {
            "デバイスの列挙"
        } else {
            "デバイスを列挙できない"
        };
        log!(level, "{}（{}）: {}: {}", what, trigger, source, text);
        self.last_enumeration_listing.insert(source, text);
    }

    /// 列挙の結果をログへ出し、設定のデバイスが Windows 側にも見えていないかを
    /// 判定する（#236）。`tick` のたびに呼ばれ、出す回でなければ何もしない。
    ///
    /// 出すのは起動時の 1 回と、接続の失敗が節目（5 回目・10 回目・以後 10 回
    /// ごと、`monitor::is_enumeration_milestone`）に届いた回。Media Foundation /
    /// DirectShow / 音声入力 / 音声出力を 1 行ずつ、台数と名前を並べる。
    /// INFO で出すのは起動時と、再試行のログの節目（`retry::is_retry_log_milestone`）と、
    /// 前回出したものから変わった経路だけで、残りは DEBUG に落とす（#506）。
    ///
    /// **接続の試行とは切り離してある。** 再接続の前に列挙しない決まり
    /// （`docs/design/reconnect.md`）はそのままで、これは試行のあとに、ログと
    /// 判定のためだけに間引いて行う。音声の列挙は実測 300ms 前後かかるので、
    /// 毎回やると再試行の間隔を食う。
    pub(super) fn log_device_enumeration(&mut self) {
        let video_failures = self.video_retry.attempts();
        let audio_failures = self.audio_retry.attempts();
        // 繋がって 0 へ戻ったら記録も戻す。また失敗が続いたら同じ節目で出す
        if video_failures == 0 {
            self.enumeration_logged_failures.0 = 0;
        }
        if audio_failures == 0 {
            self.enumeration_logged_failures.1 = 0;
        }
        let (video_logged, audio_logged) = self.enumeration_logged_failures;
        let video_due = should_log_enumeration(video_failures, video_logged);
        let audio_due = should_log_enumeration(audio_failures, audio_logged);
        if !self.startup_enumeration_pending && !video_due && !audio_due {
            return;
        }
        let Some(config) = self.config.clone() else {
            return;
        };
        let trigger = if self.startup_enumeration_pending {
            "起動時".to_string()
        } else {
            format!(
                "接続の失敗が続いている。映像 {} 回、音声 {} 回",
                video_failures, audio_failures
            )
        };
        // 列挙と判定はこれまでどおり 10 回ごとに行い、INFO で出すのは再試行の
        // ログと同じ節目に絞る（#506）。起動時は前回の記録が無いので必ず出る
        let milestone = (video_due && is_retry_log_milestone(video_failures))
            || (audio_due && is_retry_log_milestone(audio_failures));
        self.startup_enumeration_pending = false;
        self.enumeration_logged_failures = (video_failures, audio_failures);

        let video = self.video.enumerate();
        for (source, result) in &video.sources {
            self.log_listing(milestone, &trigger, source, result);
        }
        let input = self.audio.enumerate_devices(AudioDirection::Input);
        self.log_listing(milestone, &trigger, "音声入力", &input);
        let output = self.audio.enumerate_devices(AudioDirection::Output);
        self.log_listing(milestone, &trigger, "音声出力", &output);

        let video_notice = decide_device_not_visible(
            video_failures,
            config.video.0.as_deref(),
            video.selectable.as_deref(),
        );
        // 入力を先に見る。キャプチャーボードの音声は入力側に出る。
        // **入出力のどちらかの列挙に失敗したら、音声は判定しない。** 映像で
        // 失敗した経路があれば判定しないのと同じで、開けない理由が失敗した側に
        // あったかもしれない
        // 入力が映像デバイスの音声ピンなら、入力は判定しない。比べる WASAPI の名前が無い（#388）
        let configured_input = match config.audio.5 {
            AudioInputSource::Device => config.audio.0.as_deref(),
            AudioInputSource::VideoPin => None,
        };
        let audio_notice = match (input.as_deref(), output.as_deref()) {
            (Ok(input), Ok(output)) => decide_device_not_visible(
                audio_failures,
                configured_input,
                Some(input),
            )
            .or_else(|| {
                decide_device_not_visible(audio_failures, config.audio.1.as_deref(), Some(output))
            }),
            _ => None,
        };
        self.set_video_not_visible(video_notice);
        self.set_audio_not_visible(audio_notice);
    }

    /// 映像の「Windows 側にも見えていない」を更新する。
    ///
    /// 新しく付いたら、直近の失敗の理由に添えて**その場で出し直す。**
    /// 次の失敗は最大 5 秒後で、それを待つと案内が遅れる。
    fn set_video_not_visible(&mut self, notice: Option<DeviceNotVisible>) {
        if notice == self.video_not_visible {
            return;
        }
        match &notice {
            Some(notice) => warn!(
                "設定の映像デバイスが Windows 側にも見えていない: {:?}",
                notice
            ),
            None => debug!("設定の映像デバイスが列挙に戻った"),
        }
        self.video_not_visible = notice;
        if let (Some(notice), Some(reason)) = (&self.video_not_visible, &self.last_video_failure) {
            let message = failure_message(Some(notice), reason);
            self.emit(DeviceEvent::VideoFailed(message));
        }
    }

    /// 音声の「Windows 側にも見えていない」を更新する。映像と同じ扱い。
    fn set_audio_not_visible(&mut self, notice: Option<DeviceNotVisible>) {
        if notice == self.audio_not_visible {
            return;
        }
        match &notice {
            Some(notice) => warn!(
                "設定の音声デバイスが Windows 側にも見えていない: {:?}",
                notice
            ),
            None => debug!("設定の音声デバイスが列挙に戻った"),
        }
        self.audio_not_visible = notice;
        if let (Some(notice), Some(reason)) = (&self.audio_not_visible, &self.last_audio_failure) {
            let message = failure_message(Some(notice), reason);
            self.emit(DeviceEvent::AudioFailed(message));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::backend::mock::{MockAudioBackend, MockVideoBackend};
    use super::super::worker_loop::testing::{apply_config, config_for, drain, mock_state};
    use super::*;
    use std::time::{Duration, Instant};

    // 「Windows 側にも見えていない」の案内（#236）。モックの列挙結果で
    // 設定の名前が無い状態を作り、`tick` を回して失敗の理由に案内が付くかを見る。
    // 時刻はバックオフの上限（5 秒）より大きく進めて、毎回 1 回ずつ試させる

    /// 映像の失敗の理由を、届いた順に取り出す。
    fn video_failures(events: &[DeviceEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                DeviceEvent::VideoFailed(reason) => Some(reason.clone()),
                _ => None,
            })
            .collect()
    }

    /// 映像がずっと失敗するモックで、`ticks` 回だけ接続を試させる。
    fn run_failing_video(
        listed: Vec<(String, String)>,
        ticks: u32,
    ) -> (Vec<DeviceEvent>, MockVideoBackend) {
        let video = MockVideoBackend::default();
        let audio = MockAudioBackend::default();
        video.with(|state| {
            state.failures_before_success = u32::MAX;
            state.devices = listed;
        });
        audio.with(|state| state.input_devices = vec!["モック入力".to_string()]);
        let (mut state, events) = mock_state(&video, &audio);
        apply_config(
            &mut state,
            config_for(Some("キャプチャーボード"), Some("モック入力")),
            true,
        );

        let base = Instant::now();
        for step in 0..ticks {
            state.tick(base + Duration::from_secs(6) * step);
        }
        (drain(&events), video)
    }

    #[test]
    fn worker_notices_a_video_device_missing_from_the_enumeration_after_5_failures() {
        let (events, video) = run_failing_video(vec![("Web カメラ".to_string(), String::new())], 5);
        assert_eq!(video.with(|state| state.start_calls), 5);

        let reasons = video_failures(&events);
        // 4 回目までは素の理由だけ。抜いた直後は見えていなくて当然
        assert!(
            reasons[..4]
                .iter()
                .all(|reason| !reason.contains("Windows 側にも")),
            "{reasons:?}"
        );
        // 5 回目の列挙で判定し、次の失敗を待たずに案内付きで出し直す
        let last = reasons.last().expect("失敗が届いていること");
        assert!(
            last.contains("'キャプチャーボード' が Windows 側にも"),
            "{last}"
        );
        assert!(last.contains("ほかのデバイスは見えています"), "{last}");
        // 元の理由も残す
        assert!(last.contains("見つからない"), "{last}");
    }

    #[test]
    fn worker_notices_no_devices_at_all_with_a_different_message() {
        let (events, _video) = run_failing_video(Vec::new(), 5);
        let reasons = video_failures(&events);
        let last = reasons.last().expect("失敗が届いていること");
        assert!(last.contains("1 台も見えていません"), "{last}");
    }

    #[test]
    fn worker_keeps_the_notice_on_later_failures() {
        // 一度付いた案内は、以後の失敗にも添え続ける（繋がるまで）
        let (events, _video) = run_failing_video(Vec::new(), 7);
        let reasons = video_failures(&events);
        assert!(
            reasons
                .last()
                .is_some_and(|reason| reason.contains("Windows 側にも")),
            "{reasons:?}"
        );
    }

    #[test]
    fn worker_does_not_notice_a_device_that_the_enumeration_lists() {
        // OS には見えているのに開けない。デバイスマネージャーを案内しても的外れ
        let (events, _video) =
            run_failing_video(vec![("キャプチャーボード".to_string(), String::new())], 10);
        assert!(
            video_failures(&events)
                .iter()
                .all(|reason| !reason.contains("Windows 側にも")),
            "列挙に居るなら案内しないこと"
        );
    }

    #[test]
    fn worker_logs_the_enumeration_once_per_milestone() {
        // tick は 100ms ごとに回るが、同じ失敗回数のまま列挙し直さない
        let video = MockVideoBackend::default();
        let audio = MockAudioBackend::default();
        video.with(|state| state.failures_before_success = u32::MAX);
        let (mut state, _events) = mock_state(&video, &audio);
        apply_config(
            &mut state,
            config_for(Some("キャプチャーボード"), None),
            true,
        );

        let base = Instant::now();
        state.tick(base);
        assert!(
            !state.startup_enumeration_pending,
            "起動時の列挙は最初の tick で済ませること"
        );
        // 出した結果を経路ごとに覚え、次の回はそれと比べて DEBUG へ落とすか決める（#506）
        let logged = state.last_enumeration_listing.get("音声入力");
        assert_eq!(logged.map(String::as_str), Some("0 台 []"));
        for step in 1..5 {
            state.tick(base + Duration::from_secs(6) * step);
        }
        assert_eq!(state.enumeration_logged_failures.0, 5);
        // 同じ 5 回のまま tick だけ進んでも、記録は変わらない
        state.tick(base + Duration::from_secs(24) + Duration::from_millis(100));
        assert_eq!(state.enumeration_logged_failures.0, 5);
    }

    #[test]
    fn failure_message_puts_the_notice_first() {
        // トーストは 60 文字で切るので、案内を前に置く
        let message = failure_message(Some(&DeviceNotVisible::NoDevices), "理由");
        assert!(message.starts_with("Windows 側にも"), "{message}");
        assert!(message.ends_with("（理由）"), "{message}");
        assert_eq!(failure_message(None, "理由"), "理由");
    }

    #[test]
    fn enumeration_log_level_is_debug_only_when_nothing_moved() {
        // 節目でも変化でもない回は DEBUG。どちらかなら、列挙できたら INFO・できなければ WARN
        assert_eq!(enumeration_log_level(true, false, false), Level::Debug);
        assert_eq!(enumeration_log_level(false, false, false), Level::Debug);
        assert_eq!(enumeration_log_level(true, true, false), Level::Info);
        assert_eq!(enumeration_log_level(true, false, true), Level::Info);
        assert_eq!(enumeration_log_level(false, true, false), Level::Warn);
        assert_eq!(enumeration_log_level(false, false, true), Level::Warn);
    }

    #[test]
    fn enumeration_text_shows_the_count_and_the_names() {
        let listed: Result<Vec<String>, String> = Ok(vec!["A".to_string(), "B".to_string()]);
        assert_eq!(
            enumeration_text(&listed),
            ("2 台 [\"A\", \"B\"]".to_string(), true)
        );
        let failed: Result<Vec<String>, String> = Err("失敗".to_string());
        assert_eq!(enumeration_text(&failed), ("失敗".to_string(), false));
    }
}
