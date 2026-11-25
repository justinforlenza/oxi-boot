//! Runs as /init in a patched initrd, before the ISO's own first program:
//! loads the network driver, gets an address, attaches the untouched ISO as
//! /dev/nbd0 (a child process answers the kernel's block reads with HTTP
//! range requests to the server), then hands over to the original /init.
//!
//! It must run in any initrd, so it is one static program:
//! cargo build --release --target x86_64-unknown-linux-musl -p oxi-boot-helper
//!
//! The server puts /oxi-boot.conf beside it: the server's "address:port" on
//! the first line and the ISO's URL path on the second.

use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpStream, UdpSocket};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;

// systemd spares processes whose name starts with '@' when it leaves the initrd.
const SERVER_NAME: &str = "@oxi-boot-helper";

fn main() {
    if std::env::args().next().as_deref() == Some(SERVER_NAME) {
        let e = serve_nbd().unwrap_err();
        log(format!("NBD server stopped: {e}"));
        return;
    }
    log("helper started".to_string());
    // What we mount here we unmount again before handing over: Debian's init
    // script exits, and so panics the kernel, if /proc is already mounted.
    let mut mounted = Vec::new();
    for (fstype, dir) in [("proc", "/proc"), ("sysfs", "/sys"), ("devtmpfs", "/dev")] {
        let _ = fs::create_dir_all(dir);
        let (src, dir_c) = (cstr(fstype), cstr(dir));
        if unsafe { libc::mount(src.as_ptr(), dir_c.as_ptr(), src.as_ptr(), 0, std::ptr::null()) } == 0 {
            mounted.push(dir_c);
        }
    }
    let attached = attach();
    for dir in mounted.iter().rev() {
        // Lazily: the NBD child still has files open under these.
        unsafe { libc::umount2(dir.as_ptr(), libc::MNT_DETACH) };
    }
    match attached {
        Ok(()) => log("ISO attached as /dev/nbd0, starting the installer's own init".to_string()),
        Err(e) => log(format!("could not attach the ISO ({e}), continuing without it")),
    }
    // Put the original first program back so it sees nothing unusual.
    let _ = fs::rename("/init.oxi-orig", "/init");
    let e = Command::new("/init").args(std::env::args().skip(1)).exec();
    log(format!("could not start the original /init: {e}"));
    thread::sleep(Duration::from_secs(3600)); // PID 1 must not exit
}

/// To the console and, once the network is up, to the server's log: the
/// console is often unreadable or hidden behind a splash screen.
fn log(message: String) {
    eprintln!("oxi-boot: {message}");
    let Ok((host, _)) = server() else { return };
    let Ok(address) = host.parse() else { return };
    if let Ok(mut s) = TcpStream::connect_timeout(&address, Duration::from_secs(2)) {
        let text: String = message.chars().map(|c| if c.is_ascii_alphanumeric() || "/.:-".contains(c) { c } else { '_' }).collect();
        let _ = write!(s, "GET /helper-log?{text} HTTP/1.1\r\nHost: {host}\r\n\r\n");
        let _ = s.read(&mut [0u8; 64]);
    }
}

/// The server's "address:port" and the untouched ISO's URL path.
fn server() -> io::Result<(String, String)> {
    let conf = fs::read_to_string("/oxi-boot.conf")?;
    let mut lines = conf.lines().map(|line| line.trim().to_string());
    match (lines.next(), lines.next()) {
        (Some(host), Some(path)) => Ok((host, path)),
        _ => Err(io::Error::other("/oxi-boot.conf is incomplete")),
    }
}

