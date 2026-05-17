---
id: 37
priority: P3
type: refactor
status: resolved
created: 2026-05-17
related_pr: 199
resolved_pr: 199
resolved_date: 2026-05-17
---

# update.rs download_asset_impl: in-memory bytes 防 TOCTOU + 重 add bad-sha256 mismatch 单测

## 触发上下文

PR #196 (followup #34 实施) 3-agent review 派生:

- code-reviewer `IMPORTANT-2`: 删除 update_download_rejects_bad_sha_and_unsupported_platform 时把 bad-sha 子测一并删了, update.rs:467-470 `installer checksum mismatch, install cancelled` 不再有专属测试。
- silent-failure-hunter `IMPORTANT-2`: TOCTOU window 在 file_sha256(`update.rs:460`) 跟 fs::read(`update.rs:482`) 之间有一次 sig fetch network await。Linux 共享 /tmp 场景下不可信。

两项都不是 BLOCKER (RSA verify 仍是 trust gate 不被 TOCTOU 绕过, bad-sha 是 redundant defense), 留独立 PR 做。

## 问题描述

### 现状

`src-tauri/src/admin/handlers/update.rs:441-489` `download_asset_impl`:

1. 流式 download → 写 partial file (L441-453)
2. `let installer_sig = fetch_signature_text(...)` (L468, **network await**)
3. `let installer_bytes = fs::read(&partial)` (L477, 二次读取 file)
4. `verify_signed_bytes(&installer_bytes, &installer_sig)` (L482)
5. `file_sha256(&partial)` (L491, 第三次读取)
6. sha256 比对 (L498-503)

### 期望

- 累积流式 chunk 同时写 partial 跟 in-memory Vec<u8>
- 一份 bytes 同时给 sha256 + verify_signed_bytes,避免二次读取 + TOCTOU window
- partial file 只用于"verify pass 后 rename 到 target"
- 新增单测: latest.json 中 asset.sha256 = "bad" → 期望 `installer checksum mismatch, install cancelled` Err (在 RSA verify 通过的前提下,即用真签名的 installer + 故意写 wrong sha256 到 latest.json — 这要求 mock server 同时 serve 真 sig + 改过的 latest.json,但真 latest.json 被签名锁定不能改 → 简化: 用 临时 keypair 签 mock latest.json 才能改 sha256 字段,但 verify_signed_bytes 用 embedded official key 会 fail. → 真正可行: dependency-injectable verifier or signature.rs 暴露 `cfg(test)` 的 verify_with_key)

### 差距

- in-memory 改动 30-50 行,但需要重写 download_result async block
- bad-sha test 需要 signature.rs 暴露 `verify_with_key(data, sig, pub_pem)` for test-only injection (生产仍走 verify_signed_bytes 嵌入 key);或者用真 release/Codex-App-Transfer-v1.0.3-macOS-arm64.dmg + .sig 但 commit 35MB 文件不合适

## 已有调研

### TOCTOU 实际威胁面

- macOS: temp_dir = `/var/folders/<UID>/T/` per-user 0700 → 攻击者(非同 user)不能 swap
- Windows: %TEMP% 默认 per-user → 同 mitigation
- Linux: 默认 /tmp 共享 + 1777 sticky → 同主机其他 user **可以** swap `cas-update-*/updates/*.download`
- 攻击者要 read+write 我们的 partial → 必须 same user 或 root,实际门槛高

完整 fix 价值: 中等 (Linux multi-user shared-host 服务器场景实际罕见, desktop OS 主流)。

### bad-sha test 实施门槛

- 简单方案: 让 signature.rs 暴露 `verify_with_pubkey_pem(data, sig, pem)` (test 用临时 key 签)
- 风险: 扩 API surface,生产代码可能误用 (引入 trust anchor bypass)
- mitigation: `#[cfg(test)] pub(crate) fn verify_with_pubkey_pem(...)` 仅 test crate 可见

## 风险 / 不确定性

- in-memory 35MB peak — desktop Tauri 通常 RAM ample (默认 GB+),但 mobile 部署或 Wasm 时是限制
- bad-sha test mock 用 临时 key 签 mock data → 测的是"假设 verify 通过后 sha256 mismatch path",对真实生产 trust gate 验证作用有限 — 但仍 catch sha256 校验逻辑 regression

## 建议方向

下次接手按这个顺序:

1. **重写 download_result async block** 为同时写 disk + 累积 in-memory:
   ```rust
   let mut in_memory = Vec::with_capacity(64 * 1024 * 1024);  // 64MB 预留 max
   while let Some(chunk) = response.chunk().await? {
       in_memory.extend_from_slice(&chunk);
       file.write_all(&chunk)?;
   }
   ```
2. **改 verify_signed_bytes(&in_memory, &sig)** 用同一份 bytes,不再 fs::read
3. **改 file_sha256 → in_memory_sha256(&in_memory)**
4. **新增 cfg(test) verify_with_pubkey_pem** 在 signature.rs:
   ```rust
   #[cfg(test)]
   pub(crate) fn verify_with_pubkey_pem(
       data: &[u8],
       sig_b64: &str,
       pubkey_pem: &str,
   ) -> Result<(), VerifyError> { ... }
   ```
5. **更新 update.rs 单测** 加一个 mock-server-based bad-sha test 用临时 key 签 mock data,inject 临时 pubkey
6. **README 不需要改** (no behavior change for user)
7. **NIT (code-reviewer NIT-3)**: fetch_signature_text error message 改 `format!("HTTP {}", status())` 不 leak full URL

## 关联资源

- 触发 PR: #196 (followup #34 实施 — RSA verify 接入)
- review agent findings:
  - code-reviewer IMPORTANT-1 (顺序) — **已在 PR #196 修**, RSA 先 sha256 后
  - code-reviewer IMPORTANT-2 (bad-sha test 丢) — **本 followup 处理**
  - code-reviewer NIT-3 (URL leak) — **本 followup 顺手处理**
  - silent-failure-hunter IMPORTANT-1 (remove_file warn) — **已在 PR #196 修**, 5 处加 tracing::warn!
  - silent-failure-hunter IMPORTANT-2 (TOCTOU + in-memory) — **本 followup 处理**
  - type-design 重命名 SignatureMismatch → SignatureRejected — **已在 PR #196 修**
- 代码锚点:
  - `src-tauri/src/admin/handlers/update.rs:441-503` download_asset_impl 当前结构
  - `src-tauri/src/admin/handlers/update.rs:108-122` file_sha256 (streaming)
  - `src-tauri/src/admin/signature.rs:71-83` verify_signed_bytes
