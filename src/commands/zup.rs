use clap::Parser;
use std::fs;
use std::path::PathBuf;

use crate::common::zup::write::{pack, CompressConfig};

#[derive(Parser)]
pub struct ZupArgs {
    /// Input directory to compress
    #[clap(short, long)]
    pub input: PathBuf,
    /// Output .zup file
    #[clap(short, long)]
    pub output: PathBuf,
    /// Compress output with zstd
    #[clap(short, long)]
    pub compress: bool,
    /// Compression level
    #[clap(long, default_value = "7")]
    pub compress_level: i32,
    /// Compress dictionary size
    #[clap(long, default_value = "163840")]
    pub dict_size: usize,
    /// Compress dictionary training set max size
    #[clap(long, default_value = "100000000")]
    pub dict_train_size: usize,
}

pub async fn run(args: ZupArgs) -> anyhow::Result<()> {
    println!("Compressing directory: {:?}", args.input);

    // Create output directory if it doesn't exist
    if let Some(p) = args.output.parent() {
        fs::create_dir_all(p)?;
    }

    let compress = args.compress.then(|| CompressConfig {
        level: args.compress_level,
        dict_size: args.dict_size,
        dict_train_size: args.dict_train_size,
    });

    // Pack the input directory using the new pack function
    pack(&args.input, &args.output, compress)?;

    println!("Created archive: {:?}", args.output);

    Ok(())
}
