//! Wayland layer-shell client for showing solid-color test patches on a
//! chosen output.
//!
//! Usage:
//!
//! ```no_run
//! use spyder_display::PatchSurface;
//! let mut patch = PatchSurface::open("DP-1")?;
//! patch.set_color([1.0, 1.0, 1.0])?;
//! // ... measure with spyder-driver ...
//! patch.set_color([0.5, 0.0, 0.0])?;
//! // ...
//! drop(patch);  // surface goes away
//! # Ok::<(), spyder_display::Error>(())
//! ```
//!
//! The patch is a layer-shell surface anchored to all four edges of the
//! chosen output, placed in the `Overlay` layer (above normal windows),
//! with `exclusive_zone = -1` (does not push other windows around) and no
//! keyboard interactivity (user can still alt-tab away if needed).

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use std::time::{Duration, Instant};
use thiserror::Error;
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_shm, wl_surface},
    Connection, QueueHandle,
};

const PATCH_WIDTH: u32 = 512;
const PATCH_HEIGHT: u32 = 512;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Wayland connect: {0}")]
    Connect(#[from] wayland_client::ConnectError),

    #[error("Wayland dispatch: {0}")]
    Dispatch(#[from] wayland_client::DispatchError),

    #[error("Wayland I/O: {0}")]
    WaylandIo(#[from] wayland_client::backend::WaylandError),

    #[error("Wayland globals: {0}")]
    Globals(#[from] wayland_client::globals::GlobalError),

    #[error("Wayland binding: {0}")]
    Bind(#[from] wayland_client::globals::BindError),

    #[error("compositor doesn't advertise wlr-layer-shell (required)")]
    NoLayerShell,

    #[error("output '{0}' not found (have: {1:?})")]
    OutputNotFound(String, Vec<String>),

    #[error("SHM pool: {0}")]
    Shm(#[from] smithay_client_toolkit::shm::CreatePoolError),

    #[error("buffer slot: {0}")]
    Slot(#[from] smithay_client_toolkit::shm::slot::CreateBufferError),

    #[error("compositor never sent initial configure within {0:?}")]
    NoInitialConfigure(Duration),
}

/// A layer-shell surface fixed on a chosen output, holding a solid color.
pub struct PatchSurface {
    conn: Connection,
    state: AppState,
    event_queue: wayland_client::EventQueue<AppState>,
}

impl PatchSurface {
    /// Open a connection to the Wayland display, find an output matching
    /// `output_name`, and create a fullscreen layer-shell surface on it.
    ///
    /// `output_name` matches against the output's `name` (e.g. `"DP-1"`,
    /// `"HDMI-A-2"`). Use [`list_outputs`] to enumerate.
    pub fn open(output_name: &str) -> Result<Self, Error> {
        let conn = Connection::connect_to_env()?;
        let (globals, mut event_queue) = registry_queue_init::<AppState>(&conn)?;
        let qh = event_queue.handle();

        let registry_state = RegistryState::new(&globals);
        let output_state = OutputState::new(&globals, &qh);
        let compositor_state =
            CompositorState::bind(&globals, &qh).map_err(Error::Bind)?;
        let layer_shell =
            LayerShell::bind(&globals, &qh).map_err(|_| Error::NoLayerShell)?;
        let shm = Shm::bind(&globals, &qh).map_err(Error::Bind)?;

        let mut state = AppState {
            registry_state,
            output_state,
            compositor_state,
            layer_shell,
            shm,
            pool: None,
            surface_state: SurfaceState::WaitingForOutputs,
            current_color_argb: 0xFF_00_00_00,
            current_width: PATCH_WIDTH,
            current_height: PATCH_HEIGHT,
            redraw_pending: true,
        };

        // Pump events until OutputState has enumerated all outputs, so we
        // can match `output_name`. SCTK fires output events on the first
        // dispatch round-trip; one or two roundtrips is typically enough.
        event_queue.roundtrip(&mut state)?;
        event_queue.roundtrip(&mut state)?;

        // Pick the matching output.
        let wl_output = pick_output(&state.output_state, output_name)?;

        // Create the SHM pool now that we know we have a buffer to back.
        let pool_size = (PATCH_WIDTH * PATCH_HEIGHT * 4) as usize * 2; // double-buffer
        let pool = SlotPool::new(pool_size, &state.shm)?;
        state.pool = Some(pool);

        // Build the layer surface anchored fullscreen on the chosen output.
        let wl_surface = state.compositor_state.create_surface(&qh);
        let layer_surface = state.layer_shell.create_layer_surface(
            &qh,
            wl_surface,
            Layer::Overlay,
            Some("spyder-patch"),
            Some(&wl_output),
        );

        layer_surface.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        // exclusive_zone=-1 means "treat me like a normal floater, don't
        // shrink other clients' work area". Important so the desktop
        // doesn't reflow during a sweep.
        layer_surface.set_exclusive_zone(-1);
        layer_surface.set_keyboard_interactivity(KeyboardInteractivity::None);
        // Initial size — compositor will set the real size in the configure
        // event; we then resize the buffer to match.
        layer_surface.set_size(PATCH_WIDTH, PATCH_HEIGHT);
        layer_surface.commit();

        state.surface_state = SurfaceState::PendingConfigure(layer_surface);

        // Pump until first configure arrives so the surface is real.
        let deadline = Instant::now() + Duration::from_secs(2);
        while !matches!(state.surface_state, SurfaceState::Configured(_)) {
            if Instant::now() >= deadline {
                return Err(Error::NoInitialConfigure(Duration::from_secs(2)));
            }
            event_queue.blocking_dispatch(&mut state)?;
        }

        Ok(Self {
            conn,
            state,
            event_queue,
        })
    }

    /// Set the patch color (linear-or-encoded RGB in `0..=1`). The bytes
    /// written into the SHM buffer are XRGB8888 with each channel scaled to
    /// `0..=255` and clamped; what the display does with them is up to the
    /// compositor (today: niri = pass-through sRGB).
    pub fn set_color(&mut self, rgb: [f64; 3]) -> Result<(), Error> {
        let r = (rgb[0].clamp(0.0, 1.0) * 255.0).round() as u32;
        let g = (rgb[1].clamp(0.0, 1.0) * 255.0).round() as u32;
        let b = (rgb[2].clamp(0.0, 1.0) * 255.0).round() as u32;
        self.state.current_color_argb = 0xFF_00_00_00 | (r << 16) | (g << 8) | b;
        self.state.redraw_pending = true;

        self.redraw_and_settle(Duration::from_millis(50))
    }

    /// Block until the buffer is on screen (via frame callback), with a
    /// fudge factor for monitor scan-out latency. ~50 ms is enough for any
    /// modern panel + compositor combo to have actually transitioned to the
    /// new color before we start measuring.
    fn redraw_and_settle(&mut self, settle: Duration) -> Result<(), Error> {
        // Draw + commit.
        if self.state.redraw_pending {
            self.state.draw(&self.event_queue.handle())?;
        }
        // Flush + dispatch at least once so the commit reaches the server.
        self.conn.flush()?;
        self.event_queue.roundtrip(&mut self.state)?;
        // Settle for the panel.
        std::thread::sleep(settle);
        Ok(())
    }
}

impl Drop for PatchSurface {
    fn drop(&mut self) {
        // Best-effort: dispatch any pending events so the destroy is clean.
        let _ = self.event_queue.roundtrip(&mut self.state);
    }
}

/// Enumerate connected outputs known to the compositor at this moment.
/// Returns each output's name (e.g. `"DP-1"`) and a short description.
pub fn list_outputs() -> Result<Vec<OutputDescription>, Error> {
    let conn = Connection::connect_to_env()?;
    let (globals, mut event_queue) = registry_queue_init::<AppState>(&conn)?;
    let qh = event_queue.handle();
    let registry_state = RegistryState::new(&globals);
    let output_state = OutputState::new(&globals, &qh);

    // Build a minimal state — we only need the OutputState.
    let mut state = AppState {
        registry_state,
        output_state,
        compositor_state: CompositorState::bind(&globals, &qh).map_err(Error::Bind)?,
        layer_shell: LayerShell::bind(&globals, &qh).map_err(|_| Error::NoLayerShell)?,
        shm: Shm::bind(&globals, &qh).map_err(Error::Bind)?,
        pool: None,
        surface_state: SurfaceState::WaitingForOutputs,
        current_color_argb: 0,
        current_width: PATCH_WIDTH,
        current_height: PATCH_HEIGHT,
        redraw_pending: false,
    };
    event_queue.roundtrip(&mut state)?;
    event_queue.roundtrip(&mut state)?;

    let descs = state
        .output_state
        .outputs()
        .filter_map(|o| {
            let info = state.output_state.info(&o)?;
            Some(OutputDescription {
                name: info.name.clone().unwrap_or_default(),
                description: info.description.clone().unwrap_or_default(),
                make: info.make.clone(),
                model: info.model.clone(),
                size: info.modes.iter().find(|m| m.current).map(|m| m.dimensions),
            })
        })
        .collect();
    Ok(descs)
}

#[derive(Debug, Clone)]
pub struct OutputDescription {
    pub name: String,
    pub description: String,
    pub make: String,
    pub model: String,
    pub size: Option<(i32, i32)>,
}

fn pick_output(
    output_state: &OutputState,
    name: &str,
) -> Result<wl_output::WlOutput, Error> {
    let mut found_names = Vec::new();
    for output in output_state.outputs() {
        if let Some(info) = output_state.info(&output) {
            if info.name.as_deref() == Some(name) {
                return Ok(output);
            }
            if let Some(n) = info.name {
                found_names.push(n);
            }
        }
    }
    Err(Error::OutputNotFound(name.to_string(), found_names))
}

// ---------------------------------------------------------------------------
// Internal state + Wayland handlers
// ---------------------------------------------------------------------------

enum SurfaceState {
    WaitingForOutputs,
    PendingConfigure(LayerSurface),
    Configured(LayerSurface),
}

struct AppState {
    registry_state: RegistryState,
    output_state: OutputState,
    compositor_state: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    pool: Option<SlotPool>,
    surface_state: SurfaceState,
    current_color_argb: u32,
    current_width: u32,
    current_height: u32,
    redraw_pending: bool,
}

impl AppState {
    fn draw(&mut self, _qh: &QueueHandle<Self>) -> Result<(), Error> {
        let Some(pool) = self.pool.as_mut() else {
            return Ok(());
        };
        let SurfaceState::Configured(layer_surface) = &self.surface_state else {
            return Ok(());
        };
        let width = self.current_width as i32;
        let height = self.current_height as i32;
        let stride = width * 4;

        let (buffer, canvas) = pool.create_buffer(
            width,
            height,
            stride,
            wl_shm::Format::Xrgb8888,
        )?;

        // Fill with our color (XRGB8888, little-endian native u32).
        let color = self.current_color_argb.to_le_bytes();
        for px in canvas.chunks_exact_mut(4) {
            px.copy_from_slice(&color);
        }

        let wl_surface = layer_surface.wl_surface();
        wl_surface.set_buffer_scale(1);
        wl_surface.damage_buffer(0, 0, width, height);
        buffer.attach_to(wl_surface).expect("attach failed");
        wl_surface.commit();

        self.redraw_pending = false;
        Ok(())
    }
}

impl CompositorHandler for AppState {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        // Frame callbacks could be used for finer-grained settle tracking,
        // but we use a simple post-roundtrip sleep instead.
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for AppState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

impl ShmHandler for AppState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl LayerShellHandler for AppState {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {}

    fn configure(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        // Compositor told us the actual surface size. If it's nonzero, use it;
        // otherwise stay with our hint.
        if configure.new_size.0 > 0 {
            self.current_width = configure.new_size.0;
        }
        if configure.new_size.1 > 0 {
            self.current_height = configure.new_size.1;
        }
        // Transition to Configured (move the layer-surface handle in).
        let layer_owned = match std::mem::replace(
            &mut self.surface_state,
            SurfaceState::WaitingForOutputs,
        ) {
            SurfaceState::PendingConfigure(s) | SurfaceState::Configured(s) => s,
            SurfaceState::WaitingForOutputs => layer.clone(),
        };
        self.surface_state = SurfaceState::Configured(layer_owned);
        // Draw immediately so the surface has content for the configure ack.
        let _ = self.draw(qh);
    }
}

impl ProvidesRegistryState for AppState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

delegate_compositor!(AppState);
delegate_output!(AppState);
delegate_shm!(AppState);
delegate_layer!(AppState);
delegate_registry!(AppState);
