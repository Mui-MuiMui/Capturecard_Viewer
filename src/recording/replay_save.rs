//! リプレイバッファの中身だけを保存する操作（#438）の、録画との違いと無効の判定。
//!
//! 保存そのものは③の録画と同じ `super::replay_recording::ReplayRecording` が行う
//! （`super::replay::ReplayPipeline` の同じ口に入れ、押した時刻で止める）。ここに置くのは、
//! 録画と保存のどちらかを表す `RecordingKind` と、保存できない理由 `SaveReplayBlock`、
//! UI の控えから理由を決める純粋関数 `save_replay_block` だけ。
//! 設計は `docs/design/recording.md` の「リプレイだけを保存する（#438）」。

use std::fmt;
use std::time::Duration;

use super::recorder::{RecordingEvent, RecordingSummary};
use super::RecordingError;
use crate::i18n::Text;

/// リプレイバッファを通す 1 回の書き出しが、録画か、リプレイバッファの中身だけの保存か。
///
/// 書き出し方は同じで、違うのは結果のイベントの種類だけ（保存は UI の録画の控えを触らせない
/// ため `Stopped` / `Failed` ではなく `ReplaySaved` / `ReplaySaveFailed` で返す）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RecordingKind {
    /// 録画（開始から停止まで。リングの中身を先頭に入れてライブを続けて書く）
    Recording,
    /// リプレイバッファの中身だけを保存する（押した時刻で止める）
    SaveReplay,
}

impl RecordingKind {
    /// 閉じたときのイベント。
    pub(super) fn saved(self, summary: RecordingSummary) -> RecordingEvent {
        match self {
            RecordingKind::Recording => RecordingEvent::Stopped(summary),
            RecordingKind::SaveReplay => RecordingEvent::ReplaySaved(summary),
        }
    }

    /// 始められなかった、または途中で止まったときのイベント。
    pub(super) fn failed(
        self,
        error: RecordingError,
        summary: Option<RecordingSummary>,
    ) -> RecordingEvent {
        match self {
            RecordingKind::Recording => RecordingEvent::Failed { error, summary },
            RecordingKind::SaveReplay => RecordingEvent::ReplaySaveFailed { error, summary },
        }
    }
}

/// リプレイを保存できない理由。右クリックメニューのホバーとホットキーのトーストに出す。
///
/// **文言はこの型の `Display` が `crate::i18n` から引く。** 失敗ではなく「いまは押せない」
/// 状態なので、`RecordingError` ではなく別の型にして `report_error` を通さない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveReplayBlock {
    /// リプレイバッファが OFF（または用意できずに止まっている）
    ReplayOff,
    /// 録画中（`Finalize` を待っている間も含む）
    Recording,
    /// 前の保存がまだ終わっていない
    Saving,
    /// リングに映像がまだ溜まっていない（ON にした直後、映像が来ていない）
    Empty,
}

impl fmt::Display for SaveReplayBlock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            SaveReplayBlock::ReplayOff => Text::SaveReplayOff,
            SaveReplayBlock::Recording => Text::SaveReplayWhileRecording,
            SaveReplayBlock::Saving => Text::SaveReplaySaving,
            SaveReplayBlock::Empty => Text::SaveReplayEmpty,
        };
        f.write_str(text.get())
    }
}

