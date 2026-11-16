//! bindgen 的入口头文件。
//!
//! 只 include 我们真正要用的 FreeRDP / WinPR 头，并显式 allowlist，
//! 避免一次拖进整个 ABI（那会让生成的 bindings.rs 巨大且充满不可用类型）。

#include <freerdp/freerdp.h>
#include <freerdp/gdi/gdi.h>
#include <freerdp/settings_types.h>
// S8-63 剪贴板：客户端回调结构与通道入口
#include <freerdp/client/cliprdr.h>
#include <freerdp/client/channels.h>
#include <freerdp/channels/cliprdr.h>
#include <freerdp/channels/channels.h>
#include <winpr/synch.h>
#include <winpr/wtypes.h>
// S8-63 排查：打开 FreeRDP 自己的协议日志（cliprdr 的每条报文都在里面）。
// 没有它只能从回调反推「服务端发没发某条消息」，而回调不触发时
// 分不清是「没发」还是「发了被 FreeRDP 丢弃」。
#include <winpr/wlog.h>

// `cliprdr_VirtualChannelEntryEx` 在 FreeRDP 里是 .c 文件里的 #define 改名导出
// （channels/cliprdr/client/cliprdr_main.c），公开头里**没有**声明。
// bindgen 抓不到，这里补一份。签名来自该文件的 FREERDP_ENTRY_POINT 宏展开。
extern BOOL VCAPITYPE cliprdr_VirtualChannelEntryEx(PCHANNEL_ENTRY_POINTS_EX pEntryPoints,
                                                    PVOID pInitHandle);

// ---- S8-64 动态分辨率：disp 走「动态虚拟通道」，要先把 drdynvc 装上 ----
//
// disp 不是静态通道，而是 DVC 插件：drdynvc 通道打开时遍历 settings 的
// DynamicChannelArray，逐个用 `freerdp_load_channel_addin_entry(名字, ...,
// FREERDP_ADDIN_CHANNEL_DYNAMIC)` 查插件入口。库模式从没调用过
// `freerdp_register_addin_provider`，那条查找会退化成 dlopen 而必然失败，
// 所以接线时要自己补上（见 display.rs）。
#include <freerdp/client/disp.h>
#include <freerdp/channels/disp.h>
#include <freerdp/client/drdynvc.h>

// 同样只有 .c 里的改名导出（channels/drdynvc/client/drdynvc_main.c:2251）
extern BOOL VCAPITYPE drdynvc_VirtualChannelEntryEx(PCHANNEL_ENTRY_POINTS_EX pEntryPoints,
                                                    PVOID pInitHandle);

// disp 的 DVC 插件入口。头文件只声明了 `DispClientContext` 结构，
// 入口函数靠 `FREERDP_ENTRY_POINT` 宏导出，公开头里没有声明。
// **不通过 `freerdp_channels_load_static_addin_entry` 查表**——那张表会把
// 所有通道的 .o 一起拖进链接（remdesk 那几个辅助符号没人提供，直接链接失败），
// 我们自己当 provider 只返回这一支。
extern UINT VCAPITYPE disp_DVCPluginEntry(IDRDYNVC_ENTRY_POINTS* pEntryPoints);
