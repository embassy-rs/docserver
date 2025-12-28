use std::cell::Cell;
use std::fs;
use std::io::Read;
use std::io::{self, Error, ErrorKind};
#[cfg(target_os = "linux")]
use std::os::unix::fs::FileExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::str;
#[cfg(target_os = "windows")]
use windows::Win32::Foundation::HANDLE;
#[cfg(target_os = "windows")]
use windows::Win32::Storage::FileSystem::ReadFile;
use windows::Win32::System::IO::OVERLAPPED;
use zstd::Decoder;
use zstd::dict::DecoderDictionary;

use super::layout;

#[cfg(target_os = "linux")]
fn read_exact_at(file: &fs::File, buffer: &mut Vec<u8>, offset: u64) -> io::Result<()> {
    file.read_exact_at(&mut buffer, offset)
}

#[cfg(target_os = "windows")]
fn read_exact_at(file: &fs::File, buffer: &mut Vec<u8>, offset: u64) -> io::Result<()> {
    if buffer.is_empty() {
        return Ok(());
    }

    // Prepare OVERLAPPED structure with the offset
    let mut overlapped = OVERLAPPED::default();
    overlapped.Anonymous.Anonymous.Offset = offset as u32;
    overlapped.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;

    let handle = HANDLE(file.as_raw_handle());

    let mut total_read = 0;
    while total_read < buffer.len() {
        let mut bytes_read: u32 = 0;
        let success = unsafe {
            ReadFile(
                handle,
                Some(&mut buffer[total_read..]),
                Some(&mut bytes_read),
                Some(&mut overlapped),
            )
        };

        if !success.is_ok() {
            return Err(Error::last_os_error());
        }

        if bytes_read == 0 {
            return Err(Error::new(ErrorKind::UnexpectedEof, "Unexpected EOF"));
        }

        total_read += bytes_read as usize;
        // Advance offset in OVERLAPPED for next chunk
        let new_offset = offset + total_read as u64;
        overlapped.Anonymous.Anonymous.Offset = new_offset as u32;
        overlapped.Anonymous.Anonymous.OffsetHigh = (new_offset >> 32) as u32;
    }

    Ok(())
}

pub struct Reader {
    file: fs::File,
    superblock: layout::Superblock,
    dict: Option<DecoderDictionary<'static>>,
}

impl Reader {
    pub fn new<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let file = fs::File::open(path)?;
        let file_size = file.metadata()?.len();

        // Read the superblock from the end of the file
        let mut superblock_buf = vec![0u8; layout::Superblock::LEN];
        read_exact_at(
            &file,
            &mut superblock_buf,
            file_size - layout::Superblock::LEN as u64,
        )?;

        let superblock = layout::Superblock::from_bytes(superblock_buf.try_into().unwrap());

        let dict = if let Some(dict_range) = superblock.dict {
            let dict_data = Self::read_range(&file, dict_range)?;
            Some(DecoderDictionary::copy(&dict_data))
        } else {
            None
        };

        Ok(Self {
            file,
            superblock,
            dict,
        })
    }

    fn read_range(file: &fs::File, r: layout::Range) -> io::Result<Vec<u8>> {
        if r.len > 100_000_000 {
            return Err(io::Error::other("range too large"));
        }

        let mut buffer = vec![0u8; r.len as usize];
        read_exact_at(&file, &mut buffer, r.offset)?;
        Ok(buffer)
    }

    fn read_node(&self, node: layout::Node) -> io::Result<Vec<u8>> {
        let data = Self::read_range(&self.file, node.range)?;
        if node.flags & layout::FLAG_COMPRESSED != 0 {
            let Some(dict) = &self.dict else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "node is compressed, but zup has no dictionary",
                ));
            };
            let mut res = Vec::new();
            let mut dec = Decoder::with_prepared_dictionary(&data[..], dict)?;
            dec.read_to_end(&mut res)?;
            Ok(res)
        } else {
            Ok(data)
        }
    }

    pub fn root_node(&self) -> Node<'_> {
        Node::Directory(Directory {
            reader: self,
            node: self.superblock.root,
        })
    }

    pub fn open(&self, path: &[&str]) -> io::Result<Node<'_>> {
        let mut node = self.root_node();
        for (i, segment) in path.iter().enumerate() {
            match node {
                Node::File(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::NotADirectory,
                        format!("is a file, not a directory: {}", path[..i].join("/")),
                    ));
                }
                Node::Directory(dir) => {
                    let (_, child) = dir
                        .children()?
                        .into_iter()
                        .find(|(name, _)| name == segment)
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::NotFound,
                                format!("not found: {}", path[..i + 1].join("/")),
                            )
                        })?;
                    node = child
                }
            }
        }
        Ok(node)
    }

    pub fn read(&self, path: &[&str]) -> io::Result<Vec<u8>> {
        match self.open(path)? {
            Node::Directory(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::IsADirectory,
                    format!("is a directory, not a file: {}", path.join("/")),
                ));
            }
            Node::File(f) => f.read(),
        }
    }
}

