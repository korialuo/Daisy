# 测试密钥夹具

**这些是一次性生成的、故意入库的弃用密钥。它们不保护任何真实资源，权限为零。**

存在的唯一目的，是让 `crates/daisy-ssh/src/key.rs` 能验证「三种私钥格式各自能解析」——
用真实 `ssh-keygen` / `openssl` 的输出做断言，比用手写字符串可靠得多。

因此 `.gitignore` 里的 `*.pem` / `id_rsa*` 规则对**本目录**做了例外放行。

| 文件 | 生成方式 | 格式 | 口令 |
| --- | --- | --- | --- |
| `openssh_rsa` | `ssh-keygen -t rsa -b 2048 -N ""` | OpenSSH（`BEGIN OPENSSH PRIVATE KEY`） | 无 |
| `openssh_rsa.pub` | 同上 | OpenSSH 公钥 | — |
| `openssh_rsa_encrypted` | `ssh-keygen -t rsa -b 2048 -N "daisy-test-passphrase"` | OpenSSH 加密 | `daisy-test-passphrase` |
| `pkcs1_rsa.pem` | `openssl genrsa -traditional` | PKCS#1（`BEGIN RSA PRIVATE KEY`） | 无 |
| `pkcs8_rsa.pem` | `openssl pkcs8 -topk8 -nocrypt` | PKCS#8（`BEGIN PRIVATE KEY`） | 无 |

注：`pkcs1_rsa.pem` / `pkcs8_rsa.pem` 来自另一对密钥（openssl 生成），
与 `openssh_rsa*` 不是同一对——每个夹具只负责验证「该格式能否解析」。

`openssh_rsa.pub` 的指纹（`SHA256:qL9hx8j8Mde1/o8E263aiN5CeRh52ynxi/kmASCOLvY`）
被单测用来断言「加载后的私钥推导出的公钥与 .pub 文件一致」。
