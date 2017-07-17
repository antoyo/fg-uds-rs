/*
 * TODO: switch to notify() from park().
 * TODO: remove deprecated calls of futures methods.
 */

extern crate bytes;
extern crate futures;
extern crate futures_glib;
extern crate glib_sys;
extern crate libc;
#[macro_use]
extern crate tokio_io;

mod listener;
mod socket;

use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::io::{self, Read, Write};
use std::mem;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::{AsRawFd, IntoRawFd, RawFd};
use std::path::Path;
use std::time::Duration;

use bytes::{Buf, BufMut};
use futures::{Async, Future, Poll};
use futures::task::{self, Task};
use futures_glib::{IoChannel, IoCondition, MainContext, Source, SourceFuncs, UnixToken};
use libc::{c_ulong, EINPROGRESS, SOCK_CLOEXEC, SOCK_NONBLOCK, SOCK_STREAM};
use tokio_io::{AsyncRead, AsyncWrite};

pub use listener::UnixListener;
use socket::Socket;

fn cvt(i: libc::c_int) -> io::Result<libc::c_int> {
    if i == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(i)
    }
}

pub unsafe fn sockaddr_un(path: &Path)
                          -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    let mut addr: libc::sockaddr_un = mem::zeroed();
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;

    let bytes = path.as_os_str().as_bytes();

    match (bytes.get(0), bytes.len().cmp(&addr.sun_path.len())) {
        // Abstract paths don't need a null terminator
        (Some(&0), Ordering::Greater) => {
            return Err(io::Error::new(io::ErrorKind::InvalidInput,
                                      "path must be no longer than SUN_LEN"));
        }
        (_, Ordering::Greater) | (_, Ordering::Equal) => {
            return Err(io::Error::new(io::ErrorKind::InvalidInput,
                                      "path must be shorter than SUN_LEN"));
        }
        _ => {}
    }
    for (dst, src) in addr.sun_path.iter_mut().zip(bytes.iter()) {
        *dst = *src as libc::c_char;
    }
    // null byte for pathname addresses is already there because we zeroed the
    // struct

    let mut len = sun_path_offset() + bytes.len();
    match bytes.get(0) {
        Some(&0) | None => {}
        Some(_) => len += 1,
    }
    Ok((addr, len as libc::socklen_t))
}

fn sun_path_offset() -> usize {
    unsafe {
        // Work with an actual instance of the type since using a null pointer is UB
        let addr: libc::sockaddr_un = mem::uninitialized();
        let base = &addr as *const _ as usize;
        let path = &addr.sun_path as *const _ as usize;
        path - base
    }
}

#[cfg(unix)]
fn create_source(channel: IoChannel, active: IoCondition, context: &MainContext) -> Source<Inner> {
    let fd = channel.as_raw_fd();
    // Wrap the channel itself in a `Source` that we create and manage.
    let src = Source::new(Inner {
        channel: channel,
        is_closed: Cell::new(false),
        active: RefCell::new(active.clone()),
        read: RefCell::new(State::NotReady),
        write: RefCell::new(State::NotReady),
        token: RefCell::new(None),
    });
    src.attach(context);

    // And finally, add the file descriptor to the source so it knows
    // what we're tracking.
    let t = src.unix_add_fd(fd, &active);
    *src.get_ref().token.borrow_mut() = Some(t);
    src
}

#[cfg(windows)]
fn create_source(channel: IoChannel, active: IoCondition, _context: &MainContext) -> Source<IoChannelFuncs> {
    channel.create_watch(&active)
}

fn create_source_from_socket(socket: Socket, context: &MainContext) -> io::Result<Source<Inner>> {
    // Wrap the socket in a glib GIOChannel type, and configure the
    // channel to be a raw byte stream.
    let channel = IoChannel::unix_new(socket.into_raw_fd());
    channel.set_close_on_drop(true);
    channel.set_encoding(None)?;
    channel.set_buffered(false);

    let mut active = IoCondition::new();
    active.input(true).output(true);

    Ok(create_source(channel, active, context))
}

