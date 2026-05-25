//! The presenter application: state + tree.
//!
//! The shell is a trial selector plus a panel describing the presenter's own
//! display (read from the host's [`aetna_core::event::HostDiagnostics`]); the
//! content panel shows the per-trial [`crate::chart`] chromaticity diagram
//! beside the aggregate stat readout, with an opt-in color-field backdrop
//! bounded to the presenter's negotiated gamut. Still to come: hover-to-inspect.

use aetna_core::prelude::*;
use tristim_analyze::{AnalyzedCapture, AnalyzedTrial, GroundTruth, GroundTruthSource, analyze};
use tristim_capture::Capture;
use tristim_color::metrics;

use crate::chart::{PresenterGamut, chromaticity_chart};
use crate::plot::Space;

/// The loaded capture, its analysis, and which trial is in focus.
pub struct PresenterApp {
    capture: Capture,
    analyzed: AnalyzedCapture,
    selected: usize,
    /// Chromaticity projection for the diagram.
    space: Space,
    /// Whether to paint the chromaticity color-field backdrop (opt-in).
    show_field: bool,
}

impl PresenterApp {
    pub fn new(capture: Capture) -> Self {
        let analyzed = analyze(&capture);
        Self {
            capture,
            analyzed,
            selected: 0,
            space: Space::UvPrime,
            show_field: false,
        }
    }

    /// Set the chromaticity projection. Used by the headless dump to render
    /// both spaces.
    pub fn set_space(&mut self, space: Space) {
        self.space = space;
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

    /// Enable/disable the color-field backdrop. Used by the headless dump to
    /// exercise the filled layout.
    pub fn set_show_field(&mut self, on: bool) {
        self.show_field = on;
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

    fn content_panel(&self, cx: &BuildCx) -> El {
        if self.analyzed.trials.is_empty() {
            return column([text("Nothing to show.").muted()]).width(Size::Fill(1.0));
        }
        let i = self.selected.min(self.analyzed.trials.len() - 1);
        let t = &self.analyzed.trials[i];

        // The color fill is bounded to the presenter's own negotiated gamut, and
        // only painted when toggled on. No gamut → the toggle is hidden.
        let gamut = presenter_gamut(cx);
        let field = if self.show_field { gamut } else { None };

        let mut heading: Vec<El> = vec![
            column([
                h3(self.trial_label(i)),
                text(ground_truth_line(t)).muted().font_size(13.0),
            ])
            .gap(2.0),
            spacer(),
            space_toggle(self.space),
        ];
        if gamut.is_some() {
            heading.push(field_toggle(self.show_field));
        }

        column([
            row(heading)
                .gap(tokens::SPACE_2)
                .align(Align::Center)
                .width(Size::Fill(1.0)),
            row([
                chromaticity_chart(t, self.space, field, plot_size(cx)),
                column([
                    summary_card(t).width(Size::Fill(1.0)),
                    chart_legend(self.space),
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
}

/// Responsive square side (px) for the plot, derived from the window viewport.
/// Grows the diagram to fill the content area — leaving the stat/legend column
/// its minimum width — and is bounded vertically so it always fits on screen.
/// Falls back to a sensible size when no viewport is attached (headless).
fn plot_size(cx: &BuildCx) -> f32 {
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

impl App for PresenterApp {
    fn build(&self, cx: &BuildCx) -> El {
        column([
            self.header(),
            divider(),
            row([self.sidebar(cx), self.content_panel(cx)])
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

    fn shaders(&self) -> Vec<AppShader> {
        vec![crate::chart::field_shader()]
    }

    fn on_event(&mut self, e: UiEvent) {
        if !matches!(e.kind, UiEventKind::Click | UiEventKind::Activate) {
            return;
        }
        match e.route() {
            Some("field-toggle") => self.show_field = !self.show_field,
            Some("space-toggle") => self.space = self.space.toggled(),
            Some(k) => {
                if let Some(i) = k
                    .strip_prefix("trial:")
                    .and_then(|r| r.parse::<usize>().ok())
                {
                    if i < self.analyzed.trials.len() {
                        self.selected = i;
                    }
                }
            }
            None => {}
        }
    }
}

// ── view helpers ────────────────────────────────────────────────────────────

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
    if on { b } else { b.secondary() }
}

/// Toggle between the two chromaticity projections, labeled with the current.
fn space_toggle(space: Space) -> El {
    button(space.label()).key("space-toggle").secondary()
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
