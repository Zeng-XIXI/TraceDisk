use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs::OpenOptions;
use std::io::Write;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::Manager;

const LICENSE_FILE: &str = "offline-license-v1";
const SIGNING_DOMAIN: &[u8] = b"TraceDisk-License-v1\0";
const MACHINE_DOMAIN: &[u8] = b"TraceDisk-Machine-v1\0";
const PAYLOAD_BYTES: usize = 25;
const SIGNATURE_BYTES: usize = 64;
const TOKEN_BYTES: usize = PAYLOAD_BYTES + SIGNATURE_BYTES;
const TOKEN_CHARACTERS: usize = 119;
const FORMAT_VERSION: u8 = 1;
const MAX_LICENSE_DURATION_SECONDS: u64 = 3_650 * 24 * 60 * 60;
const COMPILED_PUBLIC_KEY: Option<&str> = option_env!("TRACEDISK_LICENSE_PUBLIC_KEY");

static ACCESS_GRANTED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LicenseState {
    machine_code: String,
    authorized: bool,
    status: String,
    expires_at: Option<u64>,
    message: String,
}

impl LicenseState {
    fn locked(machine_code: &str, status: &str, message: impl Into<String>) -> Self {
        Self {
            machine_code: machine_code.to_string(),
            authorized: false,
            status: status.to_string(),
            expires_at: None,
            message: message.into(),
        }
    }
}

#[tauri::command]
pub(crate) fn get_license_state(app: tauri::AppHandle) -> Result<LicenseState, String> {
    ACCESS_GRANTED.store(false, Ordering::Release);
    let machine_code = current_machine_code()?;
    let Some(license_code) = read_saved_license(&app)? else {
        return Ok(LicenseState::locked(
            &machine_code,
            "UNLICENSED",
            "尚未导入离线许可证",
        ));
    };

    let key = compiled_public_key()?;
    let state = verify_license(&license_code, &machine_code, current_epoch_seconds()?, &key);
    ACCESS_GRANTED.store(state.authorized, Ordering::Release);
    Ok(state)
}

#[tauri::command]
pub(crate) fn activate_license(
    app: tauri::AppHandle,
    license_code: String,
) -> Result<LicenseState, String> {
    ACCESS_GRANTED.store(false, Ordering::Release);
    let machine_code = current_machine_code()?;
    let normalized = license_code.trim();
    let key = compiled_public_key()?;
    let state = verify_license(normalized, &machine_code, current_epoch_seconds()?, &key);
    if state.authorized {
        save_license(&app, normalized)?;
        ACCESS_GRANTED.store(true, Ordering::Release);
    }
    Ok(state)
}

pub(crate) fn require_authenticated() -> Result<(), String> {
    if ACCESS_GRANTED.load(Ordering::Acquire) {
        Ok(())
    } else {
        Err("TraceDisk 尚未通过离线许可证验证".into())
    }
}

fn verify_license(
    license_code: &str,
    machine_code: &str,
    now: u64,
    public_key: &VerifyingKey,
) -> LicenseState {
    if license_code.len() != TOKEN_CHARACTERS
        || !license_code
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return LicenseState::locked(machine_code, "INVALID", "许可证格式不正确");
    }

    let Ok(token) = URL_SAFE_NO_PAD.decode(license_code) else {
        return LicenseState::locked(machine_code, "INVALID", "许可证编码不正确");
    };
    if token.len() != TOKEN_BYTES {
        return LicenseState::locked(machine_code, "INVALID", "许可证长度不正确");
    }

    let payload = &token[..PAYLOAD_BYTES];
    let Ok(signature) = Signature::from_slice(&token[PAYLOAD_BYTES..]) else {
        return LicenseState::locked(machine_code, "INVALID", "许可证签名长度不正确");
    };
    let mut signed = Vec::with_capacity(SIGNING_DOMAIN.len() + PAYLOAD_BYTES);
    signed.extend_from_slice(SIGNING_DOMAIN);
    signed.extend_from_slice(payload);
    if public_key.verify_strict(&signed, &signature).is_err() {
        return LicenseState::locked(machine_code, "INVALID", "许可证签名无效或内容已被修改");
    }
    if payload[0] != FORMAT_VERSION {
        return LicenseState::locked(machine_code, "INVALID", "许可证版本不受支持");
    }

    let issued_at = u32::from_be_bytes(payload[17..21].try_into().expect("fixed payload")) as u64;
    let expires_at = u32::from_be_bytes(payload[21..25].try_into().expect("fixed payload")) as u64;
    if expires_at <= issued_at || expires_at - issued_at > MAX_LICENSE_DURATION_SECONDS {
        return LicenseState::locked(machine_code, "INVALID", "许可证有效期不正确");
    }

    let Some(machine_tag) = machine_tag(machine_code) else {
        return LicenseState::locked(machine_code, "MACHINE_ERROR", "本机机器码格式异常");
    };
    if payload[1..17] != machine_tag {
        return LicenseState::locked(machine_code, "DEVICE_MISMATCH", "许可证不属于这台设备");
    }

    if now < issued_at {
        return LicenseState::locked(machine_code, "NOT_YET_VALID", "本机时间早于许可证签发时间");
    }
    if now >= expires_at {
        return LicenseState {
            machine_code: machine_code.to_string(),
            authorized: false,
            status: "EXPIRED".into(),
            expires_at: Some(expires_at),
            message: "许可证已过期".into(),
        };
    }

    LicenseState {
        machine_code: machine_code.to_string(),
        authorized: true,
        status: "ACTIVE".into(),
        expires_at: Some(expires_at),
        message: "离线许可证有效".into(),
    }
}

