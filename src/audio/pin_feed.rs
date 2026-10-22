//! DirectShow の映像デバイスの音声ピンと `AudioCapture` をつなぐ差し込み口
//! `AudioPinFeed`（#388）。
//!
//! 音声ピンのレンダラー（`video::directshow` の `Receive`）が受け取った PCM を、
//! cpal の入力コールバックの代わりに `stream::process_input_iter` へ渡す。
//! リングより後ろ（出力、変換、クロックドリフト補正、録画への分岐）は
//! WASAPI の入力と同じ経路を通る（`docs/design/directshow-audio.md` の (2)）。
//!
//! 持つものは 2 つで、書く側と読む側が違う。
//!
//! | 持つもの | 書く側 | 読む側 |
//! |---|---|---|
//! | 繋いだ音声ピン（`PinConnection`。グラフの番号・形式・塊の長さ） | 映像のバックエンド（ワーカースレッド。グラフを組んだとき・捨てたとき） | 音声のバックエンド（ワーカースレッド。開くとき） |
//! | 差し込み先（`PinSink`。リング・録画の差し込み口・数え手） | 音声のバックエンド（ワーカースレッド。開くとき差し込み、閉じるとき抜く） | 音声ピンの `Receive`（`try_lock` だけ） |
//!
//! **`Receive` はロックを待たず、確保もしない。** 差し込み先は `try_lock` で取り、
//! 取れなければそのサンプルを捨てる（`process_input` がリングと `AudioTap` を
//! `try_lock` で取るのと同じ扱い）。**自分のグラフの番号と差し込み先の番号が
//! 一致するときだけ積む。** 映像を開き直すと番号が進むので、止まる途中の古い
//! グラフが新しい差し込み先へ積むことも、新しいグラフが古い形式のリングへ
//! 積むことも起きない。
//!
//! ワーカーの中で閉じた共有で、UI スレッドは触らない（`ResampleTelemetry` と
//! 同じ扱い。`docs/design/device-worker.md`）。

use cpal::{SupportedBufferSize, SupportedStreamConfig, SupportedStreamConfigRange};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::capabilities::AudioCapabilities;
use super::pin_stream::{
    format_change_report, pack_pin_format, unpack_pin_format, FormatChangeReport, PinCarry,
    PinStream,
};
use super::sample::i16_to_f32;
use super::stream::{count_xrun, process_input_iter, AudioProducer};
use super::tap::AudioTap;

/// 音声ピンのサンプルの型。受け取るのはこの 2 つだけ
/// （`docs/design/directshow-audio.md` の (1)。24bit / 32bit 整数は要望が出てから）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinSampleType {
    /// 16bit 整数の PCM（GC551 の形）
    I16,
    /// 32bit 浮動小数点
    F32,
}

/// 音声ピンの形式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_type: PinSampleType,
}

impl PinFormat {
    /// 1 サンプルのバイト数
    pub fn bytes_per_sample(self) -> u32 {
        match self.sample_type {
            PinSampleType::I16 => 2,
            PinSampleType::F32 => 4,
        }
    }

    /// 1 フレーム（全チャンネルの 1 サンプルずつ）のバイト数
    pub(super) fn bytes_per_frame(self) -> usize {
        usize::from(self.channels) * self.bytes_per_sample() as usize
    }

    /// 1 秒ぶんのバイト数。塊の長さの提案と換算に使う
    pub fn bytes_per_second(self) -> u32 {
        self.sample_rate
            .saturating_mul(u32::from(self.channels))
            .saturating_mul(self.bytes_per_sample())
    }

    /// `bytes` バイトの塊が何 ms ぶんか（切り上げ）。形式が壊れていて
    /// 1 秒ぶんが 0 バイトなら `None`
    pub fn chunk_ms(self, bytes: u32) -> Option<u32> {
        let per_second = u64::from(self.bytes_per_second());
        if per_second == 0 {
            return None;
        }
        let ms = (u64::from(bytes) * 1000).div_ceil(per_second);
        Some(u32::try_from(ms).unwrap_or(u32::MAX))
    }

