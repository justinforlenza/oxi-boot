//! HTTP: the boot script and menu for iPXE, and the ISOs by byte range, both
//! untouched (`/iso/`) and patched (`/patched/`).

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, RawQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tower_http::services::ServeDir;

use crate::config::Config;
use crate::menu;
use crate::patch::{Patched, Patches};

#[derive(Clone)]
struct App {
    config: Arc<Config>,
    /// The server's own URL, without a trailing slash.
    base: String,
    patches: Arc<Patches>,
}

pub fn router(config: Arc<Config>, base: String, patches: Arc<Patches>) -> Router {
    Router::new()
        .route("/boot.ipxe", get(boot))
        .route("/menu.ipxe", get(menu))
        .route("/patched/{*path}", get(patched))
        .route("/helper-log", get(helper_log))
        .nest_service("/iso", ServeDir::new(&config.server.iso_dir))
        .with_state(App { config, base, patches })
}

/// The first script every client runs: it asks for the menu and says who it
/// is, so the access lists work whichever DHCP server sent it here.
async fn boot(State(app): State<App>) -> String {
    format!("#!ipxe\nchain {}/menu.ipxe?mac=${{netX/mac}}\n", app.base)
}

// shortcut: only the menu is checked against the access lists; a denied
// machine that asks for an ISO's URL by hand still gets it. MAC addresses can
// be faked anyway, so the lists are a convenience, not a security boundary.
async fn menu(State(app): State<App>, Query(query): Query<HashMap<String, String>>) -> String {
    let mac = query.get("mac").map_or("", String::as_str);
    if !app.config.access.allows(mac) {
        eprintln!("http: {mac} is denied; sent to its local disk");
        return menu::denied();
    }
    // Scanned on every request, so a new ISO shows up without a restart.
    let root = menu::scan(&app.config.server.iso_dir, "", &app.config.iso);
    // Work out the patches now, while someone reads the menu: the first one
    // for an ISO can take longer than a boot loader is willing to wait.
    for path in root.paths() {
        let (patches, path) = (app.patches.clone(), path.to_string());
        tokio::spawn(async move { patches.get(&path, &menu::url_path(&path)).await });
    }
    menu::script(&root, &app.config, &app.base)
}

/// The helper reports here, because a client's console is often unreadable
/// or hidden behind a splash screen.
async fn helper_log(RawQuery(message): RawQuery) {
    eprintln!("helper: {}", message.unwrap_or_default());
}

/// One ISO as its patched view, whole or by byte range.
async fn patched(State(app): State<App>, Path(path): Path<String>, headers: HeaderMap) -> Response {
    // The path names a file under the ISO folder and nothing else.
    let inside = path.split('/').all(|part| !part.is_empty() && !part.starts_with('.'));
    if !inside || !path.to_ascii_lowercase().ends_with(".iso") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let iso = match app.patches.get(&path, &menu::url_path(&path)).await {
        Ok(iso) => iso,
        Err(e) => {
            eprintln!("http: {path}: {e}");
            return StatusCode::NOT_FOUND.into_response();
        }
    };
    let size = iso.view.size();
    // One byte range, or everything. shortcut: a request for several ranges
    // gets the whole file, which HTTP allows.
    let range = headers
        .get(header::RANGE)
        .and_then(|value| http_range_header::parse_range_header(value.to_str().ok()?).ok()?.validate(size).ok())
        .filter(|ranges| ranges.len() == 1)
        .map(|ranges| (*ranges[0].start(), *ranges[0].end()));
    let (start, len) = range.map_or((0, size), |(start, end)| (start, end - start + 1));
    let mut response = Response::builder()
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, len);
    if let Some((start, end)) = range {
        response = response.status(StatusCode::PARTIAL_CONTENT).header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}"));
    }
    response.body(body(iso, start, len)).unwrap()
}

/// `len` bytes of the view from `start`, read a piece at a time off the async threads.
fn body(iso: Arc<Patched>, start: u64, len: u64) -> Body {
    Body::from_stream(futures_util::stream::unfold((iso, start, len), |(iso, at, left)| async move {
        if left == 0 {
            return None;
        }
        let n = left.min(1 << 20);
        let reader = iso.clone();
        let chunk = tokio::task::spawn_blocking(move || reader.view.read(&reader.file, at, n as usize)).await;
        Some((chunk.map_err(std::io::Error::other).and_then(|read| read), (iso, at + n, left - n)))
    }))
}
