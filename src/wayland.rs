//! Wayland layer-shell host and daemon event loop for wl-menu.

use crate::config::{Config, InputConfig};
use crate::debug;
use crate::ipc::{self, IpcRequest, IpcResponse};
use crate::render::{self, CardLayout};
use crate::state::MenuSession;
use calloop::timer::{TimeoutAction, Timer};
use calloop::EventLoop;
use calloop_wayland_source::WaylandSource;
use fontdue::Font;
use memmap2::MmapMut;
use std::collections::HashSet;
use std::fs::File;
use std::io::ErrorKind;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_keyboard, wl_pointer, wl_region, wl_registry, wl_seat, wl_shm,
    wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1, zwlr_layer_surface_v1,
};
use xkbcommon::xkb::{self, Keycode, Keysym};

const LOG_PREFIX: &str = "wl-menu";
const NAMESPACE: &str = "wl-menu";

/// Run the menu daemon until process exit.
pub fn run_daemon(config: Config, socket_path: PathBuf) -> Result<(), ()> {
    let listener = ipc::bind_listener(&socket_path).map_err(|_| ())?;
    listener.set_nonblocking(true).map_err(|_| ())?;
    let conn = Connection::connect_to_env().map_err(|err| {
        eprintln!("{LOG_PREFIX}: wayland connect failed: {err}");
    })?;
    let (globals, event_queue) = registry_queue_init::<App>(&conn).map_err(|err| {
        eprintln!("{LOG_PREFIX}: registry init failed: {err}");
    })?;
    let qh = event_queue.handle();

    let compositor = globals
        .bind::<wl_compositor::WlCompositor, _, _>(&qh, 1..=6, ())
        .map_err(|_| ())?;
    let shm = globals
        .bind::<wl_shm::WlShm, _, _>(&qh, 1..=1, ())
        .map_err(|_| ())?;
    let layer_shell = globals
        .bind::<zwlr_layer_shell_v1::ZwlrLayerShellV1, _, _>(&qh, 1..=4, ())
        .map_err(|_| ())?;
    let seat = globals
        .bind::<wl_seat::WlSeat, _, _>(&qh, 1..=9, ())
        .map_err(|_| ())?;

    let mut event_loop: EventLoop<App> = EventLoop::try_new().map_err(|_| ())?;

    WaylandSource::new(conn, event_queue)
        .insert(event_loop.handle())
        .map_err(|_| ())?;

    event_loop
        .handle()
        .insert_source(Timer::from_duration(Duration::from_millis(16)), |_, _, app| {
            app.tick_key_repeat();
            app.accept_ipc();
            TimeoutAction::ToDuration(Duration::from_millis(16))
        })
        .map_err(|_| ())?;

    let surface = compositor.create_surface(&qh, ());
    let layer_surface = layer_shell.get_layer_surface(
        &surface,
        None,
        zwlr_layer_shell_v1::Layer::Overlay,
        NAMESPACE.to_string(),
        &qh,
        (),
    );

    use zwlr_layer_surface_v1::Anchor;
    layer_surface.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
    layer_surface.set_keyboard_interactivity(
        zwlr_layer_surface_v1::KeyboardInteractivity::None,
    );
    layer_surface.set_size(0, 0);
    layer_surface.set_exclusive_zone(-1);
    surface.commit();

    seat.get_keyboard(&qh, ());
    seat.get_pointer(&qh, ());

    let font = render::load_font(&config.font);
    if font.is_none() {
        eprintln!("{LOG_PREFIX}: menu font not found, text disabled");
    }

    let mut app = App {
        ipc_listener: listener,
        session: MenuSession::new(config.clone()),
        config: config.clone(),
        font,
        width: 0,
        height: 0,
        configured: false,
        compositor,
        surface,
        layer_surface,
        shm,
        pool: None,
        buffer: None,
        mmap: None,
        buffer_width: 0,
        buffer_height: 0,
        card: CardLayout {
            x: 0.0,
            y: 0.0,
            width: 420.0,
            height: 200.0,
            header_height: 48.0,
            row_height: 36.0,
            visible_rows: 1,
        },
        keyboard: KeyboardState::new(config.input),
        pointer_x: 0.0,
        pointer_y: 0.0,
        pointer_armed: true,
        pointer_disarm_at: None,
        pointer_press_index: None,
        submenu_gate: false,
        scroll: ScrollAccum::new(),
        qhandle: qh,
    };

    eprintln!("{LOG_PREFIX}: daemon ready on {} (debug={})", socket_path.display(), crate::debug::enabled());
    event_loop.run(None, &mut app, |_| {}).map_err(|_| ())
}

