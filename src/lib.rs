extern crate bytes;
extern crate futures;
extern crate futures_glib;
extern crate gio_sys;
extern crate libc;
extern crate tokio_io;

//mod gio;

use std::io::{self, Read, Write};

use bytes::{Buf, BufMut};
use futures::{Async, Poll};
use futures_glib::IoChannel;
use libc::{c_ulong, SOCK_CLOEXEC, SOCK_NONBLOCK, SOCK_STREAM};
use tokio_io::{AsyncRead, AsyncWrite};

pub struct UnixStream {
    channel: IoChannel,
}

impl UnixStream {
    pub fn pair() -> io::Result<(UnixStream, UnixStream)> {
        unsafe {
            let mut fds = [0, 0];

            // Like above, see if we can set cloexec atomically
            if cfg!(target_os = "linux") {
                let flags = SOCK_STREAM | SOCK_CLOEXEC | SOCK_NONBLOCK;
                match cvt(libc::socketpair(libc::AF_UNIX, flags, 0, fds.as_mut_ptr())) {
                    Ok(_) => {
                        let socket1 = UnixStream {
                            channel: IoChannel::unix_new(fds[0]),
                        };
                        let socket2 = UnixStream {
                            channel: IoChannel::unix_new(fds[1]),
                        };
                        return Ok((socket1, socket2))
                    }
                    Err(ref e) if e.raw_os_error() == Some(libc::EINVAL) => {},
                    Err(e) => return Err(e),
                }
            }

            try!(cvt(libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr())));
            let socket1 = UnixStream {
                channel: IoChannel::unix_new(fds[0]),
            };
            let socket2 = UnixStream {
                channel: IoChannel::unix_new(fds[1]),
            };
            try!(cvt(libc::ioctl(fds[0], libc::FIOCLEX)));
            try!(cvt(libc::ioctl(fds[1], libc::FIOCLEX)));
            let mut nonblocking = 1 as c_ulong;
            try!(cvt(libc::ioctl(fds[0], libc::FIONBIO, &mut nonblocking)));
            try!(cvt(libc::ioctl(fds[1], libc::FIONBIO, &mut nonblocking)));
            Ok((socket1, socket2))
        }
    }
}

impl Read for UnixStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.channel.read(buf)
    }
}

impl Write for UnixStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.channel.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.channel.flush()
    }
}

fn cvt(i: libc::c_int) -> io::Result<libc::c_int> {
    if i == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(i)
    }
}

impl AsyncRead for UnixStream {
    unsafe fn prepare_uninitialized_buffer(&self, _: &mut [u8]) -> bool {
        false
    }

    fn read_buf<B: BufMut>(&mut self, buf: &mut B) -> Poll<usize, io::Error> {
        // TODO: make async.
        let r = unsafe {
            self.channel.read(buf.bytes_mut())
        };

        match r {
            Ok(n) => {
                unsafe { buf.advance_mut(n); }
                Ok(Async::Ready(n))
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                //self.io.need_read();
                Ok(Async::NotReady)
            }
            Err(e) => Err(e),
        }
    }
}

impl AsyncWrite for UnixStream {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        self.channel.close(true).ok(); // TODO: handle error.
        Ok(().into())
    }

    fn write_buf<B: Buf>(&mut self, buf: &mut B) -> Poll<usize, io::Error> {
        // TODO: make async.
        let r = self.channel.write(buf.bytes());
        println!("Write");
        match r {
            Ok(n) => {
                println!("{}", n);
                buf.advance(n);
                Ok(Async::Ready(n))
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                //self.io.need_write();
                Ok(Async::NotReady)
            }
            Err(e) => Err(e),
        }
    }
}
