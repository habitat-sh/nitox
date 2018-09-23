use bytes::Bytes;

use futures::{
    future::{self, Either},
    prelude::*,
    stream,
    sync::mpsc,
    task, Future,
};
use std::{
    collections::HashMap,
    net::SocketAddr,
    str::FromStr,
    sync::{Arc, RwLock},
};
use tokio_executor;
use url::Url;

use error::NatsError;
use net::{
    connect::*,
    reconnect::{Reconnect, ReconnectError},
};
use protocol::{commands::*, CommandError, Op};

type NatsSink = stream::SplitSink<NatsConnection>;
type NatsStream = stream::SplitStream<NatsConnection>;
type NatsSubscriptionId = String;

#[derive(Clone, Debug)]
struct NatsClientSender {
    tx: mpsc::UnboundedSender<Op>,
    verbose: bool,
}

impl NatsClientSender {
    pub fn new(sink: NatsSink) -> Self {
        let (tx, rx) = mpsc::unbounded();
        let rx = rx.map_err(|_| NatsError::InnerBrokenChain);
        let work = sink.send_all(rx).map(|_| ()).map_err(|_| ());
        tokio_executor::spawn(work);

        NatsClientSender { tx, verbose: false }
    }

    #[allow(dead_code)]
    pub fn set_verbose(&mut self, verbose: bool) {
        self.verbose = verbose;
    }

    pub fn send(&self, op: Op) -> impl Future<Item = (), Error = NatsError> {
        let _verbose = self.verbose.clone();
        let fut = self
            .tx
            .unbounded_send(op)
            .map_err(|_| NatsError::InnerBrokenChain)
            .into_future();

        fut
    }
}

#[derive(Debug)]
struct NatsClientMultiplexer {
    other_tx: Arc<mpsc::UnboundedSender<Op>>,
    subs_tx: Arc<RwLock<HashMap<NatsSubscriptionId, mpsc::UnboundedSender<Message>>>>,
}

impl NatsClientMultiplexer {
    pub fn new(stream: NatsStream) -> (Self, mpsc::UnboundedReceiver<Op>) {
        let subs_tx: Arc<RwLock<HashMap<NatsSubscriptionId, mpsc::UnboundedSender<Message>>>> =
            Arc::new(RwLock::new(HashMap::default()));

        let (other_tx, other_rx) = mpsc::unbounded();
        let other_tx = Arc::new(other_tx);

        let stx_inner = Arc::clone(&subs_tx);
        let otx_inner = Arc::clone(&other_tx);

        // Here we filter the incoming TCP stream Messages by subscription ID and sending it to the appropriate Sender
        let work_tx = stream
            .for_each(move |op| {
                let hwnd = task::current();
                println!("received op from raw stream {:?}", op);
                match &op {
                    Op::MSG(msg) => {
                        if let Ok(stx) = stx_inner.read() {
                            if let Some(tx) = stx.get(&msg.sid) {
                                let _ = tx.unbounded_send(msg.clone());
                            }
                        }
                    }
                    // Forward the rest of the messages to the owning client
                    op => {
                        let _ = otx_inner.unbounded_send(op.clone());
                    }
                }

                hwnd.notify();

                future::ok::<(), NatsError>(())
            }).map(|_| ())
            .map_err(|_| ());

        tokio_executor::spawn(work_tx);

        (NatsClientMultiplexer { subs_tx, other_tx }, other_rx)
    }

    pub fn for_sid(&self, sid: NatsSubscriptionId) -> impl Stream<Item = Message, Error = NatsError> {
        let (tx, rx) = mpsc::unbounded();
        if let Ok(mut subs) = self.subs_tx.write() {
            subs.insert(sid.clone(), tx);
        }

        rx.map_err(|_| NatsError::InnerBrokenChain)
    }

    pub fn remove_sid(&self, sid: NatsSubscriptionId) {
        if let Ok(mut subs) = self.subs_tx.write() {
            subs.remove(&sid);
        }
    }
}

