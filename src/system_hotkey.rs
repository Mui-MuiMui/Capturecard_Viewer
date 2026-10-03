//! キーを奪う方式のホットキー（`RegisterHotKey`、#207）。
//!
//! ホットキーのリスナースレッド（`crate::hotkey`）の中でだけ使う。
//! 既定の方式は低レベルキーボードフック（`crate::keyboard_hook`）で、こちらは
//! 「ホットキー」タブの切り替えでオンにしたときだけ使う。
//!
//! **ウィンドウを渡さずに登録する。** 押下は登録したスレッドのキューへ
//! `WM_HOTKEY`（hwnd が null）として届くので、リスナーのメッセージループ
//! （`keyboard_hook::pump_messages`）がそのまま受け取れる。登録と解除は
//! 必ず同じスレッドで行うこと（`UnregisterHotKey` も呼んだスレッドの登録しか
//! 外せない）。
//!
//! 登録した組み合わせはシステム全体で横取りされ、他のアプリには届かない。
//! 他のアプリが同じ組み合わせを登録済みなら失敗する
//! （`docs/design/hotkeys.md` の「キーを奪う方式」）。

use crate::keyboard_hook::{KeyChord, Modifiers};

// `RegisterHotKey` の修飾キーの値。判定を純粋関数にしてテストから呼べるよう、
// ここに持つ（windows クレートの型は Windows のときだけ使える）
const MOD_ALT: u32 = 0x0001;
const MOD_CONTROL: u32 = 0x0002;
const MOD_SHIFT: u32 = 0x0004;
const MOD_WIN: u32 = 0x0008;
const MOD_NOREPEAT: u32 = 0x4000;

/// `RegisterHotKey` に渡す修飾キーの値。
///
/// **`MOD_NOREPEAT` を必ず付ける。** 押しっぱなしのキーリピートでは
/// `WM_HOTKEY` を出さない。フックの方式でキーリピートを数えないのと揃える。
fn register_modifiers(modifiers: Modifiers) -> u32 {
    let mut flags = MOD_NOREPEAT;
    if modifiers.contains(Modifiers::ALT) {
        flags |= MOD_ALT;
    }
    if modifiers.contains(Modifiers::CONTROL) {
        flags |= MOD_CONTROL;
    }
    if modifiers.contains(Modifiers::SHIFT) {
        flags |= MOD_SHIFT;
    }
    if modifiers.contains(Modifiers::SUPER) {
        flags |= MOD_WIN;
    }
    flags
}

/// 登録中のホットキー 1 つ。落とすと登録を外す。
///
/// **登録したスレッドで落とすこと。** 中身は番号だけなので型の上では
/// どこへでも渡せるが、`UnregisterHotKey` は呼んだスレッドの登録しか外せない。
/// リスナースレッドの外へ出さない。
pub(crate) struct SystemHotkey {
    id: i32,
}

impl SystemHotkey {
    /// 呼んだスレッドに `chord` を登録する。`id` はスレッドの中で重ならない
    /// 番号（0x0000〜0xBFFF）。失敗したときは OS のエラー文を返す。
    pub(crate) fn register(id: i32, chord: KeyChord) -> Result<Self, String> {
        imp::register(id, register_modifiers(chord.modifiers), chord.vk)?;
        Ok(Self { id })
    }

    /// 登録の番号。`WM_HOTKEY` の `WPARAM` と同じ値。
    pub(crate) fn id(&self) -> i32 {
        self.id
    }
}

impl Drop for SystemHotkey {
    fn drop(&mut self) {
        imp::unregister(self.id);
    }
}

#[cfg(windows)]
mod imp {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS,
    };

    pub(super) fn register(id: i32, modifiers: u32, vk: u32) -> Result<(), String> {
        // SAFETY: ウィンドウを渡さず、呼んだスレッドへ登録するだけ
        unsafe { RegisterHotKey(None, id, HOT_KEY_MODIFIERS(modifiers), vk) }
            .map_err(|e| e.message())
    }

    pub(super) fn unregister(id: i32) {
        // SAFETY: 呼んだスレッドが登録した番号を外すだけ。外せなくても
        // （既に外れている）困らないので結果は見ない
        let _ = unsafe { UnregisterHotKey(None, id) };
    }
}

#[cfg(not(windows))]
mod imp {
    pub(super) fn register(_id: i32, _modifiers: u32, _vk: u32) -> Result<(), String> {
        Err("RegisterHotKey は Windows でしか使えない".to_string())
    }

    pub(super) fn unregister(_id: i32) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_modifiers_always_suppresses_key_repeat() {
        // 修飾キーなしの F5 でも MOD_NOREPEAT だけは付く
        assert_eq!(register_modifiers(Modifiers::empty()), MOD_NOREPEAT);
    }

    #[test]
    fn register_modifiers_maps_every_modifier() {
        assert_eq!(
            register_modifiers(Modifiers::CONTROL | Modifiers::SHIFT),
            MOD_NOREPEAT | MOD_CONTROL | MOD_SHIFT
        );
        assert_eq!(
            register_modifiers(
                Modifiers::CONTROL | Modifiers::ALT | Modifiers::SHIFT | Modifiers::SUPER
            ),
            MOD_NOREPEAT | MOD_CONTROL | MOD_ALT | MOD_SHIFT | MOD_WIN
        );
    }
}
