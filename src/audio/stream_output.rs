//! cpal の出力ストリームの組み立てと、出力のコールバック。
//!
//! **コールバックの中ではロックもアロケーションもしない**
//! （`docs/design/audio.md`）。出力はリングバッファから取り出し、
//! `convert::PassthroughConverter` を通して書き戻すだけにしてある。
//!
//! コールバック 1 回分の本体（`process_output`）は cpal のクロージャから
//! 切り離してあり、フェイクの出力（`super::fake`）も同じものを呼ぶ。
//! 入力側とリングバッファの型、エラーの扱いは `stream.rs`。

use cpal::traits::DeviceTrait;
use cpal::Device;
use ringbuf::traits::{Consumer, Observer};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use super::controls::{load_volume, AudioControls};
use super::convert::PassthroughConverter;
use super::stream::{handle_stream_error, update_u32, AudioConsumer};

/// 出力ストリームがデバイスワーカーへ知らせる値。
///
/// どちらも出力コールバックが書き、デバイスワーカーが読む。別々の引数にすると
/// `build_output_stream_with` の引数が増えすぎるので 1 つにまとめてある。
/// **ストリームを開き直すたびに中身ごと作り直す**（`AudioCapture` の
/// `stream_error` / `underruns` の説明を参照）。
#[derive(Clone)]
pub(super) struct OutputSignals {
    /// 稼働中のストリームでエラーが起きたことを表す旗
    pub(super) error: Arc<AtomicBool>,
    /// アンダーランの累計回数
    pub(super) underruns: Arc<AtomicU32>,
}

/// 出力コールバックがアンダーラン（リングバッファから取り出せなかった）を
/// 1 回数える。
///
/// **出力コールバックから呼ぶので、ロックもアロケーションもしない**
/// （`docs/design/audio.md`）。`u32::MAX` で頭打ちにするのは、回り切って 0 へ
/// 戻ると「直った」と読めてしまうため。数え始めからの累計で、減ることはない。
fn count_underrun(counter: &AtomicU32) {
    // `checked_add` が `None` を返す（頭打ち）と更新せずに終わる
    update_u32(counter, |current| current.checked_add(1));
}

/// 出力ストリームを組み立てる。
///
/// `to_sample` はリングバッファの f32 をデバイスのサンプル型へ戻す。
/// `converter` は入出力でレートやチャンネル数が違う場合の変換を持つ。
/// `signals` はエラーの旗とアンダーランの回数（`OutputSignals`）。
pub(super) fn build_output_stream_with<T>(
    device: &Device,
    config: &cpal::StreamConfig,
    consumer: Arc<Mutex<AudioConsumer>>,
    controls: Arc<AudioControls>,
    signals: OutputSignals,
    mut converter: PassthroughConverter,
    to_sample: impl Fn(f32) -> T + Send + 'static,
) -> Result<cpal::Stream, cpal::Error>
where
    T: cpal::SizedSample,
{
    // データのコールバックとエラーのコールバックが別々に持つので、
    // まとめて受け取ったものをここで分ける
    let OutputSignals {
        error: stream_error,
        underruns,
    } = signals;
    device.build_output_stream(
        *config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            process_output(
                data,
                &consumer,
                &controls,
                &mut converter,
                &underruns,
                &to_sample,
            );
        },
        // 入力側と同じ理由で、旗を立てるだけにする
        move |e| handle_stream_error("出力", &e, &stream_error, None),
        None,
    )
}

