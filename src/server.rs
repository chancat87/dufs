use crate::{Args, BoxResult};

use async_walkdir::WalkDir;
use async_zip::write::{EntryOptions, ZipFileWriter};
use async_zip::Compression;
use futures::stream::StreamExt;
use futures::TryStreamExt;
use hyper::header::HeaderValue;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Method, StatusCode};
use percent_encoding::percent_decode;
use serde::Serialize;
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;
use tokio::fs::File;
use tokio::io::AsyncWrite;
use tokio::{fs, io};
use tokio_util::codec::{BytesCodec, FramedRead};
use tokio_util::io::{ReaderStream, StreamReader};

type Request = hyper::Request<Body>;
type Response = hyper::Response<Body>;

macro_rules! status_code {
    ($status:expr) => {
        hyper::Response::builder()
            .status($status)
            .body($status.canonical_reason().unwrap_or_default().into())
            .unwrap()
    };
}

const INDEX_HTML: &str = include_str!("index.html");
const INDEX_CSS: &str = include_str!("index.css");
const BUF_SIZE: usize = 1024 * 16;

pub async fn serve(args: Args) -> BoxResult<()> {
    let address = args.address()?;
    let inner = Arc::new(InnerService::new(args));
    let make_svc = make_service_fn(move |_| {
        let inner = inner.clone();
        async {
            Ok::<_, Infallible>(service_fn(move |req| {
                let inner = inner.clone();
                inner.call(req)
            }))
        }
    });

    let server = hyper::Server::try_bind(&address)?.serve(make_svc);
    let address = server.local_addr();
    eprintln!("Files served on http://{}", address);
    server.await?;

    Ok(())
}

struct InnerService {
    args: Args,
}

impl InnerService {
    pub fn new(args: Args) -> Self {
        Self { args }
    }

