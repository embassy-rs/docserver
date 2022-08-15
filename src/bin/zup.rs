#![feature(io_error_more)]
#![feature(let_else)]

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use zup::layout;
use zup::read::Node;

use docserver::zup;
use docserver::zup::read::Reader;

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

    pub fn walk(&mut self, n: Node<'_>, path: String) {
        if !self.visited.insert(n.node()) {
            return;
        }

        println!("{}", path);
        match n {
            Node::Directory(n) => {
                for (name, c) in n.children().unwrap() {
                    self.walk(c, format!("{}/{}", path, name));
                }
            }
            Node::File(n) => {
                self.files += 1;
                self.bytes += n.read().unwrap().len();
                let p = PathBuf::from("extract".to_string()).join(path);
                fs::create_dir_all(p.parent().unwrap()).unwrap();
                fs::write(p, n.read().unwrap()).unwrap();
            }
        }
    }
}

pub fn main() {
    pretty_env_logger::init();

    let zup = Reader::new("./webroot/crates/embassy-stm32/git.zup").unwrap();

    let mut w = Walker::new();
    w.walk(zup.root_node(), "extract".to_string());
    println!("files {}", w.files);
    println!("byes {}", w.bytes);
}
