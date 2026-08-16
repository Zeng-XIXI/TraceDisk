mod device;

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::ipc::Channel;
use tracedisk_core::{BlockSource, RecoveryExtent};

const HELPER_SCAN_FLAG: &str = "--tracedisk-helper-scan";
const HELPER_RECOVER_FLAG: &str = "--tracedisk-helper-recover";
const HELPER_BATCH_RECOVER_FLAG: &str = "--tracedisk-helper-batch-recover";
const RECOVERY_BATCH_SIZE: usize = 8 * 1024 * 1024;
const MAX_RECOVERY_EXTENTS: usize = 65_536;
const MAX_BATCH_ITEMS: usize = 4096;
const PROGRESS_POLL_INTERVAL: Duration = Duration::from_millis(200);

static ACTIVE_SCAN_CANCEL_PATH: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct DeviceScanProgress {
    phase: String,
    bytes_examined: u64,
    total_bytes: u64,
    candidates_found: usize,
}

struct ScanControlFiles {
    directory: PathBuf,
    progress: PathBuf,
    result: PathBuf,
    cancel: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
struct RecoveryPlan {
    size_bytes: u64,
    extents: Vec<RecoveryExtent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BatchRecoveryItem {
    name: String,
    size_bytes: u64,
    extents: Vec<RecoveryExtent>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PreparedBatchRecoveryItem {
    output_path: String,
    output_name: String,
    size_bytes: u64,
    extents: Vec<RecoveryExtent>,
}

#[derive(Debug, Serialize, Deserialize)]
struct BatchRecoveryPlan {
    output_directory: String,
    total_bytes: u64,
    items: Vec<PreparedBatchRecoveryItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct BatchExportProgress {
    phase: String,
    current_file: Option<String>,
    processed_files: usize,
    successful_files: usize,
    total_files: usize,
    bytes_processed: u64,
    total_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct BatchExportFailure {
    name: String,
    error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct BatchExportResult {
    output_directory: String,
    successful_files: Vec<String>,
    failures: Vec<BatchExportFailure>,
    warnings: Vec<String>,
    bytes_written: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct DestinationCapacity {
    path: String,
    available_bytes: u64,
}

struct RecoveryControlFile {
    directory: PathBuf,
    plan: PathBuf,
}

struct BatchExportControlFiles {
    directory: PathBuf,
    plan: PathBuf,
    progress: PathBuf,
    result: PathBuf,
}

impl BatchExportControlFiles {
    fn create(plan: &BatchRecoveryPlan) -> Result<Self, String> {
        #[cfg(target_os = "macos")]
        let base = Path::new("/private/tmp");
        #[cfg(not(target_os = "macos"))]
        let base = std::env::temp_dir();

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| format!("无法创建批量导出控制目录: {error}"))?
            .as_nanos();
        let directory = base.join(format!("tracedisk-export-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&directory)
            .map_err(|error| format!("无法创建批量导出控制目录: {error}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
                .map_err(|error| format!("无法保护批量导出控制目录: {error}"))?;
        }

        let plan_path = directory.join("plan.json");
        let progress = directory.join("progress.json");
        let result = directory.join("result.json");
        let creation = (|| {
            let bytes = serde_json::to_vec(plan)
                .map_err(|error| format!("无法序列化批量导出计划: {error}"))?;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&plan_path)
                .map_err(|error| format!("无法创建批量导出计划: {error}"))?;
            file.write_all(&bytes)
                .map_err(|error| format!("无法写入批量导出计划: {error}"))?;
            file.sync_all()
                .map_err(|error| format!("无法同步批量导出计划: {error}"))?;
            create_empty_control_file(&progress)?;
            create_empty_control_file(&result)?;
            Ok(Self {
                directory: directory.clone(),
                plan: plan_path.clone(),
                progress: progress.clone(),
                result: result.clone(),
            })
        })();
        if creation.is_err() {
            let _ = std::fs::remove_file(&plan_path);
            let _ = std::fs::remove_file(&progress);
            let _ = std::fs::remove_file(&result);
            let _ = std::fs::remove_dir(&directory);
        }
        creation
    }
}

impl Drop for BatchExportControlFiles {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.plan);
        let _ = std::fs::remove_file(&self.progress);
        let _ = std::fs::remove_file(&self.result);
        let _ = std::fs::remove_dir(&self.directory);
    }
}

impl RecoveryControlFile {
    fn create(plan: &RecoveryPlan) -> Result<Self, String> {
        #[cfg(target_os = "macos")]
        let base = Path::new("/private/tmp");
        #[cfg(not(target_os = "macos"))]
        let base = std::env::temp_dir();

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| format!("无法创建恢复控制目录: {error}"))?
            .as_nanos();
        let directory = base.join(format!("tracedisk-recovery-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&directory)
            .map_err(|error| format!("无法创建恢复控制目录: {error}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
                .map_err(|error| format!("无法保护恢复控制目录: {error}"))?;
        }

        let plan_path = directory.join("plan.json");
        let result = (|| {
            let bytes = serde_json::to_vec(plan)
                .map_err(|error| format!("无法序列化恢复片段计划: {error}"))?;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&plan_path)
                .map_err(|error| format!("无法创建恢复片段计划: {error}"))?;
            file.write_all(&bytes)
                .map_err(|error| format!("无法写入恢复片段计划: {error}"))?;
            file.sync_all()
                .map_err(|error| format!("无法同步恢复片段计划: {error}"))?;
            Ok(Self {
                directory: directory.clone(),
                plan: plan_path.clone(),
            })
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&plan_path);
            let _ = std::fs::remove_dir(&directory);
        }
        result
    }
}

impl Drop for RecoveryControlFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.plan);
        let _ = std::fs::remove_dir(&self.directory);
    }
}

impl ScanControlFiles {
    fn create() -> Result<Self, String> {
        #[cfg(target_os = "macos")]
        let base = Path::new("/private/tmp");
        #[cfg(not(target_os = "macos"))]
        let base = std::env::temp_dir();

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| format!("无法创建扫描控制目录: {error}"))?
            .as_nanos();
        let directory = base.join(format!("tracedisk-scan-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&directory)
            .map_err(|error| format!("无法创建扫描控制目录: {error}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
                .map_err(|error| format!("无法保护扫描控制目录: {error}"))?;
        }

        let progress = directory.join("progress.json");
        let result = directory.join("result.json");
        create_empty_control_file(&progress)?;
        create_empty_control_file(&result)?;
        Ok(Self {
            cancel: directory.join("cancel"),
            directory,
            progress,
            result,
        })
    }

    fn activate(&self) -> Result<(), String> {
        let mut active = active_cancel_path()
            .lock()
            .map_err(|_| "扫描状态锁已损坏".to_string())?;
        if active.is_some() {
            return Err("已有扫描正在运行".into());
        }
        *active = Some(self.cancel.clone());
        Ok(())
    }
}

impl Drop for ScanControlFiles {
    fn drop(&mut self) {
        if let Ok(mut active) = active_cancel_path().lock() {
            if active.as_ref() == Some(&self.cancel) {
                *active = None;
            }
        }
        let _ = std::fs::remove_file(&self.progress);
        let _ = std::fs::remove_file(&self.result);
        let _ = std::fs::remove_file(&self.cancel);
        let _ = std::fs::remove_dir(&self.directory);
    }
}

fn active_cancel_path() -> &'static Mutex<Option<PathBuf>> {
    ACTIVE_SCAN_CANCEL_PATH.get_or_init(|| Mutex::new(None))
}

fn create_empty_control_file(path: &Path) -> Result<(), String> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map(|_| ())
        .map_err(|error| format!("无法创建扫描控制文件: {error}"))
}

#[tauri::command]
async fn inspect_image(path: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        tracedisk_core::inspect_image(path)
            .map(|report| report.to_json_pretty())
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("inspection task failed: {error}"))?
}

#[tauri::command]
async fn scan_raw_device(
    raw_device_path: String,
    size_bytes: u64,
    mode: String,
    on_progress: Channel<DeviceScanProgress>,
) -> Result<String, String> {
    if mode != "metadata" && mode != "deep" {
        return Err("未知扫描模式，只接受 metadata 或 deep".into());
    }

    tauri::async_runtime::spawn_blocking(move || {
        scan_raw_device_blocking(&raw_device_path, size_bytes, &mode, &on_progress)
    })
    .await
    .map_err(|error| format!("scan task failed: {error}"))?
}

#[tauri::command]
fn cancel_active_scan() -> Result<bool, String> {
    let cancel_path = active_cancel_path()
        .lock()
        .map_err(|_| "扫描状态锁已损坏".to_string())?
        .clone();
    let Some(cancel_path) = cancel_path else {
        return Ok(false);
    };
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(cancel_path)
    {
        Ok(mut file) => file
            .write_all(b"cancel")
            .map(|_| true)
            .map_err(|error| format!("无法发送停止请求: {error}")),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(true),
        Err(error) => Err(format!("无法发送停止请求: {error}")),
    }
}

#[tauri::command]
fn open_full_disk_access_settings() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let status = Command::new("/usr/bin/open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles")
            .status()
            .map_err(|error| format!("无法打开系统设置: {error}"))?;
        if !status.success() {
            return Err("无法打开完全磁盘访问权限设置".into());
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err("此设置仅适用于 macOS".into())
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn scan_raw_device_blocking(
    raw_device_path: &str,
    size_bytes: u64,
    mode: &str,
    on_progress: &Channel<DeviceScanProgress>,
) -> Result<String, String> {
    let control = ScanControlFiles::create()?;
    control.activate()?;
    send_scan_progress(on_progress, "preparing", 0, size_bytes, 0);
    let mounted = device::validate_external_raw_device(raw_device_path, size_bytes)?;
    #[cfg(target_os = "macos")]
    let validated = {
        device::unmount_whole_disk(&mounted.whole_disk_identifier)?;
        // diskutil can report a slightly different raw-media length after the
        // mounted filesystem view disappears. Re-read it and pass that bounded
        // post-unmount length to the privileged helper.
        device::validate_external_raw_device(raw_device_path, mounted.size_bytes)?
    };
    #[cfg(target_os = "windows")]
    let validated = mounted;

    let executable = std::env::current_exe()
        .map_err(|error| format!("无法定位 TraceDisk 可执行文件: {error}"))?;
    let executable = executable
        .to_str()
        .ok_or_else(|| "TraceDisk 可执行文件路径不是有效 UTF-8".to_string())?;
    let size = validated.size_bytes.to_string();
    let control_directory = control
        .directory
        .to_str()
        .ok_or_else(|| "扫描控制目录不是有效 UTF-8".to_string())?;
    run_scan_as_administrator(
        &[
            executable,
            HELPER_SCAN_FLAG,
            mode,
            &validated.raw_device_path,
            &size,
            control_directory,
        ],
        &control,
        on_progress,
    )?;
    let json = std::fs::read_to_string(&control.result)
        .map_err(|error| format!("无法读取扫描结果: {error}"))?;
    let report = serde_json::from_str::<tracedisk_core::ScanReport>(&json)
        .map_err(|error| format!("扫描助手返回了无效结果: {error}"))?;
    send_scan_progress(
        on_progress,
        if report.cancelled {
            "cancelled"
        } else {
            "completed"
        },
        report.bytes_examined,
        report.source_length,
        report.candidates.len(),
    );
    Ok(json)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn scan_raw_device_blocking(
    _raw_device_path: &str,
    _size_bytes: u64,
    _mode: &str,
    _on_progress: &Channel<DeviceScanProgress>,
) -> Result<String, String> {
    Err("直接扫描原始 SD 卡当前只支持 macOS 和 Windows".into())
}

#[tauri::command]
async fn recover_candidate(
    raw_device_path: String,
    source_size_bytes: u64,
    size_bytes: u64,
    extents: Vec<RecoveryExtent>,
    output_path: String,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        recover_candidate_blocking(
            &raw_device_path,
            source_size_bytes,
            size_bytes,
            &extents,
            &output_path,
        )
    })
    .await
    .map_err(|error| format!("recovery task failed: {error}"))?
}

#[tauri::command]
fn check_export_destination(output_directory: String) -> Result<DestinationCapacity, String> {
    let path = validate_output_directory(&output_directory)?;
    Ok(DestinationCapacity {
        path: path.to_string_lossy().into_owned(),
        available_bytes: available_space_bytes(&path)?,
    })
}

#[tauri::command]
async fn recover_candidates_batch(
    raw_device_path: String,
    source_size_bytes: u64,
    output_directory: String,
    items: Vec<BatchRecoveryItem>,
    on_progress: Channel<BatchExportProgress>,
) -> Result<BatchExportResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        recover_candidates_batch_blocking(
            &raw_device_path,
            source_size_bytes,
            &output_directory,
            items,
            &on_progress,
        )
    })
    .await
    .map_err(|error| format!("batch recovery task failed: {error}"))?
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn recover_candidates_batch_blocking(
    raw_device_path: &str,
    source_size_bytes: u64,
    output_directory: &str,
    items: Vec<BatchRecoveryItem>,
    on_progress: &Channel<BatchExportProgress>,
) -> Result<BatchExportResult, String> {
    let plan = prepare_batch_recovery_plan(output_directory, items, source_size_bytes)?;
    ensure_destination_capacity(&plan.output_directory, plan.total_bytes)?;
    send_batch_export_progress(
        on_progress,
        "preparing",
        None,
        0,
        0,
        plan.items.len(),
        0,
        plan.total_bytes,
    );
    let control = BatchExportControlFiles::create(&plan)?;
    let mounted = device::validate_external_raw_device(raw_device_path, source_size_bytes)?;
    #[cfg(target_os = "macos")]
    let validated = {
        device::unmount_whole_disk(&mounted.whole_disk_identifier)?;
        device::validate_external_raw_device(raw_device_path, mounted.size_bytes)?
    };
    #[cfg(target_os = "windows")]
    let validated = mounted;
    validate_batch_recovery_plan(&plan, validated.size_bytes)?;
    ensure_destination_capacity(&plan.output_directory, plan.total_bytes)?;

    let executable = std::env::current_exe()
        .map_err(|error| format!("无法定位 TraceDisk 可执行文件: {error}"))?;
    let executable = executable
        .to_str()
        .ok_or_else(|| "TraceDisk 可执行文件路径不是有效 UTF-8".to_string())?;
    let source_size = validated.size_bytes.to_string();
    let control_directory = control
        .directory
        .to_str()
        .ok_or_else(|| "批量导出控制目录路径不是有效 UTF-8".to_string())?;
    let (user_id, group_id) = current_owner_arguments()?;
    run_batch_export_as_administrator(
        &[
            executable,
            HELPER_BATCH_RECOVER_FLAG,
            &validated.raw_device_path,
            &source_size,
            control_directory,
            &user_id,
            &group_id,
        ],
        &control,
        on_progress,
    )?;
    let json = std::fs::read_to_string(&control.result)
        .map_err(|error| format!("无法读取批量导出结果: {error}"))?;
    serde_json::from_str::<BatchExportResult>(&json)
        .map_err(|error| format!("批量导出助手返回了无效结果: {error}"))
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn recover_candidates_batch_blocking(
    _raw_device_path: &str,
    _source_size_bytes: u64,
    _output_directory: &str,
    _items: Vec<BatchRecoveryItem>,
    _on_progress: &Channel<BatchExportProgress>,
) -> Result<BatchExportResult, String> {
    Err("批量恢复当前只支持 macOS 和 Windows".into())
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn recover_candidate_blocking(
    raw_device_path: &str,
    source_size_bytes: u64,
    size_bytes: u64,
    extents: &[RecoveryExtent],
    output_path: &str,
) -> Result<String, String> {
    validate_output_path(output_path)?;
    validate_recovery_plan(extents, size_bytes, source_size_bytes)?;
    let control = RecoveryControlFile::create(&RecoveryPlan {
        size_bytes,
        extents: extents.to_vec(),
    })?;
    let mounted = device::validate_external_raw_device(raw_device_path, source_size_bytes)?;
    #[cfg(target_os = "macos")]
    let validated = {
        device::unmount_whole_disk(&mounted.whole_disk_identifier)?;
        device::validate_external_raw_device(raw_device_path, mounted.size_bytes)?
    };
    #[cfg(target_os = "windows")]
    let validated = mounted;
    validate_recovery_plan(extents, size_bytes, validated.size_bytes)?;

    let executable = std::env::current_exe()
        .map_err(|error| format!("无法定位 TraceDisk 可执行文件: {error}"))?;
    let executable = executable
        .to_str()
        .ok_or_else(|| "TraceDisk 可执行文件路径不是有效 UTF-8".to_string())?;
    let source_size = validated.size_bytes.to_string();
    let plan_path = control
        .plan
        .to_str()
        .ok_or_else(|| "恢复片段计划路径不是有效 UTF-8".to_string())?;
    let (user_id, group_id) = current_owner_arguments()?;
    run_as_administrator(&[
        executable,
        HELPER_RECOVER_FLAG,
        &validated.raw_device_path,
        &source_size,
        plan_path,
        output_path,
        &user_id,
        &group_id,
    ])
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn recover_candidate_blocking(
    _raw_device_path: &str,
    _source_size_bytes: u64,
    _size_bytes: u64,
    _extents: &[RecoveryExtent],
    _output_path: &str,
) -> Result<String, String> {
    Err("直接从原始 SD 卡恢复当前只支持 macOS 和 Windows".into())
}

fn prepare_batch_recovery_plan(
    output_directory: &str,
    items: Vec<BatchRecoveryItem>,
    source_length: u64,
) -> Result<BatchRecoveryPlan, String> {
    if items.is_empty() || items.len() > MAX_BATCH_ITEMS {
        return Err("请选择至少一个且不超过 4096 个可恢复文件".into());
    }
    let directory = validate_output_directory(output_directory)?;
    let mut total_bytes = 0_u64;
    let mut used_names = HashSet::new();
    let mut prepared = Vec::with_capacity(items.len());
    for item in items {
        validate_recovery_plan(&item.extents, item.size_bytes, source_length)?;
        validate_output_name(&item.name)?;
        total_bytes = total_bytes
            .checked_add(item.size_bytes)
            .ok_or_else(|| "批量导出总大小溢出".to_string())?;
        let output_name = unique_output_name(&directory, &item.name, &mut used_names)?;
        let output_path = directory.join(&output_name);
        prepared.push(PreparedBatchRecoveryItem {
            output_path: output_path.to_string_lossy().into_owned(),
            output_name,
            size_bytes: item.size_bytes,
            extents: item.extents,
        });
    }
    Ok(BatchRecoveryPlan {
        output_directory: directory.to_string_lossy().into_owned(),
        total_bytes,
        items: prepared,
    })
}

fn validate_batch_recovery_plan(
    plan: &BatchRecoveryPlan,
    source_length: u64,
) -> Result<(), String> {
    if plan.items.is_empty() || plan.items.len() > MAX_BATCH_ITEMS {
        return Err("批量导出计划中的文件数量异常".into());
    }
    let directory = validate_output_directory(&plan.output_directory)?;
    let mut total_bytes = 0_u64;
    let mut names = HashSet::new();
    for item in &plan.items {
        validate_output_name(&item.output_name)?;
        validate_recovery_plan(&item.extents, item.size_bytes, source_length)?;
        let output_path = Path::new(&item.output_path);
        if output_path.parent() != Some(directory.as_path())
            || output_path.file_name().and_then(|name| name.to_str())
                != Some(item.output_name.as_str())
            || !names.insert(item.output_name.to_lowercase())
        {
            return Err("批量导出计划包含无效或重复的目标文件名".into());
        }
        total_bytes = total_bytes
            .checked_add(item.size_bytes)
            .ok_or_else(|| "批量导出总大小溢出".to_string())?;
    }
    if total_bytes != plan.total_bytes {
        return Err("批量导出计划总大小不一致".into());
    }
    Ok(())
}

fn validate_output_directory(output_directory: &str) -> Result<PathBuf, String> {
    let path = Path::new(output_directory);
    if !path.is_absolute() || !path.is_dir() {
        return Err("批量导出目标必须是已经存在的绝对文件夹".into());
    }
    std::fs::canonicalize(path).map_err(|error| format!("无法解析批量导出目标文件夹: {error}"))
}

fn validate_output_name(name: &str) -> Result<(), String> {
    let path = Path::new(name);
    if name.is_empty()
        || name.len() > 200
        || name.contains('\0')
        || path.file_name().and_then(|value| value.to_str()) != Some(name)
        || matches!(name, "." | "..")
    {
        return Err(format!("恢复文件名无效: {name}"));
    }
    #[cfg(target_os = "windows")]
    if !is_windows_safe_output_name(name) {
        return Err(format!(
            "恢复文件名包含 Windows 不允许的字符或保留名称: {name}"
        ));
    }
    Ok(())
}

#[cfg(any(target_os = "windows", test))]
fn is_windows_safe_output_name(name: &str) -> bool {
    if name.ends_with([' ', '.'])
        || name.chars().any(|character| {
            character < ' '
                || matches!(
                    character,
                    '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
                )
        })
    {
        return false;
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches([' ', '.'])
        .to_ascii_uppercase();
    !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        && !(stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit()
            && stem.as_bytes()[3] != b'0')
}

fn unique_output_name(
    directory: &Path,
    requested_name: &str,
    used_names: &mut HashSet<String>,
) -> Result<String, String> {
    let path = Path::new(requested_name);
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("RECOVERED");
    let extension = path.extension().and_then(|value| value.to_str());
    for index in 1..=100_000 {
        let name = if index == 1 {
            requested_name.to_string()
        } else if let Some(extension) = extension {
            format!("{stem} ({index}).{extension}")
        } else {
            format!("{stem} ({index})")
        };
        validate_output_name(&name)?;
        let key = name.to_lowercase();
        if !used_names.contains(&key) && !directory.join(&name).exists() {
            used_names.insert(key);
            return Ok(name);
        }
    }
    Err(format!("无法为恢复文件生成不重复的名称: {requested_name}"))
}

fn ensure_destination_capacity(output_directory: &str, required_bytes: u64) -> Result<(), String> {
    let directory = validate_output_directory(output_directory)?;
    let available = available_space_bytes(&directory)?;
    if available < required_bytes {
        return Err(format!(
            "目标磁盘空间不足：批量导出需要 {required_bytes} 字节，可用 {available} 字节"
        ));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn available_space_bytes(path: &Path) -> Result<u64, String> {
    let output = Command::new("/bin/df")
        .env("LC_ALL", "C")
        .args(["-kP"])
        .arg(path)
        .output()
        .map_err(|error| format!("无法查询目标磁盘可用空间: {error}"))?;
    if !output.status.success() {
        return Err("无法查询目标磁盘可用空间".into());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text
        .lines()
        .rfind(|line| !line.trim().is_empty())
        .ok_or_else(|| "目标磁盘空间查询没有返回结果".to_string())?;
    let available_kib = line
        .split_whitespace()
        .nth(3)
        .ok_or_else(|| "无法解析目标磁盘可用空间".to_string())?
        .parse::<u64>()
        .map_err(|_| "目标磁盘可用空间不是有效数字".to_string())?;
    available_kib
        .checked_mul(1024)
        .ok_or_else(|| "目标磁盘可用空间数值溢出".to_string())
}

#[cfg(target_os = "windows")]
fn available_space_bytes(path: &Path) -> Result<u64, String> {
    const SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
$path = [System.IO.Path]::GetFullPath($env:TRACEDISK_DESTINATION_PATH)
$root = [System.IO.Path]::GetPathRoot($path)
$drive = [System.IO.DriveInfo]::new($root)
[Console]::Out.Write([uint64]$drive.AvailableFreeSpace)
"#;
    let output = windows_powershell_command()
        .env("TRACEDISK_DESTINATION_PATH", path)
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            SCRIPT,
        ])
        .output()
        .map_err(|error| format!("无法启动 Windows 目标磁盘空间查询: {error}"))?;
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() || value.parse::<u64>().is_err() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if message.is_empty() {
            "无法查询 Windows 目标磁盘可用空间".into()
        } else {
            format!("无法查询 Windows 目标磁盘可用空间: {message}")
        });
    }
    value
        .parse::<u64>()
        .map_err(|_| "Windows 目标磁盘可用空间不是有效数字".to_string())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn available_space_bytes(_path: &Path) -> Result<u64, String> {
    Err("目标磁盘容量检测当前只支持 macOS 和 Windows".into())
}

#[cfg(target_os = "macos")]
fn run_as_administrator(arguments: &[&str]) -> Result<String, String> {
    const SCRIPT: &str = r#"
on run argv
    set commandText to ""
    repeat with argumentValue in argv
        if commandText is not "" then set commandText to commandText & " "
        set commandText to commandText & quoted form of (argumentValue as text)
    end repeat
    with timeout of 86400 seconds
        return do shell script commandText with administrator privileges
    end timeout
end run
"#;

    let output = Command::new("/usr/bin/osascript")
        .args(["-e", SCRIPT, "--"])
        .args(arguments)
        .output()
        .map_err(|error| format!("无法启动 macOS 管理员授权: {error}"))?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if message.contains("(-128)") {
            "你取消了管理员授权，尚未读取 SD 卡".into()
        } else if message.is_empty() {
            "管理员只读扫描未能启动".into()
        } else {
            format!("管理员只读扫描失败: {message}")
        });
    }

    let result = String::from_utf8(output.stdout)
        .map_err(|error| format!("扫描结果不是有效 UTF-8: {error}"))?;
    Ok(result.trim().to_string())
}

#[cfg(target_os = "windows")]
fn run_as_administrator(arguments: &[&str]) -> Result<String, String> {
    let output = windows_elevated_command(arguments)?
        .output()
        .map_err(|error| format!("无法启动 Windows UAC 管理员授权辅助进程: {error}"))?;
    if output.status.success() {
        Ok("ok".into())
    } else {
        Err(windows_administrator_error(&output.stderr))
    }
}

#[cfg(target_os = "windows")]
fn windows_elevated_command(arguments: &[&str]) -> Result<Command, String> {
    const SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
$process = Start-Process -FilePath $env:TRACEDISK_ELEVATED_EXE -ArgumentList $env:TRACEDISK_ELEVATED_ARGUMENTS -Verb RunAs -Wait -PassThru
exit [int]$process.ExitCode
"#;
    let (executable, helper_arguments) = arguments
        .split_first()
        .ok_or_else(|| "Windows 管理员辅助进程缺少可执行文件".to_string())?;
    let command_line = helper_arguments
        .iter()
        .map(|argument| quote_windows_argument(argument))
        .collect::<Vec<_>>()
        .join(" ");
    let mut command = windows_powershell_command();
    command
        .env("TRACEDISK_ELEVATED_EXE", executable)
        .env("TRACEDISK_ELEVATED_ARGUMENTS", command_line)
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            SCRIPT,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    Ok(command)
}

#[cfg(target_os = "windows")]
fn windows_powershell_command() -> Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut command = Command::new("powershell.exe");
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

#[cfg(any(target_os = "windows", test))]
fn quote_windows_argument(argument: &str) -> String {
    if !argument.is_empty()
        && !argument
            .chars()
            .any(|character| character.is_whitespace() || character == '"')
    {
        return argument.to_string();
    }

    let mut quoted = String::from('"');
    let mut backslashes = 0_usize;
    for character in argument.chars() {
        match character {
            '\\' => backslashes += 1,
            '"' => {
                quoted.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            _ => {
                quoted.extend(std::iter::repeat_n('\\', backslashes));
                quoted.push(character);
                backslashes = 0;
            }
        }
    }
    quoted.extend(std::iter::repeat_n('\\', backslashes * 2));
    quoted.push('"');
    quoted
}

#[cfg(target_os = "windows")]
fn windows_administrator_error(stderr: &[u8]) -> String {
    let message = String::from_utf8_lossy(stderr).trim().to_string();
    if message.contains("canceled by the user")
        || message.contains("cancelled by the user")
        || message.contains("操作已被用户取消")
    {
        "你取消了 Windows UAC 管理员授权，尚未读取 SD 卡".into()
    } else if message.is_empty() {
        "Windows 管理员只读辅助进程未能启动或执行失败".into()
    } else {
        format!("Windows 管理员只读操作失败: {message}")
    }
}

#[cfg(target_os = "macos")]
fn run_scan_as_administrator(
    arguments: &[&str],
    control: &ScanControlFiles,
    on_progress: &Channel<DeviceScanProgress>,
) -> Result<(), String> {
    const SCRIPT: &str = r#"
on run argv
    set commandText to ""
    repeat with argumentValue in argv
        if commandText is not "" then set commandText to commandText & " "
        set commandText to commandText & quoted form of (argumentValue as text)
    end repeat
    with timeout of 86400 seconds
        return do shell script commandText with administrator privileges
    end timeout
end run
"#;

    let mut child = Command::new("/usr/bin/osascript")
        .args(["-e", SCRIPT, "--"])
        .args(arguments)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("无法启动 macOS 管理员授权: {error}"))?;
    let mut last_progress = None;
    let status = loop {
        if let Ok(json) = std::fs::read_to_string(&control.progress) {
            if !json.is_empty() && last_progress.as_deref() != Some(json.as_str()) {
                if let Ok(progress) = serde_json::from_str::<DeviceScanProgress>(&json) {
                    let _ = on_progress.send(progress);
                    last_progress = Some(json);
                }
            }
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("无法等待扫描助手: {error}"))?
        {
            break status;
        }
        std::thread::sleep(PROGRESS_POLL_INTERVAL);
    };

    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        pipe.read_to_string(&mut stdout)
            .map_err(|error| format!("无法读取扫描助手输出: {error}"))?;
    }
    if let Some(mut pipe) = child.stderr.take() {
        pipe.read_to_string(&mut stderr)
            .map_err(|error| format!("无法读取扫描助手错误: {error}"))?;
    }
    if !status.success() {
        return Err(administrator_error(&stderr));
    }
    if stdout.trim() != "ok" {
        return Err("扫描助手没有返回完成状态".into());
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn run_scan_as_administrator(
    arguments: &[&str],
    control: &ScanControlFiles,
    on_progress: &Channel<DeviceScanProgress>,
) -> Result<(), String> {
    let mut child = windows_elevated_command(arguments)?
        .spawn()
        .map_err(|error| format!("无法启动 Windows UAC 管理员扫描辅助进程: {error}"))?;
    let mut last_progress = None;
    let status = loop {
        if let Ok(json) = std::fs::read_to_string(&control.progress) {
            if !json.is_empty() && last_progress.as_deref() != Some(json.as_str()) {
                if let Ok(progress) = serde_json::from_str::<DeviceScanProgress>(&json) {
                    let _ = on_progress.send(progress);
                    last_progress = Some(json);
                }
            }
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("无法等待 Windows 扫描辅助进程: {error}"))?
        {
            break status;
        }
        std::thread::sleep(PROGRESS_POLL_INTERVAL);
    };
    let mut stderr = Vec::new();
    if let Some(mut pipe) = child.stderr.take() {
        pipe.read_to_end(&mut stderr)
            .map_err(|error| format!("无法读取 Windows 扫描辅助进程错误: {error}"))?;
    }
    if !status.success() {
        return Err(windows_administrator_error(&stderr));
    }
    if !control.result.is_file()
        || std::fs::metadata(&control.result).map_or(0, |metadata| metadata.len()) == 0
    {
        return Err("Windows 扫描辅助进程没有返回扫描结果".into());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn run_batch_export_as_administrator(
    arguments: &[&str],
    control: &BatchExportControlFiles,
    on_progress: &Channel<BatchExportProgress>,
) -> Result<(), String> {
    const SCRIPT: &str = r#"
on run argv
    set commandText to ""
    repeat with argumentValue in argv
        if commandText is not "" then set commandText to commandText & " "
        set commandText to commandText & quoted form of (argumentValue as text)
    end repeat
    with timeout of 86400 seconds
        return do shell script commandText with administrator privileges
    end timeout
end run
"#;

    let mut child = Command::new("/usr/bin/osascript")
        .args(["-e", SCRIPT, "--"])
        .args(arguments)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("无法启动 macOS 批量导出授权: {error}"))?;
    let mut last_progress = None;
    let status = loop {
        if let Ok(json) = std::fs::read_to_string(&control.progress) {
            if !json.is_empty() && last_progress.as_deref() != Some(json.as_str()) {
                if let Ok(progress) = serde_json::from_str::<BatchExportProgress>(&json) {
                    let _ = on_progress.send(progress);
                    last_progress = Some(json);
                }
            }
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("无法等待批量导出助手: {error}"))?
        {
            break status;
        }
        std::thread::sleep(PROGRESS_POLL_INTERVAL);
    };

    if let Ok(json) = std::fs::read_to_string(&control.progress) {
        if !json.is_empty() && last_progress.as_deref() != Some(json.as_str()) {
            if let Ok(progress) = serde_json::from_str::<BatchExportProgress>(&json) {
                let _ = on_progress.send(progress);
            }
        }
    }
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        pipe.read_to_string(&mut stdout)
            .map_err(|error| format!("无法读取批量导出助手输出: {error}"))?;
    }
    if let Some(mut pipe) = child.stderr.take() {
        pipe.read_to_string(&mut stderr)
            .map_err(|error| format!("无法读取批量导出助手错误: {error}"))?;
    }
    if !status.success() {
        return Err(administrator_error(&stderr));
    }
    if stdout.trim() != "ok" {
        return Err("批量导出助手没有返回完成状态".into());
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn run_batch_export_as_administrator(
    arguments: &[&str],
    control: &BatchExportControlFiles,
    on_progress: &Channel<BatchExportProgress>,
) -> Result<(), String> {
    let mut child = windows_elevated_command(arguments)?
        .spawn()
        .map_err(|error| format!("无法启动 Windows UAC 管理员批量导出辅助进程: {error}"))?;
    let mut last_progress = None;
    let status = loop {
        if let Ok(json) = std::fs::read_to_string(&control.progress) {
            if !json.is_empty() && last_progress.as_deref() != Some(json.as_str()) {
                if let Ok(progress) = serde_json::from_str::<BatchExportProgress>(&json) {
                    let _ = on_progress.send(progress);
                    last_progress = Some(json);
                }
            }
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("无法等待 Windows 批量导出辅助进程: {error}"))?
        {
            break status;
        }
        std::thread::sleep(PROGRESS_POLL_INTERVAL);
    };
    if let Ok(json) = std::fs::read_to_string(&control.progress) {
        if !json.is_empty() && last_progress.as_deref() != Some(json.as_str()) {
            if let Ok(progress) = serde_json::from_str::<BatchExportProgress>(&json) {
                let _ = on_progress.send(progress);
            }
        }
    }
    let mut stderr = Vec::new();
    if let Some(mut pipe) = child.stderr.take() {
        pipe.read_to_end(&mut stderr)
            .map_err(|error| format!("无法读取 Windows 批量导出辅助进程错误: {error}"))?;
    }
    if !status.success() {
        return Err(windows_administrator_error(&stderr));
    }
    if !control.result.is_file()
        || std::fs::metadata(&control.result).map_or(0, |metadata| metadata.len()) == 0
    {
        return Err("Windows 批量导出辅助进程没有返回导出结果".into());
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn send_batch_export_progress(
    channel: &Channel<BatchExportProgress>,
    phase: &str,
    current_file: Option<String>,
    processed_files: usize,
    successful_files: usize,
    total_files: usize,
    bytes_processed: u64,
    total_bytes: u64,
) {
    let _ = channel.send(BatchExportProgress {
        phase: phase.into(),
        current_file,
        processed_files,
        successful_files,
        total_files,
        bytes_processed,
        total_bytes,
    });
}

fn send_scan_progress(
    channel: &Channel<DeviceScanProgress>,
    phase: &str,
    bytes_examined: u64,
    total_bytes: u64,
    candidates_found: usize,
) {
    let _ = channel.send(DeviceScanProgress {
        phase: phase.into(),
        bytes_examined,
        total_bytes,
        candidates_found,
    });
}

#[cfg(target_os = "macos")]
fn administrator_error(stderr: &str) -> String {
    let message = stderr.trim();
    if message.contains("(-128)") {
        "你取消了管理员授权，尚未读取 SD 卡".into()
    } else if message.is_empty() {
        "管理员只读扫描未能启动".into()
    } else {
        format!("管理员只读扫描失败: {message}")
    }
}

/// Handles the private, administrator-only read helper before Tauri starts.
/// Returns `None` for a normal GUI launch and an exit code for helper mode.
pub fn run_helper_from_args() -> Option<i32> {
    let mut arguments = std::env::args().skip(1);
    let flag = arguments.next()?;
    let result = match flag.as_str() {
        HELPER_SCAN_FLAG => run_scan_helper(&mut arguments),
        HELPER_RECOVER_FLAG => run_recovery_helper(&mut arguments),
        HELPER_BATCH_RECOVER_FLAG => run_batch_recovery_helper(&mut arguments),
        _ => return None,
    };

    match result {
        Ok(json) => {
            println!("{json}");
            Some(0)
        }
        Err(error) => {
            eprintln!("{error}");
            Some(1)
        }
    }
}

fn run_scan_helper(arguments: &mut impl Iterator<Item = String>) -> Result<String, String> {
    let mode = next_argument(arguments, "mode")?;
    let raw_device_path = next_argument(arguments, "raw device")?;
    let size_bytes = parse_u64_argument(arguments, "size")?;
    let control_directory = next_argument(arguments, "control directory")?;
    reject_extra_arguments(arguments)?;
    if mode != "metadata" && mode != "deep" {
        return Err("扫描助手收到无效 mode".into());
    }

    let control = validate_helper_control_directory(&control_directory)?;
    let validated = device::validate_external_raw_device(&raw_device_path, size_bytes)?;
    device::prepare_for_raw_read(&validated)?;
    let source = open_raw_device(&validated)?;
    write_scan_progress(
        &control.progress,
        &DeviceScanProgress {
            phase: "scanning".into(),
            bytes_examined: 0,
            total_bytes: validated.size_bytes,
            candidates_found: 0,
        },
    )?;
    let report = if mode == "metadata" {
        tracedisk_core::scan_deleted_videos(&source)
    } else {
        let mut last_update = Instant::now() - Duration::from_secs(1);
        let mut progress_error = None;
        let report = tracedisk_core::deep_scan_videos_with_progress(&source, |progress| {
            let cancelled = control.cancel.exists();
            if last_update.elapsed() >= PROGRESS_POLL_INTERVAL
                || progress.bytes_examined >= progress.total_bytes
                || cancelled
            {
                let update = DeviceScanProgress {
                    phase: if cancelled { "stopping" } else { "scanning" }.into(),
                    bytes_examined: progress.bytes_examined,
                    total_bytes: progress.total_bytes,
                    candidates_found: progress.candidates_found,
                };
                if let Err(error) = write_scan_progress(&control.progress, &update) {
                    progress_error = Some(error);
                    return false;
                }
                last_update = Instant::now();
            }
            !cancelled
        });
        if let Some(error) = progress_error {
            return Err(error);
        }
        report
    }
    .map_err(|error| format!("扫描 SD 卡失败: {error}"))?;
    let json =
        serde_json::to_string(&report).map_err(|error| format!("无法序列化扫描结果: {error}"))?;
    write_existing_file(&control.result, json.as_bytes(), "扫描结果")?;
    write_scan_progress(
        &control.progress,
        &DeviceScanProgress {
            phase: if report.cancelled {
                "cancelled"
            } else {
                "completed"
            }
            .into(),
            bytes_examined: report.bytes_examined,
            total_bytes: report.source_length,
            candidates_found: report.candidates.len(),
        },
    )?;
    Ok("ok".into())
}

struct HelperControlPaths {
    progress: PathBuf,
    result: PathBuf,
    cancel: PathBuf,
}

fn validate_helper_control_directory(directory: &str) -> Result<HelperControlPaths, String> {
    let directory = Path::new(directory);
    #[cfg(target_os = "macos")]
    if directory.parent() != Some(Path::new("/private/tmp")) {
        return Err("扫描控制目录不在允许的位置".into());
    }
    let valid_name = directory
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("tracedisk-scan-"));
    let metadata = std::fs::symlink_metadata(directory)
        .map_err(|error| format!("无法检查扫描控制目录: {error}"))?;
    if !valid_name || !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("扫描控制目录未通过安全检查".into());
    }
    let progress = directory.join("progress.json");
    let result = directory.join("result.json");
    if !progress.is_file() || !result.is_file() {
        return Err("扫描控制文件不存在".into());
    }
    Ok(HelperControlPaths {
        progress,
        result,
        cancel: directory.join("cancel"),
    })
}

fn write_scan_progress(path: &Path, progress: &DeviceScanProgress) -> Result<(), String> {
    let json =
        serde_json::to_vec(progress).map_err(|error| format!("无法序列化扫描进度: {error}"))?;
    write_existing_file(path, &json, "扫描进度")
}

fn write_existing_file(path: &Path, bytes: &[u8], label: &str) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .map_err(|error| format!("无法写入{label}: {error}"))?;
    file.write_all(bytes)
        .map_err(|error| format!("无法写入{label}: {error}"))?;
    Ok(())
}

struct BatchHelperControlPaths {
    plan: PathBuf,
    progress: PathBuf,
    result: PathBuf,
}

fn validate_batch_helper_control_directory(
    directory: &str,
) -> Result<BatchHelperControlPaths, String> {
    let directory = Path::new(directory);
    #[cfg(target_os = "macos")]
    if directory.parent() != Some(Path::new("/private/tmp")) {
        return Err("批量导出控制目录不在允许的位置".into());
    }
    let valid_name = directory
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("tracedisk-export-"));
    let metadata = std::fs::symlink_metadata(directory)
        .map_err(|error| format!("无法检查批量导出控制目录: {error}"))?;
    if !valid_name || !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("批量导出控制目录未通过安全检查".into());
    }
    let plan = directory.join("plan.json");
    let progress = directory.join("progress.json");
    let result = directory.join("result.json");
    for path in [&plan, &progress, &result] {
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|error| format!("无法检查批量导出控制文件: {error}"))?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err("批量导出控制文件未通过安全检查".into());
        }
    }
    Ok(BatchHelperControlPaths {
        plan,
        progress,
        result,
    })
}

fn run_batch_recovery_helper(
    arguments: &mut impl Iterator<Item = String>,
) -> Result<String, String> {
    let raw_device_path = next_argument(arguments, "raw device")?;
    let source_size_bytes = parse_u64_argument(arguments, "source size")?;
    let control_directory = next_argument(arguments, "batch control directory")?;
    let user_id = parse_u64_argument(arguments, "user id")?;
    let group_id = parse_u64_argument(arguments, "group id")?;
    reject_extra_arguments(arguments)?;

    let control = validate_batch_helper_control_directory(&control_directory)?;
    let plan_bytes =
        std::fs::read(&control.plan).map_err(|error| format!("无法读取批量导出计划: {error}"))?;
    let plan = serde_json::from_slice::<BatchRecoveryPlan>(&plan_bytes)
        .map_err(|error| format!("批量导出计划格式无效: {error}"))?;
    validate_batch_recovery_plan(&plan, source_size_bytes)?;
    ensure_destination_capacity(&plan.output_directory, plan.total_bytes)?;
    let validated = device::validate_external_raw_device(&raw_device_path, source_size_bytes)?;
    device::prepare_for_raw_read(&validated)?;
    let source = open_raw_device(&validated)?;

    let mut progress = BatchExportProgress {
        phase: "exporting".into(),
        current_file: None,
        processed_files: 0,
        successful_files: 0,
        total_files: plan.items.len(),
        bytes_processed: 0,
        total_bytes: plan.total_bytes,
    };
    write_batch_export_progress(&control.progress, &progress)?;
    let mut result = BatchExportResult {
        output_directory: plan.output_directory.clone(),
        successful_files: Vec::new(),
        failures: Vec::new(),
        warnings: Vec::new(),
        bytes_written: 0,
    };

    for item in &plan.items {
        progress.current_file = Some(item.output_name.clone());
        write_batch_export_progress(&control.progress, &progress)?;
        let base_bytes = progress.bytes_processed;
        let mut last_update = Instant::now() - PROGRESS_POLL_INTERVAL;
        let recovery = recover_batch_item(&source, item, |file_bytes| {
            if last_update.elapsed() >= PROGRESS_POLL_INTERVAL || file_bytes >= item.size_bytes {
                progress.bytes_processed = base_bytes.saturating_add(file_bytes);
                write_batch_export_progress(&control.progress, &progress)?;
                last_update = Instant::now();
            }
            Ok(())
        });

        progress.processed_files += 1;
        progress.bytes_processed = base_bytes.saturating_add(item.size_bytes);
        match recovery {
            Ok(()) => {
                progress.successful_files += 1;
                result.bytes_written = result.bytes_written.saturating_add(item.size_bytes);
                result.successful_files.push(item.output_name.clone());
                let owner_changed = adjust_output_owner(&item.output_path, user_id, group_id);
                if !owner_changed {
                    result.warnings.push(format!(
                        "{} 已导出，但未能自动调整所有者信息",
                        item.output_name
                    ));
                }
            }
            Err(error) => result.failures.push(BatchExportFailure {
                name: item.output_name.clone(),
                error,
            }),
        }
        write_batch_export_progress(&control.progress, &progress)?;
    }

    progress.phase = if result.failures.is_empty() {
        "completed"
    } else {
        "completed-with-errors"
    }
    .into();
    progress.current_file = None;
    write_batch_export_progress(&control.progress, &progress)?;
    let json =
        serde_json::to_vec(&result).map_err(|error| format!("无法序列化批量导出结果: {error}"))?;
    write_existing_file(&control.result, &json, "批量导出结果")?;
    Ok("ok".into())
}

fn recover_batch_item<F>(
    source: &dyn BlockSource,
    item: &PreparedBatchRecoveryItem,
    on_progress: F,
) -> Result<(), String>
where
    F: FnMut(u64) -> Result<(), String>,
{
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&item.output_path)
        .map_err(|error| format!("无法创建恢复文件（不会覆盖已有文件）: {error}"))?;
    let recovery = (|| {
        copy_recovery_extents_with_progress(source, &mut output, &item.extents, on_progress)?;
        output
            .sync_all()
            .map_err(|error| format!("同步恢复文件失败: {error}"))
    })();
    drop(output);
    if recovery.is_err() {
        let _ = std::fs::remove_file(&item.output_path);
    }
    recovery
}

