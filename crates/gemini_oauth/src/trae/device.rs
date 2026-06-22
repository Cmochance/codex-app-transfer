//! Trae `DeviceInfo`(12 字段)+ 每账号合成设备指纹。
//!
//! ## 指纹隔离(用户硬性需求)
//!
//! 同一台设备登录多个 Trae 账号时**绝不共用一套指纹**,防字节风控关联账号。
//! 落地:每个账号(= 一个 provider 条目)首登时 [`DeviceFingerprint::generate`]
//! 一套**独立**的 `DeviceID` / `MachineID` / `DeviceName` + 从可信池里按账号选一款
//! 看似真实的 Mac 机型 / CPU,落盘进凭证包后固定复用(refresh / quota 都用同一套)。
//! 切账号 = 整包切换。`DeviceID` / `MachineID` 是服务端绑定的主标识,逐账号随机即
//! 实现隔离;机型 / CPU / OS 取**可信值**(乱填会触发"不可能硬件"风控)但逐账号
//! 微调,进一步弱化关联。
//!
//! 真实 `DeviceInfo` 还带 `DevicePublicKey`(SPKI PEM,见 [`super::crypto`]),由
//! [`DeviceFingerprint::to_device_info`] 在发请求时拼入。

use serde::{Deserialize, Serialize};

use super::constants::TraeProviderConfig;

/// 可信的 Apple Silicon Mac 机型池(逐账号选一,弱化硬件关联)。
const MAC_MODELS: &[(&str, &str)] = &[
    ("MacBookPro18,1", "Apple M1 Pro"),
    ("MacBookPro18,3", "Apple M1 Pro"),
    ("MacBookPro18,2", "Apple M1 Max"),
    ("Mac14,7", "Apple M2"),
    ("Mac14,9", "Apple M2 Pro"),
    ("Mac15,3", "Apple M3"),
    ("Mac15,7", "Apple M3 Pro"),
    ("Mac16,1", "Apple M4"),
];

/// 可信 macOS 版本池。
const OS_VERSIONS: &[&str] = &["14.5", "14.6", "15.0", "15.1", "15.2"];

/// 一个账号的合成设备指纹(持久化进凭证包,首登生成、固定复用)。
///
/// 不含密钥对([`super::crypto::DeviceKeyPair`] 单独存)与 token;
/// [`to_device_info`](Self::to_device_info) 在发请求时把这些 + 公钥 + config
/// 拼成发往 Trae 的 [`DeviceInfo`]。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceFingerprint {
    pub device_id: String,
    pub machine_id: String,
    pub device_name: String,
    pub device_model: String,
    pub device_brand: String,
    pub device_cpu: String,
    pub os_info: String,
    pub os_version: String,
}

impl DeviceFingerprint {
    /// 生成一套全新的独立指纹(每账号一次)。
    pub fn generate() -> Result<Self, super::crypto::CryptoError> {
        let device_id = uuid_v4()?;
        let machine_id = random_hex_32()?;
        // 从池里按随机字节选机型 / OS(逐账号不同但都可信)
        let (model, cpu) = MAC_MODELS[pick_index(MAC_MODELS.len())?];
        let os_version = OS_VERSIONS[pick_index(OS_VERSIONS.len())?];
        // DeviceName:可信的 "<adjective> 的 MacBook Pro" 风格,带随机后缀去重
        let suffix = &uuid_v4()?[..4];
        let device_name = format!("MacBook-Pro-{suffix}");
        Ok(Self {
            device_id,
            machine_id,
            device_name,
            device_model: model.to_string(),
            device_brand: "Apple".to_string(),
            device_cpu: cpu.to_string(),
            os_info: "macOS".to_string(),
            os_version: os_version.to_string(),
        })
    }

    /// 拼成发往 Trae 的 [`DeviceInfo`] wire 对象(补 config 相关字段 + 公钥)。
    pub fn to_device_info(
        &self,
        config: &TraeProviderConfig,
        device_public_key_spki_pem: &str,
    ) -> DeviceInfo {
        DeviceInfo {
            device_id: self.device_id.clone(),
            machine_id: self.machine_id.clone(),
            platform_code: config.platform_code.to_string(),
            device_type: "PC".to_string(),
            device_name: self.device_name.clone(),
            device_model: self.device_model.clone(),
            client_version: config.ide_version.to_string(),
            device_public_key: device_public_key_spki_pem.to_string(),
            device_brand: self.device_brand.clone(),
            device_cpu: self.device_cpu.clone(),
            os_info: self.os_info.clone(),
            os_version: self.os_version.clone(),
        }
    }
}

