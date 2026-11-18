//! The relay on real sockets: it binds the given gateway address only,
//! refuses a wildcard, forwards bytes both ways, and stops when its set
//! changes.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

use chappa_stack::relay::{start, Relay, Wanted};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// An upstream that answers every line with `echo: <line>`.
fn echo_server() -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    thread::spawn(move || {
        for s in l.incoming().flatten() {
            thread::spawn(move || {
                let mut s = s;
                let mut buf = [0u8; 1024];
                while let Ok(n) = s.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    let mut out = b"echo: ".to_vec();
                    out.extend_from_slice(&buf[..n]);
                    if s.write_all(&out).is_err() {
                        break;
                    }
                }
            });
        }
    });
    addr
}

fn wanted(bind: SocketAddr, target: SocketAddr) -> Wanted {
    Wanted {
        engine: "docker".into(),
        bind,
        target,
    }
}

#[test]
fn the_relay_forwards_bytes_from_its_gateway_address_to_the_target() {
    let target = echo_server();
    let bind: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
    let mut relay = Relay::default();
    relay.reconcile(&[wanted(bind, target)]);
    assert!(relay.is_listening(bind));
    let mut c = TcpStream::connect(bind).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    c.write_all(b"hello").unwrap();
    let mut buf = [0u8; 64];
    let n = c.read(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"echo: hello");
    // A second connection works alongside.
    let mut d = TcpStream::connect(bind).unwrap();
    d.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    d.write_all(b"two").unwrap();
    let n = d.read(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"echo: two");

    // Only the gateway address listens: another loopback address on the
    // same port is not served by the relay.
    #[cfg(windows)]
    {
        let other: SocketAddr = format!("127.0.0.2:{}", bind.port()).parse().unwrap();
        assert!(TcpStream::connect_timeout(&other, Duration::from_millis(500)).is_err());
    }

    // An empty set stops it.
    relay.reconcile(&[]);
    assert!(!relay.is_listening(bind));
    thread::sleep(Duration::from_millis(300));
    assert!(TcpListener::bind(bind).is_ok(), "the port is free again");
}

#[test]
fn a_wildcard_address_is_never_bound_and_a_busy_port_is_reported() {
    let target = echo_server();
    for wild in ["0.0.0.0:0", "[::]:0"] {
        assert!(
            start(wanted(wild.parse().unwrap(), target)).is_err(),
            "{wild}"
        );
    }
    // A port someone else holds: the bind fails, the state says so, and
    // the next reconcile tries again.
    let holder = TcpListener::bind("127.0.0.1:0").unwrap();
    let busy = holder.local_addr().unwrap();
    let mut relay = Relay::default();
    relay.reconcile(&[wanted(busy, target)]);
    assert!(!relay.is_listening(busy));
    let state = relay.state(Default::default());
    assert_eq!(state.problems.len(), 1);
    assert!(
        state.problems[0].text.contains("cannot listen on"),
        "{state:?}"
    );
    drop(holder);
    relay.reconcile(&[wanted(busy, target)]);
    assert!(relay.is_listening(busy));
    assert!(relay.state(Default::default()).problems.is_empty());
}

#[test]
fn a_peer_outside_the_gateway_network_is_closed_and_counted() {
    use chappa_stack::relay::{start_serving, Network};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    let target = echo_server();
    let bind: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
    // The listener serves a network that does not hold loopback, so this
    // test's own connection is a peer from outside.
    let refused = Arc::new(AtomicU64::new(0));
    let other = Network::parse("172.29.80.0/20");
    let l = start_serving(wanted(bind, target), other, refused.clone()).unwrap();
    assert_eq!(l.network(), other);
    let mut c = TcpStream::connect(bind).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let _ = c.write_all(b"hello");
    let mut buf = [0u8; 64];
    let n = c.read(&mut buf).unwrap_or(0);
    assert_eq!(n, 0, "nothing is forwarded to a refused peer");
    assert_eq!(refused.load(Ordering::SeqCst), 1);
    drop(l);

    // The relay looks the network up itself when it binds: loopback's own
    // network holds this test, so it is served, and nothing is refused.
    let mut relay = Relay::default();
    relay.reconcile(&[wanted(bind, target)]);
    let mut c = TcpStream::connect(bind).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    c.write_all(b"in").unwrap();
    let n = c.read(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"echo: in");
    let state = relay.state(Default::default());
    assert_eq!(state.guards.len(), 1, "{state:?}");
    let net = state.guards[0]
        .network
        .expect("loopback's network is known");
    assert!(net.contains("127.0.0.1".parse().unwrap()));
    assert_eq!(relay.refused("docker"), 0);
    assert!(state.problems.is_empty(), "{state:?}");
}
