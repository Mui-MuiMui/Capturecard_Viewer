//! 「既定のデバイス」を、いまの既定の物理デバイスへ引き当てる。
//!
//! cpal の `default_input_device()` / `default_output_device()` が返すのは
//! 「既定のデバイス」という仮想の口で、開くと `ActivateAudioInterfaceAsync`
//! を通り、ストリームには既定の切り替えの通知（`IMMNotificationClient`）が
//! 付く。**cpal 0.18.2 ではこの通知のコールバックが Windows のスレッドプールの
//! スレッドを STA にしたまま残し、以後の `ActivateAudioInterfaceAsync` が
//! `RPC_E_CHANGED_MODE` で失敗し続ける**（RustAudio/cpal#1402、#552）。
//! 録画スレッドが MTA を持つこのアプリでは、Bluetooth の出力が外れたのを
//! 境に、終了まで音声を開き直せなくなっていた。
//!
//! そこで、既定のデバイスは**いまの既定の端点の ID で一覧から引き当てた
//! 物理デバイス**として開く。物理デバイスの `IMMDevice::Activate` は呼んだ
//! スレッドで済むのでスレッドプールに触れず、通知も付かない。既定の切り替えは
//! ワーカーの問い合わせ（`poll_default_audio_device`、`docs/design/reconnect.md`）
//! が追いかける。

use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{Device, DeviceId};
use log::debug;

use super::{AudioDirection, AudioError};

/// いまの既定のデバイスを、一覧から引き当てた物理デバイスとして返す。
///
/// 既定が無い・ID が取れない・一覧に見つからないときは `NoDefaultDevice`。
/// **cpal の「既定のデバイス」の口へは倒さない**（それを避けるための関数なので）。
pub(super) fn resolve_default_device(
    host: &cpal::Host,
    direction: AudioDirection,
) -> Result<Device, AudioError> {
    let default = match direction {
        AudioDirection::Input => host.default_input_device(),
        AudioDirection::Output => host.default_output_device(),
    }
    .ok_or(AudioError::NoDefaultDevice(direction))?;
    let id = default.id().map_err(|e| {
        debug!("既定のデバイスの ID を取れない（{:?}）: {}", direction, e);
        AudioError::NoDefaultDevice(direction)
    })?;
    let devices = match direction {
        AudioDirection::Input => host.input_devices(),
        AudioDirection::Output => host.output_devices(),
    }
    .map_err(|e| AudioError::DeviceEnumerationFailed {
        direction,
        source: e.to_string(),
    })?;
    find_by_id(devices, |device| device.id().ok(), &id).ok_or_else(|| {
        debug!("既定のデバイスが一覧に無い（{:?}）: {}", direction, id);
        AudioError::NoDefaultDevice(direction)
    })
}

