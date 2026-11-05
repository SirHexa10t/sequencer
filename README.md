# sequencer

## TL;DR

Read `example_profile.toml`. It specifies and demonstrates everything.

## autokey macro profiling

A profile is a TOML file of key bindings, and `sequencer profile-apply` enforces it until
you stop it. A binding either remaps a key or chord to another, or runs a sequence — taps,
holds, waits, loops and random choices — when its trigger is pressed. Bound keys are
consumed, so the application sees only what the key was bound to. Profiles stack, each can
be limited to certain programs or keyboard layouts, and each can name its own emergency
stop key.

```toml
[defaults]
emergency_stop = "ctrl shift e"   # unapplies this profile

[binds.PgUp]
bind = "volume-up"                # remap: PgUp turns the volume up

[binds.F6]
loop = "inf"                      # press F6 to start, again to stop
gap = "50ms"
seq = ["mouse1"]                  # a left click every 50 ms
```

Profiles run on Linux, in X11 and Wayland sessions: on the input devices themselves where
they can be opened, which makes a profile whole in either, and on X11's key grabs where
they can't. [How it reaches the screen](#how-it-reaches-the-screen) says what each costs;
[Applying a binds profile](#applying-a-binds-profile) covers applying, stacking and
stopping them; `example_profile.toml` documents every field and step, and
`practical_profile_examples/` holds a few profiles for real use.

## Features

- **No catch-up, ever.** A loop's repetitions are paced by the gap between them, so one
  that ran late is simply late — a busy machine never builds up a debt that comes out as
  a burst.
- **Nothing gets left held down**, including after a panic: every run releases what it
  pressed on every way out, and a drop guard covers the unwinding one — tested with a
  sink that gives up mid-sequence. The profile manager has the same guard one level up:
  a panic in its loop releases every key, frees its grabs and unapplies what it was
  enforcing, exactly as Ctrl+C does.
- **Whole profiles on Wayland.** Holding the input devices, a profile hears every trigger
  and keeps it from every app, native Wayland ones included — not only the keys the
  compositor would pass to X11 apps.
- **Mouse buttons as triggers.** On the input devices a profile can bind a mouse's side
  buttons — or any of its buttons — and the mouse is held only while some profile names
  one, through a stand-in that keeps the desktop's settings for it.
- **Your keyboards always come back.** A supervisor process holds every device grab and
  gives the keyboards back if the manager stops answering for two seconds, at once if it
  dies — tested live, `kill -9` included.
- **Out of the way at the lock screen.** While the screen is locked, profiles pause and
  every key goes through as typed, so a password reaches the lock screen untouched.
- **Nothing to configure.** What needs the devices borrows sudo for that one run — root
  is dropped the moment they are open — rather than widening what every program on the
  machine can do; and where the devices can't be had, profiles fall back to X11's grabs,
  which need no permission at all.
- **`doctor`** reports what setup is missing and the exact commands to fix it, and which
  input devices a profile would hold.
- **`bench`** measures what the machine really delivers, by reading its own virtual device
  back rather than trusting its own send count.
- **`profile-apply`** applies a binds file — key remaps, and sequences triggered by a key
  or a mouse button — until stopped. `example_profile.toml` in the repository is the annotated format reference.
- **`detect-key`** prints the name of every key you press, in the exact spelling a binds
  file accepts — reading the input devices for exact answers by default, or the terminal
  with `--no-sudo` for zero-permission use anywhere.

## How it reaches the screen

Two backends, each serving the commands that need it:

| | Out | In | Needs | Used by |
|---|---|---|---|---|
| **devices** | a virtual device, through `/dev/uinput` | `/dev/input`: held for profiles, only read by `detect-key` | device access — sudo for one run, or a standing setup; see [Setup](#setup) | `profile-apply` (first choice), `detect-key`, `bench` |
| **X11** | XTEST | passive key grabs | an X server | `profile-apply`, where the devices can't be opened |

`sequencer doctor` prints which one profiles get, as its `backend:` line. Whether there is
an X server is decided by trying to connect to one — not by reading `$DISPLAY`, which
survives the session that set it.

**The device path sits below every display server**, so it reaches X11, Wayland and the
console alike. For a profile it holds every device that sends keys — keyboards, a mouse's
keyboard interface (where the mouse's own macros type), macro pads, media, volume and
brightness keys — and types what no profile claims straight back out through one virtual
keyboard, so a trigger is kept from every application and everything else arrives as
typed. A mouse or a pointing stick is held only while some applied profile names one of
its buttons as a trigger, and passes on through a stand-in of its own: a virtual device
with its name and IDs, so the desktop's settings for that mouse — speed, scroll
direction, handedness — still apply (with one exception, under
[Limitations](#limitations)). Left alone, each for its own reason: touchpads, tablets,
touchscreens, game controllers and motion sensors, which would lose their own handling to
a stand-in (palm detection, gestures, games reading controllers directly); devices that
also report switches, such as a lid; devices nothing in the session reads; and power and
sleep buttons, which are logind's. Which device is which follows udev's own rules, and
`doctor` lists every device with its verdict.

A program sitting between you and your keyboard must never trap it, so the grab is made
safe four ways. Every grab is taken and held by a supervisor process — the same
program, run again — which gives the devices back if the manager stops answering for two
seconds, and at once if it dies. A device is taken only once no key is held on it, since a
key held across the grab would stay down in the session for good. sequencer's own virtual
devices are never taken, so nothing it types comes back as your input. And should the
thread reading the devices end while the manager runs on — a failed read, a bug — it gives
them back first. While the
screen is locked — logind's `LockedHint`, which KDE's and GNOME's lockers set — profiles
pause and every key goes through as typed.

Its costs are two. The permission: in a Wayland session `profile-apply` asks sudo for the
one run and drops root before it writes anything; in an X11 session it never asks, and
uses the devices only where you already have access. And libinput: everything written
passes through it on the way up to an application, and it is fussy about what it will
route. That is not a small detail; see [Rate ceiling](#rate-ceiling).

A keyboard plugged in while the manager runs — or one that comes back from a suspend as a
new device — is held within about a second, by the same rule, and so is a mouse a profile
names. After sudo the manager keeps only the `input` group, which opens them on most
distributions; where the device nodes are root's alone, a keyboard plugged in later types
as usual, without profiles, until the manager runs again — and since mice are opened only
once a profile names them, no mouse button can be a trigger there.

**The X11 path is the fallback, and needs nothing.** It is an ordinary X client: it opens
no device, so there is no group to join and nothing for sudo to do. It also sits *above*
libinput, which is where a display server's own automation API lives — the layer
`xdotool` and pynput use. On X11 it makes a profile whole. In a Wayland session it runs
through Xwayland, and native Wayland apps see only part of a profile: while one has focus,
a trigger fires only on the keys the compositor passes to X11 apps, and a bound key still
reaches the app. The manager says so when it starts that way, and `doctor` says what holds
on the machine at hand.

## Setup

A Rust toolchain to build. `detect-key`'s exact mode, `bench` and a whole profile in a
Wayland session also need some way to reach the input devices — `sudo` for one run is
enough. No system libraries, no `-dev` packages, no BIOS settings: `x11rb` speaks the X
protocol itself rather than linking `libX11`, and the questions put to KWin, KDE's
keyboard service and logind go over a D-Bus client of the project's own. Every command
below runs as typed in bash, zsh and fish.

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

The binary lands at `target/release/sequencer`. To build it and put it on your `PATH` in
one go — in `~/.cargo/bin`, which rustup's installer already added:

```sh
cargo install --locked --path crates/sequencer-cli
```

### 3. Input device access

`detect-key` reads the input devices for exact answers, `bench` writes a virtual one and
reads it back, and a profile holds the keyboards and types through a virtual one. Two ways
to reach them, and they trade off against each other. `sequencer doctor` tells you where
you stand either way.

**Per run (no setup).** Just run the command. When the devices are needed but not
reachable and you are at a terminal, it explains itself and asks for your password via
sudo — for that run only. Root lasts exactly as long as opening the devices takes; the
process then drops back to your user and runs the whole session unprivileged. Nothing
persists after it exits, and no other program gains anything at any point. A sudo ticket
the run created is revoked afterwards; one you already had is left alone. Pipelines are
never prompted: without a terminal, `detect-key` and `bench` fail with these instructions
instead, and a profile runs on X11's grabs. Nor is a password asked for that couldn't
help: where `/dev/uinput` doesn't exist, root can't make a virtual device either, so
`bench` fails with the fix and a profile runs on X11's grabs until the module is loaded.

`profile-apply` asks only in a Wayland session, and only the run that becomes the
manager: on X11 the X server's own grabs make a profile whole without the devices, and a
later `profile-apply` just hands its file to the manager already running.

**Standing (no password, ever).** `sequencer doctor` prints the exact commands for whatever
your machine is missing. In full:

```sh
# The uinput module, for creating virtual devices.
sudo modprobe uinput
echo uinput | sudo tee /etc/modules-load.d/uinput.conf     # persist across reboots

# Let the `input` group use it.
printf '%s\n' 'KERNEL=="uinput", GROUP="input", MODE="0660", OPTIONS+="static_node=uinput"' | sudo tee /etc/udev/rules.d/99-sequencer-uinput.rules >/dev/null
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

### 4. Optional: a USB mouse's DPI while a profile holds it

While a profile names one of its buttons, a mouse reaches the session through a stand-in
with its name and IDs. udev lists the DPI of some mice, which libinput scales motion by,
and gives it to a USB mouse only through the USB device above it — which a stand-in
hasn't, so such a mouse moves at another speed while held. To see whether yours is listed,
run this on the mouse's node (`sequencer doctor` names it); a line printed means it is:

```sh
udevadm info --query=property --name=/dev/input/event4 | grep MOUSE_DPI
```

This rule gives a USB mouse's stand-in its bus, and udev looks it up as it did the mouse.
It touches nothing but sequencer's stand-ins, and applies to those made after the reload:

```sh
printf '%s\n' 'ACTION!="remove", SUBSYSTEM=="input", KERNEL=="event*", KERNELS=="input*", ATTRS{phys}=="sequencer-stand-in", ATTRS{id/bustype}=="0003", ENV{ID_BUS}="usb"' | sudo tee /etc/udev/rules.d/69-sequencer-stand-in.rules >/dev/null
sudo udevadm control --reload-rules
```

The same rule, explained, is `udev/69-sequencer-stand-in.rules` in this repository, for
packaging. A Bluetooth mouse needs nothing: udev reads that bus from the stand-in itself.

### Optional developer tools

Only for contributing; CI installs its own copies.

```sh
cargo install cargo-hack   # checks every feature combination compiles
cargo install cargo-deny   # licence and advisory checks
```

The live suite's window tests also need KDE's `kdialog`; see [Development](#development).

## How to run

`sequencer --help` lists the subcommands; each has its own `--help`. There is no default
subcommand — more modes are planned, and one of them silently winning would get harder to
read as they arrive.

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
you press — once per press, in exactly the spelling a binds file accepts.

By default it reads the input devices, because that is the only exact answer: every key
reports as itself — modifiers pressed alone, media keys and mouse buttons included, and
kp-multiply never mistaken for the `*` it types. That needs read access to `/dev/input`,
so it may ask for sudo for that one run (see [Setup](#setup)); it never needs uinput,
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

Reports the session; what profiles run on — the devices, as you or through sudo, or X11's
grabs, and in a Wayland session without the devices, what native Wayland apps then miss;
who answers which program has focus and which layout is active; the X server's keyboard
state; and each device requirement, with the exact fix for anything failing. Then every
input device the kernel lists and what a profile manager does with it — holds it, or
leaves it alone and why — asked of sysfs, so that it needs no access to the devices.

Exit code `0` when the checks pass, `1` otherwise — except where profiles still run
without them, through sudo or on X11's grabs: there a failing device check is advice, and
the exit code stays `0`. The `backend:` line promises sudo only where root could open the
devices, which a missing `/dev/uinput` rules out. From a container — no X server, and sysfs lists the host's devices while
`/dev` has no nodes for them — with the device names made generic, up to the first of its
three fixes:

```
sequencer 0.8.0  (linux x86_64)
session: Linux virtual terminal
backend: none — profiles need the input devices, which need setting up first (see below), or an X server, and none answered

[fail] uinput kernel module: /dev/uinput does not exist
[fail] /dev/uinput writable: /dev/uinput: No such file or directory (os error 2)
[fail] /dev/input keyboards readable: 2 of 2 keyboard devices cannot be read — Wireless Mouse Keyboard (event5): No such file or directory (os error 2); Office Keyboard (event6): No such file or directory (os error 2)

input devices — what a profile manager holds:
  held  USB Audio Device (event2): keys
  held  Video Bus (event3): keys
  held  Wireless Mouse (event4): a mouse — while a profile names one of its buttons
  held  Wireless Mouse Keyboard (event5): a keyboard
  held  Office Keyboard (event6): a keyboard
  held  Office Keyboard Mouse (event9): a mouse — while a profile names one of its buttons
  left  Power Button (event0, event1): a power or sleep button — logind's
  16 more send no keys (-v lists them)

The uinput kernel module is not loaded
  Injecting input below the display server needs /dev/uinput, which the uinput module provides.
      $ sudo modprobe uinput
      write /etc/modules-load.d/uinput.conf:
          uinput
```

`-v` adds the `DISPLAY` / `WAYLAND_DISPLAY` / `XDG_SESSION_TYPE` values it read, lists the
devices that send no keys too, and gives udev's tags for each device, to set beside
`udevadm info`.

### Applying a binds profile

```sh
sequencer profile-apply my-binds.toml     # first call becomes the manager
sequencer profile-apply gaming.toml       # from any shell: adds to the running set
sequencer profile-unapply gaming          # removes; with no names, an interactive picker
sequencer profile-check my-binds.toml     # validate without applying; --format tidies it
```

Profiles stack dynamically. Each `profile-apply` validates the file and links it into
`~/.config/sequencer/active/` — **the directory is the set**: `ls` shows what's applied,
and removing a link (by hand or with `profile-unapply`) takes it out of play within a
moment. The first invocation finds no live manager, takes a PID lock and becomes one,
managing everything in the directory; later invocations see the lock and just report.
A crashed manager's stale lock is detected and replaced, and a manager whose set
empties out — every profile unapplied or emergency-stopped — quits on its own.
Ctrl+C on the manager stops everything: every held key is released **and the set is
emptied** — `active/` describes what a running manager is enforcing, so it never outlives
one. Ctrl+C is caught rather than fatal, precisely so that release happens.
`SEQUENCER_CONFIG_DIR` overrides the state location.

What a profile runs on is settled as the manager starts ([How it reaches the
screen](#how-it-reaches-the-screen)), and the manager says which. In a Wayland session the
first `profile-apply` asks sudo to open the devices — saying first what for, unless you
already hold a ticket — and sheds root before it writes anything; turn sudo down and the
run ends there. A real start, with a ticket cached (device names and paths made generic):

```
Using your cached sudo to open the input devices; root is dropped straight after.
applied: /home/you/.config/sequencer/active/letter.toml (1 binds)
managing /home/you/.config/sequencer/active; apply/unapply from any shell, Ctrl+C stops everything
holding: Office Keyboard, Wireless Mouse Keyboard, USB Audio Device, Video Bus; if this manager stops answering, they are given back within a few seconds
profile applied: letter.toml (1 binds)
to stop this script, press: F8
active: letter.toml
```

A `warning:` follows the `holding:` line when no keyboard is among the devices held — a
profile would hear nothing typed. Where the devices can't be used, the manager runs on
X11's grabs instead; in a Wayland session it says so once, and why, since through
Xwayland native Wayland apps see only part of a profile.

The format — mirrors like
`PgUp -> volume-up` (each press taps the target once, however long it is held — and a chord
trigger fires on its exact combination, lifting any modifier the target does not name
for the instant of the tap and pressing it straight back, so a held shift never
recolours what gets typed),
sequences with
`PRESS`/`RELEASE`/`WAIT` steps, chords, per-bind
timing, `loop` counts (`4`, or `"inf"` until the trigger is pressed again), `RNG`/`GNR`
chance blocks (`0.25` == `25%` == `1/4`), `bind = "NOP"` to swallow a key and emit
nothing, and the two session gates — `program` and `kb_lang`, applying the profile only
while the focused program, or the active keyboard layout, matches (`!` inverts a
pattern) — is documented by example in
[`example_profile.toml`](example_profile.toml), which doubles as the parser's test
fixture, so the documentation cannot drift from what is accepted.

With a gate set, the run watches it (~20×/s) and takes or lets go of the profile's
triggers as the match comes and goes — a dormant profile eats no keys. `program` matches
the window class and `kb_lang` the active layout's own name (`us`, `il`); `sequencer
detect-key` prints both, as `focus:` and `lang:` lines, which is how you learn the
spelling to write. On X11 the X server answers both. In a Wayland session, where Xwayland
sees only X11 windows and names only the first layout, KDE answers instead: KWin names the
focused window — a Wayland app by its app id, such as `org.kde.konsole` — through a small
script sequencer loads into it and unloads after, and KDE's keyboard service names the
layout. Elsewhere Xwayland's partial view is all there is. Together the gates express
things neither does alone: `kb_lang = "!us"`
with a handful of `NOP` binds refuses to type in one program while the keyboard is on
another language, and leaves those keys untouched everywhere else. The
`emergency_stop` key keeps its own grab either way; pressing it unapplies exactly the
profiles that named that key — nothing is global among scripts — and releases whatever
they still pressed. A stopped `loop` does the same for its own keys.

Bound keys are consumed: the application sees what the key was bound to, never the key
itself. Validation is strict and speaks the format's language — a `PRESS` nobody releases,
two spellings of one trigger (`{` and `[` are the same key), or a `suppress = false`
nothing can honour are refused with the reason, not reinterpreted.

What a profile sends is heard like typing: one profile's output can fire another's trigger,
on X11's grabs and on the devices alike. `profile-apply` refuses a set whose binds would
trigger each other in a circle, and on the devices a chain is cut eight links deep besides,
for any loop the check can't see.

Chord triggers work: `[binds."ctrl i"]` fires on `i` with exactly Ctrl held, whatever
NumLock and CapsLock are doing. A chord is modifiers plus one ordinary key — the shape an
X grab can express, and the device backend matches by the same rules, so a profile means
the same on either. A bare key and chords over it coexist: each spelling claims its own
exact combination, so `i`, `ctrl i` and `alt i` are three independent binds, and a
combination none of them claims reaches the application untouched. `shift` distinguishes
sides — `rshift >` and `shift >` can be separate binds, routed by which side is physically
down.

A mouse button can be a trigger too — `[binds.mouse4]`, or a chord such as
`[binds."ctrl mouse5"]` — on the input devices, which hold the mouse while the profile
applies. X11's key grabs can't hear a button, so the fallback refuses such a profile and
says why. `profile-check` warns when the left or right button (`mouse1`, `mouse2`) is a
trigger: while the profile applies, that click goes to the bind and never to the app under
the pointer, which a side button (`mouse4`, `mouse5`) leaves alone. Some mice send their
side buttons as the browser's back and forward keys instead; `detect-key` names what a
press is, and those bind as `browser-back` and `browser-forward` — keys, which both
backends hear.

`profile-check` runs the same validator without applying anything: no symlink, no lock,
no grabs, so it is safe in an editor hook. `--format` rewrites sound files in place —
keywords uppercased, chord modifiers first, operands column-aligned, RNG blocks indented
— preserving every comment, and it is idempotent.

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

Both virtual devices sequencer ships — `bench`'s, and the virtual keyboard a profile types
through — therefore advertise `REL_X`, `REL_Y`, `REL_WHEEL` and `REL_HWHEEL`, and never
send any of them. Four unused axes are the difference between clicks that arrive and
clicks that vanish.

The X11 backend sidesteps all of this by injecting above libinput rather than below it,
which is why it was written: before the axes fix, the device path could not hold much past
20-30/s. That specific problem is now fixed at the source, so the two are no longer
obviously far apart — but the X11 backend stays regardless, because its other property is
the one that matters more. **It needs no device access**, and that is not something the
axes fix or any other change to the device path can produce: it is what lets a profile run
where the devices can't be opened.

Whether the fixed device path now matches XTEST's rate is untested. `bench` cannot answer
it: it reads the device node, which is *below* libinput, so it measures the one layer that
was never the problem. Answering it means real clicks against something that counts them.

## Using it from another project

`sequencer-cli` is a library as well as a binary:

```toml
[dependencies]
sequencer-cli = { git = "https://github.com/SirHexa10t/sequencer", branch = "main" }
```

Every flag is a public field and each args struct has a `new()` with the command-line
defaults, so struct-update syntax survives a flag being added later:

```rust
use sequencer_cli::{Command, DetectKeyArgs};

let args = DetectKeyArgs { no_sudo: true, ..DetectKeyArgs::new() };
let exit_code: u8 = Command::DetectKey(args).run();
```

`Command` also derives clap's `Subcommand`, so it can be nested in another parser — use the
re-exported `sequencer_cli::clap` rather than depending on clap directly, so the two cannot
end up on different major versions.

To offer the same per-run sudo flow the binary has, call
`sequencer_cli::run_with_sudo_prompt(&cli, "yourtool doctor")` instead of `run_cli`; the
second argument is how *your* command line spells the doctor command, so the advice printed
is runnable.

**A program that runs profiles must route the supervisor.** The device backend's
supervisor is the running program started again, with `sequencer_cli::SUPERVISOR_MARKER`
as its first argument, so `main` hands its arguments to `supervise_if_asked` before
anything parses them:

```rust
fn main() -> std::process::ExitCode {
    if let Some(code) = sequencer_cli::supervise_if_asked(std::env::args_os()) {
        return code.into();
    }
    // ...then parse and run as usual — here, the whole command line:
    sequencer_cli::run(std::env::args_os()).into()
}
```

Both exist with the `evdev` feature, on Linux. A program that doesn't route the marker
gets no supervisor, and without one no grab — never a grab nobody watches — so its
profiles fall back to X11's grabs.

On KDE, `sequencer_cli::input::kde` also answers what Xwayland can't: `windows()` lists
every window KWin manages, Wayland ones included, and `close()` asks windows to close — each
through a script KWin runs once and sequencer unloads after. `invoke_kwin_shortcut()` runs
one of KWin's own global shortcuts by name ("Increase Opacity"), as its keys would.

The library never calls `process::exit`, never installs a logger or signal handler, and
never reads `std::env::args` for you. It logs through the `log` facade, so whatever logger
your program installs receives its messages; `init_logging` is the binary's own.

The two backends are separate features, both on by default. Profiles are built on `xtest`
and use the devices as well when `evdev` is in. `--features cli,xtest` is X11-only —
profiles on X11's grabs, and `detect-key` through the terminal — with no evdev in your
dependency graph and no device access anywhere in it; `--features cli,evdev` is the
reverse: `detect-key`'s exact mode and `bench`, but no profiles. `live-tests` is test
machinery for the live suite, never needed downstream.

## How it is put together

| Crate | What it holds |
|---|---|
| `sequencer-core` | The vocabulary: keys as positions, events, actions, time. `#![no_std]`, one dependency, no clock, no threads, no I/O. |
| `sequencer-input` | The backends: the input devices (read, held and written), X11, and the desktop services that answer what X11 can't — KWin and KDE's keyboard service, and logind for the screen lock, over D-Bus. |
| `sequencer-cli` | The clap surface, the binds format and its executor, the profile manager, session-mode sudo, and the `sequencer` binary. |

`sequencer-core` has no path to an OS input API in its dependency graph and never reads a
clock, and the executor takes its clock as a parameter — so the tests hand it a virtual one.
That is what makes "the release was noticed mid-wait, not after the 10 s `WAIT`" an exact
assertion that runs in microseconds. Timing is the whole ballgame for synthetic input, and a
test suite that has to sleep in real time is one nobody runs.

Within `sequencer-cli`, one product's worth of behaviour lives in its own directory
(`profile/`, `detect_key/`, `write_script/`), while the binds format and its executor
(`binds/`) stay general. The executor runs one binds profile — taps, holds, waits, loops,
chance. Within `sequencer-input`, `linux/` is the device backend — the grab, the router
deciding which key goes where, and the supervisor among it — `x11/` the X11 one, and
`kde/` the KWin and keyboard-service readers; `devices.rs` is udev's device
classification, which the grab, `detect-key` and `doctor` share.

## Development

```sh
cargo test --workspace --all-features    # 379 tests, all headless
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo doc --workspace --no-deps --all-features
```

Tests that need a real `/dev/uinput` skip themselves with a note when it is absent, so the
suite is green in a container and exercises the device on a real machine. Tests that need a
D-Bus bus start a private `dbus-daemon` of their own and skip the same way when there is
none; `SEQUENCER_TEST_BUS_DAEMON` names another bus daemon to use instead (busd works).

What the headless suite cannot prove lives in the `live` test target
(`crates/sequencer-cli/tests/live/`), whose hardware-touching tests are `#[ignore]`d so a
plain `cargo test` stays safe anywhere. Its tiers, by what each needs:

| Tier | Needs | Covers |
|---|---|---|
| `validation` | nothing — it runs in every `cargo test` | `profile-check`, `--format` and the profile commands' arguments, through the real binary |
| `x11` | an X session | the manager's lifecycle on X11's grabs — stacking, mirrors, loops, per-profile emergency stops, quitting once the set empties — and Ctrl+C. In a Wayland session the lifecycle test skips: the `devices` tier tells the same story there, and the routing check below covers XTEST |
| `sudo` | a cached sudo ticket | `bench` round-tripping through uinput, `doctor` on the real machine |
| `devices` | a Wayland session, sudo, the `live-tests` feature | profiles on the device backend, typed on a test keyboard of the suite's own and read back below the desktop; the manager's lifecycle there — stacking, a loop, re-applying, per-profile stops, quitting once the set empties; a keyboard plugged in again; a test mouse's button as a trigger, the rest of the mouse read back through its stand-in, and the mouse held only while a profile names it; the supervisor taking the keyboard back from a silent manager and from a killed one |
| `windows` | the above, plus KDE, `kdialog` and a US layout | the same profiles into kdialog boxes the tests open — an X11 one through Xwayland and a Wayland one — and the routing check: XTEST keys reaching an X11 grab and the focused window |

The test keyboard and mouse are made through `/dev/uinput` by a root helper,
`sequencer-live-rig`, built only with `live-tests`, and each test manager's grab is
limited to them, so your own keyboards and mice are never grabbed. The test mouse clicks
and moves only once its stand-in is held by the test, so none of it reaches your pointer. The window tests type a key only while KWin says the test's
own box has focus, so a focus slip stops the test instead of typing elsewhere. The only
keys that reach your desktop are the `x11` tier's Pause chords and, on KDE, the `windows`
tier's, into boxes of its own — where KDE may ask once to allow remote control. Run
them all via [`SUDO-TEST.sh`](SUDO-TEST.sh) — a thin launcher that caches the sudo ticket
the device tiers use (decline and they skip), runs the headless suite, then the live one —
or directly:

```sh
cargo test -p sequencer-cli --features live-tests --test live -- --ignored --test-threads=1 --nocapture
```

They refuse to start while a real manager is running — the device tiers also while a
sequencer device from an earlier run is still alive — and keep all state in temp
directories.

CI runs the suite plus fmt, clippy and docs on Linux; cross-compiles for Windows and macOS,
which is the proof `sequencer-core` is OS-free; checks every feature combination with
`cargo hack`; tries a real uinput round trip on a VM runner, not yet gating merges; and
runs `cargo deny`, since the project is GPL-3.0-or-later and a dependency's licence is a
correctness matter.

## Platform support

Linux only. The core and the executor are platform-independent and the workspace cross-compiles for Windows
and macOS — the backends are target-gated, so those builds contain none of them and every
command that needs a device says so — but no backend for either is written.

| Platform | Status |
|---|---|
| Linux — X11 | profiles on the input devices where you have access, else XTEST + key grabs with no setup; `detect-key` and `bench` through the devices |
| Linux — Wayland | profiles on the input devices, through sudo for one run, whole in every app; without the devices, through Xwayland with the limits `doctor` lists. On KDE the gates see every window and layout. `detect-key` and `bench` through the devices |
| Linux — console | `detect-key` and `bench` (uinput + evdev; need device access); profiles too where you have device access, though `profile-apply` asks sudo only in a Wayland session |
| Windows | not written (`WH_KEYBOARD_LL` + `SendInput` would be the shape) |
| macOS | not written (`CGEventTap` + `CGEventPost` would be the shape) |

The device backend has run live in a KDE Plasma Wayland session; in X11 sessions and on
the console it is built and tested headless, not yet tried live.

## Limitations

- **Synthesised input is detectable.** On the device path events come from a virtual device
  with a recognisable name and no hardware behind it; on X11 they carry the XTEST device as
  their source. Anything protected by Vanguard, EAC or
  BattlEye will not work, and using it there risks the account. Don't.
- **Clicks land at the pointer.** The tool never moves the cursor.
- **What is sent is not necessarily received.** See [Rate ceiling](#rate-ceiling).
- **Mouse buttons are triggers on the input devices only.** The X11 fallback refuses a
  profile that names one, saying why.
- **A USB mouse listed in udev's mouse database can move at another speed while a profile
  holds it**, unless [the optional udev rule](#4-optional-a-usb-mouses-dpi-while-a-profile-holds-it)
  is installed. udev gives a mouse the DPI that database lists for it — which libinput
  scales motion by — only on a USB or Bluetooth bus, and takes a USB one's bus from the USB
  device above it, which a stand-in has none of. The desktop's own settings for the mouse
  still apply, and a Bluetooth mouse keeps its entry.
- **Where device nodes are root's alone, a keyboard plugged in after a sudo start types
  without profiles** until the manager runs again, and no mouse button can be a trigger;
  elsewhere both are held within a second.
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
doctor` reports both on its `keyboard:` line — in a Wayland session, as Xwayland sees them.

- A **stuck modifier** — one the server believes is held with no key holding it — makes
  every chord arrive with an extra modifier, so shortcuts match nothing while typing looks
  fine. XTEST can leave one behind; the device backend's virtual keyboard can't, since
  removing it releases everything it held. `doctor` names it and says how to clear it: tap
  that modifier on *both* sides (only the stuck side clears it), or re-run your
  `setxkbmap` command, which resets locks, latches and the active layout group in one go.
- **Locks** (`CapsLock`, `NumLock`) toggle session-wide state that no teardown undoes —
  and on X11 the toggle happens even when a grab takes the key. On a multi-language setup
  `CapsLock` is often the layout switch, so a bind on it can leave the keyboard on another
  language. `sequencer profile-check` warns when a bind presses a lock key, as trigger or
  as target; take the hint and pick another.

**Prevention.** Quit the manager with `Ctrl`+`C` or an `emergency_stop` key rather than
`kill -9`: both release every key the run is holding, and `SIGKILL` skips exactly that —
on the device backend the supervisor still gives the keyboards back at once, but on X11
whatever XTEST held stays down (the manager also warns on the way out if the server still
has a modifier stuck). Don't bind lock keys. Run `sequencer profile-check` on a new profile
first — it catches unbalanced `PRESS`es, feedback circles and lock keys before anything is
grabbed.

## Licence

GPL-3.0-or-later. See [LICENSE](LICENSE).
