//! Release GPU frame benchmark measuring steady-state `p50`/`p99` CPU + Vulkan GPU timings
//! on the live adapter across 4 realistic `omarex` workloads:
//!
//! 1. Clean frame (0 dirty rows — `RowId` cache hit) on a 180x50 pane + 23-tab sidebar
//! 2. Partial update (1 dirty row — typing / streaming line) on a 180x50 pane + 23-tab sidebar
//! 3. Full 180x50 dirty redraw (e.g. `cat` / full-screen TUI scroll)
//! 4. 4-pane split layout (4x 90x25 panes) + floating `/pr` drawer + command palette modal
//!
//! Run with: `cargo run --release -p gpu-renderer --example bench_frame`

use std::path::Path;
use std::time::Instant;

use gpu_renderer::{
    CellFlags, Color, CornerRadii, CursorShape, DrawList, FontWeight, GpuConfig, GpuRenderer, Rect,
    SceneFrame, TerminalCursor, TerminalGridSnapshot, TerminalPaneCache, TerminalViewport, TextRun,
    layers,
};

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
const WARMUP_ITERS: usize = 20;
const SAMPLE_ITERS: usize = 120;

#[derive(Default)]
struct BenchSamples {
    build_us: Vec<u64>,
    submit_us: Vec<u64>,
    cpu_total_us: Vec<u64>,
    gpu_complete_us: Vec<u64>,
    last_draws: u32,
    last_quads: u32,
    last_glyphs: u32,
    last_rebuilt_rows: usize,
    last_cached_rows: usize,
}

impl BenchSamples {
    fn with_capacity(n: usize) -> Self {
        Self {
            build_us: Vec::with_capacity(n),
            submit_us: Vec::with_capacity(n),
            cpu_total_us: Vec::with_capacity(n),
            gpu_complete_us: Vec::with_capacity(n),
            ..Self::default()
        }
    }

    fn report(&mut self, name: &str) {
        self.build_us.sort_unstable();
        self.submit_us.sort_unstable();
        self.cpu_total_us.sort_unstable();
        self.gpu_complete_us.sort_unstable();

        let (b50, b99) = percentiles(&self.build_us);
        let (s50, s99) = percentiles(&self.submit_us);
        let (c50, c99) = percentiles(&self.cpu_total_us);
        let (g50, g99) = percentiles(&self.gpu_complete_us);

        println!("--- {name} ---");
        println!(
            "  CPU build+pane : p50 = {:>6} us ({:.3} ms) | p99 = {:>6} us ({:.3} ms)",
            b50,
            b50 as f64 / 1000.0,
            b99,
            b99 as f64 / 1000.0
        );
        println!(
            "  wgpu prep+subm : p50 = {:>6} us ({:.3} ms) | p99 = {:>6} us ({:.3} ms)",
            s50,
            s50 as f64 / 1000.0,
            s99,
            s99 as f64 / 1000.0
        );
        println!(
            "  CPU total      : p50 = {:>6} us ({:.3} ms) | p99 = {:>6} us ({:.3} ms)",
            c50,
            c50 as f64 / 1000.0,
            c99,
            c99 as f64 / 1000.0
        );
        println!(
            "  GPU wait (e2e) : p50 = {:>6} us ({:.3} ms) | p99 = {:>6} us ({:.3} ms)",
            g50,
            g50 as f64 / 1000.0,
            g99,
            g99 as f64 / 1000.0
        );
        println!(
            "  Workload       : draws={} quads={} glyphs={} rebuilt_rows={} cached_rows={}",
            self.last_draws,
            self.last_quads,
            self.last_glyphs,
            self.last_rebuilt_rows,
            self.last_cached_rows
        );
    }
}

fn percentiles(sorted: &[u64]) -> (u64, u64) {
    if sorted.is_empty() {
        return (0, 0);
    }
    let p50 = sorted[(sorted.len() - 1) * 50 / 100];
    let p99 = sorted[(sorted.len() - 1) * 99 / 100];
    (p50, p99)
}

