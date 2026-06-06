//! Client-side `wp_color_management_v1` driver — attach an image
//! description to our patch surface so the compositor scans it out as
//! the declared color encoding, and record what the compositor
//! advertised + how it responded.
//!
//! Why we write this directly instead of using a higher-level
//! wrapper: SCTK 0.19 doesn't ship one, and the protocol surface we
//! actually exercise is small (one description, one attachment per
//! surface lifetime). The dispatch impls here accumulate the manager's
//! supported_* enumeration events into [`ColorCapabilities`] (a fact
//! the validator records) and track the description's ready/failed
//! outcome for the caller.
//!
//! The full parametric surface is supported: named and power-law
//! transfer functions, named and custom (CIE xy) primaries, luminances,
//! and ST 2086 mastering metadata — plus the `windows_scrgb`
//! description shortcut. Every optional protocol request is gated on
//! the compositor's advertised features/TFs/primaries/intents
//! *before* sending: using an unadvertised one is a fatal protocol
//! error (the compositor kills the connection), so [`attach`] returns
//! [`AttachError`] instead. ICC-file descriptions are not yet
//! supported.
//!
//! [`attach`]: ColorManagedSurface::attach

use std::sync::{Arc, Mutex};

use wayland_client::{Dispatch, QueueHandle, WEnum, protocol::wl_surface::WlSurface};
use wayland_protocols::wp::color_management::v1::client::{
    wp_color_management_surface_v1::WpColorManagementSurfaceV1,
    wp_color_manager_v1::{self, WpColorManagerV1},
    wp_image_description_creator_params_v1::WpImageDescriptionCreatorParamsV1,
    wp_image_description_v1::{self, WpImageDescriptionV1},
};

/// A color description to negotiate with the compositor, plus the
/// render intent to attach it with.
#[derive(Clone, Debug, PartialEq)]
pub struct DescriptionRequest {
    pub kind: DescriptionKind,
    /// Render intent for `set_image_description`, named by the
    /// protocol's `render_intent` enum entry (`"perceptual"`,
    /// `"relative"`, `"saturation"`, `"absolute"`, `"relative_bpc"`,
    /// `"absolute_no_adaptation"`). Compositors must support
    /// perceptual; every other intent must be advertised.
    pub render_intent: String,
}

impl DescriptionRequest {
    /// A parametric description attached with perceptual intent.
    pub fn parametric(p: ParametricDescription) -> Self {
        Self {
            kind: DescriptionKind::Parametric(p),
            render_intent: "perceptual".into(),
        }
    }

    /// The common case: named TF + named primaries, no luminance or
    /// mastering metadata, perceptual intent.
    pub fn named(transfer_function: &str, primaries: &str) -> Self {
        Self::parametric(ParametricDescription::named(transfer_function, primaries))
    }

    /// The compositor-defined Windows-scRGB description (linear fp16,
    /// sRGB primaries, scRGB signaling), attached with perceptual
    /// intent. Requires the `windows_scrgb` feature; pair it with a
    /// float buffer format.
    pub fn windows_scrgb() -> Self {
        Self {
            kind: DescriptionKind::WindowsScrgb,
            render_intent: "perceptual".into(),
        }
    }
}

/// Which kind of image description to create.
// A few of these exist per capture; the variant size spread is irrelevant
// and boxing would just make construction uglier for consumers.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum DescriptionKind {
    /// Built through `create_parametric_creator` from explicit
    /// colorimetric parameters.
    Parametric(ParametricDescription),
    /// The compositor's built-in Windows-scRGB description
    /// (`create_windows_scrgb`).
    WindowsScrgb,
}

/// A parametric image description: the colorimetric parameters handed
/// to `wp_image_description_creator_params_v1`. Values are given in
/// semantic units (cd/m², CIE xy, plain exponents); they are converted
/// to the protocol's tick units at attach time.
#[derive(Clone, Debug, PartialEq)]
pub struct ParametricDescription {
    pub transfer_function: TransferChoice,
    pub primaries: PrimariesChoice,
    /// Primary color volume luminances (`set_luminances`). Requires the
    /// `set_luminances` feature. Note: with `st2084_pq` the protocol
    /// ignores the given max and uses min + 10000 cd/m².
    pub luminances: Option<Luminances>,
    /// ST 2086 mastering metadata. Parts are individually optional and
    /// individually feature-gated; see [`Mastering`].
    pub mastering: Option<Mastering>,
}

