//! A CAD-style viewport in a GPUI window: a custom `Element` whose content is
//! rendered directly by wgpu, under GPUI's own overlays.
//!
//! ```text
//! cargo run --release
//! cargo run --release -- --array 30 --spin --mode edges --exit-after 10
//! cargo run --release -- --offscreen out   # no window: PNGs, picking, frame times
//! ```

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!(
        "This demo attaches wgpu to a DirectComposition visual, so it runs on Windows. \
         gpui-fast's native_webview example shows the Linux surface handle."
    );
}

#[cfg(target_os = "windows")]
mod camera;
#[cfg(target_os = "windows")]
mod render;
#[cfg(target_os = "windows")]
mod scene;
#[cfg(target_os = "windows")]
mod ui;
#[cfg(target_os = "windows")]
mod viewport;

#[cfg(target_os = "windows")]
fn main() {
    app::run();
}

#[cfg(target_os = "windows")]
mod app {
    use std::cell::Cell;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use gpui::{App, AppContext as _, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};
    use gpui_platform::application;

    use crate::render::{DisplayMode, RenderOptions};
    use crate::ui::{CadApp, CadAppOptions, RunMetrics};
    use crate::viewport::Viewport;

    struct Args {
        array: u32,
        spin: bool,
        mode: DisplayMode,
        exit_after: Option<f32>,
        offscreen: Option<std::path::PathBuf>,
    }

    fn parse_args() -> Args {
        let mut args = Args {
            array: 1,
            spin: false,
            mode: DisplayMode::ShadedEdges,
            exit_after: None,
            offscreen: None,
        };
        let mut argv = std::env::args().skip(1);
        while let Some(flag) = argv.next() {
            match flag.as_str() {
                "--array" => args.array = argv.next().and_then(|v| v.parse().ok()).unwrap_or(1).max(1),
                "--spin" => args.spin = true,
                "--mode" => {
                    args.mode = match argv.next().as_deref() {
                        Some("shaded") => DisplayMode::Shaded,
                        Some("hidden") => DisplayMode::HiddenLine,
                        _ => DisplayMode::ShadedEdges,
                    }
                }
                "--exit-after" => args.exit_after = argv.next().and_then(|v| v.parse().ok()),
                "--offscreen" => args.offscreen = argv.next().map(Into::into),
                other => eprintln!("ignoring unknown argument {other}"),
            }
        }
        args
    }

    pub fn run() {
        let args = parse_args();
        let started = Instant::now();
        let model = Arc::new(crate::scene::gearbox());
        println!(
            "model: {} parts, {} triangles per assembly, built in {:?}",
            model.parts.len(),
            model.triangles_per_assembly(),
            started.elapsed()
        );
        if let Some(out_dir) = &args.offscreen {
            crate::render::run_offscreen(model, out_dir).expect("rendering off screen");
            return;
        }

        application().run(move |cx: &mut App| {
            let bounds = Bounds::centered(None, size(px(1360.), px(860.)), cx);
            let metrics = Rc::new(Cell::new(RunMetrics::default()));
            let (ui_sender, ui_receiver) = futures::channel::mpsc::unbounded();

            let result = cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("GPUI + wgpu · CAD 视口演示".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                {
                    let model = model.clone();
                    let metrics = metrics.clone();
                    move |window, cx| {
                        let viewport = Viewport::new(
                            window,
                            model.clone(),
                            ui_sender,
                            RenderOptions {
                                array: args.array,
                                spin: args.spin,
                                mode: args.mode,
                            },
                        )
                        .expect("creating the wgpu viewport");
                        println!("adapter: {}", viewport.adapter);
                        cx.new(|cx| {
                            CadApp::new(
                                viewport,
                                model,
                                ui_receiver,
                                CadAppOptions {
                                    array: args.array,
                                    spin: args.spin,
                                    mode: args.mode,
                                    metrics,
                                },
                                window,
                                cx,
                            )
                        })
                    }
                },
            );
            if let Err(error) = result {
                eprintln!("failed to open the window: {error:#}");
                cx.quit();
                return;
            }

            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            if let Some(seconds) = args.exit_after {
                let metrics = metrics.clone();
                cx.spawn(async move |cx| {
                    cx.background_executor()
                        .timer(Duration::from_secs_f32(seconds))
                        .await;
                    let measured = metrics.get();
                    let stats = measured.stats;
                    let rendering = measured
                        .ready_at
                        .map_or(0.0, |ready| ready.elapsed().as_secs_f32());
                    println!(
                        "after {seconds}s at {}x{}: wgpu {} frames, {:.1} fps average, {} triangles, \
                         {:.2} ms to encode; GPUI {} view rebuilds, worst UI thread stall {:.1} ms",
                        stats.width,
                        stats.height,
                        stats.frames_total,
                        stats.frames_total as f32 / rendering.max(0.001),
                        stats.triangles,
                        stats.encode_ms,
                        measured.ui_rebuilds,
                        measured.worst_ui_stall_ms
                    );
                    cx.update(|cx| cx.quit());
                })
                .detach();
            }
            cx.activate(true);
        });
    }
}
