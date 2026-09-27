//! デバイス接続の再試行を、デバイスワーカースレッドを止めずに回すための
//! 状態とバックオフ。
//!
//! デバイスワーカーのループ（`super::worker_loop::run`）が `tick()` のたびに
//! 期限を見て、来ていれば 1 回だけ試す。実際にデバイスを開くのは
//! `super::worker_connect`、途絶したかを判定するのは `super::monitor`
//! （純粋関数だけを持ち、再試行の要求自体は呼び出し元の `super::worker_loop`
//! が行う）。ここは「いつ試してよいか」だけを持つ。

use std::time::{Duration, Instant};

/// 接続に失敗したあと、最初に待つ時間。
///
/// 実測ではデバイスの列挙が 1〜2ms、`Camera::new` が 28〜90ms なので、
/// 1 回目の再試行を 200ms 後に置いても取りこぼしはほぼ無い。
const CONNECT_BACKOFF_BASE: Duration = Duration::from_millis(200);

/// 再試行の間隔の上限。
///
/// 無限に再試行するので、間隔を伸ばし続けると「後からデバイスを挿した」
/// ときの反応が悪くなる。5 秒で頭打ちにして、挿してから最大 5 秒で繋がるようにする。
const CONNECT_BACKOFF_MAX: Duration = Duration::from_millis(5000);

/// 接続に成功してから、次に開き直してよいまでの下限。
///
/// バックオフは失敗の連続でしか伸びないため、「開けた直後に切断を検出する」
/// デバイスでは成功のたびに待ち時間が 0 へ戻り、開き直しが待ち無しで回り続ける
/// （DirectShow で開いた直後に毎回 `EC_ERROR_STILLPLAYING` を出すデバイスなど）。
/// 成功した時刻から数えるこの下限で、その繰り返しを 1 秒に 1 回へ抑える（#232）。
///
/// 1 秒にしてあるのは、開いてから 1 枚目が届くまでが実測 0.8 秒で、それより
/// 短い間隔で開き直しても映像が出る前に閉じるだけになるため。一方で長くすると、
/// 開いた直後に本当に抜かれたときの復帰がその分だけ遅れる。
const RECONNECT_MIN_INTERVAL_AFTER_SUCCESS: Duration = Duration::from_secs(1);

/// 連続 `attempt` 回失敗したあとに待つ時間を返す。
///
/// `CONNECT_BACKOFF_BASE` から倍々に伸ばし、`CONNECT_BACKOFF_MAX` で頭打ちにする。
/// 200ms → 400 → 800 → 1600 → 3200 → 5000ms（以降は 5000ms のまま）。
///
/// `attempt` は失敗が続く限り際限なく増えるため、シフトでは桁あふれを起こす。
/// 頭打ちに達する回数で先に打ち切って、パニックしないようにしてある。
pub(super) fn backoff_delay(attempt: u32) -> Duration {
    let Some(shift) = attempt.checked_sub(1) else {
        // まだ 1 度も失敗していない。待たずに試す
        return Duration::ZERO;
    };
    // 1u32 << 32 は未定義。頭打ちには 6 回目で届くので、ここへ来た時点で上限でよい
    if shift >= u32::BITS {
        return CONNECT_BACKOFF_MAX;
    }
    match CONNECT_BACKOFF_BASE.checked_mul(1u32 << shift) {
        Some(delay) if delay < CONNECT_BACKOFF_MAX => delay,
        _ => CONNECT_BACKOFF_MAX,
    }
}

/// 次に試してよい時刻が来ているかを判定する。
///
/// `None` は「期限が無い＝いますぐ試してよい」を表す。境界（期限ちょうど）では
/// 試す側に倒す。1 フレーム遅らせても得るものが無いため。
fn should_retry_now(next_attempt_at: Option<Instant>, now: Instant) -> bool {
    match next_attempt_at {
        None => true,
        Some(deadline) => now >= deadline,
    }
}

/// 最後に接続に成功した時刻から、次に開いてよい時刻が来ているかを判定する。
///
/// `None` は「まだ一度も繋がっていない」で、下限は掛からない。
/// 境界（下限ちょうど）では試す側に倒す。`should_retry_now` と揃えるため。
fn min_interval_elapsed(connected_at: Option<Instant>, now: Instant) -> bool {
    match connected_at {
        None => true,
        Some(connected_at) => {
            now.saturating_duration_since(connected_at) >= RECONNECT_MIN_INTERVAL_AFTER_SUCCESS
        }
    }
}