impl ParametricDescription {
    /// Named TF + named primaries, nothing optional.
    pub fn named(transfer_function: &str, primaries: &str) -> Self {
        Self {
            transfer_function: TransferChoice::Named(transfer_function.into()),
            primaries: PrimariesChoice::Named(primaries.into()),
            luminances: None,
            mastering: None,
        }
    }
}

/// The transfer characteristic of a parametric description.
#[derive(Clone, Debug, PartialEq)]
pub enum TransferChoice {
    /// A named TF from the protocol's `transfer_function` enum (see
    /// [`tf_named`] for the mapped set). Must be advertised by the
    /// compositor.
    Named(String),
    /// A pure power curve (`set_tf_power`) with the given exponent,
    /// valid range `1.0..=10.0`. Requires the `set_tf_power` feature.
    Power(f64),
}

impl TransferChoice {
    /// A stable label for recording: the TF name, or `"power_<exp>"`.
    pub fn label(&self) -> String {
        match self {
            TransferChoice::Named(n) => n.clone(),
            TransferChoice::Power(e) => format!("power_{e}"),
        }
    }
}

/// The primaries of a parametric description.
#[derive(Clone, Debug, PartialEq)]
pub enum PrimariesChoice {
    /// Named primaries from the protocol's `primaries` enum (see
    /// [`primaries_named`] for the mapped set). Must be advertised by
    /// the compositor.
    Named(String),
    /// Explicit CIE 1931 xy chromaticities (`set_primaries`). Requires
    /// the `set_primaries` feature.
    Custom(PrimaryCoords),
}

impl PrimariesChoice {
    /// A stable label for recording: the primaries name, or `"custom"`.
    pub fn label(&self) -> &str {
        match self {
            PrimariesChoice::Named(n) => n,
            PrimariesChoice::Custom(_) => "custom",
        }
    }
}

/// RGB primaries + white point as CIE 1931 xy chromaticity coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PrimaryCoords {
    pub red: [f64; 2],
    pub green: [f64; 2],
    pub blue: [f64; 2],
    pub white: [f64; 2],
}

/// Reference luminances for the description, in cd/m².
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Luminances {
    pub min_nits: f64,
    pub max_nits: f64,
    pub reference_nits: f64,
}

/// ST 2086 / CTA-861 mastering ("target color volume") metadata. Each
/// part maps to its own protocol request and is individually optional:
///
/// - `luminance_nits` → `set_mastering_luminance`, gated on the
///   `set_mastering_display_primaries` feature
/// - `primaries` → `set_mastering_display_primaries`, same gate
/// - `max_cll_nits` / `max_fall_nits` → `set_max_cll` / `set_max_fall`,
///   ungated (plain CTA-861 metadata)
///
/// A target color volume exceeding the primary color volume
/// additionally wants the `extended_target_volume` feature; the
/// compositor may otherwise fail the description (we don't pre-judge
/// containment — the ready/failed outcome is the recorded fact).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Mastering {
    /// Mastering luminance range as `(min, max)` cd/m².
    pub luminance_nits: Option<(f64, f64)>,
    /// Mastering display primaries + white point.
    pub primaries: Option<PrimaryCoords>,
    /// Maximum content light level (CTA-861-H), cd/m².
    pub max_cll_nits: Option<f64>,
    /// Maximum frame-average light level (CTA-861-H), cd/m².
    pub max_fall_nits: Option<f64>,
}

/// Map a transfer-function name to the protocol enum, or `None` if this
/// build doesn't map it. Names are the protocol's own
/// `transfer_function` enum entries — the full set as of protocol v2.
/// (`srgb` / `ext_srgb` are deprecated since v2 in favor of
/// `compound_power_2_4`, but remain mapped: which of them a compositor
/// advertises is exactly the kind of fact a capture should record.)
pub fn tf_named(name: &str) -> Option<wp_color_manager_v1::TransferFunction> {
    use wp_color_manager_v1::TransferFunction as T;
    Some(match name {
        "bt1886" => T::Bt1886,
        "gamma22" => T::Gamma22,
        "gamma28" => T::Gamma28,
        "st240" => T::St240,
        "ext_linear" => T::ExtLinear,
        "log_100" => T::Log100,
        "log_316" => T::Log316,
        "xvycc" => T::Xvycc,
        "srgb" => T::Srgb,
        "ext_srgb" => T::ExtSrgb,
        "st2084_pq" => T::St2084Pq,
        "st428" => T::St428,
        "hlg" => T::Hlg,
        "compound_power_2_4" => T::CompoundPower24,
        _ => return None,
    })
}

