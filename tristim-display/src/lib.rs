//! Wayland layer-shell client for showing solid-color test patches on a
//! chosen output.
//!
//! A [`PatchSurface`] is opened with a [`BufferFormat`] (8-bit or fp16)
//! and an optional [`DescriptionRequest`] — a parametric
//! `wp_color_management_v1` image description to negotiate. With no
//! description the surface is *unmanaged*: the compositor interprets the
//! buffer by its own default. Patch content is set as raw code values
//! (`0..=1`) via [`PatchSurface::set_code_values`] and written to the
//! buffer verbatim — what those code values *mean* is the negotiated
//! description's job, recorded for the analysis tool to interpret.
//!
//! Usage:
//!
//! ```no_run
//! use tristim_display::{PatchSurface, BufferFormat, DescriptionRequest, Mastering};
//! // Unmanaged 8-bit SDR.
//! let mut patch = PatchSurface::open_sdr("DP-1")?;
//! patch.set_code_values([1.0, 1.0, 1.0])?;
//!
//! // fp16 surface declaring PQ + BT.2020.
//! let desc = DescriptionRequest {
//!     transfer_function: "st2084_pq".into(),
//!     primaries: "bt2020".into(),
//!     luminances: None,
//!     mastering: Some(Mastering {
//!         min_nits: 0.0005,
//!         max_nits: 400.0,
//!         max_cll_nits: 400.0,
//!         max_fall_nits: 200.0,
//!     }),
//! };
//! let mut hdr = PatchSurface::open("DP-4", BufferFormat::Xbgr16161616f, Some(desc))?;
//! hdr.set_code_values([0.5081, 0.5081, 0.5081])?;  // PQ code value ≈ 100 cd/m²
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

pub use color_mgmt::{
    AttachError, ColorCapabilities, DescriptionRequest, DescriptionState, Luminances, Mastering,
};

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use thiserror::Error;
use wayland_client::{
    Connection, Dispatch, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_output, wl_shm, wl_surface},
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

    #[error("compositor doesn't advertise wp_color_manager_v1 (required for a managed format)")]
    NoColorManager,

    #[error("compositor rejected our image description: {cause}: {message}")]
    DescriptionFailed { cause: String, message: String },

    #[error("image description never went ready within {0:?}")]
    NoDescriptionReady(Duration),

    #[error("invalid color description request: {0}")]
    BadDescription(#[from] color_mgmt::AttachError),
}

/// A layer-shell surface fixed on a chosen output, holding a solid color.
pub struct PatchSurface {
    conn: Connection,
    state: AppState,
    event_queue: wayland_client::EventQueue<AppState>,
    /// Every Wayland global the compositor advertised, as `(interface,
    /// version)` — the protocol-level compositor fingerprint, captured at
    /// connect. Recorded into the capture's `CompositorInfo`.
    advertised_globals: Vec<(String, u32)>,
    /// Compositor binary name from the socket peer credentials, if obtainable.
    compositor_process: Option<String>,
}

/// Buffer pixel format for the patch surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BufferFormat {
    /// 8-bit `XRGB8888`. Code values are quantized to `0..=255`.
    Xrgb8888,
    /// Half-float `XBGR16161616F`. Code values are written directly as
    /// `f16` — needed for HDR / wide encodings that exceed 8-bit range.
    Xbgr16161616f,
}

/// Build the [`PatchContent`] for `rgb` (code values `0..=1`) matching
/// the format of `current`. Shared by `set_code_values` / `set_border`.
fn make_content(current: PatchContent, rgb: [f64; 3]) -> PatchContent {
    match current {
        PatchContent::Sdr(_) => {
            let r = (rgb[0].clamp(0.0, 1.0) * 255.0).round() as u32;
            let g = (rgb[1].clamp(0.0, 1.0) * 255.0).round() as u32;
            let b = (rgb[2].clamp(0.0, 1.0) * 255.0).round() as u32;
            PatchContent::Sdr(0xFF_00_00_00 | (r << 16) | (g << 8) | b)
        }
        PatchContent::Fp16(_) => PatchContent::Fp16([rgb[0], rgb[1], rgb[2]]),
    }
}