fn compiled_public_key() -> Result<VerifyingKey, String> {
    let configured = COMPILED_PUBLIC_KEY
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            "此 TraceDisk 构建未嵌入许可证公钥；请设置 TRACEDISK_LICENSE_PUBLIC_KEY 后重新构建"
                .to_string()
        })?;
    parse_public_key(configured)
}

fn parse_public_key(configured: &str) -> Result<VerifyingKey, String> {
    let decoded = STANDARD
        .decode(configured)
        .or_else(|_| URL_SAFE_NO_PAD.decode(configured))
        .map_err(|_| "TRACEDISK_LICENSE_PUBLIC_KEY 不是有效的 Base64".to_string())?;
    let bytes: [u8; 32] = decoded
        .try_into()
        .map_err(|_| "TRACEDISK_LICENSE_PUBLIC_KEY 必须是 32 字节 Ed25519 公钥".to_string())?;
    VerifyingKey::from_bytes(&bytes).map_err(|_| "Ed25519 公钥格式无效".to_string())
}

fn current_machine_code() -> Result<String, String> {
    Ok(machine_code_from_platform_id(&platform_machine_id()?))
}

fn machine_code_from_platform_id(platform_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(MACHINE_DOMAIN);
    digest.update(platform_id.trim().to_ascii_lowercase().as_bytes());
    to_upper_hex(&digest.finalize())
}

fn machine_tag(machine_code: &str) -> Option<[u8; 16]> {
    if machine_code.len() != 64 || !machine_code.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut tag = [0_u8; 16];
    for (index, target) in tag.iter_mut().enumerate() {
        let start = index * 2;
        *target = u8::from_str_radix(&machine_code[start..start + 2], 16).ok()?;
    }
    Some(tag)
}

fn to_upper_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(target_os = "macos")]
fn platform_machine_id() -> Result<String, String> {
    let output = Command::new("/usr/sbin/ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()
        .map_err(|error| format!("无法读取 macOS 设备标识: {error}"))?;
    if !output.status.success() {
        return Err("macOS 未返回设备标识".into());
    }
    parse_macos_platform_uuid(&String::from_utf8_lossy(&output.stdout))
        .ok_or_else(|| "无法从 macOS 系统信息中解析设备标识".into())
}

#[cfg(target_os = "windows")]
fn platform_machine_id() -> Result<String, String> {
    let output = Command::new("reg.exe")
        .args([
            "query",
            r"HKLM\SOFTWARE\Microsoft\Cryptography",
            "/v",
            "MachineGuid",
        ])
        .output()
        .map_err(|error| format!("无法读取 Windows 设备标识: {error}"))?;
    if !output.status.success() {
        return Err("Windows 未返回设备标识".into());
    }
    parse_windows_machine_guid(&String::from_utf8_lossy(&output.stdout))
        .ok_or_else(|| "无法从 Windows 注册表中解析设备标识".into())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn platform_machine_id() -> Result<String, String> {
    Err("TraceDisk 离线机器码目前只支持 macOS 和 Windows".into())
}

#[cfg(target_os = "macos")]
fn parse_macos_platform_uuid(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        if !line.contains("IOPlatformUUID") {
            return None;
        }
        line.split_once('=')
            .map(|(_, value)| value.trim().trim_matches('"').to_string())
            .filter(|value| value.len() >= 32)
    })
}