fn write_batch_export_progress(path: &Path, progress: &BatchExportProgress) -> Result<(), String> {
    let json =
        serde_json::to_vec(progress).map_err(|error| format!("无法序列化批量导出进度: {error}"))?;
    write_existing_file(path, &json, "批量导出进度")
}

fn run_recovery_helper(arguments: &mut impl Iterator<Item = String>) -> Result<String, String> {
    let raw_device_path = next_argument(arguments, "raw device")?;
    let source_size_bytes = parse_u64_argument(arguments, "source size")?;
    let plan_path = next_argument(arguments, "recovery plan")?;
    let output_path = next_argument(arguments, "output path")?;
    let user_id = parse_u64_argument(arguments, "user id")?;
    let group_id = parse_u64_argument(arguments, "group id")?;
    reject_extra_arguments(arguments)?;
    let plan = read_recovery_plan(&plan_path, source_size_bytes)?;
    validate_output_path(&output_path)?;

    let validated = device::validate_external_raw_device(&raw_device_path, source_size_bytes)?;
    device::prepare_for_raw_read(&validated)?;
    let source = open_raw_device(&validated)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&output_path)
        .map_err(|error| format!("无法创建恢复文件（不会覆盖已有文件）: {error}"))?;

    let copy_result = (|| {
        copy_recovery_extents(&source, &mut output, &plan.extents)?;
        output
            .sync_all()
            .map_err(|error| format!("同步恢复文件失败: {error}"))
    })();
    drop(output);
    if let Err(error) = copy_result {
        let _ = std::fs::remove_file(&output_path);
        return Err(error);
    }

    let owner_changed = adjust_output_owner(&output_path, user_id, group_id);
    Ok(if owner_changed {
        "ok".into()
    } else {
        "ok-owner-warning".into()
    })
}

