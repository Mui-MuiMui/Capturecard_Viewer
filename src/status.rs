//! 失敗の記録と、接続状態の受け渡し。
//!
//! これまで、デバイスに繋がらない・音声が開けない・スクリーンショットを
//! 保存できないといった失敗はログにしか出ておらず、ユーザーには何も伝わって
//! いなかった。ここでは以下の 2 つを扱う。
//!
//! - `ErrorCenter` — 発生源ごとに直近の失敗を 1 件だけ覚え、トーストを
//!   出してよいかを判断する
//! - `ConnectionStatus` — 設定ダイアログの「接続状態」タブへ渡す観測値
//!
//! **ログは従来どおり出す。** ここに記録するのは画面へ出すためのもので、
//! `error!` / `warn!` の置き換えではない。
//!
//! 日本語の文言をこのモジュールで組み立てているのは、下位のモジュールが
//! 返す `Result<_, String>` の中身が英語の技術的なメッセージだからで、
//! 「どこで何に失敗したか」はそれを受け取る側しか知らないため。
//! エラー型の整理（`thiserror` 化）は別タスクなので、`String` のまま扱う。

use chrono::{DateTime, Local};
use std::time::{Duration, Instant};

/// 同じ発生源で同じ文言の失敗が続くときに、トーストを出し直すまでの間隔。
///
/// 接続の再試行は最大 5 秒間隔で無限に続くため、間引かないと
/// 「接続できません」が 5 秒ごとに出続ける。一方で完全に 1 度きりにすると、
/// 席を外している間に出た通知に気付けない。映像が出ていない理由は
/// プレースホルダーと設定画面の「接続状態」タブに残り続けるので、
/// トーストは気付かせるためだけのものと割り切って長めに取ってある。
pub const ERROR_NOTIFY_INTERVAL: Duration = Duration::from_secs(60);

/// トーストに載せる文言の上限（文字数）。
///
/// 画面下部中央に出るため、長いエラー文をそのまま載せると映像を覆う。
/// 元の文言は設定画面の「接続状態」タブに全文が残る。
pub const TOAST_MESSAGE_LIMIT: usize = 60;

/// 映像プレースホルダーへ添える 1 行の上限（文字数）。
///
/// トーストより短くしてある。こちらは映像の中央に出るため、
/// 2 行目が長いと画面の見た目を壊す。
pub const PLACEHOLDER_DETAIL_LIMIT: usize = 48;

/// 失敗の発生源。
///
/// 画面に出す定型文をここで持つ。発生源ごとにユーザーの取るべき行動が
/// 違うため、同じ「エラー」としてまとめない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorSource {
    /// 映像デバイスの接続
    Video,
    /// 音声デバイスの接続
    Audio,
    /// スクリーンショットの撮影と保存
    Screenshot,
    /// グローバルホットキーの登録。
    ///
    /// 記録するのは `CaptureCardViewer::apply_hotkey_assignments`。
    /// 登録できないものが 1 つでも残っていれば通知し、すべて登録できたら
    /// `ErrorCenter::clear` で取り下げる
    Hotkey,
}

impl ErrorSource {
    /// 発生源ごとの定型文。元のエラー文の前に付ける。
    pub fn headline(self) -> &'static str {
        match self {
            ErrorSource::Video => "映像デバイスに接続できません",
            ErrorSource::Audio => "音声デバイスに接続できません",
            ErrorSource::Screenshot => "スクリーンショットを出力できません",
            ErrorSource::Hotkey => "ホットキーを登録できません",
        }
    }

    /// `ErrorCenter` の格納位置。
    fn index(self) -> usize {
        match self {
            ErrorSource::Video => 0,
            ErrorSource::Audio => 1,
            ErrorSource::Screenshot => 2,
            ErrorSource::Hotkey => 3,
        }
    }
}

/// `ErrorSource` の種類数。`ErrorCenter` の配列長。
const SOURCE_COUNT: usize = 4;

/// 記録した失敗 1 件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedError {
    /// 元のエラー文（下位モジュールが返した `String` そのまま）
    pub message: String,
    /// 発生時刻。間引きの判定に使う
    pub at: Instant,
    /// 表示用の発生時刻。`Instant` は表示できないため別に持つ
    pub at_wall: DateTime<Local>,
}

impl RecordedError {
    /// 「接続状態」タブに出す発生時刻。日をまたいでも読めるよう月日を入れる。
    pub fn time_text(&self) -> String {
        self.at_wall.format("%m/%d %H:%M:%S").to_string()
    }
}

