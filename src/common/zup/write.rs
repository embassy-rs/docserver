use blake3;
use rand::seq::SliceRandom;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;
use zstd::bulk::Compressor;

use crate::common::file::{FileData, mmap_file};

use super::layout;

fn hash(data: &[u8]) -> [u8; 32] {
    *blake3::hash(data).as_bytes()
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

#[derive(Default)]
struct FileCache {
    hash_cache: HashMap<PathBuf, [u8; 32]>,
    file_cache: HashMap<[u8; 32], FileData>,
}

impl FileCache {
    pub fn get(&self, file_path: &Path) -> Option<(&[u8], [u8; 32])> {
        let hash = self.hash_cache.get(file_path)?;
        let buf = self.file_cache.get(hash)?;
        Some((buf.as_ref(), *hash))
    }
}

// ---------------------------------------------------------------------------
// In-memory filesystem tree – built once, queried zero times after.
// ---------------------------------------------------------------------------

enum FsNode {
    Dir {
        children: Vec<(OsString, FsNode)>,
    },
    File {
        path: PathBuf,
        metadata: fs::Metadata,
    },
}

/// Walk the filesystem once, capturing the full tree and all file metadata.
/// Directory children are sorted immediately so we never need to re-read.
fn build_tree(path: PathBuf) -> io::Result<FsNode> {
    let metadata = fs::metadata(&path)?;
    if metadata.is_dir() {
        let mut children = Vec::new();
        for entry in fs::read_dir(&path)?.flatten() {
            let file_type = entry.file_type()?;
            let name = entry.file_name();
            let child_path = entry.path();

            if file_type.is_dir() {
                let child = build_tree(child_path)?;
                children.push((name, child));
            } else if file_type.is_file() {
                let metadata = entry.metadata()?;
                children.push((
                    name,
                    FsNode::File {
                        path: child_path,
                        metadata,
                    },
                ));
            }
            // Symlinks and other types are skipped, matching original behaviour.
        }
        children.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(FsNode::Dir { children })
    } else {
        Ok(FsNode::File { path, metadata })
    }
}

/// Gather references to every file node for dictionary training.
fn collect_file_nodes<'a>(node: &'a FsNode, files: &mut Vec<&'a FsNode>) {
    match node {
        FsNode::Dir { children, .. } => {
            for (_, child) in children {
                collect_file_nodes(child, files);
            }
        }
        FsNode::File { .. } => files.push(node),
    }
}

// ---------------------------------------------------------------------------

pub fn pack(
    input_dir: &Path,
    output_path: &Path,
    compress: Option<CompressConfig>,
) -> anyhow::Result<()> {
    let f = fs::File::create(output_path)?;

    // Build the tree once. Everything below uses this in-memory structure.
    let tree = build_tree(input_dir.to_path_buf())?;

    let mut file_cache = FileCache::default();
    let mut vma_count: u32 = 0;

    let comp = match compress {
        Some(compress) => {
            println!("Creating dictionary...");

            let mut file_nodes = Vec::new();
            collect_file_nodes(&tree, &mut file_nodes);
            file_nodes.shuffle(&mut rand::rng());

            // Start grabbing files, stop when we reach dict_train_size
            let mut training_data = Vec::with_capacity(compress.dict_train_size);
            let mut training_sizes = Vec::new();
            let mut total_len = 0;

            for file_node in file_nodes {
                if total_len >= compress.dict_train_size {
                    break;
                }

                if let FsNode::File { path, metadata } = file_node {
                    let file_data = unsafe { mmap_file(path, metadata)? };
                    let file_hash = hash(file_data.as_ref());

                    match file_cache.file_cache.entry(file_hash) {
                        Entry::Occupied(_) => continue,
                        Entry::Vacant(e) => {
                            total_len += file_data.as_ref().len();
                            training_sizes.push(file_data.as_ref().len());
                            training_data.extend_from_slice(file_data.as_ref());
                            file_cache.hash_cache.insert(path.clone(), file_hash);

                            if file_data.is_mapped() {
                                vma_count += 1;
                            }

                            if file_data.is_mapped() && vma_count > 25_000 {
                                e.insert(file_data.into_vec());
                            } else {
                                e.insert(file_data);
                            }
                        }
                    }
                }
            }

            let dict = if training_data.len() < 100 {
                // If we don't have enough training data, create an empty dictionary
                Vec::new()
            } else {
                zstd::dict::from_continuous(&training_data, &training_sizes, compress.dict_size)
                    .unwrap_or_else(|e| {
                        println!(
                            "Warning: Failed to create compression dictionary: {}. Using no dictionary.",
                            e
                        );
                        Vec::new()
                    })
            };

            drop(training_data);
            drop(training_sizes);

            Some(WriterCompress::from_dict(compress.level, dict)?)
        }
        None => None,
    };

    println!("Packing...");

    let start = Instant::now();
    let mut w = Writer {
        f: BufWriter::with_capacity(256 * 1024, f),
        comp,
        offset: 0,
        hash_dedup: HashMap::with_capacity(4096),
        stats: Stats::default(),
    };

    let root = w.write_tree(&tree, &file_cache)?;

    println!("Time elapsed: {:?}", start.elapsed());
    w.print_stats();

    let file = w.finish(root)?;
    file.sync_all()?;

    Ok(())
}

