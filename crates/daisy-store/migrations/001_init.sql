-- Daisy schema v1
-- 与 PLAN.md §5.8 的数据模型保持一致。
--
-- 约定：
--   * 敏感字段一律以密文 BLOB 落库（secret_enc / key_passphrase_enc），
--     命名带 _enc 后缀，提醒调用方这里不是明文
--   * 时间戳统一用 Unix 秒（INTEGER）
--   * 枚举列的 CHECK 约束与 daisy-core 里各枚举的 as_str() 一一对应，
--     改枚举时两边都要动

CREATE TABLE hosts (
  id                 INTEGER PRIMARY KEY,
  name               TEXT    NOT NULL,
  protocol           TEXT    NOT NULL CHECK (protocol IN ('ssh','rdp')),
  host               TEXT    NOT NULL,
  port               INTEGER NOT NULL,
  username           TEXT,
  auth_method        TEXT    NOT NULL DEFAULT 'password'
                             CHECK (auth_method IN ('password','publickey')),
  secret_enc         BLOB,
  key_path           TEXT,
  key_passphrase_enc BLOB,
  proxy_id           INTEGER REFERENCES proxies(id) ON DELETE SET NULL,
  jump_host_id       INTEGER REFERENCES hosts(id)   ON DELETE SET NULL,
  group_id           INTEGER REFERENCES groups(id)  ON DELETE SET NULL,
  resolution         TEXT,
  scale_mode         TEXT    CHECK (scale_mode IS NULL
                                    OR scale_mode IN ('fit','fill','actual','custom')),
  scale_factor       REAL,
  extra              TEXT,
  created_at         INTEGER NOT NULL,
  updated_at         INTEGER NOT NULL
);

CREATE TABLE proxies (
  id         INTEGER PRIMARY KEY,
  name       TEXT    NOT NULL,
  kind       TEXT    NOT NULL CHECK (kind IN ('socks5','http')),
  host       TEXT    NOT NULL,
  port       INTEGER NOT NULL,
  username   TEXT,
  secret_enc BLOB
);

CREATE TABLE groups (
  id        INTEGER PRIMARY KEY,
  name      TEXT    NOT NULL,
  parent_id INTEGER REFERENCES groups(id) ON DELETE CASCADE
);

CREATE TABLE settings (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE known_hosts (
  host_key    TEXT PRIMARY KEY,
  fingerprint TEXT    NOT NULL,
  added_at    INTEGER NOT NULL
);

CREATE INDEX idx_hosts_group  ON hosts(group_id);
CREATE INDEX idx_hosts_proxy  ON hosts(proxy_id);
CREATE INDEX idx_hosts_jump   ON hosts(jump_host_id);
CREATE INDEX idx_groups_parent ON groups(parent_id);