/// 発生源ごとの直近の失敗と、それをいつトーストで見せたか。
struct Entry {
    error: RecordedError,
    /// 最後にトーストで見せた時刻。`None` は「まだ見せていない（未読）」
    notified_at: Option<Instant>,
}

/// 発生源ごとに直近の失敗を 1 件だけ持つ。
///
/// **履歴は積まない。** 同じ失敗が再試行のたびに繰り返されるため、
/// 積むと際限なく増える。過去の経緯はログファイルに残っている。
#[derive(Default)]
pub struct ErrorCenter {
    entries: [Option<Entry>; SOURCE_COUNT],
}

impl ErrorCenter {
    /// 失敗を記録する。トーストで見せるべきなら `true` を返す。
    ///
    /// `now` と `at_wall` を引数で受けるのは、判定を時計から切り離して
    /// テストできるようにするため。
    pub fn record(
        &mut self,
        source: ErrorSource,
        message: String,
        now: Instant,
        at_wall: DateTime<Local>,
    ) -> bool {
        let index = source.index();
        let previous = self.entries[index]
            .as_ref()
            .map(|entry| (entry.error.message.as_str(), entry.notified_at));
        let notify = should_notify(previous, &message, now, ERROR_NOTIFY_INTERVAL);
        // 見せなかった場合は前回見せた時刻を引き継ぐ。ここで `None` に戻すと
        // 次のフレームで「未読」と見なされ、間引きが効かなくなる
        let notified_at = if notify {
            Some(now)
        } else {
            self.entries[index]
                .as_ref()
                .and_then(|entry| entry.notified_at)
        };

        self.entries[index] = Some(Entry {
            error: RecordedError {
                message,
                at: now,
                at_wall,
            },
            notified_at,
        });
        notify
    }

    /// 記録を消す。接続に成功したときに呼ぶ。
    ///
    /// 消しておかないと、繋がったあとも設定画面に古い失敗が残る。
    /// 消すことで、次に同じ失敗が起きたときは間引かれずにトーストが出る。
    pub fn clear(&mut self, source: ErrorSource) {
        self.entries[source.index()] = None;
    }

    /// 発生源の直近の失敗。
    pub fn latest(&self, source: ErrorSource) -> Option<&RecordedError> {
        self.entries[source.index()]
            .as_ref()
            .map(|entry| &entry.error)
    }
}

/// 失敗をトーストで見せるべきかを判定する。
///
/// `previous` は同じ発生源の前回の記録 `(文言, 最後に見せた時刻)`。
/// `None` は「その発生源にまだ記録が無い」を表す。
///
/// - 記録が無い、または文言が変わった → 見せる。別の失敗なので伝える価値がある
/// - 同じ文言でまだ一度も見せていない → 見せる
/// - 同じ文言を既に見せている → `interval` が空くまで見せない
///
/// 境界（`interval` ちょうど）は見せる側に倒す。`should_retry_now` など
/// このリポジトリの他の期限判定と揃えている。
pub fn should_notify(
    previous: Option<(&str, Option<Instant>)>,
    message: &str,
    now: Instant,
    interval: Duration,
) -> bool {
    match previous {
        None => true,
        Some((previous_message, _)) if previous_message != message => true,
        Some((_, None)) => true,
        Some((_, Some(notified_at))) => now.saturating_duration_since(notified_at) >= interval,
    }
}

/// 画面へ出す文言を組み立てる。`<定型文>: <元のエラー文>`。
///
/// 元のエラー文が空のときは定型文だけを返す。`: ` だけが末尾に残ると、
/// 何かが切れているように見えるため。
pub fn format_message(source: ErrorSource, message: &str) -> String {
    let message = message.trim();
    if message.is_empty() {
        source.headline().to_string()
    } else {
        format!("{}: {}", source.headline(), message)
    }
}

/// 文字数で切り詰める。切り詰めた場合は末尾に `…` を付ける。
///
/// **バイト数ではなく文字数で数える。** 日本語と英語が混じるので、
/// バイト境界で切ると文字が壊れる（`String` のスライスはパニックする）。
///
/// `limit` が 0 の場合は空文字列を返す。省略記号だけを返しても意味が無い。
pub fn truncate(text: &str, limit: usize) -> String {
    if limit == 0 {
        return String::new();
    }
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        format!("{}…", head)
    } else {
        head
    }
}

