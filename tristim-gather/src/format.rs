//! Color-format specs: parse a `name[:k=v,...]` token into the
//! [`RenderMode`] to arrange. Shared by the CLI (`--format`) and the
//! GUI's capture form, so the set of known formats lives in one place.
//!
//! Specs name color *representations*; the buffer format realizing one
//! is tristim-display's problem ([`BufferPolicy::Auto`] — fp16
//! preferred for HDR, float required for extended-range, 8-bit for
//! SDR). Every spec accepts a `buf=<wl_shm name>` parameter pinning the
//! buffer instead ([`BufferPolicy::Exact`]) for when the buffer itself
//! is the question — e.g. `pq-bt2020:buf=xrgb2101010` to ask what the
//! compositor does with PQ in a 10-bit buffer.

use std::collections::HashMap;

use tristim_capture as cap;
use tristim_display::{
    self as display, BufferFormat, BufferPolicy, DescriptionKind, DescriptionRequest,
    ParametricDescription, PipelinePlan, RenderMode, Unarrangeable,
};

/// A parsed `--format` spec: a token plus the [`RenderMode`] it names.
/// Construct via [`parse_format`].
#[derive(Debug, Clone)]
pub struct FormatSpec {
    token: String,
    mode: RenderMode,
}

impl FormatSpec {
    /// The original spec token, e.g. `"pq-bt2020:peak=600"`.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// The color representation + buffer policy this spec names.
    pub fn mode(&self) -> &RenderMode {
        &self.mode
    }

    /// Can this spec be arranged on a compositor with the given
    /// capabilities, and through which buffer? Thin delegation to
    /// [`DisplayCapabilities::plan`](display::DisplayCapabilities::plan);
    /// the `Err` carries chip-sized
    /// [`reason`](display::Unarrangeable::reason) text.
    pub fn plan(&self, caps: &display::DisplayCapabilities) -> Result<PipelinePlan, Unarrangeable> {
        caps.plan(&self.mode)
    }

