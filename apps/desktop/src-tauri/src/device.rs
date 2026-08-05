#[cfg(target_os = "macos")]
use plist::{Dictionary, Value};
#[cfg(target_os = "windows")]
use serde::Deserialize;
use serde::Serialize;
#[cfg(target_os = "macos")]
use std::fs::File;
#[cfg(target_os = "macos")]
use std::io::Cursor;
use std::process::Command;

const MIN_CAPACITY_TOLERANCE_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug)]
pub(crate) struct ValidatedRawDevice {
    pub raw_device_path: String,
    pub whole_disk_identifier: String,
    pub size_bytes: u64,
    pub block_size: u64,
}

#[derive(Debug, Serialize)]
pub struct SdCardInfo {
    pub requested_path: String,
    pub mount_point: String,
    pub volume_name: String,
    pub media_name: String,
    pub partition_identifier: String,
    pub whole_disk_identifier: String,
    pub device_node: String,
    pub raw_device_path: String,
    pub size_bytes: u64,
    pub removable: bool,
    pub ejectable: bool,
    pub raw_readable: bool,
}

#[tauri::command]
pub async fn resolve_sd_card_path(path: String) -> Result<SdCardInfo, String> {
    crate::auth::require_authenticated()?;
    tauri::async_runtime::spawn_blocking(move || resolve_sd_card_path_blocking(&path))
        .await
        .map_err(|error| format!("device inspection task failed: {error}"))?
}

