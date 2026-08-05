# sequencer

Synthetic input for Linux. Today that means one product — `clicker`: hold or toggle a key
to click at a rate you choose, wherever the mouse already is.

It works on X11, on Wayland and on a bare console. There is no rate ceiling picked in
advance: ask for what you want, and the tool reports what it actually managed to send.

## Features

- **Hold or toggle** a chosen key to click, or to repeat a keyboard key instead.
- **Clicks at the cursor**, without moving it. Any mouse button.
- **No rate ceiling of ours.** The limit is your machine and your input stack, and
  `sequencer bench` measures both.
- **The rate you asked for.** Repetitions are scheduled against absolute deadlines in
  integer nanoseconds, so 20/s stays 20/s over hours instead of drifting. When the machine
  cannot keep up it drops the missed slots and says how many, rather than firing a
  catch-up burst.
- **Nothing gets left held down**, including after a panic — checked by a property test
  across hundreds of random cancellation scenarios.
- **Works on Wayland**, which tools built on X11 automation APIs cannot.
- **No setup required.** On X11 it needs no permissions at all; elsewhere it can borrow
  sudo for one run rather than making you widen anything permanently.
- **`doctor`** reports which backends a run would use and what setup, if any, is missing.
- **`bench`** measures what the machine really delivers, by reading its own virtual device
  back rather than trusting its own send count.
- **`simulate`** replays a scripted list of events through the real engine and prints a
  timeline, so behaviour can be checked — or a bug reported — without any hardware.

## The two backends

Which pair a run uses is decided at startup and reported by `doctor`. This is worth
knowing up front, because it decides whether you need any setup at all.

| Session | Inject | Hotkey | Needs |
|---|---|---|---|
| X11 (a reachable X server) | XTEST | X11 key grab | nothing |
| Wayland, console | `/dev/uinput` | `/dev/input` | device access — see below |

