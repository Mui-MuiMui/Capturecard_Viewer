//! エンコード済みのリング（③ リプレイバッファ）。**録画スレッドの中だけにある。**
//!
//! リプレイバッファが ON のあいだ、エンコーダ MFT が出した H.264 と AAC のサンプルを
//! ここに持つ。録画を始めたら「いま − N 秒」以降の最初のキーフレームから書き出す
//! （`docs/design/recording.md` の「リプレイバッファへの伸ばし方（#182）」）。
//!
//! - **映像は常にキーフレームから始める。** 古いものはキーフレーム境界（GOP 単位）で捨てる。
//!   キーフレームの間隔（2 秒）が、さかのぼれる長さの粒度になる
//! - **持つのは「設定の秒数 + 1 GOP」まで。** それより古い GOP は、次のキーフレームが
//!   境界より新しくなるまで残る。最後の GOP は捨てない（いま積んでいる GOP を途中で
//!   捨てると、続きのフレームを復号できなくなる）。映像が途絶えて最後の GOP が
//!   「いま − N 秒」より古くなったら、録画は次のキーフレームから始める（古い GOP から
//!   書き出すと、途絶えていた長い空白までファイルに入るため）
//! - 音声（AAC）はどのフレームからでも復号できるので、映像とは関係なく境界の時刻で切る
//!   （映像が途絶えている間も音声が溜まり続けないように）
//!
//! 判定と計算（どのキーフレームから書くか、捨てる境界、PTS の付け替え）は純粋関数にしてある。

use std::collections::VecDeque;

use super::encoder::EncodedSample;
use super::pts::UNITS_PER_SECOND;

/// エンコード済みのサンプルのリング。
#[derive(Debug, Default)]
pub(super) struct EncodedRing {
    video: VecDeque<EncodedSample>,
    audio: VecDeque<EncodedSample>,
    /// 持っているデータの大きさ（バイト）
    bytes: usize,
    /// 古い GOP を捨てた回数
    discarded_gops: u64,
}

/// 書き出すときの 1 件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Track {
    Video,
    Audio,
}

/// トラックごとに、最後に書いたサンプルの時刻（付け替える前）。まだ書いていなければ `None`。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Written {
    pub(super) video: Option<i64>,
    pub(super) audio: Option<i64>,
}

impl Written {
    pub(super) fn note(&mut self, track: Track, pts: i64) {
        match track {
            Track::Video => self.video = Some(pts),
            Track::Audio => self.audio = Some(pts),
        }
    }
}

impl EncodedRing {
    /// 映像のサンプルを積む。空のリングはキーフレームからしか始めない（それより前の
    /// 差分のフレームは単独では復号できない）。
    pub(super) fn push_video(&mut self, sample: EncodedSample) {
        if self.video.is_empty() && !sample.keyframe {
            return;
        }
        self.bytes += sample.data.len();
        self.video.push_back(sample);
    }

    pub(super) fn push_audio(&mut self, sample: EncodedSample) {
        self.bytes += sample.data.len();
        self.audio.push_back(sample);
    }

    /// `keep_from`（リプレイバッファの基準からの 100ns）より古いものを捨てる。
    /// 映像は GOP 単位、音声は `keep_from` で切る。
    ///
    /// 音声を映像の先頭に合わせて残さないのは、映像が途絶えると最後の GOP が古いまま残り、
    /// 音声の境界が進まずに溜まり続けるため。書き出すのは「いま − N 秒」（`keep_from` より新しい）
    /// 以降のキーフレームからなので、それより古い音声は使わない。
    pub(super) fn trim(&mut self, keep_from: i64) {
        let keyframes: Vec<i64> = self.keyframes().collect();
        let drop = gops_to_drop(&keyframes, keep_from);
        if drop > 0 {
            let first_kept = keyframes[drop];
            while self
                .video
                .front()
                .is_some_and(|sample| sample.pts < first_kept)
            {
                self.pop_video();
            }
            self.discarded_gops += drop as u64;
        }
        while self
            .audio
            .front()
            .is_some_and(|sample| sample.pts < keep_from)
        {
            if let Some(sample) = self.audio.pop_front() {
                self.bytes -= sample.data.len();
            }
        }
    }

    /// 全部捨てる（映像の大きさが変わってエンコーダを作り直すとき）。
    pub(super) fn clear(&mut self) {
        self.video.clear();
        self.audio.clear();
        self.bytes = 0;
    }

    /// 書き出しを始めるキーフレームの時刻。`cut` 以降の最初のキーフレーム。無ければ `None`。
    pub(super) fn start_point(&self, cut: i64) -> Option<i64> {
        replay_start(self.keyframes(), cut)
    }

