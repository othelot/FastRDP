//! Full-screen connection bar, modelled on mstsc's: pin, connection quality, computer name and the
//! live stream statistics in the middle, then minimize / restore (leave full screen) / close
//! (disconnect). Icons are drawn with the canvas, so the bar looks the same on every platform.
use iced::widget::canvas::{self, Frame, Path, Stroke};
use iced::widget::{button, canvas as canvas_widget, container, row, text, tooltip};
use iced::{Alignment, Border, Color, Element, Length, Point, Rectangle, Renderer, Size, Theme, mouse};

pub const WIDTH: f32 = 820.0;
pub const HEIGHT: f32 = 30.0;
const SIDE: f32 = 138.0; // left and right groups have the same width: the name stays centred
const CAPTION_W: f32 = 46.0;
const LEFT_W: f32 = 36.0;

const BG: Color = Color::from_rgb(0.125, 0.125, 0.125);
const FG: Color = Color::from_rgb(0.92, 0.92, 0.92);
const DIM: Color = Color::from_rgb(0.38, 0.38, 0.38);
/// statistics next to the name
const MUTED: Color = Color::from_rgb(0.62, 0.62, 0.62);

/// What the bar shows next to the computer name.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StreamStats {
    pub fps: f64,
    pub mbps: f64,
    /// remote desktop size and refresh rate (from the stream description)
    pub mode: Option<(u32, u32, u32)>,
    /// capture -> decoded latency (ms), only when measurable
    pub latency_ms: Option<f64>,
}

impl StreamStats {
    /// e.g. `58 fps · 12.4 Mbps · 2560×1440@144 · 14 ms`
    pub fn text(&self) -> String {
        let mut s = format!("{:.0} fps · {:.1} Mbps", self.fps, self.mbps);
        if let Some((w, h, hz)) = self.mode {
            s.push_str(&format!(" · {w}×{h}@{hz}"));
        }
        if let Some(ms) = self.latency_ms {
            s.push_str(&format!(" · {ms:.0} ms"));
        }
        s
    }
}

/// Connection quality shown as signal bars (1..=4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quality(pub u8);

impl Quality {
    /// From the frames lost / repaired by FEC during the last statistics interval.
    pub fn from_interval(lost: u64, recovered: u64) -> Self {
        Quality(match (lost, recovered) {
            (0, 0) => 4,
            (0, _) => 3,
            (1..=2, _) => 2,
            _ => 1,
        })
    }

    pub fn label(self) -> &'static str {
        match self.0 {
            4 => "Connection quality: excellent",
            3 => "Connection quality: good (lost packets repaired)",
            2 => "Connection quality: fair (some frames lost)",
            _ => "Connection quality: poor",
        }
    }
}

/// What the bar's buttons send.
pub struct Actions<M> {
    pub pin: M,
    pub minimize: M,
    pub restore: M,
    pub close: M,
}

#[derive(Clone, Copy)]
enum Glyph {
    Pin { pinned: bool },
    Signal(u8),
    Minimize,
    Restore,
    Close,
}

struct Icon(Glyph);

impl<M> canvas::Program<M> for Icon {
    type State = ();

    fn draw(&self, _: &(), renderer: &Renderer, _: &Theme, bounds: Rectangle, _: mouse::Cursor) -> Vec<canvas::Geometry> {
        let mut f = Frame::new(renderer, bounds.size());
        // 16x16 icon box centred in the button
        let o = Point::new(((bounds.width - 16.0) / 2.0).round() + 0.5, ((bounds.height - 16.0) / 2.0).round() + 0.5);
        let p = |x: f32, y: f32| Point::new(o.x + x, o.y + y);
        let line = |f: &mut Frame, a: Point, b: Point, c: Color| f.stroke(&Path::line(a, b), Stroke::default().with_color(c).with_width(1.0));
        match self.0 {
            Glyph::Minimize => line(&mut f, p(3.0, 8.0), p(13.0, 8.0), FG),
            Glyph::Restore => {
                let s = Stroke::default().with_color(FG).with_width(1.0);
                f.stroke(&Path::rectangle(p(3.0, 5.0), Size::new(8.0, 8.0)), s);
                // back window: top and right edges only
                f.stroke(
                    &Path::new(|b| {
                        b.move_to(p(5.0, 5.0));
                        b.line_to(p(5.0, 3.0));
                        b.line_to(p(13.0, 3.0));
                        b.line_to(p(13.0, 11.0));
                        b.line_to(p(11.0, 11.0));
                    }),
                    s,
                );
            }
            Glyph::Close => {
                line(&mut f, p(3.0, 3.0), p(13.0, 13.0), FG);
                line(&mut f, p(13.0, 3.0), p(3.0, 13.0), FG);
            }
            Glyph::Signal(level) => {
                for i in 0..4u8 {
                    let h = 4.0 + 3.0 * i as f32;
                    let x = 2.0 + 3.5 * i as f32;
                    let c = if i < level { FG } else { DIM };
                    f.fill_rectangle(Point::new(o.x - 0.5 + x, o.y - 0.5 + 14.0 - h), Size::new(2.0, h), c);
                }
            }
            Glyph::Pin { pinned } => {
                // push pin: head, body, needle; tilted when not pinned
                let s = Stroke::default().with_color(FG).with_width(1.0);
                let (sin, cos) = if pinned { (0.0f32, 1.0f32) } else { (std::f32::consts::FRAC_1_SQRT_2, std::f32::consts::FRAC_1_SQRT_2) };
                let r = |x: f32, y: f32| {
                    let (dx, dy) = (x - 8.0, y - 8.0);
                    p(8.0 + dx * cos - dy * sin, 8.0 + dx * sin + dy * cos)
                };
                f.stroke(
                    &Path::new(|b| {
                        b.move_to(r(5.0, 2.0));
                        b.line_to(r(11.0, 2.0));
                        b.move_to(r(6.0, 2.0));
                        b.line_to(r(6.0, 7.0));
                        b.line_to(r(4.0, 10.0));
                        b.line_to(r(12.0, 10.0));
                        b.line_to(r(10.0, 7.0));
                        b.line_to(r(10.0, 2.0));
                        b.move_to(r(8.0, 10.0));
                        b.line_to(r(8.0, 15.0));
                    }),
                    s,
                );
            }
        }
        vec![f.into_geometry()]
    }
}

