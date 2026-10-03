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
//! - **大きさにも上限がある**（`ring_byte_limit`、#313）。キーフレームの間隔の指定を無視する
//!   エンコーダでは最後の GOP が伸び続けるので、上限を超えたら古い GOP から捨て、キーフレームが
//!   1 つしか無ければ映像を空にして数える。キーフレームそのものは `should_force_keyframe` で
//!   エンコーダに強制する
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
    /// 上限の大きさを超えたのにキーフレームが 1 つしか無く、映像を空にした回数
    overflows: u64,
}

/// 上限の大きさ（`ring_byte_limit`）を超えたときに映像をどう切るか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ByteTrim {
    /// 上限に収まっている
    Keep,
    /// 先頭から GOP をこの数だけ捨てれば収まる
    DropGops(usize),
    /// 最後の GOP だけで上限を超えている。映像を空にし、次のキーフレームから積み直す
    Clear,
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

    /// 持っているデータが `limit`（バイト）を超えていたら、古い GOP から捨てる。最後の GOP だけで
    /// 超えていれば（エンコーダがキーフレームを出さない）映像を空にして数える。音声は `trim` が
    /// 時刻で切っているので、ここでは切らない（#313）。
    pub(super) fn cap_bytes(&mut self, limit: usize) -> ByteTrim {
        if self.bytes <= limit {
            return ByteTrim::Keep;
        }
        let mut gops: Vec<(i64, usize)> = Vec::new();
        for sample in &self.video {
            match gops.last_mut() {
                Some((_, bytes)) if !sample.keyframe => *bytes += sample.data.len(),
                _ => gops.push((sample.pts, sample.data.len())),
            }
        }
        let video_bytes: usize = gops.iter().map(|&(_, bytes)| bytes).sum();
        let sizes: Vec<usize> = gops.iter().map(|&(_, bytes)| bytes).collect();
        let decision = trim_for_bytes(&sizes, self.bytes - video_bytes, limit);
        match decision {
            ByteTrim::Keep => {}
            ByteTrim::DropGops(drop) => {
                let first_kept = gops[drop].0;
                while self
                    .video
                    .front()
                    .is_some_and(|sample| sample.pts < first_kept)
                {
                    self.pop_video();
                }
                self.discarded_gops += drop as u64;
            }
            ByteTrim::Clear => {
                self.video.clear();
                self.bytes -= video_bytes;
                self.overflows += 1;
            }
        }
        decision
    }

    /// 上限の大きさを超えて映像を空にした回数。
    pub(super) fn overflows(&self) -> u64 {
        self.overflows
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

/// リングに持つデータの上限（バイト）。「設定の秒数 + 1 GOP」を映像と音声のビットレートで
/// 流した量の 2 倍。エンコーダがビットレートを多少超えても、ここで捨て始めることは無い見込み。
///
/// **保険。** 普段は `gops_to_drop` が時間で切るので、ここに届かない。キーフレームの間隔の指定を
/// 無視し、`CODECAPI_AVEncVideoForceKeyFrame` も効かないエンコーダだと、最後の GOP を
/// 捨てられずにリングが伸び続けるので、大きさでも切る（#313）。
pub(super) fn ring_byte_limit(
    seconds: u32,
    video_kbps: u32,
    audio_kbps: Option<u32>,
    gop_units: i64,
) -> usize {
    let held_units = i64::from(seconds) * UNITS_PER_SECOND + gop_units.max(0);
    let kbps = u64::from(video_kbps) + u64::from(audio_kbps.unwrap_or(0));
    // kbps × 1000 / 8 = バイト/秒。100ns 単位の長さを掛けてから割る
    let bytes = u128::from(kbps) * 1000 / 8 * held_units as u128 / UNITS_PER_SECOND as u128 * 2;
    usize::try_from(bytes).unwrap_or(usize::MAX)
}

/// 上限の大きさを超えたときに、先頭から捨てる GOP の数を決める。`gop_bytes` は時刻の順の
/// GOP ごとの大きさ、`other_bytes` は映像以外（音声）の大きさ。
///
/// 最後の GOP は捨てない（`gops_to_drop` と同じ理由）。最後の GOP だけでも超えるなら `Clear`。
/// 映像が無いなら `Keep`（音声は時刻で切っている）。
pub(super) fn trim_for_bytes(gop_bytes: &[usize], other_bytes: usize, limit: usize) -> ByteTrim {
    let mut total = gop_bytes.iter().sum::<usize>() + other_bytes;
    if total <= limit || gop_bytes.is_empty() {
        return ByteTrim::Keep;
    }
    let mut drop = 0;
    while total > limit && drop + 1 < gop_bytes.len() {
        total -= gop_bytes[drop];
        drop += 1;
    }
    if total > limit {
        ByteTrim::Clear
    } else {
        ByteTrim::DropGops(drop)
    }
}

/// エンコーダにキーフレームを強制するか。`pts` はいまエンコーダへ渡すフレームの時刻、
/// `last_keyframe` はエンコーダが最後に出したキーフレームの時刻、`last_forced` は最後に
/// 強制した時刻。
///
/// 最後のキーフレームから GOP の 2 倍が過ぎたら強制する。キーフレームの間隔の指定を無視する
/// エンコーダだと、最後の GOP を捨てられずにリングが伸び続け、録画の先頭にする「いま − N 秒」
/// 以降のキーフレームも来ない（#313）。強制したあとは、キーフレームが出てくるまで 1 GOP の間は
/// 送り直さない（エンコーダは数枚遅れて出力するため）。まだ 1 枚も出ていなければ強制しない
/// （最初の出力はキーフレーム）。
pub(super) fn should_force_keyframe(
    pts: i64,
    last_keyframe: Option<i64>,
    last_forced: Option<i64>,
    gop_units: i64,
) -> bool {
    let Some(last) = last_keyframe else {
        return false;
    };
    if pts.saturating_sub(last) < gop_units.saturating_mul(2) {
        return false;
    }
    match last_forced {
        Some(forced) if forced >= last => pts.saturating_sub(forced) >= gop_units,
        _ => true,
    }
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

    // #313 の再現: キーフレームが最初の 1 枚しか無いと、時間では 1 バイトも捨てられない
    #[test]
    fn encoded_ring_with_a_single_keyframe_is_not_trimmed_by_time() {
        let mut ring = EncodedRing::default();
        ring.push_video(sample(0, true, 10));
        for index in 1..600 {
            ring.push_video(sample(index * SECOND / 2, false, 10));
        }
        ring.trim(keep_from(300 * SECOND, 30, 2 * SECOND));
        assert_eq!(ring.bytes(), 600 * 10, "最後の GOP は時間では捨てない");
        // 録画の先頭にする「いま − 30 秒」以降のキーフレームも無い
        assert_eq!(
            ring.start_point(replay_cut(300 * SECOND, 30)),
            None,
            "録画が始まらない"
        );
    }

    #[test]
    fn ring_byte_limit_is_twice_the_bitrate_over_the_held_length() {
        // 30 秒 + 2 秒 × (8000 + 160)kbps = 32 × 1,020,000 バイト/秒、その 2 倍
        assert_eq!(
            ring_byte_limit(30, 8000, Some(160), 2 * SECOND),
            32 * 1_020_000 * 2
        );
        assert_eq!(
            ring_byte_limit(30, 8000, None, 2 * SECOND),
            32 * 1_000_000 * 2
        );
        // 録画中に OFF にされた（0 秒）ときも 1 GOP ぶんは持てる
        assert_eq!(
            ring_byte_limit(0, 8000, None, 2 * SECOND),
            2 * 1_000_000 * 2
        );
        // 300 秒 × 大きなビットレートでも溢れない
        assert!(ring_byte_limit(300, u32::MAX, Some(u32::MAX), 2 * SECOND) > 0);
    }

    #[test]
    fn trim_for_bytes_drops_old_gops_until_it_fits() {
        assert_eq!(trim_for_bytes(&[10, 10, 10], 5, 35), ByteTrim::Keep);
        assert_eq!(trim_for_bytes(&[10, 10, 10], 5, 34), ByteTrim::DropGops(1));
        assert_eq!(trim_for_bytes(&[10, 10, 10], 5, 15), ByteTrim::DropGops(2));
        // 映像が無ければ音声だけで超えていても切らない（音声は時刻で切る）
        assert_eq!(trim_for_bytes(&[], 100, 10), ByteTrim::Keep);
    }

    #[test]
    fn trim_for_bytes_clears_when_the_last_gop_alone_is_too_big() {
        assert_eq!(trim_for_bytes(&[100], 0, 50), ByteTrim::Clear);
        assert_eq!(trim_for_bytes(&[10, 100], 0, 50), ByteTrim::Clear);
        // 音声と合わせて超えるときも、最後の GOP は途中で切れないので空にする
        assert_eq!(trim_for_bytes(&[10, 45], 10, 50), ByteTrim::Clear);
    }

    #[test]
    fn encoded_ring_cap_bytes_clears_a_single_endless_gop_and_counts_it() {
        let mut ring = EncodedRing::default();
        ring.push_video(sample(0, true, 10));
        for index in 1..100 {
            ring.push_video(sample(index * SECOND / 2, false, 10));
        }
        ring.push_audio(sample(49 * SECOND, true, 3));
        assert_eq!(ring.cap_bytes(2_000), ByteTrim::Keep);
        assert_eq!(ring.cap_bytes(500), ByteTrim::Clear);
        assert_eq!(ring.overflows(), 1);
        // 音声は残し、映像は次のキーフレームから積み直す
        assert_eq!(ring.bytes(), 3);
        ring.push_video(sample(50 * SECOND, false, 10));
        assert_eq!(ring.bytes(), 3);
        ring.push_video(sample(51 * SECOND, true, 10));
        assert_eq!(ring.start_point(i64::MIN), Some(51 * SECOND));
    }

    #[test]
    fn encoded_ring_cap_bytes_drops_whole_gops() {
        // 0〜10 秒、キーフレームは 2 秒ごと、1 GOP = 4 枚 × 10 バイト
        let mut ring = ring_with_video(10);
        assert_eq!(ring.cap_bytes(100), ByteTrim::DropGops(3));
        assert_eq!(ring.start_point(i64::MIN), Some(6 * SECOND));
        assert_eq!(ring.bytes(), 80);
        assert_eq!(ring.discarded_gops(), 3);
        assert_eq!(ring.overflows(), 0);
    }

    #[test]
    fn should_force_keyframe_after_two_gops_without_one() {
        let gop = 2 * SECOND;
        // まだ何も出ていない
        assert!(!should_force_keyframe(10 * SECOND, None, None, gop));
        // 最後のキーフレームから 2 GOP 未満
        assert!(!should_force_keyframe(4 * SECOND - 1, Some(0), None, gop));
        assert!(should_force_keyframe(4 * SECOND, Some(0), None, gop));
        // 強制したあとは 1 GOP の間は送り直さない
        assert!(!should_force_keyframe(
            5 * SECOND,
            Some(0),
            Some(4 * SECOND),
            gop
        ));
        assert!(should_force_keyframe(
            6 * SECOND,
            Some(0),
            Some(4 * SECOND),
            gop
        ));
        // 強制したあとにキーフレームが出てきたら、そこから数え直す
        assert!(!should_force_keyframe(
            7 * SECOND,
            Some(4 * SECOND),
            Some(4 * SECOND - 1),
            gop
        ));
        assert!(should_force_keyframe(
            8 * SECOND,
            Some(4 * SECOND),
            Some(4 * SECOND - 1),
            gop
        ));
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
