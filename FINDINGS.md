# Findings — sequencer on KDE Plasma Wayland

Temporary: the record behind the fixes in progress. Delete it once they land. Each claim
says what it rests on: **code** (read in this checkout), **KWin** (KWin's own source),
**run** (a test result), or **hypothesis**.

## How the tests ran

Each test applied one small profile, then had keys pressed in two message windows in
turn: an X11 app (kdialog under `QT_QPA_PLATFORM=xcb`, so it ran through Xwayland) and
the same app as a native Wayland client. What text reached each window was recorded and
compared with a prediction made from the code and KWin's source beforehand.

## The setup tested

- CachyOS, Plasma 6.8.1 (the Wayland-only release), KWin 6.8.1, Xwayland 24.1.14, kdialog 26.08.2.
- Wayland session: `XDG_SESSION_TYPE=wayland`, `DISPLAY=:0` (Xwayland), `WAYLAND_DISPLAY=wayland-1`.
- KWin's `[Xwayland]` settings in `kwinrc`: none set, so the defaults apply —
  `XwaylandEavesdrops=Combinations`, `XwaylandEavesdropsMouse=false`, `XwaylandEisNoPrompt=false`.
- Keyboard layouts: `us`, `il`. Xwayland's `_XKB_RULES_NAMES`: `"evdev", "pc105", "us", "", ""` —
  the first layout only.
- The test account was not in the `input` group.
- sequencer 0.6.0, a debug build of this checkout.

## How sequencer behaves in this session

1. **It runs as an X11 client, through Xwayland.** (code) `x11::is_usable()` checks for an X
   server by connecting to one; Xwayland answers, so `profile-apply` runs on the X11 pair —
   XTEST for output, passive key grabs for triggers. `doctor` says as much: `session: XWayland
   …`, `backend: X11`. `profile-apply` is compiled for X11 only; its README: "Wayland arrives
   with the device backend".
2. **KWin passes X11 apps only some keys while a Wayland window has focus.** (KWin: `kwin.kcfg`,
   `xwayland.cpp`) With `Combinations`: every non-character key (Qt key codes from Escape up —
   F-keys, PgUp, arrows, modifiers, media keys — except input-method and dead keys), and any key
   while Ctrl, Alt or Meta is held. Never a plain letter, digit or symbol, nor Shift+letter;
   no mouse buttons. And only **copies**: the Wayland app receives the key too, so a grab can't
   swallow it there.
3. **sequencer's output goes through KWin's gate.** (KWin, run T1) Xwayland hands XTEST input to
   KWin over EIS (libei); KWin asks once to allow "remote control". Allowed, the input reaches
   the focused window, of either kind; refused, or with a broken prompt, it's dropped silently.
4. **Focus is Xwayland's view.** (code, run T7) The program gate and `detect-key`'s `focus:` line
   read Xwayland's `_NET_ACTIVE_WINDOW` and that window's `WM_CLASS`. With a Wayland window
   focused, Xwayland's active window was `0x200000`, which has no class or name — so the focus
   reads as unknown, and a gated profile goes dormant (`licensed()` treats unknown as no match;
   never a false positive). `detect-key` then prints no new `focus:` line, so the last X11
   app's name stays on screen.
5. **The layout is Xwayland's view, and Xwayland names only `us`.** (code, setup, run T8) The
   `kb_lang` gate reads Xwayland's XKB group and `_XKB_RULES_NAMES`.
6. **The modifier lift waits for Xwayland, not for KWin.** (code) When a chord fires while its
   modifier is held, `LiftedTap` reads Xwayland's keymap, releases the held modifiers with
   XTEST, waits for a `GetInputFocus` round trip ("proves the server has processed every lift"),
   taps the output, and presses the modifiers back. Through Xwayland, that round trip proves
   only that Xwayland passed the release on to KWin — not that KWin applied it.
7. **`doctor`'s "/dev/input readable" passes if any one device opens.** (code: `probe.rs`
   `check_evdev_readable`) And `detect-key`'s device capture listens to whatever devices it can
   open (`capture.rs` `openable_devices`), without saying which — a keyboard the user can't
   read is simply never heard.

## Test results (the run of 2026-10-26 05:50)

| Test | Profile | X11 window: typed (predicted) | Wayland window: typed (predicted) |
|------|---------|-------------------------------|-----------------------------------|
| T1 | `F9 → x`, F8 stops | `xxx` (`xxx`) | `xxx` (`xxx`) — KDE asked first; F8 stopped both |
| T2 | `j → x` | `xxx` (`xxx`) | `jjj` (`jjj`) |
| T3 | `ctrl j → x` | **`xx`** (`xxx`) | **`""`** (`xxx`) |
| T4 | `shift j → x` | **`Xxx`** (`xxx`) | `JJJ` (`JJJ`) |
| T5 | `F9 → h i shift+1` | **`hi1`** (`hi!`) | `hi!` (`hi!`) |
| T6 | `ctrl k → NOP` | `abc` (`abc`) | `""` (`""`) |
| T7 | `program = "*kdialog*"` | `xxx` (`xxx`) | `""` (`""`) |
| T8 | `kb_lang = "us"` | **`xxסס`** (`xx`) | `xxסס` (unknown) |

