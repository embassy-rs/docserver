use blake3;
use rand::seq::SliceRandom;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use zstd::bulk::Compressor;
use zstd::dict::EncoderDictionary;

use crate::common::blocking_map::BlockingSlotMap;
use crate::common::file::{FileData, mmap_file};

use super::layout;

type Hash = [u8; 32];

fn hash(data: &[u8]) -> Hash {
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
    hash_cache: HashMap<PathBuf, Hash>,
    file_cache: HashMap<Hash, FileData>,
}

impl FileCache {
    pub fn get(&self, file_path: &Path) -> Option<(&[u8], Hash)> {
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

impl FsNode {
    pub fn files(&self) -> Vec<(PathBuf, fs::Metadata)> {
        fn add_nodes(files: &mut Vec<(PathBuf, fs::Metadata)>, node: &FsNode) {
            match node {
                FsNode::Dir { children } => {
                    for (_, child) in children {
                        add_nodes(files, child);
                    }
                }
                FsNode::File { path, metadata } => files.push((path.clone(), metadata.clone())),
            }
        }

        let mut files = Vec::new();

        add_nodes(&mut files, self);

        files.shrink_to_fit();
        files
    }
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

// ---------------------------------------------------------------------------
// Result produced by worker threads and consumed by the Writer.
// ---------------------------------------------------------------------------

enum FileResult {
    Node {
        node: layout::Node,
        file_len: usize,
    },
    Data {
        hash: Hash,
        data: Vec<u8>,
        file_len: usize,
        is_compressed: bool,
    },
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
    let file_nodes = tree.files();

    let mut file_cache = FileCache::default();
    let mut vma_count: u32 = 0;

    let comp = match compress {
        Some(compress) => {
            println!("Creating dictionary...");

            let mut file_nodes = file_nodes.clone();
            file_nodes.shuffle(&mut rand::rng());

            // Start grabbing files, stop when we reach dict_train_size
            let mut training_data = Vec::with_capacity(compress.dict_train_size);
            let mut training_sizes = Vec::new();
            let mut total_len = 0;

            for (path, metadata) in file_nodes {
                if total_len >= compress.dict_train_size {
                    break;
                }

                let file_data = unsafe { mmap_file(&path, &metadata)? };
                let file_hash = hash(file_data.as_ref());

                file_cache.hash_cache.insert(path.clone(), file_hash);
                let Entry::Vacant(e) = file_cache.file_cache.entry(file_hash) else {
                    continue;
                };

                total_len += file_data.as_ref().len();
                training_sizes.push(file_data.as_ref().len());
                training_data.extend_from_slice(file_data.as_ref());

                if file_data.is_mapped() {
                    vma_count += 1;
                }

                if file_data.is_mapped() && vma_count > 25_000 {
                    e.insert(file_data.into_vec());
                } else {
                    e.insert(file_data);
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

    // -----------------------------------------------------------------------
    // Parallel packing: worker threads read & compress, main thread writes
    // -----------------------------------------------------------------------

    let n_slots = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .max(1);

    // Collect file nodes in the exact order the Writer will visit them.
    // Both FsNode::files and write_tree perform a depth-first traversal
    // with children sorted by name, so the file order is identical.

    let work_items = Arc::new(file_nodes);
    let node_index = Arc::new(AtomicUsize::new(0));

    // Shared state
    let hash_dedup: Arc<Mutex<HashMap<Hash, layout::Node>>> =
        Arc::new(Mutex::new(HashMap::with_capacity(4096)));
    let file_cache = Arc::new(file_cache);
    let blocking_map = Arc::new(BlockingSlotMap::<PathBuf, _>::new(n_slots));

    // Spawn worker threads
    let mut handles = Vec::with_capacity(n_slots);
    for slot in 0..n_slots {
        let work_items = Arc::clone(&work_items);
        let node_index = Arc::clone(&node_index);
        let hash_dedup = Arc::clone(&hash_dedup);
        let file_cache = Arc::clone(&file_cache);
        let blocking_map = Arc::clone(&blocking_map);
        let mut comp = comp.clone();

        let handle = std::thread::spawn(move || {
            loop {
                let node_index = node_index.fetch_add(1, Ordering::Relaxed);
                let Some((path, metadata)) = work_items.get(node_index) else {
                    break;
                };

                let mut process_file = |buf: &[u8], file_hash, path: &Path| {
                    let file_len = buf.len();

                    // Check global dedup table; needs separate statement to avoid deadlock
                    let node = hash_dedup.lock().unwrap().get(&file_hash).copied();
                    if let Some(node) = node {
                        blocking_map.insert(
                            slot,
                            path.to_path_buf(),
                            FileResult::Node { node, file_len },
                        );
                    } else {
                        let (data, is_compressed) = if file_len > 64
                            && let Some(ref mut c) = comp
                            && let Ok(cdata) = c.compress(buf)
                            && cdata.len() < file_len
                        {
                            (cdata, true)
                        } else {
                            (buf.to_vec(), false)
                        };

                        blocking_map.insert(
                            slot,
                            path.to_path_buf(),
                            FileResult::Data {
                                hash: file_hash,
                                data,
                                file_len,
                                is_compressed,
                            },
                        );
                    }
                };

                // Load file data (from cache or mmap)
                if let Some((buf, h)) = file_cache.get(path) {
                    process_file(buf, h, path);
                } else {
                    let file_data = unsafe { mmap_file(path, metadata).expect("mmap_file failed") };
                    let h = hash(file_data.as_ref());

                    process_file(file_data.as_ref(), h, path);
                }
            }
        });
        handles.push(handle);
    }

    // Main thread: the Writer
    let start = Instant::now();
    let mut w = Writer {
        f: BufWriter::with_capacity(256 * 1024, f),
        comp,
        offset: 0,
        hash_dedup,
        stats: Stats::default(),
    };

    let root = w.write_tree(&tree, blocking_map.as_ref())?;

    // Clean up
    for h in handles {
        h.join().expect("worker thread panicked");
    }

    println!("Time elapsed: {:?}", start.elapsed());
    w.print_stats();

    let file = w.finish(root)?;
    file.sync_all()?;

    Ok(())
}

struct Writer {
    f: BufWriter<fs::File>,
    hash_dedup: Arc<Mutex<HashMap<Hash, layout::Node>>>,
    offset: u64,
    comp: Option<WriterCompress>,
    stats: Stats,
}

pub struct WriterCompress {
    // Raw dictionary bytes, accessible via dict()
    dict: Arc<Vec<u8>>,
    // Prepared encoder dictionary shared across clones
    encoder_dict: Arc<EncoderDictionary<'static>>,
    // Compressor context borrows from encoder_dict
    // Declared after encoder_dict so it drops first
    comp: Compressor<'static>,
}

impl Clone for WriterCompress {
    fn clone(&self) -> Self {
        // Safety: Arc::as_ptr points into the Arc's heap allocation.
        // The allocation is owned by this struct (and its clones),
        // so the reference is valid for the struct's lifetime.
        let encoder_dict_ref: &'static EncoderDictionary<'static> =
            unsafe { &*Arc::as_ptr(&self.encoder_dict) };

        let comp = Compressor::with_prepared_dictionary(encoder_dict_ref)
            .expect("dictionary already validated");

        Self {
            dict: Arc::clone(&self.dict),
            encoder_dict: Arc::clone(&self.encoder_dict),
            comp,
        }
    }
}

impl WriterCompress {
    pub fn from_dict(level: i32, dict: Vec<u8>) -> io::Result<Self> {
        let dict = Arc::new(dict);
        let encoder_dict = Arc::new(EncoderDictionary::copy(&dict, level));

        // Safety: Same reasoning as in Clone. The Arc owns the heap
        // allocation and is stored in this struct, so the dictionary
        // outlives the compressor context.
        let encoder_dict_ref: &'static EncoderDictionary<'static> =
            unsafe { &*Arc::as_ptr(&encoder_dict) };

        let comp = Compressor::with_prepared_dictionary(encoder_dict_ref)?;

        Ok(Self {
            dict,
            encoder_dict,
            comp,
        })
    }

    pub fn compress(&mut self, data: &[u8]) -> Result<Vec<u8>, io::Error> {
        self.comp.compress(data)
    }

    /// Returns a reference to the raw dictionary bytes.
    pub fn dict(&self) -> &[u8] {
        &self.dict
    }
}

enum NodeType {
    File {
        hash: Hash,
        file_len: usize,
        is_compressed: bool,
    },
    Dir,
}

impl Writer {
    /// Recursively write the in-memory tree. No OS calls for files.
    fn write_tree(
        &mut self,
        node: &FsNode,
        blocking_map: &BlockingSlotMap<PathBuf, FileResult>,
    ) -> io::Result<layout::Node> {
        match node {
            FsNode::Dir { children, .. } => {
                self.stats.total_dirs += 1;

                let mut buf = Vec::with_capacity(children.len() * 48);
                for (name, child) in children {
                    let node = self.write_tree(child, blocking_map)?;
                    let name = name.to_string_lossy().to_string();

                    buf.push(name.len().try_into().unwrap());
                    buf.extend_from_slice(name.as_bytes());
                    buf.extend_from_slice(&node.to_bytes());
                }

                let mut res = self.write_node(&buf, NodeType::Dir)?;
                res.flags |= layout::FLAG_DIR;
                Ok(res)
            }
            FsNode::File { path, .. } => {
                self.stats.total_files += 1;

                match blocking_map.remove(path) {
                    FileResult::Node { node, file_len } => {
                        self.stats.nodes_before_dedup += 1;
                        self.stats.uncompressed_bytes_before_dedup += file_len as u64;
                        self.stats.compressed_bytes_before_dedup += node.range.len;

                        Ok(node)
                    }
                    FileResult::Data {
                        hash,
                        data,
                        file_len,
                        is_compressed,
                    } => self.write_node(
                        &data,
                        NodeType::File {
                            hash,
                            file_len,
                            is_compressed,
                        },
                    ),
                }
            }
        }
    }

    fn write_node(&mut self, buf: &[u8], node_type: NodeType) -> io::Result<layout::Node> {
        let (hash, uncompressed_bytes, is_compressed) = match node_type {
            NodeType::Dir => (hash(buf), buf.len(), false),
            NodeType::File {
                hash,
                file_len,
                is_compressed,
            } => (hash, file_len, is_compressed),
        };

        // Track stats before dedup
        self.stats.nodes_before_dedup += 1;
        self.stats.uncompressed_bytes_before_dedup += uncompressed_bytes as u64;

        let node = self.hash_dedup.lock().unwrap().get(&hash).copied();
        if let Some(node) = node {
            self.stats.compressed_bytes_before_dedup += node.range.len;

            return Ok(node);
        }

        // This is a new unique node
        self.stats.nodes_after_dedup += 1;
        self.stats.uncompressed_bytes_after_dedup += uncompressed_bytes as u64;

        let mut flags = 0;
        if is_compressed {
            flags |= layout::FLAG_COMPRESSED;
        }

        self.stats.compressed_bytes_before_dedup += buf.len() as u64;
        self.stats.compressed_bytes_after_dedup += buf.len() as u64;

        let range = self.write_data(buf)?;
        let node = layout::Node { range, flags };
        self.hash_dedup.lock().unwrap().insert(hash, node);
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
            Some(self.write_data(comp.dict())?)
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
