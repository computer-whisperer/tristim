//! Wayland layer-shell client for showing solid-color test patches on a
//! chosen output.
//!
//! A [`PatchSurface`] is opened with a [`BufferFormat`] (any RGB-family
//! `wl_shm` format — 8-bit through 10-bit, 16-bit unorm, and fp16; see
//! [`format`]) and an optional [`DescriptionRequest`] — a
//! `wp_color_management_v1` image description to negotiate: parametric
//! (named or power-law TF, named or custom primaries, luminances,
//! mastering metadata, render intent) or the Windows-scRGB shortcut.
//! With no description the surface is *unmanaged*: the compositor
//! interprets the buffer by its own default. Patch content is set as
//! raw code values via [`PatchSurface::set_code_values`] and written to
//! the buffer verbatim — what those code values *mean* is the
//! negotiated description's job, recorded for the analysis tool to
//! interpret.
//!
//! Everything optional is gated on what the compositor actually
//! advertised — buffer formats via
//! [`DisplayCapabilities::supports_buffer_format`], color-management
//! features/TFs/primaries/intents inside
//! [`PatchSurface::open`] (an unadvertised request would be a fatal
//! protocol error, so it's refused client-side as
//! [`Error::BadDescription`] instead).
//!
//! Usage:
//!
//! ```no_run
//! use tristim_display::{
//!     PatchSurface, BufferFormat, DescriptionRequest, Mastering, ParametricDescription,
//! };
//! // Unmanaged 8-bit SDR.
//! let mut patch = PatchSurface::open_sdr("DP-1")?;
//! patch.set_code_values([1.0, 1.0, 1.0])?;
//!
//! // fp16 surface declaring PQ + BT.2020 with mastering metadata.
//! let mut params = ParametricDescription::named("st2084_pq", "bt2020");
//! params.mastering = Some(Mastering {
//!     luminance_nits: Some((0.0005, 400.0)),
//!     max_cll_nits: Some(400.0),
//!     max_fall_nits: Some(200.0),
//!     ..Default::default()
//! });
//! let desc = DescriptionRequest::parametric(params);
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
pub mod format;
pub mod pq;

pub use color_mgmt::{
    AttachError, ColorCapabilities, DescriptionKind, DescriptionRequest, DescriptionState,
    Luminances, Mastering, ParametricDescription, PrimariesChoice, PrimaryCoords, TransferChoice,
};
pub use format::{BufferFormat, EncodedPixel};

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

impl PatchSurface {
    /// Convenience: open an unmanaged 8-bit SDR patch — no color
    /// description negotiated, so the compositor interprets the buffer
    /// by its own default.
    pub fn open_sdr(output_name: &str) -> Result<Self, Error> {
        Self::open(output_name, BufferFormat::Xrgb8888, None)
    }

    /// Open a patch surface on `output_name`.
    ///
    /// `format` selects the buffer pixel format (see [`BufferFormat`] —
    /// only `Xrgb8888`/`Argb8888` are mandatory; check
    /// [`DisplayCapabilities::supports_buffer_format`] first for the
    /// rest). `description`, when `Some`, negotiates a
    /// `wp_color_management_v1` image description for the surface —
    /// recording how the compositor responds is part of what the
    /// validator measures; `None` leaves the surface unmanaged.
    ///
    /// Errors: [`Error::NoColorManager`] if a description was requested
    /// but the compositor doesn't advertise the protocol;
    /// [`Error::BadDescription`] if the request names something this
    /// build can't map, or uses a feature/TF/primaries/intent the
    /// compositor didn't advertise (sending those anyway would be a
    /// fatal protocol error); [`Error::DescriptionFailed`] if the
    /// compositor rejected the description.
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

        let mut state = AppState {
            registry_state,
            output_state,
            compositor_state,
            layer_shell,
            shm,
            pool: None,
            surface_state: SurfaceState::WaitingForOutputs,
            format,
            current_rgb: [0.0; 3],
            current_width: PATCH_WIDTH,
            current_height: PATCH_HEIGHT,
            redraw_pending: true,
            color_manager,
            color_managed: None,
            capabilities: ColorCapabilities::default(),
            description,
            window_fraction: 1.0,
            border_rgb: None,
        };

        // Pump events until OutputState has enumerated all outputs, so we
        // can match `output_name`. SCTK fires output events on the first
        // dispatch round-trip; one or two roundtrips is typically enough.
        event_queue.roundtrip(&mut state)?;
        event_queue.roundtrip(&mut state)?;

