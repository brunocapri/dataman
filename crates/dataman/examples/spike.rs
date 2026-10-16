//! GPUI spike: a window, a virtualized list of 1 M rows and a text field.
//!
//! `SPIKE_BENCH=1` scrolls the list by itself for a few seconds, prints frame statistics
//! and quits. `SPIKE_T0_MS=<epoch ms>` (set by the launcher) prints launch → first frame.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gpui::{
    App, Bounds, Context, FocusHandle, Focusable, KeyBinding, SharedString, UniformListScrollHandle,
    Window, WindowBounds, WindowOptions, actions, div, point, prelude::*, px, rgb, size,
    uniform_list,
};
use gpui_platform::application;

actions!(spike, [Quit, Backspace]);

const ROWS: usize = 1_000_000;
const COLS: usize = 8;

struct Spike {
    scroll: UniformListScrollHandle,
    focus: FocusHandle,
    text: String,
    bench: Option<Bench>,
    first_frame_seen: bool,
}

struct Bench {
    started: Option<Instant>,
    last: Option<Instant>,
    intervals: Vec<Duration>,
    renders: Vec<Duration>,
    y: f32,
}

impl Spike {
    fn on_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let now = Instant::now();
        let Some(b) = self.bench.as_mut() else { return };
        let started = *b.started.get_or_insert(now);
        if let Some(last) = b.last {
            b.intervals.push(now - last);
        }
        b.last = Some(now);
        if now - started > Duration::from_secs(6) {
            report(b);
            cx.quit();
            return;
        }
        // A steady fling: 600 px per frame, bouncing between the ends.
        b.y += 600.0;
        let max = (ROWS as f32) * 22.0 - 600.0;
        if b.y > max {
            b.y = 0.0;
        }
        self.scroll.0.borrow().base_handle.set_offset(point(px(0.), px(-b.y)));
        cx.notify();
        let this = cx.entity();
        window.on_next_frame(move |window, cx| this.update(cx, |s, cx| s.on_frame(window, cx)));
    }
}

fn pct(v: &mut [Duration], p: f64) -> f64 {
    v.sort();
    let i = ((v.len() as f64 - 1.0) * p).round() as usize;
    v[i].as_secs_f64() * 1e3
}

fn report(b: &mut Bench) {
    let n = b.intervals.len();
    let over = |ms: f64| b.intervals.iter().filter(|d| d.as_secs_f64() * 1e3 > ms).count();
    let (o17, o34) = (over(17.5), over(34.0));
    println!(
        "frames={n} interval_ms p50={:.2} p95={:.2} p99={:.2} max={:.2} >17.5ms={o17} >34ms={o34}",
        pct(&mut b.intervals, 0.5),
        pct(&mut b.intervals, 0.95),
        pct(&mut b.intervals, 0.99),
        pct(&mut b.intervals, 1.0),
    );
    println!(
        "render_ms p50={:.3} p99={:.3} max={:.3}",
        pct(&mut b.renders, 0.5),
        pct(&mut b.renders, 0.99),
        pct(&mut b.renders, 1.0)
    );
}

impl Focusable for Spike {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Spike {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = Instant::now();
        if !self.first_frame_seen {
            self.first_frame_seen = true;
            if let Some(t0) = std::env::var("SPIKE_T0_MS").ok().and_then(|v| v.parse::<u128>().ok()) {
                let this = cx.entity();
                window.on_next_frame(move |_, cx| {
                    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis();
                    println!("first_frame_ms={}", now - t0);
                    if std::env::var("SPIKE_BENCH").is_err() && std::env::var("SPIKE_EXIT").is_ok() {
                        cx.quit();
                    }
                    let _ = this;
                });
            }
            if self.bench.is_some() {
                let this = cx.entity();
                window.on_next_frame(move |window, cx| this.update(cx, |s, cx| s.on_frame(window, cx)));
            }
        }
        let el = div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x1e1f22))
            .text_color(rgb(0xd4d4d4))
            .font_family("Menlo")
            .text_size(px(13.))
            .track_focus(&self.focus)
            .key_context("Spike")
            .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _, cx| {
                if let Some(ch) = ev.keystroke.key_char.as_ref()
                    && !ev.keystroke.modifiers.platform
                {
                    this.text.push_str(ch);
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &Backspace, _, cx| {
                this.text.pop();
                cx.notify();
            }))
            .child(
                div()
                    .p_2()
                    .border_b_1()
                    .border_color(rgb(0x3c3f41))
                    .child(SharedString::from(format!("> {}▏", self.text))),
            )
            .child(
                uniform_list(
                    "rows",
                    ROWS,
                    cx.processor(|_this, range: std::ops::Range<usize>, _window, _cx| {
                        range
                            .map(|ix| {
                                div()
                                    .h(px(22.))
                                    .flex()
                                    .flex_row()
                                    .child(div().w(px(80.)).text_color(rgb(0x808080)).child(format!("{}", ix + 1)))
                                    .children((0..COLS).map(move |c| {
                                        div().w(px(140.)).overflow_hidden().child(format!(
                                            "r{ix}c{c} {}",
                                            (ix * 2654435761 + c * 97) % 100000
                                        ))
                                    }))
                            })
                            .collect()
                    }),
                )
                .track_scroll(&self.scroll)
                .flex_1(),
            );
        if let Some(b) = self.bench.as_mut() {
            b.renders.push(t.elapsed());
        }
        el
    }
}

fn main() {
    application().run(|cx: &mut App| {
        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("backspace", Backspace, None),
        ]);
        cx.on_action(|_: &Quit, cx| cx.quit());
        let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
        let bench = std::env::var("SPIKE_BENCH").is_ok().then(|| Bench {
            started: None,
            last: None,
            intervals: vec![],
            renders: vec![],
            y: 0.0,
        });
        let window = cx
            .open_window(
                WindowOptions { window_bounds: Some(WindowBounds::Windowed(bounds)), ..Default::default() },
                |_, cx| {
                    cx.new(|cx| Spike {
                        scroll: UniformListScrollHandle::new(),
                        focus: cx.focus_handle(),
                        text: String::new(),
                        bench,
                        first_frame_seen: false,
                    })
                },
            )
            .unwrap();
        window
            .update(cx, |view, window, cx| {
                window.focus(&view.focus_handle(cx), cx);
                cx.activate(true);
            })
            .unwrap();
    });
}
