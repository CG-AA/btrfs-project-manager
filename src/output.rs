//! Human tables, JSON output, and journald-friendly logging.

use crate::ctx::Ctx;
use serde::Serialize;
use std::fmt;
use std::io::IsTerminal;
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;

pub fn emit<T: Serialize>(ctx: &Ctx, value: &T, human: impl FnOnce() -> String) {
    if ctx.opts.json {
        println!("{}", serde_json::to_string_pretty(value).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}")));
    } else if !ctx.opts.quiet {
        let s = human();
        if !s.is_empty() {
            println!("{}", s.trim_end_matches('\n'));
        }
    }
}

pub struct Table {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
}

impl Table {
    pub fn new(headers: &[&str]) -> Table {
        Table { headers: headers.iter().map(|s| s.to_string()).collect(), rows: vec![] }
    }
    pub fn row(&mut self, cells: Vec<String>) {
        self.rows.push(cells);
    }
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
    pub fn render(&self) -> String {
        let n = self.headers.len();
        let mut w: Vec<usize> = self.headers.iter().map(|h| h.chars().count()).collect();
        for r in &self.rows {
            for (i, c) in r.iter().enumerate().take(n) {
                w[i] = w[i].max(c.chars().count());
            }
        }
        let line = |cells: &[String]| {
            let mut s = String::new();
            for (i, c) in cells.iter().enumerate().take(n) {
                if i + 1 == n {
                    s.push_str(c);
                } else {
                    s.push_str(&format!("{c:<width$}  ", width = w[i]));
                }
            }
            s.trim_end().to_string() + "\n"
        };
        let mut out = line(&self.headers);
        for r in &self.rows {
            out.push_str(&line(r));
        }
        out
    }
}

struct LineFormat {
    journald: bool,
}

impl<S, N> FormatEvent<S, N> for LineFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(&self, ctx: &FmtContext<'_, S, N>, mut writer: Writer<'_>, event: &Event<'_>) -> fmt::Result {
        let level = *event.metadata().level();
        if self.journald {
            let prio = match level {
                Level::ERROR => 3,
                Level::WARN => 4,
                Level::INFO => 6,
                _ => 7,
            };
            write!(writer, "<{prio}>")?;
        } else {
            match level {
                Level::ERROR => write!(writer, "bpm: error: ")?,
                Level::WARN => write!(writer, "bpm: warning: ")?,
                Level::INFO => write!(writer, "bpm: ")?,
                _ => write!(writer, "bpm: {}: ", level.as_str().to_ascii_lowercase())?,
            }
        }
        ctx.field_format().format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

pub fn init_logging(verbose: u8, quiet: bool) {
    let level = match (quiet, verbose) {
        (true, _) => tracing::level_filters::LevelFilter::WARN,
        (_, 0) => tracing::level_filters::LevelFilter::INFO,
        (_, 1) => tracing::level_filters::LevelFilter::DEBUG,
        _ => tracing::level_filters::LevelFilter::TRACE,
    };
    let journald = std::env::var_os("JOURNAL_STREAM").is_some() || !std::io::stderr().is_terminal();
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(level)
        .event_format(LineFormat { journald })
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn table_alignment() {
        let mut t = Table::new(&["NAME", "STAGE", "NOTE"]);
        t.row(vec!["a".into(), "active".into(), "x".into()]);
        t.row(vec!["longer-name".into(), "cold".into(), "".into()]);
        let r = t.render();
        assert_eq!(r.lines().next().unwrap(), "NAME         STAGE   NOTE");
        assert_eq!(r.lines().nth(2).unwrap(), "longer-name  cold");
    }
}
