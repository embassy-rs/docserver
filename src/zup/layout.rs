pub const FLAG_COMPRESSED: u32 = 1;
pub const FLAG_DIR: u32 = 2;

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub struct Range {
    pub offset: u64,
    pub len: u64,
}

impl Range {
    pub const LEN: usize = 16;
    pub fn from_bytes(b: [u8; Self::LEN]) -> Self {
        let offset = u64::from_le_bytes(b[0..8].try_into().unwrap());
        let len = u64::from_le_bytes(b[8..16].try_into().unwrap());
        Self { len, offset }
    }

    pub fn to_bytes(self) -> [u8; Self::LEN] {
        let mut res = [0; Self::LEN];
        res[0..8].copy_from_slice(&self.offset.to_le_bytes());
        res[8..16].copy_from_slice(&self.len.to_le_bytes());
        res
    }
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub struct Node {
    pub flags: u32,
    pub range: Range,
}

impl Node {
    pub const LEN: usize = 20;
    pub fn from_bytes(b: [u8; Self::LEN]) -> Self {
        let flags = u32::from_le_bytes(b[0..4].try_into().unwrap());
        let range = Range::from_bytes(b[4..20].try_into().unwrap());
        Self { flags, range }
    }

    pub fn to_bytes(self) -> [u8; Self::LEN] {
        let mut res = [0; Self::LEN];
        res[0..4].copy_from_slice(&self.flags.to_le_bytes());
        res[4..20].copy_from_slice(&self.range.to_bytes());
        res
    }
}

pub const MAGIC: u32 = 0x2170755a;
pub const VERSION: u32 = 1;

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub struct Superblock {
    pub dict: Range,
    pub root: Node,
    pub version: u32,
    pub magic: u32,
}

impl Superblock {
    pub const LEN: usize = 44;
    pub fn from_bytes(b: [u8; Self::LEN]) -> Self {
        let dict = Range::from_bytes(b[0..16].try_into().unwrap());
        let root = Node::from_bytes(b[16..36].try_into().unwrap());
        let version = u32::from_le_bytes(b[36..40].try_into().unwrap());
        let magic = u32::from_le_bytes(b[40..44].try_into().unwrap());
        Self {
            dict,
            root,
            version,
            magic,
        }
    }

    pub fn to_bytes(self) -> [u8; Self::LEN] {
        let mut res = [0; Self::LEN];
        res[0..16].copy_from_slice(&self.dict.to_bytes());
        res[16..36].copy_from_slice(&self.root.to_bytes());
        res[36..40].copy_from_slice(&self.version.to_le_bytes());
        res[40..44].copy_from_slice(&self.magic.to_le_bytes());
        res
    }
}
