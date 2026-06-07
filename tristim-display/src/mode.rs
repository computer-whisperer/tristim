//! Pipeline planning: which color representations can this compositor
//! arrange, and through which buffer?
//!
//! A [`RenderMode`] is what a consumer sweeps: a color description (or
//! unmanaged) plus a [`BufferPolicy`]. The buffer format is an
//! implementation detail of arranging the pipeline — under
//! [`BufferPolicy::Auto`] this module picks an adequate advertised
//! format for the representation (float for extended-range encodings,
//! deepest available for PQ/HLG, canonical 8-bit for SDR) and the
//! choice surfaces only as a recorded fact and in "limited by"
//! [`PipelinePlan::notes`]. Pin a format with [`BufferPolicy::Exact`]
//! when the buffer itself is the question under test.
//!
//! [`DisplayCapabilities::plan`](crate::DisplayCapabilities::plan) is
//! the entry point; [`PatchSurface::open_mode`](crate::PatchSurface::open_mode)
//! arranges a planned mode on an output.

use crate::color_mgmt::{
    AttachError, DescriptionKind, DescriptionRequest, TransferChoice, validate_description,
};
use crate::format::BufferFormat;

/// A color representation to sweep — what the compositor is told code
/// values mean, with the buffer left as this crate's problem by
/// default.
#[derive(Clone, Debug, PartialEq)]
pub struct RenderMode {
    /// The color description to negotiate; `None` = unmanaged (the
    /// compositor interprets the buffer by its own default).
    pub description: Option<DescriptionRequest>,
    pub buffer: BufferPolicy,
}

impl RenderMode {
    /// An unmanaged mode with an auto-chosen (8-bit) buffer.
    pub fn unmanaged() -> Self {
        Self {
            description: None,
            buffer: BufferPolicy::Auto,
        }
    }

    /// A described mode with an auto-chosen buffer.
    pub fn described(description: DescriptionRequest) -> Self {
        Self {
            description: Some(description),
            buffer: BufferPolicy::Auto,
        }
    }
}

/// How to choose the buffer format realizing a [`RenderMode`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BufferPolicy {
    /// Pick the best advertised format adequate for the representation:
    /// float required for extended-range encodings (`ext_linear`,
    /// `ext_srgb`, Windows-scRGB — negative code values exist), fp16
    /// preferred then deep unorm for PQ/HLG, canonical 8-bit for SDR.
    Auto,
    /// Pin a specific format — for when the buffer itself is the
    /// question ("what does the compositor do with PQ in a 10-bit
    /// buffer?").
    Exact(BufferFormat),
}

/// A concrete, arrangeable realization of a [`RenderMode`] on the
/// queried compositor.
#[derive(Clone, Debug, PartialEq)]
pub struct PipelinePlan {
    /// The buffer format the pipeline will use.
    pub buffer: BufferFormat,
    /// Human-readable "limited by" notes — e.g. fp16 being unavailable
    /// and a deep unorm format standing in. Empty when the first
    /// preference was available.
    pub notes: Vec<String>,
}

/// Why a [`RenderMode`] can't be arranged on this compositor. The
/// `reason()` text is chip-sized for UI surfacing.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Unarrangeable {
    /// The mode wants a color description but the compositor exposes no
    /// `wp_color_manager_v1`.
    NoColorManagement,
    /// The description needs a protocol value or feature the compositor
    /// didn't advertise (or that this build can't map).
    Description(AttachError),
    /// The pinned [`BufferPolicy::Exact`] format isn't advertised.
    BufferNotAdvertised(BufferFormat),
    /// No advertised buffer format is adequate for the representation
    /// (`needs` is what was required, e.g. "floating-point").
    NoAdequateBuffer { needs: &'static str },
}

impl Unarrangeable {
    /// A short reason suitable for a chip next to a disabled mode toggle.
    pub fn reason(&self) -> String {
        match self {
            Unarrangeable::NoColorManagement => "no color management".to_string(),
            Unarrangeable::Description(e) => e.reason(),
            Unarrangeable::BufferNotAdvertised(f) => format!("no {} buffer", f.name()),
            Unarrangeable::NoAdequateBuffer { needs } => format!("no {needs} buffer"),
        }
    }
}

