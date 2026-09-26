[![Rust](https://github.com/Byron/dua-cli/workflows/Rust/badge.svg)](https://github.com/byron/dua-cli/actions)
[![Crates.io](https://img.shields.io/crates/v/dua-cli.svg)](https://crates.io/crates/dua-cli)
[![Packaging status](https://repology.org/badge/tiny-repos/dua-cli.svg)](https://repology.org/project/dua-cli/badges)

**dua** (-> _Disk Usage Analyzer_) is a tool to conveniently learn about the usage of disk space of a given directory. It automatically tunes filesystem worker concurrency to the throughput of your storage. Optionally delete superfluous data, and do so more quickly than `rm`.

Run `dua i` to launch the [interactive mode](#interactive-mode) for exploring and deleting files.

[![asciicast](https://asciinema.org/a/kDnXUOeqBxZVMoWuFNqzfpeey.svg)](https://asciinema.org/a/kDnXUOeqBxZVMoWuFNqzfpeey)

### Installation

### Binary Release

#### MacOS

```sh
curl -LSfs https://raw.githubusercontent.com/Byron/dua-cli/master/ci/install.sh | \
    sh -s -- --git Byron/dua-cli --crate dua
```

#### MacOS via [MacPorts](https://www.macports.org):

```sh
sudo port selfupdate
sudo port install dua-cli
```

#### MacOS via [Homebrew](https://brew.sh)

```sh
brew update
brew install dua-cli
```

#### Linux

Linux requires the target to be specified explicitly to obtain the MUSL build.

```sh
curl -LSfs https://raw.githubusercontent.com/Byron/dua-cli/master/ci/install.sh | \
    sh -s -- --git Byron/dua-cli --target x86_64-unknown-linux-musl --crate dua
```

#### Windows via [Scoop](https://scoop.sh/)

```sh
scoop install dua
```

#### Windows via [WinGet](https://learn.microsoft.com/en-us/windows/package-manager/winget/)

```sh
winget install Byron.dua-cli
```

#### Pre-built Binaries

See the [releases section][releases] for manual installation of a binary, pre-built for many platforms.

Release archives include build provenance attestations. After downloading an archive, verify that it
was built by this repository with the [GitHub CLI](https://cli.github.com/):

```sh
gh attestation verify ./dua-v2.39.1-aarch64-apple-darwin.tar.gz --repo Byron/dua-cli
```

[releases]: https://github.com/Byron/dua-cli/releases

#### Cargo

Via `cargo`, which can be obtained using [rustup][rustup]

For _Unix_…

```
cargo install dua-cli

# And if you don't need a terminal user interface (most compatible)
cargo install dua-cli --no-default-features

# Compiles on most platforms, with terminal user interface
cargo install dua-cli --no-default-features --features tui-crossplatform
```

For _Windows_, nightly features are currently required.

```
cargo +nightly install dua-cli
```

#### VoidLinux

Via `xbps` on your VoidLinux system.

```
xbps-install dua-cli
```

#### Fedora

Via `dnf` on your Fedora system.

```
sudo dnf install dua-cli
```

#### Arch Linux

Via `pacman` on your ArchLinux system.

```
sudo pacman -S dua-cli
```

#### NixOS

https://search.nixos.org/packages?query=dua

Nix-shell (temporary)

```
nix-shell -p dua
```

NixOS configuration

```
  environment.systemPackages = [
    pkgs.dua
  ];
```

#### NetBSD

Via `pkgin` on your NetBSD system.

```
pkgin install dua-cli
```

Or, building from source

```
cd /usr/pkgsrc/sysutils/dua-cli
make install
```

#### Windows

You will find pre-built binaries for Windows in the [releases section][releases].
Alternatively, install via cargo as in

```
cargo +nightly install dua-cli
```

#### x-cmd

[x-cmd](https://www.x-cmd.com/) is a **toolbox for Posix Shell**, offering a lightweight package manager built using shell and awk.

```sh
x env use dua
```

- Additionally, the [`x dua ...`](https://www.x-cmd.com/pkg/dua#dua) command is available, which automatically installs `dua` without affecting the environment, such as not modifying the `PATH` variable.

### Usage

```bash
# count the space used in the current working directory
dua
# count the space used in all directories that are not hidden
dua *
# learn about additional functionality
dua aggregate --help
```

Scans start with the requested `--threads N` workers. Omitted or `0` uses the
available logical processors. By default, the controller searches for fewer workers
that retain **80% of initially measured useful entry throughput**. It first probes
half the initial count, halves again when accepted, and refines the first failing
bracket. Each count needs two passing windows out of at most three, measured at that count.
The initial reference is measured once and reused: the search moves directly between
candidates without returning to the initial count for reference measurements.
Successive reductions do not compound the 80% allowance.

The initial reference and candidate windows default to 250 ms each. A candidate window starts
only after retired workers finish their current jobs and acknowledge parking.
Their queues remain stealable. Large directory jobs can delay retirement, and a
short scan may finish before the search settles. Empty, completed, or substantially
consumer-backpressured windows cannot accept a reduction. Inconclusive retries are
bounded; an inconclusive candidate is rejected and the bracket is refined. Upward
steps stay within the bracket; even returning to the initial count requires first
rejecting its adjacent lower count. The pool allocates the
initial count up front and parks retired workers.

These are sequential measurements of a changing workload. Directory shape, cache
state, storage latency and consumer speed can make the initial reference stale; this is not a
measurement of disk utilization or a guarantee of the optimal count. A streaming
pool shares a search across roots; restarting a core `Walk` resets the search.

| Option | Default | Meaning |
| --- | --- | --- |
| `--threads N` | Available logical processors | Initial count; `0` also selects available logical processors. |
| `--max-threads N` | No additional cap | Cap the initial adaptive count; at least 1. |
| `--thread-baseline-ms N` | 250 | Duration of the single initial reference measurement. |
| `--thread-adjustment-ms N` | 250 | Candidate measurement duration after retirement. |
| `--thread-throughput-percent PERCENT` | 80 | Minimum candidate/reference throughput percentage, from 0 to 100. |
| `--thread-loss-percent PERCENT` | Unset | Explicitly select the legacy marginal-loss policy below. |
| `--fixed-threads` | Off | Disable both controllers and ignore adaptive caps and thresholds. |

For example, `dua --threads 16 ~/github` starts at sixteen and first probes eight.
`dua --fixed-threads --threads 8 ~/github` holds eight. Options also accept
`DUA_THREADS`, `DUA_MAX_THREADS`, `DUA_THREAD_BASELINE_MS`, `DUA_THREAD_ADJUSTMENT_MS`,
`DUA_THREAD_THROUGHPUT_PERCENT`, `DUA_THREAD_LOSS_PERCENT`, and `DUA_FIXED_THREADS=true`.
Global values override the same subcommand option; defaults apply after merging.
An explicit throughput percentage takes precedence over an explicit loss percentage.

The legacy policy remains available with `--thread-loss-percent 20`. It probes
`n - 1` and accepts only `T_n - T_(n-1) < 0.20 * T_n / n`, with strict inequality,
a refreshed accepted reference, and rollback/hold at the first rejection. Its
`AdaptiveThreads` API and defaults are unchanged.

For core API callers, both `Options::adaptive_threads` and
`Options::throughput_threads` default to `None`, preserving fixed concurrency.
`Some(ThroughputThreads::default())` enables the new 80% search and takes precedence
if both are supplied. `Some(AdaptiveThreads::default())` selects the legacy formula.
Core intervals use `Duration` and percentages use fractions. The constructor's
thread count is the initial count, bounded by the selected policy's `max_threads`.
Core count zero becomes one; CLI zero selects available logical processors.

To compare the CLI against fixed 4/8/16 on the same tree, build once, then run these
commands serially (repeat with rotated ordering to expose cache effects):

```sh
cargo build --release
/usr/bin/time -l target/release/dua --fixed-threads --threads 4 ~/github
/usr/bin/time -l target/release/dua --fixed-threads --threads 8 ~/github
/usr/bin/time -l target/release/dua --fixed-threads --threads 16 ~/github
/usr/bin/time -l target/release/dua --threads 16 ~/github
```

`/usr/bin/time -l` is the macOS form; on Linux use `/usr/bin/time -v`. A separate core
harness records elapsed milliseconds, admitted count, retirement status, entries,
and errors. It refuses paths outside `~/github` unless `--allow-home` is supplied:

```sh
cargo build --release -p dua-core --example thread_probe
/usr/bin/time -l target/release/examples/thread_probe throughput 16 ~/github 250 250 80
/usr/bin/time -l target/release/examples/thread_probe fixed 4 ~/github
/usr/bin/time -l target/release/examples/thread_probe fixed 8 ~/github
/usr/bin/time -l target/release/examples/thread_probe fixed 16 ~/github
```

Harness telemetry is observed as entries arrive, so a blocked iterator can delay
or miss a short transition. An admitted count is provisional during a probe;
`retirement_settled=false` means former workers have not all acknowledged retirement.

The controller and longer-walk measurement procedure are documented in
[THREAD-TUNING.md](etc/profiling/THREAD-TUNING.md).
`thread_tuning_complete()` separately reports whether the search has finished.
These getters are separate concurrent observations. This harness measures raw core
traversal with telemetry overhead, not CLI aggregation. The latest comparison exposed an unreliable choice: the tuner settled at four
workers in one run and one worker in another, taking 106.9 and 398.6 seconds versus
73.9 and 96.7 seconds for fixed sixteen. The unchanged initial reference can
underestimate useful throughput after a slow startup, and the final count is not
revalidated. PR #5 remains draft; the report above records the failure, traversal
errors and unresolved dataset changes. Fixed concurrency remains available with
`--fixed-threads`.

On macOS, the `--deduplicate-apfs-clones` traversal option counts fully shared
APFS file clones only once in aggregate and interactive runs. It is opt-in
because collecting the additional metadata reduces traversal performance.
Files that share only some blocks are not deduplicated, and `--apparent-size`
still reports each file's logical length.

### Tree output

By default `aggregate` prints a flat listing. Pass `--depth N` to instead print an indented tree
that descends `N` levels into each input, which is handy for sharing a disk-usage report without
opening interactive mode. The inputs form the first level, so `--depth 1` lists just them, the same
set of entries the flat listing shows.

```bash
# show each top-level entry and one level below it
dua aggregate --depth 2
```

`--no-sort` and `--no-total` work the same way they do for the flat listing.

### Excluding paths with a pattern file

`--ignore-from FILE` reads gitignore-style patterns and leaves everything they match out of the
report, in both aggregate and interactive mode. This is the `--exclude-from` of `rsync` and the
`--exclude-file` of `restic`, so the same file can answer "how much of this would actually get
backed up?".

```bash
cat .duaignore
# /target/
# **/node_modules/
# *.log
# !important.log

dua --ignore-from .duaignore
```

Patterns follow `.gitignore` syntax - `#` comments, a trailing `/` to match directories only, a
leading `/` to anchor to the current working directory or the traversal root, `**` to span directories,
and `!` to re-include something an earlier pattern excluded. They match the paths `dua` reports, 
which are relative to the directory being looked at, and matching is case-sensitive on every platform.

The option can be given more than once, in which case later files win over earlier ones, and it
can also be set through `DUA_IGNORE_FROM`. Excluded directories are not descended into at all, so
their contents cannot be re-included - the same restriction Git has.

### Interactive Mode

Launch into interactive mode with the `i` or `interactive` subcommand. Get help on keyboard
shortcuts with `?`.
Use this mode to explore, and/or to delete files and directories to release disk space.

Press `]` to minimize or restore the entire right side. `Tab` cycles through visible panes;
`?` restores and focuses Help when the right side is minimized.

Please note that great care has been taken to prevent accidental deletions due to a multi-stage
process, which makes this mode viable for exploration.

```bash
dua i
dua interactive
```

The interactive interface can be localized via the standard POSIX locale environment variables,
in the usual order of precedence `LC_ALL` > `LC_MESSAGES` > `LANG`. English is the default. The
following translations are listed in the order they were added: German (`de`), Japanese (`ja`),
Korean (`ko`), and Simplified Chinese (`zh`, `zh_CN`, `zh_SG`, or `zh_Hans`). They are available
when the locale uses UTF-8 or omits the codeset:

Please [open an issue](https://github.com/Byron/dua-cli/issues/new) to request support for your
language, if you would be available for reviewing it as well.

```bash
LANG=de_DE.UTF-8 dua i   # German interface
LANG=ja_JP.UTF-8 dua i   # Japanese interface
LANG=ko_KR.UTF-8 dua i   # Korean interface
LANG=zh_CN.UTF-8 dua i   # Simplified Chinese interface
```

### Cleanup Mode

`dua clean [DIRECTORY]...` finds disposable directories and lists them largest first as sizing
finishes. With no paths, it searches the current directory. *Nothing is deleted automatically.*

```bash
dua clean ~/dev
dua clean --depth 3 ~/dev ~/Downloads
```

Candidates include `node_modules`, Python caches and virtual environments, Cargo project `target`
directories, and Zig's `.zig-cache`, `zig-cache`, and `zig-out`. In Git repositories, candidates
must be ignored and contain no tracked files. Directories containing a `.git` entry (regardless
of case) and paths excluded by traversal options are skipped.

The hub groups sibling candidates and deeper candidates under their shared parent. Open a group
to browse, sort, or search within it; go back to return to the hub. Use the usual marking and
deletion keys. Marking a group selects only its candidates, leaving other contents untouched.

- `R` in the hub repeats discovery; `r` rechecks the selected candidate or existing group members.
- Inside a candidate, either refresh key rechecks the whole candidate. Refresh clears all marks.
- Discovery is unlimited by default. `--depth 0` checks only the supplied directories for great speedups;
  candidates are always sized completely.

Traversal options, `--no-entry-check`, and `--once` are supported. Parent scanning, snapshot
import, and snapshot export are unavailable.

### Flame graphs

`dua stacks` prints folded stacks—the "collapsed" interchange format read by flame-graph tools.
Each line is an entry's path with `;` between its components, a space, and its size in bytes:

```bash
dua stacks > disk-usage.folded
```

`dua flamegraph` renders the same data with [`inferno`](https://github.com/jonhoo/inferno), writes
the SVG to a temporary file, and opens it. Pass an output path to write the SVG without opening it.
Both commands accept the usual traversal options as well as `--depth` and `--import`:

```bash
dua flamegraph
dua flamegraph -o disk-usage.svg
```

### Configuration

`dua` can read an optional configuration file from your OS-specific config directory:

1. Linux/Unix: `$XDG_CONFIG_HOME/dua-cli/config.toml` (or the platform default config dir)
2. macOS: `~/Library/Application Support/dua-cli/config.toml`
3. Windows: `%APPDATA%\dua-cli\config.toml`

If the file is missing, defaults are used.

Run `dua config show-default` for a commented template containing every configurable keybinding.
Use a string for one binding or an array for aliases; an empty array disables the action.
For example:

```toml
[keys]
# If true (default), close_pane keys ascend from the main pane.
# If false, close_pane keys follow the quit behavior.
esc_navigates_back = true

close_pane = "esc"
toggle_right_panes = "]"
sort_by_name = "ctrl+n"

# Disable permanent deletion and moving entries to the trash.
delete_marked = []
trash_marked = []
```

### Development

Please note that all the following assumes a unix system. On Windows, the linux subsystem should do the job.

#### Run tests

```bash
make tests
```

#### Learn about other targets

```
make
```

#### But why is…

#### …there only one available backend? `termion` was available previously.

Maintaining both backends seemed more cumbersome than it's worth and add complexity I didn't like anymore. `termion` had its benefits,
but I never liked that it seems to have dropped out of support.
Thus `crossterm` is the only remaining backend and it's very actively developed.

### Limitations

- Does not show symbolic links at all if no path is provided when invoking `dua`
  - in an effort to skip symbolic links, for now there are pruned and are not used as a root. Symbolic links will be shown if they
    are not a traversal root, but will not be followed.
- Interactive mode only looks good in dark terminals (see [this issue](https://github.com/Byron/dua-cli/issues/13))
- _easy fix_: file names in main window are not truncated if too large. They are cut off on the right.
- There are plenty of examples in `tests/fixtures` which don't render correctly in interactive mode.
  This can be due to graphemes not interpreted correctly. With Chinese characters for instance,
  column sizes are not correctly computed, leading to certain columns not being shown.
  In other cases, the terminal gets things wrong - I use alacritty, and with certain characters it
  performs worse than, say iTerm3.
  See https://github.com/minimaxir/big-list-of-naughty-strings/blob/master/blns.txt for the source.
- In interactive mode, you will need about 60MB of memory for 1 million entries in the graph.
- In interactive mode, the maximum amount of files is limited to 2^32 - 1 (`u32::max_value() - 1`) entries.
  - One node is used as to 'virtual' root
  - The actual amount of nodes stored might be lower, as there might be more edges than nodes, which are also limited by a `u32` (I guess)
  - The limitation is imposed by the underlying [`petgraph`][petgraph] crate, which declares it as `unsafe` to use u64 for instance.
  - It's possibly _UB_ when that limit is reached, however, it was never observed either.

### Similar Programs

- **CLI:**
  - `du`
  - [`dust`](https://github.com/bootandy/dust)
  - [`dutree`](https://github.com/nachoparker/dutree)
  - [`pdu`](https://github.com/KSXGitHub/parallel-disk-usage)
- **TUI:**
  - [`ncdu`](https://dev.yorhel.nl/ncdu)
  - [`gdu`](https://github.com/dundee/gdu)
  - [`godu`](https://github.com/viktomas/godu)
- **GUI:**
  - [GNOME's Disk Usage Analyzer, a.k.a. `baobab`](https://wiki.gnome.org/action/show/Apps/DiskUsageAnalyzer)
  - [Filelight](https://apps.kde.org/filelight/)

[petgraph]: https://crates.io/crates/petgraph
[rustup]: https://rustup.rs/
[tui]: https://github.com/fdehau/tui-rs
