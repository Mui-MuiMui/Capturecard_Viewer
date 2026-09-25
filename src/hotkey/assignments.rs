use super::action::folded_repeats;
use super::parse::parse_hotkey;
use super::{HotkeyAction, HotkeyError, HotkeyManager};
use crate::keyboard_hook::KeyChord;
use log::{debug, error, info, warn};
use std::collections::BTreeMap;

/// アクションに割り当てたキーを登録できなかった理由。
///
/// `hotkey` を一緒に持つのは、同じアクションでもキーが変われば別の失敗として
/// 扱うため。設定画面へそのまま出す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotkeyAssignmentError {
    /// 登録しようとしたホットキー文字列
    pub hotkey: String,
    /// 画面に出す理由
    pub reason: HotkeyError,
}

impl HotkeyManager {
    /// 設定のホットキー割り当てを実際の登録へ反映する。
    ///
    /// **差分だけを処理する。** 2 秒ごとの再適用で呼ばれるため、無条件に
    /// 登録し直すとその瞬間のキー入力を取りこぼす。
    ///
    /// 登録できなかったアクションは `registered` に入らないので、次に呼ばれた
    /// ときに再試行する。ログは理由が変わったときだけ出す（同じ失敗が
    /// 2 秒ごとに積もらないように）。
    pub fn apply(&mut self, desired: &BTreeMap<HotkeyAction, String>) {
        // 一時停止中は何もしない。ホットキー入力ダイアログを開いている間に
        // 2 秒ごとの再適用が割り込むと、解除したはずのキーが登録し直されてしまう
        if self.paused {
            return;
        }

        // リスナーが途中で止まっていたら、フックを使えないのと同じ扱いにする。
        // 登録済みのものを外しておけば、下の登録で理由付きの失敗として記録され、
        // トーストと設定画面に出る
        self.take_listener_failure();

        // 解除するのは、割り当てが消えたアクションとキーが変わったアクション
        let stale: Vec<HotkeyAction> = self
            .registered
            .iter()
            .filter(|(action, (hotkey, _))| desired.get(action) != Some(hotkey))
            .map(|(action, _)| *action)
            .collect();
        for action in stale {
            self.unregister(action);
        }

        // 失敗の記録も、対象のキーが変わったら捨てる。残すと設定画面に
        // 解消済みの理由が出続ける
        self.errors
            .retain(|action, error| desired.get(action) == Some(&error.hotkey));

        for (action, hotkey) in desired {
            if self.registered.contains_key(action) {
                continue;
            }
            self.register(*action, hotkey);
        }
    }

    /// 保留している押下を取り出す。
    ///
    /// 毎フレーム UI スレッドから呼ばれる。返す順序はアクションの宣言順で、
    /// 同じフレームに複数届いても並び順は変わらない。
    ///
    /// **デバウンスはリスナー側で済んでいる**（`ListenerState::record_press`）。
    /// ここで行うのは、最小化している間に溜まった押下を何回ぶん実行するかの
    /// 判断だけで、判断そのものは `folded_repeats` が持つ。最小化していない
    /// 間はアクションごとに高々 1 回しか溜まらないので、畳んでも結果は変わらない。
    pub fn take_pressed(&mut self) -> Vec<HotkeyAction> {
        let pressed: BTreeMap<HotkeyAction, u32> = match self.state.lock() {
            Ok(mut state) => std::mem::take(&mut state.pressed),
            Err(_) => {
                // ここが失敗するのはロックが毒されたときだけで、毎フレーム呼ばれる。
                // release ビルドは panic = "abort" なので毒されること自体が起きない
                warn!("ホットキーの押下確認で共有状態のロックを取得できない");
                return Vec::new();
            }
        };

        let mut fired = Vec::new();
        for (action, presses) in pressed {
            let repeats = folded_repeats(action, presses);
            if repeats < presses {
                debug!(
                    "{} の押下 {} 回を {} 回へ畳んだ",
                    action.label(),
                    presses,
                    repeats
                );
            }
            for _ in 0..repeats {
                debug!("{} をホットキーから実行する", action.label());
                fired.push(action);
            }
        }
        fired
    }

