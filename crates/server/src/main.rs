mod config;
mod dhcp;
mod http;
mod initrd;
mod iso;
mod menu;
mod patch;

use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;

use async_tftp::server::TftpServerBuilder;
use config::DhcpMode;

type Error = Box<dyn std::error::Error>;

#[tokio::main]
async fn main() -> ExitCode {
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
    let helper_path = config.server.ipxe_dir.join("oxi-boot-helper");
    let helper = std::fs::read(&helper_path).ok();
    if helper.is_none() {
        eprintln!("oxi-boot: no helper at {}: ISOs will boot unpatched", helper_path.display());
    }
    let patches = Arc::new(patch::Patches::new(config.server.iso_dir.clone(), helper, format!("{}:80", iface.ip)));
    let app = http::router(config.clone(), format!("http://{}", iface.ip), patches);
    eprintln!("oxi-boot: serving {} on {name} ({}), DHCP mode {:?}", config.server.iso_dir.display(), iface.ip, config.dhcp.mode);

    let dhcp = async {
        match config.dhcp.mode {
            // The network's own DHCP server points clients here.
            DhcpMode::External => std::future::pending().await,
            _ => dhcp::serve(name, iface, full).await.map_err(|e| format!("DHCP: {e}")),
        }
    };
    tokio::select! {
        r = dhcp => r?,
        r = tftp.serve() => r.map_err(|e| format!("TFTP: {e}"))?,
        r = axum::serve(listener, app) => r.map_err(|e| format!("HTTP: {e}"))?,
    }
    Ok(())
}