#[cfg(target_os = "windows")]
fn parse_windows_machine_guid(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let trimmed = line.trim();
        if !trimmed.to_ascii_lowercase().starts_with("machineguid") {
            return None;
        }
        trimmed
            .split_whitespace()
            .last()
            .map(str::to_string)
            .filter(|value| value.len() >= 32)
    })
}

fn license_path(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    let directory = app
        .path()
        .app_config_dir()
        .map_err(|error| format!("无法定位 TraceDisk 配置目录: {error}"))?;
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("无法创建 TraceDisk 配置目录: {error}"))?;
    Ok(directory.join(LICENSE_FILE))
}

fn read_saved_license(app: &tauri::AppHandle) -> Result<Option<String>, String> {
    let path = license_path(app)?;
    if !path.exists() {
        return Ok(None);
    }
    let value = std::fs::read_to_string(&path)
        .map_err(|error| format!("无法读取已保存的离线许可证: {error}"))?;
    let normalized = value.trim().to_string();
    if normalized.len() > 256 {
        return Err("本机保存的许可证文件过大，已拒绝读取".into());
    }
    Ok(Some(normalized))
}

fn save_license(app: &tauri::AppHandle, license_code: &str) -> Result<(), String> {
    let path = license_path(app)?;
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .map_err(|error| format!("无法保存离线许可证: {error}"))?;
    file.write_all(license_code.as_bytes())
        .and_then(|_| file.sync_all())
        .map_err(|error| format!("无法保存离线许可证: {error}"))
}

fn current_epoch_seconds() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| "本机系统时间早于 1970 年，无法验证许可证".into())
}

#[cfg(test)]
mod tests {
    use super::{
        machine_code_from_platform_id, parse_public_key, verify_license, LicenseState,
        MACHINE_DOMAIN,
    };
    const TEST_PUBLIC: &str = "11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=";
    const MACHINE_CODE: &str = "00112233445566778899AABBCCDDEEFF102132435465768798A9BACBDCEDFE0F";
    const JAVA_SIGNED_LICENSE: &str = "AQARIjNEVWZ3iJmqu8zd7v9qew7AaqKbwPIuPiHxKs-CGkpVXzt9rJQnGokEkJrsDJj2wAHKb5hs1mgquT4CL28npkshk7ThNt3AMMGmlbObyMISgHJcNQ8";

    #[test]
    fn creates_stable_domain_separated_machine_code() {
        assert_eq!(MACHINE_DOMAIN, b"TraceDisk-Machine-v1\0");
        assert_eq!(
            machine_code_from_platform_id("ABC-123"),
            machine_code_from_platform_id("  abc-123  ")
        );
        assert_eq!(machine_code_from_platform_id("ABC-123").len(), 64);
    }

    #[test]
    fn verifies_java_signature_device_and_expiration() {
        let issued_at = 1_786_449_600_u32;
        let expires_at = issued_at + 30 * 24 * 60 * 60;
        let public_key = parse_public_key(TEST_PUBLIC).expect("test public key");

        let active = verify_license(
            JAVA_SIGNED_LICENSE,
            MACHINE_CODE,
            issued_at as u64 + 60,
            &public_key,
        );
        assert!(active.authorized);
        assert_eq!(active.status, "ACTIVE");

        let expired = verify_license(
            JAVA_SIGNED_LICENSE,
            MACHINE_CODE,
            expires_at as u64,
            &public_key,
        );
        assert!(!expired.authorized);
        assert_eq!(expired.status, "EXPIRED");
    }

    #[test]
    fn rejects_a_license_for_another_machine() {
        let state = LicenseState::locked(MACHINE_CODE, "DEVICE_MISMATCH", "mismatch");
        assert!(!state.authorized);
        assert_eq!(state.status, "DEVICE_MISMATCH");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn parses_macos_platform_uuid() {
        let value = super::parse_macos_platform_uuid(
            r#"    "IOPlatformUUID" = "ABCDEF12-3456-7890-ABCD-EF1234567890""#,
        );
        assert_eq!(
            value.as_deref(),
            Some("ABCDEF12-3456-7890-ABCD-EF1234567890")
        );
    }
}
