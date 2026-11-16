//! 帧数据转换：FreeRDP 的 XRGB32 缓冲 → 紧凑 RGBA。
//!
//! 这一层**不碰 egui**——`daisy-rdp` 是协议层，纹理上传由 `daisy-app` 做。
//! 分开的好处是转换逻辑（行宽处理、通道顺序、alpha 补齐）能单测，
//! 不必拉起窗口。
//!
//! ## 两个容易踩的点
//!
//! 1. **行宽不等于 `width × 4`**：FreeRDP 的 `gdi->stride` 常带对齐填充，
//!    按行拷贝时必须用 stride 定位、只取每行前 `width × 4` 字节。
//! 2. **像素格式名按内存字节序命名**：`XRGB32` 在内存里就是 `X, R, G, B`，
//!    所以 R 在下标 1、G 在 2、B 在 3，下标 0 那个字节丢弃。

use crate::freerdp::FrameBuffer;

/// 已转成紧凑 RGBA 的一帧。
///
/// 与 [`FrameBuffer`] 的区别：没有行尾填充，alpha 已补齐，
/// 可以直接交给 GPU 上传。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RgbaFrame {
	/// 宽度（像素）。
	pub width: u32,
	/// 高度（像素）。
	pub height: u32,
	/// 紧凑 RGBA 字节，长度 `width * height * 4`。
	pub pixels: Vec<u8>,
}

impl RgbaFrame {
	/// 一帧占用的字节数。
	#[must_use]
	pub fn byte_len(&self) -> usize {
		self.pixels.len()
	}
}

/// 把 XRGB32 帧转成紧凑 RGBA。
///
/// **数据不足时返回 `None`**：宁可这一帧不画（屏幕上还是上一帧），
/// 也不要画半张。之前是「截断补透明」，真机上看起来就是**画面被撕开**：
/// 上半是新的、下半直接黑掉——比丢一帧难看得多。
/// 行数据不足通常发生在分辨率刚变、`gdi` 缓冲正在重建的那一瞬。
#[must_use]
pub fn to_rgba(frame: &FrameBuffer) -> Option<RgbaFrame> {
	let width = frame.width as usize;
	let height = frame.height as usize;
	let row_bytes = width * 4;
	let stride = frame.stride as usize;

	if width == 0 || height == 0 {
		return Some(RgbaFrame {
			width: frame.width,
			height: frame.height,
			pixels: Vec::new(),
		});
	}
	// 整块数据不够就整帧作废（不要分段画）
	let needed = stride
		.checked_mul(height - 1)
		.and_then(|head| head.checked_add(row_bytes))?;
	if stride < row_bytes || frame.data.len() < needed {
		return None;
	}

	let mut pixels = vec![0u8; row_bytes * height];
	for y in 0..height {
		let src_start = y * stride;
		let src_row = &frame.data[src_start..src_start + row_bytes];
		let dst_start = y * row_bytes;
		let dst_row = &mut pixels[dst_start..dst_start + row_bytes];

		// as_chunks 而不是 chunks_exact：长度在类型上已知，省掉每次迭代的边界检查
		for (src_px, dst_px) in src_row
			.as_chunks::<4>()
			.0
			.iter()
			.zip(dst_row.as_chunks_mut::<4>().0.iter_mut())
		{
			dst_px[0] = src_px[1];
			dst_px[1] = src_px[2];
			dst_px[2] = src_px[3];
			dst_px[3] = 255;
		}
	}

	Some(RgbaFrame {
		width: frame.width,
		height: frame.height,
		pixels,
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	/// 造一个 2×2 的 XRGB32 帧，`stride` 可指定以模拟行尾填充。
	fn frame_with_stride(stride: u32) -> FrameBuffer {
		// 像素依次为 红、绿、蓝、白（X 字节填 0xAA 以确认它被丢弃）
		let rows: [[u8; 8]; 2] = [
			[0xAA, 255, 0, 0, 0xAA, 0, 255, 0],
			[0xAA, 0, 0, 255, 0xAA, 255, 255, 255],
		];
		let mut data = Vec::new();
		for row in rows {
			data.extend_from_slice(&row);
			// 补到 stride
			data.resize(data.len() + (stride as usize - 8), 0xEE);
		}
		FrameBuffer {
			width: 2,
			height: 2,
			stride,
			data,
		}
	}

	#[test]
	fn converts_channels_in_memory_order() {
		let rgba = to_rgba(&frame_with_stride(8)).expect("完整帧应能转换");

		assert_eq!(rgba.width, 2);
		assert_eq!(rgba.height, 2);
		assert_eq!(rgba.byte_len(), 2 * 2 * 4);

		// 红
		assert_eq!(&rgba.pixels[0..4], &[255, 0, 0, 255]);
		// 绿
		assert_eq!(&rgba.pixels[4..8], &[0, 255, 0, 255]);
		// 蓝
		assert_eq!(&rgba.pixels[8..12], &[0, 0, 255, 255]);
		// 白
		assert_eq!(&rgba.pixels[12..16], &[255, 255, 255, 255]);
	}

	#[test]
	fn drops_the_padding_x_byte() {
		let rgba = to_rgba(&frame_with_stride(8)).expect("完整帧应能转换");
		// X 字节是 0xAA，若被当成某个通道就会出现在结果里
		assert!(
			!rgba
				.pixels
				.as_chunks::<4>()
				.0
				.iter()
				.any(|px| px[0..3].contains(&0xAA)),
			"X 字节不应进入 RGB 任一通道"
		);
	}

	#[test]
	fn handles_stride_larger_than_row() {
		// 行尾 5 字节填充：如果按 stride 当行宽用，第二行就会整体错位
		let rgba = to_rgba(&frame_with_stride(13)).expect("带填充的完整帧应能转换");

		assert_eq!(rgba.byte_len(), 16, "输出必须是紧凑的");
		assert_eq!(&rgba.pixels[0..4], &[255, 0, 0, 255], "首行首像素");
		assert_eq!(&rgba.pixels[8..12], &[0, 0, 255, 255], "次行首像素应是蓝");
	}

	#[test]
	fn alpha_is_opaque_everywhere() {
		let rgba = to_rgba(&frame_with_stride(8)).expect("完整帧应能转换");
		assert!(
			rgba.pixels.as_chunks::<4>().0.iter().all(|px| px[3] == 255),
			"桌面画面不该有透明像素"
		);
	}

	#[test]
	fn short_frame_is_dropped_instead_of_drawn_half() {
		// 只有一行数据却声称两行：整帧作废，调用方保留上一帧
		let frame = FrameBuffer {
			width: 2,
			height: 2,
			stride: 8,
			data: vec![0; 8],
		};
		assert!(to_rgba(&frame).is_none(), "数据不足时不该画半张画面");
	}

	#[test]
	fn stride_smaller_than_row_is_rejected() {
		// stride 比一行还短：按它取行会读到别的行，画面会错位
		let frame = FrameBuffer {
			width: 4,
			height: 2,
			stride: 8,
			data: vec![0; 32],
		};
		assert!(to_rgba(&frame).is_none(), "stride < width*4 必须拒绝");
	}

	#[test]
	fn zero_sized_frame_is_empty() {
		let frame = FrameBuffer {
			width: 0,
			height: 0,
			stride: 0,
			data: Vec::new(),
		};
		let rgba = to_rgba(&frame).expect("零尺寸帧不该被当成畸形");
		assert!(rgba.pixels.is_empty());
	}
}