fn read_recovery_plan(path: &str, source_length: u64) -> Result<RecoveryPlan, String> {
    let path = Path::new(path);
    let directory = path
        .parent()
        .ok_or_else(|| "恢复片段计划缺少控制目录".to_string())?;
    #[cfg(target_os = "macos")]
    if directory.parent() != Some(Path::new("/private/tmp")) {
        return Err("恢复控制目录不在允许的位置".into());
    }
    let valid_directory = directory
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("tracedisk-recovery-"));
    let directory_metadata = std::fs::symlink_metadata(directory)
        .map_err(|error| format!("无法检查恢复控制目录: {error}"))?;
    let plan_metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("无法检查恢复片段计划: {error}"))?;
    if !valid_directory
        || path.file_name().and_then(|name| name.to_str()) != Some("plan.json")
        || !directory_metadata.is_dir()
        || directory_metadata.file_type().is_symlink()
        || !plan_metadata.is_file()
        || plan_metadata.file_type().is_symlink()
    {
        return Err("恢复片段计划未通过安全检查".into());
    }
    let bytes = std::fs::read(path).map_err(|error| format!("无法读取恢复片段计划: {error}"))?;
    let plan = serde_json::from_slice::<RecoveryPlan>(&bytes)
        .map_err(|error| format!("恢复片段计划格式无效: {error}"))?;
    validate_recovery_plan(&plan.extents, plan.size_bytes, source_length)?;
    Ok(plan)
}