    /// `offset` 以降のサンプルを、時刻の順に 2 つのトラックを混ぜて返す。映像は `offset` の
    /// キーフレームから、音声は `offset` 以降。Sink Writer が 2 つのトラックを揃えて
    /// まとめられるよう、片方だけを先に大量に渡さない。
    ///
    /// 録画では `samples_after` で少しずつ取り出す。これは全部を 1 度に取り出す形で、テストが使う。
    #[cfg(test)]
    pub(super) fn samples_from(&self, offset: i64) -> Vec<(Track, &EncodedSample)> {
        self.samples_after(offset, Written::default(), usize::MAX)
    }

    /// `samples_from` の続きを少しずつ取り出す。`written` より後（トラックごと）のサンプルを
    /// 最大 `limit` 個返す。返った数が `limit` に満たなければ、リングの最後まで取り出し終えた。
    ///
    /// 5 分ぶんを 1 度に書くと録画スレッドがその間止まり、ライブのフレームを取りこぼすので、
    /// 書き出しは数 ms ごとに少しずつ進める（`super::replay_recording`）。
    pub(super) fn samples_after(
        &self,
        offset: i64,
        written: Written,
        limit: usize,
    ) -> Vec<(Track, &EncodedSample)> {
        let after = |last: Option<i64>| {
            move |sample: &&EncodedSample| {
                sample.pts < offset || last.is_some_and(|last| sample.pts <= last)
            }
        };
        let mut video = self
            .video
            .iter()
            .skip_while(after(written.video))
            .peekable();
        let mut audio = self
            .audio
            .iter()
            .skip_while(after(written.audio))
            .peekable();
        let mut merged = Vec::new();
        while merged.len() < limit {
            let next = match (video.peek(), audio.peek()) {
                (Some(v), Some(a)) if a.pts < v.pts => Track::Audio,
                (Some(_), _) => Track::Video,
                (None, Some(_)) => Track::Audio,
                (None, None) => break,
            };
            match next {
                Track::Video => merged.extend(video.next().map(|sample| (Track::Video, sample))),
                Track::Audio => merged.extend(audio.next().map(|sample| (Track::Audio, sample))),
            }
        }
        merged
    }

    /// 持っている映像の長さ（先頭のキーフレームから最後のサンプルの終わりまで、100ns）。
    pub(super) fn held_units(&self) -> i64 {
        match (self.video.front(), self.video.back()) {
            (Some(first), Some(last)) => (last.pts + last.duration - first.pts).max(0),
            _ => 0,
        }
    }

    /// 持っているデータの大きさ（バイト）。
    pub(super) fn bytes(&self) -> usize {
        self.bytes
    }

    /// 古い GOP を捨てた回数。
    pub(super) fn discarded_gops(&self) -> u64 {
        self.discarded_gops
    }

    fn keyframes(&self) -> impl Iterator<Item = i64> + '_ {
        self.video
            .iter()
            .filter(|sample| sample.keyframe)
            .map(|sample| sample.pts)
    }

    fn pop_video(&mut self) {
        if let Some(sample) = self.video.pop_front() {
            self.bytes -= sample.data.len();
        }
    }
}

/// 持っておく境界（リプレイバッファの基準からの 100ns）。`now` から「設定の秒数 + 1 GOP」前。
pub(super) fn keep_from(now: i64, seconds: u32, gop_units: i64) -> i64 {
    now.saturating_sub(i64::from(seconds) * UNITS_PER_SECOND)
        .saturating_sub(gop_units)
}

/// 先頭から捨てる GOP の数。`keyframes` は時刻の順のキーフレームの時刻。
///
/// `keep_from` より前に始まる GOP を捨てるが、**最後の GOP は残す。** 捨てたあとの先頭は
/// `keep_from` 以降の最初のキーフレームになる（残す長さは「設定の秒数 + 1 GOP」以内）。
pub(super) fn gops_to_drop(keyframes: &[i64], keep_from: i64) -> usize {
    let older = keyframes.iter().take_while(|&&pts| pts < keep_from).count();
    older.min(keyframes.len().saturating_sub(1))
}

/// 録画の先頭にするキーフレーム。`cut`（いま − N 秒）以降の最初のもの。無ければ `None`
/// （次のキーフレームがエンコーダから出てくるのを待つ）。
pub(super) fn replay_start(keyframes: impl IntoIterator<Item = i64>, cut: i64) -> Option<i64> {
    keyframes.into_iter().find(|&pts| pts >= cut)
}

/// さかのぼる境界。録画を始めた時刻から `seconds` 秒前。
pub(super) fn replay_cut(requested_at: i64, seconds: u32) -> i64 {
    requested_at.saturating_sub(i64::from(seconds) * UNITS_PER_SECOND)
}

