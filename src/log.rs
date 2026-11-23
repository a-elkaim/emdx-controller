//! Logging: a friendly progress channel plus leveled diagnostics, built on
//! `tracing`, formatted in Android logcat's `threadtime` style.
//!
//! Two kinds of event share one pipeline:
//! - **Progress** — the `say!` / `say_err!` lines (🔄 Connecting…, ✅ Done), on the
//!   dedicated [`PROGRESS_TARGET`] target, tagged `emdx` unless `--quiet`.
//! - **Diagnostics** — `tracing::{error,warn,info,debug,trace}!` from anywhere, shown
//!   at a verbosity set by `-v` / `--quiet` or the `EMDX_LOG` environment variable.
//!
//! Both go to stderr (so a command's real output on stdout stays clean). With
//! `--log-file <path>` (or `EMDX_LOG_FILE`) everything is also appended to that file
//! at `debug` (or `trace` with `-vv`), for a user to attach to a bug report. Call
//! [`init`] once at start-up.
//!
//! Every line uses logcat's `threadtime` layout (`MM-DD HH:MM:SS.mmm  PID   TID L tag:
//! message`), with the level letter coloured the way `adb logcat -v color` colours it
//! on a terminal (V dim, D cyan, I green, W yellow, E red):
//!
//! ```text
//! 11-23 12:25:19.938  1234  1234 I emdx: 🔄 Connecting to "em32dx-743d"…
//! 11-23 12:25:20.033  1234  1234 V mdc: MDC → command=11 frame=AA 11 00 00 11
//! 11-23 12:25:20.037  1234  1234 D mdc: MDC command failed command=11 error=NAK …
//! ```

use std::{
    cell::Cell,
    fs::OpenOptions,
    io::{self, IsTerminal, Write},
    path::PathBuf,
    sync::{
        atomic::{AtomicU32, Ordering},
        Mutex,
    },
};

use anyhow::{Context, Result};
use tracing_subscriber::{
    filter::{EnvFilter, LevelFilter},
    fmt::{self, format::Writer, FmtContext, FormatEvent, FormatFields},
    layer::SubscriberExt,
    registry::LookupSpan,
    util::SubscriberInitExt,
    Layer,
};

/// Events the user sees as progress rather than diagnostics (the `say!` macros).
pub const PROGRESS_TARGET: &str = "emdx::progress";

/// `say!` — a friendly progress line (🔄/✅/…), shown on the console and, when a log
/// file is set, recorded there too. A no-op until [`init`] runs.
#[macro_export]
macro_rules! say {
    ($($arg:tt)*) => {
        ::tracing::info!(target: $crate::log::PROGRESS_TARGET, "{}", format_args!($($arg)*))
    };
}

/// `say_err!` — a progress line for something that went wrong but isn't fatal (⚠️ …).
#[macro_export]
macro_rules! say_err {
    ($($arg:tt)*) => {
        ::tracing::warn!(target: $crate::log::PROGRESS_TARGET, "{}", format_args!($($arg)*))
    };
}

/// How to set up logging (from the CLI flags).
#[derive(Debug, Default, Clone)]
pub struct Options {
    /// `-v` count: 0 = warnings, 1 = info, 2 = debug, 3+ = trace (diagnostics).
    pub verbose: u8,
    /// `--quiet`: hide progress lines and all but error diagnostics.
    pub quiet: bool,
    /// `--log-file`: also append everything to this file at debug/trace.
    pub file: Option<PathBuf>,
    /// Also print the `beginning of emdx log` line on the console (for the
    /// long-running `emdx serve`; the log file always gets it).
    pub console_banner: bool,
}

/// logcat opens a fresh buffer with a `--------- beginning of main ---------` line. We
/// do the same, which also separates one run from the next in a log file opened in
/// append mode.
fn banner() -> String {
    format!(
        "--------- beginning of emdx log {} (pid {}) ---------",
        chrono::Local::now().format("%m-%d %H:%M:%S%.3f"),
        std::process::id(),
    )
}

