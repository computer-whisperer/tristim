#!/usr/bin/env python3
"""Round 3 — gamma-aware CTM derivation.

Same as calibrate2.py but accounts for the fact that the CTM is applied to
*gamma-encoded* pixels (no DEGAMMA_LUT on amdgpu CRTC). The panel then EOTFs
the CTM output back to linear light. So the CTM coefficients must be
*pre-encoded* with the panel's OETF for the desired linear-space correction
to land correctly.

Per-panel γ is taken from the spyder analyze tool's log-linear fit on the
grey ramp (V ∈ [0.15, 0.95]). Power-law approximation; good enough at this
slice's precision.
"""

import csv
import sys
from pathlib import Path

import numpy as np

D65_x, D65_y = 0.3127, 0.3290

# Per-panel measured γ (EOTF exponent), from analyze output 2026-05-19
GAMMA = {
    "dp4": 2.34,
    "dp6": 2.34,
    "dp9": 2.33,
    "dp7": 2.19,  # post-OSD-adjust value (s24c230-dp7_2.csv)
    "dp8": 2.21,
}

# Applied CTMs so we can run prediction checks against post-CTM sweeps
APPLIED_CTM = {
    "dp4": [0.975016, 0.934018, 1.000000],
    "dp6": [0.919634, 0.902294, 1.000000],
    "dp9": [0.912822, 0.938701, 1.000000],
    "dp7": [0.763378, 1.000000, 0.736073],
    "dp8": [0.843508, 1.000000, 0.726982],
}


def load_patches(path: Path) -> dict:
    out = {}
    with open(path) as f:
        for row in csv.DictReader(f):
            out[row["name"]] = (float(row["X"]), float(row["Y"]), float(row["Z"]),
                                float(row["x"]), float(row["y"]))
    return out


def xy_to_xyz(x, y, Y):
    return np.array([x / y * Y, Y, (1.0 - x - y) / y * Y])


def panel_matrix(patches: dict) -> np.ndarray:
    R = np.array(patches["red_1000"][:3])
    G = np.array(patches["grn_1000"][:3])
    B = np.array(patches["blu_1000"][:3])
    return np.column_stack([R, G, B])


def derive_gamma_ctm(M_panel: np.ndarray, Y_target: float, gamma: float):
    """Diagonal CTM (gamma-encoded) that lands the panel's white at D65."""
    xyz_target = xy_to_xyz(D65_x, D65_y, Y_target)
    s_linear = np.linalg.solve(M_panel, xyz_target)
    if np.any(s_linear < 0):
        # Out-of-gamut: can't reach target with non-negative channels. Clamp.
        print(f"     WARN: target out of panel gamut, clamping; s_linear={s_linear}")
        s_linear = np.maximum(s_linear, 0)
    s_ctm = s_linear ** (1.0 / gamma)
    # Normalize so max(s_ctm) = 1.0
    s_ctm = s_ctm / np.max(s_ctm)
    s_linear_after_norm = s_ctm ** gamma  # what the panel actually sees in linear
    return s_ctm, s_linear_after_norm


def predict_with_gamma(M_panel, ctm_coeffs, gamma):
    """Given a CTM diagonal and panel γ, predict the resulting XYZ for full white."""
    s_lin = np.array(ctm_coeffs) ** gamma
    return M_panel @ s_lin


def chroma(xyz):
    s = xyz.sum()
    return xyz[0] / s, xyz[1] / s


def fmt9(diag):
    m = np.diag(diag)
    return " ".join(f"{v:+.6f}" for v in m.flatten())


def report(label, panel_key, pre_csv, post_csv=None):
    print(f"\n## {label}  γ={GAMMA[panel_key]}  ({pre_csv.name})")
    patches = load_patches(pre_csv)
    M = panel_matrix(patches)
    W = np.array(patches["grey_1000"][:3])
    Y_meas = W[1]
    print(f"   pre-CTM: xy=({patches['grey_1000'][3]:.4f}, {patches['grey_1000'][4]:.4f})  Y={Y_meas:.2f}")

    # Prediction check on the previously-applied CTM
    if post_csv is not None:
        old_ctm = APPLIED_CTM[panel_key]
        pred = predict_with_gamma(M, old_ctm, GAMMA[panel_key])
        pred_xy = chroma(pred)
        post = load_patches(post_csv)
        meas = np.array(post["grey_1000"][:3])
        meas_xy = chroma(meas)
        err_xy = ((pred_xy[0] - meas_xy[0])**2 + (pred_xy[1] - meas_xy[1])**2)**0.5
        print(f"   PREDICTION CHECK on old CTM diag({old_ctm}):")
        print(f"     gamma-model predicted: xy=({pred_xy[0]:.4f}, {pred_xy[1]:.4f})  Y={pred[1]:.2f}")
        print(f"     actual measurement:    xy=({meas_xy[0]:.4f}, {meas_xy[1]:.4f})  Y={meas[1]:.2f}")
        print(f"     prediction error: Δxy={err_xy*1000:.2f}×10⁻³  ΔY={(pred[1]-meas[1]):.2f} cd/m²")

    # Derive new gamma-aware CTM
    s_ctm, s_lin = derive_gamma_ctm(M, Y_meas, GAMMA[panel_key])
    print(f"   NEW CTM (γ-encoded):")
    print(f"     CTM coefficients: ({s_ctm[0]:.4f}, {s_ctm[1]:.4f}, {s_ctm[2]:.4f})")
    print(f"     panel sees in linear: ({s_lin[0]:.4f}, {s_lin[1]:.4f}, {s_lin[2]:.4f})")
    pred_xyz = M @ s_lin
    pred_xy = chroma(pred_xyz)
    print(f"     predicted post-CTM white: xy=({pred_xy[0]:.4f}, {pred_xy[1]:.4f})  Y={pred_xyz[1]:.2f} cd/m²  "
          f"(cost = {(Y_meas - pred_xyz[1])/Y_meas*100:.1f}%)")
    print(f"     KDL: ctm {fmt9(s_ctm)}")


MEAS = Path("/home/christian/workspace/spyder/measurements/2026-05-19")
ROOT = Path("/home/christian/workspace/spyder")

report("DP-4 (LU28R55)", "dp4", MEAS / "lu28r55s-dp4.csv")
report("DP-6 (LU28R55)", "dp6", MEAS / "lu28r55s-dp6.csv")
report("DP-9 (LU28R55)", "dp9", MEAS / "lu28r55s-dp9.csv", ROOT / "lu28r55s-dp9.csv")
report("DP-7 (S24C230)", "dp7", MEAS / "s24c230-dp7_2.csv", ROOT / "s24c230-dp7.csv")
report("DP-8 (S24C230)", "dp8", MEAS / "s24c230-dp8_2.csv")
