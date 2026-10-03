//! COM と Media Foundation の初期化を RAII で包む。
//!
//! どちらも「このスレッドで使えるようにしておく印」で、落とすと初期化を戻す。
//! 使うのはデバイスワーカー（DirectShow のために STA で初期化する）と、
//! 録画スレッド（Sink Writer のために MTA で初期化し、MF も起こす）。
//!
//! **順序は COM → MF で、戻すのは逆順。** MF の印を COM の印より先に落とすこと
//! （ローカル変数なら COM を先に宣言する。落ちるのは宣言の逆順）。

use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::Media::MediaFoundation::{MFShutdown, MFStartup, MFSTARTUP_LITE, MF_VERSION};
use windows::Win32::System::Com::{
    CoInitializeEx, CoUninitialize, COINIT, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
    COINIT_MULTITHREADED,
};

/// COM のアパートメントの種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComModel {
    /// シングルスレッドアパートメント（STA）。デバイスワーカーはこちら。
    /// 同じスレッドで nokhwa（Media Foundation）と cpal（WASAPI）がどちらも
    /// STA で初期化しており、ここだけ MTA にすると後から初期化する側が
    /// `RPC_E_CHANGED_MODE` で失敗する（nokhwa はそれを起動の失敗として扱う。
    /// `docs/design/device-worker.md` の「スレッドと COM」）
    SingleThreaded,
    /// マルチスレッドアパートメント（MTA）。録画スレッドはこちら。
    /// 自分しか COM を使わず、メッセージループも回さないため。MF のオブジェクトは
    /// 基本的にフリースレッドで、Sink Writer は内部の作業キューで動く
    /// （`docs/design/recording.md` の「COM と MF の初期化」）
    MultiThreaded,
}

impl ComModel {
    fn flags(self) -> COINIT {
        match self {
            ComModel::SingleThreaded => COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE,
            ComModel::MultiThreaded => COINIT_MULTITHREADED | COINIT_DISABLE_OLE1DDE,
        }
    }
}

/// このスレッドで COM を使えるようにしておく印。落とすと初期化を戻す。
pub struct ComApartment {
    /// `CoUninitialize` で戻す必要があるか。既に別のモデルで初期化されていて
    /// `RPC_E_CHANGED_MODE` が返ったときだけ偽
    initialized: bool,
    /// スレッドに紐づくので、ほかのスレッドへ持ち出させない
    _not_send: std::marker::PhantomData<*mut ()>,
}

impl ComApartment {
    /// このスレッドの COM を `model` で初期化する。既に同じモデルで初期化済みでも
    /// よい（回数が数えられるだけで、`Drop` で 1 回戻す）。
    pub fn enter(model: ComModel) -> Result<Self, windows::core::Error> {
        let hr = unsafe { CoInitializeEx(None, model.flags()) };
        if hr == RPC_E_CHANGED_MODE {
            // 別のモデルで初期化済み。COM は使えるので、戻さずに使う
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

/// Media Foundation を使えるようにしておく印。落とすと `MFShutdown` を呼ぶ。
///
/// **MF は呼んだ回数を数える**ので、nokhwa が別のスレッドで `MFStartup` を
/// 呼んでいても干渉しない。COM を初期化したスレッドで、`ComApartment` より
/// 後に作って先に落とす。
pub struct MfPlatform {
    _not_send: std::marker::PhantomData<*mut ()>,
}

impl MfPlatform {
    /// `MFStartup(MF_VERSION, MFSTARTUP_LITE)`。ソケットを使う機能は要らないので LITE。
    pub fn start() -> Result<Self, windows::core::Error> {
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE) }?;
        Ok(Self {
            _not_send: std::marker::PhantomData,
        })
    }
}

impl Drop for MfPlatform {
    fn drop(&mut self) {
        // 戻せなくても続きは無い（スレッドが終わるだけ）ので、結果は見ない
        let _ = unsafe { MFShutdown() };
    }
}
