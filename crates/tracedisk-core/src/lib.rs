//! Read-only disk-image inspection primitives for TraceDisk.

mod error;
mod filesystem;
mod partition;
mod report;
mod scan;
mod source;

pub use error::{Result, TraceError};
pub use filesystem::{detect_filesystems, ExFatInfo, Fat32Info, FileSystemDetails, VolumeInfo};
pub use partition::{inspect_partitions, DiskLayout, PartitionInfo, PartitionScheme};
pub use report::{inspect_image, InspectionReport};
pub use scan::{
    deep_scan_videos, deep_scan_videos_with_progress, scan_deleted_videos, RecoveryExtent,
    ScanProgress, ScanReport, VideoCandidate,
};
pub use source::{BlockSource, ImageSource, RawDeviceSource};
