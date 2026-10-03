//! 保存先のディスクの空き容量。
//!
//! 録画の開始時と録画中 5 秒ごとに確かめ、500MB を切ったら満杯になる前に止めて
//! `Finalize` する（`docs/design/recording.md` の「失敗の扱い」）。満杯まで書くと
//! `moov` を書けず、それまでの録画が再生できないファイルとして残るため。
//!
//! リプレイバッファを通す録画（③）は、始めてすぐリングの中身をまとめて書き出す。
//! 開始時は「リングの大きさ + 500MB」の空きを求め、録画中は時間だけでなく書いた
//! バイト数でも見る（#313）。

use std::path::Path;
use std::time::Duration;

use windows::core::HSTRING;
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

/// これを切ったら録画を止める空き容量（バイト）
pub(super) const MIN_FREE_BYTES: u64 = 500 * 1024 * 1024;

/// 録画中に空き容量を確かめる間隔
pub(super) const DISK_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// 録画中に空き容量を確かめる、書いたバイト数の区切り。リングの中身を書き出している間は
/// 5 秒で数百 MB 書きうるので、時間の区切りより先にこちらで見る
pub(super) const DISK_CHECK_BYTES: u64 = 64 * 1024 * 1024;

/// `folder` のあるドライブで、このユーザーが使える空き容量。取れなければ `None`。
pub(super) fn free_bytes(folder: &Path) -> Option<u64> {
    let path = HSTRING::from(folder);
    let mut available = 0u64;
    unsafe { GetDiskFreeSpaceExW(&path, Some(&mut available), None, None) }.ok()?;
    Some(available)
}

/// 空き容量が録画を続けられないほど少ないか。取れなかった（`None`）ときは続ける。
/// 取れないのに止めると、ネットワークドライブなどで一切録画できなくなる。
pub(super) fn is_low(free: Option<u64>) -> bool {
    is_short(free, MIN_FREE_BYTES)
}

/// 空き容量が `required` に足りないか。取れなかった（`None`）ときは足りるとみなす
/// （`is_low` と同じ理由）。
pub(super) fn is_short(free: Option<u64>, required: u64) -> bool {
    free.is_some_and(|bytes| bytes < required)
}

/// リプレイバッファを通す録画を始めるのに要る空き容量。リングをすべて書き出しても
/// 500MB 残る量。`ring_bytes` はリングが持っているデータの大きさ。
pub(super) fn replay_required_bytes(ring_bytes: u64) -> u64 {
    ring_bytes.saturating_add(MIN_FREE_BYTES)
}

/// 録画中に空き容量を見る時期か。前に見てから `DISK_CHECK_INTERVAL` 経ったか、
/// `DISK_CHECK_BYTES` 以上書いた。
pub(super) fn disk_check_due(elapsed: Duration, written_since: u64) -> bool {
    elapsed >= DISK_CHECK_INTERVAL || written_since >= DISK_CHECK_BYTES
}

/// 表示用に MB へ直す（切り捨て）。
pub(super) fn megabytes(bytes: u64) -> u64 {
    bytes / (1024 * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_low_below_500mb_is_true() {
        assert!(is_low(Some(MIN_FREE_BYTES - 1)));
        assert!(is_low(Some(0)));
    }

    #[test]
    fn is_low_at_or_above_500mb_is_false() {
        assert!(!is_low(Some(MIN_FREE_BYTES)));
        assert!(!is_low(Some(u64::MAX)));
    }

    #[test]
    fn is_low_unknown_keeps_recording() {
        assert!(!is_low(None));
    }

    // #313: 開始時の判定がリングの大きさを見る。300 秒 × 50Mbps ≒ 1.9GB のリングで
    // 空きが 800MB なら、500MB の境界は通っても始めない
    #[test]
    fn replay_start_needs_room_for_the_whole_ring() {
        let ring = 1_900 * 1024 * 1024;
        let free = Some(800 * 1024 * 1024);
        assert!(!is_low(free), "500MB の境界だけなら通ってしまう");
        let required = replay_required_bytes(ring);
        assert!(is_short(free, required));
        // リングを書き出しても 500MB 残るなら始める
        assert!(!is_short(Some(required), required));
        assert!(is_short(Some(required - 1), required));
        // 取れないときは始める（ネットワークドライブなど）
        assert!(!is_short(None, required));
        // 空のリングなら①②と同じ 500MB
        assert_eq!(replay_required_bytes(0), MIN_FREE_BYTES);
        assert_eq!(replay_required_bytes(u64::MAX), u64::MAX);
    }

    #[test]
    fn disk_check_due_by_time_or_by_bytes_written() {
        assert!(!disk_check_due(Duration::ZERO, 0));
        assert!(!disk_check_due(
            DISK_CHECK_INTERVAL - Duration::from_millis(1),
            DISK_CHECK_BYTES - 1
        ));
        assert!(disk_check_due(DISK_CHECK_INTERVAL, 0));
        // リングを書き出している間は 5 秒を待たずに見る
        assert!(disk_check_due(Duration::from_millis(10), DISK_CHECK_BYTES));
    }

    #[test]
    fn megabytes_rounds_down() {
        assert_eq!(megabytes(MIN_FREE_BYTES), 500);
        assert_eq!(megabytes(1024 * 1024 - 1), 0);
    }

    #[test]
    fn free_bytes_of_the_temp_folder_is_known() {
        // 一時フォルダのドライブは必ずある
        assert!(free_bytes(&std::env::temp_dir()).is_some());
    }

    // 一時フォルダの実際の空き容量で、要る量が空きを 1 バイトでも超えれば断られる
    #[test]
    fn is_short_refuses_when_the_temp_folder_lacks_room() {
        let free = free_bytes(&std::env::temp_dir());
        let available = free.expect("一時フォルダの空き容量を取れる");
        assert!(is_short(free, available.saturating_add(1)) || available == u64::MAX);
        assert!(!is_short(free, available));
    }
}
