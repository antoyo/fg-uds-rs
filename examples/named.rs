extern crate bytes;
extern crate fg_uds;
extern crate futures;
extern crate futures_glib;
extern crate tokio_io;

use std::fs::remove_file;
use std::io;
use std::str;
use std::thread;

use bytes::BytesMut;
use fg_uds::{UnixListener, UnixStream};
use futures::{Future, Sink, Stream};
use futures::sync::oneshot::channel;
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

    let path = "/tmp/named.socket";

    let cx = MainContext::default(|cx| cx.clone());
    let lp = MainLoop::new(None);
    let ex = Executor::new();
    ex.attach(&cx);

    let remote = ex.remote();

    let (sender, receiver) = channel();

    let inner_path = path.clone();
    thread::spawn(move || {
        remote.spawn(move |ex: Executor| {
            let cx = MainContext::default(|cx| cx.clone());
            remove_file(inner_path).ok();
            let listener = UnixListener::bind(inner_path, &cx).unwrap();

            sender.send(true);

            let inner_ex = ex.clone();
            let incoming = listener.incoming()
                .for_each(move |(stream, _addr)| {
                    let frame = stream.framed(LineCodec);

                    inner_ex.spawn(frame.for_each(|value| {
                        println!("Received: {:?}", value);
                        Ok(())
                    }).map_err(|_| ()));
                    Ok(())
                })
            .map_err(|_| ());

            ex.spawn(incoming);
            Ok(())
        });
    });

    let send = receiver.then(move |_| {
            UnixStream::connect(path, &cx)
        })
        .and_then(|stream2| {
            Ok(stream2.framed(LineCodec))
        })
        .and_then(|frame2| {
            frame2.send("Hello".to_string())
        });
    // TODO: send too early? Should accept before?
    ex.spawn(send.map(|_| ())
             .map_err(|_| ()));

    lp.run();
    ex.destroy();
}
