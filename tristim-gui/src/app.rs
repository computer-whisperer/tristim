//! The presenter application: state + tree.
//!
//! The app has three modes. **Setup** ([`crate::setup`]) configures and
//! launches an in-process capture run; **Running** shows live progress while
//! `tristim_gather::run_capture` drives the colorimeter on a background thread;
//! **Presenting** is the visualization — a trial selector plus a panel
//! describing the presenter's own display (from the host's
//! [`damascene_core::event::HostDiagnostics`]), beside a content panel that shows,
//! per a view selector, either the [`crate::chart`] chromaticity diagram (with
//! an opt-in color-field backdrop bounded to the presenter's negotiated gamut,
//! and hover-to-inspect that swaps the legend for a per-sample inspector) or
//! the [`crate::luminance`] measured-vs-expected plot.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};

use damascene_core::prelude::*;
use tristim_analyze::{AnalyzedCapture, AnalyzedTrial, GroundTruth, GroundTruthSource, analyze};
use tristim_capture::{self as cap, Capture};
use tristim_color::{ColorSpace, mat3_mul_vec, metrics, transfer};
use tristim_gather::{self as gather, CaptureConfig, GatherEvent};

use crate::chart::{PresenterGamut, chromaticity_chart};
use crate::luminance::{luminance_chart, luminance_units};
use crate::plot::Space;
use crate::setup::{CaptureForm, FormAction};
use crate::space3d::{REF_GAMUTS, RefCages, Space3dScene, Space3dView, space_chart, space_legend};

/// Which plot the content panel shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    /// The 3D CIELAB sample space (see [`crate::space3d`]) — first and default.
    Space3D,
    Chromaticity,
    Luminance,
}

/// Top-level app mode.
enum Mode {
    Setup,
    Running,
    Presenting,
}

/// Which moment of a live run [`PresenterApp::debug_running`] freezes. Each is
/// a distinct progress-strip + body layout; the headless dump lints them all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DebugRunPhase {
    /// Puck-placement countdown: nothing measured, waiting-for-samples body.
    Countdown,
    /// The first format's gamut probe in flight (open-ended progress estimate).
    Probing,
    /// Mid-sweep through a format, with earlier formats finished.
    Sweeping,
    /// Cancel requested mid-sweep, waiting for the loop to wind down.
    Cancelling,
    /// Every format measured, the run about to deliver its capture.
    Done,
}

/// A capture being presented: the loaded record, its analysis, and which trial
/// is in focus.
struct Presented {
    capture: Capture,
    analyzed: AnalyzedCapture,
    selected: usize,
    /// Cached 3D-space geometry for the selected trial. Built lazily by
    /// [`Presented::ensure_space3d`] (handles must outlive a single frame, or
    /// the backend re-uploads geometry every frame); `None` until first shown.
    space3d: Option<Space3dScene>,
}

impl Presented {
    fn new(capture: Capture) -> Self {
        let analyzed = analyze(&capture);
        Self {
            capture,
            analyzed,
            selected: 0,
            space3d: None,
        }
    }

    /// Index of the trial in focus, clamped to the available trials.
    fn focused(&self) -> usize {
        self.selected
            .min(self.analyzed.trials.len().saturating_sub(1))
    }

    /// Ensure the cached 3D scene matches the focused trial, enabled reference
    /// gamuts, and measured-shell toggle, rebuilding its handles only when any
    /// moved.
    fn ensure_space3d(&mut self, view: Space3dView, refs: RefCages, show_measured: bool) {
        if self.analyzed.trials.is_empty() {
            self.space3d = None;
            return;
        }
        let i = self.focused();
        if !self
            .space3d
            .as_ref()
            .is_some_and(|s| s.matches(i, view, refs, show_measured))
        {
            let gamut = self.capture.trials.get(i).and_then(|t| t.gamut.as_ref());
            self.space3d = Some(Space3dScene::build(
                &self.analyzed.trials[i],
                i,
                view,
                refs,
                gamut,
                show_measured,
            ));
        }
    }
}

/// A message from the background capture thread to the UI.
enum CaptureMsg {
    Progress(GatherEvent),
    // Boxed: a `Capture` dwarfs a `GatherEvent`, so box it to keep the channel
    // message small (clippy::large_enum_variant).
    Finished(Result<Box<Capture>, RunFailure>),
}

/// Why a capture run (or sensor probe) failed, with enough structure for the
/// form to append actionable guidance.
struct RunFailure {
    message: String,
    /// The error chain bottomed out in the driver's `AccessDenied` — the one
    /// failure with a fix worth spelling out (the udev rule).
    device_access: bool,
}

impl RunFailure {
    fn from_gather(e: &gather::GatherError) -> Self {
        Self {
            message: e.to_string(),
            device_access: matches!(
                e,
                gather::GatherError::Device(tristim_driver::Error::AccessDenied { .. })
            ),
        }
    }

    /// The message, with the udev recipe appended for permission failures.
    /// Indented lines render as commands — see `setup::message_lines`.
    fn guidance(&self) -> String {
        if self.device_access {
            format!("{}\n{UDEV_HINT}", self.message)
        } else {
            self.message.clone()
        }
    }
}

/// What to do about the driver's `AccessDenied`, shaped for `message_lines`
/// (two-space-indented lines render as shell commands).
const UDEV_HINT: &str = "The colorimeter needs a udev rule before non-root users can open it:\n  sudo cp 50-tristim.rules /etc/udev/rules.d/\n  sudo udevadm control --reload\nthen unplug and replug the instrument.";

/// Successful sensor probe: identity line + calibration slots for the form.
struct SensorReport {
    label: String,
    cals: Vec<(u8, String)>,
}

/// The full permission-failure message a real sensor probe produces (the
/// driver's `AccessDenied` text plus the udev recipe), for the headless dump
/// to lint the worst-case multi-line sensor row.
pub fn udev_hint_message() -> String {
    RunFailure {
        message: tristim_driver::Error::AccessDenied {
            vid: 0x085c,
            pid: 0x0a0b,
        }
        .to_string(),
        device_access: true,
    }
    .guidance()
}

/// Result of the background open-file dialog.
enum OpenOutcome {
    /// The dialog was dismissed.
    Cancelled,
    /// A capture loaded from the chosen path (kept for the header).
    Loaded(Box<Capture>, String),
    /// The chosen file failed to load.
    Failed(String),
}

/// Result of the background export save-dialog + write.
enum ExportOutcome {
    /// The save dialog was dismissed.
    Cancelled,
    /// The export was written; `what` describes it for the toast.
    Saved {
        path: String,
        what: String,
    },
    Failed(String),
}

/// A format trial being measured right now: enough to assemble a `FormatTrial`
/// and score it live, before the run finishes.
struct LiveTrial {
    index: usize,
    token: String,
    requested: Option<cap::ColorDescription>,
    pixel_format: String,
    outcome: Option<cap::Negotiation>,
    samples: Vec<cap::Sample>,
}

impl LiveTrial {
    fn to_trial(&self) -> cap::FormatTrial {
        cap::FormatTrial {
            requested: self.requested.clone(),
            pixel_format: self.pixel_format.clone(),
            // Negotiation always precedes samples; default until it arrives.
            outcome: self.outcome.clone().unwrap_or(cap::Negotiation::Unmanaged),
            gamut: None,
            samples: self.samples.clone(),
        }
    }
}

/// A [`LiveTrial`] frozen partway through recorded trial `t`, with its first
/// `k` samples already measured. The gather token isn't recorded in captures,
/// so it's reconstructed from the request — close enough for layout.
fn debug_live_trial(t: &cap::FormatTrial, index: usize, k: usize) -> LiveTrial {
    let token = match &t.requested {
        Some(d) => format!("{}-{}", d.transfer_function, d.primaries),
        None => "unmanaged".to_string(),
    };
    LiveTrial {
        index,
        token,
        requested: t.requested.clone(),
        pixel_format: t.pixel_format.clone(),
        outcome: Some(t.outcome.clone()),
        samples: t.samples[..k.min(t.samples.len())].to_vec(),
    }
}

/// Live state of an in-flight capture, updated from the thread's events. As
/// samples arrive they accumulate into `trials` + the in-progress `cur`, and
/// `live` caches a re-analyzed snapshot so the plots fill in during the run.
struct Running {
    rx: Receiver<CaptureMsg>,
    cancel: Arc<AtomicBool>,
    device: Option<String>,
    countdown: Option<u64>,
    total_formats: usize,
    /// Known sweep measurements per format: the deterministic sequence plus
    /// the per-format scatter count.
    sweep_per_format: usize,
    /// Whether each format runs a gamut probe before its sweep.
    probe_gamut: bool,
    /// Expected probe vertices per format. Starts at the setup preview's
    /// heuristic and is replaced by the latest actual once a probe completes
    /// (the adaptive probe's count isn't known up front).
    probe_est: usize,
    /// Measurements in finished formats — actuals, from `FormatDone`.
    done_measured: usize,
    /// Formats that have finished (`FormatDone` seen).
    formats_done: usize,
    /// Probe vertices measured so far in the current format.
    cur_probe: usize,
    /// Whether the current format's probe finished (`GamutProbed` seen).
    cur_probe_done: bool,
    /// Sweep samples measured so far in the current format.
    cur_sweep: usize,
    cancelling: bool,
    /// Completed format trials.
    trials: Vec<cap::FormatTrial>,
    /// The format currently being measured, if any.
    cur: Option<LiveTrial>,
    /// Re-analyzed snapshot of everything measured so far, for the plots.
    live: Option<Presented>,
}

impl Running {
    /// Measurements taken so far: finished formats' actuals plus the
    /// in-flight format's probe vertices and sweep samples.
    fn measured(&self) -> usize {
        self.done_measured + self.cur_probe + self.cur_sweep
    }

    /// Expected measurements for a format that hasn't started yet.
    fn per_format_est(&self) -> usize {
        self.sweep_per_format + if self.probe_gamut { self.probe_est } else { 0 }
    }

    /// Expected total, refined as the run goes: finished formats count their
    /// actuals, the in-flight format counts what's observed (floored at the
    /// estimate while its probe is still running), and pending formats count
    /// the estimate. The gamut probe's point count is adaptive, so until the
    /// last probe completes this is an estimate that tracks the probe work
    /// instead of ignoring it.
    fn target(&self) -> usize {
        let pending = self.total_formats.saturating_sub(self.formats_done);
        if pending == 0 {
            return self.done_measured;
        }
        let probe_part = if !self.probe_gamut {
            0
        } else if self.cur_probe_done {
            self.cur_probe
        } else {
            self.cur_probe.max(self.probe_est)
        };
        let inflight = probe_part + self.cur_sweep.max(self.sweep_per_format);
        self.done_measured + inflight + (pending - 1) * self.per_format_est()
    }

