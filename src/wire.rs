use std::fmt::Debug;
use std::io::Write;
use std::net::TcpListener;
use std::os::unix::io::{AsRawFd, RawFd};
use std::{io, net};

use reactor::poller::IoEv;
use reactor::Resource;

use crate::{NetConnection, NetListener, NetSession};

/// Socket read buffer size.
const READ_BUFFER_SIZE: usize = u16::MAX as usize;
/// Maximum time to wait when reading from a socket.
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(6);
/// Maximum time to wait when writing to a socket.
const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

#[derive(Debug)]
pub enum ListenerEvent<S: NetSession> {
    Accepted(S),
    Failure(io::Error),
}

#[derive(Debug)]
pub struct NetAccept<S: NetSession, L: NetListener<Stream = S::Connection> = TcpListener> {
    session_context: S::Context,
    listener: L,
}

impl<L: NetListener<Stream = S::Connection>, S: NetSession> AsRawFd for NetAccept<S, L> {
    fn as_raw_fd(&self) -> RawFd {
        self.listener.as_raw_fd()
    }
}

impl<L: NetListener<Stream = S::Connection>, S: NetSession> io::Write for NetAccept<S, L> {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        panic!("must not write to network listener")
    }

    fn flush(&mut self) -> io::Result<()> {
        panic!("must not write to network listener")
    }
}

impl<L: NetListener<Stream = S::Connection>, S: NetSession> NetAccept<S, L> {
    pub fn bind(addr: impl Into<net::SocketAddr>, session_context: S::Context) -> io::Result<Self> {
        let listener = L::bind(addr)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            session_context,
            listener,
        })
    }

    pub fn local_addr(&self) -> net::SocketAddr {
        self.listener.local_addr()
    }

    fn handle_accept(&mut self) -> io::Result<S> {
        let mut stream = self.listener.accept()?;
        stream.set_read_timeout(Some(READ_TIMEOUT))?;
        stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
        stream.set_nonblocking(true)?;
        Ok(S::accept(stream, &self.session_context))
    }
}

impl<L: NetListener<Stream = S::Connection>, S: NetSession> Resource for NetAccept<S, L> {
    type Id = net::SocketAddr;
    type Event = ListenerEvent<S>;

    fn id(&self) -> Self::Id {
        self.listener.local_addr()
    }

    fn handle_io(&mut self, ev: IoEv) -> Option<Self::Event> {
        if ev.is_writable {
            Some(match self.handle_accept() {
                Err(err) => ListenerEvent::Failure(err),
                Ok(session) => ListenerEvent::Accepted(session),
            })
        } else {
            None
        }
    }

    fn disconnect(self) -> io::Result<()> {
        // We disconnect by dropping the self
        Ok(())
    }
}

pub enum SessionEvent<S: NetSession> {
    Established(S::Id),
    Data(Vec<u8>),
    Terminated(io::Error),
}

#[derive(Clone, Copy, Ord, PartialOrd, Eq, PartialEq, Hash, Debug)]
pub enum TransportState {
    Handshake,
    Active,
    Terminated,
}

pub struct NetTransport<S: NetSession> {
    state: TransportState,
    session: S,
    inbound: bool,
}

impl<S: NetSession> AsRawFd for NetTransport<S> {
    fn as_raw_fd(&self) -> RawFd {
        self.session.as_raw_fd()
    }
}

impl<S: NetSession> NetTransport<S> {
    fn upgrade(mut session: S, inbound: bool) -> io::Result<Self> {
        session.set_read_timeout(Some(READ_TIMEOUT))?;
        session.set_write_timeout(Some(WRITE_TIMEOUT))?;
        session.set_nonblocking(true)?;
        Ok(Self {
            state: TransportState::Handshake,
            session,
            inbound,
        })
    }

    pub fn accept(session: S) -> io::Result<Self> {
        Self::upgrade(session, true)
    }

    pub fn connect(addr: S::PeerAddr, context: &S::Context) -> io::Result<Self> {
        let session = S::connect(addr, context)?;
        let mut me = Self::upgrade(session, true)?;
        me.inbound = false;
        Ok(me)
    }

    pub fn is_inbound(&self) -> bool {
        self.inbound
    }

    pub fn is_outbound(&self) -> bool {
        !self.is_inbound()
    }

    pub fn state(&self) -> TransportState {
        self.state
    }

    pub fn local_addr(&self) -> <S::Connection as NetConnection>::Addr {
        self.session.local_addr()
    }

    pub fn expect_peer_id(&self) -> S::Id {
        self.session.expect_id()
    }

    fn handle_writable(&mut self) -> Option<SessionEvent<S>> {
        debug_assert_ne!(
            self.state,
            TransportState::Terminated,
            "read on terminated transport"
        );
        match self.session.flush() {
            Err(err) => Some(SessionEvent::Terminated(err)),
            Ok(_) => None,
        }
    }

    fn handle_readable(&mut self) -> Option<SessionEvent<S>> {
        debug_assert_ne!(
            self.state,
            TransportState::Terminated,
            "read on terminated transport"
        );

        // We need to save the state before doing the read below
        let was_established = self.state == TransportState::Handshake;
        let mut buffer = Vec::with_capacity(READ_BUFFER_SIZE);
        let res = self.session.read_to_end(&mut buffer);
        match res {
            Ok(0) if !was_established => {
                if self.session.handshake_completed() {
                    self.state = TransportState::Active;
                    return Some(SessionEvent::Established(self.session.expect_id()));
                } else {
                    // Do nothing since we haven't established session yet
                    None
                }
            }
            // Nb. Since `poll`, which this reactor is based on, is *level-triggered*,
            // we will be notified again if there is still data to be read on the socket.
            // Hence, there is no use in putting this socket read in a loop, as the second
            // invocation would likely block.
            Ok(0) => {
                // If we get zero bytes read as a return value, it means the peer has
                // performed an orderly shutdown.
                self.state = TransportState::Terminated;
                Some(SessionEvent::Terminated(io::ErrorKind::Interrupted.into()))
            }
            Ok(len) => {
                debug_assert!(was_established);
                debug_assert_eq!(len, buffer.len());
                Some(SessionEvent::Data(buffer))
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => Some(SessionEvent::Data(buffer)),
            Err(err) => {
                self.state = TransportState::Terminated;
                Some(SessionEvent::Terminated(err))
            }
        }
    }
}

impl<S: NetSession> Resource for NetTransport<S>
where
    S::TransitionAddr: Into<net::SocketAddr>,
{
    type Id = RawFd;
    type Event = SessionEvent<S>;

    fn id(&self) -> Self::Id {
        self.session.as_raw_fd()
    }

    fn handle_io(&mut self, ev: IoEv) -> Option<Self::Event> {
        if ev.is_writable {
            self.handle_writable()
        } else if ev.is_readable {
            self.handle_readable()
        } else {
            unreachable!()
        }
    }

    fn disconnect(self) -> io::Result<()> {
        self.session.disconnect()
    }
}

impl<S: NetSession> Write for NetTransport<S>
where
    S::TransitionAddr: Into<net::SocketAddr>,
{
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.session.write(&buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.session.flush()
    }
}
