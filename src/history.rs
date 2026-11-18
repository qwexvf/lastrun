use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::db::{self, Hit};

/// Commands that run the next word. Both they and the wrapped command count as used.
const WRAPPERS: &[&str] = &["sudo", "doas", "env", "nice", "ionice", "time", "nohup", "exec", "command", "noglob", "aegis", "rtk"];

/// zsh writes history "metafied": 0x83 escapes the next byte, which is stored XOR 0x20.
fn unmetafy(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut it = raw.iter();
    while let Some(&b) = it.next() {
        if b == 0x83 {
            if let Some(&n) = it.next() {
                out.push(n ^ 0x20);
            }
        } else {
            out.push(b);
        }
    }
    out
}

/// Command names at the head of each pipeline segment, plus any wrapper (`sudo`, `nice`) before them.
pub fn command_words(cmd: &str) -> Vec<String> {
    let mut words = Vec::new();
    for seg in cmd.split(['|', ';', '&', '\n', '(', ')', '{', '}', '`']) {
        let mut after_wrapper = false;
        for t in seg.split_whitespace() {
            let t = t.trim_start_matches('!');
            let assignment = t.contains('=') && !t.starts_with('/');
            // Wrapper options: `sudo -E`, `nice -n 19`.
            let wrapper_arg = after_wrapper && (t.starts_with('-') || t.bytes().all(|b| b.is_ascii_digit()));
            if t.is_empty() || assignment || wrapper_arg {
                continue;
            }
            words.push(t.to_owned());
            if !WRAPPERS.contains(&t) {
                break;
            }
            after_wrapper = true;
        }
    }
    words
}

fn resolve(word: &str, path_dirs: &[PathBuf], home: &Path) -> Option<String> {
    let cand = if let Some(rest) = word.strip_prefix("~/") {
        home.join(rest)
    } else if word.starts_with('/') {
        PathBuf::from(word)
    } else if word.contains('/') {
        return None; // relative to an unknown cwd
    } else {
        path_dirs.iter().map(|d| d.join(word)).find(|p| is_exec(p))?
    };
    if !is_exec(&cand) {
        return None;
    }
    fs::canonicalize(&cand).ok().map(|p| p.to_string_lossy().into_owned())
}

fn is_exec(p: &Path) -> bool {
    fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

pub fn seed(db_path: &Path, file: &Path, path_var: &str) -> Result<()> {
    let raw = fs::read(file).with_context(|| format!("read {}", file.display()))?;
    let text = String::from_utf8_lossy(&unmetafy(&raw)).into_owned();
    let path_dirs: Vec<PathBuf> = path_var.split(':').filter(|s| !s.is_empty()).map(PathBuf::from).collect();
    // Under sudo, HOME is root's. The history file sits in the user's home, so `~/` resolves against that.
    let home = file.parent().map(Path::to_path_buf).unwrap_or_default();

    let mut hits: HashMap<String, Hit> = HashMap::new();
    let mut cache: HashMap<String, Option<String>> = HashMap::new();
    let (mut entries, mut resolved) = (0u64, 0u64);
    for line in text.lines() {
        // ": <start>:<elapsed>;<command>"
        let Some(rest) = line.strip_prefix(": ") else { continue };
        let Some((meta, cmd)) = rest.split_once(';') else { continue };
        let Some(ts) = meta.split(':').next().and_then(|s| s.parse::<i64>().ok()) else { continue };
        entries += 1;
        for w in command_words(cmd) {
            let r = cache.entry(w.clone()).or_insert_with(|| resolve(&w, &path_dirs, &home));
            if let Some(p) = r {
                resolved += 1;
                hits.entry(p.clone()).and_modify(|h| h.bump(ts)).or_insert_with(|| Hit::new(ts));
            }
        }
    }
    let mut conn = db::open_rw(db_path)?;
    db::flush(&mut conn, "history", &hits, true)?;
    println!("{entries} history entries, {resolved} commands resolved to {} distinct executables", hits.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words() {
        assert_eq!(command_words("sudo -E nice -n 19 cargo build"), ["sudo", "nice", "cargo"]);
        assert_eq!(command_words("FOO=1 git log | head -3 && rg x"), ["git", "head", "rg"]);
        assert_eq!(command_words("aegis npm install x"), ["aegis", "npm"]);
    }

    #[test]
    fn meta() {
        assert_eq!(unmetafy(&[b'a', 0x83, 0xa3, b'b']), [b'a', 0x83, b'b']);
    }
}
