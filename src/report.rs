use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::db::{self, Hit, Usage};
use crate::inventory::{self, Kind, Owns, Source, Tool};

/// Unix seconds to YYYY-MM-DD (UTC), Howard Hinnant's `civil_from_days`.
pub fn date(ts: i64) -> String {
    let z = ts.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Status {
    Unused,
    Leftover,
    Used,
    Depended,
    NoExec,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Unused => "unused",
            Status::Leftover => "leftover",
            Status::Used => "used",
            Status::Depended => "required",
            Status::NoExec => "no exec",
        }
    }
}

pub struct Row {
    pub tool: Tool,
    /// All recorded use, not only within the window.
    pub hit: Option<Hit>,
    pub status: Status,
}

pub struct Analysis {
    /// Sorted by source, status, then never-run first and oldest next.
    pub rows: Vec<Row>,
    pub usage: Usage,
    pub days: u32,
    pub now: i64,
    pub cutoff: i64,
    /// Inventory sources that failed; the rest of the analysis still holds.
    pub warnings: Vec<String>,
}

impl Analysis {
    /// What the data covers, and a warning when it is too thin to trust.
    pub fn coverage(&self) -> Vec<String> {
        let mut out = vec![format!("Tools with no recorded use since {} ({} days)", date(self.cutoff), self.days)];
        match self.usage.exec_since {
            Some(s) => {
                let tracked = (self.now - s) / 86_400;
                out.push(format!("exec tracking since {} ({tracked} days)", date(s)));
                if tracked < i64::from(self.days) {
                    out.push(format!(
                        "warning: exec tracking covers less than the {}-day window. Tools run only by scripts, cron, \
                         or services started before lastrun can still show as unused.",
                        self.days
                    ));
                }
            }
            None => out.push("warning: no exec data yet. Is the lastrun daemon running?".to_owned()),
        }
        if let Some(h) = self.usage.history_since {
            out.push(format!("shell history since {}", date(h)));
        }
        out
    }
}

/// Exec paths that count for this tool, newest first.
pub fn matching_paths<'a>(t: &Tool, u: &'a Usage) -> Vec<(&'a str, Hit)> {
    let mut v: Vec<(&str, Hit)> = match &t.owns {
        Owns::Files(fs) => fs.iter().filter_map(|f| u.hits.get_key_value(f)).map(|(k, h)| (k.as_str(), *h)).collect(),
        Owns::Prefix(p) => {
            let p = format!("{p}/");
            u.hits.iter().filter(|(k, _)| k.starts_with(&p)).map(|(k, h)| (k.as_str(), *h)).collect()
        }
    };
    v.sort_by_key(|(_, h)| std::cmp::Reverse(h.last));
    v
}

fn usage_of(t: &Tool, u: &Usage) -> Option<Hit> {
    matching_paths(t, u).into_iter().map(|(_, h)| h).reduce(|mut a, h| {
        a.merge(&h);
        a
    })
}

pub fn analyze(db_path: &Path, days: u32) -> Result<Analysis> {
    let usage = db::load(db_path)?;
    let home = PathBuf::from(std::env::var("HOME").context("HOME not set")?);
    let now = db::now();
    let cutoff = now - i64::from(days) * 86_400;

    let mut tools = Vec::new();
    let mut warnings = Vec::new();
    for (name, r) in [
        ("pacman", inventory::pacman()),
        ("mise", inventory::mise()),
        ("cargo", inventory::cargo(&home)),
        ("uv", inventory::uv(&home)),
    ] {
        match r {
            Ok(v) => tools.extend(v),
            Err(e) => warnings.push(format!("{name} inventory skipped: {e:#}")),
        }
    }
    tools.extend(inventory::local(&home));

    let mut rows: Vec<Row> = tools
        .into_iter()
        .map(|tool| {
            let hit = usage_of(&tool, &usage);
            let recent = hit.is_some_and(|h| h.last >= cutoff);
            let status = match (&tool.kind, recent) {
                (Kind::Leftover(_), _) => Status::Leftover,
                (Kind::NoExec, _) => Status::NoExec,
                (_, true) => Status::Used,
                (Kind::Depended, false) => Status::Depended,
                (Kind::Removable(_), false) => Status::Unused,
            };
            Row { tool, hit, status }
        })
        .collect();
    let src_rank = |s: Source| Source::ALL.iter().position(|x| *x == s);
    rows.sort_by(|a, b| {
        (src_rank(a.tool.source), a.status, a.hit.map_or(i64::MIN, |h| h.last), &a.tool.name).cmp(&(
            src_rank(b.tool.source),
            b.status,
            b.hit.map_or(i64::MIN, |h| h.last),
            &b.tool.name,
        ))
    });
    Ok(Analysis { rows, usage, days, now, cutoff, warnings })
}

pub fn run(db_path: &Path, days: u32, only: Option<Source>, show_used: bool) -> Result<()> {
    let a = analyze(db_path, days)?;
    for w in &a.warnings {
        eprintln!("warning: {w}");
    }
    for line in a.coverage() {
        println!("{line}");
    }

    for src in Source::ALL.into_iter().filter(|s| only.is_none_or(|o| o == *s)) {
        let of = |st: Status| a.rows.iter().filter(move |r| r.tool.source == src && r.status == st);
        let unused: Vec<&Row> = of(Status::Unused).collect();
        let used: Vec<&Row> = of(Status::Used).collect();
        let names = |st: Status| {
            let mut v: Vec<&str> = of(st).map(|r| r.tool.name.as_str()).collect();
            v.sort_unstable();
            v.join(" ")
        };
        let (depended, noexec) = (names(Status::Depended), names(Status::NoExec));
        if unused.is_empty() && used.is_empty() && depended.is_empty() && noexec.is_empty() {
            continue;
        }

        println!("\n== {} : {} unused, {} used ==", src.label(), unused.len(), used.len());
        let w = unused.iter().map(|r| r.tool.name.len()).max().unwrap_or(0).min(40);
        for r in &unused {
            let Kind::Removable(cmd) = &r.tool.kind else { continue };
            let last = r.hit.map_or_else(|| "never".to_owned(), |h| format!("{} x{}", date(h.last), h.count));
            let note = r.tool.note.as_deref().map(|n| format!("  # {n}")).unwrap_or_default();
            println!("  {:w$}  {last:<18}  {cmd}{note}", r.tool.name);
        }
        if show_used {
            let mut used = used;
            used.sort_by_key(|r| std::cmp::Reverse(r.hit.map(|h| h.last)));
            for r in used {
                let h = r.hit.expect("used rows have a hit");
                println!("  used: {}  {} x{}", r.tool.name, date(h.last), h.count);
            }
        }
        if !depended.is_empty() {
            println!("  unused but required by other packages or base system (no command): {depended}");
        }
        if !noexec.is_empty() {
            println!("  no executables, cannot judge by exec: {noexec}");
        }
    }

    let leftovers: Vec<&Row> = a.rows.iter().filter(|r| r.status == Status::Leftover).collect();
    if !leftovers.is_empty() {
        println!("\n== leftover backup binaries ==");
        for r in leftovers {
            if let Kind::Leftover(cmd) = &r.tool.kind {
                println!("  {}  {cmd}", r.tool.name);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn date() {
        assert_eq!(super::date(0), "1970-01-01");
        assert_eq!(super::date(1_758_786_318), "2025-09-25");
    }
}