/// 出力コールバック 1 回分の処理。リングバッファから取り出し、変換・音量・
/// ミュートを通して `data` へ書く。足りなければアンダーランを 1 回数える。
///
/// **cpal の出力コールバックとフェイクの出力（`super::fake`）の両方から
/// 呼ぶ。** `AudioControls` の読み方、クロックドリフト補正の水位の記録、
/// アンダーランの数え方をフェイクでも本物と同じにするため。
pub(super) fn process_output<T: Clone>(
    data: &mut [T],
    consumer: &Mutex<AudioConsumer>,
    controls: &AudioControls,
    converter: &mut PassthroughConverter,
    underruns: &AtomicU32,
    to_sample: impl Fn(f32) -> T,
) {
    let volume = load_volume(&controls.volume);
    let audible = output_is_audible(
        controls.passthrough_enabled.load(Ordering::Relaxed),
        controls.muted.load(Ordering::Relaxed),
    );
    if let Ok(mut cons) = consumer.try_lock() {
        // この呼び出し分を消費する前の水位を渡す（クロックドリフト補正の観測と、
        // 最初の水位に達したかの判定）
        if !converter.observe_water_level(cons.occupied_len()) {
            // 最初の水位に達するまでは取り出さずに無音を書く。わざと待っているので
            // アンダーランには数えない
            data.fill(to_sample(0.0));
            return;
        }
        let mut read = |dst: &mut [f32]| pop_whole_frame(&mut cons, dst);
        let starved = render_output_samples(
            data,
            volume,
            audible,
            || converter.next_sample(&mut read),
            &to_sample,
        );
        if starved {
            // 1 回のコールバックで何サンプル足りなくても 1 回として数える。
            // 足りなかったサンプル数はバッファの大きさで意味が変わり、
            // 「何回途切れたか」ほど直感的に読めないため
            count_underrun(underruns);
            converter.note_underrun();
        }
    } else {
        // 無音を表す値は型ごとに違う（u16 は 0 ではなく 32768）ので変換関数に通す
        data.fill(to_sample(0.0));
        // ロックを取れなかったときも無音を書く。聞こえ方は取り出せなかった
        // ときと同じなので、同じく 1 回数える
        count_underrun(underruns);
        converter.note_underrun();
    }
}

/// リングバッファから入力フレームを 1 つ取り出す（`PassthroughConverter::next_sample`
/// へ渡す読み口）。**フレームが丸ごと届いていなければ何も読まずに `false`。**
///
/// 入力コールバックが積むのはフレーム単位だが、公開は 1 サンプルずつなので、
/// 出力側からはフレームの途中までが見えることがある。途中まで読むと以降が
/// 1 つずれるので、残りが届くまで読まずに残す。
fn pop_whole_frame(consumer: &mut AudioConsumer, dst: &mut [f32]) -> bool {
    if consumer.occupied_len() < dst.len() {
        return false;
    }
    // 揃っていることは確かめたので、ここで取り出せる数は `dst.len()` になる
    consumer.pop_slice(dst) == dst.len()
}

/// 出力に音を書き込んでよいか。
///
/// パススルーの無効とミュートは理由も操作経路も別だが、出力コールバックから見れば
/// どちらも「無音を書く」に落ちる。**片方だけを見る書き方にしないため**に、
/// 判定をここへ 1 つにまとめてある。
fn output_is_audible(passthrough_enabled: bool, muted: bool) -> bool {
    passthrough_enabled && !muted
}

