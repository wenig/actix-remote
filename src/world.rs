use std::{io, net};
use std::any::Any;
use std::sync::Arc;
use std::time::Duration;
use std::collections::{HashMap, HashSet};

use actix::prelude::*;
use actix::actors::signal;
use futures::Future;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio_core::net::{TcpStream, TcpListener};
use tokio_core::reactor::Timeout;

use msgs;
use utils;
use worker::NetworkWorker;
use node::{NetworkNode, NodeInformation};
use remote::{Remote, RemoteMessage};
use recipient::{Provider, RecipientProxy,
                RecipientProxySender, RemoteMessageHandler};


struct Proxy {
    addr: Box<Any>,
    service: Recipient<Unsync, msgs::TypeSupported>,
}

pub struct World {
    addr: String,
    addrs: HashMap<String, NodeInformation>,
    nodes: HashMap<String, Addr<Unsync, NetworkNode>>,
    types: HashMap<String, HashSet<String>>,
    sockets: HashMap<net::SocketAddr, net::TcpListener>,
    wid: usize,
    workers: HashMap<usize, Addr<Unsync, NetworkWorker<TcpStream>>>,
    handlers: HashMap<&'static str, Arc<RemoteMessageHandler>>,
    recipients: HashMap<&'static str, Proxy>,
    exit: bool,
}

impl Actor for World {
    type Context = Context<Self>;
}

impl World {
    pub fn new(addr: String) -> io::Result<World> {
        let net = World{addr: addr.clone(),
                        addrs: HashMap::new(),
                        nodes: HashMap::new(),
                        types: HashMap::new(),
                        sockets: HashMap::new(),
                        wid: 0,
                        workers: HashMap::new(),
                        handlers: HashMap::new(),
                        recipients: HashMap::new(),
                        exit: false};
        Ok(net.bind(addr)?)
    }

    /// The socket address to bind
    ///
    /// To bind multiple addresses this method can be call multiple times.
    pub fn bind<S: net::ToSocketAddrs>(mut self, addr: S) -> io::Result<Self> {
        let mut err = None;
        let mut succ = false;
        for addr in addr.to_socket_addrs()? {
            match utils::tcp_listener(addr, 256) {
                Ok(lst) => {
                    succ = true;
                    self.sockets.insert(lst.local_addr().unwrap(), lst);
                },
                Err(e) => err = Some(e),
            }
        }

        if !succ {
            if let Some(e) = err.take() {
                Err(e)
            } else {
                Err(io::Error::new(io::ErrorKind::Other, "Can not bind to address."))
            }
        } else {
            Ok(self)
        }
    }

    /// Register network node
    pub fn add_node<S: Into<String>>(mut self, addr: Option<S>) -> Self {
        addr.map(|addr| {
            let addr = addr.into();
            self.addrs.insert(addr.clone(), NodeInformation::new(addr));
        });
        self
    }

    /// Create remote recipient for specific message type
    pub fn get_recipient<M>(&mut self) -> Recipient<Remote, M>
        where M: RemoteMessage + 'static,
              M::Result: Send + Serialize + DeserializeOwned
    {
        if let Some(info) = self.recipients.get(M::type_id()) {
            if let Some(&(_, ref saddr)) = info.addr.downcast_ref
                ::<(Addr<Unsync, RecipientProxy<M>>, Addr<Syn, RecipientProxy<M>>)>()
            {
                return Recipient::new(RecipientProxySender::new(saddr.clone()))
            }
        }

        let (addr, saddr): (Addr<Unsync, RecipientProxy<M>>,
                            Addr<Syn, RecipientProxy<M>>) = RecipientProxy::new().start();
        self.recipients.insert(
            M::type_id(), Proxy{addr: Box::new(addr.clone()),
                                service: addr.clone().recipient()});

        return Recipient::new(RecipientProxySender::new(saddr))
    }

    /// Register remote recipient provider.
    ///
    /// Announce recipient availability to all connected nodes.
    pub fn register_recipient<M>(world: &Addr<Syn, World>, recipient: Recipient<Syn, M>)
        where M: RemoteMessage + 'static, M::Result: Send + Serialize + DeserializeOwned
    {
        let r = Provider{recipient: recipient};
        world.do_send(msgs::ProvideRecipient{
            type_id: M::type_id(), handler: Arc::new(r)})
    }

    fn stop(&mut self, ctx: &mut Context<Self>) {
        if !self.exit {
            self.exit = true;

            if self.workers.is_empty() {
                self.stop_system_with_delay();
            } else {
                for (wid, worker) in &self.workers {
                    let id: usize = *wid;
                    worker.send(msgs::StopWorker).into_actor(self)
                        .then(move |_, slf, ctx| {
                            slf.workers.remove(&id);
                            if slf.workers.is_empty() {
                                ctx.stop();
                                slf.stop_system_with_delay();
                            }
                            actix::fut::ok(())
                        }).spawn(ctx);
                }
            }
        }
    }

