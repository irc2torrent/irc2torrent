#[macro_use]
extern crate log;
extern crate pub_sub;
extern crate simplelog;

use log::LevelFilter;
use simplelog::*;

/// Set this to a truthy value to run as a container init: irc2torrent becomes
/// PID 1 and supervises rTorrent and Flood as child processes. See
/// `irc2torrent::supervisor`.
const SUPERVISE_ENV: &str = "IRC2TORRENT_SUPERVISE";

fn supervise_requested() -> bool {
    match std::env::var(SUPERVISE_ENV) {
        Ok(v) => matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"),
        Err(_) => false,
    }
}

// `failure` was dropped here: it is deprecated and unmaintained
// (RUSTSEC-2020-0036, RUSTSEC-2019-0036). anyhow was already a dependency and is
// what the rest of the crate uses.
#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let mut sinks: Vec<Box<dyn SharedLogger>> = vec![
        #[cfg(all(feature = "termcolor", not(debug_assertions)))]
            TermLogger::new(LevelFilter::Info, Config::default(), TerminalMode::Mixed, ColorChoice::Auto),
        #[cfg(all(not(feature = "termcolor"), not(debug_assertions)))]
            SimpleLogger::new(LevelFilter::Info, Config::default()),
        #[cfg(debug_assertions)]
            TestLogger::new(LevelFilter::Info, Default::default()),
    ];

    // The remote sink has to be built before the logger exists, so a bad target
    // has nowhere to report itself to yet. Hold the error and log it below --
    // never fail startup over it: losing the bot because a NAS moved would be a
    // far worse outcome than losing its logs.
    let syslog_error = match irc2torrent::logging::sink() {
        Ok(Some(sink)) => {
            sinks.push(sink);
            None
        }
        Ok(None) => None,
        Err(e) => Some(e),
    };

    CombinedLogger::init(sinks).unwrap();
    info!("Started the app");

    match (&syslog_error, irc2torrent::logging::describe()) {
        (Some(e), _) => warn!("{e}; continuing without remote syslog."),
        (None, Some(target)) => info!("Remote syslog enabled: {target}"),
        (None, None) => {}
    }

    // rustls 0.23 will not choose a crypto provider for itself when more than
    // one is compiled in, and this tree has two: reqwest's rustls-tls brings
    // aws-lc-rs, the irc crate's tls-rust brings ring. Without this, the first
    // TLS handshake panics with "Could not automatically determine the
    // process-level CryptoProvider". Installing one here also keeps IRC and
    // HTTP on the same provider.
    //
    // Ignore the error: it only means a provider was already installed.
    let _ = rustls::crypto::ring::default_provider().install_default();

    if supervise_requested() {
        info!("{SUPERVISE_ENV} is set; running as container init.");
        return irc2torrent::supervisor::run().await;
    }

    let mut app = irc2torrent::Irc2Torrent::new().await;
    app.start().await;

    Ok(())
}
