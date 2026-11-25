//! HTTP: the boot script and menu for iPXE, and the ISOs by byte range.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Query, State};
use axum::routing::get;
use tower_http::services::ServeDir;

use crate::config::Config;
use crate::menu;

#[derive(Clone)]
struct App {
    config: Arc<Config>,
    /// The server's own URL, without a trailing slash.
    base: String,
}

pub fn router(config: Arc<Config>, base: String) -> Router {
    Router::new()
        .route("/boot.ipxe", get(boot))
        .route("/menu.ipxe", get(menu))
        .nest_service("/iso", ServeDir::new(&config.server.iso_dir))
        .with_state(App { config, base })
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
    menu::script(&root, &app.config, &app.base)
}