    /// Fold a progress event into the live state.
    fn apply(&mut self, ev: GatherEvent) {
        match ev {
            GatherEvent::DeviceReady {
                product, serial, ..
            } => self.device = Some(format!("{product} · SN {serial}")),
            GatherEvent::Countdown { remaining } => self.countdown = Some(remaining),
            GatherEvent::FormatStart {
                index,
                token,
                requested,
                pixel_format,
                ..
            } => {
                self.countdown = None;
                if let Some(done) = self.cur.take() {
                    self.trials.push(done.to_trial());
                }
                // Per-format counters reset at `FormatDone`; reset here too in
                // case a format ends without one (defensive, like the fold
                // above).
                self.cur_probe = 0;
                self.cur_probe_done = false;
                self.cur_sweep = 0;
                self.cur = Some(LiveTrial {
                    index,
                    token,
                    requested,
                    pixel_format,
                    outcome: None,
                    samples: Vec::new(),
                });
                self.rebuild_live();
            }
            GatherEvent::Negotiation(n) => {
                if let Some(c) = &mut self.cur {
                    c.outcome = Some(n);
                }
                self.rebuild_live();
            }
            GatherEvent::Sample { sample, .. } => {
                if let Some(c) = &mut self.cur {
                    c.samples.push(sample);
                }
                self.cur_sweep += 1;
                self.rebuild_live();
            }
            // Gamut-probe vertices: fold into the in-progress trial so they
            // plot live, and count them as probe progress (see `target` for
            // how the probe's open-ended count is estimated).
            GatherEvent::ProbeSample { sample, .. } => {
                if let Some(c) = &mut self.cur {
                    c.samples.push(sample);
                }
                self.cur_probe += 1;
                self.rebuild_live();
            }
            GatherEvent::FormatDone { samples, .. } => {
                if let Some(done) = self.cur.take() {
                    self.trials.push(done.to_trial());
                }
                // Fold the format's actuals (probe + sweep, the event counts
                // both) into the finished tally and reset the live counters.
                self.done_measured += samples;
                self.formats_done += 1;
                self.cur_probe = 0;
                self.cur_probe_done = false;
                self.cur_sweep = 0;
                self.rebuild_live();
            }
            // The per-vertex `ProbeSample` events already populate the plots;
            // the summary refines the probe estimate for the formats to come.
            GatherEvent::GamutProbed { vertices, .. } => {
                self.cur_probe_done = true;
                self.probe_est = vertices;
            }
        }
    }

    /// Rebuild the cached `live` snapshot from `trials` + `cur`, with the
    /// in-progress (or most recent) trial in focus.
    fn rebuild_live(&mut self) {
        let mut trials = self.trials.clone();
        let selected = match &self.cur {
            Some(c) => {
                trials.push(c.to_trial());
                trials.len() - 1
            }
            None => trials.len().saturating_sub(1),
        };
        if trials.is_empty() {
            self.live = None;
            return;
        }
        let capture = live_capture(trials);
        let analyzed = analyze(&capture);
        self.live = Some(Presented {
            capture,
            analyzed,
            selected,
            space3d: None,
        });
    }
}

/// A minimal `Capture` over just the measured trials, for live scoring. The
/// device/output/capabilities metadata is irrelevant to `analyze` (it scores
/// from each trial's `requested` + samples), so it's left empty.
fn live_capture(trials: Vec<cap::FormatTrial>) -> Capture {
    Capture {
        schema_version: cap::SCHEMA_VERSION,
        timestamp: String::new(),
        tool: cap::ToolInfo {
            name: "tristim".into(),
            version: String::new(),
            git_revision: None,
        },
        device: cap::DeviceInfo {
            product: String::new(),
            usb_pid: 0,
            serial: String::new(),
            hw_version: (0, 0),
            cal_index: 0,
            calibration: None,
        },
        output: cap::OutputInfo {
            name: String::new(),
            make: String::new(),
            model: String::new(),
            description: String::new(),
            mode: None,
        },
        capabilities: cap::Capabilities::default(),
        compositor: cap::CompositorInfo::default(),
        run: None,
        trials,
    }
}

/// The presenter application.
pub struct PresenterApp {
    mode: Mode,
    /// Present-mode state (a loaded or just-captured record).
    presented: Option<Presented>,
    /// Setup-mode form.
    form: CaptureForm,
    /// Running-mode live state.
    running: Option<Running>,
    /// Path of the capture in focus (auto-save target or opened file), shown
    /// in the present header.
    source_path: Option<String>,
    /// Pending open-file dialog, resolved on a background thread.
    open_rx: Option<Receiver<OpenOutcome>>,
    /// Pending sensor probe (open the colorimeter, read identity + slots,
    /// drop it), resolved on a background thread.
    sensor_rx: Option<Receiver<Result<SensorReport, RunFailure>>>,
    /// A sensor probe should start on the next frame. Deferred to
    /// [`Self::before_build`] rather than spawned where it's requested so
    /// headless dumps — which build views but never run frames — stay free
    /// of USB side effects.
    sensor_probe_pending: bool,
    /// Last open-file error, shown in the header / setup screen.
    open_error: Option<String>,
    /// Export dialog visibility (present mode).
    export_open: bool,
    /// Pending export save-dialog + write, resolved on a background thread.
    export_rx: Option<Receiver<ExportOutcome>>,
    /// Toasts queued for the host (export results); drained per frame.
    toasts: Vec<ToastSpec>,
    /// Which plot is shown (present mode).
    view: Tab,
    /// Chromaticity projection for the diagram.
    space: Space,
    /// Whether to paint the chromaticity color-field backdrop (opt-in).
    show_field: bool,
    /// Index of the sample currently hovered in the plot, if any.
    hovered_sample: Option<usize>,
    /// Which reference-gamut cages are enabled in the 3D view (abs + rel sets).
    space3d_refs: RefCages,
    /// Whether the measured-gamut shell overlay is enabled in the 3D view.
    space3d_show_measured: bool,
    /// Which colour space the 3D view projects samples into.
    space3d_view: Space3dView,
}

impl PresenterApp {
    /// Open straight into presenting a loaded capture (the file-arg path).
    pub fn new(capture: Capture) -> Self {
        Self {
            mode: Mode::Presenting,
            presented: Some(Presented::new(capture)),
            form: CaptureForm::new(),
            running: None,
            source_path: None,
            open_rx: None,
            sensor_rx: None,
            sensor_probe_pending: false,
            export_open: false,
            export_rx: None,
            toasts: Vec::new(),
            open_error: None,
            view: Tab::Space3D,
            space: Space::UvPrime,
            show_field: false,
            hovered_sample: None,
            space3d_refs: RefCages::default(),
            space3d_show_measured: false,
            space3d_view: Space3dView::default(),
        }
    }

    /// Open into the capture-setup form (no capture loaded).
    pub fn setup() -> Self {
        let mut form = CaptureForm::new();
        form.refresh_outputs();
        form.refresh_capabilities();
        Self {
            mode: Mode::Setup,
            presented: None,
            form,
            running: None,
            source_path: None,
            open_rx: None,
            sensor_rx: None,
            sensor_probe_pending: true,
            export_open: false,
            export_rx: None,
            toasts: Vec::new(),
            open_error: None,
            view: Tab::Space3D,
            space: Space::UvPrime,
            show_field: false,
            hovered_sample: None,
            space3d_refs: RefCages::default(),
            space3d_show_measured: false,
            space3d_view: Space3dView::default(),
        }
    }

    /// Build the app in Running mode, frozen at `phase` of a run over
    /// `capture`'s trials, with no live thread — for the headless dump to lint
    /// each running layout (which it otherwise can't construct, since `Running`
    /// owns a channel receiver).
    pub fn debug_running(capture: Capture, phase: DebugRunPhase) -> Self {
        let (_tx, rx) = channel();
        let mut running = Running {
            rx,
            cancel: Arc::new(AtomicBool::new(false)),
            device: Some("Spyder 2024 · SN 87000216".to_string()),
            countdown: None,
            total_formats: capture.trials.len().max(1),
            sweep_per_format: capture.trials.first().map_or(0, |t| t.samples.len()),
            probe_gamut: false,
            probe_est: crate::setup::GAMUT_PROBE_EST_POINTS,
            done_measured: 0,
            formats_done: 0,
            cur_probe: 0,
            cur_probe_done: false,
            cur_sweep: 0,
            cancelling: false,
            trials: Vec::new(),
            cur: None,
            live: None,
        };
        match phase {
            DebugRunPhase::Countdown => running.countdown = Some(5),
            DebugRunPhase::Probing => {
                // The first format's adaptive probe, frozen partway: its
                // vertices plot live and the strip names the probe phase with
                // an open-ended (~) total.
                if let Some(t) = capture.trials.first() {
                    let k = (t.samples.len() / 3).max(1);
                    running.probe_gamut = true;
                    running.cur_probe = k;
                    running.cur = Some(debug_live_trial(t, 0, k));
                }
            }
            DebugRunPhase::Sweeping | DebugRunPhase::Cancelling => {
                // Earlier formats finished, the in-flight one frozen mid-sweep
                // (so the strip shows the patch counter over finished actuals).
                let cur_index = capture.trials.len().saturating_sub(1).min(1);
                running.trials = capture.trials[..cur_index].to_vec();
                running.done_measured = running.trials.iter().map(|t| t.samples.len()).sum();
                running.formats_done = cur_index;
                if let Some(t) = capture.trials.get(cur_index) {
                    let k = (t.samples.len() / 2).max(1);
                    running.cur_sweep = k;
                    running.cur = Some(debug_live_trial(t, cur_index, k));
                }
                running.cancelling = matches!(phase, DebugRunPhase::Cancelling);
            }
            DebugRunPhase::Done => {
                running.done_measured = capture.trials.iter().map(|t| t.samples.len()).sum();
                running.formats_done = capture.trials.len().max(1);
                running.trials = capture.trials;
            }
        }
        running.rebuild_live();
        Self {
            mode: Mode::Running,
            presented: None,
            form: CaptureForm::new(),
            running: Some(running),
            source_path: None,
            open_rx: None,
            sensor_rx: None,
            sensor_probe_pending: false,
            export_open: false,
            export_rx: None,
            toasts: Vec::new(),
            open_error: None,
            // Unlike the live entry points (which default to Space3D), the
            // dump-only running harness starts on a 2D view: the headless lint
            // never calls `before_build`, so the 3D scene would render as the
            // "Preparing…" placeholder. A 2D chart keeps the running-layout
            // lint meaningful.
            view: Tab::Chromaticity,
            space: Space::UvPrime,
            show_field: false,
            hovered_sample: None,
            space3d_refs: RefCages::default(),
            space3d_show_measured: false,
            space3d_view: Space3dView::default(),
        }
    }

    /// Record the path of the loaded capture (shown in the header). Used by the
    /// windowed binary when launched with a file argument.
    pub fn set_source_path(&mut self, path: String) {
        self.source_path = Some(path);
    }

    /// Set the hovered sample. Used by the headless dump to render the inspector.
    pub fn set_hovered_sample(&mut self, i: Option<usize>) {
        self.hovered_sample = i;
    }

    /// Set the active view. Used by the headless dump to render each plot.
    pub fn set_view(&mut self, view: Tab) {
        self.view = view;
        // The 3D view reads cached geometry handles; build them up front so a
        // headless render (which never calls `before_build`) still has a scene.
        if view == Tab::Space3D {
            let s3v = self.space3d_view;
            let refs = self.space3d_refs;
            let show = self.space3d_show_measured;
            if let Some(p) = self.presented.as_mut() {
                p.ensure_space3d(s3v, refs, show);
            }
        }
    }