struct KeyboardState {
    context: xkb::Context,
    keymap: Option<xkb::Keymap>,
    state: Option<xkb::State>,
    repeat_delay_ms: u32,
    repeat_rate: u32,
    held_delta: Option<i32>,
    next_repeat: Option<Instant>,
    /// Physical keys still down; blocks compositor auto-repeat Pressed events.
    held_keys: HashSet<u32>,
}

impl KeyboardState {
    fn new(input: InputConfig) -> Self {
        Self {
            context: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
            keymap: None,
            state: None,
            repeat_delay_ms: input.repeat_delay_ms,
            repeat_rate: input.repeat_rate,
            held_delta: None,
            next_repeat: None,
            held_keys: HashSet::new(),
        }
    }
}

struct ScrollAccum {
    vertical: i32,
    got_value120: bool,
}

impl ScrollAccum {
    fn new() -> Self {
        Self {
            vertical: 0,
            got_value120: false,
        }
    }

    fn clear(&mut self) {
        self.vertical = 0;
        self.got_value120 = false;
    }

    fn add_value120(&mut self, value120: i32) {
        if value120 == 0 {
            return;
        }
        self.got_value120 = true;
        self.vertical += value120.signum();
    }

    fn add_axis(&mut self, value: f64) {
        if self.got_value120 || value.abs() < f64::EPSILON {
            return;
        }
        self.vertical += value.signum() as i32;
    }
}

struct App {
    ipc_listener: UnixListener,
    session: MenuSession,
    config: Config,
    font: Option<Font>,
    width: u32,
    height: u32,
    configured: bool,
    compositor: wl_compositor::WlCompositor,
    surface: wl_surface::WlSurface,
    layer_surface: zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
    shm: wl_shm::WlShm,
    pool: Option<wl_shm_pool::WlShmPool>,
    buffer: Option<wl_buffer::WlBuffer>,
    mmap: Option<MmapMut>,
    buffer_width: u32,
    buffer_height: u32,
    card: CardLayout,
    keyboard: KeyboardState,
    pointer_x: f64,
    pointer_y: f64,
    /// Clicks ignored until the pointer moves far enough from `pointer_disarm_at`.
    pointer_armed: bool,
    /// Cursor position when the pointer was disarmed (submenu open / IPC open).
    pointer_disarm_at: Option<(f64, f64)>,
    /// Row index captured on button press; activate runs on release.
    pointer_press_index: Option<usize>,
    /// Block Enter/Right until released once after entering a submenu (key repeat).
    submenu_gate: bool,
    scroll: ScrollAccum,
    qhandle: QueueHandle<App>,
}

impl App {
    fn accept_ipc(&mut self) {
        loop {
            match self.ipc_listener.accept() {
                Ok((stream, _)) => self.handle_ipc(stream),
                Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                Err(err) => {
                    eprintln!("{LOG_PREFIX}: ipc accept failed: {err}");
                    break;
                }
            }
        }
    }

    fn handle_ipc(&mut self, mut stream: UnixStream) {
        let response = match ipc::read_request(&mut stream) {
            Ok(request) => self.dispatch_ipc(request),
            Err(err) => IpcResponse::err(err.to_string()),
        };
        let _ = ipc::write_response(&mut stream, &response);
    }

    fn dispatch_ipc(&mut self, request: IpcRequest) -> IpcResponse {
        debug::log(format!("ipc: {request:?}"));
        match request {
            IpcRequest::Open { menu } => {
                self.session.open_route(&menu);
                self.arm_pointer();
                self.sync_keyboard_interactivity();
                self.present();
                IpcResponse::ok()
            }
            IpcRequest::Toggle { menu } => {
                self.session.toggle(&menu);
                if self.session.visible {
                    self.arm_pointer();
                }
                self.sync_keyboard_interactivity();
                self.present();
                IpcResponse::ok()
            }
            IpcRequest::Close => {
                self.session.close();
                self.sync_keyboard_interactivity();
                self.present();
                IpcResponse::ok()
            }
            IpcRequest::Reload => {
                let source = self.session.config.source_path.clone();
                self.session.config = Config::load(source.as_deref());
                self.config = self.session.config.clone();
                self.session.reload();
                self.sync_keyboard_interactivity();
                self.present();
                IpcResponse::ok()
            }
            IpcRequest::Ping => IpcResponse::ok(),
        }
    }

