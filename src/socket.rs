use std::io;
use std::mem;
use std::os::unix::io::{IntoRawFd, RawFd};
use std::path::Path;

use libc;
use libc::{c_int, c_ulong};

#[cfg(target_os = "linux")]
use libc::{SOCK_CLOEXEC, SOCK_NONBLOCK};

use super::{cvt, sockaddr_un};

pub struct Socket {
    fd: c_int,
}

impl Socket {
    #[cfg(target_os = "linux")]
    pub fn new(ty: c_int) -> io::Result<Socket> {
        unsafe {
            // On linux we first attempt to pass the SOCK_CLOEXEC flag to
            // atomically create the socket and set it as CLOEXEC. Support for
            // this option, however, was added in 2.6.27, and we still support
            // 2.6.18 as a kernel, so if the returned error is EINVAL we
            // fallthrough to the fallback.
            let flags = ty | SOCK_CLOEXEC | SOCK_NONBLOCK;
            match cvt(libc::socket(libc::AF_UNIX, flags, 0)) {
                Ok(fd) => Ok(Socket { fd: fd }),
                Err(ref e) if e.raw_os_error() == Some(libc::EINVAL) => Socket::fallback_new(ty),
                Err(e) => Err(e),
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    pub fn new(ty: c_int) -> io::Result<Socket> {
        Socket::fallback_new(ty)
    }

    fn fallback_new(ty: c_int) -> io::Result<Socket> {
        unsafe {
            let fd = Socket { fd: try!(cvt(libc::socket(libc::AF_UNIX, ty, 0))) };
            try!(cvt(libc::ioctl(fd.fd, libc::FIOCLEX)));
            Ok(fd)
        }
    }

    pub fn connect<P: AsRef<Path>>(&self, path: P) -> io::Result<()> {
        unsafe {
            let (addr, len) = sockaddr_un(path.as_ref())?;
            cvt(libc::connect(self.fd, &addr as *const _ as *const _, len))?;
        }
        Ok(())
    }

    pub fn fd(&self) -> c_int {
        self.fd
    }

    pub fn into_fd(self) -> c_int {
        let ret = self.fd;
        mem::forget(self);
        ret
    }

    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        let mut nonblocking = nonblocking as c_ulong;
        unsafe {
            cvt(libc::ioctl(self.fd, libc::FIONBIO, &mut nonblocking))?;
        }
        Ok(())
    }
}

impl IntoRawFd for Socket {
    fn into_raw_fd(self) -> RawFd {
        self.fd
    }
}
