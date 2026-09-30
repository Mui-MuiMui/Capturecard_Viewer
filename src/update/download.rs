//! 更新の資産（exe と `SHA256SUMS.txt`）の読み取り。HTTP かローカルのファイルを開き、
//! 小分けに読みながら読み取りの合間にキャンセルを見る（`docs/design/update.md` の
//! 「スレッドとキャンセル」）。
//!
//! 読んだものをどう使うか（ハッシュの計算、`.new` への書き込み）は `apply.rs`。

use super::apply::{ApplyControl, ApplyError};
use super::assets::AssetSource;
use super::{tls_config, USER_AGENT};
use log::debug;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::time::Duration;

/// 接続（TLS のハンドシェイクを含む）と、応答のヘッダーが揃うまでの上限。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// 1 回に読む大きさ。キャンセルと進捗はこの単位で見る。
const CHUNK_BYTES: usize = 64 * 1024;

pub(super) fn check_cancelled(control: &ApplyControl) -> Result<(), ApplyError> {
    if control.is_cancelled() {
        Err(ApplyError::Cancelled)
    } else {
        Ok(())
    }
}

/// 資産を読み始める。大きさが分かればそれも返す。
/// `body_timeout` は HTTP の本文を受け取り終えるまでの上限（ファイルでは使わない）。
pub(super) fn open_source(
    source: &AssetSource,
    body_timeout: Duration,
) -> Result<(Box<dyn Read>, Option<u64>), ApplyError> {
    match source {
        AssetSource::File(path) => {
            let file = File::open(path).map_err(|e| file_error(path, e))?;
            let len = file.metadata().ok().map(|m| m.len());
            Ok((Box::new(file), len))
        }
        AssetSource::Http(url) => {
            let config = ureq::Agent::config_builder()
                .timeout_connect(Some(CONNECT_TIMEOUT))
                .timeout_recv_response(Some(CONNECT_TIMEOUT))
                .timeout_recv_body(Some(body_timeout))
                .tls_config(tls_config())
                .user_agent(USER_AGENT)
                .build();
            let agent = ureq::Agent::new_with_config(config);
            debug!("更新の資産を取る: {}", url);
            // GitHub の資産の URL は別のホストへのリダイレクトを返す。ureq が辿る
            let response = agent.get(url).call().map_err(download_error_from)?;
            let body = response.into_body();
            let len = body.content_length();
            Ok((Box::new(body.into_reader()), len))
        }
    }
}

/// 小さなテキストの資産（`SHA256SUMS.txt`）を読む。
///
/// exe と同じく小分けに読み、読み取りの合間にキャンセルを見る。受け取りが止まって
/// 読み取りが上限（`body_timeout`）で失敗したときも、その間にキャンセルされていれば
/// 失敗ではなく `Cancelled` を返す（画面に失敗を出さない）。
pub(super) fn fetch_text(
    source: &AssetSource,
    limit: u64,
    body_timeout: Duration,
    cancel: &ApplyControl,
) -> Result<String, ApplyError> {
    let (mut reader, total) = open_source(source, body_timeout)?;
    if total.is_some_and(|total| total > limit) {
        return Err(ApplyError::TooLarge);
    }
    let mut bytes = Vec::new();
    read_in_chunks(&mut reader, source, limit, cancel, |chunk, _| {
        bytes.extend_from_slice(chunk);
        Ok(())
    })?;
    check_cancelled(cancel)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// `reader` を `CHUNK_BYTES` ずつ読み終えるまで読み、1 回ごとに `on_chunk` へ
/// 読んだ分とそれまでの合計を渡す。読んだ合計を返す。
///
/// 1 回読む前にキャンセルを見る。読み取りが失敗したときも、その間にキャンセル
/// されていれば `Cancelled` を返す（受け取りが止まって上限で失敗した場合など）。
/// 合計が `limit` を超えたら、`on_chunk` へ渡す前に `TooLarge` で止める。
pub(super) fn read_in_chunks(
    reader: &mut dyn Read,
    source: &AssetSource,
    limit: u64,
    cancel: &ApplyControl,
    mut on_chunk: impl FnMut(&[u8], u64) -> Result<(), ApplyError>,
) -> Result<u64, ApplyError> {
    let mut buffer = vec![0u8; CHUNK_BYTES];
    let mut total: u64 = 0;
    loop {
        check_cancelled(cancel)?;
        let read = match reader.read(&mut buffer) {
            Ok(read) => read,
            Err(error) => {
                check_cancelled(cancel)?;
                return Err(read_error(source)(error));
            }
        };
        if read == 0 {
            return Ok(total);
        }
        total += read as u64;
        if total > limit {
            return Err(ApplyError::TooLarge);
        }
        on_chunk(&buffer[..read], total)?;
    }
}

fn download_error_from(error: ureq::Error) -> ApplyError {
    match error {
        ureq::Error::StatusCode(code) => ApplyError::HttpStatus(code),
        ureq::Error::Timeout(_) => ApplyError::Timeout,
        other => ApplyError::Network(other.to_string()),
    }
}

fn read_error(source: &AssetSource) -> impl Fn(io::Error) -> ApplyError + '_ {
    move |error| match source {
        AssetSource::File(path) => file_error(path, error),
        AssetSource::Http(_) if is_timeout(&error) => ApplyError::Timeout,
        AssetSource::Http(_) => ApplyError::Network(error.to_string()),
    }
}

