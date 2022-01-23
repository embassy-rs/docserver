#![feature(io_error_more)]

use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Method, Request, Response, Server, StatusCode};
use log::info;
use std::collections::HashMap;
use std::convert::Infallible;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::{env, fs};

use crate::zup::read::Reader;

#[path = "../zup/mod.rs"]
mod zup;

struct Thing {
    static_path: PathBuf,
    crates_path: PathBuf,
    zups: Mutex<HashMap<PathBuf, &'static Reader>>,
}

impl Thing {
    fn open_zup(&self, path: &Path) -> io::Result<&'static Reader> {
        let mut zups = self.zups.lock().unwrap();
        if let Some(zup) = zups.get(path) {
            Ok(zup)
        } else {
            let zup = Reader::new(path)?;
            let zup = Box::leak(Box::new(zup));
            zups.insert(path.to_owned(), zup);
            Ok(zup)
        }
    }

    fn crate_zup(&self, krate: &str, version: &str) -> io::Result<&'static Reader> {
        let zup_path = self
            .crates_path
            .join(krate)
            .join(format!("{}.zup", version));
        self.open_zup(&zup_path)
    }

    fn list_crates(&self) -> io::Result<Vec<String>> {
        let mut res = Vec::new();
        for f in fs::read_dir(&self.crates_path)? {
            let f = f?;
            res.push(f.file_name().to_str().unwrap().to_string())
        }
        res.sort();
        Ok(res)
    }

    fn list_crate_versions(&self, krate: &str) -> io::Result<Vec<String>> {
        let path = self.crates_path.join(krate);

        let mut res = Vec::new();
        for f in fs::read_dir(&self.crates_path)? {
            let f = f?;
            let name = f.file_name();
            let name = name.to_str().unwrap();
            if let Some(name) = name.strip_suffix(".zup") {
                res.push(name.to_string())
            }
        }
        res.sort_by(|a, b| b.cmp(a)); // reverse
        Ok(res)
    }

    fn resp_404(&self) -> anyhow::Result<Response<Body>> {
        let mut r = Response::new(Body::from("404 Not Found"));
        *r.status_mut() = StatusCode::NOT_FOUND;
        Ok(r)
    }

    fn resp_500(&self, e: anyhow::Error) -> Response<Body> {
        log::error!("{:?}", e);
        let mut r = Response::new(Body::from("500 Internal Server Error"));
        *r.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
        r
    }

    fn resp_405(&self) -> anyhow::Result<Response<Body>> {
        let mut r = Response::new(Body::from("405 Method Not Allowed"));
        *r.status_mut() = StatusCode::METHOD_NOT_ALLOWED;
        Ok(r)
    }

    async fn serve_static(&self, path: &str) -> anyhow::Result<Response<Body>> {
        let path = self.static_path.join(path);
        let data = match fs::read(path) {
            Err(e) if e.kind() == ErrorKind::NotFound => return self.resp_404(),
            x => x?,
        };
        Ok(Response::new(Body::from(data)))
    }

    async fn serve_inner(&self, req: Request<Body>) -> anyhow::Result<Response<Body>> {
        if req.method() != Method::GET {
            return self.resp_405();
        }

        let raw_path = &req.uri().path()[..];
        let mut path = Vec::new();
        for x in raw_path.split('/') {
            match x {
                "" | "." => {}
                ".." => {
                    path.pop();
                }
                _ => path.push(x),
            }
        }

        match &path[..] {
            // Serve static file
            &["static", ref path @ ..] => self.serve_static(&path.join("/")).await,

            // List crates
            &[] => {
                let crates = self.list_crates()?;
                println!("crates: {:?}", crates);
                Ok(Response::new(Body::from("list krates")))
            }

            // List crate versions
            &[krate] => {
                //asdfa
                Ok(Response::new(Body::from("list krate versions")))
            }

            // List crate targets
            &[krate, version] => {
                // lol
                Ok(Response::new(Body::from(
                    "list krate targets for a version",
                )))
            }

            // Get flie from crate version+target
            &[krate, version, _target, ..] => {
                let zup = match self.crate_zup(krate, version) {
                    Err(e) if e.kind() == ErrorKind::NotFound => return self.resp_404(),
                    x => x?,
                };
                let data = match zup.read(&path[2..]) {
                    Err(e) if e.kind() == ErrorKind::NotFound => return self.resp_404(),
                    x => x?,
                };

                Ok(Response::new(Body::from(data)))
            }
            _ => self.resp_404(),
        }
    }

    pub async fn serve(&self, req: Request<Body>) -> Response<Body> {
        let method = req.method().clone();
        let uri = req.uri().clone();

        let resp = match self.serve_inner(req).await {
            Ok(resp) => resp,
            Err(e) => self.resp_500(e),
        };
        info!("{} {}: {}", method, uri, resp.status());
        resp
    }
}

#[tokio::main]
pub async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    pretty_env_logger::init();

    let thing = Thing {
        static_path: PathBuf::from("./webroot/static/"),
        crates_path: PathBuf::from("./webroot/crates/"),
        zups: Mutex::new(HashMap::new()),
    };
    let thing: &'static Thing = Box::leak(Box::new(thing));

    let addr = ([127, 0, 0, 1], 3000).into();
    let server = Server::bind(&addr).serve(make_service_fn(move |_conn| async move {
        Ok::<_, Infallible>(service_fn(move |req| async move {
            Result::<_, Infallible>::Ok(thing.serve(req).await)
        }))
    }));

    println!("Listening on http://{}", addr);

    server.await?;

    Ok(())
}
