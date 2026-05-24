//! Client-side `wp_color_management_v1` driver — just enough to
//! attach a parametric PQ + BT.2020 image description to our patch
//! surface so the compositor scans it out without color transforms.
//!
//! Why we write this directly instead of using a higher-level
//! wrapper: SCTK 0.19 doesn't ship one, and the protocol surface we
//! actually exercise is small (one description, one attachment per
//! surface lifetime). The dispatch impls here handle the manager's
//! supported_* enumeration events (we ignore them — we already
//! decided what description to build) and the description's
//! ready/failed event (we record the outcome for the caller).

use std::sync::{Arc, Mutex};

use wayland_client::{Dispatch, QueueHandle, WEnum, protocol::wl_surface::WlSurface};
use wayland_protocols::wp::color_management::v1::client::{
    wp_color_management_surface_v1::WpColorManagementSurfaceV1,
    wp_color_manager_v1::{self, WpColorManagerV1},
    wp_image_description_creator_params_v1::WpImageDescriptionCreatorParamsV1,
    wp_image_description_v1::{self, WpImageDescriptionV1},
};

/// Parameters for the PQ + BT.2020 description we attach to an HDR
/// patch surface. Values match what the OLED expects (and what the
/// compositor advertises as its preferred description for that
/// output) — feeding mastering metadata that disagrees with the
/// compositor's expectations would still validate, but produces a
/// description the compositor's renderer might tone-map.
#[derive(Clone, Copy, Debug)]
pub struct PqDescriptionParams {
    /// Mastering display minimum luminance, in cd/m² × 10000 (the
    /// protocol's `min_lum` argument unit).
    pub mastering_min_lum_ticks: u32,
    /// Mastering display peak luminance, in cd/m².
    pub mastering_max_lum: u32,
    /// Max content light level, in cd/m².
    pub max_cll: u32,
    /// Max frame-average light level, in cd/m².
    pub max_fall: u32,
}

impl PqDescriptionParams {
    /// Reasonable default for measurement: 400-nit peak (matches the
    /// ASUS PG27UCDM OLED's certified HDR400 True Black), min ~0.0005
    /// nits (OLED black). Caller may override per panel.
    pub fn pg27ucdm_default() -> Self {
        Self {
            mastering_min_lum_ticks: 5, // 0.0005 cd/m²
            mastering_max_lum: 400,
            max_cll: 400,
            max_fall: 200,
        }
    }
}

/// Outcome of building the parametric description. Updated by the
/// `wp_image_description_v1.ready` / `.failed` event. Caller polls
/// (or roundtrips) until `Pending → Ready | Failed`.
#[derive(Clone, Debug)]
pub enum DescriptionState {
    Pending,
    Ready { identity_low: u32 },
    Failed { reason: String },
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

impl ColorManagedSurface {
    /// Build a parametric description + attach it to `wl_surface`.
    /// Caller must roundtrip the event queue afterwards to flush the
    /// create + set requests and pick up the `ready`/`failed` event.
    pub fn attach<D>(
        manager: WpColorManagerV1,
        qh: &QueueHandle<D>,
        wl_surface: &WlSurface,
        params: PqDescriptionParams,
    ) -> Self
    where
        D: Dispatch<WpImageDescriptionCreatorParamsV1, ()> + 'static,
        D: Dispatch<WpImageDescriptionV1, Arc<Mutex<DescriptionState>>> + 'static,
        D: Dispatch<WpColorManagementSurfaceV1, ()> + 'static,
    {
        // 1) Build a parametric creator.
        let creator = manager.create_parametric_creator(qh, ());

        // 2) Required fields: TF + primaries. (luminances /
        //    mastering / max_cll / max_fall are optional but we set
        //    them all because the compositor's preferred description
        //    for the HDR output includes them — matching identities
        //    cleanly is the easier debug path.)
        creator.set_tf_named(wp_color_manager_v1::TransferFunction::St2084Pq);
        creator.set_primaries_named(wp_color_manager_v1::Primaries::Bt2020);

        // luminances: PQ defaults per spec are min 0.005, max ignored
        // (always min + 10000), reference 203. We pass them explicitly.
        creator.set_luminances(
            50,     // 0.005 cd/m² × 10000
            10_000, // ignored for st2084_pq
            203,    // reference white
        );

        // mastering display — primaries default to the primary color
        // volume (BT.2020), so we only need to set luminances + cll
        // + fall.
        creator.set_mastering_luminance(params.mastering_min_lum_ticks, params.mastering_max_lum);
        creator.set_max_cll(params.max_cll);
        creator.set_max_fall(params.max_fall);

        // 3) Materialize the description. The state Mutex is the
        //    side-channel the dispatch impl updates from ready/failed.
        let state = Arc::new(Mutex::new(DescriptionState::Pending));
        let description = creator.create(qh, state.clone());

        // 4) Get the surface extension + set the description on it
        //    with perceptual intent. Compositors must support
        //    perceptual; everything else is optional.
        let surface_ext = manager.get_surface(wl_surface, qh, ());
        surface_ext
            .set_image_description(&description, wp_color_manager_v1::RenderIntent::Perceptual);

        Self {
            manager,
            description,
            surface_ext,
            state,
        }
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

/// Marker trait the application's state type must satisfy. Caller
/// implements `Dispatch` for each of the four interfaces by
/// delegating to the helper impls below — see `lib.rs` for the
/// concrete forwarding pattern.
pub fn handle_manager_event(event: <WpColorManagerV1 as wayland_client::Proxy>::Event) {
    use wp_color_manager_v1::Event;
    // We don't care about the supported_* events — we already know
    // what description we want and the compositor will tell us via
    // ready/failed if it can't satisfy it. Logged at trace for
    // debugging but otherwise ignored.
    match event {
        Event::SupportedIntent { .. }
        | Event::SupportedFeature { .. }
        | Event::SupportedTfNamed { .. }
        | Event::SupportedPrimariesNamed { .. }
        | Event::Done => {}
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
                identity_low: identity,
            };
        }
        Event::Ready2 {
            identity_hi,
            identity_lo,
        } => {
            *st = DescriptionState::Ready {
                identity_low: identity_lo,
            };
            let _ = identity_hi;
        }
        Event::Failed { cause, msg } => {
            let cause_str = match cause {
                WEnum::Value(c) => format!("{c:?}"),
                WEnum::Unknown(v) => format!("unknown({v})"),
            };
            *st = DescriptionState::Failed {
                reason: format!("{cause_str}: {msg}"),
            };
        }
        _ => {}
    }
}
