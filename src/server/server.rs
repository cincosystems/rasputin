extern crate bytes;
use std::io::{Error, ErrorKind};
use std::io;
use std::thread;

use mio::{EventLoop, EventSet, PollOpt, Handler, Token, TryWrite, TryRead};
use mio::tcp::{TcpListener, TcpStream};
use mio::util::Slab;
use self::bytes::{Buf, ByteBuf, MutByteBuf, SliceBuf};

const SERVER_CLIENTS: Token = Token(0);
const SERVER_PEERS: Token = Token(1);

pub struct Server {
    peers: Vec<String>,
    cli_handler: ConnSet,
    peer_handler: ConnSet,
}

impl Server {
    pub fn new(
        peer_port: u16,
        cli_port: u16,
        peers: Vec<String>,
    ) -> Result<Server, Error> {

        let cli_addr =
            format!("0.0.0.0:{}", cli_port).parse().unwrap();
        let cli_srv_sock =
            try!(TcpListener::bind(&cli_addr));

        let peer_addr =
            format!("0.0.0.0:{}", peer_port).parse().unwrap();
        let peer_srv_sock =
            try!(TcpListener::bind(&peer_addr));

        Ok(Server {
            peers: peers,
            cli_handler: ConnSet {
                srv_sock: cli_srv_sock,
                srv_token: SERVER_CLIENTS,
                conns: Slab::new_starting_at(Token(1024), 4096),
            },
            peer_handler: ConnSet {
                srv_sock: peer_srv_sock,
                srv_token: SERVER_PEERS,
                conns: Slab::new_starting_at(Token(2), 15),
            },
        })
    }

    pub fn start(&mut self) -> Result<(), String> {

        let mut event_loop = EventLoop::new().unwrap();

        event_loop.register_opt(
            &self.cli_handler.srv_sock,
            SERVER_CLIENTS,
            EventSet::readable(),
            PollOpt::edge() | PollOpt::oneshot(),
        ).unwrap();

        event_loop.register_opt(
            &self.peer_handler.srv_sock,
            SERVER_PEERS,
            EventSet::readable(),
            PollOpt::edge() | PollOpt::oneshot(),
        ).unwrap();

        event_loop.run(self).unwrap();

        Err("event_loop should not have returned.".to_string())
    }
}

impl Handler for Server {
    type Timeout = ();
    type Message = ();

    fn ready(
        &mut self,
        event_loop: &mut EventLoop<Server>,
        token: Token,
        events: EventSet,
    ) {
       if events.is_readable() {
            match token {
                SERVER_PEERS => {
                    info!("got SERVER_PEERS accept");
                    self.peer_handler.accept(event_loop).or_else( |e| {
                        error!("failed to accept peer: all slots full");
                        Err(e)
                    });
                },
                SERVER_CLIENTS => {
                    info!("got SERVER_CLIENTS accept");
                    self.cli_handler.accept(event_loop).or_else( |e| {
                        error!("failed to accept client: all slots full");
                        Err(e)
                    });
                },
                peer if peer.as_usize() >= 2 && peer.as_usize() <= 16 => {
                    self.peer_handler.conn_readable(event_loop, peer).unwrap();
                },
                cli if cli.as_usize() >= 1024 && cli.as_usize() <= 4096 => {
                    self.cli_handler.conn_readable(event_loop, cli).unwrap();
                },
                t => panic!("unknown token: {}", t.as_usize()),
            }
        }

        if events.is_writable() {
            match token {
                SERVER_PEERS =>
                    panic!("received writable for SERVER_PEERS"),
                SERVER_CLIENTS =>
                    panic!("received writable for token SERVER_CLIENTS"),
                peer if peer.as_usize() > 1 && peer.as_usize() <= 128 =>
                    self.peer_handler.conn_writable(event_loop, peer),
                cli if cli.as_usize() > 128 && cli.as_usize() <= 4096 =>
                    self.cli_handler.conn_writable(event_loop, cli),
                t => panic!("received writable for out-of-range token: {}", t.as_usize()),
            };
        }
    }
}

