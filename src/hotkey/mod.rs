//! ホットキーの割り当て、文字列の解析、押下の検出。
//!
//! 2,208 行あった `src/hotkey.rs` を役割ごとに分けたもの（#208）。外から
//! 見える経路は下の `pub use` に集めてある（`crate::hotkey::HotkeyManager` など）。
//!
//! | ファイル | 役割 |
//! |---|---|
//! | `action.rs` | `HotkeyAction` と設定ファイル上の名前、溜まった押下の畳み方 |
//! | `parse.rs` | `HotkeyError` と、ホットキー文字列 → `KeyChord` の解析 |
//! | `egui_keys.rs` | egui のキー入力 → `KeyChord` と、egui へ渡すキー入力からホットキーのキーを取り除く判定（#217、#418） |
//! | `manager.rs` | `HotkeyManager` の本体と `BackgroundHotkeyRunner`、リスナーの起動と停止、ウィンドウ状態の受け渡し |
//! | `assignments.rs` | 割り当ての差分適用、一時停止と再開、試し登録、押下の取り出し |
//! | `method.rs` | 押下を受け取る方式の切り替えと、キーを奪う方式（`RegisterHotKey`）の登録の反映（#207） |
//! | `listener.rs` | リスナーと共有する状態 `ListenerState`、押下の照合とデバウンス |
//! | `listener_thread.rs` | リスナースレッドの本体（メッセージループ）と `HotkeyMethod`、方式ごとの OS への登録（`plan_sync`） |
//!
//! キーボードフック自体は `crate::keyboard_hook`、`RegisterHotKey` は
//! `crate::system_hotkey` にある。

mod action;
mod assignments;
mod egui_keys;
mod listener;
mod listener_thread;
mod manager;
mod method;
mod parse;

pub use action::HotkeyAction;
pub use assignments::HotkeyAssignmentError;
pub use listener_thread::HotkeyMethod;
pub use manager::{BackgroundHotkeyRunner, HotkeyManager};
pub use parse::HotkeyError;
