//! 映像の入力（NV12）のサンプルを使い回すプール `SamplePool`（#382）。
//!
//! 1080p の NV12 は 1 枚約 3MB。毎フレーム `MFCreateMemoryBuffer` で確保し直すと、
//! 触ったページがすべて新しくフォールトする（1080p60 で毎秒約 4.4 万回）。
//! そこで、Sink Writer（①②）やエンコーダ MFT（③）へ渡したサンプルを手元にも持っておき、
//! **向こうが手放したもの（参照が手元の 1 つだけに戻ったもの）だけ**を次のフレームに使う。
//!
//! - 渡したサンプルは非同期に消費される（Sink Writer は内部の作業キューへ積み、
//!   エンコーダはエンコードするまで持つ）。COM の決まりで、持っている間は参照を持つ
//! - サンプルだけでなくバッファの参照も見る。向こうがバッファだけを別のサンプルへ
//!   付け替えて持っていることがありうるため
//! - 手放されたものが無ければ新しく作る。持つのは `capacity` 個までで、それを超えた分は
//!   使い捨てにする（持ちすぎるとその分のメモリを抱え続ける）
//!
//! **録画スレッドだけが触る。** 判定（`slot_state`）は純粋関数。

use std::ptr;

use windows::core::{IUnknown_Vtbl, Interface};
use windows::Win32::Media::MediaFoundation::IMFSample;

use super::writer::memory_sample;

/// 1 つのプールが持つサンプルの数。1080p で約 12MB、4K で約 48MB を抱える。
/// フェイクの 1080p60 で録画（①）とリプレイバッファ（③）の両方を測り、
/// これで毎フレームの確保がほぼ無くなることを確かめた（`docs/design/recording.md`）。
pub(super) const SAMPLE_POOL_CAPACITY: usize = 4;

/// プールの 1 つのサンプルを次のフレームに使えるか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotState {
    /// 手放されていて、大きさも足りる
    Ready,
    /// まだ向こうが持っている
    Busy,
    /// 手放されているが、バッファが 1 つでないか大きさが足りない（大きさが変わった）。捨てる
    Unfit,
}

/// 参照の数から、使えるかを決める。
///
/// - `sample_refs`: サンプルの参照の数。プールの 1 つだけなら手放されている
/// - `buffer_refs`: バッファの参照の数。サンプルが持つ 1 つと、確かめるために取り出した
///   1 つの計 2 つだけなら手放されている。サンプルが手放されていなければ見ない（`None`）
/// - `buffer_count` / `max_length`: サンプルのバッファの数と、その大きさ
fn slot_state(
    sample_refs: u32,
    buffer_refs: Option<u32>,
    buffer_count: u32,
    max_length: u32,
    needed: u32,
) -> SlotState {
    if sample_refs != 1 {
        return SlotState::Busy;
    }
    if buffer_count != 1 || max_length < needed {
        return SlotState::Unfit;
    }
    match buffer_refs {
        Some(2) => SlotState::Ready,
        _ => SlotState::Busy,
    }
}

/// COM のオブジェクトの今の参照の数。`AddRef` と `Release` の戻り値から読む。
///
/// 戻り値は本来は診断用だが、MF のサンプルとメモリバッファは正確な数を返す
/// （`#[ignore]` のテストで確かめている）。数が 1 なら持っているのは自分だけで、
/// 他のスレッドが新しく参照を増やすこともできない（ポインタを持っていないため）。
fn ref_count<T: Interface>(object: &T) -> u32 {
    let raw = object.as_raw();
    // SAFETY: `raw` は生きている COM のインターフェイスで、先頭は IUnknown の vtable を指す。
    // AddRef と Release を 1 回ずつ呼ぶので、参照の数は呼ぶ前と同じに戻る
    unsafe {
        let vtable = *(raw as *const *const IUnknown_Vtbl);
        ((*vtable).AddRef)(raw);
        ((*vtable).Release)(raw)
    }
}

/// 映像の入力のサンプルのプール。
pub(super) struct SamplePool {
    samples: Vec<IMFSample>,
    capacity: usize,
}

