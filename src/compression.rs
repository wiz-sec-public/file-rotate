//! Compression - configuration and implementation
use flate2::write::GzEncoder;
use std::{
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
};

/// Compression type - algorithm + level
#[derive(Debug, Clone)]
pub enum CompressionType {
    /// Gzip compression
    Gzip(u32),

    /// Zstd compression
    #[cfg(feature = "zstd")]
    Zstd(u32),
}

impl CompressionType {
    /// suffix for the compressed file
    pub fn suffix(&self) -> &'static str {
        match self {
            CompressionType::Gzip(_) => "gz",
            #[cfg(feature = "zstd")]
            CompressionType::Zstd(_) => "zst",
        }
    }
}

/// Default compression type is similar to flate2::Compression::Default
impl Default for CompressionType {
    fn default() -> Self {
        CompressionType::Gzip(6)
    }
}

/// Compression mode - when to compress files.
#[derive(Debug, Clone)]
pub enum Compression {
    /// No compression
    None,
    /// Look for files to compress when rotating.
    /// First argument: How many files to keep uncompressed (excluding the original file)
    OnRotate {
        /// How many files to keep uncompressed (excluding the original file)
        keep_uncompressed: usize,
        /// Compression type
        compression: CompressionType,
    },
}

/// Sibling scratch file that `compress` writes into before renaming it to
/// `dest_path`: `/logs/app.123.zst` -> `/logs/.app.123.zst.compressing`.
///
/// The leading dot keeps it out of the way of everyone who might be watching
/// the directory: `Suffix::scan_suffixes` only considers names starting with
/// the base file name, and a plain `<base>.*` glob — which is how consumers
/// typically pick up rotated archives — skips dotfiles as well.
fn compressing_path(dest_path: &Path) -> PathBuf {
    let name = dest_path
        .file_name()
        .expect("dest_path.file_name()")
        .to_string_lossy();
    dest_path.with_file_name(format!(".{name}.compressing"))
}

pub(crate) fn compress(path: &Path, compression: &CompressionType) -> io::Result<PathBuf> {
    let dest_path = PathBuf::from(format!("{}.{}", path.display(), compression.suffix()));
    // Compress into a scratch file and rename it into place, so `dest_path`
    // only ever names a complete archive. Writing the destination directly
    // would let anyone scanning the directory observe — and consume, or
    // delete — a half-written file under its final name, and would leave a
    // truncated archive behind if we crashed mid-compression.
    let compressing_path = compressing_path(&dest_path);

    let mut src_file = File::open(path)?;

    // Purely diagnostic: the rename below replaces the destination wholesale,
    // so a pre-existing archive is not a correctness problem. It does mean we
    // are re-compressing a rotation we already archived once, which is worth
    // noticing.
    if let Ok(dest_md) = fs::metadata(&dest_path) {
        let src_size = fs::metadata(path).map(|m| m.len()).ok();
        let dest_mtime = dest_md
            .modified()
            .ok()
            .map(chrono::DateTime::<chrono::Utc>::from);
        tracing::warn!(
            dest = %dest_path.display(),
            dest_size = dest_md.len(),
            ?dest_mtime,
            src = %path.display(),
            src_size = ?src_size,
            "compressing over pre-existing destination file",
        );
    }

    // A leftover scratch file means a previous compression of this same
    // rotation died before the rename. Truncating it below is the recovery.
    if let Ok(md) = fs::metadata(&compressing_path) {
        tracing::warn!(
            path = %compressing_path.display(),
            size = md.len(),
            "found leftover scratch file from an interrupted compression; overwriting",
        );
    }

    let dest_file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&compressing_path)?;

    assert!(path.exists());
    assert!(compressing_path.exists());

    match compression {
        CompressionType::Gzip(level) => {
            let mut encoder = GzEncoder::new(dest_file, flate2::Compression::new(*level));
            io::copy(&mut src_file, &mut encoder)?;
            encoder.finish()?;
        }
        #[cfg(feature = "zstd")]
        CompressionType::Zstd(level) => {
            let file_size = fs::metadata(path)?.len();
            if file_size > u32::MAX as u64 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "File size is too large for zstd",
                ));
            }

            let mut encoder = zstd::stream::Encoder::new(dest_file, *level as i32)?;
            encoder.set_parameter(zstd::zstd_safe::CParameter::SrcSizeHint(file_size as u32))?;
            io::copy(&mut src_file, &mut encoder)?;
            encoder.finish()?;
        }
    }

    // Publish the finished archive atomically, then drop the source. Crashing
    // between the two leaves both variants of the rotation on disk, which
    // `Suffix::scan_suffixes` reconciles; crashing before the rename leaves the
    // source plus a scratch file, and the next rotation re-compresses it.
    fs::rename(&compressing_path, &dest_path)?;
    fs::remove_file(path)?;

    Ok(dest_path)
}
