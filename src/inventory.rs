use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::ValueEnum;

#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
pub enum Source {
    Pacman,
    Aur,
    Mise,
    Cargo,
    Uv,
    Local,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Pacman => "pacman",
            Source::Aur => "aur",
            Source::Mise => "mise",
            Source::Cargo => "cargo",
            Source::Uv => "uv",
            Source::Local => "local bin",
        }
    }

    pub const ALL: [Source; 6] = [Source::Pacman, Source::Aur, Source::Mise, Source::Cargo, Source::Uv, Source::Local];
}

pub enum Owns {
    /// Canonical paths of the executables the tool installed.
    Files(Vec<String>),
    /// Any exec under this canonical directory counts.
    Prefix(String),
}

/// A removal command split into its fixed part and the tool argument, so several
/// selected tools of one kind can share a single command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Removal {
    pub prefix: &'static str,
    pub arg: String,
}

impl Removal {
    fn new(prefix: &'static str, arg: &str) -> Self {
        Removal { prefix, arg: shell_quote(arg) }
    }
}

impl std::fmt::Display for Removal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.prefix, self.arg)
    }
}

/// Joins removals into one line, one command per prefix, in first-seen order.
pub fn merge_removals<'a>(rs: impl IntoIterator<Item = &'a Removal>) -> String {
    let mut groups: Vec<(&str, Vec<&str>)> = Vec::new();
    for r in rs {
        match groups.iter_mut().find(|(p, _)| *p == r.prefix) {
            Some((_, args)) => args.push(&r.arg),
            None => groups.push((r.prefix, vec![&r.arg])),
        }
    }
    groups.iter().map(|(p, args)| format!("{p} {}", args.join(" "))).collect::<Vec<_>>().join(" && ")
}

pub enum Kind {
    Removable(Removal),
    /// Another package needs it, or it is part of the base system.
    Depended,
    /// Installs no executables, so exec tracking says nothing about it.
    NoExec,
    Leftover(Removal),
}

pub struct Tool {
    pub source: Source,
    pub name: String,
    pub owns: Owns,
    pub kind: Kind,
    pub note: Option<String>,
}

