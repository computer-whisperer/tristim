//! Capture schema — the on-disk contract between the tristim **gatherer**
//! (which records facts) and the **analysis / presentation** tools (which
//! interpret them).
//!
//! A [`Capture`] is the complete record of one validation session against a
//! single output: what the compositor advertised, what color descriptions we
//! negotiated (and whether it accepted them), and the input→output samples we
//! measured.
//!
//! ## Design rule: the gatherer records facts, not interpretation
//!
//! [`Sample::requested`] is **purely the per-channel code values written to
//! the buffer**, in whatever encoding the trial's format uses. The schema
//! deliberately holds no notion of "expected" or "correct" output. Computing
//! the target colorimetry from a negotiated [`ColorDescription`] — and scoring
//! how far the measurement landed from it — is the analysis tool's job. This
//! keeps a capture interpretable no matter how the scoring logic later evolves.
//!
//! A capture is serialized as pretty JSON via [`Capture::save`] /
//! [`Capture::load`].

use serde::{Deserialize, Serialize};
use std::path::Path;
use thiserror::Error;

/// Current schema version. Bump on any breaking change to the types below;
/// readers should reject captures whose `schema_version` they don't understand.
///
/// v2 added the optional [`Capture::compositor`] section. v3 added the optional
/// per-trial [`FormatTrial::gamut`] section. v4 added [`Sample::source`] /
/// [`Sample::repeats`] (gamut-probe vertices folded in as samples). v5 made
/// [`Measured::raw`] variable-length (omitted when the device exposes no raw
/// counts, instead of fabricating zeros) and added [`DeviceInfo::calibration`],
/// [`ColorDescription::render_intent`], [`Sample::adaptive_tier`] /
/// [`Sample::elapsed_ms`], and the [`Capture::run`] block. All are
/// `#[serde(default)]`, so older captures still load (with the section/field
/// absent or at its default) and `load` does no version gate.
pub const SCHEMA_VERSION: u32 = 5;

#[derive(Debug, Error)]
pub enum Error {
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// A complete validation session against one output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capture {
    /// Schema version this capture was written against (see [`SCHEMA_VERSION`]).
    pub schema_version: u32,
    /// When the session ran, as an RFC 3339 timestamp.
    pub timestamp: String,
    /// What wrote this capture.
    pub tool: ToolInfo,
    /// The colorimeter that took the measurements.
    pub device: DeviceInfo,
    /// The output under test.
    pub output: OutputInfo,
    /// What the compositor advertised it can do (color-management-wise).
    pub capabilities: Capabilities,
    /// What we could learn about the compositor that served the session
    /// (best-effort; added in schema v2). Empty for v1 captures.
    #[serde(default)]
    pub compositor: CompositorInfo,
    /// How the run was conducted (added in schema v5): the run-level
    /// configuration facts not already recorded per sample — enough to
    /// reproduce the run exactly. `None` in pre-v5 captures.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunInfo>,
    /// One entry per color format / description we put under test.
    pub trials: Vec<FormatTrial>,
}

/// Identifies the program (and build) that produced a capture.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolInfo {
    pub name: String,
    pub version: String,
    /// Git revision of the build, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_revision: Option<String>,
}

/// The colorimeter used, plus which on-device calibration was applied.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// Human label, e.g. `"Spyder 2024"`.
    pub product: String,
    pub usb_pid: u16,
    pub serial: String,
    /// Hardware version `(major, minor)`.
    pub hw_version: (u32, u32),
    /// Calibration index downloaded from the device and used for raw→XYZ.
    pub cal_index: u8,
    /// The conversion behind `cal_index` (added in schema v5): with
    /// [`Measured::raw`], lets every XYZ value be recomputed and audited
    /// offline. `None` in pre-v5 captures or when the device doesn't expose
    /// its conversion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calibration: Option<CalibrationInfo>,
}

