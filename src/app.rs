//! Wayland client state: one transparent overlay layer-surface per output,
//! exclusive keyboard focus, pointer swallowed, shortcuts inhibited.

use std::sync::mpsc;
use std::time::Duration;

use calloop::timer::{TimeoutAction, Timer};
use calloop::{LoopHandle, RegistrationToken};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_registry,
    dispatch2::Dispatch2,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    reexports::client::{
        protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
        Connection, QueueHandle,
    },
    reexports::protocols::wp::keyboard_shortcuts_inhibit::zv1::client::{
        zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1,
        zwp_keyboard_shortcuts_inhibitor_v1::{self, ZwpKeyboardShortcutsInhibitorV1},
    },
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use zeroize::Zeroizing;

use crate::pam::AuthError;
use crate::render::{self, Phase, Style};
use crate::sway::ModeGuard;

const MAX_PASSWORD_BYTES: usize = 1024;

pub struct Config {
    pub style: Style,
    pub hide_cursor: bool,
}

struct LockSurface {
    output: wl_output::WlOutput,
    layer: LayerSurface,
    width: u32,
    height: u32,
    scale: i32,
    configured: bool,
    inhibitor: Option<ZwpKeyboardShortcutsInhibitorV1>,
}

impl Drop for LockSurface {
    fn drop(&mut self) {
        if let Some(i) = self.inhibitor.take() {
            i.destroy();
        }
    }
}

pub struct App {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    pool: SlotPool,
    inhibit_mgr: Option<ZwpKeyboardShortcutsInhibitManagerV1>,

    seat: Option<wl_seat::WlSeat>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    surfaces: Vec<LockSurface>,

    password: Zeroizing<String>,
    phase: Phase,
    /// Length shown while PAM checks a password we no longer hold.
    pending_len: usize,
    /// Attempts sent to the PAM thread that haven't been answered yet.
    in_flight: usize,
    ctrl: bool,
    auth_tx: mpsc::Sender<Zeroizing<String>>,
    wrong_timer: Option<RegistrationToken>,
    loop_handle: LoopHandle<'static, App>,

    cfg: Config,
    mode: Option<ModeGuard>,
    pub fatal: Option<String>,
    pub exit: bool,
    pub unlocked: bool,
}

pub struct Globals {
    pub registry_state: RegistryState,
    pub seat_state: SeatState,
    pub output_state: OutputState,
    pub compositor: CompositorState,
    pub layer_shell: LayerShell,
    pub shm: Shm,
    pub inhibit_mgr: Option<ZwpKeyboardShortcutsInhibitManagerV1>,
}

impl App {
    pub fn new(
        g: Globals,
        cfg: Config,
        mode: Option<ModeGuard>,
        auth_tx: mpsc::Sender<Zeroizing<String>>,
        loop_handle: LoopHandle<'static, App>,
    ) -> Self {
        let pool = SlotPool::new(1920 * 1080 * 4, &g.shm).expect("failed to create shm pool");
        App {
            registry_state: g.registry_state,
            seat_state: g.seat_state,
            output_state: g.output_state,
            compositor: g.compositor,
            layer_shell: g.layer_shell,
            shm: g.shm,
            pool,
            inhibit_mgr: g.inhibit_mgr,
            seat: None,
            keyboard: None,
            pointer: None,
            surfaces: Vec::new(),
            password: Zeroizing::new(String::new()),
            phase: Phase::Idle,
            pending_len: 0,
            in_flight: 0,
            ctrl: false,
            auth_tx,
            wrong_timer: None,
            loop_handle,
            cfg,
            mode,
            fatal: None,
            exit: false,
            unlocked: false,
        }
    }

    /// Restore sway's binding mode right away (also happens on drop / via watchdog).
    pub fn release(&mut self) {
        if let Some(m) = self.mode.as_mut() {
            m.release();
        }
        self.surfaces.clear();
    }

    fn add_output(&mut self, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        if self.surfaces.iter().any(|s| s.output == output) {
            return;
        }
        let surface = self.compositor.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Overlay,
            Some("inputlock"),
            Some(&output),
        );
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        layer.set_size(0, 0);
        layer.commit();

