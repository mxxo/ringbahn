use std::io;
use std::future::Future;
use std::net::ToSocketAddrs;
use std::os::unix::io::RawFd;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_core::ready;
use futures_io::{AsyncRead, AsyncBufRead, AsyncWrite};
use iou::sqe::SockAddr;
use nix::sys::socket::SockProtocol;

use crate::buf::Buffer;
use crate::drive::{Drive, demo::DemoDriver};
use crate::ring::Ring;
use crate::event;
use crate::Submission;

use super::socket;

pub struct TcpStream<D: Drive = DemoDriver> {
    ring: Ring<D>,
    buf: Buffer,
    active: Op,
    fd: RawFd,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Op {
    Read,
    Write,
    Close,
    Nothing,
    Closed,
}

impl TcpStream {
    pub fn connect<A: ToSocketAddrs>(addr: A) -> Connect {
        TcpStream::connect_on_driver(addr, DemoDriver::default())
    }
}

impl<D: Drive + Clone> TcpStream<D> {
    pub fn connect_on_driver<A: ToSocketAddrs>(addr: A, driver: D) -> Connect<D> {
        let (fd, addr) = match socket(addr, SockProtocol::Tcp) {
            Ok(fd)  => fd,
            Err(e)  => return Connect(Err(Some(e))),
        };
        let addr = Box::new(SockAddr::Inet(nix::sys::socket::InetAddr::from_std(&addr)));
        Connect(Ok(driver.submit(event::Connect { fd, addr })))
    }
}

impl<D: Drive> TcpStream<D> {
    pub(crate) fn from_fd(fd: RawFd, ring: Ring<D>) -> TcpStream<D> {
        TcpStream {
            buf: Buffer::default(),
            active: Op::Nothing,
            fd, ring,
        }
    }

    fn guard_op(self: Pin<&mut Self>, op: Op) {
        let (ring, buf, active) = self.split();
        if *active == Op::Closed {
            panic!("Attempted to perform IO on a closed stream");
        } else if *active != Op::Nothing && *active != op {
            ring.cancel_pinned(buf.cancellation());
        }
        *active = op;
    }

    fn cancel(&mut self) {
        self.active = Op::Nothing;
        self.ring.cancel(self.buf.cancellation());
    }

    #[inline(always)]
    fn ring(self: Pin<&mut Self>) -> Pin<&mut Ring<D>> {
        self.split().0
    }

    #[inline(always)]
    fn buf(self: Pin<&mut Self>) -> &mut Buffer {
        self.split().1
    }

    #[inline(always)]
    fn split(self: Pin<&mut Self>) -> (Pin<&mut Ring<D>>, &mut Buffer, &mut Op) {
        unsafe {
            let this = Pin::get_unchecked_mut(self);
            (Pin::new_unchecked(&mut this.ring), &mut this.buf, &mut this.active)
        }
    }

    fn confirm_close(self: Pin<&mut Self>) {
        *self.split().2 = Op::Closed;
    }
}

pub struct Connect<D: Drive = DemoDriver>(
    Result<Submission<event::Connect, D>, Option<io::Error>>
);

impl<D: Drive + Clone> Future for Connect<D> {
    type Output = io::Result<TcpStream<D>>;

    fn poll(self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project() {
            Ok(mut submission)  => {
                let (connect, result) = ready!(submission.as_mut().poll(ctx));
                result?;
                let driver = submission.driver().clone();
                Poll::Ready(Ok(TcpStream::from_fd(connect.fd, Ring::new(driver))))
            }
            Err(err)        => {
                let err = err.take().expect("polled Connect future after completion");
                Poll::Ready(Err(err))
            }
        }
    }
}

impl<D: Drive> Connect<D> {
    fn project(self: Pin<&mut Self>)
        -> Result<Pin<&mut Submission<event::Connect, D>>, &mut Option<io::Error>>
    {
        unsafe {
            match &mut Pin::get_unchecked_mut(self).0 {
                Ok(submission)  => Ok(Pin::new_unchecked(submission)),
                Err(err)        => Err(err)
            }
        }
    }
}

impl<D: Drive> AsyncRead for TcpStream<D> {
    fn poll_read(mut self: Pin<&mut Self>, ctx: &mut Context<'_>, buf: &mut [u8])
        -> Poll<io::Result<usize>>
    {
        let mut inner = ready!(self.as_mut().poll_fill_buf(ctx))?;
        let len = io::Read::read(&mut inner, buf)?;
        self.consume(len);
        Poll::Ready(Ok(len))
    }
}

impl<D: Drive> AsyncBufRead for TcpStream<D> {
    fn poll_fill_buf(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        self.as_mut().guard_op(Op::Read);
        let fd = self.fd;
        let (ring, buf, ..) = self.split();
        buf.fill_buf(|buf| {
            let n = ready!(ring.poll(ctx, 1, |sqs| { 
                let mut sqe = sqs.single().unwrap();
                unsafe {
                    sqe.prep_read(fd, buf, 0);
                }
                sqe
            }))?;
            Poll::Ready(Ok(n as u32))
        })
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        self.buf().consume(amt);
    }
}

impl<D: Drive> AsyncWrite for TcpStream<D> {
    fn poll_write(mut self: Pin<&mut Self>, ctx: &mut Context<'_>, slice: &[u8]) -> Poll<io::Result<usize>> {
        self.as_mut().guard_op(Op::Write);
        let fd = self.fd;
        let (ring, buf, ..) = self.split();
        let data = ready!(buf.fill_buf(|mut buf| {
            Poll::Ready(Ok(io::Write::write(&mut buf, slice)? as u32))
        }))?;
        let n = ready!(ring.poll(ctx, 1, |sqs| {
            let mut sqe = sqs.single().unwrap();
            unsafe {
                sqe.prep_write(fd, data, 0);
            }
            sqe
        }))?;
        buf.clear();
        Poll::Ready(Ok(n as usize))
    }

    fn poll_flush(self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<io::Result<()>> {
        ready!(self.poll_write(ctx, &[]))?;
        Poll::Ready(Ok(()))
    }

    fn poll_close(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.as_mut().guard_op(Op::Close);
        let fd = self.fd;
        ready!(self.as_mut().ring().poll(ctx, 1, |sqs| {
            let mut sqe = sqs.single().unwrap();
            unsafe {
                sqe.prep_close(fd);
            }
            sqe
        }))?;
        self.confirm_close();
        Poll::Ready(Ok(()))
    }
}

impl<D: Drive> Drop for TcpStream<D> {
    fn drop(&mut self) {
        match self.active {
            Op::Closed  => { }
            Op::Nothing => unsafe { libc::close(self.fd); },
            _           => self.cancel(),
        }
    }
}
