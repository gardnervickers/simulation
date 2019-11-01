use futures::{channel::mpsc, Future, SinkExt};
use std::{cmp, collections::{self, hash_map::Entry}, hash, io, net};
use super::{ListenerState, Listener, socket, FaultyTcpStream, SocketHalf};

#[derive(Debug)]
pub(crate) struct Inner {
    handle: crate::deterministic::DeterministicRuntimeHandle,
    connections: collections::HashSet<Connection>,
    endpoints: collections::HashMap<net::SocketAddr, ListenerState>,
}


impl Inner {
    pub(crate) fn new(handle: crate::deterministic::DeterministicRuntimeHandle) -> Self {
        Inner {
            handle,
            connections: collections::HashSet::new(),
            endpoints: collections::HashMap::new(),
        }
    }
    fn register_new_connection_pair(
        &mut self,
        source: net::SocketAddr,
        dest: net::SocketAddr,
    ) -> Result<(FaultyTcpStream<SocketHalf>, FaultyTcpStream<SocketHalf>), io::Error> {
        let (client, server) = socket::new_socket_pair(source, dest);
        let (client, client_fault_handle) =
            socket::FaultyTcpStream::wrap(self.handle.clone(), client);
        let (server, server_fault_handle) =
            socket::FaultyTcpStream::wrap(self.handle.clone(), server);
        let connection = Connection {
            source,
            dest,
            client_fault_handle,
            server_fault_handle,
        };
        if self.connections.contains(&connection) {
            return Err(io::ErrorKind::AddrInUse.into());
        }
        self.connections.insert(connection);
        Ok((client, server))
    }
    // find an unused socket port for the provided ipaddr.
    fn unused_socket_port(&self, addr: net::IpAddr) -> u16 {
        let mut start = 65535;
        let occupied: collections::HashSet<u16> = self
            .connections
            .iter()
            .filter(|v| v.source.ip() == addr)
            .map(|v| v.source.port())
            .collect();
        loop {
            if !occupied.contains(&start) {
                return start;
            }
            if start == 0 {}
            start -= 1;
        }
    }

    fn gc_dropped(&mut self) {
        let mut connections = collections::HashSet::new();
        for connection in self.connections.iter() {
            if !connection.client_fault_handle.is_dropped()
                || !connection.server_fault_handle.is_dropped()
            {
                connections.insert(connection.clone());
            }
        }
        self.connections = connections;
    }

    pub fn connect(
        &mut self,
        source: net::IpAddr,
        dest: net::SocketAddr,
    ) -> impl Future<Output = Result<socket::FaultyTcpStream<SocketHalf>, io::Error>> {
        self.gc_dropped();
        let free_socket_port = self.unused_socket_port(source);
        let source_addr = net::SocketAddr::new(source, free_socket_port);
        let registration = self.register_new_connection_pair(source_addr, dest);

        let mut channel;
        match self.endpoints.entry(dest) {
            Entry::Vacant(v) => {
                let (tx, rx) = mpsc::channel(1);
                let state = ListenerState::Unbound {
                    tx: tx.clone(), rx,
                };
                channel = tx;
                v.insert(state);
            },
            Entry::Occupied(o) => {
                match o.get() {
                    ListenerState::Bound{tx} => channel = tx.clone(),
                    ListenerState::Unbound{tx, ..} => channel = tx.clone()
                }
            },
        }

        async move {
            let (client, server) = registration?;
            match channel.send(server).await {
                Ok(_) => Ok(client),
                Err(_) => Err(io::ErrorKind::ConnectionRefused.into()),
            }
        }
    }

    pub fn listen(&mut self, bind_addr: net::SocketAddr) -> Result<Listener, io::Error> {
        self.gc_dropped();
        match self.endpoints.remove(&bind_addr) {
            Some(listener_state) => {
                if let ListenerState::Unbound { tx, rx } = listener_state {
                    let listener = Listener::new(bind_addr, rx);
                    let new_state = ListenerState::Bound { tx };
                    self.endpoints.insert(bind_addr, new_state);
                    Ok(listener)
                } else {
                    self.endpoints.insert(bind_addr, listener_state);
                    Err(io::ErrorKind::AddrInUse.into())
                }
            },
            _ => {
                    let (tx, rx) = mpsc::channel(1);
                    let state = ListenerState::Bound{tx};
                    self.endpoints.insert(bind_addr, state);
                    let listener = Listener::new(bind_addr, rx);
                    Ok(listener)
                }
        }
    }
}
#[derive(Debug, Clone)]
struct Connection {
    source: net::SocketAddr,
    dest: net::SocketAddr,
    client_fault_handle: socket::FaultyTcpStreamHandle,
    server_fault_handle: socket::FaultyTcpStreamHandle,
}
impl cmp::PartialEq for Connection {
    fn eq(&self, other: &Connection) -> bool {
        self.source.eq(&other.source) && self.dest.eq(&other.dest)
    }
}

impl cmp::Eq for Connection {}

impl hash::Hash for Connection {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        self.source.hash(state);
        self.dest.hash(state);
    }
}
