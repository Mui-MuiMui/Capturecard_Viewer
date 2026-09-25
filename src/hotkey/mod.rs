//! ホットキーの割り当て、文字列の解析、押下の検出。
//!
//! 2,208 行あった `src/hotkey.rs` を役割ごとに分けたもの（#208）。**分割は
//! 移動だけで、挙動は変えていない。** 外から見える経路は下の `pub use` で
//! 分割前と同じにしてある（`crate::hotkey::HotkeyManager` など）。
//!
//! | ファイル | 役割 |
//! |---|---|
//! | `action.rs` | `HotkeyAction` と設定ファイル上の名前、溜まった押下の畳み方 |
//! | `parse.rs` | `HotkeyError` と、ホットキー文字列 → `KeyChord` の解析 |
//! | `manager.rs` | `HotkeyManager` の本体と `BackgroundHotkeyRunner`、リスナーの起動と停止、ウィンドウ状態の受け渡し |
//! | `assignments.rs` | 割り当ての差分適用、一時停止と再開、試し登録、押下の取り出し |
//! | `listener.rs` | リスナースレッドと共有状態 `ListenerState`、押下の照合とデバウンス |
//!
//! キーボードフック自体は `crate::keyboard_hook` にある。

mod action;
mod assignments;
mod listener;
mod manager;
mod parse;

pub use action::HotkeyAction;
pub use assignments::HotkeyAssignmentError;
pub use manager::{BackgroundHotkeyRunner, HotkeyManager};
pub use parse::HotkeyError;
