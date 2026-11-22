//! The loop's one-byte wake pipe.
//!
//! The server loop blocks in `poll`/`WSAPoll`; producers (a blocking-pool
//! completion, a UI event, a shutdown) need to end that wait immediately
//! instead of at the next 100 ms tick. Each producer writes one byte to this
//! pipe and the loop drains it in `on_tick`.
//!
//! Unix uses a `UnixStream` pair. Windows has no `socketpair`, so the pair is
//! two connected loopback TCP sockets; both ends are non-blocking, so a full
//! pipe is "a wake is already pending", never a blocked producer.

use std::io::{self, Read, Write};
use std::sync::Arc;

#[cfg(windows)]
use std::net::{Ipv4Addr, TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::net::UnixStream;

use super::poll::{self, Key};

#[cfg(unix)]
type Endpoint = UnixStream;
#[cfg(windows)]
type Endpoint = TcpStream;

pub(crate) struct Reader(Endpoint);

pub(crate) struct Writer(Endpoint);

/// Create the pair. Returns the read end and the shared write end.
pub(crate) fn pair() -> io::Result<(Reader, Arc<Writer>)> {
    let (reader, writer) = connect_pair()?;
    Ok((Reader(reader), Arc::new(Writer(writer))))
}

#[cfg(unix)]
fn connect_pair() -> io::Result<(Endpoint, Endpoint)> {
    let (reader, writer) = UnixStream::pair()?;
    reader.set_nonblocking(true)?;
    writer.set_nonblocking(true)?;
    Ok((reader, writer))
}

#[cfg(windows)]
fn connect_pair() -> io::Result<(Endpoint, Endpoint)> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let address = listener.local_addr()?;
    let writer = TcpStream::connect(address)?;
    let (reader, _peer) = listener.accept()?;
    drop(listener);
    let _ = writer.set_nodelay(true);
    let _ = reader.set_nodelay(true);
    writer.set_nonblocking(true)?;
    reader.set_nonblocking(true)?;
    Ok((reader, writer))
}

impl Reader {
    /// The wait-set key for this end.
    pub(crate) fn key(&self) -> Key {
        poll::key_of(&self.0)
    }

    /// Drain every pending byte. A non-blocking read that would block means the
    /// pipe is empty and is not an error.
    pub(crate) fn drain(&mut self) {
        let mut buffer = [0u8; 64];
        loop {
            match self.0.read(&mut buffer) {
                Ok(0) => return,
                Ok(_) => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
                Err(_) => return,
            }
        }
    }
}

impl Writer {
    /// Wake the loop. A full pipe already means a wake is pending, and a closed
    /// read end means the loop is gone; neither is worth reporting.
    pub(crate) fn notify(&self) {
        let _ = (&self.0).write(&[1]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wake_wakes_the_reader_and_drains_empty() {
        let (mut reader, writer) = pair().expect("pair");
        writer.notify();
        let mut buffer = [0u8; 8];
        let read = reader.0.read(&mut buffer).expect("read");
        assert!(read > 0, "the wake byte must be readable");
        reader.drain();
        let again = reader.0.read(&mut buffer).expect_err("empty pipe");
        assert_eq!(again.kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn a_shared_writer_wakes_the_reader() {
        let (mut reader, writer) = pair().expect("pair");
        let clone = Arc::clone(&writer);
        clone.notify();
        let mut buffer = [0u8; 8];
        assert!(reader.0.read(&mut buffer).expect("read") > 0);
    }
}