/// デバイス接続の再試行を、デバイスワーカースレッドを止めずに回すための状態。
///
/// デバイスワーカーのループが `tick()` のたびに `is_due()` を見て、期限が
/// 来ていれば 1 回だけ試す。**`thread::sleep` を使わない。** 待つ代わりに
/// 次に試してよい時刻を覚えておく。以前は UI スレッドで最大 3 秒眠っていたため、
/// 接続に失敗する環境ではその間ウィンドウが固まっていた。
///
/// `T` は「いま何へ繋ごうとしているか」を表す値（デバイス名や解像度の組）。
/// 2 秒ごとの設定の再適用は同じ対象を何度も要求してくるため、対象が同じなら
/// 進行中のバックオフを維持する。これをしないと待ち時間が毎回巻き戻り、
/// 繋がらないデバイスへ 2 秒間に 4 回も接続を試みることになる。
#[derive(Debug)]
pub(super) struct ConnectRetry<T> {
    /// いま繋ごうとしている対象。`None` は「接続を要求されていない」
    target: Option<T>,
    /// 連続して失敗した回数。成功と、対象が変わったときに 0 へ戻る
    attempts: u32,
    /// 次に試してよい時刻。`None` は「いますぐ試してよい」
    next_attempt_at: Option<Instant>,
    /// 最後に接続に成功した時刻。`None` は「まだ一度も繋がっていない」。
    ///
    /// **要求や成功以外では消さない。** `request_now` がバックオフを捨てても、
    /// 成功からの下限（`RECONNECT_MIN_INTERVAL_AFTER_SUCCESS`）は残す。
    /// 切断の検出はどれも `request_now` で要求を積むため、ここで消すと下限が効かない
    connected_at: Option<Instant>,
}

impl<T> Default for ConnectRetry<T> {
    fn default() -> Self {
        Self {
            target: None,
            attempts: 0,
            next_attempt_at: None,
            connected_at: None,
        }
    }
}

impl<T: PartialEq> ConnectRetry<T> {
    /// 接続を要求する。
    ///
    /// **同じ対象を既に追いかけている場合は何もしない。** 2 秒ごとの設定の
    /// 再適用がここを通るため、毎回やり直すとバックオフが伸びなくなる。
    /// 対象が変わった場合（設定画面でデバイスを選び直した等）は数え直して
    /// 即座に試す。ユーザーの操作に対して最大 5 秒待たせる理由が無いため。
    pub(super) fn request(&mut self, target: T) {
        if self.target.as_ref() == Some(&target) {
            return;
        }
        self.request_now(target);
    }

    /// 対象が同じでもバックオフを捨てて即座に試す。
    ///
    /// 右クリックメニューの「デバイス再接続」のように、ユーザーが明示的に
    /// やり直しを求めた場合や、切断を検出した場合に使う。
    ///
    /// **接続に成功してからの下限（1 秒）は捨てない。** 成功した直後に切断を
    /// 検出した場合は、ここで要求を積むだけで、下限が過ぎるまで開き直さない。
    /// ユーザーの操作でも最大 1 秒しか待たないので、経路で分けていない。
    pub(super) fn request_now(&mut self, target: T) {
        self.target = Some(target);
        self.attempts = 0;
        self.next_attempt_at = None;
    }

    /// 接続の要求を取り下げる。繋ぐ相手が無い（デバイス名が未設定）ときに使う。
    pub(super) fn cancel(&mut self) {
        self.target = None;
        self.next_attempt_at = None;
    }

    /// このフレームで接続を試してよいか。
    ///
    /// 失敗のバックオフと、成功からの下限の両方を満たしたときだけ真になる。
    pub(super) fn is_due(&self, now: Instant) -> bool {
        self.target.is_some()
            && should_retry_now(self.next_attempt_at, now)
            && min_interval_elapsed(self.connected_at, now)
    }

    /// 接続を追いかけている最中か。繋がると `false` に戻る。
    /// 「再接続を試しています」という表示の出し分けに使う
    pub(super) fn is_active(&self) -> bool {
        self.target.is_some()
    }

    /// 連続して失敗した回数。
    pub(super) fn attempts(&self) -> u32 {
        self.attempts
    }

