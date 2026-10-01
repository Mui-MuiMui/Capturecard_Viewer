//! 録画の音声トラック（②）。**録画スレッドの中だけにある。**
//!
//! `AudioTap` のリングから入力の形のままのサンプルを取り出し、録画用に 1 つ持つ
//! `PassthroughConverter` で 48kHz 2ch へ寄せ、`f32_to_i16` で 16bit PCM にして、
//! PTS を付けた塊（`AudioChunk`）として渡す。変換器は出力コールバックのものとは
//! 共有しない（`docs/design/recording.md` の「画素と音声の変換」）。
//!
//! PTS の考え方は `super::pts` の「音声（②）」。ここが持つのはその状態だけで、
//! 判定と計算は `pts` の純粋関数に任せる。
//!
//! - 起点は t0（PTS 0）。最初のサンプルが届いたところで、届いた時刻に合わせて
//!   無音を足すか先頭を削る（録画を始める前に届いた分は削る）
//! - サンプルの並びが途切れたら（開き直し、リングの溢れ）、途切れる前の分を前の形で
//!   変換し終えてから、形を読み直して同じように揃え直す
//! - 音声が来ていない間（音声デバイスが無い、開けていない、止まっている）は、
//!   映像に合わせて無音を書き続ける。音声トラックの長さを映像と揃えるため
//! - 途切れずに続く間は、入力デバイスの時計と PC の時計の差（ドリフト）を、変換器の
//!   レート比をわずかに動かして直す（#288、`super::pts` の「ドリフトの補正」）。
//!   時計の差では説明できないほどずれが飛んだら（サンプルが落ちた）、揃え直す

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use log::info;
use ringbuf::traits::{Consumer, Observer};

use super::pts::{
    align, audio_units, frames_in, silence_until, units_from, units_since, Alignment,
    DriftCorrector, DriftDecision, DriftSpan, TapTiming, AUDIO_CHANNELS, AUDIO_SAMPLE_RATE,
    UNITS_PER_SECOND,
};
use crate::audio::{
    f32_to_i16, AudioTap, AudioTapConsumer, AudioTapSnapshot, PassthroughConverter,
    ResampleTelemetry,
};

/// 1 回に足す無音の上限（出力フレーム数、10 秒）。
///
/// 無音は録画スレッドが数 ms ごとに少しずつ足すので、通常はここに届かない。
/// 時刻の読み違いなどで桁外れの長さを求められたときに、一度に巨大な確保をしないため。
const MAX_SILENCE_FRAMES: u64 = AUDIO_SAMPLE_RATE as u64 * 10;

/// PTS を付けた 16bit PCM の塊（48kHz 2ch インターリーブ）。
pub(super) struct AudioChunk {
    /// 100ns 単位
    pub(super) pts: i64,
    pub(super) duration: i64,
    pub(super) samples: Vec<i16>,
}

impl AudioChunk {
    /// 出力フレーム数。
    pub(super) fn frames(&self) -> usize {
        self.samples.len() / usize::from(AUDIO_CHANNELS)
    }
}

/// 閉じたときにログへ残す値。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct AudioStats {
    /// 作った出力フレームの累計（48kHz）
    pub(super) frames: u64,
    /// 揃えるために足した無音（出力フレーム数）。音声が来ていない間の分も含む
    pub(super) silence_frames: u64,
    /// 揃えるために先頭から捨てた入力のサンプル数
    pub(super) trimmed_samples: u64,
    /// 揃えるために先頭から捨てた入力の長さ（100ns）。統計 OSD に出す。
    /// 入力の形は開き直しで変わりうるので、捨てたときの形で時間に直して足す
    pub(super) trimmed_units: u64,
    /// リングが溢れて捨てたコールバックの回数
    pub(super) overflows: u64,
    /// 最後に途切れずに続いた区間の `(PC の時計での経過, サンプル数 ÷ レート)`（100ns）
    pub(super) drift: Option<(i64, i64)>,
    /// ドリフトの補正で足した長さの累計（100ns、負なら削った）
    pub(super) correction_units: i64,
    /// 入力から作った出力フレームの累計（無音を除く）。補正の平均の割合を出す分母
    pub(super) converted_frames: u64,
    /// 補正しても残ったずれ。最後に揃え直してからの基準に対する、直近 5 秒の平均の動き（100ns）
    pub(super) residual: Option<i64>,
    /// ずれが飛んで（入力デバイスがサンプルを落として）揃え直した回数
    pub(super) realigns: u64,
}

