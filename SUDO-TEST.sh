#!/usr/bin/env bash
# SUDO-TEST.sh — thin launcher for the live test suite. All test logic lives in Rust:
# crates/sequencer-cli/tests/live.rs (+ live/validation.rs, live/x11.rs, live/sudo.rs).
#
# It caches a sudo ticket (device-backend tests only — decline and they skip), runs
# the headless suite, then the live suite serially. The live tests refuse to start
# while a real sequencer manager is running, isolate all state in temp dirs, and the
# only key they ever synthesize at your desktop is Pause.
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

cargo test --workspace --all-features
exec cargo test -p sequencer-cli --test live -- --ignored --test-threads=1 --nocapture
