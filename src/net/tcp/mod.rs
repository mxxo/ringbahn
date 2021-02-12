//! TCP bindings for `ringbahn`.

mod listener;
mod stream;

pub use listener::{TcpListener, Accept, AcceptNoAddr, Close, Incoming, IncomingNoAddr};
pub use stream::{TcpStream, Connect};

use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::os::unix::io::RawFd;

use nix::sys::socket as nix_socket;

fn socket<A: ToSocketAddrs>(addr: A, protocol: nix_socket::SockProtocol) -> io::Result<(RawFd, SocketAddr)> {
    use io::{Error, ErrorKind};

    let mut error = Error::new(ErrorKind::InvalidInput, "could not resolve to any addresses");

    for addr in addr.to_socket_addrs()? {
        let domain = match addr.is_ipv6() {
            true    => nix_socket::AddressFamily::Inet6,
            false   => nix_socket::AddressFamily::Inet,
        };

        let flags = nix_socket::SockFlag::SOCK_CLOEXEC;

        match nix_socket::socket(domain, nix_socket::SockType::Stream, flags, Some(protocol)) {
            Ok(fd)          => return Ok((fd, addr)),
            _               => error = io::Error::last_os_error(),
        }
    }

    Err(error)
}
