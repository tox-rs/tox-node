use std::net::{SocketAddr, ToSocketAddrs};
use std::num::ParseIntError;
use std::str::FromStr;
use std::path::Path;
use std::collections::BTreeSet as Set;

use config::{Config, File as CfgFile};
use serde::de::{self, Deserialize, Deserializer};
use clap::{App, AppSettings, Arg, ArgMatches};
use hex::FromHex;
use itertools::Itertools;
use regex::Regex;
use tox::toxcore::crypto_core::*;
use tox::toxcore::dht::packed_node::PackedNode;
use tox::toxcore::dht::packet::BOOSTRAP_SERVER_MAX_MOTD_LENGTH;

/// Config for threading.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Deserialize)]
pub enum Threads {
    /// Detect number of threads automatically by the number of CPU cores.
    Auto,
    /// Exact number of threads.
    N(u16)
}

impl FromStr for Threads {
    type Err = ParseIntError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "auto" {
            Ok(Threads::Auto)
        } else {
            u16::from_str(s).map(Threads::N)
        }
    }
}

#[cfg(unix)]
arg_enum! {
    /// Specifies where to write logs.
    #[derive(Clone, Copy, PartialEq, Eq, Debug, Deserialize)]
    pub enum LogType {
        Stderr,
        Stdout,
        Syslog,
        None,
    }
}

#[cfg(not(unix))]
arg_enum! {
    /// Specifies where to write logs.
    #[derive(Clone, Copy, PartialEq, Eq, Debug, Deserialize)]
    pub enum LogType {
        Stderr,
        Stdout,
        None,
    }
}

/// Bootstrap node with generic string address which might be either IP address
/// or DNS name.
#[derive(Clone, PartialEq, Eq, Debug, Deserialize)]
pub struct BootstrapNode {
    /// `PublicKey` of the node.
    #[serde(deserialize_with = "de_from_hex")]
    pk: PublicKey,
    /// Generic string address which might be either IP address or DNS name.
    addr: String,
}

fn de_from_hex<'de, D>(deserializer: D) -> Result<PublicKey, D::Error> where D: Deserializer<'de> {
    let s = String::deserialize(deserializer)?;

    let bootstrap_pk_bytes: [u8; 32] = FromHex::from_hex(s)
        .map_err(|e| de::Error::custom(format!("Can't make bytes from hex string {:?}", e)))?;
    PublicKey::from_slice(&bootstrap_pk_bytes)
        .ok_or(de::Error::custom("Can't make PublicKey"))
}

// TODO: Remove this function. Use default String type after bug fix released.
// Bug is here `https://github.com/mehcode/config-rs/issues/74`
fn de_log_type<'de, D>(deserializer: D) -> Result<LogType, D::Error> where D: Deserializer<'de> {
    let s = String::deserialize(deserializer)?;

    match &s[..] {
        "Stderr" => Ok(LogType::Stderr),
        "Stdout" => Ok(LogType::Stdout),
        #[cfg(unix)]
        "Syslog" => Ok(LogType::Syslog),
        "None" => Ok(LogType::None),
        e => Err(de::Error::custom(format!("Invalid LogType {}", e))),
    }
}

fn de_thread<'de, D>(deserializer: D) -> Result<Threads, D::Error> where D: Deserializer<'de> {
    let s = String::deserialize(deserializer)?;

    Threads::from_str(&s)
        .map_err(|e| de::Error::custom(format!("Can't parse Threads {:?}", e)))
}

impl BootstrapNode {
    /// Resolve string address of the node to possible multiple `SocketAddr`s.
    pub fn resolve(&self) -> impl Iterator<Item = PackedNode> {
        let pk = self.pk;
        let addrs = match self.addr.to_socket_addrs() {
            Ok(addrs) => addrs,
            Err(e) => {
                warn!("Failed to resolve bootstrap node address '{}': {}", self.addr, e);
                Vec::new().into_iter()
            },
        };
        addrs.map(move |addr| PackedNode::new(addr, &pk))
    }
}

