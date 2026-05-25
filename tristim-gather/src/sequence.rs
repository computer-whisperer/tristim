//! Color sequences: build the list of `0..=1` code-value triples a capture
//! steps through. Shared by the CLI (`--seq`) and the GUI's capture form.

/// Parse a `--seq` spec (`name[:N]`) into a list of `0..=1` code-value triples.
pub fn parse_sequence(spec: &str) -> Result<Vec<[f64; 3]>, String> {
    let (name, arg) = spec.split_once(':').unwrap_or((spec, ""));
    match name {
        "grey" | "gray" => Ok(grey_ramp(parse_count(arg, 11)?)),
        "primaries" => Ok(primary_ramps(parse_count(arg, 5)?)),
        // Fixed seed → reproducible scatter across runs.
        "scatter" => Ok(scatter(parse_count(arg, 32)?, 0x7472_6973_7469_6d01)),
        other => Err(format!(
            "unknown sequence {other:?} (known: grey, primaries, scatter)"
        )),
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
    let mut state = seed;
    let mut next = || {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        (z >> 11) as f64 / (1u64 << 53) as f64
    };
    (0..n).map(|_| [next(), next(), next()]).collect()
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
}
