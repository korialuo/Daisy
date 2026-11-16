-- S8-64：远端 DPI 缩放（disp 的 DesktopScaleFactor / DeviceScaleFactor）。
--
-- Retina 上「跟随窗口」把设备像素当分辨率下发（2x 屏 = 逻辑点 ×2），
-- 不带缩放的话 Windows 按 100% 渲染一个超大桌面，文字图标小到看不清。
-- 存百分比（100–500，25 的倍数），默认 100 与 Windows 一致。
ALTER TABLE hosts ADD COLUMN dpi INTEGER NOT NULL DEFAULT 100;