impl SamplePool {
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            samples: Vec::with_capacity(capacity),
            capacity,
        }
    }

    /// `data` を写したサンプルを返す。手放されたものがあれば使い回し、無ければ作る。
    /// `pts` と `duration` は 100ns 単位。
    pub(super) fn sample(
        &mut self,
        data: &[u8],
        pts: i64,
        duration: i64,
    ) -> windows::core::Result<IMFSample> {
        let needed = data.len() as u32;
        let mut index = 0;
        while index < self.samples.len() {
            match self.reuse(index, data, pts, duration)? {
                SlotState::Ready => return Ok(self.samples[index].clone()),
                SlotState::Busy => index += 1,
                SlotState::Unfit => {
                    self.samples.swap_remove(index);
                }
            }
        }
        let sample = memory_sample(data, pts, duration)?;
        if self.samples.len() < self.capacity && needed > 0 {
            self.samples.push(sample.clone());
        }
        Ok(sample)
    }

    /// `index` のサンプルが使えるなら `data` を写して `Ready` を返す。
    fn reuse(
        &self,
        index: usize,
        data: &[u8],
        pts: i64,
        duration: i64,
    ) -> windows::core::Result<SlotState> {
        let sample = &self.samples[index];
        let needed = data.len() as u32;
        let sample_refs = ref_count(sample);
        if sample_refs != 1 {
            return Ok(SlotState::Busy);
        }
        let count = unsafe { sample.GetBufferCount() }?;
        if count != 1 {
            return Ok(slot_state(sample_refs, None, count, 0, needed));
        }
        let buffer = unsafe { sample.GetBufferByIndex(0) }?;
        let max_length = unsafe { buffer.GetMaxLength() }?;
        let state = slot_state(
            sample_refs,
            Some(ref_count(&buffer)),
            count,
            max_length,
            needed,
        );
        if state != SlotState::Ready {
            return Ok(state);
        }
        let mut target: *mut u8 = ptr::null_mut();
        unsafe { buffer.Lock(&mut target, None, None) }?;
        // SAFETY: Lock が長さ `max_length`（>= `needed`）の書き込める領域を返している
        unsafe { ptr::copy_nonoverlapping(data.as_ptr(), target, data.len()) };
        unsafe {
            buffer.Unlock()?;
            buffer.SetCurrentLength(needed)?;
            // 前に渡したときに向こうが付けた属性を次へ持ち越さない
            sample.DeleteAllItems()?;
            sample.SetSampleTime(pts)?;
            sample.SetSampleDuration(duration)?;
        }
        Ok(SlotState::Ready)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::com::{ComApartment, ComModel, MfPlatform};
    use crate::recording::writer::{SinkWriter, WriterParams};
    use tempfile::tempdir;
    use windows::core::HSTRING;
    use windows::Win32::Media::MediaFoundation::{
        MFCreateMediaType, MFCreateSourceReaderFromURL, MFMediaType_Video, MFVideoFormat_NV12,
        MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_SOURCE_READERF_ENDOFSTREAM,
        MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    };

    #[test]
    fn slot_state_is_busy_while_the_sample_is_held_elsewhere() {
        assert_eq!(slot_state(2, None, 1, 100, 100), SlotState::Busy);
        // サンプルを持たれていれば、大きさが合わなくても捨てない（向こうがまだ使う）
        assert_eq!(slot_state(3, None, 1, 10, 100), SlotState::Busy);
    }

    #[test]
    fn slot_state_is_busy_while_the_buffer_is_held_elsewhere() {
        assert_eq!(slot_state(1, Some(3), 1, 100, 100), SlotState::Busy);
        assert_eq!(slot_state(1, None, 1, 100, 100), SlotState::Busy);
    }

    #[test]
    fn slot_state_is_ready_only_when_both_are_released_and_large_enough() {
        assert_eq!(slot_state(1, Some(2), 1, 100, 100), SlotState::Ready);
        assert_eq!(slot_state(1, Some(2), 1, 200, 100), SlotState::Ready);
    }

    #[test]
    fn slot_state_drops_a_released_sample_that_does_not_fit() {
        assert_eq!(slot_state(1, Some(2), 1, 99, 100), SlotState::Unfit);
        assert_eq!(slot_state(1, None, 2, 100, 100), SlotState::Unfit);
        assert_eq!(slot_state(1, None, 0, 0, 100), SlotState::Unfit);
    }

    #[test]
    #[ignore = "Media Foundation が必要（CI のランナーにあるかは未確認）"]
    fn pool_reuses_only_samples_that_were_released() {
        // 実行: cargo test -- --ignored pool_reuses_only_samples_that_were_released
        let _com = ComApartment::enter(ComModel::MultiThreaded).expect("COM を初期化できる");
        let _mf = MfPlatform::start().expect("MF を起こせる");
        let mut pool = SamplePool::new(2);

        let first = pool.sample(&[1, 2, 3, 4], 10, 20).expect("作れる");
        let first_raw = first.as_raw();
        // 渡した側（ここでは `first`）が持っている間は使い回さない
        let second = pool.sample(&[5, 6, 7, 8], 30, 20).expect("作れる");
        assert_ne!(second.as_raw(), first_raw);
        drop(first);
        drop(second);

        // バッファだけを持たれていても使い回さない
        let held = unsafe { pool.samples[0].GetBufferByIndex(0) }.expect("取れる");
        let held_too = unsafe { pool.samples[1].GetBufferByIndex(0) }.expect("取れる");
        let third = pool.sample(&[9, 9, 9, 9], 50, 20).expect("作れる");
        assert!(pool.samples.iter().all(|s| s.as_raw() != third.as_raw()));
        drop((held, held_too, third));

        // 手放されたら使い回し、中身・時刻・属性は新しいものになる
        let reused = pool.sample(&[7, 7, 7], 70, 40).expect("作れる");
        assert_eq!(reused.as_raw(), first_raw);
        assert_eq!(unsafe { reused.GetSampleTime() }.ok(), Some(70));
        assert_eq!(unsafe { reused.GetSampleDuration() }.ok(), Some(40));
        let buffer = unsafe { reused.GetBufferByIndex(0) }.expect("取れる");
        assert_eq!(unsafe { buffer.GetCurrentLength() }.ok(), Some(3));

        // 大きさが足りなくなったものは捨てて作り直す
        drop((reused, buffer));
        let larger = pool.sample(&[0; 16], 90, 20).expect("作れる");
        assert_eq!(pool.samples.len(), 1);
        assert_eq!(pool.samples[0].as_raw(), larger.as_raw());
        let buffer = unsafe { larger.GetBufferByIndex(0) }.expect("取れる");
        assert_eq!(unsafe { buffer.GetCurrentLength() }.ok(), Some(16));
    }

    /// Y が `value`、色差が 128 のベタ塗りの NV12。
    fn flat_nv12(width: usize, height: usize, value: u8) -> Vec<u8> {
        let mut nv12 = vec![value; width * height];
        nv12.resize(width * height * 3 / 2, 128);
        nv12
    }

    /// 書いた MP4 を NV12 へデコードして読み戻し、1 枚ごとの Y の平均を返す。
    fn decoded_luma_means(path: &std::path::Path, width: usize, height: usize) -> Vec<f64> {
        let reader = unsafe { MFCreateSourceReaderFromURL(&HSTRING::from(path), None) }
            .expect("書いた MP4 を開ける");
        let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        let nv12 = unsafe { MFCreateMediaType() }.expect("作れる");
        unsafe {
            nv12.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .expect("設定できる");
            nv12.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
                .expect("設定できる");
            reader
                .SetCurrentMediaType(stream, None, &nv12)
                .expect("NV12 へデコードできる");
        }
        let mut means = Vec::new();
        loop {
            let (mut flags, mut sample) = (0u32, None);
            unsafe {
                reader.ReadSample(stream, 0, None, Some(&mut flags), None, Some(&mut sample))
            }
            .expect("読める");
            if let Some(sample) = sample {
                let buffer = unsafe { sample.ConvertToContiguousBuffer() }.expect("まとめられる");
                let mut data: *mut u8 = ptr::null_mut();
                let mut length = 0u32;
                unsafe { buffer.Lock(&mut data, None, Some(&mut length)) }.expect("読める");
                let luma = width * height;
                assert!(length as usize >= luma);
                // SAFETY: Lock が長さ `length`（>= luma）の読める領域を返している
                let bytes = unsafe { std::slice::from_raw_parts(data, luma) };
                let sum: u64 = bytes.iter().map(|&b| u64::from(b)).sum();
                unsafe { buffer.Unlock() }.expect("戻せる");
                means.push(sum as f64 / luma as f64);
            }
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                return means;
            }
        }
    }

    #[test]
    #[ignore = "Media Foundation の H.264 エンコーダとデコーダが必要（CI のランナーにあるかは未確認）"]
    fn pooled_samples_do_not_overwrite_frames_still_queued_in_the_sink_writer() {
        // 実行: cargo test -- --ignored pooled_samples_do_not_overwrite_frames_still_queued
        // 待たずに続けて書き、Sink Writer の中にサンプルが溜まる状態を作る。
        // 向こうが持っているうちに使い回して上書きしていれば、そのフレームは後の
        // フレームの明るさで読み戻される
        let _com = ComApartment::enter(ComModel::MultiThreaded).expect("COM を初期化できる");
        let _mf = MfPlatform::start().expect("MF を起こせる");
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("pool.mp4");
        let (width, height) = (320usize, 240usize);
        let params = WriterParams {
            width: width as u32,
            height: height as u32,
            fps: 30,
            bitrate_kbps: 4000,
            hardware: false,
            audio_bitrate_kbps: None,
        };
        let mut writer = SinkWriter::create(&path, params).expect("Sink Writer を作れる");
        let value = |index: usize| (30 + (index % 10) * 20) as u8;
        let frames = 90;
        for index in 0..frames {
            let nv12 = flat_nv12(width, height, value(index));
            writer
                .write_nv12(&nv12, index as i64 * 333_333, 333_333)
                .expect("書ける");
        }
        writer.finalize().expect("閉じられる");

        let means = decoded_luma_means(&path, width, height);
        assert_eq!(means.len(), frames);
        for (index, mean) in means.iter().enumerate() {
            let expected = f64::from(value(index));
            assert!(
                (mean - expected).abs() < 4.0,
                "{index} 枚目の明るさが {mean:.1}（書いたのは {expected}）"
            );
        }
    }
}
