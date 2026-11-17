//! 中文字体安装与终端字体选择。
//!
//! **egui 自带字体不覆盖中日韩字形。** 不装中文字体的话，界面上所有中文
//! 都会渲染成方块（tofu），例如顶部状态栏的「本地终端」会变成「□□□□」。
//!
//! 终端还能另选一款**系统字体**当等宽族（设置里的「终端字体」），来源见
//! [`crate::font_catalog`]。选中的字体排在最前，egui 自带的 Hack 与中文字体
//! 依次回退——终端按格定位，中文那一份绝不能顶掉西文。
//!
//! 字体文件名与路径各平台不同，按 PLAN.md 8.1 的 D3，平台差异集中在**本文件**，
//! 其它地方不应该出现字体路径。

use std::sync::Arc;

/// 注册到 egui 里的字体名。
const FONT_KEY: &str = "daisy-cjk";

/// 终端等宽中文的注册名，与正文那个分开，免得两个族抢同一个 key。
const MONO_KEY: &str = "daisy-cjk-mono";

/// 用户在设置里选的终端字体。
const MONO_USER_KEY: &str = "daisy-terminal-font";

/// 密码框专用字体族的名字。
///
/// 单独一个族，是因为**只有密码框**需要窄的掩码点：中文把 `•` 排成满格，
/// 正文里其它地方并不需要这个改正（见 [`BULLET_SOURCE`]）。
pub const PASSWORD_FAMILY: &str = "daisy-password";

/// 掩码点从**这一份字体**里取，而不是中文字体。
///
/// 中文字体给 `•` 的前进宽度是满格（冬青黑体实测 13px/13px = 1em），
/// 一串密码就散成「• • • •」，在 280 宽的框里只放得下十几个点。
/// 这份拉丁字体的同名字形只有 **0.32em**，密度才是密码框该有的样子。
///
/// 它是 egui 自带字体里的一份，字节已经在二进制里，不需要额外读文件。
const BULLET_SOURCE: &str = "Ubuntu-Light";

/// 掩码字体的注册名。
///
/// 另起一个 `FontData` 实例（不是复用 `BULLET_SOURCE` 那个）：垂直改正要挂
/// 在 `FontData` 上，而复用会把改正带给「中文字体缺字时回落」的那些符号。
const BULLET_KEY: &str = "daisy-bullet";

/// 量「墨迹相对行框偏多少」时用的字号。
///
/// 与正文同号（`design::FONT_BASE`）：改正量按 em 取比例，用它量出来的比例
/// 直接适用于其它字号；用 13 是因为界面文字绝大多数是这个字号，量它最准。
const PROBE_SIZE: f32 = 13.0;

/// 用来探测垂直度量的字。选「国」而不是「口」：它上下都有笔画，
/// 墨迹的上下边界才是真实的中文满格范围。
const PROBE_CHAR: char = '国';

/// 各平台的中文字体候选，按优先级排列。
///
/// 每项是 `(路径, 字体集合内的序号)`。`ttc` 是字体集合，序号通常取 0。
fn candidates() -> &'static [(&'static str, u32)] {
	if cfg!(target_os = "windows") {
		&[
			("C:/Windows/Fonts/msyh.ttc", 0),   // 微软雅黑
			("C:/Windows/Fonts/simhei.ttf", 0), // 黑体
			("C:/Windows/Fonts/simsun.ttc", 0), // 宋体
		]
	} else if cfg!(target_os = "macos") {
		&[
			("/System/Library/Fonts/Hiragino Sans GB.ttc", 0), // 冬青黑体简体中文
			("/System/Library/Fonts/STHeiti Light.ttc", 0),    // 华文黑体
			("/System/Library/Fonts/Supplemental/Arial Unicode.ttf", 0),
		]
	} else {
		// 本项目不发布 Linux 版，这里只是开发期兜底
		&[
			("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", 0),
			("/usr/share/fonts/truetype/wqy/wqy-microhei.ttc", 0),
		]
	}
}