/// Map a primaries name to the protocol enum, or `None` if this build
/// doesn't map it. Names are the protocol's own `primaries` enum
/// entries — the full set.
pub fn primaries_named(name: &str) -> Option<wp_color_manager_v1::Primaries> {
    use wp_color_manager_v1::Primaries as P;
    Some(match name {
        "srgb" => P::Srgb,
        "pal_m" => P::PalM,
        "pal" => P::Pal,
        "ntsc" => P::Ntsc,
        "generic_film" => P::GenericFilm,
        "bt2020" => P::Bt2020,
        "cie1931_xyz" => P::Cie1931Xyz,
        "dci_p3" => P::DciP3,
        "display_p3" => P::DisplayP3,
        "adobe_rgb" => P::AdobeRgb,
        _ => return None,
    })
}

/// Map an optional-feature name to the protocol enum, or `None` if this
/// build doesn't map it. Names are the protocol's own `feature` enum
/// entries — the full set.
pub fn feature_named(name: &str) -> Option<wp_color_manager_v1::Feature> {
    use wp_color_manager_v1::Feature as F;
    Some(match name {
        "icc_v2_v4" => F::IccV2V4,
        "parametric" => F::Parametric,
        "set_primaries" => F::SetPrimaries,
        "set_tf_power" => F::SetTfPower,
        "set_luminances" => F::SetLuminances,
        "set_mastering_display_primaries" => F::SetMasteringDisplayPrimaries,
        "extended_target_volume" => F::ExtendedTargetVolume,
        "windows_scrgb" => F::WindowsScrgb,
        _ => return None,
    })
}

/// Map a render-intent name to the protocol enum, or `None` if this
/// build doesn't map it. Names are the protocol's own `render_intent`
/// enum entries — the full set.
pub fn intent_named(name: &str) -> Option<wp_color_manager_v1::RenderIntent> {
    use wp_color_manager_v1::RenderIntent as R;
    Some(match name {
        "perceptual" => R::Perceptual,
        "relative" => R::Relative,
        "saturation" => R::Saturation,
        "absolute" => R::Absolute,
        "relative_bpc" => R::RelativeBpc,
        "absolute_no_adaptation" => R::AbsoluteNoAdaptation,
        _ => return None,
    })
}

/// Outcome of building the image description. Updated by the
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

impl ColorCapabilities {
    /// Whether the compositor advertised this optional-feature flag.
    pub fn has_feature(&self, f: wp_color_manager_v1::Feature) -> bool {
        self.features.iter().any(|s| *s == format!("{f:?}"))
    }

    /// Whether the compositor advertised this named transfer function.
    pub fn has_tf(&self, tf: wp_color_manager_v1::TransferFunction) -> bool {
        self.transfer_functions
            .iter()
            .any(|s| *s == format!("{tf:?}"))
    }

    /// Whether the compositor advertised these named primaries.
    pub fn has_primaries(&self, p: wp_color_manager_v1::Primaries) -> bool {
        self.primaries.iter().any(|s| *s == format!("{p:?}"))
    }

    /// Whether the compositor advertised this render intent.
    pub fn has_intent(&self, i: wp_color_manager_v1::RenderIntent) -> bool {
        self.render_intents.iter().any(|s| *s == format!("{i:?}"))
    }

