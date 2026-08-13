//! Optional remote syslog sink.
//!
//! Everything the bot logs already goes through the `log` crate into
//! `simplelog`'s `CombinedLogger` (`main.rs`). This module contributes one more
//! sink to that vec, so a NAS or a central log host sees the same lines as
//! `docker logs` with no shipper, no sidecar and no file to tail.
//!
//! **Off unless `IRC2TORRENT_SYSLOG` is set.** Nothing here runs otherwise, and
//! a misconfigured target never takes the bot down -- `sink()` returns the error
//! for `main` to log once the terminal sink is up, and the bot carries on.
//!
//! Two constraints shaped the design:
//!
//!   * **This cannot be an `options.toml` key.** The logger is initialised
//!     before the config file is read, and `log`'s global logger can only be set
//!     once per process, so a syslog sink can never take part in the live reload
//!     the rest of the config enjoys. An environment variable is honest about
//!     that; a config key would look reloadable and silently not be.
//!   * **UDP is the default on purpose.** `syslog::BasicLogger` is synchronous
//!     behind a `Mutex`, and logging calls sit on the IRC hot path. A UDP send
//!     cannot block on an unreachable host; a TCP one can, which is why `tcp://`
//!     has to be asked for by name.

use std::path::PathBuf;

use log::{LevelFilter, Log, Metadata, Record};
use simplelog::{Config, SharedLogger};
use syslog::{BasicLogger, Facility, Formatter3164};

/// Where to send syslog. Unset or empty disables the sink entirely.
pub const SPEC_ENV: &str = "IRC2TORRENT_SYSLOG";
/// Syslog tag, i.e. the program name in the header. What QuLog Center and most
/// other collectors group by.
pub const TAG_ENV: &str = "IRC2TORRENT_SYSLOG_TAG";
/// Threshold for this sink alone, independent of the terminal's.
pub const LEVEL_ENV: &str = "IRC2TORRENT_SYSLOG_LEVEL";
pub const FACILITY_ENV: &str = "IRC2TORRENT_SYSLOG_FACILITY";
/// Overrides the hostname in the header. Defaults to the system hostname, which
/// inside a container is the short container id unless `--hostname` was passed.
pub const HOSTNAME_ENV: &str = "IRC2TORRENT_SYSLOG_HOSTNAME";
/// Relay rTorrent/Flood output through the `log` crate so it reaches this sink.
pub const CHILD_LOGS_ENV: &str = "IRC2TORRENT_SYSLOG_CHILD_LOGS";

const DEFAULT_TAG: &str = "irc2torrent";
const DEFAULT_PORT: u16 = 514;

