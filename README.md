# sequencer

## TL;DR

Read `example_profile.toml`. It specifies and demonstrates everything.

## Clicker

Hold or toggle a key to click at a rate you choose, wherever the mouse already is.

It works on X11, on Wayland and on a bare console. On X11 it needs no setup and no
password at all; elsewhere it goes below the display server, which is what needs
permission. There is no rate ceiling picked in advance: ask for what you want, and the tool
reports what it actually managed to send.

## autokey macro profiling

Yet to write

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
- **Nothing to set up on X11.** An X11 run is an ordinary X client: no group to join, no
  device to open, no password. Off X11 it can borrow sudo for a single run rather than
  making you widen what every program on the machine can do.
- **`doctor`** reports what setup is missing and the exact commands to fix it.
- **`bench`** measures what the machine really delivers, by reading its own virtual device
  back rather than trusting its own send count.
- **`profile-apply`** applies a binds file — key remaps and key-triggered sequences —
  until stopped. `example_profile.toml` in the repository is the annotated format reference.
- **`detect-key`** prints the name of every key you press, in the exact spelling a binds
  file accepts — reading the input devices for exact answers by default, or the terminal
  with `--no-sudo` for zero-permission use anywhere.

## How it reaches the screen

Two backends. Which one a run uses is decided at startup by trying to connect to an X
server — not by reading `$DISPLAY`, which survives the session that set it. `sequencer
doctor` prints the answer as its `backend:` line.

