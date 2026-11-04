#!/usr/bin/env bash
# SUDO-TEST.sh — thin launcher for the live test suite. All test logic lives in Rust:
# crates/sequencer-cli/tests/live/main.rs and the tier modules beside it.
#
# It caches a sudo ticket (device-backend tests only — decline and they skip), runs
# the headless suite, then the live suite serially. The live tests refuse to start
# while a real sequencer manager is running and isolate all state in temp dirs. What
# they type, and where: Pause at your desktop (the X11 tests, in an X11 session; in a
# Wayland session only their Ctrl+C test types, Pause, while the window tests' routing check
# sends XTEST keys into boxes of its own); keys on a test keyboard of their own, which only
# their own managers hold, read back before anything else sees them (the device tests);
# and on KDE, text into kdialog boxes they open themselves, each key only while KWin says
# that box has focus (the window tests). Your own keyboards are never grabbed.
set -e
cd "$(dirname "${BASH_SOURCE[0]}")"

# Refuse to start while a real manager runs: its grabs would fight the live tests'
# (the x11 tests check this too, but failing before any compile is friendlier).
config="${SEQUENCER_CONFIG_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}/sequencer}"
if pid="$(cat "$config/manager.pid" 2>/dev/null)" && [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
  echo "a sequencer manager is already running (PID $pid, config $config)."
  echo "quit it first — Ctrl+C in its terminal, or unapply its profiles — then re-run."
  exit 1
fi

echo "sudo is used only by the device-backend tests; Ctrl+C the prompt to skip them."
sudo -v || true

echo "== pass 1/2: headless suite (compiling first — a fresh tree takes a few minutes of silence) =="
cargo test --workspace --all-features
echo "== pass 2/2: live suite — hands off keyboard and mouse for the ~2 minutes it runs =="
if [ -n "${WAYLAND_DISPLAY:-}" ]; then
  echo "   One exception, in this Wayland session: KDE may ask to allow remote control when the"
  echo "   window tests first inject a key. Click Allow — they wait up to a minute for it."
fi
exec cargo test -p sequencer-cli --features live-tests --test live -- --ignored --test-threads=1 --nocapture
