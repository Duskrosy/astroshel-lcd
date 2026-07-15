// Dashboard data model: widget kinds, visualizations, themes, and templates.
// Pure data + serde -- consumed by later tasks (sensors accessor, render engine,
// config, gui). Keep this module free of rendering/IO logic.

use serde::{Deserialize, Serialize};

/// Square render canvas side length, in device pixels. Every widget `Rect` must
/// fit entirely within `0..CANVAS` on both axes (enforced by a test below).
pub const CANVAS: u32 = 320;

/// RGB color, no alpha (the LCD panel doesn't composite transparency).
pub type Color = [u8; 3];

/// What a widget displays.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum WidgetKind {
    #[default]
    GpuTemp,
    GpuUsage,
    CpuUsage,
    RamUsage,
    Clock,
    Date,
    Text,
}

impl WidgetKind {
    /// Lenient parse: case/underscore/hyphen/space-insensitive. Falls back to
    /// `GpuTemp` (the flagship widget) for unrecognized input.
    pub fn from_str(s: &str) -> Self {
        let norm: String = s.chars().filter(|c| c.is_alphanumeric()).flat_map(|c| c.to_lowercase()).collect();
        match norm.as_str() {
            "gputemp" => Self::GpuTemp,
            "gpuusage" => Self::GpuUsage,
            "cpuusage" => Self::CpuUsage,
            "ramusage" => Self::RamUsage,
            "clock" => Self::Clock,
            "date" => Self::Date,
            "text" => Self::Text,
            _ => Self::GpuTemp,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::GpuTemp => "gpu_temp",
            Self::GpuUsage => "gpu_usage",
            Self::CpuUsage => "cpu_usage",
            Self::RamUsage => "ram_usage",
            Self::Clock => "clock",
            Self::Date => "date",
            Self::Text => "text",
        }
    }
}

/// How a widget's value is drawn.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Viz {
    Gauge,
    Ring,
    Bar,
    #[default]
    Number,
    Sparkline,
    /// Analog clock face (hour/minute/second hands). Only meaningful on a
    /// `Clock` widget; on other kinds the renderer falls back to `Number`.
    Analog,
    /// A fuller "history line-graph" than `Sparkline`: a labeled polyline
    /// with a filled area beneath it and a current-value readout, over the
    /// widget's `History` series. Display label: "Line graph".
    Line,
}

impl Viz {
    /// Lenient parse; falls back to `Number` (the simplest, always-valid viz).
    pub fn from_str(s: &str) -> Self {
        let norm: String = s.chars().filter(|c| c.is_alphanumeric()).flat_map(|c| c.to_lowercase()).collect();
        match norm.as_str() {
            "gauge" => Self::Gauge,
            "ring" => Self::Ring,
            "bar" => Self::Bar,
            "number" => Self::Number,
            "sparkline" => Self::Sparkline,
            "analog" => Self::Analog,
            "line" | "linegraph" => Self::Line,
            _ => Self::Number,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Gauge => "gauge",
            Self::Ring => "ring",
            Self::Bar => "bar",
            Self::Number => "number",
            Self::Sparkline => "sparkline",
            Self::Analog => "analog",
            Self::Line => "line",
        }
    }

    /// Human-friendly label for pickers (e.g. the GUI's Viz ComboBox).
    pub fn display_label(&self) -> &'static str {
        match self {
            Self::Gauge => "Gauge",
            Self::Ring => "Ring",
            Self::Bar => "Bar",
            Self::Number => "Number",
            Self::Sparkline => "Sparkline",
            Self::Analog => "Analog",
            Self::Line => "Line graph",
        }
    }
}

/// Widget placement on the `CANVAS x CANVAS` grid, in device pixels.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub const fn new(x: u32, y: u32, w: u32, h: u32) -> Self {
        Rect { x, y, w, h }
    }
}

/// Color palette applied to a `Dashboard`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    pub bg: Color,
    pub accent: Color,
    pub text: Color,
    pub track: Color,
    pub muted: Color,
}

impl Theme {
    pub const DARK: Theme = Theme {
        bg: [8, 10, 18],
        accent: [0, 224, 150],
        text: [240, 245, 255],
        track: [38, 42, 58],
        muted: [150, 160, 180],
    };
    pub const LIGHT: Theme = Theme {
        bg: [245, 246, 250],
        accent: [0, 150, 110],
        text: [20, 22, 28],
        track: [210, 214, 222],
        muted: [110, 116, 130],
    };
    pub const NEON: Theme = Theme {
        bg: [5, 5, 10],
        accent: [255, 0, 180],
        text: [255, 255, 255],
        track: [40, 10, 40],
        muted: [180, 60, 160],
    };
    pub const MONO: Theme = Theme {
        bg: [20, 20, 20],
        accent: [220, 220, 220],
        text: [240, 240, 240],
        track: [60, 60, 60],
        muted: [140, 140, 140],
    };

