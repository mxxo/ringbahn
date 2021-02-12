use std::io;
use std::os::unix::io::RawFd;

mod listener;
mod stream;

pub use listener::{UnixListener, Close, Accept, Incoming};
pub use stream::{UnixStream, Connect};

use nix::sys::socket as nix_socket;

fn socket() -> io::Result<RawFd> {
    match nix_socket::socket(nix_socket::AddressFamily::Unix,
        nix_socket::SockType::Stream, nix_socket::SockFlag::SOCK_CLOEXEC, None)
    {
        Ok(fd)  => Ok(fd),
        Err(_)  => Err(io::Error::last_os_error()),
    }
}

fn socketpair() -> io::Result<(RawFd, RawFd)> {
    match nix_socket::socketpair(nix_socket::AddressFamily::Unix,
        nix_socket::SockType::Stream, None, nix_socket::SockFlag::SOCK_CLOEXEC) {
        Ok((fd1, fd2))  => Ok((fd1, fd2)),
        Err(_)          => Err(io::Error::last_os_error()),
    }
}
