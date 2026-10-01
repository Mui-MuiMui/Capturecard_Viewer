//! cpal のストリームの組み立てと、入力のコールバック。リングバッファの型と、
//! 入出力で共通のエラーの扱いもここに置く。
//!
//! **コールバックの中ではロックもアロケーションもしない**
//! （`docs/design/audio.md`）。入力はリングバッファへ積むだけにしてある。
//! 出力側（`convert::PassthroughConverter` を通して書き戻す）は `stream_output.rs`。
//!
//! コールバック 1 回分の本体（`process_input` / `stream_output::process_output`）は
//! cpal のクロージャから切り離してあり、フェイクの入出力（`super::fake`）も
//! 同じものを呼ぶ。

use cpal::traits::DeviceTrait;
use cpal::Device;
use log::{error, warn};
use ringbuf::traits::{Observer, Producer};
use ringbuf::{HeapCons, HeapProd};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use super::tap::AudioTap;

/// リングバッファの内部表現は f32 に統一する。デバイス側のサンプル型は
/// 入力で f32 へ正規化し、出力で書き戻す。
pub(super) type AudioProducer = HeapProd<f32>;
pub(super) type AudioConsumer = HeapCons<f32>;

/// 入力コールバックがリングバッファの満杯で捨てたフレーム数を足す。
///
/// **入力コールバックから呼ぶので、ロックもアロケーションもしない**
/// （`docs/design/audio.md`）。`count_underrun` と同じく `u32::MAX` で頭打ちにする。
/// 0 フレームなら何もしない。
fn count_dropped_frames(counter: &AtomicU32, frames: usize) {
    if frames == 0 {
        return;
    }
    let add = u32::try_from(frames).unwrap_or(u32::MAX);
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(add))
    });
}

/// 渡されたサンプル数 `offered` のうち `pushed` だけ積めたとき、捨てたフレーム数。
/// 半端なサンプルはフレームに数えない。0ch は 1ch として扱う。
fn dropped_frames(offered: usize, pushed: usize, channels: usize) -> usize {
    offered.saturating_sub(pushed) / channels.max(1)
}

/// cpal が知らせた入力の取りこぼし（`Xrun`）を 1 回数える（Issue #377）。
///
/// **エラーのコールバックから呼ぶので、ロックもアロケーションもしない。**
/// `count_dropped_frames` と同じく `u32::MAX` で頭打ちにする。
fn count_xrun(counter: &AtomicU32) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(1))
    });
}

/// ストリームのエラーのうち、ストリームが動き続けていて開き直さなくてよいものか。
///
/// cpal 0.18 からは、止まったわけではない出来事もエラーのコールバックへ届く
/// （`docs/design/audio.md` の「cpal 0.18 で変わったこと」）。これを切断として
/// 開き直すと、そのたびに数百 ms 途切れる。
///
/// - `Xrun`: WASAPI の入力で取りこぼしの印（`AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY`）が
///   付いたとき（0.18.2 から）
/// - `RealtimeDenied`: 音声スレッドの優先度を上げられなかった。音は出る
/// - `DeviceChanged`: 既定のデバイスへ自動で経路を切り替えた。ストリームは動き続ける。
///   WASAPI では出ない（既定のデバイスが替わると `StreamInvalidated` が届く）
fn is_recoverable_stream_error(kind: cpal::ErrorKind) -> bool {
    matches!(
        kind,
        cpal::ErrorKind::Xrun | cpal::ErrorKind::RealtimeDenied | cpal::ErrorKind::DeviceChanged
    )
}

/// ストリームのエラーのコールバックの本体。開き直すべきものなら旗を立てる。
///
/// `direction` はログに出す「入力」「出力」。`Xrun` はログにも出さず、`xruns` が
/// あれば回数だけ数える（「接続状態」タブへ出す、Issue #377）。取りこぼしの
/// たびに届きうるうえ、WASAPI ではデータのコールバックと同じ音声スレッドから呼ばれる
/// ため、そこでロックやアロケーションをしたくない。出力は数えない（`None`）。
pub(super) fn handle_stream_error(
    direction: &str,
    e: &cpal::Error,
    stream_error: &AtomicBool,
    xruns: Option<&AtomicU32>,
) {
    match e.kind() {
        cpal::ErrorKind::Xrun => {
            if let Some(counter) = xruns {
                count_xrun(counter);
            }
        }
        kind if is_recoverable_stream_error(kind) => {
            warn!("{}ストリームの通知（開き直さない）: {}", direction, e);
        }
        _ => {
            error!("{}ストリームのエラー: {}", direction, e);
            stream_error.store(true, Ordering::Relaxed);
        }
    }
}