    /// Set the chromaticity projection. Used by the headless dump to render
    /// both spaces.
    pub fn set_space(&mut self, space: Space) {
        self.space = space;
    }

    /// Set the 3D projection. Used by the headless dump to lint each one's
    /// heading + legend and to build its geometry on real samples. Rebuilds the
    /// cached scene so a headless render (no `before_build`) has the right one.
    pub fn set_space3d_view(&mut self, view: Space3dView) {
        self.space3d_view = view;
        if self.view == Tab::Space3D {
            let refs = self.space3d_refs;
            let show = self.space3d_show_measured;
            if let Some(p) = self.presented.as_mut() {
                p.ensure_space3d(view, refs, show);
            }
        }
    }

    /// Number of trials in the presented capture (0 if none).
    pub fn trial_count(&self) -> usize {
        self.presented
            .as_ref()
            .map_or(0, |p| p.analyzed.trials.len())
    }

    /// Focus trial `i` (clamped to the valid range). Used by the headless
    /// bundle dump to lay out every trial's panel, not just the default one.
    pub fn select(&mut self, i: usize) {
        if let Some(p) = &mut self.presented {
            p.selected = i.min(p.analyzed.trials.len().saturating_sub(1));
        }
    }

    /// Enable/disable the color-field backdrop. Used by the headless dump to
    /// exercise the filled layout.
    pub fn set_show_field(&mut self, on: bool) {
        self.show_field = on;
    }

    /// Open/close the export dialog. Used by the headless dump to lint the
    /// modal's layout.
    pub fn set_export_open(&mut self, on: bool) {
        self.export_open = on;
    }

    /// Enable the setup form's gamut-probe controls. Used by the headless dump
    /// to lint the expanded setup layout.
    pub fn set_setup_probe_gamut(&mut self, on: bool) {
        self.form.set_probe_gamut(on);
    }

    /// Inject an output list into the setup form as `(name, label)` pairs. Used
    /// by the headless dump to lint the two-column output grid (CI has no
    /// compositor, so live enumeration comes back empty there).
    pub fn set_setup_outputs(&mut self, outputs: impl IntoIterator<Item = (String, String)>) {
        self.form.set_outputs(outputs);
    }

    /// Inject a capability set into the setup form. Used by the headless dump to
    /// lint the grayed-out unreachable-format rows without a live compositor.
    pub fn set_setup_capabilities(&mut self, caps: tristim_display::DisplayCapabilities) {
        self.form.set_capabilities(caps);
    }

    /// Inject a successful sensor probe (device line + `(id, name)` calibration
    /// slots, selecting `cal_id`). Used by the headless dump to lint the
    /// sensor row and the widened named-calibration stepper without hardware.
    pub fn set_setup_sensor_found(
        &mut self,
        label: &str,
        cals: impl IntoIterator<Item = (u8, String)>,
        cal_id: u8,
    ) {
        self.form
            .set_sensor_found(label.to_string(), cals.into_iter().collect());
        self.form.select_cal(cal_id);
    }

    /// Inject a failed sensor probe. Used by the headless dump to lint the
    /// in-row failure message (including the multi-line udev-recipe form —
    /// pass [`udev_hint_message`]'s output).
    pub fn set_setup_sensor_failed(&mut self, message: &str) {
        self.form.set_sensor_failed(message.to_string());
    }

    /// Set the 3D overlays (reference cages + measured shell). Used by the
    /// headless dump to build the overlay geometry on real samples and lint
    /// the toggled controls. Rebuilds the cached scene like [`Self::set_view`].
    pub fn set_space3d_overlays(&mut self, refs: RefCages, show_measured: bool) {
        self.space3d_refs = refs;
        self.space3d_show_measured = show_measured;
        if self.view == Tab::Space3D {
            let view = self.space3d_view;
            if let Some(p) = self.presented.as_mut() {
                p.ensure_space3d(view, refs, show_measured);
            }
        }
    }

    /// Set (or clear) the open-file error. Used by the headless dump to lint
    /// the banner in both the setup and presenting layouts.
    pub fn set_open_error(&mut self, msg: Option<String>) {
        self.open_error = msg;
    }

    /// Record a validation error beneath the setup form. Used by the headless
    /// dump to lint the form's error row.
    pub fn set_setup_error(&mut self, msg: String) {
        self.form.set_error(msg);
    }

