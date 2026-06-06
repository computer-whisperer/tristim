//! Challenge–response unlock for the i1d3 family.
//!
//! Out of the box the instrument refuses measurement commands until it is
//! unlocked with a response derived from an 8-byte challenge and a 64-bit
//! per-OEM key. The scheme is plain integer arithmetic (no cryptography);
//! the keys and the transform were published by Graeme Gill in ArgyllCMS
//! (`spectro/i1d3.c`) and are re-stated here as wire-format facts.
//!
//! The flow: query lock status (`0x0020`) → request a challenge (`0x9900`)
//! → compute and send the response (`0x9a00`, success = reply byte 2 ==
//! `0x77`) → re-check status. Keys are tried in order until one unlocks.

/// One OEM unlock key and the marketing name of the variant it matches.
#[derive(Debug, Clone, Copy)]
pub struct UnlockKey {
    /// Variant name, for reporting which key worked.
    pub name: &'static str,
    /// The 64-bit key as two 32-bit words.
    pub key: [u32; 2],
}

/// Every known unlock key, in the order ArgyllCMS tries them. The device
/// itself decides which key it accepts — product-name matching is not
/// reliable (most variants report "i1Display3").
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

/// Compute the 64-byte unlock response for a 64-byte challenge reply.
///
/// The device only inspects bytes 24..40 of the response; the rest are left
/// zero (the OEM driver randomizes them, the instrument ignores them). All
/// arithmetic is wrapping 32-bit / 8-bit, matching the device's expectation.
pub fn create_unlock_response(key: [u32; 2], challenge: &[u8; 64]) -> [u8; 64] {
    // 8-byte sub-challenge at offset 35, each byte XORed with challenge[3].
    let mut sc = [0u8; 8];
    for (i, b) in sc.iter_mut().enumerate() {
        *b = challenge[3] ^ challenge[35 + i];
    }

    // Shuffle into two 32-bit words.
    let ci0 = (u32::from(sc[3]) << 24)
        | (u32::from(sc[0]) << 16)
        | (u32::from(sc[4]) << 8)
        | u32::from(sc[6]);
    let ci1 = (u32::from(sc[1]) << 24)
        | (u32::from(sc[7]) << 16)
        | (u32::from(sc[2]) << 8)
        | u32::from(sc[5]);

    let nk0 = key[0].wrapping_neg();
    let nk1 = key[1].wrapping_neg();

    let co = [
        nk0.wrapping_sub(ci1),
        nk1.wrapping_sub(ci0),
        ci1.wrapping_mul(nk0),
        ci0.wrapping_mul(nk1),
    ];

    // Sum of the sub-challenge bytes plus the bytes of both negated keys.
    let mut sum: u32 = sc.iter().map(|&b| u32::from(b)).sum();
    for k in [nk0, nk1] {
        sum += (k & 0xff) + ((k >> 8) & 0xff) + ((k >> 16) & 0xff) + ((k >> 24) & 0xff);
    }
    let s0 = (sum & 0xff) as u8;
    let s1 = ((sum >> 8) & 0xff) as u8;

    // 16 response bytes from the four words and the sum bytes.
    let byte = |w: u32, shift: u32| (w >> shift) as u8;
    let sr: [u8; 16] = [
        byte(co[0], 16).wrapping_add(s0),
        byte(co[2], 8).wrapping_sub(s1),
        byte(co[3], 0).wrapping_add(s1),
        byte(co[1], 16).wrapping_add(s0),
        byte(co[2], 16).wrapping_sub(s1),
        byte(co[3], 16).wrapping_sub(s0),
        byte(co[1], 24).wrapping_sub(s0),
        byte(co[0], 0).wrapping_sub(s1),
        byte(co[3], 8).wrapping_add(s0),
        byte(co[2], 24).wrapping_sub(s1),
        byte(co[0], 8).wrapping_add(s0),
        byte(co[1], 8).wrapping_sub(s1),
        byte(co[1], 0).wrapping_add(s1),
        byte(co[3], 24).wrapping_add(s1),
        byte(co[2], 0).wrapping_add(s0),
        byte(co[0], 24).wrapping_sub(s0),
    ];

    // The actual response: 16 bytes at offset 24, XORed with challenge[2].
    let mut resp = [0u8; 64];
    for (i, &b) in sr.iter().enumerate() {
        resp[24 + i] = challenge[2] ^ b;
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
        let r = create_unlock_response(UNLOCK_KEYS[0].key, &test_challenge());
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
        let r0 = create_unlock_response(UNLOCK_KEYS[0].key, &c);
        let r1 = create_unlock_response(UNLOCK_KEYS[1].key, &c);
        assert_ne!(r0[24..40], r1[24..40]);
    }

    /// Regression lock on the transform: a fixed challenge/key pair must keep
    /// producing these exact bytes. (Computed by this implementation, recorded
    /// to catch accidental edits to the arithmetic — NOT validated against a
    /// real instrument yet.)
    #[test]
    fn known_answer_snapshot() {
        let r = create_unlock_response([0xe962_2e9f, 0x8d63_e133], &test_challenge());
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
