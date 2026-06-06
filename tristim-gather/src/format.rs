//! Color-format specs: parse a `name[:k=v,...]` token into the buffer format
//! and color description to negotiate. Shared by the CLI (`--format`)
//! and the GUI's capture form, so the set of known formats lives in one place.
//!
//! Every spec accepts a `buf=<wl_shm name>` parameter overriding its
//! default buffer format (e.g. `srgb:buf=xrgb2101010` to probe an sRGB
//! description through a 10-bit buffer, or `pq-bt2020:buf=xbgr16161616`
//! for PQ in 16-bit unorm on compositors without fp16 shm support).

use std::collections::HashMap;

use tristim_capture as cap;
use tristim_display::{
    self as display, BufferFormat, DescriptionKind, DescriptionRequest, ParametricDescription,
    PrimariesChoice, TransferChoice,
};

/// A parsed `--format` spec: the buffer format + the description to negotiate
/// (`None` = unmanaged). Construct via [`parse_format`].
#[derive(Debug, Clone)]
pub struct FormatSpec {
    token: String,
    buffer_format: BufferFormat,
    description: Option<DescriptionRequest>,
}

impl FormatSpec {
    /// The original spec token, e.g. `"pq-bt2020:peak=600"`.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// `wl_shm` pixel-format name for this spec's buffer.
    pub fn pixel_format_str(&self) -> &'static str {
        self.buffer_format.name()
    }

    pub(crate) fn buffer_format(&self) -> BufferFormat {
        self.buffer_format
    }

    pub(crate) fn description(&self) -> Option<DescriptionRequest> {
        self.description.clone()
    }

    /// Check whether this format can be reproduced on a compositor with the
    /// given [`DisplayCapabilities`](display::DisplayCapabilities). `Ok(())`
    /// means reachable; the `Err` carries the first unmet requirement
    /// (buffer format, color-management protocol, transfer function,
    /// primaries, feature, or render intent) for display.
    pub fn reachability(&self, caps: &display::DisplayCapabilities) -> Result<(), Unreachable> {
        if !caps.supports_buffer_format(self.buffer_format) {
            return Err(Unreachable::BufferFormat(self.buffer_format));
        }
        let Some(d) = &self.description else {
            return Ok(());
        };
        if !caps.has_color_management() {
            return Err(Unreachable::NoColorManagement);
        }
        if !caps.supports_render_intent(&d.render_intent) {
            return Err(Unreachable::RenderIntent(d.render_intent.clone()));
        }
        match &d.kind {
            DescriptionKind::Parametric(p) => {
                if !caps.supports_feature("parametric") {
                    return Err(Unreachable::Feature("parametric"));
                }
                match &p.transfer_function {
                    TransferChoice::Named(tf) => {
                        if !caps.supports_transfer_function(tf) {
                            return Err(Unreachable::TransferFunction(tf.clone()));
                        }
                    }
                    TransferChoice::Power(_) => {
                        if !caps.supports_feature("set_tf_power") {
                            return Err(Unreachable::Feature("set_tf_power"));
                        }
                    }
                }
                match &p.primaries {
                    PrimariesChoice::Named(pr) => {
                        if !caps.supports_primaries(pr) {
                            return Err(Unreachable::Primaries(pr.clone()));
                        }
                    }
                    PrimariesChoice::Custom(_) => {
                        if !caps.supports_feature("set_primaries") {
                            return Err(Unreachable::Feature("set_primaries"));
                        }
                    }
                }
                if p.luminances.is_some() && !caps.supports_feature("set_luminances") {
                    return Err(Unreachable::Feature("set_luminances"));
                }
                if let Some(m) = &p.mastering {
                    if (m.luminance_nits.is_some() || m.primaries.is_some())
                        && !caps.supports_feature("set_mastering_display_primaries")
                    {
                        return Err(Unreachable::Feature("set_mastering_display_primaries"));
                    }
                }
            }
            DescriptionKind::WindowsScrgb => {
                if !caps.supports_feature("windows_scrgb") {
                    return Err(Unreachable::Feature("windows_scrgb"));
                }
            }
        }
        Ok(())
    }

    /// The capture-schema description mirroring what we requested.
    pub(crate) fn color_description(&self) -> Option<cap::ColorDescription> {
        let d = self.description.as_ref()?;
        Some(match &d.kind {
            DescriptionKind::Parametric(p) => cap::ColorDescription {
                transfer_function: p.transfer_function.label(),
                primaries: p.primaries.label().to_string(),
                reference_white_nits: p.luminances.map(|l| l.reference_nits),
                // The capture's mastering record wants the full ST 2086
                // tuple; record it only when the request carried all of it
                // (the specs this module builds always do).
                mastering: p.mastering.as_ref().and_then(|m| {
                    let (min, max) = m.luminance_nits?;
                    Some(cap::Mastering {
                        min_luminance_nits: min,
                        max_luminance_nits: max,
                        max_cll_nits: m.max_cll_nits?,
                        max_fall_nits: m.max_fall_nits?,
                    })
                }),
            },
            // Windows-scRGB is protocol-defined: sRGB primaries, extended
            // linear TF, R=G=B=1.0 ≡ 80 cd/m² (BT.2100/PQ system).
            DescriptionKind::WindowsScrgb => cap::ColorDescription {
                transfer_function: "ext_linear".to_string(),
                primaries: "srgb".to_string(),
                reference_white_nits: Some(80.0),
                mastering: None,
            },
        })
    }
}