/// How the device mapped raw channel readings to CIE XYZ during this capture:
/// `xyz = matrix · max(raw − black_floor, 0)`, then `xyz[i] = xyz[i] ·
/// gain[i] + offset[i]`. The channel count `N` (= `black_floor.len()` = each
/// matrix row's length) and the channel units are device-specific: 6 sensor
/// counts on the Spyder X2/2024, 3 on the original SpyderX (IR excluded), 3
/// internal frequencies in Hz on the i1d3 family (which exposes no raw
/// counts — there the block documents the conversion without making samples
/// recomputable).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalibrationInfo {
    /// Per-channel floor subtracted from raw readings before the matrix.
    pub black_floor: Vec<f64>,
    /// 3×N matrix taking floor-subtracted channels to (pre-gain) XYZ.
    pub matrix: [Vec<f64>; 3],
    /// Per-row gain applied after the matrix (`[1, 1, 1]` when none).
    pub gain: [f64; 3],
    /// Per-row offset added last (`[0, 0, 0]` when none).
    pub offset: [f64; 3],
}

/// The output (display) the patches were shown on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputInfo {
    /// Connector name, e.g. `"DP-4"`.
    pub name: String,
    pub make: String,
    pub model: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<OutputMode>,
}

/// The output's current video mode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputMode {
    pub width: i32,
    pub height: i32,
    /// Refresh rate in millihertz, as reported by `wl_output`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_mhz: Option<i32>,
}

/// What the compositor advertised through `wp_color_manager_v1`'s
/// `supported_*` events. Stored as the protocol's enum names (strings) so the
/// schema stays decoupled from any particular `wayland-protocols` version. An
/// empty set means the compositor exposes no color management at all.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Capabilities {
    pub supported_transfer_functions: Vec<String>,
    pub supported_primaries: Vec<String>,
    pub supported_features: Vec<String>,
    pub supported_render_intents: Vec<String>,
}

/// What we could learn about the Wayland compositor that served the session.
///
/// Every field is best-effort. There is **no** Wayland protocol that names the
/// compositor, so these come from three independent signals: the socket peer
/// process, the session environment, and the advertised global interfaces.
/// Together they pin down "which compositor, which protocols, which versions"
/// a capture was taken against.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct CompositorInfo {
    /// Compositor binary name from the Wayland socket's peer credentials
    /// (`SO_PEERCRED` → `/proc/<pid>/comm`), e.g. `"niri"`, `"kwin_wayland"`.
    /// The most authoritative signal. `None` when the peer isn't a local
    /// process (e.g. behind a proxy like waypipe) or the lookup failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<String>,
    /// `XDG_CURRENT_DESKTOP` session hint (e.g. `"niri"`, `"GNOME"`). Set by
    /// the session rather than the compositor, so it can be absent, generic,
    /// or stale — a friendly cross-check, not a source of truth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desktop: Option<String>,
    /// Wayland globals the compositor advertised (`interface` + `version`).
    /// The protocol-level fingerprint — what the compositor actually
    /// implements, independent of branding.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub globals: Vec<GlobalInfo>,
}

/// One advertised Wayland global: its interface name and version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GlobalInfo {
    pub interface: String,
    pub version: u32,
}

/// Run-level configuration facts (added in schema v5). Together with the
/// per-sample [`SampleContext`] (settle / window / border) and the trials'
/// formats, this is the full recipe to reproduce the run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunInfo {
    /// Puck-placement countdown before the first measurement, in ms.
    pub prep_ms: u64,
    /// Adaptive fast-tier integration time (ms) applied to every measurement
    /// of the run, or `None` when everything was measured at the calibration
    /// default. Which tier actually produced each sample is recorded on
    /// [`Sample::adaptive_tier`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast_integration_ms: Option<u16>,
    /// Scatter-sample generation parameters, when scatter was requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scatter: Option<ScatterInfo>,
    /// Gamut-probe parameters, when each format's gamut was probed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gamut_probe: Option<GamutProbeInfo>,
}

/// How a run's scatter samples were drawn: `count` uniform points from the
/// deterministic stream seeded with `seed` (constrained to the measured gamut
/// when one was probed).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScatterInfo {
    pub count: u32,
    pub seed: u64,
}

