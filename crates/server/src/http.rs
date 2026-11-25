//! HTTP: the boot script and menu for iPXE, and the ISOs by byte range, both
//! untouched (`/iso/`) and patched (`/patched/`), and what a Windows disc
//! needs to start (`/wimboot`, `/win/`).

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, RawQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use axum::routing::get;
use tower_http::services::{ServeDir, ServeFile};

use crate::config::Config;
use crate::menu;
use crate::patch::{Patched, Patches};
use crate::windows;

#[derive(Clone)]
struct App {
    config: Arc<Config>,
    /// The server's own URL, without a trailing slash.
    base: String,
    patches: Arc<Patches>,
    windows: Arc<windows::Isos>,
}

pub fn router(config: Arc<Config>, base: String, patches: Arc<Patches>, windows: Arc<windows::Isos>) -> Router {
    Router::new()
        .route("/boot.ipxe", get(boot))
        .route("/menu.ipxe", get(menu))
        .route("/boot/{*path}", get(boot_iso))
        .route("/patched/{*path}", get(patched))
        .route("/as-is/{*path}", get(as_is))
        .route("/direct/{*path}", get(direct))
        .route("/helper-log", get(helper_log))
        .route("/win/{*path}", get(win))
        .route_service("/wimboot", ServeFile::new(config.server.ipxe_dir.join("wimboot")))
        .nest_service("/iso", ServeDir::new(&config.server.iso_dir))
        .with_state(App { config, base, patches, windows })
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

/// The ISO at `path` under the ISO folder, with its patch worked out.
async fn open(app: &App, path: &str) -> Result<Arc<Patched>, StatusCode> {
    // The path names a file under the ISO folder and nothing else.
    let inside = path.split('/').all(|part| !part.is_empty() && !part.starts_with('.'));
    if !inside || !path.to_ascii_lowercase().ends_with(".iso") {
        return Err(StatusCode::NOT_FOUND);
    }
    app.patches.get(path, &menu::url_path(path)).await.map_err(|e| {
        eprintln!("http: {path}: {e}");
        StatusCode::NOT_FOUND
    })
}

/// The script that boots one ISO patched. It is a request of its own, not
/// part of the menu, because only the finished patch says how to boot.
async fn boot_iso(State(app): State<App>, Path(path): Path<String>) -> Result<String, StatusCode> {
    let (base, url) = (&app.base, menu::url_path(&path));
    if let Some(installer) = app.windows.tree(&path).and_then(|tree| windows::installer(&tree)) {
        eprintln!("http: {path} boots as Windows {}", if installer { "Setup, from the file share" } else { "PE, with nothing added" });
        return Ok(windows::script(base, &url, installer));
    }
    let iso = open(&app, &path).await?;
    let cd = format!("sanboot {base}/patched/{url}\n");
    Ok(match &iso.direct {
        None => format!("#!ipxe\n{cd}"),
        Some(direct) => format!(
            "#!ipxe\niseq ${{platform}} {} || goto cd\n\
             kernel {base}/direct/{url}/kernel initrd=initrd {}\n\
             initrd {base}/direct/{url}/initrd\nboot\n:cd\n{cd}",
            direct.platform, direct.options
        ),
    })
}

/// One file out of a Windows ISO, or one of the two startup files we add:
/// `/win/<iso path>?file=<path inside the ISO>`.
async fn win(State(app): State<App>, Path(path): Path<String>, Query(query): Query<HashMap<String, String>>) -> Result<Response, StatusCode> {
    let tree = app.windows.tree(&path).ok_or(StatusCode::NOT_FOUND)?;
    let reply = |len: u64, body: Body| Ok(Response::builder().header(header::CONTENT_LENGTH, len).body(body).unwrap());
    match query.get("file").map_or("", String::as_str) {
        "winpeshl.ini" => reply(windows::WINPESHL.len() as u64, windows::WINPESHL.into()),
        "install.bat" => {
            let script = app.windows.install_bat(&path);
            reply(script.len() as u64, script.into())
        }
        file => {
            let node = tree.find(file).filter(|&node| !tree.node(node).dir).ok_or(StatusCode::NOT_FOUND)?;
            let size = tree.node(node).size;
            reply(size, Body::from_stream(futures_util::stream::unfold(0, move |at| {
                let tree = tree.clone();
                async move {
                    let chunk = tokio::task::spawn_blocking(move || tree.read(node, at, 1 << 20)).await;
                    (at < size).then(|| (chunk.map_err(std::io::Error::other).and_then(|read| read), at + (1 << 20)))
                }
            })))
        }
    }
}

/// One ISO as its patched view, whole or by byte range.
async fn patched(State(app): State<App>, Path(path): Path<String>, headers: HeaderMap) -> Result<Response, StatusCode> {
    let iso = open(&app, &path).await?;
    let size = iso.view.size();
    Ok(ranged(iso, 0, size, &headers))
}

/// One ISO untouched, for booting as-is. `/iso/` serves the same bytes; this
/// route also understands the broken byte ranges of iPXE on BIOS.
async fn as_is(State(app): State<App>, Path(path): Path<String>, headers: HeaderMap) -> Result<Response, StatusCode> {
    let inside = path.split('/').all(|part| !part.is_empty() && !part.starts_with('.'));
    let file = std::fs::File::open(app.config.server.iso_dir.join(&path)).ok().filter(|_| inside).ok_or(StatusCode::NOT_FOUND)?;
    let view = crate::iso::View::unpatched(&file).map_err(|_| StatusCode::NOT_FOUND)?;
    let size = view.size();
    Ok(ranged(Arc::new(Patched { file, view, direct: None }), 0, size, &headers))
}

/// The kernel or initrd of an ISO that is started without its boot loader:
/// `/direct/<iso path>/kernel` and `/direct/<iso path>/initrd`.
async fn direct(State(app): State<App>, Path(path): Path<String>, headers: HeaderMap) -> Result<Response, StatusCode> {
    let (path, part) = path.rsplit_once('/').ok_or(StatusCode::NOT_FOUND)?;
    let iso = open(&app, path).await?;
    let direct = iso.direct.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let (offset, len) = match part {
        "kernel" => direct.kernel,
        "initrd" => direct.initrd,
        _ => return Err(StatusCode::NOT_FOUND),
    };
    Ok(ranged(iso, offset, len, &headers))
}

/// `size` bytes of the view from `offset`, whole or the byte range asked for.
fn ranged(iso: Arc<Patched>, offset: u64, size: u64, headers: &HeaderMap) -> Response {
    // One byte range, or everything. shortcut: a request for several ranges
    // gets the whole file, which HTTP allows.
    let header = headers.get(header::RANGE).and_then(|value| value.to_str().ok());
    let range = header.and_then(|value| {
        overflowed_range(value).filter(|(start, end)| start <= end && *end < size).or_else(|| {
            let ranges = http_range_header::parse_range_header(value).ok()?.validate(size).ok()?;
            (ranges.len() == 1).then(|| (*ranges[0].start(), *ranges[0].end()))
        })
    });
    let (start, len) = range.map_or((0, size), |(start, end)| (start, end - start + 1));
    let mut response = Response::builder()
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, len);
    if let Some((start, end)) = range {
        response = response.status(StatusCode::PARTIAL_CONTENT).header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}"));
    }
    response.body(body(iso, offset + start, len)).unwrap()
}