#[cfg(target_os = "macos")]
fn resolve_sd_card_path_blocking(requested_path: &str) -> Result<SdCardInfo, String> {
    let requested_path = requested_path.trim();
    if requested_path.is_empty() {
        return Err("请输入 SD 卡的挂载路径，例如 /Volumes/SD_Card".into());
    }

    let canonical_path = std::fs::canonicalize(requested_path)
        .map_err(|error| format!("无法访问路径 {requested_path}: {error}"))?;
    let volume = diskutil_info(&canonical_path.to_string_lossy())?;

    let partition_identifier = dictionary_string(&volume, "DeviceIdentifier")
        .ok_or_else(|| "diskutil 未返回 DeviceIdentifier".to_string())?
        .to_string();

    let whole_disk_identifier = dictionary_string(&volume, "ParentWholeDisk")
        .and_then(normalize_whole_disk_identifier)
        .or_else(|| normalize_whole_disk_identifier(&partition_identifier))
        .ok_or_else(|| {
            format!("无法从设备标识 {partition_identifier} 推导整盘设备，请勿继续操作")
        })?;

    let whole_device_path = format!("/dev/{whole_disk_identifier}");
    let whole_disk = diskutil_info(&whole_device_path)?;
    let internal = dictionary_bool(&whole_disk, "Internal")
        .or_else(|| dictionary_bool(&volume, "Internal"))
        .unwrap_or(true);

    if internal {
        return Err(format!(
            "安全保护：路径 {requested_path} 属于内置磁盘 {whole_device_path}，TraceDisk 拒绝处理"
        ));
    }

    let removable = dictionary_bool(&whole_disk, "RemovableMedia")
        .or_else(|| dictionary_bool(&whole_disk, "Removable"))
        .unwrap_or(false);
    let ejectable = dictionary_bool(&whole_disk, "Ejectable").unwrap_or(false);

    if !removable && !ejectable {
        return Err(format!(
            "安全保护：设备 {whole_device_path} 没有被 macOS 标记为可移除或可推出设备"
        ));
    }

    let size_bytes = dictionary_u64(&whole_disk, "TotalSize")
        .or_else(|| dictionary_u64(&whole_disk, "DiskSize"))
        .ok_or_else(|| "diskutil 未返回整盘容量".to_string())?;
    if size_bytes < 1024 * 1024 {
        return Err("设备容量异常，已停止处理".into());
    }

    let raw_device_path = format!("/dev/r{whole_disk_identifier}");
    let raw_readable = File::open(&raw_device_path).is_ok();
    let mount_point = dictionary_string(&volume, "MountPoint")
        .map(str::to_string)
        .unwrap_or_else(|| canonical_path.to_string_lossy().into_owned());
    let volume_name = dictionary_string(&volume, "VolumeName")
        .map(str::to_string)
        .or_else(|| {
            canonical_path
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "未命名 SD 卡".into());
    let media_name = dictionary_string(&whole_disk, "MediaName")
        .or_else(|| dictionary_string(&whole_disk, "IORegistryEntryName"))
        .unwrap_or("External removable media")
        .to_string();
    let device_node = dictionary_string(&volume, "DeviceNode")
        .map(str::to_string)
        .unwrap_or_else(|| format!("/dev/{partition_identifier}"));

    Ok(SdCardInfo {
        requested_path: requested_path.to_string(),
        mount_point,
        volume_name,
        media_name,
        partition_identifier,
        whole_disk_identifier,
        device_node,
        raw_device_path,
        size_bytes,
        removable,
        ejectable,
        raw_readable,
    })
}

#[cfg(target_os = "macos")]
pub(crate) fn validate_external_raw_device(
    raw_device_path: &str,
    expected_size_bytes: u64,
) -> Result<ValidatedRawDevice, String> {
    let whole_disk_identifier = raw_path_to_whole_identifier(raw_device_path).ok_or_else(|| {
        format!("安全保护：无效的原始整盘设备路径 {raw_device_path}，只接受 /dev/rdiskN")
    })?;
    let normalized_raw_path = format!("/dev/r{whole_disk_identifier}");
    if normalized_raw_path != raw_device_path {
        return Err("安全保护：原始设备路径未通过规范化检查".into());
    }

    let whole_device_path = format!("/dev/{whole_disk_identifier}");
    let whole_disk = diskutil_info(&whole_device_path)?;
    let returned_identifier = dictionary_string(&whole_disk, "DeviceIdentifier")
        .and_then(normalize_whole_disk_identifier)
        .ok_or_else(|| "diskutil 未返回有效的整盘设备标识".to_string())?;
    if returned_identifier != whole_disk_identifier {
        return Err("安全保护：设备标识在校验期间发生变化".into());
    }

    if dictionary_bool(&whole_disk, "Internal").unwrap_or(true) {
        return Err(format!(
            "安全保护：{whole_device_path} 是内置磁盘，TraceDisk 拒绝读取"
        ));
    }
    let removable = dictionary_bool(&whole_disk, "RemovableMedia")
        .or_else(|| dictionary_bool(&whole_disk, "Removable"))
        .unwrap_or(false);
    let ejectable = dictionary_bool(&whole_disk, "Ejectable").unwrap_or(false);
    if !removable && !ejectable {
        return Err(format!(
            "安全保护：{whole_device_path} 不是可移除或可推出设备"
        ));
    }

    let size_bytes = dictionary_u64(&whole_disk, "TotalSize")
        .or_else(|| dictionary_u64(&whole_disk, "DiskSize"))
        .ok_or_else(|| "diskutil 未返回整盘容量".to_string())?;
    if size_bytes < 1024 * 1024 || !capacity_within_tolerance(expected_size_bytes, size_bytes) {
        return Err(format!(
            "安全保护：设备容量差异超过允许范围（之前 {expected_size_bytes}，现在 {size_bytes} 字节），请重新识别 SD 卡"
        ));
    }
    let block_size = dictionary_u64(&whole_disk, "DeviceBlockSize")
        .or_else(|| dictionary_u64(&whole_disk, "PreferredBlockSize"))
        .unwrap_or(512);
    if !(512..=1024 * 1024).contains(&block_size)
        || !block_size.is_power_of_two()
        || !size_bytes.is_multiple_of(block_size)
    {
        return Err(format!(
            "安全保护：设备扇区参数异常（容量 {size_bytes}，扇区 {block_size} 字节）"
        ));
    }

    Ok(ValidatedRawDevice {
        raw_device_path: normalized_raw_path,
        whole_disk_identifier,
        size_bytes,
        block_size,
    })
}

fn capacity_within_tolerance(expected: u64, actual: u64) -> bool {
    if expected < 1024 * 1024 || actual < 1024 * 1024 {
        return false;
    }
    let tolerance = MIN_CAPACITY_TOLERANCE_BYTES.max(expected / 10_000);
    expected.abs_diff(actual) <= tolerance
}

#[cfg(target_os = "macos")]
pub(crate) fn unmount_whole_disk(whole_disk_identifier: &str) -> Result<(), String> {
    let normalized = normalize_whole_disk_identifier(whole_disk_identifier)
        .filter(|identifier| identifier == whole_disk_identifier)
        .ok_or_else(|| "安全保护：拒绝卸载无效设备标识".to_string())?;
    let target = format!("/dev/{normalized}");
    let output = Command::new("/usr/sbin/diskutil")
        .args(["unmountDisk", &target])
        .output()
        .map_err(|error| format!("无法运行 diskutil 卸载 SD 卡: {error}"))?;
    if output.status.success() {
        return Ok(());
    }

    let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if message.is_empty() {
        format!("无法卸载 {target}，请关闭正在使用 SD 卡文件的应用后重试")
    } else {
        format!("无法卸载 {target}: {message}")
    })
}

