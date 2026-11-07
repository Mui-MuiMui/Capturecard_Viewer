//! 映像デバイスと対になる DirectShow の音声だけのフィルター（#562）。
//!
//! 映像のフィルターに音声ピンが無く、音声を別のフィルターとして出す機種がある。
//! Elgato Game Capture HD60 Pro は「Game Capture HD60 Pro (Video)」と
//! 「Game Capture HD60 Pro (Audio)」を出し、OBS やメーカーのソフトは後者から音を取る。
//! その音声のフィルターを名前で見つけ、**映像と同じグラフに入れて**音声のレンダラーへ
//! 繋ぐ（繋ぐのは `audio_pin.rs`）。映像のフィルターの音声ピンと同じく、
//! `[audio] input_source = "video_pin"` のときだけ繋ぐ。
//!
//! **Windows の録音デバイスを包んだだけのフィルターは対にしない。**
//! `CLSID_AudioInputDeviceCategory` には Windows のすべての録音デバイスが
//! `Audio Capture Filter`（`CLSID_AudioRecord`、名札に `WaveInID` がある）に包まれて
//! 並ぶ。中身は WASAPI の入力と同じ録音デバイスなので、選んでも何も変わらない。
//! Live Gamer EXTREME 3 の「HDMI/Line In (Live Gamer EXTREME 3)」はこれに当たる
//! （2026-11-07 の実機の列挙）。
//!
//! 名前の突き合わせ（`paired_audio_filter`）と包んだだけかの判定
//! （`is_recording_device_wrapper`）は純粋関数。フェイクの映像デバイスも使う。

use windows::Win32::Media::DirectShow::{IBaseFilter, IGraphBuilder};

use super::devices::{self, bind_filter, DeviceEntry};
use super::display_name::friendly_name;

/// `Audio Capture Filter`（Windows の録音デバイスを包むフィルター）の CLSID
const CLSID_AUDIO_RECORD: &str = "{E30629D2-27E5-11CE-875D-00608CB78066}";

/// 名前を突き合わせるときに数えない語。映像と音声で入れ替わる語だけにする
const PAIRING_IGNORED_TOKENS: &[&str] = &["video", "audio", "capture"];

/// 突き合わせの候補の 1 つ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AudioFilterName<'a> {
    pub(crate) name: &'a str,
    /// Windows の録音デバイスを包んだだけのフィルターか
    pub(crate) wraps_recording_device: bool,
}

impl<'a> AudioFilterName<'a> {
    /// メーカー独自の音声のフィルター
    pub(crate) const fn vendor(name: &'a str) -> Self {
        Self {
            name,
            wraps_recording_device: false,
        }
    }

    /// Windows の録音デバイスを包んだだけのフィルター
    pub(crate) const fn recording_device(name: &'a str) -> Self {
        Self {
            name,
            wraps_recording_device: true,
        }
    }
}

/// 名札のプロパティから、Windows の録音デバイスを包んだだけのフィルターかを決める。
/// `WaveInID` があるか、`CLSID` が `Audio Capture Filter` のもの。
pub(super) fn is_recording_device_wrapper(clsid: Option<&str>, has_wave_in_id: bool) -> bool {
    has_wave_in_id
        || clsid.is_some_and(|clsid| clsid.trim().eq_ignore_ascii_case(CLSID_AUDIO_RECORD))
}

/// 名前を比べるための語の列。「(DirectShow)」の印を外し、英数字以外で区切って
/// 小文字へそろえ、映像と音声で入れ替わる語（`PAIRING_IGNORED_TOKENS`）を除く。
fn pairing_tokens(name: &str) -> Vec<String> {
    let name = friendly_name(name).unwrap_or(name);
    name.split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
        .filter(|token| !PAIRING_IGNORED_TOKENS.contains(&token.as_str()))
        .collect()
}