    /// 登録できなかったアクションと、その理由。設定画面に出す。
    ///
    /// 直っていない間は毎回の `apply` で試し直しているので、ここに残っている
    /// のは「いまも登録できていないもの」だけ。
    pub fn errors(&self) -> &BTreeMap<HotkeyAction, HotkeyAssignmentError> {
        &self.errors
    }

    /// 登録中のホットキーをすべて一時解除する。ホットキー入力ダイアログを開くときに使う。
    ///
    /// 解除しないと、割り当て済みのキーを押して付け直そうとしたときに、
    /// そのアクションまで実行されてしまう。**リスナースレッドとフックは
    /// 止めない**（止めると再開が重くなるうえ、アプリ全体で 1 本という前提が
    /// 崩れる）。照合に使う表を空にするだけ。
    ///
    /// 既に一時停止中なら何もしない。二重に呼んでも安全にしておくことで、
    /// 呼び出し側でダイアログの開閉検出が多少ずれても壊れない。
    pub fn pause(&mut self) {
        if self.paused {
            return;
        }
        self.paused = true;

        let actions: Vec<HotkeyAction> = self.registered.keys().copied().collect();
        for action in actions {
            self.unregister(action);
        }
        info!("ホットキー入力ダイアログのためホットキーを一時解除した");
    }

    /// 一時停止を終え、`desired` の内容で登録し直す。
    ///
    /// 一時停止していなければ何もしない（`pause` を呼んでいないのに解除中の
    /// キーが無いのに登録し直そうとする、という状況を防ぐ）。
    pub fn resume(&mut self, desired: &BTreeMap<HotkeyAction, String>) {
        if !self.paused {
            return;
        }
        self.paused = false;
        info!("ホットキーの一時解除を終える");
        self.apply(desired);
    }

    /// 候補のホットキーを登録できるか確かめる。
    ///
    /// ホットキー入力ダイアログでキーが確定したときに使う。見るのは
    /// 「解釈できるか」と「キーボードフックを使えているか」の 2 つだけで、
    /// ここでは何も登録しない。実際に使い続けるための登録は、この呼び出しの
    /// あとに行う `resume` が行う。
    ///
    /// **他のアプリとの競合は起きない。** キーを奪わずに観測するだけなので、
    /// 他のアプリが同じキーを使っていても両方が反応する（#202）。
    pub fn try_register(&self, hotkey_str: &str) -> Result<(), HotkeyError> {
        parse_hotkey(hotkey_str)?;
        match &self.hook_error {
            Some(e) => Err(HotkeyError::HookUnavailable(e.clone())),
            None => Ok(()),
        }
    }

    /// 1 つのアクションにホットキーを登録する。失敗は `errors` に記録する。
    fn register(&mut self, action: HotkeyAction, hotkey_str: &str) {
        let hotkey = match parse_hotkey(hotkey_str) {
            Ok(hotkey) => hotkey,
            Err(e) => {
                self.record_error(action, hotkey_str, e);
                return;
            }
        };

        // 同じキーを 2 つのアクションへ割り当てると、どちらの押下なのか
        // 区別できない。設定画面でも警告するが、設定ファイルを手で
        // 書き換えられる前提でここでも弾く。先に登録したほう（宣言順で先の
        // アクション）を残す
        if let Some(other) = self.action_for_chord(hotkey) {
            self.record_error(
                action,
                hotkey_str,
                HotkeyError::DuplicateAssignment { other },
            );
            return;
        }

        // フックが無ければ押下を観測できない。表に載せても効かないので、
        // 理由を残して設定画面とトーストに出す
        if let Some(e) = &self.hook_error {
            let reason = HotkeyError::HookUnavailable(e.clone());
            self.record_error(action, hotkey_str, reason);
            return;
        }

        // リスナーが照合に使う組み合わせを足す
        match self.state.lock() {
            Ok(mut state) => {
                state.registered.insert(hotkey, action);
            }
            Err(_) => warn!("ホットキーの共有状態のロックを取得できない"),
        }

        self.registered
            .insert(action, (hotkey_str.to_string(), hotkey));
        self.errors.remove(&action);
        info!("{} に {} を割り当てた", action.label(), hotkey_str);
    }

