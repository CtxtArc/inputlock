//! inputlock — lock keyboard and pointer on sway without hiding the screen.
//!
//! The screen stays live (videos play, terminals scroll, nothing sleeps);
//! all keyboard and mouse input is captured by a transparent overlay until
//! the user's password is typed and verified through PAM.

mod app;
mod pam;
mod render;
mod sway;

use std::ffi::CString;
use std::os::fd::AsRawFd;
use std::process::ExitCode;
use std::sync::mpsc;

use calloop::signals::{Signal, Signals};
use calloop::EventLoop;
use clap::Parser;
use smithay_client_toolkit::{
    compositor::CompositorState,
    output::OutputState,
    reexports::calloop_wayland_source::WaylandSource,
    reexports::client::{globals::registry_queue_init, Connection},
    registry::RegistryState,
    seat::SeatState,
    shell::wlr_layer::LayerShell,
    shm::Shm,
};
use zeroize::Zeroizing;

use app::{App, Config, Globals, InhibitData};
use render::Style;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Darken the screen while locked (0.0 = fully transparent, 1.0 = black).
    #[arg(short, long, default_value_t = 0.0, value_parser = parse_unit)]
    dim: f32,

    /// Don't draw the status indicator.
    #[arg(long)]
    no_indicator: bool,

    /// Keep the mouse cursor visible (it still can't click anything).
    #[arg(long)]
    show_cursor: bool,

    /// PAM service used to check the password (/etc/pam.d/<name>).
    #[arg(long, default_value = "inputlock")]
    pam_service: String,

    /// Empty sway binding mode to switch to while locked.
    #[arg(long, default_value = "inputlock")]
    sway_mode: String,

    /// Don't switch sway binding modes. Sway keybindings will keep working
    /// unless the compositor honours the shortcuts-inhibit protocol.
    #[arg(long)]
    no_sway_mode: bool,
}

fn parse_unit(s: &str) -> Result<f32, String> {
    let v: f32 = s.parse().map_err(|e| format!("{e}"))?;
    if (0.0..=1.0).contains(&v) {
        Ok(v)
    } else {
        Err("must be between 0.0 and 1.0".into())
    }
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("inputlock: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Holds an flock so two instances can't stack on top of each other.
fn single_instance() -> Result<std::fs::File, String> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").ok_or("XDG_RUNTIME_DIR is not set")?;
    let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".into());
    let path = std::path::Path::new(&dir).join(format!("inputlock-{display}.lock"));
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("already running".into());
    }
    Ok(f)
}

fn run(args: Args) -> Result<(), String> {
    // ---- preflight: everything that could leave the user stuck is checked
    //      before we grab any input.
    if !pam::service_exists(&args.pam_service) {
        return Err(format!(
            "PAM service '{}' not found in /etc/pam.d — refusing to lock \
             (you would not be able to unlock). Install the pam file or pass --pam-service.",
            args.pam_service
        ));
    }
    let user = pam::current_user().ok_or("cannot determine current user")?;
    let service = CString::new(args.pam_service.clone()).map_err(|_| "bad PAM service name")?;
    let _instance = single_instance()?;

    // ---- sway mode + watchdog (fork happens here, before any thread exists)
    let mode_guard = if args.no_sway_mode {
        None
    } else {
        let sway = sway::Sway::from_env().ok_or(
            "SWAYSOCK not set — not running under sway? Use --no-sway-mode to skip mode switching.",
        )?;
        if !sway::valid_mode_name(&args.sway_mode) {
            return Err(format!("invalid sway mode name '{}'", args.sway_mode));
        }
        let modes = sway.modes().map_err(|e| format!("sway IPC: {e}"))?;
        if !modes.iter().any(|m| m == &args.sway_mode) {
            return Err(format!(
                "sway mode '{0}' is not defined. Add this to your sway config and reload:\n\n    \
                 mode \"{0}\" {{\n        \
                 # sway ignores empty modes, so keep one do-nothing binding\n        \
                 bindsym --release XF86Launch9 nop\n    }}\n",
                args.sway_mode
            ));
        }
        let current = sway.current_mode().map_err(|e| format!("sway IPC: {e}"))?;
        let restore = if current == args.sway_mode { "default".to_string() } else { current };
        let wd = sway::spawn_watchdog(&sway, &args.sway_mode, &restore)
            .map_err(|e| format!("failed to start watchdog: {e}"))?;
        Some(sway::ModeGuard::new(sway, args.sway_mode.clone(), restore, wd))
    };

    // ---- PAM worker thread
    let (auth_tx, auth_rx) = mpsc::channel::<Zeroizing<String>>();
    let (res_tx, res_rx) = calloop::channel::channel();
    std::thread::Builder::new()
        .name("pam".into())
        .spawn(move || {
            for pw in auth_rx {
                let r = pam::authenticate(&service, &user, &pw);
                drop(pw);
                if res_tx.send(r).is_err() {
                    break;
                }
            }
        })
        .map_err(|e| format!("spawn PAM thread: {e}"))?;

    // ---- Wayland
    let conn = Connection::connect_to_env().map_err(|e| format!("wayland: {e}"))?;
    let (globals, event_queue) = registry_queue_init::<App>(&conn).map_err(|e| format!("wayland: {e}"))?;
    let qh = event_queue.handle();

    let g = Globals {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        compositor: CompositorState::bind(&globals, &qh).map_err(|_| "wl_compositor missing")?,
        layer_shell: LayerShell::bind(&globals, &qh)
            .map_err(|_| "compositor lacks wlr-layer-shell (sway/Hyprland/river required)")?,
        shm: Shm::bind(&globals, &qh).map_err(|_| "wl_shm missing")?,
        inhibit_mgr: globals.bind(&qh, 1..=1, InhibitData).ok(),
    };

    let mut event_loop: EventLoop<'static, App> =
        EventLoop::try_new().map_err(|e| format!("event loop: {e}"))?;
    let handle = event_loop.handle();

    WaylandSource::new(conn.clone(), event_queue)
        .insert(handle.clone())
        .map_err(|e| format!("event loop: {e}"))?;

    handle
        .insert_source(res_rx, |ev, _, app: &mut App| {
            if let calloop::channel::Event::Msg(r) = ev {
                app.on_auth_result(r);
            }
        })
        .map_err(|e| format!("event loop: {e}"))?;

    // SIGTERM/SIGINT/SIGHUP: exit cleanly (anyone who can signal us could kill us anyway).
    let signals = Signals::new(&[Signal::SIGTERM, Signal::SIGINT, Signal::SIGHUP])
        .map_err(|e| format!("signals: {e}"))?;
    handle
        .insert_source(signals, |_, _, app: &mut App| app.exit = true)
        .map_err(|e| format!("event loop: {e}"))?;

    let cfg = Config {
        style: Style { dim: args.dim, indicator: !args.no_indicator },
        hide_cursor: !args.show_cursor,
    };
    let mut app = App::new(g, cfg, mode_guard, auth_tx, handle);

    // Outputs/seats arrive as events; surfaces are created per output.
    while !app.exit {
        event_loop
            .dispatch(None, &mut app)
            .map_err(|e| format!("event loop: {e}"))?;
    }

    app.release();
    let _ = conn.flush();
    let _ = conn.roundtrip();

    if let Some(e) = app.fatal.take() {
        return Err(e);
    }
    Ok(())
}