/// 终端用的**等宽**中文字体候选。
///
/// 为什么单列一份：界面正文用雅黑这类比例字体好看，但终端里每个字符占
/// 几格必须可预测，比例字体会让中英混排的表格错位。
///
/// 只有 Windows 有现成的等宽中文：`simsun.ttc` 里第 1 个是关键——
/// `SimSun`（宋体，0）对拉丁是比例的，`NSimSun`（新宋体，1）才是等宽的。
///
/// macOS / Linux 返回空表，沿用上面的比例字体当等宽族的回退——那里的
/// 终端对齐本来就靠字体自身的等宽西文，中文宽度问题两个平台一样存在。
fn monospace_candidates() -> &'static [(&'static str, u32)] {
	if cfg!(target_os = "windows") {
		&[("C:/Windows/Fonts/simsun.ttc", 1)]
	} else {
		&[]
	}
}

/// 安装字体之后的结果。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct InstalledFonts {
	/// 实际用上的中文字体路径。`None` 表示一个都没找到（界面中文会是方块）。
	pub cjk_path: Option<String>,
	/// 终端字体没按预期装上时要告诉用户的一句话。
	pub warning: Option<String>,
}

/// 安装中文字体，并按需把用户选的终端字体挂到等宽族首位。
///
/// `mono_family` 是设置里选的家族名（见 [`crate::font_catalog`]）；
/// `None` 表示用内置默认（egui 自带的 Hack + 中文回退）。
///
/// # 为什么正文族把它放在**首位**
///
/// 放末尾当回退的话，拉丁字符仍由 egui 自带的细体渲染、中文走这个字体，
/// 同一行里「Windows」和「测试机」是两种字重——看着像排版出错。
/// 放首位后拉丁也用它的字形，一行内字重一致。
///
/// 等宽族例外：中文字体不是等宽的，放首位会让终端的字符列宽全乱，只能做回退。
/// 若有专门的等宽中文字体（Windows 的 NSimSun），等宽族就挂它而不是这个。
pub fn install_fonts(ctx: &egui::Context, mono_family: Option<&str>) -> InstalledFonts {
	let loaded = load_first_available();

	let mut fonts = egui::FontDefinitions::default();
	// 密码族的装法与本机有没有中文字体无关：**它必须存在**，
	// 否则 `FontId::new(.., FontFamily::Name(..))` 会在 epaint 里直接 panic。
	install_password_family(&mut fonts);

	let (path, bytes, index) = match loaded {
		Some(loaded) => loaded,
		None => {
			// 没有中文字体时界面本来就是方块，但**密码族仍要装**：
			// 密码框的族名找不到会在 epaint 里 panic，比方块严重得多
			let warning = install_terminal_font(&mut fonts, mono_family);
			ctx.set_fonts(fonts);
			return InstalledFonts {
				cjk_path: None,
				warning,
			};
		},
	};

	// 界面文字要「墨迹居中」，见 [`y_offset_factor`]；终端按格绘制，绝不能带这个改正
	let factor = y_offset_factor(bytes, index, PROBE_SIZE);
	tracing::info!("中文垂直度量改正：y_offset_factor = {factor:.3} em");

	// UI 与终端**共用同一份字形数据**（`FontTweak` 挂在 `FontData` 上，两处要不同
	// 的值就只能有两个实例）。字节用 `&'static [u8]` 借出去，22MB 的字体文件不会
	// 因为要两个 tweak 就复制一遍。
	fonts.font_data.insert(
		FONT_KEY.to_owned(),
		Arc::new(font_data(bytes, index, factor)),
	);

	fonts
		.families
		.entry(egui::FontFamily::Proportional)
		.or_default()
		.insert(0, FONT_KEY.to_owned());

	// 密码族的兜底挂上中文：输入法组字时显示的是未掩码的原文
	fonts
		.families
		.entry(password_family())
		.or_default()
		.push(FONT_KEY.to_owned());

	// 等宽族：优先挂专门的等宽中文（Windows 的 NSimSun）；没有就用同一份数据退回。
	// **两个都不带改正**：终端是按格定位的，整体下移半个字高会顶到下一行。
	let mono = match load_first_of(monospace_candidates()) {
		Some((mono_path, mono_bytes, mono_index)) => {
			tracing::info!("已加载终端等宽中文字体：{mono_path}");
			font_data(mono_bytes, mono_index, 0.0)
		},
		None => font_data(bytes, index, 0.0),
	};
	fonts.font_data.insert(MONO_KEY.to_owned(), Arc::new(mono));
	fonts
		.families
		.entry(egui::FontFamily::Monospace)
		.or_default()
		.push(MONO_KEY.to_owned());

	let warning = install_terminal_font(&mut fonts, mono_family);

	ctx.set_fonts(fonts);
	tracing::info!("已加载中文字体：{path}");
	InstalledFonts {
		cjk_path: Some(path),
		warning,
	}
}

