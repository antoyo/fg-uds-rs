extern crate bytes;
extern crate fg_uds;
extern crate futures;
extern crate futures_glib;
extern crate tokio_io;

use std::io;
use std::str;
use std::thread;

use bytes::BytesMut;
use fg_uds::UnixStream;
use futures::{Future, Sink, Stream};
use futures_glib::{Executor, MainContext, MainLoop};
use tokio_io::AsyncRead;
use tokio_io::codec::{Decoder, Encoder};

pub struct LineCodec;

impl Decoder for LineCodec {
    type Item = String;
    type Error = io::Error;

    fn decode(&mut self, buf: &mut BytesMut) -> io::Result<Option<String>> {
        if let Some(i) = buf.iter().position(|&b| b == b'\n') {
            // remove the serialized frame from the buffer.
            let line = buf.split_to(i);

            // Also remove the '\n'
            buf.split_to(1);

            // Turn this data into a UTF string and return it in a Frame.
            match str::from_utf8(&line) {
                Ok(s) => Ok(Some(s.to_string())),
                Err(_) => Err(io::Error::new(io::ErrorKind::Other,
                                             "invalid UTF-8")),
            }
        } else {
            Ok(None)
        }
    }
}

impl Encoder for LineCodec {
    type Item = String;
    type Error = io::Error;

    fn encode(&mut self, msg: String, buf: &mut BytesMut) -> io::Result<()> {
        buf.extend(msg.as_bytes());
        buf.extend(b"\n");
        Ok(())
    }
}

fn main() {
    futures_glib::init();

    let cx = MainContext::default(|cx| cx.clone());
    let lp = MainLoop::new(None);
    let ex = Executor::new();
    ex.attach(&cx);

    let (stream1, stream2) = UnixStream::pair(&cx).unwrap();
    let frame1 = stream1.framed(LineCodec);
    let frame2 = stream2.framed(LineCodec);

    ex.spawn(frame1.for_each(|value| {
        println!("Received: {:?}", value);
        Ok(())
    }).map_err(|_| ()));

    let remote = ex.remote();

    thread::spawn(move || {
        remote.spawn(|ex: Executor| {
            ex.spawn(frame2.send("Hello".to_string())
                     .map(|_| ())
                     .map_err(|_| ()));
            Ok(())
        });
    });

    lp.run();
    ex.destroy();
}