/// 映像デバイスと対になる音声のフィルターを選ぶ。候補の位置を返す。
///
/// 対とみなすのは、録音デバイスを包んだだけのものを除いた候補のうち、
/// `PAIRING_IGNORED_TOKENS` を除いた語の並びが映像デバイスの名前と同じもの
/// （「Game Capture HD60 Pro (Video)」と「Game Capture HD60 Pro (Audio)」）。
/// **映像の名前に数字を含む語（型番・番号）が 1 つも無ければ選ばない。**
/// 「USB Video」と「USB Audio」のような、別の機器でも同じになりやすい名前を
/// 対にしないため。複数が合えば列挙の順で先のもの。機種の名前は持たない。
pub(crate) fn paired_audio_filter(
    video_name: &str,
    candidates: &[AudioFilterName<'_>],
) -> Option<usize> {
    let key = pairing_tokens(video_name);
    if !key
        .iter()
        .any(|token| token.chars().any(|c| c.is_ascii_digit()))
    {
        return None;
    }
    candidates.iter().position(|candidate| {
        !candidate.wraps_recording_device && pairing_tokens(candidate.name) == key
    })
}

/// 列挙した音声のフィルターを、突き合わせの候補の形にする。
fn names_of(filters: &[DeviceEntry]) -> Vec<AudioFilterName<'_>> {
    filters
        .iter()
        .map(|entry| AudioFilterName {
            name: &entry.friendly_name,
            wraps_recording_device: entry.wraps_recording_device,
        })
        .collect()
}