    /// cpal の語彙でのサンプル型。出力をこの形に揃えて開くのに使う
    pub(super) fn cpal_sample_format(self) -> cpal::SampleFormat {
        match self.sample_type {
            PinSampleType::I16 => cpal::SampleFormat::I16,
            PinSampleType::F32 => cpal::SampleFormat::F32,
        }
    }

    /// 入力の対応設定としてのこの形式 1 つ（最小 = 最大 = このレート）。
    /// 音声ピンの入力は繋いだ形式でしか開かない（`docs/design/directshow-audio.md` の (6)）
    pub(super) fn stream_config_range(self) -> SupportedStreamConfigRange {
        SupportedStreamConfigRange::new(
            self.channels,
            self.sample_rate,
            self.sample_rate,
            SupportedBufferSize::Unknown,
            self.cpal_sample_format(),
        )
    }

    /// 入力の既定の設定としてのこの形式。`stream_config_range` と同じ形
    pub(super) fn stream_config(self) -> SupportedStreamConfig {
        SupportedStreamConfig::new(
            self.channels,
            self.sample_rate,
            SupportedBufferSize::Unknown,
            self.cpal_sample_format(),
        )
    }

    /// 設定ダイアログのサンプリングレートとチャンネル数の選択肢に使う、入力の
    /// 対応設定（この形式 1 つ）。入力が音声ピンのときは、ワーカーへ入力の対応設定を
    /// 問い合わせずにこれを使う（#394）
    pub fn capabilities(self) -> AudioCapabilities {
        AudioCapabilities::new(
            vec![self.stream_config_range()],
            self.sample_rate,
            self.channels,
        )
    }

    /// ログと「接続状態」タブに出す 1 行（「48000Hz 2ch 16bit」）
    pub fn summary(self) -> String {
        format!(
            "{}Hz {}ch {}bit",
            self.sample_rate,
            self.channels,
            self.bytes_per_sample() * 8
        )
    }
}

/// 繋いだ音声ピン。映像のバックエンドがグラフを組んだときに書く。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinConnection {
    /// このグラフの番号（`AudioPinFeed::begin_graph`）
    pub graph: u64,
    /// 映像デバイスの表示名。「接続状態」タブの音声の入力の欄に出す
    pub device: String,
    pub format: PinFormat,
    /// アロケーターが決めた 1 塊のバイト数。取れなければ `None`
    pub chunk_bytes: Option<u32>,
}

/// 音声ピンに繋げなかった理由。文言は表示するときに `crate::i18n` から引く。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinFailure {
    /// ピンとレンダラーを繋げなかった（下位のエラーの文字列）
    Connect(String),
    /// 繋ぐとグラフを動かせなかったので外した（下位のエラーの文字列）
    Run(String),
}

/// 映像デバイスの音声ピンの状態。`video::ActiveVideo::audio_pin` に入る。
///
/// **観測値の 1 項目として持つ。** trait に「音声ピンを問い合わせる」メソッドは
/// 足さない（`docs/design/directshow-audio.md` の (3)）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AudioPinState {
    /// 対象外。Media Foundation で開いた映像（音声ピンが無い）とフェイク・モック
    #[default]
    NotApplicable,
    /// DirectShow で開いたが、音声ピンが無い
    Missing,
    /// 音声ピンはあるが繋いでいない（入力の種類が「映像デバイスの音声」ではない）
    Available,
    /// 繋いだ
    Connected(PinConnection),
    /// 繋げなかった。映像は止めずに音声のレンダラーだけ外してある
    Failed(PinFailure),
}

/// 列挙の時点で調べた、映像デバイスに音声ピンがあるか（#409）。
///
/// 開く前に、設定ダイアログで映像デバイスの音声を選べるかを決めるためだけに使う。
/// 開いたあとは開いた結果（`AudioPinState`）が優先する
/// （`docs/design/directshow-audio.md` の (5)）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioPinPresence {
    /// 音声を出す出力ピンがある
    Present,
    /// 出力ピンを最後まで見たが、音声を出すものが無い
    Absent,
    /// 調べられなかった（フィルターを作れない、ピンを列挙できない、開いている映像
    /// なので掴み直さなかった）。**選べる側に倒す**
    Unknown,
}