/// 把用户选的终端字体挂到等宽族**首位**。
///
/// 顺序是「用户选的 → egui 自带的 Hack / Ubuntu → 中文回退」。终端按格定位，
/// 中文字体是最后一道兜底，不能顶掉西文——它的字形不是等宽的。
///
/// 返回要提示给用户的一句话（选错字体、读不出来）；没问题时是 `None`。
fn install_terminal_font(
	fonts: &mut egui::FontDefinitions,
	mono_family: Option<&str>,
) -> Option<String> {
	let family = mono_family?;

	let Some(font) = crate::font_catalog::find(family) else {
		return Some(format!("找不到字体「{family}」，终端已回退到内置等宽字体"));
	};

	let bytes = match std::fs::read(&font.path) {
		Ok(bytes) => bytes,
		Err(error) => {
			return Some(format!("读取字体「{family}」失败：{error}"));
		},
	};
	let leaked: &'static [u8] = Box::leak(bytes.into_boxed_slice());

	fonts.font_data.insert(
		MONO_USER_KEY.to_owned(),
		Arc::new(font_data(leaked, font.index, 0.0)),
	);
	fonts
		.families
		.entry(egui::FontFamily::Monospace)
		.or_default()
		.insert(0, MONO_USER_KEY.to_owned());

	tracing::info!(
		"终端字体：{}（{}，{}）",
		family,
		font.path.display(),
		if font.monospaced {
			"等宽"
		} else {
			"非等宽"
		}
	);

	// 不拦着用户选比例字体，但要说清楚代价：终端按「一个字符一格」记账，
	// 比例字体的字符宽窄不一，中英混排会错位
	if font.monospaced {
		None
	} else {
		Some(format!("「{family}」不是等宽字体，终端里中英混排会错位"))
	}
}

/// 装「密码掩码」字体族。
///
/// 与中文字体是否可用无关：这条路必须走通，否则密码框拿到一个不存在
/// 的族名会在 epaint 里 panic（`FontFamily::Name(..) is not bound to any fonts`）。
fn install_password_family(fonts: &mut egui::FontDefinitions) {
	let mut chain = Vec::new();
	if let Some(bullet) = fonts.font_data.get(BULLET_SOURCE).cloned() {
		// 复制的是 `FontData` 而不是字节：它的 `font` 字段是 `Cow::Borrowed`
		// （编在二进制里的静态切片），克隆只拷指针，不会复制 360KB。
		//
		// **这一族不挂 `y_offset_factor`**，与中文那一族不同：点的墨迹本来就落在
		// 行框正中（实测行框 15、墨迹中心 7.5），而按轮廓算的改正量对 3px 的
		// 小字形不准——`px_bounds` 是整像素对齐的，算出来的量又被 epaint
		// `round_ui()` 取整，反而把点顶偏 1pt（见 `password_dots_...` 用例）。
		let bullet = (*bullet).clone();
		fonts
			.font_data
			.insert(BULLET_KEY.to_owned(), Arc::new(bullet));
		chain.push(BULLET_KEY.to_owned());
	} else {
		// 理论上不可能（这份字体编在二进制里）；真缺了就让密码框退回中文的宽点，
		// 丑但能输入
		tracing::warn!("找不到密码掩码字体 {BULLET_SOURCE}，密码点会退回全角宽度");
	}
	fonts.families.insert(password_family(), chain);
}

