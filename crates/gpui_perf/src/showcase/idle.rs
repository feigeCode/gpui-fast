//! `--idle`: small updates in a large, busy window, each for a fixed time.
//!
//! Most of what an application window does all day is change a few pixels: a
//! caret blinks, a clock ticks, a row lights up under the pointer, a spinner
//! turns, a few quotes change. Each scenario here shows the trading workspace
//! in a 1600×1000 window, thousands of primitives, and changes only one small
//! thing in it, at the rate a real application does, for `--duration`
//! seconds after `--warmup` seconds:
//!
//! - `CaretBlink`: the focused search box's caret blinks every 500 ms;
//! - `Clock`: a clock in the workspace's toolbar ticks every second;
//! - `Hover`: a watchlist row is drawn hovered and not, four changes a second
//!   (by state: the real pointer is left alone);
//! - `Spinner`: a 24 px spinner in the toolbar turns every frame, asking for
//!   each with `request_animation_frame`;
//! - `Quotes`: four watchlist rows in view get a quote five times a second,
//!   through the workspace's feed, as streamed quotes do;
//! - `Scroll`: the watchlist scrolls every frame, a change of most of the
//!   window;
//! - `Idle`: nothing changes.
//!
//! The application's own timers that would change the window — the status
//! bar's statistics and the unread count — are off, so only the scenario
//! changes it. Each scenario reports the frames drawn, the main thread's CPU
//! per frame (the mean over the run, and the median and 95th percentile of
//! the CPU between two updates divided by the frames drawn between them) and
//! the whole process's CPU over the run, as a share of one core.
//!
//! The process prints `gpui_perf idle: begin <Scenario> <unix time>` and
//! `... end ...` lines on stdout around each measured stretch, for
//! `script/measure-adaptive` to line up what it samples outside the process.
//! With `GPUI_RENDER_STATS` set, log records that are the renderer's
//! statistics are printed on stderr.

