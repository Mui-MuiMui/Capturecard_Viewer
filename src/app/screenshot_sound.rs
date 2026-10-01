//! スクリーンショットの効果音ファイルの読み込みと再生。
//!
//! 適用（`apply_settings`）と設定画面の「テスト再生」の両方の読み込みを、
//! 要求ごとに起こすスレッドで行う。再生も 1 回ごとのスレッドで行う。
//! UI スレッドは結果をチャネルで受け取り、反映と失敗の報告だけをする。
//! スクリーンショットの保存スレッドと同じ流儀（`docs/design/threads.md`）。
//!
//! 大きなファイルや遅いドライブでは、読み込みとデコードの確認に時間がかかる。
//! UI スレッドで行うと、適用やテスト再生の瞬間に描画が止まる（Issue #214）。

use super::screenshot::drop_finished_threads;
use super::CaptureCardViewer;
use crate::screenshot::ScreenshotError;
use crate::screenshot_sound;
use crate::status::ErrorSource;
use log::{debug, warn};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// 効果音のスレッドから UI スレッドへ返すもの。
pub(super) enum SoundMessage {
    /// 読み込みの結果（適用・テスト再生）
    Loaded(SoundLoadResult),
    /// 再生スレッドが出力先を開けたか。開けなかったときは理由
    Output(Result<(), ScreenshotError>),
}

/// 読み込みスレッドから UI スレッドへ返す結果。
///
/// **失敗しても鳴らせるデータは必ず入っている。** `load_sound_data` は
/// 見つからない・読めないファイルを内蔵音へ倒し、デコードできないファイルは
/// 中身をそのまま返すため。`error` は報告するためだけのもの。
pub(super) struct SoundLoadResult {
    purpose: SoundLoadPurpose,
    data: Arc<[u8]>,
    error: Option<ScreenshotError>,
}

/// 何のための読み込みか。受け取った側の扱いがまったく違う。
enum SoundLoadPurpose {
    /// 撮影時に鳴らす音として反映する。`id` は `ScreenshotManager::begin_load` の番号
    Apply { id: u64 },
    /// その場で 1 回鳴らす。適用済みの音には触れない。
    /// `volume` はボタンを押した時点のドラフトの音量
    TestPlay { id: u64, volume: f32 },
}

impl CaptureCardViewer {
    /// 撮影時に鳴らす効果音として `path` の読み込みを始める。
    ///
    /// 結果が届くまでは直前の音（無ければ内蔵音）で鳴る。続けて呼ぶと、
    /// 先の読み込みの結果は届いても捨てられる。
    pub(super) fn request_sound_apply(&mut self, path: &Path) {
        let Some(id) = self
            .screenshot_manager
            .lock()
            .ok()
            .map(|mut ss| ss.begin_load())
        else {
            warn!("効果音の適用で screenshot_manager のロックを取得できない");
            return;
        };
        self.spawn_sound_load(path.to_path_buf(), SoundLoadPurpose::Apply { id });
    }

    /// 設定画面の「テスト再生」の読み込みを始める。読み込めたら `volume` で鳴らす。
    pub(super) fn request_test_sound(&mut self, path: &Path, volume: f32) {
        let Some(id) = self
            .screenshot_manager
            .lock()
            .ok()
            .map(|mut ss| ss.begin_test_play())
        else {
            warn!("テスト再生で screenshot_manager のロックを取得できない");
            return;
        };
        self.spawn_sound_load(
            path.to_path_buf(),
            SoundLoadPurpose::TestPlay { id, volume },
        );
    }

