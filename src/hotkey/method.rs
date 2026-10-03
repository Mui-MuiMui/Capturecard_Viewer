//! 押下を受け取る方式の切り替えと、キーを奪う方式（`RegisterHotKey`）の登録（#207）。
//!
//! 方式の違いと、どちらでも同じに効くもの（フォーカス・入力中の判定、
//! デバウンス、最小化中の扱い）は `docs/design/hotkeys.md` の「キーを奪う方式」。

use super::listener_thread::HotkeyMethod;
use super::{HotkeyAction, HotkeyError, HotkeyManager};
use crate::keyboard_hook::{KeyChord, KeyboardHookError};
use log::{info, warn};

/// 方式を切り替えたあとの「押下を受け取れない理由」を決める。
///
/// - リスナーが止まっている（`WaitFailed` / `ListenerStopped`）なら、方式に
///   関係なく受け取れないので前の理由を残す
/// - フックの方式なら、フックを登録できなかった理由（`install_error`）
/// - キーを奪う方式ならフックは使わないので `None`。登録の失敗はキーごとに
///   `HotkeyError::RegisterFailed` として記録する
fn hook_error_after_switch(
    previous: Option<KeyboardHookError>,
    method: HotkeyMethod,
    install_error: Option<KeyboardHookError>,
) -> Option<KeyboardHookError> {
    if previous.as_ref().is_some_and(is_listener_gone) {
        return previous;
    }
    match method {
        HotkeyMethod::Hook => install_error,
        HotkeyMethod::RegisterHotKey => None,
    }
}

/// リスナーが止まっていて、どちらの方式でも押下を受け取れない理由か。
fn is_listener_gone(error: &KeyboardHookError) -> bool {
    matches!(
        error,
        KeyboardHookError::WaitFailed(_) | KeyboardHookError::ListenerStopped
    )
}

impl HotkeyManager {
    /// 押下を受け取る方式を切り替える。
    ///
    /// 設定の反映（`apply_settings`）のたびに、`apply_hotkey_assignments` より
    /// 先に呼ばれる。同じ方式なら何もしない。
    ///
    /// 切り替えるときは登録済みのものを全て外し、リスナーに方式を切り替えさせる
    /// （フックの付け外しと `RegisterHotKey` の解除）。登録し直すのは続く `apply`。
    /// 一時停止中（ホットキー入力ダイアログ）は何も登録していないので、方式だけ
    /// 切り替わり、登録は `resume` の `apply` で行われる。
    pub fn set_method(&mut self, method: HotkeyMethod) {
        if method == self.method {
            return;
        }

        let actions: Vec<HotkeyAction> = self.registered.keys().copied().collect();
        for action in actions {
            self.unregister(action);
        }
        let previous = self.method;
        self.method = method;
        // リスナー側の RegisterHotKey の登録も、方式の切り替えで全て外れる
        self.system_dirty = false;

        let install_error = match self.listener.sync(method, Vec::new()) {
            Ok(outcome) => outcome.hook_error,
            Err(e) => {
                warn!("ホットキーのリスナーに方式を切り替えさせられない: {}", e);
                self.hook_error = Some(e);
                None
            }
        };
        self.hook_error = hook_error_after_switch(self.hook_error.take(), method, install_error);
        info!(
            "ホットキーの方式を {} から {} に切り替えた",
            previous.log_name(),
            method.log_name()
        );
    }

    /// キーを奪う方式のとき、照合の表（`registered`）を `RegisterHotKey` の
    /// 登録へ反映する。
    ///
    /// 登録できなかったもの（他のアプリが登録済みなど）は表から外し、
    /// `HotkeyError::RegisterFailed` として記録する。表から外したアクションは
    /// 次の `apply` で登録し直すので、他のアプリがキーを手放せば効くようになる。
    ///
    /// 表を変えていなければ何もしない（2 秒ごとの再適用でリスナーへ問い合わせない）。
    pub(super) fn sync_system_hotkeys(&mut self) {
        if self.method != HotkeyMethod::RegisterHotKey || !self.system_dirty {
            return;
        }
        // リスナーが止まっているなら、表には何も載っていない（`register` が弾く）
        if self.hook_error.is_some() {
            self.system_dirty = false;
            return;
        }

        let chords: Vec<KeyChord> = self.registered.values().map(|(_, chord)| *chord).collect();
        match self.listener.sync(self.method, chords) {
            Ok(outcome) => {
                for chord in outcome.registered {
                    let Some(action) = self.action_for_chord(chord) else {
                        continue;
                    };
                    // 登録できたので、前に失敗していた理由は消す
                    self.errors.remove(&action);
                    if let Some((hotkey, _)) = self.registered.get(&action) {
                        info!(
                            "{} に {} を割り当てた（RegisterHotKey で登録した）",
                            action.label(),
                            hotkey
                        );
                    }
                }
                for (chord, reason) in outcome.failed {
                    self.drop_registration(chord, HotkeyError::RegisterFailed(reason));
                }
            }
            Err(e) => {
                // リスナーが止まっている。表の全てが効かない
                self.hook_error = Some(e.clone());
                let chords: Vec<KeyChord> =
                    self.registered.values().map(|(_, chord)| *chord).collect();
                for chord in chords {
                    self.drop_registration(chord, HotkeyError::HookUnavailable(e.clone()));
                }
            }
        }
        // 外したものはリスナーにも登録されていないので、揃っている
        self.system_dirty = false;
    }

