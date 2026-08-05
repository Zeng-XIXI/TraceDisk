# TraceDisk Windows 适配说明

## 当前范围

Windows 版本复用与 macOS 相同的 Rust 恢复核心，包括分区识别、exFAT 删除目录扫描、FAT 链校验、多片段拼接、深度扫描和边界检查。Windows 平台层负责：

- 将 `E:\` 或盘内文件夹解析为 `\\.\PhysicalDriveN`。
- 使用 `Get-Partition`、`Get-Disk` 和 `Get-Volume` 复核设备身份。
- 拒绝 `IsBoot` 或 `IsSystem` 磁盘。
- 只接受 `USB`、`SD` 或 `MMC` 总线设备。
- 通过 Windows UAC 启动同一个可执行文件的管理员辅助模式。
- 在管理员辅助进程中使用 `mountvol /p` 卸载 SD 卡卷。
- 使用 Rust 只读文件句柄读取物理磁盘。
- 将恢复结果写入其他磁盘，并拒绝覆盖已有文件。

## 构建要求

- Windows 10 或 Windows 11 x64
- Rust stable x86_64-pc-windows-msvc 工具链
- Microsoft C++ Build Tools
- Node.js 20 或更高版本
- WebView2 Runtime
- Tauri 2 所需的 Windows 构建环境

构建命令：

```powershell
cd apps/desktop
npm install
npm run bundle:windows
```

生成带版本号的 NSIS 安装程序：

```powershell
npm run release:windows
```

## 真机验收清单

以下测试必须在准备发布的 Windows 10/11 x64 机器上完成：

1. 选择普通 USB U 盘时能识别到对应的 `PhysicalDriveN`。
2. 选择 SD/USB 读卡器时显示的容量与“磁盘管理”一致。
3. 选择系统盘路径时被安全保护拒绝。
4. 选择非 USB/SD/MMC 内置磁盘时被拒绝。
5. 拒绝 UAC 后不读取磁盘、不创建恢复文件。
6. 接受 UAC 后盘符从文件资源管理器暂时消失。
7. 重新插拔读卡器后 Windows 能重新分配盘符。
8. 快速扫描能读取 exFAT 元数据并显示候选文件。
9. FAT 链完整的碎片文件能按片段顺序导出。
10. 单文件和批量导出都不能覆盖已有文件。
11. 目标空间不足时批量导出在开始前被阻止。
12. 深度扫描能显示进度、发现候选并响应停止请求。
13. 拔出读卡器或发生 I/O 错误时，应用能停止并显示错误。
14. 恢复目标选择原 SD 卡时，卸载后导出失败且不写入原卡。

## 已知限制

- 某些读卡器可能被 Windows 报告为 `SCSI` 或其他总线类型。当前版本会保守拒绝，避免误读内置磁盘；需要收集真机信息后再增加安全白名单。
- Windows 原始设备和 UAC 流程不能在 macOS 上完成端到端验证。
- 深度扫描当前主要恢复物理连续的 MP4/MOV 容器，不能保证重组 FAT 链完全丢失的任意视频碎片。
