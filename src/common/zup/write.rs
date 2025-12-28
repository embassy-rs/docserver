use rand::seq::SliceRandom;
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::collections::HashMap;
use std::fs::{self};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::layout;

fn hash(data: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(data);
    hash.finalize().into()
}

pub struct CompressConfig {
    pub level: i32,
    pub dict_size: usize,
    pub dict_train_size: usize,
}

#[derive(Default)]
struct Stats {
    total_files: u64,
    total_dirs: u64,
    nodes_before_dedup: u64,
    nodes_after_dedup: u64,
    uncompressed_bytes_before_dedup: u64,
    uncompressed_bytes_after_dedup: u64,
    compressed_bytes_before_dedup: u64,
    compressed_bytes_after_dedup: u64,
}

pub fn pack(
    input_dir: &Path,
    output_path: &Path,
    compress: Option<CompressConfig>,
) -> io::Result<()> {
    let f = fs::File::create(output_path)?;

    let comp = match compress {
        Some(compress) => {
            println!("Creating dictionary...");

            // Recursively list all file paths under input_dir
            let mut file_paths: Vec<PathBuf> = Vec::new();
            let mut stack = vec![input_dir.to_path_buf()];
            while let Some(current_path) = stack.pop() {
                if let Ok(entries) = fs::read_dir(&current_path) {
                    for entry in entries.flatten() {
                        let entry_path = entry.path();
                        if entry_path.is_dir() {
                            stack.push(entry_path);
                        } else if entry_path.is_file() {
                            file_paths.push(entry_path);
                        }
                    }
                }
            }

            // Shuffle them
            file_paths.shuffle(&mut rand::rng());

            // Start grabbing files, stop when we reach dict_train_size
            let mut training_data = Vec::new();
            let mut total_len = 0;

            for file_path in file_paths {
                if total_len >= compress.dict_train_size {
                    break;
                }

                let file_data = fs::read(&file_path)?;
                total_len += file_data.len();
                training_data.push(file_data);
            }

            let training_files: Vec<_> = training_data.iter().map(|f| f.as_slice()).collect();

            let dict = if training_files.is_empty()
                || training_data.iter().map(|f| f.len()).sum::<usize>() < 100
            {
                // If we don't have enough training data, create an empty dictionary
                Vec::new()
            } else {
                zstd::dict::from_samples(&training_files, compress.dict_size)
                    .unwrap_or_else(|e| {
                        println!("Warning: Failed to create compression dictionary: {}. Using no dictionary.", e);
                        Vec::new()
                    })
            };

            Some(WriterCompress {
                dict,
                level: compress.level,
            })
        }
        None => None,
    };

    // Write stuff
    println!("Packing...");
    let mut w = Writer {
        f,
        comp,
        offset: 0,
        hash_dedup: HashMap::new(),
        stats: Stats::default(),
    };

    let root = w.write(input_dir)?;
    w.print_stats();
    w.finish(root)?;

    Ok(())
}

struct Writer {
    f: fs::File,
    hash_dedup: HashMap<[u8; 32], layout::Node>,
    offset: u64,
    comp: Option<WriterCompress>,
    stats: Stats,
}

struct WriterCompress {
    dict: Vec<u8>,
    level: i32,
}

impl Writer {
    fn write(&mut self, path: &Path) -> io::Result<layout::Node> {
        let m = fs::metadata(&path)?;
        if m.is_dir() {
            self.stats.total_dirs += 1;

            let mut readdir: Vec<_> = fs::read_dir(&path)?.try_collect()?;
            readdir.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

            let mut buf = Vec::new();
            for entry in readdir {
                let node = self.write(&entry.path())?;

                let name = entry.file_name().to_string_lossy().to_string();
                buf.push(name.len().try_into().unwrap());
                buf.extend_from_slice(name.as_bytes());
                buf.extend_from_slice(&node.to_bytes());
            }

            let mut res = self.write_node(&buf)?;
            res.flags |= layout::FLAG_DIR;
            Ok(res)
        } else {
            self.stats.total_files += 1;

            let buf = fs::read(path)?;
            let res = self.write_node(&buf)?;
            Ok(res)
        }
    }

