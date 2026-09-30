//! 更新の資産（exe と `SHA256SUMS.txt`）の読み取り。HTTP かローカルのファイルを開き、
//! 小分けに読みながら読み取りの合間にキャンセルを見る（`docs/design/update.md` の
//! 「スレッドとキャンセル」）。
//!
//! 読んだものをどう使うか（ハッシュの計算、`.new` への書き込み）は `apply.rs`。
//!
//! 受け取りが完全に止まっても（1 バイトも届かない）キャンセルに気づけるよう、
//! ureq の接続の TCP と TLS の間に `WatchedTransport` を挟み、ソケットの読み取りを
//! `POLL_INTERVAL` ずつに区切って合間にキャンセルと無通信の時間切れを見る（Issue #357）。

use super::apply::{ApplyControl, ApplyError, CancelWatch};
use super::assets::AssetSource;
use super::{https_only_for, tls_config, USER_AGENT};
use log::debug;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::time::{Duration, Instant};
use ureq::unversioned::resolver::DefaultResolver;
use ureq::unversioned::transport::{
    Buffers, ConnectProxyConnector, ConnectionDetails, Connector, NativeTlsConnector, NextTimeout,
    TcpConnector, Transport,
};

/// 接続（TLS のハンドシェイクを含む）と、応答のヘッダーが揃うまでの上限。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// 1 回に読む大きさ。キャンセルと進捗はこの単位で見る。
const CHUNK_BYTES: usize = 64 * 1024;

/// ソケットの読み取りを待つ 1 回の長さ。受け取りが止まっている間も、キャンセルには
/// この間隔で気づく。
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// 1 バイトも届かないまま待つ上限（`ReadLimits::with_body` の既定）。
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// HTTP の読み取りの上限。
#[derive(Debug, Clone, Copy)]
pub(super) struct ReadLimits {
    /// 本文を受け取り終えるまでの上限（全体）
    pub(super) body: Duration,
    /// 1 バイトも届かないまま待つ上限。本文の途中でも、応答のヘッダーを待つ間でも効く
    pub(super) idle: Duration,
}

impl ReadLimits {
    /// 本文の上限を `body` にし、無通信の上限は `IDLE_TIMEOUT` にする。
    pub(super) const fn with_body(body: Duration) -> Self {
        Self {
            body,
            idle: IDLE_TIMEOUT,
        }
    }
}

pub(super) fn check_cancelled(control: &ApplyControl) -> Result<(), ApplyError> {
    if control.is_cancelled() {
        Err(ApplyError::Cancelled)
    } else {
        Ok(())
    }
}

/// 資産を読み始める。大きさが分かればそれも返す。
/// `limits` は HTTP の読み取りの上限（ファイルでは使わない）。応答のヘッダーを待つ間に
/// キャンセルされたら `Cancelled` を返す。
pub(super) fn open_source(
    source: &AssetSource,
    limits: ReadLimits,
    cancel: &ApplyControl,
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
                .timeout_recv_body(Some(limits.body))
                .https_only(https_only_for(url))
                .tls_config(tls_config())
                .user_agent(USER_AGENT)
                .build();
            let agent = ureq::Agent::with_parts(
                config,
                watched_connector(cancel.watch(), limits.idle),
                DefaultResolver::default(),
            );
            debug!("更新の資産を取る: {}", url);
            // GitHub の資産の URL は別のホストへのリダイレクトを返す。ureq が辿る
            let response = agent.get(url).call().map_err(|error| {
                if cancel.is_cancelled() {
                    ApplyError::Cancelled
                } else {
                    download_error_from(error)
                }
            })?;
            let body = response.into_body();
            let len = body.content_length();
            Ok((Box::new(body.into_reader()), len))
        }
    }
}

/// ureq の既定の接続の組み立て（`DefaultConnector`）から、使わないもの（rustls、
/// SOCKS の警告）を除き、TCP と TLS の間に `WatchedConnector` を挟んだもの。
///
/// TLS の上ではなく下に挟むのは、区切った読み取りの時間切れを TLS に見せないため。
/// TCP の読み取りは時間切れなら何も読んでいないので、そのまま待ち直せる。
fn watched_connector(cancel: CancelWatch, idle: Duration) -> impl Connector {
    ().chain(ConnectProxyConnector::default())
        .chain(TcpConnector::default())
        .chain(WatchedConnector { cancel, idle })
        .chain(NativeTlsConnector::default())
}

/// 前の段で開いた接続を `WatchedTransport` で包む。
#[derive(Debug)]
struct WatchedConnector {
    cancel: CancelWatch,
    idle: Duration,
}

impl<In: Transport> Connector<In> for WatchedConnector {
    type Out = WatchedTransport<In>;

    fn connect(
        &self,
        _: &ConnectionDetails,
        chained: Option<In>,
    ) -> Result<Option<Self::Out>, ureq::Error> {
        Ok(chained.map(|inner| WatchedTransport {
            inner,
            cancel: self.cancel.clone(),
            idle: self.idle,
        }))
    }
}

/// 読み取りを `POLL_INTERVAL` ずつに区切って待ち、合間にキャンセルと無通信の時間切れを
/// 見る接続。ureq には「読み取りが止まってから何秒」の上限が無く、本文の上限
/// （`ReadLimits::body`）まで読み取りから戻らないため。
#[derive(Debug)]
struct WatchedTransport<T> {
    inner: T,
    cancel: CancelWatch,
    idle: Duration,
}