    fn spawn_sound_load(&mut self, path: PathBuf, purpose: SoundLoadPurpose) {
        let result_tx = self.sound_tx.clone();
        // 映像が止まっている間は update() の間隔が広がっているので、届いたら起こす。
        // 起こさないとテスト再生の音が次の再描画まで鳴らない
        let waker = self.repaint_waker.clone();
        let handle = std::thread::spawn(move || {
            let (data, error) = screenshot_sound::load_sound_data(&path);
            // ログは受け取った UI スレッド側で出す（保存スレッドと同じ）
            if result_tx
                .send(SoundMessage::Loaded(SoundLoadResult {
                    purpose,
                    data,
                    error,
                }))
                .is_err()
            {
                // 受信側が無いのはアプリが終了したときだけ。結果は捨ててよい
                debug!("効果音の読み込み結果の送り先が既に無いので捨てる");
                return;
            }
            waker.wake();
        });

        // ハンドルを持っておき、終了時に join する。溜め込まないよう、
        // 積む前に終わった分を落とす
        drop_finished_threads(&mut self.sound_load_threads);
        self.sound_load_threads.push(handle);
    }

    /// 効果音を別スレッドで 1 回鳴らす。撮影とテスト再生の両方がここを通る。
    ///
    /// 出力先を開けたかは `SoundMessage::Output` で UI スレッドへ返る。
    /// **`screenshot_manager` のロックを握ったまま呼ばない。** 渡すのは `Arc` の複製だけ。
    pub(super) fn play_sound(&self, data: Arc<[u8]>, volume: f32) {
        let tx = self.sound_tx.clone();
        let waker = self.repaint_waker.clone();
        screenshot_sound::play_sound_data(data, volume, move |outcome| {
            let failed = outcome.is_err();
            // 受信側が無いのはアプリが終了したときだけ。結果は捨ててよい
            if tx.send(SoundMessage::Output(outcome)).is_ok() && failed {
                // 映像が止まっている間でも、通知がすぐ出るよう起こす
                waker.wake();
            }
        });
    }

    /// 別スレッドから届いた効果音の読み込みと再生の結果を取り込む。`update()` の先頭で呼ぶ。
    pub(super) fn drain_sound_results(&mut self) {
        while let Ok(message) = self.sound_rx.try_recv() {
            match message {
                SoundMessage::Loaded(result) => self.apply_sound_load_result(result),
                SoundMessage::Output(outcome) => self.apply_sound_output(outcome),
            }
        }
    }

    /// 再生スレッドが出力先を開けたかを取り込む。
    ///
    /// **同じ理由が続く間は 1 度だけ報告する。** 撮影のたびに鳴らそうとするので、
    /// 出力デバイスが無いまま撮り続けると同じ失敗が撮影の回数だけ届く。
    /// 一定時間ごとの再通知（`ErrorCenter` の間隔）も入れていない。画像は保存できて
    /// いて、撮るたびに思い出させる必要が無いため。理由は「接続状態」タブに残る。
    ///
    /// 出力先を開けるようになったら記録を落とし（タブからも消える）、次に開けなく
    /// なったときはまた知らせる。
    fn apply_sound_output(&mut self, outcome: Result<(), ScreenshotError>) {
        let reason = outcome.err().map(|e| e.to_string());
        let report = should_report_sound_output(self.sound_output_failure.as_deref(), &reason);
        match &reason {
            Some(reason) if report => {
                warn!("効果音を鳴らせない: {}", reason);
                self.report_error(ErrorSource::ScreenshotSound, reason.clone());
            }
            Some(reason) => debug!("効果音を鳴らせない（報告済み）: {}", reason),
            None if self.sound_output_failure.is_some() => {
                self.errors.clear(ErrorSource::ScreenshotSound);
            }
            None => {}
        }
        self.sound_output_failure = reason;
    }