use std::{
    cell::{Cell, RefCell},
    f32::consts::TAU,
    io::Write as _,
    rc::Rc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use gpui::{
    AsyncWindowContext, Bounds, Context, Entity, IntoElement, Modifiers, PlatformInput, Render,
    ScrollDelta, ScrollWheelEvent, SharedString, Size, TouchPhase, WeakEntity, Window, canvas, div,
    fill, point, prelude::*, px, size,
};

use super::{
    Showcase, backend,
    metrics::{Cost, Sample, main_thread_cpu_time},
    theme::theme,
    workspace::{Watchlist, Workspace},
    workspace_page,
};

/// The window's size: a normal application window on a desktop monitor.
pub const WINDOW_SIZE: Size<gpui::Pixels> = Size {
    width: px(1600.),
    height: px(1000.),
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scenario {
    Idle,
    CaretBlink,
    Clock,
    Hover,
    Spinner,
    Quotes,
    Scroll,
}

/// In the order they run. `Scroll` is last: the wheel events it dispatches
/// leave the pointer over the watchlist.
const SCENARIOS: [Scenario; 7] = [
    Scenario::Idle,
    Scenario::CaretBlink,
    Scenario::Clock,
    Scenario::Hover,
    Scenario::Spinner,
    Scenario::Quotes,
    Scenario::Scroll,
];

impl Scenario {
    fn description(self) -> &'static str {
        match self {
            Scenario::Idle => "nothing changes",
            Scenario::CaretBlink => "the focused search box's caret blinks every 500 ms",
            Scenario::Clock => "a clock in the toolbar ticks every second",
            Scenario::Hover => "a watchlist row toggles hovered every 250 ms",
            Scenario::Spinner => "a 24 px spinner animates every frame",
            Scenario::Quotes => "4 watchlist rows in view get a quote every 200 ms",
            Scenario::Scroll => "the watchlist scrolls every frame",
        }
    }

    /// How often a timer changes the window, for the scenarios a timer
    /// drives; the others change it every frame, or never.
    fn interval(self) -> Option<Duration> {
        match self {
            Scenario::CaretBlink => Some(Duration::from_millis(500)),
            Scenario::Clock => Some(Duration::from_secs(1)),
            Scenario::Hover => Some(Duration::from_millis(250)),
            Scenario::Quotes => Some(Duration::from_millis(200)),
            Scenario::Idle | Scenario::Spinner | Scenario::Scroll => None,
        }
    }
}

/// Prints every scenario `--idle --only` can pick.
pub fn list() {
    for scenario in SCENARIOS {
        println!("{:<12} {}", format!("{scenario:?}"), scenario.description());
    }
}

struct Options {
    scenarios: Vec<Scenario>,
    duration: Duration,
    warmup: Duration,
    json: Option<String>,
}

fn options() -> Options {
    let args: Vec<String> = std::env::args().collect();
    let values = |name: &str| {
        args.iter()
            .enumerate()
            .filter(|(_, arg)| *arg == name)
            .filter_map(|(ix, _)| args.get(ix + 1).cloned())
            .collect::<Vec<_>>()
    };
    let seconds = |name: &str, default: f64| {
        values(name).last().map_or(default, |value| {
            value.parse::<f64>().unwrap_or_else(|_| {
                eprintln!("{name} needs a number of seconds, got {value:?}");
                std::process::exit(2);
            })
        })
    };
    let only: Vec<String> = values("--only")
        .iter()
        .flat_map(|value| value.split(','))
        .map(|name| name.trim().to_lowercase())
        .filter(|name| !name.is_empty())
        .collect();
    for name in &only {
        if !SCENARIOS
            .iter()
            .any(|scenario| format!("{scenario:?}").to_lowercase() == *name)
        {
            eprintln!("no idle scenario is called {name:?}; `--idle --list` prints them");
            std::process::exit(2);
        }
    }
    Options {
        scenarios: SCENARIOS
            .into_iter()
            .filter(|scenario| {
                only.is_empty() || only.contains(&format!("{scenario:?}").to_lowercase())
            })
            .collect(),
        duration: Duration::from_secs_f64(seconds("--duration", 20.).max(0.1)),
        warmup: Duration::from_secs_f64(seconds("--warmup", 2.).max(0.)),
        json: values("--json").last().cloned(),
    }
}

/// With `GPUI_RENDER_STATS` set, prints on stderr the log records that carry
/// the renderer's statistics, and warnings and errors, as nothing else in
/// `gpui_perf` installs a logger.
pub fn init_render_stats_log() {
    struct StatsLog;

    impl log::Log for StatsLog {
        fn enabled(&self, metadata: &log::Metadata) -> bool {
            metadata.level() <= log::Level::Info
        }

        fn log(&self, record: &log::Record) {
            let message = record.args().to_string();
            if message.contains("render stats") {
                eprintln!("{message}");
            } else if record.level() <= log::Level::Warn {
                eprintln!("[{} {}] {message}", record.level(), record.target());
            }
        }

        fn flush(&self) {}
    }

    if std::env::var("GPUI_RENDER_STATS").is_ok_and(|value| !value.is_empty() && value != "0")
        && log::set_boxed_logger(Box::new(StatsLog)).is_ok()
    {
        log::set_max_level(log::LevelFilter::Info);
    }
}

/// The main thread's CPU time and the frames drawn, taken each time the
/// scenario changes the window: the CPU between two takes over the frames
/// drawn between them is what each of those frames cost, the timer or event
/// that asked for it included.
#[derive(Default)]
struct Probe {
    recording: bool,
    last: Option<(Duration, u64)>,
    per_frame_ms: Vec<f64>,
    updates: u64,
}

type SharedProbe = Rc<RefCell<Probe>>;

impl Probe {
    fn take(&mut self, window: &Window) {
        if !self.recording {
            return;
        }
        self.updates += 1;
        let now = (main_thread_cpu_time(), backend::frames(window));
        match self.last {
            Some((cpu, frames)) if now.1 > frames => {
                let drawn = now.1 - frames;
                let ms = (now.0 - cpu).as_secs_f64() * 1e3 / drawn as f64;
                self.per_frame_ms
                    .extend(std::iter::repeat_n(ms, drawn as usize));
            }
            // No frame drawn since: its CPU counts toward the next one.
            Some(_) => return,
            None => {}
        }
        self.last = Some(now);
    }

    fn start(&mut self) {
        *self = Self {
            recording: true,
            ..Self::default()
        };
    }
}

/// The toolbar's clock and spinner, shown in every scenario so that the
/// window is the same in all of them; only the scenario's own one changes.
struct Widgets {
    clock: SharedString,
    spinning: bool,
    started: Instant,
    probe: SharedProbe,
}

fn clock_text() -> SharedString {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
        % 86_400;
    format!(
        "{:02}:{:02}:{:02} UTC",
        seconds / 3_600,
        seconds / 60 % 60,
        seconds % 60
    )
    .into()
}

impl Render for Widgets {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let spinning = self.spinning;
        if spinning {
            window.request_animation_frame();
        }
        // Turns per second: 1.25, smoothly, so every frame differs.
        let turn = if spinning {
            (self.started.elapsed().as_secs_f32() * 1.25).fract()
        } else {
            0.
        };
        let probe = self.probe.clone();
        let color = theme.foreground;
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .text_color(theme.muted_foreground)
                    .child(self.clock.clone()),
            )
            .child(
                canvas(
                    |_, _, _| {},
                    move |bounds, _, window, _| {
                        if spinning {
                            probe.borrow_mut().take(window);
                        }
                        paint_spinner(bounds, turn, color, window);
                    },
                )
                .flex_shrink_0()
                .size(px(24.)),
            )
    }
}

