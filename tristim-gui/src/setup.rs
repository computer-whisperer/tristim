//! The capture-setup form: configure and launch an in-process capture run.
//!
//! Everything routes through keyed buttons + `−/+` steppers (the same
//! pattern the presenter uses for trial/view toggles), so the form needs no
//! stateful damascene widgets. [`CaptureForm::build_config`] turns the form state
//! into a [`CaptureConfig`] for `tristim_gather::run_capture`, reusing the same
//! `parse_format` / `parse_sequence` the CLI uses (so e.g. scatter draws from
//! the identical seed).

use std::time::Duration;

use damascene_core::prelude::*;
use tristim_display::{self as display, list_outputs};
use tristim_gather::{self as gather, CaptureConfig, KNOWN_FORMATS};

/// Rough adaptive gamut-probe point count per format. The probe's real count
/// isn't known up front (it subdivides where the gamut surface curves), so this
/// seeds both the setup preview's time estimate and the live progress target
/// until the first probe reports its actual.
pub(crate) const GAMUT_PROBE_EST_POINTS: usize = 25;

/// Fast-tier integration for the run's adaptive measurements (gamut-probe
/// vertices and sweep patches alike), in ms. Bright points read ~3× faster at
/// 200 ms and escalate to the calibration default only when the fast read
/// isn't trustworthy; a device without an exposure knob falls back to
/// full-integration reads.
const FAST_INTEGRATION_MS: u16 = 200;

/// What a routed form event asks the app to do.
pub enum FormAction {
    /// Nothing beyond the in-form state change already applied.
    None,
    /// Launch a capture with this validated config. Boxed — `CaptureConfig`
    /// dwarfs the `None` variant (clippy::large_enum_variant).
    Start(Box<CaptureConfig>),
}

struct OutputItem {
    name: String,
    label: String,
}

struct FormatToggle {
    token: &'static str,
    on: bool,
}

struct SeqItem {
    name: &'static str,
    on: bool,
    count: usize,
}

/// Capture-setup form state.
pub struct CaptureForm {
    outputs: Vec<OutputItem>,
    selected_output: Option<usize>,
    formats: Vec<FormatToggle>,
    seqs: Vec<SeqItem>,
    cal_index: u8,
    settle_ms: u64,
    prep_secs: u64,
    window_pct: u32,
    /// Probe each format's reproduced gamut before its sweep, recording it and
    /// constraining scatter to it.
    probe_gamut: bool,
    /// Repeated measurements per gamut-probe point.
    gamut_repeats: usize,
    /// Adaptive-refinement depth cap for the gamut probe.
    gamut_max_depth: u32,
    /// What the compositor can fulfil, queried up front. `None` until
    /// [`Self::refresh_capabilities`] succeeds; while `None`, all formats are
    /// offered (we don't know better yet).
    capabilities: Option<display::DisplayCapabilities>,
    /// Last validation / enumeration error, shown beneath the form.
    error: Option<String>,
}

impl Default for CaptureForm {
    fn default() -> Self {
        Self::new()
    }
}

impl CaptureForm {
    /// Build the form with default selections. Outputs are *not* enumerated
    /// here (that's a Wayland roundtrip) — call [`Self::refresh_outputs`] when
    /// entering the setup screen.
    pub fn new() -> Self {
        Self {
            outputs: Vec::new(),
            selected_output: None,
            formats: KNOWN_FORMATS
                .iter()
                .map(|&token| FormatToggle {
                    token,
                    on: matches!(token, "unmanaged" | "srgb"),
                })
                .collect(),
            seqs: vec![
                SeqItem {
                    name: "grey",
                    on: true,
                    count: 11,
                },
                SeqItem {
                    name: "primaries",
                    on: true,
                    count: 5,
                },
                SeqItem {
                    name: "scatter",
                    on: true,
                    count: 32,
                },
            ],
            cal_index: 0,
            settle_ms: 250,
            prep_secs: 6,
            window_pct: 100,
            probe_gamut: false,
            gamut_repeats: 4,
            gamut_max_depth: 3,
            capabilities: None,
            error: None,
        }
    }

    /// Record a validation/run error to show beneath the form.
    pub fn set_error(&mut self, msg: String) {
        self.error = Some(msg);
    }

    /// Enable/disable the gamut-probe controls. Used by the headless dump to
    /// lint the expanded setup layout (the repeats/depth steppers row).
    pub fn set_probe_gamut(&mut self, on: bool) {
        self.probe_gamut = on;
    }

