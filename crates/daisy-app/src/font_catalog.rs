//! 系统字体清单。
//!
//! 设置里要能选「终端字体」，选项得有东西可选——这里把系统装了的字体扫一遍，
//! 拿到 family 名、文件路径与 `ttc` 内的序号。
//!
//! ## 为什么只读表头
//!
//! macOS 的 `/System/Library/Fonts` 有 560MB 以上，单个 `AppleSDGothicNeo.ttc`
//! 就 53MB。为了拿一个家族名把每个文件整读一遍，启动会卡好几秒。
//!
//! sfnt 的表目录在文件开头，`name` 与 `post` 两张表都只有几十到几百字节，
//! 所以这里按「表目录 → 只 seek 读那两张表」走：373 个字体文件加起来读不到 1MB。
//!
//! ## 等宽怎么判
//!
//! 只看 `post` 表的 `isFixedPitch`（表头偏移 12..16）。它由字体作者声明，
//! 主流等宽字体（Hack / Menlo / Courier New / Consolas）都标了 1。
//! 漏标的会被归到「非等宽」——那只是排序靠后，仍然可选，最终还有一道实测兜底
//! （见 [`crate::fonts::install_fonts`] 的返回值）。

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// 一个可选字体。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemFont {
	/// 家族名（`name` 表里的 Family，取不到时退回 Typographic Family）。
	pub family: String,
	/// 字体文件。
	pub path: PathBuf,
	/// 文件内的序号。`ttc` 里一个文件装多个字体，`ttf` 恒为 0。
	pub index: u32,
	/// 作者声明的等宽标记（或实测结果）。
	pub monospaced: bool,
	/// 是不是这个家族的正体（Regular / Book / Normal）。
	///
	/// 同一个家族常常拆成几个文件（`Courier New.ttf` 与 `Courier New Bold.ttf`），
	/// 去重时优先留正体，否则用户选的「Courier New」可能是粗体那份。
	pub regular: bool,
}

/// 系统字体全表（惰性扫描，进程内只扫一次）。
#[must_use]
pub fn all() -> &'static [SystemFont] {
	static CACHE: OnceLock<Vec<SystemFont>> = OnceLock::new();
	CACHE.get_or_init(scan_all)
}

/// 按家族名找字体。找不到返回 `None`，调用方回退到内置字体。
#[must_use]
pub fn find(family: &str) -> Option<&'static SystemFont> {
	all().iter().find(|font| font.family == family)
}

/// 扫描所有字体目录。
fn scan_all() -> Vec<SystemFont> {
	// family 作键去重：同一个家族在多个文件里出现时保留先扫到的
	let mut found: BTreeMap<String, SystemFont> = BTreeMap::new();

	for dir in font_dirs() {
		collect(&dir, 2, &mut found);
	}

	let mut list: Vec<SystemFont> = found.into_values().collect();
	// 等宽排前面——终端字体基本只会在这批里选
	list.sort_by(|a, b| {
		b.monospaced
			.cmp(&a.monospaced)
			.then_with(|| a.family.to_lowercase().cmp(&b.family.to_lowercase()))
	});
	tracing::info!(
		"系统字体：共 {} 个（等宽 {} 个）",
		list.len(),
		list.iter().filter(|font| font.monospaced).count()
	);
	list
}

/// 递归收集目录里的字体。`depth` 是还能往下走几层。
fn collect(dir: &Path, depth: usize, out: &mut BTreeMap<String, SystemFont>) {
	let Ok(entries) = std::fs::read_dir(dir) else {
		return;
	};

	for entry in entries.flatten() {
		let path = entry.path();
		if path.is_dir() {
			if depth > 0 {
				collect(&path, depth - 1, out);
			}
			continue;
		}
		if !is_font_file(&path) {
			continue;
		}

		for face in read_faces(&path) {
			// 以 `.` 开头的家族是系统的私有字体（`.SF NS Mono`、`.Apple Color Emoji UI`、
			// 各种 `.ADT *` / `.Al * PUA`）：界面上不该让用户选，选了也没有预期效果
			if face.family.starts_with('.') {
				continue;
			}
			// 同一家族常有多个字重（Regular/Bold/Italic）分布在几个文件里，
			// 抢到名字就先登记者可能是个 Bold——**Regular 优先**
			match out.get(&face.family) {
				Some(existing) if existing.regular || !face.regular => {},
				_ => {
					out.insert(face.family.clone(), face);
				},
			}
		}
	}
}