    fn require_feature(
        &self,
        f: wp_color_manager_v1::Feature,
        name: &'static str,
    ) -> Result<(), AttachError> {
        if self.has_feature(f) {
            Ok(())
        } else {
            Err(AttachError::FeatureNotAdvertised(name))
        }
    }
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

/// nits → the protocol's unscaled cd/m² argument.
fn nits_to_lum(nits: f64) -> u32 {
    nits.round().max(0.0) as u32
}

/// CIE xy coordinate → the protocol's ×1,000,000 argument.
fn coord_to_protocol(v: f64) -> i32 {
    (v * 1_000_000.0).round() as i32
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

/// Reason a description couldn't even be requested (before the
/// compositor weighs in via ready/failed). Mostly: the request needs a
/// protocol value or feature the compositor didn't advertise — sending
/// it anyway would be a fatal protocol error, so we refuse client-side.
#[derive(Debug, Clone, PartialEq)]
pub enum AttachError {
    /// TF name this build's tables don't map.
    UnknownTransferFunction(String),
    /// Primaries name this build's tables don't map.
    UnknownPrimaries(String),
    /// Render-intent name this build's tables don't map.
    UnknownRenderIntent(String),
    /// The named TF isn't in the compositor's advertised set.
    TfNotAdvertised(String),
    /// The named primaries aren't in the compositor's advertised set.
    PrimariesNotAdvertised(String),
    /// The render intent isn't in the compositor's advertised set
    /// (perceptual is always allowed per spec).
    IntentNotAdvertised(String),
    /// The request needs an optional protocol feature the compositor
    /// didn't advertise (the protocol's `feature` enum entry name).
    FeatureNotAdvertised(&'static str),
    /// `set_tf_power` exponent outside the protocol's `1.0..=10.0`.
    PowerExponentOutOfRange(f64),
}

impl std::fmt::Display for AttachError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AttachError::UnknownTransferFunction(n) => {
                write!(f, "transfer function name not mapped by this build: {n:?}")
            }
            AttachError::UnknownPrimaries(n) => {
                write!(f, "primaries name not mapped by this build: {n:?}")
            }
            AttachError::UnknownRenderIntent(n) => {
                write!(f, "render intent name not mapped by this build: {n:?}")
            }
            AttachError::TfNotAdvertised(n) => {
                write!(f, "compositor doesn't advertise transfer function {n:?}")
            }
            AttachError::PrimariesNotAdvertised(n) => {
                write!(f, "compositor doesn't advertise primaries {n:?}")
            }
            AttachError::IntentNotAdvertised(n) => {
                write!(f, "compositor doesn't advertise render intent {n:?}")
            }
            AttachError::FeatureNotAdvertised(n) => {
                write!(f, "compositor doesn't advertise the {n:?} feature")
            }
            AttachError::PowerExponentOutOfRange(e) => {
                write!(
                    f,
                    "power-TF exponent {e} outside the protocol range 1.0..=10.0"
                )
            }
        }
    }
}

impl AttachError {
    /// A short reason suitable for a chip next to a disabled mode toggle.
    pub fn reason(&self) -> String {
        match self {
            AttachError::UnknownTransferFunction(n) | AttachError::TfNotAdvertised(n) => {
                format!("TF {n} unsupported")
            }
            AttachError::UnknownPrimaries(n) | AttachError::PrimariesNotAdvertised(n) => {
                format!("primaries {n} unsupported")
            }
            AttachError::UnknownRenderIntent(n) | AttachError::IntentNotAdvertised(n) => {
                format!("intent {n} unsupported")
            }
            AttachError::FeatureNotAdvertised(f) => format!("no {f} feature"),
            AttachError::PowerExponentOutOfRange(_) => "power exponent out of range".to_string(),
        }
    }
}

impl std::error::Error for AttachError {}

