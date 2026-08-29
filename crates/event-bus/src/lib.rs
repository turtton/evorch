//! 型付きイベントストリームの内部配信基盤であり、tokio broadcast ベースで ADR 0012 の計測収集層の土台となります。

pub mod bus;
pub mod event;
pub mod ring;
pub mod usage;

// TODO: モジュール実装後に公開型の再エクスポートを追加する。