    /// Inject a capability set directly (the headless dump uses this to lint
    /// the grayed-out, unreachable-format rows without a live compositor).
    pub fn set_capabilities(&mut self, caps: display::DisplayCapabilities) {
        self.apply_capabilities(caps);
    }

    /// Inject an output list directly (the headless dump uses this to lint the
    /// two-column output grid without a live compositor — CI has none, so
    /// enumeration always comes back empty there).
    pub fn set_outputs(&mut self, outputs: impl IntoIterator<Item = (String, String)>) {
        self.outputs = outputs
            .into_iter()
            .map(|(name, label)| OutputItem { name, label })
            .collect();
        self.selected_output = (!self.outputs.is_empty()).then_some(0);
    }

    /// Re-query what the compositor can fulfil (a quick Wayland roundtrip, like
    /// [`Self::refresh_outputs`]). Formats the compositor can't reach are then
    /// shown disabled; any already-selected unreachable format is turned off.
    pub fn refresh_capabilities(&mut self) {
        match display::query_capabilities() {
            Ok(caps) => self.apply_capabilities(caps),
            Err(e) => self.error = Some(format!("capability query failed: {e}")),
        }
    }

    /// Store capabilities and force off any selected format they don't cover.
    fn apply_capabilities(&mut self, caps: display::DisplayCapabilities) {
        for f in &mut self.formats {
            if f.on {
                if let Ok(spec) = gather::parse_format(f.token) {
                    if spec.reachability(&caps).is_err() {
                        f.on = false;
                    }
                }
            }
        }
        self.capabilities = Some(caps);
    }

    /// Why `token` can't be reached on the current compositor, or `None` if it's
    /// reachable (or capabilities haven't been queried yet, so we don't gate).
    fn unreachable(&self, token: &str) -> Option<gather::Unreachable> {
        let caps = self.capabilities.as_ref()?;
        gather::parse_format(token).ok()?.reachability(caps).err()
    }

    /// Re-enumerate the compositor's outputs (a quick Wayland roundtrip).
    pub fn refresh_outputs(&mut self) {
        match list_outputs() {
            Ok(outs) => {
                self.outputs = outs
                    .into_iter()
                    .map(|o| {
                        let dims = o.size.map(|(w, h)| format!(" {w}×{h}")).unwrap_or_default();
                        let base = if o.model.is_empty() {
                            o.name.clone()
                        } else {
                            format!("{} · {}", o.name, o.model)
                        };
                        OutputItem {
                            name: o.name,
                            label: format!("{base}{dims}"),
                        }
                    })
                    .collect();
                // Keep a valid selection: default to the first output.
                match self.selected_output {
                    Some(i) if i >= self.outputs.len() => {
                        self.selected_output = (!self.outputs.is_empty()).then_some(0);
                    }
                    None if !self.outputs.is_empty() => self.selected_output = Some(0),
                    _ => {}
                }
            }
            Err(e) => self.error = Some(format!("output enumeration failed: {e}")),
        }
    }

    /// Apply a routed click. Returns [`FormAction::Start`] only for a valid
    /// `start`; on an invalid start the error is stored and shown in-form.
    pub fn handle(&mut self, route: &str) -> FormAction {
        if let Some(i) = route.strip_prefix("out:").and_then(|r| r.parse().ok()) {
            if i < self.outputs.len() {
                self.selected_output = Some(i);
            }
        } else if route == "out-refresh" {
            self.error = None;
            self.refresh_outputs();
        } else if let Some(tok) = route.strip_prefix("fmt:") {
            // Unreachable formats render as static labels, but guard the route
            // too so a stale click can't select one.
            if self.unreachable(tok).is_none() {
                if let Some(f) = self.formats.iter_mut().find(|f| f.token == tok) {
                    f.on = !f.on;
                }
            }
        } else if let Some(name) = route.strip_prefix("seq:") {
            if let Some(s) = self.seqs.iter_mut().find(|s| s.name == name) {
                s.on = !s.on;
            }
        } else if let Some(name) = route.strip_prefix("seqinc:") {
            if let Some(s) = self.seqs.iter_mut().find(|s| s.name == name) {
                s.count = (s.count + 1).min(256);
            }
        } else if let Some(name) = route.strip_prefix("seqdec:") {
            if let Some(s) = self.seqs.iter_mut().find(|s| s.name == name) {
                s.count = s.count.saturating_sub(1).max(2);
            }
        } else {
            match route {
                "settle:inc" => self.settle_ms = (self.settle_ms + 50).min(5_000),
                "settle:dec" => self.settle_ms = self.settle_ms.saturating_sub(50).max(50),
                "prep:inc" => self.prep_secs = (self.prep_secs + 1).min(120),
                "prep:dec" => self.prep_secs = self.prep_secs.saturating_sub(1),
                "window:inc" => self.window_pct = (self.window_pct + 5).min(100),
                "window:dec" => self.window_pct = self.window_pct.saturating_sub(5).max(5),
                "cal:inc" => self.cal_index = (self.cal_index + 1).min(15),
                "cal:dec" => self.cal_index = self.cal_index.saturating_sub(1),
                "gamut:toggle" => self.probe_gamut = !self.probe_gamut,
                "gamut-rep:inc" => self.gamut_repeats = (self.gamut_repeats + 1).min(64),
                "gamut-rep:dec" => self.gamut_repeats = self.gamut_repeats.saturating_sub(1).max(1),
                "gamut-depth:inc" => self.gamut_max_depth = (self.gamut_max_depth + 1).min(5),
                "gamut-depth:dec" => self.gamut_max_depth = self.gamut_max_depth.saturating_sub(1),
                "start" => match self.build_config() {
                    Ok(cfg) => {
                        self.error = None;
                        return FormAction::Start(Box::new(cfg));
                    }
                    Err(e) => self.error = Some(e),
                },
                _ => {}
            }
        }
        FormAction::None
    }

