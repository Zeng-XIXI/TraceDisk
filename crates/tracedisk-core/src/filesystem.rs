use crate::{BlockSource, DiskLayout, Result, TraceError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fat32Info {
    pub bytes_per_sector: u16,
    pub sectors_per_cluster: u8,
    pub reserved_sectors: u16,
    pub fat_count: u8,
    pub sectors_per_fat: u32,
    pub total_sectors: u32,
    pub root_cluster: u32,
    pub volume_serial: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExFatInfo {
    pub bytes_per_sector: u32,
    pub sectors_per_cluster: u32,
    pub fat_offset: u32,
    pub fat_length: u32,
    pub cluster_heap_offset: u32,
    pub cluster_count: u32,
    pub root_directory_cluster: u32,
    pub volume_serial: u32,
    pub percent_in_use: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileSystemDetails {
    Fat32(Fat32Info),
    ExFat(ExFatInfo),
    Unknown,
}

impl FileSystemDetails {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Fat32(_) => "FAT32",
            Self::ExFat(_) => "exFAT",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeInfo {
    pub partition_index: usize,
    pub byte_offset: u64,
    pub details: FileSystemDetails,
}

pub fn detect_filesystems(
    source: &dyn BlockSource,
    layout: &DiskLayout,
) -> Result<Vec<VolumeInfo>> {
    let mut volumes = Vec::with_capacity(layout.partitions.len());

    for partition in &layout.partitions {
        let byte_offset = partition
            .byte_offset(layout.logical_sector_size)
            .ok_or_else(|| TraceError::InvalidData("partition byte offset overflow".into()))?;

        let details = if byte_offset
            .checked_add(512)
            .is_some_and(|end| end <= source.len())
        {
            let sector = source.read_vec(byte_offset, 512)?;
            detect_boot_sector(&sector).unwrap_or(FileSystemDetails::Unknown)
        } else {
            FileSystemDetails::Unknown
        };

        volumes.push(VolumeInfo {
            partition_index: partition.index,
            byte_offset,
            details,
        });
    }

    Ok(volumes)
}

fn detect_boot_sector(sector: &[u8]) -> Option<FileSystemDetails> {
    if sector.get(3..11) == Some(b"EXFAT   ") {
        return parse_exfat(sector).map(FileSystemDetails::ExFat);
    }

    parse_fat32(sector).map(FileSystemDetails::Fat32)
}

fn parse_fat32(sector: &[u8]) -> Option<Fat32Info> {
    if sector.len() < 512 {
        return None;
    }

    let bytes_per_sector = u16::from_le_bytes(sector[11..13].try_into().ok()?);
    let sectors_per_cluster = sector[13];
    let reserved_sectors = u16::from_le_bytes(sector[14..16].try_into().ok()?);
    let fat_count = sector[16];
    let root_entries = u16::from_le_bytes(sector[17..19].try_into().ok()?);
    let fat16_size = u16::from_le_bytes(sector[22..24].try_into().ok()?);
    let sectors_per_fat = u32::from_le_bytes(sector[36..40].try_into().ok()?);
    let root_cluster = u32::from_le_bytes(sector[44..48].try_into().ok()?);
    let total16 = u16::from_le_bytes(sector[19..21].try_into().ok()?) as u32;
    let total32 = u32::from_le_bytes(sector[32..36].try_into().ok()?);
    let total_sectors = if total16 != 0 { total16 } else { total32 };
    let volume_serial = u32::from_le_bytes(sector[67..71].try_into().ok()?);

    if !matches!(bytes_per_sector, 512 | 1024 | 2048 | 4096)
        || !sectors_per_cluster.is_power_of_two()
        || sectors_per_cluster > 128
        || reserved_sectors == 0
        || fat_count == 0
        || root_entries != 0
        || fat16_size != 0
        || sectors_per_fat == 0
        || total_sectors == 0
        || root_cluster < 2
    {
        return None;
    }

    Some(Fat32Info {
        bytes_per_sector,
        sectors_per_cluster,
        reserved_sectors,
        fat_count,
        sectors_per_fat,
        total_sectors,
        root_cluster,
        volume_serial,
    })
}

fn parse_exfat(sector: &[u8]) -> Option<ExFatInfo> {
    if sector.len() < 512 || sector.get(3..11) != Some(b"EXFAT   ") {
        return None;
    }

    let bytes_per_sector_shift = sector[108];
    let sectors_per_cluster_shift = sector[109];
    if !(9..=12).contains(&bytes_per_sector_shift) || sectors_per_cluster_shift > 25 {
        return None;
    }

    let bytes_per_sector = 1_u32.checked_shl(bytes_per_sector_shift as u32)?;
    let sectors_per_cluster = 1_u32.checked_shl(sectors_per_cluster_shift as u32)?;
    let fat_offset = u32::from_le_bytes(sector[80..84].try_into().ok()?);
    let fat_length = u32::from_le_bytes(sector[84..88].try_into().ok()?);
    let cluster_heap_offset = u32::from_le_bytes(sector[88..92].try_into().ok()?);
    let cluster_count = u32::from_le_bytes(sector[92..96].try_into().ok()?);
    let root_directory_cluster = u32::from_le_bytes(sector[96..100].try_into().ok()?);
    let volume_serial = u32::from_le_bytes(sector[100..104].try_into().ok()?);
    let fat_count = sector[110];
    let percent_in_use = sector[112];

    if fat_offset == 0
        || fat_length == 0
        || cluster_heap_offset == 0
        || cluster_count == 0
        || root_directory_cluster < 2
        || !matches!(fat_count, 1 | 2)
        || (percent_in_use > 100 && percent_in_use != 0xff)
    {
        return None;
    }

    Some(ExFatInfo {
        bytes_per_sector,
        sectors_per_cluster,
        fat_offset,
        fat_length,
        cluster_heap_offset,
        cluster_count,
        root_directory_cluster,
        volume_serial,
        percent_in_use,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fat32_boot_sector() {
        let mut sector = [0_u8; 512];
        sector[11..13].copy_from_slice(&512_u16.to_le_bytes());
        sector[13] = 8;
        sector[14..16].copy_from_slice(&32_u16.to_le_bytes());
        sector[16] = 2;
        sector[32..36].copy_from_slice(&100_000_u32.to_le_bytes());
        sector[36..40].copy_from_slice(&100_u32.to_le_bytes());
        sector[44..48].copy_from_slice(&2_u32.to_le_bytes());
        sector[67..71].copy_from_slice(&0x1234_5678_u32.to_le_bytes());

        let details = detect_boot_sector(&sector).unwrap();
        let FileSystemDetails::Fat32(info) = details else {
            panic!("expected FAT32");
        };
        assert_eq!(info.bytes_per_sector, 512);
        assert_eq!(info.sectors_per_cluster, 8);
        assert_eq!(info.root_cluster, 2);
    }

    #[test]
    fn parses_exfat_boot_sector() {
        let mut sector = [0_u8; 512];
        sector[3..11].copy_from_slice(b"EXFAT   ");
        sector[80..84].copy_from_slice(&24_u32.to_le_bytes());
        sector[84..88].copy_from_slice(&128_u32.to_le_bytes());
        sector[88..92].copy_from_slice(&4096_u32.to_le_bytes());
        sector[92..96].copy_from_slice(&50_000_u32.to_le_bytes());
        sector[96..100].copy_from_slice(&5_u32.to_le_bytes());
        sector[100..104].copy_from_slice(&0x0102_0304_u32.to_le_bytes());
        sector[108] = 9;
        sector[109] = 7;
        sector[110] = 1;
        sector[112] = 42;

        let details = detect_boot_sector(&sector).unwrap();
        let FileSystemDetails::ExFat(info) = details else {
            panic!("expected exFAT");
        };
        assert_eq!(info.bytes_per_sector, 512);
        assert_eq!(info.sectors_per_cluster, 128);
        assert_eq!(info.percent_in_use, 42);
    }
}
