# sequencer

An autoclicker for Linux. Hold or toggle a key to click at a rate you choose, wherever the
mouse already is.

It works the same on X11, on Wayland, and on a bare console, because it reads and writes
input devices directly rather than asking a display server for permission. That is also
the fastest path available — a click is a couple of kernel writes with no display-server
round trip in the way — so the rate ceiling is whatever your machine can do rather than a
number picked in advance.

## Features

- **Hold or toggle** a chosen key to click, or to repeat a keyboard key instead.
- **Clicks at the cursor**, without moving it. Any mouse button.
- **No rate ceiling.** Ask for whatever you want; the tool reports the rate it actually
  achieved, and `sequencer bench` measures your machine's real limit end to end.
- **The rate you asked for.** Repetitions are scheduled against absolute deadlines in
  integer nanoseconds, so 20/s stays 20/s over hours instead of drifting. When the machine
  cannot keep up it drops the missed slots and says how many, rather than firing a
  catch-up burst.
- **Nothing gets left held down**, including after a panic — checked by a property test
  across hundreds of random cancellation scenarios.
- **Works on Wayland**, which the tools built on X11 automation APIs do not.
- **`doctor`** tells you exactly what setup is missing and prints the commands to fix it.
- **`simulate`** replays a scripted list of events through the real engine and prints a
  timeline, so behaviour can be checked — or a bug reported — without any hardware.
- **`bench`** measures what the machine can really deliver, by reading its own virtual
  device back rather than trusting its own send count.

## Tech Stack Setup

Two things: a Rust toolchain to build it, and permission to reach the input devices to run
it. No system libraries, no `-dev` packages, no BIOS settings.

### 1. Rust

The toolchain version is pinned in `rust-toolchain.toml` (currently **1.97**, edition
2024); `rustup` fetches it automatically on first build.

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Open a new shell and confirm:

```sh
cargo --version    # 1.97.x or newer
```

### 2. Build

```sh
git clone https://github.com/SirHexa10t/sequencer
cd sequencer
cargo build --release
```

The binary lands at `target/release/sequencer`.

### 3. Input device access

`sequencer doctor` checks all of this and prints the exact commands for whatever is
missing on your machine, so the fastest path is to run it and follow what it says. For
reference, the full setup is:

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

> **What you are agreeing to.** Membership of the `input` group lets *any* program you run
> read *every* input device on the machine. That is full keylogging capability, including
> passwords typed into any application. sequencer needs it because reading and writing the
> device nodes is exactly what makes it work under Wayland and on the console. It is a
> real trade and worth making deliberately.

Verify:

```sh
sequencer doctor    # exit code 0 once everything passes
```

### Optional developer tools

Only for contributing, and only these two; CI installs its own copies.

```sh
cargo install cargo-hack   # checks every feature combination compiles
cargo install cargo-deny   # licence and advisory checks
```

## How to Run

### Clicking

```sh
sequencer clicker --cps 20                  # hold F9 to click at 20/s; F8 quits
sequencer clicker --toggle --cps 30         # tap F9 to start, tap again to stop
sequencer clicker --cps 500                 # as fast as the machine manages
sequencer clicker --key f --cps 10          # repeat the `f` key instead of clicking
sequencer clicker --activate f7 --quit esc  # different trigger and quit keys
```

Clicks land wherever the pointer is; the tool never moves it. The subcommand is required
rather than implied — more modes are coming, and a tool where one of them silently wins
gets harder to read as they arrive. `sequencer --help` lists them; `sequencer clicker
--help` lists every flag.

On start it echoes the settings in words, so a surprising result can be traced to a flag
rather than guessed at:

```
left click at 500/s, while f9 is held (no limit). f8 quits.
```

On exit it reports the total actions, the number of repetitions, and — once there have
been enough to measure — the rate it actually achieved next to the one you asked for. If
the machine could not keep up it also says how many repetitions were skipped. The achieved
figure is the honest answer for that machine, and the way to find your ceiling is to ask
for more than you expect and read it back.

Note the trigger key is **not** hidden from whatever has focus: pressing F9 both starts the
clicker and sends F9 to the focused application, the same as the Python prototype did.
Pick a trigger the target application ignores.

### Find the real ceiling

Reporting how fast the loop *wrote* would be easy and misleading: the kernel or whatever
is reading can coalesce or drop events under load. So `bench` reads its own virtual device
back and counts what a real consumer would have seen.

```sh
sequencer bench                        # flat out for three seconds
sequencer bench --cps 2000 --seconds 5 # can this machine hold 2000/s?
```

It prints the requested, emitted and delivered rates side by side. A gap between emitted
and delivered is the interesting part: it means the bottleneck is below this process, not
in its loop.

It presses F24 rather than clicking — a key that exists on no physical keyboard and that
essentially nothing binds to — so benchmarking does not click on whatever is under the
pointer. It is still a real key press, so it is not something to run with unsaved work
focused.

