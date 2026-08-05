use crate::{Result, TraceError};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// A bounded, random-access, read-only byte source.
pub trait BlockSource: Send + Sync {
    fn len(&self) -> u64;
    fn read_exact_at(&self, offset: u64, buffer: &mut [u8]) -> Result<()>;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn read_vec(&self, offset: u64, length: usize) -> Result<Vec<u8>> {
        let mut buffer = vec![0_u8; length];
        self.read_exact_at(offset, &mut buffer)?;
        Ok(buffer)
    }
}

/// A disk image opened without write permission.
#[derive(Debug)]
pub struct ImageSource {
    path: PathBuf,
    length: u64,
    file: Mutex<File>,
}

/// A raw block or character device opened strictly read-only.
#[derive(Debug)]
pub struct RawDeviceSource {
    path: PathBuf,
    length: u64,
    block_size: u64,
    file: Mutex<File>,
}

impl RawDeviceSource {
    pub fn open(path: impl AsRef<Path>, length: u64, block_size: u64) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if length == 0 {
            return Err(TraceError::InvalidData(
                "raw device length must be greater than zero".into(),
            ));
        }
        if !(512..=1024 * 1024).contains(&block_size)
            || !block_size.is_power_of_two()
            || !length.is_multiple_of(block_size)
        {
            return Err(TraceError::InvalidData(format!(
                "invalid raw device geometry: length={length}, block_size={block_size}"
            )));
        }

        // File::open maps to read-only access; this type intentionally exposes
        // no create, truncate, or write operation.
        let file = File::open(&path)?;
        Ok(Self {
            path,
            length,
            block_size,
            file: Mutex::new(file),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl ImageSource {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = File::open(&path)?;
        let metadata = file.metadata()?;

        if !metadata.is_file() {
            return Err(TraceError::InvalidData(format!(
                "image source is not a regular file: {}",
                path.display()
            )));
        }

        Ok(Self {
            path,
            length: metadata.len(),
            file: Mutex::new(file),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl BlockSource for ImageSource {
    fn len(&self) -> u64 {
        self.length
    }

    fn read_exact_at(&self, offset: u64, buffer: &mut [u8]) -> Result<()> {
        let end = offset
            .checked_add(buffer.len() as u64)
            .ok_or(TraceError::OutOfBounds {
                offset,
                length: buffer.len(),
                source_len: self.length,
            })?;

        if end > self.length {
            return Err(TraceError::OutOfBounds {
                offset,
                length: buffer.len(),
                source_len: self.length,
            });
        }

        let mut file = self
            .file
            .lock()
            .map_err(|_| TraceError::InvalidData("image file lock was poisoned".into()))?;
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(buffer)?;
        Ok(())
    }
}

impl BlockSource for RawDeviceSource {
    fn len(&self) -> u64 {
        self.length
    }

    fn read_exact_at(&self, offset: u64, buffer: &mut [u8]) -> Result<()> {
        let end = checked_read_end(offset, buffer.len(), self.length)?;
        if end > self.length {
            return Err(TraceError::OutOfBounds {
                offset,
                length: buffer.len(),
                source_len: self.length,
            });
        }

        let mut file = self
            .file
            .lock()
            .map_err(|_| TraceError::InvalidData("raw device file lock was poisoned".into()))?;
        if buffer.is_empty() {
            return Ok(());
        }

        if offset.is_multiple_of(self.block_size)
            && (buffer.len() as u64).is_multiple_of(self.block_size)
        {
            file.seek(SeekFrom::Start(offset))?;
            file.read_exact(buffer)?;
            return Ok(());
        }

        // macOS raw character devices reject unaligned offsets and lengths
        // with EINVAL. Read the containing device blocks, then return only the
        // exact byte range requested by the filesystem parser.
        let aligned_start = offset / self.block_size * self.block_size;
        let aligned_end = end
            .div_ceil(self.block_size)
            .checked_mul(self.block_size)
            .ok_or_else(|| TraceError::InvalidData("aligned raw read overflow".into()))?;
        if aligned_end > self.length {
            return Err(TraceError::OutOfBounds {
                offset,
                length: buffer.len(),
                source_len: self.length,
            });
        }
        let aligned_length = usize::try_from(aligned_end - aligned_start)
            .map_err(|_| TraceError::InvalidData("aligned raw read is too large".into()))?;
        let mut aligned_buffer = vec![0_u8; aligned_length];
        file.seek(SeekFrom::Start(aligned_start))?;
        file.read_exact(&mut aligned_buffer)?;
        let slice_start = (offset - aligned_start) as usize;
        buffer.copy_from_slice(&aligned_buffer[slice_start..slice_start + buffer.len()]);
        Ok(())
    }
}

fn checked_read_end(offset: u64, length: usize, source_len: u64) -> Result<u64> {
    offset
        .checked_add(length as u64)
        .ok_or(TraceError::OutOfBounds {
            offset,
            length,
            source_len,
        })
}

#[cfg(test)]
mod tests {
    use super::{BlockSource, RawDeviceSource};
    use std::fs;

    #[test]
    fn raw_source_expands_unaligned_reads_to_device_blocks() {
        let path = std::env::temp_dir().join(format!(
            "tracedisk-raw-source-alignment-{}",
            std::process::id()
        ));
        let bytes = (0..1024)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        fs::write(&path, &bytes).unwrap();

        let source = RawDeviceSource::open(&path, 1024, 512).unwrap();
        assert_eq!(source.read_vec(510, 8).unwrap(), bytes[510..518]);
        assert_eq!(source.read_vec(513, 4).unwrap(), bytes[513..517]);
        drop(source);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn raw_source_rejects_invalid_device_geometry() {
        let missing = std::env::temp_dir().join("tracedisk-missing-raw-source");
        assert!(RawDeviceSource::open(&missing, 1024, 0).is_err());
        assert!(RawDeviceSource::open(&missing, 1000, 512).is_err());
    }
}
