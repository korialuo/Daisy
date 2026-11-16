//! RDP 服务端证书指纹：**首连接受并记录，变更即拒绝**。
//!
//! 对应 STEPS 步骤 65 与 PLAN 风险 R17。
//!
//! ## 为什么不能简单"打开证书校验"
//!
//! 自签名 RDP 服务端（绝大多数 Windows 机器的默认状态）用系统 CA 是验不过的。
//! 直接打开校验 = 自签名机器全部连不上。所以走 SSH `known_hosts` 的老路：
//!
//! ```text
//!   库里没有这台机器的指纹  → 接受（仅本次），连上之后由上层记入库
//!   库里有且一致            → 接受
//!   库里有但不一致          → 拒绝，并说清楚「记录的是 X，这次是 Y」
//! ```
//!
//! 三者都**由我们自己判定**，不依赖 FreeRDP 自己的 `known_hosts` 文件——
//! 那个文件可能被 `xfreerdp` 等别的客户端写过，让 Daisy 的判断变得不可解释。
//! 因此回调一律返回 `2`（只接受本次会话），不返回 `1`（接受并写盘）：
//! 事实来源只有 Daisy 的 SQLite。
//!
//! ## 线程
//!
//! 回调在 FreeRDP 的工作线程里跑，`expected` / `observed` 都是 `Mutex`；
//! 判定本身是纯函数（[`decide`]），单测直接打它。

use std::sync::Mutex;

/// 指纹判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
	/// 库里没有记录：首次见到这张证书。
	FirstSight,
	/// 与记录一致。
	Match,
	/// 与记录不一致——**拒绝连接**。
	Changed {
		/// 库里存的指纹。
		expected: String,
	},
}

impl Verdict {
	/// 这个判定是否允许继续连接。
	#[must_use]
	pub fn accepted(&self) -> bool {
		!matches!(self, Self::Changed { .. })
	}
}

/// 一次证书观察的结果。
#[derive(Debug, Clone)]
pub struct Observation {
	/// 服务端主机名（或 IP）。
	pub host: String,
	/// 端口。
	pub port: u16,
	/// **规范化之后**的指纹（小写、无分隔符）。
	pub fingerprint: String,
	/// 证书使用者（`CN=...`），可能为空。
	pub subject: Option<String>,
	/// 签发者（`CN=...`），可能为空。
	pub issuer: Option<String>,
	/// 判定结果。
	pub verdict: Verdict,
}

/// 把 FreeRDP 给的指纹规范成可比对的形式。
///
/// FreeRDP 产出的形如 `aa:bb:cc:…`（小写十六进制、冒号分隔）。不同来源
/// 可能带大写、短横线或空格，所以统一：**只留十六进制字符、转小写**。
///
/// 规范化之后 64 个字符是 SHA-256。长度不对也不报错——照原样留着，
/// 比对时自然不等（宁可贵一点也不能误判成"一致"）。
#[must_use]
pub fn normalize(raw: &str) -> String {
	raw.chars()
		.filter(char::is_ascii_hexdigit)
		.map(|c| c.to_ascii_lowercase())
		.collect()
}

/// 把规范化指纹按 `aa:bb:cc:` 分组，给界面用。
#[must_use]
pub fn format_fingerprint(normalized: &str) -> String {
	normalized
		.as_bytes()
		.chunks(2)
		.map(|pair| String::from_utf8_lossy(pair).to_string())
		.collect::<Vec<_>>()
		.join(":")
}

/// **纯判定**：给定库里存的指纹与本次观察到的指纹，决定接受与否。
///
/// 抽成自由函数是为了能直接单测——回调 trampoline 需要真的 FreeRDP 实例，
/// 测不了；判定规则本身才是容易写错的地方。
#[must_use]
pub fn decide(expected: Option<&str>, actual: &str) -> Verdict {
	let Some(expected) = expected else {
		return Verdict::FirstSight;
	};
	let expected = normalize(expected);
	let actual = normalize(actual);

	if !expected.is_empty() && expected == actual {
		Verdict::Match
	} else {
		Verdict::Changed { expected }
	}
}

/// 证书校验的跨线程状态。
///
/// 由 [`crate::RdpClient`] 持有，指针经 context 扩展内存交给回调。
#[derive(Default)]
pub struct CertificateGuard {
	/// 库里存的指纹（连接前写入）。
	expected: Mutex<Option<String>>,
	/// 最近一次观察结果（连接后由上层取走）。
	observed: Mutex<Option<Observation>>,
}

impl CertificateGuard {
	/// 空 guard：没有期望指纹（等于首连）。
	#[must_use]
	pub fn new() -> Self {
		Self::default()
	}

	/// 连接前写入「库里存的指纹」。
	///
	/// 传 `None` 表示库里没有这台机器的记录。
	pub fn trust(&self, fingerprint: Option<&str>) {
		let value = fingerprint.map(normalize).filter(|text| !text.is_empty());
		*lock(&self.expected) = value;
	}

