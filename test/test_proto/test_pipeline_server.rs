use support::{self, mock};
use tokio;
use tokio::proto::pipeline::{self, Frame, Message};
use tokio::reactor::{self, Reactor};
use tokio::util::future::{self, Receiver};
use futures::{Future, failed, finished};
use futures::stream::Stream;
use std::io;
use std::sync::{mpsc, Arc, Mutex};

// The message type is a static string for both the request and response
type Msg = &'static str;

// The body stream is a stream of u32 values
type Body = Receiver<u32, io::Error>;

// Frame written to the transport
type InFrame = Frame<Msg, io::Error, u32>;
type OutFrame = Frame<pipeline::Message<Msg, Body>, io::Error, u32>;

#[test]
fn test_immediate_done() {
    let service = tokio::simple_service(|req| {
        finished(req)
    });

    run(service, |mock| {
        mock.send(pipeline::Frame::Done);
        mock.allow_and_assert_drop();
    });
}

#[test]
fn test_immediate_writable_echo() {
    let service = tokio::simple_service(|req| {
        assert_eq!(req, "hello");
        finished((req))
    });

    run(service, |mock| {
        mock.allow_write();
        mock.send(msg("hello"));
        assert_eq!(mock.next_write().unwrap_msg(), "hello");

        mock.send(Frame::Done);
        mock.allow_and_assert_drop();
    });
}

#[test]
fn test_immediate_writable_delayed_response_echo() {
    let (c, fut) = future::pair();
    let fut = Mutex::new(Some(fut));

    let service = tokio::simple_service(move |req| {
        assert_eq!(req, "hello");
        fut.lock().unwrap().take().unwrap()
    });

    run(service, |mock| {
        mock.allow_write();
        mock.send(msg("hello"));

        support::sleep_ms(20);
        c.complete(Message::WithoutBody("goodbye"));

        assert_eq!(mock.next_write().unwrap_msg(), "goodbye");

        mock.send(Frame::Done);
        mock.allow_and_assert_drop();
    });
}

#[test]
fn test_delayed_writable_immediate_response_echo() {
    let service = tokio::simple_service(|req| {
        assert_eq!(req, "hello");
        finished((req))
    });

    run(service, |mock| {
        mock.send(msg("hello"));

        support::sleep_ms(20);

        mock.allow_write();
        assert_eq!(mock.next_write().unwrap_msg(), "hello");
    });
}

#[test]
fn test_pipelining_while_service_is_processing() {
    let (tx, rx) = channel();

    let service = tokio::simple_service(move |_| {
        let (c, fut) = future::pair();
        tx.lock().unwrap().send(c).unwrap();
        fut
    });

    run(service, |mock| {
        // Allow all the writes
        for _ in 0..3 { mock.allow_write() };

        mock.send(msg("hello"));
        let c1 = rx.recv().unwrap();

        mock.send(msg("hello"));
        let c2 = rx.recv().unwrap();

        mock.send(msg("hello"));
        let c3 = rx.recv().unwrap();

        mock.assert_no_write(20);
        c3.complete(Message::WithoutBody("three"));

        mock.assert_no_write(20);
        c2.complete(Message::WithoutBody("two"));

        mock.assert_no_write(20);
        c1.complete(Message::WithoutBody("one"));

        assert_eq!("one", mock.next_write().unwrap_msg());
        assert_eq!("two", mock.next_write().unwrap_msg());
        assert_eq!("three", mock.next_write().unwrap_msg());
    });
}

#[test]
fn test_pipelining_while_transport_not_writable() {
    let (tx, rx) = channel();

    let service = tokio::simple_service(move |req: Message<&'static str, Body>| {
        tx.lock().unwrap().send(req.clone()).unwrap();
        finished(req)
    });

    run(service, |mock| {
        mock.send(msg("one"));
        mock.send(msg("two"));
        mock.send(msg("three"));

        // Assert the service received all the requests before they are written
        // to the transport
        assert_eq!("one", rx.recv().unwrap());
        assert_eq!("two", rx.recv().unwrap());
        assert_eq!("three", rx.recv().unwrap());

        mock.allow_write();
        assert_eq!("one", mock.next_write().unwrap_msg());

        mock.allow_write();
        assert_eq!("two", mock.next_write().unwrap_msg());

        mock.allow_write();
        assert_eq!("three", mock.next_write().unwrap_msg());
    });
}