    /// 成功を記録する。以降は要求があるまで試さない。
    ///
    /// `now` は次に開き直してよい時刻の起点になる（`RECONNECT_MIN_INTERVAL_AFTER_SUCCESS`）。
    ///
    /// **呼び出し側は、開き終えた時刻ではなく試行を始めた `tick` の時刻を渡す。**
    /// 抑えたいのは試行の頻度なので、「試行の開始から次の試行の開始まで 1 秒」で
    /// 足りる。開き終えた時刻を `Instant::now()` で取ると、ワーカーが `tick` へ
    /// 渡している時刻（テストやフェイクの時計が進める時刻）と食い違う。
    pub(super) fn record_success(&mut self, now: Instant) {
        self.target = None;
        self.attempts = 0;
        self.next_attempt_at = None;
        self.connected_at = Some(now);
    }

    /// 失敗を記録し、次に試してよい時刻を決める。
    ///
    /// 対象は保持したままにする。繋がるまで無限に再試行し、後からデバイスを
    /// 挿した場合に何もしなくても繋がるようにするため。
    ///
    /// **失敗の理由はここでは持たない。** 画面へ出す記録は `ErrorCenter` が
    /// 発生源ごとにまとめて持っており、両方に置くと消し忘れた片方が古い
    /// 理由を出し続ける。
    pub(super) fn record_failure(&mut self, now: Instant) {
        self.attempts = self.attempts.saturating_add(1);
        self.next_attempt_at = now.checked_add(backoff_delay(self.attempts));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_delay_first_failure_waits_base_interval() {
        // 1 回目の失敗の後は 200ms。固定 2 秒待ちの代わりになる短さであること
        assert_eq!(backoff_delay(1), Duration::from_millis(200));
    }

    #[test]
    fn backoff_delay_doubles_until_it_reaches_the_cap() {
        // 期待値は表としてベタ書きする。実装と同じ式で作ると、式が誤っていても通る
        assert_eq!(backoff_delay(1), Duration::from_millis(200));
        assert_eq!(backoff_delay(2), Duration::from_millis(400));
        assert_eq!(backoff_delay(3), Duration::from_millis(800));
        assert_eq!(backoff_delay(4), Duration::from_millis(1600));
        assert_eq!(backoff_delay(5), Duration::from_millis(3200));
    }

    #[test]
    fn backoff_delay_beyond_the_cap_stays_at_the_cap() {
        // 6 回目は倍にすると 6400ms になるので頭打ちの 5000ms へ落ちる
        assert_eq!(backoff_delay(6), Duration::from_millis(5000));
        assert_eq!(backoff_delay(7), Duration::from_millis(5000));
        assert_eq!(backoff_delay(100), Duration::from_millis(5000));
    }

    #[test]
    fn backoff_delay_huge_attempt_count_does_not_overflow() {
        // 無限に再試行するので attempts は際限なく増える。
        // シフト量が u32 の幅を超えてもパニックしないこと
        assert_eq!(backoff_delay(31), Duration::from_millis(5000));
        assert_eq!(backoff_delay(32), Duration::from_millis(5000));
        assert_eq!(backoff_delay(33), Duration::from_millis(5000));
        assert_eq!(backoff_delay(u32::MAX), Duration::from_millis(5000));
    }

    #[test]
    fn backoff_delay_zero_attempts_is_zero() {
        // まだ 1 度も失敗していない状態。待たずに試す
        assert_eq!(backoff_delay(0), Duration::ZERO);
    }

    #[test]
    fn should_retry_now_without_deadline_returns_true() {
        // 期限が無い＝いますぐ試してよい。初回接続がこれに当たる
        assert!(should_retry_now(None, Instant::now()));
    }

    #[test]
    fn should_retry_now_before_deadline_returns_false() {
        let now = Instant::now();
        let deadline = now + Duration::from_millis(200);
        assert!(!should_retry_now(Some(deadline), now));
    }

    #[test]
    fn should_retry_now_exactly_at_deadline_returns_true() {
        // 境界。期限ちょうどでは試す
        let now = Instant::now();
        assert!(should_retry_now(Some(now), now));
    }

    #[test]
    fn should_retry_now_after_deadline_returns_true() {
        let deadline = Instant::now();
        let now = deadline + Duration::from_millis(1);
        assert!(should_retry_now(Some(deadline), now));
    }

    #[test]
    fn connect_retry_request_with_the_same_target_keeps_the_backoff() {
        // 2 秒ごとの再適用が、進行中のバックオフを巻き戻してしまう不具合の再現。
        // 同じ対象を追いかけ続けている間は、待ち時間も失敗回数も維持されること
        let now = Instant::now();
        let mut retry = ConnectRetry::default();
        retry.request("デバイス A");
        retry.record_failure(now);
        retry.record_failure(now);
        retry.record_failure(now);
        assert_eq!(retry.attempts(), 3);

        // 設定は何も変わっていないのに再度要求された状況
        retry.request("デバイス A");

        assert_eq!(retry.attempts(), 3, "失敗回数が巻き戻らない");
        assert!(!retry.is_due(now), "待ち時間も巻き戻らない");
        assert!(
            !retry.is_due(now + Duration::from_millis(799)),
            "3 回失敗したので 800ms 待ち続ける"
        );
        assert!(retry.is_due(now + Duration::from_millis(800)));
    }

    #[test]
    fn connect_retry_request_with_a_different_target_retries_immediately() {
        // 設定画面でデバイスを変えたときは、前の対象のバックオフを引きずらない
        let now = Instant::now();
        let mut retry = ConnectRetry::default();
        retry.request("デバイス A");
        retry.record_failure(now);
        retry.record_failure(now);
        assert!(!retry.is_due(now));

        retry.request("デバイス B");

        assert_eq!(retry.attempts(), 0, "対象が変われば数え直す");
        assert!(retry.is_due(now), "すぐ試す");
    }

    #[test]
    fn connect_retry_request_now_ignores_the_backoff() {
        // 右クリックの「デバイス再接続」。同じ対象でも待たずに試す
        let now = Instant::now();
        let mut retry = ConnectRetry::default();
        retry.request("デバイス A");
        retry.record_failure(now);
        retry.record_failure(now);
        assert!(!retry.is_due(now));

        retry.request_now("デバイス A");

        assert_eq!(retry.attempts(), 0);
        assert!(retry.is_due(now));
    }

    #[test]
    fn connect_retry_request_after_success_starts_a_new_cycle() {
        // 一度成功した対象をもう一度要求したら、また試しにいくこと
        let now = Instant::now();
        let mut retry = ConnectRetry::default();
        retry.request("デバイス A");
        retry.record_success(now);
        assert!(!retry.is_due(now));

        retry.request("デバイス A");
        // 成功からの下限（1 秒）を過ぎた時点で見る。下限そのものは別のテストで見る
        assert!(
            retry.is_due(now + Duration::from_secs(1)),
            "成功済みでも要求されたら試す"
        );
    }

    #[test]
    fn connect_retry_new_is_not_due() {
        // 接続を要求していない間は毎フレームの判定を素通りする
        let retry = ConnectRetry::<&str>::default();
        assert!(!retry.is_due(Instant::now()));
    }

    #[test]
    fn connect_retry_request_is_due_immediately() {
        // 固定 2 秒待ちの廃止そのもの。要求した時点で試せること
        let mut retry = ConnectRetry::default();
        retry.request("デバイス A");
        assert!(retry.is_due(Instant::now()));
    }

    #[test]
    fn connect_retry_failure_blocks_until_the_backoff_elapses() {
        let now = Instant::now();
        let mut retry = ConnectRetry::default();
        retry.request("デバイス A");
        retry.record_failure(now);

        assert!(!retry.is_due(now), "失敗直後は待つ");
        assert!(
            !retry.is_due(now + Duration::from_millis(199)),
            "200ms の手前ではまだ待つ"
        );
        assert!(
            retry.is_due(now + Duration::from_millis(200)),
            "200ms 経てば試す"
        );
    }

    #[test]
    fn connect_retry_consecutive_failures_widen_the_interval() {
        let now = Instant::now();
        let mut retry = ConnectRetry::default();
        retry.request("デバイス A");

        retry.record_failure(now);
        assert!(!retry.is_due(now + Duration::from_millis(199)));

        retry.record_failure(now);
        assert!(
            !retry.is_due(now + Duration::from_millis(399)),
            "2 回目の失敗では 400ms 待つ"
        );
        assert!(retry.is_due(now + Duration::from_millis(400)));
    }

    #[test]
    fn connect_retry_success_stops_further_attempts() {
        let now = Instant::now();
        let mut retry = ConnectRetry::default();
        retry.request("デバイス A");
        retry.record_failure(now);
        retry.record_success(now);

        assert!(
            !retry.is_due(now + Duration::from_secs(60)),
            "成功したら次のフレーム以降は試さない"
        );
    }

    #[test]
    fn connect_retry_success_resets_the_interval() {
        // 一度成功してから再び要求したときに、前回の attempts を引きずらないこと
        let now = Instant::now();
        let mut retry = ConnectRetry::default();
        retry.request("デバイス A");
        for _ in 0..8 {
            retry.record_failure(now);
        }
        retry.record_success(now);

        // 成功からの下限（1 秒）を過ぎてから失敗させ、バックオフだけを見る
        let later = now + Duration::from_secs(1);
        retry.request("デバイス A");
        retry.record_failure(later);
        assert!(
            retry.is_due(later + Duration::from_millis(200)),
            "再要求後は 200ms から数え直す"
        );
    }

    #[test]
    fn connect_retry_new_is_not_active() {
        // 要求していない状態を「再接続中」と表示しないこと
        let retry = ConnectRetry::<&str>::default();
        assert!(!retry.is_active());
    }

    #[test]
    fn connect_retry_is_active_until_it_succeeds() {
        let now = Instant::now();
        let mut retry = ConnectRetry::default();
        retry.request("デバイス A");
        assert!(retry.is_active());

        retry.record_failure(now);
        // 失敗しても追いかけ続けている間は「再接続中」
        assert!(retry.is_active());

        retry.record_success(now);
        assert!(!retry.is_active());
    }

    #[test]
    fn min_interval_elapsed_without_success_returns_true() {
        // まだ一度も繋がっていない。初回の接続に下限は掛けない
        assert!(min_interval_elapsed(None, Instant::now()));
    }

    #[test]
    fn min_interval_elapsed_just_before_the_floor_returns_false() {
        let connected_at = Instant::now();
        assert!(!min_interval_elapsed(Some(connected_at), connected_at));
        assert!(!min_interval_elapsed(
            Some(connected_at),
            connected_at + Duration::from_millis(999)
        ));
    }

    #[test]
    fn min_interval_elapsed_exactly_at_the_floor_returns_true() {
        // 境界。下限ちょうどでは試す
        let connected_at = Instant::now();
        assert!(min_interval_elapsed(
            Some(connected_at),
            connected_at + Duration::from_secs(1)
        ));
    }

    #[test]
    fn min_interval_elapsed_with_time_before_success_returns_false() {
        // 呼び出し側の時刻が成功より前でもパニックしないこと
        let now = Instant::now();
        let connected_at = now + Duration::from_millis(10);
        assert!(!min_interval_elapsed(Some(connected_at), now));
    }

    #[test]
    fn connect_retry_loss_right_after_success_waits_for_the_floor() {
        // #232 の再現。開けた直後に切断を検出して `request_now` で積んでも、
        // 成功から 1 秒経つまでは開き直さない
        let now = Instant::now();
        let mut retry = ConnectRetry::default();
        retry.request("デバイス A");
        retry.record_success(now);

        let lost_at = now + Duration::from_millis(50);
        retry.request_now("デバイス A");

        assert!(retry.is_active(), "要求は積まれている");
        assert!(!retry.is_due(lost_at), "成功直後は開き直さない");
        assert!(
            !retry.is_due(now + Duration::from_millis(999)),
            "下限の手前ではまだ待つ"
        );
        assert!(
            retry.is_due(now + Duration::from_secs(1)),
            "成功から 1 秒経てば開き直す"
        );
    }

    #[test]
    fn connect_retry_repeated_success_and_loss_is_throttled_to_the_floor() {
        // 成功 → 即切断 → 成功 → 即切断 の繰り返し。バックオフは失敗でしか
        // 伸びないので、下限が無いと待ち無しで回り続ける
        let start = Instant::now();
        let mut retry = ConnectRetry::default();
        retry.request("デバイス A");

        let mut now = start;
        let mut opened = 0;
        // 3 秒ぶんを 10ms 刻みで回し、開いた回数を数える
        while now < start + Duration::from_secs(3) {
            if retry.is_due(now) {
                opened += 1;
                retry.record_success(now);
                // 開いた直後に切断を検出する
                retry.request_now("デバイス A");
            }
            now += Duration::from_millis(10);
        }

        // 0 秒、1 秒、2 秒の 3 回だけ
        assert_eq!(opened, 3);
    }

    #[test]
    fn connect_retry_floor_and_backoff_both_apply() {
        // 下限を過ぎていても、失敗のバックオフが残っていれば待つ
        let now = Instant::now();
        let mut retry = ConnectRetry::default();
        retry.request("デバイス A");
        retry.record_success(now);

        let failed_at = now + Duration::from_millis(900);
        retry.request_now("デバイス A");
        retry.record_failure(failed_at);

        assert!(
            !retry.is_due(now + Duration::from_secs(1)),
            "下限は過ぎたがバックオフ（200ms）が残っている"
        );
        assert!(retry.is_due(failed_at + Duration::from_millis(200)));
    }
}
