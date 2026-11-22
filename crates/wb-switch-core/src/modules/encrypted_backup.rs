//! 账号备份的加密信封：口令派生密钥 + AEAD，落盘内容不暴露账号令牌。
//!
//! 设计要点（对齐社区分支做法，但只用项目已有依赖，零新增供应链风险）：
//! - **KDF**：PBKDF2-HMAC-SHA256，210,000 轮（OWASP 2023 建议量级），随机 16 字节盐；
//! - **AEAD**：AES-256-GCM，随机 12 字节 nonce，AAD 绑定信封格式与版本，
//!   防止把 v1 密文塞进 v2 信封之类的降级/混淆；
//! - **信封**：JSON，含 `format` / `version` / `kdf` / `cipher` / `ciphertext`，
//!   便于将来换算法而不破坏旧文件（版本不认就明确拒绝，不做猜测）；
//! - **错误文案**：口令错与文件损坏统一为「口令错误或文件已损坏」，
//!   不泄露"口令对但密文坏"这类区分（对离线爆破没有帮助，反而误导用户）。
//!
//! 该模块只做密码学与信封格式，不涉及账号库读写（那部分在 `export_import`）。

use aes_gcm::aead::generic_array::GenericArray;
use aes_gcm::aead::{Aead, KeyInit, OsRng, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use pbkdf2::pbkdf2_hmac;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

/// 信封标识：用于区分「加密备份」与普通账号备份 JSON。
pub const ENVELOPE_FORMAT: &str = "wb-switch-encrypted-backup";
/// 当前信封版本；不认识的版本一律拒绝，不做兼容猜测。
pub const ENVELOPE_VERSION: u32 = 1;
/// PBKDF2 轮数。数值写在信封里，旧文件仍按自身记录值派生。
const PBKDF2_ITERATIONS: u32 = 210_000;
const SALT_BYTES: usize = 16;
/// 解密时可接受的轮数区间：信封里的轮数来自文件本身，**不可信**。
/// 恶意文件写个 40 亿轮就能把 CPU 拖死（DoS），所以只接受合理范围；
/// 未来提高默认轮数时同步放宽上限即可，旧文件仍在区间内可读。
const MIN_ITERATIONS: u32 = 100_000;
const MAX_ITERATIONS: u32 = 1_000_000;
/// 盐的合理长度区间（当前写 16 字节，留出兼容余量）。
const MIN_SALT_BYTES: usize = 8;
const MAX_SALT_BYTES: usize = 64;
const KEY_BYTES: usize = 32;
const NONCE_BYTES: usize = 12;

#[derive(Debug, Serialize, Deserialize)]
struct KdfParams {
    /// 派生算法标识，便于将来切换（当前只支持 pbkdf2-hmac-sha256）。
    algo: String,
    iterations: u32,
    /// base64 编码的盐。
    salt: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct CipherParams {
    /// 加密算法标识（当前只支持 aes-256-gcm）。
    algo: String,
    /// base64 编码的 nonce。
    nonce: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Envelope {
    format: String,
    version: u32,
    kdf: KdfParams,
    cipher: CipherParams,
    /// base64 编码的密文（含 GCM tag）。
    ciphertext: String,
}

fn aad(version: u32) -> Vec<u8> {
    format!("{ENVELOPE_FORMAT}|v{version}").into_bytes()
}

/// 口令最短长度：PBKDF2 只能减缓离线爆破，弱口令仍然等于没加密。
/// 8 位是可接受的下限（与常见系统一致），更严格的要求交给 UI 提示。
pub const MIN_PASSWORD_CHARS: usize = 8;

fn derive_key(password: &str, salt: &[u8], iterations: u32) -> Result<[u8; KEY_BYTES], String> {
    if password.is_empty() {
        return Err("口令不能为空".to_string());
    }
    if password.chars().count() < MIN_PASSWORD_CHARS {
        return Err(format!("口令至少 {MIN_PASSWORD_CHARS} 位"));
    }
    let mut key = [0u8; KEY_BYTES];
    pbkdf2_hmac::<Sha256>(password.as_bytes(), salt, iterations, &mut key);
    Ok(key)
}

/// 把明文备份加密成信封 JSON 文本。
pub fn encrypt(plaintext: &str, password: &str) -> Result<String, String> {
    if plaintext.is_empty() {
        return Err("没有可加密的内容".to_string());
    }
    let mut salt = [0u8; SALT_BYTES];
    let mut nonce_bytes = [0u8; NONCE_BYTES];
    // OsRng：与 aes-gcm 同源的系统随机源，避免再引 rand 特化版本。
    use aes_gcm::aead::rand_core::RngCore;
    OsRng.fill_bytes(&mut salt);
    OsRng.fill_bytes(&mut nonce_bytes);

    let key = derive_key(password, &salt, PBKDF2_ITERATIONS)?;
    let cipher = Aes256Gcm::new(GenericArray::from_slice(&key));
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(
            nonce,
            Payload {
                msg: plaintext.as_bytes(),
                aad: &aad(ENVELOPE_VERSION),
            },
        )
        .map_err(|_| "加密失败（内容过大或内存不足）".to_string())?;

    let envelope = Envelope {
        format: ENVELOPE_FORMAT.to_string(),
        version: ENVELOPE_VERSION,
        kdf: KdfParams {
            algo: "pbkdf2-hmac-sha256".to_string(),
            iterations: PBKDF2_ITERATIONS,
            salt: B64.encode(salt),
        },
        cipher: CipherParams {
            algo: "aes-256-gcm".to_string(),
            nonce: B64.encode(nonce_bytes),
        },
        ciphertext: B64.encode(ciphertext),
    };
    serde_json::to_string_pretty(&envelope).map_err(|error| format!("生成加密信封失败：{error}"))
}

/// 判断一段文本是否是本工具的加密信封（导入时用来自动分流）。
///
/// not-optimized: 调用方随后会再解析一次做解密。这里刻意不合并——PBKDF2 210k 轮
/// 的开销比多一次 serde 解析大三个数量级，合并反而让接口变复杂（需要把解析结果
/// 透传给 decrypt）。若将来 KDF 降到万轮以下再考虑。
pub fn is_encrypted_envelope(text: &str) -> bool {
    let trimmed = text.trim_start();
    if !trimmed.starts_with('{') {
        return false;
    }
    serde_json::from_str::<Envelope>(trimmed)
        .map(|envelope| envelope.format == ENVELOPE_FORMAT)
        .unwrap_or(false)
}

/// 解密信封 JSON 文本，返回原始明文备份。
pub fn decrypt(envelope_text: &str, password: &str) -> Result<String, String> {
    let envelope: Envelope = serde_json::from_str(envelope_text.trim())
        .map_err(|_| "不是本工具导出的加密备份文件".to_string())?;
    if envelope.format != ENVELOPE_FORMAT {
        return Err("不是本工具导出的加密备份文件".to_string());
    }
    if envelope.version != ENVELOPE_VERSION {
        return Err(format!(
            "加密备份版本不受支持（v{}，当前支持 v{}）",
            envelope.version, ENVELOPE_VERSION
        ));
    }
    if envelope.kdf.algo != "pbkdf2-hmac-sha256" || envelope.cipher.algo != "aes-256-gcm" {
        return Err(format!(
            "加密备份使用了不支持的算法（{} / {}）",
            envelope.kdf.algo, envelope.cipher.algo
        ));
    }
    if !(MIN_ITERATIONS..=MAX_ITERATIONS).contains(&envelope.kdf.iterations) {
        return Err(format!(
            "加密备份的密钥派生参数不合理（{} 轮，允许 {}~{}）；文件可能被篡改",
            envelope.kdf.iterations, MIN_ITERATIONS, MAX_ITERATIONS
        ));
    }
    let salt = B64
        .decode(&envelope.kdf.salt)
        .map_err(|_| "加密备份已损坏（盐值无效）".to_string())?;
    if !(MIN_SALT_BYTES..=MAX_SALT_BYTES).contains(&salt.len()) {
        return Err("加密备份已损坏（盐值长度异常）".to_string());
    }
    let nonce_bytes = B64
        .decode(&envelope.cipher.nonce)
        .map_err(|_| "加密备份已损坏（nonce 无效）".to_string())?;
    let ciphertext = B64
        .decode(&envelope.ciphertext)
        .map_err(|_| "加密备份已损坏（密文无效）".to_string())?;
    if nonce_bytes.len() != NONCE_BYTES {
        return Err("加密备份已损坏（nonce 长度异常）".to_string());
    }

    let key = derive_key(password, &salt, envelope.kdf.iterations)?;
    let cipher = Aes256Gcm::new(GenericArray::from_slice(&key));
    let plaintext = cipher
        .decrypt(
            Nonce::from_slice(&nonce_bytes),
            Payload {
                msg: &ciphertext,
                aad: &aad(envelope.version),
            },
        )
        .map_err(|_| "口令错误或文件已损坏".to_string())?;
    String::from_utf8(plaintext).map_err(|_| "解密结果不是有效文本".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"[{"id":"acc-1","nickname":"测试账号"}]"#;

    #[test]
    fn round_trip_preserves_plaintext() {
        let envelope = encrypt(SAMPLE, "correct horse battery staple").unwrap();
        assert!(envelope.contains(ENVELOPE_FORMAT));
        let restored = decrypt(&envelope, "correct horse battery staple").unwrap();
        assert_eq!(restored, SAMPLE);
    }

    #[test]
    fn ciphertext_does_not_leak_plaintext() {
        let envelope = encrypt(SAMPLE, "test-pw-1234").unwrap();
        assert!(!envelope.contains("acc-1"));
        assert!(!envelope.contains("测试账号"));
    }

    #[test]
    fn wrong_password_is_reported_without_oracle() {
        let envelope = encrypt(SAMPLE, "right-pw-1234").unwrap();
        let error = decrypt(&envelope, "wrong-pw-1234").unwrap_err();
        assert_eq!(error, "口令错误或文件已损坏");
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let envelope = encrypt(SAMPLE, "test-pw-1234").unwrap();
        let mut parsed: serde_json::Value = serde_json::from_str(&envelope).unwrap();
        let original = parsed["ciphertext"].as_str().unwrap().to_string();
        // 改动密文中的一个字符
        let mut chars: Vec<char> = original.chars().collect();
        let mid = chars.len() / 2;
        chars[mid] = if chars[mid] == 'A' { 'B' } else { 'A' };
        parsed["ciphertext"] = serde_json::Value::String(chars.into_iter().collect());
        let error = decrypt(&parsed.to_string(), "test-pw-1234").unwrap_err();
        assert_eq!(error, "口令错误或文件已损坏");
    }

    #[test]
    fn empty_password_rejected() {
        assert_eq!(encrypt(SAMPLE, "").unwrap_err(), "口令不能为空");
        let envelope = encrypt(SAMPLE, "test-pw-1234").unwrap();
        assert_eq!(decrypt(&envelope, "").unwrap_err(), "口令不能为空");
    }

    #[test]
    fn unknown_version_is_rejected_explicitly() {
        let envelope = encrypt(SAMPLE, "test-pw-1234").unwrap();
        let mut parsed: serde_json::Value = serde_json::from_str(&envelope).unwrap();
        parsed["version"] = serde_json::Value::from(99);
        let error = decrypt(&parsed.to_string(), "test-pw-1234").unwrap_err();
        assert!(error.contains("版本不受支持"), "{error}");
    }

    #[test]
    fn short_password_is_rejected() {
        // PBKDF2 只能减缓离线爆破：少于下限的口令等于没加密。
        let too_short = "1234567";
        assert_eq!(
            encrypt(SAMPLE, too_short).unwrap_err(),
            format!("口令至少 {MIN_PASSWORD_CHARS} 位")
        );
        // 但已用短口令加密的历史文件仍应能用原口令解开（前端只做提示，不做硬拦）
        let envelope = encrypt(SAMPLE, "12345678").unwrap();
        assert!(decrypt(&envelope, "12345678").is_ok());
    }

    #[test]
    fn absurd_iteration_count_is_rejected_fast() {
        // 信封里的轮数不可信：写个天文数字必须立刻拒绝，而不是把 CPU 拖死。
        let envelope = encrypt(SAMPLE, "test-pw-1234").unwrap();
        let mut parsed: serde_json::Value = serde_json::from_str(&envelope).unwrap();
        parsed["kdf"]["iterations"] = serde_json::Value::from(4_000_000_000u64);
        let started = std::time::Instant::now();
        let error = decrypt(&parsed.to_string(), "test-pw-1234").unwrap_err();
        assert!(error.contains("密钥派生参数不合理"), "{error}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "必须快速失败"
        );
    }

    #[test]
    fn short_salt_is_rejected() {
        let envelope = encrypt(SAMPLE, "test-pw-1234").unwrap();
        let mut parsed: serde_json::Value = serde_json::from_str(&envelope).unwrap();
        parsed["kdf"]["salt"] = serde_json::Value::String(B64.encode([1u8, 2, 3]));
        let error = decrypt(&parsed.to_string(), "test-pw-1234").unwrap_err();
        assert!(error.contains("盐值长度异常"), "{error}");
    }

    #[test]
    fn plaintext_json_is_not_mistaken_for_envelope() {
        assert!(!is_encrypted_envelope(SAMPLE));
        assert!(is_encrypted_envelope(
            &encrypt(SAMPLE, "test-pw-1234").unwrap()
        ));
    }
}
