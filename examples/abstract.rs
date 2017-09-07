extern crate bytes;
extern crate fg_uds;
extern crate futures;
extern crate futures_glib;
extern crate tokio_io;

use std::io;
use std::str;
use std::thread;

use bytes::BytesMut;
use fg_uds::{UnixListener, UnixStream};
use futures::{AsyncSink, Future, Sink, Stream};
use futures_glib::{Executor, MainContext, MainLoop};
use tokio_io::AsyncRead;
use tokio_io::codec::{Decoder, Encoder, FramedRead, FramedWrite};

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

    let path = b"named-socket";

    let cx = MainContext::default(|cx| cx.clone());
    let lp = MainLoop::new(None);
    let ex = Executor::new();
    ex.attach(&cx);

    let listener = UnixListener::bind_abstract(path, &cx).unwrap();

    let remote = ex.remote();

    let inner_ex = ex.clone();
    let incoming = listener.incoming()
        .for_each(move |(stream, _addr)| {
            let (reader, writer) = stream.split();
            let mut framed_writer = FramedWrite::new(writer, LineCodec);
            let framed_reader = FramedRead::new(reader, LineCodec);

            inner_ex.spawn(framed_reader.and_then(|value| {
                println!("Received: {:?}", value);
                Ok(())
            }).for_each(move |_| {
                if let Ok(AsyncSink::Ready) = framed_writer.start_send("Received".to_string()) {
                    framed_writer.poll_complete().unwrap();
                }
                Ok(())
            }).map_err(|_| ()));
            Ok(())
        })
        .map_err(|_| ());

    ex.spawn(incoming);

    thread::spawn(move || {
        remote.spawn(move |ex: Executor| {
            let connection = UnixStream::connect_abstract(path, &cx).unwrap();
            let exe = ex.clone();
            let future = connection.and_then(move |stream| {
                let (reader, writer) = stream.split();
                let framed_writer = FramedWrite::new(writer, LineCodec);
                let framed_reader = FramedRead::new(reader, LineCodec);
                exe.spawn(framed_reader.for_each(|value| {
                    println!("Thread received: {:?}", value);
                    Ok(())
                }).map_err(|_| ()));
                exe.spawn(framed_writer.send("Hello".to_string())
                         .map(|_| ())
                         .map_err(|_| ()));
                Ok(())
            })
                .map_err(|_| ());
            ex.spawn(future);
            Ok(())
        });
    });

    lp.run();
    ex.destroy();
}
