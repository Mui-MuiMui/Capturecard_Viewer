//! 保存先のディスクの空き容量。
//!
//! 録画の開始時と録画中 5 秒ごとに確かめ、500MB を切ったら満杯になる前に止めて
//! `Finalize` する（`docs/design/recording.md` の「失敗の扱い」）。満杯まで書くと
//! `moov` を書けず、それまでの録画が再生できないファイルとして残るため。

use std::path::Path;
use std::time::Duration;

use windows::core::HSTRING;
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

/// これを切ったら録画を止める空き容量（バイト）
pub(super) const MIN_FREE_BYTES: u64 = 500 * 1024 * 1024;

/// 録画中に空き容量を確かめる間隔
pub(super) const DISK_CHECK_INTERVAL: Duration = Duration::from_secs(5);

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
    free.is_some_and(|bytes| bytes < MIN_FREE_BYTES)
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
}