fn attach() -> io::Result<()> {
    // Set by the server when it added kernel modules to the initrd (Debian's
    // installer ships none for networking): index them so modprobe finds them.
    if Path::new("/oxi-boot.depmod").exists() {
        run("depmod", &["-a"]);
    }

    // The initrd's own modprobe loads the driver for every network card.
    for dev in fs::read_dir("/sys/bus/pci/devices")?.flatten() {
        let read = |f: &str| fs::read_to_string(dev.path().join(f)).unwrap_or_default();
        if read("class").starts_with("0x02") {
            modprobe(read("modalias").trim());
        }
    }
    modprobe("nbd");

    let interface = retry(100, || {
        fs::read_dir("/sys/class/net").ok()?.flatten().map(|e| e.file_name().into_string().unwrap()).find(|n| n != "lo")
    })
    .ok_or_else(|| io::Error::other("no network interface appeared"))?;
    let (ip, mask) = dhcp(&interface)?;
    let address = Ipv4Addr::from(ip);
    let mac = fs::read_to_string(format!("/sys/class/net/{interface}/address"))?;
    log(format!("{interface} is {address}, server is {}", server()?.0));

    Command::new("/proc/self/exe")
        .arg0(SERVER_NAME)
        .env("OXI_NET", format!("{} {address} {}", mac.trim(), Ipv4Addr::from(mask)))
        .spawn()?;
    retry(300, || (fs::read_to_string("/sys/block/nbd0/size").ok()?.trim() != "0").then_some(()))
        .ok_or_else(|| io::Error::other("/dev/nbd0 did not come up"))
}

/// Polls every 100 ms.
fn retry<T>(times: u32, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    (0..times).find_map(|_| {
        f().or_else(|| {
            thread::sleep(Duration::from_millis(100));
            None
        })
    })
}

fn cstr(s: &str) -> std::ffi::CString {
    std::ffi::CString::new(s).unwrap()
}

fn modprobe(what: &str) {
    run("modprobe", &["-q", what]);
}

/// Runs one of the initrd's own tools.
fn run(tool: &str, args: &[&str]) {
    let found = ["/sbin", "/usr/sbin", "/usr/bin", "/bin"]
        .into_iter()
        .map(|dir| format!("{dir}/{tool}"))
        .find(|p| Path::new(p).exists());
    match found {
        Some(path) => drop(Command::new(path).args(args).status()),
        None => log(format!("this initrd has no {tool}")),
    }
}

// ---- network -------------------------------------------------------------

/// Brings the interface up and configures it with a DHCP lease.
/// shortcut: no default route and no lease renewal, so the server must be on
/// the client's own subnet and the boot must finish within the lease.
fn dhcp(interface: &str) -> io::Result<([u8; 4], [u8; 4])> {
    let mac: Vec<u8> = fs::read_to_string(format!("/sys/class/net/{interface}/address"))?
        .trim()
        .split(':')
        .filter_map(|b| u8::from_str_radix(b, 16).ok())
        .collect();
    if mac.len() != 6 {
        return Err(io::Error::other("interface has no ethernet address"));
    }

    let socket = UdpSocket::bind("0.0.0.0:68")?;
    socket.set_broadcast(true)?;
    socket.set_read_timeout(Some(Duration::from_secs(1)))?;
    let name = cstr(interface);
    let bound = unsafe {
        libc::setsockopt(socket.as_raw_fd(), libc::SOL_SOCKET, libc::SO_BINDTODEVICE, name.as_ptr().cast(), interface.len() as u32)
    };
    if bound != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut flags = ifreq(interface, &[]);
    ioctl(&socket, libc::SIOCGIFFLAGS, flags.as_mut_ptr())?;
    flags[16] |= libc::IFF_UP as u8;
    ioctl(&socket, libc::SIOCSIFFLAGS, flags.as_mut_ptr())?;

    let xid = [0x6f, 0x78, mac[4], mac[5]];
    let packet = |kind: u8, extra: &[u8]| {
        let mut p = vec![0u8; 240];
        p[..4].copy_from_slice(&[1, 1, 6, 0]); // request, ethernet, 6-byte address
        p[4..8].copy_from_slice(&xid);
        p[10] = 0x80; // ask for a broadcast reply: we have no address yet
        p[28..34].copy_from_slice(&mac);
        p[236..240].copy_from_slice(&[99, 130, 83, 99]);
        p.extend([53, 1, kind]);
        p.extend(extra);
        p.extend([55, 2, 1, 3, 255]); // want subnet mask and router
        p.resize(300, 0);
        p
    };
    // One reply of the wanted kind as (our address, mask, server id).
    let reply = |wanted: u8| -> Option<([u8; 4], [u8; 4], [u8; 4])> {
        let mut buf = [0u8; 1500];
        let n = socket.recv(&mut buf).ok()?;
        if n < 240 || buf[4..8] != xid {
            return None;
        }
        let (mut kind, mut mask, mut server, mut opts) = (0, [255, 255, 255, 0], [0; 4], &buf[240..n]);
        while let [code, len, rest @ ..] = opts {
            if *code == 255 || rest.len() < *len as usize {
                break;
            }
            let value = &rest[..*len as usize];
            match (*code, value.len()) {
                (53, 1) => kind = value[0],
                (1, 4) => mask.copy_from_slice(value),
                (54, 4) => server.copy_from_slice(value),
                _ => {}
            }
            opts = &rest[*len as usize..];
        }
        (kind == wanted).then(|| (buf[16..20].try_into().unwrap(), mask, server))
    };

    for _ in 0..30 {
        socket.send_to(&packet(1, &[]), "255.255.255.255:67")?; // DISCOVER
        let Some((ip, _, server)) = reply(2) else { continue }; // OFFER
        let mut want = vec![50, 4];
        want.extend(ip);
        want.extend([54, 4]);
        want.extend(server);
        socket.send_to(&packet(3, &want), "255.255.255.255:67")?; // REQUEST
        let Some((ip, mask, _)) = reply(5) else { continue }; // ACK
        ioctl(&socket, libc::SIOCSIFADDR, ifreq(interface, &ip).as_mut_ptr())?;
        ioctl(&socket, libc::SIOCSIFNETMASK, ifreq(interface, &mask).as_mut_ptr())?;
        return Ok((ip, mask));
    }
    Err(io::Error::other("no DHCP answer"))
}