/// Eight dots around a circle, the brightest at `turn`, fading behind it.
fn paint_spinner(bounds: Bounds<gpui::Pixels>, turn: f32, color: gpui::Hsla, window: &mut Window) {
    const DOTS: usize = 8;
    let center = bounds.center();
    let dot = px(4.);
    for ix in 0..DOTS {
        let angle = ix as f32 / DOTS as f32 * TAU;
        let behind = (turn * DOTS as f32 - ix as f32).rem_euclid(DOTS as f32) / DOTS as f32;
        let origin = point(
            center.x + px(8. * angle.cos()) - dot / 2.,
            center.y + px(8. * angle.sin()) - dot / 2.,
        );
        window.paint_quad(
            fill(
                Bounds::new(origin, size(dot, dot)),
                color.opacity(1. - 0.85 * behind),
            )
            .corner_radii(dot / 2.),
        );
    }
}

/// What the scenarios change.
struct Parts {
    workspace: Entity<Workspace>,
    watchlist: Entity<Watchlist>,
    widgets: Entity<Widgets>,
    probe: SharedProbe,
    scrolling: Rc<Cell<bool>>,
}

/// One scenario's results.
struct Outcome {
    scenario: Scenario,
    seconds: f64,
    frames: u64,
    updates: u64,
    cost: Cost,
    per_frame_ms: Vec<f64>,
    began_at: f64,
    ended_at: f64,
}

/// Starts the scenarios, once the window is open.
pub fn start(window: &mut Window, cx: &mut Context<Showcase>) {
    let options = options();
    cx.spawn_in(window, async move |this, cx| {
        if run(options, this, cx).await.is_none() {
            eprintln!("gpui_perf idle: the window closed before the scenarios finished");
        }
    })
    .detach();
}

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0., |since| since.as_secs_f64())
}

async fn sleep_until(deadline: Instant, cx: &mut AsyncWindowContext) {
    let now = Instant::now();
    if deadline > now {
        cx.background_executor().timer(deadline - now).await;
    }
}