    /// 1 つのアクションの登録を解除する。登録していなければ何もしない。
    pub(super) fn unregister(&mut self, action: HotkeyAction) {
        let Some((hotkey_str, hotkey)) = self.registered.remove(&action) else {
            return;
        };

        // 照合に使う組み合わせを消す。**同じロックの中で保留中の押下も落とす。**
        // 別々のロックで行うと、クリアした直後のフレームで 1 回だけ実行される
        match self.state.lock() {
            Ok(mut state) => {
                state.registered.remove(&hotkey);
                state.pressed.remove(&action);
            }
            Err(_) => warn!("ホットキーの共有状態のロックを取得できない"),
        }

        info!("{} の {} の割り当てを解除した", action.label(), hotkey_str);
    }

    /// リスナーが途中で止まっていたら、理由を `hook_error` へ移し、
    /// 登録済みのものを外す。止まっていなければ何もしない。
    ///
    /// 外したものは続く登録で `HookUnavailable` として記録し直される。
    /// 一度移したら `hook_error` が埋まるので、以降は毎回の登録がそこで失敗する。
    fn take_listener_failure(&mut self) {
        if self.hook_error.is_some() {
            return;
        }
        let failure = match self.state.lock() {
            Ok(mut state) => state.listener_failure.take(),
            Err(_) => {
                warn!(
                    "ホットキーの共有状態のロックを取得できないのでリスナーの停止を確かめられない"
                );
                None
            }
        };
        let Some(failure) = failure else {
            return;
        };

        self.hook_error = Some(failure);
        let actions: Vec<HotkeyAction> = self.registered.keys().copied().collect();
        for action in actions {
            self.unregister(action);
        }
    }

    /// その組み合わせを既に使っているアクション。
    fn action_for_chord(&self, chord: KeyChord) -> Option<HotkeyAction> {
        self.registered
            .iter()
            .find(|(_, (_, hotkey))| *hotkey == chord)
            .map(|(action, _)| *action)
    }