/// 密码族的 `FontFamily` 键。
fn password_family() -> egui::FontFamily {
	egui::FontFamily::Name(PASSWORD_FAMILY.into())
}

/// 密码输入框该用的字体。
///
/// 掩码点来自 [`PASSWORD_FAMILY`] 里的拉丁字形（0.32em），比中文的满格窄一半多。
#[must_use]
pub fn password_font(size: f32) -> egui::FontId {
	egui::FontId::new(size, password_family())
}

/// 组一份字体数据；`factor` 见 [`y_offset_factor`]。
fn font_data(bytes: &'static [u8], index: u32, factor: f32) -> egui::FontData {
	egui::FontData {
		font: std::borrow::Cow::Borrowed(bytes),
		index,
		tweak: egui::epaint::text::FontTweak {
			y_offset_factor: factor,
			..Default::default()
		},
	}
}

/// 让中文字形的**墨迹**落在行框中间所需的垂直改正量（em 的比例，正数向下）。
///
/// # 为什么需要这个
///
/// epaint 把「行框中心」当作垂直中心，可中文字体的行框是 `ascent + |descent|`
/// 那么高（实测 13px 的中文字体行框 19.5），而汉字的墨迹只占约 13px，且贴行框
/// 顶部：**墨迹中心比行框中心高 3.25pt**。所有控件的文字都是「行框居中」，
/// 于是每一处文字都整体偏高——单看一处只是「有点飘」，整屏扫下来就是明确的
/// 「文字没垂直居中」。
///
/// 改正量按 em 取，所以 11px 的小字与 15px 的标题会拿到成比例的量。
/// 用字体自身的度量算（`ascent` / `descent` / 某个汉字的轮廓），不写死平台——
/// Windows 的微软雅黑与 macOS 的冬青黑体度量并不相同。
///
/// 量不出来（非 ttc / 缺少度量）时返回 0：宁可不改，也不乱改。
fn y_offset_factor(bytes: &[u8], index: u32, size: f32) -> f32 {
	use ab_glyph::Font as _;

	let Ok(font) = ab_glyph::FontRef::try_from_slice_and_index(bytes, index) else {
		return 0.0;
	};
	let upm = font.units_per_em().unwrap_or(0.0);
	if upm <= 0.0 {
		return 0.0;
	}
	let to_px = size / upm;
	// **行框高度必须带 `line_gap`**：epaint 用的是 `ascent - descent + line_gap`
	// （`text/font.rs`）。冬青黑体的 line_gap 是 0.5em，漏掉它算出来 13、实际 19.5，
	// 改正量会连正负都反。
	let ascent_px = font.ascent_unscaled() * to_px;
	let row_px = ascent_px - font.descent_unscaled() * to_px + font.line_gap_unscaled() * to_px;
	if row_px <= 0.0 {
		return 0.0;
	}

	// 探针字的墨迹上下边界（相对基线，向上为负）
	let glyph = font
		.glyph_id(PROBE_CHAR)
		.with_scale(ab_glyph::PxScale::from(size));
	let Some(outline) = font.outline_glyph(glyph) else {
		return 0.0;
	};
	let bounds = outline.px_bounds();
	// 相对基线、**向下为正**：中文墨迹在基线之上，中心是负值
	let ink_center_from_baseline = (bounds.min.y + bounds.max.y) * 0.5;
	// 从行框顶往下量：先走 `ascent` 到基线，再加（负的）墨迹偏移
	let ink_center_from_top = ascent_px + ink_center_from_baseline;
	let shift = row_px * 0.5 - ink_center_from_top;

	// 夹一下：真出问题时（度量异常）最多挪 1/4 字高，不至于把文字推出控件
	(shift / size).clamp(-0.25, 0.25)
}

