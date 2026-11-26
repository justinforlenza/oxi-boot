mod config;
mod dhcp;
mod http;
mod initrd;
mod iso;
mod menu;
mod patch;
mod udf;
mod windows;

use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;

use async_tftp::server::TftpServerBuilder;
use config::DhcpMode;

type Error = Box<dyn std::error::Error>;

#[tokio::main]
async fn main() -> ExitCode {
    // The file share library reports through `tracing`; $OXI_DEBUG shows every request.
    let level = if std::env::var_os("OXI_DEBUG").is_some() { tracing_subscriber::filter::LevelFilter::DEBUG } else { tracing_subscriber::filter::LevelFilter::WARN };
    tracing_subscriber::fmt().with_max_level(level).with_writer(std::io::stderr).init();
    let path = std::env::args().nth(1).unwrap_or_else(|| "oxi-boot.toml".into());
    match run(&path).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("oxi-boot: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Runs the services until one of them fails.
async fn run(path: &str) -> Result<(), Error> {
    let config = Arc::new(config::load(path).map_err(|e| format!("{path}: {e}"))?);
    let name = &config.server.interface;
    let iface = dhcp::iface(name)?;
    let full = match config.dhcp.mode {
        DhcpMode::Full => Some(dhcp::Full::new(iface, &config.dhcp)?),
        _ => None,
    };

    let tftp = TftpServerBuilder::with_dir_ro(&config.server.ipxe_dir)
        .map_err(|e| format!("ipxe_dir {}: {e}", config.server.ipxe_dir.display()))?
        .bind(SocketAddr::from((iface.ip, 69)))
        .build()
        .await
        .map_err(|e| format!("TFTP port 69: {e}"))?;
    let listener = tokio::net::TcpListener::bind((iface.ip, 80)).await.map_err(|e| format!("HTTP port 80: {e}"))?;
    // Without the helper program nothing can be patched, and every ISO boots as-is.
    let helper = |name: &str, isos: &str| {
        let path = config.server.ipxe_dir.join(name);
        let helper = std::fs::read(&path).ok();
        if helper.is_none() {
            eprintln!("oxi-boot: no helper at {}: {isos} ISOs will boot unpatched", path.display());
        }
        helper
    };
    let helpers = patch::Helpers { x86_64: helper("oxi-boot-helper", "x86-64"), arm64: helper("oxi-boot-helper-arm64", "ARM64") };
    let patches = Arc::new(patch::Patches::new(config.clone(), helpers, format!("{}:80", iface.ip)));
    if !config.server.ipxe_dir.join("wimboot").exists() {
        eprintln!("oxi-boot: no wimboot in {}: Windows ISOs will not boot", config.server.ipxe_dir.display());
    }
    let windows = Arc::new(windows::Isos::new(config.server.iso_dir.clone(), iface.ip)?);
    let app = http::router(config.clone(), format!("http://{}", iface.ip), patches, windows.clone());
    eprintln!("oxi-boot: serving {} on {name} ({}), DHCP mode {:?}", config.server.iso_dir.display(), iface.ip, config.dhcp.mode);

    let dhcp = async {
        match config.dhcp.mode {
            // The network's own DHCP server points clients here.
            DhcpMode::External => std::future::pending().await,
            _ => dhcp::serve(name, iface, full).await.map_err(|e| format!("DHCP: {e}")),
        }
    };
    // Linux ISOs do not need the share, so the server carries on without it,
    // for example beside a Samba that already holds port 445.
    let share = async {
        if let Err(e) = windows.serve().await {
            eprintln!("oxi-boot: no file share ({e}): Windows Setup will not find its files");
        }
        std::future::pending().await
    };
    tokio::select! {
        r = dhcp => r?,
        r = share => r,
        r = tftp.serve() => r.map_err(|e| format!("TFTP: {e}"))?,
        r = axum::serve(listener, app) => r.map_err(|e| format!("HTTP: {e}"))?,
    }
    Ok(())
}