    /// Spawn the sensor probe: open the colorimeter, read identity +
    /// calibration slots, drop it (releasing the USB interface). The result is
    /// drained in [`Self::before_build`].
    fn spawn_sensor_probe(&mut self) {
        if self.sensor_rx.is_some() {
            return; // a probe is already in flight
        }
        self.form.set_sensor_probing();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let outcome = match tristim_driver::open_any() {
                Ok(dev) => {
                    let info = dev.info();
                    Ok(SensorReport {
                        label: format!(
                            "{} · SN {} · FW {}.{:02}",
                            info.model, info.serial, info.firmware.0, info.firmware.1
                        ),
                        cals: dev
                            .calibrations()
                            .into_iter()
                            .map(|c| (c.id.0, c.name))
                            .collect(),
                    })
                    // `dev` drops here — the interface is free again before
                    // any capture run wants it.
                }
                Err(e) => Err(RunFailure {
                    device_access: matches!(e, tristim_driver::Error::AccessDenied { .. }),
                    message: e.to_string(),
                }),
            };
            let _ = tx.send(outcome);
        });
        self.sensor_rx = Some(rx);
    }

    /// Drain a finished sensor probe into the form. A disconnected channel
    /// (probe thread died without reporting) still clears `sensor_rx` —
    /// otherwise the spawn guard would block every future probe and the form
    /// would show "detecting…" forever.
    fn drain_sensor(&mut self) {
        let outcome = match &self.sensor_rx {
            Some(rx) => match rx.try_recv() {
                Ok(o) => o,
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => Err(RunFailure {
                    message: "sensor probe ended unexpectedly".to_string(),
                    device_access: false,
                }),
            },
            None => return,
        };
        self.sensor_rx = None;
        self.apply_sensor_outcome(outcome);
    }

    fn apply_sensor_outcome(&mut self, outcome: Result<SensorReport, RunFailure>) {
        match outcome {
            Ok(report) => self.form.set_sensor_found(report.label, report.cals),
            Err(f) => self.form.set_sensor_failed(f.guidance()),
        }
    }

    /// Spawn the capture on a background thread and switch to Running. Progress
    /// flows back over a channel drained in [`Self::before_build`].
    fn launch(&mut self, cfg: CaptureConfig) {
        // An in-flight sensor probe holds the device's USB interface; wait it
        // out (it's an open + a few EEPROM reads) so the run's own open can't
        // race it into a spurious "Resource busy". The timeout only guards a
        // wedged probe — then the run proceeds and reports the conflict.
        if let Some(rx) = self.sensor_rx.take()
            && let Ok(outcome) = rx.recv_timeout(std::time::Duration::from_secs(5))
        {
            self.apply_sensor_outcome(outcome);
        }
        let total_formats = cfg.formats.len();
        let sweep_per_format = cfg.sequence.len() + cfg.scatter.as_ref().map_or(0, |s| s.count);
        let probe_gamut = cfg.gamut.is_some();
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_thread = Arc::clone(&cancel);
        let cfg_thread = cfg;
        // Detached: the run owns the device + patch surface for its lifetime.
        std::thread::spawn(move || {
            let txp = tx.clone();
            let result = gather::run_capture(
                &cfg_thread,
                move |ev| {
                    let _ = txp.send(CaptureMsg::Progress(ev));
                },
                || cancel_thread.load(Ordering::Relaxed),
            )
            .map(Box::new)
            .map_err(|e| RunFailure::from_gather(&e));
            let _ = tx.send(CaptureMsg::Finished(result));
        });

        self.running = Some(Running {
            rx,
            cancel,
            device: None,
            countdown: None,
            total_formats,
            sweep_per_format,
            probe_gamut,
            probe_est: crate::setup::GAMUT_PROBE_EST_POINTS,
            done_measured: 0,
            formats_done: 0,
            cur_probe: 0,
            cur_probe_done: false,
            cur_sweep: 0,
            cancelling: false,
            trials: Vec::new(),
            cur: None,
            live: None,
        });
        self.mode = Mode::Running;
    }

    /// A capture run finished: auto-save and switch to presenting it, or fall
    /// back to setup with the error shown.
    fn finish(&mut self, result: Result<Box<Capture>, RunFailure>) {
        self.running = None;
        match result {
            Ok(capture) => {
                let capture = *capture;
                let fname = capture_filename(&capture.timestamp);
                self.source_path = Some(match capture.save(&fname) {
                    Ok(()) => fname,
                    Err(e) => format!("save failed: {e}"),
                });
                self.presented = Some(Presented::new(capture));
                // Land on the 3D space view — the richest summary of a fresh
                // capture. `before_build` has already drained this message, so
                // it builds the scene before this frame lays out.
                self.view = Tab::Space3D;
                self.hovered_sample = None;
                self.mode = Mode::Presenting;
            }
            Err(f) => {
                self.form.set_error(f.guidance());
                self.mode = Mode::Setup;
                // The failed run says something about the sensor (unplugged?
                // permissions?) — refresh the form's sensor row to match.
                self.sensor_probe_pending = true;
            }
        }
    }

    /// Open a native file dialog on a background thread (so the Wayland event
    /// loop keeps servicing while it's up) and load the chosen capture. The
    /// result is drained in [`Self::before_build`].
    fn open_dialog(&mut self) {
        if self.open_rx.is_some() {
            return; // a dialog is already in flight
        }
        self.open_error = None;
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let outcome = match rfd::FileDialog::new()
                .add_filter("tristim capture", &["json"])
                .set_title("Open a tristim capture")
                .pick_file()
            {
                None => OpenOutcome::Cancelled,
                Some(path) => match Capture::load(&path) {
                    Ok(c) => OpenOutcome::Loaded(Box::new(c), path.display().to_string()),
                    Err(e) => OpenOutcome::Failed(format!("{}: {e}", path.display())),
                },
            };
            let _ = tx.send(outcome);
        });
        self.open_rx = Some(rx);
    }

    /// Switch to presenting a freshly opened capture, or record the error.
    fn apply_open(&mut self, outcome: OpenOutcome) {
        match outcome {
            OpenOutcome::Cancelled => {}
            OpenOutcome::Loaded(capture, path) => {
                self.presented = Some(Presented::new(*capture));
                self.source_path = Some(path);
                self.open_error = None;
                // Same default as `finish`: open into the 3D space view.
                self.view = Tab::Space3D;
                self.hovered_sample = None;
                self.mode = Mode::Presenting;
            }
            OpenOutcome::Failed(e) => self.open_error = Some(e),
        }
    }

    /// Drain the capture thread's progress, finishing the run if it completed.
    fn drain_capture(&mut self) {
        if self.running.is_none() {
            return;
        }
        let mut finished = None;
        if let Some(r) = &mut self.running {
            loop {
                match r.rx.try_recv() {
                    Ok(CaptureMsg::Progress(ev)) => r.apply(ev),
                    Ok(CaptureMsg::Finished(res)) => {
                        finished = Some(res);
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        finished = Some(Err(RunFailure {
                            message: "capture thread ended unexpectedly".to_string(),
                            device_access: false,
                        }));
                        break;
                    }
                }
            }
        }
        if let Some(res) = finished {
            self.finish(res);
        }
    }

    /// Drain a pending open-file dialog result.
    fn drain_open(&mut self) {
        let outcome = match &self.open_rx {
            Some(rx) => match rx.try_recv() {
                Ok(o) => o,
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => OpenOutcome::Cancelled,
            },
            None => return,
        };
        self.open_rx = None;
        self.apply_open(outcome);
    }

    /// Kick off an export: render the chosen format from the presented
    /// capture (cheap, on this thread), then run the native save dialog and
    /// the file write on a background thread. The result is drained into a
    /// toast in [`Self::before_build`].
    fn start_export(&mut self, csv: bool) {
        self.export_open = false;
        if self.export_rx.is_some() {
            return; // a dialog is already in flight
        }
        let Some(p) = &self.presented else { return };
        let stem = self
            .source_path
            .as_deref()
            .map(file_name)
            .unwrap_or("capture")
            .trim_end_matches(".json")
            .to_string();
        let (what, suggested, body) = if csv {
            let n: usize = p.capture.trials.iter().map(|t| t.samples.len()).sum();
            (
                format!("{n} samples"),
                format!("{stem}.csv"),
                cap::export::to_csv(&p.capture),
            )
        } else {
            let sel = p.selected.min(p.capture.trials.len().saturating_sub(1));
            match cap::export::trial_to_ti3(&p.capture, sel) {
                Some(body) => (
                    format!(
                        "trial {sel}, {} patches",
                        p.capture.trials[sel].samples.len()
                    ),
                    format!("{stem}-trial{sel}.ti3"),
                    body,
                ),
                None => {
                    self.toasts.push(ToastSpec::error(format!(
                        "can't export trial {sel} as .ti3: no samples to normalize against"
                    )));
                    return;
                }
            }
        };
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let outcome = match rfd::FileDialog::new()
                .set_title("Export measurements")
                .set_file_name(&suggested)
                .save_file()
            {
                None => ExportOutcome::Cancelled,
                Some(path) => match std::fs::write(&path, body) {
                    Ok(()) => ExportOutcome::Saved {
                        path: path.display().to_string(),
                        what,
                    },
                    Err(e) => ExportOutcome::Failed(format!("{}: {e}", path.display())),
                },
            };
            let _ = tx.send(outcome);
        });
        self.export_rx = Some(rx);
    }

    /// Drain a pending export result into a toast.
    fn drain_export(&mut self) {
        let outcome = match &self.export_rx {
            Some(rx) => match rx.try_recv() {
                Ok(o) => o,
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => ExportOutcome::Cancelled,
            },
            None => return,
        };
        self.export_rx = None;
        match outcome {
            ExportOutcome::Cancelled => {}
            ExportOutcome::Saved { path, what } => self
                .toasts
                .push(ToastSpec::success(format!("wrote {path} ({what})"))),
            ExportOutcome::Failed(e) => self
                .toasts
                .push(ToastSpec::error(format!("export failed: {e}"))),
        }
    }

    /// The export-choice modal: the focused trial as CGATS `.ti3` (the ICC
    /// pipeline's measurement input) or the whole capture as one flat CSV.
    fn export_modal(&self, p: &Presented) -> El {
        let sel = p.selected.min(p.capture.trials.len().saturating_sub(1));
        let label = self.trial_label(p, sel);
        let caption = |s: &str| text(s).muted().font_size(12.0).wrap_text().fill_width();
        // No wrapping column: `modal`'s panel is already a stretch-aligned,
        // gapped column at a fixed width.
        modal(
            "export",
            "Export measurements",
            [
                caption("Renders the capture's recorded facts for other tools."),
                button(format!("Trial {sel} ({label}) → .ti3"))
                    .key("export:ti3")
                    .primary(),
                caption(
                    "CGATS display measurement data: ArgyllCMS `colprof` builds an \
                     ICC profile from it; `profcheck` and DisplayCAL read it too.",
                ),
                button("All samples → .csv").key("export:csv").secondary(),
                caption("One flat table of every sample across all trials."),
            ],
        )
    }

    /// Short label for trial `i`: the requested (or unmanaged) basis + format.
    fn trial_label(&self, p: &Presented, i: usize) -> String {
        let fmt = &p.analyzed.trials[i].pixel_format;
        let basis = match &p.capture.trials[i].requested {
            Some(d) => format!("{}/{}", d.transfer_function, d.primaries),
            None => "unmanaged".to_string(),
        };
        format!("{basis} · {fmt}")
    }

    /// Brand + window actions only. The capture's provenance facts live in
    /// the sidebar's [`capture_card`], beside (and cleanly separated from)
    /// the live [`display_info`](Self::display_info) card.
    fn present_header(&self) -> El {
        // The wordmark subtitle names the file in focus (bounded, leftmost).
        let subtitle = self
            .source_path
            .as_deref()
            .map(file_name)
            .unwrap_or("color validation presenter");
        row([
            brand(subtitle),
            spacer(),
            button("Export…").key("export").secondary(),
            button("Open…").key("open").secondary(),
            button("New capture").key("new-capture").secondary(),
        ])
        .gap(tokens::SPACE_4)
        .align(Align::Center)
    }

    fn sidebar(&self, p: &Presented, cx: &BuildCx) -> El {
        let mut items: Vec<El> = vec![section_label("Trials")];
        for i in 0..p.analyzed.trials.len() {
            let b = button(self.trial_label(p, i))
                .key(format!("trial:{i}"))
                .width(Size::Fill(1.0));
            items.push(if i == p.selected {
                b.primary()
            } else {
                b.secondary()
            });
        }
        if p.analyzed.trials.is_empty() {
            items.push(text("(capture has no trials)").muted().font_size(12.0));
        }
        items.push(capture_card(p));
        items.push(self.display_info(cx));
        column(items).width(Size::Fixed(300.0)).gap(tokens::SPACE_2)
    }

    /// What the presenter's *own* window negotiated — live system state, the
    /// other side of the glass from the capture under test. Deliberately a
    /// separate card from [`capture_card`] so current state never reads as
    /// capture provenance.
    fn display_info(&self, cx: &BuildCx) -> El {
        let mut rows: Vec<El> = Vec::new();
        match cx.diagnostics() {
            Some(d) => {
                rows.push(kv_field("backend", d.backend));
                let ws = d.working_color_space;
                rows.push(kv_field(
                    "working space",
                    format!("{:?} / {:?}", ws.primaries, ws.transfer),
                ));
                rows.push(kv_field("color mgmt", format!("{:?}", d.color_management)));
                if let Some(sc) = &d.surface_color {
                    rows.push(kv_field("adapter", sc.adapter.clone()));
                    rows.push(kv_field("swapchain", sc.chosen_format.clone()));
                    let wide = sc.formats.iter().filter(|f| f.wide).count();
                    rows.push(kv_field(
                        "wide formats",
                        format!("{wide}/{}", sc.formats.len()),
                    ));
                }
            }
            None => rows.push(text("(no host diagnostics yet)").muted().font_size(12.0)),
        }
        sidebar_card("Presenter display", rows)
    }

    /// The plot + stats panel for the in-focus trial. `extra_chrome` is added to
    /// the vertical budget reserved above the plot (the running view sits a
    /// progress strip there).
    fn content_panel(&self, p: &Presented, cx: &BuildCx, extra_chrome: f32) -> El {
        if p.analyzed.trials.is_empty() {
            return column([text("Nothing to show.").muted()]).width(Size::Fill(1.0));
        }
        let i = p.selected.min(p.analyzed.trials.len() - 1);
        let t = &p.analyzed.trials[i];

        let layout = content_layout(cx, extra_chrome);
        // Inner plot size: the budgeted square minus the card's padding on both
        // sides, so the enclosing `plot_card` ends up at the budgeted footprint.
        let plot_px = layout.plot_card_px - 2.0 * PLOT_CARD_PAD;

        // Heading: title + the top-level view selector. Per-view controls (which
        // can be many — the 3D view alone has a projection selector, three
        // reference-gamut toggles, and a measured-shell toggle) go on their own
        // full-width row(s) below, so the heading never overflows on a narrow
        // window.
        // The title column yields to the view selector and ellipsizes — trial
        // labels and ground-truth lines are long enough to collide with it on
        // a half-width window.
        let heading: Vec<El> = vec![
            column([
                h3(self.trial_label(p, i)).nowrap_text().ellipsis(),
                text(ground_truth_line(t))
                    .muted()
                    .font_size(13.0)
                    .nowrap_text()
                    .ellipsis(),
            ])
            .gap(2.0)
            .width(Size::Fill(1.0)),
            view_selector(self.view),
        ];
        let mut control_rows: Vec<El> = Vec::new();

        // Which sample (if any) is hovered, by the active view's mechanism: the
        // 2D charts set `hovered_sample` from pointer events over per-sample hit
        // targets; the 3D scene reports its pick through `cx` (mark 0 = the
        // sample cloud; for a scored trial the point index is the sample index).
        // Either way, a hovered sample swaps the legend for the shared
        // per-sample inspector — common to all three views.
        let hovered = match self.view {
            Tab::Space3D => hovered_scene_sample(cx, t),
            _ => self.hovered_sample,
        };

        // The main plot + its tab-specific legend (shown when nothing is hovered).
        let (plot, legend) = match self.view {
            Tab::Space3D => {
                // The 3D controls float over the scene inside the plot card
                // (a hug-sized island in the top-left corner; the scene's
                // content clusters around the centre, so the corner is cheap
                // real estate). This frees the scene to fill the whole card —
                // notably its full width in stacked mode, where full-width
                // control rows above the card used to eat the vertical budget
                // and leave a collapsed square. The measured-gamut shell
                // toggle joins only when this trial was probed.
                let measured = p
                    .capture
                    .trials
                    .get(i)
                    .is_some_and(|t| t.gamut.is_some())
                    .then_some(self.space3d_show_measured);
                // Width inside the card: the full content column when
                // stacked, the budgeted square beside the stat column.
                let inner_w = if layout.stacked {
                    layout.content_w
                } else {
                    layout.plot_card_px
                } - 2.0 * PLOT_CARD_PAD;
                let controls =
                    space3d_controls(inner_w, self.space3d_view, self.space3d_refs, measured);
                // The geometry handles are cached on `Presented` and refreshed
                // in `before_build`; `None` only during a transient first frame.
                let chart = match &p.space3d {
                    Some(scene) => space_chart(scene),
                    None => column([text("Preparing 3D space…").muted()])
                        .width(Size::Fill(1.0))
                        .height(Size::Fill(1.0)),
                };
                // Scene first, controls after: layers paint in order (controls
                // on top) and hit-test in reverse (buttons win over orbit).
                let plot = stack([chart, controls]).height(Size::Fixed(plot_px));
                let plot = if layout.stacked {
                    // Stacked: span the full content width (the card fills it).
                    plot.width(Size::Fill(1.0))
                } else {
                    // Beside the stat column: keep the budgeted square footprint.
                    plot.width(Size::Fixed(plot_px))
                };
                (plot, space_legend(self.space3d_view))
            }
            Tab::Chromaticity => {
                // Color fill is bounded to the presenter's negotiated gamut.
                let gamut = presenter_gamut(cx);
                let mut c = vec![space_toggle(self.space)];
                if gamut.is_some() {
                    c.push(field_toggle(self.show_field));
                }
                control_rows.push(row(c).gap(tokens::SPACE_2).align(Align::Center));
                let field = if self.show_field { gamut } else { None };
                (
                    chromaticity_chart(t, self.space, field, plot_px, hovered),
                    chart_legend(self.space),
                )
            }
            Tab::Luminance => (luminance_chart(t, plot_px, hovered), luminance_legend(t)),
        };

        let detail = match hovered {
            Some(i) => sample_inspector(t, Some(i)),
            None => legend,
        };

        let mut rows: Vec<El> = vec![
            row(heading)
                .gap(tokens::SPACE_2)
                .align(Align::Center)
                .width(Size::Fill(1.0)),
        ];
        // The active view's control rows, full width (the 3D view's float
        // over the plot instead; Luminance has none).
        rows.extend(control_rows.into_iter().map(|r| r.width(Size::Fill(1.0))));
        if layout.stacked {
            // Narrow window (half a 1080p display): the stat/legend column
            // can't keep its minimum width beside the plot, so it stacks
            // beneath instead, each card taking the full content width.
            // The 3D card spans that width too (its plot fills the card);
            // the 2D cards keep hugging their square plots.
            let pc = plot_card(plot);
            rows.push(if self.view == Tab::Space3D {
                pc.width(Size::Fill(1.0))
            } else {
                pc
            });
            rows.push(summary_card(t).width(Size::Fill(1.0)));
            rows.push(detail.width(Size::Fill(1.0)));
        } else {
            rows.push(
                row([
                    plot_card(plot),
                    column([
                        summary_card(t).width(Size::Fill(1.0)),
                        detail.width(Size::Fill(1.0)),
                    ])
                    // SPACE_2, not _3: the column's intrinsic height is the
                    // vertical high-water mark at 800-high windows when the
                    // banner row is present.
                    .gap(tokens::SPACE_2)
                    .width(Size::Fill(1.0)),
                ])
                .gap(tokens::SPACE_4)
                .align(Align::Start)
                .width(Size::Fill(1.0))
                .height(Size::Fill(1.0)),
            );
        }

        column(rows)
            .gap(tokens::SPACE_4)
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0))
    }

    fn present_view(&self, p: &Presented, cx: &BuildCx) -> El {
        let mut items = vec![self.present_header(), divider()];
        // The banner row (and its column gap) eats into the plot's vertical
        // budget like the running view's progress strip does.
        let mut extra_chrome = 0.0;
        if let Some(e) = &self.open_error {
            items.push(open_error_banner(e));
            extra_chrome = 36.0;
        }
        items.push(
            row([self.sidebar(p, cx), self.content_panel(p, cx, extra_chrome)])
                .gap(tokens::SPACE_6)
                .align(Align::Stretch)
                .width(Size::Fill(1.0))
                .height(Size::Fill(1.0)),
        );
        column(items)
            .gap(tokens::SPACE_4)
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0))
    }

    fn setup_view(&self) -> El {
        let mut items = vec![
            row([
                brand("new capture"),
                spacer(),
                button("Open…").key("open").secondary(),
            ])
            .align(Align::Center),
            divider(),
        ];
        if let Some(e) = &self.open_error {
            items.push(open_error_banner(e));
        }
        // Scroll so the form fits any window / output count; the fixed-width
        // card centers in whatever the window gives it.
        items.push(
            scroll([column([self.form.view().width(Size::Fixed(760.0))])
                .align(Align::Center)
                .width(Size::Fill(1.0))])
            .key("setup-scroll"),
        );
        column(items)
            .gap(tokens::SPACE_4)
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0))
    }

    fn running_view(&self, r: &Running, cx: &BuildCx) -> El {
        // Below the brand row + strip, show the live plots filling in as
        // samples arrive; reserve their height in the plot's vertical budget.
        const STRIP_CHROME: f32 = 64.0;
        let body = match &r.live {
            Some(p) => self.content_panel(p, cx, STRIP_CHROME),
            None => column([text("Waiting for the first measurement…")
                .muted()
                .width(Size::Fill(1.0))])
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0)),
        };

        column([
            row([brand("running capture"), spacer()]).align(Align::Center),
            divider(),
            self.progress_strip(r),
            body,
        ])
        .gap(tokens::SPACE_4)
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0))
    }

    /// Compact one-line progress: status/countdown on the left, a progress bar
    /// + measured count + Cancel on the right.
    fn progress_strip(&self, r: &Running) -> El {
        let (measured, target) = (r.measured(), r.target());
        let frac = if target == 0 {
            0.0
        } else {
            (measured as f32 / target as f32).clamp(0.0, 1.0)
        };
        let title = if r.cancelling {
            "Cancelling…"
        } else {
            "Capturing…"
        };
        let detail = if let Some(c) = r.countdown {
            format!("place the puck against the output — starting in {c}s")
        } else if let Some(cur) = &r.cur {
            // The probe runs before the sweep; while it's in flight the patch
            // counter would lie (the probe's total isn't known), so name the
            // phase instead.
            let phase = if r.probe_gamut && !r.cur_probe_done && r.cur_sweep == 0 {
                format!("gamut probe · {} points", r.cur_probe)
            } else {
                format!("patch {}/{}", r.cur_sweep, r.sweep_per_format)
            };
            format!(
                "{} · {} ({}/{}) · {}",
                r.device.as_deref().unwrap_or("measuring"),
                cur.token,
                cur.index + 1,
                r.total_formats,
                phase,
            )
        } else {
            r.device
                .clone()
                .unwrap_or_else(|| "opening colorimeter…".to_string())
        };

        row([
            column([
                text(title).font_size(14.0),
                text(detail).muted().font_size(12.0),
            ])
            .gap(2.0),
            spacer(),
            column([
                progress(frac, tokens::PRIMARY)
                    .width(Size::Fixed(220.0))
                    .height(Size::Fixed(8.0)),
                // Probing makes the target an estimate until the last format's
                // probe has run; flag it so the count doesn't overpromise.
                mono(if r.probe_gamut && r.formats_done < r.total_formats {
                    format!("{measured}/~{target} measurements")
                } else {
                    format!("{measured}/{target} measurements")
                })
                .muted()
                .font_size(11.0),
            ])
            .gap(4.0)
            .align(Align::End),
            button("Cancel").key("cancel").secondary(),
        ])
        .gap(tokens::SPACE_4)
        .align(Align::Center)
        .width(Size::Fill(1.0))
    }

    /// Present-mode event routing (extracted so `on_event` can branch by mode).
    fn on_present_event(&mut self, e: UiEvent) {
        match e.kind {
            UiEventKind::Click | UiEventKind::Activate => match e.route() {
                Some("export") => self.export_open = self.presented.is_some(),
                Some("export:dismiss") => self.export_open = false,
                Some("export:ti3") => self.start_export(false),
                Some("export:csv") => self.start_export(true),
                Some("field-toggle") => self.show_field = !self.show_field,
                Some("space-toggle") => self.space = self.space.toggled(),
                // Clear the hovered sample on any view switch — hit geometry is
                // per-view, so a stale highlight shouldn't carry across tabs.
                Some("view:space3d") => {
                    self.view = Tab::Space3D;
                    self.hovered_sample = None;
                }
                Some("view:chroma") => {
                    self.view = Tab::Chromaticity;
                    self.hovered_sample = None;
                }
                Some("view:lum") => {
                    self.view = Tab::Luminance;
                    self.hovered_sample = None;
                }
                Some("measured-toggle") => {
                    self.space3d_show_measured = !self.space3d_show_measured;
                }
                Some(k) if k.starts_with("s3v:") => {
                    if let Some(v) = space3d_view_from_key(k) {
                        self.space3d_view = v;
                        // Hit geometry is per-projection; drop any stale highlight.
                        self.hovered_sample = None;
                    }
                }
                // `ref:<abs|rel>:<gamut-key>` — toggle one cage in one anchor set.
                Some(k) if k.starts_with("ref:") => {
                    if let Some((kind, gkey)) = k["ref:".len()..].split_once(':')
                        && let Some(i) = REF_GAMUTS.iter().position(|g| g.key == gkey)
                    {
                        match kind {
                            "abs" => self.space3d_refs.abs[i] = !self.space3d_refs.abs[i],
                            "rel" => self.space3d_refs.rel[i] = !self.space3d_refs.rel[i],
                            _ => {}
                        }
                    }
                }
                Some("new-capture") => {
                    self.form.refresh_outputs();
                    self.form.refresh_capabilities();
                    self.sensor_probe_pending = true;
                    self.mode = Mode::Setup;
                }
                Some(k) => {
                    if let Some(i) = k
                        .strip_prefix("trial:")
                        .and_then(|r| r.parse::<usize>().ok())
                        && let Some(p) = &mut self.presented
                        && i < p.analyzed.trials.len()
                    {
                        p.selected = i;
                        self.hovered_sample = None; // sample indices are per-trial
                    }
                }
                None => {}
            },
            UiEventKind::PointerEnter => {
                if let Some(i) = sample_key(e.target_key()) {
                    self.hovered_sample = Some(i);
                }
            }
            UiEventKind::PointerLeave => {
                if let Some(i) = sample_key(e.target_key())
                    && self.hovered_sample == Some(i)
                {
                    self.hovered_sample = None;
                }
            }
            _ => {}
        }
    }
}

