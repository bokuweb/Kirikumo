//! Forwarding a local port to a port on a pod.
//!
//! What `kubectl port-forward` does, split into the two halves that are
//! separately testable: the **framing** of the apiserver's channel protocol,
//! and the **pump** that moves bytes between a local socket and a tunnel.
//! The tunnel itself — a WebSocket to the apiserver — is the one part that
//! needs a cluster, and it lives behind [`Tunnel`] so a fake can stand in
//! for it here and in the scripted cluster.
//!
//! One tunnel per local connection. The WebSocket protocol the apiserver
//! speaks has no way to open a second stream on a connection that is
//! already up, so each connection a local program makes becomes its own
//! WebSocket — which is also what keeps one stuck connection from stalling
//! the rest.
//!
//! Everything here is blocking and runs on threads of its own, like a watch
//! (`AGENTS.md` rule 3): one to accept, one per connection.

use crate::error::{Error, Result};
use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

/// The subprotocol the apiserver speaks for port-forwarding.
///
/// Binary frames whose first byte names a channel. Two channels per
/// forwarded port: an even one for data and the odd one after it for
/// errors. This app forwards one port per tunnel, so the channels are
/// [`DATA`] and [`ERROR`].
pub const PROTOCOL: &str = "v4.channel.k8s.io";

/// The channel a forwarded port's bytes travel on.
pub const DATA: u8 = 0;

/// The channel the apiserver reports a forwarded port's failure on.
pub const ERROR: u8 = 1;

/// How long a tunnel waits for the pod to say something before reporting
/// that it did not.
///
/// The pump alternates between the two directions on one thread, so this
/// is the most the local side can be kept waiting while the pod is silent.
pub const POLL: Duration = Duration::from_millis(20);

/// Frame bytes for a channel.
pub fn frame(channel: u8, data: &[u8]) -> Vec<u8> {
    let mut framed = Vec::with_capacity(data.len() + 1);
    framed.push(channel);
    framed.extend_from_slice(data);
    framed
}

/// Read a frame: which channel, and what it carried.
///
/// `None` for an empty message, which the protocol does not send and which
/// would otherwise be read as channel zero with nothing on it.
pub fn parse_frame(message: &[u8]) -> Option<(u8, &[u8])> {
    let (channel, rest) = message.split_first()?;
    Some((*channel, rest))
}

/// The port number the first message on each channel opens with.
///
/// The apiserver's way of saying which port a channel is for: two bytes,
/// little-endian, and then the data. It arrives once per channel, and a
/// reader that did not strip it would hand the local program two bytes of
/// port number in front of its first packet.
pub fn opening_port(payload: &[u8]) -> Option<(u16, &[u8])> {
    let (port, rest) = payload.split_at_checked(2)?;
    Some((u16::from_le_bytes([port[0], port[1]]), rest))
}

/// What a poll of the tunnel found.
#[derive(Debug, PartialEq, Eq)]
pub enum Poll {
    /// Bytes from the pod.
    Data(Vec<u8>),
    /// Nothing yet.
    Nothing,
    /// The pod's side is closed.
    Closed,
}

/// One connection to one port on one pod.
///
/// Blocking, but only briefly: [`Self::poll`] waits at most [`POLL`], so the
/// pump can turn around and serve the other direction.
pub trait Tunnel: Send {
    /// Whatever the pod has sent, if anything arrived within [`POLL`].
    fn poll(&mut self) -> Result<Poll>;

    /// Send bytes to the pod.
    fn send(&mut self, data: &[u8]) -> Result<()>;

    /// Tell the far side the terminal is `cols` by `rows` now.
    ///
    /// Only an attached shell has anything to hear it; a port has no size,
    /// so the default does nothing.
    fn resize(&mut self, _cols: u16, _rows: u16) -> Result<()> {
        Ok(())
    }

    /// Close the pod's side.
    fn close(&mut self);
}

/// A tunnel to nowhere: what goes in comes back.
///
/// The scripted cluster's port-forward, and the pump's test double. A
/// forwarded port on the demo cluster is an echo server on `localhost`,
/// which is enough to see the mechanism work with nothing behind it.
#[derive(Debug, Default)]
pub struct Echo {
    queue: VecDeque<u8>,
    closed: bool,
}

impl Echo {
    /// A fresh echo.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Tunnel for Echo {
    fn poll(&mut self) -> Result<Poll> {
        if self.closed {
            return Ok(Poll::Closed);
        }
        match self.queue.is_empty() {
            true => Ok(Poll::Nothing),
            false => Ok(Poll::Data(self.queue.drain(..).collect())),
        }
    }

    fn send(&mut self, data: &[u8]) -> Result<()> {
        self.queue.extend(data);
        Ok(())
    }

    fn close(&mut self) {
        self.closed = true;
    }
}