impl PatchSurface {
    /// Convenience: open an unmanaged 8-bit SDR patch — no color
    /// description negotiated, so the compositor interprets the buffer
    /// by its own default.
    pub fn open_sdr(output_name: &str) -> Result<Self, Error> {
        Self::open(output_name, BufferFormat::Xrgb8888, None)
    }

    /// Open a patch surface on `output_name`.
    ///
    /// `format` selects the buffer bit depth. `description`, when
    /// `Some`, negotiates a `wp_color_management_v1` parametric image
    /// description for the surface — recording how the compositor
    /// responds is part of what the validator measures; `None` leaves
    /// the surface unmanaged.
    ///
    /// Errors: [`Error::NoColorManager`] if a description was requested
    /// but the compositor doesn't advertise the protocol;
    /// [`Error::BadDescription`] if the request named a transfer
    /// function / primaries this build can't map; [`Error::DescriptionFailed`]
    /// if the compositor rejected the description.
    pub fn open(
        output_name: &str,
        format: BufferFormat,
        description: Option<DescriptionRequest>,
    ) -> Result<Self, Error> {
        let conn = Connection::connect_to_env()?;
        let (globals, mut event_queue) = registry_queue_init::<AppState>(&conn)?;
        let qh = event_queue.handle();

        // Compositor identity, captured up front: the full advertised-globals
        // list (interface@version — the protocol fingerprint) and the peer
        // process behind the Wayland socket. Both are best-effort facts for
        // the capture; see `tristim_capture::CompositorInfo`.
        let advertised_globals = globals.contents().with_list(|list| {
            list.iter()
                .map(|g| (g.interface.clone(), g.version))
                .collect::<Vec<_>>()
        });
        let compositor_process = compositor_process_name(&conn);

        let registry_state = RegistryState::new(&globals);
        let output_state = OutputState::new(&globals, &qh);
        let compositor_state = CompositorState::bind(&globals, &qh).map_err(Error::Bind)?;
        let layer_shell = LayerShell::bind(&globals, &qh).map_err(|_| Error::NoLayerShell)?;
        let shm = Shm::bind(&globals, &qh).map_err(Error::Bind)?;

        // Optional bind: wp_color_manager_v1. Always try (so we collect
        // capabilities even when unmanaged); only error out if a
        // description was requested AND the bind failed. SCTK doesn't
        // manage this global so we go through the raw globals list.
        let color_manager = globals.bind::<WpColorManagerV1, _, _>(&qh, 1..=2, ()).ok();
        if description.is_some() && color_manager.is_none() {
            return Err(Error::NoColorManager);
        }

        let initial_content = match format {
            BufferFormat::Xrgb8888 => PatchContent::Sdr(0xFF_00_00_00),
            BufferFormat::Xbgr16161616f => PatchContent::Fp16([0.0, 0.0, 0.0]),
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
            capabilities: ColorCapabilities::default(),
            description,
            window_fraction: 1.0,
            border_content: None,
        };

        // Pump events until OutputState has enumerated all outputs, so we
        // can match `output_name`. SCTK fires output events on the first
        // dispatch round-trip; one or two roundtrips is typically enough.
        event_queue.roundtrip(&mut state)?;
        event_queue.roundtrip(&mut state)?;

        // Pick the matching output.
        let wl_output = pick_output(&state.output_state, output_name)?;

        // Pool size depends on bytes-per-pixel. fp16 buffers are 2×
        // the 8-bit size.
        let bpp = match format {
            BufferFormat::Xrgb8888 => 4,
            BufferFormat::Xbgr16161616f => 8,
        };
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

        // If a description was requested: build + attach it now that
        // the surface exists, then wait for ready/failed.
        if let (Some(req), Some(manager)) = (state.description.clone(), state.color_manager.clone())
        {
            let SurfaceState::Configured(layer_surface) = &state.surface_state else {
                unreachable!("just waited for Configured above")
            };
            let wl_surface = layer_surface.wl_surface().clone();
            let cm = ColorManagedSurface::attach(manager, &qh, &wl_surface, &req)?;
            state.color_managed = Some(cm);
            // Flush + roundtrip to send create + set requests and
            // pick up ready/failed.
            event_queue.roundtrip(&mut state)?;
            let ready_deadline = Instant::now() + Duration::from_secs(1);
            loop {
                let snapshot = state.color_managed.as_ref().unwrap().state();
                match snapshot {
                    DescriptionState::Ready { .. } => break,
                    DescriptionState::Failed { cause, message } => {
                        return Err(Error::DescriptionFailed { cause, message });
                    }
                    DescriptionState::Pending => {}
                }
                if Instant::now() >= ready_deadline {
                    return Err(Error::NoDescriptionReady(Duration::from_secs(1)));
                }
                event_queue.blocking_dispatch(&mut state)?;
            }
        }

        Ok(Self {
            conn,
            state,
            event_queue,
            advertised_globals,
            compositor_process,
        })
    }