#[test]
fn test_repeatedly_flushes_messages() {
    let service = tokio::simple_service(move |req| {
        finished(req)
    });

    run(service, |mock| {
        mock.send(msg("hello"));

        mock.allow_and_assert_flush();
        mock.allow_and_assert_flush();

        mock.allow_write();
        assert_eq!("hello", mock.next_write().unwrap_msg());

        mock.send(Frame::Done);
        mock.assert_drop();
    });
}

#[test]
fn test_returning_error_from_service() {
    let service = tokio::simple_service(move |_| {
        failed(io::Error::new(io::ErrorKind::Other, "nope"))
    });

    run(service, |mock| {
        mock.send(msg("hello"));

        mock.allow_write();
        assert_eq!(io::ErrorKind::Other, mock.next_write().unwrap_err().kind());

        mock.assert_no_write(20);

        mock.send(Frame::Done);
        mock.assert_drop();
    });
}

#[test]
fn test_reading_error_frame_from_transport() {
    let service = tokio::simple_service(move |_| {
        finished(Message::WithoutBody("omg no"))
    });

    run(service, |mock| {
        mock.send(Frame::Error(io::Error::new(io::ErrorKind::Other, "mock transport error frame")));
        mock.assert_drop();
    });
}

#[test]
fn test_reading_io_error_from_transport() {
    let service = tokio::simple_service(move |_| {
        finished(Message::WithoutBody("omg no"))
    });

    run(service, |mock| {
        mock.error(io::Error::new(io::ErrorKind::Other, "mock transport error frame"));
        mock.assert_drop();
    });
}

#[test]
#[ignore]
fn test_reading_error_while_pipelining_from_transport() {
    unimplemented!();
}

#[test]
#[ignore]
fn test_returning_would_block_from_service() {
    // Because... it could happen
}

#[test]
fn test_streaming_request_body_then_responding() {
    let (tx, rx) = channel();

    let service = tokio::simple_service(move |mut req: Message<&'static str, Body>| {
        assert_eq!(req, "omg");

        let body = req.take_body().unwrap();
        let tx = tx.clone();

        body.for_each(move |chunk| {
                tx.lock().unwrap().send(chunk).unwrap();
                Ok(())
            })
            .and_then(|_| finished(Message::WithoutBody("hi2u")))
    });

    run(service, |mock| {
        mock.allow_write();
        mock.send(msg_with_body("omg"));

        for i in 0..5 {
            mock.send(Frame::Body(Some(i)));
            assert_eq!(i, rx.recv().unwrap());
        }

        // Send end-of-stream notification
        mock.send(Frame::Body(None));

        assert_eq!(mock.next_write().unwrap_msg(), "hi2u");

        mock.send(Frame::Done);
        mock.allow_and_assert_drop();
    });
}

#[test]
fn test_responding_then_streaming_request_body() {
    let (tx, rx) = channel();

    let service = tokio::simple_service(move |mut req: Message<&'static str, Body>| {
        assert_eq!(req, "omg");

        let body = req.take_body().unwrap();
        let tx = tx.clone();

        body.for_each(move |chunk| {
                tx.lock().unwrap().send(chunk).unwrap();
                Ok(())
            })
            .forget();

        finished(Message::WithoutBody("hi2u"))
    });

    run(service, |mock| {
        mock.allow_write();
        mock.send(msg_with_body("omg"));

        assert_eq!(mock.next_write().unwrap_msg(), "hi2u");

        for i in 0..5 {
            mock.send(Frame::Body(Some(i)));
            assert_eq!(i, rx.recv().unwrap());
        }

        // Send end-of-stream notification
        mock.send(Frame::Body(None));

        mock.send(Frame::Done);
        mock.allow_and_assert_drop();
    });
}

