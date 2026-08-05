use crate::{BlockSource, Result, TraceError};

pub const LOGICAL_SECTOR_SIZE: u64 = 512;
const MBR_SIGNATURE_OFFSET: usize = 510;
const MBR_PARTITION_TABLE_OFFSET: usize = 446;
const MBR_ENTRY_SIZE: usize = 16;
const MAX_GPT_ENTRIES_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartitionScheme {
    SuperFloppy,
    Mbr,
    Gpt,
}

impl PartitionScheme {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SuperFloppy => "super-floppy",
            Self::Mbr => "mbr",
            Self::Gpt => "gpt",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionInfo {
    pub index: usize,
    pub start_lba: u64,
    pub sector_count: u64,
    pub type_id: String,
    pub name: Option<String>,
}

impl PartitionInfo {
    pub fn byte_offset(&self, sector_size: u64) -> Option<u64> {
        self.start_lba.checked_mul(sector_size)
    }

    pub fn byte_length(&self, sector_size: u64) -> Option<u64> {
        self.sector_count.checked_mul(sector_size)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskLayout {
    pub scheme: PartitionScheme,
    pub logical_sector_size: u64,
    pub partitions: Vec<PartitionInfo>,
}

pub fn inspect_partitions(source: &dyn BlockSource) -> Result<DiskLayout> {
    if source.len() < LOGICAL_SECTOR_SIZE {
        return Err(TraceError::InvalidData(format!(
            "source is shorter than one logical sector: {} bytes",
            source.len()
        )));
    }

    let sector = source.read_vec(0, LOGICAL_SECTOR_SIZE as usize)?;

    if looks_like_volume_boot_sector(&sector) {
        return Ok(super_floppy_layout(source.len()));
    }

    let has_mbr_signature =
        sector[MBR_SIGNATURE_OFFSET] == 0x55 && sector[MBR_SIGNATURE_OFFSET + 1] == 0xaa;

    if !has_mbr_signature {
        return Ok(super_floppy_layout(source.len()));
    }

    let entries = parse_mbr_entries(&sector, source.len());
    if entries.iter().any(|entry| entry.type_id == "MBR:0xee") {
        return inspect_gpt(source);
    }

    if entries.is_empty() {
        return Ok(super_floppy_layout(source.len()));
    }

    Ok(DiskLayout {
        scheme: PartitionScheme::Mbr,
        logical_sector_size: LOGICAL_SECTOR_SIZE,
        partitions: entries,
    })
}

fn super_floppy_layout(source_len: u64) -> DiskLayout {
    DiskLayout {
        scheme: PartitionScheme::SuperFloppy,
        logical_sector_size: LOGICAL_SECTOR_SIZE,
        partitions: vec![PartitionInfo {
            index: 1,
            start_lba: 0,
            sector_count: source_len / LOGICAL_SECTOR_SIZE,
            type_id: "whole-device".into(),
            name: None,
        }],
    }
}

fn looks_like_volume_boot_sector(sector: &[u8]) -> bool {
    if sector.len() < 512 {
        return false;
    }

    if sector.get(3..11) == Some(b"EXFAT   ") {
        return true;
    }

    let bytes_per_sector = le_u16(sector, 11).unwrap_or(0);
    let sectors_per_cluster = sector[13];
    let reserved = le_u16(sector, 14).unwrap_or(0);
    let fat_count = sector[16];
    let root_entries = le_u16(sector, 17).unwrap_or(u16::MAX);
    let fat16_size = le_u16(sector, 22).unwrap_or(u16::MAX);
    let fat32_size = le_u32(sector, 36).unwrap_or(0);

    valid_sector_size(bytes_per_sector)
        && sectors_per_cluster.is_power_of_two()
        && reserved > 0
        && fat_count > 0
        && root_entries == 0
        && fat16_size == 0
        && fat32_size > 0
}

fn parse_mbr_entries(sector: &[u8], source_len: u64) -> Vec<PartitionInfo> {
    let disk_sectors = source_len / LOGICAL_SECTOR_SIZE;
    let mut partitions = Vec::new();

    for table_index in 0..4 {
        let offset = MBR_PARTITION_TABLE_OFFSET + table_index * MBR_ENTRY_SIZE;
        let entry = &sector[offset..offset + MBR_ENTRY_SIZE];
        let status = entry[0];
        let partition_type = entry[4];
        let start_lba = u32::from_le_bytes(entry[8..12].try_into().unwrap()) as u64;
        let sector_count = u32::from_le_bytes(entry[12..16].try_into().unwrap()) as u64;

        if !matches!(status, 0x00 | 0x80)
            || partition_type == 0
            || sector_count == 0
            || start_lba >= disk_sectors
        {
            continue;
        }

        let clamped_count = sector_count.min(disk_sectors.saturating_sub(start_lba));
        partitions.push(PartitionInfo {
            index: table_index + 1,
            start_lba,
            sector_count: clamped_count,
            type_id: format!("MBR:0x{partition_type:02x}"),
            name: None,
        });
    }

    partitions
}

fn inspect_gpt(source: &dyn BlockSource) -> Result<DiskLayout> {
    let header = source.read_vec(LOGICAL_SECTOR_SIZE, LOGICAL_SECTOR_SIZE as usize)?;
    if header.get(0..8) != Some(b"EFI PART") {
        return Err(TraceError::InvalidData(
            "protective MBR found, but GPT header is missing".into(),
        ));
    }

    let header_size = le_u32(&header, 12)? as usize;
    if !(92..=512).contains(&header_size) {
        return Err(TraceError::InvalidData(format!(
            "invalid GPT header size: {header_size}"
        )));
    }

    let entries_lba = le_u64(&header, 72)?;
    let entry_count = le_u32(&header, 80)? as u64;
    let entry_size = le_u32(&header, 84)? as u64;

    if entry_count == 0 || !(128..=4096).contains(&entry_size) {
        return Err(TraceError::InvalidData(format!(
            "invalid GPT entry geometry: count={entry_count}, size={entry_size}"
        )));
    }

    let table_bytes = entry_count
        .checked_mul(entry_size)
        .ok_or_else(|| TraceError::InvalidData("GPT entry table length overflow".into()))?;
    if table_bytes > MAX_GPT_ENTRIES_BYTES {
        return Err(TraceError::Unsupported(format!(
            "GPT entry table is larger than {} bytes",
            MAX_GPT_ENTRIES_BYTES
        )));
    }

    let table_offset = entries_lba
        .checked_mul(LOGICAL_SECTOR_SIZE)
        .ok_or_else(|| TraceError::InvalidData("GPT entry table offset overflow".into()))?;
    let table = source.read_vec(table_offset, table_bytes as usize)?;
    let disk_sectors = source.len() / LOGICAL_SECTOR_SIZE;
    let mut partitions = Vec::new();

    for table_index in 0..entry_count as usize {
        let offset = table_index * entry_size as usize;
        let entry = &table[offset..offset + entry_size as usize];
        let type_guid = &entry[0..16];
        if type_guid.iter().all(|byte| *byte == 0) {
            continue;
        }

        let first_lba = le_u64(entry, 32)?;
        let last_lba = le_u64(entry, 40)?;
        if first_lba > last_lba || first_lba >= disk_sectors {
            continue;
        }

        let last_lba = last_lba.min(disk_sectors.saturating_sub(1));
        let name_end = entry_size.min(128) as usize;
        let name = decode_utf16_name(&entry[56..name_end]);
        partitions.push(PartitionInfo {
            index: table_index + 1,
            start_lba: first_lba,
            sector_count: last_lba - first_lba + 1,
            type_id: format_guid(type_guid),
            name,
        });
    }

    Ok(DiskLayout {
        scheme: PartitionScheme::Gpt,
        logical_sector_size: LOGICAL_SECTOR_SIZE,
        partitions,
    })
}

fn decode_utf16_name(bytes: &[u8]) -> Option<String> {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .take_while(|unit| *unit != 0)
        .collect();
    if units.is_empty() {
        None
    } else {
        Some(String::from_utf16_lossy(&units))
    }
}

fn format_guid(bytes: &[u8]) -> String {
    let d1 = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
    let d2 = u16::from_le_bytes(bytes[4..6].try_into().unwrap());
    let d3 = u16::from_le_bytes(bytes[6..8].try_into().unwrap());
    format!(
        "{d1:08x}-{d2:04x}-{d3:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
    )
}

fn valid_sector_size(size: u16) -> bool {
    matches!(size, 512 | 1024 | 2048 | 4096)
}

fn le_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    let value = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| TraceError::InvalidData("unexpected end of data".into()))?;
    Ok(u16::from_le_bytes(value.try_into().unwrap()))
}

fn le_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| TraceError::InvalidData("unexpected end of data".into()))?;
    Ok(u32::from_le_bytes(value.try_into().unwrap()))
}