fn make_dense_grid(cols: u16, rows: u16, id_offset: u64, tick: u64) -> TerminalGridSnapshot {
    let fg = Color::from_hex(0xffffff);
    let sec = Color::from_hex(0xbfbfbf);
    let muted = Color::from_hex(0x9e9e9e);
    let accent = Color::from_hex(0xe28203);
    let green = Color::from_hex(0x4ade80);
    let blue = Color::from_hex(0x4e8df5);
    let red = Color::from_hex(0xef3827);
    let badge_bg = Color::from_hex(0x1f2a1f);
    let err_bg = Color::from_hex(0x2a1414);

    let mut grid = TerminalGridSnapshot::new();
    for r in 0..rows {
        let row_id = id_offset + u64::from(r) + 1;
        let line_no = format!("{:>4} ", r + 1);
        let fill_len = usize::from(cols).saturating_sub(74);
        let filler: String = "─".repeat(fill_len.min(110));

        match r % 5 {
            0 => {
                let tail = format!("tick={tick:06} | status=streaming | latency=0.42ms ");
                grid.push_styled_row(
                    row_id,
                    i32::from(r),
                    true,
                    [
                        ("│ ", accent, None, CellFlags::NONE),
                        (line_no.as_str(), muted, None, CellFlags::NONE),
                        ("INFO ", green, Some(badge_bg), CellFlags::BOLD),
                        (
                            " omarex::workspace synced 23 T3 Code tabs via session.bootstrap ",
                            fg,
                            None,
                            CellFlags::NONE,
                        ),
                        (tail.as_str(), sec, None, CellFlags::NONE),
                    ],
                );
            }
            1 => {
                grid.push_styled_row(
                    row_id,
                    i32::from(r),
                    true,
                    [
                        ("├", accent, None, CellFlags::NONE),
                        (filler.as_str(), accent, None, CellFlags::NONE),
                        ("┤", accent, None, CellFlags::NONE),
                    ],
                );
            }
            2 => {
                grid.push_styled_row(
                    row_id,
                    i32::from(r),
                    true,
                    [
                        ("│ ", accent, None, CellFlags::NONE),
                        (line_no.as_str(), muted, None, CellFlags::NONE),
                        ("fn ", blue, None, CellFlags::BOLD),
                        ("render_frame", accent, None, CellFlags::BOLD),
                        (
                            "(&mut self, frame: &SceneFrame) -> Result<FrameStats, RenderError> {",
                            fg,
                            None,
                            CellFlags::NONE,
                        ),
                        (" // Vulkan 1.4", muted, None, CellFlags::ITALIC),
                    ],
                );
            }
            3 => {
                grid.push_styled_row(
                    row_id,
                    i32::from(r),
                    true,
                    [
                        ("│ ", accent, None, CellFlags::NONE),
                        (line_no.as_str(), muted, None, CellFlags::NONE),
                        ("WARN ", red, Some(err_bg), CellFlags::BOLD),
                        (" deprecated_symbol ", red, None, CellFlags::CURLY_UNDERLINE),
                        (
                            " https://superlogical.com/rex/docs/actions.md ",
                            blue,
                            None,
                            CellFlags::HYPERLINK,
                        ),
                        ("▁▂▃▄▅▆▇█", green, None, CellFlags::NONE),
                    ],
                );
            }
            _ => {
                grid.push_styled_row(
                    row_id,
                    i32::from(r),
                    true,
                    [
                        ("│ ", accent, None, CellFlags::NONE),
                        (line_no.as_str(), muted, None, CellFlags::NONE),
                        (
                            "let elapsed = t_end.duration_since(t_start).as_micros(); // selected",
                            fg,
                            None,
                            if r == 4 {
                                CellFlags::SELECTED
                            } else {
                                CellFlags::NONE
                            },
                        ),
                    ],
                );
            }
        }
    }

    grid.set_cursor(Some(TerminalCursor {
        col: ((tick as u16) % cols.max(1)).min(cols.saturating_sub(1)),
        row: i32::from(rows.saturating_sub(1)),
        shape: CursorShape::Bar,
        color: accent,
        text_color: Color::from_hex(0x121212),
        visible: true,
    }));
    grid
}

