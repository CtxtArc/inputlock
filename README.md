# inputlock

Lock the **keyboard and mouse** on sway without hiding the screen. Nothing
blanks, sleeps or gets covered: videos keep playing, logs keep scrolling,
dashboards keep updating. All input is swallowed until you type your password.

```
┌──────────────── your desktop, still live ─────────────────┐
│                                                           │
│                                                           │
│                 ╭──────────────────────────╮              │
│                 │ 🔒  ● ● ● ● ●            │              │
│                 ╰──────────────────────────╯              │
└───────────────────────────────────────────────────────────┘
```

## How it works

| Piece | What it does |
|---|---|
| wlr-layer-shell **overlay** surface on every output | Transparent and full-screen, so every click, scroll and drag lands on it and is dropped. The cursor is hidden. |
| `keyboard_interactivity = exclusive` | All keys go to inputlock and no other window can take focus. |
| Empty sway **binding mode** | Sway runs its own keybindings *before* any client sees the key, so inputlock switches to an empty mode while locked. Without it, `$mod+Enter` etc. would still work. |
| keyboard-shortcuts-inhibit | A second guard against compositor shortcuts, where the compositor supports it. |
| **PAM** (`/etc/pam.d/inputlock`) | Checks the password the same way swaylock does, using fresh PAM attempts in a worker thread so the UI never freezes. |
| Watchdog process | Restores your previous sway mode however inputlock exits, even after `kill -9`, so you're never stuck with dead keybindings. |

Hot-plugged monitors are covered automatically. HiDPI scales are handled.
The password buffer is zeroized after use.

## Install (Arch)

```sh
makepkg -si
```

Then add this to `~/.config/sway/config` (also installed at
`/usr/share/inputlock/sway.conf`) and run `swaymsg reload`:

```
mode "inputlock" {
    # sway ignores empty modes, so keep one do-nothing binding
    bindsym --release XF86Launch9 nop
}
bindsym $mod+Shift+x exec inputlock
```

inputlock checks for that mode at startup and refuses to lock if it's
missing. It also refuses if the PAM file is missing, because otherwise you
couldn't unlock.

## Usage

```
inputlock                 # lock, fully transparent
inputlock --dim 0.3       # lock, darken the screen 30 %
inputlock --show-cursor   # keep the cursor visible (still can't click)
inputlock --no-indicator  # no on-screen pill at all
```

While locked: type your password and press **Enter**. **Backspace** deletes,
**Esc** or **Ctrl+U** clears. The pill colour shows the state: grey idle, blue
typing, yellow checking, red wrong.

The exit status is `0` once you've unlocked, so you can chain commands:
`inputlock && notify-send "welcome back"`.

Auto-lock input on idle without blanking:

```
exec swayidle -w timeout 600 'inputlock --dim 0.3'
```

## Security notes (read this)

inputlock is an **input lock**, not a security screen locker:

- **The screen stays visible.** That's the point, but it means anyone can read it.
- **VT switching** (Ctrl+Alt+F*n*) may still work. That only lands on a TTY
  login prompt, but the session itself keeps running.
- Anyone with a shell on the machine (ssh, another TTY) can kill it. That
  unlocks input, by design: a crashed lock should fail open, not trap you.
- If you need real protection, use `swaylock` (ext-session-lock), which
  the compositor guarantees can't be bypassed even if the locker crashes.
- **pam_faillock**: Arch locks an account for 10 min after 3 wrong
  passwords by default. That applies here too, same as swaylock.

If something goes badly wrong and your sway keybindings are dead, run this
from another TTY or over ssh:

```sh
SWAYSOCK=$(ls /run/user/$UID/sway-ipc.*.sock) swaymsg mode default
```

## Building without makepkg

```sh
cargo build --release
sudo install -Dm755 target/release/inputlock /usr/local/bin/inputlock
sudo install -Dm644 pam/inputlock /etc/pam.d/inputlock
```

Runtime dependencies: `pam` and `libxkbcommon`. Build needs `cargo`.
It works on any wlroots compositor with layer-shell (sway, river,
Hyprland…). Outside sway, use `--no-sway-mode`.
