extern crate bytes;
extern crate futures;
extern crate futures_glib;
extern crate glib_sys;
extern crate libc;
#[macro_use]
extern crate tokio_io;

use std::cell::RefCell;
use std::io::{self, Read, Write};
use std::mem;
use std::os::unix::io::RawFd;
use std::time::Duration;

use bytes::{Buf, BufMut};
use futures::{Async, Poll};
use futures::task::{self, Task};
use futures_glib::{IoChannel, IoCondition, MainContext, Source, SourceFuncs, UnixToken};
use libc::{c_ulong, SOCK_CLOEXEC, SOCK_NONBLOCK, SOCK_STREAM};
use tokio_io::{AsyncRead, AsyncWrite};

pub struct UnixStream {
    inner: Source<Inner>,
}

unsafe impl Send for UnixStream {}

enum State {
    NotReady,
    Ready,
    Blocked(Task),
}

impl State {
    fn block(&mut self) -> bool {
        match *self {
            State::Ready => false,
            State::Blocked(_) |
            State::NotReady => {
                *self = State::Blocked(task::park());
                true
            }
        }
    }

    fn unblock(&mut self) -> Option<Task> {
        match mem::replace(self, State::Ready) {
            State::Ready |
            State::NotReady => None,
            State::Blocked(task) => Some(task),
        }
    }

    fn is_blocked(&self) -> bool {
        match *self {
            State::Blocked(_) => true,
            _ => false,
        }
    }
}

struct Inner {
    channel: IoChannel,
    active: RefCell<IoCondition>,
    token: RefCell<Option<UnixToken>>,
    read: RefCell<State>,
    write: RefCell<State>,
}

fn new_source(fd: RawFd, cx: &MainContext) -> Source<Inner> {
    let mut active = IoCondition::new();
    active.input(true).output(true);

    let channel = IoChannel::unix_new(fd);
    // Wrap the channel itself in a `Source` that we create and manage.
    let src = Source::new(Inner {
        channel,
        active: RefCell::new(active.clone()),
        read: RefCell::new(State::NotReady),
        write: RefCell::new(State::NotReady),
        token: RefCell::new(None),
    });
    src.attach(cx);

    // And finally, add the file descriptor to the source so it knows
    // what we're tracking.
    let t = src.unix_add_fd(fd, &active);
    *src.get_ref().token.borrow_mut() = Some(t);
    src
}

impl UnixStream {
    pub fn pair(cx: &MainContext) -> io::Result<(UnixStream, UnixStream)> {
        unsafe {
            let mut fds = [0, 0];

            // Like above, see if we can set cloexec atomically
            if cfg!(target_os = "linux") {
                let flags = SOCK_STREAM | SOCK_CLOEXEC | SOCK_NONBLOCK;
                match cvt(libc::socketpair(libc::AF_UNIX, flags, 0, fds.as_mut_ptr())) {
                    Ok(_) => {
                        let socket1 = UnixStream {
                            inner: new_source(fds[0], cx),
                        };
                        let socket2 = UnixStream {
                            inner: new_source(fds[1], cx),
                        };
                        return Ok((socket1, socket2))
                    }
                    Err(ref e) if e.raw_os_error() == Some(libc::EINVAL) => {},
                    Err(e) => return Err(e),
                }
            }

            try!(cvt(libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr())));
            let socket1 = UnixStream {
                inner: new_source(fds[0], cx),
            };
            let socket2 = UnixStream {
                inner: new_source(fds[1], cx),
            };
            try!(cvt(libc::ioctl(fds[0], libc::FIOCLEX)));
            try!(cvt(libc::ioctl(fds[1], libc::FIOCLEX)));
            let mut nonblocking = 1 as c_ulong;
            try!(cvt(libc::ioctl(fds[0], libc::FIONBIO, &mut nonblocking)));
            try!(cvt(libc::ioctl(fds[1], libc::FIONBIO, &mut nonblocking)));
            Ok((socket1, socket2))
        }
    }

    /// Blocks the current future's task internally based on the `condition`
    /// specified.
    fn block(&self, condition: &IoCondition) {
        let inner = self.inner.get_ref();
        let mut active = inner.active.borrow_mut();
        if condition.is_input() {
            *inner.read.borrow_mut() = State::Blocked(task::park());
            active.input(true);
        } else {
            *inner.write.borrow_mut() = State::Blocked(task::park());
            active.output(true);
        }

        // Be sure to update the IoCondition that we're interested so we can ge
        // events related to this condition.
        let token = inner.token.borrow();
        let token = token.as_ref().unwrap();
        unsafe {
            self.inner.unix_modify_fd(token, &active);
        }
    }

    /// Test whether this socket is ready to be read or not.
    ///
    /// If the socket is *not* readable then the current task is scheduled to
    /// get a notification when the socket does become readable. That is, this
    /// is only suitable for calling in a `Future::poll` method and will
    /// automatically handle ensuring a retry once the socket is readable again.
    pub fn poll_read(&self) -> Async<()> {
        // FIXME: probably needs to only check read.
        if self.inner.get_ref().check(&self.inner) {
            Async::Ready(())
        }
        else {
            Async::NotReady
        }
    }

    /// Tests to see if this source is ready to be written to or not.
    ///
    /// If this stream is not ready for a write then `NotReady` will be returned
    /// and the current task will be scheduled to receive a notification when
    /// the stream is writable again. In other words, this method is only safe
    /// to call from within the context of a future's task, typically done in a
    /// `Future::poll` method.
    pub fn poll_write(&self) -> Async<()> {
        // FIXME: probably needs to only check write.
        if self.inner.get_ref().check(&self.inner) {
            Async::Ready(())
        }
        else {
            Async::NotReady
        }
    }
}

