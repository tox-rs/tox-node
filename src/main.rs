extern crate chrono;
#[macro_use]
extern crate clap;
extern crate env_logger;
extern crate failure;
extern crate futures;
extern crate hex;
extern crate itertools;
#[macro_use]
extern crate log;
extern crate regex;
#[cfg(unix)]
extern crate syslog;
extern crate tokio;
extern crate tokio_codec;
extern crate tox;

mod cli_config;
mod motd;

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, SocketAddr};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use failure::Error;
use futures::sync::mpsc;
use futures::{future, Future, Sink, Stream};
use futures::future::Either;
use itertools::Itertools;
use log::LevelFilter;
use tokio::net::{TcpListener, UdpSocket, UdpFramed};
use tokio::runtime;
use tox::toxcore::crypto_core::*;
use tox::toxcore::dht::codec::{DecodeError, DhtCodec};
use tox::toxcore::dht::server::{Server as UdpServer};
use tox::toxcore::dht::lan_discovery::LanDiscoverySender;
use tox::toxcore::onion::packet::InnerOnionResponse;
use tox::toxcore::tcp::packet::OnionRequest;
use tox::toxcore::tcp::server::{Server as TcpServer, ServerExt};
#[cfg(unix)]
use syslog::Facility;

use cli_config::*;
use motd::Motd;

/// Get version in format 3AAABBBCCC, where A B and C are major, minor and patch
/// versions of node. `tox-bootstrapd` uses similar scheme but with leading 1.
/// Before it used format YYYYMMDDVV so the leading numeral was 2. To make a
/// difference with these schemes we use 3.
fn version() -> u32 {
    let major: u32 = env!("CARGO_PKG_VERSION_MAJOR").parse().expect("Invalid major version");
    let minor: u32 = env!("CARGO_PKG_VERSION_MINOR").parse().expect("Invalid minor version");
    let patch: u32 = env!("CARGO_PKG_VERSION_PATCH").parse().expect("Invalid patch version");
    assert!(major < 1000, "Invalid major version");
    assert!(minor < 1000, "Invalid minor version");
    assert!(patch < 1000, "Invalid patch version");
    3000000000 + major * 1000000 + minor * 1000 + patch
}

/// Bind a UDP listener to the socket address.
fn bind_socket(addr: SocketAddr) -> UdpSocket {
    let socket = UdpSocket::bind(&addr).expect("Failed to bind UDP socket");
    socket.set_broadcast(true).expect("set_broadcast call failed");
    if addr.is_ipv6() {
        socket.set_multicast_loop_v6(true).expect("set_multicast_loop_v6 call failed");
    }
    socket
}

/// Save DHT keys to a binary file.
fn save_keys(keys_file: &str, pk: PublicKey, sk: &SecretKey) {
    #[cfg(not(unix))]
    let mut file = File::create(keys_file).expect("Failed to create the keys file");

    #[cfg(unix)]
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .mode(0o600)
        .open(keys_file)
        .expect("Failed to create the keys file");

    file.write_all(pk.as_ref()).expect("Failed to save public key to the keys file");
    file.write_all(&sk[0..SECRETKEYBYTES]).expect("Failed to save secret key to the keys file");
}

/// Load DHT keys from a binary file.
fn load_keys(mut file: File) -> (PublicKey, SecretKey) {
    let mut buf = [0; PUBLICKEYBYTES + SECRETKEYBYTES];
    file.read_exact(&mut buf).expect("Failed to read keys from the keys file");
    let pk = PublicKey::from_slice(&buf[..PUBLICKEYBYTES]).expect("Failed to read public key from the keys file");
    let sk = SecretKey::from_slice(&buf[PUBLICKEYBYTES..]).expect("Failed to read secret key from the keys file");
    assert!(pk == sk.public_key(), "The loaded public key does not correspond to the loaded secret key");
    (pk, sk)
}

/// Load DHT keys from a binary file or generate and save them if file does not
/// exist.
fn load_or_gen_keys(keys_file: &str) -> (PublicKey, SecretKey) {
    match File::open(keys_file) {
        Ok(file) => load_keys(file),
        Err(ref e) if e.kind() == ErrorKind::NotFound => {
            info!("Generating new DHT keys and storing them to '{}'", keys_file);
            let (pk, sk) = gen_keypair();
            save_keys(keys_file, pk, &sk);
            (pk, sk)
        },
        Err(e) => panic!("Failed to read the keys file: {}", e)
    }
}

