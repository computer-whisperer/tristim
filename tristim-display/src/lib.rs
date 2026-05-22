//! Wayland layer-shell client for showing solid-color test patches on a
//! chosen output.
//!
//! Two modes:
//!
//! **SDR (default)** writes 8-bit XRGB8888 with no color description.
//! Compositor treats values as sRGB. Works on any wayland compositor.
//!
//! **HDR** writes fp16 PQ-encoded values and attaches a parametric
//! `wp_color_management_v1` description (PQ + BT.2020 + mastering
//! metadata). The compositor (prism) recognises the description and
//! scans the buffer out without color transforms; the panel applies
//! its PQ EOTF to recover absolute luminance. Requires both
//! wp_color_management_v1 and shm fp16 format support on the
//! compositor.
//!
//! Usage:
//!
//! ```no_run
//! use tristim_display::PatchSurface;
//! // SDR
//! let mut patch = PatchSurface::open("DP-1")?;
//! patch.set_color([1.0, 1.0, 1.0])?;
//!
//! // HDR — explicit per-panel mastering params
//! use tristim_display::{PqDescriptionParams};
//! let mut hdr = PatchSurface::open_hdr(
//!     "DP-4",
//!     PqDescriptionParams::pg27ucdm_default(),
//! )?;
//! hdr.set_nits([100.0, 100.0, 100.0])?;  // 100 cd/m² white
//! # Ok::<(), tristim_display::Error>(())
//! ```
//!
//! The patch is a layer-shell surface anchored to all four edges of the
//! chosen output, placed in the `Overlay` layer (above normal windows),
//! with `exclusive_zone = -1` (does not push other windows around) and no
//! keyboard interactivity (user can still alt-tab away if needed).
//!
//! **Windowed patches**: by default the patch fills the whole output.
//! For OLEDs and other panels with global power limiters, a 100%-APL
//! white fill is throttled by the panel's automatic brightness limiter
//! (ABL) and never reaches the rated peak. Use
//! [`PatchSurface::set_window_fraction`] to paint a smaller bright
//! region on a black background — the surface is still fullscreen (so
//! the rest of the desktop stays hidden during a sweep) but the
//! emitting area is reduced. Typical values: `0.10` for ~10% APL,
//! `0.04` for ~4% (close to industry-spec "peak brightness, 4% window"
//! ratings).

pub mod color_mgmt;
pub mod pq;

pub use color_mgmt::{DescriptionState, PqDescriptionParams};

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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use thiserror::Error;
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_shm, wl_surface},
    Connection, Dispatch, QueueHandle, WEnum,
};
use wayland_protocols::wp::color_management::v1::client::{
    wp_color_management_surface_v1::WpColorManagementSurfaceV1,
    wp_color_manager_v1::WpColorManagerV1,
    wp_image_description_creator_params_v1::WpImageDescriptionCreatorParamsV1,
    wp_image_description_v1::WpImageDescriptionV1,
};

use crate::color_mgmt::ColorManagedSurface;

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

    #[error("compositor doesn't advertise wp_color_manager_v1 (required for HDR mode)")]
    NoColorManager,

    #[error(
        "compositor rejected our PQ image description: {0} \
         (typically means the compositor doesn't support the requested TF or primaries)"
    )]
    DescriptionFailed(String),

    #[error("image description never went ready within {0:?}")]
    NoDescriptionReady(Duration),

    #[error("set_nits called on an SDR patch — use set_color or open_hdr instead")]
    NotHdrMode,
}

/// A layer-shell surface fixed on a chosen output, holding a solid color.
pub struct PatchSurface {
    conn: Connection,
    state: AppState,
    event_queue: wayland_client::EventQueue<AppState>,
}

impl PatchSurface {
    /// SDR convenience — see [`Self::open_with_mode`] for the full
    /// API. Equivalent to opening in SDR mode.
    pub fn open(output_name: &str) -> Result<Self, Error> {
        Self::open_with_mode(output_name, None)
    }

