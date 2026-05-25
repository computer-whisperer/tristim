//! The presenter application: state + tree.
//!
//! The app has three modes. **Setup** ([`crate::setup`]) configures and
//! launches an in-process capture run; **Running** shows live progress while
//! `tristim_gather::run_capture` drives the colorimeter on a background thread;
//! **Presenting** is the visualization — a trial selector plus a panel
//! describing the presenter's own display (from the host's
//! [`aetna_core::event::HostDiagnostics`]), beside a content panel that shows,
//! per a view selector, either the [`crate::chart`] chromaticity diagram (with
//! an opt-in color-field backdrop bounded to the presenter's negotiated gamut,
//! and hover-to-inspect that swaps the legend for a per-sample inspector) or
//! the [`crate::luminance`] measured-vs-expected plot.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};

use aetna_core::prelude::*;
use tristim_analyze::{AnalyzedCapture, AnalyzedTrial, GroundTruth, GroundTruthSource, analyze};
use tristim_capture::{self as cap, Capture};
use tristim_color::metrics;
use tristim_gather::{self as gather, CaptureConfig, GatherEvent};

use crate::chart::{PresenterGamut, chromaticity_chart};
use crate::luminance::{luminance_chart, luminance_units};
use crate::plot::Space;
use crate::setup::{CaptureForm, FormAction};
use crate::space3d::{Space3dScene, space_chart, space_legend};

/// Which plot the content panel shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Chromaticity,
    Luminance,
    /// The 3D CIELAB sample space (see [`crate::space3d`]).
    Space3D,
}

/// Top-level app mode.
enum Mode {
    Setup,
    Running,
    Presenting,
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
        self.selected.min(self.analyzed.trials.len().saturating_sub(1))
    }

    /// Ensure the cached 3D scene matches the focused trial, rebuilding its
    /// geometry handles only when the focus moved.
    fn ensure_space3d(&mut self) {
        if self.analyzed.trials.is_empty() {
            self.space3d = None;
            return;
        }
        let i = self.focused();
        if self.space3d.as_ref().map(|s| s.trial) != Some(i) {
            self.space3d = Some(Space3dScene::build(&self.analyzed.trials[i], i));
        }
    }
}

/// A message from the background capture thread to the UI.
enum CaptureMsg {
    Progress(GatherEvent),
    // Boxed: a `Capture` dwarfs a `GatherEvent`, so box it to keep the channel
    // message small (clippy::large_enum_variant).
    Finished(Result<Box<Capture>, String>),
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
            samples: self.samples.clone(),
        }
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
    per_format: usize,
    measured: usize,
    target: usize,
    cancelling: bool,
    /// Completed format trials.
    trials: Vec<cap::FormatTrial>,
    /// The format currently being measured, if any.
    cur: Option<LiveTrial>,
    /// Re-analyzed snapshot of everything measured so far, for the plots.
    live: Option<Presented>,
}

