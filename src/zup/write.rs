use rand::seq::SliceRandom;
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::collections::HashMap;
use std::fs::{self};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use zstd::block::Compressor;

use super::layout;

#[derive(PartialEq, Eq, Clone, Copy, Hash, Debug)]
pub struct NodeId(u64);

pub enum Node {
    Directory(Directory),
    File(File),
}

pub struct Directory {
    pub entries: Vec<DirectoryEntry>,
}

pub struct DirectoryEntry {
    pub name: String,
    pub node_id: NodeId,
}

enum PathOrData {
    Path(
        (
            PathBuf,
            Mutex<Arc<dyn Fn(&Path, &mut Vec<u8>) + Sync + Send>>,
            usize,
        ),
    ),
    Data(Arc<Vec<u8>>),
}

pub struct File {
    contents: PathOrData,
}

impl File {
    pub fn from_data(data: Vec<u8>) -> Self {
        Self {
            contents: PathOrData::Data(Arc::new(data)),
        }
    }

    pub fn from_path(
        path: PathBuf,
        data_filter: Arc<dyn Fn(&Path, &mut Vec<u8>) + Sync + Send>,
        len: usize,
    ) -> Self {
        Self {
            contents: PathOrData::Path((path, Mutex::new(data_filter), len)),
        }
    }

    fn len(&self) -> usize {
        match &self.contents {
            PathOrData::Path((_, _, len)) => *len,
            PathOrData::Data(data) => data.len(),
        }
    }

    fn data(&self) -> io::Result<FileData> {
        match &self.contents {
            PathOrData::Path((path, data_filter, _)) => {
                let mut data = fs::read(&path)?;
                {
                    let data_filter = data_filter.lock().unwrap();
                    (data_filter)(&path, &mut data);
                }

                Ok(FileData(Arc::new(data)))
            }
            PathOrData::Data(data) => Ok(FileData(data.clone())),
        }
    }
}

pub struct FileData(Arc<Vec<u8>>);

impl AsRef<[u8]> for FileData {
    fn as_ref(&self) -> &[u8] {
        &*self.0
    }
}

impl Node {
    fn hash(&mut self) -> [u8; 32] {
        let mut hash = Sha256::new();
        match self {
            Self::File(file) => hash.update(file.data().unwrap()),
            Self::Directory(dir) => {
                for entry in &dir.entries {
                    hash.update(entry.name.len().to_le_bytes());
                    hash.update(entry.name.as_bytes());
                    hash.update(entry.node_id.0.to_le_bytes());
                }
            }
        }
        hash.finalize().into()
    }
}

pub struct PackConfig {
    pub data_filter: Box<dyn Fn(&Path, &mut Vec<u8>) + Send + Sync>,
    pub file_filter: Box<dyn Fn(&Path) -> bool>,
}

pub struct CompressConfig {
    pub level: i32,
    pub dict_size: usize,
    pub dict_train_size: usize,
}

pub struct Tree {
    nodes: HashMap<NodeId, Node>,
    hash_dedup: HashMap<[u8; 32], NodeId>,
    next_id: u64,
    work_dir: PathBuf,
    next_dir: u64,
}

