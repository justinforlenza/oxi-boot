//! DHCP for network boot clients.
//!
//! Proxy mode adds a boot file to the answers of the network's own DHCP
//! server and hands out no addresses. Full mode is the network's DHCP server.
//! Either way firmware is sent the iPXE file for its type over TFTP, and iPXE
//! is sent the boot script over HTTP.

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use dhcproto::v4::{DhcpOption, Message, MessageType, Opcode, OptionCode};
use dhcproto::{Decodable, Encodable};
use socket2::{Domain, Socket, Type};
use tokio::net::UdpSocket;

use crate::config;

const LEASE: Duration = Duration::from_secs(3600);

/// The serving interface's address.
#[derive(Debug, Clone, Copy)]
pub struct Iface {
    pub ip: Ipv4Addr,
    pub mask: Ipv4Addr,
}

pub fn iface(name: &str) -> io::Result<Iface> {
    if_addrs::get_if_addrs()?
        .into_iter()
        .find_map(|i| match i.addr {
            if_addrs::IfAddr::V4(v4) if i.name == name => Some(Iface { ip: v4.ip, mask: v4.netmask }),
            _ => None,
        })
        .ok_or_else(|| io::Error::other(format!("interface {name} has no IPv4 address")))
}

/// A UDP socket on `port` that hears broadcasts on `iface` only and may send
/// them. Address reuse lets it share the port with another DHCP server on the
/// same host (see docs/dhcp-port-clash.md).
fn udp(iface: &str, port: u16) -> io::Result<UdpSocket> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, None)?;
    sock.set_reuse_address(true)?;
    sock.set_broadcast(true)?;
    sock.bind_device(Some(iface.as_bytes()))?;
    sock.set_nonblocking(true)?;
    sock.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)).into())?;
    UdpSocket::from_std(sock.into())
}

fn kind(packet: &Message) -> Option<MessageType> {
    packet.opts().msg_type()
}

/// What a network boot client should load: None if it is not one, or is of a
/// type we have no iPXE file for.
fn boot_file(packet: &Message, ip: Ipv4Addr) -> Option<String> {
    let Some(DhcpOption::ClassIdentifier(class)) = packet.opts().get(OptionCode::ClassIdentifier) else { return None };
    if !class.starts_with(b"PXEClient") {
        return None;
    }
    if let Some(DhcpOption::UserClass(user)) = packet.opts().get(OptionCode::UserClass)
        && user.windows(4).any(|w| w == b"iPXE") {
            return Some(format!("http://{ip}/boot.ipxe"));
        }
    let arch = match packet.opts().get(OptionCode::ClientSystemArchitecture) {
        Some(DhcpOption::ClientSystemArchitecture(arch)) => u16::from(*arch),
        // Old BIOS firmware names its type only inside the class string ("PXEClient:Arch:00000:...").
        _ => String::from_utf8_lossy(class).split(':').skip_while(|part| *part != "Arch").nth(1)?.parse().ok()?,
    };
    match arch {
        0 => Some("undionly.kpxe".into()),
        7 | 9 => Some("snponly.efi".into()),
        _ => None, // ARM64 is phase 5
    }
}

fn reply(packet: &Message, kind: MessageType, yiaddr: Ipv4Addr, ip: Ipv4Addr, file: Option<&str>, options: Vec<DhcpOption>) -> Message {
    let unset = Ipv4Addr::UNSPECIFIED;
    let mut out = Message::new_with_id(packet.xid(), unset, yiaddr, unset, packet.giaddr(), packet.chaddr());
    out.set_opcode(Opcode::BootReply).set_htype(packet.htype()).set_flags(packet.flags());
    if let Some(file) = file {
        out.set_siaddr(ip).set_fname_str(file); // siaddr is the TFTP server
    }
    out.opts_mut().insert(DhcpOption::MessageType(kind));
    out.opts_mut().insert(DhcpOption::ServerIdentifier(ip));
    for option in options {
        out.opts_mut().insert(option);
    }
    // PXE firmware drops answers that do not echo its machine identifier.
    if let Some(uuid) = packet.opts().get(OptionCode::ClientMachineIdentifier) {
        out.opts_mut().insert(uuid.clone());
    }
    out
}