fn icon<'a, M: 'a>(g: Glyph, w: f32) -> Element<'a, M> {
    canvas_widget(Icon(g)).width(w).height(HEIGHT).into()
}

fn flat(close: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let bg = match status {
            button::Status::Hovered if close => Some(Color::from_rgb8(0xC4, 0x2B, 0x1C)),
            button::Status::Pressed if close => Some(Color::from_rgb8(0xA3, 0x26, 0x1A)),
            button::Status::Hovered => Some(Color::from_rgba(1.0, 1.0, 1.0, 0.10)),
            button::Status::Pressed => Some(Color::from_rgba(1.0, 1.0, 1.0, 0.06)),
            _ => None,
        };
        button::Style { background: bg.map(Into::into), text_color: FG, ..Default::default() }
    }
}

fn icon_button<'a, M: Clone + 'a>(g: Glyph, w: f32, on: M, close: bool) -> Element<'a, M> {
    button(icon(g, w)).padding(0).on_press(on).style(flat(close)).into()
}

/// The bar (fixed size, to be centred at the top of the full-screen picture).
pub fn view<'a, M: Clone + 'a>(name: &str, stats: &StreamStats, quality: Quality, pinned: bool, a: Actions<M>) -> Element<'a, M> {
    let tip = |content: Element<'a, M>, label: &'static str| -> Element<'a, M> {
        tooltip(content, container(text(label).size(12)).padding([3, 6]).style(container::dark), tooltip::Position::Bottom).into()
    };
    let left = row![
        tip(icon_button(Glyph::Pin { pinned }, LEFT_W, a.pin, false), if pinned { "Unpin the connection bar" } else { "Pin the connection bar" }),
        tip(icon(Glyph::Signal(quality.0), LEFT_W), quality.label()),
    ];
    let right = row![
        tip(icon_button(Glyph::Minimize, CAPTION_W, a.minimize, false), "Minimize"),
        tip(icon_button(Glyph::Restore, CAPTION_W, a.restore, false), "Leave full screen"),
        tip(icon_button(Glyph::Close, CAPTION_W, a.close, true), "Disconnect"),
    ];
    let bar = row![
        container(left).width(SIDE),
        container(row![text(name.to_string()).size(13).color(FG), text(stats.text()).size(11).color(MUTED)].spacing(12).align_y(Alignment::Center))
            .width(Length::Fill)
            .center_x(Length::Fill),
        container(right).width(SIDE).align_x(Alignment::End),
    ]
    .align_y(Alignment::Center)
    .height(HEIGHT);
    container(bar)
        .width(WIDTH)
        .style(|_| container::Style {
            background: Some(BG.into()),
            border: Border { color: Color::from_rgb(0.24, 0.24, 0.24), width: 1.0, radius: iced::border::bottom(4.0) },
            ..Default::default()
        })
        .into()
}

/// Name shown in the bar: the server as typed, without the default port.
pub fn display_name(server: &str) -> String {
    let s = server.trim();
    s.strip_suffix(":33890").unwrap_or(s).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_levels() {
        assert_eq!(Quality::from_interval(0, 0), Quality(4));
        assert_eq!(Quality::from_interval(0, 3), Quality(3));
        assert_eq!(Quality::from_interval(2, 0), Quality(2));
        assert_eq!(Quality::from_interval(9, 1), Quality(1));
    }

    #[test]
    fn stats_text() {
        let s = StreamStats { fps: 57.6, mbps: 12.44, mode: Some((2560, 1440, 144)), latency_ms: None };
        assert_eq!(s.text(), "58 fps · 12.4 Mbps · 2560×1440@144");
        let s = StreamStats { latency_ms: Some(13.6), mode: None, ..s };
        assert_eq!(s.text(), "58 fps · 12.4 Mbps · 14 ms");
    }

    #[test]
    fn name_without_default_port() {
        assert_eq!(display_name(" 203.0.113.7:33890 "), "203.0.113.7");
        assert_eq!(display_name("pc:4000"), "pc:4000");
    }
}