    fn write_node(&mut self, buf: impl AsRef<[u8]>) -> io::Result<layout::Node> {
        let mut buf: Cow<[u8]> = Cow::Borrowed(buf.as_ref());
        // Track stats before dedup
        self.stats.nodes_before_dedup += 1;
        self.stats.uncompressed_bytes_before_dedup += buf.len() as u64;

        let hash = hash(&buf);
        if let Some(res) = self.hash_dedup.get(&hash) {
            self.stats.compressed_bytes_before_dedup += res.range.len;
            return Ok(*res);
        }

        // This is a new unique node
        self.stats.nodes_after_dedup += 1;
        self.stats.uncompressed_bytes_after_dedup += buf.len() as u64;

        let mut flags = 0;
        if let Some(comp) = &mut self.comp {
            if let Ok(mut compressor) =
                zstd::bulk::Compressor::with_dictionary(comp.level, &comp.dict)
            {
                if let Ok(cdata) = compressor.compress(&buf) {
                    if cdata.len() < buf.len() {
                        buf = cdata.into();
                        flags = layout::FLAG_COMPRESSED;
                    }
                }
            }
        }

        self.stats.compressed_bytes_before_dedup += buf.len() as u64;
        self.stats.compressed_bytes_after_dedup += buf.len() as u64;

        let range = self.write_data(&buf)?;
        let node = layout::Node { range, flags };
        self.hash_dedup.insert(hash, node);
        Ok(node)
    }

    fn write_data(&mut self, buf: &[u8]) -> io::Result<layout::Range> {
        self.f.write_all(buf)?;
        let res = layout::Range {
            offset: self.offset,
            len: buf.len() as _,
        };
        self.offset += res.len;
        Ok(res)
    }

    fn print_stats(&self) {
        let compression_enabled = self.comp.is_some();

        println!("Statistics:");
        println!("  Files: {}", self.stats.total_files);
        println!("  Directories: {}", self.stats.total_dirs);
        println!(
            "  Total entries: {}",
            self.stats.total_files + self.stats.total_dirs
        );
        println!("  Nodes before dedup: {}", self.stats.nodes_before_dedup);
        println!(
            "        after dedup:  {} ({:.1}% reduction)",
            self.stats.nodes_after_dedup,
            100.0 * (self.stats.nodes_before_dedup - self.stats.nodes_after_dedup) as f64
                / self.stats.nodes_before_dedup as f64
        );
        println!(
            "  Uncompressed bytes before dedup: {} ({:.1} MB)",
            self.stats.uncompressed_bytes_before_dedup,
            self.stats.uncompressed_bytes_before_dedup as f64 / 1_000_000.0
        );
        println!(
            "                     after dedup:  {} ({:.1} MB, {:.1}% reduction)",
            self.stats.uncompressed_bytes_after_dedup,
            self.stats.uncompressed_bytes_after_dedup as f64 / 1_000_000.0,
            100.0
                * (self.stats.uncompressed_bytes_before_dedup
                    - self.stats.uncompressed_bytes_after_dedup) as f64
                / self.stats.uncompressed_bytes_before_dedup as f64
        );

        if compression_enabled {
            println!(
                "  Compressed bytes   before dedup: {} ({:.1} MB)",
                self.stats.compressed_bytes_before_dedup,
                self.stats.compressed_bytes_before_dedup as f64 / 1_000_000.0
            );
            println!(
                "                     after dedup:  {} ({:.1} MB, {:.1}% reduction)",
                self.stats.compressed_bytes_after_dedup,
                self.stats.compressed_bytes_after_dedup as f64 / 1_000_000.0,
                100.0
                    * (self.stats.compressed_bytes_before_dedup
                        - self.stats.compressed_bytes_after_dedup) as f64
                    / self.stats.compressed_bytes_before_dedup as f64
            );
            if self.stats.uncompressed_bytes_after_dedup > 0 {
                println!(
                    "  Overall compression ratio: {:.1}%",
                    100.0 * self.stats.compressed_bytes_after_dedup as f64
                        / self.stats.uncompressed_bytes_before_dedup as f64
                );
            }
        }
    }

    fn finish(mut self, root: layout::Node) -> io::Result<()> {
        let dict_range = match &self.comp {
            Some(comp) => Some(self.write_data(&comp.dict.clone())?),
            None => None,
        };

        let superblock = layout::Superblock {
            version: layout::VERSION,
            magic: layout::MAGIC,
            dict: dict_range,
            root,
        };

        self.f.write_all(&superblock.to_bytes())?;
        self.f.sync_all()?;
        Ok(())
    }
}