    /// HDR convenience — opens in HDR mode with the given PQ
    /// mastering description params. The compositor must advertise
    /// `wp_color_manager_v1` and `wl_shm` Xbgr16161616f, otherwise
    /// `Error::NoColorManager` / a shm format mismatch is returned.
    pub fn open_hdr(output_name: &str, params: PqDescriptionParams) -> Result<Self, Error> {
        Self::open_with_mode(output_name, Some(params))
    }

    /// Full constructor. `hdr_params = Some(_)` puts the patch in
    /// HDR mode (fp16 buffers + wp_color_management description);
    /// `None` keeps the historical 8-bit SDR path.
    pub fn open_with_mode(
        output_name: &str,
        hdr_params: Option<PqDescriptionParams>,
    ) -> Result<Self, Error> {
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

        // Optional bind: wp_color_manager_v1. Always try; we only
        // error out if HDR mode was requested AND the bind failed.
        // SCTK doesn't manage this global so we go through the raw
        // globals list directly.
        let color_manager = globals
            .bind::<WpColorManagerV1, _, _>(&qh, 1..=2, ())
            .ok();
        if hdr_params.is_some() && color_manager.is_none() {
            return Err(Error::NoColorManager);
        }

        let initial_content = if hdr_params.is_some() {
            PatchContent::HdrPqNits([0.0, 0.0, 0.0])
        } else {
            PatchContent::Sdr(0xFF_00_00_00)
        };

        let mut state = AppState {
            registry_state,
            output_state,
            compositor_state,
            layer_shell,
            shm,
            pool: None,
            surface_state: SurfaceState::WaitingForOutputs,
            current_content: initial_content,
            current_width: PATCH_WIDTH,
            current_height: PATCH_HEIGHT,
            redraw_pending: true,
            color_manager,
            color_managed: None,
            hdr_params,
            window_fraction: 1.0,
        };

        // Pump events until OutputState has enumerated all outputs, so we
        // can match `output_name`. SCTK fires output events on the first
        // dispatch round-trip; one or two roundtrips is typically enough.
        event_queue.roundtrip(&mut state)?;
        event_queue.roundtrip(&mut state)?;

        // Pick the matching output.
        let wl_output = pick_output(&state.output_state, output_name)?;

        // Pool size depends on bytes-per-pixel. fp16 buffers are 2×
        // the SDR size.
        let bpp = if hdr_params.is_some() { 8 } else { 4 };
        let pool_size = (PATCH_WIDTH * PATCH_HEIGHT * bpp) as usize * 2; // double-buffer
        let pool = SlotPool::new(pool_size, &state.shm)?;
        state.pool = Some(pool);

        // Build the layer surface anchored fullscreen on the chosen output.
        let wl_surface = state.compositor_state.create_surface(&qh);
        let layer_surface = state.layer_shell.create_layer_surface(
            &qh,
            wl_surface,
            Layer::Overlay,
            Some("tristim-patch"),
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

        // HDR mode: build + attach the PQ image description now that
        // the surface exists. Wait for ready/failed.
        if let (Some(params), Some(manager)) =
            (state.hdr_params, state.color_manager.clone())
        {
            let SurfaceState::Configured(layer_surface) = &state.surface_state else {
                unreachable!("just waited for Configured above")
            };
            let wl_surface = layer_surface.wl_surface().clone();
            let cm =
                ColorManagedSurface::attach(manager, &qh, &wl_surface, params);
            state.color_managed = Some(cm);
            // Flush + roundtrip to send create + set requests and
            // pick up ready/failed.
            event_queue.roundtrip(&mut state)?;
            let ready_deadline = Instant::now() + Duration::from_secs(1);
            loop {
                let snapshot =
                    state.color_managed.as_ref().unwrap().state();
                match snapshot {
                    DescriptionState::Ready { .. } => break,
                    DescriptionState::Failed { reason } => {
                        return Err(Error::DescriptionFailed(reason));
                    }
                    DescriptionState::Pending => {}
                }
                if Instant::now() >= ready_deadline {
                    return Err(Error::NoDescriptionReady(
                        Duration::from_secs(1),
                    ));
                }
                event_queue.blocking_dispatch(&mut state)?;
            }
        }

        Ok(Self {
            conn,
            state,
            event_queue,
        })
    }

    /// Set the patch color (linear-or-encoded RGB in `0..=1`).
    ///
    /// SDR mode: bytes written are XRGB8888 with each channel scaled
    /// to `0..=255`; the compositor treats them as sRGB.
    ///
    /// HDR mode: the `0..=1` value is interpreted as a luminance
    /// fraction of `mastering_max_lum` (i.e. `1.0` = panel peak).
    /// Use [`Self::set_nits`] if you want to specify absolute cd/m²
    /// directly — more natural for HDR calibration where you're
    /// targeting specific luminance levels.
    pub fn set_color(&mut self, rgb: [f64; 3]) -> Result<(), Error> {
        match self.state.current_content {
            PatchContent::Sdr(_) => {
                let r = (rgb[0].clamp(0.0, 1.0) * 255.0).round() as u32;
                let g = (rgb[1].clamp(0.0, 1.0) * 255.0).round() as u32;
                let b = (rgb[2].clamp(0.0, 1.0) * 255.0).round() as u32;
                self.state.current_content =
                    PatchContent::Sdr(0xFF_00_00_00 | (r << 16) | (g << 8) | b);
            }
            PatchContent::HdrPqNits(_) => {
                let peak = self
                    .state
                    .hdr_params
                    .expect("hdr_params set whenever HDR content active")
                    .mastering_max_lum as f64;
                self.state.current_content = PatchContent::HdrPqNits([
                    rgb[0].clamp(0.0, 1.0) * peak,
                    rgb[1].clamp(0.0, 1.0) * peak,
                    rgb[2].clamp(0.0, 1.0) * peak,
                ]);
            }
        }
        self.state.redraw_pending = true;
        self.redraw_and_settle(Duration::from_millis(50))
    }

    /// HDR-mode patch luminance, in absolute cd/m² per channel.
    /// Returns [`Error::NotHdrMode`] if the patch was opened SDR.
    /// Values are clamped to [0, 10000] (the PQ EOTF's domain);
    /// >mastering_max_lum is allowed by the protocol (it's what
    /// `extended_target_volume` is about), but in practice the panel
    /// will hard-clip to its own peak.
    pub fn set_nits(&mut self, rgb_nits: [f64; 3]) -> Result<(), Error> {
        if !matches!(self.state.current_content, PatchContent::HdrPqNits(_)) {
            return Err(Error::NotHdrMode);
        }
        self.state.current_content = PatchContent::HdrPqNits(rgb_nits);
        self.state.redraw_pending = true;
        self.redraw_and_settle(Duration::from_millis(50))
    }

    /// Centered-window patch mode: paint a black background everywhere
    /// except a centered rectangle whose area is `fraction` of the
    /// surface. `fraction >= 1.0` (default) is equivalent to fullscreen.
    /// `fraction <= 0.0` is treated as fullscreen too (a zero-size
    /// window has no useful colorimeter target). The window is sized
    /// proportionally on each axis (i.e. each axis is scaled by
    /// `sqrt(fraction)`), keeping the window's aspect ratio equal to
    /// the surface's.
    ///
    /// Use small fractions (~0.04–0.10) to defeat the panel's ABL when
    /// measuring rated peak luminance on OLEDs. The current patch
    /// content is repainted with the new layout; no separate
    /// `set_color`/`set_nits` call needed.
    pub fn set_window_fraction(&mut self, fraction: f64) -> Result<(), Error> {
        let f = if fraction.is_finite() && fraction > 0.0 {
            fraction.min(1.0)
        } else {
            1.0
        };
        self.state.window_fraction = f;
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
        current_content: PatchContent::Sdr(0),
        current_width: PATCH_WIDTH,
        current_height: PATCH_HEIGHT,
        redraw_pending: false,
        color_manager: None,
        color_managed: None,
        hdr_params: None,
        window_fraction: 1.0,
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

/// What the patch holds — either an SDR XRGB8888 color or HDR
/// fp16 PQ-encoded per-channel nits.
#[derive(Clone, Copy, Debug)]
enum PatchContent {
    /// 8-bit XRGB packed little-endian; AppState.draw writes one
    /// u32 per pixel.
    Sdr(u32),
    /// Per-channel luminance in cd/m². AppState.draw forward-PQ
    /// encodes + writes IEEE 754 binary16 (4 channels: R, G, B,
    /// alpha=1.0 — the buffer format is alpha-undefined but writing
    /// 1.0 gives a sane value if the compositor ever samples it).
    HdrPqNits([f64; 3]),
}

struct AppState {
    registry_state: RegistryState,
    output_state: OutputState,
    compositor_state: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    pool: Option<SlotPool>,
    surface_state: SurfaceState,
    current_content: PatchContent,
    current_width: u32,
    current_height: u32,
    redraw_pending: bool,
    /// Bound `wp_color_manager_v1` global, captured at registry
    /// init. `None` for SDR-only mode or when the compositor doesn't
    /// advertise the protocol.
    color_manager: Option<WpColorManagerV1>,
    /// `Some` once an HDR description is attached to the surface.
    /// Kept alive for the lifetime of the surface (drop sends the
    /// destructor + unset).
    color_managed: Option<ColorManagedSurface>,
    /// HDR mastering params if open_hdr was used. Set up at
    /// construction; reused when the surface is rebuilt (today we
    /// don't, but future configure handling might).
    hdr_params: Option<PqDescriptionParams>,
    /// Centered-window area fraction. 1.0 = fullscreen (no black
    /// border). <1.0 → black surround + centered window of the patch
    /// color, sized so the window covers `sqrt(f)` of each axis.
    window_fraction: f64,
}

/// Compute a centered window rect covering `fraction` of the surface
/// area, with each axis scaled by `sqrt(fraction)`. fraction ≥ 1.0
/// returns the full surface.
fn window_rect(width: i32, height: i32, fraction: f64) -> (i32, i32, i32, i32) {
    if !(fraction.is_finite() && fraction > 0.0 && fraction < 1.0) {
        return (0, 0, width, height);
    }
    let scale = fraction.sqrt();
    let ww = ((width as f64 * scale).round() as i32).clamp(1, width);
    let wh = ((height as f64 * scale).round() as i32).clamp(1, height);
    let wx = (width - ww) / 2;
    let wy = (height - wh) / 2;
    (wx, wy, ww, wh)
}

/// Fill the canvas with `bg` everywhere except a `(win_x, win_y, win_w,
/// win_h)` rectangle that gets `fg`. 4 bytes per pixel.
fn fill_window_4bpp(
    canvas: &mut [u8],
    width: i32,
    bg: &[u8; 4],
    fg: &[u8; 4],
    win_x: i32,
    win_y: i32,
    win_w: i32,
    win_h: i32,
) {
    for px in canvas.chunks_exact_mut(4) {
        px.copy_from_slice(bg);
    }
    if win_w == width && win_x == 0 {
        // Fast path: window spans the full width — overwrite whole rows.
        let row_bytes = (width as usize) * 4;
        let fg_row: Vec<u8> = fg.iter().copied().cycle().take(row_bytes).collect();
        for row in win_y..win_y + win_h {
            let off = (row as usize) * row_bytes;
            canvas[off..off + row_bytes].copy_from_slice(&fg_row);
        }
    } else {
        let row_bytes = (width as usize) * 4;
        let span_bytes = (win_w as usize) * 4;
        let fg_span: Vec<u8> = fg.iter().copied().cycle().take(span_bytes).collect();
        for row in win_y..win_y + win_h {
            let off = (row as usize) * row_bytes + (win_x as usize) * 4;
            canvas[off..off + span_bytes].copy_from_slice(&fg_span);
        }
    }
}

/// Same as [`fill_window_4bpp`] but for 8-byte fp16 pixels.
fn fill_window_8bpp(
    canvas: &mut [u8],
    width: i32,
    bg: &[u8; 8],
    fg: &[u8; 8],
    win_x: i32,
    win_y: i32,
    win_w: i32,
    win_h: i32,
) {
    for px in canvas.chunks_exact_mut(8) {
        px.copy_from_slice(bg);
    }
    let row_bytes = (width as usize) * 8;
    let span_bytes = (win_w as usize) * 8;
    let fg_span: Vec<u8> = fg.iter().copied().cycle().take(span_bytes).collect();
    if win_w == width && win_x == 0 {
        for row in win_y..win_y + win_h {
            let off = (row as usize) * row_bytes;
            canvas[off..off + row_bytes].copy_from_slice(&fg_span);
        }
    } else {
        for row in win_y..win_y + win_h {
            let off = (row as usize) * row_bytes + (win_x as usize) * 8;
            canvas[off..off + span_bytes].copy_from_slice(&fg_span);
        }
    }
}

/// PQ-encode + half-float pack one RGB triple into Xbgr16161616f bytes.
fn hdr_pixel(rgb_nits: [f64; 3]) -> [u8; 8] {
    let pq = crate::pq::nits_triple_to_pq(rgb_nits);
    let r = half::f16::from_f64(pq[0]).to_le_bytes();
    let g = half::f16::from_f64(pq[1]).to_le_bytes();
    let b = half::f16::from_f64(pq[2]).to_le_bytes();
    // Alpha undefined for Xbgr; write 1.0 so a stray sampler isn't noise.
    let a = half::f16::from_f64(1.0).to_le_bytes();
    [r[0], r[1], g[0], g[1], b[0], b[1], a[0], a[1]]
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

        // Window placement (centered). fraction=1.0 ⇒ whole surface;
        // smaller fractions reduce the bright region's area by `f` while
        // preserving the surface's aspect ratio inside the window.
        let (win_x, win_y, win_w, win_h) = window_rect(width, height, self.window_fraction);

        let (buffer, _) = match self.current_content {
            PatchContent::Sdr(argb) => {
                let stride = width * 4;
                let (buffer, canvas) = pool.create_buffer(
                    width,
                    height,
                    stride,
                    wl_shm::Format::Xrgb8888,
                )?;
                // Background = opaque black; foreground = caller's color.
                let bg: [u8; 4] = 0xFF_00_00_00u32.to_le_bytes();
                let fg: [u8; 4] = argb.to_le_bytes();
                fill_window_4bpp(canvas, width, &bg, &fg, win_x, win_y, win_w, win_h);
                (buffer, ())
            }
            PatchContent::HdrPqNits(rgb_nits) => {
                let stride = width * 8;
                let (buffer, canvas) = pool.create_buffer(
                    width,
                    height,
                    stride,
                    wl_shm::Format::Xbgr16161616f,
                )?;
                // Xbgr16161616f memory layout is [R, G, B, X] half-floats
                // little-endian. Background = PQ-encoded 0 nits (panel's
                // floor); foreground = caller's PQ-encoded nits.
                let bg = hdr_pixel([0.0, 0.0, 0.0]);
                let fg = hdr_pixel(rgb_nits);
                fill_window_8bpp(canvas, width, &bg, &fg, win_x, win_y, win_w, win_h);
                (buffer, ())
            }
        };

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

// ─── wp_color_management_v1 client-side dispatch ──────────────────────────
//
// SCTK doesn't ship a handler for these so we wire the four
// interfaces by hand. The manager + surface + creator dispatches are
// stateless (just ignore supported_* + ack destructors); the
// description dispatch updates the shared DescriptionState Mutex so
// open_with_mode can poll ready/failed after the create round-trip.

impl Dispatch<WpColorManagerV1, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &WpColorManagerV1,
        event: <WpColorManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        crate::color_mgmt::handle_manager_event(event);
    }
}

impl Dispatch<WpImageDescriptionCreatorParamsV1, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &WpImageDescriptionCreatorParamsV1,
        _event: <WpImageDescriptionCreatorParamsV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Creator interface has no events.
    }
}

impl Dispatch<WpImageDescriptionV1, Arc<Mutex<DescriptionState>>> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &WpImageDescriptionV1,
        event: <WpImageDescriptionV1 as wayland_client::Proxy>::Event,
        data: &Arc<Mutex<DescriptionState>>,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        crate::color_mgmt::handle_description_event(event, data);
        let _ = WEnum::<()>::Unknown; // silence unused import in -D warnings build
    }
}

impl Dispatch<WpColorManagementSurfaceV1, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &WpColorManagementSurfaceV1,
        _event: <WpColorManagementSurfaceV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Surface extension has no events.
    }
}
