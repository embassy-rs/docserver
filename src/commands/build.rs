use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};

use clap::Parser;
use regex::Regex;
use regex::bytes::Regex as ByteRegex;

use crate::common::CompressionArgs;
use crate::common::manifest;
use crate::common::zup::write::pack;

fn should_include_file(path: &Path) -> bool {
    path.file_name().map_or(true, |f| {
        f != "implementors" && !f.to_str().unwrap().starts_with('_') && !path.ends_with("!.html")
    })
}

fn process_html_file(crate_name: &str, data: Vec<u8>) -> Vec<u8> {
    let crate_name = crate_name.replace('-', "_");

    // Remove settings button (it breaks due to the path rewriting, we'll provide our own version)
    let re_remove_settings = ByteRegex::new(r##"<a id="settings-menu".*?</a>"##).unwrap();

    // Remove srclinks that point to a file starting with `_`.
    let re_remove_hidden_src =
        ByteRegex::new(r##"<a class="src" href="[^"]*/_[^"]*">source</a>"##).unwrap();

    // Rewrite srclinks from `../../crate_name/foo" to "/__DOCSERVER_SRCLINK/foo".
    let re_rewrite_src =
        ByteRegex::new(&format!(r##"href="(\.\./)+src/{}"##, &crate_name)).unwrap();

    // Remove crates.js
    let re_remove_cratesjs =
        ByteRegex::new(r##"<script\s*(?:defer(="")?)?\s*src="(\.\./)+crates.js"></script>"##)
            .unwrap();

    // Rewrite links from `../crate_name" to "".
    let re_rewrite_root = ByteRegex::new(&format!(r##"\.\./{}/"##, &crate_name)).unwrap();

    let re_fix_root_path = ByteRegex::new(r##"data-root-path="\.\./"##).unwrap();

    let res = re_remove_settings.replace_all(&data, &[][..]).into_owned();
    let res = re_remove_hidden_src.replace_all(&res, &[][..]).into_owned();
    let res = re_remove_cratesjs
        .replace_all(
            &res,
            format!(
                r##"<script type="text/javascript">window.ALL_CRATES=["{}"];</script>"##,
                crate_name
            )
            .as_bytes(),
        )
        .into_owned();
    let res = re_rewrite_src
        .replace_all(&res, &b"href=\"/__DOCSERVER_SRCLINK"[..])
        .into_owned();
    let res = re_rewrite_root.replace_all(&res, &[][..]).into_owned();
    let res = re_fix_root_path
        .replace_all(&res, &b"data-root-path=\"./"[..])
        .into_owned();
    res
}

#[derive(Debug, Clone)]
struct Flavor {
    name: String,
    features: Vec<String>,
    target: String,
}

fn load_manifest_bytes(crate_path: &Path) -> Vec<u8> {
    let manifest_path = crate_path.join("Cargo.toml");
    fs::read(&manifest_path).unwrap()
}

fn load_manifest(crate_path: &Path) -> manifest::Manifest {
    toml::from_slice(&load_manifest_bytes(crate_path)).unwrap()
}

fn calc_flavors(manifest: &manifest::Manifest) -> Vec<Flavor> {
    let docs = &manifest.package.metadata.embassy_docs;

    let mut flavors = Vec::new();

    if docs.flavors.is_empty() {
        flavors.push(Flavor {
            name: "default".to_string(),
            features: docs.features.clone(),
            target: docs.target.clone().unwrap(),
        })
    }

    let mut processed = HashSet::new();
    for rule in &docs.flavors {
        let mut name_feats: Vec<(String, Vec<String>)> = Vec::new();
        match (&rule.name, &rule.regex_feature) {
            (Some(name), None) => name_feats.push((name.clone(), vec![])),
            (None, Some(re)) => {
                let re = Regex::new(&format!("^{}$", re)).unwrap();
                for feature in manifest.features.keys().filter(|s| re.is_match(s)) {
                    if !processed.contains::<String>(feature) {
                        name_feats.push((feature.clone(), vec![feature.clone()]));
                        processed.insert(feature.clone());
                    }
                }
            }
            _ => panic!(
                "Invalid flavor: Exactly one of `name` or `regex_feature` has to be specified."
            ),
        }

        for (name, mut features) in name_feats {
            features.extend_from_slice(&docs.features);
            features.extend_from_slice(&rule.features);
            flavors.push(Flavor {
                name,
                features,
                target: rule.target.clone().or(docs.target.clone()).unwrap(),
            })
        }
    }

    assert!(!flavors.is_empty());

    flavors
}

#[derive(Parser)]
pub struct BuildArgs {
    /// Input crate directory (the directory containing the Cargo.toml)
    #[clap(short)]
    pub input: PathBuf,

    /// Output path (directory or .zup file)
    #[clap(short)]
    pub output: PathBuf,

    /// Output directory containing static files.
    #[clap(long)]
    pub output_static: Option<PathBuf>,

    /// Temporary directory for intermediate files
    #[clap(long, default_value = "./work")]
    pub temp_dir: PathBuf,

    #[clap(flatten)]
    pub compression: CompressionArgs,
}

// Helper function to copy and process a directory recursively
fn copy_and_process_dir(src_dir: &Path, dest_dir: &Path, crate_name: &str) -> anyhow::Result<()> {
    for entry in fs::read_dir(src_dir)? {
        let entry = entry?;
        let src_path = entry.path();
        let file_name = entry.file_name();
        let dest_path = dest_dir.join(&file_name);

        if src_path.is_dir() {
            // Skip directories that should be filtered
            if should_include_file(&src_path) {
                fs::create_dir_all(&dest_path)?;
                copy_and_process_dir(&src_path, &dest_path, crate_name)?;
            }
        } else {
            // Skip files that should be filtered
            if should_include_file(&src_path) {
                let data = fs::read(&src_path)?;
                let processed_data =
                    if src_path.extension().and_then(|s| s.to_str()) == Some("html") {
                        process_html_file(crate_name, data)
                    } else {
                        data
                    };

                fs::write(&dest_path, &processed_data)?;
            }
        }
    }

    Ok(())
}

pub async fn run(args: BuildArgs) -> anyhow::Result<()> {
    // Determine if we're building to a .zup archive or a directory
    let is_zup_output = args
        .output
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s == "zup")
        .unwrap_or(false);

    // Set up temp directory structure
    let cargo_target_dir = args.temp_dir.join("target");
    let cargo_out_dir = args.temp_dir.join("out");
    let build_tree_dir = args.temp_dir.join("tree");

    // Clean the temp directories (but not the target dir)
    if cargo_out_dir.exists() {
        fs::remove_dir_all(&cargo_out_dir)?;
    }
    fs::create_dir_all(&cargo_out_dir)?;

    if is_zup_output && build_tree_dir.exists() {
        fs::remove_dir_all(&build_tree_dir)?;
    }

    // Determine the actual build output directory
    let build_output_dir = if is_zup_output {
        // For .zup output, build to the tree temp directory
        fs::create_dir_all(&build_tree_dir)?;
        build_tree_dir
    } else {
        // Check if output directory exists, create it if it doesn't
        if args.output.exists() {
            return Err(anyhow::anyhow!(
                "Output directory '{}' already exists. Please remove it or choose a different output path.",
                args.output.display()
            ));
        }
        fs::create_dir_all(&args.output)?;
        args.output.clone()
    };

    let manifest_bytes = load_manifest_bytes(&args.input);
    let manifest = load_manifest(&args.input);

    let mut cmd = Command::new("git");
    cmd.args(&["rev-parse", "HEAD"]);
    cmd.current_dir(&args.input);
    let output = cmd.output().unwrap();
    assert!(output.status.success());
    let docserver_info = manifest::DocserverInfo {
        git_commit: String::from_utf8(output.stdout).unwrap(),
    };
    let docserver_info_bytes = serde_json::to_vec(&docserver_info).unwrap();

    // Collect all flavors first to build the cargo batch command
    let flavors: Vec<_> = calc_flavors(&manifest);

    // Build the cargo batch command
    let mut cmd = Command::new("cargo");
    cmd.arg("batch")
        .arg("--target-dir")
        .arg(&cargo_target_dir)
        .arg("-Zunstable-options")
        .arg("-Zrustdoc-map")
        .arg("--stdin")
        .env("CARGO_TARGET_DIR", &cargo_target_dir)
        .stdin(Stdio::piped());

    let mut child = cmd.spawn()?;
    let mut debug = String::new();
    {
        let mut stdin = child.stdin.take().unwrap();

        for (i, flavor) in flavors.iter().enumerate() {
            let mut cmdargs = Vec::<String>::new();

            cmdargs.push("rustdoc".to_string());
            cmdargs.push("--manifest-path".to_string());
            cmdargs.push(args.input.join("Cargo.toml").to_str().unwrap().to_string());
            cmdargs.push("--artifact-dir".to_string());
            cmdargs.push(
                cargo_out_dir
                    .join(i.to_string())
                    .to_str()
                    .unwrap()
                    .to_string(),
            );
            cmdargs.push("--features".to_string());
            cmdargs.push(flavor.features.join(",").to_string());
            cmdargs.push("--target".to_string());
            cmdargs.push(flavor.target.to_string());
            cmdargs.push("--".to_string());
            cmdargs.push("-Zunstable-options".to_string());
            cmdargs.push("--static-root-path".to_string());
            cmdargs.push("/static/".to_string());

            for (dep_name, dep) in &manifest.dependencies {
                if let Some(_) = &dep.path {
                    cmdargs.push(format!(
                        "--extern-html-root-url={}=/__DOCSERVER_DEPLINK/{}/",
                        dep_name.replace('-', "_"),
                        dep_name,
                    ));
                }
            }

            let line = shell_words::join(cmdargs);

            writeln!(stdin, "{}", &line)?;
            writeln!(debug, "    --- {}", &line)?;
        }
    }

    println!("Running cargo batch with {} flavors...", flavors.len());
    let status = child
        .wait_with_output()
        .expect("failed to execute process")
        .status;

    if !status.success() {
        println!("===============");
        println!("failed to execute cmd : {:?}", cmd);
        println!("{}", debug);
        println!("exit code : {:?}", status);
        println!("===============");
        process::exit(1);
    }

    drop(debug);

    // Create flavors directory in output
    let flavors_dir = build_output_dir.join("flavors");
    fs::create_dir_all(&flavors_dir)?;

    let crate_name = &manifest.package.name;
    let mut statics_copied = false;

    // Process all flavors serially
    for (i, flavor) in flavors.iter().enumerate() {
        println!("processing {:?} ...", flavor);
        let doc_dir = cargo_out_dir.join(i.to_string());
        let doc_crate_dir = doc_dir.join(crate_name.replace('-', "_"));

        // Move search files to the crate directory if they exist
        let search_desc = doc_dir.join("search.desc");
        if search_desc.exists() {
            fs::rename(&search_desc, doc_crate_dir.join("search.desc")).unwrap();
        }

        // new search index (post nightly-2025-08-xx)
        let search_index = doc_dir.join("search.index");
        if search_index.exists() {
            fs::rename(&search_index, doc_crate_dir.join("search.index")).unwrap();
        }

        // old search index (pre nightly-2025-08-xx)
        let search_index = doc_dir.join("search-index.js");
        if search_index.exists() {
            let bytes = fs::read(&search_index).unwrap();
            fs::write(doc_crate_dir.join("search-index.js"), &bytes).unwrap();
        }

        // Create flavor directory in output
        let flavor_output_dir = flavors_dir.join(&flavor.name);
        fs::create_dir_all(&flavor_output_dir)?;

        // Copy and process the documentation files
        copy_and_process_dir(&doc_crate_dir, &flavor_output_dir, crate_name)?;

        // Copy static files only once
        if let Some(static_path) = &args.output_static {
            if !statics_copied {
                fs::create_dir_all(static_path).unwrap();
                // recursive copy
                let doc_static_dir = doc_dir.join("static.files");
                let mut stack = vec![doc_static_dir.clone()];
                while let Some(path) = stack.pop() {
                    if path.is_dir() {
                        for entry in fs::read_dir(path).unwrap() {
                            stack.push(entry.unwrap().path());
                        }
                    } else {
                        let rel_path = path.strip_prefix(&doc_static_dir).unwrap();
                        let target_path = static_path.join(rel_path);
                        let _ = fs::create_dir_all(target_path.parent().unwrap());
                        fs::copy(path, target_path).unwrap();
                    }
                }
                statics_copied = true;
            }
        }
    }

    // Write the manifest and info files to the output directory
    fs::write(build_output_dir.join("Cargo.toml"), manifest_bytes)?;
    fs::write(build_output_dir.join("info.json"), docserver_info_bytes)?;

    if is_zup_output {
        // Create the final .zup archive
        println!("Creating .zup archive: {:?}", args.output);

        // Create output directory for .zup file if it doesn't exist
        if let Some(parent) = args.output.parent() {
            fs::create_dir_all(parent)?;
        }

        let compress = args.compression.to_config();

        pack(&build_output_dir, &args.output, compress)?;

        println!("Archive created: {:?}", args.output);
    } else {
        println!("Output written to: {:?}", build_output_dir);
    }

    Ok(())
}