/// 扩展名判断。`.otc` 也是字体集合，与 `.ttc` 同构。
fn is_font_file(path: &Path) -> bool {
	path.extension()
		.and_then(|ext| ext.to_str())
		.is_some_and(|ext| {
			matches!(
				ext.to_ascii_lowercase().as_str(),
				"ttf" | "ttc" | "otf" | "otc"
			)
		})
}

/// 各平台的字体目录。平台差异只集中在这里。
fn font_dirs() -> Vec<PathBuf> {
	let mut dirs = Vec::new();

	if cfg!(target_os = "windows") {
		let windir = std::env::var_os("WINDIR").map_or_else(|| PathBuf::from("C:/Windows"), PathBuf::from);
		dirs.push(windir.join("Fonts"));
		// 当前用户装的字体（免管理员安装，Office 之类常放这里）
		if let Some(appdata) = std::env::var_os("LOCALAPPDATA") {
			dirs.push(PathBuf::from(appdata).join("Microsoft/Windows/Fonts"));
		}
	} else if cfg!(target_os = "macos") {
		dirs.push(PathBuf::from("/System/Library/Fonts"));
		dirs.push(PathBuf::from("/System/Library/Fonts/Supplemental"));
		dirs.push(PathBuf::from("/Library/Fonts"));
		if let Some(home) = std::env::var_os("HOME") {
			dirs.push(PathBuf::from(home).join("Library/Fonts"));
		}
	} else {
		dirs.push(PathBuf::from("/usr/share/fonts"));
		dirs.push(PathBuf::from("/usr/local/share/fonts"));
		if let Some(home) = std::env::var_os("HOME") {
			let home = PathBuf::from(home);
			dirs.push(home.join(".local/share/fonts"));
			dirs.push(home.join(".fonts"));
		}
	}

	dirs
}

/// 读一个字体文件里的所有 face。
fn read_faces(path: &Path) -> Vec<SystemFont> {
	let Ok(mut file) = File::open(path) else {
		return Vec::new();
	};
	let Some(offsets) = face_offsets(&mut file) else {
		return Vec::new();
	};

	let mut out = Vec::new();
	for (index, offset) in offsets {
		if let Some((family, subfamily, monospaced)) = read_face(&mut file, offset) {
			let regular = is_regular(&subfamily);
			out.push(SystemFont {
				family,
				path: path.to_path_buf(),
				index,
				monospaced,
				regular,
			});
		}
	}
	out
}

/// `name` 表里的子家族名（Regular / Bold / Italic…）判断正体。
///
/// 少数老字体写 `Book` / `Normal` / `Roman`，一并认。
fn is_regular(subfamily: &str) -> bool {
	let name = subfamily.trim().to_lowercase();
	if name.is_empty() {
		return false;
	}
	// "Bold Italic" 这类组合里含 bold/italic 就不是正体
	if name.contains("bold") || name.contains("italic") || name.contains("oblique") {
		return false;
	}
	matches!(name.as_str(), "regular" | "book" | "normal" | "roman")
}

/// 文件里每个 face 的表目录起点。
///
/// `ttcf` 是字体集合（一个文件多个字体），其余按单字体处理。
fn face_offsets(file: &mut File) -> Option<Vec<(u32, u64)>> {
	let mut header = [0u8; 12];
	file.seek(SeekFrom::Start(0)).ok()?;
	file.read_exact(&mut header).ok()?;

	if &header[0..4] != b"ttcf" {
		return Some(vec![(0, 0)]);
	}

	let count = u32::from_be_bytes([header[8], header[9], header[10], header[11]]) as usize;
	// 字体集合的 face 数不可能很多；给个上限防止坏文件让我们分配一大块
	if count == 0 || count > 512 {
		return None;
	}

	let mut raw = vec![0u8; count * 4];
	file.read_exact(&mut raw).ok()?;

	let offsets = raw
		.as_chunks::<4>()
		.0
		.iter()
		.enumerate()
		.map(|(index, chunk)| {
			(
				index as u32,
				u64::from(u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])),
			)
		})
		.collect();

	Some(offsets)
}