/// 録画の音声に掛けたドリフトの補正（#288）。`RecordingSummary` に入れて UI スレッドへ返す。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AudioDriftCorrection {
    /// 補正で音声の長さを変えた割合の平均（ppm）。正なら伸ばした（入力デバイスの時計が
    /// PC の時計より遅い）
    pub ppm: i32,
    /// 補正で足した長さの累計（µs、負なら削った）
    pub total_us: i64,
    /// 補正しても残ったずれ（µs、負なら音声が映像より先行する向き）。測れていなければ `None`
    pub residual_us: Option<i64>,
}

impl AudioStats {
    /// `base` からの差。リプレイバッファ（③）は音声トラックを録画をまたいで使い続けるので、
    /// 1 回の録画の値は録画を始めたときの値からの差で出す。ドリフトの区間と補正しても
    /// 残ったずれはそのまま使う（揃え直すたびに始め直す値で、録画の始まりとは関係しない）。
    pub(super) fn since(&self, base: &AudioStats) -> AudioStats {
        AudioStats {
            frames: self.frames.saturating_sub(base.frames),
            silence_frames: self.silence_frames.saturating_sub(base.silence_frames),
            trimmed_samples: self.trimmed_samples.saturating_sub(base.trimmed_samples),
            trimmed_units: self.trimmed_units.saturating_sub(base.trimmed_units),
            overflows: self.overflows.saturating_sub(base.overflows),
            drift: self.drift,
            correction_units: self.correction_units.saturating_sub(base.correction_units),
            converted_frames: self.converted_frames.saturating_sub(base.converted_frames),
            residual: self.residual,
            realigns: self.realigns.saturating_sub(base.realigns),
        }
    }

    /// 掛けた補正。割合は「足した長さ ÷ 入力から作った長さ」の平均。
    pub(super) fn drift_correction(&self) -> AudioDriftCorrection {
        let converted = audio_units(self.converted_frames);
        let ppm = if converted > 0 {
            (self.correction_units as f64 / converted as f64 * 1_000_000.0).round() as i32
        } else {
            0
        };
        AudioDriftCorrection {
            ppm,
            total_us: self.correction_units / 10,
            residual_us: self.residual.map(|units| units / 10),
        }
    }

    /// ログへ 1 行で残す。録画スレッドが閉じたときに呼ぶ（失敗ではないので `info`）。
    ///
    /// - **入力デバイスの時計のずれ**: 途切れずに続いた区間での「映像の時計（PC）での経過」と
    ///   「音声のサンプル数 ÷ レート」の差。補正しなければ、録画の中で音が映像からずれていく量
    ///   （ドリフト）。2 点の時刻から出すので 10ms ほどの粒度がある
    /// - **ドリフトの補正**（#288）: 掛けた補正の平均（ppm）と足した長さ、補正しても残ったずれ
    ///   （直近 5 秒の平均）。残ったずれが、録画の中で音が映像からずれた量になる
    ///
    /// `docs/design/recording.md` の「ドリフト」。
    pub(super) fn log(&self, video: Duration) {
        let secs = |units: i64| units as f64 / UNITS_PER_SECOND as f64;
        let millis = |units: i64| units as f64 * 1000.0 / UNITS_PER_SECOND as f64;
        let drift = match self.drift {
            Some((by_clock, by_samples)) if by_clock > 0 => format!(
                "入力デバイスの時計: 途切れずに続いた最後の区間で、映像の時計（PC）の {:.3} 秒に対して音声のサンプル数 ÷ レートは {:.3} 秒（差 {:+.1}ms、{:+.0}ppm。負なら補正しないと録画の音声が映像より先行していく）",
                secs(by_clock),
                secs(by_samples),
                millis(by_samples - by_clock),
                (by_samples - by_clock) as f64 / by_clock as f64 * 1_000_000.0
            ),
            _ => "音声が途切れずに続いた区間が無い（音声が届かなかった）".to_string(),
        };
        let residual = match self.residual {
            Some(units) => format!(
                "補正しても残ったずれ {:+.1}ms（揃え直してからの基準に対する直近 5 秒の平均。負なら音声が先行する向き）",
                millis(units)
            ),
            None => "補正しても残ったずれは未測定（揃え直してから 10 秒経っていない）".to_string(),
        };
        info!(
            "録画の音声: 長さ {:.3} 秒（映像 {:.3} 秒）。{}。ドリフトの補正: 平均 {:+}ppm（足した長さ {:+.1}ms）、{}、ずれが飛んで揃え直した回数 {}。揃えるために足した無音 {:.1}ms、削った入力 {} サンプル、リングの溢れ {} 回",
            secs(audio_units(self.frames)),
            secs(units_from(video)),
            drift,
            self.drift_correction().ppm,
            millis(self.correction_units),
            residual,
            self.realigns,
            millis(audio_units(self.silence_frames)),
            self.trimmed_samples,
            self.overflows
        );
    }
}

