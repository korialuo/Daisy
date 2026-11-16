//! Daisy RDP 层 —— 基于 FreeRDP 的**库模式**集成（不使用其自带窗口）。
//!
//! 预研结论见 `spike/RESULT.md`，几条已经踩实的约定：
//!
//! - 用底层 API：`freerdp_new()` → 覆盖 `ContextSize` → `freerdp_context_new()`。
//!   高层次 API `freerdp_client_context_new` 对树外客户端不可用
//! - 主要只链 `libfreerdp3.a` + `libwinpr3.a`；**剪贴板通道额外链
//!   `libfreerdp-client3.a`**（S8-63）——它的入口 `cliprdr_VirtualChannelEntryEx`
//!   与静态插件表在那个库里。仍不走 client common 那套高层次 API
//! - 必须显式设 `FreeRDP_ProxyType = PROXY_TYPE_NONE`，否则本机代理会劫持 RDP 流量
//! - 像素格式名按**内存字节序**命名：`XRGB32` 在内存中是 `X,R,G,B`
//! - 帧行宽用 `gdi->stride`，它不等于 `width × 4`
//!
//! | 模块 | 职责 | 阶段 |
//! | --- | --- | --- |
//! | [`target`] | 地址规范化与 FreeRDP 设置映射（域名 / IPv4 / IPv6） | ✅ 步骤 64c |
//! | [`freerdp`] | RAII 封装、连接、事件泵、帧回调 | ✅ 步骤 58–60 |
//! | [`render`] | 帧格式转换（XRGB32 → 紧凑 RGBA） | ✅ 步骤 61（库层） |
//! | [`input`] | 键鼠输入注入（扫描码表、指针标志、坐标） | ✅ 步骤 62 |
//! | [`clipboard`] | 剪贴板 `cliprdr` 文本双向 | ✅ 步骤 63 |
//! | [`certificate`] | 服务端证书指纹（首连记录、变更拒绝） | ✅ 步骤 65 |
//! | [`display`] | 分辨率动态调整 | ✅ 步骤 64 |
//!
//! **当前状态**：S8 库层已可用（连接、事件泵、整屏帧回调、像素格式转换、
//! 键鼠输入注入、剪贴板双向都通），画面已在界面中显示；分辨率待做。

pub mod certificate;
pub mod clipboard;
pub mod cursor;
pub mod display;
pub mod error;
pub mod ffi;
pub mod freerdp;
pub mod input;
pub mod render;
pub mod target;

pub use certificate::{CertificateGuard, Observation, Verdict, format_fingerprint};
pub use clipboard::{Clipboard, LocalFile, RemoteFile};
pub use cursor::{CursorBitmap, CursorSprite, decode};
pub use display::DisplayControl;
pub use error::{RdpError, Result};
pub use freerdp::{ConnectionParams, FrameBuffer, RdpClient};
pub use input::{InputEvent, Key, PointerButton, Scancode, WHEEL_DELTA, WheelAxis};
pub use render::{RgbaFrame, to_rgba};
pub use target::{DEFAULT_RDP_PORT, RdpTarget};
