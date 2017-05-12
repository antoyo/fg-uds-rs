use std::ffi::CString;
use std::path::Path;

use gio_sys;

pub struct UnixSocketStream {
    inner: *mut gio_sys::GSocketAddress,
}

impl UnixSocketStream {
    pub fn new_with_type<P: AsRef<Path>>(path: P, typ: UnixSocketAddressType) -> Option<Self> {
        let path = path.as_ref();
        path.to_str()
            .and_then(|string| CString::new(string).ok().map(|cstring| (cstring, string.len())))
            .map(|(cstring, len)| {
                let typ = typ.to_glib();
                let str_ptr = cstring.as_ptr() as *mut _;
                UnixSocketStream {
                    inner: unsafe { gio_sys::g_unix_socket_address_new_with_type(str_ptr, len as i32, typ) },
                }
            })
    }
}

pub enum UnixSocketAddressType {
    Invalid = 0,
    Anonymous,
    Path,
    Abstract,
    AbstractPadded,
}

impl UnixSocketAddressType {
    fn to_glib(&self) -> gio_sys::GUnixSocketAddressType {
        match *self {
            Invalid => gio_sys::G_UNIX_SOCKET_ADDRESS_INVALID,
            Anonymous => gio_sys::G_UNIX_SOCKET_ADDRESS_ANONYMOUS,
            Path => gio_sys::G_UNIX_SOCKET_ADDRESS_PATH,
            Abstract => gio_sys::G_UNIX_SOCKET_ADDRESS_ABSTRACT,
            AbstractPadded => gio_sys::G_UNIX_SOCKET_ADDRESS_ABSTRACT_PADDED,
        }
    }
}