/// Inset between the plot card's border and the plot inside it. `content_panel`
/// subtracts it (twice) from the budgeted square so the card's *outer* footprint
/// matches the old bare-plot footprint and the stat column keeps its width.
const PLOT_CARD_PAD: f32 = tokens::SPACE_4;

/// Wrap a plot in damascene's panel card, so all three views share one
/// content-separation surface (CARD fill + border + radius + shadow) instead
/// of a hand-drawn frame. Hugs the plot rather than filling the row width;
/// the stacked 3D view overrides that to span the content width.
fn plot_card(plot: El) -> El {
    card([plot]).width(Size::Hug).padding(PLOT_CARD_PAD)
}

/// The trial-sample index hovered in the 3D scene, from damascene's hover pick
/// (`cx.hovered_scene_point()`, a frame late). `None` unless the cursor is over
/// a point in the sample cloud — mark 0; mark 1 is the gamut name labels, which
/// we ignore. For a scored trial every sample is plotted in order, so the
/// pick's point index *is* the `t.samples` index.
fn hovered_scene_sample(cx: &BuildCx, t: &AnalyzedTrial) -> Option<usize> {
    let pick = cx.hovered_scene_point()?;
    (pick.mark == 0 && pick.point < t.samples.len()).then_some(pick.point)
}

