use std::cell::RefCell;
use std::io;
use std::os::unix::io::{FromRawFd, IntoRawFd, RawFd};
use std::os::unix::net::{self, SocketAddr};
use std::path::Path;
use std::time::Duration;

use futures::{Async, Future, Poll, Stream};
use futures::sync::oneshot;
use futures::task;
use futures_glib::{IoCondition, MainContext, Source, SourceFuncs, UnixToken};
use glib_sys;
use libc;
use tokio_io::IoStream;

use socket::Socket;
use super::{State, UnixStream, cvt, sockaddr_un};

struct Inner {
    listener: net::UnixListener,
    active: RefCell<IoCondition>,
    token: RefCell<Option<UnixToken>>,
    read: RefCell<State>,
    write: RefCell<State>,
}

fn new_source(socket: Socket, cx: &MainContext) -> Source<Inner> {
    let mut active = IoCondition::new();
    active.input(true).output(true);

    let mut active = IoCondition::new();
    active.input(true).output(true);

    let inner = Inner {
        listener: unsafe { net::UnixListener::from_raw_fd(socket.fd()) },
        active: RefCell::new(active.clone()),
        read: RefCell::new(State::NotReady),
        write: RefCell::new(State::NotReady),
        token: RefCell::new(None),
    };
    let src = Source::new(inner);
    src.attach(cx);

    // And finally, add the file descriptor to the source so it knows
    // what we're tracking.
    let t = src.unix_add_fd(socket.fd(), &active);
    *src.get_ref().token.borrow_mut() = Some(t);
    src
}

pub struct UnixListener {
    inner: Source<Inner>,
    pending_accept: Option<oneshot::Receiver<io::Result<(UnixStream, SocketAddr)>>>,
}

impl UnixListener {
    /// Creates a new `UnixListener` bound to the specified socket.
    pub fn bind<P: AsRef<Path>>(path: P, cx: &MainContext) -> io::Result<UnixListener> {
        UnixListener::_bind(path.as_ref(), cx)
    }

    fn _bind(path: &Path, cx: &MainContext) -> io::Result<UnixListener> {
        unsafe {
            let (addr, len) = try!(sockaddr_un(path));
            let socket = try!(Socket::new(libc::SOCK_STREAM));

            let addr = &addr as *const _ as *const _;
            try!(cvt(libc::bind(socket.fd(), addr, len)));
            try!(cvt(libc::listen(socket.fd(), 128)));

            Ok(UnixListener {
                inner: new_source(socket, cx),
                pending_accept: None,
            })
        }
    }

    /// Attempt to accept a connection and create a new connected `UnixStream`
    /// if successful.
    ///
    /// This function will attempt an accept operation, but will not block
    /// waiting for it to complete. If the operation would block then a "would
    /// block" error is returned. Additionally, if this method would block, it
    /// registers the current task to receive a notification when it would
    /// otherwise not block.
    ///
    /// Note that typically for simple usage it's easier to treat incoming
    /// connections as a `Stream` of `UnixStream`s with the `incoming` method
    /// below.
    ///
    /// # Panics
    ///
    /// This function will panic if it is called outside the context of a
    /// future's task. It's recommended to only call this from the
    /// implementation of a `Future::poll`, if necessary.
    pub fn accept(&mut self) -> io::Result<(UnixStream, SocketAddr)> {
        loop {
            if let Some(mut pending) = self.pending_accept.take() {
                match pending.poll().expect("shouldn't be canceled") {
                    Async::NotReady => {
                        self.pending_accept = Some(pending);
                        return Err(would_block())
                    },
                    Async::Ready(r) => {
                        return r;
                    },
                }
            }

            let cx = MainContext::default(|cx| cx.clone());

            match try!(self.accept_inner()) {
                None => {
                    self.block(IoCondition::new().input(true));
                    return Err(io::Error::new(io::ErrorKind::WouldBlock,
                                              "not ready"))
                }
                Some((fd, addr)) => {
                    let inner = super::new_source(fd, &cx);
                    return Ok((UnixStream {
                        inner,
                    }, addr))
                }
            }
        }
    }

    /// Accepts a new incoming connection to this listener.
    ///
    /// When established, the corresponding `UnixStream` and the remote peer's
    /// address will be returned as `Ok(Some(...))`. If there is no connection
    /// waiting to be accepted, then `Ok(None)` is returned.
    ///
    /// If an error happens while accepting, `Err` is returned.
    pub fn accept_inner(&self) -> io::Result<Option<(RawFd, net::SocketAddr)>> {
        match self.inner.get_ref().listener.accept() {
            Ok((socket, addr)) => {
                try!(socket.set_nonblocking(true));
                Ok(Some((socket.into_raw_fd(), addr)))
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
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

    /// Consumes this listener, returning a stream of the sockets this listener
    /// accepts.
    ///
    /// This method returns an implementation of the `Stream` trait which
    /// resolves to the sockets the are accepted on this listener.
    pub fn incoming(self) -> IoStream<(UnixStream, SocketAddr)> {
        struct Incoming {
            inner: UnixListener,
        }

        impl Stream for Incoming {
            type Item = (UnixStream, SocketAddr);
            type Error = io::Error;

            fn poll(&mut self) -> Poll<Option<Self::Item>, io::Error> {
                Ok(Some(try_nb!(self.inner.accept())).into())
            }
        }

        Incoming { inner: self }.boxed()
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
}

fn would_block() -> io::Error {
    io::Error::new(io::ErrorKind::WouldBlock, "would block")
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