### Check the machine

```sh
sequencer doctor
```

Checks each requirement and, for anything failing, prints why and the commands that fix
it. Exit code `0` when everything passes, `1` otherwise. Sample from a container with no
input devices at all:

```
sequencer 0.1.0  (linux x86_64)
session: Linux virtual terminal

[fail] uinput kernel module: /dev/uinput does not exist
[fail] /dev/uinput writable: /dev/uinput: No such file or directory (os error 2)
[fail] /dev/input readable: /dev/input cannot be listed

The uinput kernel module is not loaded
  Injecting input below the display server needs /dev/uinput, which the uinput module provides.
      $ sudo modprobe uinput
      write /etc/modules-load.d/uinput.conf:
          uinput
```

### Try it without touching anything

`simulate` runs a scripted list of events through the real engine on a virtual clock. No
devices, no permissions, works anywhere — useful for checking what a set of flags does, and
for reporting a bug reproducibly.

```sh
sequencer simulate crates/sequencer-cli/tests/fixtures/hold.txt --until-ms 300
```

```
0 BD:left BU:left | 50 BD:left BU:left | 100 BD:left BU:left | 150 BD:left BU:left | 200 BD:left BU:left | 250 BD:left BU:left | 300 BD:left BU:left

14 actions, 7 repetitions, 0 skipped.
nothing left held.
```

Groups read `<milliseconds> <actions>`; `BD:left` is a press and `BU:left` a release. A
script is one event per line, `<milliseconds> <down|up> <key>`, `#` for comments:

```
# Hold F9 for just over half a second, then let go.
0    down f9
520  up   f9
```

`--dry-run` is the other no-op mode — it resolves the flags and explains them without
opening a device:

```sh
$ sequencer clicker --cps 25 --key f --dry-run
f key press at 25/s, while f9 is held (no limit). f8 quits.

dry run: nothing was sent to any application.
```

### Differences from the Python prototype

`contrib/clicker.py` is the original, kept as the behavioural reference. Behaviour matches
it — including toggle mode flipping on key *release* — with three intentional exceptions:

- `--verbose` prints structured progress rather than a per-click line and a `|`-per-wait bar.
- `--cps 0` is a usage error (exit 2) instead of a division by zero.
- `--key_press` still works, but is spelled `--key` in help.

The prototype also could not have worked on Wayland: `pynput`'s global listener is X11-only.

## Platform support

Linux only, today. The engine is platform-independent and the backend interface is in
place, but only the Linux backend is written.

| Platform | Capture | Inject | Status |
|---|---|---|---|
| Linux — X11, Wayland, console | `/dev/input` | `/dev/uinput` | works |
| Windows | `WH_KEYBOARD_LL` | `SendInput` | not written |
| macOS | `CGEventTap` | `CGEventPost` | not written |

The workspace still cross-compiles for Windows and macOS — the backend is target-gated, so
those builds simply contain none of it and every command that needs a device says so.

There is no X11-specific backend and none is planned: XTEST and XInput2 would cover only
X11 sessions, which the current backend already handles, at the cost of a second keymap
and a second self-echo problem.

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

`Command` also derives clap's `Subcommand`, so it can be nested in another parser — use
the re-exported `sequencer_cli::clap` rather than depending on clap directly, so the two
cannot end up on different major versions.

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

The engine is also more general than the clicker built on it: it runs sequences of steps
with loops, waits and per-binding repeat and cancellation policies. That is what the
clicker is expressed in, not a promise about what comes next — richer tools may end up as
a separate system rather than grown from here.

## Development

```sh
cargo test --workspace --all-features    # 156 tests, all headless
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Tests that need a real `/dev/uinput` skip themselves with a note when it is absent, so the
suite is green on a container and exercises the device on a real machine.

CI runs the suite plus fmt, clippy and docs on Linux; cross-compiles for Windows and
macOS, which is the proof `sequencer-core` is OS-free; checks every feature combination
with `cargo hack`; and runs `cargo deny`, since the project is GPL-3.0-or-later and a
dependency's licence is a correctness matter.

## Limitations

- **Synthesised input is detectable.** Events come from a virtual device with a
  recognisable name and no physical device behind it. Anything protected by Vanguard, EAC
  or BattlEye will not work and using it there risks the account. Don't.
- **Clicks land at the pointer and cannot move it.** The virtual device declares buttons
  and keys but no pointer axes, which is what lets it click without touching the cursor.
- **The trigger key is not hidden** from the focused application. Hiding it would mean
  grabbing the keyboard exclusively and re-injecting every other keystroke, which puts this
  process between you and your keyboard — if it hangs, the keyboard stops responding until
  it is killed from another virtual terminal. Not a trade worth making here.
- **`input` group membership is required**, with the keylogging implications described
  above.
- **Linux only** for now.

## Licence

GPL-3.0-or-later. See [LICENSE](LICENSE).