/// 从一个 face 的表目录里取家族名、子家族名与等宽标记。
fn read_face(file: &mut File, offset: u64) -> Option<(String, String, bool)> {
	file.seek(SeekFrom::Start(offset)).ok()?;

	let mut header = [0u8; 12];
	file.read_exact(&mut header).ok()?;
	let num_tables = u16::from_be_bytes([header[4], header[5]]) as usize;
	if num_tables == 0 || num_tables > 512 {
		return None;
	}

	let mut dir = vec![0u8; num_tables * 16];
	file.read_exact(&mut dir).ok()?;

	let mut tables = TableDir::default();
	for chunk in dir.as_chunks::<16>().0 {
		let tag = &chunk[0..4];
		let table_offset = u32::from_be_bytes([chunk[8], chunk[9], chunk[10], chunk[11]]);
		let length = u32::from_be_bytes([chunk[12], chunk[13], chunk[14], chunk[15]]);
		match tag {
			b"name" => tables.name = Some((table_offset, length)),
			b"post" => tables.post = Some((table_offset, length)),
			b"cmap" => tables.cmap = Some((table_offset, length)),
			b"hhea" => tables.hhea = Some((table_offset, length)),
			b"hmtx" => tables.hmtx = Some((table_offset, length)),
			_ => {},
		}
	}

	let (name_offset, name_length) = tables.name?;
	let name_data = read_table(file, name_offset, name_length, 1_048_576)?;
	// 家族名优先取 Typographic Family(16)：老式命名会把字重塞进 Family，
	// 出现 "Arial Bold" 这种「家族」，同一字体就被拆成好几项
	let family = name_string(&name_data, &[16, 1])?;
	let subfamily = name_string(&name_data, &[17, 2]).unwrap_or_default();

	// 先看 `post` 表声明的 `isFixedPitch`（表头偏移 12..16，Fixed 16.16）：
	// 命中就不用再读别的表了
	if tables.fixed_pitch(file) {
		return Some((family, subfamily, true));
	}

	// 声明为 0 的不可全信——macOS 的 Monaco 明明等宽却标 0。
	// 退一步实测几个字符的前进宽度，一致才算等宽。
	let monospaced = tables.metrics_agree(file);
	Some((family, subfamily, monospaced))
}

/// 一个 face 里我们关心的几张表的位置。
#[derive(Debug, Default)]
struct TableDir {
	name: Option<(u32, u32)>,
	post: Option<(u32, u32)>,
	cmap: Option<(u32, u32)>,
	hhea: Option<(u32, u32)>,
	hmtx: Option<(u32, u32)>,
}

impl TableDir {
	/// `post.isFixedPitch`。缺表或读不出来按「不是」处理。
	fn fixed_pitch(&self, file: &mut File) -> bool {
		let Some((offset, length)) = self.post else {
			return false;
		};
		if length < 16 {
			return false;
		}
		let mut post = [0u8; 16];
		if file.seek(SeekFrom::Start(u64::from(offset))).is_err()
			|| file.read_exact(&mut post).is_err()
		{
			return false;
		}
		u32::from_be_bytes([post[12], post[13], post[14], post[15]]) != 0
	}