	/// 取走观察结果（取一次就清空）。
	pub fn take(&self) -> Option<Observation> {
		lock(&self.observed).take()
	}

	/// 回调入口：判定、记账、返回是否允许本次连接。
	///
	/// 返回 `true` 时回调应回 `2`（**仅本次接受**），`false` 回 `0`。
	pub fn observe(
		&self,
		host: &str,
		port: u16,
		fingerprint: &str,
		subject: Option<String>,
		issuer: Option<String>,
	) -> bool {
		let normalized = normalize(fingerprint);
		let verdict = {
			let expected = lock(&self.expected);
			decide(expected.as_deref(), &normalized)
		};
		let accepted = verdict.accepted();

		tracing::info!(
			"[证书] {}:{} 指纹 {}，判定 {:?}",
			host,
			port,
			format_fingerprint(&normalized),
			verdict
		);

		*lock(&self.observed) = Some(Observation {
			host: host.to_string(),
			port,
			fingerprint: normalized,
			subject,
			issuer,
			verdict,
		});

		accepted
	}
}

/// 锁中毒也照常取用——证书判定不该因为别处 panic 过一次就永远失败。
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
	mutex
		.lock()
		.unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
	use super::*;

	const A: &str = "aa:bb:cc:dd";
	const A_NORM: &str = "aabbccdd";

	#[test]
	fn normalize_strips_separators_and_case() {
		assert_eq!(normalize("AA:BB:CC:DD"), "aabbccdd");
		assert_eq!(normalize("aa bb-cc dd"), "aabbccdd");
		assert_eq!(normalize(A_NORM), "aabbccdd");
		// 非十六进制字符一律丢弃（PEM 文本会退化成一段短 hex，见下面用例）
		assert_eq!(normalize("-----BEGIN CERTIFICATE-----"), "becefcae");
	}

	#[test]
	fn format_groups_in_pairs() {
		assert_eq!(format_fingerprint("aabbccdd"), "aa:bb:cc:dd");
		assert_eq!(format_fingerprint(""), "");
	}

	#[test]
	fn unknown_host_is_first_sight_and_accepted() {
		assert_eq!(decide(None, "aa:bb:cc:dd"), Verdict::FirstSight);
		assert!(decide(None, "aa:bb:cc:dd").accepted());
	}

	#[test]
	fn same_fingerprint_matches() {
		assert_eq!(decide(Some("AABBCCDD"), A), Verdict::Match);
		assert!(decide(Some(A_NORM), A).accepted());
	}

	#[test]
	fn different_fingerprint_is_rejected() {
		let verdict = decide(Some("deadbeef"), A);
		assert_eq!(
			verdict,
			Verdict::Changed {
				expected: "deadbeef".to_string()
			}
		);
		assert!(
			!verdict.accepted(),
			"指纹变了必须拒绝，否则这个机制没有意义"
		);
	}

	/// 空指纹不能当成「一致」：某些异常情况下 FreeRDP 传空串，
	/// 若判成 Match 就等于给任何坏证书开了后门。
	#[test]
	fn empty_fingerprint_never_matches() {
		assert!(!decide(Some("aabbccdd"), "").accepted());
		// 库里存着空串属于坏数据，同样不能放行——`trust()` 会把它过滤成 `None`，
		// 走到这里说明数据是从别处来的，宁可拒绝
		assert!(!decide(Some(""), "").accepted());
	}

	#[test]
	fn guard_records_and_reports() {
		let guard = CertificateGuard::new();
		guard.trust(Some("aa:bb:cc:dd"));

		assert!(guard.observe("10.0.0.1", 3389, "AABBCCDD", None, None));
		let seen = guard.take().expect("应当有观察结果");
		assert_eq!(seen.fingerprint, A_NORM);
		assert_eq!(seen.verdict, Verdict::Match);
		assert!(guard.take().is_none(), "取过一次就该清空");
	}

	#[test]
	fn guard_rejects_changed_certificate() {
		let guard = CertificateGuard::new();
		guard.trust(Some("deadbeef"));

		assert!(
			!guard.observe("10.0.0.1", 3389, "aabbccdd", None, None),
			"与记录不符时必须拒绝本次连接"
		);
		assert!(matches!(
			guard.take().map(|o| o.verdict),
			Some(Verdict::Changed { .. })
		));
	}

	/// 首连（库里没记录）要接受但**不写盘**——回调返回 2，事实来源留在 Daisy 的库里。
	#[test]
	fn first_sight_is_accepted_without_recording_trust() {
		let guard = CertificateGuard::new();
		assert!(guard.observe("10.0.0.1", 3389, "aabbccdd", None, None));
		assert_eq!(
			guard.take().map(|o| o.verdict),
			Some(Verdict::FirstSight),
			"首连要标记成 FirstSight，上层据此写库并提示"
		);
	}
}