    /// 候補のキーを、キーを奪う方式で登録できるか試す。フックの方式では何もしない。
    ///
    /// いまの登録に候補を足してリスナーへ渡し、結果を見てから元の登録へ戻す。
    /// ホットキー入力ダイアログを開いている間（一時停止中）は登録が空なので、
    /// 候補だけを登録して外すことになる。
    pub(super) fn probe_system_hotkey(&self, candidate: KeyChord) -> Result<(), HotkeyError> {
        if self.method != HotkeyMethod::RegisterHotKey {
            return Ok(());
        }
        let current: Vec<KeyChord> = self.registered.values().map(|(_, chord)| *chord).collect();
        let mut probe = current.clone();
        if !probe.contains(&candidate) {
            probe.push(candidate);
        }

        let result = match self.listener.sync(self.method, probe) {
            Ok(outcome) => match outcome
                .failed
                .into_iter()
                .find(|(chord, _)| *chord == candidate)
            {
                Some((_, reason)) => Err(HotkeyError::RegisterFailed(reason)),
                None => Ok(()),
            },
            Err(e) => Err(HotkeyError::HookUnavailable(e)),
        };
        // 試した登録を外す。失敗しても次の `sync_system_hotkeys` で揃う
        if let Err(e) = self.listener.sync(self.method, current) {
            warn!("試しに登録したホットキーを外せない: {}", e);
        }
        result
    }