/// Run a future with the runtime specified by config.
fn run<F>(future: F, threads_config: ThreadsConfig)
    where F: Future<Item = (), Error = Error> + Send + 'static
{
    if threads_config == ThreadsConfig::N(1) {
        let mut runtime = runtime::current_thread::Runtime::new().expect("Failed to create runtime");
        runtime.block_on(future).expect("Execution was terminated with error");
    } else {
        let mut builder = runtime::Builder::new();
        builder.name_prefix("tox-node-");
        match threads_config {
            ThreadsConfig::N(n) => { builder.core_threads(n as usize); },
            ThreadsConfig::Auto => { }, // builder will detect number of cores automatically
        }
        let mut runtime = builder
            .build()
            .expect("Failed to create runtime");
        runtime.block_on(future).expect("Execution was terminated with error");
    };
}

/// Onion sink and stream for TCP.
struct TcpOnion {
    /// Sink for onion packets from TCP to UDP.
    tx: mpsc::UnboundedSender<(OnionRequest, SocketAddr)>,
    /// Stream of onion packets from TCP to UDP.
    rx: mpsc::UnboundedReceiver<(InnerOnionResponse, SocketAddr)>,
}

/// Onion sink and stream for UDP.
struct UdpOnion {
    /// Sink for onion packets from UDP to TCP.
    tx: mpsc::UnboundedSender<(InnerOnionResponse, SocketAddr)>,
    /// Stream of onion packets from TCP to UDP.
    rx: mpsc::UnboundedReceiver<(OnionRequest, SocketAddr)>,
}

/// Create onion streams for TCP and UDP servers communication.
fn create_onion_streams() -> (TcpOnion, UdpOnion) {
    let (udp_onion_tx, udp_onion_rx) = mpsc::unbounded();
    let (tcp_onion_tx, tcp_onion_rx) = mpsc::unbounded();
    let tcp_onion = TcpOnion {
        tx: tcp_onion_tx,
        rx: udp_onion_rx,
    };
    let udp_onion = UdpOnion {
        tx: udp_onion_tx,
        rx: tcp_onion_rx,
    };
    (tcp_onion, udp_onion)
}

fn run_tcp(cli_config: &CliConfig, dht_sk: SecretKey, tcp_onion: TcpOnion) -> impl Future<Item = (), Error = Error> {
    if cli_config.tcp_addrs.is_empty() {
        // If TCP address is not specified don't start TCP server and only drop
        // all onion packets from DHT server
        let tcp_onion_future = tcp_onion.rx
            .map_err(|()| unreachable!("rx can't fail"))
            .for_each(|_| future::ok(()));
        return Either::A(tcp_onion_future)
    }

    let mut tcp_server = TcpServer::new();
    tcp_server.set_udp_onion_sink(tcp_onion.tx);

    let tcp_server_c = tcp_server.clone();
    let tcp_server_futures = cli_config.tcp_addrs.iter().map(move |&addr| {
        let tcp_server_c = tcp_server_c.clone();
        let dht_sk = dht_sk.clone();
        let listener = TcpListener::bind(&addr).expect("Failed to bind TCP listener");
        tcp_server_c.run(listener, dht_sk)
            .map_err(Error::from)
    });

    let tcp_server_future = future::select_all(tcp_server_futures)
        .map(|_| ())
        .map_err(|(e, _, _)| e);

    let tcp_onion_future = tcp_onion.rx
        .map_err(|()| unreachable!("rx can't fail"))
        .for_each(move |(onion_response, addr)|
            tcp_server.handle_udp_onion_response(addr.ip(), addr.port(), onion_response).or_else(|err| {
                warn!("Failed to handle UDP onion response: {:?}", err);
                future::ok(())
            })
        );

    info!("Running TCP relay on {}", cli_config.tcp_addrs.iter().format(","));

    Either::B(tcp_server_future
        .join(tcp_onion_future)
        .map(|_| ()))
}

