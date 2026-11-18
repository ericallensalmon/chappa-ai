//! The TCP relay: how containers on an engine that reaches the host only
//! at its gateway address (Windows containers under docker, behind NAT)
//! get to ports another engine published on host loopback.
//!
//! One listener per published port of the running instance, bound to the
//! gateway address of each engine in the stack's `relay.engines`, each
//! forwarding to `127.0.0.1:<same port>`. Rules:
//!
//! - it binds **only** a gateway address. An unspecified address
//!   (`0.0.0.0`, `::`) would put every stack port on the LAN; it is refused
//!   and reported as an alert, never bound;
//! - it serves only the container network: when a listener binds, the
//!   relay looks up the interface that owns the gateway address and its
//!   prefix (read again on every bind, since the gateway's subnet moves),
//!   and every accepted connection from a peer outside that prefix is
//!   closed at once and counted. Binding the gateway address alone does not
//!   guarantee that: a host with forwarding or weak-host receive on can
//!   deliver traffic from elsewhere to it;
//! - a gateway that is an alias the engine resolves itself
//!   (`host-gateway`) needs no relay: those containers already reach host
//!   loopback;
//! - ports of components on the relayed engine itself are not relayed:
//!   its containers reach those inside the engine;
//! - each listener serves at most [`MAX_CONNECTIONS`] connections at once;
//!   more are closed on accept;
//! - listeners start when the instance has something running and stop when
//!   it has nothing (the set is reconciled on every status);
//! - they are real listeners on an address other than loopback, so on
//!   Windows they need an inbound firewall rule: [`crate::relay_firewall`]
//!   checks for one (and for what would defeat it) and offers to add it.
//!
//! Plain std threads: one accept loop per listener (non-blocking, polled,
//! so a stop takes effect within [`ACCEPT_POLL`]), two copy threads per
//! connection.

use indexmap::IndexMap;
use std::collections::BTreeSet;
use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Open connections per listener.
pub const MAX_CONNECTIONS: usize = 64;
/// How often an idle accept loop checks for a stop.
pub const ACCEPT_POLL: Duration = Duration::from_millis(100);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// One listener the instance should have.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Wanted {
    pub engine: String,
    pub bind: SocketAddr,
    pub target: SocketAddr,
}

/// An address prefix: `172.20.0.0/20`, `fd00::/64`. Always stored with the
/// host bits cleared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Network {
    addr: IpAddr,
    len: u8,
}

impl Network {
    /// The prefix of `len` bits holding `addr`; `None` when `len` is longer
    /// than the family allows.
    pub fn new(addr: IpAddr, len: u8) -> Option<Network> {
        let addr = match addr {
            IpAddr::V4(a) if len <= 32 => IpAddr::V4(Ipv4Addr::from(mask_v4(u32::from(a), len))),
            IpAddr::V6(a) if len <= 128 => IpAddr::V6(Ipv6Addr::from(mask_v6(u128::from(a), len))),
            _ => return None,
        };
        Some(Network { addr, len })
    }

    /// `a.b.c.d/len`, `a.b.c.d/m.m.m.m` (a contiguous mask) or `v6/len`.
    pub fn parse(text: &str) -> Option<Network> {
        let (addr, len) = text.trim().split_once('/')?;
        let addr: IpAddr = addr.parse().ok()?;
        let len = match (addr, len.parse::<u8>()) {
            (_, Ok(n)) => n,
            (IpAddr::V4(_), Err(_)) => {
                let mask = u32::from(len.parse::<Ipv4Addr>().ok()?);
                let ones = mask.leading_ones();
                if mask.checked_shl(ones).unwrap_or(0) != 0 {
                    return None;
                }
                ones as u8
            }
            (IpAddr::V6(_), Err(_)) => return None,
        };
        Network::new(addr, len)
    }

    pub fn addr(&self) -> IpAddr {
        self.addr
    }

    pub fn prefix_len(&self) -> u8 {
        self.len
    }

