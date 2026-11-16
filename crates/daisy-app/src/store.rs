//! 加密库在界面层的入口。
//!
//! 只做三件事：按标准目录开库、转发主机 CRUD、转发 known_hosts 读写。
//! 界面层不直接碰 `rusqlite`，也不自己拼 SQL。
//!
//! 密钥来源按平台分（见 [`key_provider`]）：
//!
//! | 平台 | 实现 | 强度 |
//! | --- | --- | --- |
//! | Windows | [`DpapiKey`] | 真加密，密钥绑定 Windows 用户 |
//! | macOS | [`EmbeddedKey`] | 混淆（Keychain 实现尚未接入） |
//!
//! 这一层只认 [`KeyProvider`]，分平台的选择收在 [`key_provider`] 一处。

use daisy_core::config::AppPaths;
#[cfg(windows)]
use daisy_core::key::DpapiKey;
#[cfg(not(windows))]
use daisy_core::key::EmbeddedKey;
use daisy_core::key::KeyProvider;
use daisy_core::model::{Group, Host, Proxy};
use daisy_store::known_hosts;
use daisy_store::known_hosts::KnownHost;
use daisy_store::{Database, HostSecrets, StoreError};
use zeroize::Zeroizing;

/// 按平台选择主密钥来源。
///
/// Windows 走 DPAPI（S11 步骤 86）：密钥是真随机的，由系统按当前用户加密，
/// 拷走数据库也解不开。SQLCipher 的库密钥与字段密钥都由它经 HKDF 派生。
///
/// macOS 仍用内置密钥——它只是混淆。Keychain 实现是 [`KeyProvider`] 的
/// 另一个实现，接上以后调用方不需要改。
fn key_provider(paths: &AppPaths) -> Box<dyn KeyProvider> {
	#[cfg(windows)]
	{
		Box::new(DpapiKey::new(paths.key_file(), paths.database_file()))
	}

	#[cfg(not(windows))]
	{
		// macOS 不使用密钥文件，路径也就不用解析
		let _ = paths;
		Box::new(EmbeddedKey)
	}
}

/// 已打开的加密库。
pub struct Store {
	db: Database,
}

impl Store {
	/// 在指定目录下打开（首次运行自动建库并迁移）。
	///
	/// 目录由调用方给出——`DAISY_DATA_DIR` 的解析在应用入口做一次，
	/// 库与配置共用同一个 `AppPaths`。早先只有 Store 自己读环境变量，
	/// 配置仍走真实用户目录，出现「库是临时的、配置是本机的」分裂。
	pub fn open_at(paths: &AppPaths) -> Result<Self, StoreError> {
		let provider = key_provider(paths);
		let db = Database::open(paths, provider.as_ref())?;
		tracing::info!(
			"加密库已打开：{}（密钥来源：{}）",
			paths
				.config_file()
				.parent()
				.unwrap_or(&paths.config_file())
				.display(),
			provider.describe()
		);
		Ok(Self { db })
	}

	/// 全部主机。
	pub fn hosts(&self) -> Result<Vec<Host>, StoreError> {
		self.db.hosts()
	}

	/// 新增或更新主机，返回主键。
	pub fn save_host(&self, host: &Host, secrets: &HostSecrets) -> Result<i64, StoreError> {
		self.db.save_host(host, secrets)
	}

	/// 删除主机。
	pub fn delete_host(&self, id: i64) -> Result<bool, StoreError> {
		self.db.delete_host(id)
	}

	/// 解密后的登录密码。未保存时为 `None`。
	pub fn password(&self, id: i64) -> Result<Option<Zeroizing<String>>, StoreError> {
		self.db.host_password(id)
	}

	/// 解密后的私钥口令。未设置时为 `None`。
	pub fn key_passphrase(&self, id: i64) -> Result<Option<Zeroizing<String>>, StoreError> {
		self.db.host_key_passphrase(id)
	}

	/// 列出全部代理。
	pub fn proxies(&self) -> Result<Vec<Proxy>, StoreError> {
		self.db.proxies()
	}

