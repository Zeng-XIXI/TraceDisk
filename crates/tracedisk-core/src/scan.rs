use crate::{
    detect_filesystems, inspect_partitions, BlockSource, ExFatInfo, FileSystemDetails, Result,
    TraceError,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

const DIRECTORY_LIMIT: u64 = 64 * 1024 * 1024;
const DEEP_READ_BATCH: u64 = 8 * 1024 * 1024;
const MAX_DEEP_CANDIDATES: usize = 4096;
const MAX_FAT_CACHE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_RECOVERY_EXTENTS: usize = 65_536;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecoveryExtent {
    pub byte_offset: u64,
    pub length: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VideoCandidate {
    pub id: u64,
    pub name: String,
    pub original_path: Option<String>,
    pub extension: String,
    pub byte_offset: u64,
    pub size_bytes: u64,
    pub start_cluster: Option<u32>,
    pub contiguous: bool,
    pub extents: Vec<RecoveryExtent>,
    pub fat_chain_status: String,
    pub free_cluster_ratio: f64,
    pub recoverability: String,
    pub source: String,
    pub has_mdat: bool,
    pub has_moov: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScanReport {
    pub mode: String,
    pub filesystem: String,
    pub source_length: u64,
    pub bytes_examined: u64,
    pub candidates: Vec<VideoCandidate>,
    pub warnings: Vec<String>,
    #[serde(default)]
    pub cancelled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScanProgress {
    pub bytes_examined: u64,
    pub total_bytes: u64,
    pub candidates_found: usize,
}

pub fn scan_deleted_videos(source: &dyn BlockSource) -> Result<ScanReport> {
    let layout = inspect_partitions(source)?;
    let volumes = detect_filesystems(source, &layout)?;
    let mut report = ScanReport {
        mode: "metadata".into(),
        filesystem: "unknown".into(),
        source_length: source.len(),
        bytes_examined: 0,
        candidates: Vec::new(),
        warnings: Vec::new(),
        cancelled: false,
    };

    for volume in volumes {
        let Some(partition) = layout
            .partitions
            .iter()
            .find(|partition| partition.index == volume.partition_index)
        else {
            continue;
        };

        match volume.details {
            FileSystemDetails::ExFat(info) => {
                report.filesystem = "exFAT".into();
                let partition_offset = partition
                    .byte_offset(layout.logical_sector_size)
                    .ok_or_else(|| TraceError::InvalidData("partition offset overflow".into()))?;
                let scanner = ExFatScanner::new(source, partition_offset, info)?;
                scanner.scan_metadata(&mut report)?;
            }
            FileSystemDetails::Fat32(_) => {
                report.filesystem = "FAT32".into();
                report.warnings.push(
                    "FAT32 deleted-directory scanning is not implemented in this milestone; use deep scan."
                        .into(),
                );
            }
            FileSystemDetails::Unknown => {}
        }
    }

    if report.filesystem == "unknown" {
        report
            .warnings
            .push("No supported FAT32 or exFAT volume was detected.".into());
    }
    assign_candidate_ids(&mut report.candidates);
    Ok(report)
}

pub fn deep_scan_videos(source: &dyn BlockSource) -> Result<ScanReport> {
    deep_scan_videos_with_progress(source, |_| true)
}

pub fn deep_scan_videos_with_progress<F>(
    source: &dyn BlockSource,
    mut on_progress: F,
) -> Result<ScanReport>
where
    F: FnMut(&ScanProgress) -> bool,
{
    let layout = inspect_partitions(source)?;
    let volumes = detect_filesystems(source, &layout)?;
    let mut report = ScanReport {
        mode: "deep".into(),
        filesystem: "unknown".into(),
        source_length: source.len(),
        bytes_examined: 0,
        candidates: Vec::new(),
        warnings: Vec::new(),
        cancelled: false,
    };
    let mut offsets = HashSet::new();

    for volume in volumes {
        match volume.details {
            FileSystemDetails::ExFat(_) => {
                report.filesystem = "exFAT".into();
            }
            FileSystemDetails::Fat32(_) => {
                report.filesystem = "FAT32".into();
            }
            FileSystemDetails::Unknown => {}
        }
    }

    if report.filesystem == "unknown" {
        report
            .warnings
            .push("No supported FAT32 or exFAT volume was detected.".into());
    }
    report.warnings.push(
        "Deep scan streamed across the complete raw device; no disk image was created.".into(),
    );
    report.warnings.push(
        "Full-device carving can also find videos that are still present on the card; carved candidates do not prove deletion by themselves."
            .into(),
    );
    let completed = scan_byte_range(
        source,
        0,
        source.len(),
        &mut report,
        &mut offsets,
        &mut on_progress,
    )?;
    if !completed {
        report.cancelled = true;
        report.warnings.push(
            "Deep scan was stopped by the user; candidates found so far were preserved.".into(),
        );
    }
    assign_candidate_ids(&mut report.candidates);
    Ok(report)
}

fn assign_candidate_ids(candidates: &mut [VideoCandidate]) {
    for (index, candidate) in candidates.iter_mut().enumerate() {
        candidate.id = index as u64 + 1;
    }
}

struct ExFatScanner<'a> {
    source: &'a dyn BlockSource,
    partition_offset: u64,
    info: ExFatInfo,
    bytes_per_sector: u64,
    cluster_size: u64,
    fat_cache: Option<Vec<u8>>,
}

impl<'a> ExFatScanner<'a> {
    fn new(source: &'a dyn BlockSource, partition_offset: u64, info: ExFatInfo) -> Result<Self> {
        let bytes_per_sector = info.bytes_per_sector as u64;
        let cluster_size = bytes_per_sector
            .checked_mul(info.sectors_per_cluster as u64)
            .ok_or_else(|| TraceError::InvalidData("exFAT cluster size overflow".into()))?;
        if cluster_size == 0 || cluster_size > 32 * 1024 * 1024 {
            return Err(TraceError::InvalidData(format!(
                "invalid exFAT cluster size: {cluster_size}"
            )));
        }
        let fat_length = (info.fat_length as u64)
            .checked_mul(bytes_per_sector)
            .ok_or_else(|| TraceError::InvalidData("exFAT FAT length overflow".into()))?;
        let fat_offset = partition_offset
            .checked_add(info.fat_offset as u64 * bytes_per_sector)
            .ok_or_else(|| TraceError::InvalidData("exFAT FAT offset overflow".into()))?;
        if fat_length == 0
            || fat_offset
                .checked_add(fat_length)
                .is_none_or(|end| end > source.len())
        {
            return Err(TraceError::InvalidData(
                "exFAT FAT is outside the source bounds".into(),
            ));
        }
        let fat_cache = if fat_length <= MAX_FAT_CACHE_BYTES {
            let length = usize::try_from(fat_length)
                .map_err(|_| TraceError::InvalidData("exFAT FAT is too large to cache".into()))?;
            Some(source.read_vec(fat_offset, length)?)
        } else {
            None
        };
        Ok(Self {
            source,
            partition_offset,
            info,
            bytes_per_sector,
            cluster_size,
            fat_cache,
        })
    }

    fn scan_metadata(&self, report: &mut ScanReport) -> Result<()> {
        report.bytes_examined = report
            .bytes_examined
            .saturating_add(self.fat_cache.as_ref().map_or(0, |fat| fat.len() as u64));
        let root = self.read_directory_chain(self.info.root_directory_cluster, None, false)?;
        report.bytes_examined = report.bytes_examined.saturating_add(root.len() as u64);
        let bitmap = self.read_allocation_bitmap(&root, report)?;
        let mut visited = HashSet::new();
        self.scan_directory(
            self.info.root_directory_cluster,
            root,
            String::new(),
            &bitmap,
            &mut visited,
            report,
        )?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn scan_directory(
        &self,
        directory_cluster: u32,
        data: Vec<u8>,
        parent_path: String,
        bitmap: &Option<AllocationBitmap>,
        visited: &mut HashSet<u32>,
        report: &mut ScanReport,
    ) -> Result<()> {
        if !visited.insert(directory_cluster) {
            return Ok(());
        }

        let mut offset = 0_usize;
        while offset + 32 <= data.len() {
            let entry_type = data[offset];
            if entry_type == 0 {
                break;
            }

            if entry_type & 0x7f != 0x05 {
                offset += 32;
                continue;
            }

            let secondary_count = data[offset + 1] as usize;
            let set_length = (secondary_count + 1).saturating_mul(32);
            if secondary_count == 0 || secondary_count > 18 || offset + set_length > data.len() {
                offset += 32;
                continue;
            }

            let set = &data[offset..offset + set_length];
            let active = entry_type & 0x80 != 0;
            if let Some(parsed) = parse_file_entry_set(set) {
                let original_path = if parent_path.is_empty() {
                    parsed.name.clone()
                } else {
                    format!("{parent_path}/{}", parsed.name)
                };

                if active && parsed.is_directory && self.is_valid_cluster(parsed.first_cluster) {
                    let child = self.read_directory_chain(
                        parsed.first_cluster,
                        Some(parsed.data_length),
                        parsed.contiguous,
                    )?;
                    report.bytes_examined =
                        report.bytes_examined.saturating_add(child.len() as u64);
                    self.scan_directory(
                        parsed.first_cluster,
                        child,
                        original_path,
                        bitmap,
                        visited,
                        report,
                    )?;
                } else if !active
                    && !parsed.is_directory
                    && is_video_name(&parsed.name)
                    && self.is_valid_cluster(parsed.first_cluster)
                {
                    let layout = self.recovery_layout(
                        parsed.first_cluster,
                        parsed.data_length,
                        parsed.contiguous,
                    )?;
                    let byte_offset = self.cluster_offset(parsed.first_cluster)?;
                    let free_ratio = bitmap.as_ref().map_or(0.0, |bitmap| {
                        bitmap.free_ratio_for_clusters(&layout.clusters)
                    });
                    let recoverability = if layout.extents.is_empty() {
                        "needs-deep-scan"
                    } else if free_ratio >= 0.999 {
                        "high"
                    } else if free_ratio > 0.0 {
                        "partially-overwritten"
                    } else {
                        "overwritten-or-reallocated"
                    };
                    let extension = file_extension(&parsed.name)
                        .unwrap_or("MP4")
                        .to_ascii_uppercase();
                    report.candidates.push(VideoCandidate {
                        id: 0,
                        name: parsed.name,
                        original_path: Some(original_path),
                        extension,
                        byte_offset,
                        size_bytes: parsed.data_length,
                        start_cluster: Some(parsed.first_cluster),
                        contiguous: parsed.contiguous,
                        extents: layout.extents,
                        fat_chain_status: layout.fat_chain_status.into(),
                        free_cluster_ratio: free_ratio,
                        recoverability: recoverability.into(),
                        source: "deleted-directory-entry".into(),
                        has_mdat: false,
                        has_moov: false,
                    });
                }
            }
            offset += set_length;
        }
        Ok(())
    }

    fn read_allocation_bitmap(
        &self,
        root: &[u8],
        report: &mut ScanReport,
    ) -> Result<Option<AllocationBitmap>> {
        for entry in root.chunks_exact(32) {
            if entry[0] == 0 {
                break;
            }
            if entry[0] & 0x7f != 0x01 || entry[0] & 0x80 == 0 {
                continue;
            }

            let first_cluster = le_u32(entry, 20)?;
            let data_length = le_u64(entry, 24)?;
            if first_cluster < 2 || data_length == 0 || data_length > DIRECTORY_LIMIT {
                continue;
            }
            let bytes = self.read_contiguous(first_cluster, data_length)?;
            report.bytes_examined = report.bytes_examined.saturating_add(bytes.len() as u64);
            return Ok(Some(AllocationBitmap { bytes }));
        }
        report
            .warnings
            .push("exFAT allocation bitmap entry was not found in the root directory.".into());
        Ok(None)
    }

    fn read_directory_chain(
        &self,
        first_cluster: u32,
        data_length: Option<u64>,
        contiguous: bool,
    ) -> Result<Vec<u8>> {
        if first_cluster < 2 {
            return Err(TraceError::InvalidData(format!(
                "invalid directory cluster: {first_cluster}"
            )));
        }
        if contiguous {
            return self.read_contiguous(
                first_cluster,
                data_length
                    .unwrap_or(self.cluster_size)
                    .min(DIRECTORY_LIMIT),
            );
        }

        let target_length = data_length.unwrap_or(DIRECTORY_LIMIT).min(DIRECTORY_LIMIT);
        let mut output = Vec::new();
        let mut cluster = first_cluster;
        let mut visited = HashSet::new();

        while (output.len() as u64) < target_length && visited.insert(cluster) {
            let bytes = self
                .source
                .read_vec(self.cluster_offset(cluster)?, self.cluster_size as usize)?;
            let has_end_marker = bytes.chunks_exact(32).any(|entry| entry[0] == 0);
            output.extend_from_slice(&bytes);
            if has_end_marker || data_length.is_some_and(|length| output.len() as u64 >= length) {
                break;
            }

            let next = self.fat_entry(cluster)?;
            if !(2..0xfffffff8).contains(&next) || next >= self.info.cluster_count + 2 {
                break;
            }
            cluster = next;
        }
        output.truncate(target_length.min(output.len() as u64) as usize);
        Ok(output)
    }

    fn read_contiguous(&self, first_cluster: u32, data_length: u64) -> Result<Vec<u8>> {
        let length = data_length.min(DIRECTORY_LIMIT);
        self.source
            .read_vec(self.cluster_offset(first_cluster)?, length as usize)
    }

    fn cluster_offset(&self, cluster: u32) -> Result<u64> {
        if !self.is_valid_cluster(cluster) {
            return Err(TraceError::InvalidData(format!(
                "exFAT cluster is outside the heap: {cluster}"
            )));
        }
        let heap_offset = self
            .partition_offset
            .checked_add(self.info.cluster_heap_offset as u64 * self.bytes_per_sector)
            .ok_or_else(|| TraceError::InvalidData("cluster heap offset overflow".into()))?;
        heap_offset
            .checked_add((cluster as u64 - 2) * self.cluster_size)
            .ok_or_else(|| TraceError::InvalidData("cluster byte offset overflow".into()))
    }

    fn is_valid_cluster(&self, cluster: u32) -> bool {
        (2..self.info.cluster_count.saturating_add(2)).contains(&cluster)
    }

    fn fat_entry(&self, cluster: u32) -> Result<u32> {
        let entry_offset = cluster as usize * 4;
        if let Some(fat) = &self.fat_cache {
            let bytes = fat.get(entry_offset..entry_offset + 4).ok_or_else(|| {
                TraceError::InvalidData(format!("exFAT FAT entry is missing: {cluster}"))
            })?;
            return Ok(u32::from_le_bytes(bytes.try_into().unwrap()));
        }
        let fat_offset = self
            .partition_offset
            .checked_add(self.info.fat_offset as u64 * self.bytes_per_sector)
            .and_then(|offset| offset.checked_add(cluster as u64 * 4))
            .ok_or_else(|| TraceError::InvalidData("FAT entry offset overflow".into()))?;
        let bytes = self.source.read_vec(fat_offset, 4)?;
        Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
    }

    fn recovery_layout(
        &self,
        first_cluster: u32,
        data_length: u64,
        contiguous: bool,
    ) -> Result<CandidateLayout> {
        let cluster_count = data_length.div_ceil(self.cluster_size);
        if cluster_count == 0 || cluster_count > self.info.cluster_count as u64 {
            return Ok(CandidateLayout::broken());
        }

        let mut clusters = Vec::with_capacity(cluster_count.min(1_048_576) as usize);
        if contiguous {
            let last_cluster = first_cluster
                .checked_add(cluster_count as u32 - 1)
                .filter(|cluster| self.is_valid_cluster(*cluster));
            if last_cluster.is_none() {
                return Ok(CandidateLayout::broken());
            }
            clusters.extend((0..cluster_count).map(|index| first_cluster + index as u32));
            let extents = self.coalesce_extents(&clusters, data_length)?;
            if extents.is_empty() {
                return Ok(CandidateLayout::broken());
            }
            return Ok(CandidateLayout {
                clusters,
                extents,
                fat_chain_status: "not-required",
            });
        }

        let mut current = first_cluster;
        let mut visited = HashSet::new();
        for index in 0..cluster_count {
            if !self.is_valid_cluster(current) || !visited.insert(current) {
                return Ok(CandidateLayout::broken());
            }
            clusters.push(current);
            if index + 1 < cluster_count {
                let next = self.fat_entry(current)?;
                if !self.is_valid_cluster(next) {
                    return Ok(CandidateLayout::broken());
                }
                current = next;
            }
        }
        if self.fat_entry(current)? < 0xffff_fff8 {
            return Ok(CandidateLayout::broken());
        }
        let extents = self.coalesce_extents(&clusters, data_length)?;
        if extents.is_empty() || extents.len() > MAX_RECOVERY_EXTENTS {
            return Ok(CandidateLayout::broken());
        }
        Ok(CandidateLayout {
            clusters,
            extents,
            fat_chain_status: "complete",
        })
    }

    fn coalesce_extents(&self, clusters: &[u32], data_length: u64) -> Result<Vec<RecoveryExtent>> {
        let mut extents: Vec<RecoveryExtent> = Vec::new();
        let mut remaining = data_length;
        for cluster in clusters {
            if remaining == 0 {
                break;
            }
            let byte_offset = self.cluster_offset(*cluster)?;
            let length = remaining.min(self.cluster_size);
            if byte_offset
                .checked_add(length)
                .is_none_or(|end| end > self.source.len())
            {
                return Ok(Vec::new());
            }
            if let Some(previous) = extents.last_mut() {
                if previous.byte_offset.checked_add(previous.length) == Some(byte_offset) {
                    previous.length = previous.length.checked_add(length).ok_or_else(|| {
                        TraceError::InvalidData("recovery extent length overflow".into())
                    })?;
                    remaining -= length;
                    continue;
                }
            }
            extents.push(RecoveryExtent {
                byte_offset,
                length,
            });
            remaining -= length;
        }
        if remaining == 0 {
            Ok(extents)
        } else {
            Ok(Vec::new())
        }
    }
}

struct CandidateLayout {
    clusters: Vec<u32>,
    extents: Vec<RecoveryExtent>,
    fat_chain_status: &'static str,
}

impl CandidateLayout {
    fn broken() -> Self {
        Self {
            clusters: Vec::new(),
            extents: Vec::new(),
            fat_chain_status: "broken",
        }
    }
}

#[derive(Debug)]
struct ParsedFileEntry {
    name: String,
    first_cluster: u32,
    data_length: u64,
    contiguous: bool,
    is_directory: bool,
}

fn parse_file_entry_set(set: &[u8]) -> Option<ParsedFileEntry> {
    let primary = set.get(0..32)?;
    let attributes = u16::from_le_bytes(primary[4..6].try_into().ok()?);
    let stream = set
        .chunks_exact(32)
        .skip(1)
        .find(|entry| entry[0] & 0x7f == 0x40)?;
    let name_length = stream[3] as usize;
    let first_cluster = u32::from_le_bytes(stream[20..24].try_into().ok()?);
    let data_length = u64::from_le_bytes(stream[24..32].try_into().ok()?);
    if first_cluster < 2 || data_length == 0 || name_length == 0 {
        return None;
    }

    let mut units = Vec::with_capacity(name_length);
    for name_entry in set
        .chunks_exact(32)
        .skip(1)
        .filter(|entry| entry[0] & 0x7f == 0x41)
    {
        for pair in name_entry[2..32].chunks_exact(2) {
            if units.len() == name_length {
                break;
            }
            units.push(u16::from_le_bytes([pair[0], pair[1]]));
        }
    }
    if units.len() != name_length {
        return None;
    }

    Some(ParsedFileEntry {
        name: String::from_utf16_lossy(&units),
        first_cluster,
        data_length,
        contiguous: stream[1] & 0x02 != 0,
        is_directory: attributes & 0x10 != 0,
    })
}

struct AllocationBitmap {
    bytes: Vec<u8>,
}

impl AllocationBitmap {
    fn is_allocated(&self, cluster: u32) -> bool {
        let Some(index) = cluster.checked_sub(2).map(|value| value as usize) else {
            return true;
        };
        self.bytes
            .get(index / 8)
            .is_none_or(|byte| byte & (1 << (index % 8)) != 0)
    }

    fn free_ratio_for_clusters(&self, clusters: &[u32]) -> f64 {
        if clusters.is_empty() {
            return 0.0;
        }
        let free = clusters
            .iter()
            .filter(|cluster| !self.is_allocated(**cluster))
            .count();
        free as f64 / clusters.len() as f64
    }
}

fn scan_byte_range<F>(
    source: &dyn BlockSource,
    offset: u64,
    length: u64,
    report: &mut ScanReport,
    offsets: &mut HashSet<u64>,
    on_progress: &mut F,
) -> Result<bool>
where
    F: FnMut(&ScanProgress) -> bool,
{
    let available = source.len().saturating_sub(offset);
    let mut cursor = 0_u64;
    let length = length.min(available);
    if !on_progress(&ScanProgress {
        bytes_examined: report.bytes_examined,
        total_bytes: length,
        candidates_found: report.candidates.len(),
    }) {
        return Ok(false);
    }
    while cursor < length && report.candidates.len() < MAX_DEEP_CANDIDATES {
        let overlap = if cursor == 0 { 0 } else { 15 };
        let absolute = offset + cursor - overlap;
        let remaining = length - cursor + overlap;
        let chunk_length = remaining.min(DEEP_READ_BATCH + overlap) as usize;
        let bytes = source.read_vec(absolute, chunk_length)?;
        let newly_examined = (chunk_length as u64).saturating_sub(overlap);
        report.bytes_examined = report.bytes_examined.saturating_add(newly_examined);
        scan_chunk_for_ftyp(source, &bytes, absolute, report, offsets)?;
        cursor += newly_examined;
        if !on_progress(&ScanProgress {
            bytes_examined: report.bytes_examined,
            total_bytes: length,
            candidates_found: report.candidates.len(),
        }) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn scan_chunk_for_ftyp(
    source: &dyn BlockSource,
    bytes: &[u8],
    absolute_offset: u64,
    report: &mut ScanReport,
    offsets: &mut HashSet<u64>,
) -> Result<()> {
    if bytes.len() < 16 {
        return Ok(());
    }
    for index in 0..=bytes.len() - 16 {
        if &bytes[index + 4..index + 8] != b"ftyp" {
            continue;
        }
        let box_size = u32::from_be_bytes(bytes[index..index + 4].try_into().unwrap()) as u64;
        let candidate_offset = absolute_offset + index as u64;
        if !(16..=1024 * 1024).contains(&box_size)
            || box_size > source.len().saturating_sub(candidate_offset)
        {
            continue;
        }
        let brand = &bytes[index + 8..index + 12];
        if !brand.iter().all(|byte| byte.is_ascii_graphic()) {
            continue;
        }
        if !offsets.insert(candidate_offset) {
            continue;
        }
        let Some(probe) = probe_mp4_extent(source, candidate_offset)? else {
            continue;
        };
        report.candidates.push(VideoCandidate {
            id: 0,
            name: format!("CARVED_{candidate_offset:016x}.MP4"),
            original_path: None,
            extension: "MP4".into(),
            byte_offset: candidate_offset,
            size_bytes: probe.size,
            start_cluster: None,
            contiguous: true,
            extents: vec![RecoveryExtent {
                byte_offset: candidate_offset,
                length: probe.size,
            }],
            fat_chain_status: "not-applicable".into(),
            free_cluster_ratio: 1.0,
            recoverability: if probe.has_moov {
                "container-complete".into()
            } else {
                "container-repair-needed".into()
            },
            source: "mp4-carving".into(),
            has_mdat: probe.has_mdat,
            has_moov: probe.has_moov,
        });
        if report.candidates.len() == MAX_DEEP_CANDIDATES {
            report.warnings.push(format!(
                "Candidate limit of {MAX_DEEP_CANDIDATES} reached; the scan stopped early."
            ));
            break;
        }
    }
    Ok(())
}

struct Mp4Probe {
    size: u64,
    has_mdat: bool,
    has_moov: bool,
}

fn probe_mp4_extent(source: &dyn BlockSource, start: u64) -> Result<Option<Mp4Probe>> {
    let mut cursor = start;
    let mut has_mdat = false;
    let mut has_moov = false;

    for box_index in 0..128 {
        if cursor.saturating_add(16) > source.len() {
            break;
        }
        let header = source.read_vec(cursor, 16)?;
        let size32 = u32::from_be_bytes(header[0..4].try_into().unwrap()) as u64;
        let box_type: [u8; 4] = header[4..8].try_into().unwrap();
        if box_index == 0 && &box_type != b"ftyp" {
            return Ok(None);
        }
        if !is_top_level_mp4_box(&box_type) {
            break;
        }

        let (box_size, header_size) = if size32 == 1 {
            (u64::from_be_bytes(header[8..16].try_into().unwrap()), 16)
        } else if size32 == 0 {
            break;
        } else {
            (size32, 8)
        };
        if box_size < header_size || box_size > source.len().saturating_sub(cursor) {
            break;
        }

        has_mdat |= &box_type == b"mdat";
        has_moov |= &box_type == b"moov";
        cursor += box_size;
        if has_mdat && has_moov {
            return Ok(Some(Mp4Probe {
                size: cursor - start,
                has_mdat,
                has_moov,
            }));
        }
    }

    if has_mdat {
        Ok(Some(Mp4Probe {
            size: cursor.saturating_sub(start),
            has_mdat,
            has_moov,
        }))
    } else {
        Ok(None)
    }
}

fn is_top_level_mp4_box(box_type: &[u8; 4]) -> bool {
    matches!(
        box_type,
        b"ftyp"
            | b"free"
            | b"skip"
            | b"wide"
            | b"mdat"
            | b"moov"
            | b"uuid"
            | b"meta"
            | b"moof"
            | b"mfra"
            | b"sidx"
            | b"styp"
    )
}

fn is_video_name(name: &str) -> bool {
    file_extension(name).is_some_and(|extension| {
        extension.eq_ignore_ascii_case("MP4")
            || extension.eq_ignore_ascii_case("MOV")
            || extension.eq_ignore_ascii_case("LRV")
    })
}

fn file_extension(name: &str) -> Option<&str> {
    name.rsplit_once('.')
        .map(|(_, extension)| extension)
        .filter(|extension| {
            extension.eq_ignore_ascii_case("MP4")
                || extension.eq_ignore_ascii_case("MOV")
                || extension.eq_ignore_ascii_case("LRV")
        })
}

fn le_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let slice = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| TraceError::InvalidData("unexpected end of exFAT entry".into()))?;
    Ok(u32::from_le_bytes(slice.try_into().unwrap()))
}

fn le_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    let slice = bytes
        .get(offset..offset + 8)
        .ok_or_else(|| TraceError::InvalidData("unexpected end of exFAT entry".into()))?;
    Ok(u64::from_le_bytes(slice.try_into().unwrap()))
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
    fn finds_deleted_exfat_video_without_deep_scan() {
        let image = synthetic_exfat_image();
        let report = scan_deleted_videos(&MemorySource(Arc::new(image))).unwrap();
        assert_eq!(report.filesystem, "exFAT");
        assert_eq!(report.candidates.len(), 1);
        let candidate = &report.candidates[0];
        assert_eq!(candidate.name, "DJI_0001.MP4");
        assert_eq!(
            candidate.original_path.as_deref(),
            Some("DCIM/DJI_0001.MP4")
        );
        assert_eq!(candidate.size_bytes, 1024);
        assert_eq!(candidate.recoverability, "high");
        assert_eq!(candidate.free_cluster_ratio, 1.0);
        assert_eq!(candidate.fat_chain_status, "not-required");
        assert_eq!(
            candidate.extents,
            vec![RecoveryExtent {
                byte_offset: 35 * 512,
                length: 1024,
            }]
        );
    }

    #[test]
    fn resolves_and_coalesces_a_complete_fragmented_exfat_chain() {
        let mut image = synthetic_exfat_image();
        let fat = 24 * 512;
        image[fat + 5 * 4..fat + 6 * 4].copy_from_slice(&7_u32.to_le_bytes());
        image[fat + 7 * 4..fat + 8 * 4].copy_from_slice(&8_u32.to_le_bytes());
        image[fat + 8 * 4..fat + 9 * 4].copy_from_slice(&0xffff_ffff_u32.to_le_bytes());
        let dcim = 34 * 512;
        image[dcim + 33] = 0x01;
        image[dcim + 40..dcim + 48].copy_from_slice(&1400_u64.to_le_bytes());
        image[dcim + 56..dcim + 64].copy_from_slice(&1400_u64.to_le_bytes());

        let report = scan_deleted_videos(&MemorySource(Arc::new(image))).unwrap();
        let candidate = &report.candidates[0];
        assert!(!candidate.contiguous);
        assert_eq!(candidate.fat_chain_status, "complete");
        assert_eq!(candidate.recoverability, "high");
        assert_eq!(
            candidate.extents,
            vec![
                RecoveryExtent {
                    byte_offset: 35 * 512,
                    length: 512,
                },
                RecoveryExtent {
                    byte_offset: 37 * 512,
                    length: 888,
                },
            ]
        );
    }

    #[test]
    fn marks_a_broken_fragmented_exfat_chain_for_deep_scan() {
        let mut image = synthetic_exfat_image();
        let dcim = 34 * 512;
        image[dcim + 33] = 0x01;

        let report = scan_deleted_videos(&MemorySource(Arc::new(image))).unwrap();
        let candidate = &report.candidates[0];
        assert!(!candidate.contiguous);
        assert_eq!(candidate.fat_chain_status, "broken");
        assert_eq!(candidate.recoverability, "needs-deep-scan");
        assert!(candidate.extents.is_empty());
    }

    #[test]
    fn carves_mp4_while_streaming_the_complete_source() {
        let image = synthetic_exfat_image();
        let image_length = image.len() as u64;
        let report = deep_scan_videos(&MemorySource(Arc::new(image))).unwrap();
        assert_eq!(report.candidates.len(), 1);
        assert_eq!(report.bytes_examined, image_length);
        let candidate = &report.candidates[0];
        assert!(candidate.has_mdat);
        assert!(candidate.has_moov);
        assert_eq!(candidate.size_bytes, 56);
        assert_eq!(candidate.recoverability, "container-complete");
        assert_eq!(candidate.extents.len(), 1);
    }

    #[test]
    fn deep_scan_can_stop_after_the_current_read_batch() {
        let mut image = synthetic_exfat_image();
        image.resize(DEEP_READ_BATCH as usize + 4096, 0);
        let report = deep_scan_videos_with_progress(&MemorySource(Arc::new(image)), |progress| {
            progress.bytes_examined < DEEP_READ_BATCH
        })
        .unwrap();
        assert!(report.cancelled);
        assert_eq!(report.bytes_examined, DEEP_READ_BATCH);
        assert!(report.bytes_examined < report.source_length);
        assert_eq!(report.candidates.len(), 1);
    }

    fn synthetic_exfat_image() -> Vec<u8> {
        let mut image = vec![0_u8; 512 * 96];
        image[3..11].copy_from_slice(b"EXFAT   ");
        image[80..84].copy_from_slice(&24_u32.to_le_bytes());
        image[84..88].copy_from_slice(&1_u32.to_le_bytes());
        image[88..92].copy_from_slice(&32_u32.to_le_bytes());
        image[92..96].copy_from_slice(&64_u32.to_le_bytes());
        image[96..100].copy_from_slice(&2_u32.to_le_bytes());
        image[108] = 9;
        image[109] = 0;
        image[110] = 1;
        image[112] = 5;

        let fat = 24 * 512;
        image[fat + 2 * 4..fat + 3 * 4].copy_from_slice(&0xffff_ffff_u32.to_le_bytes());

        let root = 32 * 512;
        image[root] = 0x81;
        image[root + 20..root + 24].copy_from_slice(&3_u32.to_le_bytes());
        image[root + 24..root + 32].copy_from_slice(&8_u64.to_le_bytes());
        write_file_set(
            &mut image[root + 32..root + 128],
            true,
            true,
            "DCIM",
            4,
            512,
        );

        let bitmap = 33 * 512;
        image[bitmap] = 0b0000_0111;

        let dcim = 34 * 512;
        write_file_set(
            &mut image[dcim..dcim + 96],
            false,
            false,
            "DJI_0001.MP4",
            5,
            1024,
        );

        let video = 35 * 512;
        image[video..video + 4].copy_from_slice(&24_u32.to_be_bytes());
        image[video + 4..video + 8].copy_from_slice(b"ftyp");
        image[video + 8..video + 12].copy_from_slice(b"isom");
        image[video + 12..video + 16].copy_from_slice(&0_u32.to_be_bytes());
        image[video + 16..video + 20].copy_from_slice(b"isom");
        image[video + 20..video + 24].copy_from_slice(b"mp42");
        image[video + 24..video + 28].copy_from_slice(&24_u32.to_be_bytes());
        image[video + 28..video + 32].copy_from_slice(b"mdat");
        image[video + 48..video + 52].copy_from_slice(&8_u32.to_be_bytes());
        image[video + 52..video + 56].copy_from_slice(b"moov");
        image
    }

    fn write_file_set(
        target: &mut [u8],
        active: bool,
        directory: bool,
        name: &str,
        first_cluster: u32,
        data_length: u64,
    ) {
        let active_bit = if active { 0x80 } else { 0 };
        target[0] = active_bit | 0x05;
        target[1] = 2;
        let attributes = if directory { 0x10_u16 } else { 0x20_u16 };
        target[4..6].copy_from_slice(&attributes.to_le_bytes());

        target[32] = active_bit | 0x40;
        target[33] = 0x03;
        target[35] = name.encode_utf16().count() as u8;
        target[40..48].copy_from_slice(&data_length.to_le_bytes());
        target[52..56].copy_from_slice(&first_cluster.to_le_bytes());
        target[56..64].copy_from_slice(&data_length.to_le_bytes());

        target[64] = active_bit | 0x41;
        for (index, unit) in name.encode_utf16().enumerate() {
            let offset = 66 + index * 2;
            target[offset..offset + 2].copy_from_slice(&unit.to_le_bytes());
        }
    }
}
