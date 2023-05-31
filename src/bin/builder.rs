#![feature(io_error_more)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::sync::Mutex;
use std::{fs, io};

use clap::Parser;
use crossbeam::channel::unbounded;
use crossbeam::thread;
use regex::bytes::Regex as ByteRegex;
use regex::Regex;

use docserver::manifest;
use docserver::zup;
use docserver::zup::write::*;

fn pack_config(crate_name: &str) -> PackConfig {
    let crate_name = crate_name.replace('-', "_");

    // Remove settings button (it breaks due to the path rewriting, we'll provide our own version)
    let re_remove_settings = ByteRegex::new("<a id=\"settings-menu\".*?</a>").unwrap();

    // Remove srclinks that point to a file starting with `_`.
    let re_remove_hidden_src =
        ByteRegex::new("<a class=\"srclink[a-zA-Z0-9 ]*\" href=\"[^\"]*/_[^\"]*\">source</a>")
            .unwrap();

    // Rewrite srclinks from `../../crate_name/foo" to "/__DOCSERVER_SRCLINK/foo".
    let re_rewrite_src = ByteRegex::new(&format!(
        "<a class=\"srclink([a-zA-Z0-9 ]*)\" href=\"(\\.\\./)+src/{}",
        &crate_name
    ))
    .unwrap();

    // Remove crates.js
    let re_remove_cratesjs =
        ByteRegex::new("<script src=\"(\\.\\./)+crates.js\"></script>").unwrap();

    // Rewrite links from `../crate_name" to "".
    let re_rewrite_root = ByteRegex::new(&format!("\\.\\./{}/", &crate_name)).unwrap();

    let re_fix_root_path = ByteRegex::new("data-root-path=\"\\.\\./").unwrap();

    PackConfig {
        file_filter: Box::new(|path| {
            path.file_name().map_or(true, |f| {
                f != "implementors"
                    && !f.to_str().unwrap().starts_with('_')
                    && !path.ends_with("!.html")
            })
        }),
        data_filter: Box::new(move |path, data| {
            if path.to_str().unwrap().ends_with(".html") {
                let res = &data;
                let res = re_remove_settings.replace_all(&res, &[][..]).into_owned();
                let res = re_remove_hidden_src.replace_all(&res, &[][..]).into_owned();
                let res = re_remove_cratesjs
                    .replace_all(
                        &res,
                        format!(
                            "<script type=\"text/javascript\">window.ALL_CRATES=[\"{}\"];</script>",
                            crate_name
                        )
                        .as_bytes(),
                    )
                    .into_owned();
                let res = re_rewrite_src
                    .replace_all(
                        &res,
                        &b"<a class=\"srclink$1\" href=\"/__DOCSERVER_SRCLINK"[..],
                    )
                    .into_owned();
                let res = re_rewrite_root.replace_all(&res, &[][..]).into_owned();
                let res = re_fix_root_path
                    .replace_all(&res, &b"data-root-path=\"./"[..])
                    .into_owned();
                *data = res;
            }
        }),
    }
}