/// 1 回の録画の音声トラック。
pub(super) struct AudioTrack {
    tap: AudioTap,
    consumer: AudioTapConsumer,
    t0: Instant,
    /// 次に取り出すサンプルの、累計での番号
    next_index: u64,
    /// 見た途切れの回数。`AudioTap` の値が進んだら途切れた
    seen_breaks: u64,
    /// 入力のレートとチャンネル数。まだ 1 度も開いていなければ `None`
    format: Option<(u32, u16)>,
    /// 入力の形から 48kHz 2ch へ寄せる。形が分からない間は `None`
    converter: Option<PassthroughConverter>,
    /// 取り出したがまだ変換していない入力（出力フレームに足りない端数）
    input: VecDeque<f32>,
    /// 揃え直しが要る（録画の開始、途切れ、無音で埋めたあと）
    needs_anchor: bool,
    /// 揃えるために、これから先頭から捨てる入力のサンプル数
    trim_remaining: u64,
    /// 作った出力フレームの累計。まだ渡していない `pcm` の分も含む
    produced_frames: u64,
    /// 渡した出力フレームの累計
    emitted_frames: u64,
    /// まだ渡していない PCM
    pcm: Vec<i16>,
    /// リングから取り出す先と、変換の出力先。使い回す
    popped: Vec<f32>,
    converted: Vec<f32>,
    drift: Option<DriftSpan>,
    /// ドリフトの補正（#288）。変換器へ補正係数を渡す器。**録画スレッドの中だけで使い、
    /// 出力コールバックの補正とは共有しない。** 変換器を作り直しても同じものを紐づけ、
    /// 揃え直しをまたいで係数を持ち越す（入力デバイスの時計は揃え直しでは変わらない）。
    /// ずれが飛んで揃え直すときだけ 1.0 に戻す（`correct_drift`）
    correction: Arc<ResampleTelemetry>,
    /// いま掛けている補正係数。`correction` に書いた値の控え
    current_correction: f64,
    /// ずれの窓と基準。揃え直すたびに作り直す
    corrector: DriftCorrector,
    /// 補正で増やした出力フレーム数の累計（負なら減らした）
    corrected_frames: f64,
    /// 入力から作った出力フレームの累計（無音を除く）。補正の平均の割合を出す分母
    converted_frames: u64,
    /// ずれが飛んで揃え直した回数
    realigns: u64,
    silence_frames: u64,
    trimmed_samples: u64,
    trimmed_units: u64,
}

