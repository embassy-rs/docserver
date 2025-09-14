use clap::Parser;
use std::fs;
use std::path::PathBuf;

use crate::common::zup::write::*;

#[derive(Parser)]
pub struct ZupArgs {
    /// Input directory to compress
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
    let mut zup_tree = Tree::new(PathBuf::from("zup_tree_work"));

    // Simple file filter that includes all files
    let file_filter: Box<dyn Fn(&std::path::Path) -> bool> = Box::new(|_path| true);

    // Simple data filter that doesn't modify data
    let data_filter: std::sync::Arc<dyn Fn(&std::path::Path, &mut Vec<u8>) + Send + Sync> =
        std::sync::Arc::new(|_path, _data| {});

    println!("Compressing directory: {:?}", args.input);

    // Pack the input directory
    let root_id = zup_tree
        .pack(&args.input, &file_filter, &data_filter)?
        .ok_or_else(|| anyhow::anyhow!("Failed to pack directory"))?;

    if let Some(p) = args.output.parent() {
        let _ = fs::create_dir_all(p);
    }

    println!("total nodes: {}", zup_tree.node_count());
    println!("total bytes: {}", zup_tree.total_bytes());

    let compress = args.compress.then(|| CompressConfig {
        level: args.compress_level,
        dict_size: args.dict_size,
        dict_train_size: args.dict_train_size,
    });

    zup_tree.write(&args.output, root_id, compress)?;

    println!("Created archive: {:?}", args.output);

    Ok(())
}
