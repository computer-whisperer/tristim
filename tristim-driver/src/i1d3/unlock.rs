//! Challenge–response unlock for the i1d3 family.
//!
//! Out of the box the instrument refuses measurement commands until it is
//! unlocked with a response derived from an 8-byte challenge and a 64-bit
//! per-OEM key. The scheme is plain integer arithmetic (no cryptography);
//! the keys and the transform were published by Graeme Gill in ArgyllCMS
//! (`spectro/i1d3.c`) and are re-stated here as wire-format facts — the
//! device firmware accepts exactly one response per (key, challenge) pair,
//! so any interoperable implementation computes the same function.
//!
//! The flow: query lock status (`0x0020`) → request a challenge (`0x9900`)
//! → compute and send the response (`0x9a00`, success = reply byte 2 ==
//! `0x77`) → re-check status. Keys are tried until one unlocks; the device
//! decides which key it accepts (product-name matching is not reliable —
//! most variants report "i1Display3").

/// One OEM unlock key and the marketing name of the variant it matches.
#[derive(Debug, Clone, Copy)]
pub struct UnlockKey {
    /// Variant name, for reporting which key worked.
    pub name: &'static str,
    /// The 64-bit key as two 32-bit words.
    pub key: [u32; 2],
}

/// Every known OEM unlock key.
pub const UNLOCK_KEYS: &[UnlockKey] = &[
    UnlockKey {
        name: "i1Display Pro",
        key: [0xe962_2e9f, 0x8d63_e133],
    },
    UnlockKey {
        name: "ColorMunki Display",
        key: [0xe01e_6e0a, 0x2574_62de],
    },
    UnlockKey {
        name: "i1Display Pro (OEM)",
        key: [0xcaa6_2b2c, 0x3081_5b61],
    },
    UnlockKey {
        name: "NEC SpectraSensor Pro",
        key: [0xa911_9479, 0x5b16_8761],
    },
    UnlockKey {
        name: "Quato Silver Haze 3",
        key: [0x160e_b6ae, 0x1444_0e70],
    },
    UnlockKey {
        name: "HP DreamColor",
        key: [0x291e_41d7, 0x5193_7bdd],
    },
    UnlockKey {
        name: "Wacom DC",
        key: [0x1abf_ae03, 0xf25a_c8e8],
    },
    UnlockKey {
        name: "Toshiba TPA-1",
        key: [0x828c_43e9, 0xcbb8_a8ed],
    },
    UnlockKey {
        name: "Barco",
        key: [0xe8d1_a980, 0xd146_f7ad],
    },
    UnlockKey {
        name: "PhotoCrysta",
        key: [0x171a_e295, 0x2e5c_7664],
    },
    UnlockKey {
        name: "ViewSonic CS-XRi1",
        key: [0x64d8_c546, 0x4b24_b4a7],
    },
];

/// Order in which the 8 (XOR-decoded) challenge bytes pack into the two
/// 32-bit working words, most-significant byte first.
const PACK: [[usize; 4]; 2] = [[3, 0, 4, 6], [1, 7, 2, 5]];

/// How each of the 16 response bytes is assembled:
/// `(word index, byte-within-word, sum byte index, add?)` — take one byte of
/// one mixed word, then add or subtract one of the two checksum bytes.
const MIX: [(usize, usize, usize, bool); 16] = [
    (0, 2, 0, true),
    (2, 1, 1, false),
    (3, 0, 1, true),
    (1, 2, 0, true),
    (2, 2, 1, false),
    (3, 2, 0, false),
    (1, 3, 0, false),
    (0, 0, 1, false),
    (3, 1, 0, true),
    (2, 3, 1, false),
    (0, 1, 0, true),
    (1, 1, 1, false),
    (1, 0, 1, true),
    (3, 3, 1, true),
    (2, 0, 0, true),
    (0, 3, 0, false),
];