/// 从候选表里挑第一个能读到的字体。返回的字节是**借出去不再回收**的。
///
/// `Box::leak` 在这里是刻意的：UI 与终端要两份 `FontData` 但共用同一段字节，
/// 而 `Cow<'static, [u8]>` 要求 `'static`。进程内字体只加载一次，泄露的量固定。
fn load_first_of(table: &[(&'static str, u32)]) -> Option<(String, &'static [u8], u32)> {
	for (path, index) in table {
		match std::fs::read(path) {
			Ok(bytes) => {
				let leaked: &'static [u8] = Box::leak(bytes.into_boxed_slice());
				return Some(((*path).to_string(), leaked, *index));
			},
			Err(error) => {
				tracing::debug!("跳过字体 {path}：{error}");
			},
		}
	}
	None
}

fn load_first_available() -> Option<(String, &'static [u8], u32)> {
	if let Some(found) = load_first_of(candidates()) {
		return Some(found);
	}

	tracing::warn!(
		"未找到可用的中文字体，界面中文将显示为方块。已尝试：{:?}",
		candidates()
			.iter()
			.map(|(path, _)| *path)
			.collect::<Vec<_>>()
	);
	None
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn candidates_are_not_empty() {
		assert!(!candidates().is_empty(), "每个平台都应至少有一个候选字体");
	}
	#[test]
	fn candidates_are_absolute_paths() {
		for (path, _) in candidates() {
			assert!(
				path.starts_with('/') || path.contains(":/"),
				"字体路径应当是绝对路径：{path}"
			);
		}
	}

	/// 等宽中文只在 Windows 上有现成的（NSimSun）；其它平台留空走回退。
	#[test]
	fn monospace_candidates_follow_the_platform() {
		if cfg!(target_os = "windows") {
			assert!(
				!monospace_candidates().is_empty(),
				"Windows 应有等宽中文候选"
			);
		} else {
			assert!(
				monospace_candidates().is_empty(),
				"非 Windows 平台不该凭空指向一个不存在的等宽中文"
			);
		}
	}

	#[test]
	fn monospace_candidates_are_absolute_paths() {
		for (path, _) in monospace_candidates() {
			assert!(
				path.starts_with('/') || path.contains(":/"),
				"字体路径应当是绝对路径：{path}"
			);
		}
	}

	/// 开发机上至少应能找到一个候选字体。
	///
	/// 这条在 CI 的 macOS runner 上同样成立；如果失败，说明字体候选清单需要更新。
	#[test]
	fn a_font_is_available_on_this_host() {
		let loaded = load_first_available();
		assert!(loaded.is_some(), "当前机器上找不到任何候选中文字体");

		let (path, bytes, _) = loaded.expect("已确认有值");
		assert!(!bytes.is_empty(), "字体文件不该为空：{path}");
	}

	/// 算出来的改正量必须真的把墨迹挪到行框中间。
	///
	/// 这是本轮修的「文字不垂直居中」的核心断言：**裁判是 egui 自己排出来的版**，
	/// 不是我按字体度量推的公式。规则如果和 epaint 的实际算法走散（换字体、
	/// 升级 epaint），这条会先红。
	#[test]
	fn y_offset_factor_centers_the_ink() {
		let Some((_path, bytes, index)) = load_first_available() else {
			return;
		};

		// 不改正之前，中文墨迹一定偏上；改完必须基本居中
		let raw = y_offset_factor(bytes, index, PROBE_SIZE);
		assert!(raw > 0.05, "中文墨迹偏上，改正量应当是正的，实际 {raw}");

		let ctx = egui::Context::default();
		let mut fonts = egui::FontDefinitions::default();
		fonts.font_data.insert(
			"probe".to_owned(),
			std::sync::Arc::new(font_data(bytes, index, raw)),
		);
		fonts
			.families
			.entry(egui::FontFamily::Proportional)
			.or_default()
			.insert(0, "probe".to_owned());
		ctx.set_fonts(fonts);

		let mut input = egui::RawInput {
			screen_rect: Some(egui::Rect::from_min_size(
				egui::Pos2::ZERO,
				egui::vec2(400.0, 300.0),
			)),
			..Default::default()
		};
		// 字体要跑过一帧才建好
		let mut warmup = ctx.run_ui(std::mem::take(&mut input), |_ui| {});
		warmup.textures_delta.clear();

		let galley = ctx.fonts_mut(|f| {
			f.layout(
				"国".to_owned(),
				egui::FontId::proportional(PROBE_SIZE),
				egui::Color32::WHITE,
				f32::INFINITY,
			)
		});
		let shift = galley.size().y * 0.5 - galley.mesh_bounds.center().y;
		assert!(
			shift.abs() < 0.6,
			"改正后墨迹仍偏 {shift:.2}pt（行框 {}，墨迹 {}）",
			galley.size().y,
			galley.mesh_bounds.height()
		);
	}

	/// 密码点必须比中文字体排出来的窄，且仍然垂直居中。
	///
	/// 「窄」是这条用例的**唯一目的**：中文字体给 `•` 的前进宽度是满格 1em，
	/// 一串密码就散成「• • • •」，用户的原话是「密码的点间距小一些」。
	/// 只断言宽度比例——具体数值随字体变，比例错了才是回归。
	#[test]
	fn password_dots_are_narrower_than_full_width() {
		let ctx = egui::Context::default();
		let mut fonts = egui::FontDefinitions::default();
		install_password_family(&mut fonts);

		// 中文字体也装上，才有「同一个点在中文字体里多宽」可比
		let cjk = load_first_available();
		let has_cjk = cjk.is_some();
		if let Some((_path, bytes, index)) = cjk {
			fonts.font_data.insert(
				FONT_KEY.to_owned(),
				std::sync::Arc::new(font_data(bytes, index, 0.0)),
			);
			fonts
				.families
				.entry(egui::FontFamily::Proportional)
				.or_default()
				.insert(0, FONT_KEY.to_owned());
			fonts
				.families
				.entry(password_family())
				.or_default()
				.push(FONT_KEY.to_owned());
		}
		ctx.set_fonts(fonts);

		let mut input = egui::RawInput {
			screen_rect: Some(egui::Rect::from_min_size(
				egui::Pos2::ZERO,
				egui::vec2(400.0, 300.0),
			)),
			..Default::default()
		};
		let mut warmup = ctx.run_ui(std::mem::take(&mut input), |_ui| {});
		warmup.textures_delta.clear();

		let sample = "•".repeat(10);
		let rows = ctx.fonts_mut(|f| {
			let password = f.layout(
				sample.clone(),
				password_font(PROBE_SIZE),
				egui::Color32::WHITE,
				f32::INFINITY,
			);
			let normal = f.layout(
				sample.clone(),
				egui::FontId::proportional(PROBE_SIZE),
				egui::Color32::WHITE,
				f32::INFINITY,
			);
			(password, normal)
		});

		let (password, normal) = rows;
		if has_cjk {
			assert!(
				password.size().x < normal.size().x * 0.6,
				"密码点应当明显窄于全角：{} vs {}",
				password.size().x,
				normal.size().x
			);
		}

		// 垂直仍要居中：掩码族的 `FontTweak` 是按 `•` 自己算的
		let shift = password.size().y * 0.5 - password.mesh_bounds.center().y;
		assert!(
			shift.abs() < 0.6,
			"密码点没垂直居中，偏 {shift:.2}pt（行框 {}，墨迹 {}）",
			password.size().y,
			password.mesh_bounds.height()
		);
	}
}