struct Writer {
    f: BufWriter<fs::File>,
    hash_dedup: HashMap<[u8; 32], layout::Node>,
    offset: u64,
    comp: Option<WriterCompress>,
    stats: Stats,
}

struct WriterCompress {
    dict: Vec<u8>,
    comp: Compressor<'static>,
}

impl WriterCompress {
    pub fn from_dict(level: i32, dict: Vec<u8>) -> io::Result<Self> {
        let comp = Compressor::with_dictionary(level, &dict)?;
        Ok(Self { dict, comp })
    }

    pub fn compress(&mut self, data: &[u8]) -> Result<Vec<u8>, io::Error> {
        self.comp.compress(data)
    }
}

impl Writer {
    /// Recursively write the in-memory tree. No OS calls.
    fn write_tree(&mut self, node: &FsNode, cache: &FileCache) -> io::Result<layout::Node> {
        match node {
            FsNode::Dir { children, .. } => {
                self.stats.total_dirs += 1;

                let mut buf = Vec::with_capacity(children.len() * 48);
                for (name, child) in children {
                    let node = self.write_tree(child, cache)?;
                    let name = name.to_string_lossy().to_string();

                    buf.push(name.len().try_into().unwrap());
                    buf.extend_from_slice(name.as_bytes());
                    buf.extend_from_slice(&node.to_bytes());
                }

                let mut res = self.write_node(&buf, None)?;
                res.flags |= layout::FLAG_DIR;
                Ok(res)
            }
            FsNode::File { path, metadata } => {
                self.stats.total_files += 1;

                if let Some((buf, cached_hash)) = cache.get(path) {
                    self.write_node(buf, Some(cached_hash))
                } else {
                    let file_data = unsafe { mmap_file(path, metadata)? };
                    self.write_node(file_data.as_ref(), None)
                }
            }
        }
    }

    fn write_node(
        &mut self,
        buf: &[u8],
        cached_hash: Option<[u8; 32]>,
    ) -> io::Result<layout::Node> {
        // Track stats before dedup
        self.stats.nodes_before_dedup += 1;
        self.stats.uncompressed_bytes_before_dedup += buf.len() as u64;

        let hash = cached_hash.unwrap_or_else(|| hash(buf));
        if let Some(&res) = self.hash_dedup.get(&hash) {
            self.stats.compressed_bytes_before_dedup += res.range.len;
            return Ok(res);
        }

        // This is a new unique node
        self.stats.nodes_after_dedup += 1;
        self.stats.uncompressed_bytes_after_dedup += buf.len() as u64;

        let mut flags = 0;
        let mut owned: Option<Vec<u8>> = None;

        if buf.len() > 64
            && let Some(comp) = &mut self.comp
            && let Ok(cdata) = comp.compress(buf)
            && cdata.len() < buf.len()
        {
            owned = Some(cdata);
            flags = layout::FLAG_COMPRESSED;
        }

        let final_buf: &[u8] = owned.as_deref().unwrap_or(buf);

        self.stats.compressed_bytes_before_dedup += final_buf.len() as u64;
        self.stats.compressed_bytes_after_dedup += final_buf.len() as u64;

        let range = self.write_data(final_buf)?;
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

    fn finish(mut self, root: layout::Node) -> io::Result<fs::File> {
        let dict_range = if let Some(comp) = self.comp.take() {
            Some(self.write_data(&comp.dict)?)
        } else {
            None
        };

        let superblock = layout::Superblock {
            version: layout::VERSION,
            magic: layout::MAGIC,
            dict: dict_range,
            root,
        };

        self.f.write_all(&superblock.to_bytes())?;
        self.f.flush()?;
        Ok(self.f.into_inner()?)
    }
}