/// 发往 Trae ExchangeToken 的 `DeviceInfo`(12 字段,PascalCase 对齐 wire)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    #[serde(rename = "DeviceID")]
    pub device_id: String,
    #[serde(rename = "MachineID")]
    pub machine_id: String,
    #[serde(rename = "PlatformCode")]
    pub platform_code: String,
    #[serde(rename = "DeviceType")]
    pub device_type: String,
    #[serde(rename = "DeviceName")]
    pub device_name: String,
    #[serde(rename = "DeviceModel")]
    pub device_model: String,
    #[serde(rename = "ClientVersion")]
    pub client_version: String,
    #[serde(rename = "DevicePublicKey")]
    pub device_public_key: String,
    #[serde(rename = "DeviceBrand")]
    pub device_brand: String,
    #[serde(rename = "DeviceCPU")]
    pub device_cpu: String,
    #[serde(rename = "OSInfo")]
    pub os_info: String,
    #[serde(rename = "OSVersion")]
    pub os_version: String,
}

/// RFC 4122 v4 UUID(随机),小写带连字符。
fn uuid_v4() -> Result<String, super::crypto::CryptoError> {
    let mut b = [0u8; 16];
    getrandom::getrandom(&mut b).map_err(|e| super::crypto::CryptoError::Rng(e.to_string()))?;
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // variant 10
    Ok(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    ))
}

/// 32 字符随机 hex(machine id 用)。
fn random_hex_32() -> Result<String, super::crypto::CryptoError> {
    let mut b = [0u8; 16];
    getrandom::getrandom(&mut b).map_err(|e| super::crypto::CryptoError::Rng(e.to_string()))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// 从 `[0, len)` 里随机选一个索引(池选择用)。
fn pick_index(len: usize) -> Result<usize, super::crypto::CryptoError> {
    let mut b = [0u8; 4];
    getrandom::getrandom(&mut b).map_err(|e| super::crypto::CryptoError::Rng(e.to_string()))?;
    Ok((u32::from_le_bytes(b) as usize) % len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trae::constants::TRAE_CN_CONFIG;

    #[test]
    fn generate_produces_distinct_fingerprints() {
        let a = DeviceFingerprint::generate().unwrap();
        let b = DeviceFingerprint::generate().unwrap();
        // 隔离核心:device_id / machine_id 逐账号不同
        assert_ne!(a.device_id, b.device_id, "device_id 必须逐账号唯一");
        assert_ne!(a.machine_id, b.machine_id, "machine_id 必须逐账号唯一");
    }

    #[test]
    fn uuid_v4_format() {
        let u = uuid_v4().unwrap();
        assert_eq!(u.len(), 36);
        assert_eq!(u.as_bytes()[14], b'4', "version nibble 应为 4");
        let variant = u.as_bytes()[19];
        assert!(
            matches!(variant, b'8' | b'9' | b'a' | b'b'),
            "variant 应为 8-b"
        );
    }

    #[test]
    fn machine_id_is_32_hex() {
        let m = random_hex_32().unwrap();
        assert_eq!(m.len(), 32);
        assert!(m.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn device_info_carries_config_and_pubkey() {
        let fp = DeviceFingerprint::generate().unwrap();
        let di = fp.to_device_info(&TRAE_CN_CONFIG, "PUBKEY-PEM");
        assert_eq!(di.platform_code, "SOLO_PC");
        assert_eq!(di.device_type, "PC");
        assert_eq!(di.client_version, "0.1.21");
        assert_eq!(di.device_public_key, "PUBKEY-PEM");
        assert_eq!(di.device_id, fp.device_id);
        // 机型出自可信池
        assert!(MAC_MODELS.iter().any(|(m, _)| *m == di.device_model));
    }

    #[test]
    fn device_info_serializes_pascal_case() {
        let fp = DeviceFingerprint::generate().unwrap();
        let di = fp.to_device_info(&TRAE_CN_CONFIG, "PK");
        let json = serde_json::to_string(&di).unwrap();
        for key in [
            "DeviceID",
            "MachineID",
            "PlatformCode",
            "DeviceType",
            "DeviceName",
            "DeviceModel",
            "ClientVersion",
            "DevicePublicKey",
            "DeviceBrand",
            "DeviceCPU",
            "OSInfo",
            "OSVersion",
        ] {
            assert!(json.contains(key), "DeviceInfo JSON 缺字段 {key}: {json}");
        }
    }
}
