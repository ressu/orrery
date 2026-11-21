//! File uploads (`POST /server/files/upload`) and file downloads and deletes
//! (`/server/files/<root>/<path>`), after Moonraker's `FileUploadHandler` and
//! `FileRequestHandler`.

use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::body::Body;
use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Multipart, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use super::{changed_event, delete_file, is_gcode, parse_metadata};
use crate::api::ApiError;
use crate::server::App;
use crate::web::error_response;

/// Python's `urllib.parse.quote`: everything but letters, digits, `_.-~/` is escaped.
const QUOTE: &AsciiSet = &NON_ALPHANUMERIC.remove(b'_').remove(b'.').remove(b'-').remove(b'~').remove(b'/');

fn json_response(status: StatusCode, body: &Value) -> Response {
    (status, [(header::CONTENT_TYPE, "application/json; charset=UTF-8")], body.to_string()).into_response()
}

fn api_error(e: ApiError) -> Response {
    error_response(e.status, &e.message)
}

static UPLOAD_COUNT: AtomicU64 = AtomicU64::new(0);

/// A temp file that's removed unless it was moved into place.
struct TempUpload(PathBuf);

impl Drop for TempUpload {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub async fn upload(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    mut form: Multipart,
) -> Response {
    // A client with no rights at all is turned away before it sends the file. The root is
    // checked once its field has arrived.
    let access = match crate::web::request_access(&app, peer, &headers, query.as_deref()) {
        Ok(a) => a,
        Err(e) => return api_error(e),
    };
    if access.role.is_none() {
        let e = ApiError::new(401, "Unauthorized");
        app.auth.log_refusal(&access, "POST /server/files/upload", &e);
        return api_error(e);
    }
    let n = UPLOAD_COUNT.fetch_add(1, Ordering::Relaxed);
    let temp = TempUpload(std::env::temp_dir().join(format!("orrery.upload-{}-{n}.mru", std::process::id())));
    let mut fields: Map<String, Value> = Map::new();
    let mut filename: Option<String> = None;
    let mut digest = Sha256::new();
    loop {
        let mut field = match form.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => {
                tracing::info!("Chunk Parsing Error: {e}");
                return error_response(500, "File Upload Parsing Failed");
            }
        };
        let name = field.name().unwrap_or_default().to_owned();
        if name == "file" {
            filename = field.file_name().map(str::to_owned);
            let mut out = match tokio::fs::File::create(&temp.0).await {
                Ok(f) => f,
                Err(e) => return error_response(500, &format!("Unable to save file: {e}")),
            };
            loop {
                match field.chunk().await {
                    Ok(Some(chunk)) => {
                        digest.update(&chunk);
                        if out.write_all(&chunk).await.is_err() {
                            return error_response(500, "Unable to save file");
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::info!("Chunk Parsing Error: {e}");
                        return error_response(500, "File Upload Parsing Failed");
                    }
                }
            }
            if out.flush().await.is_err() {
                return error_response(500, "Unable to save file");
            }
        } else if ["root", "print", "path", "checksum"].contains(&name.as_str()) {
            match field.text().await {
                Ok(t) if !t.is_empty() => {
                    fields.insert(name, json!(t));
                }
                Ok(_) => {}
                Err(_) => return error_response(500, "File Upload Parsing Failed"),
            }
        }
    }
    let calculated: String = digest.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if let Some(Value::String(expected)) = fields.remove("checksum") {
        let expected = expected.to_lowercase();
        if expected != calculated {
            return error_response(
                422,
                &format!("File checksum mismatch: expected {expected}, calculated {calculated}"),
            );
        }
    }
    let Some(filename) = filename.filter(|f| !f.trim().is_empty()) else {
        return error_response(400, "Multipart filename omitted");
    };
    tracing::info!("Processing Uploaded File: {filename}");
    let result = match finalize_upload(&app, &access, &fields, &filename, &temp.0).await {
        Ok(r) => r,
        Err(e) => return api_error(e),
    };
    let mut resp = json_response(StatusCode::CREATED, &Value::Object(result.clone()));
    let item = &result["item"];
    if let (Some(root), Some(path)) = (item["root"].as_str(), item["path"].as_str()) {
        let host = headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("localhost");
        let location = format!("http://{host}/server/files/{root}/{}", utf8_percent_encode(path, QUOTE));
        if let Ok(v) = HeaderValue::from_str(&location) {
            resp.headers_mut().insert(header::LOCATION, v);
        }
    }
    resp
}

async fn finalize_upload(
    app: &Arc<App>,
    access: &crate::auth::Access,
    fields: &Map<String, Value>,
    filename: &str,
    temp: &Path,
) -> Result<Map<String, Value>, ApiError> {
    let fm = &app.files;
    let _guard = fm.op_lock.lock().await;
    let field = |k: &str| fields.get(k).and_then(Value::as_str).unwrap_or_default();
    let root = if field("root").is_empty() { "gcodes".to_owned() } else { field("root").to_lowercase() };
    let args = crate::api::Args([("root".to_owned(), json!(root))].into_iter().collect());
    if let Err(e) = crate::auth::authorize(app, access, "/server/files/upload", crate::api::RequestType::Post, &args) {
        app.auth.log_refusal(access, "POST /server/files/upload", &e);
        return Err(e);
    }
    let root_path = fm
        .root(&root)
        .map(|r| r.path.clone())
        .ok_or_else(|| ApiError::bad_request(format!("Root {root} not available")))?;
    let dir_path = field("path").trim_start_matches('/').to_owned();
    let mut name = filename.trim().trim_start_matches('/').to_owned();
    if !dir_path.is_empty() {
        name = Path::new(&dir_path).join(&name).to_string_lossy().into_owned();
    }
    let dest = super::normalize(&root_path.join(&name));
    if !dest.starts_with(&root_path) {
        return Err(ApiError::bad_request(format!("Cannot write to path: {}", dest.display())));
    }
    let start_print = field("print") == "true";
    fm.check_reserved(&dest, true)?;
    if !fm.full_access(&root) {
        return Err(ApiError::bad_request(format!("Invalid root request: {root}")));
    }
    let is_gcode_upload = root == "gcodes" && is_gcode(&name);
    if super::extension(&name) == ".ufp" {
        return Err(ApiError::bad_request("UFP uploads aren't supported by orrery yet"));
    }
    let mut can_start = false;
    if is_gcode_upload {
        match fm.operation_check(app, &dest) {
            Ok(free) => can_start = free,
            Err(e) if e.status == 403 => return Err(ApiError::new(403, "File is loaded, upload not permitted")),
            Err(_) => {}
        }
    }
    let is_link = dest.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink());
    if !dir_path.is_empty() {
        std::fs::create_dir_all(root_path.join(&dir_path)).map_err(|e| {
            tracing::info!("Upload Write Error: {e}");
            ApiError::new(500, "Unable to save file")
        })?;
    }
    let target = if is_link { std::fs::canonicalize(&dest).unwrap_or_else(|_| dest.clone()) } else { dest.clone() };
    let _own = fm.own_change(&[&dest, &target]);
    super::move_path(temp, &target).map_err(|e| {
        tracing::info!("Upload Write Error: {e}");
        ApiError::new(500, "Unable to save file")
    })?;
    if !is_gcode_upload {
        return Ok(changed_event(app, "create_file", &root, &dest, None));
    }
    parse_metadata(app, &dest).await;
    let mut started = false;
    if start_print && can_start {
        let rel = fm.relative_path("gcodes", &dest);
        started = crate::klippy::start_print(app, &rel).await.is_ok();
    }
    let mut queued = false;
    if start_print && !started && fm.queue_gcodes {
        let rel = fm.relative_path("gcodes", &dest);
        crate::job_queue::queue_job(app, vec![rel], false, false).await?;
        queued = true;
    }
    let mut result = changed_event(app, "create_file", "gcodes", &dest, None);
    result.insert("print_started".into(), json!(started));
    result.insert("print_queued".into(), json!(queued));
    Ok(result)
}

