//! COM の初期化を RAII で包む。
//!
//! 「このスレッドで COM を使えるようにしておく印」で、落とすと初期化を戻す。
//! DirectShow のバックエンド（`video::directshow`）がデバイスワーカースレッドで使う。
//! `video` の外に置いてあるのは、録画（`docs/design/recording.md`）でも使うため。

use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::System::Com::{
    CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
};

/// このスレッドで COM を使えるようにしておく印。落とすと初期化を戻す。
///
/// **シングルスレッドアパートメント（STA）で初期化する。** 同じワーカー
/// スレッドの上で nokhwa（Media Foundation）と cpal（WASAPI）がどちらも STA で
/// 初期化しており、ここだけ MTA にすると、後から初期化する側が
/// `RPC_E_CHANGED_MODE` で失敗する（nokhwa はそれを起動の失敗として扱う）。
pub struct ComApartment {
    /// `CoUninitialize` で戻す必要があるか。既に別のモデルで初期化されていて
    /// `RPC_E_CHANGED_MODE` が返ったときだけ偽
    initialized: bool,
    /// スレッドに紐づくので、ほかのスレッドへ持ち出させない
    _not_send: std::marker::PhantomData<*mut ()>,
}

impl ComApartment {
    /// このスレッドの COM を初期化する。既に同じモデルで初期化済みでもよい
    /// （回数が数えられるだけで、`Drop` で 1 回戻す）。
    pub fn enter() -> Result<Self, windows::core::Error> {
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
        if hr == RPC_E_CHANGED_MODE {
            // 別のモデル（MTA）で初期化済み。COM は使えるので、戻さずに使う
            return Ok(Self {
                initialized: false,
                _not_send: std::marker::PhantomData,
            });
        }
        hr.ok()?;
        Ok(Self {
            initialized: true,
            _not_send: std::marker::PhantomData,
        })
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.initialized {
            unsafe { CoUninitialize() };
        }
    }
}
