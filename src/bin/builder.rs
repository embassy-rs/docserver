#![feature(io_error_more)]
#![feature(let_else)]

use std::borrow::Cow;
use std::env::{self};
use std::io::Write;
use std::path::PathBuf;
use std::process::{self, Command};
use std::sync::Mutex;
use std::{fs, io};

use crossbeam::channel::unbounded;
use crossbeam::thread;
use regex::bytes::Regex as ByteRegex;
use regex::Regex;

#[path = "../manifest.rs"]
mod manifest;
#[path = "../zup/mod.rs"]
mod zup;

use zup::write::*;

fn pack_config(crate_name: &str) -> PackConfig {
    let crate_name = crate_name.replace('-', "_");

    // Remove settings button (it breaks due to the path rewriting, we'll provide our own version)
    let re_remove_settings = ByteRegex::new("<a id=\"settings-menu\".*?</a>").unwrap();

    // Remove srclinks that point to a file starting with `_`.
    let re_remove_hidden_src =
        ByteRegex::new("<a class=\"srclink\" href=\"[^\"]*/_[^\"]*\">source</a>").unwrap();

    // Rewrite srclinks from `../../crate_name/foo" to "/__DOCSERVER_SRCLINK/foo".
    let re_rewrite_src = ByteRegex::new(&format!(
        "<a class=\"srclink\" href=\"(\\.\\./)+src/{}",
        &crate_name
    ))
    .unwrap();

    // Remove crates.js
    let re_remove_cratesjs =
        ByteRegex::new("<script src=\"(\\.\\./)+crates.js\"></script>").unwrap();

    // Rewrite links from `../crate_name" to "".
    let re_rewrite_root = ByteRegex::new(&format!("\\.\\./{}/", &crate_name)).unwrap();

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
                let res = re_remove_cratesjs.replace_all(&res, &[][..]).into_owned();
                let res = re_rewrite_src
                    .replace_all(
                        &res,
                        &b"<a class=\"srclink\" href=\"/__DOCSERVER_SRCLINK"[..],
                    )
                    .into_owned();
                let res = re_rewrite_root.replace_all(&res, &[][..]).into_owned();
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

fn main() -> io::Result<()> {
    let mut zup_tree = zup::write::Tree::new();
    let mut zup_flavors = Vec::new();

    let args: Vec<_> = env::args().collect();
    let manifest_path = PathBuf::from(&args[1]);
    let output_path = PathBuf::from(&args[2]);

    let m = Mutex::new((&mut zup_tree, &mut zup_flavors));

    let manifest_bytes = fs::read(&manifest_path).unwrap();
    let manifest: manifest::Manifest = toml::from_slice(&manifest_bytes).unwrap();
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
        for i in 0..6 {
            let j = i;
            let rx = &rx;
            let manifest_path = &manifest_path;
            let m = &m;
            let crate_name = &manifest.package.name;
            s.spawn(move |_| {
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
                        manifest_path.to_str().unwrap(),
                        "--features",
                        &flavor.features.join(","),
                        "--target",
                        &flavor.target,
                        "--",
                        "-Z",
                        "unstable-options",
                        "--static-root-path",
                        "/__DOCSERVER_STATIC/",
                    ]);

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

                    fs::rename(
                        doc_dir.join("search-index.js"),
                        doc_crate_dir.join("search-index.js"),
                    )
                    .unwrap();

                    let dir = zup_tree
                        .pack(&doc_crate_dir, &pack_config)
                        .unwrap()
                        .unwrap();
                    zup_flavors.push(DirectoryEntry {
                        name: flavor.name.clone(),
                        node_id: dir,
                    });

                    fs::remove_dir_all(doc_crate_dir).unwrap();
                    fs::remove_dir_all(doc_dir.join("src")).unwrap();
                    fs::remove_dir_all(doc_dir.join("implementors")).unwrap();
                    fs::remove_file(doc_dir.join("crates.js")).unwrap();
                    fs::remove_file(doc_dir.join("source-files.js")).unwrap();
                }
            });
        }
    })
    .unwrap();

    if let Some(p) = output_path.parent() {
        let _ = fs::create_dir_all(p);
    }

    println!("total nodes: {}", zup_tree.node_count());
    println!("total bytes: {}", zup_tree.total_bytes());
    println!("compressing...");

    let zup_flavors = zup_tree.add_node(Node::Directory(Directory {
        entries: zup_flavors,
    }));

    let zup_manifest = zup_tree.add_node(Node::File(File {
        data: manifest_bytes,
    }));

    let zup_root = Node::Directory(Directory {
        entries: vec![
            DirectoryEntry {
                name: "flavors".to_string(),
                node_id: zup_flavors,
            },
            DirectoryEntry {
                name: "Cargo.toml".to_string(),
                node_id: zup_manifest,
            },
        ],
    });

    let zup_root = zup_tree.add_node(zup_root);
    zup_tree.write(&output_path, zup_root)?;

    Ok(())
}
