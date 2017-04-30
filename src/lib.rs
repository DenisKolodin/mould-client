#[macro_use] extern crate log;
#[macro_use] extern crate error_chain;
extern crate serde;
#[macro_use] extern crate serde_derive;
pub extern crate serde_json;
#[macro_use] extern crate futures;
extern crate futures_state_stream;
extern crate tokio_core;
extern crate tokio_io;
extern crate tungstenite;
extern crate tokio_tungstenite;
extern crate url;

use std::{fmt, io, str, result};
use serde::{Serialize, Serializer, Deserialize, Deserializer};
use serde::de::{Visitor};
use serde_json::{Map, Value};
use std::marker::PhantomData;
use url::Url;
use futures::{Future, IntoFuture, Async, AsyncSink, Poll, Stream, Sink, StartSend};
use tokio_io::{AsyncRead, AsyncWrite};
use tungstenite::Message;
use tokio_tungstenite::{client_async, ConnectAsync, WebSocketStream};


error_chain! {
    foreign_links {
        IoError(io::Error);
        EncodingError(str::Utf8Error);
        SerdeError(serde_json::Error);
        AsycWebSocketError(tungstenite::Error);
    }
    errors {
        InteractionFinished {
            description("interaction finished")
        }
        UnexpectedFormat {
            description("unexpected data format")
        }
        UnexpectedKind(s: String) {
            description("unsexpected event")
            display("unexpected event: '{}'", s)
        }
        Interrupted {
            description("connection interrupted")
        }
        ActionRejected(s: String) {
            description("action rejected")
            display("action rejected: '{}'", s)
        }
        ActionFailed(s: String) {
            description("action failed")
            display("action failed: '{}'", s)
        }
        NoDataProvided {
            description("no data provided")
        }
        Other(s: String) {
            description("other error")
            display("other error: '{}'", s)
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Event {
    pub event: EventKind,
    pub data: Option<Value>,
}

impl Event {
    pub fn is_terminated(&self) -> bool {
        use EventKind::*;
        match self.event {
            Done | Fail | Reject => true,
            _ => false,
        }
    }

    pub fn is_ready(&self) -> bool {
        use EventKind::*;
        match self.event {
            Ready => true,
            _ => false,
        }
    }

    pub fn empty(kind: EventKind) -> Self {
        Event {
            event: kind,
            data: None,
        }
    }
}

#[derive(Debug)]
pub enum EventKind {
    Request,
    Ready,
    Item,
    Next,
    Reject,
    Fail,
    Done,
    Cancel,
    Suspended,
}

impl Serialize for EventKind {
    fn serialize<S>(&self, serializer: S) -> result::Result<S::Ok, S::Error>
        where S: Serializer
    {
        let kind = match *self {
            EventKind::Request => "request",
            EventKind::Ready => "ready",
            EventKind::Item => "item",
            EventKind::Next => "next",
            EventKind::Reject => "reject",
            EventKind::Fail => "fail",
            EventKind::Done => "done",
            EventKind::Cancel => "cancel",
            EventKind::Suspended => "suspended",
        };
        serializer.serialize_str(kind)
    }
}

impl<'de> Deserialize<'de> for EventKind {
    fn deserialize<D>(deserializer: D) -> result::Result<EventKind, D::Error>
        where D: Deserializer<'de>
    {
        struct FieldVisitor {
            min: usize,
        };

        impl<'vi> Visitor<'vi> for FieldVisitor {
            type Value = EventKind;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                write!(formatter, "a string containing at least {} bytes", self.min)
            }

            fn visit_str<E>(self, value: &str) -> result::Result<EventKind, E>
                where E: serde::de::Error
            {
                let kind = match value {
                    "request" => EventKind::Request,
                    "ready" => EventKind::Ready,
                    "item" => EventKind::Item,
                    "next" => EventKind::Next,
                    "reject" => EventKind::Reject,
                    "fail" => EventKind::Fail,
                    "done" => EventKind::Done,
                    "cancel" => EventKind::Cancel,
                    "suspended" => EventKind::Suspended,
                    s => {
                        return Err(serde::de::Error::invalid_value(serde::de::Unexpected::Str(s), &self));
                    },
                };
                Ok(kind)
            }
        }
        deserializer.deserialize_str(FieldVisitor { min: 4 })
    }
}


#[derive(Serialize, Deserialize)]
pub struct InteractionRequest {
    pub service: String,
    pub action: String,
    pub payload: Map<String, Value>,
}

#[derive(Serialize, Deserialize)]
pub struct Request {
    pub action: String,
    pub payload: Map<String, Value>,
}

pub fn mould_connect<S: AsyncRead + AsyncWrite>(url: Url, stream: S) -> Connecting<S> {
    Connecting {
        inner: client_async(url, stream),
    }
}

pub struct Connecting<S> {
    inner: ConnectAsync<S>,
}

impl<S: AsyncRead + AsyncWrite> Future for Connecting<S> {
    type Item = MouldTransport<S>;
    type Error = Error;

    fn poll(&mut self) -> Poll<MouldTransport<S>, Error> {
        self.inner.poll().map(|async| {
            async.map(MouldTransport::new)
        })
        .map_err(Error::from)
    }
}

pub struct MouldTransport<S> {
    inner: WebSocketStream<S>,
}

impl<S> MouldTransport<S> {
    fn new(wss: WebSocketStream<S>) -> Self {
        MouldTransport {
            inner: wss,
        }
    }
}

impl<T> Stream for MouldTransport<T> where T: AsyncRead + AsyncWrite {
    type Item = Event;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Event>, Error> {
        match self.inner.poll() {
            Ok(Async::Ready(Some(Message::Text(ref text)))) => {
                let event = serde_json::from_str(text)?;
                Ok(Async::Ready(Some(event)))
            },
            Ok(Async::Ready(None)) => {
                Ok(Async::Ready(None))
            },
            Ok(Async::Ready(Some(_))) => {
                Err(ErrorKind::UnexpectedFormat.into())
            },
            Ok(Async::NotReady) => {
                Ok(Async::NotReady)
            },
            Err(e) => {
                Err(e.into())
            },
        }
    }
}

impl<T> Sink for MouldTransport<T> where T: AsyncRead + AsyncWrite {
    type SinkItem = Event;
    type SinkError = Error;

    fn start_send(&mut self, event: Event) -> StartSend<Event, Error> {
        let text = serde_json::to_string(&event)?;
        let message = Message::Text(text);
        self.inner.start_send(message)?; // Put to a send queue
        Ok(AsyncSink::Ready)
    }

    fn poll_complete(&mut self) -> Poll<(), Error> {
        self.inner.poll_complete().map_err(|e| e.into())
    }
}

pub trait MouldStream {

    fn do_interaction<T, F, I, R, O>(self, request: InteractionRequest, init: T, f: F) -> DoInteraction<T, F, I, R, O, Self>
        where R: IntoFuture<Item=(T, Option<O>)>, Self: Sized,
              F: FnMut((T, I)) -> R, Self: Sized,
    {
        DoInteraction::new(self, init, request, f)
    }

}

impl<S> MouldStream for S
    where S: Sized + Stream<Item=Event, Error=Error> + Sink<SinkItem=Event, SinkError=Error>,
{
}

pub struct DoInteraction<T, F, I, R, O, S>
    where R: IntoFuture
{
    fold: Option<T>,
    request: Option<InteractionRequest>,
    need_next: bool,
    is_done: bool,
    stream: Option<S>,
    f: F,
    pending: Option<R::Future>,
    input: PhantomData<I>,
    output: PhantomData<O>,
}

impl<T, F, I, R, O, S> DoInteraction<T, F, I, R, O, S>
    where R: IntoFuture
{
    pub fn new(s: S, init: T, i: InteractionRequest, f: F) -> Self {
        DoInteraction {
            fold: Some(init),
            request: Some(i),
            need_next: true,
            is_done: false,
            stream: Some(s),
            f: f,
            pending: None,
            input: PhantomData,
            output: PhantomData,
        }
    }
}

impl<T, F, I, R, O, S> Future for DoInteraction<T, F, I, R, O, S>
    where S: Stream<Item=Event, Error=Error> + Sink<SinkItem=Event, SinkError=Error>,
          F: FnMut((T, I)) -> R, Self: Sized,
          R: IntoFuture<Item=(T, Option<O>), Error=S::Error>,
          for<'de> I: Deserialize<'de>,
          O: Serialize,
{
    type Item = (T, S);
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if let Some(request) = self.request.take() {
            match serde_json::to_value(request) {
                Ok(request) => {
                    let stream = self.stream.as_mut().expect("polling StartInteraction twice");
                    let event = Event {
                        event: EventKind::Request,
                        data: Some(request),
                    };
                    stream.start_send(event)?;
                },
                Err(err) => {
                    return Err(err.into());
                },
            }
        }
        let res = self.pending.as_mut().map(|fut| fut.poll());
        match res {
            Some(Ok(Async::Ready((fold, value)))) => {
                if self.is_done {
                    let stream = self.stream.take().unwrap();
                    let fold = self.fold.take().unwrap();
                    return Ok(Async::Ready((fold, stream)));
                } else {
                    let value = serde_json::to_value(value)?;
                    let event = Event {
                        event: EventKind::Next,
                        data: Some(value),
                    };
                    let sink = self.stream.as_mut().expect("polling DoInteraction twice");
                    sink.start_send(event)?;
                    self.fold = Some(fold);
                    self.pending = None;
                    // No need to send `cancel`, because impossible
                }
            },
            None | Some(Ok(Async::NotReady)) => {
            },
            Some(Err(_)) => {
                // TODO Send cancel
                return Err(ErrorKind::Interrupted.into());
            },
        }
        loop {
            let item = self.stream.as_mut().expect("polling DoInteraction twice").poll();
            match try_ready!(item) {
                Some(Event { event, data }) => {
                    match event {
                        EventKind::Item => {
                            if let Some(data) = data {
                                let res = serde_json::from_value(data);
                                if let Ok(value) = res {
                                    if let Some(fold) = self.fold.take() {
                                        let fut = (self.f)((fold, value)).into_future();
                                        self.pending = Some(fut);
                                    }
                                } else {
                                    // TODO Send `cancel` event
                                    return Err(ErrorKind::UnexpectedFormat.into());
                                }
                            } else {
                                // TODO Send `cancel` event
                                return Err(ErrorKind::NoDataProvided.into());
                            }
                        },
                        EventKind::Ready => {
                            if self.need_next {
                                let stream = self.stream.as_mut().expect("polling StartInteraction twice");
                                let event = Event {
                                    event: EventKind::Next,
                                    data: None,
                                };
                                stream.start_send(event)?;
                                self.need_next = false;
                            } else {
                                let res = self.pending.as_mut().map(|fut| fut.poll());
                                match res {
                                    Some(Ok(Async::Ready((fold, value)))) => {
                                        let value = serde_json::to_value(value)?;
                                        let event = Event {
                                            event: EventKind::Next,
                                            data: Some(value),
                                        };
                                        let sink = self.stream.as_mut().expect("polling DoInteraction twice");
                                        sink.start_send(event)?;
                                        self.fold = Some(fold);
                                        self.pending = None;
                                        // No need to send `cancel`, because impossible
                                    },
                                    None | Some(Ok(Async::NotReady)) => {
                                    },
                                    Some(Err(_)) => {
                                        // TODO Send cancel
                                        return Err(ErrorKind::Interrupted.into());
                                    },
                                }
                            }
                        },
                        EventKind::Reject => {
                            let reason = data.as_ref().and_then(Value::as_str).unwrap_or("<no reject reason>");
                            return Err(ErrorKind::ActionRejected(reason.into()).into());
                        },
                        EventKind::Fail => {
                            let reason = data.as_ref().and_then(Value::as_str).unwrap_or("<no fail reason>");
                            return Err(ErrorKind::ActionFailed(reason.into()).into());
                        },
                        EventKind::Done => {
                            self.is_done = true;
                            let res = self.pending.as_mut().map(|fut| {
                                fut.poll()
                            });
                            match res {
                                Some(Ok(Async::Ready((fold, _)))) => {
                                    let stream = self.stream.take().unwrap();
                                    return Ok(Async::Ready((fold, stream)));
                                },
                                Some(Ok(Async::NotReady)) => {
                                    // Ignore...
                                },
                                Some(Err(err)) => {
                                    return Err(err);
                                },
                                None => {
                                    let stream = self.stream.take().unwrap();
                                    let fold = self.fold.take().unwrap();
                                    return Ok(Async::Ready((fold, stream)));
                                },
                            }
                        },
                        kind => {
                            // TODO Send `cancel` event
                            return Err(ErrorKind::UnexpectedKind(format!("{:?}", kind)).into());
                        },
                    }
                },
                None => {
                    let stream = self.stream.take().unwrap();
                    let fold = self.fold.take().unwrap();
                    return Ok(Async::Ready((fold, stream)));
                },
            }
        }
    }
}