	/// 实测：`M` / `i` / `W` / `.` 的前进宽度是否全相同。
	fn metrics_agree(&self, file: &mut File) -> bool {
		let (Some((cmap_at, cmap_len)), Some((hhea_at, hhea_len)), Some((hmtx_at, hmtx_len))) =
			(self.cmap, self.hhea, self.hmtx)
		else {
			return false;
		};

		// 几张表都不大：cmap 几十 KB，hmtx 与字形数成正比
		let Some(cmap) = read_table(file, cmap_at, cmap_len, 2_000_000) else {
			return false;
		};
		let Some(hhea) = read_table(file, hhea_at, hhea_len, 64) else {
			return false;
		};
		if hhea.len() < 36 {
			return false;
		}
		let metrics_count = u16::from_be_bytes([hhea[34], hhea[35]]) as usize;
		if metrics_count == 0 {
			return false;
		}
		let Some(hmtx) = read_table(file, hmtx_at, hmtx_len, 8_000_000) else {
			return false;
		};

		let advance = |glyph: u16| -> Option<u16> {
			// 超过 numberOfHMetrics 的字形沿用最后一条记录
			let index = (glyph as usize).min(metrics_count - 1);
			let at = index * 4;
			let pair = hmtx.get(at..at + 2)?;
			Some(u16::from_be_bytes([pair[0], pair[1]]))
		};

		let mut widths = [0u16; 4];
		for (slot, ch) in widths.iter_mut().zip(['M', 'i', 'W', '.']) {
			let Some(glyph) = cmap_glyph(&cmap, ch) else {
				return false;
			};
			let Some(width) = advance(glyph) else {
				return false;
			};
			*slot = width;
		}
		widths.iter().all(|width| *width == widths[0])
	}
}

/// 读一张表。长度超上限当作坏文件，不读。
fn read_table(file: &mut File, offset: u32, length: u32, max: u32) -> Option<Vec<u8>> {
	if length == 0 || length > max {
		return None;
	}
	let mut data = vec![0u8; length as usize];
	file.seek(SeekFrom::Start(u64::from(offset))).ok()?;
	file.read_exact(&mut data).ok()?;
	Some(data)
}

/// 在 `cmap` 表里查一个字符的字形编号。
///
/// 只认 `format 4`（BMP）与 `format 12`（含增补平面）——覆盖了所有常见字体。
/// 子表优先选 Windows Unicode（3/10、3/1），其次 Unicode 平台（0/x）。
fn cmap_glyph(cmap: &[u8], ch: char) -> Option<u16> {
	if cmap.len() < 4 {
		return None;
	}
	let count = u16::from_be_bytes([cmap[2], cmap[3]]) as usize;

	let mut best: Option<(u8, usize)> = None;
	for index in 0..count {
		let record = 4 + index * 8;
		let head = cmap.get(record..record + 8)?;
		let platform = u16::from_be_bytes([head[0], head[1]]);
		let encoding = u16::from_be_bytes([head[2], head[3]]);
		let offset = u32::from_be_bytes([head[4], head[5], head[6], head[7]]) as usize;

		let rank = match (platform, encoding) {
			(3, 10) => 0,
			(3, 1) => 1,
			(0, _) => 2,
			_ => continue,
		};
		if best.is_none_or(|(best_rank, _)| rank < best_rank) {
			best = Some((rank, offset));
		}
	}

	let (_, offset) = best?;
	let sub = cmap.get(offset..)?;
	let format = u16::from_be_bytes([*sub.first()?, *sub.get(1)?]);
	let code = u32::from(ch);

	match format {
		4 => glyph_from_format4(sub, code),
		12 => glyph_from_format12(sub, code),
		_ => None,
	}
}

fn glyph_from_format4(sub: &[u8], code: u32) -> Option<u16> {
	if code > 0xFFFF {
		return None;
	}
	let code = code as u16;
	let seg_count = u16::from_be_bytes([*sub.get(6)?, *sub.get(7)?]) as usize / 2;
	if seg_count == 0 {
		return None;
	}

	let end_at = 14;
	let start_at = end_at + seg_count * 2 + 2;
	let delta_at = start_at + seg_count * 2;
	let range_at = delta_at + seg_count * 2;

	for index in 0..seg_count {
		let end = u16::from_be_bytes([*sub.get(end_at + index * 2)?, *sub.get(end_at + index * 2 + 1)?]);
		if code > end {
			continue;
		}
		let start =
			u16::from_be_bytes([*sub.get(start_at + index * 2)?, *sub.get(start_at + index * 2 + 1)?]);
		if code < start {
			return None;
		}
		let delta = i16::from_be_bytes([
			*sub.get(delta_at + index * 2)?,
			*sub.get(delta_at + index * 2 + 1)?,
		]);
		let range_offset = u16::from_be_bytes([
			*sub.get(range_at + index * 2)?,
			*sub.get(range_at + index * 2 + 1)?,
		]);

		if range_offset == 0 {
			return Some((i32::from(code) + i32::from(delta)) as u16);
		}

		let at = range_at
			+ index * 2
			+ range_offset as usize
			+ (code - start) as usize * 2;
		let glyph = u16::from_be_bytes([*sub.get(at)?, *sub.get(at + 1)?]);
		if glyph == 0 {
			return None;
		}
		return Some((i32::from(glyph) + i32::from(delta)) as u16);
	}

	None
}

