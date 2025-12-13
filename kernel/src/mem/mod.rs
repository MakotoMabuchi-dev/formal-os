// kernel/src/mem/mod.rs
//
// 役割:
// - メモリ関連のサブモジュールをまとめる中継点。
// - addr.rs / paging.rs / address_space.rs / layout.rs を公開する。

pub mod addr;
pub mod paging;
pub mod address_space;
pub mod layout;