/// Why a format can't be reached on the current compositor. See
/// [`FormatSpec::reachability`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unreachable {
    /// The compositor doesn't advertise the required `wl_shm` buffer format.
    BufferFormat(BufferFormat),
    /// The format needs a color description but the compositor exposes no
    /// `wp_color_manager_v1`.
    NoColorManagement,
    /// The transfer function isn't in the compositor's advertised set.
    TransferFunction(String),
    /// The primaries aren't in the compositor's advertised set.
    Primaries(String),
    /// A required optional feature (protocol `feature` enum entry name)
    /// isn't advertised.
    Feature(&'static str),
    /// The render intent isn't in the compositor's advertised set.
    RenderIntent(String),
}

impl Unreachable {
    /// A short reason suitable for a chip next to a disabled format toggle.
    pub fn reason(&self) -> String {
        match self {
            Unreachable::BufferFormat(f) => format!("no {} buffer", f.name()),
            Unreachable::NoColorManagement => "no color management".to_string(),
            Unreachable::TransferFunction(tf) => format!("TF {tf} unsupported"),
            Unreachable::Primaries(p) => format!("primaries {p} unsupported"),
            Unreachable::Feature(f) => format!("no {f} feature"),
            Unreachable::RenderIntent(i) => format!("intent {i} unsupported"),
        }
    }
}

/// The set of format tokens this build understands, in menu order. Each is a
/// valid prefix for [`parse_format`]; all accept `:k=v` params (`buf=` on
/// every one, the PQ ones also `peak=`/`min=`/`maxcll=`/`maxfall=`).
pub const KNOWN_FORMATS: &[&str] = &[
    "unmanaged",
    "srgb",
    "srgb-p3",
    "pq-bt2020",
    "pq-p3",
    "scrgb",
];

/// Parse a `--format` spec (`name[:k=v,...]`) into a [`FormatSpec`].
pub fn parse_format(spec: &str) -> Result<FormatSpec, String> {
    let (name, params_str) = spec.split_once(':').unwrap_or((spec, ""));
    let params = parse_params(params_str)?;
    let token = spec.to_string();

    let num = |key: &str| -> Result<Option<f64>, String> {
        params
            .get(key)
            .map(|v| {
                v.parse::<f64>()
                    .map_err(|_| format!("bad number in param {key}={v}"))
            })
            .transpose()
    };

    let mk_mastering = |default_peak: f64| -> Result<display::Mastering, String> {
        let peak = num("peak")?.unwrap_or(default_peak);
        Ok(display::Mastering {
            luminance_nits: Some((num("min")?.unwrap_or(0.0005), peak)),
            primaries: None,
            max_cll_nits: Some(num("maxcll")?.unwrap_or(peak)),
            max_fall_nits: Some(num("maxfall")?.unwrap_or(peak / 2.0)),
        })
    };
    // Buffer override: any wl_shm RGB format name this build can write.
    let buf = |default: BufferFormat| -> Result<BufferFormat, String> {
        match params.get("buf") {
            None => Ok(default),
            Some(name) => BufferFormat::from_name(name)
                .ok_or_else(|| format!("unknown buffer format {name:?} in buf= param")),
        }
    };
    let managed = |bf, tf: &str, prim: &str, mastering| {
        let mut p = ParametricDescription::named(tf, prim);
        p.mastering = mastering;
        Ok::<_, String>(FormatSpec {
            token: token.clone(),
            buffer_format: buf(bf)?,
            description: Some(DescriptionRequest::parametric(p)),
        })
    };

    match name {
        "unmanaged" => Ok(FormatSpec {
            token: token.clone(),
            buffer_format: buf(BufferFormat::Xrgb8888)?,
            description: None,
        }),
        "srgb" => managed(BufferFormat::Xrgb8888, "srgb", "srgb", None),
        "srgb-p3" => managed(BufferFormat::Xrgb8888, "srgb", "display_p3", None),
        "pq-bt2020" => managed(
            BufferFormat::Xbgr16161616f,
            "st2084_pq",
            "bt2020",
            Some(mk_mastering(400.0)?),
        ),
        "pq-p3" => managed(
            BufferFormat::Xbgr16161616f,
            "st2084_pq",
            "display_p3",
            Some(mk_mastering(400.0)?),
        ),
        "scrgb" => Ok(FormatSpec {
            token: token.clone(),
            buffer_format: buf(BufferFormat::Xbgr16161616f)?,
            description: Some(DescriptionRequest::windows_scrgb()),
        }),
        other => Err(format!(
            "unknown format {other:?} (known: {})",
            KNOWN_FORMATS.join(", ")
        )),
    }
}

