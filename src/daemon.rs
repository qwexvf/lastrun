use std::collections::HashMap;
use std::fs::File;
use std::os::fd::{AsFd, AsRawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use nix::errno::Errno;
use nix::libc::c_int;
use nix::poll::{poll, PollFd, PollFlags};
use nix::sys::fanotify::{EventFFlags, Fanotify, InitFlags, MarkFlags, MaskFlags};
use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};

use crate::db::{self, Hit};

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: c_int) {
    STOP.store(true, Ordering::SeqCst);
}

/// Build output is not an installed tool, and cargo build scripts get a fresh
/// hashed path on every build, which would grow the table without bound.
fn ignored(p: &str) -> bool {
    p.starts_with("/tmp/")
        || p.starts_with("/run/user/")
        || p.contains("/target/debug/")
        || p.contains("/target/release/")
}

pub fn run(db_path: &Path, flush_every: Duration, mounts: &[PathBuf]) -> Result<()> {
    // No SA_RESTART, so poll returns EINTR and the loop sees STOP before the next flush.
    let sa = SigAction::new(SigHandler::Handler(on_signal), SaFlags::empty(), SigSet::empty());
    for s in [Signal::SIGTERM, Signal::SIGINT] {
        // SAFETY: the handler only stores to an atomic.
        unsafe { sigaction(s, &sa) }?;
    }

    let mut conn = db::open_rw(db_path)?;
    let fan = Fanotify::init(
        InitFlags::FAN_CLASS_NOTIF | InitFlags::FAN_CLOEXEC | InitFlags::FAN_NONBLOCK,
        EventFFlags::O_RDONLY | EventFFlags::O_LARGEFILE | EventFFlags::O_CLOEXEC,
    )
    .context("fanotify_init (needs CAP_SYS_ADMIN)")?;
    for m in mounts {
        let dir = File::open(m).with_context(|| format!("open {}", m.display()))?;
        fan.mark(
            MarkFlags::FAN_MARK_ADD | MarkFlags::FAN_MARK_FILESYSTEM,
            MaskFlags::FAN_OPEN_EXEC,
            dir.as_fd(),
            None::<&Path>,
        )
        .with_context(|| format!("fanotify_mark {}", m.display()))?;
        eprintln!("lastrun: watching filesystem of {}", m.display());
    }

    let mut pending: HashMap<String, Hit> = HashMap::new();
    let mut overflows = 0u64;
    let mut last_flush = Instant::now();

    while !STOP.load(Ordering::SeqCst) {
        let mut fds = [PollFd::new(fan.as_fd(), PollFlags::POLLIN)];
        match poll(&mut fds, 1000u16) {
            Ok(0) | Err(Errno::EINTR) => {}
            Ok(_) => drain(&fan, &mut pending, &mut overflows)?,
            Err(e) => return Err(e).context("poll"),
        }
        if last_flush.elapsed() >= flush_every {
            persist(&mut conn, &mut pending, &mut overflows)?;
            last_flush = Instant::now();
        }
    }
    persist(&mut conn, &mut pending, &mut overflows)?;
    eprintln!("lastrun: stopped");
    Ok(())
}

fn drain(fan: &Fanotify, pending: &mut HashMap<String, Hit>, overflows: &mut u64) -> Result<()> {
    loop {
        let events = match fan.read_events() {
            Ok(ev) if !ev.is_empty() => ev,
            Ok(_) | Err(Errno::EAGAIN) => return Ok(()),
            Err(e) => return Err(e).context("read fanotify events"),
        };
        let ts = db::now();
        for ev in &events {
            if ev.mask().contains(MaskFlags::FAN_Q_OVERFLOW) {
                *overflows += 1;
                continue;
            }
            let Some(fd) = ev.fd() else { continue };
            let Ok(target) = std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd())) else {
                continue;
            };
            let target = target.to_string_lossy();
            let path = target.strip_suffix(" (deleted)").unwrap_or(&target);
            if !ignored(path) {
                pending.entry(path.to_owned()).and_modify(|h| h.bump(ts)).or_insert_with(|| Hit::new(ts));
            }
        }
        // Dropping `events` closes the fd the kernel opened for each one.
    }
}

fn persist(conn: &mut rusqlite::Connection, pending: &mut HashMap<String, Hit>, overflows: &mut u64) -> Result<()> {
    if *overflows > 0 {
        eprintln!("lastrun: event queue overflowed {overflows} time(s); some execs were not counted");
        *overflows = 0;
    }
    if !pending.is_empty() {
        db::flush(conn, "exec", pending, false)?;
        pending.clear();
    }
    Ok(())
}