    /// 表から組み合わせを外し、理由を失敗として記録する。
    fn drop_registration(&mut self, chord: KeyChord, reason: HotkeyError) {
        let Some(action) = self.action_for_chord(chord) else {
            return;
        };
        let Some((hotkey, _)) = self.registered.get(&action).cloned() else {
            return;
        };
        self.unregister(action);
        self.record_error(action, &hotkey, reason);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn assignments(pairs: &[(HotkeyAction, &str)]) -> BTreeMap<HotkeyAction, String> {
        pairs
            .iter()
            .map(|(action, key)| (*action, (*key).to_string()))
            .collect()
    }

    // ---- 切り替えたあとの理由 ----

    #[test]
    fn hook_error_after_switch_to_hook_is_the_install_result() {
        let install = KeyboardHookError::InstallFailed("denied".to_string());
        assert_eq!(
            hook_error_after_switch(None, HotkeyMethod::Hook, Some(install.clone())),
            Some(install)
        );
        assert_eq!(
            hook_error_after_switch(None, HotkeyMethod::Hook, None),
            None
        );
    }

    #[test]
    fn hook_error_after_switch_to_register_hotkey_forgets_the_hook_failure() {
        // フックを登録できない環境でも、キーを奪う方式なら使える
        let previous = KeyboardHookError::InstallFailed("denied".to_string());
        assert_eq!(
            hook_error_after_switch(Some(previous), HotkeyMethod::RegisterHotKey, None),
            None
        );
    }

    #[test]
    fn hook_error_after_switch_keeps_a_stopped_listener() {
        // リスナーが止まっていれば、どちらの方式でも押下を受け取れない
        for previous in [
            KeyboardHookError::WaitFailed("failed".to_string()),
            KeyboardHookError::ListenerStopped,
        ] {
            for method in [HotkeyMethod::Hook, HotkeyMethod::RegisterHotKey] {
                assert_eq!(
                    hook_error_after_switch(Some(previous.clone()), method, None),
                    Some(previous.clone()),
                    "{method:?} へ切り替えたら {previous:?} が消えた"
                );
            }
        }
    }

    // ---- 切り替えと登録 ----
    //
    // RegisterHotKey は CI の環境でも呼べるが、同じキーを他のプロセスが
    // 登録していれば失敗する。登録の成否に依存しないよう、「成功したら表に載り、
    // 失敗したら理由が残る」ことだけを確かめる

    #[test]
    fn set_method_with_the_same_method_keeps_registrations() {
        // 2 秒ごとの再適用で毎回呼ばれる。外して登録し直すと押下を取りこぼす
        let mut manager = HotkeyManager::new();
        manager.hook_error = None;
        manager.apply(&assignments(&[(HotkeyAction::Screenshot, "F5")]));

        manager.set_method(HotkeyMethod::Hook);

        assert!(manager.registered.contains_key(&HotkeyAction::Screenshot));
    }

    #[test]
    fn set_method_switch_unregisters_and_apply_registers_again() {
        let mut manager = HotkeyManager::new();
        manager.hook_error = None;
        let desired = assignments(&[(HotkeyAction::Screenshot, "Ctrl+Alt+Shift+F8")]);
        manager.apply(&desired);
        assert!(manager.registered.contains_key(&HotkeyAction::Screenshot));

        manager.set_method(HotkeyMethod::RegisterHotKey);

        assert_eq!(manager.method, HotkeyMethod::RegisterHotKey);
        assert!(manager.registered.is_empty(), "切り替えで外れること");
        assert!(manager
            .state
            .lock()
            .expect("ロックが毒されていないこと")
            .registered
            .is_empty());

        manager.apply(&desired);

        // 登録できたなら表に載り、できなければ理由が残る。どちらか一方だけ
        let registered = manager.registered.contains_key(&HotkeyAction::Screenshot);
        let failed = matches!(
            manager
                .errors
                .get(&HotkeyAction::Screenshot)
                .map(|error| &error.reason),
            Some(HotkeyError::RegisterFailed(_))
        );
        assert!(registered != failed, "登録 {registered} / 失敗 {failed}");
        assert_eq!(
            manager
                .state
                .lock()
                .expect("ロックが毒されていないこと")
                .registered
                .len(),
            usize::from(registered),
            "照合の表は登録できたものだけを持つこと"
        );
        assert!(!manager.system_dirty);

        // 戻すと RegisterHotKey の登録は外れ、フックの方式で登録し直される
        manager.set_method(HotkeyMethod::Hook);
        manager.hook_error = None;
        manager.apply(&desired);
        assert!(manager.registered.contains_key(&HotkeyAction::Screenshot));
    }

    #[test]
    fn register_hotkey_conflict_is_reported_as_register_failed() {
        // 他が登録済みのキーは、キーを奪う方式では登録できない。同じプロセスの
        // 別のスレッド（別の HotkeyManager のリスナー）が登録していても同じ
        let desired = assignments(&[(HotkeyAction::VolumeUp, "Ctrl+Alt+Shift+F7")]);
        let mut first = HotkeyManager::new();
        first.set_method(HotkeyMethod::RegisterHotKey);
        first.apply(&desired);
        if !first.registered.contains_key(&HotkeyAction::VolumeUp) {
            // 他のプロセスが先に使っている環境。競合は確かめられない
            return;
        }

        let mut second = HotkeyManager::new();
        second.set_method(HotkeyMethod::RegisterHotKey);
        second.apply(&desired);

        assert!(!second.registered.contains_key(&HotkeyAction::VolumeUp));
        assert!(matches!(
            second
                .errors
                .get(&HotkeyAction::VolumeUp)
                .map(|error| &error.reason),
            Some(HotkeyError::RegisterFailed(_))
        ));
        // 試し登録でも同じ理由が返る
        assert!(matches!(
            second.try_register("Ctrl+Alt+Shift+F7"),
            Err(HotkeyError::RegisterFailed(_))
        ));

        // 先の登録が外れたら、次の再適用で登録できる
        drop(first);
        second.apply(&desired);
        assert!(second.registered.contains_key(&HotkeyAction::VolumeUp));
        assert!(second.errors.is_empty());
    }

    #[test]
    fn probe_system_hotkey_with_the_hook_does_nothing() {
        // フックの方式ではキーを奪わないので、他のアプリとの競合は起きない
        let manager = HotkeyManager::new();
        let chord = KeyChord {
            modifiers: crate::keyboard_hook::Modifiers::empty(),
            vk: 0x74,
        };

        assert_eq!(manager.probe_system_hotkey(chord), Ok(()));
    }
}