fn run_udp(cli_config: &CliConfig, dht_pk: PublicKey, dht_sk: &SecretKey, udp_onion: UdpOnion) -> impl Future<Item = (), Error = Error> {
    let udp_addr = if let Some(udp_addr) = cli_config.udp_addr {
        udp_addr
    } else {
        // If UDP address is not specified don't start DHT server and only drop
        // all onion packets from TCP server
        let udp_onion_future = udp_onion.rx
            .map_err(|()| unreachable!("rx can't fail"))
            .for_each(|_| future::ok(()));
        return Either::A(udp_onion_future)
    };

    let socket = bind_socket(udp_addr);
    let (sink, stream) = UdpFramed::new(socket, DhtCodec).split();

    // Create a channel for server to communicate with network
    let (tx, rx) = mpsc::unbounded();

    let lan_discovery_future = if cli_config.lan_discovery_enabled {
        Either::A(LanDiscoverySender::new(tx.clone(), dht_pk, udp_addr.is_ipv6())
            .run()
            .map_err(Error::from))
    } else {
        Either::B(future::empty())
    };

    let mut server = UdpServer::new(tx, dht_pk, dht_sk.clone());
    let motd = Motd::new(cli_config.motd.clone());
    server.set_bootstrap_info(version(), Box::new(move |_| motd.format().as_bytes().to_owned()));
    server.enable_lan_discovery(cli_config.lan_discovery_enabled);
    server.set_tcp_onion_sink(udp_onion.tx);
    server.enable_ipv6_mode(udp_addr.is_ipv6());

    let server_c = server.clone();
    let udp_onion_future = udp_onion.rx
        .map_err(|()| unreachable!("rx can't fail"))
        .for_each(move |(onion_request, addr)|
            server_c.handle_tcp_onion_request(onion_request, addr).or_else(|err| {
                warn!("Failed to handle TCP onion request: {:?}", err);
                future::ok(())
            })
        );

    if cli_config.bootstrap_nodes.is_empty() {
        warn!("No bootstrap nodes!");
    }

    for &node in &cli_config.bootstrap_nodes {
        server.add_initial_bootstrap(node);
    }

    // The server task asynchronously iterates over and processes each
    // incoming packet.
    let server_c = server.clone();
    let network_reader = stream.then(future::ok).filter(|event|
        match event {
            Ok(_) => true,
            Err(ref e) => {
                error!("packet receive error = {:?}", e);
                // ignore packet decode errors
                e.as_fail().downcast_ref::<DecodeError>().is_none()
            }
        }
    ).and_then(|event| event).for_each(move |(packet, addr)| {
        trace!("Received packet {:?}", packet);
        server_c.handle_packet(packet, addr).or_else(|err| {
            error!("Failed to handle packet: {:?}", err);
            future::ok(())
        })
    });

    let network_writer = rx
        .map_err(|()| unreachable!("rx can't fail"))
        // filter out IPv6 packets if node is running in IPv4 mode
        .filter(move |&(ref _packet, addr)| !(udp_addr.is_ipv4() && addr.is_ipv6()))
        .fold(sink, move |sink, (packet, mut addr)| {
            if udp_addr.is_ipv6() {
                if let IpAddr::V4(ip) = addr.ip() {
                    addr = SocketAddr::new(IpAddr::V6(ip.to_ipv6_mapped()), addr.port());
                }
            }
            trace!("Sending packet {:?} to {:?}", packet, addr);
            sink.send((packet, addr))
        })
        // drop sink when rx stream is exhausted
        .map(|_sink| ());

    info!("Running DHT server on {}", udp_addr);

    Either::B(network_reader
        .select(network_writer).map(|_| ()).map_err(|(e, _)| e)
        .select(server.run().map_err(Error::from)).map(|_| ()).map_err(|(e, _)| e)
        .select(lan_discovery_future).map(|_| ()).map_err(|(e, _)| e)
        .join(udp_onion_future).map(|_| ()))
}

fn main() {
    if !crypto_init() {
        panic!("Crypto initialization failed.");
    }

    let cli_config = cli_parse();

    match cli_config.log_type {
        LogType::Stderr => {
            env_logger::Builder::from_default_env()
                .filter_level(LevelFilter::Info)
                .init();
        },
        LogType::Stdout => {
            env_logger::Builder::from_default_env()
                .target(env_logger::fmt::Target::Stdout)
                .filter_level(LevelFilter::Info)
                .init();
        },
        #[cfg(unix)]
        LogType::Syslog => {
            syslog::init(Facility::LOG_USER, LevelFilter::Info, None)
                .expect("Failed to initialize syslog backend.");
        },
        LogType::None => { },
    }

    let (dht_pk, dht_sk) = if let Some(ref sk) = cli_config.sk {
        (sk.public_key(), sk.clone())
    } else if let Some(ref keys_file) = cli_config.keys_file {
        load_or_gen_keys(keys_file)
    } else {
        panic!("Neither secret key nor keys file is specified")
    };
    if cli_config.sk_passed_as_arg {
        warn!("You should not pass the secret key via arguments due to \
               security reasons. Use the environment variable instead");
    }

    info!("DHT public key: {}", hex::encode(dht_pk.as_ref()).to_uppercase());

    let (tcp_onion, udp_onion) = create_onion_streams();

    let udp_server_future = run_udp(&cli_config, dht_pk, &dht_sk, udp_onion);
    let tcp_server_future = run_tcp(&cli_config, dht_sk, tcp_onion);

    let future = udp_server_future.select(tcp_server_future).map(|_| ()).map_err(|(e, _)| e);

    run(future, cli_config.threads_config);
}