/// The content type a browser needs for files it shows, like thumbnails and logs.
/// The type Python's `mimetypes` gives, plus the text types Moonraker adds for `.log`,
/// `.gcode`, and `.cfg`. Tornado sends any gzip file as `application/gzip`.
fn content_type(path: &Path) -> &'static str {
    match super::extension(&path.to_string_lossy()).as_str() {
        ".log" | ".gcode" | ".cfg" => "text/plain",
        ".gz" => "application/gzip",
        ".png" => "image/png",
        ".jpg" | ".jpeg" => "image/jpeg",
        ".gif" => "image/gif",
        ".svg" => "image/svg+xml",
        ".txt" => "text/plain",
        ".json" => "application/json",
        ".zip" => "application/zip",
        ".html" => "text/html",
        ".css" => "text/css",
        ".js" => "application/javascript",
        _ => "application/octet-stream",
    }
}

/// `filename="ascii"; filename*=UTF-8''quoted`, as Moonraker sets it.
fn content_disposition(path: &Path) -> String {
    let base = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let ascii: String =
        base.chars().map(|c| if c.is_ascii() { c } else { '?' }).collect::<String>().replace('"', "\\\"");
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{}", utf8_percent_encode(&base, QUOTE))
}

/// Parses a single `bytes=start-end` range. Returns `(start, end)` with `end` exclusive.
fn parse_range(header: &str, size: u64) -> Option<Result<(u64, u64), ()>> {
    let spec = header.strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (start, end) = spec.split_once('-')?;
    let (start, end) = match (start.trim(), end.trim()) {
        ("", "") => return None,
        ("", suffix) => {
            let n: u64 = suffix.parse().ok()?;
            if n == 0 {
                return Some(Err(()));
            }
            (size.saturating_sub(n), size)
        }
        (s, "") => (s.parse().ok()?, size),
        (s, e) => {
            let e: u64 = e.parse().ok()?;
            (s.parse().ok()?, (e + 1).min(size))
        }
    };
    if start >= size || start >= end {
        return Some(Err(()));
    }
    Some(Ok((start, end)))
}