/// The measurement depth and refinement thresholds of a run's gamut probes
/// (the probes' results live on each trial's [`FormatTrial::gamut`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GamutProbeInfo {
    /// Repeated measurements per probe point.
    pub repeats: u32,
    /// Maximum subdivision depth per cube face.
    pub max_depth: u32,
    /// Stop subdividing once the patch center is within this ΔE of the
    /// bilinear average of its corners.
    pub flat_eps: f64,
    /// Corner spread (ΔE) below which a patch counts as collapsed/clamped…
    pub fold_eps: f64,
    /// …provided it still spans at least this much code-value side length.
    pub fold_min_side: f64,
}

/// One color format put under test: the description we asked for, whether the
/// compositor accepted it, and the samples taken under it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FormatTrial {
    /// The colorimetry we requested ("settings Y"). The analysis tool derives
    /// the expected output from this. `None` for an unmanaged trial (a plain
    /// buffer with no negotiated description) — its samples carry no verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested: Option<ColorDescription>,
    /// `wl_shm` pixel format the buffer used, e.g. `"xrgb8888"` or
    /// `"xbgr16161616f"`.
    pub pixel_format: String,
    /// Whether the compositor accepted the description. A rejection is itself
    /// a result worth recording.
    pub outcome: Negotiation,
    /// The display's reproduced gamut for this encoding, if a gamut probe was
    /// run as a prerequisite to the sweep (added in schema v3). `None` when not
    /// probed. When present, this trial's samples were constrained to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gamut: Option<MeasuredGamut>,
    /// Measurements taken while this trial's format was active.
    pub samples: Vec<Sample>,
}

/// The adaptively-probed reproduced gamut for one encoding: the measured image
/// of the code-cube surface (8 corners + 6 face centers, recursively refined),
/// in the measured-white Lab frame. Recorded as a prerequisite to a capture so
/// the sweep can stay inside what the display can actually reproduce.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeasuredGamut {
    /// Measured white (code value 1,1,1) XYZ — the Lab reference for vertices.
    pub white_xyz: [f64; 3],
    /// Probed boundary vertices (deduped; patches index into this).
    pub vertices: Vec<GamutVertex>,
    /// Quadtree leaf patches over the 6 cube faces.
    pub patches: Vec<GamutPatch>,
}

/// One measured boundary vertex: the requested code value and what came back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GamutVertex {
    /// Requested code value in the trial's encoding.
    pub code_value: [f64; 3],
    /// Measured CIE XYZ (`Y` in cd/m²).
    pub xyz: [f64; 3],
    /// CIELAB relative to [`MeasuredGamut::white_xyz`].
    pub lab: [f64; 3],
    /// Whether the reading passed the confidence gate (false near black / on
    /// saturated corners read near the sensor floor).
    pub trustworthy: bool,
}

/// One refined leaf patch of a cube face: its 4 corner vertices and why
/// subdivision stopped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GamutPatch {
    /// Face the patch lies on, e.g. `"R=1"`.
    pub face: String,
    /// Indices into [`MeasuredGamut::vertices`] — the quad's corners, CCW.
    pub corners: [usize; 4],
    /// Why subdivision stopped: `"flat"`, `"folded"` (clamped), `"max_depth"`,
    /// or `"low_trust"`.
    pub status: String,
}

/// A parametric color description, in semantic units (nits, not protocol
/// ticks). Mirrors the colorimetry expressible through
/// `wp_color_management_v1`'s parametric creator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColorDescription {
    /// Transfer function name, e.g. `"srgb"`, `"st2084_pq"`, `"gamma22"`.
    pub transfer_function: String,
    /// Render intent the description was attached with, by the protocol's
    /// `render_intent` enum name (added in schema v5; omitted — and assumed —
    /// when `"perceptual"`, the protocol's mandatory baseline).
    #[serde(default = "perceptual", skip_serializing_if = "is_perceptual")]
    pub render_intent: String,
    /// Primaries name, e.g. `"srgb"`, `"bt2020"`, `"dci_p3"`.
    pub primaries: String,
    /// Reference white luminance in cd/m², if set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_white_nits: Option<f64>,
    /// Mastering-display metadata, if set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mastering: Option<Mastering>,
}

/// Mastering-display luminance metadata, all in cd/m².
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mastering {
    pub min_luminance_nits: f64,
    pub max_luminance_nits: f64,
    pub max_cll_nits: f64,
    pub max_fall_nits: f64,
}

