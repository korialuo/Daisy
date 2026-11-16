//! 网络层错误。
//!
//! 连接失败的原因必须分得足够细，因为界面要给出**可操作的**提示：
//! 「域名查不到」「端口没人听」「超时」三者对用户意味着完全不同的下一步动作。

use thiserror::Error;

/// 网络层错误。
#[derive(Debug, Error)]
pub enum NetError {
	/// 目标地址格式不合法。
	#[error("目标地址不合法：{0}")]
	InvalidTarget(String),

	/// 主机名无法解析。
	#[error("无法解析主机名 {host}：{source}")]
	Resolve {
		/// 主机名。
		host: String,
		/// 底层错误。
		source: std::io::Error,
	},

	/// 主机名解析成功，但没有返回任何地址。
	#[error("主机名 {host} 没有解析出任何地址")]
	NoAddresses {
		/// 主机名。
		host: String,
	},

	/// 连接被对端拒绝（端口没人监听，或防火墙直接回 RST）。
	#[error("连接 {host}:{port} 被拒绝，请确认服务已启动且端口正确")]
	Refused {
		/// 主机名或 IP。
		host: String,
		/// 端口。
		port: u16,
	},

	/// 连接超时。
	#[error("连接 {host}:{port} 超时（{timeout_ms} ms），可能是网络不通或被防火墙丢弃")]
	Timeout {
		/// 主机名或 IP。
		host: String,
		/// 端口。
		port: u16,
		/// 超时毫秒数。
		timeout_ms: u64,
	},

	/// 其它连接错误（网络不可达、权限不足等）。
	#[error("连接 {host}:{port} 失败：{source}")]
	Connect {
		/// 主机名或 IP。
		host: String,
		/// 端口。
		port: u16,
		/// 底层错误。
		source: std::io::Error,
	},