    fn sync_keyboard_interactivity(&mut self) {
        let mode = if self.session.visible {
            zwlr_layer_surface_v1::KeyboardInteractivity::Exclusive
        } else {
            zwlr_layer_surface_v1::KeyboardInteractivity::None
        };
        // Do not commit here: present() attaches the buffer and commits once.
        // A bare commit after hide/show races with layer-shell configure.
        self.layer_surface.set_keyboard_interactivity(mode);
        self.sync_input_region();
        if !self.session.visible {
            self.reset_keyboard_tracking();
        }
    }

    /// Pass clicks through while the menu is closed (transparent buffer stays mapped).
    fn sync_input_region(&mut self) {
        if self.session.visible {
            self.surface.set_input_region(None);
            return;
        }
        let region = self.compositor.create_region(&self.qhandle, ());
        self.surface.set_input_region(Some(&region));
        region.destroy();
    }

    fn present(&mut self) {
        if !self.configured || self.width == 0 || self.height == 0 {
            return;
        }
        // Keep a transparent buffer attached while closed. attach(None) unmaps
        // the layer surface and Hyprland then rejects the next buffer until a
        // fresh configure ("layerSurface was not configured, but a buffer was
        // attached"), so the second IPC open would fail.
        self.sync_input_region();
        self.card = CardLayout::compute(&self.session, self.width, self.height);
        let frame = render::render_frame(
            self.width,
            self.height,
            &self.session,
            self.font.as_ref(),
            &self.card,
        );
        self.blit(&frame);
    }

    fn blit(&mut self, frame: &tiny_skia::Pixmap) {
        if self.buffer_width != self.width || self.buffer_height != self.height {
            self.buffer = None;
            self.pool = None;
            self.mmap = None;
            self.buffer_width = self.width;
            self.buffer_height = self.height;
        }

        let stride = self.width * 4;
        let size = (stride * self.height) as usize;
        if self.mmap.as_ref().map(|m| m.len()).unwrap_or(0) != size {
            let fd = create_memfd(size).expect("memfd");
            let mut mmap = unsafe { MmapMut::map_mut(&fd).expect("mmap") };
            mmap.fill(0);
            let pool = self
                .shm
                .create_pool(fd.as_fd(), size as i32, &self.qhandle, ());
            self.pool = Some(pool);
            self.mmap = Some(mmap);
        }

        if let Some(mmap) = self.mmap.as_mut() {
            copy_pixmap_to_mmap(frame, mmap, self.width, self.height);
        }

        if self.buffer.is_none() {
            if let Some(pool) = &self.pool {
                let buffer = pool.create_buffer(
                    0,
                    self.width as i32,
                    self.height as i32,
                    stride as i32,
                    wl_shm::Format::Abgr8888,
                    &self.qhandle,
                    (),
                );
                self.buffer = Some(buffer);
            }
        }

        if let Some(buffer) = &self.buffer {
            self.surface.attach(Some(buffer), 0, 0);
            self.surface
                .damage_buffer(0, 0, self.width as i32, self.height as i32);
            self.surface.commit();
        }
    }

    fn arm_pointer(&mut self) {
        debug::log(format!(
            "pointer: arm at ({:.0},{:.0}) active={:?}",
            self.pointer_x, self.pointer_y, self.session.active
        ));
        self.pointer_armed = true;
        self.pointer_disarm_at = None;
        self.pointer_press_index = None;
        self.submenu_gate = false;
    }

    /// Block further clicks until the cursor moves ~12px (click jitter must not re-arm).
    fn disarm_pointer(&mut self) {
        debug::log(format!(
            "pointer: disarm at ({:.0},{:.0}) active={:?}",
            self.pointer_x, self.pointer_y, self.session.active
        ));
        self.pointer_armed = false;
        self.pointer_disarm_at = Some((self.pointer_x, self.pointer_y));
        self.pointer_press_index = None;
    }

    fn handle_pointer_motion(&mut self, x: f64, y: f64) {
        self.pointer_x = x;
        self.pointer_y = y;
        let Some((ox, oy)) = self.pointer_disarm_at else {
            return;
        };
        let dx = x - ox;
        let dy = y - oy;
        let dist = (dx * dx + dy * dy).sqrt();
        if dist >= 12.0 {
            debug::log(format!(
                "pointer: re-arm after move {:.1}px from ({ox:.0},{oy:.0})",
                dist
            ));
            self.arm_pointer();
        }
    }

