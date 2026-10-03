//! リプレイバッファ（③）の設定 `ReplayConfig` と、エンコーダの作り直しが要るかの判定。
//!
//! UI スレッドが組み立てて録画スレッドへ渡す。使うのは `ReplayPipeline`（`replay.rs`）と
//! 録画スレッドの経路の切り替え（`recorder_loop.rs`）。

use super::session::FALLBACK_FPS;

/// リプレイバッファの設定。UI スレッドが `[recording]` と映像の公称 fps から組み立てる。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayConfig {
    /// さかのぼる長さ（秒）。5〜300 に丸めてある
    pub seconds: u32,
    pub video_bitrate_kbps: u32,
    pub hardware_encoder: bool,
    /// 音声（AAC）のビットレート。`None` なら音声を持たない
    pub audio_bitrate_kbps: Option<u32>,
    /// 公称 fps。映像が無ければ `None`（始めるときは 60 として扱い、動いている間に届いたら
    /// 「変化なし」として扱う。`same_encoders`）
    pub nominal_fps: Option<u32>,
    /// 映像と音声のずれの補正（ms、正なら音声を遅らせる、#404）。±200 に丸めてある。
    /// 音声トラックは作るときに決めるので、変わったら作り直す（`same_encoders`）
    pub audio_offset_ms: i32,
}

impl ReplayConfig {
    pub(super) fn fps(&self) -> u32 {
        self.nominal_fps.unwrap_or(FALLBACK_FPS).max(1)
    }

    /// エンコーダ（と音声トラック）の作り直しが要らない違いか（さかのぼる長さだけが違う）。
    /// 映像と音声のずれの補正（#404）が変わったときも作り直す。リングに溜めた分は前の補正で
    /// 書いてあり、そのあとに新しい補正の音声を繋ぐと、録画の中で音声の位置が飛ぶため。
    /// `self` がいま動いているもの、`other` が新しく届いたもの。
    ///
    /// **新しい方の公称 fps が `None`（映像が途絶えて閉じた）なら fps は変わっていないとみなす**
    /// （#306）。途絶のたびに 60 扱いで作り直すと、切断の直前という残したい分がリングから消える。
    /// 映像が戻って本当に fps が変わったときだけ作り直す。
    pub(super) fn same_encoders(&self, other: &ReplayConfig) -> bool {
        self.video_bitrate_kbps == other.video_bitrate_kbps
            && self.hardware_encoder == other.hardware_encoder
            && self.audio_bitrate_kbps == other.audio_bitrate_kbps
            && self.audio_offset_ms == other.audio_offset_ms
            && (other.nominal_fps.is_none() || self.fps() == other.fps())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ReplayConfig {
        ReplayConfig {
            seconds: 30,
            video_bitrate_kbps: 8000,
            hardware_encoder: true,
            audio_bitrate_kbps: Some(160),
            nominal_fps: Some(60),
            audio_offset_ms: 0,
        }
    }

    #[test]
    fn replay_config_same_encoders_ignores_only_the_seconds() {
        let base = config();
        assert!(base.same_encoders(&ReplayConfig {
            seconds: 300,
            ..config()
        }));
        // 映像が無い（None）が届いても fps は変わっていないとみなす（#306）
        assert!(base.same_encoders(&ReplayConfig {
            nominal_fps: None,
            ..config()
        }));
        for changed in [
            ReplayConfig {
                video_bitrate_kbps: 12_000,
                ..config()
            },
            ReplayConfig {
                hardware_encoder: false,
                ..config()
            },
            ReplayConfig {
                audio_bitrate_kbps: None,
                ..config()
            },
            ReplayConfig {
                nominal_fps: Some(30),
                ..config()
            },
            ReplayConfig {
                audio_offset_ms: 100,
                ..config()
            },
        ] {
            assert!(!base.same_encoders(&changed), "{changed:?}");
        }
    }

    // #306: 30fps で開いているときに映像が途絶えても（公称 fps が None になっても）
    // 作り直さない。映像が戻って本当に fps が変わったときだけ作り直す
    #[test]
    fn replay_config_treats_missing_fps_as_unchanged() {
        let at_30 = ReplayConfig {
            nominal_fps: Some(30),
            ..config()
        };
        let lost = ReplayConfig {
            nominal_fps: None,
            ..config()
        };
        assert!(at_30.same_encoders(&lost), "途絶で作り直さない");
        assert!(
            at_30.same_encoders(&at_30),
            "同じ fps で戻ったら作り直さない"
        );
        assert!(
            !at_30.same_encoders(&config()),
            "30 → 60 に変わったら作り直す"
        );
        // 映像が無いまま始めた（60 として作った）ものは、映像が来て fps が分かったら作り直す
        assert!(!lost.same_encoders(&at_30));
        assert!(lost.same_encoders(&lost));
        // fps 以外が変わっていれば、fps が None でも作り直す
        assert!(!at_30.same_encoders(&ReplayConfig {
            video_bitrate_kbps: 12_000,
            ..lost.clone()
        }));
    }
}