/// How [`PresenterApp::content_panel`] arranges itself at the current viewport.
struct ContentLayout {
    /// Footprint (square side, px) of the plot card, padding included.
    plot_card_px: f32,
    /// Stats and legend stack beneath the plot instead of beside it — a window
    /// at half a 1080p display can't fit the stat column at readable width.
    stacked: bool,
    /// Width (px) of the whole content column (right of the sidebar). The
    /// plot card's width when a view spans it instead of hugging the square.
    content_w: f32,
}

/// Derive the panel arrangement from the window viewport: side-by-side with
/// the plot grown to fill the content area when the stat/legend column keeps
/// its minimum width, stacked otherwise. Bounded vertically so the panel
/// always fits on screen (present mode has no scroll — the 3D view owns the
/// wheel). Falls back to a sensible size when no viewport is attached
/// (headless).
fn content_layout(cx: &BuildCx, extra_chrome: f32) -> ContentLayout {
    const ROOT_PAD: f32 = 24.0; // SPACE_6, window padding each side
    const SIDEBAR_W: f32 = 300.0;
    const COL_GAP: f32 = 24.0; // sidebar ↔ content
    const ROW_GAP: f32 = 16.0; // chart ↔ stat column
    const RIGHT_MIN: f32 = 410.0; // keep the stat/legend column readable
    const V_CHROME: f32 = 238.0; // header + heading + one controls row + gaps
    const PLOT_MIN: f32 = 360.0;
    const PLOT_MAX: f32 = 920.0;
    // Stacked mode: vertical room kept beneath the plot for the summary and
    // legend/inspector cards, and a smaller plot floor (the cards win).
    const STACKED_STATS: f32 = 510.0;
    const STACKED_PLOT_MIN: f32 = 240.0;

    let (vw, vh) = cx.viewport().unwrap_or((1280.0, 800.0));
    let content_w = vw - 2.0 * ROOT_PAD - SIDEBAR_W - COL_GAP;
    let v_budget = vh - extra_chrome - V_CHROME;
    if content_w < PLOT_MIN + ROW_GAP + RIGHT_MIN {
        let plot = (v_budget - STACKED_STATS).min(content_w);
        ContentLayout {
            plot_card_px: plot.clamp(STACKED_PLOT_MIN, PLOT_MAX),
            stacked: true,
            content_w,
        }
    } else {
        let h_budget = content_w - ROW_GAP - RIGHT_MIN;
        ContentLayout {
            plot_card_px: h_budget.min(v_budget).clamp(PLOT_MIN, PLOT_MAX),
            stacked: false,
            content_w,
        }
    }
}

/// The presenter window's negotiated gamut, mapped from host diagnostics.
/// `None` when no diagnostics are attached or the primaries aren't one we fill.
fn presenter_gamut(cx: &BuildCx) -> Option<PresenterGamut> {
    use damascene_core::color::Primaries;
    match cx.diagnostics()?.working_color_space.primaries {
        Primaries::Srgb => Some(PresenterGamut::Srgb),
        Primaries::DisplayP3 => Some(PresenterGamut::DisplayP3),
        Primaries::Bt2020 => Some(PresenterGamut::Bt2020),
        Primaries::AdobeRgb => None,
    }
}

/// Filename for an auto-saved capture, derived from its RFC-3339 timestamp.
fn capture_filename(timestamp: &str) -> String {
    let stamp: String = timestamp
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    format!("capture-{stamp}.json")
}

impl App for PresenterApp {
    fn before_build(&mut self) {
        // Kick a requested sensor probe off here — not where it was requested
        // — so headless dumps (which never run frames) stay USB-free. Gated
        // to setup mode: a probe spawned mid-run would race the capture
        // thread for the device. (A request that misses the gate fires when
        // the app next returns to setup, which is exactly when it's wanted.)
        if self.sensor_probe_pending && matches!(self.mode, Mode::Setup) {
            self.sensor_probe_pending = false;
            self.spawn_sensor_probe();
        }
        // Drain background work before laying out the frame.
        self.drain_capture();
        self.drain_open();
        self.drain_sensor();
        self.drain_export();
        // Keep the 3D scene's cached geometry handles current with the focused
        // trial. Done here (not in `build`, which is `&self`) so the handles
        // persist across frames and the backend re-uploads nothing on orbit.
        if self.view == Tab::Space3D {
            let s3v = self.space3d_view;
            let refs = self.space3d_refs;
            let show = self.space3d_show_measured;
            if let Some(p) = self.presented.as_mut() {
                p.ensure_space3d(s3v, refs, show);
            }
            if let Some(live) = self.running.as_mut().and_then(|r| r.live.as_mut()) {
                live.ensure_space3d(s3v, refs, show);
            }
        }
    }

    fn build(&self, cx: &BuildCx) -> El {
        let body = match self.mode {
            Mode::Setup => self.setup_view(),
            Mode::Running => match &self.running {
                Some(r) => self.running_view(r, cx),
                None => self.setup_view(),
            },
            Mode::Presenting => match &self.presented {
                Some(p) => self.present_view(p, cx),
                None => self.setup_view(),
            },
        };
        let body = body
            .padding(tokens::SPACE_6)
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0));
        // The export dialog floats over the padded page (so its scrim covers
        // the whole window).
        match (&self.mode, &self.presented) {
            (Mode::Presenting, Some(p)) if self.export_open => {
                overlays(body, [Some(self.export_modal(p))])
            }
            _ => body,
        }
    }

    fn drain_toasts(&mut self) -> Vec<ToastSpec> {
        std::mem::take(&mut self.toasts)
    }

    fn shaders(&self) -> Vec<AppShader> {
        vec![crate::chart::field_shader()]
    }

    fn on_event(&mut self, e: UiEvent, _cx: &EventCx) {
        // "Open…" works from both the setup screen and the presenter.
        if matches!(e.kind, UiEventKind::Click | UiEventKind::Activate) && e.route() == Some("open")
        {
            self.open_dialog();
            return;
        }
        match self.mode {
            Mode::Setup => {
                if matches!(e.kind, UiEventKind::Click | UiEventKind::Activate)
                    && let Some(route) = e.route()
                {
                    // Re-detect is the app's (the probe is a thread it owns);
                    // everything else is the form's.
                    if route == "sensor:refresh" {
                        self.sensor_probe_pending = true;
                    } else if let FormAction::Start(cfg) = self.form.handle(route) {
                        self.launch(*cfg);
                    }
                }
            }
            Mode::Running => {
                if matches!(e.kind, UiEventKind::Click | UiEventKind::Activate)
                    && e.route() == Some("cancel")
                {
                    if let Some(r) = &mut self.running {
                        r.cancel.store(true, Ordering::Relaxed);
                        r.cancelling = true;
                    }
                } else {
                    // The live plots are interactive: reuse the present-mode
                    // routing for the view selector, projection/fill toggles,
                    // and hover-to-inspect.
                    self.on_present_event(e);
                }
            }
            Mode::Presenting => self.on_present_event(e),
        }
    }
}

// ── view helpers ────────────────────────────────────────────────────────────

/// The product wordmark + a muted subtitle, shared by every mode's header. The
/// subtitle is bounded + ellipsized so a long filename can't widen the header.
fn brand(subtitle: &str) -> El {
    column([
        h2("tristim"),
        text(subtitle)
            .muted()
            .font_size(12.0)
            .nowrap_text()
            .ellipsis()
            .width(Size::Fixed(220.0)),
    ])
    .gap(2.0)
}

/// The final path component of `path` (for the header subtitle).
fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// Human label for the capture's compositor: the socket-peer process if known,
/// else the desktop hint, with the other in parens when it adds information.
/// `None` when nothing was recorded (e.g. a pre-v2 capture).
fn compositor_label(c: &cap::CompositorInfo) -> Option<String> {
    match (c.process.as_deref(), c.desktop.as_deref()) {
        (Some(p), Some(d)) if !p.eq_ignore_ascii_case(d) => Some(format!("{p} ({d})")),
        (Some(p), _) => Some(p.to_string()),
        (None, Some(d)) => Some(d.to_string()),
        (None, None) => None,
    }
}

/// A full-width wrapping banner for a failed open (wraps, so a long serde
/// message can't overflow the row).
fn open_error_banner(msg: &str) -> El {
    text(format!("couldn't open capture — {msg}"))
        .text_color(tokens::DESTRUCTIVE)
        .font_size(12.0)
        .wrap_text()
        .width(Size::Fill(1.0))
}

fn section_label(s: &str) -> El {
    text(s).muted().font_size(12.0)
}

/// Toggle for the color-field backdrop. Labeled with its current state.
fn field_toggle(on: bool) -> El {
    let b = button(if on {
        "color fill: on"
    } else {
        "color fill: off"
    })
    .key("field-toggle");
    if on { b.primary() } else { b.secondary() }
}

/// Toggle between the two chromaticity projections, labeled with the current.
fn space_toggle(space: Space) -> El {
    button(space.label()).key("space-toggle").secondary()
}

/// The 3D view's floating control island: the projection selector plus the
/// gamut-overlay toggles, stacked over the scene's top-left corner inside the
/// plot card. The overlay toggles split onto more rows as `inner_w` (the
/// width available inside the card) narrows, so the island never overflows
/// the card. The thresholds are this build's measured intrinsic row widths
/// plus slack — the bundle-dump layout lint guards them against drift.
fn space3d_controls(inner_w: f32, view: Space3dView, refs: RefCages, measured: Option<bool>) -> El {
    const FULL_ROW: f32 = 560.0; // abs | rel | measured shell (~536 intrinsic)
    const REF_GROUPS_ROW: f32 = 410.0; // abs | rel (~389 intrinsic)
    let mut rows: Vec<El> = vec![space3d_view_selector(view)];
    if inner_w >= FULL_ROW {
        rows.push(overlays_row(refs, measured));
    } else if inner_w >= REF_GROUPS_ROW {
        rows.push(overlays_row(refs, None));
        if let Some(on) = measured {
            rows.push(measured_toggle(on));
        }
    } else {
        rows.push(ref_group("abs", "abs", refs.abs));
        rows.push(ref_group("rel", "rel", refs.rel));
        if let Some(on) = measured {
            rows.push(measured_toggle(on));
        }
    }
    column(rows)
        .gap(tokens::SPACE_2)
        // Start, not the default Stretch: a bare `measured_toggle` button
        // would otherwise stretch to the widest row's width.
        .align(Align::Start)
        .width(Size::Hug)
        .height(Size::Hug)
}

/// The 3D view's gamut-overlay toggles, on one row: the reference cages in
/// their two anchor groups (absolute — each at its gamut's spec white — and
/// relative — scaled to this trial), plus the measured-gamut shell when this
/// trial was probed (`measured` carries its on/off state, or `None` to omit).
/// Separator-delimited groups, so they don't read as one long toggle strip.
fn overlays_row(refs: RefCages, measured: Option<bool>) -> El {
    // Fixed-height rules: `vertical_separator` fills its row's cross axis,
    // which collapses to zero in a hug-height row.
    let rule = || vertical_separator().height(Size::Fixed(20.0));
    let mut items = vec![
        ref_group("abs", "abs", refs.abs),
        rule(),
        ref_group("rel", "rel", refs.rel),
    ];
    if let Some(on) = measured {
        items.push(rule());
        items.push(measured_toggle(on));
    }
    row(items).gap(tokens::SPACE_3).align(Align::Center)
}