impl<T: Transport> Transport for WatchedTransport<T> {
    fn buffers(&mut self) -> &mut dyn Buffers {
        self.inner.buffers()
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        self.inner.transmit_output(amount, timeout)
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        let started = Instant::now();
        // ureq の上限。0 は「すぐ」ではなく 1 秒として扱う（`TcpTransport` と同じ）
        let limit = timeout.not_zero().map_or(Duration::MAX, |after| *after);
        loop {
            if self.cancel.is_cancelled() {
                // 呼び出し側（`read_in_chunks` / `open_source`）がキャンセルを見て
                // `Cancelled` にする。`Interrupted` は読み直されうるので使わない
                return Err(ureq::Error::Io(io::Error::other("更新がキャンセルされた")));
            }
            let waited = started.elapsed();
            let left = limit.min(self.idle).saturating_sub(waited);
            if left.is_zero() {
                return Err(ureq::Error::Timeout(timeout.reason));
            }
            let slice = NextTimeout {
                after: left.min(POLL_INTERVAL).into(),
                reason: timeout.reason,
            };
            match self.inner.await_input(slice) {
                Err(ureq::Error::Timeout(_)) => continue,
                other => return other,
            }
        }
    }

    fn is_open(&mut self) -> bool {
        self.inner.is_open()
    }

    fn is_tls(&self) -> bool {
        self.inner.is_tls()
    }
}

/// 小さなテキストの資産（`SHA256SUMS.txt`）を読む。
///
/// exe と同じく小分けに読み、読み取りの合間にキャンセルを見る。受け取りが止まって
/// 読み取りが上限（`limits`）で失敗したときも、その間にキャンセルされていれば
/// 失敗ではなく `Cancelled` を返す（画面に失敗を出さない）。
pub(super) fn fetch_text(
    source: &AssetSource,
    limit: u64,
    limits: ReadLimits,
    cancel: &ApplyControl,
) -> Result<String, ApplyError> {
    let (mut reader, total) = open_source(source, limits, cancel)?;
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

    const LONG: Duration = Duration::from_secs(600);

    /// `interval` ごとに 1 バイト届く（`None` なら止まった）本文を `limits` で読み、
    /// 結果とかかった時間を返す。
    fn fetch_slow(
        interval: Option<Duration>,
        limits: ReadLimits,
        control: &ApplyControl,
    ) -> (Result<String, ApplyError>, Duration) {
        let source = AssetSource::Http(slow_http_server(interval));
        let started = Instant::now();
        let result = fetch_text(&source, MAX_CHECKSUMS_BYTES, limits, control);
        (result, started.elapsed())
    }

    #[test]
    fn fetch_text_stalled_body_gives_up_at_the_body_timeout() {
        let limits = ReadLimits {
            body: Duration::from_secs(1),
            idle: LONG,
        };

        let (result, elapsed) = fetch_slow(None, limits, &ApplyControl::default());

        assert_eq!(result, Err(ApplyError::Timeout));
        assert!(elapsed < Duration::from_secs(10), "{elapsed:?}");
    }

    #[test]
    fn fetch_text_stalled_body_gives_up_at_the_idle_timeout() {
        // 本文の上限（全体）より前に、1 バイトも届かない時間の上限で打ち切る
        let limits = ReadLimits {
            body: LONG,
            idle: Duration::from_millis(500),
        };

        let (result, elapsed) = fetch_slow(None, limits, &ApplyControl::default());

        assert_eq!(result, Err(ApplyError::Timeout));
        assert!(elapsed < Duration::from_secs(10), "{elapsed:?}");
    }

    #[test]
    fn fetch_text_trickling_body_is_not_an_idle_timeout() {
        // 少しずつでも届いていれば無通信の上限では止めず、本文の上限まで読む
        let limits = ReadLimits {
            body: Duration::from_secs(1),
            idle: Duration::from_millis(300),
        };

        let (result, elapsed) = fetch_slow(
            Some(Duration::from_millis(20)),
            limits,
            &ApplyControl::default(),
        );

        assert_eq!(result, Err(ApplyError::Timeout));
        assert!(elapsed >= Duration::from_millis(900), "{elapsed:?}");
    }

    #[test]
    fn fetch_text_cancel_while_stalled_returns_without_waiting_for_the_timeouts() {
        // 1 バイトも届かなくても、どちらの上限も待たずにキャンセルとして返す
        let limits = ReadLimits {
            body: LONG,
            idle: LONG,
        };
        let control = ApplyControl::default();

        let (result, elapsed) = std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(200));
                control.cancel();
            });
            fetch_slow(None, limits, &control)
        });

        assert_eq!(result, Err(ApplyError::Cancelled));
        assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
    }

    #[test]
    #[ignore = "ネットワーク（github.com への HTTPS）が必要"]
    fn fetch_text_reaches_a_release_asset_over_https() {
        // 実行: cargo test fetch_text_reaches_a_release_asset_over_https -- --ignored
        // 接続の組み立て（`watched_connector`）を変えたら一度は通す。TLS（native-tls）と、
        // 資産の URL から別のホストへのリダイレクトを辿れること
        let source = AssetSource::Http(
            "https://github.com/Mui-MuiMui/Capturecard_Viewer/releases/download/v1.2.1/SHA256SUMS.txt"
                .to_string(),
        );

        let sums = fetch_text(
            &source,
            MAX_CHECKSUMS_BYTES,
            ReadLimits::with_body(Duration::from_secs(30)),
            &ApplyControl::default(),
        )
        .expect("Release の資産を読めなければならない");

        assert!(sums.contains("capturecard_viewer.exe"), "{sums}");
    }
}