#[derive(Debug)]
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

    for rule in &docs.flavors {
        let mut name_feats: Vec<(String, Vec<String>)> = Vec::new();
        match (&rule.name, &rule.regex_feature) {
            (Some(name), None) => name_feats.push((name.clone(), vec![])),
            (None, Some(re)) => {
                let re = Regex::new(&format!("^{}$", re)).unwrap();
                for feature in manifest.features.keys().filter(|s| re.is_match(s)) {
                    name_feats.push((feature.clone(), vec![feature.clone()]))
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

fn match_flavor<'a>(local: &Flavor, dep: &'a [Flavor]) -> Option<&'a Flavor> {
    // Match by name.
    if let Some(f) = dep.iter().find(|f| f.name == local.name) {
        return Some(f);
    }

    // Match by target.
    if let Some(f) = dep.iter().find(|f| f.target == local.target) {
        return Some(f);
    }

    // Just pick any, or none if there are no flavors.
    dep.get(0)
}

#[derive(clap::Parser)]
#[clap(version = "1.0", author = "Dario Nieuwenhuis <dirbaio@dirbaio.net>")]
struct Cli {
    /// Input crate directory (the directory containing the Cargo.toml)
    #[clap(short)]
    input: PathBuf,

    /// Output .zup
    #[clap(short)]
    output: PathBuf,

    /// Output directory containing static files.
    #[clap(long)]
    output_static: Option<PathBuf>,

    /// Compress output with zstd
    #[clap(short, env = "BUILDER_COMPRESS")]
    compress: bool,

    #[clap(long, default_value = "7", env = "BUILDER_COMPRESS_LEVEL")]
    compress_level: i32,

    /// Compress dictionary size
    #[clap(long, default_value = "163840", env = "BUILDER_DICT_SIZE")]
    dict_size: usize,

    /// Compress dictionary training set max size
    #[clap(long, default_value = "100000000", env = "BUILDER_DICT_TRAIN_SIZE")]
    dict_train_size: usize,

    /// Compress dictionary training set max size
    #[clap(short = 'j', env = "BUILDER_THREADS")]
    num_threads: Option<usize>,
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();

    let mut zup_tree = zup::write::Tree::new();
    let mut zup_flavors = Vec::new();

    let num_threads = cli.num_threads.unwrap_or(1);
    println!("using {} threads", num_threads);

    let m = Mutex::new((&mut zup_tree, &mut zup_flavors));

    let manifest_bytes = load_manifest_bytes(&cli.input);
    let manifest = load_manifest(&cli.input);

    let mut cmd = Command::new("git");
    cmd.args(&["rev-parse", "HEAD"]);
    cmd.current_dir(&cli.input);
    let output = cmd.output().unwrap();
    assert!(output.status.success());
    let docserver_info = manifest::DocserverInfo {
        git_commit: String::from_utf8(output.stdout).unwrap(),
    };
    let docserver_info_bytes = serde_json::to_vec(&docserver_info).unwrap();

    let (tx, rx) = unbounded();
    for flavor in calc_flavors(&manifest) {
        tx.send(flavor).unwrap();
    }
    drop(tx);

    let statics_copied: &Mutex<bool> = &Mutex::new(false);
    let static_path = &cli.output_static;

    thread::scope(|s| {
        // Spawn workers
        for i in 0..num_threads {
            let j = i;
            let rx = &rx;
            let crate_path = &cli.input;
            let manifest = &manifest;
            let m = &m;
            s.spawn(move |_| {
                let crate_name = &manifest.package.name;
                let pack_config = pack_config(crate_name);
                let target_dir = format!("target_doc/work{}", j);

                while let Ok(flavor) = rx.recv() {
                    println!("documenting {:?} ...", flavor);
                    let doc_dir = PathBuf::from(&target_dir).join(&flavor.target).join("doc");
                    let _ = fs::remove_dir_all(&doc_dir);

                    let mut cmd = Command::new("cargo");
                    cmd.args([
                        "rustdoc",
                        "--target-dir",
                        &target_dir,
                        "--manifest-path",
                        crate_path.join("Cargo.toml").to_str().unwrap(),
                        "--features",
                        &flavor.features.join(","),
                        "--target",
                        &flavor.target,
                        "-Zunstable-options",
                        "-Zrustdoc-map",
                        "--",
                        "-Zunstable-options",
                        "--disable-per-crate-search",
                        "--static-root-path",
                        "/static/",
                    ]);

                    for (dep_name, dep) in &manifest.dependencies {
                        if let Some(_) = &dep.path {
                            cmd.arg(format!(
                                "--extern-html-root-url={}=/__DOCSERVER_DEPLINK/{}/",
                                dep_name.replace('-', "_"),
                                dep_name,
                            ));
                        }
                    }

                    let output = cmd.output().expect("failed to execute process");

                    let (zup_tree, zup_flavors) = &mut *m.lock().unwrap();

                    if !output.status.success() {
                        println!("===============");
                        println!("failed to execute cmd : {:?}", cmd);
                        println!("exit code : {:?}", cmd.status());
                        println!("=============== STDOUT");
                        io::stdout().write_all(&output.stdout).unwrap();
                        println!("=============== STDERR");
                        io::stdout().write_all(&output.stderr).unwrap();
                        println!("===============");

                        process::exit(1);
                    }

                    let doc_crate_dir = doc_dir.join(crate_name.replace('-', "_"));

                    let bytes = fs::read(doc_dir.join("search-index.js")).unwrap();
                    fs::write(doc_crate_dir.join("search-index.js"), &bytes).unwrap();

                    let dir = zup_tree
                        .pack(&doc_crate_dir, &pack_config)
                        .unwrap()
                        .unwrap();
                    zup_flavors.push(DirectoryEntry {
                        name: flavor.name.clone(),
                        node_id: dir,
                    });

                    //fs::remove_dir_all(doc_crate_dir).unwrap();
                    //fs::remove_dir_all(doc_dir.join("src")).unwrap();
                    //fs::remove_dir_all(doc_dir.join("implementors")).unwrap();
                    //fs::remove_file(doc_dir.join("crates.js")).unwrap();
                    //fs::remove_file(doc_dir.join("source-files.js")).unwrap();

                    if let Some(static_path) = static_path {
                        let copy_done =
                            std::mem::replace(&mut *statics_copied.lock().unwrap(), true);
                        if !copy_done {
                            fs::create_dir_all(static_path).unwrap();
                            // recursive copy
                            let mut stack = vec![doc_dir.join("static.files")];
                            while let Some(path) = stack.pop() {
                                if path.is_dir() {
                                    for entry in fs::read_dir(path).unwrap() {
                                        stack.push(entry.unwrap().path());
                                    }
                                } else {
                                    let rel_path = path.strip_prefix(&doc_dir).unwrap();
                                    let target_path = static_path.join(rel_path);
                                    let _ = fs::create_dir_all(target_path.parent().unwrap());
                                    fs::copy(path, target_path).unwrap();
                                }
                            }
                        }
                    }
                }
            });
        }
    })
    .unwrap();

    if let Some(p) = cli.output.parent() {
        let _ = fs::create_dir_all(p);
    }

    println!("total nodes: {}", zup_tree.node_count());
    println!("total bytes: {}", zup_tree.total_bytes());

    let zup_flavors = zup_tree.add_node(Node::Directory(Directory {
        entries: zup_flavors,
    }));

    let zup_root = Node::Directory(Directory {
        entries: vec![
            DirectoryEntry {
                name: "flavors".to_string(),
                node_id: zup_flavors,
            },
            DirectoryEntry {
                name: "Cargo.toml".to_string(),
                node_id: zup_tree.add_node(Node::File(File {
                    data: manifest_bytes,
                })),
            },
            DirectoryEntry {
                name: "info.json".to_string(),
                node_id: zup_tree.add_node(Node::File(File {
                    data: docserver_info_bytes,
                })),
            },
        ],
    });

    let zup_root = zup_tree.add_node(zup_root);
    let compress = cli.compress.then(|| CompressConfig {
        level: cli.compress_level,
        dict_size: cli.dict_size,
        dict_train_size: cli.dict_train_size,
    });
    zup_tree.write(&cli.output, zup_root, compress)?;

    Ok(())
}