/// 読み取りの失敗が上限の時間切れか。ureq は本文の上限（`timeout_recv_body`）を
/// `ErrorKind::Other` の中に `ureq::Error::Timeout` を包んで返すので、中身も見る。
fn is_timeout(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::TimedOut
        || error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<ureq::Error>())
            .is_some_and(|inner| matches!(inner, ureq::Error::Timeout(_)))
}

pub(super) fn file_error(path: &Path, error: io::Error) -> ApplyError {
    ApplyError::File(format!("{}: {}", path.display(), error))
}

/// 応答のヘッダーを返したあと、本文を `interval` ごとに 1 バイトずつ流す
/// （`None` なら 1 バイトも送らずに止まる）ローカルの HTTP サーバー。
/// 相手が切断したら抜ける。URL を返す。`apply.rs` のテストも使う。
#[cfg(test)]
pub(super) fn slow_http_server(interval: Option<Duration>) -> String {
    use std::io::Write;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("ローカルのポート");
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        // 要求（本文なし）を読み捨ててから応答のヘッダーを返す
        let mut request = [0u8; 4096];
        let _ = stream.read(&mut request);
        let header = b"HTTP/1.1 200 OK\r\nContent-Length: 60000\r\n\r\n";
        if stream.write_all(header).is_err() {
            return;
        }
        let Some(interval) = interval else {
            // 相手が切断するまで何も送らない
            let _ = stream.read(&mut request);
            return;
        };
        // 60000 バイトを送り切る前にテストが終われば、切断されてここで抜ける
        for _ in 0..60_000 {
            if stream.write_all(b"0").and_then(|_| stream.flush()).is_err() {
                return;
            }
            std::thread::sleep(interval);
        }
    });
    format!("http://{addr}/SHA256SUMS.txt")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::apply::MAX_CHECKSUMS_BYTES;

    #[test]
    fn fetch_text_stalled_body_gives_up_at_the_body_timeout() {
        // 受け取りが完全に止まると読み取りから戻れないので、本文の上限で打ち切る。
        // その間にキャンセルされていれば、失敗ではなくキャンセルとして返す
        let timeout = Duration::from_secs(1);
        let stalled = || AssetSource::Http(slow_http_server(None));
        let cancelled = ApplyControl::default();
        let running = ApplyControl::default();
        let fetch = |control: &ApplyControl| {
            let started = std::time::Instant::now();
            let result = fetch_text(&stalled(), MAX_CHECKSUMS_BYTES, timeout, control);
            (result, started.elapsed())
        };

        let (timed_out, elapsed) = fetch(&running);
        assert_eq!(timed_out, Err(ApplyError::Timeout));
        assert!(elapsed < Duration::from_secs(10), "{elapsed:?}");

        let (result, _) = std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(200));
                cancelled.cancel();
            });
            fetch(&cancelled)
        });
        assert_eq!(result, Err(ApplyError::Cancelled));
    }
}