    /// Whether `ip` is inside. An IPv4 address mapped into IPv6
    /// (`::ffff:a.b.c.d`) counts as the IPv4 address; the families never
    /// match each other otherwise.
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, ip.to_canonical()) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => mask_v4(u32::from(ip), self.len) == u32::from(net),
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                mask_v6(u128::from(ip), self.len) == u128::from(net)
            }
            _ => false,
        }
    }

    /// Whether every address of `other` is inside this one.
    pub fn covers(&self, other: &Network) -> bool {
        other.len >= self.len && self.contains(other.addr)
    }
}

impl fmt::Display for Network {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.len)
    }
}

fn mask_v4(v: u32, len: u8) -> u32 {
    if len == 0 {
        0
    } else {
        v & (u32::MAX << (32 - u32::from(len)))
    }
}

fn mask_v6(v: u128, len: u8) -> u128 {
    if len == 0 {
        0
    } else {
        v & (u128::MAX << (128 - u32::from(len)))
    }
}

/// Whether a connection from `peer` is served: only when it comes from
/// inside the gateway's network. With no network known (the lookup failed,
/// which the state reports) the gateway-only bind is the remaining guard
/// and the connection is served.
pub fn admit(peer: IpAddr, network: Option<&Network>) -> bool {
    network.is_none_or(|n| n.contains(peer))
}

/// The prefix of the interface that owns `ip`, from the operating system's
/// interface table. `Ok(None)` when no interface has that address.
pub fn interface_network(ip: IpAddr) -> io::Result<Option<Network>> {
    Ok(os_networks()?
        .into_iter()
        .find(|(addr, _)| *addr == ip)
        .and_then(|(addr, len)| Network::new(addr, len)))
}

/// Every unicast address of every interface with its on-link prefix length.
#[cfg(windows)]
fn os_networks() -> io::Result<Vec<(IpAddr, u8)>> {
    use windows_sys::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, NO_ERROR};
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
        GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows_sys::Win32::Networking::WinSock::{
        AF_INET, AF_INET6, AF_UNSPEC, SOCKADDR_IN, SOCKADDR_IN6,
    };

    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    // The table changes between the size query and the read when an
    // adapter comes or goes, so a too-small buffer is retried a few times.
    let mut size: u32 = 16 * 1024;
    for _ in 0..4 {
        // u64 elements keep the buffer aligned for the structures in it.
        let mut buf = vec![0u64; (size as usize).div_ceil(8)];
        let head = buf.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
        // SAFETY: `head` points at `size` writable bytes, as the call is told.
        let rc = unsafe {
            GetAdaptersAddresses(
                u32::from(AF_UNSPEC),
                flags,
                std::ptr::null(),
                head,
                &mut size,
            )
        };
        if rc == ERROR_BUFFER_OVERFLOW {
            continue;
        }
        if rc != NO_ERROR {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
        let mut out = Vec::new();
        let mut adapter = head.cast_const();
        // SAFETY: on success the buffer holds a linked list of adapters and
        // their unicast addresses, all inside `buf`, which outlives the walk.
        unsafe {
            while let Some(a) = adapter.as_ref() {
                let mut unicast = a.FirstUnicastAddress.cast_const();
                while let Some(u) = unicast.as_ref() {
                    let sa = u.Address.lpSockaddr;
                    if !sa.is_null() {
                        let ip = match (*sa).sa_family {
                            f if f == AF_INET => {
                                let v4 = &*sa.cast::<SOCKADDR_IN>();
                                Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(
                                    v4.sin_addr.S_un.S_addr,
                                ))))
                            }
                            f if f == AF_INET6 => {
                                let v6 = &*sa.cast::<SOCKADDR_IN6>();
                                Some(IpAddr::V6(Ipv6Addr::from(v6.sin6_addr.u.Byte)))
                            }
                            _ => None,
                        };
                        if let Some(ip) = ip {
                            out.push((ip, u.OnLinkPrefixLength));
                        }
                    }
                    unicast = u.Next.cast_const();
                }
                adapter = a.Next.cast_const();
            }
        }
        return Ok(out);
    }
    Err(io::Error::other(
        "the interface table kept changing while it was read",
    ))
}

