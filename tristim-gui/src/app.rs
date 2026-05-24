//! The presenter application: state + tree.
//!
//! Phase 1 is the shell — a trial selector, an aggregate stat readout per
//! trial, and a panel describing the presenter's own display (read from the
//! host's [`aetna_core::event::HostDiagnostics`]). The chromaticity field and
//! per-sample error visualization land in later phases; their slot is the
//! [`plot_placeholder`] in the content panel.

use aetna_core::prelude::*;
use tristim_analyze::{AnalyzedCapture, AnalyzedTrial, GroundTruth, GroundTruthSource, analyze};
use tristim_capture::Capture;
use tristim_color::metrics;

use crate::chart::chromaticity_chart;

/// The loaded capture, its analysis, and which trial is in focus.
pub struct PresenterApp {
    capture: Capture,
    analyzed: AnalyzedCapture,
    selected: usize,
}

impl PresenterApp {
    pub fn new(capture: Capture) -> Self {
        let analyzed = analyze(&capture);
        Self {
            capture,
            analyzed,
            selected: 0,
        }
    }

    /// Number of trials in the loaded capture.
    pub fn trial_count(&self) -> usize {
        self.analyzed.trials.len()
    }

    /// Focus trial `i` (clamped to the valid range). Used by the headless
    /// bundle dump to lay out every trial's panel, not just the default one.
    pub fn select(&mut self, i: usize) {
        self.selected = i.min(self.trial_count().saturating_sub(1));
    }

    /// Short label for trial `i`: the requested (or unmanaged) basis + format.
    fn trial_label(&self, i: usize) -> String {
        let fmt = &self.analyzed.trials[i].pixel_format;
        let basis = match &self.capture.trials[i].requested {
            Some(d) => format!("{}/{}", d.transfer_function, d.primaries),
            None => "unmanaged".to_string(),
        };
        format!("{basis} · {fmt}")
    }

    fn header(&self) -> El {
        let dev = &self.capture.device;
        let out = &self.capture.output;
        let out_label = match &out.mode {
            Some(m) => format!("{} · {}×{}", out.name, m.width, m.height),
            None => out.name.clone(),
        };
        row([
            column([
                h2("tristim"),
                text("color validation presenter").muted().font_size(12.0),
            ])
            .gap(2.0),
            spacer(),
            fact(
                "device",
                format!(
                    "{} · SN {} · cal {}",
                    dev.product, dev.serial, dev.cal_index
                ),
            ),
            fact("output", out_label),
            fact("captured", self.capture.timestamp.clone()),
        ])
        .gap(tokens::SPACE_6)
        .align(Align::Center)
    }

    fn sidebar(&self, cx: &BuildCx) -> El {
        let mut items: Vec<El> = vec![section_label("Trials")];
        for i in 0..self.analyzed.trials.len() {
            let b = button(self.trial_label(i))
                .key(format!("trial:{i}"))
                .width(Size::Fill(1.0));
            items.push(if i == self.selected { b } else { b.secondary() });
        }
        if self.analyzed.trials.is_empty() {
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
                rows.push(stat_row("backend", d.backend));
                rows.push(stat_row(
                    "working space",
                    format!("{:?}", d.working_color_space),
                ));
                rows.push(stat_row("color mgmt", format!("{:?}", d.color_management)));
                if let Some(sc) = &d.surface_color {
                    rows.push(stat_row("adapter", sc.adapter.clone()));
                    rows.push(stat_row("swapchain", sc.chosen_format.clone()));
                    let wide = sc.formats.iter().filter(|f| f.wide).count();
                    rows.push(stat_row(
                        "wide formats",
                        format!("{wide}/{}", sc.formats.len()),
                    ));
                }
            }
            None => rows.push(text("(no host diagnostics yet)").muted().font_size(12.0)),
        }
        column(rows).gap(tokens::SPACE_2)
    }

    fn content_panel(&self) -> El {
        if self.analyzed.trials.is_empty() {
            return column([text("Nothing to show.").muted()]).width(Size::Fill(1.0));
        }
        let i = self.selected.min(self.analyzed.trials.len() - 1);
        let t = &self.analyzed.trials[i];
        column([
            column([
                h3(self.trial_label(i)),
                text(ground_truth_line(t)).muted().font_size(13.0),
            ])
            .gap(2.0),
            row([
                chromaticity_chart(t),
                column([summary_card(t).width(Size::Fill(1.0)), chart_legend()])
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
}

impl App for PresenterApp {
    fn build(&self, cx: &BuildCx) -> El {
        column([
            self.header(),
            divider(),
            row([self.sidebar(cx), self.content_panel()])
                .gap(tokens::SPACE_6)
                .align(Align::Stretch)
                .width(Size::Fill(1.0))
                .height(Size::Fill(1.0)),
        ])
        .padding(tokens::SPACE_6)
        .gap(tokens::SPACE_4)
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0))
    }

    fn on_event(&mut self, e: UiEvent) {
        if !matches!(e.kind, UiEventKind::Click | UiEventKind::Activate) {
            return;
        }
        if let Some(rest) = e.route().and_then(|k| k.strip_prefix("trial:")) {
            if let Ok(i) = rest.parse::<usize>() {
                if i < self.analyzed.trials.len() {
                    self.selected = i;
                }
            }
        }
    }
}

// ── view helpers ────────────────────────────────────────────────────────────

fn section_label(s: &str) -> El {
    text(s).muted().font_size(12.0)
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

fn chart_legend() -> El {
    titled_card(
        "Legend",
        [
            text("dot — measured chromaticity").muted().font_size(12.0),
            text("line — error from the target").muted().font_size(12.0),
            text("triangle — target gamut").muted().font_size(12.0),
            text("ring — target white point").muted().font_size(12.0),
            text("color — ΔE*ab, green → red").muted().font_size(12.0),
            text("CIE 1976 u'v' diagram").muted().font_size(11.0),
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