    fn apply_sound_load_result(&mut self, result: SoundLoadResult) {
        let SoundLoadResult {
            purpose,
            data,
            error,
        } = result;

        match purpose {
            SoundLoadPurpose::Apply { id } => {
                // report_error は self 全体を借りるので、ロックはここで離す
                let accepted = match self.screenshot_manager.lock() {
                    Ok(mut ss) => ss.finish_load(id, data),
                    Err(_) => {
                        warn!("効果音の反映で screenshot_manager のロックを取得できない");
                        false
                    }
                };
                if !accepted {
                    // 後から出した要求があるか、「鳴らさない」へ切り替えられた。
                    // 失敗も報告しない。報告すべきなのは最後の要求の結果だけ
                    debug!("古い効果音の読み込み結果が届いたので捨てる");
                    return;
                }
                let Some(e) = error else {
                    // 読み込めたので、前に読めなかった記録を取り下げる。ただし
                    // 出力先を開けない失敗が続いている間は、その記録を消さない
                    if self.sound_output_failure.is_none() {
                        self.errors.clear(ErrorSource::ScreenshotSound);
                    }
                    return;
                };
                warn!("効果音の適用: {}", e);
                // ファイルがあるのに読めなかった場合は、次の適用タイミングで
                // 読み直す。デコードできないファイルは読み直しても変わらないので
                // 適用済みのまま（撮影時は無音。docs/design/assets.md）
                if !matches!(e, ScreenshotError::SoundFileUndecodable { .. }) {
                    self.last_sound_file = None;
                }
                self.report_error(ErrorSource::ScreenshotSound, e.to_string());
            }
            SoundLoadPurpose::TestPlay { id, volume } => {
                let accepted = match self.screenshot_manager.lock() {
                    Ok(mut ss) => ss.finish_test_play(id),
                    Err(_) => {
                        warn!("テスト再生で screenshot_manager のロックを取得できない");
                        false
                    }
                };
                if !accepted {
                    debug!("後からテスト再生が押されたので、古い読み込み結果は鳴らさない");
                    return;
                }
                // 読めなければ内蔵音で鳴らし、理由をトーストへ出す
                if let Some(e) = error {
                    warn!("テスト再生で効果音を読み込めない: {}", e);
                    self.report_error(ErrorSource::ScreenshotSound, e.to_string());
                }
                self.play_sound(data, volume);
            }
        }
    }

    /// 進行中の効果音の読み込みがすべて終わるまで待つ。終了時に呼ぶ。
    ///
    /// 結果は取り込まない。終了中に反映しても撮影は起きず、テスト再生を
    /// 鳴らしても閉じる途中で途切れるだけのため。
    pub(super) fn join_sound_load_threads(&mut self) {
        let handles = std::mem::take(&mut self.sound_load_threads);
        if handles.is_empty() {
            return;
        }

        debug!("効果音の読み込みスレッド {} 件を待つ", handles.len());
        for handle in handles {
            if handle.join().is_err() {
                // release ビルドは panic = "abort" なのでここには来ない
                warn!("効果音の読み込みスレッドがパニックした");
            }
        }
    }
}

/// 再生スレッドの結果を報告するかを決める。
///
/// `previous` は前回の再生で開けなかった理由（開けていれば `None`）、
/// `current` は今回の結果。開けなかったうえで、前回と違う理由のときだけ報告する。
fn should_report_sound_output(previous: Option<&str>, current: &Option<String>) -> bool {
    match current {
        Some(reason) => previous != Some(reason.as_str()),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_report_sound_output_first_failure_is_reported() {
        assert!(should_report_sound_output(None, &Some("NoDevice".into())));
    }

    #[test]
    fn should_report_sound_output_same_failure_again_is_not_reported() {
        // 出力デバイスが無いまま撮り続けた場合。撮影のたびにトーストを出さない
        assert!(!should_report_sound_output(
            Some("NoDevice"),
            &Some("NoDevice".into())
        ));
    }

    #[test]
    fn should_report_sound_output_different_failure_is_reported() {
        assert!(should_report_sound_output(
            Some("NoDevice"),
            &Some("Busy".into())
        ));
    }

    #[test]
    fn should_report_sound_output_success_is_never_reported() {
        // 開けたときは報告しない。記録は落とすので、次に開けなくなったらまた知らせる
        assert!(!should_report_sound_output(Some("NoDevice"), &None));
        assert!(!should_report_sound_output(None, &None));
    }
}