/// 入力ストリームを組み立てる。
///
/// `to_f32` でデバイスのサンプル型をリングバッファの表現（f32）へ正規化する。
/// `tap` は録画へ回す差し込み口（録画中だけ同じ値を積む）。
/// `dropped_frames` はリングバッファの満杯で捨てたフレーム数の数え手。
/// `xruns` は cpal が知らせた取りこぼし（`Xrun`）の回数の数え手。
// 引数はどれもストリームのクロージャへ move する部品で、束ねる型を作っても呼び出し側が組み立て直すだけなので許容する
#[allow(clippy::too_many_arguments)]
pub(super) fn build_input_stream_with<T>(
    device: &Device,
    config: &cpal::StreamConfig,
    producer: Arc<Mutex<AudioProducer>>,
    tap: AudioTap,
    stream_error: Arc<AtomicBool>,
    dropped_frames: Arc<AtomicU32>,
    xruns: Arc<AtomicU32>,
    to_f32: impl Fn(T) -> f32 + Send + 'static,
) -> Result<cpal::Stream, cpal::Error>
where
    T: cpal::SizedSample,
{
    let channels = usize::from(config.channels);
    device.build_input_stream(
        *config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            process_input(data, channels, &producer, &tap, &dropped_frames, &to_f32);
        },
        // 呼ばれるのは cpal のストリームスレッド。ここで開き直すと
        // ストリーム自身を drop することになるので、旗を立てるだけにする
        move |e| handle_stream_error("入力", &e, &stream_error, Some(&xruns)),
        None,
    )
}

/// 入力コールバック 1 回分の処理。デバイスのサンプルを f32 へ直して
/// リングバッファへ積む。録画中なら同じ値を録画のリング（`AudioTap`）にも積む。
///
/// **cpal の入力コールバックとフェイクの入力（`super::fake`）の両方から
/// 呼ぶ。** ロックは `try_lock` だけで、取れなければそのコールバック分を
/// 捨てる（待たない）。パススルーと録画は別々に判定し、片方を取れなくても
/// もう片方には積む。
///
/// **パススルーのリングバッファへはフレーム（`channels` サンプル）単位でだけ
/// 積む。** 溢れる分はフレームごと捨てる。1 サンプル単位で捨てると、捨てた
/// 位置から後ろが 1 つずれて左右が入れ替わったまま戻らない（`docs/design/audio.md`）。
/// 捨てたフレーム数は `dropped` に足す（「接続状態」タブへ出す、Issue #350）。
/// リングを握れずに捨てた分は数えない（満杯とは別の理由のため）。
///
/// 録画へは入力の形のまま積む。音量・ミュート・パススルーの無効は出力
/// コールバックの判定なので、録画には効かない（`docs/design/recording.md`）。
pub(super) fn process_input<T: Copy>(
    data: &[T],
    channels: usize,
    producer: &Mutex<AudioProducer>,
    tap: &AudioTap,
    dropped: &AtomicU32,
    to_f32: impl Fn(T) -> f32,
) {
    let mut passthrough = producer.try_lock().ok();
    let mut recording = tap.writer(data.len());
    if passthrough.is_none() && recording.is_none() {
        return;
    }
    // リングバッファへ積むサンプル数。空きに入るフレームの数だけで、半端な
    // フレームは積まない。空きは出力側が読むと増えるだけで減らないので、
    // ここで数えた分は必ず入る
    let mut room = passthrough.as_ref().map_or(0, |prod| {
        whole_frame_samples(prod.vacant_len().min(data.len()), channels)
    });
    if passthrough.is_some() {
        count_dropped_frames(dropped, dropped_frames(data.len(), room, channels));
    }
    for &sample in data {
        let value = to_f32(sample);
        if room > 0 {
            if let Some(prod) = passthrough.as_mut() {
                let _ = prod.try_push(value);
                room -= 1;
            }
        }
        if let Some(writer) = recording.as_mut() {
            writer.push(value);
        }
    }
}

