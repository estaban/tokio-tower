//! Buffer requests when the inner service is out of capacity.
//!
//! Buffering works by spawning a new task that is dedicated to pulling requests
//! out of the buffer and dispatching them to the inner service. By adding a
//! buffer and a dedicated task, the `Buffer` layer in front of the service can
//! be `Clone` even if the inner service is not.
//!
//! This is a version of `tower-buffer` adapted to use `DirectService`.

use futures::future::Executor;
use futures::sync::mpsc;
use futures::sync::oneshot;
use futures::{Async, Future, Poll, Stream};
use tower_service::Service;
use DirectService;

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::{error, fmt};

/// Adds a buffer in front of an inner service.
///
/// See crate level documentation for more details.
pub struct Buffer<T, Request>
where
    T: DirectService<Request>,
{
    tx: mpsc::Sender<Message<T, Request>>,
    state: Arc<State>,
}

/// Future eventually completed with the response to the original request.
pub struct ResponseFuture<T, Request>
where
    T: DirectService<Request>,
{
    state: ResponseState<T::Future>,
}

/// Errors produced by `Buffer`.
#[derive(Debug)]
pub enum Error<T> {
    /// The `Service` call errored.
    Inner(T),
    /// The underlying `Service` failed.
    Closed,
}

/// Task that handles processing the buffer. This type should not be used
/// directly, instead `Buffer` requires an `Executor` that can accept this task.
pub struct Worker<T, Request>
where
    T: DirectService<Request>,
{
    current_message: Option<Message<T, Request>>,
    rx: mpsc::Receiver<Message<T, Request>>,
    service: T,
    finish: bool,
    state: Arc<State>,
}

/// Error produced when spawning the worker fails
#[derive(Debug)]
pub struct SpawnError<T> {
    inner: T,
}

/// Message sent over buffer
#[derive(Debug)]
struct Message<T, Request>
where
    T: DirectService<Request>,
{
    request: Request,
    tx: oneshot::Sender<T::Future>,
}

/// State shared between `Buffer` and `Worker`
struct State {
    open: AtomicBool,
}

enum ResponseState<T> {
    Failed,
    Rx(oneshot::Receiver<T>),
    Poll(T),
}

impl<T, Request> Buffer<T, Request>
where
    T: DirectService<Request>,
{
    /// Creates a new `Buffer` wrapping `service`.
    ///
    /// `executor` is used to spawn a new `Worker` task that is dedicated to
    /// draining the buffer and dispatching the requests to the internal
    /// service.
    ///
    /// `bound` gives the maximal number of requests that can be queued for the service before
    /// backpressure is applied to callers.
    pub fn new<E>(service: T, bound: usize, executor: &E) -> Result<Self, SpawnError<T>>
    where
        E: Executor<Worker<T, Request>>,
    {
        let (tx, rx) = mpsc::channel(bound);

        let state = Arc::new(State {
            open: AtomicBool::new(true),
        });

        let worker = Worker {
            current_message: None,
            rx,
            service,
            finish: false,
            state: state.clone(),
        };

        // TODO: handle error
        executor.execute(worker).ok().unwrap();

        Ok(Buffer { tx, state: state })
    }
}

impl<T, Request> Service<Request> for Buffer<T, Request>
where
    T: DirectService<Request>,
{
    type Response = T::Response;
    type Error = Error<T::Error>;
    type Future = ResponseFuture<T, Request>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        // If the inner service has errored, then we error here.
        if !self.state.open.load(Ordering::Acquire) {
            return Err(Error::Closed);
        } else {
            self.tx.poll_ready().map_err(|_| Error::Closed)
        }
    }

    fn call(&mut self, request: Request) -> Self::Future {
        // TODO:
        // ideally we'd poll_ready again here so we don't allocate the oneshot
        // if the try_send is about to fail, but sadly we can't call poll_ready
        // outside of task context.
        let (tx, rx) = oneshot::channel();

        let sent = self.tx.try_send(Message { request, tx });
        if sent.is_err() {
            self.state.open.store(false, Ordering::Release);
            ResponseFuture {
                state: ResponseState::Failed,
            }
        } else {
            ResponseFuture {
                state: ResponseState::Rx(rx),
            }
        }
    }
}

impl<T, Request> Clone for Buffer<T, Request>
where
    T: DirectService<Request>,
{
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            state: self.state.clone(),
        }
    }
}

// ===== impl ResponseFuture =====