        let mut ls = LockSurface {
            output,
            layer,
            width: 0,
            height: 0,
            scale: 1,
            configured: false,
            inhibitor: None,
        };
        self.inhibit(&mut ls, qh);
        self.surfaces.push(ls);
    }

    fn inhibit(&self, ls: &mut LockSurface, qh: &QueueHandle<Self>) {
        if ls.inhibitor.is_some() {
            return;
        }
        if let (Some(mgr), Some(seat)) = (&self.inhibit_mgr, &self.seat) {
            ls.inhibitor =
                Some(mgr.inhibit_shortcuts(ls.layer.wl_surface(), seat, qh, InhibitData));
        }
    }

    fn engage_mode(&mut self) {
        if let Some(m) = self.mode.as_mut() {
            if let Err(e) = m.engage() {
                // Don't hold the user's input hostage if we couldn't disable
                // sway's bindings: that's a broken, half-working lock.
                self.fatal = Some(format!("could not switch sway mode: {e}"));
                self.exit = true;
            }
        }
    }

    fn draw(&mut self, idx: usize) {
        let s = &self.surfaces[idx];
        if !s.configured || s.width == 0 || s.height == 0 {
            return;
        }
        let scale = s.scale.max(1) as u32;
        let (w, h) = (s.width * scale, s.height * scale);
        let stride = w as i32 * 4;
        let (buffer, canvas) =
            match self.pool.create_buffer(w as i32, h as i32, stride, wl_shm::Format::Argb8888) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("inputlock: buffer allocation failed: {e}");
                    return;
                }
            };
        let len = if self.phase == Phase::Verifying && self.password.is_empty() {
            self.pending_len
        } else {
            self.password.chars().count()
        };
        render::draw(canvas, w, h, scale, self.cfg.style, self.phase, len);

        let surf = s.layer.wl_surface();
        surf.set_buffer_scale(scale as i32);
        surf.damage_buffer(0, 0, w as i32, h as i32);
        if let Err(e) = buffer.attach_to(surf) {
            eprintln!("inputlock: attach failed: {e}");
            return;
        }
        s.layer.commit();
    }

    fn redraw_all(&mut self) {
        for i in 0..self.surfaces.len() {
            self.draw(i);
        }
    }

    fn set_phase(&mut self, p: Phase) {
        if let Some(t) = self.wrong_timer.take() {
            self.loop_handle.remove(t);
        }
        self.phase = p;
        self.redraw_all();
    }

    fn typing_phase(&self) -> Phase {
        if self.password.is_empty() {
            Phase::Idle
        } else {
            Phase::Typing
        }
    }

    fn submit(&mut self) {
        if self.password.is_empty() {
            return;
        }
        let pw = std::mem::replace(&mut self.password, Zeroizing::new(String::new()));
        self.pending_len = pw.chars().count();
        // Attempts queue up in the channel and are checked in order, so
        // nothing typed during PAM's fail delay is lost or merged.
        if self.auth_tx.send(pw).is_err() {
            self.fatal = Some("authentication thread died".into());
            self.exit = true;
            return;
        }
        self.in_flight += 1;
        self.set_phase(Phase::Verifying);
    }

    pub fn on_auth_result(&mut self, r: Result<(), AuthError>) {
        self.in_flight = self.in_flight.saturating_sub(1);
        match r {
            Ok(()) => {
                self.unlocked = true;
                self.exit = true;
            }
            Err(e) => {
                // PAM messages (e.g. pam_faillock "account locked") mean the
                // right password won't work either — show that differently.
                let (phase, millis) = match e {
                    AuthError::Denied { reason, messages } => {
                        eprintln!("inputlock: authentication failed: {reason}");
                        for m in &messages {
                            eprintln!("inputlock: PAM: {m}");
                        }
                        if messages.is_empty() {
                            (Phase::Wrong, 1500)
                        } else {
                            (Phase::Blocked, 4000)
                        }
                    }
                    AuthError::System(m) => {
                        eprintln!("inputlock: PAM error: {m}");
                        (Phase::Blocked, 4000)
                    }
                };

                // Another attempt is already queued behind this one: keep
                // showing "checking" until it answers too.
                if self.in_flight > 0 {
                    return;
                }

                self.set_phase(phase);
                let token = self
                    .loop_handle
                    .insert_source(Timer::from_duration(Duration::from_millis(millis)), |_, _, app| {
                        app.wrong_timer = None;
                        if matches!(app.phase, Phase::Wrong | Phase::Blocked) {
                            let p = app.typing_phase();
                            app.phase = p;
                            app.redraw_all();
                        }
                        TimeoutAction::Drop
                    })
                    .ok();
                self.wrong_timer = token;
            }
        }
    }

    fn handle_key(&mut self, event: KeyEvent) {
        // Keys are always accepted, even while PAM is busy (it sleeps ~2 s
        // after a failure), so nothing typed is ever dropped.
        let busy = self.phase == Phase::Verifying;
        match event.keysym {
            Keysym::Return | Keysym::KP_Enter => return self.submit(),
            Keysym::BackSpace => {
                self.password.pop();
            }
            Keysym::Escape => self.password.clear(),
            k if self.ctrl && (k == Keysym::u || k == Keysym::U) => self.password.clear(),
            _ => {
                if let Some(t) = event.utf8.as_deref() {
                    if !t.is_empty()
                        && !t.chars().any(char::is_control)
                        && self.password.len() + t.len() <= MAX_PASSWORD_BYTES
                    {
                        self.password.push_str(t);
                    }
                }
            }
        }
        if busy {
            self.redraw_all();
        } else {
            let p = self.typing_phase();
            self.set_phase(p);
        }
    }
}