/// Every address of every interface with its netmask's prefix length.
#[cfg(unix)]
fn os_networks() -> io::Result<Vec<(IpAddr, u8)>> {
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: `getifaddrs` fills `head` with a list freed below.
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut out = Vec::new();
    let mut cur = head;
    // SAFETY: the list and its sockaddrs stay valid until `freeifaddrs`.
    unsafe {
        while let Some(ifa) = cur.as_ref() {
            if let (Some(addr), Some(mask)) =
                (sockaddr_ip(ifa.ifa_addr), sockaddr_ip(ifa.ifa_netmask))
            {
                let len = match mask {
                    IpAddr::V4(m) => u32::from(m).leading_ones() as u8,
                    IpAddr::V6(m) => u128::from(m).leading_ones() as u8,
                };
                if addr.is_ipv4() == mask.is_ipv4() {
                    out.push((addr, len));
                }
            }
            cur = ifa.ifa_next;
        }
        libc::freeifaddrs(head);
    }
    Ok(out)
}

/// # Safety
/// `sa` is null or points at a valid sockaddr.
#[cfg(unix)]
unsafe fn sockaddr_ip(sa: *const libc::sockaddr) -> Option<IpAddr> {
    let sa = sa.as_ref()?;
    match i32::from(sa.sa_family) {
        libc::AF_INET => {
            let v4 = &*(sa as *const libc::sockaddr).cast::<libc::sockaddr_in>();
            Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(v4.sin_addr.s_addr))))
        }
        libc::AF_INET6 => {
            let v6 = &*(sa as *const libc::sockaddr).cast::<libc::sockaddr_in6>();
            Some(IpAddr::V6(Ipv6Addr::from(v6.sin6_addr.s6_addr)))
        }
        _ => None,
    }
}

#[cfg(not(any(windows, unix)))]
fn os_networks() -> io::Result<Vec<(IpAddr, u8)>> {
    Err(io::Error::other("no interface table on this platform"))
}

/// Why the relay could not do what it should, for the alerts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayProblem {
    pub engine: String,
    pub text: String,
}

/// Which peers one gateway address serves, and how many it turned away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerGuard {
    pub engine: String,
    pub gateway: IpAddr,
    /// The gateway interface's prefix; `None` when it could not be found.
    pub network: Option<Network>,
    /// Connections closed because they came from outside `network`, since
    /// the relay first served this engine.
    pub refused: u64,
}

impl PeerGuard {
    /// The facts row's value: what is served and what was turned away.
    pub fn summary(&self) -> String {
        let serves = match &self.network {
            Some(n) => format!("serves only {n}, the container network"),
            None => "serves any peer (the gateway's network is not known)".to_string(),
        };
        match self.refused {
            0 => serves,
            1 => format!("{serves}; refused 1 connection from outside the container network"),
            n => format!("{serves}; refused {n} connections from outside the container network"),
        }
    }
}

/// What the relay is doing, for the facts section.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RelayState {
    /// `engine → bind → target`, every listener up now.
    pub listening: Vec<Wanted>,
    pub problems: Vec<RelayProblem>,
    /// Engines that need no relay, with the reason.
    pub not_needed: Vec<(String, String)>,
    /// One per gateway address listened on now.
    pub guards: Vec<PeerGuard>,
}