    /// Named presets a user can pick from. Order matters for UI display.
    pub fn presets() -> Vec<(&'static str, Theme)> {
        vec![
            ("Dark", Theme::DARK),
            ("Light", Theme::LIGHT),
            ("Neon", Theme::NEON),
            ("Mono", Theme::MONO),
        ]
    }
}

impl Default for Theme {
    fn default() -> Self {
        Theme::DARK
    }
}

fn default_true() -> bool {
    true
}
fn default_font_scale() -> f32 {
    1.0
}
fn default_date_fmt() -> String {
    "%a %d %b".to_string()
}

/// A single dashboard element: what it shows (`kind`), how it's drawn (`viz`),
/// where it sits (`rect`), plus per-widget overrides. All fields are
/// `#[serde(default)]` so older/partial config files deserialize cleanly as
/// new fields are added over time.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Widget {
    #[serde(default)]
    pub kind: WidgetKind,
    #[serde(default)]
    pub viz: Viz,
    #[serde(default)]
    pub rect: Rect,
    #[serde(default = "default_true")]
    pub label: bool,
    #[serde(default)]
    pub min: f32,
    #[serde(default = "default_max")]
    pub max: f32,
    #[serde(default)]
    pub accent: Option<Color>,
    #[serde(default = "default_font_scale")]
    pub font_scale: f32,
    #[serde(default)]
    pub clock_24h: bool,
    #[serde(default)]
    pub show_seconds: bool,
    #[serde(default = "default_date_fmt")]
    pub date_fmt: String,
    #[serde(default)]
    pub text: String,
}

fn default_max() -> f32 {
    100.0
}

impl Widget {
    /// Sensible per-kind defaults, e.g. `GpuTemp` -> a 30..90 gauge,
    /// `CpuUsage`/`GpuUsage`/`RamUsage` -> a 0..100 bar, `Clock` -> 12h clock
    /// without seconds. `rect` is left zeroed; callers (templates, the editor
    /// UI) place the widget.
    pub fn new(kind: WidgetKind) -> Widget {
        let mut w = Widget {
            kind,
            viz: Viz::Number,
            rect: Rect::default(),
            label: true,
            min: 0.0,
            max: 100.0,
            accent: None,
            font_scale: 1.0,
            clock_24h: false,
            show_seconds: false,
            date_fmt: default_date_fmt(),
            text: String::new(),
        };
        match kind {
            WidgetKind::GpuTemp => {
                w.viz = Viz::Gauge;
                w.min = 30.0;
                w.max = 90.0;
            }
            WidgetKind::GpuUsage | WidgetKind::CpuUsage | WidgetKind::RamUsage => {
                w.viz = Viz::Bar;
                w.min = 0.0;
                w.max = 100.0;
            }
            WidgetKind::Clock => {
                w.viz = Viz::Number;
                w.clock_24h = false;
                w.show_seconds = false;
            }
            WidgetKind::Date => {
                w.viz = Viz::Number;
                w.date_fmt = default_date_fmt();
            }
            WidgetKind::Text => {
                w.viz = Viz::Number;
                w.text = String::new();
            }
        }
        w
    }

    fn at(mut self, rect: Rect) -> Widget {
        self.rect = rect;
        self
    }
}

impl Default for Widget {
    fn default() -> Self {
        Widget::new(WidgetKind::default())
    }
}

/// A full dashboard: the color theme plus the widgets placed on the canvas.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Dashboard {
    #[serde(default)]
    pub theme: Theme,
    #[serde(default)]
    pub widgets: Vec<Widget>,
}

impl Default for Dashboard {
    fn default() -> Self {
        gauge_focus()
    }
}

/// Mirrors the current (pre-customization) fixed layout: clock across the
/// top, a big GPU-temp gauge centered, CPU%/GPU% bars along the bottom.
fn gauge_focus() -> Dashboard {
    Dashboard {
        theme: Theme::default(),
        widgets: vec![
            Widget::new(WidgetKind::Clock).at(Rect::new(0, 6, 320, 70)),
            Widget::new(WidgetKind::GpuTemp).at(Rect::new(40, 76, 240, 170)),
            Widget::new(WidgetKind::CpuUsage).at(Rect::new(16, 270, 130, 40)),
            Widget::new(WidgetKind::GpuUsage).at(Rect::new(174, 270, 130, 40)),
        ],
    }
}