/// `samples` サンプルのうち、丸ごと入るフレームのサンプル数（`channels` の倍数へ
/// 切り捨てる）。`channels` が 0 なら 1 として扱う（剰余で落とさないため）。
fn whole_frame_samples(samples: usize, channels: usize) -> usize {
    let channels = channels.max(1);
    samples / channels * channels
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::controls::AudioControls;
    use crate::audio::convert::PassthroughConverter;
    use crate::audio::sample::i16_to_f32;
    use crate::audio::stream_output::process_output;
    use crate::audio::tests::ring;
    use ringbuf::traits::Consumer;

    #[test]
    fn dropped_frames_counts_whole_frames_not_pushed() {
        assert_eq!(dropped_frames(8, 8, 2), 0);
        assert_eq!(dropped_frames(8, 4, 2), 2);
        assert_eq!(dropped_frames(8, 0, 2), 4);
        // 半端なサンプルはフレームに数えない
        assert_eq!(dropped_frames(5, 0, 2), 2);
        // 0ch は 1ch として扱う
        assert_eq!(dropped_frames(3, 1, 0), 2);
    }

    #[test]
    fn count_dropped_frames_adds_and_saturates() {
        let counter = AtomicU32::new(0);
        count_dropped_frames(&counter, 0);
        assert_eq!(counter.load(Ordering::Relaxed), 0);
        count_dropped_frames(&counter, 3);
        count_dropped_frames(&counter, 2);
        assert_eq!(counter.load(Ordering::Relaxed), 5);

        let counter = AtomicU32::new(u32::MAX - 1);
        count_dropped_frames(&counter, 10);
        assert_eq!(counter.load(Ordering::Relaxed), u32::MAX);
    }

    #[test]
    fn process_input_does_not_count_drops_when_the_ring_is_busy() {
        // 握れずに捨てた分は満杯とは別の理由なので数えない
        let (producer, _consumer) = ring(8);
        let dropped = AtomicU32::new(0);
        let held = producer.lock().expect("ロックできる");
        process_input(
            &[0.25f32, 0.5],
            2,
            &producer,
            &AudioTap::new(),
            &dropped,
            |s| s,
        );
        drop(held);
        assert_eq!(dropped.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn process_input_drops_whole_frames_when_the_ring_is_full() {
        // Issue #307。容量 3 のリングへ 2ch を 2 フレーム積む。入るのは 1 フレーム
        // だけで、2 フレーム目は丸ごと捨てる。修正前は 2 フレーム目の左だけが入り、
        // 以降が [2.0, 3.0] のように 1 つずれたフレームとして読まれていた
        let (producer, consumer) = ring(3);
        let controls = AudioControls::default();
        let underruns = AtomicU32::new(0);
        let mut converter = PassthroughConverter::new(48_000, 2, 48_000, 2);
        let tap = AudioTap::new();
        let mut data = [9.0f32; 2];

        let dropped = AtomicU32::new(0);
        process_input(
            &[1.0f32, -1.0, 2.0, -2.0],
            2,
            &producer,
            &tap,
            &dropped,
            |s| s,
        );
        assert_eq!(consumer.lock().expect("ロックできる").occupied_len(), 2);
        // 捨てた 2 フレーム目を数える（Issue #350）
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
        process_output(
            &mut data,
            &consumer,
            &controls,
            &mut converter,
            &underruns,
            |s| s,
        );
        assert_eq!(data, [1.0, -1.0]);

        process_input(
            &[3.0f32, -3.0],
            2,
            &producer,
            &tap,
            &AtomicU32::new(0),
            |s| s,
        );
        process_output(
            &mut data,
            &consumer,
            &controls,
            &mut converter,
            &underruns,
            |s| s,
        );
        assert_eq!(data, [3.0, -3.0]);
    }

    #[test]
    fn process_input_never_leaves_a_partial_frame_in_the_ring() {
        // 空きが 3 サンプルでも、積むのは 2ch の 1 フレーム（2 サンプル）まで
        let (producer, consumer) = ring(5);
        process_input(
            &[0.1f32, -0.1],
            2,
            &producer,
            &AudioTap::new(),
            &AtomicU32::new(0),
            |s| s,
        );

        process_input(
            &[0.2f32, -0.2, 0.3, -0.3],
            2,
            &producer,
            &AudioTap::new(),
            &AtomicU32::new(0),
            |s| s,
        );

        let mut read = [0.0f32; 5];
        let count = consumer.lock().expect("ロックできる").pop_slice(&mut read);
        assert_eq!(&read[..count], &[0.1, -0.1, 0.2, -0.2]);
    }

    #[test]
    fn whole_frame_samples_rounds_down_to_frames() {
        assert_eq!(whole_frame_samples(5, 2), 4);
        assert_eq!(whole_frame_samples(6, 2), 6);
        assert_eq!(whole_frame_samples(1, 2), 0);
        assert_eq!(whole_frame_samples(7, 6), 6);
        // 0ch は 1ch として扱い、剰余で落ちない
        assert_eq!(whole_frame_samples(3, 0), 3);
    }

    #[test]
    fn process_input_feeds_the_recording_tap_with_the_input_values() {
        // 録画は入力から取るので、音量・ミュートに関係なく元の値が入る
        let (producer, _consumer) = ring(8);
        let tap = AudioTap::new();
        let mut attachment = tap.attach(8);

        process_input(
            &[16_384i16, -32_768],
            2,
            &producer,
            &tap,
            &AtomicU32::new(0),
            i16_to_f32,
        );

        let mut read = [0.0f32; 4];
        let count = attachment.consumer.pop_slice(&mut read);
        assert_eq!(&read[..count], &[0.5, -1.0]);
        assert_eq!(tap.snapshot().samples_total, 2);
    }

    #[test]
    fn process_input_records_even_when_the_passthrough_ring_is_busy() {
        let (producer, _consumer) = ring(8);
        let tap = AudioTap::new();
        let mut attachment = tap.attach(8);

        // パススルーのリングを別の誰かが握っていても、録画には積む
        let held = producer.lock().expect("ロックできる");
        process_input(
            &[0.25f32, 0.5],
            2,
            &producer,
            &tap,
            &AtomicU32::new(0),
            |sample| sample,
        );
        drop(held);

        let mut read = [0.0f32; 4];
        assert_eq!(attachment.consumer.pop_slice(&mut read), 2);
        assert!(producer.lock().expect("ロックできる").is_empty());
    }

    #[test]
    fn is_recoverable_stream_error_keeps_running_streams() {
        // ストリームが動き続けている通知は開き直さない
        assert!(is_recoverable_stream_error(cpal::ErrorKind::Xrun));
        assert!(is_recoverable_stream_error(cpal::ErrorKind::RealtimeDenied));
        assert!(is_recoverable_stream_error(cpal::ErrorKind::DeviceChanged));
        // 止まった・使えなくなったものは開き直す
        assert!(!is_recoverable_stream_error(
            cpal::ErrorKind::StreamInvalidated
        ));
        assert!(!is_recoverable_stream_error(
            cpal::ErrorKind::DeviceNotAvailable
        ));
        assert!(!is_recoverable_stream_error(cpal::ErrorKind::BackendError));
        assert!(!is_recoverable_stream_error(cpal::ErrorKind::Other));
    }

    #[test]
    fn handle_stream_error_raises_flag_only_for_fatal_errors() {
        let flag = AtomicBool::new(false);
        handle_stream_error("入力", &cpal::ErrorKind::Xrun.into(), &flag, None);
        handle_stream_error("出力", &cpal::ErrorKind::RealtimeDenied.into(), &flag, None);
        assert!(!flag.load(Ordering::Relaxed));

        handle_stream_error(
            "出力",
            &cpal::ErrorKind::StreamInvalidated.into(),
            &flag,
            None,
        );
        assert!(flag.load(Ordering::Relaxed));
    }

    #[test]
    fn handle_stream_error_counts_only_xruns() {
        let flag = AtomicBool::new(false);
        let xruns = AtomicU32::new(0);
        handle_stream_error("入力", &cpal::ErrorKind::Xrun.into(), &flag, Some(&xruns));
        handle_stream_error("入力", &cpal::ErrorKind::Xrun.into(), &flag, Some(&xruns));
        handle_stream_error(
            "入力",
            &cpal::ErrorKind::RealtimeDenied.into(),
            &flag,
            Some(&xruns),
        );
        assert_eq!(xruns.load(Ordering::Relaxed), 2);
        // 取りこぼしは開き直さない
        assert!(!flag.load(Ordering::Relaxed));
    }

    #[test]
    fn count_xrun_saturates() {
        let counter = AtomicU32::new(u32::MAX - 1);
        count_xrun(&counter);
        count_xrun(&counter);
        assert_eq!(counter.load(Ordering::Relaxed), u32::MAX);
    }
}