/// Compute the 64-byte unlock response for a 64-byte challenge reply.
///
/// The firmware checks only bytes 24..40 of the response and derives them
/// solely from challenge bytes 2, 3, and 35..43 — everything else is left
/// zero (the OEM driver fills it with random bytes; the instrument ignores
/// it). All arithmetic is wrapping, per the firmware's u32/u8 semantics.
pub fn unlock_response(key: [u32; 2], challenge: &[u8; 64]) -> [u8; 64] {
    // Decode the 8 live challenge bytes (offset 35, XOR-masked with
    // challenge[3]) and pack them into two words per PACK.
    let decoded: Vec<u8> = challenge[35..43].iter().map(|&b| challenge[3] ^ b).collect();
    let ci: Vec<u32> = PACK
        .iter()
        .map(|order| order.iter().fold(0u32, |w, &i| (w << 8) | u32::from(decoded[i])))
        .collect();

    // Mix with the (negated) key words: two differences, two products.
    let nk = [key[0].wrapping_neg(), key[1].wrapping_neg()];
    let mixed = [
        nk[0].wrapping_sub(ci[1]),
        nk[1].wrapping_sub(ci[0]),
        ci[1].wrapping_mul(nk[0]),
        ci[0].wrapping_mul(nk[1]),
    ];

    // Checksum over all 16 input bytes (decoded challenge + negated key),
    // kept as its two low bytes.
    let sum: u32 = decoded
        .iter()
        .copied()
        .chain(nk.iter().flat_map(|k| k.to_le_bytes()))
        .map(u32::from)
        .sum();
    let s = [sum as u8, (sum >> 8) as u8];

    // Assemble the 16 payload bytes per MIX and place them at offset 24,
    // XOR-masked with challenge[2].
    let mut resp = [0u8; 64];
    for (slot, &(word, byte, sb, add)) in resp[24..40].iter_mut().zip(&MIX) {
        let b = (mixed[word] >> (8 * byte)) as u8;
        let b = if add {
            b.wrapping_add(s[sb])
        } else {
            b.wrapping_sub(s[sb])
        };
        *slot = challenge[2] ^ b;
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_challenge() -> [u8; 64] {
        let mut c = [0u8; 64];
        c[2] = 0x5a; // response XOR byte
        c[3] = 0xa5; // sub-challenge XOR byte
        for i in 0..8 {
            c[35 + i] = 0x10 * i as u8 + 3;
        }
        c
    }

    /// Only bytes 24..40 carry the response; everything else must be zero.
    #[test]
    fn response_is_zero_outside_payload() {
        let r = unlock_response(UNLOCK_KEYS[0].key, &test_challenge());
        for (i, &b) in r.iter().enumerate() {
            if !(24..40).contains(&i) {
                assert_eq!(b, 0, "byte {i} not zero");
            }
        }
    }

    /// Different keys must produce different responses to the same challenge
    /// (otherwise the key table would be pointless).
    #[test]
    fn keys_produce_distinct_responses() {
        let c = test_challenge();
        let r0 = unlock_response(UNLOCK_KEYS[0].key, &c);
        let r1 = unlock_response(UNLOCK_KEYS[1].key, &c);
        assert_ne!(r0[24..40], r1[24..40]);
    }

    /// Regression lock on the transform: a fixed challenge/key pair must keep
    /// producing these exact bytes. (Recorded from the original implementation
    /// of this transform; guards refactors of the arithmetic — NOT validated
    /// against a real instrument yet.)
    #[test]
    fn known_answer_snapshot() {
        let r = unlock_response([0xe962_2e9f, 0x8d63_e133], &test_challenge());
        assert_eq!(
            &r[24..40],
            &[
                0xcf, 0xaa, 0xcd, 0x99, 0x16, 0xd6, 0x57, 0x38, 0x94, 0x11, 0x42, 0x75, 0x4a, 0x52,
                0x5e, 0xcb
            ],
            "unlock transform changed — verify against spec before updating"
        );
    }
}