/// 映像か音声、片方の接続状態。設定ダイアログの「接続状態」タブへ渡す。
///
/// **`VideoCapture` / `AudioCapture` から値を複製して作る。** 描画中に
/// ロックを取らないようにするためで、`stats()` / `link_state()` と同じ流儀。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkStatus {
    /// ストリームを開けているか
    pub connected: bool,
    /// 接続を追いかけている最中か（`ConnectRetry::is_active`）
    pub reconnecting: bool,
    /// 連続して失敗した回数
    pub attempts: u32,
    /// 実際に開いた内容の説明。行ごとに分けて渡す
    pub details: Vec<String>,
    /// 直近の失敗。`(整形済みの文言, 発生時刻)`
    pub error: Option<(String, String)>,
}

/// 映像と音声の接続状態。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectionStatus {
    pub video: LinkStatus,
    pub audio: LinkStatus,
}

impl LinkStatus {
    /// 状態を 1 行で表す見出し。
    pub fn headline(&self) -> &'static str {
        match (self.connected, self.reconnecting) {
            (true, _) => "接続中",
            (false, true) => "未接続（再接続を試しています）",
            (false, false) => "未接続",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wall() -> DateTime<Local> {
        Local::now()
    }

    #[test]
    fn should_notify_without_previous_record_returns_true() {
        assert!(should_notify(
            None,
            "Device 'X' not found",
            Instant::now(),
            ERROR_NOTIFY_INTERVAL
        ));
    }

    #[test]
    fn should_notify_with_a_different_message_returns_true() {
        // 直前に見せたばかりでも、内容が変われば伝える価値がある
        let now = Instant::now();
        assert!(should_notify(
            Some(("Device 'X' not found", Some(now))),
            "Failed to open camera stream",
            now,
            ERROR_NOTIFY_INTERVAL
        ));
    }

    #[test]
    fn should_notify_with_the_same_message_not_yet_shown_returns_true() {
        let now = Instant::now();
        assert!(should_notify(
            Some(("Device 'X' not found", None)),
            "Device 'X' not found",
            now,
            ERROR_NOTIFY_INTERVAL
        ));
    }

    #[test]
    fn should_notify_with_the_same_message_within_the_interval_returns_false() {
        let start = Instant::now();
        assert!(!should_notify(
            Some(("Device 'X' not found", Some(start))),
            "Device 'X' not found",
            start + Duration::from_secs(59),
            ERROR_NOTIFY_INTERVAL
        ));
    }

    #[test]
    fn should_notify_with_the_same_message_exactly_at_the_interval_returns_true() {
        // 境界。期限ちょうどでは見せる側に倒す
        let start = Instant::now();
        assert!(should_notify(
            Some(("Device 'X' not found", Some(start))),
            "Device 'X' not found",
            start + ERROR_NOTIFY_INTERVAL,
            ERROR_NOTIFY_INTERVAL
        ));
    }

    #[test]
    fn should_notify_with_a_time_that_went_backwards_returns_false() {
        // 呼び出し側が時刻を持ち回るため、過去の時刻が渡っても落ちないこと
        let start = Instant::now();
        assert!(!should_notify(
            Some((
                "Device 'X' not found",
                Some(start + Duration::from_secs(10))
            )),
            "Device 'X' not found",
            start,
            ERROR_NOTIFY_INTERVAL
        ));
    }

    #[test]
    fn error_center_first_record_is_shown_and_kept() {
        let now = Instant::now();
        let mut center = ErrorCenter::default();

        assert!(center.record(ErrorSource::Video, "not found".to_string(), now, wall()));
        assert_eq!(
            center
                .latest(ErrorSource::Video)
                .map(|e| e.message.as_str()),
            Some("not found")
        );
    }

    #[test]
    fn error_center_repeated_record_updates_without_showing() {
        let start = Instant::now();
        let mut center = ErrorCenter::default();
        center.record(ErrorSource::Video, "not found".to_string(), start, wall());

        let second = start + Duration::from_secs(5);
        assert!(!center.record(ErrorSource::Video, "not found".to_string(), second, wall()));
        // 見せなくても記録は最新の時刻に更新される
        assert_eq!(
            center.latest(ErrorSource::Video).map(|e| e.at),
            Some(second)
        );
    }

    #[test]
    fn error_center_repeated_record_is_shown_again_after_the_interval() {
        let start = Instant::now();
        let mut center = ErrorCenter::default();
        center.record(ErrorSource::Video, "not found".to_string(), start, wall());
        // 間引かれている間に何度呼ばれても、最後に見せた時刻は動かない
        center.record(
            ErrorSource::Video,
            "not found".to_string(),
            start + Duration::from_secs(30),
            wall(),
        );

        assert!(center.record(
            ErrorSource::Video,
            "not found".to_string(),
            start + ERROR_NOTIFY_INTERVAL,
            wall()
        ));
    }

    #[test]
    fn error_center_sources_are_independent() {
        let now = Instant::now();
        let mut center = ErrorCenter::default();
        center.record(ErrorSource::Video, "same".to_string(), now, wall());

        // 発生源が違えば同じ文言でも間引かない
        assert!(center.record(ErrorSource::Audio, "same".to_string(), now, wall()));
        assert_eq!(
            center
                .latest(ErrorSource::Video)
                .map(|e| e.message.as_str()),
            Some("same")
        );
    }

    #[test]
    fn error_center_clear_drops_the_record_and_the_suppression() {
        let start = Instant::now();
        let mut center = ErrorCenter::default();
        center.record(ErrorSource::Video, "not found".to_string(), start, wall());
        center.clear(ErrorSource::Video);

        assert!(center.latest(ErrorSource::Video).is_none());
        // 繋がったあとに同じ失敗が起きたら、間引かずに見せる
        assert!(center.record(
            ErrorSource::Video,
            "not found".to_string(),
            start + Duration::from_secs(1),
            wall()
        ));
    }

    #[test]
    fn error_center_clear_of_an_empty_source_does_nothing() {
        let mut center = ErrorCenter::default();
        center.clear(ErrorSource::Screenshot);

        assert!(center.latest(ErrorSource::Screenshot).is_none());
    }

    #[test]
    fn format_message_puts_the_headline_before_the_raw_error() {
        assert_eq!(
            format_message(ErrorSource::Video, "Device 'X' not found"),
            "映像デバイスに接続できません: Device 'X' not found"
        );
    }

    #[test]
    fn format_message_with_an_empty_error_keeps_only_the_headline() {
        assert_eq!(
            format_message(ErrorSource::Audio, "   "),
            "音声デバイスに接続できません"
        );
    }

    #[test]
    fn format_message_uses_a_different_headline_per_source() {
        assert_eq!(
            format_message(ErrorSource::Screenshot, "access denied"),
            "スクリーンショットを出力できません: access denied"
        );
        assert_eq!(
            format_message(ErrorSource::Hotkey, "F5 is in use"),
            "ホットキーを登録できません: F5 is in use"
        );
    }

    #[test]
    fn truncate_shorter_than_the_limit_is_unchanged() {
        assert_eq!(truncate("接続できません", 20), "接続できません");
    }

    #[test]
    fn truncate_exactly_at_the_limit_has_no_ellipsis() {
        // 境界。ちょうど収まる場合は切り詰めない
        assert_eq!(truncate("abcde", 5), "abcde");
    }

    #[test]
    fn truncate_longer_than_the_limit_adds_an_ellipsis() {
        assert_eq!(truncate("abcdef", 5), "abcde…");
    }

    #[test]
    fn truncate_counts_characters_not_bytes() {
        // 日本語は 1 文字 3 バイト。バイトで数えると途中で切れて壊れる
        assert_eq!(truncate("あいうえお", 3), "あいう…");
    }

    #[test]
    fn truncate_with_a_zero_limit_returns_an_empty_string() {
        assert_eq!(truncate("あいうえお", 0), "");
    }

    #[test]
    fn truncate_an_empty_string_returns_an_empty_string() {
        assert_eq!(truncate("", 10), "");
    }

    #[test]
    fn link_status_headline_reflects_the_three_states() {
        let connected = LinkStatus {
            connected: true,
            ..LinkStatus::default()
        };
        let reconnecting = LinkStatus {
            connected: false,
            reconnecting: true,
            ..LinkStatus::default()
        };
        let idle = LinkStatus::default();

        assert_eq!(connected.headline(), "接続中");
        assert_eq!(reconnecting.headline(), "未接続（再接続を試しています）");
        assert_eq!(idle.headline(), "未接続");
    }

    #[test]
    fn recorded_error_time_text_includes_the_date_and_time() {
        let at_wall = Local::now();
        let error = RecordedError {
            message: "not found".to_string(),
            at: Instant::now(),
            at_wall,
        };

        assert_eq!(
            error.time_text(),
            at_wall.format("%m/%d %H:%M:%S").to_string()
        );
    }
}