fn push_sidebar_23_tabs(dl: &mut DrawList) {
    let c_frame = Color::from_hex(0x0e0e0e);
    let c_row_active = Color::from_hex(0x1e1e1e);
    let c_icon_inactive = Color::from_hex(0x262626);
    let c_fg = Color::from_hex(0xffffff);
    let c_fg_sec = Color::from_hex(0xbfbfbf);
    let c_fg_muted = Color::from_hex(0x9e9e9e);
    let c_accent = Color::from_hex(0xe28203);
    let c_claude = Color::from_hex(0xd96b43);
    let c_red = Color::from_hex(0xef3827);
    let c_green = Color::from_hex(0x4ade80);
    let c_border_hair = Color::from_hex_alpha(0xffffff, 0.08);
    let c_border_subtle = Color::from_hex_alpha(0xffffff, 0.14);

    dl.with_layer(layers::CANVAS, |dl| {
        dl.push_rect(Rect::new(0.0, 0.0, 248.0, HEIGHT as f32), c_frame);
        dl.push_rect(Rect::new(247.0, 0.0, 1.0, HEIGHT as f32), c_border_hair);
    });

    dl.with_layer(layers::CHROME, |dl| {
        dl.push_icon("sidebar", Rect::new(12.0, 10.0, 16.0, 16.0), Some(c_fg_sec));
        dl.push_icon(
            "activity",
            Rect::new(194.0, 10.0, 16.0, 16.0),
            Some(c_fg_sec),
        );
        dl.push_icon("plus", Rect::new(220.0, 10.0, 16.0, 16.0), Some(c_fg_sec));

        dl.push_text(
            TextRun::new("This Machine", 14.0, 40.0, 12.5, c_fg).weight(FontWeight::Medium),
        );
        dl.push_text(
            TextRun::new("T3 Code · 23 threads", 14.0, 62.0, 11.0, c_fg_muted)
                .weight(FontWeight::SemiBold),
        );

        let icons = ["claude", "openai", "gemini", "terminal"];
        let subtitles = [
            "omarex · Needs you",
            "rex-proto · Working",
            "vt-engine · Working",
            "ui-chrome · Done",
        ];
        dl.with_clip(Rect::new(0.0, 82.0, 246.0, HEIGHT as f32 - 118.0), |dl| {
            for i in 0..23usize {
                let y = 84.0 + (i as f32) * 44.0;
                if y > HEIGHT as f32 - 36.0 {
                    break;
                }
                let active = i == 0;
                if active {
                    dl.push_rounded_rect(
                        Rect::new(8.0, y, 232.0, 42.0),
                        CornerRadii::uniform(7.0),
                        c_row_active,
                        c_border_subtle,
                        1.0,
                    );
                }
                let tile_bg = if active { c_claude } else { c_icon_inactive };
                dl.push_rounded_rect(
                    Rect::new(16.0, y + 8.0, 26.0, 26.0),
                    CornerRadii::uniform(6.0),
                    tile_bg,
                    Color::TRANSPARENT,
                    0.0,
                );
                dl.push_icon(
                    icons[i % icons.len()],
                    Rect::new(21.0, y + 13.0, 16.0, 16.0),
                    Some(if active { c_fg } else { c_fg_sec }),
                );
                if i < 4 {
                    let dot = match i {
                        0 | 1 => c_accent,
                        2 => c_red,
                        _ => c_green,
                    };
                    dl.push_rounded_rect(
                        Rect::new(37.0, y + 5.0, 7.0, 7.0),
                        CornerRadii::uniform(3.5),
                        dot,
                        if active { c_row_active } else { c_frame },
                        1.5,
                    );
                }
                dl.push_text(
                    TextRun::new(
                        format!("Thread #{:02} · GPU frame pipeline", i + 1),
                        50.0,
                        y + 5.0,
                        12.0,
                        if active { c_fg } else { c_fg_sec },
                    )
                    .weight(FontWeight::Medium),
                );
                dl.push_text(TextRun::new(
                    subtitles[i % subtitles.len()],
                    50.0,
                    y + 22.0,
                    10.5,
                    c_fg_muted,
                ));
                dl.push_text(
                    TextRun::new(format!("{}m", i + 1), 212.0, y + 22.0, 10.0, c_fg_muted)
                        .monospace(),
                );
            }
        });
    });
}