/// logcat's one-letter level, with the colour `adb logcat -v color` gives it.
fn logcat_level(level: &tracing::Level) -> (char, &'static str) {
    match LevelFilter::from(*level) {
        LevelFilter::TRACE => ('V', "\x1b[90m"),
        LevelFilter::DEBUG => ('D', "\x1b[36m"),
        LevelFilter::INFO => ('I', "\x1b[32m"),
        LevelFilter::WARN => ('W', "\x1b[33m"),
        _ => ('E', "\x1b[31m"),
    }
}

/// logcat's tag: our own crate prefix is noise to the user, so it goes; progress lines
/// are tagged `emdx`, and everything else keeps its target.
fn tag_of(target: &str) -> &str {
    if target == PROGRESS_TARGET {
        return "emdx";
    }
    ["emdx_controller::", "emdx::"]
        .into_iter()
        .filter_map(|prefix| target.strip_prefix(prefix))
        .find(|tag| !tag.is_empty())
        .unwrap_or(target)
}

/// Stand-in for logcat's TID column: a small number per thread, counted from 1 in the
/// order the threads first log something (`std` has no OS thread id).
fn thread_id() -> u32 {
    static NEXT: AtomicU32 = AtomicU32::new(1);
    thread_local! { static TID: Cell<u32> = const { Cell::new(0) } }
    TID.with(|tid| {
        let id = tid.get();
        if id != 0 {
            return id;
        }
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        tid.set(id);
        id
    })
}

/// Diagnostic level the console shows, from the `-v` count (or `--quiet`).
fn console_level(opts: &Options) -> LevelFilter {
    if opts.quiet {
        return LevelFilter::ERROR;
    }
    match opts.verbose {
        0 => LevelFilter::WARN,
        1 => LevelFilter::INFO,
        2 => LevelFilter::DEBUG,
        _ => LevelFilter::TRACE,
    }
}

/// Base filter directives: `EMDX_LOG` if set, otherwise everything at `warn` with
/// the emdx crates raised to `level` (so third-party crates stay quiet).
fn base_directives(level: LevelFilter) -> String {
    match std::env::var("EMDX_LOG") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => format!("warn,emdx={level},emdx_controller={level}"),
    }
}

/// Set up logging for the process. Call once, before any `say!`.
pub fn init(opts: Options) -> Result<()> {
    // Console: progress always visible (unless --quiet), diagnostics at the chosen
    // level.
    let progress = if opts.quiet { "off" } else { "info" };
    let console_filter = EnvFilter::new(format!(
        "{},{PROGRESS_TARGET}={progress}",
        base_directives(console_level(&opts))
    ));
    let console = fmt::layer()
        .event_format(EmdxFormat)
        .with_ansi(io::stderr().is_terminal())
        .with_writer(io::stderr)
        .with_filter(console_filter);

    let file = match &opts.file {
        Some(path) => Some(file_layer(path, &opts)?),
        None => None,
    };

    tracing_subscriber::registry()
        .with(console)
        .with(file)
        .try_init()
        .map_err(|e| anyhow::anyhow!("logging is already set up: {e}"))?;
    if opts.console_banner && !opts.quiet {
        eprintln!("{}", banner());
    }
    Ok(())
}

/// The file layer: append, no colour, everything (progress + diagnostics) at debug
/// (trace with `-vv`), or whatever `EMDX_LOG` says.
fn file_layer<S>(path: &PathBuf, opts: &Options) -> Result<Box<dyn Layer<S> + Send + Sync>>
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating the log directory {}", dir.display()))?;
    }
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening the log file {}", path.display()))?;
    let level = if opts.verbose >= 2 {
        LevelFilter::TRACE
    } else {
        LevelFilter::DEBUG
    };
    writeln!(f, "{}", banner())
        .with_context(|| format!("writing to the log file {}", path.display()))?;
    // Keep progress in the file whatever the base level says.
    let filter = EnvFilter::new(format!(
        "{},{PROGRESS_TARGET}=trace",
        base_directives(level)
    ));
    Ok(fmt::layer()
        .event_format(EmdxFormat)
        .with_ansi(false)
        .with_writer(FileWriter(Mutex::new(f).into()))
        .with_filter(filter)
        .boxed())
}