/// 1 回の録画で書き出す範囲と、PTS の付け替え。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Cut {
    /// 先頭のキーフレームの時刻。ここを 0 にする
    pub(super) offset: i64,
    /// 止めた時刻。これ以降のサンプルは書かない。止めるまでは `None`
    pub(super) stop_at: Option<i64>,
}

/// サンプルが書き出す範囲のどこにあるか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Placement {
    /// 先頭より前。書かない
    Before,
    /// 範囲の中。付け替えた時刻（`pts − offset`）で書く
    Inside(i64),
    /// 止めた時刻以降。書かない
    After,
}

impl Cut {
    pub(super) fn new(offset: i64) -> Self {
        Self {
            offset,
            stop_at: None,
        }
    }

    pub(super) fn place(&self, pts: i64) -> Placement {
        if pts < self.offset {
            Placement::Before
        } else if self.stop_at.is_some_and(|stop| pts >= stop) {
            Placement::After
        } else {
            Placement::Inside(pts - self.offset)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    const SECOND: i64 = UNITS_PER_SECOND;

    fn sample(pts: i64, keyframe: bool, bytes: usize) -> EncodedSample {
        EncodedSample {
            pts,
            duration: SECOND / 2,
            keyframe,
            data: Arc::from(vec![0u8; bytes]),
        }
    }

    /// 0.5 秒ごとの映像を `seconds` 秒ぶん。キーフレームは 2 秒ごと。
    fn ring_with_video(seconds: i64) -> EncodedRing {
        let mut ring = EncodedRing::default();
        for index in 0..seconds * 2 {
            ring.push_video(sample(index * SECOND / 2, index % 4 == 0, 10));
        }
        ring
    }

    #[test]
    fn keep_from_subtracts_the_seconds_and_one_gop() {
        assert_eq!(keep_from(100 * SECOND, 30, 2 * SECOND), 68 * SECOND);
        // 始めてすぐは負になる（全部持つ）
        assert_eq!(keep_from(10 * SECOND, 30, 2 * SECOND), -22 * SECOND);
    }

    #[test]
    fn gops_to_drop_keeps_from_the_first_keyframe_after_the_boundary() {
        let keyframes = [0, 2 * SECOND, 4 * SECOND, 6 * SECOND];
        assert_eq!(gops_to_drop(&keyframes, 3 * SECOND), 2);
        // 境界ちょうどのキーフレームは残す
        assert_eq!(gops_to_drop(&keyframes, 4 * SECOND), 2);
        assert_eq!(gops_to_drop(&keyframes, 0), 0);
        assert_eq!(gops_to_drop(&keyframes, -SECOND), 0);
    }

    #[test]
    fn gops_to_drop_never_drops_the_last_gop() {
        // 映像が途絶えて、どのキーフレームも境界より古い
        assert_eq!(gops_to_drop(&[0, 2 * SECOND], 100 * SECOND), 1);
        assert_eq!(gops_to_drop(&[0], 100 * SECOND), 0);
        assert_eq!(gops_to_drop(&[], 100 * SECOND), 0);
    }

    #[test]
    fn replay_start_picks_the_first_keyframe_at_or_after_the_cut() {
        let keyframes = [0, 2 * SECOND, 4 * SECOND];
        assert_eq!(replay_start(keyframes, SECOND), Some(2 * SECOND));
        assert_eq!(replay_start(keyframes, 2 * SECOND), Some(2 * SECOND));
        assert_eq!(replay_start(keyframes, -5 * SECOND), Some(0));
        // 境界より後にキーフレームが無ければ、次のキーフレームを待つ
        assert_eq!(replay_start(keyframes, 5 * SECOND), None);
        assert_eq!(replay_start([], 0), None);
    }

    #[test]
    fn replay_cut_goes_back_by_the_seconds() {
        assert_eq!(replay_cut(40 * SECOND, 30), 10 * SECOND);
        assert_eq!(replay_cut(10 * SECOND, 30), -20 * SECOND);
    }

    #[test]
    fn cut_rebases_inside_and_rejects_outside() {
        let mut cut = Cut::new(10 * SECOND);
        assert_eq!(cut.place(9 * SECOND), Placement::Before);
        assert_eq!(cut.place(10 * SECOND), Placement::Inside(0));
        assert_eq!(cut.place(12 * SECOND), Placement::Inside(2 * SECOND));
        cut.stop_at = Some(15 * SECOND);
        assert_eq!(
            cut.place(15 * SECOND - 1),
            Placement::Inside(5 * SECOND - 1)
        );
        assert_eq!(cut.place(15 * SECOND), Placement::After);
    }

    #[test]
    fn encoded_ring_starts_only_at_a_keyframe() {
        let mut ring = EncodedRing::default();
        ring.push_video(sample(0, false, 10));
        assert_eq!(ring.held_units(), 0);
        assert_eq!(ring.bytes(), 0);
        ring.push_video(sample(SECOND, true, 10));
        ring.push_video(sample(SECOND * 3 / 2, false, 10));
        assert_eq!(ring.start_point(0), Some(SECOND));
        assert_eq!(ring.bytes(), 20);
    }

    #[test]
    fn encoded_ring_trim_drops_whole_gops_and_counts_them() {
        // 0〜10 秒の映像、キーフレームは 0, 2, 4, 6, 8 秒
        let mut ring = ring_with_video(10);
        ring.trim(5 * SECOND);
        // 6 秒のキーフレームから残る
        assert_eq!(ring.start_point(i64::MIN), Some(6 * SECOND));
        assert_eq!(ring.discarded_gops(), 3);
        assert_eq!(ring.held_units(), 4 * SECOND);
        assert_eq!(ring.bytes(), 8 * 10);
    }

    #[test]
    fn encoded_ring_trim_cuts_audio_at_the_boundary() {
        let mut ring = ring_with_video(10);
        for index in 0..20 {
            ring.push_audio(sample(index * SECOND / 2, true, 1));
        }
        ring.trim(5 * SECOND);
        // 映像の先頭（6 秒）より前でも、境界（5 秒）以降の音声は残す
        assert_eq!(first_audio_pts(&ring), Some(5 * SECOND));
    }

    #[test]
    fn encoded_ring_trim_keeps_cutting_audio_while_video_is_stalled() {
        // 映像は最初の 2 秒（キーフレーム 1 つ）で途絶え、音声だけが 100 秒続いた
        let mut ring = ring_with_video(2);
        for index in 0..200 {
            ring.push_audio(sample(index * SECOND / 2, true, 1));
        }
        ring.trim(60 * SECOND);
        // 最後の GOP（0 秒）は残るが、音声はそれに引きずられずに境界で切る
        assert_eq!(ring.start_point(i64::MIN), Some(0));
        assert_eq!(first_audio_pts(&ring), Some(60 * SECOND));
        assert_eq!(ring.bytes(), 4 * 10 + 80);
    }

    fn first_audio_pts(ring: &EncodedRing) -> Option<i64> {
        ring.samples_from(i64::MIN)
            .into_iter()
            .find(|(track, _)| *track == Track::Audio)
            .map(|(_, sample)| sample.pts)
    }

    #[test]
    fn encoded_ring_samples_from_interleaves_by_time() {
        let mut ring = EncodedRing::default();
        ring.push_video(sample(0, true, 1));
        ring.push_video(sample(SECOND, false, 1));
        ring.push_video(sample(2 * SECOND, true, 1));
        ring.push_video(sample(3 * SECOND, false, 1));
        for pts in [SECOND / 2, 2 * SECOND + SECOND / 2, 4 * SECOND] {
            ring.push_audio(sample(pts, true, 1));
        }
        let order: Vec<(Track, i64)> = ring
            .samples_from(2 * SECOND)
            .into_iter()
            .map(|(track, sample)| (track, sample.pts))
            .collect();
        assert_eq!(
            order,
            vec![
                (Track::Video, 2 * SECOND),
                (Track::Audio, 2 * SECOND + SECOND / 2),
                (Track::Video, 3 * SECOND),
                (Track::Audio, 4 * SECOND),
            ]
        );
    }

    #[test]
    fn encoded_ring_samples_after_continues_where_the_last_batch_ended() {
        let mut ring = ring_with_video(4);
        for index in 0..8 {
            ring.push_audio(sample(index * SECOND / 2 + 1, true, 1));
        }
        let all: Vec<(Track, i64)> = ring
            .samples_from(0)
            .into_iter()
            .map(|(track, sample)| (track, sample.pts))
            .collect();

        // 3 個ずつ取り出して、全部を 1 度に取り出したときと同じ並びになる
        let mut written = Written::default();
        let mut batches = Vec::new();
        loop {
            let batch = ring.samples_after(0, written, 3);
            let done = batch.len() < 3;
            for (track, sample) in batch {
                written.note(track, sample.pts);
                batches.push((track, sample.pts));
            }
            if done {
                break;
            }
        }
        assert_eq!(batches, all);
        assert_eq!(all.len(), 16);
        // 書き終えたら何も返らない
        assert!(ring.samples_after(0, written, 3).is_empty());
    }

    #[test]
    fn encoded_ring_clear_empties_both_tracks() {
        let mut ring = ring_with_video(4);
        ring.push_audio(sample(0, true, 5));
        ring.clear();
        assert_eq!(ring.bytes(), 0);
        assert_eq!(ring.held_units(), 0);
        assert!(ring.samples_from(i64::MIN).is_empty());
    }
}
