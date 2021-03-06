use Service;
use super::{pipeline, Error, Message, Transport, NewTransport};
use reactor::{self, ReactorHandle};
use util::channel::{Receiver};
use util::future::{self, Complete, Val};
use futures::stream::Stream;
use mio::channel;
use std::io;
use std::collections::VecDeque;

/// Client `Service` for the pipeline protocol.
///
/// Initiated requests are sent to the client pipeline task running on the
/// Reactor where they are processed. The response is returned by completing
/// the future.
pub struct Client<Req, Resp, ReqBody, E>
    where Req: Send + 'static,
          Resp: Send + 'static,
          ReqBody: Stream<Error = E>,
          E: From<Error<E>> + Send + 'static,
{
    tx: channel::Sender<(Message<Req, ReqBody>, Complete<Resp, E>)>,
}

struct Dispatch<T, B, E>
    where T: Transport<Error = E>,
          B: Stream<Item = T::BodyIn, Error = E>,
          E: From<Error<E>> + Send + 'static,
{
    requests: Receiver<(Message<T::In, B>, Complete<T::Out, E>)>,
    in_flight: VecDeque<Complete<T::Out, E>>,
}

/// Connect to the given `addr` and handle using the given Transport and protocol pipelining.
pub fn connect<T, B, E>(reactor: &ReactorHandle, new_transport: T)
        -> Client<T::In, T::Out, B, E>
    where T: NewTransport<Error = E>,
          B: Stream<Item = T::BodyIn, Error = E>,
          E: From<Error<E>> + Send + 'static,
{
    let (tx, rx) = channel::channel();

    reactor.oneshot(move || {
        // Convert to Tokio receiver
        let rx = try!(Receiver::watch(rx));

        // Create the transport
        let transport = try!(new_transport.new_transport());

        // Create the client dispatch
        let dispatch: Dispatch<T::Item, B, E> = Dispatch {
            requests: rx,
            in_flight: VecDeque::with_capacity(32),
        };

        // Create the pipeline with the dispatch and transport
        let pipeline = try!(pipeline::Pipeline::new(dispatch, transport));

        try!(reactor::schedule(pipeline));
        Ok(())
    });

    Client { tx: tx }
}

impl<Req, Resp, ReqBody, E> Service for Client<Req, Resp, ReqBody, E>
    where Req: Send + 'static,
          Resp: Send + 'static,
          ReqBody: Stream<Error = E>,
          E: From<Error<E>> + Send + 'static,
{
    type Req = Message<Req, ReqBody>;
    type Resp = Resp;
    type Error = E;
    type Fut = Val<Self::Resp, E>;

    fn call(&self, request: Self::Req) -> Self::Fut {
        let (c, val) = future::pair();

        // TODO: handle error
        self.tx.send((request, c)).ok().unwrap();

        val
    }
}

impl<Req, Resp, ReqBody, E> Clone for Client<Req, Resp, ReqBody, E>
    where Req: Send + 'static,
          Resp: Send + 'static,
          ReqBody: Stream<Error = E>,
          E: From<Error<E>> + Send + 'static,
{
    fn clone(&self) -> Client<Req, Resp, ReqBody, E> {
        Client { tx: self.tx.clone() }
    }
}

impl<T, B, E> pipeline::Dispatch for Dispatch<T, B, E>
    where T: Transport<Error = E>,
          B: Stream<Item = T::BodyIn, Error = E>,
          E: From<Error<E>> + Send + 'static,
{
    type InMsg = T::In;
    type InBody = T::BodyIn;
    type InBodyStream = B;
    type OutMsg = T::Out;
    type Error = E;

    fn dispatch(&mut self, response: Self::OutMsg) -> io::Result<()> {
        if let Some(complete) = self.in_flight.pop_front() {
            complete.complete(response);
        } else {
            return Err(io::Error::new(io::ErrorKind::Other, "request / response mismatch"));
        }

        Ok(())
    }

    fn poll(&mut self) -> Option<Result<Message<Self::InMsg, Self::InBodyStream>, Self::Error>> {
        // Try to get a new request frame
        match self.requests.recv() {
            Ok(Some((request, complete))) => {
                trace!("received request");

                // Track complete handle
                self.in_flight.push_back(complete);

                Some(Ok(request))

            }
            Ok(None) => None,
            Err(e) => {
                // An error on receive can only happen when the other half
                // disconnected. In this case, the client needs to be
                // shutdown
                panic!("unimplemented error handling: {:?}", e);
            }
        }
    }

    fn has_in_flight(&self) -> bool {
        !self.in_flight.is_empty()
    }
}

impl<T, B, E> Drop for Dispatch<T, B, E>
    where T: Transport<Error = E>,
          B: Stream<Item = T::BodyIn, Error = E>,
          E: From<Error<E>> + Send + 'static,
{
    fn drop(&mut self) {
        // Complete any pending requests with an error
        while let Some(complete) = self.in_flight.pop_front() {
            let err = Error::Io(broken_pipe());
            complete.error(err.into());
        }
    }
}

fn broken_pipe() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "broken pipe")
}
