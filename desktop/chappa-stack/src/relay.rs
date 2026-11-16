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
//!   Windows the first one raises the Windows Firewall prompt, and container
//!   traffic gets through only once it is allowed (the stack docs say so
//!   where `relay:` is described).
//!
//! Plain std threads: one accept loop per listener (non-blocking, polled,
//! so a stop takes effect within [`ACCEPT_POLL`]), two copy threads per
//! connection.

use indexmap::IndexMap;
use std::collections::BTreeSet;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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

/// Why the relay could not do what it should, for the alerts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayProblem {
    pub engine: String,
    pub text: String,
}

/// What the relay is doing, for the facts section.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RelayState {
    /// `engine → bind → target`, every listener up now.
    pub listening: Vec<Wanted>,
    pub problems: Vec<RelayProblem>,
    /// Engines that need no relay, with the reason.
    pub not_needed: Vec<(String, String)>,
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

/// A started listener; dropping it stops it.
pub struct Listener {
    wanted: Wanted,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
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
}

impl Relay {
    /// Make the running set equal `wanted`: stop what is no longer wanted,
    /// start what is missing (a failed bind is remembered and retried on
    /// the next call).
    pub fn reconcile(&mut self, wanted: &[Wanted]) {
        self.listeners.retain(|l| wanted.contains(&l.wanted));
        self.failures.retain(|w, _| wanted.contains(w));
        for w in wanted {
            if self.listeners.iter().any(|l| &l.wanted == w) {
                continue;
            }
            match start(w.clone()) {
                Ok(l) => {
                    self.failures.shift_remove(w);
                    self.listeners.push(l);
                }
                Err(e) => {
                    self.failures.insert(w.clone(), e.to_string());
                }
            }
        }
    }

    pub fn stop_all(&mut self) {
        self.listeners.clear();
        self.failures.clear();
    }

    /// The state for the view, merged with what [`plan`] found.
    pub fn state(&self, mut planned: RelayState) -> RelayState {
        planned.listening = self.listeners.iter().map(|l| l.wanted.clone()).collect();
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
}

/// Bind and start one listener. Refuses an unspecified address outright,
/// whatever the caller planned.
pub fn start(wanted: Wanted) -> io::Result<Listener> {
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
    let target = wanted.target;
    let flag = stop.clone();
    let handle = thread::spawn(move || accept_loop(listener, target, flag, open));
    Ok(Listener {
        wanted,
        stop,
        handle: Some(handle),
    })
}

fn accept_loop(
    listener: TcpListener,
    target: SocketAddr,
    stop: Arc<AtomicBool>,
    open: Arc<AtomicUsize>,
) {
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((client, _)) => {
                if open.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
                    let _ = client.shutdown(Shutdown::Both);
                    continue;
                }
                let _ = client.set_nonblocking(false);
                open.fetch_add(1, Ordering::SeqCst);
                let open = open.clone();
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
}