/// Proxy mode's answer: an offer on the DHCP port, and an ACK on the boot
/// server port for firmware that asks again there.
fn proxy_reply(packet: &Message, ip: Ipv4Addr, boot_server_port: bool) -> Option<Message> {
    let file = boot_file(packet, ip)?;
    let answer = match (kind(packet)?, boot_server_port) {
        (MessageType::Discover, false) => MessageType::Offer,
        (MessageType::Request, true) => MessageType::Ack,
        _ => return None,
    };
    let options = vec![
        DhcpOption::ClassIdentifier(b"PXEClient".to_vec()),
        // PXE discovery control 8: boot the named file, do not look for boot servers.
        DhcpOption::VendorExtensions(vec![6, 1, 8, 255]),
    ];
    Some(reply(packet, answer, Ipv4Addr::UNSPECIFIED, ip, Some(&file), options))
}

/// Full mode: the address pool and what each client is told.
pub struct Full {
    iface: Iface,
    first: u32,
    last: u32,
    router: Option<Ipv4Addr>,
    dns: Vec<Ipv4Addr>,
    // shortcut: leases live in memory, so a restart forgets them and a client
    // keeps its address only if it asks again before another client takes it.
    // Save them to a file if that causes address clashes.
    leases: HashMap<Vec<u8>, (Ipv4Addr, Instant)>,
}

impl Full {
    pub fn new(iface: Iface, cfg: &config::Dhcp) -> Result<Full, String> {
        let range = cfg.range.as_deref().ok_or("dhcp.mode = \"full\" needs dhcp.range")?;
        let parsed = range.split_once('-').and_then(|(a, b)| Some((a.trim().parse().ok()?, b.trim().parse().ok()?)));
        let (first, last): (Ipv4Addr, Ipv4Addr) = parsed.ok_or(format!("dhcp.range {range:?} is not \"first-last\""))?;
        let (ip, mask) = (u32::from(iface.ip), u32::from(iface.mask));
        if first > last || u32::from(first) & mask != ip & mask || u32::from(last) & mask != ip & mask {
            return Err(format!("dhcp.range {range} is not inside the interface's network ({}/{})", iface.ip, iface.mask));
        }
        let (router, dns) = (cfg.router, cfg.dns.clone());
        Ok(Full { iface, first: first.into(), last: last.into(), router, dns, leases: HashMap::new() })
    }

    /// The client's current address, else the one it wants if free, else the first free one.
    fn allocate(&mut self, mac: &[u8], wanted: Option<Ipv4Addr>, now: Instant) -> Option<Ipv4Addr> {
        self.leases.retain(|_, (_, expires)| *expires > now);
        let taken = |ip: &Ipv4Addr| *ip == self.iface.ip || self.leases.values().any(|(held, _)| held == ip);
        let ip = match self.leases.get(mac) {
            Some((ip, _)) => *ip,
            None => wanted
                .filter(|ip| (self.first..=self.last).contains(&u32::from(*ip)) && !taken(ip))
                .or_else(|| (self.first..=self.last).map(Ipv4Addr::from).find(|ip| !taken(ip)))?,
        };
        self.leases.insert(mac.to_vec(), (ip, now + LEASE));
        Some(ip)
    }