impl AudioTrack {
    /// リングを差し込んで音声トラックを始める。`t0` は映像と同じ録画の基準。
    pub(super) fn attach(tap: AudioTap, t0: Instant) -> Self {
        let attachment = tap.attach(tap.one_second_capacity());
        let format = tap.format();
        let correction = Arc::new(ResampleTelemetry::for_recording());
        Self {
            consumer: attachment.consumer,
            t0,
            next_index: attachment.start_index,
            seen_breaks: attachment.breaks,
            format,
            converter: format.map(|format| converter_for(format, &correction)),
            input: VecDeque::new(),
            needs_anchor: true,
            trim_remaining: 0,
            produced_frames: 0,
            emitted_frames: 0,
            pcm: Vec::new(),
            popped: Vec::new(),
            converted: Vec::new(),
            drift: None,
            correction,
            current_correction: 1.0,
            corrector: DriftCorrector::default(),
            corrected_frames: 0.0,
            converted_frames: 0,
            realigns: 0,
            silence_frames: 0,
            trimmed_samples: 0,
            trimmed_units: 0,
            tap,
        }
    }

    /// リングを抜く。抜く前に積まれた分は、このあとの `pump` で読める。
    pub(super) fn detach(&self) {
        self.tap.detach();
    }

    /// リングに溜まった分を取り出して PCM にする。録画スレッドが数 ms ごとに呼ぶ。
    pub(super) fn pump(&mut self, now: Instant) {
        let snapshot = self.tap.snapshot();
        if snapshot.breaks != self.seen_breaks {
            // 途切れの前までは、前の形・前の並びのまま変換し終える
            self.take_until(snapshot.break_at, &snapshot);
            self.convert();
            self.seen_breaks = snapshot.breaks;
            self.format = snapshot.format;
            self.input.clear();
            self.trim_remaining = 0;
            self.needs_anchor = true;
        }
        self.take_until(snapshot.samples_total, &snapshot);
        self.convert();

        // 音声が来ていなければ、映像に合わせて無音を書く
        let now_units = units_since(self.t0, now);
        let last_push = snapshot
            .last_push
            .map(|elapsed| units_since(self.t0, self.tap.base() + elapsed));
        if let Some(until) = silence_until(now_units, last_push, self.format.is_some()) {
            let expected = audio_units(self.produced_frames);
            if until > expected {
                self.insert_silence(frames_in(until - expected, AUDIO_SAMPLE_RATE));
                // 出力フレームに足りなかった端数は、無音のあとへは繋げない。
                // 次に届いたサンプルは、埋めた無音との位置を揃え直す
                self.input.clear();
                self.needs_anchor = true;
            }
        }

        let timing = self.timing(&snapshot);
        if let (Some(span), Some(timing)) = (self.drift.as_mut(), timing) {
            span.update(timing);
        }
        if let Some(timing) = timing {
            self.correct_drift(now_units, timing);
        }
    }

    /// ずれ（次に書く位置 − そのサンプルを受け取った時刻）を測って補正の窓へ足し、窓を
    /// 閉じたら補正係数を変換器へ渡す（#288、`super::pts` の「ドリフトの補正」）。
    /// 揃え直しを待っている間と、揃えるための削りが残っている間は測らない（位置がまだ飛ぶ）。
    fn correct_drift(&mut self, now: i64, timing: TapTiming) {
        if self.needs_anchor || self.trim_remaining > 0 || self.converter.is_none() {
            return;
        }
        // 変換待ちの入力の手前までが、書いた出力フレームになっている（補間が持つ 1〜2
        // フレームの分は数十 µs なので無視する）
        let consumed = self.next_index.saturating_sub(self.input.len() as u64);
        let offset = audio_units(self.produced_frames).saturating_sub(timing.time_of(consumed));
        match self.corrector.observe(now, offset) {
            Some(DriftDecision::Correct(correction)) => self.set_correction(correction),
            Some(DriftDecision::Realign) => {
                // サンプルが落ちた。補正の窓に混ざった飛びで決めた係数は捨て、次に届いた
                // サンプルで揃え直す（無音で埋めたあとと同じ扱い）
                self.set_correction(1.0);
                self.input.clear();
                self.needs_anchor = true;
                self.realigns += 1;
            }
            None => {}
        }
    }