/// Work out the listeners from the engines' gateways and the ports.
/// `gateways` maps each relayed engine to its gateway address as the
/// engine reported it; `ports` are the host ports to relay.
pub fn plan(
    relayed: &[String],
    gateways: &IndexMap<String, String>,
    ports: &BTreeSet<u16>,
) -> (Vec<Wanted>, RelayState) {
    let mut wanted = Vec::new();
    let mut state = RelayState::default();
    for engine in relayed {
        let Some(address) = gateways.get(engine) else {
            state.problems.push(RelayProblem {
                engine: engine.clone(),
                text: format!("relay for {engine}: the gateway address is not known yet"),
            });
            continue;
        };
        let ip: IpAddr = match address.parse() {
            Ok(ip) => ip,
            Err(_) => {
                state.not_needed.push((
                    engine.clone(),
                    format!("{engine} resolves `{address}` itself; no relay needed"),
                ));
                continue;
            }
        };
        if ip.is_unspecified() {
            state.problems.push(RelayProblem {
                engine: engine.clone(),
                text: format!(
                    "relay for {engine}: refused to listen on {ip}, which would expose the stack's ports to the network"
                ),
            });
            continue;
        }
        for port in ports {
            wanted.push(Wanted {
                engine: engine.clone(),
                bind: SocketAddr::new(ip, *port),
                target: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), *port),
            });
        }
    }
    (wanted, state)
}

/// The gateway addresses the relay listens on for `relayed` (each
/// engine's, once), whatever ports are relayed now: the firewall check
/// looks at them before anything runs.
pub fn gateway_ips(relayed: &[String], gateways: &IndexMap<String, String>) -> Vec<IpAddr> {
    let mut out: Vec<IpAddr> = Vec::new();
    for engine in relayed {
        let Some(ip) = gateways.get(engine).and_then(|a| a.parse::<IpAddr>().ok()) else {
            continue;
        };
        if !ip.is_unspecified() && !out.contains(&ip) {
            out.push(ip);
        }
    }
    out
}