/// Serves `GET` and `DELETE` on `/server/files/<root>/<path>`. Returns `None` when the path
/// isn't in a root.
pub async fn file_request(app: &Arc<App>, method: &Method, path: &str, headers: &HeaderMap) -> Option<Response> {
    let rest = path.strip_prefix("/server/files/")?;
    let rest = percent_decode_str(rest).decode_utf8_lossy().into_owned();
    // Moonraker serves these two log files at fixed addresses, outside the roots.
    let fixed = match rest.as_str() {
        "klippy.log" => Some(app.klippy.log_file()),
        "moonraker.log" => Some(app.paths.log_file.clone().unwrap_or_default()),
        _ => None,
    };
    if let Some(path) = fixed {
        if method != Method::GET && method != Method::HEAD {
            return Some(error_response(405, "Method Not Allowed"));
        }
        if !path.is_file() {
            return Some(error_response(404, "Not Found"));
        }
        return Some(send_file(&path, headers, method == Method::HEAD).await);
    }
    let root = rest.split('/').next().unwrap_or_default();
    app.files.root(root)?;
    if method == Method::DELETE {
        return Some(match delete_file(app, &rest).await {
            Ok(r) => json_response(StatusCode::OK, &json!({"result": r})),
            Err(e) => api_error(e),
        });
    }
    if method != Method::GET && method != Method::HEAD {
        return Some(error_response(405, "Method Not Allowed"));
    }
    Some(download(app, &rest, headers, method == Method::HEAD).await)
}

async fn download(app: &Arc<App>, request: &str, headers: &HeaderMap, head_only: bool) -> Response {
    let (_, full) = match app.files.convert_request_path(request) {
        Ok(p) => p,
        Err(_) => return error_response(404, "Not Found"),
    };
    if !full.is_file() {
        return error_response(404, "Not Found");
    }
    if let Err(e) = app.files.check_reserved(&full, false) {
        return api_error(e);
    }
    send_file(&full, headers, head_only).await
}

/// Streams a file, or the single byte range the request asks for.
async fn send_file(full: &Path, headers: &HeaderMap, head_only: bool) -> Response {
    let Ok(mut file) = tokio::fs::File::open(full).await else {
        return error_response(404, "Not Found");
    };
    let size = file.metadata().await.map(|m| m.len()).unwrap_or(0);
    let range = headers.get(header::RANGE).and_then(|h| h.to_str().ok()).and_then(|h| parse_range(h, size));
    let (status, start, end) = match range {
        Some(Err(())) => {
            return (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_TYPE, "text/plain".to_owned()), (header::CONTENT_RANGE, format!("bytes */{size}"))],
            )
                .into_response();
        }
        Some(Ok((s, e))) if e - s != size => (StatusCode::PARTIAL_CONTENT, s, e),
        _ => (StatusCode::OK, 0, size),
    };
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type(full))
        .header(header::CONTENT_LENGTH, end - start)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_DISPOSITION, content_disposition(full));
    if status == StatusCode::PARTIAL_CONTENT {
        builder = builder.header(header::CONTENT_RANGE, format!("bytes {start}-{}/{size}", end - 1));
    }
    if head_only {
        return builder.body(Body::empty()).unwrap_or_else(|_| error_response(500, "Internal Server Error"));
    }
    if start > 0 && file.seek(SeekFrom::Start(start)).await.is_err() {
        return error_response(500, "Internal Server Error");
    }
    let stream = tokio_util::io::ReaderStream::with_capacity(file.take(end - start), 64 * 1024);
    builder.body(Body::from_stream(stream)).unwrap_or_else(|_| error_response(500, "Internal Server Error"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-9", 100), Some(Ok((0, 10))));
        assert_eq!(parse_range("bytes=90-", 100), Some(Ok((90, 100))));
        assert_eq!(parse_range("bytes=-10", 100), Some(Ok((90, 100))));
        assert_eq!(parse_range("bytes=100-", 100), Some(Err(())));
        assert_eq!(parse_range("items=1-2", 100), None);
    }

    #[test]
    fn content_types() {
        assert_eq!(content_type(Path::new("logs/klippy.log")), "text/plain");
        assert_eq!(content_type(Path::new("a.GCODE")), "text/plain");
        assert_eq!(content_type(Path::new("crowsnest.log.2026-11-13.gz")), "application/gzip");
        assert_eq!(content_type(Path::new("a.ufp")), "application/octet-stream");
    }

    #[test]
    fn disposition() {
        assert_eq!(
            content_disposition(Path::new("a/ä b.gcode")),
            "attachment; filename=\"? b.gcode\"; filename*=UTF-8''%C3%A4%20b.gcode"
        );
    }
}