    fn set_correction(&mut self, correction: f64) {
        // 変換器は f32 で読むので、控えも f32 に丸めた値にする（補正量の累計を合わせる）
        let correction = correction as f32;
        self.correction.set_correction(correction);
        self.current_correction = f64::from(correction);
    }

    /// 録画の終わり。音声が映像より短ければ、`video_end`（100ns）まで無音で埋める。
    pub(super) fn finish(&mut self, video_end: i64) {
        let expected = audio_units(self.produced_frames);
        if video_end > expected {
            // 1 回に足す無音には上限があるので、足りなくなるまで繰り返す。
            // 録画スレッドが長く止まったあとで止めると、上限を超える差が残りうる
            let mut remaining = frames_in(video_end - expected, AUDIO_SAMPLE_RATE);
            while remaining > 0 {
                let frames = remaining.min(MAX_SILENCE_FRAMES);
                self.insert_silence(frames);
                remaining -= frames;
            }
        }
    }

    /// 溜まった PCM を塊にして渡す。`min_frames` に満たなければ `None`。
    /// 小さく刻んで `WriteSample` を増やさないため、書くときは一定の長さを溜める。
    pub(super) fn take_chunk(&mut self, min_frames: usize) -> Option<AudioChunk> {
        let frames = self.pcm.len() / usize::from(AUDIO_CHANNELS);
        if frames == 0 || frames < min_frames {
            return None;
        }
        let samples = std::mem::take(&mut self.pcm);
        let pts = audio_units(self.emitted_frames);
        self.emitted_frames += frames as u64;
        // 長さは差で取る。1 フレームずつ切り捨てた誤差を溜めないため
        let duration = audio_units(self.emitted_frames) - pts;
        Some(AudioChunk {
            pts,
            duration,
            samples,
        })
    }

    /// 閉じたときにログへ残す値。
    pub(super) fn stats(&self) -> AudioStats {
        AudioStats {
            frames: self.produced_frames,
            silence_frames: self.silence_frames,
            trimmed_samples: self.trimmed_samples,
            trimmed_units: self.trimmed_units,
            overflows: self.tap.overflows(),
            drift: self.drift.map(|span| span.measure()),
            correction_units: (self.corrected_frames * UNITS_PER_SECOND as f64
                / f64::from(AUDIO_SAMPLE_RATE))
            .round() as i64,
            converted_frames: self.converted_frames,
            residual: self.corrector.last_error(),
            realigns: self.realigns,
        }
    }

    /// リングから `limit`（累計の番号）の手前まで取り出して、変換待ちへ積む。
    fn take_until(&mut self, limit: u64, snapshot: &AudioTapSnapshot) {
        let wanted = limit.saturating_sub(self.next_index);
        if wanted == 0 {
            return;
        }
        if self.needs_anchor {
            self.anchor(snapshot);
        }
        let wanted = usize::try_from(wanted).unwrap_or(usize::MAX);
        self.popped
            .resize(wanted.min(self.consumer.occupied_len()), 0.0);
        let count = self.consumer.pop_slice(&mut self.popped);
        self.next_index += count as u64;

        let skip = usize::try_from(self.trim_remaining)
            .unwrap_or(usize::MAX)
            .min(count);
        self.trim_remaining -= skip as u64;
        self.trimmed_samples += skip as u64;
        if let Some((sample_rate, channels)) = self.format {
            self.trimmed_units += samples_to_units(skip as u64, sample_rate, channels);
        }
        // 形が分からないサンプルは解釈できない（開く前に積まれることは無いので来ない）
        if self.converter.is_some() {
            self.input.extend(&self.popped[skip..count]);
        }
    }