impl Tree {
    pub fn new(work_dir: PathBuf) -> Self {
        Self {
            nodes: HashMap::new(),
            hash_dedup: HashMap::new(),
            next_id: 0,
            work_dir: work_dir,
            next_dir: 0,
        }
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn total_bytes(&self) -> usize {
        self.nodes
            .iter()
            .map(|(_, n)| match n {
                Node::File(f) => f.len(),
                _ => 0,
            })
            .sum()
    }

    pub fn gen_id(&mut self) -> NodeId {
        self.next_id += 1;
        NodeId(self.next_id)
    }

    pub fn pack(
        &mut self,
        path: &Path,
        file_filter: &Box<dyn Fn(&Path) -> bool>,
        data_filter: &Arc<dyn Fn(&Path, &mut Vec<u8>) + Send + Sync>,
    ) -> io::Result<Option<NodeId>> {
        let to = self
            .work_dir
            .join(self.next_dir.to_string())
            .join(path.file_name().unwrap());

        fs::create_dir_all(to.parent().unwrap())?;
        fs::rename(path, &to)?;

        self.next_dir += 1;
        self.pack_inner(&to, file_filter, data_filter)
    }

    fn pack_inner(
        &mut self,
        path: &Path,
        file_filter: &Box<dyn Fn(&Path) -> bool>,
        data_filter: &Arc<dyn Fn(&Path, &mut Vec<u8>) + Send + Sync>,
    ) -> io::Result<Option<NodeId>> {
        let path = path.canonicalize()?;

        let m = fs::metadata(&path)?;
        let node = if m.is_dir() {
            let mut readdir = Vec::new();
            for entry in fs::read_dir(&path)? {
                readdir.push(entry?);
            }
            readdir.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

            let mut entries = Vec::new();

            for entry in readdir {
                let child = entry.path();

                if !(file_filter)(&child) {
                    continue;
                }

                let Some(node_id) = self.pack_inner(&child, &file_filter, &data_filter)? else {
                    continue;
                };
                let name = entry.file_name().to_string_lossy().to_string();
                entries.push(DirectoryEntry { name, node_id });
            }
            if entries.is_empty() {
                return Ok(None);
            }
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            Node::Directory(Directory { entries })
        } else if m.is_file() {
            Node::File(File::from_path(
                path.clone(),
                data_filter.clone(),
                m.len().try_into().unwrap(),
            ))
        } else {
            panic!("unknown type {:?} {:?}", path, m);
        };

        Ok(Some(self.add_node(node)))
    }

    pub fn add_node(&mut self, mut node: Node) -> NodeId {
        let hash = node.hash();
        if let Some(id) = self.hash_dedup.get(&hash) {
            return *id;
        }

        let id = self.gen_id();
        self.nodes.insert(id, node);
        self.hash_dedup.insert(hash, id);

        id
    }

    pub fn write(
        &mut self,
        path: &Path,
        root: NodeId,
        compress: Option<CompressConfig>,
    ) -> io::Result<()> {
        let f = fs::File::create(path)?;

        let comp = compress.map(|compress| {
            println!("compressing...");
            let mut files: Vec<_> = self
                .nodes
                .iter()
                .filter_map(|(_, n)| match n {
                    Node::File(f) => Some(f),
                    _ => None,
                })
                .collect();
            files.shuffle(&mut rand::thread_rng());
            let mut len = 0;
            let mut i = 0;
            while len < compress.dict_train_size && i < files.len() {
                len += files[i].len();
                i += 1;
            }
            let files: Vec<_> = files[..i].iter().map(|f| f.data().unwrap()).collect();

            let dict = zstd::dict::from_samples(&files, compress.dict_size).unwrap();
            WriterCompress {
                c: zstd::block::Compressor::with_dict(dict.clone()),
                dict,
                level: compress.level,
            }
        });

        // Write stuff
        let mut w = Writer {
            f,
            comp,
            nodes: HashMap::new(),
            offset: 0,
            tree: self,
        };

        let root = w.write(root)?;
        w.finish(root)?;

        fs::remove_dir_all(&self.work_dir)?;

        Ok(())
    }
}

struct Writer<'a> {
    tree: &'a Tree,
    f: fs::File,
    nodes: HashMap<NodeId, layout::Node>,
    offset: u64,
    comp: Option<WriterCompress>,
}

struct WriterCompress {
    c: Compressor,
    dict: Vec<u8>,
    level: i32,
}

impl<'a> Writer<'a> {
    fn write(&mut self, node_id: NodeId) -> io::Result<layout::Node> {
        if let Some(res) = self.nodes.get(&node_id) {
            return Ok(*res);
        }

        let res = match self.tree.nodes.get(&node_id).unwrap() {
            Node::Directory(dir) => {
                let mut buf = Vec::new();
                for entry in &dir.entries {
                    let node = self.write(entry.node_id)?;

                    buf.push(entry.name.len().try_into().unwrap());
                    buf.extend_from_slice(entry.name.as_bytes());
                    buf.extend_from_slice(&node.to_bytes());
                }
                let mut res = self.write_node(&buf)?;
                res.flags |= layout::FLAG_DIR;
                res
            }
            Node::File(file) => {
                let result = self.write_node(file.data().unwrap())?;

                result
            }
        };

        self.nodes.insert(node_id, res);
        Ok(res)
    }

    fn write_node(&mut self, buf: impl AsRef<[u8]>) -> io::Result<layout::Node> {
        let mut flags = 0;
        let mut buf: Cow<[u8]> = Cow::Borrowed(buf.as_ref());

        if let Some(comp) = &mut self.comp {
            if let Ok(cdata) = comp.c.compress(&buf, comp.level) {
                if cdata.len() < buf.len() {
                    buf = cdata.into();
                    flags = layout::FLAG_COMPRESSED;
                }
            }
        }

        let range = self.write_data(&buf)?;
        Ok(layout::Node { range, flags })
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
