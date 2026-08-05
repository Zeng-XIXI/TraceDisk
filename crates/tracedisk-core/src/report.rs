use crate::{
    detect_filesystems, inspect_partitions, ExFatInfo, Fat32Info, FileSystemDetails, ImageSource,
    Result, VolumeInfo,
};
use crate::{BlockSource, DiskLayout};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct InspectionReport {
    pub source_path: PathBuf,
    pub source_length: u64,
    pub layout: DiskLayout,
    pub volumes: Vec<VolumeInfo>,
}

pub fn inspect_image(path: impl AsRef<Path>) -> Result<InspectionReport> {
    let source = ImageSource::open(path)?;
    let layout = inspect_partitions(&source)?;
    let volumes = detect_filesystems(&source, &layout)?;

    Ok(InspectionReport {
        source_path: source.path().to_path_buf(),
        source_length: source.len(),
        layout,
        volumes,
    })
}

impl InspectionReport {
    pub fn to_json_pretty(&self) -> String {
        let mut output = String::new();
        let _ = writeln!(output, "{{");
        let _ = writeln!(
            output,
            "  \"source_path\": \"{}\",",
            escape_json(&self.source_path.to_string_lossy())
        );
        let _ = writeln!(output, "  \"source_length\": {},", self.source_length);
        let _ = writeln!(
            output,
            "  \"partition_scheme\": \"{}\",",
            self.layout.scheme.as_str()
        );
        let _ = writeln!(
            output,
            "  \"logical_sector_size\": {},",
            self.layout.logical_sector_size
        );
        let _ = writeln!(output, "  \"partitions\": [");

        for (position, partition) in self.layout.partitions.iter().enumerate() {
            let comma = if position + 1 == self.layout.partitions.len() {
                ""
            } else {
                ","
            };
            let name = partition
                .name
                .as_deref()
                .map(|value| format!("\"{}\"", escape_json(value)))
                .unwrap_or_else(|| "null".into());
            let _ = writeln!(
                output,
                "    {{\"index\": {}, \"start_lba\": {}, \"sector_count\": {}, \"type_id\": \"{}\", \"name\": {}}}{}",
                partition.index,
                partition.start_lba,
                partition.sector_count,
                escape_json(&partition.type_id),
                name,
                comma
            );
        }

        let _ = writeln!(output, "  ],");
        let _ = writeln!(output, "  \"volumes\": [");
        for (position, volume) in self.volumes.iter().enumerate() {
            let comma = if position + 1 == self.volumes.len() {
                ""
            } else {
                ","
            };
            let _ = writeln!(output, "{}{}", volume_json(volume), comma);
        }
        let _ = writeln!(output, "  ]");
        let _ = write!(output, "}}");
        output
    }

    pub fn to_human_readable(&self) -> String {
        let mut output = String::new();
        let _ = writeln!(output, "TraceDisk image inspection");
        let _ = writeln!(output, "Source: {}", self.source_path.display());
        let _ = writeln!(output, "Size: {} bytes", self.source_length);
        let _ = writeln!(output, "Partition scheme: {}", self.layout.scheme.as_str());

        for partition in &self.layout.partitions {
            let _ = writeln!(
                output,
                "Partition {}: start LBA {}, {} sectors, type {}{}",
                partition.index,
                partition.start_lba,
                partition.sector_count,
                partition.type_id,
                partition
                    .name
                    .as_deref()
                    .map(|name| format!(", name {name}"))
                    .unwrap_or_default()
            );
        }

        for volume in &self.volumes {
            let _ = writeln!(
                output,
                "Volume on partition {}: {} at byte offset {}",
                volume.partition_index,
                volume.details.name(),
                volume.byte_offset
            );
        }
        output
    }
}

fn volume_json(volume: &VolumeInfo) -> String {
    let detail = match &volume.details {
        FileSystemDetails::Fat32(info) => fat32_json(info),
        FileSystemDetails::ExFat(info) => exfat_json(info),
        FileSystemDetails::Unknown => "{}".into(),
    };

    format!(
        "    {{\"partition_index\": {}, \"byte_offset\": {}, \"filesystem\": \"{}\", \"details\": {}}}",
        volume.partition_index,
        volume.byte_offset,
        volume.details.name(),
        detail
    )
}

fn fat32_json(info: &Fat32Info) -> String {
    format!(
        concat!(
            "{{\"bytes_per_sector\": {}, \"sectors_per_cluster\": {}, ",
            "\"reserved_sectors\": {}, \"fat_count\": {}, ",
            "\"sectors_per_fat\": {}, \"total_sectors\": {}, ",
            "\"root_cluster\": {}, \"volume_serial\": {}}}"
        ),
        info.bytes_per_sector,
        info.sectors_per_cluster,
        info.reserved_sectors,
        info.fat_count,
        info.sectors_per_fat,
        info.total_sectors,
        info.root_cluster,
        info.volume_serial
    )
}

fn exfat_json(info: &ExFatInfo) -> String {
    format!(
        concat!(
            "{{\"bytes_per_sector\": {}, \"sectors_per_cluster\": {}, ",
            "\"fat_offset\": {}, \"fat_length\": {}, ",
            "\"cluster_heap_offset\": {}, \"cluster_count\": {}, ",
            "\"root_directory_cluster\": {}, \"volume_serial\": {}, ",
            "\"percent_in_use\": {}}}"
        ),
        info.bytes_per_sector,
        info.sectors_per_cluster,
        info.fat_offset,
        info.fat_length,
        info.cluster_heap_offset,
        info.cluster_count,
        info.root_directory_cluster,
        info.volume_serial,
        info.percent_in_use
    )
}

fn escape_json(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character if character.is_control() => {
                let _ = write!(escaped, "\\u{:04x}", character as u32);
            }
            character => escaped.push(character),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PartitionInfo, PartitionScheme};
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn report_json_escapes_source_path() {
        let report = InspectionReport {
            source_path: PathBuf::from("camera\"card.img"),
            source_length: 1024,
            layout: DiskLayout {
                scheme: PartitionScheme::SuperFloppy,
                logical_sector_size: 512,
                partitions: vec![PartitionInfo {
                    index: 1,
                    start_lba: 0,
                    sector_count: 2,
                    type_id: "whole-device".into(),
                    name: None,
                }],
            },
            volumes: vec![VolumeInfo {
                partition_index: 1,
                byte_offset: 0,
                details: FileSystemDetails::Unknown,
            }],
        };

        let json = report.to_json_pretty();
        assert!(json.contains("camera\\\"card.img"));
        assert!(json.contains("\"partition_scheme\": \"super-floppy\""));
    }

    #[test]
    fn inspects_synthetic_exfat_image_end_to_end() {
        let mut image = vec![0_u8; 512 * 32];
        image[3..11].copy_from_slice(b"EXFAT   ");
        image[80..84].copy_from_slice(&24_u32.to_le_bytes());
        image[84..88].copy_from_slice(&8_u32.to_le_bytes());
        image[88..92].copy_from_slice(&32_u32.to_le_bytes());
        image[92..96].copy_from_slice(&100_u32.to_le_bytes());
        image[96..100].copy_from_slice(&2_u32.to_le_bytes());
        image[108] = 9;
        image[109] = 3;
        image[110] = 1;
        image[112] = 10;

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "tracedisk-test-{}-{unique}.img",
            std::process::id()
        ));
        fs::write(&path, image).unwrap();

        let report = inspect_image(&path).unwrap();
        let _ = fs::remove_file(&path);

        assert_eq!(report.layout.scheme, PartitionScheme::SuperFloppy);
        assert_eq!(report.volumes[0].details.name(), "exFAT");
    }
}