fn stdout_of(cmd: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(cmd).args(args).output().with_context(|| format!("run {cmd}"))?;
    if !out.status.success() {
        bail!("{cmd} {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn lines_set(s: &str) -> HashSet<String> {
    s.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect()
}

fn is_exec_file(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Shared libraries often carry the exec bit but are mmapped, not exec'd, so they never get a hit.
fn is_shared_lib(p: &Path) -> bool {
    let n = p.file_name().map_or_else(Default::default, |n| n.to_string_lossy());
    n.ends_with(".so") || n.contains(".so.")
}

fn canon(p: &Path) -> Option<String> {
    fs::canonicalize(p).ok().map(|p| p.to_string_lossy().into_owned())
}

fn shell_quote(s: &str) -> String {
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+@:".contains(c)) {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Base-system packages that stay even when nothing execs them directly.
fn protected(name: &str) -> bool {
    matches!(name, "base" | "base-devel" | "pacman" | "sudo" | "systemd" | "glibc" | "filesystem")
        || name.starts_with("linux")
        || name.ends_with("-firmware")
        || name.ends_with("-ucode")
}

pub fn pacman() -> Result<Vec<Tool>> {
    let explicit = lines_set(&stdout_of("pacman", &["-Qqe"])?);
    // Both exit 1 when nothing matches, so a failure means an empty list.
    let foreign = lines_set(&stdout_of("pacman", &["-Qqm"]).unwrap_or_default());
    // Explicit packages that nothing requires, not even optionally.
    let unrequired = lines_set(&stdout_of("pacman", &["-Qqet"]).unwrap_or_default());

    let mut files: BTreeMap<String, Vec<String>> = explicit.iter().map(|p| (p.clone(), Vec::new())).collect();
    for line in stdout_of("pacman", &["-Ql"])?.lines() {
        let Some((pkg, path)) = line.split_once(' ') else { continue };
        let Some(v) = files.get_mut(pkg) else { continue };
        let p = Path::new(path);
        if path.ends_with('/') || is_shared_lib(p) || !is_exec_file(p) {
            continue;
        }
        v.push(path.to_owned());
    }

    Ok(files
        .into_iter()
        .map(|(name, fs)| {
            let aur = foreign.contains(&name);
            let kind = if fs.is_empty() {
                Kind::NoExec
            } else if protected(&name) || !unrequired.contains(&name) {
                Kind::Depended
            } else if aur {
                Kind::Removable(Removal::new("paru -Rns", &name))
            } else {
                Kind::Removable(Removal::new("sudo pacman -Rns", &name))
            };
            Tool {
                source: if aur { Source::Aur } else { Source::Pacman },
                note: None,
                name,
                owns: Owns::Files(fs),
                kind,
            }
        })
        .collect())
}

pub fn mise() -> Result<Vec<Tool>> {
    let json: serde_json::Value = serde_json::from_str(&stdout_of("mise", &["ls", "--json"])?).context("parse mise ls --json")?;
    let mut out = Vec::new();
    let Some(map) = json.as_object() else { return Ok(out) };
    for (tool, installs) in map {
        for i in installs.as_array().into_iter().flatten() {
            if i["installed"].as_bool() != Some(true) {
                continue;
            }
            let (Some(ver), Some(path)) = (i["version"].as_str(), i["install_path"].as_str()) else { continue };
            let Some(prefix) = canon(Path::new(path)) else { continue };
            out.push(Tool {
                source: Source::Mise,
                name: format!("{tool}@{ver}"),
                owns: Owns::Prefix(prefix),
                kind: Kind::Removable(Removal::new("mise uninstall", &format!("{tool}@{ver}"))),
                note: (i["active"].as_bool() == Some(true)).then(|| "active in mise config".to_owned()),
            });
        }
    }
    Ok(out)
}

fn is_bak(name: &str) -> bool {
    name.ends_with(".bak") || name.contains(".bak.") || name.ends_with('~') || name.ends_with(".old")
}

pub fn cargo(home: &Path) -> Result<Vec<Tool>> {
    let bin = home.join(".cargo/bin");
    // "name vX.Y.Z (source):" followed by indented binary names.
    let mut crates: Vec<(String, Vec<String>)> = Vec::new();
    for line in stdout_of("cargo", &["install", "--list"]).unwrap_or_default().lines() {
        if line.starts_with(char::is_whitespace) {
            if let Some((_, bins)) = crates.last_mut() {
                bins.push(line.trim().to_owned());
            }
        } else if let Some(name) = line.split_whitespace().next() {
            crates.push((name.to_owned(), Vec::new()));
        }
    }
    let listed: HashSet<String> = crates.iter().flat_map(|(_, b)| b.iter().cloned()).collect();
    let mut out: Vec<Tool> = crates
        .into_iter()
        .map(|(name, bins)| Tool {
            source: Source::Cargo,
            kind: Kind::Removable(Removal::new("cargo uninstall", &name)),
            owns: Owns::Files(bins.iter().filter_map(|b| canon(&bin.join(b))).collect()),
            note: Some(format!("bins: {}", bins.join(", "))),
            name,
        })
        .collect();

    // rustup proxies are hard links to rustup; they are not separately installed tools.
    let rustup_ino = fs::metadata(bin.join("rustup")).ok().map(|m| m.ino());
    out.extend(loose_bins(&bin, Source::Cargo, &listed, rustup_ino, &[]));
    Ok(out)
}

pub fn uv(home: &Path) -> Result<Vec<Tool>> {
    let dir = home.join(".local/share/uv/tools");
    let Ok(rd) = fs::read_dir(&dir) else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !e.path().is_dir() {
            continue;
        }
        let Some(prefix) = canon(&e.path()) else { continue };
        out.push(Tool {
            source: Source::Uv,
            kind: Kind::Removable(Removal::new("uv tool uninstall", &name)),
            owns: Owns::Prefix(prefix),
            note: None,
            name,
        });
    }
    Ok(out)
}

/// Entries of ~/.local/bin and ~/go/bin. Symlinks into uv or mise trees are skipped:
/// those tools are already reported under their own source.
pub fn local(home: &Path) -> Vec<Tool> {
    let skip_under: Vec<String> = [".local/share/uv/tools", ".local/share/mise/installs"]
        .iter()
        .filter_map(|d| canon(&home.join(d)))
        .map(|d| d + "/")
        .collect();
    let mut out = Vec::new();
    for d in [".local/bin", "go/bin"] {
        out.extend(loose_bins(&home.join(d), Source::Local, &HashSet::new(), None, &skip_under));
    }
    out
}

fn loose_bins(dir: &Path, source: Source, skip_names: &HashSet<String>, skip_ino: Option<u64>, skip_under: &[String]) -> Vec<Tool> {
    let Ok(rd) = fs::read_dir(dir) else { return Vec::new() };
    let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    let mut out = Vec::new();
    for p in entries {
        let name = p.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        if skip_names.contains(&name) {
            continue;
        }
        let Ok(meta) = fs::metadata(&p) else { continue }; // dangling symlink
        if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
            continue;
        }
        if skip_ino.is_some_and(|i| i == meta.ino()) {
            continue;
        }
        let Some(target) = canon(&p) else { continue };
        if skip_under.iter().any(|s| target.starts_with(s)) {
            continue;
        }
        let shown = p.to_string_lossy().into_owned();
        let is_link = fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink());
        let rm = Removal::new("rm", &shown);
        out.push(Tool {
            source,
            kind: if is_bak(&name) { Kind::Leftover(rm) } else { Kind::Removable(rm) },
            note: is_link.then(|| format!("symlink to {target}")),
            owns: Owns::Files(vec![target]),
            name,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge() {
        let rs = [
            Removal::new("sudo pacman -Rns", "a"),
            Removal::new("rm", "/x y"),
            Removal::new("sudo pacman -Rns", "b"),
        ];
        assert_eq!(merge_removals(&rs), "sudo pacman -Rns a b && rm '/x y'");
    }
}