impl Running {
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
                self.measured += 1;
                self.rebuild_live();
            }
            GatherEvent::FormatDone { .. } => {
                if let Some(done) = self.cur.take() {
                    self.trials.push(done.to_trial());
                }
                self.rebuild_live();
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
        },
        output: cap::OutputInfo {
            name: String::new(),
            make: String::new(),
            model: String::new(),
            description: String::new(),
            mode: None,
        },
        capabilities: cap::Capabilities::default(),
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
    /// Last open-file error, shown in the header / setup screen.
    open_error: Option<String>,
    /// Which plot is shown (present mode).
    view: Tab,
    /// Chromaticity projection for the diagram.
    space: Space,
    /// Whether to paint the chromaticity color-field backdrop (opt-in).
    show_field: bool,
    /// Index of the sample currently hovered in the plot, if any.
    hovered_sample: Option<usize>,
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
            open_error: None,
            view: Tab::Chromaticity,
            space: Space::UvPrime,
            show_field: false,
            hovered_sample: None,
        }
    }

    /// Open into the capture-setup form (no capture loaded).
    pub fn setup() -> Self {
        let mut form = CaptureForm::new();
        form.refresh_outputs();
        Self {
            mode: Mode::Setup,
            presented: None,
            form,
            running: None,
            source_path: None,
            open_rx: None,
            open_error: None,
            view: Tab::Chromaticity,
            space: Space::UvPrime,
            show_field: false,
            hovered_sample: None,
        }
    }

    /// Build the app in Running mode over an already-measured `capture`, with
    /// no live thread — for the headless dump to lint the running layout (which
    /// it otherwise can't construct, since `Running` owns a channel receiver).
    pub fn debug_running(capture: Capture) -> Self {
        let analyzed = analyze(&capture);
        let total: usize = capture.trials.iter().map(|t| t.samples.len()).sum();
        let (_tx, rx) = channel();
        let running = Running {
            rx,
            cancel: Arc::new(AtomicBool::new(false)),
            device: Some("Spyder 2024 · SN 87000216".to_string()),
            countdown: None,
            total_formats: capture.trials.len().max(1),
            per_format: capture.trials.first().map_or(0, |t| t.samples.len()),
            measured: total,
            target: total.max(1),
            cancelling: false,
            trials: capture.trials.clone(),
            cur: None,
            live: Some(Presented {
                capture,
                analyzed,
                selected: 0,
                space3d: None,
            }),
        };
        Self {
            mode: Mode::Running,
            presented: None,
            form: CaptureForm::new(),
            running: Some(running),
            source_path: None,
            open_rx: None,
            open_error: None,
            view: Tab::Chromaticity,
            space: Space::UvPrime,
            show_field: false,
            hovered_sample: None,
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
            if let Some(p) = self.presented.as_mut() {
                p.ensure_space3d();
            }
        }
    }

    /// Set the chromaticity projection. Used by the headless dump to render
    /// both spaces.
    pub fn set_space(&mut self, space: Space) {
        self.space = space;
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

    /// Spawn the capture on a background thread and switch to Running. Progress
    /// flows back over a channel drained in [`Self::before_build`].
    fn launch(&mut self, cfg: CaptureConfig) {
        let total_formats = cfg.formats.len();
        let per_format = cfg.sequence.len();
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
            .map_err(|e| e.to_string());
            let _ = tx.send(CaptureMsg::Finished(result));
        });

        self.running = Some(Running {
            rx,
            cancel,
            device: None,
            countdown: None,
            total_formats,
            per_format,
            measured: 0,
            target: total_formats * per_format,
            cancelling: false,
            trials: Vec::new(),
            cur: None,
            live: None,
        });
        self.mode = Mode::Running;
    }

    /// A capture run finished: auto-save and switch to presenting it, or fall
    /// back to setup with the error shown.
    fn finish(&mut self, result: Result<Box<Capture>, String>) {
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
                self.view = Tab::Chromaticity;
                self.hovered_sample = None;
                self.mode = Mode::Presenting;
            }
            Err(e) => {
                self.form.set_error(e);
                self.mode = Mode::Setup;
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
                self.view = Tab::Chromaticity;
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
                        finished = Some(Err("capture thread ended unexpectedly".to_string()));
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

    /// Short label for trial `i`: the requested (or unmanaged) basis + format.
    fn trial_label(&self, p: &Presented, i: usize) -> String {
        let fmt = &p.analyzed.trials[i].pixel_format;
        let basis = match &p.capture.trials[i].requested {
            Some(d) => format!("{}/{}", d.transfer_function, d.primaries),
            None => "unmanaged".to_string(),
        };
        format!("{basis} · {fmt}")
    }

    fn present_header(&self, p: &Presented) -> El {
        let dev = &p.capture.device;
        let out = &p.capture.output;
        let out_label = match &out.mode {
            Some(m) => format!("{} · {}×{}", out.name, m.width, m.height),
            None => out.name.clone(),
        };
        // The wordmark subtitle names the file in focus (bounded, leftmost).
        let subtitle = self
            .source_path
            .as_deref()
            .map(file_name)
            .unwrap_or("color validation presenter");
        row([
            brand(subtitle),
            spacer(),
            fact(
                "device",
                format!(
                    "{} · SN {} · cal {}",
                    dev.product, dev.serial, dev.cal_index
                ),
            ),
            fact("output", out_label),
            fact("captured", p.capture.timestamp.clone()),
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
        items.push(divider());
        items.push(self.display_info(cx));
        column(items).width(Size::Fixed(300.0)).gap(tokens::SPACE_2)
    }

    /// What the presenter's *own* window negotiated — the other side of the
    /// glass from the capture under test.
    fn display_info(&self, cx: &BuildCx) -> El {
        let mut rows: Vec<El> = vec![section_label("Presenter display")];
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
        column(rows).gap(tokens::SPACE_2)
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

        let plot_px = plot_size(cx, extra_chrome);

        // Heading: title + view selector, plus chromaticity-only controls.
        let mut heading: Vec<El> = vec![
            column([
                h3(self.trial_label(p, i)),
                text(ground_truth_line(t)).muted().font_size(13.0),
            ])
            .gap(2.0),
            spacer(),
            view_selector(self.view),
        ];

        // The main plot + its detail card, per view.
        let (plot, detail) = match self.view {
            Tab::Chromaticity => {
                // Color fill is bounded to the presenter's negotiated gamut.
                let gamut = presenter_gamut(cx);
                heading.push(space_toggle(self.space));
                if gamut.is_some() {
                    heading.push(field_toggle(self.show_field));
                }
                let field = if self.show_field { gamut } else { None };
                let detail = match self.hovered_sample {
                    Some(_) => sample_inspector(t, self.hovered_sample),
                    None => chart_legend(self.space),
                };
                (
                    chromaticity_chart(t, self.space, field, plot_px, self.hovered_sample),
                    detail,
                )
            }
            Tab::Luminance => (luminance_chart(t, plot_px), luminance_legend(t)),
            Tab::Space3D => {
                // The geometry handles are cached on `Presented` and refreshed
                // in `before_build`; `None` only during a transient first frame.
                let chart = match &p.space3d {
                    Some(scene) => space_chart(scene, plot_px),
                    None => column([text("Preparing 3D space…").muted()])
                        .width(Size::Fixed(plot_px))
                        .height(Size::Fixed(plot_px)),
                };
                (chart, space_legend())
            }
        };

        column([
            row(heading)
                .gap(tokens::SPACE_2)
                .align(Align::Center)
                .width(Size::Fill(1.0)),
            row([
                plot,
                column([
                    summary_card(t).width(Size::Fill(1.0)),
                    detail.width(Size::Fill(1.0)),
                ])
                .gap(tokens::SPACE_3)
                .width(Size::Fill(1.0)),
            ])
            .gap(tokens::SPACE_4)
            .align(Align::Start)
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0)),
        ])
        .gap(tokens::SPACE_4)
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0))
    }

    fn present_view(&self, p: &Presented, cx: &BuildCx) -> El {
        let mut items = vec![self.present_header(p), divider()];
        if let Some(e) = &self.open_error {
            items.push(open_error_banner(e));
        }
        items.push(
            row([self.sidebar(p, cx), self.content_panel(p, cx, 0.0)])
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
        // Scroll so the form fits any window / output count.
        items.push(scroll([self.form.view().width(Size::Fixed(720.0))]).key("setup-scroll"));
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
        let frac = if r.target == 0 {
            0.0
        } else {
            (r.measured as f32 / r.target as f32).clamp(0.0, 1.0)
        };
        let title = if r.cancelling {
            "Cancelling…"
        } else {
            "Capturing…"
        };
        let detail = if let Some(c) = r.countdown {
            format!("place the puck against the output — starting in {c}s")
        } else if let Some(cur) = &r.cur {
            format!(
                "{} · {} ({}/{}) · patch {}/{}",
                r.device.as_deref().unwrap_or("measuring"),
                cur.token,
                cur.index + 1,
                r.total_formats,
                cur.samples.len(),
                r.per_format,
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
                mono(format!("{}/{} measurements", r.measured, r.target))
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
                Some("field-toggle") => self.show_field = !self.show_field,
                Some("space-toggle") => self.space = self.space.toggled(),
                Some("view:chroma") => self.view = Tab::Chromaticity,
                Some("view:lum") => {
                    self.view = Tab::Luminance;
                    self.hovered_sample = None; // hover is chromaticity-only
                }
                Some("view:space3d") => {
                    self.view = Tab::Space3D;
                    self.hovered_sample = None; // 2D hover is N/A in the 3D view
                }
                Some("new-capture") => {
                    self.form.refresh_outputs();
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

/// Responsive square side (px) for the plot, derived from the window viewport.
/// Grows the diagram to fill the content area — leaving the stat/legend column
/// its minimum width — and is bounded vertically so it always fits on screen.
/// Falls back to a sensible size when no viewport is attached (headless).
fn plot_size(cx: &BuildCx, extra_chrome: f32) -> f32 {
    const ROOT_PAD: f32 = 24.0; // SPACE_6, window padding each side
    const SIDEBAR_W: f32 = 300.0;
    const COL_GAP: f32 = 24.0; // sidebar ↔ content
    const ROW_GAP: f32 = 16.0; // chart ↔ stat column
    const RIGHT_MIN: f32 = 410.0; // keep the stat/legend column readable
    const V_CHROME: f32 = 230.0; // header + heading + paddings/gaps above the chart
    const PLOT_MIN: f32 = 360.0;
    const PLOT_MAX: f32 = 920.0;

    let (vw, vh) = cx.viewport().unwrap_or((1280.0, 800.0));
    let content_w = vw - 2.0 * ROOT_PAD - SIDEBAR_W - COL_GAP;
    let h_budget = content_w - ROW_GAP - RIGHT_MIN;
    let vh = vh - extra_chrome;
    let v_budget = vh - V_CHROME;
    h_budget.min(v_budget).clamp(PLOT_MIN, PLOT_MAX)
}

/// The presenter window's negotiated gamut, mapped from host diagnostics.
/// `None` when no diagnostics are attached or the primaries aren't one we fill.
fn presenter_gamut(cx: &BuildCx) -> Option<PresenterGamut> {
    use aetna_core::color::Primaries;
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
        // Drain background work before laying out the frame.
        self.drain_capture();
        self.drain_open();
        // Keep the 3D scene's cached geometry handles current with the focused
        // trial. Done here (not in `build`, which is `&self`) so the handles
        // persist across frames and the backend re-uploads nothing on orbit.
        if self.view == Tab::Space3D {
            if let Some(p) = self.presented.as_mut() {
                p.ensure_space3d();
            }
            if let Some(live) = self.running.as_mut().and_then(|r| r.live.as_mut()) {
                live.ensure_space3d();
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
        body.padding(tokens::SPACE_6)
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0))
    }

    fn shaders(&self) -> Vec<AppShader> {
        vec![crate::chart::field_shader()]
    }

    fn on_event(&mut self, e: UiEvent) {
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
                    && let FormAction::Start(cfg) = self.form.handle(route)
                {
                    self.launch(cfg);
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

/// Segmented selector for the active view; the current view's button is primary.
fn view_selector(view: Tab) -> El {
    let tab = |label: &str, key: &str, active: bool| {
        let b = button(label).key(key.to_string());
        if active { b.primary() } else { b.secondary() }
    };
    row([
        tab("Chromaticity", "view:chroma", view == Tab::Chromaticity),
        tab("Luminance", "view:lum", view == Tab::Luminance),
        tab("3D Space", "view:space3d", view == Tab::Space3D),
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

/// A `label · value` column used in the header strip.
fn fact(label: &str, value: impl Into<String>) -> El {
    column([
        text(label).muted().font_size(11.0),
        text(value).font_size(13.0),
    ])
    .gap(2.0)
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
    let mut rows = vec![
        stat_row("requested", format!("{r:.3}  {g:.3}  {b:.3}")),
        stat_row("measured xy", fmt_xy(s.measured_xy)),
        stat_row("target xy", fmt_xy(s.expected_xy)),
        stat_row("Δu'v'", fmt_opt(s.delta_uv, 4)),
        stat_row("ΔE*ab", fmt_opt(s.delta_e, 2)),
    ];
    if let Some(l) = s.luminance {
        let unit = if l.absolute { "cd/m²" } else { "× white" };
        rows.push(stat_row(
            "luminance",
            format!("{:.3} vs {:.3} {unit}", l.measured, l.expected),
        ));
    }
    titled_card("Sample", rows).gap(tokens::SPACE_2)
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

    fn sample(y: f64) -> cap::Sample {
        cap::Sample {
            requested: [0.5, 0.5, 0.5],
            measured: cap::Measured {
                raw: [0u16; 6],
                xyz: [0.31 * y, y, 0.34 * y],
                xy: Some([0.31, 0.33]),
            },
            context: cap::SampleContext {
                window_fraction: 1.0,
                border: None,
                settle_ms: 0,
            },
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
            per_format: 3,
            measured: 0,
            target: 3,
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
        assert_eq!(r.measured, 3);

        // Closing the format keeps it presentable (cur folds into trials).
        r.apply(GatherEvent::FormatDone {
            index: 0,
            samples: 3,
        });
        assert!(r.cur.is_none());
        assert_eq!(r.trials.len(), 1);
        assert_eq!(r.live.as_ref().unwrap().analyzed.trials.len(), 1);
    }
}
