//! 跨模块事件总线。
//!
//! ## 设计取舍
//!
//! - **用回调而非异步 channel**：`daisy-core` 不依赖 tokio，保持轻量；
//!   界面层在每帧开始处 drain 需求由调用方自行决定。
//! - **发布时不持有锁**：先把订阅者快照出来再逐个调用，
//!   因此订阅者内部再次 `publish` 不会死锁。
//! - **订阅走 RAII**：`Subscription` 被 drop 即自动退订，不需要手工注销。

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::model::Protocol;

/// 会话标识。由界面层在创建会话时分配。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(pub u64);

impl fmt::Display for SessionId {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "#{}", self.0)
	}
}

/// 应用内事件。
///
/// 保持精简：只放**跨模块**需要感知的变化。模块内部状态不要做成事件。
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
	/// 主机列表有增删改。
	HostsChanged,
	/// 代理列表有增删改。
	ProxiesChanged,
	/// 分组有增删改。
	GroupsChanged,
	/// 配置被修改。
	ConfigChanged,
	/// 会话已建立。
	SessionOpened {
		/// 会话标识。
		id: SessionId,
		/// 协议类型。
		protocol: Protocol,
		/// 标签页标题。
		title: String,
	},
	/// 会话已关闭。
	SessionClosed {
		/// 会话标识。
		id: SessionId,
	},
	/// 会话出错。
	SessionError {
		/// 会话标识。
		id: SessionId,
		/// 面向用户的错误描述。
		message: String,
	},
	/// 远端分辨率已变更（仅在成功改变远端时发出）。
	ResolutionChanged {
		/// 会话标识。
		id: SessionId,
		/// 新的宽度。
		width: u32,
		/// 新的高度。
		height: u32,
	},
	/// 状态栏提示。
	Status {
		/// 提示文本。
		message: String,
	},
}

type Handler = Arc<dyn Fn(&Event) + Send + Sync>;

/// 事件总线。可自由 `clone`，各副本共享同一套订阅关系。
#[derive(Clone, Default)]
pub struct EventBus {
	inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
	handlers: Mutex<HashMap<u64, Handler>>,
	next_id: AtomicU64,
}

// 订阅者本身是 trait object，无法 derive，只报告数量
impl fmt::Debug for Inner {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("Inner")
			.field("handlers", &lock(&self.handlers).len())
			.finish()
	}
}

/// 取锁，并在遭遇中毒时继续使用内部数据。
///
/// 事件回调不应该让整个总线不可用——中毒只说明某个订阅者曾经 panic。
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
	mutex
		.lock()
		.unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl EventBus {
	/// 建立一条空总线。
	#[must_use]
	pub fn new() -> Self {
		Self::default()
	}

	/// 订阅事件。返回的 [`Subscription`] 被 drop 时自动退订。
	pub fn subscribe<F>(&self, handler: F) -> Subscription
	where
		F: Fn(&Event) + Send + Sync + 'static,
	{
		let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
		lock(&self.inner.handlers).insert(id, Arc::new(handler));
		Subscription {
			inner: Arc::clone(&self.inner),
			id,
		}
	}

	/// 发布事件给当前所有订阅者。
	///
	/// 允许在订阅者内部再次调用 `publish`。
	pub fn publish(&self, event: &Event) {
		let snapshot: Vec<Handler> = {
			let guard = lock(&self.inner.handlers);
			guard.values().cloned().collect()
		};

		for handler in snapshot {
			handler(event);
		}
	}

	/// 当前订阅者数量。
	#[must_use]
	pub fn subscriber_count(&self) -> usize {
		lock(&self.inner.handlers).len()
	}
}

impl fmt::Debug for EventBus {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("EventBus")
			.field("subscribers", &self.subscriber_count())
			.finish()
	}
}

/// 订阅凭据。drop 即退订。
#[derive(Debug)]
pub struct Subscription {
	inner: Arc<Inner>,
	id: u64,
}

impl Drop for Subscription {
	fn drop(&mut self) {
		lock(&self.inner.handlers).remove(&self.id);
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::sync::atomic::AtomicUsize;

	#[test]
	fn publish_reaches_subscriber() {
		let bus = EventBus::new();
		let seen = Arc::new(Mutex::new(Vec::new()));

		let sink = Arc::clone(&seen);
		let _sub = bus.subscribe(move |event| {
			lock(&sink).push(event.clone());
		});

		bus.publish(&Event::HostsChanged);
		bus.publish(&Event::Status {
			message: "ok".to_string(),
		});

		assert_eq!(
			*lock(&seen),
			vec![
				Event::HostsChanged,
				Event::Status {
					message: "ok".to_string()
				}
			]
		);
	}

	#[test]
	fn multiple_subscribers_all_receive() {
		let bus = EventBus::new();
		let count = Arc::new(AtomicUsize::new(0));

		let mut subs = Vec::new();
		for _ in 0..3 {
			let c = Arc::clone(&count);
			subs.push(bus.subscribe(move |_| {
				c.fetch_add(1, Ordering::Relaxed);
			}));
		}

		assert_eq!(bus.subscriber_count(), 3);
		bus.publish(&Event::ConfigChanged);
		assert_eq!(count.load(Ordering::Relaxed), 3);

		drop(subs);
		assert_eq!(bus.subscriber_count(), 0);
	}

	#[test]
	fn dropping_subscription_unsubscribes() {
		let bus = EventBus::new();
		let hits = Arc::new(AtomicUsize::new(0));

		let c = Arc::clone(&hits);
		let sub = bus.subscribe(move |_| {
			c.fetch_add(1, Ordering::Relaxed);
		});

		bus.publish(&Event::HostsChanged);
		assert_eq!(hits.load(Ordering::Relaxed), 1);

		drop(sub);
		bus.publish(&Event::HostsChanged);
		assert_eq!(hits.load(Ordering::Relaxed), 1, "退订后不应再收到事件");
	}

	#[test]
	fn reentrant_publish_does_not_deadlock() {
		// 订阅者内部再次 publish —— 若实现持锁回调就会死锁
		let bus = EventBus::new();
		let depth = Arc::new(AtomicUsize::new(0));

		let inner_bus = bus.clone();
		let d = Arc::clone(&depth);
		let _sub = bus.subscribe(move |event| {
			if *event == Event::HostsChanged {
				let level = d.fetch_add(1, Ordering::Relaxed);
				if level == 0 {
					inner_bus.publish(&Event::ConfigChanged);
				}
			}
		});

		bus.publish(&Event::HostsChanged);
		assert_eq!(depth.load(Ordering::Relaxed), 1);
	}

	#[test]
	fn cloned_bus_shares_subscriptions() {
		let bus = EventBus::new();
		let clone = bus.clone();
		let hits = Arc::new(AtomicUsize::new(0));

		let c = Arc::clone(&hits);
		let _sub = bus.subscribe(move |_| {
			c.fetch_add(1, Ordering::Relaxed);
		});

		clone.publish(&Event::ProxiesChanged);
		assert_eq!(hits.load(Ordering::Relaxed), 1);
		assert_eq!(clone.subscriber_count(), 1);
	}

	#[test]
	fn session_id_displays_with_hash() {
		assert_eq!(SessionId(42).to_string(), "#42");
	}
}