#[test]
fn test_pipeline_streaming_body_without_consuming() {
    let (tx, rx) = channel();

    let service = tokio::simple_service(move |mut req: Message<&'static str, Body>| {
        let body = req.take_body().unwrap();

        if req == "one" {
            finished(Message::WithoutBody("resp-one")).boxed()
        } else {
            let tx = tx.clone();

            body.for_each(move |chunk| {
                    tx.lock().unwrap().send(chunk).unwrap();
                    Ok(())
                })
                .and_then(|_| finished(Message::WithoutBody("resp-two")))
                .boxed()
        }
    });

    run(service, |mock| {
        mock.allow_write();
        mock.send(msg_with_body("one"));

        for i in 0..5 {
            mock.send(Frame::Body(Some(i)));
            support::sleep_ms(20);
            assert!(rx.try_recv().is_err());
        }

        assert_eq!(mock.next_write().unwrap_msg(), "resp-one");

        // Send the next request
        mock.send(msg_with_body("two"));

        for i in 0..5 {
            mock.send(Frame::Body(Some(i)));
            assert_eq!(i, rx.recv().unwrap());
        }

        mock.send(Frame::Body(None));

        mock.allow_write();
        assert_eq!(mock.next_write().unwrap_msg(), "resp-two");

        mock.send(Frame::Done);
        mock.allow_and_assert_drop();
    });
}

#[test]
#[ignore]
fn test_transport_error_during_body_stream() {
}

#[test]
fn test_streaming_response_body() {
    let (tx, rx) = future::channel::<u32, io::Error>();
    let rx = Mutex::new(Some(rx));

    let service = tokio::simple_service(move |req| {
        assert_eq!(req, "omg");
        finished(Message::WithBody("hi2u", rx.lock().unwrap().take().unwrap()))
    });

    run(service, |mock| {
        mock.allow_write();
        mock.send(msg("omg"));

        assert_eq!(mock.next_write().unwrap_msg(), "hi2u");

        mock.assert_no_write(20);

        mock.allow_write();
        let tx = support::await(tx.send(Ok(1)).unwrap()).unwrap();
        assert_eq!(Some(1), mock.next_write().unwrap_body());

        let _ = support::await(tx.send(Ok(2)).unwrap());
        mock.assert_no_write(20);
        mock.allow_write();
        assert_eq!(Some(2), mock.next_write().unwrap_body());

        mock.allow_write();
        assert_eq!(None, mock.next_write().unwrap_body());

        mock.send(Frame::Done);
        mock.allow_and_assert_drop();
    });
}

fn channel<T>() -> (Arc<Mutex<mpsc::Sender<T>>>, mpsc::Receiver<T>) {
    let (tx, rx) = mpsc::channel();
    let tx = Arc::new(Mutex::new(tx));
    (tx, rx)
}

fn msg(msg: Msg) -> OutFrame {
    Frame::Message(Message::WithoutBody(msg))
}

fn msg_with_body(msg: Msg) -> OutFrame {
    let (tx, rx) = future::channel();
    Frame::MessageWithBody(Message::WithBody(msg, rx), tx)
}

/// Setup a reactor running a pipeline::Server with the given service and a
/// mock transport. Yields the mock transport handle to the function.
fn run<S, F>(service: S, f: F)
    where S: pipeline::ServerService<Req = pipeline::Message<Msg, Body>, Resp = Msg, Body = u32, BodyStream = Body, Error = io::Error>,
          F: FnOnce(mock::TransportHandle<InFrame, OutFrame>),
{
    let _ = ::env_logger::init();
    let r = Reactor::default().unwrap();
    let h = r.handle();

    let (mock, new_transport) = mock::transport::<InFrame, OutFrame>();

    // Spawn the reactor
    r.spawn();

    h.oneshot(move || {
        let transport = try!(new_transport.new_transport());
        let dispatch = try!(pipeline::Server::new(service, transport));

        try!(reactor::schedule(dispatch));

        Ok(())
    });

    f(mock);


    h.shutdown();
}
