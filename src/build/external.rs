//! Writes the data file that holds `source!(..., external = true)` files.

use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use installrs::__private::{DATA_HEADER_LEN, DATA_MAGIC};

const ZSTD_LEVEL: i32 = 3;
/// A file is stored uncompressed when a sample of it doesn't shrink below this.
const WORTH_COMPRESSING: f64 = 0.97;
const SAMPLE_SIZE: usize = 8 << 20;

/// Where one file ended up in the data file.
#[derive(Debug, Clone)]
pub struct ExternalInfo {
    pub offset: u64,
    pub stored_size: u64,
    pub size: u64,
    pub compression: &'static str,
    pub sha256: [u8; 32],
}

pub struct DataWriter {
    final_path: PathBuf,
    partial_path: PathBuf,
    output: BufWriter<File>,
    offset: u64,
    /// Identifies the build, so an installer refuses another build's data file.
    id_hasher: Sha256,
}

/// Counts and hashes what passes through to `inner`.
struct HashingWriter<'w, W: Write> {
    inner: &'w mut W,
    hasher: Sha256,
    written: u64,
}

impl<W: Write> Write for HashingWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.written += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl DataWriter {
    pub fn create(final_path: &Path) -> Result<DataWriter> {
        let partial_path = final_path.with_extension("data.partial");
        let file = File::create(&partial_path)
            .with_context(|| format!("failed to create {}", partial_path.display()))?;
        let mut output = BufWriter::with_capacity(4 << 20, file);
        output.write_all(&[0u8; DATA_HEADER_LEN as usize])?;
        Ok(DataWriter {
            final_path: final_path.to_path_buf(),
            partial_path,
            output,
            offset: DATA_HEADER_LEN,
            id_hasher: Sha256::new(),
        })
    }

    pub fn file_name(&self) -> String {
        self.final_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Append one file, compressed when that makes it noticeably smaller.
    pub fn add(&mut self, source: &Path) -> Result<ExternalInfo> {
        let size = std::fs::metadata(source)
            .with_context(|| format!("failed to stat: {}", source.display()))?
            .len();
        let offset = self.offset;

        let mut info = None;
        if size > 0 && worth_compressing(source)? {
            let compressed = self.write_zstd(source, offset, size)?;
            if (compressed.stored_size as f64) < size as f64 * WORTH_COMPRESSING {
                info = Some(compressed);
            } else {
                self.output.flush()?;
                self.output.get_mut().set_len(offset)?;
                self.output.seek(SeekFrom::Start(offset))?;
            }
        }
        let info = match info {
            Some(info) => info,
            None => self.write_raw(source, offset, size)?,
        };

        self.offset = offset + info.stored_size;
        self.id_hasher.update(info.sha256);
        self.id_hasher.update(info.size.to_le_bytes());
        log::debug!(
            "Stored {} ({} bytes as {} {})",
            source.display(),
            info.size,
            info.stored_size,
            info.compression
        );
        Ok(info)
    }

    fn write_raw(&mut self, source: &Path, offset: u64, size: u64) -> Result<ExternalInfo> {
        let mut input =
            File::open(source).with_context(|| format!("failed to read: {}", source.display()))?;
        let mut writer = HashingWriter {
            inner: &mut self.output,
            hasher: Sha256::new(),
            written: 0,
        };
        std::io::copy(&mut input, &mut writer)
            .with_context(|| format!("failed to store: {}", source.display()))?;
        Ok(ExternalInfo {
            offset,
            stored_size: writer.written,
            size,
            compression: "none",
            sha256: writer.hasher.finalize().into(),
        })
    }

    fn write_zstd(&mut self, source: &Path, offset: u64, size: u64) -> Result<ExternalInfo> {
        let mut input =
            File::open(source).with_context(|| format!("failed to read: {}", source.display()))?;
        let mut writer = HashingWriter {
            inner: &mut self.output,
            hasher: Sha256::new(),
            written: 0,
        };
        {
            let mut encoder = zstd::stream::write::Encoder::new(&mut writer, ZSTD_LEVEL)?;
            let workers = std::thread::available_parallelism().map_or(1, |n| n.get()) as u32;
            encoder.multithread(workers)?;
            std::io::copy(&mut input, &mut encoder)
                .with_context(|| format!("failed to compress: {}", source.display()))?;
            encoder.finish()?;
        }
        Ok(ExternalInfo {
            offset,
            stored_size: writer.written,
            size,
            compression: "zstd",
            sha256: writer.hasher.finalize().into(),
        })
    }

    /// Write the build id into the header and move the file into place.
    pub fn finish(mut self) -> Result<[u8; 16]> {
        let mut id = [0u8; 16];
        id.copy_from_slice(&self.id_hasher.finalize()[..16]);
        self.output.seek(SeekFrom::Start(0))?;
        self.output.write_all(DATA_MAGIC)?;
        self.output.write_all(&id)?;
        self.output.flush()?;
        drop(self.output);
        if self.final_path.exists() {
            std::fs::remove_file(&self.final_path)
                .with_context(|| format!("failed to replace {}", self.final_path.display()))?;
        }
        std::fs::rename(&self.partial_path, &self.final_path)
            .with_context(|| format!("failed to write {}", self.final_path.display()))?;
        log::info!(
            "Data file ready: {} ({} bytes)",
            self.final_path.display(),
            self.offset
        );
        Ok(id)
    }
}

/// Compress the start of the file to see whether the whole is worth it.
fn worth_compressing(source: &Path) -> Result<bool> {
    let mut sample = Vec::with_capacity(SAMPLE_SIZE);
    File::open(source)
        .with_context(|| format!("failed to read: {}", source.display()))?
        .take(SAMPLE_SIZE as u64)
        .read_to_end(&mut sample)?;
    let compressed = zstd::bulk::compress(&sample, ZSTD_LEVEL)?;
    Ok((compressed.len() as f64) < sample.len() as f64 * WORTH_COMPRESSING)
}
