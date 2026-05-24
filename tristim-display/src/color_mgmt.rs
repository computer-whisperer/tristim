//! Client-side `wp_color_management_v1` driver — attach a parametric
//! image description to our patch surface so the compositor scans it
//! out as the declared color encoding, and record what the compositor
//! advertised + how it responded.
//!
//! Why we write this directly instead of using a higher-level
//! wrapper: SCTK 0.19 doesn't ship one, and the protocol surface we
//! actually exercise is small (one description, one attachment per
//! surface lifetime). The dispatch impls here accumulate the manager's
//! supported_* enumeration events into [`ColorCapabilities`] (a fact
//! the validator records) and track the description's ready/failed
//! outcome for the caller.

use std::sync::{Arc, Mutex};

use wayland_client::{Dispatch, QueueHandle, WEnum, protocol::wl_surface::WlSurface};
use wayland_protocols::wp::color_management::v1::client::{
    wp_color_management_surface_v1::WpColorManagementSurfaceV1,
    wp_color_manager_v1::{self, WpColorManagerV1},
    wp_image_description_creator_params_v1::WpImageDescriptionCreatorParamsV1,
    wp_image_description_v1::{self, WpImageDescriptionV1},
};

/// A parametric color description to negotiate with the compositor.
///
/// Transfer function and primaries are named by their protocol-enum
/// strings (e.g. `"st2084_pq"`, `"bt2020"`) — see [`tf_named`] /
/// [`primaries_named`] for the supported set. Luminances and mastering
/// metadata are optional and given in semantic units (cd/m²); they are
/// converted to the protocol's tick units at attach time.
#[derive(Clone, Debug, PartialEq)]
pub struct DescriptionRequest {
    pub transfer_function: String,
    pub primaries: String,
    pub luminances: Option<Luminances>,
    pub mastering: Option<Mastering>,
}

/// Reference luminances for the description, in cd/m².
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Luminances {
    pub min_nits: f64,
    pub max_nits: f64,
    pub reference_nits: f64,
}

/// Mastering-display metadata for the description, in cd/m².
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mastering {
    pub min_nits: f64,
    pub max_nits: f64,
    pub max_cll_nits: f64,
    pub max_fall_nits: f64,
}

/// Map a transfer-function name to the protocol enum, or `None` if this
/// build doesn't map it. Extend as needed — names are the protocol's own
/// `transfer_function` enum entries.
pub fn tf_named(name: &str) -> Option<wp_color_manager_v1::TransferFunction> {
    use wp_color_manager_v1::TransferFunction as T;
    Some(match name {
        "bt1886" => T::Bt1886,
        "gamma22" => T::Gamma22,
        "gamma28" => T::Gamma28,
        "srgb" => T::Srgb,
        "ext_srgb" => T::ExtSrgb,
        "st2084_pq" => T::St2084Pq,
        "hlg" => T::Hlg,
        _ => return None,
    })
}

/// Map a primaries name to the protocol enum, or `None` if this build
/// doesn't map it. Names are the protocol's own `primaries` enum entries.
pub fn primaries_named(name: &str) -> Option<wp_color_manager_v1::Primaries> {
    use wp_color_manager_v1::Primaries as P;
    Some(match name {
        "srgb" => P::Srgb,
        "pal" => P::Pal,
        "ntsc" => P::Ntsc,
        "bt2020" => P::Bt2020,
        "dci_p3" => P::DciP3,
        "display_p3" => P::DisplayP3,
        "adobe_rgb" => P::AdobeRgb,
        _ => return None,
    })
}

/// Outcome of building the parametric description. Updated by the
/// `wp_image_description_v1.ready` / `.failed` event. Caller polls
/// (or roundtrips) until `Pending → Ready | Failed`.
#[derive(Clone, Debug)]
pub enum DescriptionState {
    Pending,
    /// Compositor accepted the description and assigned it `identity`.
    Ready {
        identity: u64,
    },
    /// Compositor rejected the description. `cause` is the protocol enum
    /// name (e.g. `"unsupported"`), `message` its human string.
    Failed {
        cause: String,
        message: String,
    },
}

/// What the compositor advertised through `wp_color_manager_v1`'s
/// `supported_*` enumeration events. Each list holds the protocol enum
/// variant names (e.g. `"St2084Pq"`, `"Bt2020"`), or `"unknown(N)"` for
/// values this build's protocol bindings don't recognise. An all-empty
/// set with `done == false` means the compositor exposes no color
/// management at all.
#[derive(Clone, Debug, Default)]
pub struct ColorCapabilities {
    pub transfer_functions: Vec<String>,
    pub primaries: Vec<String>,
    pub features: Vec<String>,
    pub render_intents: Vec<String>,
    /// Whether the manager's `done` event arrived (enumeration complete).
    pub done: bool,
}

/// Render a `WEnum` capability value as a stable string: the bound
/// enum's `Debug` name for known values, `unknown(N)` otherwise.
fn wenum_str<T: std::fmt::Debug>(w: WEnum<T>) -> String {
    match w {
        WEnum::Value(v) => format!("{v:?}"),
        WEnum::Unknown(n) => format!("unknown({n})"),
    }
}

/// nits → the protocol's `min_lum` tick unit (0.0001 cd/m²).
fn nits_to_min_ticks(nits: f64) -> u32 {
    (nits * 10_000.0).round().max(0.0) as u32
}

/// Wraps the lifetime of a color-management binding for one patch
/// surface. Holds the manager + description + per-surface extension
/// objects; dropping detaches the description (and the surface
/// extension destructor sends `unset_image_description` per spec).
pub struct ColorManagedSurface {
    pub manager: WpColorManagerV1,
    pub description: WpImageDescriptionV1,
    pub surface_ext: WpColorManagementSurfaceV1,
    pub state: Arc<Mutex<DescriptionState>>,
}