/// A started listener; dropping it stops it.
pub struct Listener {
    wanted: Wanted,
    network: Option<Network>,
    lookup_error: Option<String>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Listener {
    /// The network this listener serves, as looked up when it bound.
    pub fn network(&self) -> Option<Network> {
        self.network
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// The running listeners.
#[derive(Default)]
pub struct Relay {
    listeners: Vec<Listener>,
    /// The last bind failures, kept until the set changes or binds.
    failures: IndexMap<Wanted, String>,
    /// Refused connections per engine, kept across rebinds.
    refused: IndexMap<String, Arc<AtomicU64>>,
}

impl Relay {
    /// Make the running set equal `wanted`: stop what is no longer wanted,
    /// start what is missing (a failed bind is remembered and retried on
    /// the next call). Each gateway address that binds has its network
    /// looked up again. True when anything new started listening.
    pub fn reconcile(&mut self, wanted: &[Wanted]) -> bool {
        self.listeners.retain(|l| wanted.contains(&l.wanted));
        self.failures.retain(|w, _| wanted.contains(w));
        let mut looked_up: IndexMap<IpAddr, Result<Option<Network>, String>> = IndexMap::new();
        let mut bound = false;
        for w in wanted {
            if self.listeners.iter().any(|l| &l.wanted == w) {
                continue;
            }
            let network = looked_up
                .entry(w.bind.ip())
                .or_insert_with(|| match interface_network(w.bind.ip()) {
                    Ok(Some(n)) => Ok(Some(n)),
                    Ok(None) => Err(format!("no interface has the address {}", w.bind.ip())),
                    Err(e) => Err(e.to_string()),
                })
                .clone();
            let counter = self.refused.entry(w.engine.clone()).or_default().clone();
            let (network, lookup_error) = match network {
                Ok(n) => (n, None),
                Err(e) => (None, Some(e)),
            };
            match start_serving(w.clone(), network, counter) {
                Ok(mut l) => {
                    l.lookup_error = lookup_error;
                    self.failures.shift_remove(w);
                    self.listeners.push(l);
                    bound = true;
                }
                Err(e) => {
                    self.failures.insert(w.clone(), e.to_string());
                }
            }
        }
        bound
    }

    pub fn stop_all(&mut self) {
        self.listeners.clear();
        self.failures.clear();
    }

    /// The state for the view, merged with what [`plan`] found.
    pub fn state(&self, mut planned: RelayState) -> RelayState {
        planned.listening = self.listeners.iter().map(|l| l.wanted.clone()).collect();
        for l in &self.listeners {
            let gateway = l.wanted.bind.ip();
            if planned
                .guards
                .iter()
                .any(|g| g.engine == l.wanted.engine && g.gateway == gateway)
            {
                continue;
            }
            if let Some(e) = &l.lookup_error {
                planned.problems.push(RelayProblem {
                    engine: l.wanted.engine.clone(),
                    text: format!(
                        "relay for {}: cannot find the network of {gateway} ({e}), so connections are not checked by where they come from",
                        l.wanted.engine
                    ),
                });
            }
            planned.guards.push(PeerGuard {
                engine: l.wanted.engine.clone(),
                gateway,
                network: l.network,
                refused: self
                    .refused
                    .get(&l.wanted.engine)
                    .map_or(0, |c| c.load(Ordering::SeqCst)),
            });
        }
        for (w, e) in &self.failures {
            planned.problems.push(RelayProblem {
                engine: w.engine.clone(),
                text: format!("relay for {}: cannot listen on {} ({e})", w.engine, w.bind),
            });
        }
        planned
    }

    pub fn is_listening(&self, bind: SocketAddr) -> bool {
        self.listeners.iter().any(|l| l.wanted.bind == bind)
    }

    /// Connections refused for `engine` so far.
    pub fn refused(&self, engine: &str) -> u64 {
        self.refused
            .get(engine)
            .map_or(0, |c| c.load(Ordering::SeqCst))
    }
}

/// Bind and start one listener serving the network that owns its address.
/// Refuses an unspecified address outright, whatever the caller planned.
pub fn start(wanted: Wanted) -> io::Result<Listener> {
    let network = interface_network(wanted.bind.ip()).ok().flatten();
    start_serving(wanted, network, Arc::default())
}

/// [`start`] serving `network` only (every peer when `None`), counting the
/// connections it closes in `refused`.
pub fn start_serving(
    wanted: Wanted,
    network: Option<Network>,
    refused: Arc<AtomicU64>,
) -> io::Result<Listener> {
    if wanted.bind.ip().is_unspecified() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the relay never listens on an unspecified address",
        ));
    }
    let listener = TcpListener::bind(wanted.bind)?;
    listener.set_nonblocking(true)?;
    let stop = Arc::new(AtomicBool::new(false));
    let open = Arc::new(AtomicUsize::new(0));
    let guard = Guard {
        target: wanted.target,
        network,
        refused,
    };
    let flag = stop.clone();
    let handle = thread::spawn(move || accept_loop(listener, guard, flag, open));
    Ok(Listener {
        wanted,
        network,
        lookup_error: None,
        stop,
        handle: Some(handle),
    })
}

/// What one accept loop needs to decide and forward.
struct Guard {
    target: SocketAddr,
    network: Option<Network>,
    refused: Arc<AtomicU64>,
}

fn accept_loop(listener: TcpListener, guard: Guard, stop: Arc<AtomicBool>, open: Arc<AtomicUsize>) {
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((client, peer)) => {
                if !admit(peer.ip(), guard.network.as_ref()) {
                    guard.refused.fetch_add(1, Ordering::SeqCst);
                    let _ = client.shutdown(Shutdown::Both);
                    continue;
                }
                if open.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
                    let _ = client.shutdown(Shutdown::Both);
                    continue;
                }
                let _ = client.set_nonblocking(false);
                open.fetch_add(1, Ordering::SeqCst);
                let open = open.clone();
                let target = guard.target;
                thread::spawn(move || {
                    let _ = forward(client, target);
                    open.fetch_sub(1, Ordering::SeqCst);
                });
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(ACCEPT_POLL),
            Err(_) => thread::sleep(ACCEPT_POLL),
        }
    }
}