/// Check `req` against the compositor's advertised capabilities without
/// sending anything — exactly the gating [`ColorManagedSurface::attach`]
/// applies before its first request. Pre-flight callers
/// ([`DisplayCapabilities::plan`](crate::DisplayCapabilities::plan)) and
/// the attach path share this so the two can't drift.
pub fn validate_description(
    req: &DescriptionRequest,
    caps: &ColorCapabilities,
) -> Result<(), AttachError> {
    use wp_color_manager_v1::{Feature, RenderIntent};

    let intent = intent_named(&req.render_intent)
        .ok_or_else(|| AttachError::UnknownRenderIntent(req.render_intent.clone()))?;
    // Perceptual is the protocol's baseline; anything else must be
    // advertised or set_image_description raises a protocol error.
    if intent != RenderIntent::Perceptual && !caps.has_intent(intent) {
        return Err(AttachError::IntentNotAdvertised(req.render_intent.clone()));
    }

    match &req.kind {
        DescriptionKind::Parametric(p) => {
            caps.require_feature(Feature::Parametric, "parametric")?;
            match &p.transfer_function {
                TransferChoice::Named(name) => {
                    let tf = tf_named(name)
                        .ok_or_else(|| AttachError::UnknownTransferFunction(name.clone()))?;
                    if !caps.has_tf(tf) {
                        return Err(AttachError::TfNotAdvertised(name.clone()));
                    }
                }
                TransferChoice::Power(exp) => {
                    if !(1.0..=10.0).contains(exp) {
                        return Err(AttachError::PowerExponentOutOfRange(*exp));
                    }
                    caps.require_feature(Feature::SetTfPower, "set_tf_power")?;
                }
            }
            match &p.primaries {
                PrimariesChoice::Named(name) => {
                    let pr = primaries_named(name)
                        .ok_or_else(|| AttachError::UnknownPrimaries(name.clone()))?;
                    if !caps.has_primaries(pr) {
                        return Err(AttachError::PrimariesNotAdvertised(name.clone()));
                    }
                }
                PrimariesChoice::Custom(_) => {
                    caps.require_feature(Feature::SetPrimaries, "set_primaries")?;
                }
            }
            if p.luminances.is_some() {
                caps.require_feature(Feature::SetLuminances, "set_luminances")?;
            }
            if let Some(m) = &p.mastering {
                if m.luminance_nits.is_some() || m.primaries.is_some() {
                    caps.require_feature(
                        Feature::SetMasteringDisplayPrimaries,
                        "set_mastering_display_primaries",
                    )?;
                }
                // max_cll / max_fall are plain CTA-861 metadata, ungated.
            }
            Ok(())
        }
        DescriptionKind::WindowsScrgb => {
            caps.require_feature(Feature::WindowsScrgb, "windows_scrgb")
        }
    }
}