/// 一覧から ID が `target` と一致する最初の項目を返す。ID が取れない項目は飛ばす。
fn find_by_id<T>(
    items: impl IntoIterator<Item = T>,
    id_of: impl Fn(&T) -> Option<DeviceId>,
    target: &DeviceId,
) -> Option<T> {
    items
        .into_iter()
        .find(|item| id_of(item).as_ref() == Some(target))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpal::HostId;

    fn id(s: &str) -> DeviceId {
        DeviceId::new(HostId::Wasapi, s)
    }

    #[test]
    fn find_by_id_returns_the_matching_item() {
        let items = vec![("a", Some(id("{a}"))), ("b", Some(id("{b}")))];
        let found = find_by_id(items, |(_, item_id)| item_id.clone(), &id("{b}"));
        assert_eq!(found.map(|(name, _)| name), Some("b"));
    }

    #[test]
    fn find_by_id_skips_items_without_an_id() {
        // ID が取れない項目（外れかけのデバイスなど）があっても、後ろの一致を拾う
        let items = vec![("broken", None), ("b", Some(id("{b}")))];
        let found = find_by_id(items, |(_, item_id)| item_id.clone(), &id("{b}"));
        assert_eq!(found.map(|(name, _)| name), Some("b"));
    }

    #[test]
    fn find_by_id_returns_none_when_the_default_is_not_listed() {
        let items = vec![("a", Some(id("{a}")))];
        let found = find_by_id(items, |(_, item_id)| item_id.clone(), &id("{b}"));
        assert!(found.is_none());
    }

    /// RustAudio/cpal#1402 の再現。スレッドプールのスレッドを cpal に STA へ
    /// 変えさせたあと、cpal の「既定のデバイス」の口は開けず、引き当てた物理
    /// デバイスは開けて、出力ストリームも流せることを確かめる（#552）。
    ///
    /// **プロセスのスレッドプールを STA にしたまま戻せない**ので、ほかのテスト
    /// （rodio の既定の出力で鳴らすものなど）と同じプロセスで走らせないこと。
    #[test]
    #[ignore = "音声の出力デバイスが必要。プロセスのスレッドプールを STA にするので単独で実行する"]
    fn resolved_default_survives_sta_poisoned_thread_pool() {
        // 実行: cargo test audio::default_device::tests::resolved_default_survives_sta_poisoned_thread_pool -- --ignored --exact
        use crate::com::{ComApartment, ComModel};
        use cpal::traits::StreamTrait;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::mpsc;
        use std::time::{Duration, Instant};
        use windows::Win32::System::Threading::{
            TrySubmitThreadpoolCallback, PTP_CALLBACK_INSTANCE,
        };

        const POOL_CALLBACKS: usize = 64;
        static DONE: AtomicUsize = AtomicUsize::new(0);

        unsafe extern "system" fn poison(_: PTP_CALLBACK_INSTANCE, _: *mut core::ffi::c_void) {
            // cpal の通知のコールバックと同じ経路（current_default_endpoint →
            // com_initialized）で、このプールのスレッドを STA にする
            let _ = cpal::default_host().default_output_device();
            // ほかのコールバックが同じスレッドへ重ならないよう、少し居座る
            std::thread::sleep(Duration::from_millis(200));
            DONE.fetch_add(1, Ordering::SeqCst);
        }

        // 録画スレッドと同じく、プロセスに MTA を持たせておく
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let (ready_tx, ready_rx) = mpsc::channel::<()>();
        let mta = std::thread::spawn(move || {
            let _com = ComApartment::enter(ComModel::MultiThreaded).expect("MTA に入れる");
            ready_tx.send(()).unwrap();
            let _ = stop_rx.recv();
        });
        ready_rx.recv().unwrap();

        // ワーカーと同じく STA のスレッドで開く
        let open_on_sta = |resolve: bool| {
            std::thread::spawn(move || -> Result<(), String> {
                let _com = ComApartment::enter(ComModel::SingleThreaded).expect("STA に入れる");
                let host = cpal::default_host();
                let device = if resolve {
                    resolve_default_device(&host, AudioDirection::Output)
                        .map_err(|e| e.to_string())?
                } else {
                    host.default_output_device()
                        .ok_or_else(|| "既定の出力が無い".to_string())?
                };
                let config = device.default_output_config().map_err(|e| e.to_string())?;
                if !resolve || config.sample_format() != cpal::SampleFormat::F32 {
                    return Ok(());
                }
                // 引き当てた物理デバイスは、パススルーと同じくストリームまで開けること。
                // 無音を書くだけなので鳴らない
                let stream = device
                    .build_output_stream(
                        config.config(),
                        |data: &mut [f32], _| data.fill(0.0),
                        |_| {},
                        None,
                    )
                    .map_err(|e| e.to_string())?;
                stream.play().map_err(|e| e.to_string())?;
                std::thread::sleep(Duration::from_millis(100));
                Ok(())
            })
            .join()
            .unwrap()
        };

        assert_eq!(
            open_on_sta(false),
            Ok(()),
            "汚す前は cpal の既定の口も開ける"
        );

        for _ in 0..POOL_CALLBACKS {
            unsafe { TrySubmitThreadpoolCallback(Some(poison), None, None) }
                .expect("スレッドプールへ積める");
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        while DONE.load(Ordering::SeqCst) < POOL_CALLBACKS {
            assert!(
                Instant::now() < deadline,
                "プールのコールバックが終わらない"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        let raw = open_on_sta(false);
        let resolved = open_on_sta(true);
        let _ = stop_tx.send(());
        mta.join().unwrap();

        println!("cpal の既定の口: {:?}", raw);
        println!("引き当てた物理デバイス: {:?}", resolved);
        assert_eq!(resolved, Ok(()), "引き当てた物理デバイスは開ける");
        assert!(
            raw.as_ref().is_err_and(|e| e.contains("-2147417850")),
            "cpal の既定の口は RPC_E_CHANGED_MODE で開けない（cpal#1402 が直っていればこの行を見直す）: {:?}",
            raw
        );
    }
}