/// リプレイを保存できるか。できなければ理由を返す。
///
/// - `replay_on`: 窓口が最後に送ったリプレイバッファの設定が ON か
/// - `held`: リングが持っている映像の長さ（`RecordingTelemetry` の値）
/// - `recording`: 録画中か（`Finalize` を待っている間も含む）
/// - `saving`: 前の保存の結果がまだ届いていないか
///
/// 当てはまるものが複数あれば、表の上（直し方が根本的なもの）を返す。OFF なら ON にする
/// しかなく、録画中なら止めるしかない。保存中と溜まっていないのは待てば直る。
pub(super) fn save_replay_block(
    replay_on: bool,
    held: Duration,
    recording: bool,
    saving: bool,
) -> Option<SaveReplayBlock> {
    if !replay_on {
        Some(SaveReplayBlock::ReplayOff)
    } else if recording {
        Some(SaveReplayBlock::Recording)
    } else if saving {
        Some(SaveReplayBlock::Saving)
    } else if held.is_zero() {
        Some(SaveReplayBlock::Empty)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::AudioTap;
    use crate::i18n::{with_language, Language};
    use crate::recording::recorder::Recorder;
    use crate::recording::test_support::{
        poll_for, poll_until, read_mp4, recording_request, replay_config, start_fakes,
    };
    use crate::video::VideoTap;

    const HELD: Duration = Duration::from_secs(20);

    #[test]
    fn save_replay_block_allows_saving_when_the_ring_has_footage() {
        assert_eq!(save_replay_block(true, HELD, false, false), None);
        // 1ms でも溜まっていれば保存できる（キーフレームの有無は録画スレッドが見る）
        assert_eq!(
            save_replay_block(true, Duration::from_millis(1), false, false),
            None
        );
    }

    #[test]
    fn save_replay_block_reports_each_reason() {
        assert_eq!(
            save_replay_block(false, HELD, false, false),
            Some(SaveReplayBlock::ReplayOff)
        );
        assert_eq!(
            save_replay_block(true, HELD, true, false),
            Some(SaveReplayBlock::Recording)
        );
        assert_eq!(
            save_replay_block(true, HELD, false, true),
            Some(SaveReplayBlock::Saving)
        );
        assert_eq!(
            save_replay_block(true, Duration::ZERO, false, false),
            Some(SaveReplayBlock::Empty)
        );
    }

    #[test]
    fn save_replay_block_prefers_the_fundamental_reason() {
        // OFF は他の何よりも先に出す（ON にしない限り直らない）
        assert_eq!(
            save_replay_block(false, Duration::ZERO, true, true),
            Some(SaveReplayBlock::ReplayOff)
        );
        // 録画中は保存中・溜まっていないより先
        assert_eq!(
            save_replay_block(true, Duration::ZERO, true, true),
            Some(SaveReplayBlock::Recording)
        );
        assert_eq!(
            save_replay_block(true, Duration::ZERO, false, true),
            Some(SaveReplayBlock::Saving)
        );
    }

    #[test]
    fn recording_kind_picks_the_event_for_each_kind() {
        let summary = RecordingSummary {
            path: "a.mp4".into(),
            duration: Duration::from_secs(20),
            frames_written: 1200,
            frames_dropped: 0,
            recycle_misses: 0,
            replay_lead: Some(Duration::from_secs(20)),
            audio_drift: None,
        };
        assert_eq!(
            RecordingKind::Recording.saved(summary.clone()),
            RecordingEvent::Stopped(summary.clone())
        );
        assert_eq!(
            RecordingKind::SaveReplay.saved(summary.clone()),
            RecordingEvent::ReplaySaved(summary)
        );
        assert_eq!(
            RecordingKind::SaveReplay.failed(RecordingError::NoVideo, None),
            RecordingEvent::ReplaySaveFailed {
                error: RecordingError::NoVideo,
                summary: None
            }
        );
        assert_eq!(
            RecordingKind::Recording.failed(RecordingError::NoVideo, None),
            RecordingEvent::Failed {
                error: RecordingError::NoVideo,
                summary: None
            }
        );
    }

    #[test]
    fn save_replay_block_display_is_localized() {
        for block in [
            SaveReplayBlock::ReplayOff,
            SaveReplayBlock::Recording,
            SaveReplayBlock::Saving,
            SaveReplayBlock::Empty,
        ] {
            let english = with_language(Language::English, || block.to_string());
            assert!(english.is_ascii(), "{english}");
            let japanese = with_language(Language::Japanese, || block.to_string());
            assert!(!japanese.is_ascii(), "{japanese}");
        }
    }

    // #438: リプレイバッファが OFF なら保存を頼んでも何もしない（スレッドも起こさない）
    #[test]
    fn recorder_save_replay_without_replay_does_nothing() {
        let mut recorder = Recorder::new(VideoTap::new(), AudioTap::new());
        assert_eq!(
            recorder.save_replay_block(),
            Some(SaveReplayBlock::ReplayOff)
        );
        let dir = tempfile::tempdir().expect("一時ディレクトリを作れること");
        assert!(recorder
            .save_replay(recording_request(dir.path(), "replay"))
            .is_ok());
        assert!(!recorder.is_saving_replay());
        assert!(!recorder.has_thread());
    }

    /// リプレイの保存の結果を待つ。保存できたらその内容。
    fn wait_for_saved_replay(recorder: &mut Recorder) -> RecordingSummary {
        poll_until(recorder, Duration::from_secs(10), |event| match event {
            RecordingEvent::ReplaySaved(summary) => Some(summary.clone()),
            RecordingEvent::ReplaySaveFailed { .. } | RecordingEvent::ReplaySaveRefused(_) => {
                panic!("リプレイを保存できなかった: {event:?}")
            }
            _ => None,
        })
    }

    #[test]
    #[ignore = "Media Foundation の H.264 / AAC エンコーダが必要。30 秒ほどかかる"]
    fn recorder_save_replay_writes_only_the_buffered_footage() {
        // 実行: cargo test -- --ignored recorder_save_replay_writes_only_the_buffered_footage
        //
        // 30 秒で ON にして 20 秒待ち、リプレイだけを保存する（#438）。溜まっているのは
        // 20 秒ぶんなので約 20 秒の MP4 になり、押した時刻で止まる（さかのぼり ≒ 長さ）。
        // 続けて押せば 2 本目ができ（リングは捨てない）、録画中は録画スレッドも保存しない
        let frames = crate::video::VideoFrames::new();
        let audio_tap = AudioTap::new();
        let (mut video, mut audio) = start_fakes(&frames, &audio_tap);
        let dir = tempfile::tempdir().expect("一時ディレクトリを作れること");
        let mut recorder = Recorder::new(frames.tap(), audio_tap);
        recorder
            .set_replay(Some(replay_config(30)))
            .expect("リプレイバッファを始められる");
        // ON にした直後はまだ溜まっていない
        assert_eq!(recorder.save_replay_block(), Some(SaveReplayBlock::Empty));
        poll_for(&mut recorder, Duration::from_secs(20));
        assert_eq!(recorder.save_replay_block(), None);

        recorder
            .save_replay(recording_request(dir.path(), "replay"))
            .expect("保存を頼める");
        assert!(recorder.is_saving_replay());
        assert_eq!(recorder.save_replay_block(), Some(SaveReplayBlock::Saving));
        // 保存中は録画を始めない（書き出しの口は 1 つ）
        recorder
            .start(recording_request(dir.path(), "recording"))
            .expect("頼むこと自体はできる");
        assert!(!recorder.is_recording());
        let first = wait_for_saved_replay(&mut recorder);
        assert!(!recorder.is_saving_replay());

        // リングは捨てていないので、続けて押せばすぐ 2 本目を保存できる（同じ名前なので _2）
        assert_eq!(recorder.save_replay_block(), None);
        recorder
            .save_replay(recording_request(dir.path(), "replay"))
            .expect("保存を頼める");
        let second = wait_for_saved_replay(&mut recorder);
        assert_ne!(first.path, second.path);

        // 録画中は窓口が断る。窓口を通さずに送っても、録画スレッドが断る
        recorder
            .start(recording_request(dir.path(), "recording"))
            .expect("録画を始められる");
        poll_for(&mut recorder, Duration::from_secs(1));
        assert_eq!(
            recorder.save_replay_block(),
            Some(SaveReplayBlock::Recording)
        );
        recorder
            .send_save_replay_unchecked(recording_request(dir.path(), "replay"))
            .expect("送れる");
        let refused = poll_until(&mut recorder, Duration::from_secs(3), |event| match event {
            RecordingEvent::ReplaySaveRefused(block) => Some(*block),
            _ => None,
        });
        assert_eq!(refused, SaveReplayBlock::Recording);
        recorder.request_stop();
        let recorded = poll_until(&mut recorder, Duration::from_secs(5), |event| match event {
            RecordingEvent::Stopped(summary) => Some(summary.clone()),
            _ => None,
        });
        recorder.shutdown();
        video.stop_capture();
        audio.stop_capture();

        for summary in [&first, &second] {
            let mp4 = read_mp4(&summary.path);
            println!(
                "保存したリプレイ: 長さ {:.2} 秒、さかのぼり {:?}、書いた {} 枚、先頭の時刻 {}",
                mp4.duration as f64 / 1e7,
                summary.replay_lead,
                summary.frames_written,
                mp4.first_video_time
            );
            assert!(mp4.has_audio, "{summary:?}");
            assert!(mp4.first_video_time.abs() < 10_000, "{summary:?}");
            assert!(
                (180_000_000..=240_000_000).contains(&mp4.duration),
                "長さ {}（{summary:?}）",
                mp4.duration
            );
            // 押した時刻で止めたので、さかのぼった長さとファイルの長さがほぼ同じ
            let lead = summary.replay_lead.expect("さかのぼった");
            let gap = lead.abs_diff(summary.duration);
            assert!(gap < Duration::from_millis(500), "{summary:?}");
        }
        // 保存した 2 本と録画の 1 本だけ。録画中の保存はファイルを作っていない
        let files = std::fs::read_dir(dir.path())
            .expect("保存先を読める")
            .count();
        assert_eq!(files, 3, "{first:?} {second:?} {recorded:?}");
    }
}
