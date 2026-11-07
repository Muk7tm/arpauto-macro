# ARPAuto Macro

Repeated, timed mouse-button holds for Linux X11. ARPAuto presses a mouse button, keeps it
held for a configurable time, releases it, waits, and repeats. It is not an auto-clicker:
during the hold the button stays logically down (one press event, one release event).

ARPAuto Macro is a standalone project. It shares nothing (code, assets, configuration or
runtime) with any other application.

---

## Contents

1. [Requirements](#requirements)
2. [Building](#building)
3. [Installing and uninstalling](#installing-and-uninstalling)
4. [Using ARPAuto Macro](#using-arpauto-macro)
5. [Timing model and presets](#timing-model-and-presets)
6. [Hold calibration](#hold-calibration)
7. [Hotkeys](#hotkeys)
8. [Focus protection and safety](#focus-protection-and-safety)
9. [Input backends (XTest and uinput)](#input-backends)
10. [Configuration and profiles](#configuration-and-profiles)
11. [Sober / Roblox compatibility](#sober--roblox-compatibility)
12. [Testing](#testing)
13. [Limitations](#limitations)
14. [Project layout](#project-layout)

---

## Requirements

* Linux x86-64 with an **X11** session. Wayland sessions are not supported: under XWayland,
  global hotkeys and synthetic input reach at most other XWayland applications.
* An X server with the XTEST extension (every standard Xorg server has it).
* Runtime libraries: `libxkbcommon`, `fontconfig`, `freetype`, and OpenGL (Mesa). If
  OpenGL is unavailable, start with `SLINT_BACKEND=winit-software`.
* To build: Rust 1.85 or newer and a C toolchain.

## Building

```sh
cd arpauto-macro
cargo build --release --locked
./target/release/arpauto-macro
```

The first build compiles the Slint UI toolkit and takes a few minutes. The binary embeds
its fonts and icon and does not need this source tree at runtime.

## Installing and uninstalling

```sh
./packaging/install.sh            # installs to ~/.local (binary, icon, desktop entry)
PREFIX=/usr/local sudo -E ./packaging/install.sh   # system-wide
./packaging/uninstall.sh          # removes the program, keeps settings
./packaging/uninstall.sh --purge  # also deletes ~/.config/arpauto-macro
```

`install.sh` uses a prebuilt `dist/arpauto-macro` if present, otherwise it builds one.
Only files named `arpauto-macro*` are installed.

## Using ARPAuto Macro

The main window shows only:

* **Mouse button**: Left, Middle or Right;
* **Hold**: how long the button stays down (slider up to 500 ms, field up to 10 000 ms);
* **Release**: how long the button stays up before the next hold;
* **Repeat**: on repeats until stopped; off performs a single hold;
* **Start / Stop** (also on the hotkey, **F7** by default);
* the small **settings** button.

Typical use: point at the game and press **F7**; press **F7** again to stop. **F10** is an
emergency stop. If you click **Start** in the window instead, ARPAuto waits until another
window is focused and under the pointer, then locks onto it.

Settings are grouped in tabs:

| Tab       | Contents |
|-----------|----------|
| General   | Preset, hold and release duration, mouse button, repeat, repetitions (0 = until stopped), start delay, activation (toggle / hold to run), reset to defaults |
| Hotkeys   | Start/stop and emergency-stop hotkeys, typed or captured |
| Input     | Backend (XTest / uinput), focus protection, stop or pause when the target is lost, optional target window class with **Detect** |
| Profiles  | Save / load / rename / duplicate / delete configuration profiles, import and export |
| Calibrate | Guided search for the shortest hold your game recognizes |

Changes are saved automatically. Changing settings while running does not affect the run in
progress; the new settings apply from the next start.

## Timing model and presets

One cycle is: **press → hold → release → wait (release duration) → repeat**. The release
wait is skipped after the final hold of a finite run.

| Preset   | Hold   | Release |
|----------|-------:|--------:|
| Fast     | 60 ms  | 30 ms   |
| Balanced | 120 ms | 60 ms   |
| Stable   | 200 ms | 100 ms  |

These are starting points, not guaranteed values for any game. Use **Calibrate** and test
in the game.

Implementation: a single long-lived worker thread drives the cycle with monotonic
deadlines and 1 µs timer slack. Wake-up overshoot is absorbed instead of accumulating; a
stall of more than 1 ms re-anchors the schedule instead of bursting, so holds are never
shortened. Every XTest event is a checked request, so the hold is timed from the moment the
X server accepted the press. Waits are `recv_timeout` calls on the command channel, so a
stop interrupts a hold immediately. Idle CPU use is zero.

Measured on the development machine (X server timestamps, 1 ms resolution, Balanced
preset, both backends): holds 119–122 ms and releases 60–61 ms. During calibration, five
200 ms holds arrived at a test window as 200–201 ms.

## Hold calibration

*Settings → Calibrate* helps you find the shortest hold the game reliably reacts to.
**ARPAuto cannot observe the game, so you judge every test.**

1. Choose the first test hold (default 200 ms), holds per test (default 5) and the release
   between holds, then click **Start calibration**.
2. Click **Run test**, then click into the game window. Testing starts one second after the
   game is focused with the pointer over it, so your own click is finished first.
3. Watch the game and answer **Recognized** or **Not recognized** (or **Run again**).
4. ARPAuto doubles the hold until something is recognized, then bisects between the
   longest unrecognized and the shortest recognized hold until they are 5 ms apart.
5. It suggests the shortest recognized hold plus a 10 % margin (at least 5 ms). **Use …
   ms** applies it.

After each test the panel shows the hold durations actually measured on the X connection,
so you can tell timing problems apart from game-side recognition.

## Hotkeys

Defaults: **F7** start/stop, **F10** emergency stop. Change them under *Settings → Hotkeys*,
by typing (`F8`, `Ctrl+F7`) or capturing. Hotkeys work regardless of CapsLock, NumLock and
ScrollLock.

* **Toggle** mode: press to start, press again to stop.
* **Hold to run** mode: holds repeat only while the start key is held down.
* **Emergency stop** stops in any mode and releases the button.

Hotkeys are passive X key grabs. When one fires, the X server would send all keyboard input
to ARPAuto while the key is held, so the game would lose your movement keys. ARPAuto
releases that grab immediately and detects the physical key release by polling the
key-state bitmap. This also makes key auto-repeat harmless. If another application already
owns a hotkey, the Hotkeys tab reports the conflict.

## Focus protection and safety

* **Focus protection** (on by default): ARPAuto locks onto the window focused when it
  starts (after the start delay). It only presses the button while that window is focused
  **and under the mouse pointer**, because clicks go to the window under the pointer, not
  the focused one. If either condition breaks, it **stops** (default) or **pauses**: it
  releases the button and resumes with a full hold when the window is back.
* **Target window** (optional): a WM_CLASS substring such as `sober`. With it set, ARPAuto
  only starts on, and only clicks into, a matching window. **Detect** fills it in from the
  window you focus within 3 seconds; `xprop WM_CLASS` shows classes manually.
* ARPAuto never clicks while its own window is focused, even with protection off.
* The held button is tracked and released on stop, emergency stop, target loss, errors,
  window close, `SIGINT`/`SIGTERM`/`SIGHUP`, and worker panics. A target window that is
  closed while ARPAuto runs counts as a lost target, not an error.
* Only one instance can run at a time (lock file in `$XDG_RUNTIME_DIR`), and start
  requests while running are ignored.

## Input backends

**XTest** (default) sends synthetic button events through the XTEST extension. It needs no
permissions and no root.

**uinput** creates a virtual kernel mouse ("ARPAuto Macro virtual mouse …") whose events
look exactly like a physical mouse. Use it only if a program ignores XTest input. It needs
write access to `/dev/uinput`:

```sh
ls -l /dev/uinput
sudo cp packaging/60-arpauto-macro-uinput.rules /etc/udev/rules.d/
sudo udevadm control --reload && sudo udevadm trigger /dev/uinput
```

(or add yourself to a group that owns `/dev/uinput`, often `input`). ARPAuto waits until the
X server lists the new device before the first press; if that takes more than 3 seconds,
the run fails with a message instead of silently losing clicks.

Neither backend lets the application tell synthetic input from your own. For example, if
you physically hold the same button, the macro's release also releases yours.

## Configuration and profiles

* Settings: `$XDG_CONFIG_HOME/arpauto-macro/config.toml` (default
  `~/.config/arpauto-macro/config.toml`)
* Profiles: `~/.config/arpauto-macro/profiles/*.toml`

The file is human-readable TOML with a `version` field. Writes are atomic (temp file,
fsync, rename). Missing values take defaults, out-of-range values are clamped, and an
unparsable file is renamed to `config.toml.corrupt-<time>` and replaced with defaults. The
status line reports this.

A **profile** stores the mouse button, hold and release timing, repeat settings and start
delay. *Save* stores the current settings under a name, *Load* applies a profile, *Export*
writes the selected profile to a file or folder, and *Import* validates a file and adds it,
renaming on name clashes. Format:

```toml
format = "arpauto-profile"
version = 1
name = "Left 120/60"

[settings]
button = "left"          # left | middle | right
start_delay_ms = 0

[settings.timing]
hold_ms = 120
release_ms = 60

[settings.repeat]
enabled = true
count = 0                # 0 = until stopped
```

## Sober / Roblox compatibility

**Status: not verified in a game.** Button holds are delivered to X11 clients correctly:
one press and one release per hold, with server-measured timing, focus and pointer
protection tested against a real X server and both backends (see [Testing](#testing)).
Whether a particular game *acts* on synthetic input, and at which minimum hold, can only be
confirmed in the game. Successful event submission is not proof of game compatibility.

Suggested check with Sober:

1. Start Sober and join an experience where holding the chosen button has a visible
   effect.
2. In ARPAuto, set *Target window* with **Detect** while Sober is focused.
3. Run **Calibrate** with the default 200 ms first test. Answer honestly after each test.
4. Apply the suggested hold, then run normally and confirm every hold registers.
5. If nothing registers with XTest, switch the backend to uinput and repeat.

## Testing

```sh
cargo test                       # unit tests + hold-engine behaviour tests (no X server)
cargo test --test x11_delivery -- --ignored --test-threads=1
```

The default tests (52) cover key parsing, the timing scheduler, configuration recovery and
validation, profiles, the calibration search (convergence for many simulated thresholds),
and the hold worker against a mock backend: press/hold/release timing, single continuous
press per hold, stop latency, focus loss, the pointer leaving the target, pause/resume,
waiting for the pointer, own-window exclusion, class filtering, invalid settings,
calibration runs, single-worker guarantee, hold-to-run, emergency release, start-delay
cancellation, shutdown cleanup, snapshot isolation and closed windows.

The ignored `x11_delivery` tests need an X11 session. They open a test window, let the
window manager focus it, put the pointer over it (moved back afterwards), and verify what
the X server delivers:

* hold timing with XTest and uinput;
* the pointer leaving mid-hold (stop and release);
* closing the target window mid-hold;
* the full F7 hotkey path.

Don't touch the keyboard or mouse while they run (about 4 seconds).

## Limitations

* X11 only. No Wayland support.
* A `SIGKILL` (or power loss) cannot be intercepted. If the process is killed during a
  hold, the button may stay logically pressed until you click it.
* The pointer check compares top-level windows. Overlapping popups that belong to the game
  window are treated as "another window".
* Running together with other macro tools works as long as hotkeys differ.

## Project layout

```text
src/
  main.rs          CLI entry, logging, instance lock, signal mask
  app.rs           UI controller (Slint ↔ worker/hotkeys/config/profiles/calibration)
  hold_engine.rs   hold worker, state machine, cleanup, hold measurement
  calibration.rs   guided shortest-hold search
  timing.rs        deadline scheduler, timer slack
  input.rs         XTest and uinput mouse backends, keymap snapshot
  hotkeys.rs       global hotkeys, key capture, window detection
  safety.rs        focus and pointer tracking, held-button tracking, lock file, signals
  keys.rs          keysyms, modifiers, key names, mouse buttons
  config.rs        configuration, XDG paths, atomic save, recovery
  profiles.rs      configuration profiles, import/export
ui/                theme.slint, widgets.slint, state.slint, main.slint, settings.slint
assets/            icon, embedded Noto Sans fonts (SIL OFL 1.1)
packaging/         install/uninstall scripts, desktop entry, uinput udev rule
tests/             hold-engine behaviour tests, real-X11 delivery tests
```

License: MIT (see `LICENSE`). The bundled Noto Sans fonts are under the SIL Open Font
License 1.1 (`assets/fonts/OFL.txt`).
