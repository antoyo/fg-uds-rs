use std::cell::RefCell;
use std::io;
use std::io::ErrorKind::WouldBlock;
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::os::unix::net::{self, SocketAddr};
use std::path::Path;
use std::time::Duration;

use futures::{Poll, Stream};
use futures::Async::{NotReady, Ready};
use futures::task::Task;
use futures::task;
use futures_glib::{IoCondition, MainContext, Source, SourceFuncs, UnixToken};
use glib_sys;
use nix;
use nix::sys::socket::{AddressFamily, SockAddr, SockType, UnixAddr, bind, listen, socket, SOCK_NONBLOCK};

use super::{UnixStream, new_source};

impl AsRawFd for UnixListener {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.get_ref().listener.as_raw_fd()
    }
}

pub struct UnixListener {
    context: MainContext,
    inner: Source<UnixListenerInner>,
}

impl UnixListener {
    pub fn bind<P: AsRef<Path>>(path: P, context: &MainContext) -> io::Result<Self> {
        let listener = net::UnixListener::bind(path)?;
        listener.set_nonblocking(true)?;
        Ok(Self::from_unix_listener(listener, context))
    }

    pub fn bind_abstract(name: &[u8], context: &MainContext) -> nix::Result<Self> {
        let fd = socket(AddressFamily::Unix, SockType::Stream, SOCK_NONBLOCK, 0)?;
        let addr = SockAddr::Unix(UnixAddr::new_abstract(name)?);
        bind(fd, &addr)?;
        listen(fd, 128)?;
        Ok(Self::from_fd(fd, context))
    }

    pub fn from_fd(fd: RawFd, context: &MainContext) -> Self {
        let listener = unsafe { net::UnixListener::from_raw_fd(fd) };
        Self::from_unix_listener(listener, context)
    }

    fn from_unix_listener(listener: net::UnixListener, context: &MainContext) -> Self {
        let fd = listener.as_raw_fd();
        let mut active = IoCondition::new();
        active.input(true);
        let inner = Source::new(UnixListenerInner {
            active: RefCell::new(active.clone()),
            listener,
            task: RefCell::new(None),
            token: RefCell::new(None),
        });
        inner.attach(context);

        // Add the file descriptor to the source so it knows what we're tracking.
        let token = inner.unix_add_fd(fd, &active);
        *inner.get_ref().token.borrow_mut() = Some(token);
        UnixListener {
            context: context.clone(),
            inner,
        }
    }

    pub fn accept(&mut self) -> io::Result<(UnixStream, SocketAddr)> {
        let inner = self.inner.get_ref();
        match inner.listener.accept() {
            Err(err) => {
                if err.kind() == WouldBlock {
                    *inner.task.borrow_mut() = Some(task::current());

                    // Be sure to update the IoCondition that we're interested so we can get
                    // events related to this condition.
                    let token = inner.token.borrow();
                    let token = token.as_ref().unwrap();
                    let mut active = inner.active.borrow_mut();
                    active.input(true);
                    unsafe {
                        self.inner.unix_modify_fd(token, &active);
                    }
                }
                Err(err)
            },
            Ok((socket, addr)) => {
                socket.set_nonblocking(true)?;
                let inner = new_source(socket.into_raw_fd(), &self.context);
                let stream = UnixStream {
                    inner,
                };
                Ok((stream, addr))
            },
        }
    }

    pub fn incoming(self) -> Incoming {
        Incoming {
            listener: self,
        }
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.get_ref().listener.local_addr()
    }
}

pub struct Incoming {
    listener: UnixListener,
}

impl Stream for Incoming {
    type Item = (UnixStream, SocketAddr);
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        match self.listener.accept() {
            Err(err) => {
                if err.kind() == WouldBlock {
                    // The current task was registered to receive a notification in the accept()
                    // method.
                    Ok(NotReady)
                }
                else {
                    Err(err)
                }
            },
            Ok(val) => {
                Ok(Ready(Some(val)))
            },
        }
    }
}

struct UnixListenerInner {
    active: RefCell<IoCondition>,
    listener: net::UnixListener,
    task: RefCell<Option<Task>>,
    token: RefCell<Option<UnixToken>>,
}

impl SourceFuncs for UnixListenerInner {
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

        ready.is_input() && self.task.borrow().is_some()
    }

    fn dispatch(&self, _source: &Source<Self>, _f: glib_sys::GSourceFunc, _data: glib_sys::gpointer) -> bool {
        if let Some(task) = self.task.borrow_mut().take() {
            task.notify();
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