async fn run(
    options: Options,
    this: WeakEntity<Showcase>,
    cx: &mut AsyncWindowContext,
) -> Option<()> {
    let probe = SharedProbe::default();
    let parts = this
        .update_in(cx, |showcase, window, cx| {
            showcase.select(workspace_page(), cx);
            showcase.focus_page(window, cx);
            let workspace = showcase.container.read(cx).workspace.clone()?;
            let widgets = cx.new(|_| Widgets {
                clock: clock_text(),
                spinning: false,
                started: Instant::now(),
                probe: probe.clone(),
            });
            let watchlist = workspace.update(cx, |workspace, cx| {
                workspace.toolbar_extra = Some(widgets.clone().into());
                workspace.search.update(cx, |search, cx| {
                    search.caret = Some(true);
                    cx.notify();
                });
                cx.notify();
                workspace.watchlist.clone()
            });
            Some(Parts {
                workspace,
                watchlist,
                widgets,
                probe: probe.clone(),
                scrolling: Rc::default(),
            })
        })
        .ok()??;
    // The first frames lay out the workspace and fill the atlas.
    cx.background_executor().timer(Duration::from_secs(1)).await;

    let mut outcomes = Vec::new();
    for &scenario in &options.scenarios {
        outcomes.push(measure(scenario, &options, &parts, cx).await?);
    }

    let viewport = cx
        .update(|window, _| (window.viewport_size(), window.scale_factor()))
        .ok()?;
    report(&outcomes, viewport);
    if let Some(path) = &options.json
        && let Err(error) = std::fs::write(path, json(&outcomes, viewport))
    {
        eprintln!("could not write {path}: {error}");
    }

    // As `--auto` does: blur the search box and draw once more, so that the
    // window gives its input handler back, then close and quit.
    cx.update(|window, cx| window.blur(cx)).ok()?;
    cx.background_executor()
        .timer(Duration::from_millis(200))
        .await;
    cx.update(|window, cx| {
        window.remove_window();
        cx.quit();
    })
    .ok()
}

async fn measure(
    scenario: Scenario,
    options: &Options,
    parts: &Parts,
    cx: &mut AsyncWindowContext,
) -> Option<Outcome> {
    cx.update(|window, cx| begin(scenario, parts, window, cx))
        .ok()?;
    let started = Instant::now();
    let measure_from = started + options.warmup;
    let measure_to = measure_from + options.duration;
    let mut first: Option<(Sample, Instant, f64)> = None;
    let mut tick = 0u32;
    let outcome = loop {
        let next_tick = scenario
            .interval()
            .map(|interval| started + interval * (tick + 1));
        let mark = if first.is_none() {
            measure_from
        } else {
            measure_to
        };
        if let Some(next_tick) = next_tick.filter(|&next_tick| next_tick < mark) {
            sleep_until(next_tick, cx).await;
            tick += 1;
            cx.update(|window, cx| update(scenario, tick, parts, window, cx))
                .ok()?;
            continue;
        }
        sleep_until(mark, cx).await;
        let sample = cx.update(|window, _| Sample::take(window)).ok()?;
        match first.take() {
            None => {
                parts.probe.borrow_mut().start();
                let at = unix_now();
                println!("gpui_perf idle: begin {scenario:?} {at:.3}");
                std::io::stdout().flush().ok();
                first = Some((sample, Instant::now(), at));
            }
            Some((from, from_instant, began_at)) => {
                let ended_at = unix_now();
                println!("gpui_perf idle: end {scenario:?} {ended_at:.3}");
                std::io::stdout().flush().ok();
                let mut probe = parts.probe.borrow_mut();
                probe.recording = false;
                let cost = Cost::between(&from, &sample);
                let seconds = from_instant.elapsed().as_secs_f64();
                break Outcome {
                    scenario,
                    seconds,
                    frames: (cost.fps * seconds).round() as u64,
                    updates: probe.updates,
                    cost,
                    per_frame_ms: std::mem::take(&mut probe.per_frame_ms),
                    began_at,
                    ended_at,
                };
            }
        }
    };
    cx.update(|window, cx| finish(scenario, parts, window, cx))
        .ok()?;
    // Lets the last change draw before the next scenario starts.
    cx.background_executor()
        .timer(Duration::from_millis(300))
        .await;
    Some(outcome)
}

