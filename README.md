# lastrun

Records every executable that runs on the machine. It then lists installed tools that have not run in N days, so they can be removed.

A root daemon watches `FAN_OPEN_EXEC` with fanotify on the whole filesystem. This catches tools started by scripts, cron, services, editors and agents, not only commands typed in a shell. A tool that only ever runs from a script would look unused to a shell hook, and that is the wrong mistake to make before deleting something.

Only paths are stored: no arguments, no environment.

## Requirements

- Linux 5.0 or newer with fanotify enabled (`CONFIG_FANOTIFY`). `FAN_OPEN_EXEC` arrived in 5.0.
- Root for the daemon. `report` and `tui` only read the database.
- Rust 1.89 or newer (the minimum of the pinned `rusqlite`) and a C compiler, since SQLite is built from source.
- For the inventory: whichever of `pacman`, `mise`, `cargo`, `uv` you use. Missing ones are skipped.
- For `tui` copying: `wl-copy` (wl-clipboard) and `notify-send` (libnotify). Wayland only.

## Install

```sh
git clone https://github.com/qwexvf/lastrun.git
cd lastrun
nice -n 19 cargo build --release -j 2
sudo install -Dm755 target/release/lastrun /usr/local/bin/lastrun
sudo install -Dm644 contrib/lastrun.service /etc/systemd/system/lastrun.service
sudo systemctl daemon-reload
sudo systemctl enable --now lastrun
sudo lastrun seed-history ~/.zsh_history --path "$PATH"
```

`seed-history` backfills from zsh history (EXTENDED_HISTORY format), so the first report says something before weeks of exec data exist. It resolves names with the given `PATH`, because sudo resets it. Re-running it replaces the history rows instead of adding to them.

On dinit (Artix), use `contrib/lastrun.dinit` instead. It has not been tested yet.

Check that the daemon is recording:

```sh
systemctl status lastrun
ls -l /var/lib/lastrun/usage.db
```

The database is written every flush interval, not on every exec, so give it a minute after the first start.

### Uninstall

```sh
sudo systemctl disable --now lastrun
sudo rm /etc/systemd/system/lastrun.service /usr/local/bin/lastrun
sudo rm -r /var/lib/lastrun
```

## Report

```sh
lastrun report                 # all sources, 30 days
lastrun report --days 90 --source aur
lastrun report --show-used
```

Example, cut down to a few rows per section:

```text
Tools with no recorded use since 2026-10-21 (30 days)
exec tracking since 2026-11-18 (1 days)
warning: exec tracking covers less than the 30-day window. Tools run only by scripts, cron,
or services started before lastrun can still show as unused. Services are seen after the next boot.
shell history since 2025-09-25

== pacman : 47 unused, 57 used ==
  gimp               never               sudo pacman -Rns gimp
  vlc                never               sudo pacman -Rns vlc
  nano               2025-09-30 x2       sudo pacman -Rns nano
  htop               2026-08-21 x1       sudo pacman -Rns htop
  unused but required by other packages or base system (no command): less wget ...
  no executables, cannot judge by exec: base linux noto-fonts ...

== mise : 46 unused, 7 used ==
  bun@1.3.12                   never               mise uninstall bun@1.3.12

== cargo : 5 unused, 2 used ==
  cargo-audit    never               cargo uninstall cargo-audit  # bins: cargo-audit

== local bin : 20 unused, 6 used ==
  mytool             never               rm /home/me/.local/bin/mytool

== leftover backup binaries ==
  mytool.0.9.1.bak  rm /home/me/.local/bin/mytool.0.9.1.bak
```

The date is the last recorded run and `x2` the number of runs, counting execs and shell history together. `never` means no run is recorded at all.

Sources: pacman explicit packages, AUR packages, mise installs, `cargo install` crates, uv tools, and `~/.local/bin` / `~/go/bin`. Each unused tool gets the command that removes it. The report removes nothing.

Not offered for removal:
- packages that another package requires, and base system packages (kernel, firmware, microcode, `pacman`, `sudo`, `systemd`)
- packages with no executables (fonts, themes, libraries), which exec tracking cannot judge

Backup copies such as `*.bak` binaries are listed separately.

Read the coverage line at the top. Until the daemon has run for the whole window and across a reboot, tools started only at boot or only occasionally show as unused.

## TUI

```sh
lastrun tui [--days 30]
```

Same data as `report`, browsable. Tabs per source, `/` filters by name, `u` also shows used, required and library-only packages. The detail pane lists the exec paths that counted for the selected tool.

`space` marks tools and `a` marks every unused one in view. `y` copies one command line for the marks (or the current row) to the clipboard with `wl-copy` and sends a `notify-send` notification. Tools of one kind share a command, such as `sudo pacman -Rns a b && mise uninstall node@22`. The TUI does not run anything.

## Testing

`tests/container.sh` runs the daemon in a Docker container with `CAP_SYS_ADMIN`, watching only a tmpfs, so it needs no host root. It checks exact counts across two daemon restarts, including 10000 execs per round from 4 parallel loops. A script run counts both the script and its interpreter.

## Limits

- Execs under `/tmp`, `/run/user` and cargo `target/{debug,release}` dirs are skipped: they are build output, and cargo build scripts get a new path on every build.
- When the kernel event queue overflows, some execs are lost. The daemon logs each overflow.
- Shared libraries are loaded with mmap, not exec, so a package that only ships libraries is reported as "no executables".

## License

MIT or Apache-2.0, at your option. See `LICENSE-MIT` and `LICENSE-APACHE`.
