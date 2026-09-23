//! 低レベルキーボードフック（`WH_KEYBOARD_LL`）による押下の観測。
//!
//! ホットキーのリスナースレッド（`crate::hotkey`）の中でだけ使う。
//! **フックは見るだけで、キーを奪わない。** 受け取ったキーは必ず
//! `CallNextHookEx` で次へ渡すので、フォアグラウンドのアプリにもそのまま届く
//! （#202、`docs/design/hotkeys.md` の「キーを奪わない」）。
//!
//! フックのコールバックは、システム全体のキー入力がこのプロセスを通るたびに
//! 呼ばれる。**ここが遅れると他のアプリの入力まで遅れる**ため、コールバックの
//! 中ではロックもアロケーションもしない。行うのは「押下か」「修飾キーか」
//! 「キーリピートか」の判定と、自分のスレッドへのメッセージの投函だけで、
//! アクションとの照合はメッセージを受け取った側（`KeyboardHook::pump` を
//! 呼んだリスナー）が行う。

use std::fmt;
use std::ops::{BitOr, BitOrAssign};
use std::time::Duration;

/// 修飾キーの組み合わせ。
///
/// 左右の区別はしない（左 Ctrl でも右 Ctrl でも `CONTROL`）。
/// `RegisterHotKey` を使っていたころと同じ扱い。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub(crate) struct Modifiers(u8);

impl Modifiers {
    pub(crate) const CONTROL: Self = Self(1);
    pub(crate) const ALT: Self = Self(1 << 1);
    pub(crate) const SHIFT: Self = Self(1 << 2);
    pub(crate) const SUPER: Self = Self(1 << 3);

    pub(crate) const fn empty() -> Self {
        Self(0)
    }

    /// メッセージの `LPARAM` へ載せるための値。
    #[cfg_attr(not(windows), allow(dead_code))]
    const fn bits(self) -> u8 {
        self.0
    }

    /// メッセージの `LPARAM` から戻す。知らないビットは落とす。
    #[cfg_attr(not(windows), allow(dead_code))]
    const fn from_bits(bits: u8) -> Self {
        let known = Self::CONTROL.0 | Self::ALT.0 | Self::SHIFT.0 | Self::SUPER.0;
        Self(bits & known)
    }
}

impl BitOr for Modifiers {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for Modifiers {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// 修飾キーと通常キー（仮想キーコード）の組。ホットキー 1 つぶん。
///
/// **修飾キーは完全一致で比べる。** `F5` に割り当てたときに `Ctrl+F5` では
/// 反応しない。`RegisterHotKey` を使っていたころと同じ判定にしてある。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct KeyChord {
    pub(crate) modifiers: Modifiers,
    /// Win32 の仮想キーコード（`VK_F5` など）
    pub(crate) vk: u32,
}

/// フックを使えない理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KeyboardHookError {
    /// Windows 以外では使えない。Windows 版では作られない
    #[cfg_attr(windows, allow(dead_code))]
    Unsupported,
    /// `SetWindowsHookExW` が失敗した。OS のエラー文を持つ
    InstallFailed(String),
    /// リスナースレッドが起動の結果を返さないまま終わった
    ListenerStopped,
    /// 動いていたリスナーがキー入力を待てなくなって終わった。OS のエラー文を持つ
    WaitFailed(String),
}

impl fmt::Display for KeyboardHookError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyboardHookError::Unsupported => write!(f, "この OS には対応していません"),
            KeyboardHookError::InstallFailed(source) => {
                write!(f, "キーボードフックを登録できません: {source}")
            }
            KeyboardHookError::ListenerStopped => {
                write!(f, "ホットキーのリスナースレッドが起動しませんでした")
            }
            KeyboardHookError::WaitFailed(source) => write!(
                f,
                "キー入力を待てなくなったのでホットキーを止めました。アプリを再起動してください: {source}"
            ),
        }
    }
}

impl std::error::Error for KeyboardHookError {}

// Win32 の値。判定を純粋関数にしてテストから呼べるよう、winapi に頼らず
// ここに持つ（winapi は Windows のときだけの依存）。
const WM_KEYDOWN: u32 = 0x0100;
const WM_SYSKEYDOWN: u32 = 0x0104;
const VK_SHIFT: u32 = 0x10;
const VK_CONTROL: u32 = 0x11;
const VK_MENU: u32 = 0x12;
const VK_LWIN: u32 = 0x5B;
const VK_RWIN: u32 = 0x5C;
const VK_LSHIFT: u32 = 0xA0;
const VK_RMENU: u32 = 0xA5;