/// Starts what a scenario changes every frame.
fn begin(scenario: Scenario, parts: &Parts, window: &mut Window, cx: &mut gpui::App) {
    match scenario {
        Scenario::Spinner => parts.widgets.update(cx, |widgets, cx| {
            widgets.spinning = true;
            widgets.started = Instant::now();
            cx.notify();
        }),
        Scenario::Scroll => {
            parts.scrolling.set(true);
            scroll_frames(
                parts.scrolling.clone(),
                Rc::new(Cell::new(1.)),
                parts.watchlist.clone(),
                parts.probe.clone(),
                window,
            );
        }
        _ => {}
    }
}

/// One change of a scenario a timer drives, the `tick`th.
fn update(scenario: Scenario, tick: u32, parts: &Parts, window: &mut Window, cx: &mut gpui::App) {
    parts.probe.borrow_mut().take(window);
    match scenario {
        Scenario::CaretBlink => {
            let search = parts.workspace.read(cx).search.clone();
            search.update(cx, |search, cx| {
                search.caret = Some(tick.is_multiple_of(2));
                cx.notify();
            });
        }
        Scenario::Clock => parts.widgets.update(cx, |widgets, cx| {
            widgets.clock = clock_text();
            cx.notify();
        }),
        Scenario::Hover => parts.watchlist.update(cx, |watchlist, cx| {
            watchlist.highlighted = (!tick.is_multiple_of(2)).then_some(3);
            cx.notify();
        }),
        Scenario::Quotes => {
            // Rows of the first screen, not the quote panels' symbol, whose
            // quotes redraw those panels too.
            const ROWS: [usize; 15] = [0, 1, 2, 3, 4, 5, 6, 8, 9, 10, 11, 12, 13, 14, 15];
            let symbols: Vec<usize> = (0..4)
                .map(|ix| ROWS[(tick as usize * 4 + ix * 4 + ix) % ROWS.len()])
                .collect();
            parts
                .workspace
                .update(cx, |workspace, cx| workspace.push_quotes(&symbols, cx));
        }
        Scenario::Idle | Scenario::Spinner | Scenario::Scroll => {}
    }
}

/// Puts the window back as every scenario starts from.
fn finish(scenario: Scenario, parts: &Parts, _: &mut Window, cx: &mut gpui::App) {
    match scenario {
        Scenario::CaretBlink => {
            let search = parts.workspace.read(cx).search.clone();
            search.update(cx, |search, cx| {
                search.caret = Some(true);
                cx.notify();
            });
        }
        Scenario::Hover => parts.watchlist.update(cx, |watchlist, cx| {
            watchlist.highlighted = None;
            cx.notify();
        }),
        Scenario::Spinner => parts.widgets.update(cx, |widgets, cx| {
            widgets.spinning = false;
            cx.notify();
        }),
        Scenario::Scroll => {
            parts.scrolling.set(false);
            parts.watchlist.update(cx, |watchlist, cx| {
                let handle = watchlist.scroll.0.borrow().base_handle.clone();
                handle.set_offset(point(px(0.), px(0.)));
                cx.notify();
            });
        }
        Scenario::Idle | Scenario::Clock | Scenario::Quotes => {}
    }
}

/// Scrolls the watchlist by a wheel event over it every frame, turning back
/// at either end, while `running`.
fn scroll_frames(
    running: Rc<Cell<bool>>,
    direction: Rc<Cell<f32>>,
    watchlist: Entity<Watchlist>,
    probe: SharedProbe,
    window: &mut Window,
) {
    window.on_next_frame(move |window, cx| {
        if !running.get() {
            return;
        }
        probe.borrow_mut().take(window);
        let handle = watchlist.read(cx).scroll.0.borrow().base_handle.clone();
        let (offset, max) = (-handle.offset().y, handle.max_offset().y);
        let speed = px(32.) * direction.get();
        if offset + speed >= max {
            direction.set(-1.);
        } else if offset + speed <= px(0.) {
            direction.set(1.);
        }
        window.dispatch_event(
            PlatformInput::ScrollWheel(ScrollWheelEvent {
                position: handle.bounds().center(),
                delta: ScrollDelta::Pixels(point(px(0.), -speed)),
                modifiers: Modifiers::default(),
                touch_phase: TouchPhase::Moved,
            }),
            cx,
        );
        scroll_frames(running, direction, watchlist, probe, window);
    });
}

