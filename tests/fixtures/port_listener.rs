//! Payload for the `ready: port` end-to-end test in `tests/plugin.rs`.
//!
//! The test compiles this file with `rustc` and ships the binary in a package,
//! because the payload has to bind the declared port itself: a `.sh` / `.cmd`
//! script cannot bind TCP, and the test process must not hold the listener —
//! the daemon's preflight bind would correctly call the port taken.
//!
//! Two modes:
//!
//!   port_listener --listen <addr> <delay-ms> <marker>
//!   port_listener --check  <addr> <events>
//!
//! `--listen` is the service: sleep, bind, write the marker, then accept
//! connections forever. `--check` is the `started` action: it succeeds only
//! when the port already accepts, so a start that runs it before Ready fails,
//! and a successful start proves the action ran after the port was live.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["--listen", addr, delay, marker] => listen(addr, delay, marker),
        ["--check", addr, events] => check(addr, events),
        _ => {
            eprintln!(
                "usage: port_listener --listen <addr> <delay-ms> <marker>\n       \
                 port_listener --check <addr> <events>"
            );
            std::process::exit(2);
        }
    }
}

fn address(addr: &str) -> SocketAddr {
    addr.parse().unwrap_or_else(|error| {
        eprintln!("not a socket address: {addr}: {error}");
        std::process::exit(2);
    })
}

fn listen(addr: &str, delay: &str, marker: &str) -> ! {
    let addr = address(addr);
    let delay: u64 = delay.parse().unwrap_or_else(|error| {
        eprintln!("not a delay in milliseconds: {delay}: {error}");
        std::process::exit(2);
    });
    // The test asserts the start waited: the port only opens after this.
    std::thread::sleep(Duration::from_millis(delay));
    let listener = TcpListener::bind(addr).unwrap_or_else(|error| {
        eprintln!("bind {addr}: {error}");
        std::process::exit(1);
    });
    // Written after the bind, so a reader knows this process owns the port.
    let mut file = File::create(marker).unwrap_or_else(|error| {
        eprintln!("marker {marker}: {error}");
        std::process::exit(1);
    });
    let _ = writeln!(file, "listening {addr}");
    // The probe only needs a completed connect; drop whatever it sends.
    for stream in listener.incoming() {
        drop(stream);
    }
    unreachable!("incoming() never ends")
}

fn check(addr: &str, events: &str) {
    let addr = address(addr);
    // Deliberately short: Windows can hold a connect to a loopback port that
    // nothing listens on for two seconds. A long timeout would let a start
    // that trusted the spawn (instead of the probe) succeed anyway — the
    // connect would simply wait for the payload to open the port.
    match TcpStream::connect_timeout(&addr, Duration::from_millis(500)) {
        Ok(_) => {
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(events)
                .unwrap_or_else(|error| {
                    eprintln!("events {events}: {error}");
                    std::process::exit(1);
                });
            let _ = writeln!(file, "started-after-listening");
        }
        Err(error) => {
            eprintln!("{addr} does not accept yet: {error}");
            std::process::exit(3);
        }
    }
}