    /// 次に取り出すサンプルを受け取った時刻と、次に書く位置を比べて揃える。
    fn anchor(&mut self, snapshot: &AudioTapSnapshot) {
        let Some(timing) = self.timing(snapshot) else {
            // 時刻がまだ無い（開いた直後で 1 度も積んでいない）。積まれてから揃える
            return;
        };
        // 途切れの前後で補間を繋げない。補正係数は持ち越す
        self.converter = self
            .format
            .map(|format| converter_for(format, &self.correction));
        // 揃え直しで位置が飛ぶので、ずれの基準は測り直す
        self.corrector = DriftCorrector::default();
        let actual = timing.time_of(self.next_index);
        let expected = audio_units(self.produced_frames);
        match align(expected, actual, timing.sample_rate) {
            Alignment::Keep => {}
            Alignment::InsertSilence { frames } => self.insert_silence(frames),
            Alignment::Trim { input_frames } => {
                self.trim_remaining = input_frames.saturating_mul(u64::from(timing.channels));
            }
        }
        self.drift = Some(DriftSpan::new(timing));
        self.needs_anchor = false;
    }

    /// 逆算に使う値。入力の形か時刻がまだ無ければ `None`。
    fn timing(&self, snapshot: &AudioTapSnapshot) -> Option<TapTiming> {
        let (sample_rate, channels) = self.format?;
        let last_push = snapshot.last_push?;
        Some(TapTiming {
            last_push: units_since(self.t0, self.tap.base() + last_push),
            samples_total: snapshot.samples_total,
            sample_rate,
            channels,
        })
    }

    /// 変換待ちの入力を、出せるだけ PCM にする。
    fn convert(&mut self) {
        let Some(converter) = self.converter.as_mut() else {
            return;
        };
        self.converted.clear();
        converter.convert_buffered(&mut self.input, &mut self.converted);
        self.pcm
            .extend(self.converted.iter().map(|&sample| f32_to_i16(sample)));
        let frames = (self.converted.len() / usize::from(AUDIO_CHANNELS)) as u64;
        self.produced_frames += frames;
        self.converted_frames += frames;
        // 係数 c で作った n フレームは、補正しなければ n × c フレームだった
        self.corrected_frames += frames as f64 * (1.0 - self.current_correction);
    }

    fn insert_silence(&mut self, frames: u64) {
        let frames = frames.min(MAX_SILENCE_FRAMES);
        let samples = frames as usize * usize::from(AUDIO_CHANNELS);
        self.pcm.resize(self.pcm.len() + samples, 0);
        self.produced_frames += frames;
        self.silence_frames += frames;
    }
}

/// 入力のサンプル数（インターリーブ）を、その形での長さ（100ns）に直す。
fn samples_to_units(samples: u64, sample_rate: u32, channels: u16) -> u64 {
    if sample_rate == 0 || channels == 0 {
        return 0;
    }
    let frames = samples / u64::from(channels);
    frames.saturating_mul(UNITS_PER_SECOND as u64) / u64::from(sample_rate)
}

