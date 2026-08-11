# TraceDisk

TraceDisk is a read-only recovery tool for deleted camera videos. It can scan a
mounted SD card through a validated macOS or Windows raw-device path, or inspect
an existing image. Direct scanning does not create a card-sized local copy.

> **Safety:** Stop using the affected SD card immediately. TraceDisk unmounts
> the card before raw access, opens the source read-only, and requires recovered
> files to be saved to a different disk.

## Current milestone

- Read-only image access with checked offsets
- Read-only raw-device access with strict external/removable-device validation
- MBR, GPT, and super-floppy detection
- FAT32 and exFAT boot-sector inspection
- Fast exFAT deleted-directory-entry scanning without reading the whole card
- exFAT FAT-chain validation and ordered multi-extent recovery
- Optional full-device MP4/MOV container carving in bounded memory
- Single-file and capacity-checked batch export with no-overwrite output creation
- Human-readable and JSON reports plus synthetic scanner tests
- React + TypeScript + Tauri desktop application for macOS and Windows

The fast metadata stage currently targets exFAT, which is typical for large
camera cards. FAT32 cards can use the full-device deep scan.

## Build and test

```bash
cargo test --workspace
cargo build --release
```

## Inspect an image

```bash
cargo run -p tracedisk -- inspect ./dji_sd.img
cargo run -p tracedisk -- inspect ./dji_sd.img --json
```

The CLI accepts image files. Administrator raw-device access is kept inside the
desktop application's validated helper flow.

## Workspace

```text
apps/
└── desktop/         # React, TypeScript, and Tauri desktop application
crates/
├── tracedisk-core/  # Read-only source, partition, and filesystem parsers
└── tracedisk-cli/   # Command-line application
```

## Desktop application

```bash
cd apps/desktop
npm install
npm run tauri dev
```

Every desktop launch verifies a machine-bound Ed25519 license locally. The
activation page shows a SHA-256 machine code derived from the operating system's
stable device identifier; the customer sends that code to the license issuer
and imports the returned license. No network request or signing secret exists in
the desktop application. A valid license is saved locally and rechecked on each
launch, and all disk commands also enforce the authorized state in Rust. Build
with `TRACEDISK_LICENSE_PUBLIC_KEY` set to the issuer's Base64 public key. The
format, key setup, and offline-clock limitation are described in
[`docs/AUTHENTICATION.md`](docs/AUTHENTICATION.md).

If you only know the mounted SD card path (for example `/Volumes/SD_Card` on
macOS or `E:\` on Windows), choose **选择已挂载 SD 卡** or enter that path in
the application. TraceDisk resolves it to `/dev/rdiskN` or
`\\.\PhysicalDriveN`, rejects internal/system disks, and offers a two-stage
flow:

1. **快速扫描** reads filesystem metadata, deleted directory entries, the FAT,
   and the allocation bitmap. Contiguous files and candidates with a complete
   FAT chain can be recovered immediately, including fragmented files.
2. **整卡深度扫描** streams over the raw device looking for MP4/MOV container
   structures, without storing a full image.

macOS requests administrator authorization because raw disks are normally not
readable by a standard account. The privileged helper revalidates the device,
opens it read-only, and exposes no raw-device write operation. Recoverable
candidates can be exported individually or in a capacity-checked batch to
another disk without overwriting existing files.

Build the macOS application bundle with:

```bash
npm run bundle:mac
```

The signed local development bundle is written to:

```text
../../target/release/bundle/macos/TraceDisk.app
```

## Package a versioned macOS release

The application, Cargo workspace, and Tauri configuration currently share the
version `0.1.0`. On an Apple Silicon Mac, create a versioned DMG, ZIP, checksum
file, and release notes with:

```bash
cd apps/desktop
npm run release:mac
```

Artifacts are written to:

```text
release/v0.1.0/
├── TraceDisk-v0.1.0-macos-arm64.dmg
├── TraceDisk-v0.1.0-macos-arm64.zip
├── SHA256SUMS.txt
└── RELEASE_NOTES.md
```

The local preview build is ad-hoc signed but not notarized with an Apple
Developer ID. Public download instructions must therefore include the macOS
Control-click **Open** flow and the Full Disk Access setup described in the
release notes.

## Unified release command

After each update, prepare and build a versioned release with the cross-platform
Node script:

```bash
cd apps/desktop
npm run release -- 0.1.1
```

The first run updates the Cargo, npm, and Tauri versions together. If the
version notes do not exist, it creates `docs/releases/v0.1.1.md` and stops so
the notes can be completed. Run the same command again to execute formatting,
tests, Clippy, the frontend build, and the native platform packager. Existing
artifacts for the same version are protected from accidental overwrite; use
`--force` only when intentionally rebuilding that version.

Useful non-publishing modes:

```bash
npm run release -- 0.1.1 --prepare-only
npm run release -- 0.1.1 --check
```

## Windows application

The Windows platform layer maps a selected drive letter to a physical disk with
PowerShell `Get-Partition` and `Get-Disk`, then rejects boot/system disks and
accepts only USB, SD, or MMC bus types. Raw scanning and recovery run in the
same validated helper mode after Windows UAC approval. The elevated helper
removes the SD card volume mount point with `mountvol /p`, revalidates the disk,
and opens `\\.\PhysicalDriveN` read-only.

Build an x64 NSIS installer on a Windows development machine with Node.js,
Rust, and the Tauri prerequisites installed:

```powershell
cd apps/desktop
npm install
npm run release:windows
```

The versioned installer and SHA-256 file are written under `release/v0.1.0/`.
The Windows platform code can be cross-checked from macOS, but raw-device,
UAC, volume-dismount, installer, and real SD-card behavior must be signed off on
a Windows 10/11 x64 machine before publishing it as a stable release. See
[`docs/WINDOWS.md`](docs/WINDOWS.md) for the test checklist.