/// Move bytes between a local connection and a tunnel until either side
/// closes or `stop` is set.
///
/// One thread, both directions, taking turns: the local socket is read
/// without blocking and the tunnel is polled with a short wait, so neither
/// side can starve the other. A pod that says nothing costs [`POLL`] per
/// turn and nothing else.
pub fn pump(mut local: TcpStream, mut tunnel: Box<dyn Tunnel>, stop: &AtomicBool) -> Result<()> {
    local.set_nonblocking(true)?;
    let mut buffer = [0u8; 16 * 1024];
    let outcome = loop {
        if stop.load(Ordering::Relaxed) {
            break Ok(());
        }
        let mut moved = false;
        match tunnel.poll() {
            Ok(Poll::Data(data)) => {
                write_all(&mut local, &data, stop)?;
                moved = true;
            }
            Ok(Poll::Nothing) => {}
            Ok(Poll::Closed) => break Ok(()),
            Err(error) => break Err(error),
        }
        match local.read(&mut buffer) {
            // The local program hung up.
            Ok(0) => break Ok(()),
            Ok(count) => {
                tunnel.send(&buffer[..count])?;
                moved = true;
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => break Err(Error::Transport(error.to_string())),
        }
        if !moved {
            // Neither side had anything. A tunnel that polls with a real
            // wait spends that here; an echo would otherwise spin.
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    tunnel.close();
    outcome
}

/// Write to a non-blocking socket, waiting out a full buffer.
fn write_all(local: &mut TcpStream, mut data: &[u8], stop: &AtomicBool) -> Result<()> {
    while !data.is_empty() {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        match local.write(data) {
            Ok(0) => return Err(Error::Transport("the local side closed".into())),
            Ok(written) => data = &data[written..],
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(Error::Transport(error.to_string())),
        }
    }
    Ok(())
}

/// A local port being forwarded.
///
/// Owns the listening thread; dropping it stops the listener and tells every
/// connection's pump to end at its next turn.
#[derive(Debug)]
pub struct Forwarder {
    local_port: u16,
    stop: Arc<AtomicBool>,
    open: Arc<AtomicUsize>,
    served: Arc<AtomicUsize>,
    error: Arc<std::sync::Mutex<Option<String>>>,
}

impl Forwarder {
    /// Listen on `local_port` — or on any free port, when it is zero — and
    /// open a tunnel for every connection that arrives.
    ///
    /// `connect` is called on the connection's own thread, once per
    /// connection: it is where the WebSocket to the apiserver is opened, so
    /// a cluster that refuses is reported per connection rather than up
    /// front, and a local program sees its connection reset the way it
    /// would if the pod were down.
    pub fn serve<F>(local_port: u16, connect: F) -> Result<Self>
    where
        F: Fn() -> Result<Box<dyn Tunnel>> + Send + Sync + 'static,
    {
        // Loopback only. A forwarded port is a hole into a cluster, and the
        // machine's other interfaces are not who asked for it.
        let listener = TcpListener::bind(("127.0.0.1", local_port))
            .map_err(|error| Error::Transport(format!("could not listen: {error}")))?;
        let local_port = listener.local_addr()?.port();
        listener.set_nonblocking(true)?;

        let stop = Arc::new(AtomicBool::new(false));
        let open = Arc::new(AtomicUsize::new(0));
        let served = Arc::new(AtomicUsize::new(0));
        let error = Arc::new(std::sync::Mutex::new(None));
        let connect = Arc::new(connect);
        let accepting = Accepting {
            listener,
            stop: stop.clone(),
            open: open.clone(),
            served: served.clone(),
            error: error.clone(),
            connect,
        };
        std::thread::Builder::new()
            .name(format!("kirikumo-forward-{local_port}"))
            .spawn(move || accepting.run())
            .map_err(|error| Error::Transport(error.to_string()))?;
        Ok(Self {
            local_port,
            stop,
            open,
            served,
            error,
        })
    }

    /// The port on `localhost` this is listening on.
    pub fn local_port(&self) -> u16 {
        self.local_port
    }

    /// How many connections are open right now.
    pub fn open_connections(&self) -> usize {
        self.open.load(Ordering::Relaxed)
    }

    /// How many connections have been served since it started.
    pub fn served(&self) -> usize {
        self.served.load(Ordering::Relaxed)
    }

    /// What went wrong with the last connection, if something did.
    pub fn last_error(&self) -> Option<String> {
        self.error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Stop listening and end every connection at its next turn.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for Forwarder {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The accept loop's state, on its own thread.
struct Accepting {
    listener: TcpListener,
    stop: Arc<AtomicBool>,
    open: Arc<AtomicUsize>,
    served: Arc<AtomicUsize>,
    error: Arc<std::sync::Mutex<Option<String>>>,
    connect: Arc<dyn Fn() -> Result<Box<dyn Tunnel>> + Send + Sync>,
}

impl Accepting {
    fn run(self) {
        while !self.stop.load(Ordering::Relaxed) {
            match self.listener.accept() {
                Ok((local, _)) => self.spawn(local),
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(30));
                }
                Err(error) => {
                    tracing::warn!(%error, "the forward's listener failed");
                    break;
                }
            }
        }
        // Dropping the listener here is what frees the port.
    }

    /// One connection, on a thread of its own.
    fn spawn(&self, local: TcpStream) {
        let stop = self.stop.clone();
        let open = self.open.clone();
        let error = self.error.clone();
        let connect = self.connect.clone();
        self.open.fetch_add(1, Ordering::Relaxed);
        self.served.fetch_add(1, Ordering::Relaxed);
        let spawned = std::thread::Builder::new()
            .name("kirikumo-forward-conn".into())
            .spawn(move || {
                let outcome = connect().and_then(|tunnel| pump(local, tunnel, &stop));
                if let Err(failure) = outcome {
                    tracing::warn!(%failure, "a forwarded connection failed");
                    *error
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                        Some(failure.to_string());
                }
                open.fetch_sub(1, Ordering::Relaxed);
            });
        if spawned.is_err() {
            self.open.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_is_a_channel_byte_and_then_the_bytes() {
        assert_eq!(frame(DATA, b"hi"), vec![0, b'h', b'i']);
        assert_eq!(parse_frame(&[1, b'x']), Some((ERROR, &b"x"[..])));
        assert_eq!(parse_frame(&[]), None);
    }

    #[test]
    fn the_first_message_on_a_channel_opens_with_the_port() {
        // 8080 little-endian, then a byte of data.
        let (port, rest) = opening_port(&[0x90, 0x1f, b'!']).unwrap();
        assert_eq!(port, 8080);
        assert_eq!(rest, b"!");
        assert!(opening_port(&[0x90]).is_none());
    }

    #[test]
    fn an_echo_gives_back_what_it_was_given_and_then_nothing() {
        let mut echo = Echo::new();
        assert_eq!(echo.poll().unwrap(), Poll::Nothing);
        echo.send(b"abc").unwrap();
        assert_eq!(echo.poll().unwrap(), Poll::Data(b"abc".to_vec()));
        assert_eq!(echo.poll().unwrap(), Poll::Nothing);
        echo.close();
        assert_eq!(echo.poll().unwrap(), Poll::Closed);
    }

    fn read_exactly(stream: &mut TcpStream, count: usize) -> Vec<u8> {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut got = vec![0u8; count];
        stream.read_exact(&mut got).unwrap();
        got
    }

    #[test]
    fn a_forwarded_port_carries_bytes_both_ways() {
        let forwarder =
            Forwarder::serve(0, || Ok(Box::new(Echo::new()) as Box<dyn Tunnel>)).unwrap();
        let mut client = TcpStream::connect(("127.0.0.1", forwarder.local_port())).unwrap();
        client.write_all(b"hello, pod").unwrap();
        assert_eq!(read_exactly(&mut client, 10), b"hello, pod");
        // A second round on the same connection.
        client.write_all(b"again").unwrap();
        assert_eq!(read_exactly(&mut client, 5), b"again");
        assert_eq!(forwarder.served(), 1);
        assert!(forwarder.last_error().is_none());
    }

    #[test]
    fn each_connection_gets_a_tunnel_of_its_own() {
        let forwarder =
            Forwarder::serve(0, || Ok(Box::new(Echo::new()) as Box<dyn Tunnel>)).unwrap();
        let mut one = TcpStream::connect(("127.0.0.1", forwarder.local_port())).unwrap();
        let mut two = TcpStream::connect(("127.0.0.1", forwarder.local_port())).unwrap();
        one.write_all(b"1").unwrap();
        two.write_all(b"2").unwrap();
        // Each hears only its own echo.
        assert_eq!(read_exactly(&mut one, 1), b"1");
        assert_eq!(read_exactly(&mut two, 1), b"2");
        // Both connections were counted.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while forwarder.served() < 2 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(forwarder.served(), 2);
    }

    #[test]
    fn a_tunnel_that_cannot_be_opened_resets_the_connection_and_is_reported() {
        let forwarder = Forwarder::serve(0, || {
            Err(Error::Forbidden("pods/portforward is forbidden".into()))
        })
        .unwrap();
        let mut client = TcpStream::connect(("127.0.0.1", forwarder.local_port())).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        // The connection ends without data.
        let mut buffer = [0u8; 8];
        let outcome = client.read(&mut buffer);
        assert!(matches!(outcome, Ok(0) | Err(_)), "{outcome:?}");
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while forwarder.last_error().is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            forwarder
                .last_error()
                .is_some_and(|error| error.contains("forbidden"))
        );
    }

    #[test]
    fn stopping_frees_the_port() {
        let forwarder =
            Forwarder::serve(0, || Ok(Box::new(Echo::new()) as Box<dyn Tunnel>)).unwrap();
        let port = forwarder.local_port();
        drop(forwarder);
        // The accept loop notices within one of its sleeps and lets go of
        // the socket; binding the same port again is the proof.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut rebound = false;
        while std::time::Instant::now() < deadline {
            if TcpListener::bind(("127.0.0.1", port)).is_ok() {
                rebound = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            rebound,
            "the port should be free after the forwarder is dropped"
        );
    }

    #[test]
    fn a_port_that_is_taken_is_refused_with_a_reason() {
        let holder = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = holder.local_addr().unwrap().port();
        let refused = Forwarder::serve(port, || Ok(Box::new(Echo::new()) as Box<dyn Tunnel>));
        assert!(matches!(refused, Err(Error::Transport(_))));
    }
}
