#![feature(io_error_more)]
#![feature(let_else)]

use std::env::{self};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::sync::Mutex;
use std::{fs, io};

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

    // Rewrite links from `__REMOVE_NEXT_PATH_COMPONENT__/blah" to "".
    let re_remove_next_path_component =
        ByteRegex::new("__REMOVE_NEXT_PATH_COMPONENT__/[a-zA-Z0-9_-]+/").unwrap();

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
                let res = re_remove_next_path_component
                    .replace_all(&res, &[][..])
                    .into_owned();
                let res = re_rewrite_src
                    .replace_all(
                        &res,
                        &b"<a class=\"srclink\" href=\"/__DOCSERVER_SRCLINK"[..],
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

fn main() -> io::Result<()> {
    let mut zup_tree = zup::write::Tree::new();
    let mut zup_flavors = Vec::new();

    let mut num_threads = 1usize;
    if let Ok(v) = env::var("BUILDER_THREADS") {
        if let Ok(n) = v.parse() {
            num_threads = n;
        }
    }
    println!("using {} threads", num_threads);

    let args: Vec<_> = env::args().collect();
    let crate_path = PathBuf::from(&args[1]);
    let output_path = PathBuf::from(&args[2]);

    let m = Mutex::new((&mut zup_tree, &mut zup_flavors));

    let manifest_bytes = load_manifest_bytes(&crate_path);
    let manifest = load_manifest(&crate_path);

    let mut cmd = Command::new("git");
    cmd.args(&["rev-parse", "HEAD"]);
    cmd.current_dir(&crate_path);
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

    thread::scope(|s| {
        // Spawn workers
        for i in 0..num_threads {
            let j = i;
            let rx = &rx;
            let crate_path = &crate_path;
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
                        "/__DOCSERVER_STATIC/",
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
                    fs::write(
                        doc_crate_dir.join("search-index.js"),
                        &bytes
                    )
                    .unwrap();


                    // monkeypatch search.js to remove the crate name from the path.
                    let js = fs::read(doc_dir.join("search.js")).unwrap();
                    let monkeypatch = ByteRegex::new("return\\[displayPath,href\\]").unwrap();
                    let js = monkeypatch.replace_all(&js, &b"href=href.slice(ROOT_PATH.length);href=ROOT_PATH+href.slice(href.indexOf('/')+1);return[displayPath,href]"[..]);
                    fs::write(
                        doc_crate_dir.join("search.js"),
                        &js
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

                    //fs::remove_dir_all(doc_crate_dir).unwrap();
                    //fs::remove_dir_all(doc_dir.join("src")).unwrap();
                    //fs::remove_dir_all(doc_dir.join("implementors")).unwrap();
                    //fs::remove_file(doc_dir.join("crates.js")).unwrap();
                    //fs::remove_file(doc_dir.join("source-files.js")).unwrap();
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
    zup_tree.write(&output_path, zup_root)?;

    Ok(())
}
