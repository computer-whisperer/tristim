//! Color-format specs: parse a `name[:k=v,...]` token into the buffer format
//! and parametric description to negotiate. Shared by the CLI (`--format`)
//! and the GUI's capture form, so the set of known formats lives in one place.

use std::collections::HashMap;

use tristim_capture as cap;
use tristim_display::{self as display, BufferFormat, DescriptionRequest};

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
        match self.buffer_format {
            BufferFormat::Xrgb8888 => "xrgb8888",
            BufferFormat::Xbgr16161616f => "xbgr16161616f",
        }
    }

    pub(crate) fn buffer_format(&self) -> BufferFormat {
        self.buffer_format
    }

    pub(crate) fn description(&self) -> Option<DescriptionRequest> {
        self.description.clone()
    }

    /// Check whether this format can be reproduced on a compositor with the
    /// given [`DisplayCapabilities`]. `Ok(())` means reachable; the `Err`
    /// carries the first unmet requirement (buffer format, color-management
    /// protocol, transfer function, or primaries) for display.
    pub fn reachability(&self, caps: &display::DisplayCapabilities) -> Result<(), Unreachable> {
        if !caps.supports_buffer_format(self.buffer_format) {
            return Err(Unreachable::BufferFormat(self.buffer_format));
        }
        if let Some(d) = &self.description {
            if !caps.has_color_management() {
                return Err(Unreachable::NoColorManagement);
            }
            if !caps.supports_transfer_function(&d.transfer_function) {
                return Err(Unreachable::TransferFunction(d.transfer_function.clone()));
            }
            if !caps.supports_primaries(&d.primaries) {
                return Err(Unreachable::Primaries(d.primaries.clone()));
            }
        }
        Ok(())
    }

    /// The capture-schema description mirroring what we requested.
    pub(crate) fn color_description(&self) -> Option<cap::ColorDescription> {
        self.description.as_ref().map(|d| cap::ColorDescription {
            transfer_function: d.transfer_function.clone(),
            primaries: d.primaries.clone(),
            reference_white_nits: d.luminances.map(|l| l.reference_nits),
            mastering: d.mastering.map(|m| cap::Mastering {
                min_luminance_nits: m.min_nits,
                max_luminance_nits: m.max_nits,
                max_cll_nits: m.max_cll_nits,
                max_fall_nits: m.max_fall_nits,
            }),
        })
    }
}

/// Why a format can't be reached on the current compositor. See
/// [`FormatSpec::reachability`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unreachable {
    /// The compositor doesn't advertise the required `wl_shm` buffer format
    /// (in practice, the fp16 buffer for HDR / wide encodings).
    BufferFormat(BufferFormat),
    /// The format needs a color description but the compositor exposes no
    /// `wp_color_manager_v1`.
    NoColorManagement,
    /// The transfer function isn't in the compositor's advertised set.
    TransferFunction(String),
    /// The primaries aren't in the compositor's advertised set.
    Primaries(String),
}

impl Unreachable {
    /// A short reason suitable for a chip next to a disabled format toggle.
    pub fn reason(&self) -> String {
        match self {
            Unreachable::BufferFormat(BufferFormat::Xbgr16161616f) => "no fp16 buffer".to_string(),
            Unreachable::BufferFormat(BufferFormat::Xrgb8888) => "no 8-bit buffer".to_string(),
            Unreachable::NoColorManagement => "no color management".to_string(),
            Unreachable::TransferFunction(tf) => format!("TF {tf} unsupported"),
            Unreachable::Primaries(p) => format!("primaries {p} unsupported"),
        }
    }
}

/// The set of format tokens this build understands, in menu order. Each is a
/// valid prefix for [`parse_format`] (the PQ ones also accept `:k=v` params).
pub const KNOWN_FORMATS: &[&str] = &["unmanaged", "srgb", "srgb-p3", "pq-bt2020", "pq-p3"];