    fn handle_button_pressed(&mut self) {
        debug::log(format!(
            "pointer: press at ({:.0},{:.0}) armed={} visible={} active={:?}",
            self.pointer_x,
            self.pointer_y,
            self.pointer_armed,
            self.session.visible,
            self.session.active
        ));
        if !self.session.visible || !self.pointer_armed {
            debug::log(format!(
                "pointer: press ignored (visible={} armed={})",
                self.session.visible, self.pointer_armed
            ));
            self.pointer_press_index = None;
            return;
        }
        self.pointer_press_index = render::hit_test_row(
            &self.session,
            &self.card,
            self.pointer_x as f32,
            self.pointer_y as f32,
        );
        debug::log(format!(
            "pointer: press hit={:?} scroll={} card=({:.0},{:.0} {:.0}x{:.0}) rows={}",
            self.pointer_press_index,
            self.session.scroll_offset(self.card.visible_rows),
            self.card.x,
            self.card.y,
            self.card.width,
            self.card.height,
            self.session.rows.len()
        ));
        if let Some(index) = self.pointer_press_index {
            debug::log(self.session.row_debug(index));
            self.session.selected = index;
            self.present();
        }
    }

    fn handle_button_released(&mut self) {
        debug::log(format!(
            "pointer: release at ({:.0},{:.0}) armed={} press_index={:?}",
            self.pointer_x,
            self.pointer_y,
            self.pointer_armed,
            self.pointer_press_index
        ));
        if !self.session.visible || !self.pointer_armed {
            debug::log("pointer: release ignored (not visible or disarmed)");
            self.pointer_press_index = None;
            return;
        }
        let Some(press_index) = self.pointer_press_index.take() else {
            debug::log("pointer: release ignored (no press_index)");
            return;
        };
        let release_index = render::hit_test_row(
            &self.session,
            &self.card,
            self.pointer_x as f32,
            self.pointer_y as f32,
        );
        if release_index != Some(press_index) {
            debug::log(format!(
                "pointer: release cancelled press={press_index} release={release_index:?}"
            ));
            return;
        }
        self.session.selected = press_index;
        debug::log(format!(
            "pointer: release -> activate {}",
            self.session.row_debug(press_index)
        ));
        let depth_before = self.session.nav_stack.len();
        self.session.activate_selected();
        if self.session.visible && self.session.nav_stack.len() > depth_before {
            self.on_submenu_entered();
        }
        self.sync_keyboard_interactivity();
        self.present();
    }

    fn activate_from_keyboard(&mut self) {
        if self.submenu_gate {
            debug::log("keyboard: activate blocked (submenu_gate, waiting for Enter release)");
            return;
        }
        debug::log(format!(
            "keyboard: activate Enter/Right selected={} {}",
            self.session.selected,
            self.session.row_debug(self.session.selected)
        ));
        let depth_before = self.session.nav_stack.len();
        self.session.activate_selected();
        if self.session.visible && self.session.nav_stack.len() > depth_before {
            debug::log("keyboard: submenu_gate ON (entered submenu)");
            self.submenu_gate = true;
            self.disarm_pointer();
        }
        self.sync_keyboard_interactivity();
        self.present();
    }

    fn on_submenu_entered(&mut self) {
        debug::log("input: submenu_gate ON + pointer disarm");
        self.submenu_gate = true;
        self.disarm_pointer();
    }

    fn flush_scroll(&mut self) {
        let delta = self.scroll.vertical;
        self.scroll.clear();
        if delta == 0 || !self.session.visible {
            return;
        }
        let step = if delta > 0 { 1 } else { -1 };
        self.session.select_delta(step);
        self.present();
    }

    fn tick_key_repeat(&mut self) {
        let Some(delta) = self.keyboard.held_delta else {
            return;
        };
        let Some(next) = self.keyboard.next_repeat else {
            return;
        };
        if Instant::now() < next {
            return;
        }
        if self.session.visible {
            self.session.select_delta(delta);
            self.present();
        }
        let interval_ms = (1000u32 / self.keyboard.repeat_rate.max(1)).max(1);
        self.keyboard.next_repeat =
            Some(Instant::now() + Duration::from_millis(u64::from(interval_ms)));
    }

