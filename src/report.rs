use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::db::{self, Hit, Usage};
use crate::inventory::{self, Kind, Owns, Source, Tool};

/// Unix seconds to YYYY-MM-DD (UTC), Howard Hinnant's civil_from_days.
fn date(ts: i64) -> String {
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

fn usage_of(t: &Tool, u: &Usage) -> Option<Hit> {
    let mut acc: Option<Hit> = None;
    let mut add = |h: &Hit| match acc.as_mut() {
        Some(a) => a.merge(h),
        None => acc = Some(*h),
    };
    match &t.owns {
        Owns::Files(fs) => fs.iter().filter_map(|f| u.hits.get(f)).for_each(&mut add),
        Owns::Prefix(p) => {
            let p = format!("{p}/");
            u.hits.iter().filter(|(k, _)| k.starts_with(&p)).for_each(|(_, h)| add(h));
        }
    }
    acc
}

pub fn run(db_path: &Path, days: u32, only: Option<Source>, show_used: bool) -> Result<()> {
    let usage = db::load(db_path)?;
    let home = PathBuf::from(std::env::var("HOME").context("HOME not set")?);
    let now = db::now();
    let cutoff = now - i64::from(days) * 86_400;

    let mut tools = Vec::new();
    let mut add = |name: &str, r: Result<Vec<Tool>>| match r {
        Ok(v) => tools.extend(v),
        Err(e) => eprintln!("warning: {name} inventory skipped: {e:#}"),
    };
    add("pacman", inventory::pacman());
    add("mise", inventory::mise());
    add("cargo", inventory::cargo(&home));
    add("uv", inventory::uv(&home));
    tools.extend(inventory::local(&home));

    println!("Tools with no recorded use since {} ({days} days)", date(cutoff));
    match usage.exec_since {
        Some(s) => {
            let tracked = (now - s) / 86_400;
            println!("exec tracking since {} ({tracked} days)", date(s));
            if tracked < i64::from(days) {
                println!(
                    "warning: exec tracking covers less than the {days}-day window. Tools run only by scripts, cron,\n\
                     or services started before lastrun can still show as unused. Services are seen after the next boot."
                );
            }
        }
        None => println!("warning: no exec data yet. Is the lastrun daemon running?"),
    }
    if let Some(h) = usage.history_since {
        println!("shell history since {}", date(h));
    }

    let mut leftovers = Vec::new();
    for src in Source::ALL.into_iter().filter(|s| only.is_none_or(|o| o == *s)) {
        let mut unused = Vec::new();
        let mut used = Vec::new();
        let mut depended = Vec::new();
        let mut noexec = Vec::new();
        for t in tools.iter().filter(|t| t.source == src) {
            let hit = usage_of(t, &usage);
            let recent = hit.filter(|h| h.last >= cutoff);
            match (&t.kind, recent) {
                (Kind::Leftover(cmd), _) => leftovers.push((t, cmd)),
                (Kind::NoExec, _) => noexec.push(t.name.as_str()),
                (_, Some(h)) => used.push((t, h)),
                (Kind::Depended, None) => depended.push(t.name.as_str()),
                (Kind::Removable(cmd), None) => unused.push((t, hit, cmd)),
            }
        }
        if unused.is_empty() && used.is_empty() && depended.is_empty() && noexec.is_empty() {
            continue;
        }
        // Never-run first, then oldest.
        unused.sort_by_key(|(t, h, _)| (h.map_or(i64::MIN, |h| h.last), t.name.clone()));

        println!("\n== {} : {} unused, {} used ==", src.label(), unused.len(), used.len());
        let w = unused.iter().map(|(t, _, _)| t.name.len()).max().unwrap_or(0).min(40);
        for (t, h, cmd) in &unused {
            let last = h.map_or_else(|| "never".to_owned(), |h| format!("{} x{}", date(h.last), h.count));
            let note = t.note.as_deref().map(|n| format!("  # {n}")).unwrap_or_default();
            println!("  {:w$}  {last:<18}  {cmd}{note}", t.name);
        }
        if show_used {
            used.sort_by_key(|(_, h)| std::cmp::Reverse(h.last));
            for (t, h) in &used {
                println!("  used: {}  {} x{}", t.name, date(h.last), h.count);
            }
        }
        if !depended.is_empty() {
            println!("  unused but required by other packages or base system (no command): {}", depended.join(" "));
        }
        if !noexec.is_empty() {
            println!("  no executables, cannot judge by exec: {}", noexec.join(" "));
        }
    }

    if !leftovers.is_empty() {
        println!("\n== leftover backup binaries ==");
        for (t, cmd) in leftovers {
            println!("  {}  {cmd}", t.name);
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