    /// Turn the current form state into a runnable config, or an error message.
    pub fn build_config(&self) -> Result<CaptureConfig, String> {
        let output = self
            .selected_output
            .and_then(|i| self.outputs.get(i))
            .map(|o| o.name.clone())
            .ok_or("select an output to measure")?;

        // Defensive: an unreachable format should already be off (the UI
        // disables it), but never hand one to a capture — it would only fail
        // at negotiation.
        if let Some(f) = self
            .formats
            .iter()
            .filter(|f| f.on)
            .find(|f| self.unreachable(f.token).is_some())
        {
            let why = self.unreachable(f.token).unwrap().reason();
            return Err(format!("format {} is unreachable here: {why}", f.token));
        }

        let formats = self
            .formats
            .iter()
            .filter(|f| f.on)
            .map(|f| gather::parse_format(f.token))
            .collect::<Result<Vec<_>, _>>()?;
        if formats.is_empty() {
            return Err("enable at least one format".into());
        }

        // Grey/primaries are the fixed sequence; scatter is split into a request
        // so it's generated per-format — and, when probing, constrained to the
        // measured gamut (matching the CLI).
        let mut sequence = Vec::new();
        let mut scatter_count = 0;
        for s in &self.seqs {
            if !s.on {
                continue;
            }
            if s.name == "scatter" {
                scatter_count += s.count;
            } else {
                sequence.extend(gather::parse_sequence(&format!("{}:{}", s.name, s.count))?);
            }
        }
        if sequence.is_empty() && scatter_count == 0 {
            return Err("enable at least one sequence".into());
        }
        let scatter = (scatter_count > 0).then_some(gather::ScatterRequest {
            count: scatter_count,
            seed: gather::SCATTER_SEED,
        });

        let gamut = self.probe_gamut.then(|| gather::GamutProbeOpts {
            repeats: self.gamut_repeats,
            refine: gather::RefineParams {
                max_depth: self.gamut_max_depth,
                ..Default::default()
            },
        });

        Ok(CaptureConfig {
            output,
            cal_index: self.cal_index,
            settle: Duration::from_millis(self.settle_ms),
            fast_integration_ms: Some(FAST_INTEGRATION_MS),
            prep: Duration::from_secs(self.prep_secs),
            window_fraction: self.window_pct as f64 / 100.0,
            border: None,
            formats,
            sequence,
            scatter,
            gamut,
        })
    }

    /// (total sweep measurements, rough estimated seconds) for the current
    /// selection. The gamut probe's points aren't sweep samples, but their time
    /// is folded into the estimate.
    fn preview(&self) -> (usize, u64) {
        let fmts = self.formats.iter().filter(|f| f.on).count();
        let seq_len: usize = self
            .seqs
            .iter()
            .filter(|s| s.on)
            .map(|s| {
                if s.name == "scatter" {
                    s.count
                } else {
                    gather::parse_sequence(&format!("{}:{}", s.name, s.count))
                        .map(|v| v.len())
                        .unwrap_or(0)
                }
            })
            .sum();
        let count = fmts * seq_len;
        // Per-patch cost ≈ settle + one adaptive fast-tier read (~0.25 s at
        // the 200 ms integration; the occasional dim patch escalates).
        let per = self.settle_ms as f64 / 1000.0 + 0.25;
        // Gamut probe: roughly ~25 adaptive points per format, each a burst of
        // `repeats` fast-tier reads (~0.25 s at the 200 ms adaptive
        // integration; the occasional dim point escalates) after the settle.
        let gamut_secs = if self.probe_gamut {
            (fmts * GAMUT_PROBE_EST_POINTS) as f64
                * (self.settle_ms as f64 / 1000.0 + self.gamut_repeats as f64 * 0.25)
        } else {
            0.0
        };
        let est = self.prep_secs + (count as f64 * per) as u64 + gamut_secs as u64;
        (count, est)
    }