/// Four equal-size metric widgets, one per canvas quadrant, laid out as four
/// clean 148x148 cells with ~8px margins/gaps -- fully inside the canvas with
/// even padding on every edge (no clipping at the borders).
fn stats_grid() -> Dashboard {
    const MARGIN: u32 = 8;
    const CELL: u32 = 148;
    const GAP: u32 = 8;
    let x0 = MARGIN;
    let x1 = MARGIN + CELL + GAP; // 164
    let y0 = MARGIN;
    let y1 = MARGIN + CELL + GAP; // 164
    Dashboard {
        theme: Theme::default(),
        widgets: vec![
            Widget::new(WidgetKind::GpuTemp).at(Rect::new(x0, y0, CELL, CELL)),
            Widget::new(WidgetKind::GpuUsage).at(Rect::new(x1, y0, CELL, CELL)),
            Widget::new(WidgetKind::CpuUsage).at(Rect::new(x0, y1, CELL, CELL)),
            Widget::new(WidgetKind::RamUsage).at(Rect::new(x1, y1, CELL, CELL)),
        ],
    }
}

/// One large analog clock centered in the upper ~2/3 of the canvas, plus a
/// single small metric (GPU temp, as a plain number) underneath.
fn big_clock() -> Dashboard {
    Dashboard {
        theme: Theme::default(),
        widgets: vec![
            {
                // 250x250 square (so the analog face is a circle, not an ellipse),
                // horizontally centered (35 + 250 + 35 == 320).
                let mut clock = Widget::new(WidgetKind::Clock).at(Rect::new(35, 8, 250, 250));
                clock.viz = Viz::Analog;
                clock.show_seconds = true;
                clock
            },
            {
                let mut temp = Widget::new(WidgetKind::GpuTemp).at(Rect::new(60, 266, 200, 46));
                temp.viz = Viz::Number;
                temp
            },
        ],
    }
}

/// A single GPU-temp readout as a big number -- the leanest possible layout.
fn minimal() -> Dashboard {
    Dashboard {
        theme: Theme::default(),
        widgets: vec![{
            let mut temp = Widget::new(WidgetKind::GpuTemp).at(Rect::new(20, 60, 280, 200));
            temp.viz = Viz::Number;
            temp
        }],
    }
}

/// Built-in dashboard templates a user can start from. Every widget rect is
/// guaranteed to fit within `CANVAS` (see `all_template_widgets_within_canvas`).
pub fn templates() -> Vec<(&'static str, Dashboard)> {
    vec![
        ("Gauge Focus", gauge_focus()),
        ("Stats Grid", stats_grid()),
        ("Big Clock", big_clock()),
        ("Minimal", minimal()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_viz_roundtrip() {
        assert_eq!(WidgetKind::from_str("gpu_temp"), WidgetKind::GpuTemp);
        assert_eq!(WidgetKind::CpuUsage.as_str(), "cpu_usage");
        assert_eq!(Viz::from_str("ring"), Viz::Ring);
    }
    #[test]
    fn analog_viz_roundtrip() {
        assert_eq!(Viz::from_str("analog"), Viz::Analog);
        assert_eq!(Viz::Analog.as_str(), "analog");
    }
    #[test]
    fn line_viz_roundtrip() {
        assert_eq!(Viz::from_str("line"), Viz::Line);
        assert_eq!(Viz::from_str("Line Graph"), Viz::Line);
        assert_eq!(Viz::Line.as_str(), "line");
        assert_eq!(Viz::Line.display_label(), "Line graph");
    }
    #[test]
    fn all_template_widgets_within_canvas() {
        for (_name, d) in templates() {
            assert!(!d.widgets.is_empty());
            for w in &d.widgets {
                assert!(w.rect.x + w.rect.w <= CANVAS, "widget overflows canvas");
                assert!(w.rect.y + w.rect.h <= CANVAS);
            }
        }
    }
    #[test]
    fn default_dashboard_is_a_template() {
        let d = Dashboard::default();
        assert!(!d.widgets.is_empty());
    }
    #[test]
    fn theme_presets_include_dark() {
        assert!(Theme::presets().iter().any(|(n, _)| *n == "Dark"));
    }
    #[test]
    fn widget_new_defaults_sane() {
        let w = Widget::new(WidgetKind::GpuTemp);
        assert_eq!(w.viz, Viz::Gauge);
        assert!(w.max > w.min);
    }
}
