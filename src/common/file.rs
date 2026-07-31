//! File access utils

use std::fs::Metadata;
use std::path::Path;
use std::{fs, io};

/// Represents file data
pub enum FileData {
    Vec(Vec<u8>),
    Mmap(memmap2::Mmap),
}

impl AsRef<[u8]> for FileData {
    fn as_ref(&self) -> &[u8] {
        match self {
            FileData::Vec(v) => v.as_slice(),
            FileData::Mmap(m) => m.as_ref(),
        }
    }
}

// SAFETY: mmap is safe as long as the file is not truncated concurrently.
// For a packer reading a static tree this is a reasonable assumption.
pub unsafe fn mmap_file(path: &Path, meta: &Metadata) -> io::Result<FileData> {
    if meta.len() > 4096
        && let Ok(file) = fs::File::open(path)
        && let Ok(mmap) = unsafe { memmap2::Mmap::map(&file) }
    {
        #[cfg(unix)]
        let _ = mmap.advise(memmap2::Advice::Sequential);

        Ok(FileData::Mmap(mmap))
    } else {
        Ok(FileData::Vec(fs::read(path)?))
    }
}