fn push_floating_drawer_and_modal(dl: &mut DrawList) {
    let c_pane = Color::from_hex(0x121212);
    let c_modal = Color::from_hex(0x181818);
    let c_fg = Color::from_hex(0xffffff);
    let c_fg_sec = Color::from_hex(0xbfbfbf);
    let c_fg_muted = Color::from_hex(0x9e9e9e);
    let c_accent = Color::from_hex(0xe28203);
    let c_border_subtle = Color::from_hex_alpha(0xffffff, 0.16);
    let c_border_hair = Color::from_hex_alpha(0xffffff, 0.08);

    // Floating `/pr` right-side drawer (`[0.45, 0.0, 0.55, 1.0]` over workspace)
    let drawer = Rect::new(1000.0, 8.0, 912.0, HEIGHT as f32 - 16.0);
    dl.with_layer(layers::FLOATING, |dl| {
        dl.push_rect(
            Rect::new(248.0, 0.0, WIDTH as f32 - 248.0, HEIGHT as f32),
            Color::rgba(0.0, 0.0, 0.0, 0.22),
        );
        dl.push_shadow(
            drawer,
            CornerRadii::uniform(10.0),
            Color::rgba(0.0, 0.0, 0.0, 0.50),
            28.0,
            -8.0,
            0.0,
        );
        dl.push_rounded_rect(
            drawer,
            CornerRadii::uniform(10.0),
            c_pane,
            c_accent.with_alpha(0.65),
            1.5,
        );
        dl.push_text(
            TextRun::new(
                "Pull Request #42 · Native Vulkan Renderer + tr3x Rev 24",
                drawer.x + 16.0,
                drawer.y + 12.0,
                13.0,
                c_fg,
            )
            .weight(FontWeight::SemiBold),
        );
        dl.push_rect(
            Rect::new(drawer.x + 1.5, drawer.y + 36.0, drawer.width - 3.0, 1.0),
            c_border_hair,
        );
        for line in 0..18usize {
            dl.push_text(
                TextRun::new(
                    format!(
                        "+ crates/gpu-renderer/src/lib.rs:{:>3}  instanced draw call verified",
                        line * 12 + 1
                    ),
                    drawer.x + 16.0,
                    drawer.y + 48.0 + (line as f32) * 20.0,
                    12.5,
                    if line % 3 == 0 {
                        Color::from_hex(0x4ade80)
                    } else {
                        c_fg_sec
                    },
                )
                .monospace(),
            );
        }
    });

    // Command Palette Modal on top
    let modal = Rect::new(660.0, 190.0, 600.0, 420.0);
    dl.with_layer(layers::MODAL, |dl| {
        dl.push_rect(
            Rect::new(0.0, 0.0, WIDTH as f32, HEIGHT as f32),
            Color::rgba(0.0, 0.0, 0.0, 0.35),
        );
        dl.push_shadow(
            modal,
            CornerRadii::uniform(12.0),
            Color::rgba(0.0, 0.0, 0.0, 0.58),
            36.0,
            0.0,
            16.0,
        );
        dl.push_rounded_rect(
            modal,
            CornerRadii::uniform(12.0),
            c_modal,
            c_border_subtle,
            1.0,
        );
        dl.push_icon(
            "search",
            Rect::new(modal.x + 16.0, modal.y + 15.0, 16.0, 16.0),
            Some(c_fg_muted),
        );
        dl.push_text(TextRun::new(
            "Type a command or thread…",
            modal.x + 42.0,
            modal.y + 14.0,
            14.0,
            c_fg_muted,
        ));
        dl.push_rect(
            Rect::new(modal.x + 1.0, modal.y + 46.0, modal.width - 2.0, 1.0),
            c_border_hair,
        );
        for row in 0..8usize {
            let ry = modal.y + 56.0 + (row as f32) * 42.0;
            if row == 0 {
                dl.push_rounded_rect(
                    Rect::new(modal.x + 8.0, ry, modal.width - 16.0, 38.0),
                    CornerRadii::uniform(7.0),
                    Color::from_hex(0x2a2a2a),
                    c_accent.with_alpha(0.5),
                    1.0,
                );
            }
            dl.push_icon(
                "terminal",
                Rect::new(modal.x + 18.0, ry + 11.0, 15.0, 15.0),
                Some(if row == 0 { c_accent } else { c_fg_sec }),
            );
            dl.push_text(
                TextRun::new(
                    format!("Action {:02}: T3 Code / Workspace Command", row + 1),
                    modal.x + 42.0,
                    ry + 10.0,
                    13.0,
                    if row == 0 { c_fg } else { c_fg_sec },
                )
                .weight(if row == 0 {
                    FontWeight::Medium
                } else {
                    FontWeight::Regular
                }),
            );
        }
    });
}

