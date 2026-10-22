//! 音声ピンの流れの状態と、流れの途中での形式の変化の扱い（#511）。
//!
//! `pin_feed.rs` の `AudioPinFeed` が使う部品のうち、`Receive` のスレッドが触る
//! 流れの状態（`PinStream` と、塊の切れ目の端数を持ち越す `PinCarry`）と、
//! 形式の変化と捨てた塊の数を `Receive` からワーカー・`push` へ渡す Atomic の控え
//! （`StreamChanges`、形式は `pack_pin_format` で詰める）・ワーカーがログを
//! どう出すかの判定（`format_change_report`、純粋関数）を置く。どれも
//! **ロックもアロケーションもしない。**

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use super::pin_feed::{PinFormat, PinSampleType};

/// 流れの途中での形式の変化と、`Receive` が `push` を呼ばずに捨てた塊の数。
/// `AudioPinFeed` が 1 つ持つ。
#[derive(Default)]
pub(super) struct StreamChanges {
    /// いまのグラフで、流れの途中で形式が変わった回数。`Receive` が足す
    formats: AtomicU32,
    /// 最後に変わった先の形式（`pack_pin_format`）。`Receive` が書く
    latest: AtomicU64,
    /// `formats` のうち、ワーカーがログへ出し終えたところ
    logged: AtomicU32,
    /// `Receive` が `push` を呼ばずに塊を捨てた回数（形式の変化を含む）。`push` は
    /// 前に見た値と違えば、持ち越した端数を捨てる（捨てた区間をまたいでつながない）。
    /// 数え直さない（単調に増える）
    breaks: AtomicU32,
}

impl StreamChanges {
    /// 形式の変化を数え直す。グラフを組む前に呼ぶ（前のグラフの `Receive` は止まっている）
    pub(super) fn reset_formats(&self) {
        self.formats.store(0, Ordering::Relaxed);
        self.logged.store(0, Ordering::Relaxed);
        self.latest.store(0, Ordering::Relaxed);
    }

    /// 形式が変わった。`Receive` から呼ぶ。端数もつながらなくなる
    pub(super) fn record_format(&self, format: Option<PinFormat>) {
        self.latest
            .store(pack_pin_format(format), Ordering::Relaxed);
        self.formats.fetch_add(1, Ordering::Release);
        self.note_break();
    }

    /// `push` を呼ばずに塊を捨てた。`Receive` から呼ぶ
    pub(super) fn note_break(&self) {
        self.breaks.fetch_add(1, Ordering::Release);
    }

    /// 捨てた塊の数。`push` が前に見た値と比べる
    pub(super) fn breaks(&self) -> u32 {
        self.breaks.load(Ordering::Acquire)
    }

    /// 前に読んだときから形式が変わっていれば、その回数と最後の形式。
    /// 読んだところまでを出し終えたことにする。ワーカーだけが呼ぶ
    pub(super) fn take_formats(&self) -> Option<(u32, Option<PinFormat>)> {
        let count = self.formats.load(Ordering::Acquire);
        let logged = self.logged.swap(count, Ordering::Relaxed);
        if count == logged {
            return None;
        }
        let latest = unpack_pin_format(self.latest.load(Ordering::Relaxed));
        Some((count.wrapping_sub(logged), latest))
    }
}

/// 差し込み先ごとの、流れの状態。触るのは `push`（`Receive` のスレッド）だけ。
#[derive(Default)]
pub(super) struct PinStream {
    /// 1 つでも積んだか。最初のサンプルには不連続の印が付くのがふつうなので、
    /// それは取りこぼしに数えない
    pub(super) started: bool,
    /// 前の塊の終わりに残った、1 フレームに満たない端数
    pub(super) carry: PinCarry,
    /// 前の `push` で見た `stream_breaks`（`AudioPinFeed`）。差し込んだ直後は 0 で、
    /// 違っていても端数は空なので何も捨てない
    pub(super) breaks_seen: u32,
}

