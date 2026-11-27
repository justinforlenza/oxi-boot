# oxi-boot

oxi-boot is a PXE boot server. It is a single program with its own DHCP, TFTP, HTTP and SMB servers, so you do not set up dnsmasq, a TFTP server or Samba beside it.

## Quick start

```bash
docker run --rm --network host -v /path/to/isos:/iso:ro ghcr.io/justinforlenza/oxi-boot
```

Change `/path/to/isos` to your folder of ISO files. Then start another computer on the same network from its network card (the boot menu is usually F12, and the entry is called network or PXE) and pick an ISO from the menu.

There is no config file to write. oxi-boot serves on the network card your default route uses and lets your router keep handing out addresses. ISOs you add to the folder later show up in the menu without a restart, and subfolders become submenus.

## What to expect

- **Linux ISOs** start through their own boot menu, as if from a CD. The client keeps reading the ISO over the network while it runs, so the server must stay up until the install has finished.
- **Windows ISOs** load their installer into memory first, which takes a minute or two with no progress on screen beyond a file list. Then a command window connects to the server and starts Setup. The client needs at least 4 GB of memory.
- **Some ISOs are loaded into the client's memory whole**. The client then needs more memory than the ISO's size.
- The first boot of each ISO can pause for a few seconds while the server works out how to boot it. The result is kept until the file changes.

## Configuration

Every setting is optional, and each can be given in three ways. Taking `dhcp.mode` as the example:

| Where | How | With Docker |
| --- | --- | --- |
| Environment variable | `OXI_BOOT_DHCP__MODE=full` | `-e OXI_BOOT_DHCP__MODE=full` |
| Argument | `--dhcp.mode full` | after the image name |
| Config file | `mode = "full"` under `[dhcp]` | `-v ./oxi-boot.toml:/etc/oxi-boot.toml:ro` |

An argument overrides an environment variable, which overrides the file. In a variable's name, two underscores separate the section from the setting. Outside the file a list is written `[a,b]`. [oxi-boot.example.toml](oxi-boot.example.toml) shows every setting.

| Setting | Default | Meaning |
| --- | --- | --- |
| `server.interface` | the card your default route uses | The network card to serve on. |
| `server.iso_dir` | `/iso` | The folder of ISO files. In the container, mount your folder there instead of changing this. |
| `server.ipxe_dir` | a `boot` folder next to the program, else `/usr/share/oxi-boot` | Where the boot files are. You rarely need to set it. |
| `dhcp.mode` | `proxy` | `proxy`, `full` or `external`. See below. |
| `dhcp.range` | none | `full` mode: the addresses to hand out, as `first-last`. |
| `dhcp.router`, `dhcp.dns` | none | `full` mode: what to tell clients, if they need the internet. |
| `menu.timeout` | `10` | Seconds before the default entry boots. `0` waits forever. |
| `menu.default` | `local` | `local` boots the client's own disk. Or an ISO's path under `iso_dir`. |
| `menu.title` | none | A line of text at the very top of the menu. |
| `menu.header` | the oxi-boot logo | Lines of text shown above the entries, centred: your own ASCII logo, say. `""` removes it. Keep it under about 40 columns; a wider one moves left or is cut off. |
| `menu.hide` | empty | Built-in entries to leave out: `as_is`, `local`, `reboot`, `firmware`, `shell`. If you hide `local` while it is the default, the menu waits forever. |
| `menu.colours.text`, `menu.colours.background` | `black`, `white` | The menu's colours: `black`, `red`, `green`, `yellow`, `blue`, `magenta`, `cyan` or `white`. |
| `menu.colours.heading` | `blue` | The header and the "ISOs" and "Tools" headings. |
| `menu.colours.selected_text`, `menu.colours.selected_background` | `white`, `blue` | The entry the cursor is on. |
| `access.policy` | `allow` | What machines on neither list get: `allow` or `deny`. |
| `access.allow`, `access.deny` | empty | Lists of MAC addresses. A denied machine boots its own disk. |

Per-ISO settings go in a section named after the ISO's path under `iso_dir`:

```toml
[iso."linux/debian-13.iso"]
name = "Debian 13"   # shown in the menu instead of the file name
hidden = true        # left out of the menu
as_is = true         # boot it unchanged, as a plain virtual CD
attach = "ram"       # nbd (default), fuse or ram: how the client reads the ISO
```

The access lists are a convenience, not security: MAC addresses can be faked.

### DHCP modes

| Your network | Use |
| --- | --- |
| A router hands out addresses | `mode = "proxy"`, the default. oxi-boot only adds the boot information. Nothing on the router changes. |
| Nothing hands out addresses (a separate lab network) | `mode = "full"` with a `range`. oxi-boot is the DHCP server. |
| You run your own DHCP server and want it to point clients at oxi-boot | `mode = "external"`. |

## Without Docker

Each [release](https://github.com/justinforlenza/oxi-boot/releases) has a download for x86-64 Linux and one for ARM64 Linux. Unpack it, write a config that names your ISO folder, and run it as root:

```toml
[server]
iso_dir = "/path/to/isos"
```

```bash
sudo ./oxi-boot oxi-boot.toml
```

Keep the `boot` folder next to the program; it is found there without a setting.

## Building it yourself

You need Rust with the `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` targets and `musl-gcc`.

```bash
cargo build --release -p oxi-boot --target x86_64-unknown-linux-musl
```

```bash
cargo build --release -p oxi-boot-helper --target x86_64-unknown-linux-musl
```

```bash
cargo build --release -p oxi-boot-helper --target aarch64-unknown-linux-musl
```

On an ARM64 machine, build the server with `--target aarch64-unknown-linux-musl` instead; the two helpers stay the same, because they run on the computers being booted.

## For developers

| Path | What it is |
| --- | --- |
| `crates/server` | `oxi-boot`, the server: DHCP, TFTP, HTTP, the menu, the ISO patcher, and a read-only SMB share for Windows Setup. |
| `crates/helper` | `oxi-boot-helper`, a small static program the server adds to a Linux ISO's early-boot image. It runs on the client and reattaches the ISO over the network. |
| `.github/workflows/release.yml` | Builds, releases and publishes the image when a version tag is pushed. |

```bash
cargo test --workspace
```

## Licences

oxi-boot is MIT. The container image also carries iPXE and wimboot, which are GPL and are shipped as separate, unmodified files. See [third-party-licences.md](third-party-licences.md).
