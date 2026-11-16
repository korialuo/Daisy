-- S8-65：RDP 服务端证书指纹（对应 PLAN R17）。
--
-- 与 known_hosts 平行但**分开建表**：SSH 存的是 `SHA256:base64` 公钥指纹，
-- RDP 存的是证书的 sha256 十六进制串，两者的格式、来源与清理时机都不同，
-- 混在一张表里会让「清除 SSH 记录」顺手抹掉 RDP 的信任关系。
--
-- 键同样带端口：同一台机器的不同端口可能是不同的服务（RDP 改端口很常见）。
CREATE TABLE IF NOT EXISTS rdp_certs (
  host_key    TEXT PRIMARY KEY,
  fingerprint TEXT    NOT NULL,
  subject     TEXT,
  issuer      TEXT,
  added_at    INTEGER NOT NULL
);