    /// 失敗を記録する。同じ理由が続く間はログに出さない。
    ///
    /// 登録に失敗したアクションは 2 秒ごとの再適用で試し直すため、毎回
    /// ログへ書くと同じ行が延々と積もる。
    fn record_error(&mut self, action: HotkeyAction, hotkey: &str, reason: HotkeyError) {
        let error = HotkeyAssignmentError {
            hotkey: hotkey.to_string(),
            reason,
        };
        if self.errors.get(&action) != Some(&error) {
            error!(
                "{} に {} を割り当てられない: {}",
                action.label(),
                error.hotkey,
                error.reason
            );
        }
        self.errors.insert(action, error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyboard_hook::{KeyboardHookError, Modifiers};

    // テストで使う仮想キーコード。英字は ASCII の大文字と同じ値
    const VK_S: u32 = 0x53;

    // ---- 登録の差分処理 ----
    //
    // HotkeyManager::new は実際にキーボードフックを登録するため、CI では
    // 成功しないことがある。ここで確かめるのは「解除と押下の扱い」だけにし、
    // フックの登録の成否には依存しないテストにしてある。

    fn assignments(pairs: &[(HotkeyAction, &str)]) -> BTreeMap<HotkeyAction, String> {
        pairs
            .iter()
            .map(|(action, key)| (*action, (*key).to_string()))
            .collect()
    }

    #[test]
    fn apply_empty_assignment_registers_nothing() {
        let mut manager = HotkeyManager::new();

        manager.apply(&BTreeMap::new());

        assert!(manager.registered.is_empty());
        assert!(manager.errors.is_empty());
    }

    #[test]
    fn apply_unparsable_hotkey_records_an_error() {
        // 設定ファイルを手で書き換えた場合。登録へ進まずに理由を残す
        let mut manager = HotkeyManager::new();

        manager.apply(&assignments(&[(HotkeyAction::Screenshot, "Ctrl+Shift")]));

        assert!(!manager.registered.contains_key(&HotkeyAction::Screenshot));
        let error = manager
            .errors
            .get(&HotkeyAction::Screenshot)
            .expect("理由が残ること");
        assert_eq!(error.hotkey, "Ctrl+Shift");
    }

    #[test]
    fn apply_clearing_an_assignment_drops_the_error() {
        // 直せないキーを入れたあとにクリアしたら、設定画面から理由も消える
        let mut manager = HotkeyManager::new();
        manager.apply(&assignments(&[(HotkeyAction::Screenshot, "Ctrl+Shift")]));
        assert!(manager.errors.contains_key(&HotkeyAction::Screenshot));

        manager.apply(&BTreeMap::new());

        assert!(manager.errors.is_empty());
    }

    #[test]
    fn apply_changing_the_key_drops_the_old_error() {
        // 別のキーへ変えた時点で、前のキーの理由は意味を失う
        let mut manager = HotkeyManager::new();
        manager.apply(&assignments(&[(HotkeyAction::Screenshot, "Ctrl+Shift")]));

        manager.apply(&assignments(&[(HotkeyAction::Screenshot, "Alt+Shift")]));

        let error = manager
            .errors
            .get(&HotkeyAction::Screenshot)
            .expect("新しいキーの理由が残ること");
        assert_eq!(error.hotkey, "Alt+Shift");
    }

    #[test]
    fn unregister_drops_pending_press() {
        // 解除の直前に届いた押下を残すと、クリアした直後に 1 回だけ実行される。
        // 登録の成否に依存しないよう、共有状態を直接組み立てて確かめる
        let mut manager = HotkeyManager::new();
        let hotkey = parse_hotkey("F5").expect("F5 は解析できる");
        manager
            .registered
            .insert(HotkeyAction::Screenshot, ("F5".to_string(), hotkey));
        {
            let mut state = manager.state.lock().expect("ロックが毒されていないこと");
            state.registered.insert(hotkey, HotkeyAction::Screenshot);
            state.pressed.insert(HotkeyAction::Screenshot, 1);
        }

        manager.unregister(HotkeyAction::Screenshot);

        let state = manager.state.lock().expect("ロックが毒されていないこと");
        assert!(state.pressed.is_empty(), "保留中の押下が残っている");
        assert!(
            state.registered.is_empty(),
            "登録中の組み合わせが残っている"
        );
    }

    #[test]
    fn unregister_keeps_other_actions_pressed() {
        // 1 つのアクションを解除しても、他のアクションの押下は捨てない
        let mut manager = HotkeyManager::new();
        let hotkey = parse_hotkey("F5").expect("F5 は解析できる");
        manager
            .registered
            .insert(HotkeyAction::Screenshot, ("F5".to_string(), hotkey));
        {
            let mut state = manager.state.lock().expect("ロックが毒されていないこと");
            state.pressed.insert(HotkeyAction::Screenshot, 1);
            state.pressed.insert(HotkeyAction::VolumeUp, 1);
        }

        manager.unregister(HotkeyAction::Screenshot);

        let state = manager.state.lock().expect("ロックが毒されていないこと");
        assert_eq!(
            state.pressed.keys().copied().collect::<Vec<_>>(),
            vec![HotkeyAction::VolumeUp]
        );
    }

    #[test]
    fn take_pressed_returns_actions_in_declaration_order() {
        let mut manager = HotkeyManager::new();
        {
            let mut state = manager.state.lock().expect("ロックが毒されていないこと");
            state.pressed.insert(HotkeyAction::VolumeDown, 1);
            state.pressed.insert(HotkeyAction::Screenshot, 1);
            state.pressed.insert(HotkeyAction::ReconnectDevices, 1);
        }

        let fired = manager.take_pressed();

        assert_eq!(
            fired,
            vec![
                HotkeyAction::Screenshot,
                HotkeyAction::ReconnectDevices,
                HotkeyAction::VolumeDown,
            ]
        );
    }

    #[test]
    fn take_pressed_consumes_the_press() {
        let mut manager = HotkeyManager::new();
        manager
            .state
            .lock()
            .expect("ロックが毒されていないこと")
            .pressed
            .insert(HotkeyAction::Screenshot, 1);

        assert_eq!(manager.take_pressed(), vec![HotkeyAction::Screenshot]);
        assert!(manager.take_pressed().is_empty());
    }

    #[test]
    fn take_pressed_folds_repeated_presses() {
        // 最小化している間に溜まった押下。トグルは偶数回なら実行しない
        let mut manager = HotkeyManager::new();
        {
            let mut state = manager.state.lock().expect("ロックが毒されていないこと");
            state.pressed.insert(HotkeyAction::ToggleFullscreen, 2);
            state.pressed.insert(HotkeyAction::ToggleAlwaysOnTop, 3);
            state.pressed.insert(HotkeyAction::Screenshot, 4);
        }

        assert_eq!(
            manager.take_pressed(),
            vec![HotkeyAction::Screenshot, HotkeyAction::ToggleAlwaysOnTop,]
        );
    }

    // ---- 一時停止と再開 ----

    #[test]
    fn pause_clears_currently_registered_actions() {
        // 実際の登録成否に依存せず、bookkeeping だけを直接組み立てて確かめる
        let mut manager = HotkeyManager::new();
        let hotkey = parse_hotkey("F5").expect("F5 は解析できる");
        manager
            .registered
            .insert(HotkeyAction::Screenshot, ("F5".to_string(), hotkey));
        manager
            .state
            .lock()
            .expect("ロックが毒されていないこと")
            .registered
            .insert(hotkey, HotkeyAction::Screenshot);

        manager.pause();

        assert!(manager.registered.is_empty());
        assert!(manager
            .state
            .lock()
            .expect("ロックが毒されていないこと")
            .registered
            .is_empty());
        assert!(manager.paused);
    }

    #[test]
    fn pause_twice_is_a_no_op() {
        let mut manager = HotkeyManager::new();
        manager.pause();
        manager.pause();

        assert!(manager.paused);
    }

    #[test]
    fn apply_while_paused_does_nothing() {
        // 一時停止中に 2 秒ごとの再適用が割り込んでも、登録し直されないこと
        let mut manager = HotkeyManager::new();
        manager.pause();

        manager.apply(&assignments(&[(HotkeyAction::Screenshot, "Ctrl+Shift")]));

        assert!(manager.registered.is_empty());
        assert!(
            manager.errors.is_empty(),
            "一時停止中は apply が何もしないこと"
        );
    }

    #[test]
    fn resume_without_pause_does_nothing() {
        // pause を呼んでいない状態で resume しても apply は走らない
        let mut manager = HotkeyManager::new();

        manager.resume(&assignments(&[(HotkeyAction::Screenshot, "Ctrl+Shift")]));

        assert!(manager.errors.is_empty());
    }

    #[test]
    fn resume_after_pause_applies_the_given_assignments() {
        // 解析に失敗するキーを使い、実際の OS 登録に依存せず「apply が走ったこと」
        // だけを確かめる（登録そのものの成否は他のテストと同じ理由で見ない）
        let mut manager = HotkeyManager::new();
        manager.pause();

        manager.resume(&assignments(&[(HotkeyAction::Screenshot, "Ctrl+Shift")]));

        assert!(!manager.paused);
        let error = manager
            .errors
            .get(&HotkeyAction::Screenshot)
            .expect("再開後は apply が走り、失敗が記録されること");
        assert_eq!(error.hotkey, "Ctrl+Shift");
    }

    // ---- 試し登録 ----

    #[test]
    fn try_register_unparsable_hotkey_returns_the_parse_error() {
        // 解釈できない理由をそのまま返す。フックの有無より先に見る
        let manager = HotkeyManager::new();

        assert_eq!(
            manager.try_register("Ctrl+Shift"),
            Err(HotkeyError::MissingKey)
        );
    }

    #[test]
    fn try_register_does_not_register_anything() {
        // 確かめるだけで、照合の表には載せない。載せるのは resume の apply
        let manager = HotkeyManager::new();

        let _ = manager.try_register("F5");

        assert!(manager.registered.is_empty());
        assert!(manager
            .state
            .lock()
            .expect("ロックが毒されていないこと")
            .registered
            .is_empty());
    }

    #[test]
    fn try_register_without_hook_reports_why() {
        // フックを使えない環境では、どのキーを選んでも効かない。
        // ダイアログを閉じさせず、理由を出させる
        let mut manager = HotkeyManager::new();
        manager.hook_error = Some(KeyboardHookError::Unsupported);

        assert_eq!(
            manager.try_register("F5"),
            Err(HotkeyError::HookUnavailable(KeyboardHookError::Unsupported))
        );
    }

    #[test]
    fn apply_without_hook_records_the_reason() {
        // 表に載せても押下を観測できないので、失敗として設定画面へ出す
        let mut manager = HotkeyManager::new();
        manager.hook_error = Some(KeyboardHookError::Unsupported);

        manager.apply(&assignments(&[(HotkeyAction::Screenshot, "F5")]));

        assert!(!manager.registered.contains_key(&HotkeyAction::Screenshot));
        assert_eq!(
            manager
                .errors
                .get(&HotkeyAction::Screenshot)
                .map(|error| &error.reason),
            Some(&HotkeyError::HookUnavailable(
                KeyboardHookError::Unsupported
            ))
        );
    }

    #[test]
    fn apply_after_listener_failure_reports_every_assignment() {
        // 動いていたリスナーが止まったら、登録済みのものも含めて失敗として
        // 記録し直す。記録しないと、効かないのに何も表示されない
        let mut manager = HotkeyManager::new();
        manager.hook_error = None;
        let desired = assignments(&[(HotkeyAction::Screenshot, "F5")]);
        manager.apply(&desired);
        assert!(manager.registered.contains_key(&HotkeyAction::Screenshot));

        let failure = KeyboardHookError::WaitFailed("failed".to_string());
        manager
            .state
            .lock()
            .expect("ロックが毒されていないこと")
            .listener_failure = Some(failure.clone());
        manager.apply(&desired);

        assert!(manager.registered.is_empty());
        assert!(manager
            .state
            .lock()
            .expect("ロックが毒されていないこと")
            .registered
            .is_empty());
        assert_eq!(
            manager
                .errors
                .get(&HotkeyAction::Screenshot)
                .map(|error| &error.reason),
            Some(&HotkeyError::HookUnavailable(failure))
        );
    }

    #[test]
    fn apply_with_hook_registers_the_chord_for_the_listener() {
        // フックが使えていれば、リスナーが照合する表に組み合わせが載る
        let mut manager = HotkeyManager::new();
        manager.hook_error = None;

        manager.apply(&assignments(&[(HotkeyAction::Screenshot, "Ctrl+S")]));

        let chord = KeyChord {
            modifiers: Modifiers::CONTROL,
            vk: VK_S,
        };
        assert_eq!(
            manager
                .state
                .lock()
                .expect("ロックが毒されていないこと")
                .registered
                .get(&chord),
            Some(&HotkeyAction::Screenshot)
        );
        assert!(manager.errors.is_empty());
    }

    #[test]
    fn apply_duplicate_chord_keeps_the_first_action() {
        // 同じ組み合わせを 2 つのアクションへ割り当てたら、宣言順で先のほうを残す
        let mut manager = HotkeyManager::new();
        manager.hook_error = None;

        manager.apply(&assignments(&[
            (HotkeyAction::Screenshot, "F5"),
            (HotkeyAction::VolumeUp, "f5"),
        ]));

        assert!(manager.registered.contains_key(&HotkeyAction::Screenshot));
        assert_eq!(
            manager
                .errors
                .get(&HotkeyAction::VolumeUp)
                .map(|error| &error.reason),
            Some(&HotkeyError::DuplicateAssignment {
                other: HotkeyAction::Screenshot
            })
        );
    }
}