fn run_scenario<F>(
    name: &str,
    renderer: &mut GpuRenderer,
    target_view: &wgpu::TextureView,
    target_format: wgpu::TextureFormat,
    mut build_frame: F,
) where
    F: FnMut(&mut GpuRenderer, usize) -> (SceneFrame, usize, usize),
{
    for i in 0..WARMUP_ITERS {
        let (frame, _, _) = build_frame(renderer, i);
        let _ = renderer
            .render_to_view(&frame, target_view, target_format)
            .expect("warmup render");
        renderer
            .device()
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .expect("warmup poll");
    }

    let mut samples = BenchSamples::with_capacity(SAMPLE_ITERS);
    for i in 0..SAMPLE_ITERS {
        let t0 = Instant::now();
        let (frame, rebuilt, cached) = build_frame(renderer, WARMUP_ITERS + i);
        let t1 = Instant::now();

        let stats = renderer
            .render_to_view(&frame, target_view, target_format)
            .expect("bench render");
        let t2 = Instant::now();

        renderer
            .device()
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .expect("gpu wait poll");
        let t3 = Instant::now();

        samples
            .build_us
            .push(t1.duration_since(t0).as_micros() as u64);
        samples
            .submit_us
            .push(t2.duration_since(t1).as_micros() as u64);
        samples
            .cpu_total_us
            .push(t2.duration_since(t0).as_micros() as u64);
        samples
            .gpu_complete_us
            .push(t3.duration_since(t1).as_micros() as u64);
        samples.last_draws = stats.draw_calls;
        samples.last_quads = stats.quad_count;
        samples.last_glyphs = stats.glyph_count;
        samples.last_rebuilt_rows = rebuilt;
        samples.last_cached_rows = cached;
    }

    samples.report(name);
}