    fn begin_key_repeat(&mut self, delta: i32) {
        self.keyboard.held_delta = Some(delta);
        if self.keyboard.repeat_rate == 0 {
            self.keyboard.next_repeat = None;
            return;
        }
        // Compositor may report delay=0; avoid a second step before key release.
        let delay = u64::from(self.keyboard.repeat_delay_ms).max(400);
        self.keyboard.next_repeat = Some(Instant::now() + Duration::from_millis(delay));
    }

    fn clear_key_repeat(&mut self) {
        self.keyboard.held_delta = None;
        self.keyboard.next_repeat = None;
    }

    /// Drop tracked key state when focus is lost without matching Release events.
    fn reset_keyboard_tracking(&mut self) {
        self.keyboard.held_keys.clear();
        self.clear_key_repeat();
    }

    fn handle_key(&mut self, keycode: u32, state: wl_keyboard::KeyState) {
        if !self.session.visible {
            return;
        }
        let Some(xkb_state) = &self.keyboard.state else {
            return;
        };
        let xkb_keycode = Keycode::new(keycode);
        let sym = xkb_state.key_get_one_sym(xkb_keycode);

        if matches!(state, wl_keyboard::KeyState::Released) {
            self.keyboard.held_keys.remove(&keycode);
            match sym {
                Keysym::Return | Keysym::KP_Enter | Keysym::Right => {
                    if self.submenu_gate {
                        debug::log(format!("keyboard: submenu_gate OFF (release {sym:?})"));
                        self.submenu_gate = false;
                    }
                    self.clear_key_repeat();
                }
                Keysym::Up | Keysym::Down | Keysym::Tab => self.clear_key_repeat(),
                _ => {}
            }
            return;
        }
        if !matches!(state, wl_keyboard::KeyState::Pressed) {
            return;
        }

        if !self.keyboard.held_keys.insert(keycode) {
            debug::log(format!(
                "keyboard: ignore compositor repeat keycode={keycode} sym={sym:?}"
            ));
            return;
        }

        debug::log(format!("keyboard: press sym={sym:?}"));

        match sym {
            Keysym::Escape => {
                self.clear_key_repeat();
                if self.session.confirm.is_some() {
                    self.session.confirm = None;
                } else {
                    self.session.close();
                }
                self.sync_keyboard_interactivity();
                self.present();
            }
            Keysym::Return | Keysym::KP_Enter => {
                self.clear_key_repeat();
                self.activate_from_keyboard();
            }
            Keysym::Right => {
                self.clear_key_repeat();
                // Right drills into submenus only; leaf actions need Enter.
                if self.session.selected_opens_submenu() {
                    self.activate_from_keyboard();
                }
            }
            Keysym::BackSpace => {
                self.clear_key_repeat();
                self.session.backspace();
                self.present();
            }
            Keysym::Up => {
                self.session.select_delta(-1);
                self.begin_key_repeat(-1);
                self.present();
            }
            Keysym::Down => {
                self.session.select_delta(1);
                self.begin_key_repeat(1);
                self.present();
            }
            Keysym::Tab => {
                let shift =
                    xkb_state.mod_name_is_active(xkb::MOD_NAME_SHIFT, xkb::STATE_MODS_EFFECTIVE);
                let delta = if shift { -1 } else { 1 };
                self.session.select_delta(delta);
                self.begin_key_repeat(delta);
                self.present();
            }
            Keysym::Left => {
                self.clear_key_repeat();
                self.session.go_back();
                self.sync_keyboard_interactivity();
                self.present();
            }
            _ => {
                let text = xkb_state.key_get_utf8(xkb_keycode);
                if !text.is_empty() {
                    for ch in text.chars() {
                        if !ch.is_control() {
                            self.session.push_char(ch);
                        }
                    }
                    self.present();
                }
            }
        }
    }
}

fn copy_pixmap_to_mmap(pixmap: &tiny_skia::Pixmap, mmap: &mut MmapMut, width: u32, height: u32) {
    let stride = width as usize * 4;
    for y in 0..height as usize {
        let src = &pixmap.data()[y * stride..(y + 1) * stride];
        let dst = &mut mmap[y * stride..(y + 1) * stride];
        dst.copy_from_slice(src);
    }
}