/// フックへ届いたメッセージが押下か。
///
/// Alt を押している間の押下は `WM_SYSKEYDOWN` で届くので両方を見る。
/// 解放（`WM_KEYUP` / `WM_SYSKEYUP`）は数えない。
fn is_key_down_message(message: u32) -> bool {
    message == WM_KEYDOWN || message == WM_SYSKEYDOWN
}

/// 修飾キーそのものの押下か。
///
/// 修飾キーは組み合わせの一部として押下時の状態から読むので、単独の押下と
/// しては扱わない。左右を区別する `VK_LSHIFT`〜`VK_RMENU`（0xA0〜0xA5）も、
/// フックには左右付きの値で届くことがあるので含める。
fn is_modifier_key(vk: u32) -> bool {
    matches!(vk, VK_SHIFT | VK_CONTROL | VK_MENU | VK_LWIN | VK_RWIN)
        || (VK_LSHIFT..=VK_RMENU).contains(&vk)
}

/// 押されている修飾キーから組み合わせを作る。
fn modifiers_from_state(control: bool, alt: bool, shift: bool, win: bool) -> Modifiers {
    let mut modifiers = Modifiers::empty();
    if control {
        modifiers |= Modifiers::CONTROL;
    }
    if alt {
        modifiers |= Modifiers::ALT;
    }
    if shift {
        modifiers |= Modifiers::SHIFT;
    }
    if win {
        modifiers |= Modifiers::SUPER;
    }
    modifiers
}

/// 登録中の低レベルキーボードフック。
///
/// **登録したスレッドでしか使えない。** フックのコールバックは登録した
/// スレッドがメッセージを取り出すときに、そのスレッドの上で呼ばれる。
/// 中身が生のハンドルなので `Send` ではなく、別のスレッドへは渡せない。
/// 落とすとフックを外す。
pub(crate) struct KeyboardHook {
    inner: imp::Hook,
}

impl KeyboardHook {
    /// 呼んだスレッドにフックを登録する。
    pub(crate) fn install() -> Result<Self, KeyboardHookError> {
        imp::Hook::install().map(|inner| Self { inner })
    }

    /// 最大 `timeout` だけメッセージを待ち、届いていたものを処理する。
    ///
    /// フックのコールバックはこの中で呼ばれる。観測した押下は
    /// `on_key_down` へ 1 回ずつ渡す。**`on_key_down` の間は次のキー入力の
    /// コールバックが待たされる**（他のアプリの入力も待たされる）ので、
    /// ロックを長く握ったりブロックしたりしないこと。
    ///
    /// 待てなかった（OS の呼び出しが失敗した）ときは `false` を返す。
    pub(crate) fn pump(&self, timeout: Duration, on_key_down: impl FnMut(KeyChord)) -> bool {
        self.inner.pump(timeout, on_key_down)
    }
}

#[cfg(windows)]
mod imp {
    use super::{
        is_key_down_message, is_modifier_key, modifiers_from_state, KeyChord, KeyboardHookError,
        Modifiers,
    };
    use std::time::Duration;
    use winapi::ctypes::c_int;
    use winapi::shared::minwindef::{FALSE, LPARAM, LRESULT, UINT, WPARAM};
    use winapi::shared::windef::HHOOK;
    use winapi::um::libloaderapi::GetModuleHandleW;
    use winapi::um::processthreadsapi::GetCurrentThreadId;
    use winapi::um::winbase::WAIT_FAILED;
    use winapi::um::winuser::{
        CallNextHookEx, GetAsyncKeyState, MsgWaitForMultipleObjects, PeekMessageW,
        PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx, HC_ACTION, KBDLLHOOKSTRUCT,
        MSG, PM_NOREMOVE, PM_REMOVE, QS_ALLINPUT, WH_KEYBOARD_LL, WM_APP,
    };

    /// フックが自分のスレッドへ投函する「押下を観測した」メッセージ。
    /// `WPARAM` に仮想キーコード、`LPARAM` に `Modifiers` のビットを載せる。
    const WM_KEY_OBSERVED: UINT = WM_APP + 0x0202;