    /// Every Wayland global the compositor advertised, as `(interface,
    /// version)`. The protocol-level compositor fingerprint.
    pub fn advertised_globals(&self) -> &[(String, u32)] {
        &self.advertised_globals
    }

    /// Compositor binary name from the Wayland socket's peer credentials
    /// (`SO_PEERCRED` → `/proc/<pid>/comm`), e.g. `"niri"`. `None` when the
    /// peer isn't a local process or the lookup failed.
    pub fn compositor_process(&self) -> Option<&str> {
        self.compositor_process.as_deref()
    }

    /// What the compositor advertised through `wp_color_manager_v1`'s
    /// `supported_*` events. Empty (and `done == false`) if the
    /// compositor exposes no color management. A fact the validator
    /// records verbatim.
    pub fn color_capabilities(&self) -> &ColorCapabilities {
        &self.state.capabilities
    }

    /// The negotiation outcome for the color description attached to
    /// this surface, or `None` if no description was attached (SDR /
    /// unmanaged). The caller maps `None → Unmanaged`,
    /// `Some(Ready) → accepted`, `Some(Failed) → rejected`.
    pub fn description_state(&self) -> Option<DescriptionState> {
        self.state.color_managed.as_ref().map(|cm| cm.state())
    }

    /// Write the per-channel code values (`0..=1`) to the patch.
    ///
    /// These are *exactly* the values handed to the compositor — no
    /// encoding or interpretation. For an `Xrgb8888` surface each
    /// channel is quantized to `0..=255`; for `Xbgr16161616f` it is
    /// written directly as a half-float. What those code values *mean*
    /// (e.g. PQ-encoded luminance) is determined by the negotiated
    /// color description and is the analysis tool's concern, not ours.
    pub fn set_code_values(&mut self, rgb: [f64; 3]) -> Result<(), Error> {
        self.state.current_content = make_content(self.state.current_content, rgb);
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
    /// `set_code_values` call needed.
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

    /// Set the surround code values (`0..=1`) painted outside the
    /// centered window when [`set_window_fraction`](Self::set_window_fraction)
    /// is below 1.0. Encoded into the surface's buffer format exactly
    /// like [`set_code_values`](Self::set_code_values).
    ///
    /// Most useful as an **anti-CABL** measure: panels that gate the
    /// backlight off below some frame-average brightness threshold
    /// will render low-intensity centred patches as black if the
    /// surround is also black. A modest border value keeps average
    /// frame brightness above the threshold without contaminating the
    /// colorimeter's view of the central patch.
    ///
    /// Persists across subsequent `set_code_values` calls until cleared
    /// (`clear_border`) or overwritten.
    pub fn set_border(&mut self, rgb: [f64; 3]) -> Result<(), Error> {
        self.state.border_content = Some(make_content(self.state.current_content, rgb));
        self.state.redraw_pending = true;
        self.redraw_and_settle(Duration::from_millis(50))
    }

    /// Revert the surround to the black default.
    pub fn clear_border(&mut self) -> Result<(), Error> {
        self.state.border_content = None;
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
        capabilities: ColorCapabilities::default(),
        description: None,
        window_fraction: 1.0,
        border_content: None,
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

/// What the compositor can do, queried up front without placing a surface: the
/// `wp_color_manager_v1` advertised set plus the `wl_shm` buffer formats.
/// Lets a caller tell, *before* a capture, which trial formats the compositor
/// can actually reproduce (vs. failing at run time). These facts are
/// connection-global, not per-output.
#[derive(Clone, Debug, Default)]
pub struct DisplayCapabilities {
    /// Color-management capabilities. `done == false` with empty lists means
    /// the compositor advertises no `wp_color_manager_v1` at all.
    pub color: ColorCapabilities,
    /// The `wl_shm` buffer formats the compositor advertised.
    shm_formats: Vec<wl_shm::Format>,
}

impl DisplayCapabilities {
    /// Build a capability set advertising the named transfer functions and
    /// primaries (same names as [`DescriptionRequest`]) plus optional fp16
    /// buffer support. The live equivalent comes from [`query_capabilities`];
    /// this is for callers (and tests) holding the facts another way. Names
    /// this build doesn't map are dropped; `done` is set iff any TF is known.
    pub fn advertising(transfer_functions: &[&str], primaries: &[&str], fp16: bool) -> Self {
        let tf: Vec<String> = transfer_functions
            .iter()
            .filter_map(|n| color_mgmt::tf_named(n).map(|t| format!("{t:?}")))
            .collect();
        let prim: Vec<String> = primaries
            .iter()
            .filter_map(|n| color_mgmt::primaries_named(n).map(|p| format!("{p:?}")))
            .collect();
        let done = !tf.is_empty();
        let mut shm_formats = vec![wl_shm::Format::Xrgb8888];
        if fp16 {
            shm_formats.push(wl_shm::Format::Xbgr16161616f);
        }
        Self {
            color: ColorCapabilities {
                transfer_functions: tf,
                primaries: prim,
                features: Vec::new(),
                render_intents: Vec::new(),
                done,
            },
            shm_formats,
        }
    }

    /// Whether the compositor can present this buffer format. `Xrgb8888` is
    /// mandatory per the `wl_shm` spec, so it's always available; `fp16` must
    /// be explicitly advertised.
    pub fn supports_buffer_format(&self, f: BufferFormat) -> bool {
        match f {
            BufferFormat::Xrgb8888 => true,
            BufferFormat::Xbgr16161616f => {
                self.shm_formats.contains(&wl_shm::Format::Xbgr16161616f)
            }
        }
    }

    /// Whether the compositor exposes color management at all (the
    /// `wp_color_manager_v1` global, with its enumeration completed).
    pub fn has_color_management(&self) -> bool {
        self.color.done && !self.color.transfer_functions.is_empty()
    }

    /// Whether the named transfer function (e.g. `"srgb"`, `"st2084_pq"`) is in
    /// the advertised set. Names match [`DescriptionRequest::transfer_function`].
    pub fn supports_transfer_function(&self, name: &str) -> bool {
        match color_mgmt::tf_named(name) {
            Some(tf) => self
                .color
                .transfer_functions
                .iter()
                .any(|s| *s == format!("{tf:?}")),
            None => false,
        }
    }

    /// Whether the named primaries (e.g. `"srgb"`, `"bt2020"`) are advertised.
    pub fn supports_primaries(&self, name: &str) -> bool {
        match color_mgmt::primaries_named(name) {
            Some(p) => self.color.primaries.iter().any(|s| *s == format!("{p:?}")),
            None => false,
        }
    }
}

/// Query what the compositor can do — color management + buffer formats —
/// without placing a surface. A quick connect + bind + roundtrip, like
/// [`list_outputs`]. Requires `wlr-layer-shell` (as the capture itself does),
/// so the error path matches.
pub fn query_capabilities() -> Result<DisplayCapabilities, Error> {
    let conn = Connection::connect_to_env()?;
    let (globals, mut event_queue) = registry_queue_init::<AppState>(&conn)?;
    let qh = event_queue.handle();
    let registry_state = RegistryState::new(&globals);
    let output_state = OutputState::new(&globals, &qh);
    // Bind the manager so it emits its supported_* enumeration. Absent on
    // compositors without color management (capabilities then stay empty).
    let color_manager = globals.bind::<WpColorManagerV1, _, _>(&qh, 1..=2, ()).ok();

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
        color_manager,
        color_managed: None,
        capabilities: ColorCapabilities::default(),
        description: None,
        window_fraction: 1.0,
        border_content: None,
    };
    // Two roundtrips: the first delivers the `wl_shm` formats, the second the
    // manager's `supported_*` + `done` enumeration.
    event_queue.roundtrip(&mut state)?;
    event_queue.roundtrip(&mut state)?;

    Ok(DisplayCapabilities {
        color: state.capabilities,
        shm_formats: state.shm.formats().to_vec(),
    })
}

/// The compositor's binary name, via the Wayland socket's peer credentials.
///
/// `SO_PEERCRED` on the connection fd gives the server-side PID; its
/// `/proc/<pid>/comm` is the compositor binary (e.g. `niri`, `kwin_wayland`,
/// `gnome-shell`). `None` when the peer isn't a local process — a non-`AF_UNIX`
/// transport, a proxy like waypipe, or any failure. Linux-only, as is the rest
/// of this Wayland crate.
fn compositor_process_name(conn: &Connection) -> Option<String> {
    use std::os::fd::AsRawFd;

    let backend = conn.backend();
    let fd = backend.poll_fd().as_raw_fd();

    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `fd` is the live Wayland socket for the duration of this call
    // (`backend` is held above); `cred`/`len` are correctly sized and only
    // read back on success.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if rc != 0 || cred.pid <= 0 {
        return None;
    }
    let comm = std::fs::read_to_string(format!("/proc/{}/comm", cred.pid)).ok()?;
    let name = comm.trim();
    (!name.is_empty()).then(|| name.to_string())
}

fn pick_output(output_state: &OutputState, name: &str) -> Result<wl_output::WlOutput, Error> {
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

/// What the patch holds — raw buffer code values in one of the two
/// supported pixel formats. No encoding happens here: the values are
/// written to the buffer verbatim (quantized for 8-bit).
#[derive(Clone, Copy, Debug)]
enum PatchContent {
    /// 8-bit XRGB packed little-endian; AppState.draw writes one
    /// u32 per pixel.
    Sdr(u32),
    /// Per-channel code values in `0..=1`. AppState.draw writes them as
    /// IEEE 754 binary16 (4 channels: R, G, B, alpha=1.0 — the buffer
    /// format is alpha-undefined but writing 1.0 gives a sane value if
    /// the compositor ever samples it).
    Fp16([f64; 3]),
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
    /// Capabilities accumulated from the manager's `supported_*`
    /// events during registry enumeration. Empty if no manager bound.
    capabilities: ColorCapabilities,
    /// The color description to negotiate, if any. `None` = unmanaged.
    /// Held so the attach step (after configure) can build it.
    description: Option<DescriptionRequest>,
    /// Centered-window area fraction. 1.0 = fullscreen (no black
    /// border). <1.0 → surround of `border_content` (or black if
    /// unset) + centered window of the patch color, sized so the
    /// window covers `sqrt(f)` of each axis.
    window_fraction: f64,
    /// Surround colour when `window_fraction < 1.0`. `None` = legacy
    /// black surround (`#000000` SDR / 0 nits HDR). `Some(...)` paints
    /// the chosen colour everywhere outside the centered window —
    /// useful when the panel does content-adaptive backlight dimming
    /// and would otherwise gate the backlight off during low-intensity
    /// measurements, killing dim patch measurements.
    border_content: Option<PatchContent>,
}

/// A pixel rectangle (origin + size) on the surface, used for
/// centered-window patch placement.
#[derive(Clone, Copy)]
struct Rect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

/// Compute a centered window rect covering `fraction` of the surface
/// area, with each axis scaled by `sqrt(fraction)`. fraction ≥ 1.0
/// returns the full surface.
fn window_rect(width: i32, height: i32, fraction: f64) -> Rect {
    if !(fraction.is_finite() && fraction > 0.0 && fraction < 1.0) {
        return Rect {
            x: 0,
            y: 0,
            w: width,
            h: height,
        };
    }
    let scale = fraction.sqrt();
    let ww = ((width as f64 * scale).round() as i32).clamp(1, width);
    let wh = ((height as f64 * scale).round() as i32).clamp(1, height);
    Rect {
        x: (width - ww) / 2,
        y: (height - wh) / 2,
        w: ww,
        h: wh,
    }
}

/// Fill the canvas with `bg` everywhere except the `win` rectangle,
/// which gets `fg`. 4 bytes per pixel.
fn fill_window_4bpp(canvas: &mut [u8], width: i32, bg: &[u8; 4], fg: &[u8; 4], win: &Rect) {
    for px in canvas.chunks_exact_mut(4) {
        px.copy_from_slice(bg);
    }
    if win.w == width && win.x == 0 {
        // Fast path: window spans the full width — overwrite whole rows.
        let row_bytes = (width as usize) * 4;
        let fg_row: Vec<u8> = fg.iter().copied().cycle().take(row_bytes).collect();
        for row in win.y..win.y + win.h {
            let off = (row as usize) * row_bytes;
            canvas[off..off + row_bytes].copy_from_slice(&fg_row);
        }
    } else {
        let row_bytes = (width as usize) * 4;
        let span_bytes = (win.w as usize) * 4;
        let fg_span: Vec<u8> = fg.iter().copied().cycle().take(span_bytes).collect();
        for row in win.y..win.y + win.h {
            let off = (row as usize) * row_bytes + (win.x as usize) * 4;
            canvas[off..off + span_bytes].copy_from_slice(&fg_span);
        }
    }
}

/// Same as [`fill_window_4bpp`] but for 8-byte fp16 pixels.
fn fill_window_8bpp(canvas: &mut [u8], width: i32, bg: &[u8; 8], fg: &[u8; 8], win: &Rect) {
    for px in canvas.chunks_exact_mut(8) {
        px.copy_from_slice(bg);
    }
    let row_bytes = (width as usize) * 8;
    let span_bytes = (win.w as usize) * 8;
    let fg_span: Vec<u8> = fg.iter().copied().cycle().take(span_bytes).collect();
    if win.w == width && win.x == 0 {
        for row in win.y..win.y + win.h {
            let off = (row as usize) * row_bytes;
            canvas[off..off + row_bytes].copy_from_slice(&fg_span);
        }
    } else {
        for row in win.y..win.y + win.h {
            let off = (row as usize) * row_bytes + (win.x as usize) * 8;
            canvas[off..off + span_bytes].copy_from_slice(&fg_span);
        }
    }
}

/// Half-float pack one RGB code-value triple into Xbgr16161616f bytes.
/// Values are written verbatim — no PQ/encoding (that meaning lives in
/// the negotiated color description, not here).
fn fp16_pixel(rgb: [f64; 3]) -> [u8; 8] {
    let r = half::f16::from_f64(rgb[0]).to_le_bytes();
    let g = half::f16::from_f64(rgb[1]).to_le_bytes();
    let b = half::f16::from_f64(rgb[2]).to_le_bytes();
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
        let win = window_rect(width, height, self.window_fraction);

        let (buffer, _) = match self.current_content {
            PatchContent::Sdr(argb) => {
                let stride = width * 4;
                let (buffer, canvas) =
                    pool.create_buffer(width, height, stride, wl_shm::Format::Xrgb8888)?;
                // Background = configured border colour, falling back to
                // opaque black for the legacy unset case.
                let bg: [u8; 4] = match self.border_content {
                    Some(PatchContent::Sdr(b)) => b.to_le_bytes(),
                    _ => 0xFF_00_00_00u32.to_le_bytes(),
                };
                let fg: [u8; 4] = argb.to_le_bytes();
                fill_window_4bpp(canvas, width, &bg, &fg, &win);
                (buffer, ())
            }
            PatchContent::Fp16(rgb) => {
                let stride = width * 8;
                let (buffer, canvas) =
                    pool.create_buffer(width, height, stride, wl_shm::Format::Xbgr16161616f)?;
                // Xbgr16161616f memory layout is [R, G, B, X] half-floats
                // little-endian. Background = configured border code
                // values, falling back to 0 for the unset case.
                let bg = match self.border_content {
                    Some(PatchContent::Fp16(border)) => fp16_pixel(border),
                    _ => fp16_pixel([0.0, 0.0, 0.0]),
                };
                let fg = fp16_pixel(rgb);
                fill_window_8bpp(canvas, width, &bg, &fg, &win);
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
        let layer_owned =
            match std::mem::replace(&mut self.surface_state, SurfaceState::WaitingForOutputs) {
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
// interfaces by hand. The manager dispatch accumulates supported_*
// into AppState.capabilities; the surface + creator dispatches are
// stateless (ack destructors); the description dispatch updates the
// shared DescriptionState Mutex so open_with_mode can poll
// ready/failed after the create round-trip.

impl Dispatch<WpColorManagerV1, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &WpColorManagerV1,
        event: <WpColorManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        crate::color_mgmt::handle_manager_event(event, &mut state.capabilities);
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