impl std::fmt::Display for Unarrangeable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unarrangeable::NoColorManagement => {
                write!(f, "compositor doesn't advertise wp_color_manager_v1")
            }
            Unarrangeable::Description(e) => write!(f, "{e}"),
            Unarrangeable::BufferNotAdvertised(b) => {
                write!(
                    f,
                    "compositor doesn't advertise the {} buffer format",
                    b.name()
                )
            }
            Unarrangeable::NoAdequateBuffer { needs } => {
                write!(f, "no advertised buffer format is {needs}")
            }
        }
    }
}

impl std::error::Error for Unarrangeable {}

/// What a representation demands of its buffer.
enum BufferNeeds {
    /// Extended-range encoding: code values outside `0..=1` are part of
    /// the representation, so only float formats qualify.
    Float,
    /// High dynamic range in `0..=1`: fp16 preferred for code-value
    /// resolution, deep unorm acceptable.
    Deep,
    /// SDR: canonical 8-bit.
    Sdr,
}

fn buffer_needs(description: Option<&DescriptionRequest>) -> BufferNeeds {
    let Some(d) = description else {
        return BufferNeeds::Sdr;
    };
    match &d.kind {
        DescriptionKind::WindowsScrgb => BufferNeeds::Float,
        DescriptionKind::Parametric(p) => match &p.transfer_function {
            TransferChoice::Named(n) => match n.as_str() {
                "ext_linear" | "ext_srgb" => BufferNeeds::Float,
                "st2084_pq" | "hlg" => BufferNeeds::Deep,
                _ => BufferNeeds::Sdr,
            },
            TransferChoice::Power(_) => BufferNeeds::Sdr,
        },
    }
}

/// Float formats, X-variants first (alpha is dead weight for a patch).
const FLOAT_PREFS: &[BufferFormat] = &[
    BufferFormat::Xbgr16161616f,
    BufferFormat::Abgr16161616f,
    BufferFormat::Xrgb16161616f,
    BufferFormat::Argb16161616f,
];

/// Deep unorm fallbacks for HDR-in-`0..=1`, deepest first.
const DEEP_UNORM_PREFS: &[BufferFormat] = &[
    BufferFormat::Xbgr16161616,
    BufferFormat::Abgr16161616,
    BufferFormat::Xrgb16161616,
    BufferFormat::Argb16161616,
    BufferFormat::Xrgb2101010,
    BufferFormat::Xbgr2101010,
    BufferFormat::Argb2101010,
    BufferFormat::Abgr2101010,
];