/// Outcome of negotiating a [`ColorDescription`] with the compositor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Negotiation {
    /// The compositor produced a ready image description. `identity` is the
    /// protocol identity it assigned.
    Accepted { identity: u64 },
    /// The compositor rejected the description (e.g. unsupported TF/primaries).
    Rejected { cause: String, message: String },
    /// No color management was negotiated — either the compositor exposes none
    /// or we committed a plain buffer. Samples under this trial carry no
    /// correctness verdict.
    Unmanaged,
}

/// How a [`Sample`] was obtained. Sweep samples are single-shot readings of
/// the deterministic sequence + scatter; gamut-probe samples are the
/// repeat-averaged code-cube boundary vertices the gamut probe measured, folded
/// in so the probe's measurements count toward the results rather than being
/// discarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SampleSource {
    /// A point from the sequence + scatter sweep (single reading).
    #[default]
    Sweep,
    /// A gamut-probe boundary vertex (repeat-averaged; see [`Sample::repeats`]).
    GamutProbe,
}

impl SampleSource {
    /// Whether this is the default ([`SampleSource::Sweep`]); used to omit the
    /// field for sweep samples on serialization.
    pub fn is_sweep(&self) -> bool {
        matches!(self, SampleSource::Sweep)
    }
}

fn one() -> u32 {
    1
}

fn is_one(n: &u32) -> bool {
    *n == 1
}

fn perceptual() -> String {
    "perceptual".to_string()
}

fn is_perceptual(s: &str) -> bool {
    s == "perceptual"
}

/// One measured patch: what we sent, what came back, under what conditions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    /// The per-channel code values written to the buffer, normalized to
    /// `0..=1` in the trial format's own encoding. This is *exactly what we
    /// handed the compositor* — all interpretation is left to the analysis
    /// tool.
    pub requested: [f64; 3],
    /// What the colorimeter measured.
    pub measured: Measured,
    /// Conditions the measurement was taken under.
    pub context: SampleContext,
    /// How the sample was obtained. Omitted (defaults to [`SampleSource::Sweep`])
    /// for ordinary sweep samples.
    #[serde(default, skip_serializing_if = "SampleSource::is_sweep")]
    pub source: SampleSource,
    /// Number of repeated readings averaged into [`Sample::measured`]. `1` for
    /// single-shot sweep samples; the probe's repeat count for gamut-probe
    /// samples (whose `measured.raw` is the rounded per-channel mean).
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub repeats: u32,
    /// Which adaptive tier produced this sample, recorded only when the run
    /// used adaptive integration ([`RunInfo::fast_integration_ms`]). Added in
    /// schema v5; `None` in older captures or non-adaptive runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_tier: Option<AdaptiveTier>,
    /// Milliseconds elapsed since the run's measurement phase began — the
    /// prep countdown excluded (added in schema v5; `None` in older
    /// captures). Lets warm-up / ABL drift over a long run be correlated
    /// with measurement order and wall time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
}

/// Which tier of the driver's adaptive measurement produced a sample (see
/// [`RunInfo::fast_integration_ms`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdaptiveTier {
    /// The fast short-integration reading passed the trust check and is what
    /// the sample records.
    Fast,
    /// The fast reading was untrustworthy; the sample records the
    /// default-integration re-measurement that followed.
    EscalatedFull,
    /// The fast tier was unavailable (device has no integration override);
    /// a single default-integration reading.
    SingleFull,
}

/// A colorimeter reading.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Measured {
    /// Raw sensor channel counts before raw→XYZ conversion, when the device
    /// exposes them. Channel count is device-specific (6 on the Spyder
    /// X2/2024, 3 on the original SpyderX); empty for XYZ-only devices like
    /// the i1d3 family. Until schema v5 this was a fixed 6-wide array with
    /// zeros standing in for missing channels — pre-v5 captures from
    /// raw-less devices read as 6 zeros, not as absent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub raw: Vec<u16>,
    /// CIE 1931 XYZ. `Y` is luminance in cd/m².
    pub xyz: [f64; 3],
    /// CIE 1931 xy chromaticity. `None` for pure black (undefined).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xy: Option<[f64; 2]>,
}

