//! RDP 层错误。
//!
//! 设计原则同 SSH 层：**用户能据此决定下一步做什么**。
//! 「连接失败」没有价值——要区分是认证错、证书警告、还是网络不通。

use thiserror::Error;

/// RDP 层错误。
#[derive(Debug, Error)]
pub enum RdpError {
	/// 连接参数不合法（主机为空、分辨率越界等）。
	#[error("RDP 参数不合法：{0}")]
	InvalidParams(String),

	/// 连接被拒绝 / 网络不可达，或协议协商失败。
	///
	/// `code` 是 FreeRDP 的 `freerdp_get_last_error` 返回码（十六进制）。
	#[error("RDP 连接失败：{message}（错误码 0x{code:08X}）")]
	Connect {
		/// FreeRDP 错误码。
		code: u32,
		/// 可读的错误描述。
		message: String,
	},

	/// 认证被拒绝。`reason` 是**具体到能让人知道下一步做什么**的说法——
	/// 「用户名或密码不正确」只覆盖其中一种，帐户禁用、密码过期、没有远程
	/// 桌面登录权限则完全是另一回事（FreeRDP 给了不同的错误码，见
	/// `freerdp.rs::classify_connect_error`）。
	///
	/// 错误码不进文案：它对用户没意义，排查时看日志。
	#[error("RDP 认证失败：{reason}")]
	AuthRejected {
		/// FreeRDP 原始错误码（保留以便日志追溯）。
		code: u32,
		/// 给用户看的原因。
		reason: String,
	},

	/// 证书问题（自签名、过期、主机名不匹配）。
	#[error("RDP 证书警告：{reason}")]
	Certificate {
		/// 具体原因。
		reason: String,
	},

	/// GDI 初始化失败。
	#[error("RDP 图形初始化失败：{0}")]
	Gdi(String),

	/// 显示通道（disp）还没连上。
	///
	/// **这是连接初期的正常状态，不是故障**：动态通道要等服务端在握手之后
	/// 才接受，而界面一出来就会把「跟随窗口」的尺寸排进队列，必然早于通道。
	///
	/// 单独一个变体是为了让上层能**静默重试**——它若和 `Gdi` 混在一起，
	/// 用户就会在刚连上时看到一条「RDP 图形初始化失败」的红字，过几秒又
	/// 自己消失（真实反馈）。
	#[error("显示通道还没就绪")]
	DisplayNotReady,

	/// 会话已断开。
	#[error("RDP 会话已断开：{reason}")]
	Disconnected {
		/// 断开原因。
		reason: String,
	},

	/// 帧缓冲相关错误。
	#[error("RDP 帧处理失败：{0}")]
	Frame(String),

	/// 分辨率变更失败（服务端不支持动态调整）。
	#[error("RDP 分辨率变更失败：{reason}。可能需要断开重连")]
	Resize {
		/// 具体原因。
		reason: String,
	},

	/// 剪贴板操作失败。
	#[error("RDP 剪贴板错误：{0}")]
	Clipboard(String),

	/// 输入注入失败（按键 / 指针 / 滚轮没能发出去）。
	#[error("RDP 输入注入失败：{0}")]
	Input(String),

	/// 平台相关的初始化失败（目前只有 Windows 的 WinSock）。
	///
	/// 单独一类而不是塞进 `Gdi` / `Connect`：它发生在**协议之前**，
	/// 报出来时用户该看的是系统环境，不是网络或密码。
	#[error("RDP 平台初始化失败：{0}")]
	Platform(String),
}

/// RDP 层结果别名。
pub type Result<T, E = RdpError> = std::result::Result<T, E>;
