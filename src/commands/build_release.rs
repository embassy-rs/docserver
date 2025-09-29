use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};
use clap::Parser;
use serde::Deserialize;

use crate::commands::build::{run as build_run, BuildArgs};
use crate::common::CompressionArgs;

#[derive(Deserialize)]
struct CratesIoResponse {
    versions: Vec<VersionInfo>,
}

#[derive(Deserialize)]
struct VersionInfo {
    #[serde(rename = "num")]
    version: String,
    yanked: bool,
}

#[derive(Parser)]
pub struct BuildReleaseArgs {
    /// Crate name to download from crates.io
    #[clap(long)]
    pub crate_name: String,

    /// Version of the crate to download
    #[clap(long)]
    pub version: Option<String>,

    /// Download and build all versions of the crate
    #[clap(long)]
    pub all_versions: bool,

    /// Webroot directory where the .zup file will be placed
    #[clap(long)]
    pub webroot: PathBuf,

    /// Temporary directory for intermediate files
    #[clap(long, default_value = "./work")]
    pub temp_dir: PathBuf,

    /// Force rebuild even if the output file already exists
    #[clap(long)]
    pub force: bool,

    #[clap(flatten)]
    pub compression: CompressionArgs,
}

async fn fetch_crate_versions(crate_name: &str) -> Result<Vec<String>> {
    let url = format!("https://crates.io/api/v1/crates/{}", crate_name);
    
    let mut cmd = Command::new("curl");
    cmd.args(&["-s", "-f", &url]);
    
    let output = cmd.output().context("Failed to execute curl command")?;
    
    if !output.status.success() {
        return Err(anyhow::anyhow!(
            "Failed to fetch crate info for {}: curl exited with status {}",
            crate_name, output.status
        ));
    }
    
    let response_text = String::from_utf8(output.stdout)
        .context("Failed to parse curl output as UTF-8")?;
    
    let response: CratesIoResponse = serde_json::from_str(&response_text)
        .context("Failed to parse crates.io API response")?;
    
    // Filter out yanked versions and 0.0.x versions
    let versions: Vec<String> = response.versions
        .into_iter()
        .filter(|v| !v.yanked)
        .map(|v| v.version)
        .filter(|v| !v.starts_with("0.0."))
        .collect();
    
    Ok(versions)
}

async fn build_single_version(crate_name: &str, version: &str, args: &BuildReleaseArgs) -> Result<()> {
    println!(
        "Downloading crate {} version {} from crates.io",
        crate_name, version
    );

    // Validate that required tools are available
    let curl_check = Command::new("curl")
        .arg("--version")
        .output()
        .context("Failed to check curl availability - is curl installed?")?;

    if !curl_check.status.success() {
        return Err(anyhow::anyhow!(
            "curl is required but not available or not working"
        ));
    }

    let tar_check = Command::new("tar")
        .arg("--version")
        .output()
        .context("Failed to check tar availability - is tar installed?")?;

    if !tar_check.status.success() {
        return Err(anyhow::anyhow!(
            "tar is required but not available or not working"
        ));
    }

    // Create temp directory structure
    let download_dir = args.temp_dir.join("download");
    let extract_dir = args.temp_dir.join("extract");

    // Clean and create directories
    if download_dir.exists() {
        fs::remove_dir_all(&download_dir)?;
    }
    if extract_dir.exists() {
        fs::remove_dir_all(&extract_dir)?;
    }
    fs::create_dir_all(&download_dir)?;
    fs::create_dir_all(&extract_dir)?;

    // Download the crate
    let crate_file = format!("{}-{}.crate", crate_name, version);
    let crate_path = download_dir.join(&crate_file);
    let download_url = format!(
        "https://crates.io/api/v1/crates/{}/{}/download",
        crate_name, version
    );

    println!("Downloading from: {}", download_url);

    let mut cmd = Command::new("curl");
    cmd.args(&[
        "-L", // Follow redirects
        "-f", // Fail on HTTP error codes
        "-o",
        crate_path.to_str().unwrap(),
        &download_url,
    ]);

    let status = cmd.status().context("Failed to execute curl command")?;
    if !status.success() {
        return Err(anyhow::anyhow!(
            "Failed to download crate {}-{}: curl exited with status {}. Check that the crate name and version are correct.", 
            crate_name, version, status
        ));
    }

    println!("Downloaded crate to: {}", crate_path.display());

    // Extract the crate (it's a .tar.gz file despite the .crate extension)
    let mut cmd = Command::new("tar");
    cmd.args(&[
        "-xzf",
        crate_path.to_str().unwrap(),
        "-C",
        extract_dir.to_str().unwrap(),
    ]);

    let status = cmd.status().context("Failed to execute tar command")?;
    if !status.success() {
        return Err(anyhow::anyhow!(
            "Failed to extract crate: tar exited with status {}",
            status
        ));
    }

    // The extracted directory should be named {crate_name}-{version}
    let crate_dir = extract_dir.join(format!("{}-{}", crate_name, version));

    if !crate_dir.exists() {
        return Err(anyhow::anyhow!(
            "Expected extracted directory does not exist: {}. The crate archive may not have the expected structure.", 
            crate_dir.display()
        ));
    }

    // Check that the extracted directory contains a Cargo.toml
    let cargo_toml = crate_dir.join("Cargo.toml");
    if !cargo_toml.exists() {
        return Err(anyhow::anyhow!(
            "No Cargo.toml found in extracted crate directory: {}. This doesn't appear to be a valid Rust crate.",
            crate_dir.display()
        ));
    }

    println!("Extracted crate to: {}", crate_dir.display());

    // Create the appropriate directory structure in webroot and determine output path
    // Following the pattern: webroot/crates/{crate_name}/{version}.zup
    let crate_webroot_dir = args.webroot.join("crates").join(&args.crate_name);
    fs::create_dir_all(&crate_webroot_dir)?;

    let output_zup_path = crate_webroot_dir.join(format!("{}.zup", version));
    let output_static_dir = args.webroot.join("static");

    println!("Output .zup file will be: {}", output_zup_path.display());

    // Now use the regular build command with the extracted directory
    let build_args = BuildArgs {
        input: crate_dir,
        output: output_zup_path,
        output_static: Some(output_static_dir),
        temp_dir: args.temp_dir.clone(),
        compression: args.compression.clone(),
    };

    build_run(build_args).await
}

