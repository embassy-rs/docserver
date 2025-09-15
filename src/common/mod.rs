pub mod manifest;
pub mod zup;

use clap::Args;

/// Shared compression configuration
#[derive(Debug, Clone, Args)]
pub struct CompressionArgs {
    /// Disable compression for .zup archives (compression is enabled by default)
    #[clap(long)]
    pub no_compress: bool,

    /// Compression level (only for .zup archives)
    #[clap(long, default_value = "7")]
    pub compress_level: i32,

    /// Compress dictionary size (only for .zup archives)
    #[clap(long, default_value = "163840")]
    pub dict_size: usize,

    /// Compress dictionary training set max size (only for .zup archives)
    #[clap(long, default_value = "100000000")]
    pub dict_train_size: usize,
}

impl CompressionArgs {
    /// Convert to CompressConfig if compression is enabled
    pub fn to_config(&self) -> Option<crate::common::zup::write::CompressConfig> {
        (!self.no_compress).then(|| crate::common::zup::write::CompressConfig {
            level: self.compress_level,
            dict_size: self.dict_size,
            dict_train_size: self.dict_train_size,
        })
    }
}
