use clap::crate_description;
use clap::{Arg, ArgMatches};
use std::env;
use std::fs::canonicalize;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use crate::BoxResult;

const ABOUT: &str = concat!("\n", crate_description!()); // Add extra newline.

fn app() -> clap::Command<'static> {
    clap::command!()
        .about(ABOUT)
        .arg(
            Arg::new("address")
                .short('b')
                .long("bind")
                .default_value("127.0.0.1")
                .help("Specify bind address")
                .value_name("address"),
        )
        .arg(
            Arg::new("port")
                .short('p')
                .long("port")
                .default_value("5000")
                .help("Specify port to listen on")
                .value_name("port"),
        )
        .arg(
            Arg::new("path")
                .default_value(".")
                .allow_invalid_utf8(true)
                .help("Path to a directory for serving files"),
        )
        .arg(
            Arg::new("no-edit")
                .short('E')
                .long("no-edit")
                .help("Disable editing operations such as update or delete"),
        )
        .arg(
            Arg::new("auth")
                .short('a')
                .long("auth")
                .help("Authenticate with user and pass")
                .value_name("user:pass"),
        )
}

pub fn matches() -> ArgMatches {
    app().get_matches()
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Args {
    pub address: String,
    pub port: u16,
    pub path: PathBuf,
    pub readonly: bool,
    pub auth: Option<String>,
}

impl Args {
    /// Parse command-line arguments.
    ///
    /// If a parsing error ocurred, exit the process and print out informative
    /// error message to user.
    pub fn parse(matches: ArgMatches) -> BoxResult<Args> {
        let address = matches.value_of("address").unwrap_or_default().to_owned();
        let port = matches.value_of_t::<u16>("port")?;
        let path = matches.value_of_os("path").unwrap_or_default();
        let path = Args::parse_path(path)?;
        let readonly = matches.is_present("no-edit");
        let auth = matches.value_of("auth").map(|v| v.to_owned());

        Ok(Args {
            address,
            port,
            path,
            readonly,
            auth,
        })
    }

    /// Parse path.
    fn parse_path<P: AsRef<Path>>(path: P) -> BoxResult<PathBuf> {
        let path = path.as_ref();
        if !path.exists() {
            bail!("error: path \"{}\" doesn't exist", path.display());
        }

        env::current_dir()
            .and_then(|mut p| {
                p.push(path); // If path is absolute, it replaces the current path.
                canonicalize(p)
            })
            .or_else(|err| {
                bail!(
                    "error: failed to access path \"{}\": {}",
                    path.display(),
                    err,
                )
            })
    }

    /// Construct socket address from arguments.
    pub fn address(&self) -> BoxResult<SocketAddr> {
        format!("{}:{}", self.address, self.port)
            .parse()
            .or_else(|err| {
                bail!(
                    "error: invalid address {}:{} : {}",
                    self.address,
                    self.port,
                    err,
                )
            })
    }
}