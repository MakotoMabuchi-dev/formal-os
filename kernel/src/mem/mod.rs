// src/mem/mod.rs
//
// 役割:
// - メモリ関連のサブモジュールをまとめる中継点。
// - addr.rs / paging.rs などを公開する。
// やること:
// - pub mod addr;
// - pub mod paging;

pub mod addr;
pub mod paging;
pub mod address_space;
pub mod layout;