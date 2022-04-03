#![feature(io_error_more)]
#![feature(let_else)]

use hyper::header::HeaderValue;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Method, Request, Response, Server, StatusCode};
use log::info;
use regex::bytes::Regex as ByteRegex;
use std::convert::Infallible;
use std::io::{self, ErrorKind};
use std::path::PathBuf;
use std::{env, fs};
use tera::Tera;

use crate::zup::read::Reader;

#[path = "../zup/mod.rs"]
mod zup;

fn extension(path: &str) -> &str {
    match path.rfind('.') {
        Some(x) => &path[x + 1..],
        None => "",
    }
}

fn mime_type(extension: &str) -> &'static str {
    match extension {
        "html" => "text/html",
        "css" => "text/css",
        "js" => "application/javascript",
        "json" => "application/json",
        "ttf" => "application/x-font-ttf",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" => "image/jpeg",
        "txt" => "text/plain",
        _ => "application/octet-stream", // unknown
    }
}

struct Thing {
    static_path: PathBuf,
    crates_path: PathBuf,
    templates: Tera,
}

impl Thing {
    fn crate_zup(&self, krate: &str, version: &str) -> io::Result<Reader> {
        let zup_path = self
            .crates_path
            .join(krate)
            .join(format!("{}.zup", version));
        Reader::new(&zup_path)
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
        let _path = self.crates_path.join(krate);

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
                Ok(Response::new(Body::from("TODO: list crates")))
            }

            // List crate versions
            &[_krate] => {
                //asdfa
                Ok(Response::new(Body::from("TODO: list crate versions")))
            }

            // List crate flavors
            &[_krate, _version] => {
                // lol
                Ok(Response::new(Body::from(
                    "TODO: list crate flavors for a version",
                )))
            }

            // Get file from crate version+flavor
            &[krate, version, _flavor, ..] => {
                let zup = match self.crate_zup(krate, version) {
                    Err(e) if e.kind() == ErrorKind::NotFound => return self.resp_404(),
                    x => x?,
                };
                let mut data = match zup.read(&path[2..]) {
                    Err(e) if e.kind() == ErrorKind::NotFound => return self.resp_404(),
                    x => x?,
                }
                .into_owned();

                let ext = extension(path[path.len() - 1]);
                let mime = mime_type(ext);

                if ext == "html" {
                    let re_head = ByteRegex::new("</head>").unwrap();
                    let re_body = ByteRegex::new("<body class=\"([^\"]*)\">").unwrap();
                    if let (Some(head), Some(body)) = (re_head.find(&data), re_body.captures(&data))
                    {
                        let m = body.get(0).unwrap();
                        let mut data2 = Vec::new();
                        data2.extend_from_slice(&data[..head.start()]);
                        data2.extend_from_slice(&fs::read("templates/head.html").unwrap());
                        data2.extend_from_slice(&data[head.start()..m.start()]);
                        data2.extend_from_slice(b"<body>");
                        data2.extend_from_slice(&fs::read("templates/nav.html").unwrap());
                        data2.extend_from_slice(b"<div class=\"body-wrapper ");
                        data2.extend_from_slice(&body[1]);
                        data2.extend_from_slice(b"\">");
                        data2.extend_from_slice(&data[m.end()..]);
                        data = data2;
                    }
                }

                let mut resp = Response::new(Body::from(data));
                resp.headers_mut()
                    .insert("Content-Type", HeaderValue::from_static(mime.into()));
                Ok(resp)
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

async fn shutdown_signal() {
    // Wait for the CTRL+C signal
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install CTRL+C signal handler");
}

#[tokio::main]
pub async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    pretty_env_logger::init();

    let templates = Tera::new("templates/**/*.html").unwrap();

    let static_path: PathBuf = env::var_os("DOCSERVER_STATIC_PATH")
        .expect("Missing DOCSERVER_STATIC_PATH")
        .into();
    let crates_path: PathBuf = env::var_os("DOCSERVER_CRATES_PATH")
        .expect("Missing DOCSERVER_CRATES_PATH")
        .into();

    let thing = Thing {
        static_path,
        crates_path,
        templates,
    };
    let thing: &'static Thing = Box::leak(Box::new(thing));

    let addr = ([0, 0, 0, 0], 3000).into();
    let server = Server::bind(&addr).serve(make_service_fn(move |_conn| async move {
        Ok::<_, Infallible>(service_fn(move |req| async move {
            Result::<_, Infallible>::Ok(thing.serve(req).await)
        }))
    }));

    let server = server.with_graceful_shutdown(shutdown_signal());

    println!("Listening on http://{}", addr);

    server.await?;

    Ok(())
}