impl<T, Request> Future for ResponseFuture<T, Request>
where
    T: DirectService<Request>,
{
    type Item = T::Response;
    type Error = Error<T::Error>;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        use self::ResponseState::*;

        loop {
            let fut;

            match self.state {
                Failed => {
                    return Err(Error::Closed);
                }
                Rx(ref mut rx) => match rx.poll() {
                    Ok(Async::Ready(f)) => fut = f,
                    Ok(Async::NotReady) => return Ok(Async::NotReady),
                    Err(_) => return Err(Error::Closed),
                },
                Poll(ref mut fut) => {
                    return fut.poll().map_err(Error::Inner);
                }
            }

            self.state = Poll(fut);
        }
    }
}

// ===== impl Worker =====

impl<T, Request> Worker<T, Request>
where
    T: DirectService<Request>,
{
    /// Return the next queued Message that hasn't been canceled.
    fn poll_next_msg(&mut self) -> Poll<Option<Message<T, Request>>, ()> {
        if self.finish {
            // We've already received None and are shutting down
            return Ok(Async::Ready(None));
        }

        if let Some(mut msg) = self.current_message.take() {
            // poll_cancel returns Async::Ready is the receiver is dropped.
            // Returning NotReady means it is still alive, so we should still
            // use it.
            if msg.tx.poll_cancel()?.is_not_ready() {
                return Ok(Async::Ready(Some(msg)));
            }
        }

        // Get the next request
        while let Some(mut msg) = try_ready!(self.rx.poll()) {
            if msg.tx.poll_cancel()?.is_not_ready() {
                return Ok(Async::Ready(Some(msg)));
            }
            // Otherwise, request is canceled, so pop the next one.
        }

        Ok(Async::Ready(None))
    }
}

impl<T, Request> Future for Worker<T, Request>
where
    T: DirectService<Request>,
{
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<(), ()> {
        let mut any_outstanding = true;
        loop {
            match self.poll_next_msg()? {
                Async::Ready(Some(msg)) => {
                    // Wait for the service to be ready
                    match self.service.poll_ready() {
                        Ok(Async::Ready(())) => {
                            let response = self.service.call(msg.request);

                            // Send the response future back to the sender.
                            //
                            // An error means the request had been canceled in-between
                            // our calls, the response future will just be dropped.
                            let _ = msg.tx.send(response);

                            // Try to queue another request before we poll outstanding requests.
                            any_outstanding = true;
                            continue;
                        }
                        Ok(Async::NotReady) => {
                            // Put out current message back in its slot.
                            self.current_message = Some(msg);
                            // We don't want to return quite yet
                            // We want to also make progress on current requests
                            break;
                        }
                        Err(_) => {
                            self.state.open.store(false, Ordering::Release);
                            return Ok(().into());
                        }
                    }
                }
                Async::Ready(None) => {
                    // No more more requests _ever_.
                    self.finish = true;
                }
                Async::NotReady if any_outstanding => {
                    // Make some progress on the service if we can.
                }
                Async::NotReady => {
                    // There are no outstanding requests to make progress on.
                    // And we don't have any new requests to enqueue.
                    // So we yield.
                    return Ok(Async::NotReady);
                }
            }

            if self.finish {
                try_ready!(self.service.poll_close().map_err(|_| ()));
                // We are all done!
                break;
            } else {
                if let Async::Ready(()) = self.service.poll_outstanding().map_err(|_| ())? {
                    // Note to future iterations that there's no reason to call poll_outsanding.
                    any_outstanding = false;
                }
            }
        }

        // All senders are dropped... the task is no longer needed
        Ok(().into())
    }
}

// ===== impl Error =====

impl<T> fmt::Display for Error<T>
where
    T: fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Error::Inner(ref why) => fmt::Display::fmt(why, f),
            Error::Closed => f.pad("buffer closed"),
        }
    }
}

impl<T> error::Error for Error<T>
where
    T: error::Error,
{
    fn cause(&self) -> Option<&error::Error> {
        if let Error::Inner(ref why) = *self {
            Some(why)
        } else {
            None
        }
    }

    fn description(&self) -> &str {
        match *self {
            Error::Inner(ref e) => e.description(),
            Error::Closed => "buffer closed",
        }
    }
}

// ===== impl SpawnError =====

impl<T> fmt::Display for SpawnError<T>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "error spawning buffer task: {:?}", self.inner)
    }
}

impl<T> error::Error for SpawnError<T>
where
    T: error::Error,
{
    fn cause(&self) -> Option<&error::Error> {
        Some(&self.inner)
    }

    fn description(&self) -> &str {
        "error spawning buffer task"
    }
}