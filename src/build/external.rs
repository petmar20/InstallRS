//! Writes the data file that holds `source!(..., external = true)` files.
//!
//! An index of what the data file holds is written next to it. When a later build stores the same
//! files, unchanged in size and modification time and in the same order, the data file is reused
//! as it is, so only the installer itself is built again.

use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};

use installrs::__private::{DATA_HEADER_LEN, DATA_MAGIC};

const ZSTD_LEVEL: i32 = 3;
/// A file is stored uncompressed when a sample of it doesn't shrink below this.
const WORTH_COMPRESSING: f64 = 0.97;
const SAMPLE_SIZE: usize = 8 << 20;
/// The first word of an index file and its format version.
const INDEX_HEADER: &str = "installrs-data-index\t1";

/// Where one file ended up in the data file.
#[derive(Debug, Clone)]
pub struct ExternalInfo {
    pub offset: u64,
    pub stored_size: u64,
    pub size: u64,
    pub compression: &'static str,
    pub sha256: [u8; 32],
}

/// One stored file as the index records it.
#[derive(Debug, Clone)]
struct IndexEntry {
    path: String,
    /// Modification time in nanoseconds since 1970.
    modified: u128,
    info: ExternalInfo,
}

pub struct DataWriter {
    final_path: PathBuf,
    partial_path: PathBuf,
    /// None while the files match the previous data file, which is then reused.
    output: Option<BufWriter<File>>,
    offset: u64,
    /// Identifies the build, so an installer refuses another build's data file.
    id_hasher: Sha256,
    /// What the previous data file holds, empty when there is none to reuse.
    previous: Vec<IndexEntry>,
    /// How many files in a row matched the previous data file.
    reused: usize,
    entries: Vec<IndexEntry>,
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
        let previous = read_index(final_path).unwrap_or_else(|error| {
            log::debug!("Not reusing {}: {error:#}", final_path.display());
            Vec::new()
        });
        let mut writer = DataWriter {
            final_path: final_path.to_path_buf(),
            partial_path: final_path.with_extension("data.partial"),
            output: None,
            offset: DATA_HEADER_LEN,
            id_hasher: Sha256::new(),
            previous,
            reused: 0,
            entries: Vec::new(),
        };
        if writer.previous.is_empty() {
            writer.start_writing()?;
        }
        Ok(writer)
    }

    pub fn file_name(&self) -> String {
        self.final_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Append one file, compressed when that makes it noticeably smaller.
    pub fn add(&mut self, source: &Path) -> Result<ExternalInfo> {
        let metadata = std::fs::metadata(source)
            .with_context(|| format!("failed to stat: {}", source.display()))?;
        let size = metadata.len();
        let modified = modified_nanos(&metadata);
        let path = source.to_string_lossy().into_owned();

        if self.output.is_none() {
            let same_as_before = self
                .previous
                .get(self.reused)
                .filter(|entry| {
                    entry.path == path && entry.modified == modified && entry.info.size == size
                })
                .map(|entry| entry.info.clone());
            if let Some(info) = same_as_before {
                self.reused += 1;
                log::debug!("Reused {} from the previous data file", source.display());
                self.record(path, modified, info.clone());
                return Ok(info);
            }
            self.start_writing()?;
        }

        let offset = self.offset;
        let mut info = None;
        if size > 0 && worth_compressing(source)? {
            let compressed = self.write_zstd(source, offset, size)?;
            if (compressed.stored_size as f64) < size as f64 * WORTH_COMPRESSING {
                info = Some(compressed);
            } else {
                let output = self.output_mut();
                output.flush()?;
                output.get_mut().set_len(offset)?;
                output.seek(SeekFrom::Start(offset))?;
            }
        }
        let info = match info {
            Some(info) => info,
            None => self.write_raw(source, offset, size)?,
        };
        log::debug!(
            "Stored {} ({} bytes as {} {})",
            source.display(),
            info.size,
            info.stored_size,
            info.compression
        );
        self.record(path, modified, info.clone());
        Ok(info)
    }

    fn record(&mut self, path: String, modified: u128, info: ExternalInfo) {
        self.offset = info.offset + info.stored_size;
        self.id_hasher.update(info.sha256);
        self.id_hasher.update(info.size.to_le_bytes());
        self.entries.push(IndexEntry {
            path,
            modified,
            info,
        });
    }

    /// Starts a new data file, copying the part of the previous one the files so far matched.
    fn start_writing(&mut self) -> Result<()> {
        if self.reused > 0 {
            log::info!(
                "The stored files changed after the first {}, writing {} again",
                self.reused,
                self.final_path.display()
            );
        }
        let _ = std::fs::remove_file(index_path(&self.final_path));
        let file = File::create(&self.partial_path)
            .with_context(|| format!("failed to create {}", self.partial_path.display()))?;
        let mut output = BufWriter::with_capacity(4 << 20, file);
        output.write_all(&[0u8; DATA_HEADER_LEN as usize])?;
        if self.offset > DATA_HEADER_LEN {
            let mut previous = File::open(&self.final_path)
                .with_context(|| format!("failed to read {}", self.final_path.display()))?;
            previous.seek(SeekFrom::Start(DATA_HEADER_LEN))?;
            let mut matched = previous.take(self.offset - DATA_HEADER_LEN);
            std::io::copy(&mut matched, &mut output)
                .with_context(|| format!("failed to copy from {}", self.final_path.display()))?;
        }
        self.output = Some(output);
        self.previous.clear();
        Ok(())
    }

    fn output_mut(&mut self) -> &mut BufWriter<File> {
        self.output
            .as_mut()
            .expect("the data file is being written")
    }

    fn write_raw(&mut self, source: &Path, offset: u64, size: u64) -> Result<ExternalInfo> {
        let mut input =
            File::open(source).with_context(|| format!("failed to read: {}", source.display()))?;
        let mut writer = HashingWriter {
            inner: self.output_mut(),
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
            inner: self.output_mut(),
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

    /// Write the build id into the header and move the file into place, or keep the previous
    /// data file when every file matched it.
    pub fn finish(mut self) -> Result<[u8; 16]> {
        let mut id = [0u8; 16];
        id.copy_from_slice(&self.id_hasher.clone().finalize()[..16]);
        if self.output.is_none() {
            if self.reused == self.previous.len() {
                log::info!("Data file unchanged, reusing {}", self.final_path.display());
                return Ok(id);
            }
            // files were left out since last time, so the data file is written without them
            self.start_writing()?;
        }

        let mut output = self.output.take().expect("the data file is being written");
        output.seek(SeekFrom::Start(0))?;
        output.write_all(DATA_MAGIC)?;
        output.write_all(&id)?;
        output.flush()?;
        drop(output);
        if self.final_path.exists() {
            std::fs::remove_file(&self.final_path)
                .with_context(|| format!("failed to replace {}", self.final_path.display()))?;
        }
        std::fs::rename(&self.partial_path, &self.final_path)
            .with_context(|| format!("failed to write {}", self.final_path.display()))?;
        write_index(&self.final_path, self.offset, &self.entries)?;
        log::info!(
            "Data file ready: {} ({} bytes)",
            self.final_path.display(),
            self.offset
        );
        Ok(id)
    }
}

fn index_path(data_path: &Path) -> PathBuf {
    let mut name = data_path.as_os_str().to_owned();
    name.push(".index");
    PathBuf::from(name)
}

fn modified_nanos(metadata: &std::fs::Metadata) -> u128 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_nanos())
}