/// 列挙した音声のフィルターから、映像デバイスと対になるものを探す。
///
/// 映像のフィルターに音声ピンが無いときに呼ぶ。候補と結果は INFO でログへ出す
/// （実機の無い機種で、包んだだけで外したのか名前が合わなかったのかを持ち主の
/// ログから読めるように）。列挙に失敗したら `None`。
pub(super) fn find_paired(video_name: &str) -> Option<DeviceEntry> {
    let filters = match devices::enumerate_audio() {
        Ok(filters) => filters,
        Err(e) => {
            log::debug!("DirectShow の音声のフィルターを列挙できない: {}", e);
            return None;
        }
    };
    let names = names_of(&filters);
    let paired = paired_audio_filter(video_name, &names);
    log::info!(
        "映像デバイス {} に音声ピンが無いので、対になる DirectShow の音声のフィルターを探した: {}（候補: {}）",
        video_name,
        paired.map_or_else(|| "無い".to_string(), |index| names[index].name.to_string()),
        if names.is_empty() {
            "無し".to_string()
        } else {
            names
                .iter()
                .map(|candidate| {
                    if candidate.wraps_recording_device {
                        format!("{}（録音デバイスの包み）", candidate.name)
                    } else {
                        candidate.name.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("、")
        }
    );
    paired.and_then(|index| filters.into_iter().nth(index))
}

/// 音声のフィルターがあるか（列挙の時点の有無、#409）。フィルターは作らない。
pub(super) fn has_paired(video_name: &str, filters: &[DeviceEntry]) -> bool {
    let names = names_of(filters);
    paired_audio_filter(video_name, &names).is_some()
}

/// 対の音声のフィルターを作ってグラフへ入れる。**ここで音声のデバイスを掴む。**
pub(super) fn add_to_graph(
    graph: &IGraphBuilder,
    entry: &DeviceEntry,
) -> windows::core::Result<IBaseFilter> {
    let filter = bind_filter(entry)?;
    unsafe { graph.AddFilter(&filter, windows::core::w!("Capture Audio Source"))? };
    Ok(filter)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vendor(name: &str) -> AudioFilterName<'_> {
        AudioFilterName::vendor(name)
    }

    fn wrapper(name: &str) -> AudioFilterName<'_> {
        AudioFilterName::recording_device(name)
    }

    #[test]
    fn paired_audio_filter_matches_the_video_and_audio_siblings() {
        let candidates = [
            wrapper("マイク (HyperX Quadcast)"),
            wrapper("デジタル オーディオ インターフェイス (Game Capture HD60 Pro)"),
            vendor("Game Capture HD60 S (Audio)"),
            vendor("Game Capture HD60 Pro (Audio)"),
        ];
        assert_eq!(
            paired_audio_filter("Game Capture HD60 Pro (Video) (DirectShow)", &candidates),
            Some(3)
        );
        assert_eq!(
            paired_audio_filter("Game Capture HD60 Pro (Video)", &candidates),
            Some(3)
        );
        // 大文字小文字と区切りは問わない。番号の付いた 2 枚目も番号で分かれる
        let numbered = [
            vendor("Game Capture HD60 Pro (Audio) (#01)"),
            vendor("game capture hd60 pro audio #02"),
        ];
        assert_eq!(
            paired_audio_filter("Game Capture HD60 Pro (Video) #02", &numbered),
            Some(1)
        );
        assert_eq!(
            paired_audio_filter(
                "AVerMedia GC551 Video Capture",
                &[vendor("AVerMedia GC551 Audio")]
            ),
            Some(0)
        );
    }

    #[test]
    fn paired_audio_filter_ignores_wrapped_recording_devices() {
        // Live Gamer EXTREME 3 は Windows の録音デバイスを包んだものしか無い（実機の列挙）
        let extreme3 = [
            wrapper("マイク (HyperX Quadcast)"),
            wrapper("HDMI/Line In (Live Gamer EXTREME 3)"),
        ];
        assert_eq!(paired_audio_filter("Live Gamer EXTREME 3", &extreme3), None);
        // 名前が合っていても、包んだだけなら対にしない
        assert_eq!(
            paired_audio_filter(
                "Game Capture HD60 Pro (Video)",
                &[wrapper("Game Capture HD60 Pro (Audio)")]
            ),
            None
        );
    }

    #[test]
    fn paired_audio_filter_needs_the_same_words_and_a_number() {
        // 余分な語がある名前は対にしない
        assert_eq!(
            paired_audio_filter(
                "Live Gamer EXTREME 3",
                &[vendor("HDMI/Line In (Live Gamer EXTREME 3)")]
            ),
            None
        );
        // 数字を含む語が無い名前では選ばない
        assert_eq!(
            paired_audio_filter("USB Video", &[vendor("USB Audio")]),
            None
        );
        assert_eq!(
            paired_audio_filter("Video Capture", &[vendor("Audio Capture")]),
            None
        );
        assert_eq!(
            paired_audio_filter("Game Capture HD60 Pro (Video)", &[]),
            None
        );
    }

    #[test]
    fn is_recording_device_wrapper_reads_wave_in_id_and_clsid() {
        // 実機の Audio Capture Filter（CLSID_AudioRecord、WaveInID あり）
        assert!(is_recording_device_wrapper(
            Some("{E30629D2-27E5-11CE-875D-00608CB78066}"),
            true
        ));
        assert!(is_recording_device_wrapper(
            Some("{e30629d2-27e5-11ce-875d-00608cb78066}"),
            false
        ));
        assert!(is_recording_device_wrapper(None, true));
        // ksproxy（KS のフィルター）や CLSID の無いものは包みではない
        assert!(!is_recording_device_wrapper(
            Some("{17CCA71B-ECD7-11D0-B908-00A0C9223196}"),
            false
        ));
        assert!(!is_recording_device_wrapper(None, false));
    }

    // 実行: cargo test enumerate_audio_marks_recording_device_wrappers -- --ignored --nocapture
    #[test]
    #[ignore = "Windows の録音デバイス（マイクなど）が必要"]
    fn enumerate_audio_marks_recording_device_wrappers() {
        // 実機の列挙（#562）。Windows の録音デバイスは Audio Capture Filter に包まれて並び、
        // 包みの印が付く。Live Gamer EXTREME 3 の「HDMI/Line In (...)」もここに入る
        let _com = crate::com::ComApartment::enter(crate::com::ComModel::SingleThreaded)
            .expect("COM を初期化できる");
        let filters = devices::enumerate_audio().expect("列挙できる");
        for entry in &filters {
            println!(
                "{}（録音デバイスの包み: {}）",
                entry.friendly_name, entry.wraps_recording_device
            );
        }
        assert!(!filters.is_empty(), "音声のフィルターが 1 つも無い");
        assert!(
            filters.iter().any(|entry| entry.wraps_recording_device),
            "録音デバイスの包みが見分けられていない"
        );
    }
}