- **D1** (`detect-key`): printed `lang: us` and `focus: kdialog` and **no key names at all**
  over a minute. (The test's windows were also closed early, by accident.)
- **Observed while testing T3/T4**: a chord fired when the modifier went down with the key, but
  not on a second press of the key with the modifier still held.

## The run of 2026-10-26 06:08 — modifiers held throughout

T1–T8 repeated, this time with the modifier held down throughout each chord test — and every
result came out **identical** to the run before: T3 `xx` / `""`, T4 `Xxx` / `JJJ`, T5 `hi1` /
`hi!`, T8 `xxסס` / `xxסס`, the rest as predicted. The failures are deterministic, not
fumbles. (T9 — Ctrl held across three fires — didn't run separately: with the modifier held
throughout, this run's T3 answers it.)

- **Input devices**: the keyboard and the mouse are **unreadable**; the only readable devices
  are two game controllers, which the session grants its user.
- **D1, in terminal mode** (`detect-key --no-sudo`, as the keyboard is unreadable): reported
  `a` and `f9`, and nothing for modifiers alone — terminal mode working as designed. `focus:
  kdialog` came for the X11 window and nothing new for the Wayland one; `lang:` stayed `us`
  through the layout switch. (The `>` at the end of its log is `ESC >`, the keypad-mode reset
  `detect-key` sends on exit — not a key.)

## The runs of 2026-10-26 06:25 and 06:28 — D2 and T10

- **T10** (`F9` → Shift held 100 ms on either side of `1`): `!` in **both** kinds of window —
  where T5's `shift 1`, the same keys with no pause, gave `1` in the X11 window.
- **D2** (`sudo -- sequencer detect-key`, the way sequencer's own sudo flow runs it — devices
  opened as root, then root dropped): every key named, sides told apart — `a`, `shift`,
  `rshift`, `ctrl`, `rctrl`, `alt`, `ralt`, `meta`, `F9`, `volume-up`, `mouse1`, `mouse2` — and
  Ctrl+C quit it. `focus: kdialog` and `lang: us` printed (so the X display reached it through
  sudo), and nothing new for the Wayland window or after Alt+Shift switched the layout —
  Xwayland's view, as in D1.

## The run of 2026-11-03 — after the first fixes

sequencer 0.7.0, the checkout's build. T7, T8 and D1 run twice, with the same results; D2
once, with extra clicks and scrolls to move between windows.

- **T7** (`program = "*kdialog*"`): `xxx` in **both** kinds of window — the Wayland one too,
  where the gate used to stay dormant.
- **T8** (`kb_lang = "us"`): `xx` in both — the bind held back under `il`, where it used to
  fire. The Wayland run's log shows the profile go dormant on the switch to `il` and come
  back on the switch to `us`.
- **D1** (`detect-key --no-sudo`): `focus:` names the X11 window `kdialog`, the Wayland one
  `org.kde.kdialog` and the terminal `org.kde.konsole`; `lang:` follows the switch to `il`
  and back, in each window.
- **D2** (`sudo -- sequencer detect-key`): the same `focus:` and `lang:` lines through sudo,
  which drops the session's variables — so the Xwayland test and the bus found at
  `/run/user/<uid>/bus` both work. Every key named, sides included, and
  `org.kde.plasmashell` when Meta opened the launcher.
- **doctor**: `backend: X11 through Xwayland`, the note quoting KDE's setting in force
  (the default), `focus: KWin`, `layout: KDE's keyboard service`, and the keyboard check
  failing on the two keyboard devices by name — the mouse's keyboard interface among them,
  which reports a full keyboard's keys.

## Diagnosis so far

Confirmed:

- Plain triggers and outputs work in both kinds of window, once KDE's prompt is allowed (T1).
- What KDE doesn't pass on never fires in a Wayland window — letters, Shift+letter (T2, T4) —
  and nothing is swallowed there (T6). As designed by KWin.
- The program gate stays dormant in Wayland windows (T7): safe, but no per-program binds for
  native Wayland apps.
- `kb_lang` is blind: Xwayland names only the first layout, and its group didn't follow the
  switch to Hebrew, so the bind fired under `il` too, in both kinds of window (T8). (The `ס` is
  by design: outputs are physical keys, and the `x` key types `ס` in the Hebrew layout.)

Hypotheses, under test:

- **T3–T5 — the modifier lift races KWin** (from 6): the output is tapped before KWin has
  released the modifier (Ctrl+X instead of `x` in the Wayland window; `X` instead of `x`), the
  pressed-back modifier leaves KWin and Xwayland disagreeing whether it's held (repeats with
  it held don't match the chord's grab), and a chord's own Shift reaches KWin late (`hi1`). T9
  (Ctrl held across three fires) and T10 (Shift held 100 ms around the key) test this.
  **Partly confirmed**: T10 shows modifier changes need time to take effect through Xwayland,
  which is what the lift doesn't give them; why only the first lifted tap is recoloured in an
  X11 window, and every one in a Wayland window, remains unexplained.
- ~~**D1 — the keyboard is unreadable**, so `detect-key` heard nothing (from 7).~~ **Confirmed**
  (06:08 run): the keyboard is unreadable, and the two readable game controllers satisfy
  `doctor`'s check — so `detect-key` listened to the controllers alone, in silence.

What the 06:08, 06:25 and 06:28 runs add:

- **Shifted output loses its Shift to timing — confirmed** (T10). A chord's keys are pressed
  back to back; through Xwayland each one goes to KWin and back, and the Shift isn't in effect
  yet when the key arrives. Held 100 ms around the key, it is, in both kinds of window.
- **`detect-key`'s device mode is reliable once it can read the keyboard** (D2): exact names,
  sides, media keys, mouse buttons. What fails is getting there — issue 1 below keeps
  sequencer's own sudo flow from starting.

- With Shift held throughout (T4, X11 window), all three fires produced output, and only the
  **first** kept its Shift (`X`). T3's `xx` reads the same way: the first output kept its Ctrl
  — Ctrl+X, which types nothing — and the next two came out right. So: the **first** lifted tap
  after the modifier goes down is recoloured, later ones aren't. That fits the lift racing KWin
  on the first fire; why the later ones come out right is not established.
- In the Wayland window, no Ctrl-chord output ever came out plain (T3: `""` both runs) — the
  lift never takes effect there.
- T5's shifted output loses its Shift in the X11 window only, both runs — timing, as T10 then
  showed.

## Issues found in sequencer

1. The device-readability check passes on any one readable device, so `doctor` says "ok" and
   `detect-key` listens in silence while the keyboard is unreadable.
2. In a Wayland session it takes the X11 backend whenever Xwayland answers, with the limits
   above; `doctor` reports the Wayland session and picks X11 regardless.
3. `kb_lang` can't tell layouts apart through Xwayland on KDE.
4. Its README spells the unapply command `unprofile-apply`; it's `profile-unapply`.
5. Through Xwayland, modifier changes are timing-sensitive, and sequencer doesn't wait for them:
   a chord in an output loses its modifier (T5, fixed by a pause in T10), and the lift that
   clears a held modifier comes too late for the first output (T3, T4) — or, in a Wayland
   window, every time.

## Status — what full Wayland support still needs

The numbering used since the first round of fixes. "Fixed" means in the code, with tests;
items marked *live check* still need a run on the KDE machine.

1. **Fixed (2026-10-27):** the device check now needs every keyboard readable (found
   through sysfs, by udev's keyboard rule), so `doctor` fails and `detect-key` asks for
   sudo when the keyboard is unreadable; a capture with no keyboard says so. `doctor`'s
   failure confirmed live (2026-11-03); the sudo prompt of a plain `detect-key` not yet
   seen.
2. **Fixed (2026-10-27), confirmed live (2026-11-03):** `doctor` says when profiles run
   through Xwayland and what native Wayland apps then miss, naming KDE's own setting; the
   manager says so once at start.
3. **Fixed (2026-10-27), confirmed live (2026-11-03):** `kb_lang` and `detect-key`'s
   `lang:` line ask KDE's keyboard service in a Wayland session, and follow every switch
   (T8, D1, D2).
4. **Fixed (2026-10-27):** the README spells the unapply command `profile-unapply`.
5. Modifier changes need time through Xwayland: chord outputs lose their modifier, and
   the lift of a held one comes too late (T3–T5, T10).
6. Character-key triggers (letters, digits, symbols, Shift+letter) never fire while a
   Wayland window has focus: KDE passes only what its Xwayland setting allows (T2, T4).
7. Binds can't block keys in Wayland windows: the app gets the key too (T6).
8. **Fixed (2026-10-27), confirmed live (2026-11-03):** the program gate and
   `detect-key`'s `focus:` line ask KWin in a Wayland session, through a script loaded
   for the run, and see Wayland windows by their app id (T7, D1, D2).
9. Output depends on KDE's one-time "remote control" permission; refused, it's dropped
   with no error, and sequencer can't tell (T1).
10. Mouse-button triggers can't fire in Wayland windows (KWin's source; untested).
11. `doctor`'s `keyboard:` line and the manager's stuck-modifier warning read Xwayland's
    keyboard state, which may not match KWin's (code; untested).
12. Other compositors (GNOME, Sway, Hyprland) are untested; whether "full" means KDE only
    is still to decide.
13. No automated tests of any Wayland path.
14. The README describes the Xwayland limits now; it needs the full story once the route
    for 5–7 and 9–11 is settled.

## Open questions

- The design for Wayland: keep the X11 backend and document its limits; a device backend for
  `profile-apply` (needs input access, and swallowing would mean intercepting the whole
  keyboard); or KDE's own routes (global shortcuts through kglobalaccel, output through libei).
- Whether a pause after each modifier change — what T10 did by hand — is enough to make chords
  and lifts reliable through Xwayland, and how long it needs to be.
