//! スクリーンショットの効果音ファイルの読み込み。
//!
//! 適用（`apply_settings`）と設定画面の「テスト再生」の両方の読み込みを、
//! 要求ごとに起こすスレッドで行う。UI スレッドは結果をチャネルで受け取り、
//! 反映と失敗の報告だけをする。スクリーンショットの保存スレッドと同じ流儀
//! （`docs/design/threads.md`）。
//!
//! 大きなファイルや遅いドライブでは、読み込みとデコードの確認に時間がかかる。
//! UI スレッドで行うと、適用やテスト再生の瞬間に描画が止まる（Issue #214）。

use super::screenshot::drop_finished_threads;
use super::CaptureCardViewer;
use crate::screenshot::{self, ScreenshotError};
use crate::status::ErrorSource;
use log::{debug, warn};
use std::path::{Path, PathBuf};

/// 読み込みスレッドから UI スレッドへ返す結果。
///
/// **失敗しても鳴らせるデータは必ず入っている。** `load_sound_data` は
/// 見つからない・読めないファイルを内蔵音へ倒し、デコードできないファイルは
/// 中身をそのまま返すため。`error` は報告するためだけのもの。
pub(super) struct SoundLoadResult {
    purpose: SoundLoadPurpose,
    data: Vec<u8>,
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
        let result_tx = self.sound_load_tx.clone();
        // 映像が止まっている間は update() の間隔が広がっているので、届いたら起こす。
        // 起こさないとテスト再生の音が次の再描画まで鳴らない
        let waker = self.repaint_waker.clone();
        let handle = std::thread::spawn(move || {
            let (data, error) = screenshot::load_sound_data(&path);
            // ログは受け取った UI スレッド側で出す（保存スレッドと同じ）
            if result_tx
                .send(SoundLoadResult {
                    purpose,
                    data,
                    error,
                })
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

    /// 別スレッドから届いた効果音の読み込み結果を取り込む。`update()` の先頭で呼ぶ。
    pub(super) fn drain_sound_load_results(&mut self) {
        while let Ok(result) = self.sound_load_rx.try_recv() {
            self.apply_sound_load_result(result);
        }
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
                    return;
                };
                warn!("効果音の適用: {}", e);
                // ファイルがあるのに読めなかった場合は、次の適用タイミングで
                // 読み直す。デコードできないファイルは読み直しても変わらないので
                // 適用済みのまま（撮影時は無音。docs/design/assets.md）
                if !matches!(e, ScreenshotError::SoundFileUndecodable { .. }) {
                    self.last_sound_file = None;
                }
                self.report_error(ErrorSource::Screenshot, e.to_string());
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
                    self.report_error(ErrorSource::Screenshot, e.to_string());
                }
                screenshot::play_sound_data(data, volume);
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