    /// いま押されているか。`GetAsyncKeyState` の最上位ビットを見る。
    ///
    /// **低レベルフックの中では、処理中のキーの状態はまだ更新されていない。**
    /// そのため処理中のキーについて真なら「前から押されていた」、つまり
    /// キーリピートと分かる。修飾キーは先に押されているので、組み合わせの
    /// 判定にはそのまま使える。
    fn is_down(vk: u32) -> bool {
        // SAFETY: 引数の範囲に制約のない読み取りだけの呼び出し
        unsafe { GetAsyncKeyState(vk as c_int) < 0 }
    }

    /// `WH_KEYBOARD_LL` のコールバック。
    ///
    /// **ロックもアロケーションもしない。** 判定して投函したら、何があっても
    /// `CallNextHookEx` へ渡す。非ゼロを返すとキーを握りつぶすことになるので、
    /// 戻り値は必ず次のフックのものをそのまま返す。
    unsafe extern "system" fn low_level_keyboard_proc(
        code: c_int,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if code == HC_ACTION && is_key_down_message(wparam as u32) {
            // SAFETY: HC_ACTION のとき lparam は KBDLLHOOKSTRUCT を指す
            let vk = (*(lparam as *const KBDLLHOOKSTRUCT)).vkCode;
            // 押しっぱなしのキーリピートは数えない（RegisterHotKey の
            // MOD_NOREPEAT と同じ）。デバウンスだけに任せると、押している
            // 間 200ms ごとに実行されてしまう
            if !is_modifier_key(vk) && !is_down(vk) {
                let modifiers = modifiers_from_state(
                    is_down(super::VK_CONTROL),
                    is_down(super::VK_MENU),
                    is_down(super::VK_SHIFT),
                    is_down(super::VK_LWIN) || is_down(super::VK_RWIN),
                );
                // 自分のスレッドのキューへ積むだけ。失敗（キューが一杯）しても
                // このキーの押下が 1 回落ちるだけなので、ここでは何もしない
                PostThreadMessageW(
                    GetCurrentThreadId(),
                    WM_KEY_OBSERVED,
                    vk as WPARAM,
                    LPARAM::from(modifiers.bits()),
                );
            }
        }
        CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
    }

    pub(super) struct Hook {
        handle: HHOOK,
    }

    impl Hook {
        pub(super) fn install() -> Result<Self, KeyboardHookError> {
            // SAFETY: どれも呼んだスレッドのメッセージキューとフックを扱うだけ
            unsafe {
                // 先にこのスレッドのメッセージキューを作っておく。フックが
                // 投函する先が無いと、最初の押下を取りこぼしうる
                let mut msg: MSG = std::mem::zeroed();
                PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_NOREMOVE);

                let module = GetModuleHandleW(std::ptr::null());
                let handle =
                    SetWindowsHookExW(WH_KEYBOARD_LL, Some(low_level_keyboard_proc), module, 0);
                if handle.is_null() {
                    return Err(KeyboardHookError::InstallFailed(
                        std::io::Error::last_os_error().to_string(),
                    ));
                }
                Ok(Self { handle })
            }
        }