/// Reason a description couldn't even be requested (before the compositor
/// weighs in via ready/failed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachError {
    UnknownTransferFunction(String),
    UnknownPrimaries(String),
}

impl std::fmt::Display for AttachError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AttachError::UnknownTransferFunction(n) => {
                write!(f, "unsupported transfer function name: {n:?}")
            }
            AttachError::UnknownPrimaries(n) => write!(f, "unsupported primaries name: {n:?}"),
        }
    }
}

impl std::error::Error for AttachError {}

impl ColorManagedSurface {
    /// Build a parametric description from `req` + attach it to
    /// `wl_surface`. Caller must roundtrip the event queue afterwards to
    /// flush the create + set requests and pick up the `ready`/`failed`
    /// event.
    pub fn attach<D>(
        manager: WpColorManagerV1,
        qh: &QueueHandle<D>,
        wl_surface: &WlSurface,
        req: &DescriptionRequest,
    ) -> Result<Self, AttachError>
    where
        D: Dispatch<WpImageDescriptionCreatorParamsV1, ()> + 'static,
        D: Dispatch<WpImageDescriptionV1, Arc<Mutex<DescriptionState>>> + 'static,
        D: Dispatch<WpColorManagementSurfaceV1, ()> + 'static,
    {
        let tf = tf_named(&req.transfer_function)
            .ok_or_else(|| AttachError::UnknownTransferFunction(req.transfer_function.clone()))?;
        let primaries = primaries_named(&req.primaries)
            .ok_or_else(|| AttachError::UnknownPrimaries(req.primaries.clone()))?;

        // 1) Build a parametric creator.
        let creator = manager.create_parametric_creator(qh, ());

        // 2) Required fields: TF + primaries.
        creator.set_tf_named(tf);
        creator.set_primaries_named(primaries);

        // 3) Optional luminances + mastering metadata.
        if let Some(l) = req.luminances {
            creator.set_luminances(
                nits_to_min_ticks(l.min_nits),
                l.max_nits.round().max(0.0) as u32,
                l.reference_nits.round().max(0.0) as u32,
            );
        }
        if let Some(m) = req.mastering {
            creator.set_mastering_luminance(
                nits_to_min_ticks(m.min_nits),
                m.max_nits.round().max(0.0) as u32,
            );
            creator.set_max_cll(m.max_cll_nits.round().max(0.0) as u32);
            creator.set_max_fall(m.max_fall_nits.round().max(0.0) as u32);
        }

        // 4) Materialize the description. The state Mutex is the
        //    side-channel the dispatch impl updates from ready/failed.
        let state = Arc::new(Mutex::new(DescriptionState::Pending));
        let description = creator.create(qh, state.clone());

        // 5) Get the surface extension + set the description on it with
        //    perceptual intent. Compositors must support perceptual;
        //    everything else is optional.
        let surface_ext = manager.get_surface(wl_surface, qh, ());
        surface_ext
            .set_image_description(&description, wp_color_manager_v1::RenderIntent::Perceptual);

        Ok(Self {
            manager,
            description,
            surface_ext,
            state,
        })
    }

    /// Snapshot the current description state. Useful for polling
    /// after a roundtrip.
    pub fn state(&self) -> DescriptionState {
        self.state.lock().unwrap().clone()
    }
}

impl Drop for ColorManagedSurface {
    fn drop(&mut self) {
        // Explicit destructors so the compositor sees ordered teardown:
        // surface extension first (which spec-mandates an unset), then
        // description, then manager. wayland-client's auto-destruct on
        // proxy drop would do this in proxy-drop order which isn't
        // guaranteed; explicit is safer.
        self.surface_ext.destroy();
        self.description.destroy();
        self.manager.destroy();
    }
}

// ─── Dispatch impls ────────────────────────────────────────────────────────

/// Accumulate the manager's `supported_*` enumeration into `caps`.
/// Called from the `WpColorManagerV1` dispatch impl in `lib.rs`.
pub fn handle_manager_event(
    event: <WpColorManagerV1 as wayland_client::Proxy>::Event,
    caps: &mut ColorCapabilities,
) {
    use wp_color_manager_v1::Event;
    match event {
        Event::SupportedIntent { render_intent } => {
            caps.render_intents.push(wenum_str(render_intent));
        }
        Event::SupportedFeature { feature } => caps.features.push(wenum_str(feature)),
        Event::SupportedTfNamed { tf } => caps.transfer_functions.push(wenum_str(tf)),
        Event::SupportedPrimariesNamed { primaries } => caps.primaries.push(wenum_str(primaries)),
        Event::Done => caps.done = true,
        _ => {}
    }
}

pub fn handle_description_event(
    event: <WpImageDescriptionV1 as wayland_client::Proxy>::Event,
    state: &Arc<Mutex<DescriptionState>>,
) {
    use wp_image_description_v1::Event;
    let mut st = state.lock().unwrap();
    match event {
        Event::Ready { identity } => {
            *st = DescriptionState::Ready {
                identity: identity as u64,
            };
        }
        Event::Ready2 {
            identity_hi,
            identity_lo,
        } => {
            *st = DescriptionState::Ready {
                identity: ((identity_hi as u64) << 32) | identity_lo as u64,
            };
        }
        Event::Failed { cause, msg } => {
            *st = DescriptionState::Failed {
                cause: wenum_str(cause),
                message: msg,
            };
        }
        _ => {}
    }
}