	/// 列出全部分组（先顶层后子级，各自按名称排）。
	pub fn groups(&self) -> Result<Vec<Group>, StoreError> {
		self.db.groups()
	}

	/// 新建分组。`parent` 为 `None` 建顶层分组，否则挂为子分组（只支持两层）。
	pub fn create_group(&self, name: &str, parent: Option<i64>) -> Result<i64, StoreError> {
		self.db.create_group(name, parent)
	}

	/// 重命名分组。
	pub fn rename_group(&self, id: i64, name: &str) -> Result<(), StoreError> {
		self.db.rename_group(id, name)
	}

	/// 删除分组。子分组级联删除，组内主机退回「未分组」。
	pub fn delete_group(&self, id: i64) -> Result<bool, StoreError> {
		self.db.delete_group(id)
	}

	/// 新增或更新代理。
	///
	/// `password` 为 `None` 会清空已存密码（存储层整列覆盖）——
	/// 「留空即保持原值」的语义由调用方读回原值兑现。
	pub fn save_proxy(
		&self,
		proxy_entry: &Proxy,
		password: Option<&str>,
	) -> Result<i64, StoreError> {
		self.db.save_proxy(proxy_entry, password)
	}

	/// 删除代理。
	pub fn delete_proxy(&self, id: i64) -> Result<bool, StoreError> {
		self.db.delete_proxy(id)
	}

	/// 解密后的代理密码。未保存时为 `None`。
	pub fn proxy_password(&self, id: i64) -> Result<Option<Zeroizing<String>>, StoreError> {
		self.db.proxy_password(id)
	}

	/// 查 known_hosts 记录。键是 `host:port`（端口不同视为不同主机）。
	pub fn known_host(&self, host: &str, port: u16) -> Result<Option<KnownHost>, StoreError> {
		known_hosts::find(self.db.connection(), &known_hosts::host_key(host, port))
	}

	/// 这台机器记录过的 RDP 证书指纹（S8-65）。`None` 表示首连。
	pub fn rdp_cert(&self, host: &str, port: u16) -> Result<Option<String>, StoreError> {
		self.db.rdp_cert_fingerprint(host, port)
	}

	/// 记录首次确认的 RDP 证书指纹。
	pub fn remember_rdp_cert(
		&self,
		host: &str,
		port: u16,
		fingerprint: &str,
		subject: Option<&str>,
		issuer: Option<&str>,
	) -> Result<(), StoreError> {
		self.db
			.remember_rdp_cert(host, port, fingerprint, subject, issuer)
	}

	/// 忘掉这台机器的证书指纹（下次连接会重新按首连处理）。
	///
	/// 用在两个地方：机器重装后指纹变了、用户确认「这台就是它」；
	/// 以及主机详情里的「重新信任」按钮。返回是否真的删掉了一条记录。
	pub fn forget_rdp_cert(&self, host: &str, port: u16) -> Result<bool, StoreError> {
		self.db.forget_rdp_cert(host, port)
	}

	/// 把首次确认的主机密钥写进 known_hosts。
	///
	/// 已存在记录时覆盖——调用方只应在「首次见到」时调这里。
	pub fn remember_host_key(
		&self,
		host: &str,
		port: u16,
		fingerprint: &str,
	) -> Result<(), StoreError> {
		known_hosts::remember(
			self.db.connection(),
			&known_hosts::host_key(host, port),
			fingerprint,
		)
	}
}

impl std::fmt::Debug for Store {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Store").finish_non_exhaustive()
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use daisy_core::model::Protocol;