/// 音声のバックエンドが差し込む受け口。
pub(super) struct PinSink {
    /// 差し込んだときのグラフの番号
    pub(super) graph: u64,
    /// 差し込んだときの形式。流れの途中で形式が変わったサンプルは積まない
    pub(super) format: PinFormat,
    pub(super) producer: Arc<Mutex<AudioProducer>>,
    pub(super) tap: AudioTap,
    pub(super) dropped_frames: Arc<AtomicU32>,
    pub(super) xruns: Arc<AtomicU32>,
    /// 流れの状態。差し込むときは `Default::default()`（まだ何も受け取っていない）
    pub(super) stream: PinStream,
}

#[derive(Default)]
struct FeedInner {
    /// 最後に配ったグラフの番号。0 はまだ配っていない
    graph: AtomicU64,
    /// 繋いだ音声ピン。触るのはワーカースレッドだけ
    connection: Mutex<Option<PinConnection>>,
    /// 差し込み先。`Receive` は `try_lock` だけ
    sink: Mutex<Option<PinSink>>,
    /// いまのグラフで受け取った塊の最大のバイト数。0 はまだ受け取っていない
    observed_chunk_bytes: AtomicU32,
    /// いまのグラフで、流れの途中で形式が変わった回数。`Receive` が足す
    format_changes: AtomicU32,
    /// 最後に変わった先の形式（`pack_pin_format`）。`Receive` が書く
    format_latest: AtomicU64,
    /// `format_changes` のうち、ワーカーがログへ出し終えたところ
    format_changes_logged: AtomicU32,
}

/// 音声ピンの差し込み口。複製しても同じ中身を指す。
#[derive(Clone, Default)]
pub struct AudioPinFeed {
    inner: Arc<FeedInner>,
}

impl AudioPinFeed {
    pub fn new() -> Self {
        Self::default()
    }

