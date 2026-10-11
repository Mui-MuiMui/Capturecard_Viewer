//! 1 フレームの処理（`update`）の前後。描画より前に済ませる取り込みと起動直後の
//! 処理（`begin_frame`）と、描画を終えたあとの書き出しと次の再描画の予約
//! （`end_frame`）。
//!
//! **呼び出しの順に意味がある**ので、それぞれの理由を各行のコメントに残してある。
//! 描画の本体と、その間に挟まるダイアログの処理は `app/mod.rs` の `update` に置く。
//! 状態は `app/mod.rs` の `CaptureCardViewer` が持ち、ここは `impl` を足すだけ。

use super::{hotkeys, CaptureCardViewer};
use crate::repaint::{next_repaint_delay, should_wake_on_event, RepaintCondition};
use eframe::egui;
use log::info;
use std::time::Instant;

impl CaptureCardViewer {
    /// 描画より前に済ませる処理。別スレッドから届いた結果の取り込みと、
    /// 起動直後に 1 度だけ行う処理。
    pub(super) fn begin_frame(&mut self, ctx: &egui::Context) {
        // 再描画の窓口を Context と結びつける。2 回目以降は何もしない。
        // **デバイスを開くより先に済ませること。** 開いたあとだと、
        // 最初のフレームの到着を知らせる先が無い
        self.repaint_waker.bind(ctx);

        // ワーカーから届いた結果（接続の成否、デバイス能力、デバイス一覧）を
        // 取り込む。設定ダイアログを開いていなくても受け取る
        self.drain_device_events();

        // デバイスワーカーの観測値を 1 回だけ読む。以降の描画や
        // 「接続状態」タブはここから引く（ロックを取り直さない）。
        // **イベントを取り込んだ後に読む。** ワーカーはイベントを送る前に
        // 観測値を書き出すので、この順なら少なくともそのイベントの時点の値が入る
        self.refresh_device_snapshot();

        // 別スレッドで行ったスクリーンショットの保存結果を取り込む。
        // 失敗はここでトーストになる
        self.drain_screenshot_results();
        // 別スレッドで読み込んだ効果音を取り込む。テスト再生はここで鳴る
        self.drain_sound_results();
        // 録画スレッドから届いた結果（開始・保存・失敗）を取り込む
        self.drain_recording_events();
        // 別スレッドで行った更新の確認の結果を取り込む
        self.drain_update_results();
        // 別スレッドで行っている更新（ダウンロードと差し替え）の進み具合を取り込む。
        // 差し替えが済んでいればここでウィンドウを閉じる
        self.drain_update_apply_results(ctx);

        // 起動直後に 1 度だけ行う処理。
        //
        // 以前はここで「起動から 2 秒」待ってからデバイスを開いていた。
        // 待つ根拠がコードにもコミットにも残っておらず、実測でも接続自体は
        // 0.1 秒で終わるため、最初のフレームで要求する。開けなかった場合は
        // ワーカー側のバックオフが繋がるまで面倒を見る
        if !self.startup_applied {
            self.startup_applied = true;
            info!("起動直後の設定適用とデバイスの接続を始める");
            self.apply_settings(true);

            // 最前面表示。設定を取り込んだあとに適用する
            self.apply_startup_window_level(ctx);

            // 前回の更新で残った `.old` / `.new` を消す（別スレッド）
            self.clean_up_update_leftovers();
            // 更新の確認は別スレッドで行うので、ネットワークが無くても起動は待たない
            self.check_for_updates_on_startup();
        }
    }

    /// 描画を終えたあとの処理。一時表示のオーバーレイ、設定の書き出し、
    /// 次の再描画の予約と、ホットキーのリスナーへのウィンドウの状態の受け渡し。
    ///
    /// `viewport` はこのフレームの描画の前に読んだもの。
    pub(super) fn end_frame(&mut self, ctx: &egui::Context, viewport: &egui::ViewportInfo) {
        // 一時表示のオーバーレイ（フルスクリーン切替・音量）。
        // 期限が来れば自分で消え、消える時刻の再描画も自分で予約する
        self.transient_overlay.draw(ctx, Instant::now());

        // 保留中の設定変更を、操作が落ち着いたところでまとめて書き出す
        self.flush_settings_if_due(ctx);

        // 最小化しているか。Windows では egui-winit が毎フレーム入れてくれる。
        // 取れない環境では「最小化していない」に倒す（描きすぎる側は安全）
        let minimized = viewport.minimized.unwrap_or(false);

        // 次の update() をいつ呼ぶかを、このフレームの状態から 1 か所で決める。
        //
        // **ここは上限であって下限ではない。** もっと早く起きたい処理
        // （OSD の消滅、設定の書き出し、フレームの到着）はそれぞれ自分で
        // 予約しており、egui は同じフレームで要求された中の最短を採る
        let condition = RepaintCondition { minimized };
        // 最小化していなければ、別スレッドからの通知（映像フレームの到着を含む）で
        // 起こしてもらう
        self.repaint_waker
            .set_enabled(should_wake_on_event(condition));

        // 最小化しているかをホットキーのリスナーへ伝える。最小化すると
        // ここが呼ばれなくなるので、**最後に書いた値がそのまま残る**のが狙い。
        // リスナーは真の間だけ、画面の要らないアクションをワーカーへ回す（#133）。
        // フォーカスは「フォーカスがあるときだけ反応する」の判定に使う（#202）。
        // 出入りのたびに egui-winit が再描画を要求するので、ここで拾える。
        // 取れない環境では「フォーカスあり」に倒す（反応しなくなる側に倒さない）
        let focused = viewport.focused.unwrap_or(true);
        // テキスト欄に入力中かも伝える。キーを奪わないので、プリセット名などへ
        // 打った文字がホットキーとしても実行されてしまう（#206）。
        // 見るのはテキスト欄のフォーカスだけで、ボタンなどのフォーカスは数えない（#238）。
        // **描画を全て終えたここで読む。** このフレームでフォーカスが移った分まで
        // 反映される。フックの中から egui へは問い合わせない
        let typing = hotkeys::is_typing_in_text_field(ctx);
        self.hotkey_manager
            .set_window_state(minimized, focused, typing);
        ctx.request_repaint_after(next_repaint_delay(condition));
    }
}