fn le_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    let value = bytes
        .get(offset..offset + 8)
        .ok_or_else(|| TraceError::InvalidData("unexpected end of data".into()))?;
    Ok(u64::from_le_bytes(value.try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct MemorySource(Arc<Vec<u8>>);

    impl BlockSource for MemorySource {
        fn len(&self) -> u64 {
            self.0.len() as u64
        }

        fn read_exact_at(&self, offset: u64, buffer: &mut [u8]) -> Result<()> {
            let start = offset as usize;
            let end = start + buffer.len();
            buffer.copy_from_slice(&self.0[start..end]);
            Ok(())
        }
    }

    #[test]
    fn detects_mbr_partition() {
        let mut image = vec![0_u8; 512 * 128];
        image[510] = 0x55;
        image[511] = 0xaa;
        let entry = &mut image[446..462];
        entry[4] = 0x0c;
        entry[8..12].copy_from_slice(&1_u32.to_le_bytes());
        entry[12..16].copy_from_slice(&100_u32.to_le_bytes());

        let layout = inspect_partitions(&MemorySource(Arc::new(image))).unwrap();
        assert_eq!(layout.scheme, PartitionScheme::Mbr);
        assert_eq!(layout.partitions[0].start_lba, 1);
        assert_eq!(layout.partitions[0].sector_count, 100);
    }

    #[test]
    fn detects_super_floppy_exfat() {
        let mut image = vec![0_u8; 512 * 16];
        image[3..11].copy_from_slice(b"EXFAT   ");

        let layout = inspect_partitions(&MemorySource(Arc::new(image))).unwrap();
        assert_eq!(layout.scheme, PartitionScheme::SuperFloppy);
        assert_eq!(layout.partitions[0].start_lba, 0);
    }

    #[test]
    fn detects_gpt_partition_and_name() {
        let mut image = vec![0_u8; 512 * 128];

        image[510] = 0x55;
        image[511] = 0xaa;
        let protective_entry = &mut image[446..462];
        protective_entry[4] = 0xee;
        protective_entry[8..12].copy_from_slice(&1_u32.to_le_bytes());
        protective_entry[12..16].copy_from_slice(&127_u32.to_le_bytes());

        let header = &mut image[512..1024];
        header[0..8].copy_from_slice(b"EFI PART");
        header[12..16].copy_from_slice(&92_u32.to_le_bytes());
        header[72..80].copy_from_slice(&2_u64.to_le_bytes());
        header[80..84].copy_from_slice(&4_u32.to_le_bytes());
        header[84..88].copy_from_slice(&128_u32.to_le_bytes());

        let entry = &mut image[1024..1152];
        entry[0] = 0xa2;
        entry[32..40].copy_from_slice(&34_u64.to_le_bytes());
        entry[40..48].copy_from_slice(&100_u64.to_le_bytes());
        for (index, unit) in "DJI".encode_utf16().enumerate() {
            let offset = 56 + index * 2;
            entry[offset..offset + 2].copy_from_slice(&unit.to_le_bytes());
        }

        let layout = inspect_partitions(&MemorySource(Arc::new(image))).unwrap();
        assert_eq!(layout.scheme, PartitionScheme::Gpt);
        assert_eq!(layout.partitions.len(), 1);
        assert_eq!(layout.partitions[0].start_lba, 34);
        assert_eq!(layout.partitions[0].sector_count, 67);
        assert_eq!(layout.partitions[0].name.as_deref(), Some("DJI"));
    }
}