    fn stop_system_with_delay(&self) {
        Arbiter::handle().spawn(
            Timeout::new(Duration::from_secs(1), Arbiter::handle()).unwrap()
                .then(|_| {
                    Arbiter::system().do_send(actix::msgs::SystemExit(0));
                    Ok(())
                }));
    }

    /// Create network nodes, and start listening for incoming connections
    pub fn start(mut self) -> Addr<Syn, Self> {
        let addrs: Vec<(net::SocketAddr, net::TcpListener)> =
            self.sockets.drain().collect();

        // start network
        Actor::create(move |ctx| {
            let h = Arbiter::handle();

            // subscribe to signals
            signal::ProcessSignals::from_registry().do_send(
                signal::Subscribe(ctx.address::<Addr<_, _>>().recipient()));

            // start workers
            for (addr, sock) in addrs {
                info!("Starting actix remote server on {}", addr);
                let lst = TcpListener::from_listener(sock, &addr, h)
                    .unwrap();
                ctx.add_stream(lst.incoming());
            }

            for info in self.addrs.values() {
                let net = ctx.address();
                let info2 = info.clone();
                let addr2 = self.addr.clone();
                let node: Addr<Unsync, _> =
                    Supervisor::start(move |_| NetworkNode::new(addr2, net, info2));
                self.nodes.insert(info.address().to_string(), node);
            }

            self
        })
    }
}

/// Register remote message recipient
impl Handler<msgs::ProvideRecipient> for World {
    type Result = ();

    fn handle(&mut self, msg: msgs::ProvideRecipient, _: &mut Self::Context) {
        // notify all workers
        for addr in self.workers.values() {
            addr.do_send(msg.clone());
        }

        self.handlers.insert(msg.type_id, msg.handler);
    }
}

/// New client connection, create new downstream connection or re-connect existing
impl StreamHandler<(TcpStream, net::SocketAddr), io::Error> for World
{
    fn handle(&mut self, msg: (TcpStream, net::SocketAddr), ctx: &mut Context<Self>) {
        self.wid += 1;
        let addr = NetworkWorker::start(
            self.wid, msg.0, self.handlers.clone(), ctx.address());
        self.workers.insert(self.wid, addr);
    }
}

/// Worker disconnected notification
impl Handler<msgs::WorkerDisconnected> for World {
    type Result = ();

    fn handle(&mut self, msg: msgs::WorkerDisconnected, _: &mut Self::Context) {
        self.workers.remove(&msg.0);
    }
}

/// Connected to remote node
impl Handler<msgs::NodeConnected> for World {
    type Result = ();

    fn handle(&mut self, msg: msgs::NodeConnected, ctx: &mut Context<Self>) {
        if let Some(node) = self.nodes.get(&msg.0) {
            node.do_send(msgs::ReconnectNode);
            return
        }

        let addr = msg.0.clone();
        let naddr = self.addr.clone();
        let net = ctx.address();
        let info = NodeInformation::new(msg.0.clone());
        let node: Addr<Unsync, _> =
            Supervisor::start(move |_| NetworkNode::new(naddr, net, info));
        self.nodes.insert(addr, node);
    }
}

/// Handle NodeSupportedTypes message
///
/// Node notifies about supported remote types
impl Handler<msgs::NodeSupportedTypes> for World {
    type Result = ();

    fn handle(&mut self, msg: msgs::NodeSupportedTypes, _: &mut Context<Self>) {
        // register in internal registry
        for tp in &msg.types {
            if !self.types.contains_key(tp) {
                self.types.insert(tp.clone(), HashSet::new());
            }
            self.types.get_mut(tp).unwrap().insert(msg.node.clone());
        }

        // notify all recipient proxies
        if let Some(node) = self.nodes.get(&msg.node) {
            for tp in msg.types {
                if let Some(proxy) = self.recipients.get(tp.as_str()) {
                    let _ = proxy.service.do_send(
                        msgs::TypeSupported {
                            type_id: tp,
                            node_id: msg.node.clone(),
                            node: node.clone(),
                        });
                }
            }
        }
    }
}

/// Signals support
/// Handle `SIGINT`, `SIGTERM`, `SIGQUIT` signals and send `SystemExit(0)`
/// message to `System` actor.
impl Handler<signal::Signal> for World {
    type Result = ();

    fn handle(&mut self, msg: signal::Signal, ctx: &mut Context<Self>) {
        match msg.0 {
            signal::SignalType::Int => {
                info!("SIGINT received, exiting");
                self.stop(ctx);
            }
            signal::SignalType::Term => {
                info!("SIGTERM received, stopping");
                self.stop(ctx);
            }
            signal::SignalType::Quit => {
                info!("SIGQUIT received, exiting");
                self.stop(ctx);
            }
            _ => (),
        }
    }
}