#[cfg(any(target_os = "macos", test))]
fn raw_path_to_whole_identifier(raw_device_path: &str) -> Option<String> {
    let digits = raw_device_path.strip_prefix("/dev/rdisk")?;
    (!digits.is_empty() && digits.chars().all(|character| character.is_ascii_digit()))
        .then(|| format!("disk{digits}"))
}

#[cfg(target_os = "windows")]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct WindowsVolumeInfo {
    drive_root: String,
    drive_letter: String,
    volume_name: String,
    disk_number: u32,
    partition_number: u32,
    size_bytes: u64,
    logical_sector_size: u64,
    bus_type: String,
    friendly_name: String,
    is_boot: bool,
    is_system: bool,
}

#[cfg(target_os = "windows")]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct WindowsDiskInfo {
    disk_number: u32,
    size_bytes: u64,
    logical_sector_size: u64,
    bus_type: String,
    is_boot: bool,
    is_system: bool,
}

#[cfg(target_os = "windows")]
fn resolve_sd_card_path_blocking(requested_path: &str) -> Result<SdCardInfo, String> {
    const SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
$requested = $env:TRACEDISK_SELECTED_PATH
if ([string]::IsNullOrWhiteSpace($requested) -or -not (Test-Path -LiteralPath $requested)) {
    throw '所选路径不存在或当前无法访问'
}
$fullPath = [System.IO.Path]::GetFullPath($requested)
$driveRoot = [System.IO.Path]::GetPathRoot($fullPath)
if ($driveRoot -notmatch '^[A-Za-z]:\\$') {
    throw '请选择带盘符的本地 SD 卡，例如 E:\'
}
$driveLetter = $driveRoot.Substring(0, 1).ToUpperInvariant()
$partitions = @(Get-Partition -DriveLetter $driveLetter -ErrorAction Stop)
if ($partitions.Count -ne 1) {
    throw '无法唯一确定所选盘符对应的磁盘分区'
}
$partition = $partitions[0]
$disk = $partition | Get-Disk -ErrorAction Stop
$volume = Get-Volume -DriveLetter $driveLetter -ErrorAction Stop
[pscustomobject]@{
    DriveRoot = $driveRoot
    DriveLetter = $driveLetter
    VolumeName = [string]$volume.FileSystemLabel
    DiskNumber = [uint32]$disk.Number
    PartitionNumber = [uint32]$partition.PartitionNumber
    SizeBytes = [uint64]$disk.Size
    LogicalSectorSize = [uint64]$disk.LogicalSectorSize
    BusType = [string]$disk.BusType
    FriendlyName = [string]$disk.FriendlyName
    IsBoot = [bool]$disk.IsBoot
    IsSystem = [bool]$disk.IsSystem
} | ConvertTo-Json -Compress
"#;

    let requested_path = requested_path.trim();
    if requested_path.is_empty() {
        return Err("请输入 SD 卡盘符或路径，例如 E:\\".into());
    }
    let output = windows_powershell()
        .env("TRACEDISK_SELECTED_PATH", requested_path)
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            SCRIPT,
        ])
        .output()
        .map_err(|error| format!("无法启动 Windows 磁盘检查: {error}"))?;
    let volume: WindowsVolumeInfo = parse_windows_powershell_json(output, "Windows 磁盘检查")?;
    validate_windows_disk_safety(
        volume.disk_number,
        &volume.bus_type,
        volume.is_boot,
        volume.is_system,
    )?;
    validate_windows_geometry(
        volume.size_bytes,
        volume.logical_sector_size,
        volume.disk_number,
    )?;

    let raw_device_path = format!(r"\\.\PhysicalDrive{}", volume.disk_number);
    Ok(SdCardInfo {
        requested_path: requested_path.to_string(),
        mount_point: volume.drive_root,
        volume_name: if volume.volume_name.trim().is_empty() {
            format!("{}:\\", volume.drive_letter)
        } else {
            volume.volume_name
        },
        media_name: if volume.friendly_name.trim().is_empty() {
            format!("Windows removable disk {}", volume.disk_number)
        } else {
            volume.friendly_name
        },
        partition_identifier: format!(
            "Disk {} Partition {}",
            volume.disk_number, volume.partition_number
        ),
        whole_disk_identifier: format!("PhysicalDrive{}", volume.disk_number),
        device_node: raw_device_path.clone(),
        raw_device_path,
        size_bytes: volume.size_bytes,
        removable: true,
        ejectable: true,
        raw_readable: false,
    })
}