/// One anchor group ("abs" / "rel") of reference-gamut cage toggles: a caption
/// followed by a button per gamut, routed `ref:<kind>:<gamut-key>`. Compact
/// labels — the overlay row carries six of these.
fn ref_group(caption: &str, kind: &str, set: [bool; crate::space3d::N_REF_GAMUTS]) -> El {
    let mut items = vec![text(format!("{caption}:")).muted().font_size(12.0)];
    for (gi, g) in REF_GAMUTS.iter().enumerate() {
        let b = button(g.short).key(format!("ref:{kind}:{}", g.key));
        items.push(if set[gi] { b.primary() } else { b.secondary() });
    }
    row(items).gap(tokens::SPACE_1).align(Align::Center)
}

/// Toggle for the measured-gamut shell overlay in the 3D view; primary when on.
fn measured_toggle(on: bool) -> El {
    let b = button("measured shell").key("measured-toggle");
    if on { b.primary() } else { b.secondary() }
}

/// The four 3D projections, paired with their route keys and button labels —
/// the single source of truth for the selector and the event router.
const SPACE3D_VIEWS: [(Space3dView, &str, &str); 4] = [
    (Space3dView::LabRelative, "s3v:lab", "Lab"),
    (Space3dView::LabAbsolute, "s3v:lababs", "Lab·abs"),
    (Space3dView::XyYNits, "s3v:xyy", "xyY·nits"),
    (Space3dView::ICtCp, "s3v:ictcp", "ICtCp"),
];

/// Route key (`s3v:…`) → projection.
fn space3d_view_from_key(key: &str) -> Option<Space3dView> {
    SPACE3D_VIEWS
        .iter()
        .find(|(_, k, _)| *k == key)
        .map(|(v, _, _)| *v)
}

/// Segmented selector for the 3D projection; the current one's button is primary.
fn space3d_view_selector(current: Space3dView) -> El {
    row(SPACE3D_VIEWS.map(|(v, key, label)| {
        let b = button(label).key(key.to_string());
        if v == current {
            b.primary()
        } else {
            b.secondary()
        }
    }))
    .gap(tokens::SPACE_2)
}

/// Segmented selector for the active view; the current view's button is primary.
fn view_selector(view: Tab) -> El {
    let tab = |label: &str, key: &str, active: bool| {
        let b = button(label).key(key.to_string());
        if active { b.primary() } else { b.secondary() }
    };
    row([
        tab("3D Space", "view:space3d", view == Tab::Space3D),
        tab("Chromaticity", "view:chroma", view == Tab::Chromaticity),
        tab("Luminance", "view:lum", view == Tab::Luminance),
    ])
    .gap(tokens::SPACE_2)
}

fn luminance_legend(t: &AnalyzedTrial) -> El {
    let units = luminance_units(t);
    titled_card(
        "Legend",
        [
            text("dot — a sample's luminance").muted().font_size(12.0),
            text("x — expected · y — measured").muted().font_size(12.0),
            text("diagonal — ideal (measured = expected)")
                .muted()
                .font_size(12.0),
            text("above the line → too bright, below → too dim")
                .muted()
                .font_size(12.0),
            text("color — ΔE*ab, green → red").muted().font_size(12.0),
            text(format!("units: {units}")).muted().font_size(11.0),
        ],
    )
    .gap(tokens::SPACE_2)
}

/// A stacked label/value field for the narrow sidebar: a muted label over a
/// monospaced value bounded to the column width and ellipsized, so arbitrarily
/// long values (adapter names, format strings) can't overrun into the chart.
fn kv_field(label: &str, value: impl Into<String>) -> El {
    column([
        text(label).muted().font_size(11.0),
        mono(value)
            .font_size(12.0)
            .width(Size::Fill(1.0))
            .nowrap_text()
            .ellipsis(),
    ])
    .gap(1.0)
    .width(Size::Fill(1.0))
}

/// The capture's provenance facts, from the capture file alone: instrument,
/// measured output, serving compositor (v2+ files), and timestamp. The live
/// counterpart is [`PresenterApp::display_info`]; the two are separate cards
/// so file facts and current system state never mix.
fn capture_card(p: &Presented) -> El {
    let dev = &p.capture.device;
    let out = &p.capture.output;
    let out_label = match &out.mode {
        Some(m) => format!("{} · {}×{}", out.name, m.width, m.height),
        None => out.name.clone(),
    };
    let mut rows = vec![
        kv_field(
            "device",
            format!(
                "{} · SN {} · cal {}",
                dev.product, dev.serial, dev.cal_index
            ),
        ),
        kv_field("output", out_label),
    ];
    if let Some(comp) = compositor_label(&p.capture.compositor) {
        rows.push(kv_field("compositor", comp));
    }
    rows.push(kv_field("captured", p.capture.timestamp.clone()));
    sidebar_card("Capture", rows)
}

/// A compact card for the sidebar: `card()`'s separation surface with a
/// section-label title, tight padding, and SPACE_1 field gaps (the kv
/// fields' muted-label/bright-value rhythm separates them). The standard
/// `titled_card` header band alone is 64px — two sidebar cards at standard
/// metrics overflow an 800-high window's vertical budget (the bundle-dump
/// lint's `present-error` state is the tightest case).
fn sidebar_card(title: &str, rows: Vec<El>) -> El {
    let mut items = vec![section_label(title)];
    items.extend(rows);
    card([column(items).gap(tokens::SPACE_1).width(Size::Fill(1.0))])
        .padding(tokens::SPACE_4)
        .width(Size::Fill(1.0))
}

/// A fixed-label / monospaced-value row used throughout the stat panels.
fn stat_row(label: &str, value: impl Into<String>) -> El {
    row([
        text(label)
            .muted()
            .font_size(13.0)
            .width(Size::Fixed(130.0)),
        mono(value).font_size(13.0),
    ])
    .gap(tokens::SPACE_3)
    .align(Align::Center)
}

fn stat_row_colored(label: &str, value: impl Into<String>, color: Color) -> El {
    row([
        text(label)
            .muted()
            .font_size(13.0)
            .width(Size::Fixed(130.0)),
        mono(value).font_size(13.0).text_color(color),
    ])
    .gap(tokens::SPACE_3)
    .align(Align::Center)
}

fn summary_card(t: &AnalyzedTrial) -> El {
    match &t.summary {
        Some(s) => titled_card(
            "Reproduction error",
            [
                stat_row("scored samples", format!("{}", s.scored_samples)),
                stat_row_colored(
                    "Δu'v' mean",
                    format!("{:.4}", s.mean_delta_uv),
                    duv_color(s.mean_delta_uv),
                ),
                stat_row_colored(
                    "Δu'v' max",
                    format!("{:.4}  {}", s.max_delta_uv, duv_verdict(s.max_delta_uv)),
                    duv_color(s.max_delta_uv),
                ),
                stat_row("ΔE*ab mean", format!("{:.2}", s.mean_delta_e)),
                stat_row("ΔE*ab max", format!("{:.2}", s.max_delta_e)),
                stat_row("measured white", format!("{:.1} cd/m²", s.measured_white_y)),
                white_point_row(t),
            ],
        ),
        None => titled_card(
            "Reproduction error",
            [text("No scored samples for this trial.").muted()],
        ),
    }
}

fn white_point_row(t: &AnalyzedTrial) -> El {
    match measured_white(t) {
        Some((xy, cct)) => {
            let cct_s = cct.map_or_else(|| "—".to_string(), |k| format!("{k:.0} K"));
            stat_row(
                "white point",
                format!("({:.4}, {:.4}) · {cct_s}", xy[0], xy[1]),
            )
        }
        None => stat_row("white point", "—"),
    }
}

/// The measured white = brightest measured patch; its chromaticity + CCT.
fn measured_white(t: &AnalyzedTrial) -> Option<([f64; 2], Option<f64>)> {
    let brightest = t
        .samples
        .iter()
        .max_by(|a, b| a.measured_xyz[1].total_cmp(&b.measured_xyz[1]))?;
    let xy = brightest.measured_xy?;
    Some((xy, metrics::cct_mccamy(xy)))
}

/// Parse a `sample:{i}` route/target key into its index.
fn sample_key(key: Option<&str>) -> Option<usize> {
    key?.strip_prefix("sample:")?.parse().ok()
}

fn fmt_xy(xy: Option<[f64; 2]>) -> String {
    match xy {
        Some([x, y]) => format!("({x:.4}, {y:.4})"),
        None => "—".to_string(),
    }
}

fn fmt_opt(v: Option<f64>, prec: usize) -> String {
    match v {
        Some(v) => format!("{v:.prec$}"),
        None => "—".to_string(),
    }
}

/// Render an absolute-XYZ colour as an (encoded-sRGB `Color`, out-of-gamut?)
/// pair, normalised to the reference white. The swatch is painted in sRGB —
/// the viewer's working colour space — so `true` means the colour lay outside
/// sRGB and the swatch is a clamped approximation, not the real colour.
fn swatch_color(xyz: [f64; 3], white_y: f64) -> (Color, bool) {
    let s = if white_y > 0.0 { 1.0 / white_y } else { 1.0 };
    let lin = mat3_mul_vec(
        &ColorSpace::SRGB.xyz_to_rgb(),
        &[xyz[0] * s, xyz[1] * s, xyz[2] * s],
    );
    let approx = lin.iter().any(|&c| !(-1e-3..=1.001).contains(&c));
    let enc = |c: f64| (transfer::srgb_oetf(c.clamp(0.0, 1.0)) * 255.0).round() as u8;
    (
        Color::srgb_u8(enc(lin[0]), enc(lin[1]), enc(lin[2])),
        approx,
    )
}

/// A labelled colour chip. When `approx`, a warning-tinted border and a small
/// `≈` on the label flag it as a clamped (out-of-gamut) approximation.
fn swatch(label: &str, color: Color, approx: bool) -> El {
    let chip = El::default()
        .width(Size::Fixed(64.0))
        .height(Size::Fixed(28.0))
        .fill(color)
        .stroke(if approx {
            tokens::WARNING
        } else {
            tokens::BORDER
        })
        .radius(tokens::RADIUS_SM)
        // The fill is a measured/expected colour, not a theme token — that's
        // the point of a swatch, so opt out of the raw-colour lint here.
        .allow_lint(FindingKind::RawColor);
    let caption = if approx {
        row([
            text(label).font_size(12.0),
            text("≈").font_size(12.0).text_color(tokens::WARNING),
        ])
        .gap(4.0)
        .align(Align::Center)
    } else {
        text(label).muted().font_size(12.0)
    };
    column([chip, caption]).gap(4.0).align(Align::Center)
}

