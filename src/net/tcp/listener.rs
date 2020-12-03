use std::io;
use std::future::Future;
use std::net::{ToSocketAddrs, SocketAddr};
use std::os::unix::io::{RawFd};
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_core::{ready, Stream};
use iou::sqe::SockAddrStorage;
use nix::sys::socket::{self as nix_socket, SockProtocol, SockFlag};

use crate::drive::{Drive, demo::DemoDriver};
use crate::ring::{Cancellation, Ring};

use super::TcpStream;

pub struct TcpListener<D: Drive = DemoDriver> {
    ring: Ring<D>,
    fd: RawFd,
    active: Op,
    addr: Option<Box<iou::sqe::SockAddrStorage>>,
}

#[derive(Eq, PartialEq, Copy, Clone, Debug)]
enum Op {
    Nothing = 0,
    Accept,
    Close,
    Closed,
}

impl TcpListener {
    pub fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<TcpListener> {
        TcpListener::bind_on_driver(addr, DemoDriver::default())
    }
}

impl<D: Drive> TcpListener<D> {
    pub fn bind_on_driver<A: ToSocketAddrs>(addr: A, driver: D) -> io::Result<TcpListener<D>> {
        let (fd, addr) = super::socket(addr, SockProtocol::Tcp)?;
        let addr = iou::sqe::SockAddr::Inet(nix_socket::InetAddr::from_std(&addr));
        nix_socket::setsockopt(fd, nix_socket::sockopt::ReuseAddr, &true)
            .map_err(|e| e.as_errno().unwrap_or(nix::errno::Errno::EIO))?;
        nix_socket::bind(fd, &addr).map_err(|e| e.as_errno().unwrap_or(nix::errno::Errno::EIO))?;
        nix_socket::listen(fd, 128).map_err(|e| e.as_errno().unwrap_or(nix::errno::Errno::EIO))?;
        let ring = Ring::new(driver);
        Ok(TcpListener {
            active: Op::Nothing,
            addr: None,
            fd, ring,
        })
    }

    pub fn close(&mut self) -> Close<D> where D: Unpin {
        Pin::new(self).close_pinned()
    }

    pub fn close_pinned(self: Pin<&mut Self>) -> Close<D> {
        Close { socket: self }
    }

    fn guard_op(self: Pin<&mut Self>, op: Op) {
        let (ring, addr, active) = self.split();
        if *active == Op::Closed {
            panic!("Attempted to perform IO on a closed TcpListener");
        } else if *active != Op::Nothing && *active != op {
            ring.cancel_pinned(Cancellation::from(addr.take()));
        }
        *active = op;
    }

    fn cancel(&mut self) {
        let cancellation = match self.active {
            Op::Accept  => Cancellation::from(self.addr.take()),
            Op::Close   => Cancellation::from(()),
            Op::Closed  => return,
            Op::Nothing => return,
        };
        self.active = Op::Nothing;
        self.ring.cancel(cancellation);
    }

    fn drop_addr(self: Pin<&mut Self>) {
        self.split().1.take();
    }

    fn ring(self: Pin<&mut Self>) -> Pin<&mut Ring<D>> {
        self.split().0
    }

    fn split(self: Pin<&mut Self>) 
        -> (Pin<&mut Ring<D>>, &mut Option<Box<SockAddrStorage>>, &mut Op)
    {
        unsafe {
            let this = Pin::get_unchecked_mut(self);
            (Pin::new_unchecked(&mut this.ring), &mut this.addr, &mut this.active)
        }
    }

    fn split_with_addr(self: Pin<&mut Self>)
        -> (Pin<&mut Ring<D>>, &mut SockAddrStorage, &mut Op)
    {
        let (ring, addr, active) = self.split();
        if addr.is_none() {
            *addr = Some(Box::new(SockAddrStorage::uninit()));
        }
        (ring, &mut **addr.as_mut().unwrap(), active)
    }

    fn confirm_close(self: Pin<&mut Self>) {
        *self.split().2 = Op::Closed;
    }
}