#[cfg(target_os = "windows")]
pub(crate) fn validate_external_raw_device(
    raw_device_path: &str,
    expected_size_bytes: u64,
) -> Result<ValidatedRawDevice, String> {
    const SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
$diskNumber = [uint32]$env:TRACEDISK_DISK_NUMBER
$disk = Get-Disk -Number $diskNumber -ErrorAction Stop
[pscustomobject]@{
    DiskNumber = [uint32]$disk.Number
    SizeBytes = [uint64]$disk.Size
    LogicalSectorSize = [uint64]$disk.LogicalSectorSize
    BusType = [string]$disk.BusType
    IsBoot = [bool]$disk.IsBoot
    IsSystem = [bool]$disk.IsSystem
} | ConvertTo-Json -Compress
"#;

    let disk_number = windows_physical_drive_number(raw_device_path).ok_or_else(|| {
        format!(r"安全保护：无效的原始整盘设备路径 {raw_device_path}，只接受 \\.\PhysicalDriveN")
    })?;
    let normalized_path = format!(r"\\.\PhysicalDrive{disk_number}");
    if !normalized_path.eq_ignore_ascii_case(raw_device_path) {
        return Err("安全保护：Windows 原始设备路径未通过规范化检查".into());
    }
    let output = windows_powershell()
        .env("TRACEDISK_DISK_NUMBER", disk_number.to_string())
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            SCRIPT,
        ])
        .output()
        .map_err(|error| format!("无法启动 Windows 物理磁盘复核: {error}"))?;
    let disk: WindowsDiskInfo = parse_windows_powershell_json(output, "Windows 物理磁盘复核")?;
    if disk.disk_number != disk_number {
        return Err("安全保护：Windows 物理磁盘编号在校验期间发生变化".into());
    }
    validate_windows_disk_safety(
        disk.disk_number,
        &disk.bus_type,
        disk.is_boot,
        disk.is_system,
    )?;
    validate_windows_geometry(disk.size_bytes, disk.logical_sector_size, disk.disk_number)?;
    if !capacity_within_tolerance(expected_size_bytes, disk.size_bytes) {
        return Err(format!(
            "安全保护：设备容量差异超过允许范围（之前 {expected_size_bytes}，现在 {} 字节），请重新识别 SD 卡",
            disk.size_bytes
        ));
    }

    Ok(ValidatedRawDevice {
        raw_device_path: normalized_path,
        whole_disk_identifier: format!("PhysicalDrive{disk_number}"),
        size_bytes: disk.size_bytes,
        block_size: disk.logical_sector_size,
    })
}