    /// The setup form El.
    pub fn view(&self) -> El {
        let (count, est) = self.preview();
        let plan = if count == 0 {
            "nothing selected".to_string()
        } else {
            let probe = if self.probe_gamut {
                " + gamut probe"
            } else {
                ""
            };
            format!("{count} measurements{probe} · ~{}", fmt_dur(est))
        };

        let mut rows = vec![
            text(
                "The patch fills the selected output; run this window on another \
                 display to watch progress while the puck reads it.",
            )
            .muted()
            .font_size(12.0)
            .wrap_text(),
            field("Output", self.outputs_view()),
            field("Formats", self.formats_view()),
            field("Sequences", self.seqs_view()),
            divider(),
            // The four numeric settings pair up into a 2×2 grid — each cell is
            // a `field` filling half the row, so the second column's labels
            // align down the card's midline.
            row([
                field(
                    "Settle",
                    stepper(format!("{} ms", self.settle_ms), "settle:dec", "settle:inc"),
                ),
                field(
                    "Prep",
                    stepper(format!("{} s", self.prep_secs), "prep:dec", "prep:inc"),
                ),
            ])
            .gap(tokens::SPACE_3)
            .width(Size::Fill(1.0)),
            row([
                field(
                    "Window",
                    stepper(format!("{} %", self.window_pct), "window:dec", "window:inc"),
                ),
                field(
                    "Cal index",
                    stepper(format!("{}", self.cal_index), "cal:dec", "cal:inc"),
                ),
            ])
            .gap(tokens::SPACE_3)
            .width(Size::Fill(1.0)),
            field("Gamut probe", self.gamut_view()),
            divider(),
            row([
                text(plan).muted().font_size(12.0),
                spacer(),
                button("Start capture").key("start").primary(),
            ])
            .align(Align::Center)
            .width(Size::Fill(1.0)),
        ];
        if let Some(e) = &self.error {
            rows.push(text(e).font_size(12.0).text_color(tokens::DESTRUCTIVE));
        }

        // Gap the body ourselves: `titled_card` drops the rows into
        // `card_content`, which sets no default gap, so chaining `.gap()` on the
        // card would only space the header from the content — not the fields
        // from each other (they'd stack flush, steppers touching).
        titled_card(
            "New capture",
            [column(rows).gap(tokens::SPACE_3).width(Size::Fill(1.0))],
        )
    }

    fn outputs_view(&self) -> El {
        // A two-column grid: output labels are short enough (`name · model
        // W×H`) to pair up, and the output list is usually the tallest section
        // of the form. Labels stay honest because each cell still fills half
        // the field width.
        let buttons: Vec<El> = self
            .outputs
            .iter()
            .enumerate()
            .map(|(i, o)| {
                let b = button(o.label.clone())
                    .key(format!("out:{i}"))
                    .ellipsis() // model names are unbounded; the cell is not
                    .width(Size::Fill(1.0));
                if Some(i) == self.selected_output {
                    b.primary()
                } else {
                    b.secondary()
                }
            })
            .collect();
        let mut items: Vec<El> = Vec::new();
        let mut iter = buttons.into_iter();
        while let Some(a) = iter.next() {
            let second = iter.next().unwrap_or_else(spacer);
            items.push(
                row([a, second])
                    .gap(tokens::SPACE_2)
                    .width(Size::Fill(1.0)),
            );
        }
        if self.outputs.is_empty() {
            items.push(text("(no outputs found)").muted().font_size(12.0));
        }
        items.push(
            row([button("Refresh").key("out-refresh").secondary(), spacer()])
                .width(Size::Fill(1.0)),
        );
        column(items).gap(tokens::SPACE_2).width(Size::Fill(1.0))
    }