#[derive(Debug, Default, Clone, Builder)]
#[builder(setter(into))]
pub struct NatsClientOptions {
    connect_command: ConnectCommand,
    cluster_uri: String,
}

#[derive(Debug)]
pub struct NatsClient {
    opts: NatsClientOptions,
    other_rx: mpsc::UnboundedReceiver<Op>,
    tx: NatsClientSender,
    rx: Arc<NatsClientMultiplexer>,
}

/*impl Stream for NatsClient {
    type Item = Op;
    type Error = NatsError;
    fn poll(&mut self) -> Result<Async<Option<Self::Item>>, Self::Error> {
        self.other_rx.poll().map_err(|_| NatsError::InnerBrokenChain)
    }
}*/

impl NatsClient {
    pub fn from_options(opts: NatsClientOptions) -> impl Future<Item = Self, Error = NatsError> {
        let cluster_uri = opts.cluster_uri.clone();
        let tls_required = opts.connect_command.tls_required.clone();

        future::result(SocketAddr::from_str(&cluster_uri))
            .from_err()
            .and_then(move |cluster_sa| {
                if tls_required {
                    match Url::parse(&cluster_uri) {
                        Ok(url) => match url.host_str() {
                            Some(host) => future::ok(Either::B(connect_tls(host.to_string(), &cluster_sa))),
                            None => future::err(NatsError::TlsHostMissingError),
                        },
                        Err(e) => future::err(e.into()),
                    }
                } else {
                    future::ok(Either::A(connect(&cluster_sa)))
                }
            }).and_then(|either| either)
            .and_then(move |connection| {
                let (sink, stream): (NatsSink, NatsStream) = connection.split();
                let (rx, other_rx) = NatsClientMultiplexer::new(stream);
                let tx = NatsClientSender::new(sink);

                let client = NatsClient {
                    tx,
                    other_rx,
                    rx: Arc::new(rx),
                    opts,
                };

                future::ok(client)
            })
    }

    pub fn connect(self) -> impl Future<Item = Self, Error = NatsError> {
        self.tx
            .send(Op::CONNECT(self.opts.connect_command.clone()))
            .into_future()
            .and_then(move |_| future::ok(self))
    }

    pub fn publish(&self, cmd: PubCommand) -> impl Future<Item = (), Error = NatsError> {
        self.tx.send(Op::PUB(cmd)).map(|r| r).into_future()
    }

    pub fn unsubscribe(&self, cmd: UnsubCommand) -> impl Future<Item = (), Error = NatsError> {
        self.tx.send(Op::UNSUB(cmd)).map(|r| r).into_future()
    }

    pub fn subscribe(&self, cmd: SubCommand) -> impl Future<Item = impl Stream<Item = Message, Error = NatsError>> {
        let inner_rx = self.rx.clone();
        self.tx
            .send(Op::SUB(cmd.clone()))
            .and_then(move |_| future::ok(inner_rx.for_sid(cmd.sid)))
    }

    pub fn request(&self, subject: String, payload: Bytes) -> impl Future<Item = Message, Error = NatsError> {
        let inbox = PubCommandBuilder::generate_reply_to();
        let pub_cmd = PubCommand {
            subject,
            payload,
            reply_to: Some(inbox.clone()),
        };

        let sub_cmd = SubCommand {
            queue_group: None,
            sid: SubCommandBuilder::generate_sid(),
            subject: inbox,
        };

        let sid = sub_cmd.sid.clone();

        let unsub_cmd = UnsubCommand {
            sid: sub_cmd.sid.clone(),
            max_msgs: Some(1),
        };

        let tx1 = self.tx.clone();
        let tx2 = self.tx.clone();
        let rx = Arc::clone(&self.rx);
        self.tx
            .send(Op::SUB(sub_cmd))
            .and_then(move |_| tx1.send(Op::UNSUB(unsub_cmd)))
            .and_then(move |_| tx2.send(Op::PUB(pub_cmd)))
            .and_then(move |_| {
                rx.for_sid(sid)
                    .take(1)
                    .into_future()
                    .map(|(maybe_message, _)| maybe_message.unwrap())
                    .map_err(|_| NatsError::InnerBrokenChain)
            })
    }
}

