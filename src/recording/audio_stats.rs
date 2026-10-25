//! 録画の音声トラックの観測値 `AudioStats` と、録画の要約へ入れるドリフトの補正
//! `AudioDriftCorrection`（#288）。停止時のドリフトのログ（`AudioStats::log`）もここ。
//!
//! 値を数えるのは `audio.rs` の `AudioTrack`。ここは集計と書式だけを持つ。

use std::time::Duration;

use log::info;

use super::pts::{audio_units, units_from, UNITS_PER_SECOND};

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
    ///   （ドリフト）。2 点の時刻から出すので 10ms ほどの粒度があり、入力デバイスが落とした
    ///   サンプルも差に入る（GC551 では時計のずれより落としたサンプルのほうが大きかった）
    /// - **ドリフトの補正**（#288）: 掛けた補正の平均（ppm）と足した長さ、補正しても残ったずれ
    ///   （直近 5 秒の平均）。残ったずれが、録画の中で音が映像からずれた量になる
    ///
    /// `docs/design/recording.md` の「ドリフト」。
    pub(super) fn log(&self, video: Duration) {
        let secs = |units: i64| units as f64 / UNITS_PER_SECOND as f64;
        let millis = |units: i64| units as f64 * 1000.0 / UNITS_PER_SECOND as f64;
        let drift = match self.drift {
            Some((by_clock, by_samples)) if by_clock > 0 => format!(
                "入力デバイスの時計: 途切れずに続いた最後の区間で、映像の時計（PC）の {:.3} 秒に対して音声のサンプル数 ÷ レートは {:.3} 秒（差 {:+.1}ms、{:+.0}ppm。入力デバイスが落としたサンプルも差に入る。負なら補正しないと録画の音声が映像より先行していく）",
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