    fn formats_view(&self) -> El {
        // Reachable formats share one row of natural-width toggles (five short
        // tokens fit comfortably). Unreachable formats drop below it, each with
        // its reason chip — too long to share the toggle row.
        let mut toggles: Vec<El> = Vec::new();
        let mut blocked: Vec<El> = Vec::new();
        for f in &self.formats {
            match self.unreachable(f.token) {
                None => {
                    let b = button(f.token).key(format!("fmt:{}", f.token));
                    toggles.push(if f.on { b.primary() } else { b.secondary() });
                }
                Some(reason) => blocked.push(
                    row([
                        mono(f.token)
                            .font_size(13.0)
                            .muted()
                            .width(Size::Fixed(120.0)),
                        text(reason.reason())
                            .font_size(12.0)
                            .muted()
                            .wrap_text()
                            .width(Size::Fill(1.0)),
                    ])
                    .gap(tokens::SPACE_2)
                    .align(Align::Center)
                    .width(Size::Fill(1.0)),
                ),
            }
        }
        let mut rows: Vec<El> = Vec::new();
        if !toggles.is_empty() {
            rows.push(row(toggles).gap(tokens::SPACE_2).align(Align::Center));
        }
        rows.extend(blocked);
        column(rows).gap(tokens::SPACE_2).width(Size::Fill(1.0))
    }

    fn seqs_view(&self) -> El {
        // All three `toggle − N +` groups share one row; natural-width toggles
        // keep each group compact enough for the field width.
        let groups: Vec<El> = self
            .seqs
            .iter()
            .map(|s| {
                let toggle = {
                    let b = button(s.name).key(format!("seq:{}", s.name));
                    if s.on { b.primary() } else { b.secondary() }
                };
                row([
                    toggle,
                    button("−").key(format!("seqdec:{}", s.name)).secondary(),
                    mono(format!("{}", s.count))
                        .font_size(13.0)
                        .center_text()
                        .width(Size::Fixed(34.0)),
                    button("+").key(format!("seqinc:{}", s.name)).secondary(),
                ])
                .gap(tokens::SPACE_2)
                .align(Align::Center)
            })
            .collect();
        row(groups).gap(tokens::SPACE_4).align(Align::Center)
    }

    /// The gamut-probe controls: an on/off toggle, and — when on — the repeats
    /// and refinement-depth steppers.
    fn gamut_view(&self) -> El {
        let toggle = {
            let b = button(if self.probe_gamut { "on" } else { "off" }).key("gamut:toggle");
            if self.probe_gamut {
                b.primary()
            } else {
                b.secondary()
            }
        };
        let mut items = vec![
            row([
                toggle,
                text("probe each format's reproduced gamut first; constrains scatter to it")
                    .muted()
                    .font_size(12.0)
                    .wrap_text()
                    .width(Size::Fill(1.0)),
            ])
            .gap(tokens::SPACE_2)
            .align(Align::Center)
            .width(Size::Fill(1.0)),
        ];
        if self.probe_gamut {
            items.push(
                row([
                    text("repeats")
                        .muted()
                        .font_size(12.0)
                        .width(Size::Fixed(56.0)),
                    stepper(
                        format!("{}", self.gamut_repeats),
                        "gamut-rep:dec",
                        "gamut-rep:inc",
                    ),
                    text("depth")
                        .muted()
                        .font_size(12.0)
                        .width(Size::Fixed(48.0)),
                    stepper(
                        format!("{}", self.gamut_max_depth),
                        "gamut-depth:dec",
                        "gamut-depth:inc",
                    ),
                ])
                .gap(tokens::SPACE_2)
                .align(Align::Center),
            );
        }
        column(items).gap(tokens::SPACE_2).width(Size::Fill(1.0))
    }
}

/// A `label : body` row used throughout the form. Fills the card width so wide
/// bodies (the stacked output list) can expand; the label top-aligns so it
/// reads correctly against a tall body.
fn field(label: &str, body: El) -> El {
    row([
        text(label).muted().font_size(13.0).width(Size::Fixed(96.0)),
        body,
    ])
    .gap(tokens::SPACE_3)
    .align(Align::Start)
    .width(Size::Fill(1.0))
}

/// A `−  value  +` stepper bound to two routes. The value centers in its fixed
/// box so it sits midway between the buttons instead of hugging `−`.
fn stepper(value: String, dec: &str, inc: &str) -> El {
    row([
        button("−").key(dec.to_string()).secondary(),
        mono(value)
            .font_size(13.0)
            .center_text()
            .width(Size::Fixed(72.0)),
        button("+").key(inc.to_string()).secondary(),
    ])
    .gap(tokens::SPACE_2)
    .align(Align::Center)
}

/// Human-friendly duration: `45s`, `3m 20s`.
fn fmt_dur(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m {}s", secs / 60, secs % 60)
    }
}