fn parse_params(s: &str) -> Result<HashMap<String, String>, String> {
    let mut m = HashMap::new();
    if s.is_empty() {
        return Ok(m);
    }
    for kv in s.split(',') {
        let (k, v) = kv
            .split_once('=')
            .ok_or_else(|| format!("bad param {kv:?} (expected key=value)"))?;
        m.insert(k.to_string(), v.to_string());
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tristim_display::DisplayCapabilities;

    #[test]
    fn reachability_gates_on_capabilities() {
        // niri-like: no color management, only the mandatory 8-bit buffers.
        let bare = DisplayCapabilities::default();
        assert!(
            parse_format("unmanaged")
                .unwrap()
                .reachability(&bare)
                .is_ok()
        );
        assert_eq!(
            parse_format("srgb").unwrap().reachability(&bare),
            Err(Unreachable::NoColorManagement)
        );
        // fp16 is checked before color management, so the HDR format trips on
        // the buffer first.
        assert!(matches!(
            parse_format("pq-bt2020").unwrap().reachability(&bare),
            Err(Unreachable::BufferFormat(_))
        ));

        // A wide-gamut + HDR compositor: everything reachable.
        let rich = DisplayCapabilities::advertising(
            &["srgb", "st2084_pq"],
            &["srgb", "bt2020", "display_p3"],
            &[BufferFormat::Xbgr16161616f],
        );
        for tok in KNOWN_FORMATS {
            assert!(
                parse_format(tok).unwrap().reachability(&rich).is_ok(),
                "{tok} should be reachable on a full compositor"
            );
        }

        // Same, but without display_p3 primaries → the P3 formats trip.
        let no_p3 = DisplayCapabilities::advertising(
            &["srgb", "st2084_pq"],
            &["srgb", "bt2020"],
            &[BufferFormat::Xbgr16161616f],
        );
        assert!(matches!(
            parse_format("srgb-p3").unwrap().reachability(&no_p3),
            Err(Unreachable::Primaries(_))
        ));
        assert!(
            parse_format("pq-bt2020")
                .unwrap()
                .reachability(&no_p3)
                .is_ok()
        );

        // fp16 absent but color management present → only the fp16 formats
        // trip, and on the buffer.
        let sdr_cm = DisplayCapabilities::advertising(&["srgb"], &["srgb", "display_p3"], &[]);
        assert!(
            parse_format("srgb-p3")
                .unwrap()
                .reachability(&sdr_cm)
                .is_ok()
        );
        assert!(matches!(
            parse_format("pq-p3").unwrap().reachability(&sdr_cm),
            Err(Unreachable::BufferFormat(_))
        ));
        // ...unless buf= retargets PQ at an advertised deep-unorm buffer;
        // then only the TF gate remains.
        let deep = DisplayCapabilities::advertising(
            &["srgb", "st2084_pq"],
            &["srgb", "bt2020"],
            &[BufferFormat::Xbgr2101010],
        );
        assert!(
            parse_format("pq-bt2020:buf=xbgr2101010")
                .unwrap()
                .reachability(&deep)
                .is_ok()
        );
    }

    #[test]
    fn format_unmanaged_has_no_description() {
        let f = parse_format("unmanaged").unwrap();
        assert!(f.description.is_none());
        assert_eq!(f.pixel_format_str(), "xrgb8888");
        assert!(f.color_description().is_none());
    }

    #[test]
    fn format_srgb_declares_srgb() {
        let f = parse_format("srgb").unwrap();
        let d = f.color_description().unwrap();
        assert_eq!(d.transfer_function, "srgb");
        assert_eq!(d.primaries, "srgb");
        assert!(d.mastering.is_none());
    }

    #[test]
    fn format_pq_params_override_mastering() {
        let f = parse_format("pq-bt2020:peak=600,maxfall=300").unwrap();
        assert_eq!(f.pixel_format_str(), "xbgr16161616f");
        let d = f.color_description().unwrap();
        assert_eq!(d.transfer_function, "st2084_pq");
        assert_eq!(d.primaries, "bt2020");
        let m = d.mastering.unwrap();
        assert_eq!(m.max_luminance_nits, 600.0);
        assert_eq!(m.max_cll_nits, 600.0); // defaults to peak
        assert_eq!(m.max_fall_nits, 300.0);
    }

    #[test]
    fn buf_param_overrides_buffer_format() {
        let f = parse_format("srgb:buf=xrgb2101010").unwrap();
        assert_eq!(f.pixel_format_str(), "xrgb2101010");
        // The description is unchanged by the buffer override.
        assert_eq!(f.color_description().unwrap().transfer_function, "srgb");
        assert!(parse_format("srgb:buf=nope").is_err());
    }

    #[test]
    fn format_scrgb_records_protocol_definition() {
        let f = parse_format("scrgb").unwrap();
        assert_eq!(f.pixel_format_str(), "xbgr16161616f");
        let d = f.color_description().unwrap();
        assert_eq!(d.transfer_function, "ext_linear");
        assert_eq!(d.primaries, "srgb");
        assert_eq!(d.reference_white_nits, Some(80.0));
    }

    #[test]
    fn format_unknown_errors() {
        assert!(parse_format("nope").is_err());
        assert!(parse_format("pq-bt2020:peak=x").is_err());
    }
}