/// Inspector for the hovered sample: what was sent, what was measured, and the
/// error from the target. Shows a hint when nothing is hovered.
fn sample_inspector(t: &AnalyzedTrial, hovered: Option<usize>) -> El {
    let Some(s) = hovered.and_then(|i| t.samples.get(i)) else {
        return titled_card(
            "Sample",
            [text("hover a point to inspect it").muted().font_size(12.0)],
        )
        .gap(tokens::SPACE_2);
    };
    let [r, g, b] = s.requested;
    let mut body: Vec<El> = Vec::new();

    // Asked-vs-got colour swatches, when there's a luminance reference to
    // normalise against (a scored trial). Each is painted in the viewer's sRGB
    // space; a warning border marks any that had to be clamped into gamut.
    let white_y = t.reference_white_xyz.map_or(0.0, |w| w[1]);
    if white_y > 0.0 {
        let expected = s.expected_xyz.map(|xyz| swatch_color(xyz, white_y));
        let (measured, m_approx) = swatch_color(s.measured_xyz, white_y);
        let mut chips = Vec::new();
        if let Some((color, approx)) = expected {
            chips.push(swatch("expected", color, approx));
        }
        chips.push(swatch("measured", measured, m_approx));
        body.push(row(chips).gap(tokens::SPACE_4).align(Align::Start));
        if m_approx || expected.is_some_and(|(_, a)| a) {
            body.push(
                text("≈ approximated — outside the viewer's sRGB gamut")
                    .muted()
                    .font_size(11.0)
                    .wrap_text(),
            );
        }
    }

    body.push(stat_row("requested", format!("{r:.3}  {g:.3}  {b:.3}")));
    body.push(stat_row("measured xy", fmt_xy(s.measured_xy)));
    body.push(stat_row("target xy", fmt_xy(s.expected_xy)));
    body.push(stat_row("Δu'v'", fmt_opt(s.delta_uv, 4)));
    body.push(stat_row("ΔE*ab", fmt_opt(s.delta_e, 2)));
    if let Some(l) = s.luminance {
        let unit = if l.absolute { "cd/m²" } else { "× white" };
        body.push(stat_row(
            "luminance",
            format!("{:.3} vs {:.3} {unit}", l.measured, l.expected),
        ));
    }
    titled_card("Sample", body).gap(tokens::SPACE_2)
}

fn chart_legend(space: Space) -> El {
    titled_card(
        "Legend",
        [
            text("dot — measured chromaticity").muted().font_size(12.0),
            text("line — error from the target").muted().font_size(12.0),
            text("triangle — target gamut").muted().font_size(12.0),
            text("ring — target white point").muted().font_size(12.0),
            text("color — ΔE*ab, green → red").muted().font_size(12.0),
            text(format!("{} diagram", space.label()))
                .muted()
                .font_size(11.0),
        ],
    )
    .gap(tokens::SPACE_2)
}

fn ground_truth_line(t: &AnalyzedTrial) -> String {
    match &t.ground_truth {
        GroundTruth::Known {
            transfer,
            source,
            absolute,
            ..
        } => {
            let src = match source {
                GroundTruthSource::Negotiated => "negotiated",
                GroundTruthSource::AssumedSrgb => "assumed sRGB",
            };
            let abs = if *absolute { " · absolute (PQ)" } else { "" };
            format!("ground truth: {transfer} — {src}{abs}")
        }
        GroundTruth::Unscored { reason } => format!("unscored — {reason}"),
    }
}

/// Δu'v' verdict, matching the `tristim report` CLI thresholds.
fn duv_verdict(max: f64) -> &'static str {
    if max < 0.005 {
        "(imperceptible)"
    } else if max < 0.015 {
        "(perceptible)"
    } else if max < 0.030 {
        "(obvious)"
    } else {
        "(severe)"
    }
}

fn duv_color(v: f64) -> Color {
    if v < 0.005 {
        tokens::SUCCESS
    } else if v < 0.015 {
        tokens::FOREGROUND
    } else if v < 0.030 {
        tokens::WARNING
    } else {
        tokens::DESTRUCTIVE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-sRGB-gamut colours render faithfully (no warning); colours outside
    /// sRGB are flagged as clamped approximations.
    #[test]
    fn swatch_color_flags_out_of_gamut() {
        // sRGB white is in gamut → not an approximation.
        let white = ColorSpace::SRGB.white_xyz();
        assert!(!swatch_color(white, 1.0).1);
        // The BT.2020 green primary is far outside sRGB → approximated.
        let bt2020_green = tristim_color::chromaticity_to_xyz([0.170, 0.797]);
        assert!(swatch_color(bt2020_green, 1.0).1);
    }

    /// Compositor label: peer process wins, falls back to the desktop hint,
    /// shows both when they differ, and is absent when nothing is recorded.
    #[test]
    fn compositor_label_prefers_process() {
        let with = |p: Option<&str>, d: Option<&str>| {
            compositor_label(&cap::CompositorInfo {
                process: p.map(str::to_string),
                desktop: d.map(str::to_string),
                globals: vec![],
            })
        };
        assert_eq!(with(Some("niri"), Some("niri")).as_deref(), Some("niri"));
        assert_eq!(
            with(Some("kwin_wayland"), Some("KDE")).as_deref(),
            Some("kwin_wayland (KDE)")
        );
        assert_eq!(with(None, Some("GNOME")).as_deref(), Some("GNOME"));
        assert_eq!(with(None, None), None);
    }

    fn sample(y: f64) -> cap::Sample {
        cap::Sample {
            requested: [0.5, 0.5, 0.5],
            measured: cap::Measured {
                raw: Vec::new(),
                xyz: [0.31 * y, y, 0.34 * y],
                xy: Some([0.31, 0.33]),
            },
            context: cap::SampleContext {
                window_fraction: 1.0,
                border: None,
                settle_ms: 0,
            },
            source: cap::SampleSource::Sweep,
            repeats: 1,
            adaptive_tier: None,
            elapsed_ms: None,
        }
    }

    fn empty_running() -> Running {
        let (_tx, rx) = channel();
        Running {
            rx,
            cancel: Arc::new(AtomicBool::new(false)),
            device: None,
            countdown: None,
            total_formats: 1,
            sweep_per_format: 3,
            probe_gamut: false,
            probe_est: 0,
            done_measured: 0,
            formats_done: 0,
            cur_probe: 0,
            cur_probe_done: false,
            cur_sweep: 0,
            cancelling: false,
            trials: Vec::new(),
            cur: None,
            live: None,
        }
    }

    /// Progress events accumulate into a scored, presentable live snapshot.
    #[test]
    fn live_snapshot_accumulates_and_scores() {
        let mut r = empty_running();
        r.apply(GatherEvent::DeviceReady {
            product: "Spyder 2024".into(),
            serial: "x".into(),
            hw_version: (6, 0),
        });
        // No trial has started, so nothing to show yet.
        assert!(r.live.is_none());

        r.apply(GatherEvent::FormatStart {
            index: 0,
            total: 1,
            token: "srgb".into(),
            pixel_format: "xrgb8888".into(),
            requested: Some(cap::ColorDescription {
                transfer_function: "srgb".into(),
                render_intent: "perceptual".into(),
                primaries: "srgb".into(),
                reference_white_nits: None,
                mastering: None,
            }),
        });
        r.apply(GatherEvent::Negotiation(cap::Negotiation::Unmanaged));
        for i in 0..3 {
            r.apply(GatherEvent::Sample {
                format_index: 0,
                index: i,
                total: 3,
                sample: sample(50.0 + i as f64),
            });
        }

        let live = r.live.as_ref().expect("live snapshot present mid-run");
        assert_eq!(live.analyzed.trials.len(), 1);
        assert_eq!(live.capture.trials[0].samples.len(), 3);
        assert_eq!(live.selected, 0);
        assert_eq!(r.measured(), 3);
        assert_eq!(r.target(), 3);

        // Closing the format keeps it presentable (cur folds into trials).
        r.apply(GatherEvent::FormatDone {
            index: 0,
            samples: 3,
        });
        assert!(r.cur.is_none());
        assert_eq!(r.trials.len(), 1);
        assert_eq!(r.live.as_ref().unwrap().analyzed.trials.len(), 1);
        // All formats done: the target is exactly what was measured.
        assert_eq!(r.measured(), 3);
        assert_eq!(r.target(), 3);
    }

    /// Gamut-probe samples plot live (land in the trial) and advance the
    /// progress counter against an estimated probe target that reconciles to
    /// the actual once the probe finishes.
    #[test]
    fn probe_samples_count_against_estimated_target() {
        let mut r = empty_running();
        r.probe_gamut = true;
        r.probe_est = 5;
        r.apply(GatherEvent::FormatStart {
            index: 0,
            total: 1,
            token: "srgb".into(),
            pixel_format: "xrgb8888".into(),
            requested: None,
        });
        // Estimated probe (5) + known sweep (3).
        assert_eq!(r.target(), 8);

        let mut probe = sample(80.0);
        probe.source = cap::SampleSource::GamutProbe;
        probe.repeats = 8;
        for _ in 0..2 {
            r.apply(GatherEvent::ProbeSample {
                format_index: 0,
                sample: probe.clone(),
            });
        }
        // Probe vertices advance the counter; the target holds its estimate.
        assert_eq!(r.measured(), 2);
        assert_eq!(r.target(), 8);
        // Both probe vertices are in the plot data.
        let live = r.live.as_ref().expect("live snapshot present");
        assert_eq!(live.capture.trials[0].samples.len(), 2);

        // The probe converges early (2 vertices): the target reconciles.
        r.apply(GatherEvent::GamutProbed {
            index: 0,
            vertices: 2,
            folds: 0,
        });
        assert_eq!(r.target(), 5);

        r.apply(GatherEvent::Sample {
            format_index: 0,
            index: 0,
            total: 3,
            sample: sample(50.0),
        });
        assert_eq!(r.measured(), 3);
        assert_eq!(r.target(), 5);
    }

    /// A finished probe's actual vertex count becomes the estimate for the
    /// formats still pending, and an in-flight probe that overshoots the
    /// estimate grows the target rather than overflowing it.
    #[test]
    fn probe_actuals_refine_pending_estimates() {
        let mut r = empty_running();
        r.total_formats = 2;
        r.probe_gamut = true;
        r.probe_est = 5;
        // Two formats, each estimated probe (5) + sweep (3).
        assert_eq!(r.target(), 16);

        r.apply(GatherEvent::FormatStart {
            index: 0,
            total: 2,
            token: "srgb".into(),
            pixel_format: "xrgb8888".into(),
            requested: None,
        });
        let mut probe = sample(80.0);
        probe.source = cap::SampleSource::GamutProbe;
        for _ in 0..9 {
            r.apply(GatherEvent::ProbeSample {
                format_index: 0,
                sample: probe.clone(),
            });
        }
        // 9 observed > 5 estimated: the in-flight format grows the target
        // (9 + 3) while the pending one keeps the old estimate (5 + 3).
        assert_eq!(r.measured(), 9);
        assert_eq!(r.target(), 20);

        r.apply(GatherEvent::GamutProbed {
            index: 0,
            vertices: 9,
            folds: 0,
        });
        // The actual (9) is now the pending format's estimate too.
        assert_eq!(r.target(), (9 + 3) + (9 + 3));

        r.apply(GatherEvent::FormatDone {
            index: 0,
            samples: 9, // probe vertices only; the sweep was cancelled
        });
        // The finished format contributes its actuals (9, no sweep), the
        // pending one the refined estimate.
        assert_eq!(r.measured(), 9);
        assert_eq!(r.target(), 9 + (9 + 3));
    }
}