/// Config parsed from command line arguments.
#[derive(Clone, PartialEq, Eq, Debug, Deserialize)]
pub struct NodeConfig {
    /// UDP address to run DHT node
    #[serde(rename = "udp-address")]
    #[serde(default)]
    pub udp_addr: Option<SocketAddr>,
    /// TCP addresses to run TCP relay
    #[serde(rename = "tcp-addresses")]
    #[serde(default)]
    pub tcp_addrs: Vec<SocketAddr>,
    /// DHT SecretKey
    #[serde(skip_deserializing)]
    pub sk: Option<SecretKey>,
    /// True if the SecretKey was passed as an argument instead of environment
    /// variable. Necessary to print a warning since the logger backend is not
    /// initialized when we parse arguments.
    #[serde(skip_deserializing)]
    pub sk_passed_as_arg: bool,
    /// Path to the file where DHT keys are stored.
    /// When run with config, this field is required.
    #[serde(rename = "keys-file")]
    pub keys_file_config: String,
    #[serde(skip_deserializing)]
    pub keys_file: Option<String>,
    /// List of bootstrap nodes.
    #[serde(rename = "bootstrap-nodes")]
    #[serde(default)]
    pub bootstrap_nodes: Vec<BootstrapNode>,
    /// Number of threads for execution.
    #[serde(deserialize_with = "de_thread")]
    pub threads: Threads,
    /// Specifies where to write logs.
    #[serde(deserialize_with = "de_log_type")]
    #[serde(rename = "log-type")]
    pub log_type: LogType,
    /// Message of the day
    pub motd: String,
    /// Whether LAN discovery is enabled
    #[serde(rename = "no-lan")]
    pub lan_discovery_enabled: bool,
    /// Unused fields while parsing config file
    #[serde(skip_deserializing)]
    pub unused: Set<String>,
}

/// Parse command line arguments.
pub fn cli_parse() -> NodeConfig {
    let matches = App::new(crate_name!())
        .version(crate_version!())
        .author(crate_authors!("\n"))
        .about(crate_description!())
        .setting(AppSettings::ColoredHelp)
        .setting(AppSettings::SubcommandsNegateReqs)
        .arg(Arg::with_name("udp-address")
            .short("u")
            .long("udp-address")
            .help("UDP address to run DHT node")
            .takes_value(true)
            .required_unless("tcp-address"))
        .arg(Arg::with_name("tcp-address")
            .short("t")
            .long("tcp-address")
            .help("TCP address to run TCP relay")
            .multiple(true)
            .takes_value(true)
            .use_delimiter(true)
            .required_unless("udp-address"))
        .arg(Arg::with_name("secret-key")
            .short("s")
            .long("secret-key")
            .help("DHT secret key. Note that you should not pass the key via \
                   arguments due to security reasons. Use this argument for \
                   test purposes only. In the real world use the environment \
                   variable instead")
            .takes_value(true)
            .conflicts_with("keys-file")
            .env("TOX_SECRET_KEY")
            .hidden(true))
        .arg(Arg::with_name("keys-file")
            .short("k")
            .long("keys-file")
            .help("Path to the file where DHT keys are stored")
            .takes_value(true)
            .required_unless("secret-key")
            .conflicts_with("secret-key"))
        .arg(Arg::with_name("bootstrap-node")
            .short("b")
            .long("bootstrap-node")
            .help("Node to perform initial bootstrap")
            .multiple(true)
            .takes_value(true)
            .number_of_values(2)
            .value_names(&["public key", "address"]))
        .arg(Arg::with_name("threads")
            .short("j")
            .long("threads")
            .help("Number of threads to use. The value 'auto' means that the \
                   number of threads will be determined automatically by the \
                   number of CPU cores")
            .takes_value(true)
            .default_value("1"))
        .arg(Arg::with_name("log-type")
            .short("l")
            .long("log-type")
            .help("Where to write logs")
            .takes_value(true)
            .default_value("Stderr")
            .possible_values(&LogType::variants()))
        .arg(Arg::with_name("motd")
            .short("m")
            .long("motd")
            .help("Message of the day. Must be no longer than 256 bytes. May \
                   contain next variables placed in {{ }}:\n\
                   - start_date: time when the node was started\n\
                   - uptime: uptime in the format 'XX days XX hours XX minutes'\n")
            .takes_value(true)
            .validator(|m| {
                let template_regex = Regex::new(r"\{\{.*\}\}")
                    .expect("Failed to compile template regex");
                if !template_regex.is_match(&m) && m.len() > BOOSTRAP_SERVER_MAX_MOTD_LENGTH {
                    Err(format!("Message of the day must not be longer than {} bytes", BOOSTRAP_SERVER_MAX_MOTD_LENGTH))
                } else {
                    Ok(())
                }
            })
            .default_value("This is tox-rs"))
        .arg(Arg::with_name("no-lan")
            .long("no-lan")
            .help("Disable LAN discovery"))
        .get_matches();

    match matches.subcommand() {
        ("config", Some(m)) => run_config(m),
        _ => run_args(&matches),
    }
}