impl ColorManagedSurface {
    /// Build an image description from `req` + attach it to
    /// `wl_surface`, refusing client-side (instead of dying to a
    /// protocol error) anything `caps` says the compositor doesn't
    /// support. Caller must roundtrip the event queue afterwards to
    /// flush the create + set requests and pick up the
    /// `ready`/`failed` event.
    pub fn attach<D>(
        manager: WpColorManagerV1,
        qh: &QueueHandle<D>,
        wl_surface: &WlSurface,
        req: &DescriptionRequest,
        caps: &ColorCapabilities,
    ) -> Result<Self, AttachError>
    where
        D: Dispatch<WpImageDescriptionCreatorParamsV1, ()> + 'static,
        D: Dispatch<WpImageDescriptionV1, Arc<Mutex<DescriptionState>>> + 'static,
        D: Dispatch<WpColorManagementSurfaceV1, ()> + 'static,
    {
        // Validate everything before sending anything: a mid-build error
        // would otherwise leave a half-configured creator queued.
        validate_description(req, caps)?;
        let intent = intent_named(&req.render_intent).expect("validated above");

        // The state Mutex is the side-channel the dispatch impl updates
        // from ready/failed.
        let state = Arc::new(Mutex::new(DescriptionState::Pending));

        let description = match &req.kind {
            DescriptionKind::Parametric(p) => {
                let creator = manager.create_parametric_creator(qh, ());

                match &p.transfer_function {
                    TransferChoice::Named(name) => {
                        creator.set_tf_named(tf_named(name).expect("validated above"));
                    }
                    TransferChoice::Power(exp) => {
                        creator.set_tf_power((exp * 10_000.0).round() as u32);
                    }
                }

                match &p.primaries {
                    PrimariesChoice::Named(name) => {
                        creator
                            .set_primaries_named(primaries_named(name).expect("validated above"));
                    }
                    PrimariesChoice::Custom(c) => {
                        creator.set_primaries(
                            coord_to_protocol(c.red[0]),
                            coord_to_protocol(c.red[1]),
                            coord_to_protocol(c.green[0]),
                            coord_to_protocol(c.green[1]),
                            coord_to_protocol(c.blue[0]),
                            coord_to_protocol(c.blue[1]),
                            coord_to_protocol(c.white[0]),
                            coord_to_protocol(c.white[1]),
                        );
                    }
                }

                if let Some(l) = p.luminances {
                    creator.set_luminances(
                        nits_to_min_ticks(l.min_nits),
                        nits_to_lum(l.max_nits),
                        nits_to_lum(l.reference_nits),
                    );
                }

                if let Some(m) = &p.mastering {
                    if let Some((min, max)) = m.luminance_nits {
                        creator.set_mastering_luminance(nits_to_min_ticks(min), nits_to_lum(max));
                    }
                    if let Some(c) = m.primaries {
                        creator.set_mastering_display_primaries(
                            coord_to_protocol(c.red[0]),
                            coord_to_protocol(c.red[1]),
                            coord_to_protocol(c.green[0]),
                            coord_to_protocol(c.green[1]),
                            coord_to_protocol(c.blue[0]),
                            coord_to_protocol(c.blue[1]),
                            coord_to_protocol(c.white[0]),
                            coord_to_protocol(c.white[1]),
                        );
                    }
                    if let Some(cll) = m.max_cll_nits {
                        creator.set_max_cll(nits_to_lum(cll));
                    }
                    if let Some(fall) = m.max_fall_nits {
                        creator.set_max_fall(nits_to_lum(fall));
                    }
                }

                creator.create(qh, state.clone())
            }
            DescriptionKind::WindowsScrgb => manager.create_windows_scrgb(qh, state.clone()),
        };

        // Get the surface extension + set the description on it with
        // the chosen (validated) intent.
        let surface_ext = manager.get_surface(wl_surface, qh, ());
        surface_ext.set_image_description(&description, intent);

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

#[cfg(test)]
mod tests {
    use super::*;
    use wp_color_manager_v1::{Primaries, RenderIntent, TransferFunction};

    #[test]
    fn tf_table_maps_the_full_protocol_enum() {
        // Every protocol TF entry as of v2.
        let all = [
            ("bt1886", TransferFunction::Bt1886),
            ("gamma22", TransferFunction::Gamma22),
            ("gamma28", TransferFunction::Gamma28),
            ("st240", TransferFunction::St240),
            ("ext_linear", TransferFunction::ExtLinear),
            ("log_100", TransferFunction::Log100),
            ("log_316", TransferFunction::Log316),
            ("xvycc", TransferFunction::Xvycc),
            ("srgb", TransferFunction::Srgb),
            ("ext_srgb", TransferFunction::ExtSrgb),
            ("st2084_pq", TransferFunction::St2084Pq),
            ("st428", TransferFunction::St428),
            ("hlg", TransferFunction::Hlg),
            ("compound_power_2_4", TransferFunction::CompoundPower24),
        ];
        for (name, tf) in all {
            assert_eq!(tf_named(name), Some(tf), "{name}");
        }
        assert_eq!(tf_named("nonsense"), None);
    }

    #[test]
    fn primaries_table_maps_the_full_protocol_enum() {
        let all = [
            ("srgb", Primaries::Srgb),
            ("pal_m", Primaries::PalM),
            ("pal", Primaries::Pal),
            ("ntsc", Primaries::Ntsc),
            ("generic_film", Primaries::GenericFilm),
            ("bt2020", Primaries::Bt2020),
            ("cie1931_xyz", Primaries::Cie1931Xyz),
            ("dci_p3", Primaries::DciP3),
            ("display_p3", Primaries::DisplayP3),
            ("adobe_rgb", Primaries::AdobeRgb),
        ];
        for (name, p) in all {
            assert_eq!(primaries_named(name), Some(p), "{name}");
        }
        assert_eq!(primaries_named("nonsense"), None);
    }

    #[test]
    fn intent_table_maps_the_full_protocol_enum() {
        let all = [
            ("perceptual", RenderIntent::Perceptual),
            ("relative", RenderIntent::Relative),
            ("saturation", RenderIntent::Saturation),
            ("absolute", RenderIntent::Absolute),
            ("relative_bpc", RenderIntent::RelativeBpc),
            ("absolute_no_adaptation", RenderIntent::AbsoluteNoAdaptation),
        ];
        for (name, i) in all {
            assert_eq!(intent_named(name), Some(i), "{name}");
        }
        assert_eq!(intent_named("nonsense"), None);
    }

    #[test]
    fn unit_conversions() {
        assert_eq!(nits_to_min_ticks(0.0005), 5);
        assert_eq!(nits_to_lum(400.2), 400);
        assert_eq!(coord_to_protocol(0.3127), 312_700);
        assert_eq!(coord_to_protocol(0.708), 708_000);
    }
}
