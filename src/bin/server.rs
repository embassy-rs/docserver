use http_body_util::Full;
use hyper::body::Incoming;
use hyper::header::HeaderValue;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use log::info;
use regex::bytes::{Captures, Regex as ByteRegex};
use std::collections::HashMap;
use std::convert::Infallible;
use std::io::{self, ErrorKind};
use std::path::PathBuf;
use std::{env, fs};
use tera::{Context, Tera};
use tokio::net::TcpListener;

use docserver::manifest;
use docserver::zup::read::{Node, Reader};

type Body = Full<hyper::body::Bytes>;

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
    path: PathBuf,
    templates: Tera,
}

impl Thing {
    fn crates_path(&self) -> PathBuf {
        self.path.join("crates")
    }
    fn crate_path(&self, krate: &str) -> PathBuf {
        self.path.join("crates").join(&krate)
    }

    fn crate_zup(&self, krate: &str, version: &str) -> io::Result<Reader> {
        let zup_path = self
            .path
            .join("crates")
            .join(krate)
            .join(format!("{}.zup", version));
        Reader::new(&zup_path)
    }

    fn list_crates(&self) -> io::Result<Vec<String>> {
        let mut res = Vec::new();
        for f in fs::read_dir(self.crates_path())? {
            let f = f?;
            res.push(f.file_name().to_str().unwrap().to_string())
        }
        res.sort();
        Ok(res)
    }

