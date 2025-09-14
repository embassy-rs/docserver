use clap::Parser;
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use crate::common::zup::{
    layout,
    read::{Node, Reader},
};

struct Walker {
    files: usize,
    bytes: usize,
    visited: HashSet<layout::Node>,
}

impl Walker {
    pub fn new() -> Self {
        Self {
            visited: HashSet::new(),
            bytes: 0,
            files: 0,
        }
    }

    pub fn walk(&mut self, n: Node<'_>, path: &PathBuf) {
        if !self.visited.insert(n.node()) {
            return;
        }

        println!("{}", path.display());
        match n {
            Node::Directory(n) => {
                fs::create_dir(path).unwrap();
                for (name, c) in n.children().unwrap() {
                    self.walk(c, &path.join(name));
                }
            }
            Node::File(n) => {
                self.files += 1;
                self.bytes += n.read().unwrap().len();
                fs::write(path, n.read().unwrap()).unwrap();
            }
        }
    }
}

#[derive(Parser)]
pub struct UnzupArgs {
    /// Path to the .zup archive to extract
    pub archive: PathBuf,
    /// Destination directory to extract to
    #[clap(short, long)]
    pub destination: PathBuf,
}

pub async fn run(args: UnzupArgs) -> anyhow::Result<()> {
    // Check if destination exists
    if args.destination.exists() {
        return Err(anyhow::anyhow!(
            "Destination directory '{}' already exists. Please remove it or choose a different destination.",
            args.destination.display()
        ));
    }

    let zup = Reader::new(&args.archive)?;

    let mut w = Walker::new();
    w.walk(zup.root_node(), &args.destination);
    println!("files {}", w.files);
    println!("bytes {}", w.bytes);

    Ok(())
}