pub enum Node<'a> {
    File(File<'a>),
    Directory(Directory<'a>),
}

impl<'a> Node<'a> {
    pub fn node(&self) -> layout::Node {
        match self {
            Self::File(n) => n.node(),
            Self::Directory(n) => n.node(),
        }
    }
}

pub struct File<'a> {
    reader: &'a Reader,
    node: layout::Node,
}

impl<'a> File<'a> {
    pub fn node(&self) -> layout::Node {
        self.node
    }
    pub fn read(&self) -> io::Result<Vec<u8>> {
        self.reader.read_node(self.node)
    }
}

pub struct Directory<'a> {
    reader: &'a Reader,
    node: layout::Node,
}

impl<'a> Directory<'a> {
    pub fn node(&self) -> layout::Node {
        self.node
    }

    pub fn children(&self) -> io::Result<Vec<(String, Node<'a>)>> {
        let data = self.reader.read_node(self.node).unwrap();
        let data = ByteReader::new(&data);

        let mut res = Vec::new();

        while !data.eof() {
            let name = str::from_utf8(data.read_slice_len8()?)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid utf8 filename"))?
                .to_string();
            let node = layout::Node::from_bytes(data.read()?);
            let node = if node.flags & layout::FLAG_DIR != 0 {
                Node::Directory(Directory {
                    reader: self.reader,
                    node,
                })
            } else {
                Node::File(File {
                    reader: self.reader,
                    node,
                })
            };
            res.push((name, node));
        }

        Ok(res)
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
struct ReadError;

impl From<ReadError> for io::Error {
    fn from(_: ReadError) -> Self {
        io::Error::new(io::ErrorKind::UnexpectedEof, "Unexpected EOF")
    }
}

struct ByteReader<'a> {
    data: Cell<&'a [u8]>,
}

impl<'a> ByteReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data: Cell::new(data),
        }
    }

    fn eof(&self) -> bool {
        self.data.get().is_empty()
    }

    fn read<const N: usize>(&self) -> Result<[u8; N], ReadError> {
        let n = self.data.get().get(0..N).ok_or(ReadError)?;
        self.data.set(&self.data.get()[N..]);
        Ok(n.try_into().unwrap())
    }

    fn read_u8(&self) -> Result<u8, ReadError> {
        Ok(u8::from_le_bytes(self.read()?))
    }

    #[allow(dead_code)]
    fn read_u16(&self) -> Result<u16, ReadError> {
        Ok(u16::from_le_bytes(self.read()?))
    }

    #[allow(dead_code)]
    fn read_u32(&self) -> Result<u32, ReadError> {
        Ok(u32::from_le_bytes(self.read()?))
    }

    #[allow(dead_code)]
    fn read_u64(&mut self) -> Result<u64, ReadError> {
        Ok(u64::from_le_bytes(self.read()?))
    }

    fn read_slice(&self, len: usize) -> Result<&[u8], ReadError> {
        let res = self.data.get().get(0..len).ok_or(ReadError)?;
        self.data.set(&self.data.get()[len..]);
        Ok(res)
    }

    fn read_slice_len8(&self) -> Result<&[u8], ReadError> {
        let len = self.read_u8()? as usize;
        self.read_slice(len)
    }

    #[allow(dead_code)]
    fn read_slice_len16(&self) -> Result<&[u8], ReadError> {
        let len = self.read_u16()? as usize;
        self.read_slice(len)
    }
}