**X11** goes through the X server: XTEST for the clicks, a passive grab for the two keys
the run binds. Neither touches an input device, so an X11 session needs no group
membership, no udev rule and no password. It is also *above* libinput, which matters for
the achievable rate — see [Rate ceiling](#rate-ceiling).

**Everywhere else** the tool writes `/dev/uinput` and reads `/dev/input` directly, below
the display server. That is the only way to reach a Wayland compositor or the console, and
it is what needs permission.

The two halves are always chosen **together**, and the choice is made by actually
connecting to the X server rather than by checking whether `$DISPLAY` is set — a variable
left over from a dead session would otherwise send a run down the X11 path and strand it.
A mixed pair would be the worst of both: XTEST's rate with evdev's permissions.

If a hotkey is refused because another program already owns it, the run stops and says so.
It does not quietly drop to the device path, which would demand access — possibly a
password — for something no privilege can fix. Pick a different `--activate` or `--quit`
key.

## Setup

A Rust toolchain to build, and — outside X11 — some way to reach the input devices. No
system libraries, no `-dev` packages, no BIOS settings.

### 1. Rust

The toolchain version is pinned in `rust-toolchain.toml` (currently **1.97**, edition
2024); `rustup` fetches it automatically on first build.

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

### 2. Build

```sh
git clone https://github.com/SirHexa10t/sequencer
cd sequencer
cargo build --release
```

The binary lands at `target/release/sequencer`. Run `sequencer doctor` — on an X11 session
it will already say `Ready.`, and you can stop here.

### 3. Input device access — only if you are not on X11

Two ways, and they trade off against each other.

**Per run (no setup).** Just run `sequencer clicker`. When the devices are not reachable
and you are at a terminal, it explains itself and asks for your password via sudo — for
that run only. Root lasts exactly as long as opening the devices takes; the process then
drops back to your user and runs the whole session unprivileged. Nothing persists after it
exits, and no other program gains anything at any point. A sudo ticket the run created is
revoked afterwards; one you already had is left alone. Pipelines are never prompted —
without a terminal the run fails with these instructions instead.

**Standing (no password, ever).** `sequencer doctor` prints the exact commands for whatever
your machine is missing. In full:

```sh
# The uinput module, for creating the virtual device that does the clicking.
sudo modprobe uinput
echo uinput | sudo tee /etc/modules-load.d/uinput.conf     # persist across reboots

# Let the `input` group use it.
sudo tee /etc/udev/rules.d/99-sequencer-uinput.rules <<'EOF'
KERNEL=="uinput", GROUP="input", MODE="0660", OPTIONS+="static_node=uinput"
EOF
sudo udevadm control --reload-rules
sudo udevadm trigger

# Join that group.
sudo usermod -aG input "$USER"
```

Then **log out and back in** — group membership is only applied at login. To test in the
current shell without logging out, `newgrp input`.

> **What the standing setup costs.** Membership of the `input` group lets *any* program you
> run read *every* input device on the machine. That is full keylogging capability,
> including passwords typed into any application. The per-run option exists precisely so
> this is a choice rather than a prerequisite.

### Optional developer tools

Only for contributing; CI installs its own copies.

```sh
cargo install cargo-hack   # checks every feature combination compiles
cargo install cargo-deny   # licence and advisory checks
```

## How to run

`sequencer --help` lists the subcommands; each has its own `--help`. There is no default
subcommand — more modes are planned, and one of them silently winning would get harder to
read as they arrive.

### Clicking

```sh
sequencer clicker --cps 20                  # hold F9 to click at 20/s; F8 quits
sequencer clicker --toggle --cps 30         # tap F9 to start, tap again to stop
sequencer clicker --cps 500                 # as fast as the machine manages
sequencer clicker --kb-key f --cps 10       # repeat the `f` key instead of clicking
sequencer clicker --m-key right             # a different mouse button
sequencer clicker --activate f7 --quit esc  # different trigger and quit keys
sequencer clicker --limit 100               # stop after 100 repetitions
```

Clicks land wherever the pointer is; the tool never moves it. `--kb-key` also answers to
`--key` and `--key_press`, and `--m-key` to `--button`.

It opens with a one-line banner — the four things worth re-reading before pressing
anything:

```
f key 25/s | HOLD: F9 | Quit: F8 | Limit: 5
```

On exit it reports the actions and repetitions it **sent**, the rate that works out to, and
the rate you asked for — with the word "sent" repeated, deliberately never "achieved".
Everything in that line is counted as this process hands events to the backend. What an
application ends up acting on can be lower, and this tool cannot see that from the inside.
`bench` is the one that measures what came back out. If the machine could not keep up it
also says how many repetitions were skipped.

Two hold durations matter and are separately tunable: `--button-hold-ms` (default 8) and
`--key-hold-ms` (default 1). A click of zero duration is not a click as far as most
applications are concerned.

**The trigger key is not hidden from the focused application** when the run is using
devices: pressing F9 both starts the clicker and sends F9 to whatever has focus. On X11 the
key grab *is* exclusive, so there the trigger does not reach other programs while the run
is active. Either way, pick a trigger the target application ignores.

### Measuring the real ceiling

Reporting how fast the loop *wrote* would be easy and misleading: the kernel, or whatever
is reading, can coalesce or drop events under load. So `bench` reads its own virtual device
back and counts what a real consumer would have seen.

```sh
sequencer bench                         # flat out for three seconds
sequencer bench --cps 2000 --seconds 5  # can this machine hold 2000/s?
```

It prints requested, emitted and delivered rates side by side, with a live progress line
while it runs. A gap between emitted and delivered means the bottleneck is below this
process, not in its loop.

It presses F24 rather than clicking — a key that exists on no physical keyboard and that
essentially nothing binds to — so benchmarking does not click on whatever is under the
pointer. It is still a real key press, so it is not something to run with unsaved work
focused. Note it measures the **uinput device path** specifically, which is the path with a
ceiling worth measuring.

### Checking the machine

```sh
sequencer doctor
```

Reports the session, which backends a run would pick, and each requirement — with the exact
fix for anything failing. Exit code `0` when a run would work, `1` otherwise. From a
container with no input devices at all:

```
sequencer 0.1.0  (linux x86_64)
session: Linux virtual terminal
backend: uinput device for clicks, evdev for hotkeys

[fail] uinput kernel module: /dev/uinput does not exist
[fail] /dev/uinput writable: /dev/uinput: No such file or directory (os error 2)
[fail] /dev/input readable: /dev/input cannot be listed

The uinput kernel module is not loaded
  Injecting input below the display server needs /dev/uinput, which the uinput module provides.
      $ sudo modprobe uinput
      write /etc/modules-load.d/uinput.conf:
          uinput
```

`-v` adds the `DISPLAY` / `WAYLAND_DISPLAY` / `XDG_SESSION_TYPE` values it read.

### Trying it without touching anything

`simulate` runs a scripted list of events through the real engine on a virtual clock. No
devices, no permissions, works anywhere — useful for seeing what a set of flags does, and
for reporting a bug reproducibly.

```sh
sequencer simulate crates/sequencer-cli/tests/fixtures/hold.txt --until-ms 300
```

```
0 BD:left | 8 BU:left | 50 BD:left | 58 BU:left | 100 BD:left | 108 BU:left | 150 BD:left | 158 BU:left | 200 BD:left | 208 BU:left | 250 BD:left | 258 BU:left | 300 BD:left

13 actions, 7 repetitions, 0 skipped.
STILL HELD: [(Button(Left), 1)]
```

Groups read `<milliseconds> <actions>`; `BD:left` is a press and `BU:left` a release, 8 ms
apart at the default hold. `STILL HELD` here is the simulation being cut off mid-click at
300 ms, not a leak — a real run releases on shutdown.

A script is one event per line, `<milliseconds> <down|up> <key>`, `#` for comments:

```
# Hold F9 for just over half a second, then let go.
0    down f9
520  up   f9
```

### write-script

A placeholder. It prints what it will be and exits zero; the format and its parser are not
written yet.

## Rate ceiling

`bench` measures how fast this tool can write to the kernel, and that number is real. It is
**not** the same as how many clicks an application will register.

Everything written to `/dev/uinput` passes through libinput before any application sees it,
and libinput decides what a device *is* from the axes it advertises rather than from its
buttons. Two consequences, both discovered the hard way and both now handled:

- A device advertising `BTN_LEFT` and no axes is not classified as a pointer at all, so
  libinput routes its buttons nowhere. Every write succeeds, a read-back counts every
  event, and not a single click arrives.
- A device with no wheel gets *button* scrolling as its scroll method, which makes libinput
  hold each press back to see whether it begins a scroll gesture — turning rapid clicking
  into one long held button, around 20-30/s.

The virtual device therefore advertises `REL_X`, `REL_Y`, `REL_WHEEL` and `REL_HWHEEL`, and
never sends any of them. Four unused axes are the difference between a clicker that works
and one whose events vanish.

**On X11 none of this applies**, because XTEST injects into the X server's own queue, above
libinput — the layer `xdotool` and pynput-based clickers use. That backend is chosen
automatically when `$DISPLAY` is set. It is a second sink, not a replacement: reaching
Wayland and the console is why the device path exists at all. Build
`--no-default-features --features cli,logging,evdev` for a uinput-only binary.

## Using it from another project

`sequencer-cli` is a library as well as a binary:

```toml
[dependencies]
sequencer-cli = { git = "https://github.com/SirHexa10t/sequencer", branch = "main" }
```

Every flag is a public field and each args struct has a `new()` with the command-line
defaults, so struct-update syntax survives a flag being added later:

```rust
use sequencer_cli::{ClickerArgs, Command};

let args = ClickerArgs { cps: 12.5, toggle: true, ..ClickerArgs::new() };
let exit_code: u8 = Command::Clicker(args).run();
```

`Command` also derives clap's `Subcommand`, so it can be nested in another parser — use the
re-exported `sequencer_cli::clap` rather than depending on clap directly, so the two cannot
end up on different major versions.

To offer the same per-run sudo flow the binary has, call
`sequencer_cli::run_with_sudo_prompt(&cli, "yourtool doctor")` instead of `run_cli`; the
second argument is how *your* command line spells the doctor command, so the advice printed
is runnable.

The library never calls `process::exit`, never installs a tracing subscriber or signal
handler, and never reads `std::env::args` for you. For the engine with no clap at all,
depend on it with `default-features = false` and call `run_clicker`.

## How it is put together

| Crate | What it holds |
|---|---|
| `sequencer-core` | The engine. `#![no_std]`, one dependency, no clock, no threads, no I/O. |
| `sequencer-input` | Every operating-system call in the project. |
| `sequencer-cli` | The clap surface, the run loop, and the `sequencer` binary. |

`sequencer-core` has no path to an OS input API in its dependency graph, and never reads a
clock — the runner passes time in. That is what makes "iteration 10,000 starts at exactly
500 seconds" an exact assertion that runs in microseconds. Timing is the whole ballgame for
an autoclicker, and a test suite that has to sleep in real time is one nobody runs.

Within each crate, one product's worth of behaviour lives in its own directory
(`clicker/`, and `write_script/` next to it), while the engine, the step IR, the run loop
and both input backends stay general. The engine already runs sequences of steps with
loops, waits and per-binding repeat and cancellation policies; the clicker is one profile
expressed in that.

## Development

```sh
cargo test --workspace --all-features    # 180 tests, all headless
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo doc --workspace --no-deps --all-features
```

Tests that need a real `/dev/uinput` skip themselves with a note when it is absent, so the
suite is green in a container and exercises the device on a real machine.

CI runs the suite plus fmt, clippy and docs on Linux; cross-compiles for Windows and macOS,
which is the proof `sequencer-core` is OS-free; checks every feature combination with
`cargo hack`; and runs `cargo deny`, since the project is GPL-3.0-or-later and a
dependency's licence is a correctness matter.

## Platform support

Linux only. The engine is platform-independent and the workspace cross-compiles for Windows
and macOS — the backends are target-gated, so those builds contain none of them and every
command that needs a device says so — but no backend for either is written.

| Platform | Status |
|---|---|
| Linux — X11 | XTEST + key grabs, no setup |
| Linux — Wayland, console | uinput + evdev, needs device access |
| Windows | not written (`WH_KEYBOARD_LL` + `SendInput` would be the shape) |
| macOS | not written (`CGEventTap` + `CGEventPost` would be the shape) |

## Limitations

- **Synthesised input is detectable.** Events come from a virtual device with a
  recognisable name, or from XTEST, which is equally visible. Anything protected by
  Vanguard, EAC or BattlEye will not work, and using it there risks the account. Don't.
- **Clicks land at the pointer.** The tool never moves the cursor.
- **The trigger key reaches the focused application** on the device backends. On X11 the
  grab is exclusive, so there it does not.
- **What is sent is not necessarily received.** See [Rate ceiling](#rate-ceiling); the
  clicker's own report says "sent" for exactly this reason.
- **Linux only.**

## Licence

GPL-3.0-or-later. See [LICENSE](LICENSE).