/// One line per stored file: offset, stored size, size, compression, SHA-256, modified, path.
fn write_index(data_path: &Path, length: u64, entries: &[IndexEntry]) -> Result<()> {
    let mut text = format!("{INDEX_HEADER}\t{length}\n");
    for entry in entries {
        let info = &entry.info;
        text.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            info.offset,
            info.stored_size,
            info.size,
            info.compression,
            to_hex(&info.sha256),
            entry.modified,
            entry.path
        ));
    }
    let path = index_path(data_path);
    std::fs::write(&path, text).with_context(|| format!("failed to write {}", path.display()))
}

/// The entries of the index next to the data file, when both are there and agree.
fn read_index(data_path: &Path) -> Result<Vec<IndexEntry>> {
    let text = std::fs::read_to_string(index_path(data_path)).context("no index")?;
    let mut lines = text.lines();
    let header = lines.next().unwrap_or_default();
    let length = header
        .strip_prefix(INDEX_HEADER)
        .and_then(|rest| rest.trim().parse::<u64>().ok())
        .ok_or_else(|| anyhow!("unknown index format"))?;
    let actual_length = std::fs::metadata(data_path).context("no data file")?.len();
    if actual_length != length {
        return Err(anyhow!(
            "the data file is {actual_length} bytes, the index says {length}"
        ));
    }
    let mut entries = Vec::new();
    for line in lines {
        let fields: Vec<&str> = line.splitn(7, '\t').collect();
        let [offset, stored_size, size, compression, sha256, modified, path] = fields[..] else {
            return Err(anyhow!("bad index line: {line}"));
        };
        entries.push(IndexEntry {
            path: path.to_string(),
            modified: modified.parse()?,
            info: ExternalInfo {
                offset: offset.parse()?,
                stored_size: stored_size.parse()?,
                size: size.parse()?,
                compression: match compression {
                    "zstd" => "zstd",
                    "none" => "none",
                    other => return Err(anyhow!("unknown compression {other}")),
                },
                sha256: from_hex(sha256)?,
            },
        });
    }
    Ok(entries)
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn from_hex(text: &str) -> Result<[u8; 32]> {
    let mut bytes = [0u8; 32];
    if text.len() != 64 {
        return Err(anyhow!("bad hash {text}"));
    }
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16)?;
    }
    Ok(bytes)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn write_data(dir: &Path, files: &[&Path]) -> (PathBuf, [u8; 16]) {
        let data = dir.join("out.data");
        let mut writer = DataWriter::create(&data).unwrap();
        for file in files {
            writer.add(file).unwrap();
        }
        (data.clone(), writer.finish().unwrap())
    }

    #[test]
    fn unchanged_files_reuse_the_data_file() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("a.txt");
        let second = dir.path().join("b.bin");
        std::fs::write(&first, "hello ".repeat(1000)).unwrap();
        std::fs::write(&second, [7u8; 5000]).unwrap();

        let (data, id) = write_data(dir.path(), &[&first, &second]);
        let written = std::fs::metadata(&data).unwrap().modified().unwrap();
        let (_, reused_id) = write_data(dir.path(), &[&first, &second]);
        assert_eq!(id, reused_id);
        assert_eq!(
            written,
            std::fs::metadata(&data).unwrap().modified().unwrap()
        );

        std::fs::write(&second, [8u8; 6000]).unwrap();
        let (_, changed_id) = write_data(dir.path(), &[&first, &second]);
        assert_ne!(id, changed_id);
        let fresh = tempfile::tempdir().unwrap();
        let fresh_first = fresh.path().join("a.txt");
        let fresh_second = fresh.path().join("b.bin");
        std::fs::copy(&first, &fresh_first).unwrap();
        std::fs::copy(&second, &fresh_second).unwrap();
        let (fresh_data, fresh_id) = write_data(fresh.path(), &[&fresh_first, &fresh_second]);
        assert_eq!(changed_id, fresh_id);
        assert_eq!(
            std::fs::read(&data).unwrap(),
            std::fs::read(&fresh_data).unwrap()
        );
    }
}