// ---------------------------------------------------------------- handlers

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        if let Some(i) = self.surfaces.iter().position(|s| s.layer.wl_surface() == surface) {
            if self.surfaces[i].scale != new_factor {
                self.surfaces[i].scale = new_factor;
                self.draw(i);
            }
        }
    }
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.add_output(qh, output);
    }
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.surfaces.retain(|s| s.output != output);
    }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        // Compositor closed one surface (e.g. output went away).
        self.surfaces.retain(|s| &s.layer != layer);
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        let Some(i) = self.surfaces.iter().position(|s| &s.layer == layer) else { return };
        {
            let s = &mut self.surfaces[i];
            s.width = configure.new_size.0;
            s.height = configure.new_size.1;
            s.configured = true;
        }
        self.engage_mode();
        self.draw(i);
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        if self.seat.is_none() {
            self.seat = Some(seat);
            let mut surfaces = std::mem::take(&mut self.surfaces);
            for s in &mut surfaces {
                self.inhibit(s, qh);
            }
            self.surfaces = surfaces;
        }
    }

    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, cap: Capability) {
        if cap == Capability::Keyboard && self.keyboard.is_none() {
            match self.seat_state.get_keyboard(qh, &seat, None) {
                Ok(k) => self.keyboard = Some(k),
                Err(e) => eprintln!("inputlock: keyboard init failed: {e}"),
            }
        }
        if cap == Capability::Pointer && self.pointer.is_none() {
            if let Ok(p) = self.seat_state.get_pointer(qh, &seat) {
                self.pointer = Some(p);
            }
        }
    }

    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, cap: Capability) {
        if cap == Capability::Keyboard {
            if let Some(k) = self.keyboard.take() {
                k.release();
            }
        }
        if cap == Capability::Pointer {
            if let Some(p) = self.pointer.take() {
                p.release();
            }
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for App {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32, _: &[u32], _: &[Keysym]) {}
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32) {}

    fn press_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, event: KeyEvent) {
        self.handle_key(event);
    }
    fn repeat_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: KeyEvent) {}
    fn release_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: KeyEvent) {}

    fn update_modifiers(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, m: Modifiers, _: RawModifiers, _: u32) {
        self.ctrl = m.ctrl;
    }
}

impl PointerHandler for App {
    fn pointer_frame(&mut self, _: &Connection, _: &QueueHandle<Self>, pointer: &wl_pointer::WlPointer, events: &[PointerEvent]) {
        // Every click/scroll/motion lands on our overlay and is simply dropped.
        if !self.cfg.hide_cursor {
            return;
        }
        for ev in events {
            if let PointerEventKind::Enter { serial } = ev.kind {
                pointer.set_cursor(serial, None, 0, 0);
            }
        }
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

delegate_registry!(App);
smithay_client_toolkit::delegate_dispatch2!(App);

// ------------------------------------------- keyboard-shortcuts-inhibit glue

pub struct InhibitData;

impl Dispatch2<ZwpKeyboardShortcutsInhibitManagerV1, App> for InhibitData {
    fn event(&self, _: &mut App, _: &ZwpKeyboardShortcutsInhibitManagerV1, _: <ZwpKeyboardShortcutsInhibitManagerV1 as smithay_client_toolkit::reexports::client::Proxy>::Event, _: &Connection, _: &QueueHandle<App>) {}
}

impl Dispatch2<ZwpKeyboardShortcutsInhibitorV1, App> for InhibitData {
    fn event(&self, _: &mut App, _: &ZwpKeyboardShortcutsInhibitorV1, ev: zwp_keyboard_shortcuts_inhibitor_v1::Event, _: &Connection, _: &QueueHandle<App>) {
        // Informational only; the empty sway mode is the primary mechanism.
        if let zwp_keyboard_shortcuts_inhibitor_v1::Event::Inactive = ev {
            eprintln!("inputlock: compositor deactivated shortcuts inhibitor");
        }
    }
}
