//! `SHA256SUMS.txt` の読み方と、SHA-256 の照合（`docs/design/update.md` の「適用」）。
//!
//! 形式は `docs/RELEASE.md` の「配布物」。**照合は大文字小文字を区別しない。**

/// `SHA256SUMS.txt` から `file_name` の行の hash を引く。
///
/// 形式は `sha256sum` と同じ `<64 桁の 16 進>  <ファイル名>`。バイナリモードの
/// 印（名前の前の `*`）、CRLF、先頭の BOM も読む。hash の大文字小文字は問わない。
/// 名前は完全に一致するものだけ。hash の形がおかしい行は無いものとして扱う。
pub fn find_checksum<'a>(sums: &'a str, file_name: &str) -> Option<&'a str> {
    sums.trim_start_matches('\u{feff}')
        .lines()
        .find_map(|line| {
            let (hash, rest) = line.trim().split_once(char::is_whitespace)?;
            let name = rest.trim_start();
            let name = name.strip_prefix('*').unwrap_or(name);
            let is_hash = hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit());
            (is_hash && name == file_name).then_some(hash)
        })
}

/// 期待する hash（16 進）と計算した SHA-256 が一致するか。大文字小文字は問わない。
pub fn checksum_matches(expected_hex: &str, actual: &[u8]) -> bool {
    expected_hex.eq_ignore_ascii_case(&to_hex(actual))
}

pub(super) fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    bytes.iter().fold(String::new(), |mut hex, b| {
        let _ = write!(hex, "{b:02x}");
        hex
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    // "hello" の SHA-256
    const HELLO_SHA256: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    #[test]
    fn find_checksum_reads_the_sha256sum_format() {
        let sums = format!(
            "{}  capturecard_viewer-v1.2.0-windows-x64.zip\n{HELLO_SHA256}  capturecard_viewer-v1.2.0-windows-x64.exe\n",
            "0".repeat(64)
        );

        assert_eq!(
            find_checksum(&sums, "capturecard_viewer-v1.2.0-windows-x64.exe"),
            Some(HELLO_SHA256)
        );
        assert_eq!(
            find_checksum(&sums, "capturecard_viewer-v1.2.0-windows-x64.zip"),
            Some("0".repeat(64).as_str())
        );
    }

    #[test]
    fn find_checksum_accepts_crlf_bom_and_binary_marker() {
        let upper = HELLO_SHA256.to_uppercase();
        let sums = format!("\u{feff}{upper} *a.exe\r\n");

        assert_eq!(find_checksum(&sums, "a.exe"), Some(upper.as_str()));
    }

    #[test]
    fn find_checksum_requires_an_exact_name_and_a_valid_hash() {
        let sums = format!(
            "{HELLO_SHA256}  a.exe.bak\n{HELLO_SHA256}  b/a.exe\nnot-a-hash  a.exe\n{}  a.exe\n{}  a.exe\n",
            "0".repeat(63),
            "g".repeat(64)
        );

        assert_eq!(find_checksum(&sums, "a.exe"), None);
        assert_eq!(find_checksum("", "a.exe"), None);
    }

    #[test]
    fn checksum_matches_ignores_case() {
        let hello = Sha256::digest(b"hello");

        assert!(checksum_matches(HELLO_SHA256, hello.as_slice()));
        assert!(checksum_matches(
            &HELLO_SHA256.to_uppercase(),
            hello.as_slice()
        ));
        assert!(!checksum_matches(&"0".repeat(64), hello.as_slice()));
        assert!(!checksum_matches("", hello.as_slice()));
    }
}