fn create_memfd(size: usize) -> std::io::Result<OwnedFd> {
    let path = format!("/wl-menu-{}", std::process::id());
    let fd = unsafe { libc::memfd_create(path.as_ptr() as *const libc::c_char, libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    file.set_len(size as u64)?;
    Ok(file.into())
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for App {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for App {
    fn event(
        _: &mut Self,
        _: &wl_compositor::WlCompositor,
        _: wl_compositor::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm::WlShm, ()> for App {
    fn event(
        _: &mut Self,
        _: &wl_shm::WlShm,
        _: wl_shm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for App {
    fn event(
        _: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        {
            if caps.contains(wl_seat::Capability::Keyboard) {
                seat.get_keyboard(qh, ());
            }
            if caps.contains(wl_seat::Capability::Pointer) {
                seat.get_pointer(qh, ());
            }
        }
    }
}

impl Dispatch<wl_region::WlRegion, ()> for App {
    fn event(
        _: &mut Self,
        _: &wl_region::WlRegion,
        _: wl_region::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for App {
    fn event(
        _: &mut Self,
        _: &wl_surface::WlSurface,
        _: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for App {
    fn event(
        _: &mut Self,
        _: &wl_buffer::WlBuffer,
        _: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for App {
    fn event(
        _: &mut Self,
        _: &wl_shm_pool::WlShmPool,
        _: wl_shm_pool::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwlr_layer_shell_v1::ZwlrLayerShellV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &zwlr_layer_shell_v1::ZwlrLayerShellV1,
        _: zwlr_layer_shell_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for App {
    fn event(
        state: &mut Self,
        layer_surface: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, width, height } => {
                layer_surface.ack_configure(serial);
                state.width = if width == 0 {
                    state.width.max(1920)
                } else {
                    width as u32
                };
                state.height = if height == 0 {
                    state.height.max(1080)
                } else {
                    height as u32
                };
                state.configured = true;
                state.present();
            }
            zwlr_layer_surface_v1::Event::Closed => {}
            _ => {}
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for App {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap { format, fd, size } => {
                if format != WEnum::Value(wl_keyboard::KeymapFormat::XkbV1) {
                    return;
                }
                let owned = unsafe { OwnedFd::from_raw_fd(fd.as_raw_fd()) };
                let keymap = unsafe {
                    xkb::Keymap::new_from_fd(
                        &state.keyboard.context,
                        owned,
                        size as usize,
                        xkb::KEYMAP_FORMAT_TEXT_V1,
                        0,
                    )
                };
                if let Ok(Some(keymap)) = keymap {
                    let xkb_state = xkb::State::new(&keymap);
                    state.keyboard.keymap = Some(keymap);
                    state.keyboard.state = Some(xkb_state);
                }
            }
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                if let Some(xkb_state) = &mut state.keyboard.state {
                    xkb_state.update_mask(
                        mods_depressed,
                        mods_latched,
                        mods_locked,
                        0,
                        0,
                        group,
                    );
                }
            }
            wl_keyboard::Event::RepeatInfo { rate, delay } => {
                state.keyboard.repeat_rate = rate as u32;
                state.keyboard.repeat_delay_ms = delay as u32;
            }
            wl_keyboard::Event::Enter { .. } | wl_keyboard::Event::Leave { .. } => {
                state.reset_keyboard_tracking();
            }
            wl_keyboard::Event::Key { key, state: key_state, .. } => {
                if let WEnum::Value(key_state) = key_state {
                    state.handle_key(key + 8, key_state);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for App {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                state.handle_pointer_motion(surface_x, surface_y);
            }
            wl_pointer::Event::Button { state: button_state, .. } => {
                match button_state {
                    WEnum::Value(wl_pointer::ButtonState::Pressed) => {
                        state.handle_button_pressed();
                    }
                    WEnum::Value(wl_pointer::ButtonState::Released) => {
                        state.handle_button_released();
                    }
                    _ => {}
                }
            }
            wl_pointer::Event::Axis { axis, value, .. } => {
                if let WEnum::Value(axis) = axis {
                    if axis == wl_pointer::Axis::VerticalScroll {
                        state.scroll.add_axis(value);
                    }
                }
            }
            wl_pointer::Event::AxisValue120 { axis, value120, .. } => {
                if let WEnum::Value(axis) = axis {
                    if axis == wl_pointer::Axis::VerticalScroll {
                        state.scroll.add_value120(value120);
                    }
                }
            }
            wl_pointer::Event::Frame => state.flush_scroll(),
            _ => {}
        }
    }
}