	#[test]
	fn open_at_creates_a_usable_store() {
		let dir = tempfile::TempDir::new().expect("创建临时目录失败");
		let paths = AppPaths::from_base(dir.path());

		let store = Store::open_at(&paths).expect("开库失败");

		let mut host = Host::draft(Protocol::Ssh);
		host.name = "界面层测试机".to_string();
		host.host = "10.9.8.7".to_string();
		host.username = Some("tester".to_string());

		let id = store
			.save_host(&host, &HostSecrets::with_password("pw-for-ui"))
			.expect("保存失败");

		let hosts = store.hosts().expect("列表失败");
		assert_eq!(hosts.len(), 1);
		assert_eq!(hosts[0].name, "界面层测试机");

		assert_eq!(
			store
				.password(id)
				.expect("读取密码失败")
				.expect("应有密码")
				.as_str(),
			"pw-for-ui"
		);
	}

	#[test]
	fn group_round_trip() {
		let dir = tempfile::TempDir::new().expect("创建临时目录失败");
		let store = Store::open_at(&AppPaths::from_base(dir.path())).expect("开库失败");

		let root = store.create_group("生产", None).expect("建顶层分组失败");
		let child = store
			.create_group("华东", Some(root))
			.expect("建子分组失败");

		let groups = store.groups().expect("列分组失败");
		assert_eq!(groups.len(), 2);
		assert_eq!(groups[0].name, "生产", "顶层分组应排在前面");
		assert_eq!(groups[1].parent_id, Some(root));

		// 只支持两层——三层要被存储层挡住
		assert!(
			store.create_group("孙级", Some(child)).is_err(),
			"三层分组应被拒绝"
		);

		store.rename_group(child, "华东区").expect("重命名失败");
		let renamed = store.groups().expect("列分组失败");
		assert!(renamed.iter().any(|group| group.name == "华东区"));

		assert!(store.delete_group(root).expect("删除失败"));
		assert!(
			store.groups().expect("列分组失败").is_empty(),
			"子分组应随父级联删除"
		);
	}

	#[test]
	fn known_host_round_trip() {
		let dir = tempfile::TempDir::new().expect("创建临时目录失败");
		let store = Store::open_at(&AppPaths::from_base(dir.path())).expect("开库失败");

		assert!(
			store
				.known_host("10.9.8.7", 22)
				.expect("查询失败")
				.is_none(),
			"未记录时应为 None"
		);

		store
			.remember_host_key("10.9.8.7", 22, "SHA256:testfingerprint")
			.expect("记录失败");

		let record = store
			.known_host("10.9.8.7", 22)
			.expect("查询失败")
			.expect("记录后应能查到");
		assert_eq!(record.fingerprint, "SHA256:testfingerprint");
		assert_eq!(record.host_key, "10.9.8.7:22", "键应带端口");

		assert!(
			store
				.known_host("10.9.8.7", 2222)
				.expect("查询失败")
				.is_none(),
			"不同端口应是另一条记录"
		);
	}

	/// 「重新信任」走的就是这一对：记住 → 忘掉 → 再忘返回 false。
	///
	/// 界面上的按钮只负责递一个 `(地址, 端口)`，真正删库在这里。
	#[test]
	fn rdp_certificate_can_be_forgotten() {
		let dir = tempfile::TempDir::new().expect("创建临时目录失败");
		let store = Store::open_at(&AppPaths::from_base(dir.path())).expect("开库失败");

		assert!(
			store
				.rdp_cert("10.0.0.9", 3389)
				.expect("查询失败")
				.is_none(),
			"还没记录过时应当是 None"
		);

		store
			.remember_rdp_cert("10.0.0.9", 3389, "aabbccdd", None, None)
			.expect("记录失败");
		assert_eq!(
			store.rdp_cert("10.0.0.9", 3389).expect("查询失败"),
			Some("aabbccdd".to_string())
		);

		assert!(
			store.forget_rdp_cert("10.0.0.9", 3389).expect("删除失败"),
			"第一次删应当确实删掉了一条"
		);
		assert!(
			store
				.rdp_cert("10.0.0.9", 3389)
				.expect("查询失败")
				.is_none(),
			"删完之后应当是首连状态"
		);
		assert!(
			!store.forget_rdp_cert("10.0.0.9", 3389).expect("删除失败"),
			"再删一次应当返回 false，而不是报错"
		);
	}
}