pub(crate) fn plan(
    caps: &crate::DisplayCapabilities,
    mode: &RenderMode,
) -> Result<PipelinePlan, Unarrangeable> {
    // The description must be arrangeable before the buffer matters.
    if let Some(d) = &mode.description {
        if !caps.has_color_management() {
            return Err(Unarrangeable::NoColorManagement);
        }
        validate_description(d, &caps.color).map_err(Unarrangeable::Description)?;
    }

    let pick = |prefs: &[BufferFormat]| {
        prefs
            .iter()
            .copied()
            .find(|&f| caps.supports_buffer_format(f))
    };

    match mode.buffer {
        BufferPolicy::Exact(f) => {
            if caps.supports_buffer_format(f) {
                Ok(PipelinePlan {
                    buffer: f,
                    notes: Vec::new(),
                })
            } else {
                Err(Unarrangeable::BufferNotAdvertised(f))
            }
        }
        BufferPolicy::Auto => match buffer_needs(mode.description.as_ref()) {
            BufferNeeds::Sdr => Ok(PipelinePlan {
                buffer: BufferFormat::Xrgb8888, // mandatory per wl_shm
                notes: Vec::new(),
            }),
            BufferNeeds::Float => match pick(FLOAT_PREFS) {
                Some(buffer) => Ok(PipelinePlan {
                    buffer,
                    notes: Vec::new(),
                }),
                None => Err(Unarrangeable::NoAdequateBuffer {
                    needs: "floating-point",
                }),
            },
            BufferNeeds::Deep => match pick(FLOAT_PREFS) {
                Some(buffer) => Ok(PipelinePlan {
                    buffer,
                    notes: Vec::new(),
                }),
                None => match pick(DEEP_UNORM_PREFS) {
                    Some(buffer) => Ok(PipelinePlan {
                        buffer,
                        notes: vec![format!(
                            "fp16 unavailable; using {} ({}-bit unorm)",
                            buffer.name(),
                            buffer.color_depth()
                        )],
                    }),
                    None => Err(Unarrangeable::NoAdequateBuffer {
                        needs: "floating-point or ≥10-bit",
                    }),
                },
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DisplayCapabilities;
    use crate::color_mgmt::ParametricDescription;

    fn pq_mode() -> RenderMode {
        RenderMode::described(DescriptionRequest::parametric(
            ParametricDescription::named("st2084_pq", "bt2020"),
        ))
    }

    #[test]
    fn sdr_modes_plan_onto_8bit_anywhere() {
        let bare = DisplayCapabilities::default();
        let plan = bare.plan(&RenderMode::unmanaged()).unwrap();
        assert_eq!(plan.buffer, BufferFormat::Xrgb8888);
        assert!(plan.notes.is_empty());
    }

    #[test]
    fn described_modes_need_color_management() {
        let bare = DisplayCapabilities::default();
        assert_eq!(bare.plan(&pq_mode()), Err(Unarrangeable::NoColorManagement));
    }

    #[test]
    fn pq_prefers_fp16_then_degrades_with_a_note() {
        let tfs = &["srgb", "st2084_pq"];
        let prims = &["srgb", "bt2020"];

        let fp16 = DisplayCapabilities::advertising(tfs, prims, &[BufferFormat::Xbgr16161616f]);
        assert_eq!(
            fp16.plan(&pq_mode()).unwrap().buffer,
            BufferFormat::Xbgr16161616f
        );

        let deep = DisplayCapabilities::advertising(tfs, prims, &[BufferFormat::Xrgb2101010]);
        let plan = deep.plan(&pq_mode()).unwrap();
        assert_eq!(plan.buffer, BufferFormat::Xrgb2101010);
        assert_eq!(plan.notes.len(), 1, "degradation should be noted");

        let sdr_only = DisplayCapabilities::advertising(tfs, prims, &[]);
        assert!(matches!(
            sdr_only.plan(&pq_mode()),
            Err(Unarrangeable::NoAdequateBuffer { .. })
        ));
    }

    #[test]
    fn extended_range_requires_float_no_unorm_fallback() {
        let caps = DisplayCapabilities::advertising(
            &["srgb", "ext_linear"],
            &["srgb"],
            &[BufferFormat::Xbgr16161616], // deep unorm can't carry negatives
        );
        let mode = RenderMode::described(DescriptionRequest::named("ext_linear", "srgb"));
        assert!(matches!(
            caps.plan(&mode),
            Err(Unarrangeable::NoAdequateBuffer {
                needs: "floating-point"
            })
        ));

        let scrgb = RenderMode::described(DescriptionRequest::windows_scrgb());
        let with_float =
            DisplayCapabilities::advertising(&["srgb"], &["srgb"], &[BufferFormat::Abgr16161616f]);
        assert_eq!(
            with_float.plan(&scrgb).unwrap().buffer,
            BufferFormat::Abgr16161616f
        );
    }

    #[test]
    fn exact_policy_gates_on_advertisement() {
        let caps = DisplayCapabilities::advertising(&["srgb"], &["srgb"], &[]);
        let mut mode = RenderMode::described(DescriptionRequest::named("srgb", "srgb"));
        mode.buffer = BufferPolicy::Exact(BufferFormat::Xrgb2101010);
        assert_eq!(
            caps.plan(&mode),
            Err(Unarrangeable::BufferNotAdvertised(
                BufferFormat::Xrgb2101010
            ))
        );
        // Mandatory formats are always plannable.
        mode.buffer = BufferPolicy::Exact(BufferFormat::Argb8888);
        assert_eq!(caps.plan(&mode).unwrap().buffer, BufferFormat::Argb8888);
    }

    #[test]
    fn description_problems_surface_before_buffer_problems() {
        // PQ not advertised → Description error even though no deep buffer
        // exists either.
        let caps = DisplayCapabilities::advertising(&["srgb"], &["srgb"], &[]);
        assert!(matches!(
            caps.plan(&pq_mode()),
            Err(Unarrangeable::Description(_))
        ));
    }
}