fn glyph_from_format12(sub: &[u8], code: u32) -> Option<u16> {
	let groups = u32::from_be_bytes([
		*sub.get(12)?,
		*sub.get(13)?,
		*sub.get(14)?,
		*sub.get(15)?,
	]) as usize;

	for index in 0..groups {
		let at = 16 + index * 12;
		let start = u32::from_be_bytes([*sub.get(at)?, *sub.get(at + 1)?, *sub.get(at + 2)?, *sub.get(at + 3)?]);
		let end = u32::from_be_bytes([*sub.get(at + 4)?, *sub.get(at + 5)?, *sub.get(at + 6)?, *sub.get(at + 7)?]);
		let glyph = u32::from_be_bytes([
			*sub.get(at + 8)?,
			*sub.get(at + 9)?,
			*sub.get(at + 10)?,
			*sub.get(at + 11)?,
		]);
		if (start..=end).contains(&code) {
			return u16::try_from(glyph + (code - start)).ok();
		}
	}

	None
}

/// 从 `name` 表里取一条字符串。
///
/// `ids` 是要的名字编号，按优先级排列：`[1, 16]` 取家族名（Family / Typographic
/// Family），`[2, 17]` 取子家族名（Regular / Bold…）。同一编号有多条语言版本时，
/// 优先 Windows 平台英文——各平台字体里它最稳定。
fn name_string(data: &[u8], ids: &[u16]) -> Option<String> {
	if data.len() < 6 {
		return None;
	}
	let count = u16::from_be_bytes([data[2], data[3]]) as usize;
	let string_offset = u16::from_be_bytes([data[4], data[5]]) as usize;

	let mut best: Option<(u8, String)> = None;
	for index in 0..count {
		let record = 6 + index * 12;
		if record + 12 > data.len() {
			break;
		}
		let chunk = &data[record..record + 12];
		let platform = u16::from_be_bytes([chunk[0], chunk[1]]);
		let encoding = u16::from_be_bytes([chunk[2], chunk[3]]);
		let language = u16::from_be_bytes([chunk[4], chunk[5]]);
		let name_id = u16::from_be_bytes([chunk[6], chunk[7]]);
		let length = u16::from_be_bytes([chunk[8], chunk[9]]) as usize;
		let offset = u16::from_be_bytes([chunk[10], chunk[11]]) as usize;

		let Some(id_rank) = ids.iter().position(|id| *id == name_id) else {
			continue;
		};
		let start = string_offset + offset;
		let Some(raw) = data.get(start..start + length) else {
			continue;
		};

		let text = match (platform, encoding) {
			// Windows 平台：UTF-16BE
			(3, _) => utf16be(raw),
			// Macintosh 平台：单字节，名字基本是 ASCII
			(1, _) => raw.iter().map(|byte| *byte as char).collect::<String>(),
			_ => continue,
		};
		let text = text.trim().to_string();
		if text.is_empty() {
			continue;
		}

		let locale_rank = if platform == 3 && language == 0x0409 {
			0
		} else if platform == 3 {
			1
		} else {
			2
		};
		let score = (id_rank as u8) * 4 + locale_rank;

		if best.as_ref().is_none_or(|(best_score, _)| score < *best_score) {
			best = Some((score, text));
		}
	}

	best.map(|(_, text)| text)
}

