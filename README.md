# oxi-boot

Open-source PXE boot server written in Rust. Drop an ISO into a folder and network-boot it: Linux installer and live ISOs, and Windows 10 and 11 install ISOs, on x86 BIOS, x86 UEFI and ARM64 UEFI clients.

## Layout

| Path | What it is |
| --- | --- |
| `crates/server` | `oxi-boot`, the server: config and access lists, DHCP, TFTP, HTTP, menu generator, ISO reader, early-boot unpacker, SMB share. Only config exists today. |
| `crates/helper` | `oxi-boot-helper`, a small static program that runs on the client and reattaches the ISO once the Linux kernel is up. Placeholder today. |
| `oxi-boot.example.toml` | Example config, checked by the server's tests. |

## Build and run

```bash
cargo build --workspace
cargo test --workspace
cargo run -p oxi-boot -- oxi-boot.example.toml
```

The server takes the config path as its only argument and defaults to `./oxi-boot.toml`.