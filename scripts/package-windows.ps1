param(
    [string]$Version
)

$ErrorActionPreference = "Stop"
$ProjectDirectory = Split-Path -Parent $PSScriptRoot
$PackageJson = Get-Content -Raw (Join-Path $ProjectDirectory "apps/desktop/package.json") | ConvertFrom-Json
$TauriConfig = Get-Content -Raw (Join-Path $ProjectDirectory "apps/desktop/src-tauri/tauri.conf.json") | ConvertFrom-Json
$CargoManifest = Get-Content -Raw (Join-Path $ProjectDirectory "Cargo.toml")
$PackageVersion = [string]$PackageJson.version
$TauriVersion = [string]$TauriConfig.version
$CargoVersion = [regex]::Match($CargoManifest, '(?m)^version = "([^"]+)"').Groups[1].Value

if ([string]::IsNullOrWhiteSpace($Version)) {
    $Version = $PackageVersion
}
if ($Version -notmatch '^\d+\.\d+\.\d+$') {
    throw "版本号必须采用语义化数字格式，例如 0.1.0"
}
if ($PackageVersion -ne $Version -or $TauriVersion -ne $Version -or $CargoVersion -ne $Version) {
    throw "版本号不一致：Cargo=$CargoVersion, package=$PackageVersion, Tauri=$TauriVersion, requested=$Version"
}

$NativeArchitecture = if ($env:PROCESSOR_ARCHITEW6432) {
    $env:PROCESSOR_ARCHITEW6432
} else {
    $env:PROCESSOR_ARCHITECTURE
}
if ($NativeArchitecture -ne "AMD64") {
    throw "TraceDisk Windows 预览版当前只构建 x64 安装包"
}

$DesktopDirectory = Join-Path $ProjectDirectory "apps/desktop"
$ReleaseDirectory = Join-Path $ProjectDirectory "release/v$Version"
$NsisDirectory = Join-Path $ProjectDirectory "target/release/bundle/nsis"
$ArtifactName = "TraceDisk-v$Version-windows-x64-setup.exe"
$ArtifactPath = Join-Path $ReleaseDirectory $ArtifactName

New-Item -ItemType Directory -Path $ReleaseDirectory -Force | Out-Null
Push-Location $DesktopDirectory
try {
    npm run bundle:windows
}
finally {
    Pop-Location
}

$Installer = Get-ChildItem -Path $NsisDirectory -Filter "*.exe" |
    Sort-Object LastWriteTimeUtc -Descending |
    Select-Object -First 1
if ($null -eq $Installer) {
    throw "Tauri 构建完成后没有找到 NSIS 安装程序"
}
Copy-Item -LiteralPath $Installer.FullName -Destination $ArtifactPath -Force
$Checksum = Get-FileHash -Algorithm SHA256 -LiteralPath $ArtifactPath
"$($Checksum.Hash.ToLowerInvariant())  $ArtifactName" |
    Set-Content -Encoding ascii (Join-Path $ReleaseDirectory "SHA256SUMS-windows.txt")

Write-Host "TraceDisk v$Version Windows 发布包已生成："
Write-Host $ArtifactPath
