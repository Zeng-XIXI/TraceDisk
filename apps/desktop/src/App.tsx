import { FormEvent, useMemo, useState } from "react";
import { Channel, invoke } from "@tauri-apps/api/core";
import { confirm, open, save } from "@tauri-apps/plugin-dialog";

type ScanState = "idle" | "selecting" | "scanning" | "success" | "error";
type DeviceScanMode = "metadata" | "deep";

const isWindows = navigator.userAgent.includes("Windows");
const sdPathExample = isWindows ? "E:\\" : "/Volumes/SD_Card";

type Partition = {
  index: number;
  start_lba: number;
  sector_count: number;
  type_id: string;
  name: string | null;
};

type Volume = {
  partition_index: number;
  byte_offset: number;
  filesystem: string;
  details: Record<string, number>;
};

type InspectionReport = {
  source_path: string;
  source_length: number;
  partition_scheme: string;
  logical_sector_size: number;
  partitions: Partition[];
  volumes: Volume[];
};

type SdCardInfo = {
  requested_path: string;
  mount_point: string;
  volume_name: string;
  media_name: string;
  partition_identifier: string;
  whole_disk_identifier: string;
  device_node: string;
  raw_device_path: string;
  size_bytes: number;
  removable: boolean;
  ejectable: boolean;
  raw_readable: boolean;
};

type RecoveryExtent = {
  byte_offset: number;
  length: number;
};

type VideoCandidate = {
  id: number;
  name: string;
  original_path: string | null;
  extension: string;
  byte_offset: number;
  size_bytes: number;
  start_cluster: number | null;
  contiguous: boolean;
  extents: RecoveryExtent[];
  fat_chain_status: "not-required" | "complete" | "broken" | "not-applicable";
  free_cluster_ratio: number;
  recoverability: string;
  source: string;
  has_mdat: boolean;
  has_moov: boolean;
};

type DeviceScanReport = {
  mode: DeviceScanMode;
  filesystem: string;
  source_length: number;
  bytes_examined: number;
  candidates: VideoCandidate[];
  warnings: string[];
  cancelled: boolean;
};

type DeviceScanProgress = {
  phase: "preparing" | "scanning" | "stopping" | "cancelled" | "completed";
  bytesExamined: number;
  totalBytes: number;
  candidatesFound: number;
};

type DestinationCapacity = {
  path: string;
  availableBytes: number;
};

type BatchExportProgress = {
  phase: "preparing" | "exporting" | "completed" | "completed-with-errors";
  currentFile: string | null;
  processedFiles: number;
  successfulFiles: number;
  totalFiles: number;
  bytesProcessed: number;
  totalBytes: number;
};

type BatchExportFailure = {
  name: string;
  error: string;
};

type BatchExportResult = {
  outputDirectory: string;
  successfulFiles: string[];
  failures: BatchExportFailure[];
  warnings: string[];
  bytesWritten: number;
};

function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return "—";
  if (bytes === 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  const index = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1);
  const value = bytes / 1024 ** index;
  return `${value >= 100 || index === 0 ? value.toFixed(0) : value.toFixed(1)} ${units[index]}`;
}

function fileName(path: string): string {
  return path.split(/[\\/]/).filter(Boolean).at(-1) ?? path;
}

function filesystemLabel(name: string): string {
  return name === "unknown" ? "未识别" : name;
}

function recoveryLabel(value: string): string {
  const labels: Record<string, string> = {
    high: "恢复希望高",
    "partially-overwritten": "可能部分覆盖",
    "overwritten-or-reallocated": "可能已覆盖",
    "needs-deep-scan": "需要深度扫描",
    "container-complete": "容器结构完整",
    "container-repair-needed": "可能需要修复容器",
  };
  return labels[value] ?? value;
}

function canRecover(candidate: VideoCandidate): boolean {
  return candidate.extents.length > 0;
}

function candidateKey(candidate: VideoCandidate): string {
  return `${candidate.source}:${candidate.byte_offset}`;
}

type AccessGateProps = {
  onVerified: () => void;
};