/// Parse settings from a saved file.
fn parse_config(config_path: String) -> NodeConfig {
    let mut settings = Config::default();

    settings.set_default("log-type", "Stderr").expect("Can't set default value for `log-type`");
    settings.set_default("motd", "This is tox-rs").expect("Can't set default value for `motd`");
    settings.set_default("no-lan", "False").expect("Can't set default value for `no-lan`");
    settings.set_default("threads", "1").expect("Can't set default value for `threads`");

    let config_file = if !Path::new(&config_path).exists() {
        panic!("Can't find config file {}", config_path);
    } else {
        CfgFile::with_name(&config_path)
    };

    settings.merge(config_file).expect("Merging config file with default value fails");

    // Collect unrecognized fields to warn about them
    let mut unused = Set::new();
    let mut config: NodeConfig = serde_ignored::deserialize(settings, |path| {
        unused.insert(path.to_string());
    }).expect("Can't deserialize config");

    config.unused = unused;
    config.sk_passed_as_arg = false;
    config.lan_discovery_enabled = !config.lan_discovery_enabled;
    config.keys_file = Some(config.keys_file_config.clone());

    config
}

fn run_config(matches: &ArgMatches) -> NodeConfig {
    let config_path = value_t!(matches.value_of("cfg-file"), String).unwrap_or_else(|e| e.exit());

    parse_config(config_path)
}

fn run_args(matches: &ArgMatches) -> NodeConfig {
    let udp_addr = if matches.is_present("udp-address") {
        Some(value_t!(matches.value_of("udp-address"), SocketAddr).unwrap_or_else(|e| e.exit()))
    } else {
        None
    };

    let tcp_addrs = if matches.is_present("tcp-address") {
        values_t!(matches.values_of("tcp-address"), SocketAddr).unwrap_or_else(|e| e.exit())
    } else {
        Vec::new()
    };

    let sk = matches.value_of("secret-key").map(|s| {
        let sk_bytes: [u8; 32] = FromHex::from_hex(s).expect("Invalid DHT secret key");
        SecretKey::from_slice(&sk_bytes).expect("Invalid DHT secret key")
    });

    let sk_passed_as_arg = matches.occurrences_of("secret-key") > 0;

    let keys_file = matches.value_of("keys-file").map(|s| s.to_owned());

    let bootstrap_nodes = matches
        .values_of("bootstrap-node")
        .into_iter()
        .flat_map(|values| values)
        .tuples()
        .map(|(pk, addr)| {
            // get PK bytes of the bootstrap node
            let bootstrap_pk_bytes: [u8; 32] = FromHex::from_hex(pk).expect("Invalid node key");
            // create PK from bytes
            let bootstrap_pk = PublicKey::from_slice(&bootstrap_pk_bytes).expect("Invalid node key");

            BootstrapNode {
                pk: bootstrap_pk,
                addr: addr.to_owned(),
            }
        })
        .collect();

    let threads = value_t!(matches.value_of("threads"), Threads).unwrap_or_else(|e| e.exit());

    let log_type = value_t!(matches.value_of("log-type"), LogType).unwrap_or_else(|e| e.exit());

    let motd = value_t!(matches.value_of("motd"), String).unwrap_or_else(|e| e.exit());

    let lan_discovery_enabled = !matches.is_present("no-lan");

    let keys_file_config = String::new();

    NodeConfig {
        udp_addr,
        tcp_addrs,
        sk,
        sk_passed_as_arg,
        keys_file,
        keys_file_config,
        bootstrap_nodes,
        threads,
        log_type,
        motd,
        lan_discovery_enabled,
        unused: Set::new(),
    }
}