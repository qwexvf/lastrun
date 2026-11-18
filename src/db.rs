use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OpenFlags};

#[derive(Clone, Copy, Debug)]
pub struct Hit {
    pub count: u64,
    pub first: i64,
    pub last: i64,
}

impl Hit {
    pub fn new(ts: i64) -> Self {
        Hit { count: 1, first: ts, last: ts }
    }

    pub fn bump(&mut self, ts: i64) {
        self.count += 1;
        self.first = self.first.min(ts);
        self.last = self.last.max(ts);
    }

    pub fn merge(&mut self, o: &Hit) {
        self.count += o.count;
        self.first = self.first.min(o.first);
        self.last = self.last.max(o.last);
    }
}

/// A daemon flush holds the write lock briefly; wait instead of failing with SQLITE_BUSY.
const BUSY_WAIT: Duration = Duration::from_secs(10);

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs().cast_signed())
}

pub fn open_rw(path: &Path) -> Result<Connection> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
    conn.busy_timeout(BUSY_WAIT)?;
    // report runs as the normal user and only reads, so the file must be world-readable.
    fs::set_permissions(path, fs::Permissions::from_mode(0o644))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS usage(
            path TEXT NOT NULL,
            source TEXT NOT NULL,
            count INTEGER NOT NULL,
            first_seen INTEGER NOT NULL,
            last_seen INTEGER NOT NULL,
            PRIMARY KEY(path, source)
        ) WITHOUT ROWID;",
    )?;
    Ok(conn)
}

/// `replace` overwrites the count instead of adding to it, so re-running a seed is idempotent.
pub fn flush(conn: &mut Connection, source: &str, hits: &HashMap<String, Hit>, replace: bool) -> Result<()> {
    let sql = if replace {
        "INSERT INTO usage VALUES(?1,?2,?3,?4,?5) ON CONFLICT(path,source) DO UPDATE SET
            count=excluded.count, first_seen=excluded.first_seen, last_seen=excluded.last_seen"
    } else {
        "INSERT INTO usage VALUES(?1,?2,?3,?4,?5) ON CONFLICT(path,source) DO UPDATE SET
            count=count+excluded.count,
            first_seen=min(first_seen,excluded.first_seen),
            last_seen=max(last_seen,excluded.last_seen)"
    };
    let tx = conn.transaction()?;
    {
        let mut st = tx.prepare_cached(sql)?;
        for (p, h) in hits {
            st.execute(params![p, source, h.count.cast_signed(), h.first, h.last])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub struct Usage {
    /// Hits per path, all sources combined.
    pub hits: HashMap<String, Hit>,
    /// Oldest exec row: how far back daemon data reaches.
    pub exec_since: Option<i64>,
    pub history_since: Option<i64>,
}

pub fn load(path: &Path) -> Result<Usage> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open {} (is the daemon installed?)", path.display()))?;
    conn.busy_timeout(BUSY_WAIT)?;
    let mut hits: HashMap<String, Hit> = HashMap::new();
    let mut st = conn.prepare("SELECT path, count, first_seen, last_seen FROM usage")?;
    let rows = st.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, Hit { count: r.get::<_, i64>(1)?.cast_unsigned(), first: r.get(2)?, last: r.get(3)? }))
    })?;
    for row in rows {
        let (p, h) = row?;
        hits.entry(p).and_modify(|e| e.merge(&h)).or_insert(h);
    }
    let since = |src: &str| -> Result<Option<i64>> {
        Ok(conn.query_row("SELECT min(first_seen) FROM usage WHERE source=?1", [src], |r| r.get(0))?)
    };
    Ok(Usage { exec_since: since("exec")?, history_since: since("history")?, hits })
}
