---
id: 34
priority: P2
type: bug
status: resolved
created: 2026-05-17
related_pr: 197
resolved_pr: 197
resolved_date: 2026-05-17
---

# 客户端 latest.json + installer RSA 验签(签了但客户端不验)

## 触发上下文

2026-05-17 调研 AiMaMi "自动更新 + 签名校验"是否值得借鉴时,grok 总结称"本项目 release/ 目录已实现 Ed25519 签名"。实际核对发现两处事实错误:

1. **算法不是 Ed25519,是 RSA-3072 PKCS#1-v1.5-SHA256** —— `xtask/src/release_bundle.rs:178` `"algorithm": "RSA-PKCS1-V15-SHA256"`,line 26-27 引用 `rsa = "0.9"` + `Pkcs1v15Sign::new::<Sha256>()`
2. **客户端代码完全不验签 `.sig`** —— `src-tauri/src/admin/handlers/update.rs` 全文 grep `signature` / `\.sig` / `verify` 命中数为 0;只 SHA256 校验(`update.rs:422-432`)

`release/Codex-App-Transfer-release-public.pem` 是 RSA-3072 公钥(PEM 头 `MIIBojANBgkqhkiG9w0BAQEFAA` 对应 PKCS#8 RSA OID),CI 走 `RELEASE_PRIVATE_KEY_PEM` secret(`.github/workflows/release.yml:373-383`) → `xtask release-bundle`(`release.yml:391-396`)输出 base64 raw signature。

## 问题描述

### 现状

`update.rs:308-343 check_update_impl`:
- 下载 `latest.json` → 解析 platforms/assets/version
- **不下载 `latest.json.sig`,不验签**

`update.rs:391-432 download_asset_impl`:
- 下载 installer bytes 到 partial
- 计算 sha256 → 跟 latest.json 里 `asset.sha256` 字段比对
- **不下载对应 `.sig`,不验签**

### 期望

客户端把 `.sig` 跟 `latest.json` / installer 一起下,用 build-time embedded 的 RSA 公钥 verify;验签失败硬 fail,不 fallback 到 sha256-only。

### 差距 = 攻击面

只要 attacker 能 MITM `https://github.com/Cmochance/.../latest.json` 响应(用户企业代理 / 公共 wifi / DNS 劫持 / GitHub 仓库被改名),改 `url` 指向恶意 binary + 同步改 `sha256` 字段 → 客户端 update flow 直接下载执行任意 .pkg / .exe / .dmg。

公钥 PEM 已经做出来放在 release/ 目录里,签名 CI 已经在生产,**所有基础设施都在,只差客户端验签**。

## 已有调研

### 现有签名生成流程

`.github/workflows/release.yml:373-396`:
```
KEY: ${{ secrets.RELEASE_PRIVATE_KEY_PEM }}
  → .release-signing/release-private-key.pem (chmod 600)
  → cargo run -p xtask --release -- release-bundle --version v.. --include macos linux windows
```

`xtask/src/release_bundle.rs`:
- L26-27: `use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey, EncodePublicKey, LineEnding}; use rsa::{Pkcs1v15Sign, RsaPrivateKey, RsaPublicKey};`
- L178: latest.json 嵌入字段:
  ```json
  "signature": {
    "algorithm": "RSA-PKCS1-V15-SHA256",
    "public_key": PUBLIC_KEY_BASENAME,
    "format": "base64 raw signature over file bytes"
  }
  ```
- L187: `sign_file(&latest_path, &private_key)?;` — latest.json 自身也签
- L282-290: `sign_file` 实现 `private_key.sign(Pkcs1v15Sign::new::<Sha256>(), &hashed)` + base64
- L453-457: 自带 round-trip 单测验证签名 deterministic

### 验证体现

`release/Codex-App-Transfer-v1.0.3-macOS-arm64.dmg.sig` xxd 头部 `bK3oLRJ/K1PO1W14...` — base64 字符,长度 512 char(对应 RSA-3072 = 384 bytes raw),跟 release_bundle.rs 描述 byte-for-byte 一致。

### 公钥分发

`release/Codex-App-Transfer-release-public.pem` 已经跟 .sig 一起上传 release。**理论上**客户端可以从 latest.json 旁边的 .pem 拉公钥,但**这本身就是 MITM 攻击面**(替换公钥 + 替换 .sig 就能伪造) → 必须 **build-time embed** 公钥到 binary,不能动态拉。

## 风险 / 不确定性

### Key compromise / rotation

公钥嵌入 binary 后,如果将来 private key 泄露 / 需要换 key,**已发布所有版本的客户端都需要新一次升级才能信任新 key**。建议:
- 在 binary 里嵌入"主 key + N 个候选 key"列表(只要任一 verify 通过即可),为将来 rotation 留余地
- 或者改用证书 chain(过度工程化,先 P2 不做)

### 过渡期问题

v2.1.x 已经在用户机器上的旧版本**没有验签代码**。如果攻击者在新版本(带验签)上线后 24h 内推假 latest.json:
- 旧 v2.1.x 用户的 update.rs 不验签 → 仍可被骗下载假 installer
- 新版本 user 不受影响

mitigation: 把"建议尽快升级到 vX.Y.Z (verify enabled)" 单独发一次推送或 release notes 顶部置顶;不解决根本问题,只缩小窗口。

### Linux / 其他平台

`update.rs:136-144 allowed_install_extensions` 当前只允许 Windows .exe / macOS .pkg .dmg;Linux 走 download-only(`update.rs:143`),没 install 流程。验签**对 Linux 同样应该加**(用户手动安装也希望验签,但目前 download_asset_impl 是 macOS/Windows-only path) → 单独 follow-up 或本任务一起。

### 算法选择

不推荐改 Ed25519(虽然 AiMaMi 通过 tauri-plugin-updater 走 minisign Ed25519 是行业更新趋势):
- CI 端 `xtask/src/release_bundle.rs` 已经稳定跑 RSA-3072,迁移 Ed25519 要重新生成 key pair + 历史发布签名失效
- ROI 偏低:RSA-3072 PKCS1-v15 至 2030 年前都不存在已知现实破解

## 建议方向

下次接手按这个顺序:

1. **build-time embed 公钥** —— `crates/registry/build.rs` 或 `src-tauri/build.rs` 加 `include_str!("../release/Codex-App-Transfer-release-public.pem")`,导出为常量 `RELEASE_PUBLIC_KEY_PEM`
2. **新增 `verify_file(path, sig_b64, pub_key) -> Result<()>`** —— 反向写 `release_bundle.rs:282-290` 的 sign_file,用 `Pkcs1v15Sign::new::<Sha256>().verify(&public_key, &hashed, &sig_bytes)`
3. **fetch_latest_json 同时拉 .sig** —— `update.rs:278-306` 改成:
   ```rust
   let json_bytes = fetch_bytes(client, url).await?;
   let sig_bytes = fetch_bytes(client, &format!("{}.sig", url)).await?;
   verify_signature(&json_bytes, &sig_bytes, RELEASE_PUBLIC_KEY_PEM)?;
   ```
4. **download_asset_impl 同时拉 installer .sig** —— `update.rs:391-432` 在 sha256 通过后再加一次 RSA 验签,失败硬 fail + 删 partial
5. **测试**: 给 mock latest.json + 故意篡改字节 verify 必须 fail;sign 一份 known 输入用 release_bundle.rs round-trip 单测确保两边算法一致(`release_bundle.rs:453-457` 已有 sign side,只缺 verify side)
6. **release notes** 单独 1 段强调"v?.?.? 起客户端开始验签,建议尽快升级"

## 关联资源

- 上游借鉴(类似机制但算法不同):
  - AiMaMi `/private/tmp/AiMaMi/src-tauri/Cargo.toml:29` `tauri-plugin-updater = "2"`(Ed25519 + minisign 默认)
  - AiMaMi `/private/tmp/AiMaMi/src-tauri/tauri.conf.json plugins: {}` —— **AiMaMi 自己 plugin endpoint 也没接通**,纯 stub
- 触发调研:2026-05-17 用户问 grok 总结的 AiMaMi 借鉴清单
- 代码锚点:
  - `xtask/src/release_bundle.rs:26-27, 178, 282-290, 453-457` 签名生成 + round-trip 单测
  - `src-tauri/src/admin/handlers/update.rs:278-343 / 391-447 / 422-432` 当前 update flow + sha256-only 校验
  - `.github/workflows/release.yml:373-396` CI 签名链路
  - `release/Codex-App-Transfer-release-public.pem` 公钥
  - `release/latest.json.sig` / `release/*.sig` 现有签名样本(可作单测 fixture)
- RFC / 标准:RFC 8017 PKCS#1 v2.2(RSASSA-PKCS1-v1.5)
