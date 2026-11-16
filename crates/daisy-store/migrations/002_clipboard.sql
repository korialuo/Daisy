-- S8-63：是否映射剪贴板（对应 mstsc 连接前的「剪贴板」勾选项）。
--
-- 默认 1（开）：远程桌面的主要用途就是跨机搬文本。
-- SQLite 的 ALTER TABLE ADD COLUMN 不带 DEFAULT 时会把已有行填成 NULL，
-- 所以这里显式给 DEFAULT 1，老主机升级后直接就是开启状态。
ALTER TABLE hosts ADD COLUMN clipboard INTEGER NOT NULL DEFAULT 1;