#[cfg(target_os = "windows")]
pub(crate) fn prepare_for_raw_read(device: &ValidatedRawDevice) -> Result<(), String> {
    const SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
$diskNumber = [uint32]$env:TRACEDISK_DISK_NUMBER
$partitions = @(Get-Partition -DiskNumber $diskNumber -ErrorAction Stop)
foreach ($partition in $partitions) {
    $letter = [string]$partition.DriveLetter
    if ($letter -match '^[A-Za-z]$') {
        $mountPoint = $letter.ToUpperInvariant() + ':\'
        & "$env:SystemRoot\System32\mountvol.exe" $mountPoint /p
        if ($LASTEXITCODE -ne 0) {
            throw "无法卸载卷 $mountPoint"
        }
    }
}
"#;

    let disk_number = windows_physical_drive_number(&device.raw_device_path)
        .ok_or_else(|| "安全保护：拒绝卸载无效 Windows 物理磁盘标识".to_string())?;
    if device.whole_disk_identifier != format!("PhysicalDrive{disk_number}") {
        return Err("安全保护：Windows 物理磁盘标识在卸载前不一致".into());
    }
    let output = windows_powershell()
        .env("TRACEDISK_DISK_NUMBER", disk_number.to_string())
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            SCRIPT,
        ])
        .output()
        .map_err(|error| format!("无法启动 Windows 卷卸载: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if message.is_empty() {
            "无法卸载 SD 卡卷，请关闭资源管理器和正在使用该盘符的应用后重试".into()
        } else {
            format!("无法卸载 SD 卡卷: {message}")
        })
    }
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn prepare_for_raw_read(_device: &ValidatedRawDevice) -> Result<(), String> {
    Ok(())
}

#[cfg(any(target_os = "windows", test))]
fn windows_physical_drive_number(path: &str) -> Option<u32> {
    const PREFIX: &str = r"\\.\PhysicalDrive";
    if !path
        .get(..PREFIX.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(PREFIX))
    {
        return None;
    }
    let digits = &path[PREFIX.len()..];
    if digits.is_empty() || !digits.chars().all(|character| character.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

#[cfg(target_os = "windows")]
fn validate_windows_disk_safety(
    disk_number: u32,
    bus_type: &str,
    is_boot: bool,
    is_system: bool,
) -> Result<(), String> {
    if is_boot || is_system {
        return Err(format!(
            "安全保护：PhysicalDrive{disk_number} 是 Windows 启动盘或系统盘，TraceDisk 拒绝读取"
        ));
    }
    if !matches!(bus_type.to_ascii_uppercase().as_str(), "USB" | "SD" | "MMC") {
        return Err(format!(
            "安全保护：PhysicalDrive{disk_number} 的总线类型是 {bus_type}，只接受 USB、SD 或 MMC 可移动设备"
        ));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn validate_windows_geometry(
    size_bytes: u64,
    block_size: u64,
    disk_number: u32,
) -> Result<(), String> {
    if size_bytes < 1024 * 1024
        || !(512..=1024 * 1024).contains(&block_size)
        || !block_size.is_power_of_two()
        || !size_bytes.is_multiple_of(block_size)
    {
        return Err(format!(
            "安全保护：PhysicalDrive{disk_number} 的设备参数异常（容量 {size_bytes}，扇区 {block_size} 字节）"
        ));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn parse_windows_powershell_json<T: for<'de> Deserialize<'de>>(
    output: std::process::Output,
    label: &str,
) -> Result<T, String> {
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if message.is_empty() {
            format!("{label}没有返回结果")
        } else {
            format!("{label}失败: {message}")
        });
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("无法解析{label}返回的数据: {error}"))
}

#[cfg(target_os = "windows")]
fn windows_powershell() -> Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut command = Command::new("powershell.exe");
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn resolve_sd_card_path_blocking(_requested_path: &str) -> Result<SdCardInfo, String> {
    Err("当前 SD 卡路径解析流程只支持 macOS 和 Windows".into())
}

#[cfg(target_os = "macos")]
fn diskutil_info(target: &str) -> Result<Dictionary, String> {
    let output = Command::new("/usr/sbin/diskutil")
        .args(["info", "-plist", target])
        .output()
        .map_err(|error| format!("无法运行 diskutil: {error}"))?;

    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if message.is_empty() {
            format!("diskutil 无法识别路径或设备：{target}")
        } else {
            message
        });
    }

    let value = Value::from_reader(Cursor::new(output.stdout))
        .map_err(|error| format!("无法解析 diskutil 返回的数据: {error}"))?;
    value
        .into_dictionary()
        .ok_or_else(|| "diskutil 返回的不是设备属性字典".into())
}

#[cfg(any(target_os = "macos", test))]
fn normalize_whole_disk_identifier(identifier: &str) -> Option<String> {
    let remainder = identifier.strip_prefix("disk")?;
    let digit_count = remainder
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .count();
    if digit_count == 0 {
        return None;
    }

    let digits = &remainder[..digit_count];
    let suffix = &remainder[digit_count..];
    let valid_suffix = suffix.is_empty()
        || suffix
            .strip_prefix('s')
            .is_some_and(|slice| !slice.is_empty() && slice.chars().all(|c| c.is_ascii_digit()));
    valid_suffix.then(|| format!("disk{digits}"))
}

#[cfg(target_os = "macos")]
fn dictionary_string<'a>(dictionary: &'a Dictionary, key: &str) -> Option<&'a str> {
    dictionary.get(key)?.as_string()
}