struct ServerConn {
    sock: TcpStream,
    buf: Option<ByteBuf>,
    mut_buf: Option<MutByteBuf>,
    token: Option<Token>,
    interest: EventSet
}

impl ServerConn {
    fn new(sock: TcpStream) -> ServerConn {
        ServerConn {
            sock: sock,
            buf: None,
            mut_buf: Some(ByteBuf::mut_with_capacity(2048)),
            token: None,
            interest: EventSet::hup()
        }
    }

    fn writable(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
        let mut buf = self.buf.take().unwrap();

        match self.sock.try_write_buf(&mut buf) {
            Ok(None) => {
                info!("client flushing buf; WOULDBLOCK");

                self.buf = Some(buf);
                self.interest.insert(EventSet::writable());
            }
            Ok(Some(r)) => {
                info!("CONN : we wrote {} bytes!", r);

                self.mut_buf = Some(buf.flip());

                self.interest.insert(EventSet::readable());
                self.interest.remove(EventSet::writable());
            }
            Err(e) => {
                match e.raw_os_error() {
                    Some(32) => {
                        info!("client disconnected");
                    },
                    Some(e) =>
                        info!("not implemented; client os err={:?}", e),
                    _ =>
                        info!("not implemented; client err={:?}", e),
                };
                // Don't reregister.
                return Err(e);
            },
        }

        event_loop.reregister(
            &self.sock,
            self.token.unwrap(),
            self.interest,
            PollOpt::edge() | PollOpt::oneshot(),
        )
    }

    fn readable(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
        let mut buf = self.mut_buf.take().unwrap();

        match self.sock.try_read_buf(&mut buf) {
            Ok(None) => {
                panic!("We just got readable, but were unable to read from the socket?");
            }
            Ok(Some(r)) => {
                info!("CONN : we read {} bytes!", r);
                self.interest.remove(EventSet::readable());
                self.interest.insert(EventSet::writable());
            }
            Err(e) => {
                info!("not implemented; client err={:?}", e);
                self.interest.remove(EventSet::readable());
            }

        };

        // prepare to provide this to writable
        self.buf = Some(buf.flip());
        event_loop.reregister(
            &self.sock,
            self.token.unwrap(),
            self.interest,
            PollOpt::edge(),
        )
    }
}

pub struct ConnSet {
    srv_sock: TcpListener,
    srv_token: Token,
    conns: Slab<ServerConn>,
}

impl ConnSet {
    fn accept(
        &mut self,
        event_loop: &mut EventLoop<Server>,
    ) -> io::Result<()> {

        info!("ConnSet accepting socket");

        let sock = self.srv_sock.accept().unwrap().unwrap();
        let conn = ServerConn::new(sock);

        // Re-register accepting socket
        event_loop.reregister(
            &self.srv_sock,
            self.srv_token,
            EventSet::readable(),
            PollOpt::edge() | PollOpt::oneshot(),
        );

        self.conns.insert(conn).map(|tok| {
            // Register the connection
            self.conns[tok].token = Some(tok);
            event_loop.register_opt(
                &self.conns[tok].sock,
                tok,
                EventSet::readable(),
                PollOpt::edge() | PollOpt::oneshot()
            ).ok().expect("could not register socket with event loop");
            ()
        }).or_else(|e| Err(Error::new(ErrorKind::Other, "All connection slots full.")))
    }

    fn conn_readable(
        &mut self,
        event_loop: &mut EventLoop<Server>,
        tok: Token,
    ) -> io::Result<()> {

        info!("ConnSet conn readable; tok={:?}", tok);
        self.conn(tok).readable(event_loop)
    }

    fn conn_writable(
        &mut self,
        event_loop: &mut EventLoop<Server>,
        tok: Token,
    ) -> io::Result<()> {

        info!("ConnSet conn writable; tok={:?}", tok);
        match self.conn(tok).writable(event_loop) {
            Err(e) => {
                self.conns.remove(tok);
                Err(e)
            },
            w => w,
        }
    }

    fn conn<'a>(&'a mut self, tok: Token) -> &'a mut ServerConn {
        &mut self.conns[tok]
    }
}
