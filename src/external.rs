//! Files stored in a data file next to the installer instead of inside it.
//!
//! The installer binary can't hold more than a few gigabytes, so sources marked
//! `source!("path", external = true)` are written by the build tool into
//! `<installer name>.data`, which ships beside the installer. They are streamed
//! from there at install time and counted in the byte progress.

use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};

use crate::embedded::{DirChild, DirChildKind, EmbeddedEntry, ExternalFile};
use crate::Installer;

/// The data file starts with this, followed by the 16 byte id of the build.
#[doc(hidden)]
pub const DATA_MAGIC: &[u8; 8] = b"IRSDATA1";
#[doc(hidden)]
pub const DATA_HEADER_LEN: u64 = 24;

const COPY_BUFFER_SIZE: usize = 1 << 20;

/// The data file an installer was built with.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ExternalData {
    pub(crate) file_name: &'static str,
    pub(crate) id: [u8; 16],
}

/// Reads through to `inner` and hashes everything read.
struct HashingReader<'h, R> {
    inner: R,
    hasher: &'h mut Sha256,
}

impl<R: Read> Read for HashingReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }
}

impl Installer {
    /// Tell the installer about its data file. Called by the generated `main`.
    #[doc(hidden)]
    pub fn set_external_data(&mut self, file_name: &'static str, id: [u8; 16]) {
        self.external_data = Some(ExternalData { file_name, id });
        let total = external_bytes_in(self.entries);
        self.bytes.lock().unwrap().1 = total;
    }

    /// Override the byte total shown by the progress, for example when only
    /// some of the external files will be installed. Defaults to the size of
    /// all of them.
    pub fn set_bytes_total(&self, total: u64) {
        self.bytes.lock().unwrap().1 = total;
        self.emit_bytes();
    }

    /// Bytes of external files installed so far and the expected total.
    pub fn bytes_progress(&self) -> (u64, u64) {
        *self.bytes.lock().unwrap()
    }

    /// Path of the data file next to the running installer.
    pub fn external_data_path(&self) -> Result<PathBuf> {
        let data = self
            .external_data
            .ok_or_else(|| anyhow!("this installer was built without external files"))?;
        let exe = std::env::current_exe().context("failed to find the installer's own path")?;
        let dir = exe
            .parent()
            .ok_or_else(|| anyhow!("the installer has no parent directory"))?;
        Ok(dir.join(data.file_name))
    }

    pub(crate) fn emit_bytes(&self) {
        if let Some(sink) = self.sink.as_ref() {
            let (done, total) = self.bytes_progress();
            sink.set_bytes(done, total);
        }
    }

    fn add_bytes(&self, count: u64) {
        self.bytes.lock().unwrap().0 += count;
        self.emit_bytes();
    }

    fn open_external_data(&self) -> Result<File> {
        let path = self.external_data_path()?;
        let mut file = File::open(&path).with_context(|| {
            format!(
                "the data file {} must be in the same folder as the installer",
                path.display()
            )
        })?;
        let mut header = [0u8; DATA_HEADER_LEN as usize];
        file.read_exact(&mut header)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let expected_id = self.external_data.map(|d| d.id).unwrap_or_default();
        if &header[..8] != DATA_MAGIC || header[8..] != expected_id {
            return Err(anyhow!(
                "{} does not belong to this installer",
                path.display()
            ));
        }
        Ok(file)
    }

    /// Stream one external file to `dest`, checking its hash on the way.
    pub(crate) fn install_external_file(&self, file: &ExternalFile, dest: &Path) -> Result<()> {
        let mut data = self.open_external_data()?;
        data.seek(SeekFrom::Start(file.offset))
            .context("failed to seek in the data file")?;

        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create parent dir for: {}", dest.display()))?;
        }
        let output =
            File::create(dest).with_context(|| format!("failed to write: {}", dest.display()))?;
        let mut output = BufWriter::with_capacity(COPY_BUFFER_SIZE, output);

        let mut hasher = Sha256::new();
        let mut stored = HashingReader {
            inner: (&mut data).take(file.stored_size),
            hasher: &mut hasher,
        };
        let mut written: u64 = 0;
        {
            let mut reader = decoder(&mut stored, file.compression)?;
            let mut buffer = vec![0u8; COPY_BUFFER_SIZE];
            loop {
                self.check_cancelled()?;
                let n = reader
                    .read(&mut buffer)
                    .context("failed to read the data file")?;
                if n == 0 {
                    break;
                }
                output
                    .write_all(&buffer[..n])
                    .with_context(|| format!("failed to write: {}", dest.display()))?;
                written += n as u64;
                self.add_bytes(n as u64);
                if file.size > 0 {
                    self.set_step_progress(written as f64 / file.size as f64);
                }
            }
        }
        // a decoder may stop before the end of its frame padding
        std::io::copy(&mut stored, &mut std::io::sink()).context("failed to read the data file")?;
        output
            .flush()
            .with_context(|| format!("failed to write: {}", dest.display()))?;

        let hash: [u8; 32] = hasher.finalize().into();
        if hash != file.sha256 || written != file.size {
            return Err(anyhow!(
                "the data file is damaged ({} did not match)",
                dest.display()
            ));
        }
        Ok(())
    }
}

fn decoder<'r, R: Read + 'r>(stored: R, compression: &str) -> Result<Box<dyn Read + 'r>> {
    match compression {
        "" | "none" => Ok(Box::new(stored)),
        #[cfg(feature = "zstd")]
        "zstd" => Ok(Box::new(
            zstd::Decoder::new(stored).context("failed to start zstd decompression")?,
        )),
        #[cfg(feature = "lzma")]
        "lzma" => Ok(Box::new(lzma_rust2::XzReader::new(stored, false))),
        #[cfg(feature = "gzip")]
        "gzip" => Ok(Box::new(flate2::read::GzDecoder::new(stored))),
        #[cfg(feature = "bzip2")]
        "bzip2" => Ok(Box::new(bzip2::read::BzDecoder::new(stored))),
        other => Err(anyhow!("unsupported compression: {other}")),
    }
}

/// Total installed size of every external file in the entries table.
fn external_bytes_in(entries: &[EmbeddedEntry]) -> u64 {
    fn in_children(children: &[DirChild]) -> u64 {
        children
            .iter()
            .map(|child| match &child.kind {
                DirChildKind::External(file) => file.size,
                DirChildKind::Dir { children } => in_children(children),
                DirChildKind::File { .. } => 0,
            })
            .sum()
    }
    entries
        .iter()
        .map(|entry| match entry {
            EmbeddedEntry::External { file, .. } => file.size,
            EmbeddedEntry::Dir { children, .. } => in_children(children),
            EmbeddedEntry::File { .. } => 0,
        })
        .sum()
}
