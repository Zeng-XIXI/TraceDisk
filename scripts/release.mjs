#!/usr/bin/env node

import { existsSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const scriptDirectory = dirname(fileURLToPath(import.meta.url));
const projectDirectory = resolve(scriptDirectory, "..");
const desktopDirectory = join(projectDirectory, "apps", "desktop");
const cargoManifestPath = join(projectDirectory, "Cargo.toml");
const packageJsonPath = join(desktopDirectory, "package.json");
const tauriConfigPath = join(desktopDirectory, "src-tauri", "tauri.conf.json");

const rawArguments = process.argv.slice(2);
const flags = new Set(rawArguments.filter((argument) => argument.startsWith("--")));
const positionals = rawArguments.filter((argument) => !argument.startsWith("--"));
const version = positionals[0];

if (flags.has("--help") || !version) {
  printUsage();
  process.exit(flags.has("--help") ? 0 : 1);
}
if (positionals.length !== 1) {
  fail("只接受一个版本号参数");
}
for (const flag of flags) {
  if (!["--help", "--check", "--force", "--prepare-only"].includes(flag)) {
    fail(`未知参数：${flag}`);
  }
}
if (!/^\d+\.\d+\.\d+$/.test(version)) {
  fail("版本号必须是 x.y.z 格式，例如 0.1.1");
}

const checkOnly = flags.has("--check");
const force = flags.has("--force");
const prepareOnly = flags.has("--prepare-only");
const releaseNotesPath = join(projectDirectory, "docs", "releases", `v${version}.md`);
const releaseDirectory = join(projectDirectory, "release", `v${version}`);

const packageJson = readJson(packageJsonPath, "package.json");
const tauriConfig = readJson(tauriConfigPath, "tauri.conf.json");
const cargoManifest = readFileSync(cargoManifestPath, "utf8");
const cargoVersionMatch = cargoManifest.match(
  /(\[workspace\.package\][\s\S]*?^version\s*=\s*")([^"]+)(")/m,
);
if (!cargoVersionMatch) {
  fail("无法在 Cargo.toml 的 [workspace.package] 中找到版本号");
}

const currentVersions = {
  cargo: cargoVersionMatch[2],
  package: String(packageJson.version ?? ""),
  tauri: String(tauriConfig.version ?? ""),
};

if (checkOnly) {
  console.log(`目标版本：v${version}`);
  console.log(
    `当前版本：Cargo=${currentVersions.cargo}, package=${currentVersions.package}, Tauri=${currentVersions.tauri}`,
  );
  console.log(`版本说明：${existsSync(releaseNotesPath) ? "已存在" : "尚未创建"}`);
  console.log(`发布目录：${existsSync(releaseDirectory) ? "已存在" : "尚未创建"}`);
  console.log(`当前平台：${platformLabel()}`);
  process.exit(0);
}

packageJson.version = version;
tauriConfig.version = version;
const nextCargoManifest = cargoManifest.replace(
  cargoVersionMatch[0],
  `${cargoVersionMatch[1]}${version}${cargoVersionMatch[3]}`,
);

writeJsonIfChanged(packageJsonPath, packageJson);
writeJsonIfChanged(tauriConfigPath, tauriConfig);
writeTextIfChanged(cargoManifestPath, nextCargoManifest);
console.log(`已统一项目版本号为 v${version}`);

let releaseNotesCreated = false;
if (!existsSync(releaseNotesPath)) {
  writeFileSync(releaseNotesPath, releaseNotesTemplate(version), "utf8");
  releaseNotesCreated = true;
  console.log(`已创建版本说明模板：${releaseNotesPath}`);
}

if (releaseNotesCreated || prepareOnly) {
  console.log("");
  console.log("发布准备已完成。请检查并填写版本说明，然后重新运行：");
  console.log(`  node scripts/release.mjs ${version}`);
  process.exit(0);
}

assertReleaseNotesReady(releaseNotesPath);
assertSupportedPlatform();
assertReleaseDirectoryAvailable(releaseDirectory, force);

run("cargo", ["fmt", "--all", "--", "--check"], projectDirectory);
run("cargo", ["test", "--workspace"], projectDirectory);
run(
  "cargo",
  ["clippy", "--workspace", "--all-targets", "--", "-D", "warnings"],
  projectDirectory,
);
run(npmCommand(), ["run", "build"], desktopDirectory);
run(
  npmCommand(),
  ["run", process.platform === "darwin" ? "release:mac" : "release:windows"],
  desktopDirectory,
);

console.log("");
console.log(`TraceDisk v${version} ${platformLabel()} 发布版本已经生成：`);
console.log(releaseDirectory);

function printUsage() {
  console.log(`
TraceDisk 统一发布脚本

用法：
  node scripts/release.mjs <版本号> [选项]

示例：
  node scripts/release.mjs 0.1.1
  node scripts/release.mjs 0.1.1 --prepare-only
  node scripts/release.mjs 0.1.1 --check
  node scripts/release.mjs 0.1.1 --force

选项：
  --prepare-only  只更新版本号并准备版本说明，不执行测试和打包
  --check         只显示版本和发布状态，不修改任何文件
  --force         允许覆盖同版本已存在的发布产物
  --help          显示帮助
`);
}

function readJson(path, label) {
  try {
    return JSON.parse(readFileSync(path, "utf8"));
  } catch (error) {
    fail(`无法读取 ${label}: ${error.message}`);
  }
}

function writeJsonIfChanged(path, value) {
  writeTextIfChanged(path, `${JSON.stringify(value, null, 2)}\n`);
}

function writeTextIfChanged(path, value) {
  if (readFileSync(path, "utf8") !== value) {
    writeFileSync(path, value, "utf8");
  }
}

function releaseNotesTemplate(targetVersion) {
  const date = new Intl.DateTimeFormat("en-CA", {
    timeZone: "Asia/Shanghai",
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
  }).format(new Date());
  return `# TraceDisk v${targetVersion}\n\n发布日期：${date}\n\n## 新增\n\n- TODO：填写本版本新增功能。\n\n## 修复\n\n- TODO：填写本版本修复内容。\n\n## 使用提醒\n\n- 发现误删后立即停止使用原 SD 卡。\n- 恢复结果必须保存到其他磁盘。\n- macOS 用户可能需要重新授予完全磁盘访问权限。\n`;
}

function assertReleaseNotesReady(path) {
  const notes = readFileSync(path, "utf8");
  if (notes.includes("TODO：")) {
    fail(`版本说明仍包含 TODO，请先编辑：${path}`);
  }
}

function assertSupportedPlatform() {
  if (process.platform !== "darwin" && process.platform !== "win32") {
    fail("发布打包目前只支持 macOS 和 Windows");
  }
}

function assertReleaseDirectoryAvailable(path, allowOverwrite) {
  if (!existsSync(path) || allowOverwrite) {
    return;
  }
  const existingArtifacts = readdirSync(path).filter((name) =>
    /\.(dmg|zip|exe)$/i.test(name),
  );
  if (existingArtifacts.length > 0) {
    fail(`v${version} 已存在发布产物；请提升版本号，或确认后添加 --force`);
  }
}

function npmCommand() {
  return process.platform === "win32" ? "npm.cmd" : "npm";
}

function platformLabel() {
  if (process.platform === "darwin") return "macOS";
  if (process.platform === "win32") return "Windows";
  return process.platform;
}

function run(command, arguments_, cwd) {
  console.log("");
  console.log(`> ${command} ${arguments_.join(" ")}`);
  const result = spawnSync(command, arguments_, {
    cwd,
    env: process.env,
    stdio: "inherit",
    shell: false,
  });
  if (result.error) {
    fail(`无法启动 ${command}: ${result.error.message}`);
  }
  if (result.status !== 0) {
    fail(`${command} 执行失败，退出码 ${result.status ?? "unknown"}`);
  }
}

function fail(message) {
  console.error(`发布失败：${message}`);
  process.exit(1);
}