fn forward(client: TcpStream, target: SocketAddr) -> io::Result<()> {
    let upstream = match TcpStream::connect_timeout(&target, CONNECT_TIMEOUT) {
        Ok(s) => s,
        Err(e) => {
            let _ = client.shutdown(Shutdown::Both);
            return Err(e);
        }
    };
    let (mut c_read, mut c_write) = (client.try_clone()?, client);
    let (mut u_read, mut u_write) = (upstream.try_clone()?, upstream);
    let up = thread::spawn(move || {
        let _ = io::copy(&mut c_read, &mut u_write);
        let _ = u_write.shutdown(Shutdown::Write);
    });
    let _ = io::copy(&mut u_read, &mut c_write);
    let _ = c_write.shutdown(Shutdown::Write);
    let _ = up.join();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_bind_the_gateway_only_and_refuse_a_wildcard() {
        let ports: BTreeSet<u16> = [14001, 14002].into_iter().collect();
        let mut gw = IndexMap::new();
        gw.insert("docker".to_string(), "172.20.0.1".to_string());
        let (w, s) = plan(&["docker".into()], &gw, &ports);
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].bind.to_string(), "172.20.0.1:14001");
        assert_eq!(w[0].target.to_string(), "127.0.0.1:14001");
        assert!(s.problems.is_empty());

        gw.insert("docker".into(), "0.0.0.0".into());
        let (w, s) = plan(&["docker".into()], &gw, &ports);
        assert!(w.is_empty());
        assert!(s.problems[0].text.contains("refused to listen on 0.0.0.0"));
        gw.insert("docker".into(), "::".into());
        assert!(plan(&["docker".into()], &gw, &ports).0.is_empty());

        gw.insert("docker".into(), "host-gateway".into());
        let (w, s) = plan(&["docker".into()], &gw, &ports);
        assert!(w.is_empty());
        assert!(s.not_needed[0].1.contains("no relay needed"));

        let (_, s) = plan(&["podman".into()], &gw, &ports);
        assert!(s.problems[0].text.contains("not known"));
    }

    #[test]
    fn start_refuses_an_unspecified_address() {
        let w = Wanted {
            engine: "docker".into(),
            bind: "0.0.0.0:0".parse().unwrap(),
            target: "127.0.0.1:1".parse().unwrap(),
        };
        assert!(start(w).is_err());
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn v4_prefixes_clear_host_bits_and_contain_only_their_range() {
        let n = Network::new(ip("172.20.5.1"), 20).unwrap();
        assert_eq!(n.to_string(), "172.20.0.0/20");
        assert!(n.contains(ip("172.20.0.0")));
        assert!(n.contains(ip("172.20.15.255")));
        assert!(!n.contains(ip("172.20.16.0")));
        assert!(!n.contains(ip("172.19.255.255")));
        assert!(!n.contains(ip("10.0.0.1")));
        // A v4 peer reported as a mapped v6 address is the v4 address.
        assert!(n.contains(ip("::ffff:172.20.3.4")));
        assert!(!n.contains(ip("::ffff:192.168.1.1")));
        // Families never match each other otherwise.
        assert!(!n.contains(ip("fd00::1")));

        let host = Network::new(ip("172.20.5.1"), 32).unwrap();
        assert!(host.contains(ip("172.20.5.1")));
        assert!(!host.contains(ip("172.20.5.2")));
        let all = Network::new(ip("1.2.3.4"), 0).unwrap();
        assert_eq!(all.to_string(), "0.0.0.0/0");
        assert!(all.contains(ip("255.255.255.255")));
        assert!(Network::new(ip("1.2.3.4"), 33).is_none());
    }

    #[test]
    fn v6_prefixes_contain_only_their_range() {
        let n = Network::new(ip("fd12:3456:789a:1::1"), 64).unwrap();
        assert_eq!(n.to_string(), "fd12:3456:789a:1::/64");
        assert!(n.contains(ip("fd12:3456:789a:1:ffff:ffff:ffff:ffff")));
        assert!(!n.contains(ip("fd12:3456:789a:2::1")));
        assert!(!n.contains(ip("172.20.0.1")));
        let host = Network::new(ip("fe80::1"), 128).unwrap();
        assert!(host.contains(ip("fe80::1")) && !host.contains(ip("fe80::2")));
        assert!(Network::new(ip("::"), 0)
            .unwrap()
            .contains(ip("2001:db8::1")));
        assert!(Network::new(ip("::1"), 129).is_none());
    }

    #[test]
    fn prefixes_parse_from_length_or_mask_and_compare_by_cover() {
        let range = Network::parse("172.16.0.0/12").unwrap();
        assert_eq!(Network::parse("172.16.0.0/255.240.0.0"), Some(range));
        assert_eq!(Network::parse(" 172.16.0.0/12 "), Some(range));
        assert!(
            Network::parse("172.16.0.0/255.0.255.0").is_none(),
            "a mask with holes"
        );
        assert!(Network::parse("172.16.0.0").is_none());
        assert!(Network::parse("fd00::/ffff::").is_none());
        assert_eq!(Network::parse("fd00::/8").unwrap().prefix_len(), 8);
        let docker = Network::parse("172.29.80.0/20").unwrap();
        assert!(range.covers(&docker));
        assert!(!docker.covers(&range));
        assert!(!range.covers(&Network::parse("10.0.0.0/8").unwrap()));
        assert!(!range.covers(&Network::parse("172.0.0.0/8").unwrap()));
    }

    #[test]
    fn the_accept_decision_serves_the_gateway_network_only() {
        let n = Network::parse("172.29.80.0/20").unwrap();
        assert!(admit(ip("172.29.80.2"), Some(&n)));
        assert!(admit(ip("172.29.95.254"), Some(&n)));
        assert!(!admit(ip("172.29.96.1"), Some(&n)));
        assert!(!admit(ip("192.168.1.30"), Some(&n)));
        assert!(!admit(ip("127.0.0.1"), Some(&n)));
        // Unknown network: the bind address is the only guard left.
        assert!(admit(ip("192.168.1.30"), None));
    }

    #[test]
    fn a_guard_says_what_it_serves_and_how_many_it_refused() {
        let mut g = PeerGuard {
            engine: "docker".into(),
            gateway: ip("172.29.80.1"),
            network: Network::parse("172.29.80.0/20"),
            refused: 0,
        };
        assert_eq!(
            g.summary(),
            "serves only 172.29.80.0/20, the container network"
        );
        g.refused = 1;
        assert!(g
            .summary()
            .ends_with("refused 1 connection from outside the container network"));
        g.refused = 3;
        assert!(g
            .summary()
            .ends_with("refused 3 connections from outside the container network"));
        g.network = None;
        assert!(g.summary().starts_with("serves any peer"));
    }

    #[test]
    fn loopback_is_found_in_the_interface_table() {
        let n = interface_network(ip("127.0.0.1")).unwrap().unwrap();
        assert!(n.contains(ip("127.0.0.1")));
        assert!(n.prefix_len() <= 8, "{n}");
        assert!(interface_network(ip("192.0.2.123")).unwrap().is_none());
    }

    #[test]
    fn gateways_are_listed_once_each_and_never_a_wildcard_or_alias() {
        let mut gw = IndexMap::new();
        gw.insert("docker".to_string(), "172.20.0.1".to_string());
        gw.insert("other".to_string(), "172.20.0.1".to_string());
        gw.insert("podman".to_string(), "host-gateway".to_string());
        gw.insert("wild".to_string(), "0.0.0.0".to_string());
        let relayed: Vec<String> = ["docker", "other", "podman", "wild", "missing"]
            .map(String::from)
            .to_vec();
        assert_eq!(gateway_ips(&relayed, &gw), vec![ip("172.20.0.1")]);
    }
}