#[cfg(test)]
mod client_test {
    use super::*;
    use futures::sync::oneshot;
    use futures::{prelude::*, stream, Future, Sink, Stream};
    use tokio;

    fn run_and_wait<R, E, F>(f: F) -> Result<R, E>
    where
        F: Future<Item = R, Error = E> + Send + 'static,
        R: ::std::fmt::Debug + Send + 'static,
        E: ::std::fmt::Debug + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let mut runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.spawn(f.then(|r| tx.send(r).map_err(|_| panic!("Cannot send Result"))));
        let result = rx.wait().expect("Cannot wait for a result");
        let _ = runtime.shutdown_now().wait();
        result
    }

    #[test]
    fn can_connect_raw() {
        let connect_cmd = ConnectCommandBuilder::default().build().unwrap();
        let options = NatsClientOptionsBuilder::default()
            .connect_command(connect_cmd)
            .cluster_uri("127.0.0.1:4222")
            .build()
            .unwrap();

        let connection = NatsClient::from_options(options);
        let connection_result = run_and_wait(connection);
        assert!(connection_result.is_ok());
    }

    #[test]
    fn can_connect() {
        let connect_cmd = ConnectCommandBuilder::default().build().unwrap();
        let options = NatsClientOptionsBuilder::default()
            .connect_command(connect_cmd)
            .cluster_uri("127.0.0.1:4222")
            .build()
            .unwrap();

        let connection = NatsClient::from_options(options).and_then(|client| client.connect());
        let connection_result = run_and_wait(connection);
        assert!(connection_result.is_ok());
    }

    /*#[test]
    fn can_sub_and_pub() {
        let connect_cmd = ConnectCommandBuilder::default().build().unwrap();
        let options = NatsClientOptionsBuilder::default()
            .connect_command(connect_cmd)
            .cluster_uri("127.0.0.1:4222")
            .build()
            .unwrap();

        let fut = NatsClient::from_options(options)
            .and_then(|client| client.connect())
            .and_then(|client| {
                client
                    .subscribe(SubCommandBuilder::default().subject("foo").build().unwrap())
                    .map_err(|_| NatsError::InnerBrokenChain)
                    .and_then(move |stream| {
                        let _ = client
                            .publish(
                                PubCommandBuilder::default()
                                    .subject("foo")
                                    .payload("bar")
                                    .build()
                                    .unwrap(),
                            ).wait();

                        stream
                            .inspect(|msg| println!("{:?}", msg))
                            .take(1)
                            .into_future()
                            .map(|(maybe_message, _)| maybe_message.unwrap())
                            .map_err(|_| NatsError::InnerBrokenChain)
                    })
            });

        let connection_result = run_and_wait(fut);
        assert!(connection_result.is_ok());
        let msg = connection_result.unwrap();
        assert_eq!(msg.payload, "bar");
    }*/

    #[test]
    fn can_request() {
        let connect_cmd = ConnectCommandBuilder::default().build().unwrap();
        let options = NatsClientOptionsBuilder::default()
            .connect_command(connect_cmd)
            .cluster_uri("127.0.0.1:4222")
            .build()
            .unwrap();

        let fut = NatsClient::from_options(options)
            .and_then(|client| client.connect())
            .and_then(|client| client.request("foo2".into(), "bar".into()));

        let connection_result = run_and_wait(fut);
        assert!(connection_result.is_ok());
        let msg = connection_result.unwrap();
        assert_eq!(msg.payload, "bar");
    }
}