    fn reply(&mut self, packet: &Message, now: Instant) -> Option<Message> {
        let ip = self.iface.ip;
        let mac = packet.chaddr();
        let requested = match packet.opts().get(OptionCode::RequestedIpAddress) {
            Some(DhcpOption::RequestedIpAddress(ip)) => Some(*ip),
            _ => None,
        };
        let granted = match kind(packet)? {
            MessageType::Discover => self.allocate(mac, requested, now)?,
            MessageType::Request => {
                if let Some(DhcpOption::ServerIdentifier(id)) = packet.opts().get(OptionCode::ServerIdentifier)
                    && *id != ip {
                        self.leases.remove(mac); // the client took another server's offer
                        return None;
                    }
                let wanted = requested.unwrap_or(packet.ciaddr());
                if self.allocate(mac, Some(wanted), now) != Some(wanted) {
                    return Some(reply(packet, MessageType::Nak, Ipv4Addr::UNSPECIFIED, ip, None, vec![]));
                }
                wanted
            }
            MessageType::Decline | MessageType::Release => {
                self.leases.remove(mac);
                return None;
            }
            _ => return None,
        };
        let mut options = vec![DhcpOption::AddressLeaseTime(LEASE.as_secs() as u32), DhcpOption::SubnetMask(self.iface.mask)];
        if let Some(router) = self.router {
            options.push(DhcpOption::Router(vec![router]));
        }
        if !self.dns.is_empty() {
            options.push(DhcpOption::DomainNameServer(self.dns.clone()));
        }
        let answer = if kind(packet) == Some(MessageType::Discover) { MessageType::Offer } else { MessageType::Ack };
        Some(reply(packet, answer, granted, ip, boot_file(packet, ip).as_deref(), options))
    }
}

/// Answers clients until a socket fails. `full` is None in proxy mode.
/// shortcut: requests forwarded by a DHCP relay are answered as if local, so
/// clients on other subnets get nothing. Add relay support if asked for.
pub async fn serve(iface_name: &str, iface: Iface, mut full: Option<Full>) -> io::Result<()> {
    let sock = udp(iface_name, 67)?;
    if full.is_none() {
        let boot_server = udp(iface_name, 4011)?;
        tokio::spawn(async move {
            let result = listen(&boot_server, |packet, peer| Some((proxy_reply(packet, iface.ip, true)?, peer))).await;
            eprintln!("dhcp: boot server port stopped: {result:?}");
        });
    }
    listen(&sock, |packet, _| {
        let answer = match &mut full {
            Some(full) => full.reply(packet, Instant::now())?,
            None => proxy_reply(packet, iface.ip, false)?,
        };
        // A client that already has an address is answered there; one that has none hears a broadcast.
        let to = if packet.ciaddr().is_unspecified() { Ipv4Addr::BROADCAST } else { packet.ciaddr() };
        Some((answer, SocketAddr::from((to, 68))))
    })
    .await
}

