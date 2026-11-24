mod config;

use std::process::ExitCode;

fn main() -> ExitCode {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "oxi-boot.toml".into());
    match config::load(&path) {
        Ok(config) => {
            println!("{config:#?}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("oxi-boot: {path}: {e}");
            ExitCode::FAILURE
        }
    }
}