	/// 已建立连接后的读写错误。
	#[error("网络读写失败：{0}")]
	Io(#[from] std::io::Error),

	/// 隧道（SOCKS5 / HTTP CONNECT）协商失败。S7 使用。
	#[error("通过代理建立隧道失败：{0}")]
	Tunnel(String),

	/// **连不上代理本身**。
	///
	/// 与「连不上目标」必须分开：这两件事的排查方向完全不同。
	///
	/// `reason` 存的是从底层错误提炼出的**短语**而不是嵌套的错误原文——
	/// 否则文案会变成「无法连接代理 10.0.0.1:1080：连接 10.0.0.1:1080 被拒绝」，
	/// 地址重复两遍，读起来很蠢。
	#[error("无法连接代理 {proxy}：{reason}")]
	ProxyConnect {
		/// 代理地址 `host:port`。
		proxy: String,
		/// 提炼后的原因短语。
		reason: String,
	},

	/// **代理拒绝了认证**。
	#[error("代理 {proxy} 的认证未通过：{reason}")]
	ProxyAuthRejected {
		/// 代理地址 `host:port`。
		proxy: String,
		/// 具体原因。
		reason: String,
	},

	/// **代理拒绝建立到目标的隧道**（或目标自身不可达）。
	#[error("代理 {proxy} 未能建立到 {target} 的隧道：{reason}")]
	ProxyRefused {
		/// 代理地址 `host:port`。
		proxy: String,
		/// 目标地址 `host:port`。
		target: String,
		/// 具体原因。
		reason: String,
	},

	/// 代理的响应不符合协议，无法解析。
	#[error("代理 {proxy} 的响应无法解析：{reason}")]
	ProxyProtocol {
		/// 代理地址 `host:port`。
		proxy: String,
		/// 具体原因。
		reason: String,
	},
}

/// 网络层结果别名。
pub type Result<T, E = NetError> = std::result::Result<T, E>;

impl NetError {
	/// 提炼出一句短语，用于拼接「XX 失败：<短语>」这类上层文案。
	///
	/// 直接用 `Display` 拼会发生地址重复（如「无法连接代理 10.0.0.1:1080：
	/// 连接 10.0.0.1:1080 被拒绝」）。这里只保留**原因**，去掉重复的地址。
	#[must_use]
	pub fn short_reason(&self) -> String {
		match self {
			Self::Refused { .. } => "连接被拒绝，请检查地址与端口是否正确".to_string(),
			Self::Timeout { timeout_ms, .. } if *timeout_ms > 0 => {
				format!("连接超时（{timeout_ms} ms）")
			},
			Self::Timeout { .. } => "连接超时".to_string(),
			Self::Resolve { host, .. } => format!("无法解析主机名 {host}"),
			Self::NoAddresses { host } => format!("主机名 {host} 没有解析出任何地址"),
			Self::InvalidTarget(reason) => format!("地址不合法：{reason}"),
			other => other.to_string(),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn messages_are_actionable() {
		let refused = NetError::Refused {
			host: "10.0.0.1".to_string(),
			port: 22,
		};
		let text = refused.to_string();
		assert!(text.contains("10.0.0.1:22"), "应带地址：{text}");
		assert!(text.contains("端口"), "应提示检查端口：{text}");

		let timeout = NetError::Timeout {
			host: "example.com".to_string(),
			port: 443,
			timeout_ms: 5000,
		};
		let text = timeout.to_string();
		assert!(text.contains("5000"), "应带超时值：{text}");
	}

	#[test]
	fn proxy_failures_are_distinguishable() {
		// 三类代理故障必须能区分开——它们对应完全不同的排查动作：
		// 「代理地址/端口对不对」「凭据对不对」「目标能不能到」
		let unreachable = NetError::ProxyConnect {
			proxy: "10.0.0.1:1080".to_string(),
			reason: "连接被拒绝，请检查代理地址与端口".to_string(),
		};
		let auth = NetError::ProxyAuthRejected {
			proxy: "10.0.0.1:1080".to_string(),
			reason: "代理拒绝了用户名或密码".to_string(),
		};
		let refused = NetError::ProxyRefused {
			proxy: "10.0.0.1:1080".to_string(),
			target: "10.0.0.9:3389".to_string(),
			reason: "目标端口未监听".to_string(),
		};

		let texts = [
			unreachable.to_string(),
			auth.to_string(),
			refused.to_string(),
		];

		// 都带上了代理地址，便于定位
		for text in &texts {
			assert!(text.contains("10.0.0.1:1080"), "应含代理地址：{text}");
		}

		assert!(texts[0].contains("无法连接代理"), "{}", texts[0]);
		assert!(texts[1].contains("认证"), "{}", texts[1]);
		assert!(
			texts[2].contains("10.0.0.9:3389"),
			"应含目标地址：{}",
			texts[2]
		);

		// 三条文案互不相同
		assert_ne!(texts[0], texts[1]);
		assert_ne!(texts[1], texts[2]);
		assert_ne!(texts[0], texts[2]);
	}

	#[test]
	fn short_reason_drops_the_address_to_avoid_duplication() {
		let refused = NetError::Refused {
			host: "10.0.0.1".to_string(),
			port: 1080,
		};
		let reason = refused.short_reason();
		assert!(
			!reason.contains("10.0.0.1"),
			"短语里不该再出现地址，否则上层拼接会重复：{reason}"
		);
		assert!(reason.contains("拒绝"), "应说明原因：{reason}");

		// 拼起来的效果
		let wrapped = NetError::ProxyConnect {
			proxy: "10.0.0.1:1080".to_string(),
			reason,
		};
		let text = wrapped.to_string();
		assert_eq!(
			text.matches("10.0.0.1:1080").count(),
			1,
			"地址只应出现一次：{text}"
		);

		let resolved = NetError::Resolve {
			host: "no-such-host.invalid".to_string(),
			source: std::io::Error::other("dns"),
		};
		assert!(resolved.short_reason().contains("no-such-host.invalid"));
	}
}