impl<D: Drive + Clone> TcpListener<D> {
    pub fn accept(&mut self) -> Accept<'_, D> where D: Unpin {
        Pin::new(self).accept_pinned()
    }

    pub fn accept_pinned(self: Pin<&mut Self>) -> Accept<'_, D> {
        Accept { socket: self }
    }

    pub fn incoming(&mut self) -> Incoming<'_, D> where D: Unpin {
        Pin::new(self).incoming_pinned()
    }

    pub fn incoming_pinned(self: Pin<&mut Self>) -> Incoming<'_, D> {
        Incoming { accept: self.accept_pinned() }
    }

    pub fn accept_no_addr(&mut self) -> AcceptNoAddr<'_, D> where D: Unpin {
        Pin::new(self).accept_no_addr_pinned()
    }

    pub fn accept_no_addr_pinned(self: Pin<&mut Self>) -> AcceptNoAddr<'_, D> {
        AcceptNoAddr { socket: self }
    }

    pub fn incoming_no_addr(&mut self) -> IncomingNoAddr<'_, D> where D: Unpin {
        Pin::new(self).incoming_no_addr_pinned()
    }

    pub fn incoming_no_addr_pinned(self: Pin<&mut Self>) -> IncomingNoAddr<'_, D> {
        IncomingNoAddr { accept: self.accept_no_addr_pinned() }
    }

    pub fn poll_accept(mut self: Pin<&mut Self>, ctx: &mut Context<'_>)
        -> Poll<io::Result<(TcpStream<D>, SocketAddr)>>
    {
        self.as_mut().guard_op(Op::Accept);
        let fd = self.fd;
        let (ring, addr, ..) = self.as_mut().split_with_addr();
        let fd = ready!(ring.poll(ctx, 1, |sqs| {
            let mut sqe = sqs.single().unwrap();
            unsafe {
                sqe.prep_accept(fd, Some(addr), SockFlag::empty());
            }
            sqe
        }))? as RawFd;
        let addr = {
            let result = unsafe { addr.as_socket_addr() };
            self.as_mut().drop_addr();
            match result? {
                iou::sqe::SockAddr::Inet(addr) => addr.to_std(),
                addr => panic!("TcpListener addr cannot be {:?}", addr.family()),
            }
        };

        Poll::Ready(Ok((TcpStream::from_fd(fd, self.ring().clone()), addr)))
    }

    pub fn poll_accept_no_addr(mut self: Pin<&mut Self>, ctx: &mut Context<'_>)
        -> Poll<io::Result<TcpStream<D>>>
    {
        self.as_mut().guard_op(Op::Accept);
        let fd = self.fd;
        let fd = ready!(self.as_mut().ring().poll(ctx, 1, |sqs| {
            let mut sqe = sqs.single().unwrap();
            unsafe {
                sqe.prep_accept(fd, None, SockFlag::empty());
            }
            sqe
        }))? as RawFd;
        Poll::Ready(Ok(TcpStream::from_fd(fd, self.ring().clone())))
    }
}

impl<D: Drive> Drop for TcpListener<D> {
    fn drop(&mut self) {
        match self.active {
            Op::Closed  => { }
            Op::Nothing => unsafe { libc::close(self.fd); }
            _           => self.cancel(),
        }
    }
}

pub struct Accept<'a, D: Drive> {
    socket: Pin<&'a mut TcpListener<D>>,
}

impl<'a, D: Drive + Clone> Future for Accept<'a, D> {
    type Output = io::Result<(TcpStream<D>, SocketAddr)>;

    fn poll(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Self::Output> {
        self.socket.as_mut().poll_accept(ctx)
    }
}

pub struct AcceptNoAddr<'a, D: Drive> {
    socket: Pin<&'a mut TcpListener<D>>,
}

impl<'a, D: Drive + Clone> Future for AcceptNoAddr<'a, D> {
    type Output = io::Result<TcpStream<D>>;

    fn poll(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Self::Output> {
        self.socket.as_mut().poll_accept_no_addr(ctx)
    }
}

pub struct Incoming<'a, D: Drive> {
    accept: Accept<'a, D>,
}

impl<'a, D: Drive> Incoming<'a, D> {
    fn inner(self: Pin<&mut Self>) -> Pin<&mut Accept<'a, D>> {
        unsafe { Pin::map_unchecked_mut(self, |this| &mut this.accept) }
    }
}

impl<'a, D: Drive + Clone> Stream for Incoming<'a, D> {
    type Item = io::Result<(TcpStream<D>, SocketAddr)>;

    fn poll_next(self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let next = ready!(self.inner().poll(ctx));
        Poll::Ready(Some(next))
    }
}

pub struct IncomingNoAddr<'a, D: Drive> {
    accept: AcceptNoAddr<'a, D>,
}

impl<'a, D: Drive> IncomingNoAddr<'a, D> {
    fn inner(self: Pin<&mut Self>) -> Pin<&mut AcceptNoAddr<'a, D>> {
        unsafe { Pin::map_unchecked_mut(self, |this| &mut this.accept) }
    }
}

impl<'a, D: Drive + Clone> Stream for IncomingNoAddr<'a, D> {
    type Item = io::Result<TcpStream<D>>;

    fn poll_next(self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let next = ready!(self.inner().poll(ctx));
        Poll::Ready(Some(next))
    }
}

pub struct Close<'a, D: Drive> {
    socket: Pin<&'a mut TcpListener<D>>,
}

impl<'a, D: Drive> Future for Close<'a, D> {
    type Output = io::Result<()>;

    fn poll(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.socket.as_mut().guard_op(Op::Close);
        let fd = self.socket.fd;
        ready!(self.socket.as_mut().ring().poll(ctx, 1, |sqs| {
            let mut sqe = sqs.single().unwrap();
            unsafe {
                sqe.prep_close(fd);
            }
            sqe
        }))?;
        self.socket.as_mut().confirm_close();
        Poll::Ready(Ok(()))
    }
}
