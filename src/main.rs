#[macro_use]
extern crate log;
extern crate pub_sub;
extern crate simplelog;

use log::LevelFilter;
use simplelog::*;
use time::UtcOffset;

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

/// The log timestamp offset, taken from chrono rather than from `time`.
///
/// simplelog's `Config::default()` timestamps in UTC and never consults `TZ`,
/// so a container told `TZ=Europe/Istanbul` still logged three hours behind the
/// clock it had been given.
///
/// The obvious fix, `ConfigBuilder::set_time_offset_to_local()`, does not work
/// here: it resolves the offset through the `time` crate, which refuses the
/// question once a process is multi-threaded -- a soundness guard around
/// `localtime_r` and the environment. `#[tokio::main]` has already started the
/// runtime by the time the logger is built, so that call returns `Err` and
/// silently leaves UTC in place, which looks exactly like doing nothing.
///
/// chrono carries no such restriction, and is already the clock the daily
/// summary schedules against (`notify.rs`), so reading the offset from there
/// gives the whole program one notion of local time instead of two.
///
/// Resolved once, at startup. A process that runs across a DST transition keeps
/// the offset it started with until it is restarted; that affects only the
/// label on a log line, since the summary re-derives its own local time on
/// every wake-up. `None` falls back to UTC rather than failing to start.
fn local_time_offset() -> Option<UtcOffset> {
    let seconds = chrono::Local::now().offset().local_minus_utc();
    UtcOffset::from_whole_seconds(seconds).ok()
}

// `failure` was dropped here: it is deprecated and unmaintained
// (RUSTSEC-2020-0036, RUSTSEC-2019-0036). anyhow was already a dependency and is
// what the rest of the crate uses.
#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let log_config = match local_time_offset() {
        Some(offset) => ConfigBuilder::new().set_time_offset(offset).build(),
        None => Config::default(),
    };

    let mut sinks: Vec<Box<dyn SharedLogger>> = vec![
        #[cfg(all(feature = "termcolor", not(debug_assertions)))]
            TermLogger::new(LevelFilter::Info, log_config.clone(), TerminalMode::Mixed, ColorChoice::Auto),
        #[cfg(all(not(feature = "termcolor"), not(debug_assertions)))]
            SimpleLogger::new(LevelFilter::Info, log_config.clone()),
        #[cfg(debug_assertions)]
            TestLogger::new(LevelFilter::Info, log_config.clone()),
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