        // Pick the matching output.
        let wl_output = pick_output(&state.output_state, output_name)?;

        // Pool size depends on the format's bytes-per-pixel.
        let pool_size = (PATCH_WIDTH * PATCH_HEIGHT) as usize * format.bytes_per_pixel() * 2; // double-buffer
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
            let cm =
                ColorManagedSurface::attach(manager, &qh, &wl_surface, &req, &state.capabilities)?;
            // Keep a handle to the negotiation state before `cm` moves into
            // `state` — the dispatch loop below needs `&mut state`.
            let desc_state = cm.state.clone();
            state.color_managed = Some(cm);
            // Flush + roundtrip to send create + set requests and
            // pick up ready/failed.
            event_queue.roundtrip(&mut state)?;
            let ready_deadline = Instant::now() + Duration::from_secs(1);
            loop {
                let snapshot = desc_state.lock().unwrap().clone();
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

    /// Write the per-channel code values to the patch.
    ///
    /// These are *exactly* the values handed to the compositor — no
    /// encoding or interpretation. Unorm formats quantize `0..=1` to
    /// the channel depth (out-of-range clamps); float formats carry the
    /// value bit-exactly, including extended-range values outside
    /// `0..=1` (scRGB). What those code values *mean* (e.g. PQ-encoded
    /// luminance) is determined by the negotiated color description
    /// and is the analysis tool's concern, not ours. See
    /// [`BufferFormat::encode`] for the exact packing.
    pub fn set_code_values(&mut self, rgb: [f64; 3]) -> Result<(), Error> {
        self.state.current_rgb = rgb;
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
        self.state.border_rgb = Some(rgb);
        self.state.redraw_pending = true;
        self.redraw_and_settle(Duration::from_millis(50))
    }

    /// Revert the surround to the black default.
    pub fn clear_border(&mut self) -> Result<(), Error> {
        self.state.border_rgb = None;
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
        format: BufferFormat::Xrgb8888,
        current_rgb: [0.0; 3],
        current_width: PATCH_WIDTH,
        current_height: PATCH_HEIGHT,
        redraw_pending: false,
        color_manager: None,
        color_managed: None,
        capabilities: ColorCapabilities::default(),
        description: None,
        window_fraction: 1.0,
        border_rgb: None,
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
    /// primaries (same names as [`DescriptionRequest`]) plus the given
    /// buffer formats. The live equivalent comes from
    /// [`query_capabilities`]; this is for callers (and tests) holding the
    /// facts another way. Names this build doesn't map are dropped; `done`
    /// is set iff any TF is known. The advertised features are filled with
    /// the full parametric set (this constructor's callers care about
    /// format reachability, not feature gating).
    pub fn advertising(
        transfer_functions: &[&str],
        primaries: &[&str],
        formats: &[BufferFormat],
    ) -> Self {
        use wayland_protocols::wp::color_management::v1::client::wp_color_manager_v1::Feature;
        let tf: Vec<String> = transfer_functions
            .iter()
            .filter_map(|n| color_mgmt::tf_named(n).map(|t| format!("{t:?}")))
            .collect();
        let prim: Vec<String> = primaries
            .iter()
            .filter_map(|n| color_mgmt::primaries_named(n).map(|p| format!("{p:?}")))
            .collect();
        let done = !tf.is_empty();
        let mut shm_formats = vec![wl_shm::Format::Xrgb8888, wl_shm::Format::Argb8888];
        shm_formats.extend(formats.iter().map(|f| f.wl_format()));
        Self {
            color: ColorCapabilities {
                transfer_functions: tf,
                primaries: prim,
                features: [
                    Feature::Parametric,
                    Feature::SetPrimaries,
                    Feature::SetTfPower,
                    Feature::SetLuminances,
                    Feature::SetMasteringDisplayPrimaries,
                    Feature::WindowsScrgb,
                ]
                .iter()
                .map(|f| format!("{f:?}"))
                .collect(),
                render_intents: Vec::new(),
                done,
            },
            shm_formats,
        }
    }

    /// Whether the compositor can present this buffer format.
    /// `Xrgb8888` and `Argb8888` are mandatory per the `wl_shm` spec, so
    /// they're always available; everything else must be explicitly
    /// advertised.
    pub fn supports_buffer_format(&self, f: BufferFormat) -> bool {
        matches!(f, BufferFormat::Xrgb8888 | BufferFormat::Argb8888)
            || self.shm_formats.contains(&f.wl_format())
    }

    /// Every advertised `wl_shm` format this crate can write (plus the
    /// spec-mandatory 8-bit pair), in [`BufferFormat::ALL`] table order.
    pub fn supported_buffer_formats(&self) -> Vec<BufferFormat> {
        BufferFormat::ALL
            .iter()
            .copied()
            .filter(|&f| self.supports_buffer_format(f))
            .collect()
    }

    /// The first format in `preference` order the compositor supports —
    /// e.g. `first_supported(&[Xbgr16161616f, Abgr16161616f, Xbgr16161616,
    /// Xrgb2101010])` for "fp16 preferred, deep unorm acceptable".
    pub fn first_supported(&self, preference: &[BufferFormat]) -> Option<BufferFormat> {
        preference
            .iter()
            .copied()
            .find(|&f| self.supports_buffer_format(f))
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

    /// Whether the named optional feature (e.g. `"set_luminances"`,
    /// `"windows_scrgb"`) is advertised. Names match the protocol's
    /// `feature` enum entries.
    pub fn supports_feature(&self, name: &str) -> bool {
        match color_mgmt::feature_named(name) {
            Some(f) => self.color.has_feature(f),
            None => false,
        }
    }

    /// Whether the named render intent is advertised. `"perceptual"` is
    /// the protocol baseline and always reports `true` when the
    /// compositor has color management at all.
    pub fn supports_render_intent(&self, name: &str) -> bool {
        if name == "perceptual" {
            return self.has_color_management();
        }
        match color_mgmt::intent_named(name) {
            Some(i) => self.color.has_intent(i),
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
        format: BufferFormat::Xrgb8888,
        current_rgb: [0.0; 3],
        current_width: PATCH_WIDTH,
        current_height: PATCH_HEIGHT,
        redraw_pending: false,
        color_manager,
        color_managed: None,
        capabilities: ColorCapabilities::default(),
        description: None,
        window_fraction: 1.0,
        border_rgb: None,
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

struct AppState {
    registry_state: RegistryState,
    output_state: OutputState,
    compositor_state: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    pool: Option<SlotPool>,
    surface_state: SurfaceState,
    /// The surface's buffer pixel format, fixed at open time. Patch and
    /// border code values are packed into it at draw time by
    /// [`BufferFormat::encode`].
    format: BufferFormat,
    /// The patch's per-channel code values, written verbatim.
    current_rgb: [f64; 3],
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
    /// Surround code values when `window_fraction < 1.0`. `None` = the
    /// black default (all channels 0). `Some(...)` paints the chosen
    /// values everywhere outside the centered window — useful when the
    /// panel does content-adaptive backlight dimming and would
    /// otherwise gate the backlight off during low-intensity
    /// measurements, killing dim patch measurements.
    border_rgb: Option<[f64; 3]>,
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
/// which gets `fg`. Pixel size is `bg.len()` (== `fg.len()`), any
/// format's bytes-per-pixel.
fn fill_window(canvas: &mut [u8], width: i32, bg: &[u8], fg: &[u8], win: &Rect) {
    let bpp = bg.len();
    debug_assert_eq!(fg.len(), bpp);
    for px in canvas.chunks_exact_mut(bpp) {
        px.copy_from_slice(bg);
    }
    let row_bytes = (width as usize) * bpp;
    let span_bytes = (win.w as usize) * bpp;
    let fg_span: Vec<u8> = fg.iter().copied().cycle().take(span_bytes).collect();
    if win.w == width && win.x == 0 {
        // Fast path: window spans the full width — overwrite whole rows.
        for row in win.y..win.y + win.h {
            let off = (row as usize) * row_bytes;
            canvas[off..off + row_bytes].copy_from_slice(&fg_span);
        }
    } else {
        for row in win.y..win.y + win.h {
            let off = (row as usize) * row_bytes + (win.x as usize) * bpp;
            canvas[off..off + span_bytes].copy_from_slice(&fg_span);
        }
    }
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

        // One generic path for every format: pack the patch and border
        // code values into the surface's pixel format, then fill.
        let stride = width * self.format.bytes_per_pixel() as i32;
        let (buffer, canvas) =
            pool.create_buffer(width, height, stride, self.format.wl_format())?;
        let fg = self.format.encode(self.current_rgb);
        let bg = self.format.encode(self.border_rgb.unwrap_or([0.0; 3]));
        fill_window(canvas, width, &bg, &fg, &win);

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