fn new_source(fd: RawFd, cx: &MainContext) -> Source<Inner> {
    let mut active = IoCondition::new();
    active.input(true).output(true);

    let channel = IoChannel::unix_new(fd);
    channel.set_encoding(None); // TODO: check error.
    // Wrap the channel itself in a `Source` that we create and manage.
    let src = Source::new(Inner {
        channel,
        is_closed: Cell::new(false),
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

/// A raw Unix byte stream connected to a file.
pub struct UnixStream {
    #[cfg(unix)]
    inner: Source<Inner>,
    #[cfg(windows)]
    inner: Source<IoChannelFuncs>,
}

#[cfg(unix)]
struct Inner {
    channel: IoChannel,
    is_closed: Cell<bool>,
    active: RefCell<IoCondition>,
    token: RefCell<Option<UnixToken>>,
    read: RefCell<State>,
    write: RefCell<State>,
}

/// Future returned from `UnixStream::connect` representing a connecting Unix
/// stream.
pub struct UnixStreamConnect {
    state: Option<io::Result<UnixStream>>,
}

impl UnixStream {
    /// Creates a new TCP stream that will connect to the specified address.
    ///
    /// The returned TCP stream will be associated with the provided context. A
    /// future is returned representing the connected TCP stream.
    pub fn connect<P: AsRef<Path>>(path: P, context: &MainContext) -> UnixStreamConnect {
        let socket = (|| -> io::Result<_> {
            // Create the raw socket, set it to nonblocking mode,
            // and then issue a connection to the remote address
            let socket = Socket::new(libc::SOCK_STREAM)?;
            socket.set_nonblocking(true)?;
            match socket.connect(path) {
                Ok(..) => {}
                // TODO: also skip for ECONNREFUSED?
                Err(ref e) if e.raw_os_error() == Some(EINPROGRESS as i32) => {}
                Err(e) => return Err(e),
            }

            let src = create_source_from_socket(socket, context)?;

            Ok(UnixStream { inner: src })
        })();

        UnixStreamConnect {
            state: Some(socket),
        }
    }

    /// Blocks the current future's task internally based on the `condition`
    /// specified.
    #[cfg(unix)]
    fn block(&self, condition: &IoCondition) {
        let inner = self.inner.get_ref();
        let mut active = inner.active.borrow_mut();
        if condition.is_input() {
            *inner.read.borrow_mut() = State::Blocked(task::current());
            active.input(true);
        } else {
            *inner.write.borrow_mut() = State::Blocked(task::current());
            active.output(true);
        }

        // Be sure to update the IoCondition that we're interested so we can get
        // events related to this condition.
        let token = inner.token.borrow();
        let token = token.as_ref().unwrap();
        unsafe {
            self.inner.unix_modify_fd(token, &active);
        }
    }

    #[cfg(windows)]
    fn block(&self, condition: &IoCondition) {
        let inner = self.inner.get_ref();
        if condition.is_output() {
            *inner.write.borrow_mut() = State::Blocked(task::current());
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
        <&UnixStream>::read(&mut &*self, buf)
    }
}

impl<'a> Read for &'a UnixStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.inner.get_ref().is_closed.get() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, io::Error::last_os_error()));
        }
        match (&self.inner.get_ref().channel).read(buf) {
            Ok(n) => Ok(n),
            Err(e) => {
                #[cfg(unix)]
                {
                    if e.kind() == io::ErrorKind::WouldBlock {
                        self.block(IoCondition::new().input(true))
                    }
                }
                Err(e)
            }
        }
    }
}

impl Write for UnixStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        <&UnixStream>::write(&mut &*self, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        <&UnixStream>::flush(&mut &*self)
    }
}

impl<'a> Write for &'a UnixStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match (&self.inner.get_ref().channel).write(buf) {
            Ok(n) => Ok(n),
            Err(e) => {
                #[cfg(unix)]
                {
                    if e.kind() == io::ErrorKind::WouldBlock {
                        self.block(IoCondition::new().output(true))
                    }
                }
                Err(e)
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match (&self.inner.get_ref().channel).flush() {
            Ok(n) => Ok(n),
            Err(e) => {
                #[cfg(unix)]
                {
                    if e.kind() == io::ErrorKind::WouldBlock {
                        self.block(IoCondition::new().output(true))
                    }
                }
                Err(e)
            }
        }
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

impl Future for UnixStreamConnect {
    type Item = UnixStream;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<UnixStream, io::Error> {
        // Wait for the socket to become writable
        let stream = self.state.take().expect("cannot poll twice")?;
        if stream.inner.get_ref().write.borrow_mut().block() {
            self.state = Some(Ok(stream));
            Ok(Async::NotReady)
        } else {
            // TODO: call take_error() and return that error if one exists
            Ok(stream.into())
        }
    }
}

#[cfg(unix)]
impl SourceFuncs for Inner {
    type CallbackArg = ();

    fn prepare(&self, _source: &Source<Self>) -> (bool, Option<Duration>) {
        (false, None)
    }

    fn check(&self, source: &Source<Self>) -> bool {
        // FIXME: check() should not be called after it is closed.
        if source.get_ref().is_closed.get() {
            return false;
        }

        // Test to see whether the events on the fd indicate that we're ready to
        // do some work.
        let token = self.token.borrow();
        let token = token.as_ref().unwrap();
        let ready = unsafe { source.unix_query_fd(token) };

        let inner = source.get_ref();
        if ready.is_hang_up() || ready.is_not_open() {
            // TODO: is `is_not_open()` still needed?
            inner.channel.close(true).unwrap();
            unsafe { source.unix_remove_fd(token) };
            inner.is_closed.set(true);
            return false;
        }

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
                task.notify();
            }
            active.input(false);
        }

        if ready.is_output() {
            if let Some(task) = self.write.borrow_mut().unblock() {
                task.notify();
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

impl AsRawFd for UnixStream {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.get_ref().channel.as_raw_fd()
    }
}