/// The ISO drive lives on this network link, but the installer we hand over
/// to manages the same card and may take it down, rename it or strip its
/// address (Debian's does at startup; NetworkManager can on leaving the
/// initrd). Once a second, put back whatever is missing.
/// shortcut: fights the installer instead of using a link of our own.
fn keep_network(net: &str) {
    let mut fields = net.split(' ');
    let (Some(mac), Some(ip), Some(mask)) = (fields.next(), fields.next(), fields.next()) else { return };
    let (Ok(ip), Ok(mask)) = (ip.parse::<Ipv4Addr>(), mask.parse::<Ipv4Addr>()) else { return };
    loop {
        thread::sleep(Duration::from_secs(1));
        // By hardware address: the installer may have renamed the interface.
        let Some(name) = fs::read_dir("/sys/class/net").ok().and_then(|dir| {
            dir.flatten()
                .find(|e| fs::read_to_string(e.path().join("address")).is_ok_and(|a| a.trim() == mac))
                .and_then(|e| e.file_name().into_string().ok())
        }) else {
            continue;
        };
        let Ok(socket) = UdpSocket::bind("0.0.0.0:0") else { continue };
        let mut repaired = Vec::new();
        let mut flags = ifreq(&name, &[]);
        if ioctl(&socket, libc::SIOCGIFFLAGS, flags.as_mut_ptr()).is_ok() && flags[16] & libc::IFF_UP as u8 == 0 {
            flags[16] |= libc::IFF_UP as u8;
            let _ = ioctl(&socket, libc::SIOCSIFFLAGS, flags.as_mut_ptr());
            repaired.push("link was down");
        }
        let mut current = ifreq(&name, &[]);
        let has_address = ioctl(&socket, libc::SIOCGIFADDR, current.as_mut_ptr()).is_ok() && current[20..24] == ip.octets();
        if !has_address {
            let _ = ioctl(&socket, libc::SIOCSIFADDR, ifreq(&name, &ip.octets()).as_mut_ptr());
            let _ = ioctl(&socket, libc::SIOCSIFNETMASK, ifreq(&name, &mask.octets()).as_mut_ptr());
            repaired.push("address was gone");
        }
        if !repaired.is_empty() {
            log(format!("network repaired on {name}: {}", repaired.join(", ")));
        }
    }
}

/// A `struct ifreq` holding the interface name and, optionally, an IPv4 address.
fn ifreq(interface: &str, address: &[u8]) -> [u8; 40] {
    let mut req = [0u8; 40];
    req[..interface.len()].copy_from_slice(interface.as_bytes());
    if !address.is_empty() {
        req[16..18].copy_from_slice(&(libc::AF_INET as u16).to_ne_bytes());
        req[20..24].copy_from_slice(address);
    }
    req
}