| | Clicks out | Hotkeys in | Needs |
|---|---|---|---|
| **X11** | XTEST | passive key grabs | nothing |
| **everything else** | `/dev/uinput` | `/dev/input` | device access — see [Setup](#setup) |

The two halves are always chosen together. A mixed pair would take the permission cost of
one backend and the behaviour of the other, so there is no configuration that produces one.

**The X11 path is the reason there is no setup step on X11.** It is an ordinary X client:
it opens no device, so there is no group to join and nothing for sudo to do. It also sits
*above* libinput, which is where a display server's own automation API lives — the layer
`xdotool` and pynput-based clickers use.

**The device path is the only one that reaches Wayland and the console**, because it sits
below all of them. Its cost is the permission, and that everything written passes through
libinput on the way up to an application — and libinput is fussy about what it will route.
That is not a small detail; see [Rate ceiling](#rate-ceiling).

## Setup

A Rust toolchain to build. On X11 that is the whole list; off X11 you also need some way to
reach the input devices. No system libraries, no `-dev` packages, no BIOS settings — `x11rb`
speaks the X protocol itself rather than linking `libX11`.

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

The binary lands at `target/release/sequencer`.

### 3. Input device access — off X11 only

**On X11 there is nothing to do here.** Skip to [How to run](#how-to-run); the run goes
through the X server and opens no device. This section is for Wayland and the console.

Two ways, and they trade off against each other. `sequencer doctor` tells you where you
stand either way.

**Per run (no setup).** Just run `sequencer clicker`. When the devices are needed but not
reachable and you are at a terminal, it explains itself and asks for your password via sudo — for
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

On exit it reports what it **sent** — repetitions, actions, the rate that works out to and
the rate you asked for — with "not all may arrive" right on the line, deliberately never
"achieved". Every number is counted as this process hands events to the backend; what an
application ends up acting on can be lower, and this tool cannot see that from the inside.
`bench` is the one that measures what came back out. If the machine could not keep up it
also says how many repetitions were skipped, and a final `ran 12.3s` line is the stopwatch
for the session.

Two hold durations matter and are separately tunable: `--button-hold-ms` (default 8) and
`--key-hold-ms` (default 1). A click of zero duration is not a click as far as most
applications are concerned.

**Whether the trigger key also reaches the focused application depends on the backend.** On
X11 it does not — the grab is exclusive, and only for the keys you named. On the device
path it does: pressing F9 both starts the clicker and sends F9 to whatever has focus.
Hiding it there would mean grabbing the whole keyboard and re-injecting every other
keystroke, which puts this process between you and your keyboard. Off X11, pick a trigger
the target application ignores.

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
focused.

### What is this key called?

```sh
sequencer detect-key             # exact: reads the input devices (may ask for sudo)
sequencer detect-key --no-sudo   # terminal reader: no permissions, works anywhere
```

Prints an illustrated keyboard and the full list of key names, then the name of every key
you press — once per press, in exactly the spelling a binds file (and the other flags)
accepts.

By default it reads the input devices, because that is the only exact answer: every key
reports as itself — modifiers pressed alone, media keys and mouse buttons included, and
kp-multiply never mistaken for the `*` it types. That needs read access to `/dev/input`,
so it may ask for sudo the same one-run way the clicker does; it never needs uinput,
since this mode only reads. The terminal is silenced while it runs, so presses don't also
type into your shell — and Ctrl+C still quits.

`-n`/`--no-sudo` reads the terminal instead: no permission on X11, Wayland, SSH and the
console alike, and the press is consumed just the same. The trade is that a terminal only
receives characters, so it reports the key behind the char (`{` prints `[`, exactly as a
binds file reads it), keys that type nothing print nothing, and with NumLock on `kp8` is
indistinguishable from `8`.

### Checking the machine

```sh
sequencer doctor
```

Reports the session, the backend a run would pick, and each requirement, with the exact fix
for anything failing. Exit code `0` when a run would work, `1` otherwise — and on X11 that
is `0` even with every device check failing, because the run does not touch them. From a
container with no input devices and no X server:

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

### Applying a binds profile

```sh
sequencer profile-apply my-binds.toml     # first call becomes the manager
sequencer profile-apply gaming.toml       # from any shell: adds to the running set
sequencer unprofile-apply gaming          # removes; with no names, an interactive picker
sequencer profile-check my-binds.toml     # validate without applying; --format tidies it
```

Profiles stack dynamically. Each `profile-apply` validates the file and links it into
`~/.config/sequencer/active/` — **the directory is the set**: `ls` shows what's applied,
and removing a link (by hand or with `unprofile-apply`) takes it out of play within a
moment. The first invocation finds no live manager, takes a PID lock and becomes one,
managing everything in the directory; later invocations see the lock and just report.
A crashed manager's stale lock is detected and replaced, and a manager whose set
empties out — every profile unapplied or emergency-stopped — quits on its own.
Ctrl+C on the manager stops everything: every held key is released **and the set is emptied** — `active/` describes
what a running manager is enforcing, so it never outlives one. Ctrl+C is caught rather
than fatal, precisely so that release happens. `SEQUENCER_CONFIG_DIR` overrides the
state location.

The format — mirrors like
`PgUp -> volume-up` (each press taps the target; holding repeats it — and a chord
trigger fires on its exact combination, lifting any modifier the target does not name
for the instant of the tap and pressing it straight back, so a held shift never
recolours what gets typed),
sequences with
`PRESS`/`RELEASE`/`WAIT` steps, chords, per-bind
timing, `loop` counts (`4`, or `"inf"` until the trigger is pressed again), `RNG`/`GNR`
chance blocks (`0.25` == `25%` == `1/4`), and a `program` pattern that applies the
profile only while a matching program has focus — is documented by example in
[`example_profile.toml`](example_profile.toml), which doubles as the parser's test
fixture, so the documentation cannot drift from what is accepted.

With `program` set, the run watches focus (~5×/s) and grabs or releases its triggers as
the matching program comes and goes — a dormant profile eats no keys. The
`emergency_stop` key keeps its own grab either way; pressing it unapplies exactly the
profiles that named that key — nothing is global among scripts — and releases whatever
they still pressed. A stopped `loop` does the same for its own keys.

Bound keys are consumed: the application sees what the key was bound to, never the key
itself. Validation is strict and speaks the format's language — a `PRESS` nobody releases,
two spellings of one trigger (`{` and `[` are the same key), or a `suppress = false`
nothing can honour are refused with the reason, not reinterpreted.

Chord triggers work: `[binds."ctrl i"]` grabs the key with its modifier mask (and the
NumLock/CapsLock variants, so locks don't break it). A chord is modifiers plus one
ordinary key — the shape an X grab can express. A bare key and chords over it coexist:
each spelling grabs its own exact mask, so `i`, `ctrl i` and `alt i` are three
independent binds, and a combination none of them claims reaches the application
untouched. `shift` distinguishes sides — `rshift >` and `shift >` can be separate
binds, routed by which side is physically down.

`profile-check` runs the same validator without applying anything: no symlink, no lock,
no grabs, so it is safe in an editor hook. `--format` rewrites sound files in place —
keywords uppercased, chord modifiers first, operands column-aligned, RNG blocks indented
— preserving every comment, and it is idempotent.

X11 only for now: triggers are heard by key grab and output goes through XTEST, so it
needs no device access and no password. Wayland arrives with the device backend.

### write-script

A placeholder. It prints what it will be and exits zero; the format and its parser are not
written yet.

## Rate ceiling

`bench` measures how fast the *device* backend can write to the kernel, and that number is
real. It is **not** the same as how many clicks an application will register, and it says
nothing about the X11 backend, which has no device to read back from.

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

The X11 backend sidesteps all of this by injecting above libinput rather than below it,
which is why it was written: before the axes fix, the device path could not hold much past
20-30/s. That specific problem is now fixed at the source, so the two are no longer
obviously far apart — but the X11 backend stays regardless, because its other property is
the one that matters more. **It needs no device access**, and that is not something the
axes fix or any other change to the device path can produce.

Whether the fixed device path now matches XTEST's rate is untested. `bench` cannot answer
it: it reads the device node, which is *below* libinput, so it measures the one layer that
was never the problem. Answering it means a real clicker against something that counts
clicks.

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

The two backends are separate features, both on by default, and either alone is a complete
build. `--features cli,xtest` is X11-only with no evdev in your dependency graph and no
device access anywhere in it; `--features cli,evdev` is the reverse, and covers every
session type at the cost of the permission.

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
(`clicker/`, `profile/`, `detect_key/`, `write_script/`), while the engine, the step IR, the run loop
and both input backends (`linux/` and `x11/`) stay general. The engine already runs sequences of steps with
loops, waits and per-binding repeat and cancellation policies; the clicker is one profile
expressed in that.

## Development

```sh
cargo test --workspace --all-features    # 262 tests, all headless
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo doc --workspace --no-deps --all-features
```

What the headless suite cannot prove — live grabs, injection reaching the desktop,
the manager's lifecycle, per-profile emergency stops, signal handling, the sudo-backed
device path — lives in the `live` test target (`crates/sequencer-cli/tests/live/`),
whose hardware-touching tests are `#[ignore]`d so a plain `cargo test` stays safe
anywhere. Run them on an X11 session via [`SUDO-TEST.sh`](SUDO-TEST.sh) (a thin
launcher that also caches the sudo ticket the device tests use), or directly:

```sh
cargo test -p sequencer-cli --test live -- --ignored --test-threads=1 --nocapture
```

They refuse to start while a real manager is running, keep all state in temp
directories, and the only key they synthesize at your desktop is Pause.

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
| Linux — X11 | XTEST + key grabs; no setup |
| Linux — Wayland, console | uinput + evdev; needs device access |
| Windows | not written (`WH_KEYBOARD_LL` + `SendInput` would be the shape) |
| macOS | not written (`CGEventTap` + `CGEventPost` would be the shape) |

## Limitations

- **Synthesised input is detectable.** On the device path events come from a virtual device
  with a recognisable name and no hardware behind it; on X11 they carry the XTEST device as
  their source. Anything protected by Vanguard, EAC or
  BattlEye will not work, and using it there risks the account. Don't.
- **Clicks land at the pointer.** The tool never moves the cursor.
- **The trigger key behaves differently per backend.** On the device path it reaches the
  focused application as well as triggering the run. On X11 it does not: a key grab is
  exclusive, so while the run is active the grabbed keys go to this process only. Neither
  is configurable.
- **What is sent is not necessarily received.** See [Rate ceiling](#rate-ceiling); the
  clicker's own report says "sent" for exactly this reason.
- **Linux only.**

## Troubleshoot

**If only one keyboard misbehaves, the keyboard is the cause.** Keycodes are assigned by
the device itself, below the kernel and below X. The layout, the modifier state and every
shortcut binding on the machine are *shared* between your keyboards — so a board that
sends the wrong codes misbehaves alone, and nothing you change in software will move it.
Signs: `Ctrl`+`Alt`+`T` (or any `Alt` shortcut) doing nothing while ordinary typing is
perfect, a layout toggle that will not fire, the top row acting like media keys.

The usual cause is a **mode latch in the keyboard's own firmware** — commonly a Mac/Windows
toggle, which swaps `Alt` with `Super` so every `Alt` shortcut silently becomes a `Super`
one; also an Fn-lock making the F-row media-first, or an onboard "gaming" profile with
remapped keys. These live in the board's memory, so they survive unplugging, rebooting and
every software remedy there is.

**Hold `Fn`+`Esc` for a few seconds.** That resets many external keyboards to their default
mode, and it is what cured a real case of `Alt` and `Super` being swapped. Other boards use
`Fn`+a letter, a dedicated profile key, or a reset done by holding `Esc` while plugging in —
worth a look at the model's manual. `Fn` itself is resolved inside the keyboard: it has no
evdev code and no X keycode, so no software can read it, press it, or undo what it latched.

**Confirm it in one press**, before changing anything else:

```sh
sequencer detect-key      # reads the devices, below X: it names what the keyboard really sends
```

Press the key labelled `Alt`. If it reports `meta`, the swap is in the hardware — and if
you have a second keyboard, pressing the same key there is the comparison that settles it.

**The other half: state the X server holds.** Two kinds outlive a run, and `sequencer
doctor` reports both on its `keyboard:` line.

- A **stuck modifier** — one the server believes is held with no key holding it — makes
  every chord arrive with an extra modifier, so shortcuts match nothing while typing looks
  fine. `doctor` names it and says how to clear it: tap that modifier on *both* sides (only
  the stuck side clears it), or re-run your `setxkbmap` command, which resets locks,
  latches and the active layout group in one go.
- **Locks** (`CapsLock`, `NumLock`) toggle server-wide state that no grab intercepts and no
  teardown undoes. On a multi-language setup `CapsLock` is often the layout switch, so a
  bind on it can leave the keyboard on another language. `sequencer profile-check` warns
  when a bind presses a lock key, as trigger or as target; take the hint and pick another.

**Prevention.** Quit the manager with `Ctrl`+`C` or an `emergency_stop` key rather than
`kill -9`: both release every key the run is holding, and `SIGKILL` skips exactly that (the
manager also warns on the way out if the server still has a modifier stuck). Don't bind lock
keys. Run `sequencer profile-check` on a new profile first — it catches unbalanced
`PRESS`es, feedback circles and lock keys before anything is grabbed.

## Licence

GPL-3.0-or-later. See [LICENSE](LICENSE).