/// iPXE on BIOS keeps a disk offset in a signed 32-bit number, so a read
/// between 2 and 4 GiB arrives as a negative offset ("bytes=-888604672--888602625").
/// The real offset is that number plus 4 GiB. Reads past 4 GiB wrap around to
/// valid-looking low offsets and cannot be told apart; the patch keeps what a
/// boot loader needs below 4 GiB for that reason.
fn overflowed_range(header: &str) -> Option<(u64, u64)> {
    let spec = header.strip_prefix("bytes=")?;
    // The separator is the first '-' that is not the start's own sign.
    let split = 1 + spec.get(1..)?.find('-')?;
    let (start, end): (i64, i64) = (spec[..split].parse().ok()?, spec[split + 1..].parse().ok()?);
    if start >= 0 && end >= 0 {
        return None; // an ordinary range
    }
    let fix = |offset: i64| if offset < 0 { offset + (1 << 32) } else { offset } as u64;
    Some((fix(start), fix(end)))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_ranges_that_overflowed_on_the_client() {
        // 3406362624 and 3406364671 as signed 32-bit numbers
        assert_eq!(overflowed_range("bytes=-888604672--888602625"), Some((3406362624, 3406364671)));
        // A read that starts just under 2 GiB and ends over it
        assert_eq!(overflowed_range("bytes=2147481600--2147481601"), Some((2147481600, 2147485695)));
        assert_eq!(overflowed_range("bytes=0-2047"), None);
        assert_eq!(overflowed_range("bytes=-500"), None); // a real "last 500 bytes" request
        assert_eq!(overflowed_range("bytes=100-"), None);
    }
}
