use std::io;
use std::mem;

use libc;
use libc::{c_int, c_ulong, SOCK_CLOEXEC, SOCK_NONBLOCK};

use super::cvt;

pub struct Socket {
    fd: c_int,
}

impl Socket {
    pub fn new(ty: c_int) -> io::Result<Socket> {
        unsafe {
            // On linux we first attempt to pass the SOCK_CLOEXEC flag to
            // atomically create the socket and set it as CLOEXEC. Support for
            // this option, however, was added in 2.6.27, and we still support
            // 2.6.18 as a kernel, so if the returned error is EINVAL we
            // fallthrough to the fallback.
            if cfg!(target_os = "linux") {
                let flags = ty | SOCK_CLOEXEC | SOCK_NONBLOCK;
                match cvt(libc::socket(libc::AF_UNIX, flags, 0)) {
                    Ok(fd) => return Ok(Socket { fd: fd }),
                    Err(ref e) if e.raw_os_error() == Some(libc::EINVAL) => {}
                    Err(e) => return Err(e),
                }
            }

            let fd = Socket { fd: try!(cvt(libc::socket(libc::AF_UNIX, ty, 0))) };
            try!(cvt(libc::ioctl(fd.fd, libc::FIOCLEX)));
            let mut nonblocking = 1 as c_ulong;
            try!(cvt(libc::ioctl(fd.fd, libc::FIONBIO, &mut nonblocking)));
            Ok(fd)
        }
    }

    pub fn fd(&self) -> c_int {
        self.fd
    }

    pub fn into_fd(self) -> c_int {
        let ret = self.fd;
        mem::forget(self);
        ret
    }
}
