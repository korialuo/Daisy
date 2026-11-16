//! bindgen 生成的 FreeRDP 绑定，收口在这里。
//!
//! bindgen 输出必然触发一批 lint（`BOOL`/`BYTE`/`DWORD` 等大写类型名命中
//! `non_camel_case_types` 与 `upper_case_acronyms`）。这里把放行范围**限定在
//! 本模块内**，不在 crate 级全局关闭——其余 Rust 代码仍享受完整 lint。

#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(dead_code)]
#![allow(clippy::upper_case_acronyms)]
#![allow(clippy::missing_safety_doc)]
#![allow(improper_ctypes)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