function AccessGate({ onVerified }: AccessGateProps) {
  const [code, setCode] = useState("");
  const [isVerifying, setIsVerifying] = useState(false);
  const [validationError, setValidationError] = useState<string | null>(null);

  async function submitAccessCode(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!code) {
      setValidationError("请输入访问验证码");
      return;
    }

    setIsVerifying(true);
    setValidationError(null);
    try {
      const verified = await invoke<boolean>("verify_access_code", { code });
      if (verified) {
        onVerified();
      } else {
        setValidationError("验证码错误，应用即将关闭");
      }
    } catch (caught) {
      setValidationError(caught instanceof Error ? caught.message : String(caught));
    } finally {
      setIsVerifying(false);
    }
  }

  return (
    <div className="access-shell">
      <div className="access-ambient access-ambient-left" />
      <div className="access-ambient access-ambient-right" />
      <main className="access-main">
        <section className="access-card" aria-labelledby="access-title">
          <div className="access-brand">
            <div className="brand-mark" aria-hidden="true"><span /><span /></div>
            <div><strong>TraceDisk</strong><small>SECURE RECOVERY WORKSPACE</small></div>
          </div>
          <div className="access-lock" aria-hidden="true">
            <span className="access-lock-shackle" />
            <span className="access-lock-body"><i /></span>
          </div>
          <p className="eyebrow">ACCESS VERIFICATION</p>
          <h1 id="access-title">输入验证码后继续</h1>
          <p className="access-description">
            为保护磁盘扫描和恢复功能，本次启动必须先完成验证。验证成功仅对当前应用进程有效。
          </p>
          <form className="access-form" onSubmit={submitAccessCode}>
            <label htmlFor="access-code">访问验证码</label>
            <div className={`access-input-wrap ${validationError ? "access-input-error" : ""}`}>
              <span aria-hidden="true">••</span>
              <input
                id="access-code"
                type="password"
                inputMode="numeric"
                autoComplete="one-time-code"
                autoFocus
                maxLength={32}
                value={code}
                disabled={isVerifying}
                onChange={(event) => setCode(event.target.value)}
                placeholder="请输入验证码"
                aria-describedby="access-help"
              />
            </div>
            {validationError && <p className="access-error" role="alert">{validationError}</p>}
            <button className="access-submit" type="submit" disabled={isVerifying || !code}>
              {isVerifying ? <><span className="spinner" />正在验证</> : "验证并进入"}
            </button>
          </form>
          <div className="access-security-note" id="access-help">
            <span className="status-dot" />
            <div><strong>当前进程验证</strong><small>验证码错误时，TraceDisk 将自动关闭且不会读取任何磁盘。</small></div>
          </div>
        </section>
      </main>
      <footer className="access-footer"><span>TraceDisk 0.1.0</span><span>Protected read-only recovery toolkit</span></footer>
    </div>
  );
}