/// Android logcat `threadtime` formatter for both progress and diagnostic events.
struct EmdxFormat;

impl<S, N> FormatEvent<S, N> for EmdxFormat
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        let meta = event.metadata();
        // `MM-DD HH:MM:SS.mmm  PID   TID L tag: message`
        let (letter, colour) = logcat_level(meta.level());
        let (paint, reset) = ansi(&writer, colour);
        write!(
            writer,
            "{} {:>5} {:>5} {}{}{} {}: ",
            chrono::Local::now().format("%m-%d %H:%M:%S%.3f"),
            std::process::id(),
            thread_id(),
            paint,
            letter,
            reset,
            tag_of(meta.target()),
        )?;
        ctx.field_format().format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

/// Colour only where the writer accepts ANSI escapes (never in a log file).
fn ansi(writer: &Writer<'_>, colour: &'static str) -> (&'static str, &'static str) {
    if writer.has_ansi_escapes() {
        (colour, "\x1b[0m")
    } else {
        ("", "")
    }
}

/// A `MakeWriter` over a shared file handle (so all writes serialise).
struct FileWriter(std::sync::Arc<Mutex<std::fs::File>>);

impl<'a> fmt::MakeWriter<'a> for FileWriter {
    type Writer = FileHandle;
    fn make_writer(&'a self) -> Self::Writer {
        FileHandle(self.0.clone())
    }
}

struct FileHandle(std::sync::Arc<Mutex<std::fs::File>>);

impl Write for FileHandle {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().map_or(Ok(buf.len()), |mut f| f.write(buf))
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().map_or(Ok(()), |mut f| f.flush())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_events_in_logcat_style() {
        #[derive(Clone, Default)]
        struct Buf(std::sync::Arc<Mutex<Vec<u8>>>);

        impl<'a> fmt::MakeWriter<'a> for Buf {
            type Writer = BufWriter;
            fn make_writer(&'a self) -> Self::Writer {
                BufWriter(self.0.clone())
            }
        }

        struct BufWriter(std::sync::Arc<Mutex<Vec<u8>>>);
        impl Write for BufWriter {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let buf = Buf::default();
        let subscriber = tracing_subscriber::registry().with(
            fmt::layer()
                .event_format(EmdxFormat)
                .with_ansi(false)
                .with_writer(buf.clone()),
        );
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: PROGRESS_TARGET, "Connecting...");
            tracing::debug!(target: "emdx_controller::mdc", command = 11, "failed");
        });

        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);

        let pid = format!("{:>5}", std::process::id());
        let tid = format!("{:>5}", thread_id());
        assert!(lines[0].ends_with(&format!(" {pid} {tid} I emdx: Connecting...")));
        assert!(lines[1].ends_with(&format!(" {pid} {tid} D mdc: failed command=11")));
    }

    #[test]
    fn tags_are_logcat_short() {
        assert_eq!(tag_of(PROGRESS_TARGET), "emdx");
        assert_eq!(tag_of("emdx_controller::cms"), "cms");
        assert_eq!(tag_of("emdx_controller::ble::bluez"), "ble::bluez");
        assert_eq!(tag_of("emdx"), "emdx");
        assert_eq!(tag_of("rustls::stream"), "rustls::stream");
    }

    #[test]
    fn levels_are_logcat_letters() {
        for (level, letter) in [
            (tracing::Level::TRACE, 'V'),
            (tracing::Level::DEBUG, 'D'),
            (tracing::Level::INFO, 'I'),
            (tracing::Level::WARN, 'W'),
            (tracing::Level::ERROR, 'E'),
        ] {
            assert_eq!(logcat_level(&level).0, letter);
        }
    }

    #[test]
    fn every_thread_gets_its_own_id() {
        let main = thread_id();
        let other = std::thread::spawn(thread_id).join().unwrap();
        assert_eq!(thread_id(), main);
        assert_ne!(main, other);
    }
}