async fn listen(sock: &UdpSocket, mut answer: impl FnMut(&Message, SocketAddr) -> Option<(Message, SocketAddr)>) -> io::Result<()> {
    let mut buf = [0; 1500];
    loop {
        let (n, peer) = sock.recv_from(&mut buf).await?;
        let Ok(packet) = Message::from_bytes(&buf[..n]) else { continue };
        if packet.opcode() != Opcode::BootRequest {
            continue;
        }
        let Some((message, to)) = answer(&packet, peer) else { continue };
        let mac = packet.chaddr().iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":");
        eprintln!("dhcp: {mac}: {:?}, address {}, file {:?}", message.opts().msg_type(), message.yiaddr(), message.fname_str());
        let Ok(mut bytes) = message.to_vec() else { continue };
        bytes.resize(bytes.len().max(300), 0); // some firmware drops packets under the BOOTP minimum
        if let Err(e) = sock.send_to(&bytes, to).await {
            eprintln!("dhcp: cannot answer {to}: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dhcproto::v4::Architecture;

    const IP: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 5);

    fn request(kind: MessageType, mac_last: u8, options: Vec<DhcpOption>) -> Message {
        let unset = Ipv4Addr::UNSPECIFIED;
        let mut m = Message::new(unset, unset, unset, unset, &[0x52, 0x54, 0, 0, 0, mac_last]);
        m.opts_mut().insert(DhcpOption::MessageType(kind));
        for option in options {
            m.opts_mut().insert(option);
        }
        // Through the wire format, as a real request arrives.
        Message::from_bytes(&m.to_vec().unwrap()).unwrap()
    }

    fn pxe(arch: u16) -> Vec<DhcpOption> {
        vec![DhcpOption::ClassIdentifier(b"PXEClient".to_vec()), DhcpOption::ClientSystemArchitecture(Architecture::from(arch))]
    }

    fn file(m: &Message) -> &str {
        m.fname_str().map_or("", |f| f.unwrap())
    }

    #[test]
    fn proxy_picks_the_boot_file_by_client_type() {
        use MessageType::*;
        let ask = |options, kind, port| proxy_reply(&request(kind, 1, options), IP, port);

        let old = vec![DhcpOption::ClassIdentifier(b"PXEClient:Arch:00000:UNDI:002001".to_vec())];
        let bios = ask(old, Discover, false).unwrap();
        assert_eq!(file(&bios), "undionly.kpxe");
        assert_eq!(bios.siaddr(), IP);
        assert!(bios.yiaddr().is_unspecified()); // a proxy offers no address
        assert_eq!(kind(&bios), Some(Offer));
        assert_eq!(bios.opts().get(OptionCode::ClassIdentifier), Some(&DhcpOption::ClassIdentifier(b"PXEClient".to_vec())));

        let uuid = DhcpOption::ClientMachineIdentifier(vec![0, 9, 9]);
        let uefi = ask([pxe(7), vec![uuid.clone()]].concat(), Request, true).unwrap();
        assert_eq!(file(&uefi), "snponly.efi");
        assert_eq!(kind(&uefi), Some(Ack));
        assert_eq!(uefi.opts().get(OptionCode::ClientMachineIdentifier), Some(&uuid));

        let ipxe = ask([pxe(0), vec![DhcpOption::UserClass(b"iPXE".to_vec())]].concat(), Discover, false).unwrap();
        assert_eq!(file(&ipxe), "http://192.168.1.5/boot.ipxe");

        assert!(ask(vec![], Discover, false).is_none()); // an ordinary DHCP client
        assert!(ask(pxe(11), Discover, false).is_none()); // ARM64
        assert!(ask(pxe(0), Request, false).is_none()); // meant for the real server
    }

    #[test]
    fn full_mode_leases_addresses() {
        use MessageType::*;
        let iface = Iface { ip: IP, mask: Ipv4Addr::new(255, 255, 255, 0) };
        let cfg = config::Dhcp { range: Some("192.168.1.10-192.168.1.11".into()), router: Some(IP), ..Default::default() };
        let mut full = Full::new(iface, &cfg).unwrap();
        let now = Instant::now();
        let mut ask = |kind, mac, options, at| full.reply(&request(kind, mac, options), at);
        let want = |ip: [u8; 4]| DhcpOption::RequestedIpAddress(ip.into());

        let offer = ask(Discover, 1, pxe(0), now).unwrap();
        assert_eq!(offer.yiaddr(), Ipv4Addr::new(192, 168, 1, 10));
        assert_eq!(offer.opts().get(OptionCode::SubnetMask), Some(&DhcpOption::SubnetMask(iface.mask)));
        assert_eq!(offer.opts().get(OptionCode::Router), Some(&DhcpOption::Router(vec![IP])));
        assert_eq!(file(&offer), "undionly.kpxe");

        let ack = ask(Request, 1, vec![want([192, 168, 1, 10]), DhcpOption::ServerIdentifier(IP)], now).unwrap();
        assert_eq!(kind(&ack), Some(Ack));
        // Someone else's address is refused; an ordinary client gets an address and no boot file.
        let nak = ask(Request, 2, vec![want([192, 168, 1, 10])], now).unwrap();
        assert_eq!(kind(&nak), Some(Nak));
        let second = ask(Discover, 2, vec![], now).unwrap();
        assert_eq!(second.yiaddr(), Ipv4Addr::new(192, 168, 1, 11));
        assert_eq!(file(&second), "");
        // The pool is empty until a lease runs out.
        assert!(ask(Discover, 3, vec![], now).is_none());
        assert!(ask(Discover, 3, vec![], now + LEASE + Duration::from_secs(1)).is_some());
    }

    #[test]
    fn full_mode_checks_its_range() {
        let iface = Iface { ip: IP, mask: Ipv4Addr::new(255, 255, 255, 0) };
        let with = |range: Option<&str>| Full::new(iface, &config::Dhcp { range: range.map(Into::into), ..Default::default() });
        assert!(with(None).is_err());
        assert!(with(Some("192.168.1.10")).is_err());
        assert!(with(Some("192.168.2.10-192.168.2.20")).is_err());
        assert!(with(Some("192.168.1.10 - 192.168.1.20")).is_ok());
    }
}