function TraceDiskWorkspace() {
  const [scanState, setScanState] = useState<ScanState>("idle");
  const [activeMode, setActiveMode] = useState<DeviceScanMode | null>(null);
  const [report, setReport] = useState<InspectionReport | null>(null);
  const [deviceReport, setDeviceReport] = useState<DeviceScanReport | null>(null);
  const [sdCardInfo, setSdCardInfo] = useState<SdCardInfo | null>(null);
  const [sdPathInput, setSdPathInput] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [recoveringId, setRecoveringId] = useState<number | null>(null);
  const [recoveryMessage, setRecoveryMessage] = useState<string | null>(null);
  const [scanProgress, setScanProgress] = useState<DeviceScanProgress | null>(null);
  const [isStopping, setIsStopping] = useState(false);
  const [selectedCandidateKeys, setSelectedCandidateKeys] = useState<Set<string>>(() => new Set());
  const [batchDestination, setBatchDestination] = useState<DestinationCapacity | null>(null);
  const [batchProgress, setBatchProgress] = useState<BatchExportProgress | null>(null);
  const [isBatchExporting, setIsBatchExporting] = useState(false);
  const busy = scanState === "selecting" || scanState === "scanning" || recoveringId !== null || isBatchExporting;

  const recognizedVolumes = useMemo(
    () => report?.volumes.filter((volume) => volume.filesystem !== "unknown").length ?? 0,
    [report],
  );

  const recoverableCandidates = useMemo(
    () => deviceReport?.candidates.filter(canRecover) ?? [],
    [deviceReport],
  );

  const selectedCandidates = useMemo(
    () => recoverableCandidates.filter((candidate) => selectedCandidateKeys.has(candidateKey(candidate))),
    [recoverableCandidates, selectedCandidateKeys],
  );

  const selectedBytes = useMemo(
    () => selectedCandidates.reduce((total, candidate) => total + candidate.size_bytes, 0),
    [selectedCandidates],
  );

  const batchHasEnoughSpace = batchDestination !== null && batchDestination.availableBytes >= selectedBytes;

  const statusText = useMemo(() => {
    if (isBatchExporting) return "正在批量导出恢复文件";
    if (recoveringId !== null) return "正在导出恢复文件";
    if (isStopping) return "正在安全停止深度扫描";
    if (scanState === "selecting") return "正在等待选择";
    if (scanState === "scanning" && activeMode === "metadata") return "正在快速扫描元数据";
    if (scanState === "scanning" && activeMode === "deep") return "正在逐块深度扫描整张卡";
    if (scanState === "scanning") return "正在识别磁盘";
    if (scanState === "success") return "操作已完成";
    if (scanState === "error") return "操作未完成";
    return "等待选择 SD 卡";
  }, [activeMode, isBatchExporting, isStopping, recoveringId, scanState]);

  const deepProgressPercent = useMemo(() => {
    if (!scanProgress || scanProgress.totalBytes <= 0) return 0;
    return Math.min(100, (scanProgress.bytesExamined / scanProgress.totalBytes) * 100);
  }, [scanProgress]);

  const batchProgressPercent = useMemo(() => {
    if (!batchProgress || batchProgress.totalBytes <= 0) return 0;
    return Math.min(100, (batchProgress.bytesProcessed / batchProgress.totalBytes) * 100);
  }, [batchProgress]);

  async function resolveSdCardPath(path: string) {
    const normalizedPath = path.trim();
    if (!normalizedPath) {
      setScanState("error");
      setError(`请输入 SD 卡路径，例如 ${sdPathExample}`);
      return;
    }

    setError(null);
    setRecoveryMessage(null);
    setActiveMode(null);
    setSelectedCandidateKeys(new Set());
    setBatchDestination(null);
    setBatchProgress(null);
    setScanState("scanning");
    try {
      const info = await invoke<SdCardInfo>("resolve_sd_card_path", { path: normalizedPath });
      setSdCardInfo(info);
      setDeviceReport(null);
      setReport(null);
      setSdPathInput(info.mount_point);
      setScanState("success");
    } catch (caught) {
      setScanState("error");
      setError(caught instanceof Error ? caught.message : String(caught));
    }
  }

  async function chooseSdCard() {
    setError(null);
    setScanState("selecting");
    try {
      const selected = await open({
        multiple: false,
        directory: true,
        title: "选择已挂载的 SD 卡",
      });
      if (!selected) {
        setScanState(report || sdCardInfo ? "success" : "idle");
        return;
      }
      setSdPathInput(selected);
      await resolveSdCardPath(selected);
    } catch (caught) {
      setScanState("error");
      setError(caught instanceof Error ? caught.message : String(caught));
    }
  }

  async function chooseImageAndInspect() {
    setError(null);
    setScanState("selecting");
    try {
      const selected = await open({
        multiple: false,
        directory: false,
        title: "选择已有 SD 卡镜像（可选）",
        filters: [{ name: "磁盘镜像", extensions: ["img", "raw", "dd", "bin"] }],
      });
      if (!selected) {
        setScanState(report || sdCardInfo ? "success" : "idle");
        return;
      }

      setActiveMode(null);
      setScanState("scanning");
      const rawReport = await invoke<string>("inspect_image", { path: selected });
      setReport(JSON.parse(rawReport) as InspectionReport);
      setSdCardInfo(null);
      setDeviceReport(null);
      setSelectedCandidateKeys(new Set());
      setBatchDestination(null);
      setBatchProgress(null);
      setScanState("success");
    } catch (caught) {
      setScanState("error");
      setError(caught instanceof Error ? caught.message : String(caught));
    }
  }

  async function startDeviceScan(mode: DeviceScanMode) {
    if (!sdCardInfo) return;
    if (mode === "deep" && deviceReport?.mode !== "metadata" && deviceReport?.mode !== "deep") {
      setError("请先完成快速扫描，再决定是否进行整卡深度扫描");
      setScanState("error");
      return;
    }

    const approved = await confirm(
      mode === "metadata"
        ? `TraceDisk 将卸载这张 SD 卡，只读取文件系统元数据和已删除目录项，不会扫描整张卡，也不会创建镜像。随后 ${isWindows ? "Windows UAC" : "macOS"} 会请求管理员授权。`
        : `TraceDisk 将直接逐块读取 ${formatBytes(sdCardInfo.size_bytes)} 的整张 SD 卡。这不会创建同等大小的镜像，但可能需要较长时间。请保持读卡器连接。`,
      {
        title: mode === "metadata" ? "开始快速只读扫描？" : "继续整卡深度扫描？",
        kind: "warning",
        okLabel: mode === "metadata" ? "开始快速扫描" : "开始整卡扫描",
        cancelLabel: "取消",
      },
    );
    if (!approved) return;

    setError(null);
    setActiveMode(mode);
    setIsStopping(false);
    setScanProgress({
      phase: "preparing",
      bytesExamined: 0,
      totalBytes: sdCardInfo.size_bytes,
      candidatesFound: 0,
    });
    setScanState("scanning");
    try {
      const onProgress = new Channel<DeviceScanProgress>();
      onProgress.onmessage = (progress) => setScanProgress(progress);
      const rawReport = await invoke<string>("scan_raw_device", {
        rawDevicePath: sdCardInfo.raw_device_path,
        sizeBytes: sdCardInfo.size_bytes,
        mode,
        onProgress,
      });
      const parsedReport = JSON.parse(rawReport) as DeviceScanReport;
      if (mode === "deep" && deviceReport?.mode === "metadata") {
        const quickCandidates = new Map(
          deviceReport.candidates.map((candidate) => [candidate.byte_offset, candidate]),
        );
        parsedReport.candidates = parsedReport.candidates.map((candidate) => {
          const quickCandidate = quickCandidates.get(candidate.byte_offset);
          return quickCandidate
            ? { ...candidate, name: quickCandidate.name, original_path: quickCandidate.original_path }
            : candidate;
        });
      }
      setDeviceReport(parsedReport);
      setSelectedCandidateKeys(new Set());
      setBatchProgress(null);
      setSdCardInfo((current) => current ? { ...current, size_bytes: parsedReport.source_length } : current);
      setScanState("success");
    } catch (caught) {
      setScanState("error");
      setError(caught instanceof Error ? caught.message : String(caught));
    } finally {
      setActiveMode(null);
      setIsStopping(false);
    }
  }

  async function stopDeepScan() {
    setIsStopping(true);
    setScanProgress((current) => current ? { ...current, phase: "stopping" } : current);
    try {
      const accepted = await invoke<boolean>("cancel_active_scan");
      if (!accepted) {
        setIsStopping(false);
        setError("当前没有可停止的深度扫描");
      }
    } catch (caught) {
      setIsStopping(false);
      setError(caught instanceof Error ? caught.message : String(caught));
    }
  }

  function toggleCandidate(candidate: VideoCandidate) {
    if (!canRecover(candidate) || busy) return;
    const key = candidateKey(candidate);
    setSelectedCandidateKeys((current) => {
      const next = new Set(current);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  }

  function selectAllRecoverable() {
    setSelectedCandidateKeys(new Set(recoverableCandidates.map(candidateKey)));
  }

  async function chooseBatchDestination() {
    setError(null);
    try {
      const selected = await open({
        multiple: false,
        directory: true,
        title: "选择批量恢复文件的存放文件夹",
      });
      if (!selected) return;
      if (sdCardInfo) {
        const sourceMount = sdCardInfo.mount_point.replace(/\/+$/, "");
        if (selected === sourceMount || selected.startsWith(`${sourceMount}/`)) {
          setError("批量恢复目标不能位于原 SD 卡，请选择电脑内置磁盘或另一块外置磁盘。");
          return;
        }
      }
      const capacity = await invoke<DestinationCapacity>("check_export_destination", {
        outputDirectory: selected,
      });
      setBatchDestination(capacity);
    } catch (caught) {
      setError(caught instanceof Error ? caught.message : String(caught));
    }
  }

  async function exportSelectedCandidates() {
    if (!sdCardInfo || selectedCandidates.length === 0) {
      setError("请先勾选至少一个可恢复的视频文件。");
      return;
    }
    if (!batchDestination) {
      setError("请先选择批量导出的存放文件夹。");
      return;
    }

    try {
      const refreshedCapacity = await invoke<DestinationCapacity>("check_export_destination", {
        outputDirectory: batchDestination.path,
      });
      setBatchDestination(refreshedCapacity);
      if (refreshedCapacity.availableBytes < selectedBytes) {
        setError(`目标磁盘空间不足：需要 ${formatBytes(selectedBytes)}，当前可用 ${formatBytes(refreshedCapacity.availableBytes)}。`);
        return;
      }

      const approved = await confirm(
        `将一次性恢复 ${selectedCandidates.length} 个视频，共 ${formatBytes(selectedBytes)}。\n\n目标文件夹：${refreshedCapacity.path}\n可用空间：${formatBytes(refreshedCapacity.availableBytes)}\n\n已有同名文件不会被覆盖，TraceDisk 会自动使用不重复的新名称。`,
        { title: "开始批量只读导出？", kind: "warning", okLabel: "一键导出", cancelLabel: "取消" },
      );
      if (!approved) return;

      setError(null);
      setRecoveryMessage(null);
      setIsBatchExporting(true);
      setBatchProgress({
        phase: "preparing",
        currentFile: null,
        processedFiles: 0,
        successfulFiles: 0,
        totalFiles: selectedCandidates.length,
        bytesProcessed: 0,
        totalBytes: selectedBytes,
      });
      const onProgress = new Channel<BatchExportProgress>();
      onProgress.onmessage = (progress) => setBatchProgress(progress);
      const result = await invoke<BatchExportResult>("recover_candidates_batch", {
        rawDevicePath: sdCardInfo.raw_device_path,
        sourceSizeBytes: sdCardInfo.size_bytes,
        outputDirectory: refreshedCapacity.path,
        items: selectedCandidates.map((candidate) => ({
          name: candidate.name,
          sizeBytes: candidate.size_bytes,
          extents: candidate.extents,
        })),
        onProgress,
      });

      const warningText = result.warnings.length > 0 ? `；另有 ${result.warnings.length} 条权限提示` : "";
      if (result.successfulFiles.length > 0) {
        setRecoveryMessage(`批量导出成功 ${result.successfulFiles.length} 个文件，共 ${formatBytes(result.bytesWritten)}，存放于 ${result.outputDirectory}${warningText}`);
      }
      if (result.failures.length > 0) {
        const details = result.failures.slice(0, 3).map((failure) => `${failure.name}：${failure.error}`).join("；");
        const remaining = result.failures.length > 3 ? `；另有 ${result.failures.length - 3} 个错误` : "";
        setError(`批量导出有 ${result.failures.length} 个文件失败。${details}${remaining}`);
        setScanState("error");
      } else {
        setScanState("success");
      }
      void invoke<DestinationCapacity>("check_export_destination", {
        outputDirectory: result.outputDirectory,
      }).then(setBatchDestination).catch(() => undefined);
    } catch (caught) {
      setScanState("error");
      setError(caught instanceof Error ? caught.message : String(caught));
    } finally {
      setIsBatchExporting(false);
    }
  }

  async function recoverVideo(candidate: VideoCandidate) {
    if (!sdCardInfo) return;
    if (!canRecover(candidate)) {
      setError("该候选文件的 FAT 链不完整，暂时无法安全拼接；请继续进行深度扫描。");
      return;
    }

    const outputPath = await save({
      title: "将恢复视频保存到另一块磁盘",
      defaultPath: candidate.name,
      filters: [{ name: `${candidate.extension} 视频`, extensions: [candidate.extension.toLowerCase()] }],
    });
    if (!outputPath) return;

    const approved = await confirm(
      `将从 SD 卡按 ${candidate.extents.length} 个只读片段导出 ${formatBytes(candidate.size_bytes)} 到：\n${outputPath}\n\n请确认目标不在原 SD 卡上。已有文件不会被覆盖。`,
      { title: "开始恢复这个视频？", kind: "warning", okLabel: "开始导出", cancelLabel: "取消" },
    );
    if (!approved) return;

    setError(null);
    setRecoveryMessage(null);
    setRecoveringId(candidate.id);
    try {
      const result = await invoke<string>("recover_candidate", {
        rawDevicePath: sdCardInfo.raw_device_path,
        sourceSizeBytes: sdCardInfo.size_bytes,
        sizeBytes: candidate.size_bytes,
        extents: candidate.extents,
        outputPath,
      });
      setRecoveryMessage(
        result === "ok-owner-warning"
          ? `恢复完成：${outputPath}（文件可读取，但所有者信息未能自动调整）`
          : `恢复完成：${outputPath}`,
      );
    } catch (caught) {
      setError(caught instanceof Error ? caught.message : String(caught));
    } finally {
      setRecoveringId(null);
    }
  }

  return (
    <div className="app-shell">
      <header className="topbar">
        <div className="brand">
          <div className="brand-mark" aria-hidden="true"><span /><span /></div>
          <div><strong>TraceDisk</strong><small>Camera recovery workspace</small></div>
        </div>
        <div className="read-only-badge"><span className="status-dot" />原卡只读</div>
      </header>

      <main>
        <section className="hero-panel">
          <div className="hero-copy">
            <p className="eyebrow">DIRECT SD RECOVERY</p>
            <h1>先快速查找，<br />不够再扫全卡。</h1>
            <p className="hero-description">
              不需要先制作 116 GB 镜像。TraceDisk 先读取少量文件系统元数据；找不到时，再直接流式扫描原卡，不占用同等本地空间。
            </p>
            <div className="hero-actions">
              <button className="primary-button" type="button" onClick={chooseSdCard} disabled={busy}>
                {busy ? <span className="spinner" aria-hidden="true" /> : <span className="folder-icon" aria-hidden="true" />}
                选择已挂载 SD 卡
              </button>
              <button className="secondary-button" type="button" onClick={chooseImageAndInspect} disabled={busy}>
                打开已有镜像（可选）
              </button>
              <div className={`scan-status status-${scanState}`}><span />{statusText}</div>
            </div>
            <div className="path-entry">
              <span>PATH</span>
              <input
                value={sdPathInput}
                onChange={(event) => setSdPathInput(event.target.value)}
                onKeyDown={(event) => {
                  if (event.key === "Enter" && !busy) void resolveSdCardPath(sdPathInput);
                }}
                placeholder={sdPathExample}
                aria-label="SD 卡挂载路径"
                spellCheck={false}
                disabled={busy}
              />
              <button type="button" onClick={() => resolveSdCardPath(sdPathInput)} disabled={busy || !sdPathInput.trim()}>
                识别路径
              </button>
            </div>
          </div>
          <div className="disk-visual" aria-hidden="true">
            <div className="disk-ring disk-ring-outer" />
            <div className="disk-ring disk-ring-middle" />
            <div className="disk-core"><div className="disk-core-label">TD</div></div>
            <div className="scan-line" />
          </div>
        </section>

        {error && (
          <section className="error-banner" role="alert">
            <div className="error-icon">!</div>
            <div className="error-copy">
              <strong>操作没有完成</strong><p>{error}</p>
              {error.includes("完全磁盘访问权限") && (
                <button
                  type="button"
                  onClick={() => void invoke("open_full_disk_access_settings").catch((caught) =>
                    setError(caught instanceof Error ? caught.message : String(caught))
                  )}
                >
                  打开“完全磁盘访问权限”设置
                </button>
              )}
            </div>
          </section>
        )}

        {recoveryMessage && (
          <section className="success-banner" role="status">
            <div className="success-icon">✓</div>
            <div><strong>视频已导出到其他磁盘</strong><p>{recoveryMessage}</p></div>
          </section>
        )}

        {report ? (
          <section className="results-section">
            <div className="section-heading">
              <div><p className="eyebrow">IMAGE INSPECTION</p><h2>{fileName(report.source_path)}</h2></div>
              <p className="source-path" title={report.source_path}>{report.source_path}</p>
            </div>
            <div className="summary-grid">
              <article className="metric-card"><span>镜像大小</span><strong>{formatBytes(report.source_length)}</strong><small>{report.source_length.toLocaleString()} bytes</small></article>
              <article className="metric-card"><span>分区方案</span><strong>{report.partition_scheme.toUpperCase()}</strong><small>{report.partitions.length} 个分区</small></article>
              <article className="metric-card"><span>文件系统</span><strong>{recognizedVolumes ? report.volumes.map((volume) => filesystemLabel(volume.filesystem)).join(" · ") : "未识别"}</strong><small>{recognizedVolumes} 个有效卷</small></article>
              <article className="metric-card accent-card"><span>扇区大小</span><strong>{report.logical_sector_size} B</strong><small>逻辑扇区</small></article>
            </div>
          </section>
        ) : sdCardInfo ? (
          <section className="sd-card-section">
            <div className="section-heading">
              <div><p className="eyebrow">REMOVABLE DEVICE</p><h2>{sdCardInfo.volume_name}</h2></div>
              <p className="source-path" title={sdCardInfo.mount_point}>{sdCardInfo.mount_point}</p>
            </div>

            <div className="summary-grid sd-summary-grid">
              <article className="metric-card"><span>整卡容量</span><strong>{formatBytes(sdCardInfo.size_bytes)}</strong><small>无需预留同等本地空间</small></article>
              <article className="metric-card"><span>挂载分区</span><strong>{sdCardInfo.partition_identifier}</strong><small>{sdCardInfo.device_node}</small></article>
              <article className="metric-card accent-card"><span>原始设备</span><strong>{sdCardInfo.whole_disk_identifier}</strong><small>{sdCardInfo.raw_device_path}</small></article>
              <article className="metric-card"><span>安全状态</span><strong>外置设备已验证</strong><small>扫描前自动卸载，后端只读打开</small></article>
            </div>

            <article className="scan-workflow-panel">
              <div className="workflow-heading">
                <div className="shield-icon" aria-hidden="true">✓</div>
                <div><p className="eyebrow">TWO-STAGE SCAN</p><h3>按风险和耗时逐级扫描</h3></div>
                <span className="device-safe-badge">内置磁盘保护已通过</span>
              </div>
              <div className="scan-stage-list">
                <section className={`scan-stage ${deviceReport?.mode === "metadata" ? "stage-complete" : ""}`}>
                  <div className="stage-index">1</div>
                  <div className="stage-copy">
                    <strong>快速扫描：不扫整张卡</strong>
                    <p>读取分区、exFAT 目录项、FAT 和分配位图，找回文件信息并逐个检查碎片链。</p>
                    <small>FAT 链完整的文件会立即开放恢复，不需要等待深度扫描。</small>
                  </div>
                  <button className="primary-button stage-button" type="button" disabled={busy} onClick={() => startDeviceScan("metadata")}>
                    {busy && activeMode === "metadata" ? <><span className="spinner" />扫描中</> : deviceReport?.mode === "metadata" ? "重新快速扫描" : "开始快速扫描"}
                  </button>
                </section>

                <div className="stage-connector"><span /></div>

                <section className={`scan-stage deep-stage ${deviceReport?.mode === "deep" ? "stage-complete" : ""}`}>
                  <div className="stage-index">2</div>
                  <div className="stage-copy">
                    <strong>深度扫描：逐块读取整张卡</strong>
                    <p>快速结果不够时，直接在原卡上搜索 MP4/MOV 容器特征；只在内存保留小块数据，不生成整卡副本。</p>
                    <small>会读取约 {formatBytes(sdCardInfo.size_bytes)}，耗时取决于 SD 卡和读卡器速度。</small>
                  </div>
                  <div className="deep-scan-controls">
                    {busy && activeMode === "deep" ? (
                      <button
                        className="stop-scan-button"
                        type="button"
                        disabled={isStopping}
                        onClick={stopDeepScan}
                      >
                        {isStopping ? "正在停止…" : "停止扫描"}
                      </button>
                    ) : (
                      <button
                        className="secondary-button stage-button deep-button"
                        type="button"
                        disabled={busy || !deviceReport}
                        onClick={() => startDeviceScan("deep")}
                        title={!deviceReport ? "请先完成快速扫描" : undefined}
                      >
                        {deviceReport?.mode === "deep" ? "重新深度扫描" : "继续深度扫描"}
                      </button>
                    )}
                  </div>
                  {activeMode === "deep" && scanProgress && (
                    <div className="deep-progress" role="progressbar" aria-valuenow={Math.round(deepProgressPercent)} aria-valuemin={0} aria-valuemax={100}>
                      <div className="progress-summary">
                        <span>{isStopping ? "正在完成当前读取块并停止" : scanProgress.phase === "preparing" ? "正在准备管理员只读扫描" : "正在逐块读取原始设备"}</span>
                        <strong>{deepProgressPercent.toFixed(1)}%</strong>
                      </div>
                      <div className="progress-track"><span style={{ width: `${deepProgressPercent}%` }} /></div>
                      <small>
                        已读取 {formatBytes(scanProgress.bytesExamined)} / {formatBytes(scanProgress.totalBytes)}
                        <i>·</i> 已发现 {scanProgress.candidatesFound} 个候选
                      </small>
                    </div>
                  )}
                </section>
              </div>
              <p className="terminal-note">扫描会让 SD 卡暂时从 {isWindows ? "文件资源管理器" : "Finder"} 消失，这是安全卸载，不是推出。重新拔插读卡器即可再次挂载。</p>
            </article>

            {deviceReport && (
              <section className="candidate-section">
                <div className="candidate-heading">
                  <div>
                    <p className="eyebrow">SCAN RESULT</p>
                    <h3>{deviceReport.mode === "metadata" ? "快速扫描结果" : deviceReport.cancelled ? "已停止的深度扫描结果" : "整卡深度扫描结果"}</h3>
                  </div>
                  <div className="candidate-count"><strong>{deviceReport.candidates.length}</strong><span>个视频候选</span></div>
                </div>
                <div className="scan-metrics">
                  <span>文件系统 <strong>{filesystemLabel(deviceReport.filesystem)}</strong></span>
                  <span>已读取 <strong>{formatBytes(deviceReport.bytes_examined)}</strong></span>
                  <span>扫描范围 <strong>{deviceReport.mode === "deep" ? "整张原始设备" : "文件系统元数据"}</strong></span>
                </div>

                {deviceReport.candidates.length > 0 && (
                  <section className="batch-export-panel">
                    <div className="batch-export-heading">
                      <div>
                        <strong>批量恢复</strong>
                        <small>已选 {selectedCandidates.length} / 可恢复 {recoverableCandidates.length} 个，共 {formatBytes(selectedBytes)}</small>
                      </div>
                      <div className="batch-selection-actions">
                        <button type="button" disabled={busy || recoverableCandidates.length === 0} onClick={selectAllRecoverable}>全选可恢复</button>
                        <button type="button" disabled={busy || selectedCandidates.length === 0} onClick={() => setSelectedCandidateKeys(new Set())}>清空选择</button>
                      </div>
                    </div>
                    <div className="batch-destination-row">
                      <button className="destination-button" type="button" disabled={busy} onClick={chooseBatchDestination}>
                        {batchDestination ? "更换存放地点" : "选择存放地点"}
                      </button>
                      <div className="destination-summary">
                        <strong title={batchDestination?.path}>{batchDestination?.path ?? "尚未选择目标文件夹"}</strong>
                        {batchDestination ? (
                          <small className={batchHasEnoughSpace ? "capacity-ok" : "capacity-insufficient"}>
                            需要 {formatBytes(selectedBytes)} · 可用 {formatBytes(batchDestination.availableBytes)} · {batchHasEnoughSpace ? "空间满足" : "空间不足"}
                          </small>
                        ) : (
                          <small>选择其他磁盘上的文件夹后，将自动检查可用空间。</small>
                        )}
                      </div>
                      <button
                        className="batch-export-button"
                        type="button"
                        disabled={busy || selectedCandidates.length === 0 || !batchDestination || !batchHasEnoughSpace}
                        onClick={exportSelectedCandidates}
                      >
                        {isBatchExporting ? <><span className="spinner" />正在导出</> : "一键导出所选"}
                      </button>
                    </div>
                    {batchProgress && (isBatchExporting || batchProgress.phase.startsWith("completed")) && (
                      <div className="batch-progress" role="progressbar" aria-valuenow={Math.round(batchProgressPercent)} aria-valuemin={0} aria-valuemax={100}>
                        <div className="progress-summary">
                          <span>{batchProgress.phase === "preparing" ? "正在准备管理员只读导出" : batchProgress.currentFile ? `正在导出 ${batchProgress.currentFile}` : "批量导出已处理完成"}</span>
                          <strong>{batchProgressPercent.toFixed(1)}%</strong>
                        </div>
                        <div className="progress-track"><span style={{ width: `${batchProgressPercent}%` }} /></div>
                        <small>
                          已处理 {formatBytes(batchProgress.bytesProcessed)} / {formatBytes(batchProgress.totalBytes)}
                          <i>·</i> 已完成 {batchProgress.processedFiles} / {batchProgress.totalFiles} 个
                          <i>·</i> 成功 {batchProgress.successfulFiles} 个
                        </small>
                      </div>
                    )}
                  </section>
                )}

                {deviceReport.candidates.length ? (
                  <div className="candidate-list">
                    {deviceReport.candidates.slice(0, 200).map((candidate) => (
                      <article className={`candidate-card ${selectedCandidateKeys.has(candidateKey(candidate)) ? "candidate-selected" : ""}`} key={`${candidate.source}-${candidate.byte_offset}`}>
                        <label className="candidate-checkbox" title={canRecover(candidate) ? "选择此文件用于批量恢复" : "该文件当前不可安全恢复"}>
                          <input
                            type="checkbox"
                            checked={selectedCandidateKeys.has(candidateKey(candidate))}
                            disabled={busy || !canRecover(candidate)}
                            onChange={() => toggleCandidate(candidate)}
                          />
                          <span aria-hidden="true" />
                        </label>
                        <div className="candidate-icon">{candidate.extension}</div>
                        <div className="candidate-main">
                          <strong>{candidate.name}</strong>
                          <small>{candidate.original_path || "通过视频容器特征发现，原文件名未知"}</small>
                        </div>
                        <div className="candidate-size"><strong>{formatBytes(candidate.size_bytes)}</strong><small>偏移 0x{candidate.byte_offset.toString(16)}</small></div>
                        <div className="candidate-actions">
                          <div>
                            <span className={`recovery-pill recovery-${candidate.recoverability}`}>{recoveryLabel(candidate.recoverability)}</span>
                            <button
                              type="button"
                              disabled={busy || !canRecover(candidate)}
                              onClick={() => recoverVideo(candidate)}
                              title={canRecover(candidate) ? "保存到另一块磁盘" : "FAT 链不完整，需要继续深度扫描"}
                            >
                              {recoveringId === candidate.id ? "正在导出…" : "恢复到…"}
                            </button>
                          </div>
                          {candidate.fat_chain_status === "complete" && (
                            <small className="chain-complete">FAT 链完整，已解析为 {candidate.extents.length} 个物理片段，可立即恢复。</small>
                          )}
                          {candidate.fat_chain_status === "broken" && (
                            <small>FAT 链中断、循环或越界；快速扫描无法安全拼接，需要深度扫描。</small>
                          )}
                          {candidate.fat_chain_status === "not-required" && (
                            <small className="chain-complete">目录项标记为连续存储，可立即恢复。</small>
                          )}
                        </div>
                      </article>
                    ))}
                    {deviceReport.candidates.length > 200 && <p className="result-note">界面先显示前 200 个候选，共 {deviceReport.candidates.length} 个。</p>}
                  </div>
                ) : (
                  <div className="no-candidates">
                    <strong>这一阶段没有找到视频候选</strong>
                    <p>{deviceReport.mode === "metadata" ? "删除目录项可能已被清理。现在可以继续深度扫描整张卡。" : deviceReport.cancelled ? "扫描已按你的请求停止，目前没有发现可用候选；可以稍后重新开始。" : "整卡特征扫描也未识别到可用 MP4/MOV 容器，数据可能已覆盖或采用了暂未支持的碎片布局。"}</p>
                  </div>
                )}

                {deviceReport.warnings.length > 0 && (
                  <div className="scan-warnings">{deviceReport.warnings.map((warning) => <p key={warning}>• {warning}</p>)}</div>
                )}
              </section>
            )}
          </section>
        ) : (
          <section className="empty-dashboard">
            <article><span className="step-number">01</span><div><strong>停止写入</strong><p>先不要再拍摄、删除或格式化原 SD 卡。</p></div></article>
            <article><span className="step-number">02</span><div><strong>快速扫描</strong><p>读取删除目录项，优先用最少 I/O 找回视频线索。</p></div></article>
            <article><span className="step-number">03</span><div><strong>按需深扫</strong><p>快速扫描不够时，再流式读取整张卡，不创建镜像。</p></div></article>
          </section>
        )}

        <section className="safety-panel">
          <div className="shield-icon" aria-hidden="true">✓</div>
          <div><strong>源数据保护已启用</strong><p>原始设备仅通过只读文件句柄访问；扫描结果只保存在内存中。</p></div>
          <div className="safety-rule" />
          <p className="safety-note">后续导出恢复文件时，也必须保存到另一块磁盘。</p>
        </section>
      </main>

      <footer><span>TraceDisk 0.1.0</span><span>Direct read-only recovery toolkit</span></footer>
    </div>
  );
}

function App() {
  const [accessGranted, setAccessGranted] = useState(false);
  return accessGranted
    ? <TraceDiskWorkspace />
    : <AccessGate onVerified={() => setAccessGranted(true)} />;
}

export default App;