fn main() {
    let mut renderer =
        GpuRenderer::new_headless(GpuConfig::default()).expect("initialize Vulkan headless GPU");
    let icons_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("assets")
        .join("icons");
    if icons_dir.is_dir() {
        let _ = renderer.atlas_mut().load_icons_from_dir(&icons_dir);
    }

    let adapter = renderer.adapter_summary().clone();
    println!("=========================================================================");
    println!(
        "omarex gpu-renderer frame benchmark ({WIDTH}x{HEIGHT} @ 1.0x, {WARMUP_ITERS} warmup + {SAMPLE_ITERS} samples)"
    );
    println!(
        "Adapter : {} ({:?}, {:?}, {})",
        adapter.name, adapter.backend, adapter.device_type, adapter.driver
    );
    println!("=========================================================================");

    let offscreen = renderer
        .create_offscreen_target(WIDTH, HEIGHT)
        .expect("offscreen target");
    let metrics = renderer.atlas_mut().measure_monospace_cell(14.0, 1.0);

    let pane_rect = Rect::new(260.0, 12.0, 1648.0, 1056.0);
    let single_viewport = TerminalViewport {
        rect: Rect::new(
            pane_rect.x + 12.0,
            pane_rect.y + 10.0,
            pane_rect.width - 24.0,
            pane_rect.height - 20.0,
        ),
        clip_rect: pane_rect.to_clip_rect(),
        cols: 180,
        rows: 50,
        metrics,
        scroll_offset_y: 0.0,
        default_bg: Color::from_hex(0x121212),
        default_fg: Color::from_hex(0xffffff),
        selection_bg: Color::from_hex(0x2a2a2a),
        selection_fg: None,
        hyperlink_color: Color::from_hex(0x4e8df5),
        dim_alpha: 1.0,
    };

    // Scenario 1: Clean frame (0 dirty rows — RowId cache hit) on 180x50 + 23-tab sidebar
    {
        let mut grid = make_dense_grid(180, 50, 10_000, 1);
        let mut cache = TerminalPaneCache::new();
        // Prime cache on first pass then mark all rows clean.
        {
            let mut dl = DrawList::new(WIDTH, HEIGHT, 1.0);
            let queue = renderer.queue().clone();
            cache.build_pane(
                &grid,
                &single_viewport,
                renderer.atlas_mut(),
                &queue,
                &mut dl,
            );
        }
        grid.mark_all_clean();

        run_scenario(
            "1. Clean frame (0 dirty rows, 180x50 pane + 23-tab sidebar)",
            &mut renderer,
            &offscreen.view,
            offscreen.format,
            |r, _i| {
                let mut dl = DrawList::new(WIDTH, HEIGHT, 1.0);
                dl.set_clear_color(Color::from_hex(0x0e0e0e));
                push_sidebar_23_tabs(&mut dl);
                dl.with_layer(layers::CANVAS, |dl| {
                    dl.push_rounded_rect(
                        pane_rect,
                        CornerRadii::uniform(8.0),
                        Color::from_hex(0x121212),
                        Color::from_hex_alpha(0xe28203, 0.75),
                        1.5,
                    );
                });
                let queue = r.queue().clone();
                let st = cache.build_pane(&grid, &single_viewport, r.atlas_mut(), &queue, &mut dl);
                (dl.finish(), st.rebuilt_rows, st.cached_rows)
            },
        );
    }

    // Scenario 2: Partial update (1 dirty row — typing/streaming line) on 180x50 + 23-tab sidebar
    {
        let mut grid = make_dense_grid(180, 50, 20_000, 1);
        let mut cache = TerminalPaneCache::new();
        {
            let mut dl = DrawList::new(WIDTH, HEIGHT, 1.0);
            let queue = renderer.queue().clone();
            cache.build_pane(
                &grid,
                &single_viewport,
                renderer.atlas_mut(),
                &queue,
                &mut dl,
            );
        }
        grid.mark_all_clean();

        run_scenario(
            "2. Partial update (1 dirty row, 180x50 pane + 23-tab sidebar)",
            &mut renderer,
            &offscreen.view,
            offscreen.format,
            |r, i| {
                grid.mark_all_clean();
                grid.mark_row_dirty(i % 50);
                let mut dl = DrawList::new(WIDTH, HEIGHT, 1.0);
                dl.set_clear_color(Color::from_hex(0x0e0e0e));
                push_sidebar_23_tabs(&mut dl);
                dl.with_layer(layers::CANVAS, |dl| {
                    dl.push_rounded_rect(
                        pane_rect,
                        CornerRadii::uniform(8.0),
                        Color::from_hex(0x121212),
                        Color::from_hex_alpha(0xe28203, 0.75),
                        1.5,
                    );
                });
                let queue = r.queue().clone();
                let st = cache.build_pane(&grid, &single_viewport, r.atlas_mut(), &queue, &mut dl);
                (dl.finish(), st.rebuilt_rows, st.cached_rows)
            },
        );
    }

    // Scenario 3: Full 180x50 dirty redraw (every row dirty on every frame)
    {
        let mut cache = TerminalPaneCache::new();
        let grid = make_dense_grid(180, 50, 30_000, 7);

        run_scenario(
            "3. Full dirty redraw (50/50 dirty rows, 180x50 pane + 23-tab sidebar)",
            &mut renderer,
            &offscreen.view,
            offscreen.format,
            |r, _i| {
                let mut dl = DrawList::new(WIDTH, HEIGHT, 1.0);
                dl.set_clear_color(Color::from_hex(0x0e0e0e));
                push_sidebar_23_tabs(&mut dl);
                dl.with_layer(layers::CANVAS, |dl| {
                    dl.push_rounded_rect(
                        pane_rect,
                        CornerRadii::uniform(8.0),
                        Color::from_hex(0x121212),
                        Color::from_hex_alpha(0xe28203, 0.75),
                        1.5,
                    );
                });
                let queue = r.queue().clone();
                let st = cache.build_pane(&grid, &single_viewport, r.atlas_mut(), &queue, &mut dl);
                (dl.finish(), st.rebuilt_rows, st.cached_rows)
            },
        );
    }

    // Scenario 4: 4-pane split (4x 90x25) + floating /pr drawer + command palette modal
    {
        let pane_rects = [
            Rect::new(258.0, 10.0, 820.0, 524.0),
            Rect::new(1086.0, 10.0, 824.0, 524.0),
            Rect::new(258.0, 542.0, 820.0, 528.0),
            Rect::new(1086.0, 542.0, 824.0, 528.0),
        ];
        let viewports: Vec<TerminalViewport> = pane_rects
            .iter()
            .enumerate()
            .map(|(idx, &r)| TerminalViewport {
                rect: Rect::new(r.x + 10.0, r.y + 8.0, r.width - 20.0, r.height - 16.0),
                clip_rect: r.to_clip_rect(),
                cols: 90,
                rows: 25,
                metrics,
                scroll_offset_y: 0.0,
                default_bg: Color::from_hex(0x121212),
                default_fg: Color::from_hex(0xffffff),
                selection_bg: Color::from_hex(0x2a2a2a),
                selection_fg: None,
                hyperlink_color: Color::from_hex(0x4e8df5),
                dim_alpha: if idx == 0 { 1.0 } else { 0.78 },
            })
            .collect();
        let grids: Vec<TerminalGridSnapshot> = (0..4)
            .map(|p| make_dense_grid(90, 25, 40_000 + (p as u64) * 1000, p as u64 + 1))
            .collect();
        let mut caches: Vec<TerminalPaneCache> = (0..4).map(|_| TerminalPaneCache::new()).collect();

        run_scenario(
            "4. 4-pane split (4x 90x25, 100% dirty) + /pr drawer + Command Palette",
            &mut renderer,
            &offscreen.view,
            offscreen.format,
            |r, _i| {
                let mut dl = DrawList::new(WIDTH, HEIGHT, 1.0);
                dl.set_clear_color(Color::from_hex(0x0e0e0e));
                push_sidebar_23_tabs(&mut dl);
                let mut total_rebuilt = 0usize;
                let mut total_cached = 0usize;
                let queue = r.queue().clone();
                for p in 0..4 {
                    let rect = pane_rects[p];
                    dl.with_layer(layers::CANVAS, |dl| {
                        dl.push_rounded_rect(
                            rect,
                            CornerRadii::uniform(8.0),
                            Color::from_hex(0x121212),
                            if p == 0 {
                                Color::from_hex_alpha(0xe28203, 0.75)
                            } else {
                                Color::from_hex_alpha(0xffffff, 0.08)
                            },
                            if p == 0 { 1.5 } else { 1.0 },
                        );
                    });
                    let st = caches[p].build_pane(
                        &grids[p],
                        &viewports[p],
                        r.atlas_mut(),
                        &queue,
                        &mut dl,
                    );
                    total_rebuilt += st.rebuilt_rows;
                    total_cached += st.cached_rows;
                }
                push_floating_drawer_and_modal(&mut dl);
                (dl.finish(), total_rebuilt, total_cached)
            },
        );
    }
}