    /// Label for records when no pipeline was arranged: the pinned
    /// buffer name, or `"auto"` when display would have chosen.
    pub(crate) fn buffer_label(&self) -> &'static str {
        match self.mode.buffer {
            BufferPolicy::Exact(f) => f.name(),
            BufferPolicy::Auto => "auto",
        }
    }

    pub(crate) fn description(&self) -> Option<DescriptionRequest> {
        self.mode.description.clone()
    }

    /// The capture-schema description mirroring what we requested.
    pub(crate) fn color_description(&self) -> Option<cap::ColorDescription> {
        let d = self.mode.description.as_ref()?;
        Some(match &d.kind {
            DescriptionKind::Parametric(p) => cap::ColorDescription {
                transfer_function: p.transfer_function.label(),
                render_intent: d.render_intent.clone(),
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
                render_intent: d.render_intent.clone(),
                primaries: "srgb".to_string(),
                reference_white_nits: Some(80.0),
                mastering: None,
            },
            // `DescriptionKind` is `#[non_exhaustive]`, but every request this
            // module formats is one it built itself, so no other kind can
            // reach here.
            _ => unreachable!("description kind not built by this module"),
        })
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
    // Buffer policy: display chooses unless buf= pins a format.
    let buffer = || -> Result<BufferPolicy, String> {
        match params.get("buf") {
            None => Ok(BufferPolicy::Auto),
            Some(name) => BufferFormat::from_name(name)
                .map(BufferPolicy::Exact)
                .ok_or_else(|| format!("unknown buffer format {name:?} in buf= param")),
        }
    };
    let described = |description: DescriptionRequest| {
        Ok::<_, String>(FormatSpec {
            token: token.clone(),
            mode: RenderMode {
                description: Some(description),
                buffer: buffer()?,
            },
        })
    };
    let managed = |tf: &str, prim: &str, mastering| {
        let mut p = ParametricDescription::named(tf, prim);
        p.mastering = mastering;
        described(DescriptionRequest::parametric(p))
    };

    match name {
        "unmanaged" => Ok(FormatSpec {
            token: token.clone(),
            mode: RenderMode {
                description: None,
                buffer: buffer()?,
            },
        }),
        "srgb" => managed("srgb", "srgb", None),
        "srgb-p3" => managed("srgb", "display_p3", None),
        "pq-bt2020" => managed("st2084_pq", "bt2020", Some(mk_mastering(400.0)?)),
        "pq-p3" => managed("st2084_pq", "display_p3", Some(mk_mastering(400.0)?)),
        "scrgb" => described(DescriptionRequest::windows_scrgb()),
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
    fn planning_gates_on_capabilities() {
        // niri-like: no color management, only the mandatory 8-bit buffers.
        let bare = DisplayCapabilities::default();
        assert_eq!(
            parse_format("unmanaged")
                .unwrap()
                .plan(&bare)
                .unwrap()
                .buffer,
            BufferFormat::Xrgb8888
        );
        assert_eq!(
            parse_format("srgb").unwrap().plan(&bare),
            Err(Unarrangeable::NoColorManagement)
        );

        // A wide-gamut + HDR compositor: everything arrangeable, HDR on fp16.
        let rich = DisplayCapabilities::advertising(
            &["srgb", "st2084_pq"],
            &["srgb", "bt2020", "display_p3"],
            &[BufferFormat::Xbgr16161616f],
        );
        for tok in KNOWN_FORMATS {
            assert!(
                parse_format(tok).unwrap().plan(&rich).is_ok(),
                "{tok} should be arrangeable on a full compositor"
            );
        }
        assert_eq!(
            parse_format("pq-bt2020")
                .unwrap()
                .plan(&rich)
                .unwrap()
                .buffer,
            BufferFormat::Xbgr16161616f
        );

        // Same, but without display_p3 primaries → the P3 formats trip.
        let no_p3 = DisplayCapabilities::advertising(
            &["srgb", "st2084_pq"],
            &["srgb", "bt2020"],
            &[BufferFormat::Xbgr16161616f],
        );
        assert!(matches!(
            parse_format("srgb-p3").unwrap().plan(&no_p3),
            Err(Unarrangeable::Description(_))
        ));
        assert!(parse_format("pq-bt2020").unwrap().plan(&no_p3).is_ok());

        // fp16 absent: PQ degrades to an advertised deep unorm with a note,
        // or fails when nothing adequate exists.
        let deep = DisplayCapabilities::advertising(
            &["srgb", "st2084_pq"],
            &["srgb", "bt2020"],
            &[BufferFormat::Xrgb2101010],
        );
        let plan = parse_format("pq-bt2020").unwrap().plan(&deep).unwrap();
        assert_eq!(plan.buffer, BufferFormat::Xrgb2101010);
        assert!(!plan.notes.is_empty());
        let sdr_only =
            DisplayCapabilities::advertising(&["srgb", "st2084_pq"], &["srgb", "bt2020"], &[]);
        assert!(matches!(
            parse_format("pq-bt2020").unwrap().plan(&sdr_only),
            Err(Unarrangeable::NoAdequateBuffer { .. })
        ));
    }

    #[test]
    fn format_unmanaged_has_no_description() {
        let f = parse_format("unmanaged").unwrap();
        assert!(f.mode().description.is_none());
        assert_eq!(f.buffer_label(), "auto");
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
        let d = f.color_description().unwrap();
        assert_eq!(d.transfer_function, "st2084_pq");
        assert_eq!(d.primaries, "bt2020");
        let m = d.mastering.unwrap();
        assert_eq!(m.max_luminance_nits, 600.0);
        assert_eq!(m.max_cll_nits, 600.0); // defaults to peak
        assert_eq!(m.max_fall_nits, 300.0);
    }

    #[test]
    fn buf_param_pins_the_buffer() {
        let f = parse_format("srgb:buf=xrgb2101010").unwrap();
        assert_eq!(
            f.mode().buffer,
            BufferPolicy::Exact(BufferFormat::Xrgb2101010)
        );
        assert_eq!(f.buffer_label(), "xrgb2101010");
        // The description is unchanged by the buffer pin.
        assert_eq!(f.color_description().unwrap().transfer_function, "srgb");
        assert!(parse_format("srgb:buf=nope").is_err());
    }

    #[test]
    fn format_scrgb_records_protocol_definition() {
        let f = parse_format("scrgb").unwrap();
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