/// 端数を持ち越せる 1 フレームの長さの上限（バイト）。16ch の f32 = 64 バイトまで。
/// それより大きいフレームの端数は捨て、`process_input_iter` の切り詰めに任せる。
/// 置き場所は 1 フレームぶん（端数に次の塊の頭を足して組み直すため）
const MAX_CARRY_FRAME_BYTES: usize = 64;

/// 塊の終わりに残った端数の置き場所。**`Receive` が触るので固定長で、確保しない。**
pub(super) struct PinCarry {
    bytes: [u8; MAX_CARRY_FRAME_BYTES],
    len: usize,
}

impl Default for PinCarry {
    fn default() -> Self {
        Self {
            bytes: [0; MAX_CARRY_FRAME_BYTES],
            len: 0,
        }
    }
}

impl PinCarry {
    pub(super) fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub(super) fn clear(&mut self) {
        self.len = 0;
    }

    pub(super) fn bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }

    /// 端数のあとへ `data` の頭を足して 1 フレーム（`frame` バイト）を目指す。
    /// 揃ったら `true` と、`data` の残りを返す。揃わなければ `data` を全部吸う
    pub(super) fn fill<'a>(&mut self, frame: usize, data: &'a [u8]) -> (bool, &'a [u8]) {
        let need = frame.saturating_sub(self.len);
        let take = need.min(data.len());
        let (head, rest) = data.split_at(take);
        self.bytes[self.len..self.len + take].copy_from_slice(head);
        self.len += take;
        (self.len == frame, rest)
    }

    /// 塊の終わりの端数（1 フレームに満たない）を次へ持ち越す。入らなければ捨てる
    pub(super) fn keep(&mut self, tail: &[u8], frame: usize) {
        if tail.is_empty() || tail.len() >= frame || frame > MAX_CARRY_FRAME_BYTES {
            self.len = 0;
            return;
        }
        self.bytes[..tail.len()].copy_from_slice(tail);
        self.len = tail.len();
    }
}

/// 流れの途中で形式が変わったことを、ワーカーがどうログへ出すか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FormatChangeReport {
    /// 繋いだときの形式と食い違う形式に変わった。その音声は捨てている
    Mismatch(PinFormat),
    /// 受け取れない形式に変わった。その音声は捨てている
    Unsupported,
    /// 前の確認で食い違いを出したあと、繋いだときの形式に戻った（1 回だけ変わった）。
    /// また積んでいる
    Restored,
    /// 周期の間に 2 回以上変わり、最後は繋いだときの形式に戻った（A → B → A）。
    /// 途中の形式の間の音声は捨てた
    RestoredAfterDetour,
}

/// 周期の間に変わった回数・最後に変わった先の形式・繋いだときの形式から、ログの
/// 出し方を決める。繋いだ記録が無ければ（差し込む前）比べる相手が無いので `None`
pub(super) fn format_change_report(
    changes: u32,
    connected: Option<PinFormat>,
    latest: Option<PinFormat>,
) -> Option<FormatChangeReport> {
    let connected = connected?;
    Some(match latest {
        None => FormatChangeReport::Unsupported,
        Some(latest) if latest == connected && changes >= 2 => {
            FormatChangeReport::RestoredAfterDetour
        }
        Some(latest) if latest == connected => FormatChangeReport::Restored,
        Some(latest) => FormatChangeReport::Mismatch(latest),
    })
}

const PIN_FORMAT_PRESENT: u64 = 1 << 63;
const PIN_FORMAT_F32: u64 = 1 << 62;

/// 形式を 1 つの `u64` に詰める。`None`（受け取れない形式）は 0
pub(super) fn pack_pin_format(format: Option<PinFormat>) -> u64 {
    let Some(format) = format else {
        return 0;
    };
    let sample_type = match format.sample_type {
        PinSampleType::I16 => 0,
        PinSampleType::F32 => PIN_FORMAT_F32,
    };
    PIN_FORMAT_PRESENT
        | sample_type
        | (u64::from(format.channels) << 32)
        | u64::from(format.sample_rate)
}

