# lastrun

Records every executable that runs on the machine. It then lists installed tools that have not run in N days, so they can be removed.

A root daemon watches `FAN_OPEN_EXEC` with fanotify on the whole filesystem. This catches tools started by scripts, cron, services, editors and agents, not only commands typed in a shell. A tool that only ever runs from a script would look unused to a shell hook, and that is the wrong mistake to make before deleting something.

Only paths are stored: no arguments, no environment.

## Install

```sh
nice -n 19 cargo build --release -j 2
sudo install -Dm755 target/release/lastrun /usr/local/bin/lastrun
sudo install -Dm644 contrib/lastrun.service /etc/systemd/system/lastrun.service
sudo systemctl daemon-reload
sudo systemctl enable --now lastrun
sudo lastrun seed-history ~/.zsh_history --path "$PATH"
```

`seed-history` backfills from zsh history (EXTENDED_HISTORY format), so the first report says something before weeks of exec data exist. It resolves names with the given `PATH`, because sudo resets it. Re-running it replaces the history rows instead of adding to them.

On dinit (Artix), use `contrib/lastrun.dinit` instead. It has not been tested yet.

## Report

```sh
lastrun report                 # all sources, 30 days
lastrun report --days 90 --source aur
lastrun report --show-used
```

Sources: pacman explicit packages, AUR packages, mise installs, `cargo install` crates, uv tools, and `~/.local/bin` / `~/go/bin`. Each unused tool gets the command that removes it. The report removes nothing.

Not offered for removal:
- packages that another package requires, and base system packages (kernel, firmware, microcode, `pacman`, `sudo`, `systemd`)
- packages with no executables (fonts, themes, libraries), which exec tracking cannot judge

Backup copies such as `*.bak` binaries are listed separately.

Read the coverage line at the top. Until the daemon has run for the whole window and across a reboot, tools started only at boot or only occasionally show as unused.

## Limits

- Execs under `/tmp`, `/run/user` and cargo `target/{debug,release}` dirs are skipped: they are build output, and cargo build scripts get a new path on every build.
- When the kernel event queue overflows, some execs are lost. The daemon logs each overflow.
- Shared libraries are loaded with mmap, not exec, so a package that only ships libraries is reported as "no executables".