/// The spelling of "on" used by every other flag in this crate.
pub fn truthy(value: &str) -> bool {
    matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

/// Whether child output should be relayed through the `log` crate.
///
/// Off by default: routing child lines through `log` means they pick up
/// simplelog's own timestamp and level prefix, which destroys the property
/// `IRC2TORRENT_RAW_CHILD_LOGS` exists to preserve -- Flood's stdout is JSON,
/// and a prefixed JSON line is no longer parseable by a log shipper. Turn it on
/// when you would rather have rTorrent and Flood visible in syslog than
/// machine-readable in `docker logs`.
pub fn child_logs_via_log() -> bool {
    std::env::var(CHILD_LOGS_ENV).map(|v| truthy(&v)).unwrap_or(false)
}

#[derive(Debug, PartialEq)]
enum Target {
    Udp(String),
    Tcp(String),
    /// `None` means the platform's default socket path (`/dev/log`, …).
    Unix(Option<PathBuf>),
}

/// Build the syslog sink from the environment.
///
/// `Ok(None)` means the feature is switched off, which is the normal case and
/// not a problem. `Err` carries a message for the caller to log after the
/// terminal sink exists -- there is nowhere to log it before then.
pub fn sink() -> Result<Option<Box<dyn SharedLogger>>, String> {
    let spec = match std::env::var(SPEC_ENV) {
        Ok(v) if !v.trim().is_empty() => v,
        _ => return Ok(None),
    };

    let target = parse_target(&spec)?;
    let level = parse_level(&env_opt(LEVEL_ENV).unwrap_or_else(|| "info".into()))?;
    let facility = parse_facility(&env_opt(FACILITY_ENV).unwrap_or_else(|| "daemon".into()))?;

    // RFC 3164 puts the hostname between the timestamp and the tag, and the
    // syslog crate omits the field entirely when this is None -- which leaves a
    // line a strict collector parses by position, reading our tag as the
    // hostname. Always send something.
    let hostname = env_opt(HOSTNAME_ENV).or_else(|| {
        hostname::get().ok().map(|h| h.to_string_lossy().into_owned()).filter(|h| !h.is_empty())
    });

    let formatter = Formatter3164 {
        facility,
        hostname,
        process: env_opt(TAG_ENV).unwrap_or_else(|| DEFAULT_TAG.into()),
        pid: std::process::id(),
    };

    Ok(Some(connect(&target, formatter, level)?))
}

/// Open the transport and wrap it as a `CombinedLogger` sink.
///
/// Split from `sink()` so it can be driven from a test without going through
/// the environment: the process-wide env is shared by every test in the binary
/// and cannot be set safely from one.
fn connect(
    target: &Target,
    formatter: Formatter3164,
    level: LevelFilter,
) -> Result<Box<dyn SharedLogger>, String> {
    let logger = match target {
        // Bind the wildcard on an ephemeral port: we only ever send.
        Target::Udp(addr) => syslog::udp(formatter, "0.0.0.0:0", addr.as_str())
            .map_err(|e| format!("{SPEC_ENV}: could not open UDP syslog to {addr}: {e}"))?,
        Target::Tcp(addr) => syslog::tcp(formatter, addr.as_str())
            .map_err(|e| format!("{SPEC_ENV}: could not connect TCP syslog to {addr}: {e}"))?,
        Target::Unix(None) => syslog::unix(formatter)
            .map_err(|e| format!("{SPEC_ENV}: could not open the local syslog socket: {e}"))?,
        Target::Unix(Some(path)) => syslog::unix_custom(formatter, path)
            .map_err(|e| format!("{SPEC_ENV}: could not open syslog socket {}: {e}", path.display()))?,
    };

    Ok(Box::new(SyslogSink {
        level,
        config: Config::default(),
        // Only TCP needs framing; see `SyslogSink::frame_with_newline`.
        frame_with_newline: matches!(target, Target::Tcp(_)),
        inner: BasicLogger::new(logger),
    }))
}

/// A human-readable description of the configured target, for the startup line.
pub fn describe() -> Option<String> {
    let spec = std::env::var(SPEC_ENV).ok().filter(|v| !v.trim().is_empty())?;
    let target = parse_target(&spec).ok()?;
    Some(match target {
        Target::Udp(addr) => format!("udp://{addr}"),
        Target::Tcp(addr) => format!("tcp://{addr}"),
        Target::Unix(None) => "the local syslog socket".into(),
        Target::Unix(Some(p)) => p.display().to_string(),
    })
}

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// Adapts `syslog`'s `log::Log` to the `SharedLogger` that `CombinedLogger` wants.
///
/// The level check is not redundant. `CombinedLogger::log` dispatches to every
/// sink without consulting the sink's own `enabled()`, and `BasicLogger::enabled`
/// tests the *global* max level -- which `CombinedLogger` sets to the maximum
/// across all sinks. Drop that check and a terminal at `debug` would quietly put
/// every debug record on the network.
struct SyslogSink {
    level: LevelFilter,
    config: Config,
    /// Append `\n` to each message, for TCP only.
    ///
    /// UDP and unix-datagram carry one message per datagram, so the transport
    /// supplies the boundary. TCP is a byte stream and does not: RFC 6587 wants
    /// either octet-counting or a trailing LF, and the `syslog` crate writes
    /// neither -- it writes the formatted line and flushes. Two records then
    /// arrive as `<30>…first<30>…second` and a collector reads them as one
    /// message. LF framing is the variant rsyslog, syslog-ng and QuLog default
    /// to, so add it here.
    frame_with_newline: bool,
    inner: BasicLogger,
}

impl Log for SyslogSink {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        if !self.frame_with_newline {
            self.inner.log(record);
            return;
        }
        // `format_args!` cannot outlive the statement it appears in, so the
        // rebuilt record has to be consumed inline rather than bound first.
        let framed = format!("{}\n", record.args());
        self.inner.log(
            &Record::builder()
                .args(format_args!("{framed}"))
                .level(record.level())
                .target(record.target())
                .module_path(record.module_path())
                .file(record.file())
                .line(record.line())
                .build(),
        );
    }

    fn flush(&self) {
        self.inner.flush();
    }
}