/// 入力の形から録画の形（48kHz 2ch）への変換器。ドリフトの補正係数（#288）を紐づけるので、
/// 入力が 48kHz 2ch でも補間の経路を通る（係数が 1.0 の間は入力の値がそのまま出る）。
fn converter_for(
    (sample_rate, channels): (u32, u16),
    correction: &Arc<ResampleTelemetry>,
) -> PassthroughConverter {
    PassthroughConverter::new(sample_rate, channels, AUDIO_SAMPLE_RATE, AUDIO_CHANNELS)
        .with_telemetry(Some(Arc::clone(correction)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_track_without_any_stream_writes_silence_up_to_now_minus_the_stale_margin() {
        let tap = AudioTap::new();
        let t0 = Instant::now();
        let mut track = AudioTrack::attach(tap, t0);

        track.pump(t0 + Duration::from_secs(1));

        // 音声デバイスが無い間は無音。1 秒 − 200ms ぶん
        let chunk = track.take_chunk(0).expect("無音が書かれる");
        assert_eq!(chunk.pts, 0);
        assert_eq!(chunk.frames(), 38_400);
        assert!(chunk.samples.iter().all(|&sample| sample == 0));
        assert_eq!(track.stats().silence_frames, 38_400);
    }

    #[test]
    fn audio_track_chunks_are_contiguous() {
        let tap = AudioTap::new();
        let t0 = Instant::now();
        let mut track = AudioTrack::attach(tap, t0);

        track.pump(t0 + Duration::from_millis(500));
        let first = track.take_chunk(0).expect("1 つ目");
        track.pump(t0 + Duration::from_millis(900));
        let second = track.take_chunk(0).expect("2 つ目");

        // 次の塊は前の塊の終わりから始まる（PCM は途切れさせない）
        assert_eq!(second.pts, first.pts + first.duration);
        assert_eq!(first.frames() + second.frames(), 33_600);
    }

    #[test]
    fn audio_track_take_chunk_waits_for_the_minimum_length() {
        let tap = AudioTap::new();
        let t0 = Instant::now();
        let mut track = AudioTrack::attach(tap, t0);

        track.pump(t0 + Duration::from_millis(210));
        // 10ms（480 フレーム）しか溜まっていない
        assert!(track.take_chunk(1024).is_none());
        assert_eq!(track.take_chunk(0).map(|chunk| chunk.frames()), Some(480));
    }

    #[test]
    fn audio_track_finish_pads_to_the_video_end() {
        let tap = AudioTap::new();
        let t0 = Instant::now();
        let mut track = AudioTrack::attach(tap, t0);

        track.finish(20_000_000);

        // 音声が 1 度も来なくても、映像の長さ（2 秒）まで無音で揃える
        assert_eq!(track.stats().frames, 96_000);
    }

    #[test]
    fn audio_track_finish_pads_beyond_the_per_insert_limit() {
        let tap = AudioTap::new();
        let t0 = Instant::now();
        let mut track = AudioTrack::attach(tap, t0);

        // 1 回に足せる無音（10 秒）を超える差（25 秒）でも、映像の終わりまで揃える
        track.finish(250_000_000);

        assert_eq!(track.stats().frames, 1_200_000);
        assert_eq!(
            track.take_chunk(0).map(|chunk| chunk.frames()),
            Some(1_200_000)
        );
    }

    #[test]
    fn audio_track_converts_pushed_samples_to_48k_stereo_pcm() {
        let tap = AudioTap::new();
        tap.begin_stream(48_000, 1);
        // 録画を 1 秒前に始めたことにする（届いたサンプルが t0 より前として削られないように）
        let t0 = Instant::now() - Duration::from_secs(1);
        let mut track = AudioTrack::attach(tap.clone(), t0);

        // 入力（モノラル）に 0.5 を 4800 サンプル（100ms）積む
        tap.push_for_test(&vec![0.5; 4_800]);
        track.pump(Instant::now());
        let chunk = track.take_chunk(0).expect("変換した PCM がある");

        // 左右に同じ値が入る（0.5 は i16 で 16384）。変換の遅れの 1 フレームは次へ残る
        let stats = track.stats();
        assert!(chunk.samples.contains(&16_384));
        assert_eq!(chunk.samples.len() % 2, 0);
        assert_eq!(
            stats.frames,
            stats.silence_frames + (4_800 - stats.trimmed_samples) - 1
        );
    }

    #[test]
    fn audio_track_reopen_restarts_with_the_new_format() {
        let tap = AudioTap::new();
        tap.begin_stream(48_000, 2);
        let t0 = Instant::now() - Duration::from_secs(1);
        let mut track = AudioTrack::attach(tap.clone(), t0);
        tap.push_for_test(&[0.25; 960]);
        track.pump(Instant::now());

        // 開き直してモノラルになった。途切れの前は 2ch、後は 1ch として読む
        tap.begin_stream(44_100, 1);
        tap.push_for_test(&[0.25; 441]);
        track.pump(Instant::now());

        assert_eq!(track.format, Some((44_100, 1)));
        assert_eq!(track.next_index, 960 + 441);
        assert!(track.stats().drift.is_some());
    }

    #[test]
    fn audio_stats_since_subtracts_the_counters_and_keeps_the_drift() {
        let base = AudioStats {
            frames: 48_000,
            silence_frames: 4_800,
            trimmed_samples: 10,
            trimmed_units: 1_000,
            overflows: 1,
            drift: None,
            correction_units: 2_000,
            converted_frames: 40_000,
            residual: None,
            realigns: 1,
        };
        let now = AudioStats {
            frames: 96_000,
            silence_frames: 9_600,
            trimmed_samples: 30,
            trimmed_units: 3_000,
            overflows: 4,
            drift: Some((10_000_000, 9_999_000)),
            correction_units: -3_000,
            converted_frames: 80_000,
            residual: Some(-4_000),
            realigns: 3,
        };
        assert_eq!(
            now.since(&base),
            AudioStats {
                frames: 48_000,
                silence_frames: 4_800,
                trimmed_samples: 20,
                trimmed_units: 2_000,
                overflows: 3,
                drift: Some((10_000_000, 9_999_000)),
                // 補正量は符号付きの差（途中で向きが変わっても引き算のまま）
                correction_units: -5_000,
                converted_frames: 40_000,
                residual: Some(-4_000),
                realigns: 2,
            }
        );
        // 差し込み直しで溢れた回数が 0 に戻っていても、負にはしない
        assert_eq!(base.since(&now).overflows, 0);
    }

    #[test]
    fn audio_stats_drift_correction_reports_the_average_ppm_and_totals() {
        // 10 分（2880 万フレーム）で +30.6ms 足した。平均 +51ppm
        let stats = AudioStats {
            correction_units: 306_000,
            converted_frames: 28_800_000,
            residual: Some(-8_000),
            ..AudioStats::default()
        };
        assert_eq!(
            stats.drift_correction(),
            AudioDriftCorrection {
                ppm: 51,
                total_us: 30_600,
                residual_us: Some(-800),
            }
        );
        // 削った向き
        let stats = AudioStats {
            correction_units: -12_000,
            converted_frames: 4_800_000,
            ..AudioStats::default()
        };
        assert_eq!(stats.drift_correction().ppm, -12);
        assert_eq!(stats.drift_correction().residual_us, None);
        // 入力から何も作っていない（音声が届かなかった）なら 0 で、割り算で落とさない
        assert_eq!(
            AudioStats::default().drift_correction(),
            AudioDriftCorrection::default()
        );
    }

    #[test]
    fn audio_track_converter_follows_the_drift_correction_even_for_48k_stereo() {
        // 入力が録画と同じ 48kHz 2ch でも補間の経路を通り、補正係数で出力の数が変わる
        let correction = Arc::new(ResampleTelemetry::for_recording());
        let mut converter = converter_for((48_000, 2), &correction);
        let mut input: VecDeque<f32> = std::iter::repeat_n(0.25, 96_000).collect();
        let mut output = Vec::new();
        converter.convert_buffered(&mut input, &mut output);
        // 係数 1.0 の間は値を変えない（補間の右端を待つ 1 フレームが残る）
        assert_eq!(output.len(), 95_998);
        assert!(output.iter().all(|&sample| sample == 0.25));

        // 0.1% 遅く進める（入力デバイスの時計が遅い向き）と、1 秒の入力から 48 フレーム多く出る
        correction.set_correction(0.999);
        let mut input: VecDeque<f32> = std::iter::repeat_n(0.25, 96_000).collect();
        output.clear();
        converter.convert_buffered(&mut input, &mut output);
        // 1 秒の入力から 48048 フレーム。前の呼び出しで残した 1 フレームを足して 48049
        assert_eq!(output.len() / 2, 48_049);
    }

    #[test]
    fn samples_to_units_uses_the_input_format() {
        // 48kHz 2ch の 9600 サンプルは 4800 フレーム = 100ms
        assert_eq!(samples_to_units(9_600, 48_000, 2), 1_000_000);
        // 44.1kHz 1ch の 441 サンプルは 10ms
        assert_eq!(samples_to_units(441, 44_100, 1), 100_000);
        assert_eq!(samples_to_units(0, 48_000, 2), 0);
        // 形が分からないときは 0（割り算で落とさない）
        assert_eq!(samples_to_units(100, 0, 2), 0);
        assert_eq!(samples_to_units(100, 48_000, 0), 0);
    }
}