    fn list_versions(&self, krate: &str) -> io::Result<Vec<String>> {
        let path = self.crate_path(krate);

        let mut res = Vec::new();
        for f in fs::read_dir(path)? {
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

    fn list_flavors(&self, krate: &str, version: &str) -> io::Result<Vec<String>> {
        let zup = self.crate_zup(krate, version)?;
        let flavors = zup.open(&["flavors"])?;
        let Node::Directory(dir) = flavors else {
            panic!("flavors is not a dir")
        };

        let mut res = Vec::new();
        for (name, _) in dir.children()? {
            res.push(name)
        }
        res.sort();
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

    fn resp_redirect(&self, path: &str) -> anyhow::Result<Response<Body>> {
        let mut resp = Response::new(Body::from("Redirect"));
        *resp.status_mut() = StatusCode::FOUND;
        let h = resp.headers_mut();
        h.append("Location", path.try_into().unwrap());
        Ok(resp)
    }

    async fn serve_static(&self, pathh: &str) -> anyhow::Result<Response<Body>> {
        let path = self.path.join("static").join(pathh);
        let data = match fs::read(path) {
            Err(e) if e.kind() == ErrorKind::NotFound => return self.resp_404(),
            x => x?,
        };

        let ext = extension(pathh);
        let mime = mime_type(ext);

        let mut resp = Response::new(Body::from(data));
        let h = resp.headers_mut();
        h.insert("Content-Type", HeaderValue::from_static(mime.into()));
        h.insert(
            "Cache-Control",
            HeaderValue::from_static("max-age=31536000"),
        );
        Ok(resp)
    }

    fn cookies(&self, req: &Request<Incoming>) -> HashMap<String, String> {
        // Parse cookies
        let mut cookies = HashMap::new();
        if let Some(h) = req.headers().get("Cookie") {
            for item in h.to_str().unwrap().split(';') {
                if let Some((k, v)) = item.trim().split_once('=') {
                    cookies.insert(k.to_string(), v.to_string());
                }
            }
        }
        cookies
    }

    async fn guess_redirect(
        &self,
        req: &Request<Incoming>,
        mut krate: Option<&str>,
        mut version: Option<&str>,
    ) -> anyhow::Result<Response<Body>> {
        let cookies = self.cookies(req);

        // Crate
        let krates = self.list_crates()?;
        if krate == None {
            krate = cookies.get(&"crate".to_string()).map(|s| s.as_str());
        }
        let mut krate = krate.unwrap_or("embassy-executor");
        if krates.iter().find(|s| *s == krate).is_none() {
            krate = "embassy-executor";
        }

        // Version
        let versions = self.list_versions(krate)?;
        if version == None {
            version = cookies
                .get(&format!("crate-{}-version", krate))
                .map(|s| s.as_str());
        }
        let mut version = version.unwrap_or(&versions[0]);
        if versions.iter().find(|s| *s == version).is_none() {
            version = &versions[0];
        }

        // Flavor
        let flavors = self.list_flavors(krate, version)?;
        let flavor = cookies
            .get(&format!("crate-{}-flavor", krate))
            .map(|s| s.as_str());
        let mut flavor = flavor.unwrap_or(&flavors[0]);
        if flavors.iter().find(|s| *s == flavor).is_none() {
            flavor = &flavors[0];
        }

        self.resp_redirect(&format!("/{}/{}/{}/index.html", krate, version, flavor))
    }

    async fn serve_inner(&self, req: Request<Incoming>) -> anyhow::Result<Response<Body>> {
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

            &[] => self.guess_redirect(&req, None, None).await,
            &[krate] => self.guess_redirect(&req, Some(krate), None).await,
            &[krate, version] => self.guess_redirect(&req, Some(krate), Some(version)).await,

            // Get file from crate version+flavor
            &[krate, version, flavor, ..] => {
                let zup = match self.crate_zup(krate, version) {
                    Err(e) if e.kind() == ErrorKind::NotFound => return self.resp_404(),
                    x => x?,
                };

                // redirect remove extra crate name in path.
                if path.len() > 3 && path[3] == &krate.replace('-', "_") {
                    return self.resp_redirect(&format!(
                        "/{}/{}/{}/{}",
                        krate,
                        version,
                        flavor,
                        path[4..].join("/")
                    ));
                }

                let mut zup_path = vec!["flavors"];
                zup_path.extend_from_slice(&path[2..]);
                let mut data = match zup.read(&zup_path) {
                    Err(e) if e.kind() == ErrorKind::NotFound => {
                        // check if it's due to incorrect flavor.
                        if path.len() > 3 && zup.open(&["flavors", path[3]]).is_ok() {
                            // if flavor exists, path is wrong, so do 404.
                            return self.resp_404();
                        } else {
                            // flavor doesn't exist, redirect to the default flavor.
                            let cookies = self.cookies(&req);

                            let flavors = self.list_flavors(krate, version)?;
                            let flavor = cookies
                                .get(&format!("crate-{}-flavor", krate))
                                .map(|s| s.as_str());
                            let mut flavor = flavor.unwrap_or(&flavors[0]);
                            if flavors.iter().find(|s| *s == flavor).is_none() {
                                flavor = &flavors[0];
                            }

                            return self.resp_redirect(&format!(
                                "/{}/{}/{}/{}",
                                krate,
                                version,
                                flavor,
                                path[3..].join("/")
                            ));
                        }
                    }
                    Err(e) if e.kind() == ErrorKind::IsADirectory => {
                        return self.resp_redirect(&format!(
                            "/{}/{}/{}/{}index.html",
                            krate,
                            version,
                            flavor,
                            path[3..].iter().fold(String::new(), |mut s, p| {
                                s.push_str(p);
                                s.push('/');
                                s
                            })
                        ))
                    }
                    x => x?,
                }
                .into_owned();

                let ext = extension(path[path.len() - 1]);
                let mime = mime_type(ext);

                if ext == "html" {
                    let manifest = zup.read(&["Cargo.toml"]).unwrap();
                    let manifest: manifest::Manifest = toml::from_slice(&manifest).unwrap();
                    let meta = &manifest.package.metadata.embassy_docs;

                    let info = zup.read(&["info.json"]).unwrap();
                    let info: manifest::DocserverInfo = serde_json::from_slice(&info).unwrap();

                    let srclink_base = if version == "git" {
                        meta.src_base_git
                            .replace("$COMMIT", &info.git_commit)
                            .to_string()
                    } else {
                        meta.src_base.replace("$VERSION", version).to_string()
                    };

                    let re = ByteRegex::new("(src|href)=\"([^\"]+)\"").unwrap();
                    data = re
                        .replace_all(&data, |c: &Captures| {
                            let attr = c.get(1).unwrap().as_bytes();
                            let mut link = c.get(2).unwrap().as_bytes().to_vec();

                            if link.starts_with(b"/__DOCSERVER_SRCLINK/") {
                                let link_path = std::str::from_utf8(&link[21..]).unwrap();
                                let i = link_path.find('#').unwrap();
                                let link_fragment = link_path[i + 1..].replace('-', "-L");
                                let link_path = link_path[..i].replace(".html", "");
                                link = format!("{}{}#L{}", srclink_base, link_path, link_fragment)
                                    .into();
                            }

                            if link.starts_with(b"/__DOCSERVER_DEPLINK/") {
                                let link_path = std::str::from_utf8(&link[21..]).unwrap();
                                let (krate, link_path) = link_path.split_once('/').unwrap();
                                let (_, link_path) = link_path.split_once('/').unwrap();

                                link = format!("/{krate}/git/{flavor}/{link_path}").into();
                            }

                            let mut res = Vec::new();
                            res.extend_from_slice(attr);
                            res.extend_from_slice(b"=");
                            res.extend_from_slice(b"\"");
                            res.extend_from_slice(&link);
                            res.extend_from_slice(b"\"");
                            res
                        })
                        .into_owned();
                    let re_head = ByteRegex::new("</head>").unwrap();
                    let re_body = ByteRegex::new("<body class=\"([^\"]*)\">").unwrap();
                    if let (Some(head), Some(body)) = (re_head.find(&data), re_body.captures(&data))
                    {
                        let mut context = Context::new();
                        context.insert("crate", &krate);
                        context.insert("version", &version);
                        context.insert("flavor", &flavor);
                        context.insert("crates", &self.list_crates().unwrap());
                        context.insert("versions", &self.list_versions(krate).unwrap());
                        context.insert("flavors", &self.list_flavors(krate, version).unwrap());

                        let rendered_head = self.templates.render("head.html", &context).unwrap();
                        let rendered_nav = self.templates.render("nav.html", &context).unwrap();

                        let m = body.get(0).unwrap();
                        let mut data2 = Vec::new();
                        data2.extend_from_slice(&data[..head.start()]);
                        data2.extend_from_slice(rendered_head.as_bytes());
                        data2.extend_from_slice(&data[head.start()..m.start()]);
                        data2.extend_from_slice(b"<body>");
                        data2.extend_from_slice(rendered_nav.as_bytes());
                        data2.extend_from_slice(b"<div class=\"body-wrapper ");
                        data2.extend_from_slice(&body[1]);
                        data2.extend_from_slice(b"\">");
                        data2.extend_from_slice(&data[m.end()..]);
                        data = data2;
                    }
                }

                let mut resp = Response::new(Body::from(data));
                let h = resp.headers_mut();
                h.append("Content-Type", mime.try_into().unwrap());

                let mut set_cookie = |k, v| {
                    h.append(
                        "Set-Cookie",
                        format!("{}={}; Path=/; Max-Age=31536000", k, v)
                            .try_into()
                            .unwrap(),
                    );
                };

                let cookie_version = format!("crate-{}-version", krate);
                let cookie_flavor = format!("crate-{}-flavor", krate);
                set_cookie("crate", &krate);
                set_cookie(&cookie_version, &version);
                set_cookie(&cookie_flavor, &flavor);

                Ok(resp)
            }
        }
    }

    pub async fn serve(&self, req: Request<Incoming>) -> Response<Body> {
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

    let templates = Tera::new("templates/**/*.html").unwrap();

    let path: PathBuf = env::var_os("DOCSERVER_PATH")
        .expect("Missing DOCSERVER_PATH")
        .into();

    let thing = Thing { path, templates };
    let thing: &'static Thing = Box::leak(Box::new(thing));

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], 3000));
    let listener = TcpListener::bind(addr).await?;

    println!("Listening on http://{}", addr);

    loop {
        let (stream, _) = listener.accept().await?;
        let io = TokioIo::new(stream);

        tokio::task::spawn(async move {
            if let Err(err) = http1::Builder::new()
                .serve_connection(
                    io,
                    service_fn(move |req| async move {
                        Result::<_, Infallible>::Ok(thing.serve(req).await)
                    }),
                )
                .await
            {
                println!("Error serving connection: {:?}", err);
            }
        });
    }
}
