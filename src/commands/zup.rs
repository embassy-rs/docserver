use clap::Parser;
use std::fs;
use std::path::PathBuf;

use crate::common::zup::write::pack;
use crate::common::CompressionArgs;

#[derive(Parser)]
pub struct ZupArgs {
    /// Input directory to compress
    #[clap(short, long)]
    pub input: PathBuf,
    /// Output .zup file
    #[clap(short, long)]
    pub output: PathBuf,
    
    #[clap(flatten)]
    pub compression: CompressionArgs,
}

pub async fn run(args: ZupArgs) -> anyhow::Result<()> {
    println!("Compressing directory: {:?}", args.input);

    // Create output directory if it doesn't exist
    if let Some(p) = args.output.parent() {
        fs::create_dir_all(p)?;
    }

    let compress = args.compression.to_config();

    // Pack the input directory using the new pack function
    pack(&args.input, &args.output, compress)?;

    println!("Created archive: {:?}", args.output);

    Ok(())
}