    /// 新しいグラフの番号を配る。**映像のバックエンドがグラフを組む前に呼ぶ。**
    /// 前のグラフの繋いだ記録と、受け取った塊の長さはここで消える
    pub fn begin_graph(&self) -> u64 {
        self.set_connection(None);
        self.inner.observed_chunk_bytes.store(0, Ordering::Relaxed);
        // 数え直す。前のグラフの `Receive` はもう止まっている（グラフを捨ててから呼ぶ）
        self.inner.format_changes.store(0, Ordering::Relaxed);
        self.inner.format_changes_logged.store(0, Ordering::Relaxed);
        self.inner.format_latest.store(0, Ordering::Relaxed);
        self.inner.graph.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// 流れの途中で音声ピンの形式が変わった。**音声ピンの `Receive` から呼ぶ。**
    /// ロックも確保もせず、回数と新しい形式を Atomic へ書くだけ。ログはワーカーが
    /// `log_format_changes` で出す
    pub fn record_format_change(&self, format: Option<PinFormat>) {
        self.inner
            .format_latest
            .store(pack_pin_format(format), Ordering::Relaxed);
        self.inner.format_changes.fetch_add(1, Ordering::Release);
    }

    /// 前に読んだときから形式が変わっていれば、その回数と最後の形式。
    /// 読んだところまでを出し終えたことにする。ワーカーだけが呼ぶ
    pub(super) fn take_format_changes(&self) -> Option<(u32, Option<PinFormat>)> {
        let count = self.inner.format_changes.load(Ordering::Acquire);
        let logged = self
            .inner
            .format_changes_logged
            .swap(count, Ordering::Relaxed);
        if count == logged {
            return None;
        }
        let latest = unpack_pin_format(self.inner.format_latest.load(Ordering::Relaxed));
        Some((count.wrapping_sub(logged), latest))
    }

    /// 音声ピンの形式が流れの途中で変わっていたら、ログへ出す。**ワーカーの監視の
    /// 周期で呼ぶ**（`video::directshow` の `link_state`）。
    ///
    /// 繋いだときと食い違う形式のサンプルは `push` が積まずに捨てるので、音が止まる。
    /// `Receive` の中ではログを出せないので、ここで理由を残す
    /// （`docs/design/directshow-audio.md` の「`Receive` の中でしないこと」）。
    pub fn log_format_changes(&self) {
        let Some((changes, latest)) = self.take_format_changes() else {
            return;
        };
        let connected = self.connection().map(|connection| connection.format);
        let Some(report) = format_change_report(changes, connected, latest) else {
            // 繋いだ記録が書かれる前（グラフを動かした直後）。比べる相手が無い
            log::debug!(
                "音声ピンの形式が繋いだ記録の前に変わった（前回の確認から {} 回）",
                changes
            );
            return;
        };
        let connected_text = connected.map_or_else(String::new, PinFormat::summary);
        match report {
            FormatChangeReport::Mismatch(format) => log::warn!(
                "音声ピンの形式が繋いだときの形式（{}）から {} に変わったので、その音声を捨てている（前回の確認から {} 回）",
                connected_text,
                format.summary(),
                changes
            ),
            FormatChangeReport::Unsupported => log::warn!(
                "音声ピンの形式が受け取れない形式に変わったので、その音声を捨てている（繋いだときの形式: {}、前回の確認から {} 回）",
                connected_text,
                changes
            ),
            FormatChangeReport::RestoredAfterDetour => log::warn!(
                "音声ピンの形式が途中で変わり、繋いだときの形式（{}）に戻った。変わっていた間の音声は捨てた（前回の確認から {} 回）",
                connected_text,
                changes
            ),
            FormatChangeReport::Restored => log::info!(
                "音声ピンの形式が繋いだときの形式（{}）に戻った（前回の確認から {} 回）",
                connected_text,
                changes
            ),
        }
    }

    /// 音声ピンを繋いだことを書く。映像のバックエンドがグラフを動かしたあとに呼ぶ
    pub fn set_connected(&self, connection: PinConnection) {
        self.set_connection(Some(connection));
    }

    /// 繋いだ記録を消す。映像のバックエンドがグラフを捨てるときに呼ぶ
    pub fn clear(&self) {
        self.set_connection(None);
    }

    fn set_connection(&self, value: Option<PinConnection>) {
        match self.inner.connection.lock() {
            Ok(mut guard) => *guard = value,
            Err(poisoned) => *poisoned.into_inner() = value,
        }
    }

    /// いま繋いでいる音声ピン。繋いでいなければ `None`
    pub fn connection(&self) -> Option<PinConnection> {
        match self.inner.connection.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// いまのグラフで実際に受け取った塊の最大のバイト数。まだ受け取っていなければ `None`。
    /// アロケーターから塊の大きさを取れなかったときの代わりと、ログに使う
    pub fn observed_chunk_bytes(&self) -> Option<u32> {
        match self.inner.observed_chunk_bytes.load(Ordering::Relaxed) {
            0 => None,
            bytes => Some(bytes),
        }
    }

    /// 差し込む。**音声のバックエンドが開くときに呼ぶ**（ワーカースレッド）。
    /// `Receive` が積んでいる最中なら終わるまで待つ（`Receive` 側は待たない）
    pub(super) fn attach(&self, sink: PinSink) {
        self.set_sink(Some(sink));
    }

    /// 抜く。**音声のバックエンドが閉じるときに、出力を落とす前に呼ぶ**
    pub(super) fn detach(&self) {
        self.set_sink(None);
    }

    fn set_sink(&self, value: Option<PinSink>) {
        match self.inner.sink.lock() {
            Ok(mut guard) => *guard = value,
            Err(poisoned) => *poisoned.into_inner() = value,
        }
    }

    /// 音声ピンの `Receive` が受け取った塊を積む。
    ///
    /// **キャプチャーフィルターのストリーミングスレッドから呼ばれる。ロックを
    /// 待たず、確保もしない。** 差し込み先が無い・使用中・グラフの番号か形式が
    /// 違うときは捨てる。`discontinuity` はサンプルの不連続の印で、取りこぼしとして
    /// 数える（「接続状態」タブの「入力の取りこぼし」）。
    ///
    /// **塊がフレームの途中で切れていたら、端数を次の塊へ持ち越す**（`PinCarry`）。
    /// 次の塊の頭で 1 フレームに組み直してから積むので、塊の切れ目で左右がずれない。
    /// 不連続の印が付いた塊・形式が食い違った塊では、持ち越した端数を捨てる。
    pub fn push(&self, graph: u64, format: PinFormat, data: &[u8], discontinuity: bool) {
        let len = u32::try_from(data.len()).unwrap_or(u32::MAX);
        self.inner
            .observed_chunk_bytes
            .fetch_max(len, Ordering::Relaxed);
        let Ok(mut guard) = self.inner.sink.try_lock() else {
            return;
        };
        let Some(sink) = guard.as_mut() else {
            return;
        };
        if sink.graph != graph {
            return;
        }
        if sink.format != format {
            sink.stream.carry.clear();
            return;
        }
        if discontinuity && sink.stream.started {
            count_xrun(&sink.xruns);
        }
        if discontinuity {
            sink.stream.carry.clear();
        }
        sink.stream.started = true;
        let frame = format.bytes_per_frame();
        let mut data = data;
        if !sink.stream.carry.is_empty() {
            let (joined, rest) = sink.stream.carry.fill(frame, data);
            data = rest;
            if !joined {
                // この塊は全部端数に吸われた。まだ 1 フレームに揃わない
                return;
            }
            let mut carried = PinCarry::default();
            std::mem::swap(&mut carried, &mut sink.stream.carry);
            Self::push_whole(sink, format, carried.bytes());
        }
        let whole = data.len() / frame.max(1) * frame;
        let (head, tail) = data.split_at(whole);
        Self::push_whole(sink, format, head);
        sink.stream.carry.keep(tail, frame);
    }

    /// 丸ごとのフレームが並んだバイト列を、リングと録画の差し込み口へ積む。
    fn push_whole(sink: &PinSink, format: PinFormat, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let channels = usize::from(format.channels);
        match format.sample_type {
            PinSampleType::I16 => process_input_iter(
                data.as_chunks::<2>()
                    .0
                    .iter()
                    .map(|bytes| i16::from_le_bytes(*bytes)),
                channels,
                &sink.producer,
                &sink.tap,
                &sink.dropped_frames,
                i16_to_f32,
            ),
            PinSampleType::F32 => process_input_iter(
                data.as_chunks::<4>()
                    .0
                    .iter()
                    .map(|bytes| f32::from_le_bytes(*bytes)),
                channels,
                &sink.producer,
                &sink.tap,
                &sink.dropped_frames,
                |sample| sample,
            ),
        }
    }
}

/// 音声ピンの塊の長さに合わせて、リングバッファの長さ（ms）を決める。
///
/// 目標水位（設定のバッファ長ぶん）が塊 2 つぶんに満たなければ、塊 2 つぶんまで
/// 広げる。塊が 10ms 前後なら設定のまま（下限の 20ms でも足りる）。
/// 塊の長さが分からなければ設定のまま。**設定の `buffer_ms` は書き換えない**
/// （デバイスを替えれば元の長さで開く。`docs/design/directshow-audio.md` の「塊の長さ」）。
/// 壊れた値で巨大なリングを確保しないよう、`MAX_WIDENED_BUFFER_MS` で頭打ちにする。
pub(super) fn widened_buffer_ms(buffer_ms: u32, chunk_ms: Option<u32>) -> u32 {
    let Some(chunk_ms) = chunk_ms else {
        return buffer_ms;
    };
    buffer_ms.max(chunk_ms.saturating_mul(2).min(MAX_WIDENED_BUFFER_MS))
}

/// 塊の長さに合わせて広げるときの上限（ms）
const MAX_WIDENED_BUFFER_MS: u32 = 2_000;

#[cfg(test)]
mod tests {
    use super::*;
    use ringbuf::traits::{Consumer, Observer};

    const GC551: PinFormat = PinFormat {
        sample_rate: 48_000,
        channels: 2,
        sample_type: PinSampleType::I16,
    };

    /// 差し込んだ受け口と、そこから読み出す側
    fn attached(
        feed: &AudioPinFeed,
        graph: u64,
        format: PinFormat,
    ) -> (
        Arc<Mutex<super::super::stream::AudioConsumer>>,
        Arc<AtomicU32>,
    ) {
        use ringbuf::traits::Split;
        let (producer, consumer) = ringbuf::HeapRb::<f32>::new(64).split();
        let xruns = Arc::new(AtomicU32::new(0));
        feed.attach(PinSink {
            graph,
            format,
            producer: Arc::new(Mutex::new(producer)),
            tap: AudioTap::new(),
            dropped_frames: Arc::new(AtomicU32::new(0)),
            xruns: xruns.clone(),
            stream: Default::default(),
        });
        (Arc::new(Mutex::new(consumer)), xruns)
    }

    fn drain(consumer: &Mutex<super::super::stream::AudioConsumer>) -> Vec<f32> {
        let mut consumer = consumer.lock().expect("読める");
        let mut out = Vec::new();
        while let Some(sample) = consumer.try_pop() {
            out.push(sample);
        }
        out
    }

    #[test]
    fn push_decodes_little_endian_i16_into_the_ring() {
        let feed = AudioPinFeed::new();
        let graph = feed.begin_graph();
        let (consumer, _) = attached(&feed, graph, GC551);
        // 0x4000 = 16384 → 0.5、0xC000 = -16384 → -0.5
        feed.push(graph, GC551, &[0x00, 0x40, 0x00, 0xC0], false);
        assert_eq!(drain(&consumer), vec![0.5, -0.5]);
    }

    #[test]
    fn push_decodes_f32_samples() {
        let format = PinFormat {
            sample_type: PinSampleType::F32,
            ..GC551
        };
        let feed = AudioPinFeed::new();
        let graph = feed.begin_graph();
        let (consumer, _) = attached(&feed, graph, format);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0.25f32.to_le_bytes());
        bytes.extend_from_slice(&(-1.0f32).to_le_bytes());
        feed.push(graph, format, &bytes, false);
        assert_eq!(drain(&consumer), vec![0.25, -1.0]);
    }

    #[test]
    fn push_from_another_graph_is_dropped() {
        // 映像を開き直したあと、止まる途中の古いグラフの Receive が届いた場合
        let feed = AudioPinFeed::new();
        let old = feed.begin_graph();
        let new = feed.begin_graph();
        let (consumer, _) = attached(&feed, new, GC551);
        feed.push(old, GC551, &[0x00, 0x40, 0x00, 0x40], false);
        assert!(drain(&consumer).is_empty());
        feed.push(new, GC551, &[0x00, 0x40, 0x00, 0x40], false);
        assert_eq!(drain(&consumer).len(), 2);
    }

    #[test]
    fn push_with_a_changed_format_is_dropped() {
        // 流れの途中で形式が変わったら、差し込んだときの形のリングへは積まない
        let feed = AudioPinFeed::new();
        let graph = feed.begin_graph();
        let (consumer, _) = attached(&feed, graph, GC551);
        let mono = PinFormat {
            channels: 1,
            ..GC551
        };
        feed.push(graph, mono, &[0x00, 0x40, 0x00, 0x40], false);
        assert!(drain(&consumer).is_empty());
    }

    #[test]
    fn push_without_a_sink_only_records_the_chunk_length() {
        let feed = AudioPinFeed::new();
        let graph = feed.begin_graph();
        assert_eq!(feed.observed_chunk_bytes(), None);
        feed.push(graph, GC551, &[0; 1920], false);
        feed.push(graph, GC551, &[0; 960], false);
        // 長いほうを覚える
        assert_eq!(feed.observed_chunk_bytes(), Some(1920));
        // 次のグラフでは数え直す
        feed.begin_graph();
        assert_eq!(feed.observed_chunk_bytes(), None);
    }

    #[test]
    fn push_ignores_a_trailing_partial_sample() {
        // 2 バイトに満たない端は読まない（並びを前提にしない）
        let feed = AudioPinFeed::new();
        let graph = feed.begin_graph();
        let (consumer, _) = attached(&feed, graph, GC551);
        feed.push(graph, GC551, &[0x00, 0x40, 0x00, 0x40, 0x7F], false);
        assert_eq!(drain(&consumer).len(), 2);
    }

    #[test]
    fn push_carries_a_split_frame_into_the_next_chunk() {
        // 連続した 2ch 16bit の流れを、フレームの途中（奇数のサンプル、サンプルの
        // 途中のバイトも含む）で切って渡す。録画とパススルーの両方で、最後まで
        // 左（正）と右（負）が入れ替わらずに全フレームが並ぶ
        use ringbuf::traits::Split;
        const FRAMES: usize = 40;
        let feed = AudioPinFeed::new();
        let graph = feed.begin_graph();
        let (producer, consumer) = ringbuf::HeapRb::<f32>::new(FRAMES * 2).split();
        let consumer = Mutex::new(consumer);
        let tap = AudioTap::new();
        let mut attachment = tap.attach(FRAMES * 2);
        feed.attach(PinSink {
            graph,
            format: GC551,
            producer: Arc::new(Mutex::new(producer)),
            tap: tap.clone(),
            dropped_frames: Arc::new(AtomicU32::new(0)),
            xruns: Arc::new(AtomicU32::new(0)),
            stream: Default::default(),
        });
        let mut expected = Vec::new();
        let mut bytes = Vec::new();
        for i in 0..FRAMES {
            let value = 1000 + i as i16;
            for sample in [value, -value] {
                bytes.extend_from_slice(&sample.to_le_bytes());
                expected.push(i16_to_f32(sample));
            }
        }
        // 6 バイト = 1.5 フレーム、3 バイト = サンプルの途中、など
        let mut rest = bytes.as_slice();
        for size in [6, 3, 7, 2, 1, 1, 10, 5].iter().cycle() {
            if rest.is_empty() {
                break;
            }
            let (chunk, tail) = rest.split_at((*size).min(rest.len()));
            feed.push(graph, GC551, chunk, false);
            rest = tail;
        }

        let mut recorded = vec![0.0f32; FRAMES * 2];
        let count = attachment.consumer.pop_slice(&mut recorded);
        assert_eq!(&recorded[..count], expected.as_slice());
        assert_eq!(drain(&consumer), expected);
    }

    #[test]
    fn push_drops_the_carried_bytes_on_a_discontinuity_or_a_format_change() {
        let feed = AudioPinFeed::new();
        let graph = feed.begin_graph();
        let (consumer, _) = attached(&feed, graph, GC551);
        // 1 フレームに満たない 3 バイトは積まずに持ち越す
        feed.push(graph, GC551, &[0x00, 0x40, 0x00], false);
        // 不連続の印が付いたら、端数を足さずに新しいフレームから積む
        feed.push(graph, GC551, &[0x00, 0xC0, 0x00, 0x40], true);
        assert_eq!(drain(&consumer), vec![-0.5, 0.5]);
        // 形式が食い違った塊を挟んでも、端数は捨てる
        feed.push(graph, GC551, &[0x00, 0x40, 0x00], false);
        let mono = PinFormat {
            channels: 1,
            ..GC551
        };
        feed.push(graph, mono, &[0x00, 0x40], false);
        feed.push(graph, GC551, &[0x00, 0x40, 0x00, 0xC0], false);
        assert_eq!(drain(&consumer), vec![0.5, -0.5]);
    }

    #[test]
    fn take_format_changes_reports_only_new_changes() {
        let feed = AudioPinFeed::new();
        feed.begin_graph();
        assert_eq!(feed.take_format_changes(), None);
        let mono = PinFormat {
            channels: 1,
            ..GC551
        };
        feed.record_format_change(Some(mono));
        feed.record_format_change(None);
        // 周期の間に 2 回変わったら、回数と最後の形式を 1 度だけ返す
        assert_eq!(feed.take_format_changes(), Some((2, None)));
        assert_eq!(feed.take_format_changes(), None);
        feed.record_format_change(Some(GC551));
        assert_eq!(feed.take_format_changes(), Some((1, Some(GC551))));
        // 次のグラフでは数え直す
        feed.record_format_change(Some(mono));
        feed.begin_graph();
        assert_eq!(feed.take_format_changes(), None);
    }

    #[test]
    fn push_counts_discontinuities_after_the_first_sample() {
        // 最初のサンプルの不連続の印は、流れ始めの印なので数えない
        let feed = AudioPinFeed::new();
        let graph = feed.begin_graph();
        let (_consumer, xruns) = attached(&feed, graph, GC551);
        feed.push(graph, GC551, &[0; 4], true);
        assert_eq!(xruns.load(Ordering::Relaxed), 0);
        feed.push(graph, GC551, &[0; 4], false);
        feed.push(graph, GC551, &[0; 4], true);
        assert_eq!(xruns.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn push_after_detach_is_dropped() {
        let feed = AudioPinFeed::new();
        let graph = feed.begin_graph();
        let (consumer, _) = attached(&feed, graph, GC551);
        feed.detach();
        feed.push(graph, GC551, &[0x00, 0x40, 0x00, 0x40], false);
        assert!(consumer.lock().expect("読める").is_empty());
    }

    #[test]
    fn begin_graph_numbers_increase_and_clear_the_connection() {
        let feed = AudioPinFeed::new();
        let first = feed.begin_graph();
        feed.set_connected(PinConnection {
            graph: first,
            device: "AVerMedia GC551 Video Capture".to_string(),
            format: GC551,
            chunk_bytes: Some(1920),
        });
        assert_eq!(feed.connection().map(|c| c.graph), Some(first));
        let second = feed.begin_graph();
        assert!(second > first);
        assert_eq!(feed.connection(), None);
    }

    #[test]
    fn chunk_ms_rounds_up_and_rejects_a_broken_format() {
        // 48kHz 2ch 16bit の 10ms は 1920 バイト
        assert_eq!(GC551.chunk_ms(1920), Some(10));
        assert_eq!(GC551.chunk_ms(1921), Some(11));
        assert_eq!(GC551.chunk_ms(96_000), Some(500));
        let broken = PinFormat {
            channels: 0,
            ..GC551
        };
        assert_eq!(broken.chunk_ms(1920), None);
    }

    #[test]
    fn summary_shows_rate_channels_and_bits() {
        assert_eq!(GC551.summary(), "48000Hz 2ch 16bit");
    }

    #[test]
    fn widened_buffer_ms_keeps_the_setting_for_short_chunks() {
        assert_eq!(widened_buffer_ms(50, Some(10)), 50);
        // 下限の 20ms でも 10ms の塊なら足りる
        assert_eq!(widened_buffer_ms(20, Some(10)), 20);
        assert_eq!(widened_buffer_ms(50, None), 50);
    }

    #[test]
    fn widened_buffer_ms_grows_to_two_chunks() {
        // 500ms の塊は 1 回で溢れるので、塊 2 つぶんまで広げる
        assert_eq!(widened_buffer_ms(50, Some(500)), 1000);
        assert_eq!(widened_buffer_ms(50, Some(30)), 60);
        // 壊れた値で巨大なリングを確保しない
        assert_eq!(widened_buffer_ms(50, Some(u32::MAX)), 2_000);
    }
}