pub async fn run(args: BuildReleaseArgs) -> Result<()> {
    // Validate that exactly one of --version or --all-versions is provided
    match (args.version.as_ref(), args.all_versions) {
        (Some(_), true) => {
            return Err(anyhow::anyhow!(
                "Cannot specify both --version and --all-versions. Use exactly one."
            ));
        }
        (None, false) => {
            return Err(anyhow::anyhow!(
                "Must specify either --version or --all-versions."
            ));
        }
        _ => {} // Valid: either (Some(_), false) or (None, true)
    }

    if args.all_versions {
        // Build all versions
        println!("Fetching all versions for crate: {}", args.crate_name);
        
        let all_versions = fetch_crate_versions(&args.crate_name).await?;
        
        if all_versions.is_empty() {
            println!("No valid versions found for crate: {}", args.crate_name);
            return Ok(());
        }
        
        println!("Found {} versions to potentially build", all_versions.len());
        
        // Create crate directory in webroot to check existing versions
        let crate_webroot_dir = args.webroot.join("crates").join(&args.crate_name);
        fs::create_dir_all(&crate_webroot_dir)?;
        
        let mut built_count = 0;
        let mut skipped_count = 0;
        
        for version in all_versions {
            let zup_path = crate_webroot_dir.join(format!("{}.zup", version));
            
            if zup_path.exists() && !args.force {
                println!("Skipping version {} (already exists)", version);
                skipped_count += 1;
                continue;
            }
            
            if zup_path.exists() && args.force {
                println!("Rebuilding version {} (--force specified)", version);
            } else {
                println!("Building version {}", version);
            }
            
            match build_single_version(&args.crate_name, &version, &args).await {
                Ok(()) => {
                    println!("Successfully built version {}", version);
                    built_count += 1;
                }
                Err(e) => {
                    eprintln!("Failed to build version {}: {}", version, e);
                    // Continue with other versions instead of stopping
                }
            }
        }
        
        println!("Built {} new versions, skipped {} existing versions", built_count, skipped_count);
    } else {
        // Build single version
        let version = args.version.as_ref().unwrap(); // Safe due to validation above
        
        // Check if version already exists before building
        let crate_webroot_dir = args.webroot.join("crates").join(&args.crate_name);
        let zup_path = crate_webroot_dir.join(format!("{}.zup", version));
        
        if zup_path.exists() && !args.force {
            println!("Version {} already exists at: {}", version, zup_path.display());
            println!("Use --force to rebuild, or remove the existing file first.");
            return Ok(());
        }
        
        if zup_path.exists() && args.force {
            println!("Rebuilding version {} (--force specified)", version);
        }
        
        build_single_version(&args.crate_name, version, &args).await?;
        println!("Successfully built version {}", version);
    }
    
    Ok(())
}