#[cfg(target_os = "macos")]
fn dictionary_bool(dictionary: &Dictionary, key: &str) -> Option<bool> {
    dictionary.get(key)?.as_boolean()
}

#[cfg(target_os = "macos")]
fn dictionary_u64(dictionary: &Dictionary, key: &str) -> Option<u64> {
    dictionary.get(key)?.as_unsigned_integer()
}

#[cfg(test)]
mod tests {
    use super::{
        capacity_within_tolerance, normalize_whole_disk_identifier, raw_path_to_whole_identifier,
        windows_physical_drive_number,
    };

    #[test]
    fn normalizes_partition_and_whole_disk_identifiers() {
        assert_eq!(
            normalize_whole_disk_identifier("disk7s1").as_deref(),
            Some("disk7")
        );
        assert_eq!(
            normalize_whole_disk_identifier("disk12").as_deref(),
            Some("disk12")
        );
    }

    #[test]
    fn rejects_untrusted_device_identifiers() {
        assert_eq!(normalize_whole_disk_identifier("/dev/disk7"), None);
        assert_eq!(normalize_whole_disk_identifier("disk"), None);
        assert_eq!(normalize_whole_disk_identifier("disk7;rm"), None);
        assert_eq!(normalize_whole_disk_identifier("disk7s"), None);
    }

    #[test]
    fn accepts_only_raw_whole_disk_paths() {
        assert_eq!(
            raw_path_to_whole_identifier("/dev/rdisk7").as_deref(),
            Some("disk7")
        );
        assert_eq!(raw_path_to_whole_identifier("/dev/rdisk7s1"), None);
        assert_eq!(raw_path_to_whole_identifier("/dev/disk7"), None);
        assert_eq!(raw_path_to_whole_identifier("/dev/rdisk7;touch"), None);
    }

    #[test]
    fn allows_small_diskutil_capacity_drift_but_rejects_device_changes() {
        assert!(capacity_within_tolerance(124_697_837_568, 124_702_949_376));
        assert!(!capacity_within_tolerance(124_697_837_568, 128_000_000_000));
        assert!(!capacity_within_tolerance(0, 124_702_949_376));
    }

    #[test]
    fn accepts_only_normalized_windows_physical_drive_paths() {
        assert_eq!(
            windows_physical_drive_number(r"\\.\PhysicalDrive7"),
            Some(7)
        );
        assert_eq!(
            windows_physical_drive_number(r"\\.\physicaldrive12"),
            Some(12)
        );
        assert_eq!(
            windows_physical_drive_number(r"\\.\PhysicalDrive7\Partition1"),
            None
        );
        assert_eq!(windows_physical_drive_number(r"E:\"), None);
        assert_eq!(windows_physical_drive_number("设备路径"), None);
    }
}