/// 出力コールバック 1 回分のサンプルを書き込む。
///
/// `audible` が false のときは無音を書き込む。ストリームは止めない。
///
/// `next_sample` はリングバッファから 1 サンプル取り出す。取り出せなければ `None`。
/// `to_sample` は音量を掛けた f32 を出力ストリームのサンプル型へ変換する。
///
/// **1 サンプルでも取り出せなかったら `true` を返す**（アンダーラン）。
/// 数を増やすのは呼び出し側（出力コールバック）の仕事で、ここは判定だけを持つ。
fn render_output_samples<T>(
    data: &mut [T],
    volume: f32,
    audible: bool,
    mut next_sample: impl FnMut() -> Option<f32>,
    to_sample: impl Fn(f32) -> T,
) -> bool {
    // 無音を書くときもリングバッファは同じ数だけ消費する。
    // 消費を止めるとバッファが溢れ、再度鳴らしたときに古い音から再生されてしまう。
    let mut starved = false;
    for slot in data.iter_mut() {
        let sample = match next_sample() {
            Some(sample) => sample,
            None => {
                starved = true;
                0.0
            }
        };
        let value = if audible { sample * volume } else { 0.0 };
        *slot = to_sample(value);
    }
    starved
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::sample::{f32_to_i16, f32_to_i32, f32_to_u16};
    use crate::audio::stream::process_input;
    use crate::audio::tap::AudioTap;
    use crate::audio::tests::ring;
    use ringbuf::traits::Producer;

    /// テスト用のサンプル供給源。取り出した回数も数える。
    struct SampleSource {
        samples: std::collections::VecDeque<f32>,
        pop_count: usize,
    }

    impl SampleSource {
        fn new(samples: &[f32]) -> Self {
            Self {
                samples: samples.iter().copied().collect(),
                pop_count: 0,
            }
        }

        fn pop(&mut self) -> Option<f32> {
            self.pop_count += 1;
            self.samples.pop_front()
        }
    }

    #[test]
    fn output_is_audible_only_when_passthrough_on_and_not_muted() {
        assert!(output_is_audible(true, false));
        // ミュート中はパススルーが有効でも鳴らさない
        assert!(!output_is_audible(true, true));
        // パススルーが無効なら、ミュートを解除しても鳴らさない
        assert!(!output_is_audible(false, false));
        assert!(!output_is_audible(false, true));
    }

    #[test]
    fn render_output_samples_passthrough_enabled_applies_volume() {
        let mut source = SampleSource::new(&[1.0, 0.5, -0.25]);
        let mut data = [9.0f32; 3];

        render_output_samples(&mut data, 0.5, true, || source.pop(), |value| value);

        assert_eq!(data, [0.5, 0.25, -0.125]);
    }

    #[test]
    fn render_output_samples_passthrough_disabled_writes_silence() {
        let mut source = SampleSource::new(&[1.0, 0.5, -0.25]);
        let mut data = [9.0f32; 3];

        render_output_samples(&mut data, 1.0, false, || source.pop(), |value| value);

        assert_eq!(data, [0.0, 0.0, 0.0]);
    }

    #[test]
    fn render_output_samples_passthrough_disabled_still_consumes_source() {
        // 消費を止めるとリングバッファが溢れ、再有効化した瞬間に古い音が出るため、
        // 無効時もバッファからは同じ数だけ取り出す
        let mut source = SampleSource::new(&[1.0, 0.5, -0.25, 0.75]);
        let mut data = [9.0f32; 3];

        render_output_samples(&mut data, 1.0, false, || source.pop(), |value| value);

        assert_eq!(source.pop_count, 3);
        assert_eq!(source.samples.len(), 1);
    }

    #[test]
    fn render_output_samples_source_underrun_fills_remainder_with_silence() {
        let mut source = SampleSource::new(&[1.0]);
        let mut data = [9.0f32; 3];

        render_output_samples(&mut data, 1.0, true, || source.pop(), |value| value);

        assert_eq!(data, [1.0, 0.0, 0.0]);
    }

    #[test]
    fn render_output_samples_reports_starvation_when_the_source_runs_out() {
        // 途中で尽きた場合。出力コールバックはこれを見てアンダーランを数える
        let mut source = SampleSource::new(&[1.0]);
        let mut data = [9.0f32; 3];

        let starved = render_output_samples(&mut data, 1.0, true, || source.pop(), |value| value);

        assert!(starved);
    }

    #[test]
    fn render_output_samples_reports_no_starvation_when_the_source_has_enough() {
        let mut source = SampleSource::new(&[1.0, 0.5, -0.25]);
        let mut data = [9.0f32; 3];

        let starved = render_output_samples(&mut data, 1.0, true, || source.pop(), |value| value);

        assert!(!starved);
    }

    #[test]
    fn render_output_samples_reports_starvation_even_when_inaudible() {
        // ミュート中・パススルー無効でもリングバッファは同じだけ消費する。
        // 数え方を変えると、ミュートを解除した瞬間だけ数が跳ねることになる
        let mut source = SampleSource::new(&[]);
        let mut data = [9.0f32; 2];

        let starved = render_output_samples(&mut data, 1.0, false, || source.pop(), |value| value);

        assert!(starved);
    }

    #[test]
    fn count_underrun_increments_by_one() {
        let counter = AtomicU32::new(0);

        count_underrun(&counter);
        count_underrun(&counter);

        assert_eq!(counter.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn count_underrun_saturates_at_the_maximum() {
        // 回り切って 0 へ戻ると「直った」と読めてしまうので頭打ちにする
        let counter = AtomicU32::new(u32::MAX);

        count_underrun(&counter);

        assert_eq!(counter.load(Ordering::Relaxed), u32::MAX);
    }

    #[test]
    fn render_output_samples_empty_source_writes_silence() {
        let mut source = SampleSource::new(&[]);
        let mut data = [9.0f32; 2];

        render_output_samples(&mut data, 1.0, true, || source.pop(), |value| value);

        assert_eq!(data, [0.0, 0.0]);
    }

    #[test]
    fn render_output_samples_converts_to_i16() {
        let mut source = SampleSource::new(&[1.0, -1.0]);
        let mut data = [9i16; 2];

        render_output_samples(&mut data, 1.0, true, || source.pop(), f32_to_i16);

        assert_eq!(data, [32767, -32768]);
    }

    #[test]
    fn render_output_samples_i16_passthrough_disabled_writes_zero() {
        let mut source = SampleSource::new(&[1.0, -1.0]);
        let mut data = [9i16; 2];

        render_output_samples(&mut data, 1.0, false, || source.pop(), f32_to_i16);

        assert_eq!(data, [0, 0]);
    }

    #[test]
    fn render_output_samples_converts_to_u16() {
        // u16 の無音は 0 ではなく 32768
        let mut source = SampleSource::new(&[1.0, -1.0, 0.0]);
        let mut data = [9u16; 3];

        render_output_samples(&mut data, 1.0, true, || source.pop(), f32_to_u16);

        assert_eq!(data, [65535, 0, 32768]);
    }

    #[test]
    fn render_output_samples_u16_passthrough_disabled_writes_midpoint() {
        // 無効時に 0 を書くと u16 では最大振幅の直流になるため、原点を書く
        let mut source = SampleSource::new(&[1.0, -1.0]);
        let mut data = [9u16; 2];

        render_output_samples(&mut data, 1.0, false, || source.pop(), f32_to_u16);

        assert_eq!(data, [32768, 32768]);
    }

    #[test]
    fn render_output_samples_converts_to_i32() {
        let mut source = SampleSource::new(&[1.0, -1.0, 0.0]);
        let mut data = [9i32; 3];

        render_output_samples(&mut data, 1.0, true, || source.pop(), f32_to_i32);

        assert_eq!(data, [2147483647, -2147483648, 0]);
    }

    #[test]
    fn render_output_samples_empty_output_buffer_does_not_consume_source() {
        let mut source = SampleSource::new(&[1.0]);
        let mut data: [f32; 0] = [];

        render_output_samples(&mut data, 1.0, true, || source.pop(), |value| value);

        assert_eq!(source.pop_count, 0);
    }

    #[test]
    fn process_output_applies_volume_to_what_process_input_pushed() {
        // 入力 → リングバッファ → 出力を、コールバックの本体だけで通す
        let (producer, consumer) = ring(8);
        let controls = AudioControls::default();
        controls.set_volume(50.0);
        let underruns = AtomicU32::new(0);
        let mut converter = PassthroughConverter::new(48_000, 2, 48_000, 2);

        process_input(
            &[1.0f32, -0.5],
            2,
            &producer,
            &AudioTap::new(),
            &AtomicU32::new(0),
            |sample| sample,
        );
        let mut data = [9.0f32; 2];
        process_output(
            &mut data,
            &consumer,
            &controls,
            &mut converter,
            &underruns,
            |sample| sample,
        );

        assert_eq!(data, [0.5, -0.25]);
        assert_eq!(underruns.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn process_output_muted_writes_silence_but_consumes() {
        let (producer, consumer) = ring(8);
        let controls = AudioControls::default();
        controls.set_muted(true);
        let underruns = AtomicU32::new(0);
        let mut converter = PassthroughConverter::new(48_000, 2, 48_000, 2);

        process_input(
            &[1.0f32, 1.0, 1.0, 1.0],
            2,
            &producer,
            &AudioTap::new(),
            &AtomicU32::new(0),
            |sample| sample,
        );
        let mut data = [9.0f32; 2];
        process_output(
            &mut data,
            &consumer,
            &controls,
            &mut converter,
            &underruns,
            |sample| sample,
        );

        assert_eq!(data, [0.0, 0.0]);
        // ミュート中も同じだけ取り出す（残りは 1 フレーム）
        assert_eq!(consumer.lock().expect("ロックできる").occupied_len(), 2);
    }

    #[test]
    fn process_output_identity_waits_for_the_rest_of_a_partial_frame() {
        // Issue #307。入力コールバックは 1 サンプルずつ公開するので、出力側からは
        // フレームの左だけが見えることがある。修正前は左を読んで残りを無音にし、
        // 次のコールバックで右から読み始めて左右が入れ替わったままになっていた
        let (producer, consumer) = ring(8);
        let controls = AudioControls::default();
        let underruns = AtomicU32::new(0);
        let mut converter = PassthroughConverter::new(48_000, 2, 48_000, 2);
        let mut data = [9.0f32; 2];

        // 左だけが公開された瞬間を作る
        let _ = producer.lock().expect("ロックできる").try_push(1.0);
        process_output(
            &mut data,
            &consumer,
            &controls,
            &mut converter,
            &underruns,
            |s| s,
        );
        assert_eq!(data, [0.0, 0.0]);
        assert_eq!(underruns.load(Ordering::Relaxed), 1);
        // 左は読まずに残っている
        assert_eq!(consumer.lock().expect("ロックできる").occupied_len(), 1);

        let mut prod = producer.lock().expect("ロックできる");
        for value in [-1.0, 0.5, -0.5] {
            let _ = prod.try_push(value);
        }
        drop(prod);
        let mut data = [9.0f32; 4];
        process_output(
            &mut data,
            &consumer,
            &controls,
            &mut converter,
            &underruns,
            |s| s,
        );
        assert_eq!(data, [1.0, -1.0, 0.5, -0.5]);
    }

    #[test]
    fn process_output_writes_silence_until_the_ring_reaches_the_prebuffer() {
        // Issue #308。最初の水位（ここでは 2 フレーム）まではリングバッファから
        // 取り出さずに無音を書く。わざと待っているのでアンダーランには数えない
        let (producer, consumer) = ring(16);
        let controls = AudioControls::default();
        let underruns = AtomicU32::new(0);
        let mut converter = PassthroughConverter::new(48_000, 2, 48_000, 2).with_prebuffer(4);
        let tap = AudioTap::new();
        let mut data = [9.0f32; 2];

        process_input(
            &[1.0f32, -1.0],
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
        assert_eq!(data, [0.0, 0.0]);
        assert_eq!(consumer.lock().expect("ロックできる").occupied_len(), 2);
        assert_eq!(underruns.load(Ordering::Relaxed), 0);

        // 目標に達したら、溜まった先頭から出す
        process_input(
            &[0.5f32, -0.5],
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
        assert_eq!(data, [1.0, -1.0]);
        assert_eq!(consumer.lock().expect("ロックできる").occupied_len(), 2);
        assert_eq!(underruns.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn process_output_empty_ring_counts_one_underrun() {
        let (_producer, consumer) = ring(8);
        let controls = AudioControls::default();
        let underruns = AtomicU32::new(0);
        let mut converter = PassthroughConverter::new(48_000, 2, 48_000, 2);

        let mut data = [9.0f32; 4];
        process_output(
            &mut data,
            &consumer,
            &controls,
            &mut converter,
            &underruns,
            |sample| sample,
        );

        assert_eq!(data, [0.0; 4]);
        assert_eq!(underruns.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn process_output_underrun_marks_the_drift_correction_window() {
        // アンダーランを数えた窓では、クロックドリフト補正が速める側へ動かない
        // （`decide_resample_correction`）。その印を出力コールバックが付ける
        let (_producer, consumer) = ring(8);
        let controls = AudioControls::default();
        let underruns = AtomicU32::new(0);
        let telemetry = Arc::new(super::super::resample::ResampleTelemetry::new(4));
        let mut converter = PassthroughConverter::new(48_000, 2, 48_000, 2)
            .with_telemetry(Some(Arc::clone(&telemetry)));

        let mut data = [9.0f32; 4];
        process_output(
            &mut data,
            &consumer,
            &controls,
            &mut converter,
            &underruns,
            |sample| sample,
        );

        assert_eq!(underruns.load(Ordering::Relaxed), 1);
        assert!(telemetry.take_window().underran());
    }
}