impl Read for UnixStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match (&self.inner.get_ref().channel).read(buf) {
            Ok(n) => Ok(n),
            Err(e) => {
                if e.kind() == io::ErrorKind::WouldBlock {
                    self.block(IoCondition::new().input(true))
                }
                Err(e)
            }
        }
    }
}

impl Write for UnixStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match (&self.inner.get_ref().channel).write(buf) {
            Ok(n) => Ok(n),
            Err(e) => {
                if e.kind() == io::ErrorKind::WouldBlock {
                    self.block(IoCondition::new().output(true))
                }
                Err(e)
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match (&self.inner.get_ref().channel).flush() {
            Ok(n) => Ok(n),
            Err(e) => {
                if e.kind() == io::ErrorKind::WouldBlock {
                    self.block(IoCondition::new().output(true))
                }
                Err(e)
            }
        }
    }
}

fn cvt(i: libc::c_int) -> io::Result<libc::c_int> {
    if i == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(i)
    }
}

impl SourceFuncs for Inner {
    type CallbackArg = ();

    fn prepare(&self, _source: &Source<Self>) -> (bool, Option<Duration>) {
        (false, None)
    }

    fn check(&self, source: &Source<Self>) -> bool {
        // Test to see whether the events on the fd indicate that we're ready to
        // do some work.
        let token = self.token.borrow();
        let token = token.as_ref().unwrap();
        let ready = unsafe { source.unix_query_fd(token) };

        // TODO: handle hup/error events here as well and translate that to
        //       readable/writable.
        (ready.is_input() && self.read.borrow().is_blocked()) ||
            (ready.is_output() && self.write.borrow().is_blocked())
    }

    fn dispatch(&self,
                source: &Source<Self>,
                _f: glib_sys::GSourceFunc,
                _data: glib_sys::gpointer) -> bool {
        // Learn about how we're ready
        let token = self.token.borrow();
        let token = token.as_ref().unwrap();
        let ready = unsafe { source.unix_query_fd(token) };
        let mut active = self.active.borrow_mut();

        // Wake up the read/write tasks as appropriate
        if ready.is_input() {
            if let Some(task) = self.read.borrow_mut().unblock() {
                task.unpark();
            }
            active.input(false);
        }

        if ready.is_output() {
            if let Some(task) = self.write.borrow_mut().unblock() {
                task.unpark();
            }
            active.output(false);
        }

        // Configure the active set of conditions we're listening for.
        unsafe {
            source.unix_modify_fd(token, &active);
        }

        true
    }

    fn g_source_func<F>() -> glib_sys::GSourceFunc
        where F: FnMut(Self::CallbackArg) -> bool
    {
        // we never register a callback on this source, so no need to implement
        // this
        panic!()
    }
}

impl AsyncRead for UnixStream {
    unsafe fn prepare_uninitialized_buffer(&self, _: &mut [u8]) -> bool {
        false
    }

    fn read_buf<B: BufMut>(&mut self, buf: &mut B) -> Poll<usize, io::Error> {
        if let Async::NotReady = <UnixStream>::poll_read(self) {
            return Ok(Async::NotReady)
        }
        let r = (&self.inner.get_ref().channel).read(unsafe { buf.bytes_mut() });

        match r {
            Ok(n) => {
                unsafe { buf.advance_mut(n); }
                Ok(Async::Ready(n))
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                self.block(IoCondition::new().input(true));
                Ok(Async::NotReady)
            }
            Err(e) => Err(e),
        }
    }
}

impl AsyncWrite for UnixStream {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        try_nb!(self.flush());
        Ok(().into())
    }

    fn write_buf<B: Buf>(&mut self, buf: &mut B) -> Poll<usize, io::Error> {
        if let Async::NotReady = <UnixStream>::poll_write(self) {
            return Ok(Async::NotReady)
        }
        let r = (&self.inner.get_ref().channel).write(buf.bytes());
        match r {
            Ok(n) => {
                buf.advance(n);
                Ok(Async::Ready(n))
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                self.block(IoCondition::new().output(true));
                Ok(Async::NotReady)
            }
            Err(e) => Err(e),
        }
    }
}