impl SharedLogger for SyslogSink {
    fn level(&self) -> LevelFilter {
        self.level
    }

    fn config(&self) -> Option<&Config> {
        Some(&self.config)
    }

    fn as_log(self: Box<Self>) -> Box<dyn Log> {
        self
    }
}

/// `udp://host[:port]`, `tcp://host[:port]`, `unix`, `unix:/path`, or a bare
/// `host[:port]` meaning UDP.
fn parse_target(spec: &str) -> Result<Target, String> {
    let spec = spec.trim();

    if let Some(rest) = spec.strip_prefix("udp://") {
        return Ok(Target::Udp(with_default_port(rest)?));
    }
    if let Some(rest) = spec.strip_prefix("tcp://") {
        return Ok(Target::Tcp(with_default_port(rest)?));
    }
    if spec.eq_ignore_ascii_case("unix") {
        return Ok(Target::Unix(None));
    }
    // Accept both `unix:/dev/log` and `unix:///dev/log`.
    for prefix in ["unix://", "unix:"] {
        if let Some(rest) = spec.strip_prefix(prefix) {
            let rest = rest.trim();
            if rest.is_empty() {
                return Ok(Target::Unix(None));
            }
            return Ok(Target::Unix(Some(PathBuf::from(rest))));
        }
    }
    if let Some((scheme, _)) = spec.split_once("://") {
        return Err(format!("{SPEC_ENV}: unknown scheme `{scheme}://`; use udp://, tcp:// or unix:"));
    }

    Ok(Target::Udp(with_default_port(spec)?))
}

/// Append `:514` when the host carries no port.
///
/// Bracketed IPv6 is handled explicitly: a bare `::1` has colons of its own, so
/// counting them cannot tell a port from an address.
fn with_default_port(host: &str) -> Result<String, String> {
    let host = host.trim().trim_end_matches('/');
    if host.is_empty() {
        return Err(format!("{SPEC_ENV}: no host given"));
    }

    if let Some(close) = host.rfind(']') {
        // Bracketed IPv6, with or without a trailing `:port`.
        return Ok(if host[close + 1..].starts_with(':') {
            host.to_string()
        } else {
            format!("{host}:{DEFAULT_PORT}")
        });
    }

    match host.matches(':').count() {
        0 => Ok(format!("{host}:{DEFAULT_PORT}")),
        1 => Ok(host.to_string()),
        // Unbracketed IPv6. Guessing where the address ends is how you end up
        // shipping logs to the wrong port, so say so instead.
        _ => Err(format!(
            "{SPEC_ENV}: `{host}` looks like an IPv6 address without brackets; write [{host}]:{DEFAULT_PORT}"
        )),
    }
}

fn parse_level(value: &str) -> Result<LevelFilter, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "off" => Ok(LevelFilter::Off),
        "error" => Ok(LevelFilter::Error),
        "warn" | "warning" => Ok(LevelFilter::Warn),
        "info" => Ok(LevelFilter::Info),
        "debug" => Ok(LevelFilter::Debug),
        "trace" => Ok(LevelFilter::Trace),
        other => Err(format!("{LEVEL_ENV}: `{other}` is not a level (off/error/warn/info/debug/trace)")),
    }
}

