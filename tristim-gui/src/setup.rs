//! The capture-setup form: configure and launch an in-process capture run.
//!
//! Everything routes through keyed buttons + `−/+` steppers (the same
//! pattern the presenter uses for trial/view toggles), so the form needs no
//! stateful aetna widgets. [`CaptureForm::build_config`] turns the form state
//! into a [`CaptureConfig`] for `tristim_gather::run_capture`, reusing the same
//! `parse_format` / `parse_sequence` the CLI uses (so e.g. scatter draws from
//! the identical seed).

use std::time::Duration;

use aetna_core::prelude::*;
use tristim_display::list_outputs;
use tristim_gather::{self as gather, CaptureConfig, KNOWN_FORMATS};

/// What a routed form event asks the app to do.
pub enum FormAction {
    /// Nothing beyond the in-form state change already applied.
    None,
    /// Launch a capture with this validated config.
    Start(CaptureConfig),
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
            error: None,
        }
    }

    /// Record a validation/run error to show beneath the form.
    pub fn set_error(&mut self, msg: String) {
        self.error = Some(msg);
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
            if let Some(f) = self.formats.iter_mut().find(|f| f.token == tok) {
                f.on = !f.on;
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
                "start" => match self.build_config() {
                    Ok(cfg) => {
                        self.error = None;
                        return FormAction::Start(cfg);
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

        let formats = self
            .formats
            .iter()
            .filter(|f| f.on)
            .map(|f| gather::parse_format(f.token))
            .collect::<Result<Vec<_>, _>>()?;
        if formats.is_empty() {
            return Err("enable at least one format".into());
        }

        let mut sequence = Vec::new();
        for s in &self.seqs {
            if s.on {
                sequence.extend(gather::parse_sequence(&format!("{}:{}", s.name, s.count))?);
            }
        }
        if sequence.is_empty() {
            return Err("enable at least one sequence".into());
        }

        Ok(CaptureConfig {
            output,
            cal_index: self.cal_index,
            settle: Duration::from_millis(self.settle_ms),
            prep: Duration::from_secs(self.prep_secs),
            window_fraction: self.window_pct as f64 / 100.0,
            border: None,
            formats,
            sequence,
        })
    }

    /// (total measurements, rough estimated seconds) for the current selection.
    fn preview(&self) -> (usize, u64) {
        let fmts = self.formats.iter().filter(|f| f.on).count();
        let seq_len: usize = self
            .seqs
            .iter()
            .filter(|s| s.on)
            .filter_map(|s| gather::parse_sequence(&format!("{}:{}", s.name, s.count)).ok())
            .map(|v| v.len())
            .sum();
        let count = fmts * seq_len;
        // Per-patch cost ≈ settle + a fixed measurement overhead.
        let per = self.settle_ms as f64 / 1000.0 + 0.4;
        let est = self.prep_secs + (count as f64 * per) as u64;
        (count, est)
    }

    /// The setup form El.
    pub fn view(&self) -> El {
        let (count, est) = self.preview();
        let plan = if count == 0 {
            "nothing selected".to_string()
        } else {
            format!("{count} measurements · ~{}", fmt_dur(est))
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
            field(
                "Settle",
                stepper(format!("{} ms", self.settle_ms), "settle:dec", "settle:inc"),
            ),
            field(
                "Prep",
                stepper(format!("{} s", self.prep_secs), "prep:dec", "prep:inc"),
            ),
            field(
                "Window",
                stepper(format!("{} %", self.window_pct), "window:dec", "window:inc"),
            ),
            field(
                "Cal index",
                stepper(format!("{}", self.cal_index), "cal:dec", "cal:inc"),
            ),
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

        titled_card("New capture", rows).gap(tokens::SPACE_3)
    }

    fn outputs_view(&self) -> El {
        // A vertical list (not a row): output count and label length are
        // unbounded, so stacking full-width buttons can't overflow horizontally.
        let mut items: Vec<El> = self
            .outputs
            .iter()
            .enumerate()
            .map(|(i, o)| {
                let b = button(o.label.clone())
                    .key(format!("out:{i}"))
                    .width(Size::Fill(1.0));
                if Some(i) == self.selected_output {
                    b.primary()
                } else {
                    b.secondary()
                }
            })
            .collect();
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
        let items: Vec<El> = self
            .formats
            .iter()
            .map(|f| {
                let b = button(f.token).key(format!("fmt:{}", f.token));
                if f.on { b.primary() } else { b.secondary() }
            })
            .collect();
        row(items).gap(tokens::SPACE_2)
    }

    fn seqs_view(&self) -> El {
        let rows: Vec<El> = self
            .seqs
            .iter()
            .map(|s| {
                let toggle = {
                    let b = button(s.name)
                        .key(format!("seq:{}", s.name))
                        .width(Size::Fixed(100.0));
                    if s.on { b.primary() } else { b.secondary() }
                };
                row([
                    toggle,
                    button("−").key(format!("seqdec:{}", s.name)).secondary(),
                    mono(format!("N={}", s.count))
                        .font_size(13.0)
                        .width(Size::Fixed(64.0)),
                    button("+").key(format!("seqinc:{}", s.name)).secondary(),
                ])
                .gap(tokens::SPACE_2)
                .align(Align::Center)
            })
            .collect();
        column(rows).gap(tokens::SPACE_2)
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

/// A `−  value  +` stepper bound to two routes.
fn stepper(value: String, dec: &str, inc: &str) -> El {
    row([
        button("−").key(dec.to_string()).secondary(),
        mono(value).font_size(13.0).width(Size::Fixed(72.0)),
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