fn ioctl<T>(fd: &impl AsRawFd, request: libc::c_ulong, arg: T) -> io::Result<()> {
    match unsafe { libc::ioctl(fd.as_raw_fd(), request as _, arg) } {
        -1 => Err(io::Error::last_os_error()),
        _ => Ok(()),
    }
}

// ---- NBD -----------------------------------------------------------------

/// Appends a netlink attribute, padded to 4 bytes.
fn attr(out: &mut Vec<u8>, kind: u16, payload: &[u8]) {
    out.extend((4 + payload.len() as u16).to_ne_bytes());
    out.extend(kind.to_ne_bytes());
    out.extend(payload);
    out.resize(out.len().next_multiple_of(4), 0);
}

/// Sends one generic-netlink request and returns the reply's attributes.
/// With `ack`, waits for the kernel's verdict and turns a refusal into an error.
fn genl(socket: i32, family: u16, command: u8, ack: bool, attrs: &[u8]) -> io::Result<Vec<u8>> {
    let flags: u16 = if ack { 1 | 4 } else { 1 }; // NLM_F_REQUEST, NLM_F_ACK
    let mut msg = Vec::new();
    msg.extend((20 + attrs.len() as u32).to_ne_bytes());
    msg.extend(family.to_ne_bytes());
    msg.extend(flags.to_ne_bytes());
    msg.extend([0; 8]); // sequence, port
    msg.extend([command, 1, 0, 0]); // genlmsghdr: command, version
    msg.extend(attrs);
    if unsafe { libc::send(socket, msg.as_ptr().cast(), msg.len(), 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut reply = Vec::new();
    loop {
        let mut buf = [0u8; 4096];
        let n = unsafe { libc::recv(socket, buf.as_mut_ptr().cast(), buf.len(), 0) };
        if n < 20 {
            return Err(io::Error::last_os_error());
        }
        let mut rest = &buf[..n as usize];
        while rest.len() >= 16 {
            let len = (u32::from_ne_bytes(rest[..4].try_into().unwrap()) as usize).clamp(16, rest.len());
            if u16::from_ne_bytes([rest[4], rest[5]]) == 2 {
                // NLMSG_ERROR: a negative errno, or 0 for an acknowledgement
                return match i32::from_ne_bytes(rest[16..20].try_into().unwrap()) {
                    0 => Ok(reply),
                    e => Err(io::Error::from_raw_os_error(-e)),
                };
            }
            reply = rest[20..len].to_vec();
            rest = &rest[len.next_multiple_of(4).min(rest.len())..];
        }
        if !ack {
            return Ok(reply);
        }
    }
}

/// Connects /dev/nbd0 to `server_end` through the kernel's netlink interface.
/// Not the older ioctl interface: that needs a thread parked in the kernel,
/// and systemd's SIGSTOP of every process on leaving the initrd interrupts it
/// and drops the device.
fn connect_nbd(server_end: &UnixStream, size: u64) -> io::Result<()> {
    let socket = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW, libc::NETLINK_GENERIC) };
    if socket < 0 {
        return Err(io::Error::last_os_error());
    }
    // Ask the controller (family 16, CTRL_CMD_GETFAMILY) for the "nbd" family's id.
    let mut name = Vec::new();
    attr(&mut name, 2, b"nbd\0");
    let mut reply = genl(socket, 16, 3, false, &name)?;
    let mut family = None;
    while reply.len() >= 4 {
        let len = (u16::from_ne_bytes([reply[0], reply[1]]) as usize).clamp(4, reply.len());
        if u16::from_ne_bytes([reply[2], reply[3]]) == 1 && len >= 6 {
            family = Some(u16::from_ne_bytes([reply[4], reply[5]]));
        }
        reply.drain(..len.next_multiple_of(4).min(reply.len()));
    }
    let family = family.ok_or_else(|| io::Error::other("kernel has no nbd netlink family"))?;

    // NBD_CMD_CONNECT: device index, size, server flags (has-flags | read-only), sockets.
    let (mut fd, mut item, mut attrs) = (Vec::new(), Vec::new(), Vec::new());
    attr(&mut fd, 1, &(server_end.as_raw_fd() as u32).to_ne_bytes());
    attr(&mut item, 1 | 0x8000, &fd);
    attr(&mut attrs, 1, &0u32.to_ne_bytes());
    attr(&mut attrs, 2, &size.to_ne_bytes());
    attr(&mut attrs, 5, &3u64.to_ne_bytes());
    attr(&mut attrs, 7 | 0x8000, &item);
    let result = genl(socket, family, 1, true, &attrs).map(drop);
    unsafe { libc::close(socket) };
    result
}

