//! Color sequences: build the list of `0..=1` code-value triples a capture
//! steps through. Shared by the CLI (`--seq`) and the GUI's capture form.

/// Parse a `--seq` spec (`name[:N]`) into a list of `0..=1` code-value triples.
pub fn parse_sequence(spec: &str) -> Result<Vec<[f64; 3]>, String> {
    let (name, arg) = spec.split_once(':').unwrap_or((spec, ""));
    match name {
        "grey" | "gray" => Ok(grey_ramp(parse_count(arg, 11)?)),
        "primaries" => Ok(primary_ramps(parse_count(arg, 5)?)),
        "scatter" => Ok(scatter(parse_count(arg, 32)?, SCATTER_SEED)),
        other => Err(format!(
            "unknown sequence {other:?} (known: grey, primaries, scatter)"
        )),
    }
}

/// Default scatter seed — fixed so scatter is reproducible across runs.
pub const SCATTER_SEED: u64 = 0x7472_6973_7469_6d01;

/// If `spec` is a `scatter[:N]` spec, return its sample count; otherwise `None`.
/// Scatter is split out from the fixed sequence so it can be generated
/// per-format at capture time — and there, constrained to the measured gamut.
pub fn parse_scatter(spec: &str) -> Result<Option<usize>, String> {
    let (name, arg) = spec.split_once(':').unwrap_or((spec, ""));
    if name == "scatter" {
        Ok(Some(parse_count(arg, 32)?))
    } else {
        Ok(None)
    }
}

fn parse_count(arg: &str, default: usize) -> Result<usize, String> {
    if arg.is_empty() {
        return Ok(default);
    }
    arg.parse()
        .map_err(|_| format!("bad count {arg:?} (expected a positive integer)"))
}

/// N-step grey ramp from 0 to 1 inclusive.
pub fn grey_ramp(n: usize) -> Vec<[f64; 3]> {
    let n = n.max(2);
    (0..n)
        .map(|k| {
            let v = k as f64 / (n - 1) as f64;
            [v, v, v]
        })
        .collect()
}

/// Per-channel R/G/B ramps, `n - 1` steps each from `1/(n-1)` to `1.0`
/// (skipping 0, the shared black already covered by grey ramps).
pub fn primary_ramps(n: usize) -> Vec<[f64; 3]> {
    let n = n.max(2);
    let mut out = Vec::new();
    for ch in 0..3 {
        for k in 1..n {
            let v = k as f64 / (n - 1) as f64;
            let mut rgb = [0.0; 3];
            rgb[ch] = v;
            out.push(rgb);
        }
    }
    out
}

/// `n` uniform code-value triples in `[0, 1)`, deterministic from `seed`
/// (splitmix64) so captures are reproducible and comparable across runs.
pub fn scatter(n: usize, seed: u64) -> Vec<[f64; 3]> {
    scatter_accepted(n, seed, |_| true)
}

/// Draw uniform `[0, 1)^3` candidates from the `seed` stream (splitmix64),
/// keeping those `accept`ed, until `count` are kept or the attempt budget
/// (`count * 100`, min 1000) is exhausted — a tight gamut can reject most.
/// Deterministic for a given seed, so captures stay reproducible. With an
/// accept-all predicate this reproduces [`scatter`] exactly.
pub fn scatter_accepted(
    count: usize,
    seed: u64,
    accept: impl Fn([f64; 3]) -> bool,
) -> Vec<[f64; 3]> {
    let mut state = seed;
    let mut next = || {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        (z >> 11) as f64 / (1u64 << 53) as f64
    };
    let budget = count.saturating_mul(100).max(1000);
    let mut out = Vec::with_capacity(count);
    let mut tries = 0;
    while out.len() < count && tries < budget {
        let p = [next(), next(), next()];
        tries += 1;
        if accept(p) {
            out.push(p);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grey_ramp_spans_zero_to_one() {
        let r = grey_ramp(11);
        assert_eq!(r.len(), 11);
        assert_eq!(r[0], [0.0, 0.0, 0.0]);
        assert_eq!(r[10], [1.0, 1.0, 1.0]);
    }

    #[test]
    fn primary_ramps_cover_three_channels() {
        let r = primary_ramps(5);
        assert_eq!(r.len(), 3 * 4); // (n-1) per channel
        assert_eq!(r[3], [1.0, 0.0, 0.0]); // last red step = full red
        assert_eq!(r[7], [0.0, 1.0, 0.0]); // last green step = full green
    }

    #[test]
    fn scatter_is_deterministic_and_in_range() {
        let a = scatter(16, 0x1234);
        let b = scatter(16, 0x1234);
        assert_eq!(a, b);
        assert_ne!(scatter(16, 0x1234), scatter(16, 0x5678));
        for p in a {
            for c in p {
                assert!((0.0..1.0).contains(&c), "{c} out of range");
            }
        }
    }

    #[test]
    fn scatter_accepted_fills_to_count_with_only_accepted_points() {
        // Reject the half-cube x >= 0.5; we should still get exactly 20 points,
        // all in the accepted half.
        let pts = scatter_accepted(20, 0x99, |p| p[0] < 0.5);
        assert_eq!(pts.len(), 20);
        assert!(pts.iter().all(|p| p[0] < 0.5));
    }

    #[test]
    fn scatter_accepted_all_matches_scatter() {
        assert_eq!(scatter(16, 0x1234), scatter_accepted(16, 0x1234, |_| true));
    }
}