/// Parse a `--format` spec (`name[:k=v,...]`) into a [`FormatSpec`].
pub fn parse_format(spec: &str) -> Result<FormatSpec, String> {
    let (name, params_str) = spec.split_once(':').unwrap_or((spec, ""));
    let params = parse_params(params_str)?;
    let token = spec.to_string();

    let mk_mastering = |default_peak: f64| {
        let peak = params.get("peak").copied().unwrap_or(default_peak);
        display::Mastering {
            min_nits: params.get("min").copied().unwrap_or(0.0005),
            max_nits: peak,
            max_cll_nits: params.get("maxcll").copied().unwrap_or(peak),
            max_fall_nits: params.get("maxfall").copied().unwrap_or(peak / 2.0),
        }
    };
    let managed = |bf, tf: &str, prim: &str, mastering| FormatSpec {
        token: token.clone(),
        buffer_format: bf,
        description: Some(DescriptionRequest {
            transfer_function: tf.to_string(),
            primaries: prim.to_string(),
            luminances: None,
            mastering,
        }),
    };

    Ok(match name {
        "unmanaged" => FormatSpec {
            token: token.clone(),
            buffer_format: BufferFormat::Xrgb8888,
            description: None,
        },
        "srgb" => managed(BufferFormat::Xrgb8888, "srgb", "srgb", None),
        "srgb-p3" => managed(BufferFormat::Xrgb8888, "srgb", "display_p3", None),
        "pq-bt2020" => managed(
            BufferFormat::Xbgr16161616f,
            "st2084_pq",
            "bt2020",
            Some(mk_mastering(400.0)),
        ),
        "pq-p3" => managed(
            BufferFormat::Xbgr16161616f,
            "st2084_pq",
            "display_p3",
            Some(mk_mastering(400.0)),
        ),
        other => {
            return Err(format!(
                "unknown format {other:?} (known: unmanaged, srgb, srgb-p3, pq-bt2020, pq-p3)"
            ));
        }
    })
}

fn parse_params(s: &str) -> Result<HashMap<String, f64>, String> {
    let mut m = HashMap::new();
    if s.is_empty() {
        return Ok(m);
    }
    for kv in s.split(',') {
        let (k, v) = kv
            .split_once('=')
            .ok_or_else(|| format!("bad param {kv:?} (expected key=value)"))?;
        let val: f64 = v
            .parse()
            .map_err(|_| format!("bad number in param {kv:?}"))?;
        m.insert(k.to_string(), val);
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tristim_display::DisplayCapabilities;

    #[test]
    fn reachability_gates_on_capabilities() {
        // niri-like: no color management, only the 8-bit buffer.
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
            true,
        );
        for tok in KNOWN_FORMATS {
            assert!(
                parse_format(tok).unwrap().reachability(&rich).is_ok(),
                "{tok} should be reachable on a full compositor"
            );
        }

        // Same, but without display_p3 primaries → the P3 formats trip.
        let no_p3 =
            DisplayCapabilities::advertising(&["srgb", "st2084_pq"], &["srgb", "bt2020"], true);
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

        // fp16 absent but color management present → only the PQ formats trip,
        // and on the buffer.
        let sdr_cm = DisplayCapabilities::advertising(&["srgb"], &["srgb", "display_p3"], false);
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
        let d = f.description.unwrap();
        assert_eq!(d.transfer_function, "srgb");
        assert_eq!(d.primaries, "srgb");
        assert!(d.mastering.is_none());
    }

    #[test]
    fn format_pq_params_override_mastering() {
        let f = parse_format("pq-bt2020:peak=600,maxfall=300").unwrap();
        assert_eq!(f.pixel_format_str(), "xbgr16161616f");
        let d = f.description.unwrap();
        assert_eq!(d.transfer_function, "st2084_pq");
        assert_eq!(d.primaries, "bt2020");
        let m = d.mastering.unwrap();
        assert_eq!(m.max_nits, 600.0);
        assert_eq!(m.max_cll_nits, 600.0); // defaults to peak
        assert_eq!(m.max_fall_nits, 300.0);
    }

    #[test]
    fn format_unknown_errors() {
        assert!(parse_format("nope").is_err());
        assert!(parse_format("pq-bt2020:peak=x").is_err());
    }
}