fn percentile(sorted: &[f64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    sorted
        .get(((sorted.len() as f64 - 1.) * p).round() as usize)
        .copied()
}

fn sorted(values: &[f64]) -> Vec<f64> {
    let mut values = values.to_vec();
    values.sort_by(f64::total_cmp);
    values
}

fn report(outcomes: &[Outcome], (viewport, scale): (Size<gpui::Pixels>, f32)) {
    println!(
        "\nwindow {:.0}x{:.0} logical, scale {scale}",
        f32::from(viewport.width),
        f32::from(viewport.height)
    );
    println!(
        "{:<11} {:>6} {:>7} {:>6} {:>7} {:>9} {:>9} {:>9} {:>9} {:>8} {:>7}",
        "scenario",
        "secs",
        "frames",
        "fps",
        "updates",
        "main/frm",
        "main p50",
        "main p95",
        "gpui/frm",
        "process",
        "memory"
    );
    let ms = |value: Option<f64>| value.map_or("-".to_string(), |v| format!("{v:.2}ms"));
    for outcome in outcomes {
        let cost = &outcome.cost;
        let samples = sorted(&outcome.per_frame_ms);
        println!(
            "{:<11} {:>6.1} {:>7} {:>6.1} {:>7} {:>9} {:>9} {:>9} {:>9} {:>7.1}% {:>7}",
            format!("{:?}", outcome.scenario),
            outcome.seconds,
            outcome.frames,
            cost.fps,
            outcome.updates,
            ms((outcome.frames > 0).then_some(cost.main_cpu_per_frame_ms)),
            ms(percentile(&samples, 0.5)),
            ms(percentile(&samples, 0.95)),
            ms(cost
                .phases
                .filter(|_| outcome.frames > 0)
                .map(|p| p.build_ms + p.prepaint_ms + p.paint_ms)),
            cost.process_cpu_percent,
            cost.memory_mib
                .map_or("-".to_string(), |mib| format!("{mib:.0}MB")),
        );
    }
    println!(
        "\nmain/frm: the main thread's CPU over the run per frame drawn. main p50/p95: the \
         main thread's CPU between two updates, per frame drawn between them. gpui/frm: GPUI's \
         build, prepaint and paint per frame. process: CPU of every thread, as a share of one \
         core."
    );
}

fn json(outcomes: &[Outcome], (viewport, scale): (Size<gpui::Pixels>, f32)) -> String {
    let results: Vec<serde_json::Value> = outcomes
        .iter()
        .map(|outcome| {
            let cost = &outcome.cost;
            let samples = sorted(&outcome.per_frame_ms);
            serde_json::json!({
                "scenario": format!("{:?}", outcome.scenario),
                "description": outcome.scenario.description(),
                "seconds": outcome.seconds,
                "began_at": outcome.began_at,
                "ended_at": outcome.ended_at,
                "frames": outcome.frames,
                "fps": cost.fps,
                "updates": outcome.updates,
                "process_cpu_percent": cost.process_cpu_percent,
                "process_cpu_seconds": cost.process_cpu_percent / 100. * outcome.seconds,
                "main_cpu_percent": cost.main_cpu_percent,
                "main_ms_per_frame": (outcome.frames > 0).then_some(cost.main_cpu_per_frame_ms),
                "main_ms_per_frame_p50": percentile(&samples, 0.5),
                "main_ms_per_frame_p95": percentile(&samples, 0.95),
                "main_ms_per_frame_samples": samples.len(),
                "gpui_ms_per_frame": cost.phases
                    .filter(|_| outcome.frames > 0)
                    .map(|p| p.build_ms + p.prepaint_ms + p.paint_ms),
                "memory_mib": cost.memory_mib,
            })
        })
        .collect();
    serde_json::to_string_pretty(&serde_json::json!({
        "window": {
            "width": f32::from(viewport.width),
            "height": f32::from(viewport.height),
            "scale": scale,
        },
        "results": results,
    }))
    .unwrap_or_default()
}
