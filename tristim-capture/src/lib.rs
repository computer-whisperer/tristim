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
pub const SCHEMA_VERSION: u32 = 1;

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

/// One color format put under test: the description we asked for, whether the
/// compositor accepted it, and the samples taken under it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FormatTrial {
    /// The colorimetry we requested ("settings Y"). The analysis tool derives
    /// the expected output from this.
    pub requested: ColorDescription,
    /// `wl_shm` pixel format the buffer used, e.g. `"xrgb8888"` or
    /// `"xbgr16161616f"`.
    pub pixel_format: String,
    /// Whether the compositor accepted the description. A rejection is itself
    /// a result worth recording.
    pub outcome: Negotiation,
    /// Measurements taken while this trial's format was active.
    pub samples: Vec<Sample>,
}

/// A parametric color description, in semantic units (nits, not protocol
/// ticks). Mirrors the colorimetry expressible through
/// `wp_color_management_v1`'s parametric creator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColorDescription {
    /// Transfer function name, e.g. `"srgb"`, `"st2084_pq"`, `"gamma22"`.
    pub transfer_function: String,
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
}

/// A colorimeter reading.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Measured {
    /// The 6 raw sensor channel counts, before raw→XYZ conversion.
    pub raw: [u16; 6],
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
            trials: vec![FormatTrial {
                requested: ColorDescription {
                    transfer_function: "st2084_pq".to_string(),
                    primaries: "bt2020".to_string(),
                    reference_white_nits: Some(203.0),
                    mastering: Some(Mastering {
                        min_luminance_nits: 0.0005,
                        max_luminance_nits: 400.0,
                        max_cll_nits: 400.0,
                        max_fall_nits: 200.0,
                    }),
                },
                pixel_format: "xbgr16161616f".to_string(),
                outcome: Negotiation::Accepted { identity: 42 },
                samples: vec![Sample {
                    requested: [0.5081, 0.0, 0.0],
                    measured: Measured {
                        raw: [100, 2, 3, 4, 5, 6],
                        xyz: [41.2, 21.3, 1.9],
                        xy: Some([0.64, 0.33]),
                    },
                    context: SampleContext {
                        window_fraction: 1.0,
                        border: None,
                        settle_ms: 250,
                    },
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
            requested: ColorDescription {
                transfer_function: "srgb".to_string(),
                primaries: "dci_p3".to_string(),
                reference_white_nits: None,
                mastering: None,
            },
            pixel_format: "xrgb8888".to_string(),
            outcome: Negotiation::Rejected {
                cause: "unsupported_primaries".to_string(),
                message: "no DCI-P3".to_string(),
            },
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
        capture.trials[0].requested.reference_white_nits = None;
        capture.trials[0].requested.mastering = None;
        let json = capture.to_json_pretty().expect("serialize");
        assert!(!json.contains("git_revision"));
        assert!(!json.contains("\"mode\""));
        assert!(!json.contains("reference_white_nits"));
        assert!(!json.contains("mastering"));
    }
}