    pub async fn call(self: Arc<Self>, req: Request) -> Result<Response, hyper::Error> {
        let method = req.method().clone();
        let uri = req.uri().clone();
        let res = self
            .handle(req)
            .await
            .unwrap_or_else(|_| status_code!(StatusCode::INTERNAL_SERVER_ERROR));
        info!(r#""{} {}" - {}"#, method, uri, res.status());
        Ok(res)
    }

    pub async fn handle(self: Arc<Self>, req: Request) -> BoxResult<Response> {
        if !self.auth_guard(&req).unwrap_or_default() {
            let mut res = status_code!(StatusCode::UNAUTHORIZED);
            res.headers_mut()
                .insert("WWW-Authenticate", HeaderValue::from_static("Basic"));
            return Ok(res);
        }

        if req.method() == Method::GET {
            self.handle_static(req).await
        } else if req.method() == Method::PUT {
            if self.args.readonly {
                return Ok(status_code!(StatusCode::FORBIDDEN));
            }
            self.handle_upload(req).await
        } else if req.method() == Method::DELETE {
            self.handle_delete(req).await
        } else {
            return Ok(status_code!(StatusCode::NOT_FOUND));
        }
    }

    async fn handle_static(&self, req: Request) -> BoxResult<Response> {
        let req_path = req.uri().path();
        let path = match self.get_file_path(req_path)? {
            Some(path) => path,
            None => return Ok(status_code!(StatusCode::FORBIDDEN)),
        };
        match fs::metadata(&path).await {
            Ok(meta) => {
                if meta.is_dir() {
                    let req_query = req.uri().query().unwrap_or_default();
                    if req_query == "zip" {
                        return self.handle_send_dir_zip(path.as_path()).await;
                    }
                    if let Some(q) = req_query.strip_prefix("q=") {
                        return self.handle_query_dir(path.as_path(), q).await;
                    }
                    self.handle_ls_dir(path.as_path(), true).await
                } else {
                    self.handle_send_file(path.as_path()).await
                }
            }
            Err(_) => {
                if req_path.ends_with('/') {
                    self.handle_ls_dir(path.as_path(), false).await
                } else {
                    Ok(status_code!(StatusCode::NOT_FOUND))
                }
            }
        }
    }

    async fn handle_upload(&self, mut req: Request) -> BoxResult<Response> {
        let forbidden = status_code!(StatusCode::FORBIDDEN);
        let path = match self.get_file_path(req.uri().path())? {
            Some(path) => path,
            None => return Ok(forbidden),
        };

        match path.parent() {
            Some(parent) => match fs::metadata(parent).await {
                Ok(meta) => {
                    if !meta.is_dir() {
                        return Ok(forbidden);
                    }
                }
                Err(_) => fs::create_dir_all(parent).await?,
            },
            None => return Ok(forbidden),
        }

        let mut file = fs::File::create(path).await?;

        let body_with_io_error = req
            .body_mut()
            .map_err(|err| io::Error::new(io::ErrorKind::Other, err));

        let body_reader = StreamReader::new(body_with_io_error);

        futures::pin_mut!(body_reader);

        io::copy(&mut body_reader, &mut file).await?;

        return Ok(status_code!(StatusCode::OK));
    }

    async fn handle_delete(&self, req: Request) -> BoxResult<Response> {
        let path = match self.get_file_path(req.uri().path())? {
            Some(path) => path,
            None => return Ok(status_code!(StatusCode::FORBIDDEN)),
        };

        let meta = fs::metadata(&path).await?;
        if meta.is_file() {
            fs::remove_file(path).await?;
        } else {
            fs::remove_dir_all(path).await?;
        }
        Ok(status_code!(StatusCode::OK))
    }

    async fn handle_ls_dir(&self, path: &Path, exist: bool) -> BoxResult<Response> {
        let mut paths: Vec<PathItem> = vec![];
        if exist {
            let mut rd = fs::read_dir(path).await?;
            while let Some(entry) = rd.next_entry().await? {
                let entry_path = entry.path();
                if let Ok(item) = get_path_item(entry_path, path.to_path_buf()).await {
                    paths.push(item);
                }
            }
        }
        self.send_index(path, paths)
    }

    async fn handle_query_dir(&self, path: &Path, q: &str) -> BoxResult<Response> {
        let mut paths: Vec<PathItem> = vec![];
        let mut walkdir = WalkDir::new(path);
        while let Some(entry) = walkdir.next().await {
            if let Ok(entry) = entry {
                if !entry
                    .file_name()
                    .to_string_lossy()
                    .to_lowercase()
                    .contains(&q.to_lowercase())
                {
                    continue;
                }
                if fs::symlink_metadata(entry.path()).await.is_err() {
                    continue;
                }
                if let Ok(item) = get_path_item(entry.path(), path.to_path_buf()).await {
                    paths.push(item);
                }
            }
        }
        self.send_index(path, paths)
    }

    async fn handle_send_dir_zip(&self, path: &Path) -> BoxResult<Response> {
        let (mut writer, reader) = tokio::io::duplex(BUF_SIZE);
        let path = path.to_owned();
        tokio::spawn(async move {
            if let Err(e) = dir_zip(&mut writer, &path).await {
                error!("Fail to zip {}, {}", path.display(), e.to_string());
            }
        });
        let stream = ReaderStream::new(reader);
        let body = Body::wrap_stream(stream);
        Ok(Response::new(body))
    }

    async fn handle_send_file(&self, path: &Path) -> BoxResult<Response> {
        let file = fs::File::open(path).await?;
        let stream = FramedRead::new(file, BytesCodec::new());
        let body = Body::wrap_stream(stream);
        Ok(Response::new(body))
    }

    fn send_index(&self, path: &Path, mut paths: Vec<PathItem>) -> BoxResult<Response> {
        paths.sort_unstable();
        let breadcrumb = self.get_breadcrumb(path);
        let data = IndexData {
            breadcrumb,
            paths,
            readonly: self.args.readonly,
        };
        let data = serde_json::to_string(&data).unwrap();
        let mut output =
            INDEX_HTML.replace("__STYLE__", &format!("<style>\n{}</style>", INDEX_CSS));
        output = output.replace("__DATA__", &data);

        Ok(hyper::Response::builder().body(output.into()).unwrap())
    }

    fn auth_guard(&self, req: &Request) -> BoxResult<bool> {
        if let Some(auth) = &self.args.auth {
            if let Some(value) = req.headers().get("Authorization") {
                let value = value.to_str()?;
                let value = if value.contains("Basic ") {
                    &value[6..]
                } else {
                    return Ok(false);
                };
                let value = base64::decode(value)?;
                let value = std::str::from_utf8(&value)?;
                return Ok(value == auth);
            } else {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn get_breadcrumb(&self, path: &Path) -> String {
        let path = match self.args.path.parent() {
            Some(p) => path.strip_prefix(p).unwrap(),
            None => path,
        };
        normalize_path(path)
    }

    fn get_file_path(&self, path: &str) -> BoxResult<Option<PathBuf>> {
        let decoded_path = percent_decode(path[1..].as_bytes()).decode_utf8()?;
        let slashes_switched = if cfg!(windows) {
            decoded_path.replace('/', "\\")
        } else {
            decoded_path.into_owned()
        };
        let path = self.args.path.join(&slashes_switched);
        if path.starts_with(&self.args.path) {
            Ok(Some(path))
        } else {
            Ok(None)
        }
    }
}

#[derive(Debug, Serialize, Eq, PartialEq, Ord, PartialOrd)]
struct IndexData {
    breadcrumb: String,
    paths: Vec<PathItem>,
    readonly: bool,
}

#[derive(Debug, Serialize, Eq, PartialEq, Ord, PartialOrd)]
struct PathItem {
    path_type: PathType,
    name: String,
    mtime: Option<u64>,
    size: Option<u64>,
}

#[derive(Debug, Serialize, Eq, PartialEq, Ord, PartialOrd)]
enum PathType {
    Dir,
    SymlinkDir,
    File,
    SymlinkFile,
}

async fn get_path_item<P: AsRef<Path>>(path: P, base_path: P) -> BoxResult<PathItem> {
    let path = path.as_ref();
    let rel_path = path.strip_prefix(base_path).unwrap();
    let meta = fs::metadata(&path).await?;
    let s_meta = fs::symlink_metadata(&path).await?;
    let is_dir = meta.is_dir();
    let is_symlink = s_meta.file_type().is_symlink();
    let path_type = match (is_symlink, is_dir) {
        (true, true) => PathType::SymlinkDir,
        (false, true) => PathType::Dir,
        (true, false) => PathType::SymlinkFile,
        (false, false) => PathType::File,
    };
    let mtime = meta
        .modified()?
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .map(|v| v.as_millis() as u64);
    let size = match path_type {
        PathType::Dir | PathType::SymlinkDir => None,
        PathType::File | PathType::SymlinkFile => Some(meta.len()),
    };
    let name = normalize_path(rel_path);
    Ok(PathItem {
        path_type,
        name,
        mtime,
        size,
    })
}

fn normalize_path<P: AsRef<Path>>(path: P) -> String {
    let path = path.as_ref().to_str().unwrap_or_default();
    if cfg!(windows) {
        path.replace('\\', "/")
    } else {
        path.to_string()
    }
}

async fn dir_zip<W: AsyncWrite + Unpin>(writer: &mut W, dir: &Path) -> BoxResult<()> {
    let mut writer = ZipFileWriter::new(writer);
    let mut walkdir = WalkDir::new(dir);
    while let Some(entry) = walkdir.next().await {
        if let Ok(entry) = entry {
            let meta = match fs::symlink_metadata(entry.path()).await {
                Ok(meta) => meta,
                Err(_) => continue,
            };
            if meta.is_file() {
                let filepath = entry.path();
                let filename = match filepath.strip_prefix(dir).ok().and_then(|v| v.to_str()) {
                    Some(v) => v,
                    None => continue,
                };
                let entry_options = EntryOptions::new(filename.to_owned(), Compression::Deflate);
                let mut file = File::open(&filepath).await?;
                let mut file_writer = writer.write_entry_stream(entry_options).await?;
                io::copy(&mut file, &mut file_writer).await?;
                file_writer.close().await?;
            }
        }
    }
    writer.close().await?;
    Ok(())
}