fn copy_recovery_extents(
    source: &dyn BlockSource,
    output: &mut impl Write,
    extents: &[RecoveryExtent],
) -> Result<(), String> {
    copy_recovery_extents_with_progress(source, output, extents, |_| Ok(()))
}

fn copy_recovery_extents_with_progress<F>(
    source: &dyn BlockSource,
    output: &mut impl Write,
    extents: &[RecoveryExtent],
    mut on_progress: F,
) -> Result<(), String>
where
    F: FnMut(u64) -> Result<(), String>,
{
    let mut buffer = vec![0_u8; RECOVERY_BATCH_SIZE];
    let mut total_written = 0_u64;
    for extent in extents {
        let mut cursor = 0_u64;
        while cursor < extent.length {
            let length = (extent.length - cursor).min(buffer.len() as u64) as usize;
            source
                .read_exact_at(extent.byte_offset + cursor, &mut buffer[..length])
                .map_err(|error| format!("读取恢复数据失败: {error}"))?;
            output
                .write_all(&buffer[..length])
                .map_err(|error| format!("写入恢复文件失败: {error}"))?;
            cursor += length as u64;
            total_written = total_written.saturating_add(length as u64);
            on_progress(total_written)?;
        }
    }
    Ok(())
}

fn open_raw_device(
    validated: &device::ValidatedRawDevice,
) -> Result<tracedisk_core::RawDeviceSource, String> {
    tracedisk_core::RawDeviceSource::open(
        &validated.raw_device_path,
        validated.size_bytes,
        validated.block_size,
    )
    .map_err(raw_device_open_error)
}