        pub(super) fn pump(
            &self,
            timeout: Duration,
            mut on_key_down: impl FnMut(KeyChord),
        ) -> bool {
            // INFINITE（u32::MAX）にならないよう 1 つ手前で止める
            let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
            // SAFETY: ハンドルを渡さない待機と、このスレッドのキューの読み出しだけ
            unsafe {
                // QS_ALLINPUT には送られたメッセージ（QS_SENDMESSAGE）も含まれる。
                // フックの呼び出しはそれとして届くので、キー入力があれば
                // タイムアウトを待たずに起きる
                let woke =
                    MsgWaitForMultipleObjects(0, std::ptr::null(), FALSE, millis, QS_ALLINPUT);
                if woke == WAIT_FAILED {
                    return false;
                }

                // フックのコールバックは PeekMessageW の中で呼ばれ、そこで
                // 投函された分も同じループで取り出す。全部取り出してから
                // 待ちに戻らないと、残った分で次の待ちが起きなくなる
                let mut msg: MSG = std::mem::zeroed();
                while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                    if msg.hwnd.is_null() && msg.message == WM_KEY_OBSERVED {
                        on_key_down(KeyChord {
                            modifiers: Modifiers::from_bits(msg.lParam as u8),
                            vk: msg.wParam as u32,
                        });
                    }
                }
            }
            true
        }
    }

    impl Drop for Hook {
        fn drop(&mut self) {
            // SAFETY: install が返したハンドルを 1 度だけ外す
            unsafe {
                UnhookWindowsHookEx(self.handle);
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::{KeyChord, KeyboardHookError};
    use std::time::Duration;

    /// Windows 以外では作れない。値が存在しないので `pump` は呼ばれない。
    pub(super) enum Hook {}

    impl Hook {
        pub(super) fn install() -> Result<Self, KeyboardHookError> {
            Err(KeyboardHookError::Unsupported)
        }

        pub(super) fn pump(&self, _timeout: Duration, _on_key_down: impl FnMut(KeyChord)) -> bool {
            match *self {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_key_down_message_accepts_both_down_messages() {
        // Alt を押している間の押下は WM_SYSKEYDOWN で届く。見落とすと
        // Alt+F5 のような割り当てが効かない
        assert!(is_key_down_message(WM_KEYDOWN));
        assert!(is_key_down_message(WM_SYSKEYDOWN));
    }

    #[test]
    fn is_key_down_message_ignores_releases() {
        // 解放まで数えると 1 回の操作で 2 回実行される
        const WM_KEYUP: u32 = 0x0101;
        const WM_SYSKEYUP: u32 = 0x0105;
        assert!(!is_key_down_message(WM_KEYUP));
        assert!(!is_key_down_message(WM_SYSKEYUP));
    }

    #[test]
    fn is_modifier_key_covers_generic_and_sided_keys() {
        for vk in [
            VK_SHIFT, VK_CONTROL, VK_MENU, VK_LWIN, VK_RWIN, 0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5,
        ] {
            assert!(is_modifier_key(vk), "修飾キーとして扱われない: {vk:#x}");
        }
    }

    #[test]
    fn is_modifier_key_rejects_ordinary_keys() {
        // F5、A、0、Space、Enter、Escape
        for vk in [0x74, 0x41, 0x30, 0x20, 0x0D, 0x1B] {
            assert!(!is_modifier_key(vk), "通常キーが修飾キー扱い: {vk:#x}");
        }
    }

    #[test]
    fn modifiers_from_state_combines_every_pressed_modifier() {
        assert_eq!(
            modifiers_from_state(false, false, false, false),
            Modifiers::empty()
        );
        assert_eq!(
            modifiers_from_state(true, false, true, false),
            Modifiers::CONTROL | Modifiers::SHIFT
        );
        assert_eq!(
            modifiers_from_state(true, true, true, true),
            Modifiers::CONTROL | Modifiers::ALT | Modifiers::SHIFT | Modifiers::SUPER
        );
    }

    #[test]
    fn modifiers_roundtrip_through_message_bits() {
        // フックからリスナーへはメッセージの LPARAM で渡す
        let all = Modifiers::CONTROL | Modifiers::ALT | Modifiers::SHIFT | Modifiers::SUPER;
        assert_eq!(Modifiers::from_bits(all.bits()), all);
        assert_eq!(Modifiers::from_bits(Modifiers::ALT.bits()), Modifiers::ALT);
    }

    #[test]
    fn modifiers_from_bits_drops_unknown_bits() {
        assert_eq!(Modifiers::from_bits(0xF0), Modifiers::empty());
    }

    #[test]
    fn key_chord_compares_modifiers_exactly() {
        // F5 に割り当てたときに Ctrl+F5 で反応しないこと
        let f5 = KeyChord {
            modifiers: Modifiers::empty(),
            vk: 0x74,
        };
        let ctrl_f5 = KeyChord {
            modifiers: Modifiers::CONTROL,
            vk: 0x74,
        };
        assert_ne!(f5, ctrl_f5);
    }

    #[test]
    fn keyboard_hook_error_display_is_japanese() {
        for error in [
            KeyboardHookError::Unsupported,
            KeyboardHookError::InstallFailed("failed".to_string()),
            KeyboardHookError::ListenerStopped,
            KeyboardHookError::WaitFailed("failed".to_string()),
        ] {
            let text = error.to_string();
            assert!(!text.is_ascii(), "日本語が含まれていない: {text}");
        }
    }
}
