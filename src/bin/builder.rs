#![feature(io_error_more)]

use std::env::{self, args};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::sync::Mutex;
use std::{fs, io};

use crossbeam::channel::unbounded;
use crossbeam::thread;
use regex::Regex;

#[path = "../zup/mod.rs"]
mod zup;
use zup::write::*;

mod manifest {
    use serde::Deserialize;
    use std::collections::HashMap;

    #[derive(Deserialize)]
    pub struct Manifest {
        pub features: HashMap<String, Vec<String>>,
        pub package: Package,
    }

    #[derive(Deserialize)]
    pub struct Package {
        #[serde(default)]
        pub metadata: Metadata,
    }

    #[derive(Deserialize, Default)]
    pub struct Metadata {
        #[serde(default)]
        pub embassy_docs: Docs,
    }

    #[derive(Deserialize, Default)]
    pub struct Docs {
        #[serde(default)]
        pub flavors: Vec<DocsFlavor>,
        #[serde(default)]
        pub features: Vec<String>,
    }

    #[derive(Deserialize)]
    pub struct DocsFlavor {
        // One of either has to be specified
        pub regex_feature: Option<String>,
        pub name: Option<String>,

        #[serde(default)]
        pub features: Vec<String>,
        pub target: String,
    }
}

fn pack_config() -> PackConfig {
    PackConfig {
        file_filter: Box::new(|path| !path.ends_with("!.html")),
        data_filter: Box::new(|path, data| {}),
    }
}

#[derive(Debug)]
struct Flavor {
    name: String,
    features: Vec<String>,
    target: String,
}

fn main() -> io::Result<()> {
    let mut tree = zup::write::Tree::new();
    let mut root = Vec::new();

    let args: Vec<_> = env::args().collect();
    let manifest_path = PathBuf::from(&args[1]);
    let output_path = PathBuf::from(&args[2]);

    let m = Mutex::new((&mut tree, &mut root));

    let manifest: manifest::Manifest =
        toml::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    let docs = &manifest.package.metadata.embassy_docs;

    let mut flavors = Vec::new();
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
                target: rule.target.clone(),
            })
        }
    }

    let (tx, rx) = unbounded();
    for flavor in flavors {
        tx.send(flavor).unwrap();
    }
    drop(tx);

    thread::scope(|s| {
        // Spawn workers
        for i in 0..12 {
            let j = i;
            let rx = &rx;
            let manifest_path = &manifest_path;
            let m = &m;
            s.spawn(move |_| {
                let pack_config = pack_config();
                let target_dir = format!("target/work{}", j);

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
                        manifest_path.to_str().unwrap(),
                        "--features",
                        &flavor.features.join(","),
                        "--target",
                        &flavor.target,
                        "--",
                        "-Z",
                        "unstable-options",
                        "--static-root-path",
                        "/static/",
                    ]);

                    let output = cmd.output().expect("failed to execute process");

                    let (tree, root) = &mut *m.lock().unwrap();

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

                    let dir = tree.pack(&doc_dir, &pack_config).unwrap();
                    root.push(DirectoryEntry {
                        name: flavor.name.clone(),
                        node_id: dir,
                    });
                }
            });
        }
    })
    .unwrap();

    println!("compressing...");
    let root = Node::Directory(Directory { entries: root });
    let root = tree.add_node(root);
    tree.write(&output_path, root)?;

    Ok(())
}