#[cfg(target_os = "macos")]
fn raw_device_open_error(error: tracedisk_core::TraceError) -> String {
    match &error {
        tracedisk_core::TraceError::Io(io_error)
            if matches!(io_error.raw_os_error(), Some(1 | 13)) =>
        {
            "macOS 已阻止对原始 SD 卡的读取。请在“系统设置 → 隐私与安全性 → 完全磁盘访问权限”中添加并启用 TraceDisk.app，然后用 ⌘Q 完全退出并重新打开应用。管理员密码本身不足以授予这项隐私权限。"
                .into()
        }
        _ => format!("无法以只读方式打开 SD 卡: {error}"),
    }
}

#[cfg(target_os = "windows")]
fn raw_device_open_error(error: tracedisk_core::TraceError) -> String {
    match &error {
        tracedisk_core::TraceError::Io(io_error)
            if matches!(io_error.raw_os_error(), Some(5)) =>
        {
            "Windows 拒绝读取物理 SD 卡。请确认已允许 UAC 管理员授权，并关闭资源管理器或其他正在占用该盘符的程序。"
                .into()
        }
        _ => format!("无法以只读方式打开 Windows 物理 SD 卡: {error}"),
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn raw_device_open_error(error: tracedisk_core::TraceError) -> String {
    format!("无法以只读方式打开 SD 卡: {error}")
}

fn next_argument(
    arguments: &mut impl Iterator<Item = String>,
    name: &str,
) -> Result<String, String> {
    arguments
        .next()
        .ok_or_else(|| format!("扫描助手缺少 {name} 参数"))
}

fn parse_u64_argument(
    arguments: &mut impl Iterator<Item = String>,
    name: &str,
) -> Result<u64, String> {
    next_argument(arguments, name)?
        .parse::<u64>()
        .map_err(|_| format!("扫描助手收到无效 {name} 参数"))
}

fn reject_extra_arguments(arguments: &mut impl Iterator<Item = String>) -> Result<(), String> {
    if arguments.next().is_some() {
        Err("扫描助手收到多余参数".into())
    } else {
        Ok(())
    }
}

fn validate_recovery_range(offset: u64, length: u64, source_length: u64) -> Result<(), String> {
    if length == 0
        || offset
            .checked_add(length)
            .is_none_or(|end| end > source_length)
    {
        return Err("恢复范围超出 SD 卡边界，已拒绝导出".into());
    }
    Ok(())
}

fn validate_recovery_plan(
    extents: &[RecoveryExtent],
    expected_size: u64,
    source_length: u64,
) -> Result<(), String> {
    if extents.is_empty() || extents.len() > MAX_RECOVERY_EXTENTS || expected_size == 0 {
        return Err("恢复片段计划为空或片段数量异常，已拒绝导出".into());
    }
    let mut total = 0_u64;
    for extent in extents {
        validate_recovery_range(extent.byte_offset, extent.length, source_length)?;
        total = total
            .checked_add(extent.length)
            .ok_or_else(|| "恢复片段总长度溢出，已拒绝导出".to_string())?;
    }
    if total != expected_size {
        return Err(format!(
            "恢复片段总长度与文件大小不一致（片段 {total} 字节，文件 {expected_size} 字节）"
        ));
    }
    Ok(())
}

fn validate_output_path(output_path: &str) -> Result<(), String> {
    let path = Path::new(output_path);
    if !path.is_absolute() || path.file_name().is_none() {
        return Err("恢复目标必须是有效的绝对文件路径".into());
    }
    if path.exists() {
        return Err("目标文件已经存在；为防止覆盖，请选择其他名称".into());
    }
    let parent = path
        .parent()
        .ok_or_else(|| "恢复目标缺少父目录".to_string())?;
    if !parent.is_dir() {
        return Err("恢复目标的父目录不存在".into());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn command_number(command: &str, argument: &str) -> Result<String, String> {
    let output = Command::new(command)
        .arg(argument)
        .output()
        .map_err(|error| format!("无法查询当前用户标识: {error}"))?;
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() || value.parse::<u64>().is_err() {
        return Err("无法读取当前用户标识".into());
    }
    Ok(value)
}

#[cfg(target_os = "macos")]
fn current_owner_arguments() -> Result<(String, String), String> {
    Ok((
        command_number("/usr/bin/id", "-u")?,
        command_number("/usr/bin/id", "-g")?,
    ))
}

#[cfg(target_os = "windows")]
fn current_owner_arguments() -> Result<(String, String), String> {
    Ok(("0".into(), "0".into()))
}

#[cfg(target_os = "macos")]
fn adjust_output_owner(output_path: &str, user_id: u64, group_id: u64) -> bool {
    let owner = format!("{user_id}:{group_id}");
    Command::new("/usr/sbin/chown")
        .args([&owner, output_path])
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(target_os = "windows")]
fn adjust_output_owner(_output_path: &str, _user_id: u64, _group_id: u64) -> bool {
    true
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            inspect_image,
            scan_raw_device,
            cancel_active_scan,
            recover_candidate,
            recover_candidates_batch,
            check_export_destination,
            open_full_disk_access_settings,
            device::resolve_sd_card_path
        ])
        .run(tauri::generate_context!())
        .expect("error while running TraceDisk");
}

#[cfg(test)]
mod tests {
    use super::{
        available_space_bytes, copy_recovery_extents, is_windows_safe_output_name,
        prepare_batch_recovery_plan, quote_windows_argument, read_recovery_plan,
        validate_batch_helper_control_directory, validate_batch_recovery_plan,
        validate_helper_control_directory, validate_output_name, validate_output_path,
        validate_recovery_plan, validate_recovery_range, BatchExportControlFiles,
        BatchRecoveryItem, RecoveryControlFile, RecoveryPlan, ScanControlFiles,
    };
    use std::time::{SystemTime, UNIX_EPOCH};
    use tracedisk_core::{BlockSource, RecoveryExtent, TraceError};

    struct MemorySource(Vec<u8>);

    impl BlockSource for MemorySource {
        fn len(&self) -> u64 {
            self.0.len() as u64
        }

        fn read_exact_at(&self, offset: u64, buffer: &mut [u8]) -> tracedisk_core::Result<()> {
            let start = offset as usize;
            let end = start + buffer.len();
            let bytes = self.0.get(start..end).ok_or(TraceError::OutOfBounds {
                offset,
                length: buffer.len(),
                source_len: self.0.len() as u64,
            })?;
            buffer.copy_from_slice(bytes);
            Ok(())
        }
    }

    #[test]
    fn accepts_only_bounded_nonempty_recovery_ranges() {
        assert!(validate_recovery_range(100, 200, 1000).is_ok());
        assert!(validate_recovery_range(100, 0, 1000).is_err());
        assert!(validate_recovery_range(900, 101, 1000).is_err());
        assert!(validate_recovery_range(u64::MAX, 2, u64::MAX).is_err());
    }

    #[test]
    fn quotes_windows_helper_arguments_without_command_injection() {
        assert_eq!(quote_windows_argument("plain"), "plain");
        assert_eq!(quote_windows_argument(""), "\"\"");
        assert_eq!(quote_windows_argument("two words"), "\"two words\"");
        assert_eq!(
            quote_windows_argument(r#"C:\Program Files\TraceDisk\"#),
            r#""C:\Program Files\TraceDisk\\""#
        );
        assert_eq!(quote_windows_argument(r#"a"b"#), r#""a\"b""#);
    }

    #[test]
    fn rejects_windows_reserved_or_unsafe_output_names() {
        assert!(is_windows_safe_output_name("DJI_0001.MP4"));
        assert!(!is_windows_safe_output_name("CON.mp4"));
        assert!(!is_windows_safe_output_name("LPT1.mov"));
        assert!(!is_windows_safe_output_name("bad:name.mp4"));
        assert!(!is_windows_safe_output_name("trailing.mp4."));
    }

    #[test]
    fn validates_and_copies_multiple_recovery_extents_in_file_order() {
        let extents = vec![
            RecoveryExtent {
                byte_offset: 1,
                length: 3,
            },
            RecoveryExtent {
                byte_offset: 7,
                length: 2,
            },
        ];
        assert!(validate_recovery_plan(&extents, 5, 10).is_ok());
        assert!(validate_recovery_plan(&extents, 6, 10).is_err());
        assert!(validate_recovery_plan(&[], 0, 10).is_err());

        let source = MemorySource(b"0123456789".to_vec());
        let mut output = Vec::new();
        copy_recovery_extents(&source, &mut output, &extents).unwrap();
        assert_eq!(output, b"12378");
    }

    #[test]
    fn rejects_unsafe_output_paths_before_privilege_escalation() {
        assert!(validate_output_path("relative/video.mp4").is_err());
        assert!(validate_output_path("/path/that/does/not/exist/video.mp4").is_err());
        assert!(validate_output_path("/").is_err());
    }

    #[test]
    fn creates_and_validates_private_scan_control_files() {
        let control = ScanControlFiles::create().unwrap();
        let paths = validate_helper_control_directory(control.directory.to_str().unwrap()).unwrap();
        assert_eq!(paths.progress, control.progress);
        assert_eq!(paths.result, control.result);
        assert_eq!(paths.cancel, control.cancel);
        assert!(validate_helper_control_directory("/private/tmp/not-tracedisk").is_err());
    }

    #[test]
    fn creates_and_validates_a_private_recovery_plan() {
        let plan = RecoveryPlan {
            size_bytes: 5,
            extents: vec![RecoveryExtent {
                byte_offset: 10,
                length: 5,
            }],
        };
        let control = RecoveryControlFile::create(&plan).unwrap();
        let parsed = read_recovery_plan(control.plan.to_str().unwrap(), 100).unwrap();
        assert_eq!(parsed.size_bytes, 5);
        assert_eq!(parsed.extents, plan.extents);
        assert!(read_recovery_plan("/private/tmp/not-tracedisk/plan.json", 100).is_err());
    }

    #[test]
    fn prepares_unique_batch_names_and_validates_total_size() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "tracedisk-batch-test-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("DJI_0001.MP4"), b"existing").unwrap();
        let extent = RecoveryExtent {
            byte_offset: 10,
            length: 5,
        };
        let items = vec![
            BatchRecoveryItem {
                name: "DJI_0001.MP4".into(),
                size_bytes: 5,
                extents: vec![extent.clone()],
            },
            BatchRecoveryItem {
                name: "DJI_0001.MP4".into(),
                size_bytes: 5,
                extents: vec![extent],
            },
        ];
        let plan = prepare_batch_recovery_plan(directory.to_str().unwrap(), items, 100).unwrap();
        assert_eq!(plan.total_bytes, 10);
        assert_eq!(plan.items[0].output_name, "DJI_0001 (2).MP4");
        assert_eq!(plan.items[1].output_name, "DJI_0001 (3).MP4");
        assert!(validate_batch_recovery_plan(&plan, 100).is_ok());
        assert!(available_space_bytes(&directory).unwrap() > 0);
        assert!(validate_output_name("../unsafe.mp4").is_err());

        std::fs::remove_file(directory.join("DJI_0001.MP4")).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn creates_and_validates_private_batch_control_files() {
        let output_directory = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let item_path = output_directory.join("tracedisk-control-test.mp4");
        let plan = super::BatchRecoveryPlan {
            output_directory: output_directory.to_string_lossy().into_owned(),
            total_bytes: 5,
            items: vec![super::PreparedBatchRecoveryItem {
                output_path: item_path.to_string_lossy().into_owned(),
                output_name: "tracedisk-control-test.mp4".into(),
                size_bytes: 5,
                extents: vec![RecoveryExtent {
                    byte_offset: 10,
                    length: 5,
                }],
            }],
        };
        let control = BatchExportControlFiles::create(&plan).unwrap();
        let paths =
            validate_batch_helper_control_directory(control.directory.to_str().unwrap()).unwrap();
        assert_eq!(paths.plan, control.plan);
        assert_eq!(paths.progress, control.progress);
        assert_eq!(paths.result, control.result);
        assert!(validate_batch_helper_control_directory("/private/tmp/not-tracedisk").is_err());
    }
}
