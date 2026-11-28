//! Agent トランスクリプトペインの描画。

pub(crate) mod async_layout;
mod goal;
mod virtual_list;
pub(crate) use virtual_list::mark_async_layout;