fn utf16be(bytes: &[u8]) -> String {
	let units: Vec<u16> = bytes
		.as_chunks::<2>()
		.0
		.iter()
		.map(|pair| u16::from_be_bytes(*pair))
		.collect();
	String::from_utf16_lossy(&units)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn font_dirs_are_absolute() {
		for dir in font_dirs() {
			assert!(dir.is_absolute(), "字体目录应当是绝对路径：{}", dir.display());
		}
	}

	#[test]
	fn font_extensions_are_recognised() {
		assert!(is_font_file(Path::new("a.ttf")));
		assert!(is_font_file(Path::new("a.TTC")));
		assert!(is_font_file(Path::new("a.otf")));
		assert!(!is_font_file(Path::new("a.txt")));
		assert!(!is_font_file(Path::new("a")));
	}

	/// 造一张最小的 `name` 表来验解析，不依赖本机字体。
	#[test]
	fn family_name_prefers_windows_english() {
		let mut data = Vec::new();
		let records = 2u16;
		let string_offset = 6 + records * 12;
		data.extend_from_slice(&0u16.to_be_bytes()); // format
		data.extend_from_slice(&records.to_be_bytes());
		data.extend_from_slice(&string_offset.to_be_bytes());

		// 记录 1：Mac 平台，家族名 "MacFamily"（8 字节）
		let mac = b"MacFamily";
		push_record(&mut data, 1, 0, 0, 1, mac.len(), 0);
		// 记录 2：Windows 英文，家族名 "WinFamily"（UTF-16BE，18 字节）
		let win: String = "WinFamily".to_string();
		let win_bytes: Vec<u8> = win
			.encode_utf16()
			.flat_map(u16::to_be_bytes)
			.collect();
		push_record(&mut data, 3, 1, 0x0409, 1, win_bytes.len(), mac.len());
		data.extend_from_slice(mac);
		data.extend_from_slice(&win_bytes);

		assert_eq!(name_string(&data, &[1, 16]).as_deref(), Some("WinFamily"));
		assert_eq!(name_string(&data, &[2, 17]), None, "这条表里没有子家族名");
	}

	#[test]
	fn regular_subfamilies_are_recognised() {
		assert!(is_regular("Regular"));
		assert!(is_regular("Book"));
		assert!(is_regular("Normal"));
		assert!(is_regular("Roman"));
		assert!(!is_regular("Bold"));
		assert!(!is_regular("Bold Italic"));
		assert!(!is_regular("Italic"));
		assert!(!is_regular("Oblique"));
		assert!(!is_regular(""));
	}

	fn push_record(
		out: &mut Vec<u8>,
		platform: u16,
		encoding: u16,
		language: u16,
		name_id: u16,
		length: usize,
		offset: usize,
	) {
		out.extend_from_slice(&platform.to_be_bytes());
		out.extend_from_slice(&encoding.to_be_bytes());
		out.extend_from_slice(&language.to_be_bytes());
		out.extend_from_slice(&name_id.to_be_bytes());
		out.extend_from_slice(&(length as u16).to_be_bytes());
		out.extend_from_slice(&(offset as u16).to_be_bytes());
	}

	/// 本机至少能扫出一批字体，且里面有等宽的。
	///
	/// 这条只能在有字体目录的机器上跑；容器里没有字体就跳过。
	#[test]
	fn scan_finds_monospaced_fonts_on_this_host() {
		let fonts = all();
		if fonts.is_empty() {
			return;
		}
		// `--nocapture` 时用来核对实际扫到了什么
		eprintln!(
			"扫描到 {} 个字体，前 12 个等宽的：{:?}",
			fonts.len(),
			fonts
				.iter()
				.take(12)
				.map(|font| font.family.as_str())
				.collect::<Vec<_>>()
		);
		assert!(
			fonts.iter().any(|font| font.monospaced),
			"系统里应当至少有一个等宽字体"
		);
		// 排序规则：等宽的都在前面
		let first_not_mono = fonts.iter().position(|font| !font.monospaced);
		if let Some(index) = first_not_mono {
			assert!(
				fonts[index..].iter().all(|font| !font.monospaced),
				"等宽字体应当排在前面"
			);
		}
	}
}