/// Gives the kernel one end of a socket pair as /dev/nbd0's "server" and
/// answers its requests on the other end.
fn serve_nbd() -> io::Result<()> {
    let (host, path) = server()?;
    let mut http = Http { host, path, connection: None };
    let size = http.fetch("HEAD", 0, 0).0;
    let (kernel_end, mut ours) = UnixStream::pair()?;
    connect_nbd(&kernel_end, size)?;
    drop(kernel_end); // the kernel holds its own reference now

    if let Ok(net) = std::env::var("OXI_NET") {
        thread::spawn(move || keep_network(&net));
    }

    loop {
        // magic, flags, type, handle, offset, length
        let mut request = [0u8; 28];
        ours.read_exact(&mut request)?;
        if request[..4] != [0x25, 0x60, 0x95, 0x13] {
            return Err(io::Error::other("bad NBD request"));
        }
        let kind = u16::from_be_bytes([request[6], request[7]]);
        let offset = u64::from_be_bytes(request[16..24].try_into().unwrap());
        let length = u32::from_be_bytes(request[24..28].try_into().unwrap());
        // magic, error, handle
        let mut reply = vec![0x67, 0x44, 0x66, 0x98, 0, 0, 0, 0];
        reply.extend(&request[8..16]);
        match kind {
            0 => reply.extend(http.fetch("GET", offset, length as u64).1),
            2 => return Err(io::Error::other("kernel disconnected")),
            _ => reply[7] = libc::EPERM as u8, // read-only
        }
        ours.write_all(&reply)?;
    }
}

struct Http {
    host: String,
    path: String,
    connection: Option<BufReader<TcpStream>>,
}

impl Http {
    /// (total size, bytes of the range). Retries forever: the installer may
    /// reconfigure the network under us, and a failed read would kill it.
    fn fetch(&mut self, method: &str, offset: u64, len: u64) -> (u64, Vec<u8>) {
        loop {
            match self.try_fetch(method, offset, len) {
                Ok(result) => return result,
                Err(e) => {
                    log(format!("{method} at {offset} failed ({e}), retrying"));
                    self.connection = None;
                    thread::sleep(Duration::from_secs(1));
                }
            }
        }
    }

    fn try_fetch(&mut self, method: &str, offset: u64, len: u64) -> io::Result<(u64, Vec<u8>)> {
        if self.connection.is_none() {
            let stream = TcpStream::connect(&self.host)?;
            stream.set_read_timeout(Some(Duration::from_secs(10)))?;
            stream.set_nodelay(true)?;
            self.connection = Some(BufReader::new(stream));
        }
        let (host, path, conn) = (&self.host, &self.path, self.connection.as_mut().unwrap());
        let range = match len {
            0 => String::new(),
            _ => format!("Range: bytes={offset}-{}\r\n", offset + len - 1),
        };
        write!(conn.get_mut(), "{method} {path} HTTP/1.1\r\nHost: {host}\r\n{range}\r\n")?;

        let (mut line, mut length, mut total) = (String::new(), 0u64, 0u64);
        conn.read_line(&mut line)?;
        if !line.starts_with("HTTP/1.1 20") && !line.starts_with("HTTP/1.0 20") {
            return Err(io::Error::other(format!("server said {}", line.trim())));
        }
        loop {
            line.clear();
            conn.read_line(&mut line)?;
            let Some((name, value)) = line.trim().split_once(':') else { break };
            match name.to_ascii_lowercase().as_str() {
                "content-length" => length = value.trim().parse().unwrap_or(0),
                "content-range" => total = value.rsplit('/').next().and_then(|t| t.parse().ok()).unwrap_or(0),
                _ => {}
            }
        }
        if method == "HEAD" {
            return Ok((length, Vec::new()));
        }
        if length != len {
            return Err(io::Error::other("server ignored the range"));
        }
        let mut body = vec![0; length as usize];
        conn.read_exact(&mut body)?;
        Ok((total, body))
    }
}