/// `pack_pin_format` の逆
pub(super) fn unpack_pin_format(packed: u64) -> Option<PinFormat> {
    if packed & PIN_FORMAT_PRESENT == 0 {
        return None;
    }
    Some(PinFormat {
        sample_rate: (packed & 0xFFFF_FFFF) as u32,
        channels: ((packed >> 32) & 0xFFFF) as u16,
        sample_type: if packed & PIN_FORMAT_F32 != 0 {
            PinSampleType::F32
        } else {
            PinSampleType::I16
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const GC551: PinFormat = PinFormat {
        sample_rate: 48_000,
        channels: 2,
        sample_type: PinSampleType::I16,
    };

    #[test]
    fn pack_pin_format_round_trips() {
        let f32_mono = PinFormat {
            sample_rate: 44_100,
            channels: 1,
            sample_type: PinSampleType::F32,
        };
        for format in [GC551, f32_mono] {
            assert_eq!(
                unpack_pin_format(pack_pin_format(Some(format))),
                Some(format)
            );
        }
        assert_eq!(pack_pin_format(None), 0);
        assert_eq!(unpack_pin_format(0), None);
    }

    #[test]
    fn format_change_report_compares_with_the_connected_format() {
        let mono = PinFormat {
            channels: 1,
            ..GC551
        };
        assert_eq!(
            format_change_report(1, Some(GC551), Some(mono)),
            Some(FormatChangeReport::Mismatch(mono))
        );
        assert_eq!(
            format_change_report(1, Some(GC551), Some(GC551)),
            Some(FormatChangeReport::Restored)
        );
        assert_eq!(
            format_change_report(1, Some(GC551), None),
            Some(FormatChangeReport::Unsupported)
        );
        // 繋いだ記録が無ければ比べる相手が無いので出さない
        assert_eq!(format_change_report(1, None, Some(GC551)), None);
    }

    #[test]
    fn take_formats_reports_only_new_changes() {
        let changes = StreamChanges::default();
        assert_eq!(changes.take_formats(), None);
        let mono = PinFormat {
            channels: 1,
            ..GC551
        };
        changes.record_format(Some(mono));
        changes.record_format(None);
        // 周期の間に 2 回変わったら、回数と最後の形式を 1 度だけ返す
        assert_eq!(changes.take_formats(), Some((2, None)));
        assert_eq!(changes.take_formats(), None);
        changes.record_format(Some(GC551));
        assert_eq!(changes.take_formats(), Some((1, Some(GC551))));
        // 次のグラフでは数え直す
        changes.record_format(Some(mono));
        changes.reset_formats();
        assert_eq!(changes.take_formats(), None);
    }

    #[test]
    fn breaks_count_dropped_chunks_and_format_changes() {
        let changes = StreamChanges::default();
        changes.note_break();
        changes.record_format(None);
        assert_eq!(changes.breaks(), 2);
        // 形式の数え直しでは減らさない（`push` が前に見た値と比べるため）
        changes.reset_formats();
        assert_eq!(changes.breaks(), 2);
    }

    #[test]
    fn format_change_report_tells_a_detour_within_one_period() {
        // 周期の間に A → B → A と変わったら、戻っただけでなく途中で捨てたことを出す
        assert_eq!(
            format_change_report(2, Some(GC551), Some(GC551)),
            Some(FormatChangeReport::RestoredAfterDetour)
        );
        // 食い違ったままなら回数によらず食い違い
        let mono = PinFormat {
            channels: 1,
            ..GC551
        };
        assert_eq!(
            format_change_report(3, Some(GC551), Some(mono)),
            Some(FormatChangeReport::Mismatch(mono))
        );
    }
}