/// Accepts both the bare name and the `LOG_`-prefixed C spelling.
fn parse_facility(value: &str) -> Result<Facility, String> {
    let name = value.trim().to_ascii_lowercase();
    let name = name.strip_prefix("log_").unwrap_or(&name);
    match name {
        "kern" => Ok(Facility::LOG_KERN),
        "user" => Ok(Facility::LOG_USER),
        "mail" => Ok(Facility::LOG_MAIL),
        "daemon" => Ok(Facility::LOG_DAEMON),
        "auth" => Ok(Facility::LOG_AUTH),
        "syslog" => Ok(Facility::LOG_SYSLOG),
        "lpr" => Ok(Facility::LOG_LPR),
        "news" => Ok(Facility::LOG_NEWS),
        "uucp" => Ok(Facility::LOG_UUCP),
        "cron" => Ok(Facility::LOG_CRON),
        "authpriv" => Ok(Facility::LOG_AUTHPRIV),
        "ftp" => Ok(Facility::LOG_FTP),
        "local0" => Ok(Facility::LOG_LOCAL0),
        "local1" => Ok(Facility::LOG_LOCAL1),
        "local2" => Ok(Facility::LOG_LOCAL2),
        "local3" => Ok(Facility::LOG_LOCAL3),
        "local4" => Ok(Facility::LOG_LOCAL4),
        "local5" => Ok(Facility::LOG_LOCAL5),
        "local6" => Ok(Facility::LOG_LOCAL6),
        "local7" => Ok(Facility::LOG_LOCAL7),
        other => Err(format!("{FACILITY_ENV}: `{other}` is not a facility (daemon, user, local0-local7, …)")),
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::net::UdpSocket;
    use std::time::Duration;

    fn formatter(hostname: Option<&str>) -> Formatter3164 {
        Formatter3164 {
            facility: Facility::LOG_DAEMON,
            hostname: hostname.map(str::to_string),
            process: DEFAULT_TAG.into(),
            pid: 4242,
        }
    }

    /// Send one record to a throwaway local socket and return the datagram.
    fn capture(level: LevelFilter, record_level: log::Level, hostname: Option<&str>) -> Option<String> {
        let server = UdpSocket::bind("127.0.0.1:0").expect("bind");
        server.set_read_timeout(Some(Duration::from_millis(250))).unwrap();
        let addr = server.local_addr().unwrap().to_string();

        // `BasicLogger::enabled` consults the *global* max level, which is Off
        // until a logger is installed -- and none is, in a test binary. Raising
        // it only allows more through; it cannot make another test fail.
        log::set_max_level(LevelFilter::Trace);

        let sink = connect(&Target::Udp(addr), formatter(hostname), level).expect("connect");
        sink.log(
            &Record::builder()
                .args(format_args!("hello from the bot"))
                .level(record_level)
                .target("irc2torrent")
                .build(),
        );

        let mut buf = [0u8; 2048];
        server.recv(&mut buf).ok().map(|n| String::from_utf8_lossy(&buf[..n]).into_owned())
    }

    #[test]
    fn a_udp_datagram_carries_a_parseable_rfc3164_header() {
        let line = capture(LevelFilter::Info, log::Level::Info, Some("nas")).expect("a datagram");
        // <PRI>TIMESTAMP HOSTNAME TAG[PID]: MESSAGE -- priority 30 is
        // daemon(3)*8 + info(6). Collectors parse this by position, so every
        // field has to be present and in order.
        assert!(line.starts_with("<30>"), "{line}");
        assert!(line.contains(" nas irc2torrent[4242]: hello from the bot"), "{line}");
    }

    #[test]
    fn the_hostname_field_is_never_silently_dropped() {
        // Without a hostname the syslog crate omits the field altogether, and a
        // positional parser then reads the tag as the hostname. `sink()` always
        // supplies one; this pins the failure mode that makes it necessary.
        let with = capture(LevelFilter::Info, log::Level::Info, Some("nas")).unwrap();
        let without = capture(LevelFilter::Info, log::Level::Info, None).unwrap();
        assert!(with.contains(" nas irc2torrent["), "{with}");
        assert!(!without.contains(" nas "), "{without}");
    }

    #[test]
    fn the_sinks_own_level_filters_independently_of_the_global_one() {
        // The global max is the maximum across all sinks, so a terminal at
        // debug must not drag debug records onto the network.
        assert!(capture(LevelFilter::Warn, log::Level::Debug, Some("nas")).is_none());
        assert!(capture(LevelFilter::Warn, log::Level::Error, Some("nas")).is_some());
    }

    #[test]
    fn tcp_messages_are_lf_framed_so_a_collector_can_split_them() {
        use std::io::Read;
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap().to_string();

        log::set_max_level(LevelFilter::Trace);
        let sink = connect(&Target::Tcp(addr), formatter(Some("nas")), LevelFilter::Info)
            .expect("connect");
        let mut server = listener.accept().expect("accept").0;
        server.set_read_timeout(Some(Duration::from_millis(250))).unwrap();

        for msg in ["first", "second"] {
            sink.log(
                &Record::builder()
                    .args(format_args!("{msg}"))
                    .level(log::Level::Info)
                    .build(),
            );
        }
        drop(sink);

        let mut got = String::new();
        let _ = server.read_to_string(&mut got);

        // Without framing this arrives as `<30>…first<30>…second` on one line.
        let lines: Vec<_> = got.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 2, "expected two framed messages, got {got:?}");
        assert!(lines[0].ends_with("first"), "{:?}", lines[0]);
        assert!(lines[1].ends_with("second"), "{:?}", lines[1]);
    }

    #[test]
    fn udp_datagrams_are_not_padded_with_a_newline() {
        // The datagram is its own boundary; a trailing LF would just show up as
        // a blank line in the collector.
        let line = capture(LevelFilter::Info, log::Level::Info, Some("nas")).unwrap();
        assert!(!line.ends_with('\n'), "{line:?}");
    }

    #[test]
    fn severity_is_encoded_per_record_not_per_sink() {
        // daemon(3)*8 + err(3) = 27.
        let line = capture(LevelFilter::Trace, log::Level::Error, Some("nas")).unwrap();
        assert!(line.starts_with("<27>"), "{line}");
    }

    #[test]
    fn bare_host_defaults_to_udp_on_514() {
        assert_eq!(parse_target("192.168.1.10").unwrap(), Target::Udp("192.168.1.10:514".into()));
    }

    #[test]
    fn an_explicit_port_is_kept() {
        assert_eq!(parse_target("udp://nas.local:5514").unwrap(), Target::Udp("nas.local:5514".into()));
        assert_eq!(parse_target("tcp://nas.local:601").unwrap(), Target::Tcp("nas.local:601".into()));
    }

    #[test]
    fn tcp_must_be_asked_for_by_name() {
        // The default has to stay UDP: a blocking TCP send sits on the IRC path.
        assert!(matches!(parse_target("nas.local:514").unwrap(), Target::Udp(_)));
    }

    #[test]
    fn unix_targets_parse_with_and_without_a_path() {
        assert_eq!(parse_target("unix").unwrap(), Target::Unix(None));
        assert_eq!(parse_target("UNIX").unwrap(), Target::Unix(None));
        assert_eq!(parse_target("unix:/dev/log").unwrap(), Target::Unix(Some("/dev/log".into())));
        assert_eq!(parse_target("unix:///dev/log").unwrap(), Target::Unix(Some("/dev/log".into())));
    }

    #[test]
    fn bracketed_ipv6_keeps_its_colons() {
        assert_eq!(parse_target("udp://[fd00::1]").unwrap(), Target::Udp("[fd00::1]:514".into()));
        assert_eq!(parse_target("udp://[fd00::1]:5514").unwrap(), Target::Udp("[fd00::1]:5514".into()));
    }

    #[test]
    fn unbracketed_ipv6_is_rejected_rather_than_guessed() {
        // `fd00::1` would otherwise be read as host `fd00:` port `:1`.
        let err = parse_target("udp://fd00::1").unwrap_err();
        assert!(err.contains("brackets"), "{err}");
    }

    #[test]
    fn an_unknown_scheme_is_an_error_not_a_hostname() {
        let err = parse_target("http://nas.local").unwrap_err();
        assert!(err.contains("unknown scheme"), "{err}");
    }

    #[test]
    fn an_empty_host_is_rejected() {
        assert!(parse_target("udp://").is_err());
    }

    #[test]
    fn levels_parse_case_insensitively() {
        assert_eq!(parse_level("INFO").unwrap(), LevelFilter::Info);
        assert_eq!(parse_level(" warning ").unwrap(), LevelFilter::Warn);
        assert_eq!(parse_level("off").unwrap(), LevelFilter::Off);
        assert!(parse_level("chatty").is_err());
    }

    #[test]
    fn facilities_accept_both_spellings() {
        assert_eq!(parse_facility("daemon").unwrap() as i32, Facility::LOG_DAEMON as i32);
        assert_eq!(parse_facility("LOG_LOCAL3").unwrap() as i32, Facility::LOG_LOCAL3 as i32);
        assert!(parse_facility("nonsense").is_err());
    }

    #[test]
    fn truthy_matches_the_other_flags() {
        for on in ["1", "true", "YES", " on "] {
            assert!(truthy(on), "{on}");
        }
        for off in ["0", "false", "", "no"] {
            assert!(!truthy(off), "{off}");
        }
    }
}