/// The conditions a [`Sample`] was measured under.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SampleContext {
    /// Centered-window area fraction: `1.0` = fullscreen patch.
    pub window_fraction: f64,
    /// Surround code values when `window_fraction < 1.0`. `None` = black.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub border: Option<[f64; 3]>,
    /// Milliseconds waited after committing the patch before measuring.
    pub settle_ms: u64,
}

impl Capture {
    /// Serialize to pretty JSON.
    pub fn to_json_pretty(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Parse from a JSON string.
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }

    /// Write this capture to `path` as pretty JSON.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), Error> {
        let json = self.to_json_pretty()?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// Read a capture from `path`.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, Error> {
        let text = std::fs::read_to_string(path)?;
        Ok(Self::from_json(&text)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_capture() -> Capture {
        Capture {
            schema_version: SCHEMA_VERSION,
            timestamp: "2026-05-24T12:00:00Z".to_string(),
            tool: ToolInfo {
                name: "tristim".to_string(),
                version: "0.1.0".to_string(),
                git_revision: Some("abc1234".to_string()),
            },
            device: DeviceInfo {
                product: "Spyder 2024".to_string(),
                usb_pid: 0x0a0b,
                serial: "87000216".to_string(),
                hw_version: (6, 0),
                cal_index: 0,
                calibration: Some(CalibrationInfo {
                    black_floor: vec![6.0; 6],
                    matrix: [
                        vec![0.1, 0.2, 0.3, 0.4, 0.5, 0.6],
                        vec![0.2, 0.3, 0.4, 0.5, 0.6, 0.7],
                        vec![0.3, 0.4, 0.5, 0.6, 0.7, 0.8],
                    ],
                    gain: [1.0, 1.0, 1.0],
                    offset: [0.0, 0.0, 0.0],
                }),
            },
            output: OutputInfo {
                name: "DP-4".to_string(),
                make: "ASUS".to_string(),
                model: "PG27UCDM".to_string(),
                description: "ASUS PG27UCDM".to_string(),
                mode: Some(OutputMode {
                    width: 3840,
                    height: 2160,
                    refresh_mhz: Some(240_000),
                }),
            },
            capabilities: Capabilities {
                supported_transfer_functions: vec!["srgb".into(), "st2084_pq".into()],
                supported_primaries: vec!["srgb".into(), "bt2020".into()],
                supported_features: vec!["parametric".into()],
                supported_render_intents: vec!["perceptual".into()],
            },
            compositor: CompositorInfo {
                process: Some("niri".to_string()),
                desktop: Some("niri".to_string()),
                globals: vec![
                    GlobalInfo {
                        interface: "wl_compositor".to_string(),
                        version: 6,
                    },
                    GlobalInfo {
                        interface: "wp_color_manager_v1".to_string(),
                        version: 1,
                    },
                ],
            },
            run: Some(RunInfo {
                prep_ms: 6000,
                fast_integration_ms: Some(100),
                scatter: Some(ScatterInfo {
                    count: 24,
                    seed: 0xC0FFEE,
                }),
                gamut_probe: Some(GamutProbeInfo {
                    repeats: 4,
                    max_depth: 3,
                    flat_eps: 2.0,
                    fold_eps: 1.0,
                    fold_min_side: 0.125,
                }),
            }),
            trials: vec![FormatTrial {
                requested: Some(ColorDescription {
                    transfer_function: "st2084_pq".to_string(),
                    render_intent: "perceptual".to_string(),
                    primaries: "bt2020".to_string(),
                    reference_white_nits: Some(203.0),
                    mastering: Some(Mastering {
                        min_luminance_nits: 0.0005,
                        max_luminance_nits: 400.0,
                        max_cll_nits: 400.0,
                        max_fall_nits: 200.0,
                    }),
                }),
                pixel_format: "xbgr16161616f".to_string(),
                outcome: Negotiation::Accepted { identity: 42 },
                gamut: None,
                samples: vec![Sample {
                    requested: [0.5081, 0.0, 0.0],
                    measured: Measured {
                        raw: vec![100, 2, 3, 4, 5, 6],
                        xyz: [41.2, 21.3, 1.9],
                        xy: Some([0.64, 0.33]),
                    },
                    context: SampleContext {
                        window_fraction: 1.0,
                        border: None,
                        settle_ms: 250,
                    },
                    source: SampleSource::Sweep,
                    repeats: 1,
                    adaptive_tier: Some(AdaptiveTier::Fast),
                    elapsed_ms: Some(12_345),
                }],
            }],
        }
    }

    #[test]
    fn json_round_trip() {
        let capture = sample_capture();
        let json = capture.to_json_pretty().expect("serialize");
        let back = Capture::from_json(&json).expect("deserialize");
        assert_eq!(capture, back);
    }

    #[test]
    fn unmanaged_and_rejected_round_trip() {
        let mut capture = sample_capture();
        capture.trials[0].outcome = Negotiation::Unmanaged;
        capture.trials.push(FormatTrial {
            requested: Some(ColorDescription {
                transfer_function: "srgb".to_string(),
                render_intent: "perceptual".to_string(),
                primaries: "dci_p3".to_string(),
                reference_white_nits: None,
                mastering: None,
            }),
            pixel_format: "xrgb8888".to_string(),
            outcome: Negotiation::Rejected {
                cause: "unsupported_primaries".to_string(),
                message: "no DCI-P3".to_string(),
            },
            gamut: None,
            samples: vec![],
        });
        let json = capture.to_json_pretty().expect("serialize");
        let back = Capture::from_json(&json).expect("deserialize");
        assert_eq!(capture, back);
    }

    #[test]
    fn optional_fields_are_omitted_when_none() {
        let mut capture = sample_capture();
        capture.tool.git_revision = None;
        capture.output.mode = None;
        if let Some(d) = &mut capture.trials[0].requested {
            d.reference_white_nits = None;
            d.mastering = None;
        }
        let json = capture.to_json_pretty().expect("serialize");
        assert!(!json.contains("git_revision"));
        assert!(!json.contains("\"mode\""));
        assert!(!json.contains("reference_white_nits"));
        assert!(!json.contains("\"mastering\""));
    }

    #[test]
    fn gamut_section_round_trips() {
        let mut capture = sample_capture();
        capture.trials[0].gamut = Some(MeasuredGamut {
            white_xyz: [95.0, 100.0, 108.0],
            vertices: vec![
                GamutVertex {
                    code_value: [1.0, 1.0, 1.0],
                    xyz: [95.0, 100.0, 108.0],
                    lab: [100.0, 0.0, 0.0],
                    trustworthy: true,
                },
                GamutVertex {
                    code_value: [0.0, 0.0, 0.0],
                    xyz: [0.2, 0.2, 0.2],
                    lab: [1.8, 0.0, 0.0],
                    trustworthy: false,
                },
            ],
            patches: vec![GamutPatch {
                face: "R=1".to_string(),
                corners: [0, 1, 0, 1],
                status: "folded".to_string(),
            }],
        });
        let json = capture.to_json_pretty().expect("serialize");
        let back = Capture::from_json(&json).expect("deserialize");
        assert_eq!(capture, back);
    }

    /// A capture without a probed gamut omits the section, and it still loads
    /// (the field is `#[serde(default)]`).
    #[test]
    fn missing_gamut_omitted_and_loads() {
        let capture = sample_capture(); // no gamut set
        let json = capture.to_json_pretty().expect("serialize");
        assert!(!json.contains("\"gamut\""));
        let back = Capture::from_json(&json).expect("deserialize");
        assert!(back.trials[0].gamut.is_none());
    }

    /// A gamut-probe sample carries its source + repeat count through a
    /// round-trip, while an ordinary sweep sample omits both (they default to
    /// `Sweep` / `1`), so older captures and the common case stay compact.
    #[test]
    fn sample_source_round_trips_and_omits_default() {
        let mut capture = sample_capture();
        // Drop the run block: its `gamut_probe.repeats` field would trip the
        // sample-level "repeats absent" assertion below.
        capture.run = None;
        // The fixture sample is a sweep sample: both fields should be absent.
        let json = capture.to_json_pretty().expect("serialize");
        assert!(!json.contains("\"source\""));
        assert!(!json.contains("\"repeats\""));

        // Add a probe-derived sample and confirm both fields survive.
        capture.trials[0].samples.push(Sample {
            requested: [1.0, 0.0, 0.0],
            measured: Measured {
                raw: vec![120, 4, 5, 6, 7, 8],
                xyz: [44.0, 22.0, 2.0],
                xy: Some([0.64, 0.33]),
            },
            context: SampleContext {
                window_fraction: 1.0,
                border: None,
                settle_ms: 250,
            },
            source: SampleSource::GamutProbe,
            repeats: 8,
            adaptive_tier: None,
            elapsed_ms: None,
        });
        let json = capture.to_json_pretty().expect("serialize");
        assert!(json.contains("\"gamut_probe\""));
        assert!(json.contains("\"repeats\""));
        let back = Capture::from_json(&json).expect("deserialize");
        assert_eq!(capture, back);

        // A document with both fields stripped loads to the defaults.
        let stripped: serde_json::Value = {
            let mut v: serde_json::Value = serde_json::from_str(&json).unwrap();
            for s in v["trials"][0]["samples"].as_array_mut().unwrap() {
                let o = s.as_object_mut().unwrap();
                o.remove("source");
                o.remove("repeats");
            }
            v
        };
        let back = Capture::from_json(&stripped.to_string()).expect("deserialize stripped");
        let s = back.trials[0].samples.last().unwrap();
        assert_eq!(s.source, SampleSource::Sweep);
        assert_eq!(s.repeats, 1);
    }

    /// A pre-v5 capture loads with every v5 addition at its default: fixed
    /// 6-wide `raw` arrays parse into the variable-length field, and the new
    /// sections/fields come back absent.
    #[test]
    fn v4_capture_loads_with_v5_defaults() {
        let mut capture = sample_capture();
        capture.schema_version = 4;
        capture.run = None;
        capture.device.calibration = None;
        capture.trials[0].samples[0].adaptive_tier = None;
        capture.trials[0].samples[0].elapsed_ms = None;
        let mut v: serde_json::Value =
            serde_json::from_str(&capture.to_json_pretty().unwrap()).unwrap();
        // v4 writers always emitted a 6-wide raw array and never the v5 keys.
        v["trials"][0]["samples"][0]["measured"]["raw"] = serde_json::json!([100, 2, 3, 4, 5, 6]);
        v["trials"][0]["requested"]
            .as_object_mut()
            .unwrap()
            .remove("render_intent");
        let back = Capture::from_json(&v.to_string()).expect("v4 capture loads");
        assert_eq!(back.schema_version, 4);
        assert!(back.run.is_none());
        assert!(back.device.calibration.is_none());
        let s = &back.trials[0].samples[0];
        assert_eq!(s.measured.raw, vec![100, 2, 3, 4, 5, 6]);
        assert!(s.adaptive_tier.is_none());
        assert!(s.elapsed_ms.is_none());
        assert_eq!(
            back.trials[0].requested.as_ref().unwrap().render_intent,
            "perceptual"
        );
    }

    /// A v1 capture (no `compositor` section) still loads — the field is
    /// `#[serde(default)]`, so the missing section becomes an empty
    /// `CompositorInfo` rather than a parse error.
    #[test]
    fn v1_capture_without_compositor_loads() {
        let mut capture = sample_capture();
        capture.compositor = CompositorInfo::default();
        let json = capture.to_json_pretty().expect("serialize");
        // An empty CompositorInfo serializes to `{}` (all fields skipped).
        let back = Capture::from_json(&json).expect("deserialize");
        assert_eq!(back.compositor, CompositorInfo::default());

        // And a literal v1 document with the section entirely absent.
        let stripped: serde_json::Value = {
            let mut v: serde_json::Value = serde_json::from_str(&json).unwrap();
            v.as_object_mut().unwrap().remove("compositor");
            v
        };
        let back = Capture::from_json(&stripped.to_string()).expect("deserialize v1");
        assert_eq!(back.compositor, CompositorInfo::default());
    }
}
