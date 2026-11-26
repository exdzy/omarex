//! Release performance benchmark gate for `vt-engine` (`fast-anything` / `rust-anything`).
//!
//! Measures:
//! 1. VT parser write throughput (`MiB/s` on realistic 180x50 dense ANSI + Unicode box-drawing output)
//! 2. Two-phase render-state latency (`p50`/`p99` us):
//!    - `begin_update` lock hold time (<50 us target)
//!    - Clean frame skip (`DirtyState::Clean`)
//!    - 1-row partial update (`for_each_dirty_row`)
//!    - Full 180x50 pane extraction (`for_each_row`)
//! 3. Streaming `GHOSTSNP` time-to-`READY` on a 2,000-line deep scrollback snapshot (<5 ms target)

use std::hint::black_box;
use std::time::{Duration, Instant};

use vt_engine::{
    BackgroundStep, CellContent, DirtyState, Overscan, RenderState, SnapshotDecoder,
    SnapshotFeedStatus, Terminal, TerminalConfig,
};

fn build_tr3x_ansi_corpus(target_bytes: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(target_bytes + 4096);
    let mut line_no: usize = 0;
    while out.len() < target_bytes {
        let header = format!(
            "\x1b[38;2;137;180;250m╭──────────────────────────────────────────────────────────────────────────────────────────────╮\x1b[0m\r\n\
             \x1b[38;2;137;180;250m│\x1b[0m \x1b[1;38;2;205;214;244mClaude · omarex\x1b[0m \x1b[38;2;166;227;161m● working\x1b[0m  \x1b[38;2;147;153;178mstep {line_no:06} · crates/vt-engine/src/lib.rs:412\x1b[0m                           \x1b[38;2;137;180;250m│\x1b[0m\r\n\
             \x1b[38;2;137;180;250m╰──────────────────────────────────────────────────────────────────────────────────────────────╯\x1b[0m\r\n"
        );
        out.extend_from_slice(header.as_bytes());
        for j in 0..8 {
            let row = format!(
                "  \x1b[38;5;245m{n:05}\x1b[0m \x1b[38;2;166;227;161m+ pub fn render_dirty_row_{j}\x1b[0m(row: &DirtyRow<'_>, cells: &[CellData]) -> usize {{ black_box(cells.len()) }} // Unicode: äöüß ──▶ ✓\r\n",
                n = line_no + j
            );
            out.extend_from_slice(row.as_bytes());
        }
        line_no += 8;
    }
    out
}

fn summarize_us(samples_ns: &mut [u64]) -> (f64, f64, f64) {
    samples_ns.sort_unstable();
    let len = samples_ns.len();
    let p50 = samples_ns[len / 2] as f64 / 1_000.0;
    let p99 = samples_ns[((len * 99) / 100).min(len - 1)] as f64 / 1_000.0;
    let mean = (samples_ns.iter().copied().sum::<u64>() as f64) / (len as f64) / 1_000.0;
    (p50, mean, p99)
}

fn bench_vt_throughput() {
    const CORPUS_BYTES: usize = 8 * 1024 * 1024; // 8 MiB realistic tr3x ANSI/Unicode stream
    let corpus = build_tr3x_ansi_corpus(CORPUS_BYTES);

    let term = Terminal::new(
        180,
        50,
        TerminalConfig {
            scrollback_max_bytes: Some(16 * 1024 * 1024),
            ..TerminalConfig::default()
        },
    )
    .expect("terminal creation");

    // Warmup
    term.write(&corpus[..256 * 1024]);

    // Measure 64 KiB chunks (Rex /api/data native chunk size)
    let mut rates_64k = Vec::with_capacity(5);
    for _ in 0..5 {
        let start = Instant::now();
        for chunk in corpus.chunks(65_536) {
            term.write(black_box(chunk));
        }
        let elapsed = start.elapsed().as_secs_f64();
        let mib_s = (corpus.len() as f64 / (1024.0 * 1024.0)) / elapsed;
        rates_64k.push(mib_s);
    }
    rates_64k.sort_by(|a, b| a.total_cmp(b));
    let median_64k = rates_64k[rates_64k.len() / 2];

    // Also measure pure ASCII/SGR dense output throughput (classic vt benchmark comparison)
    let mut ascii_corpus = Vec::with_capacity(CORPUS_BYTES);
    let line = b"\x1b[32mINFO\x1b[0m 2026-11-26T09:30:00Z omarex::vt_engine: rendered 180x50 viewport in 0.42ms (dirty_rows=1, atlas_hits=9000, vram_mb=12.4)\r\n";
    while ascii_corpus.len() < CORPUS_BYTES {
        ascii_corpus.extend_from_slice(line);
    }
    let mut rates_ascii = Vec::with_capacity(5);
    for _ in 0..5 {
        let start = Instant::now();
        for chunk in ascii_corpus.chunks(65_536) {
            term.write(black_box(chunk));
        }
        let elapsed = start.elapsed().as_secs_f64();
        rates_ascii.push((ascii_corpus.len() as f64 / (1024.0 * 1024.0)) / elapsed);
    }
    rates_ascii.sort_by(|a, b| a.total_cmp(b));
    let median_ascii = rates_ascii[rates_ascii.len() / 2];

    println!("=== 1. VT Parser Write Throughput (180x50, 8 MiB / 64 KiB chunks) ===");
    println!(
        "  Dense tr3x ANSI + Unicode box-drawing : {median_64k:7.1} MiB/s (target >150 MiB/s)"
    );
    println!(
        "  Dense ASCII + SGR log stream          : {median_ascii:7.1} MiB/s (target >150 MiB/s)"
    );
}

fn bench_render_state() {
    let term = Terminal::new(
        180,
        50,
        TerminalConfig {
            scrollback_max_bytes: Some(8 * 1024 * 1024),
            ..TerminalConfig::default()
        },
    )
    .expect("terminal creation");

    // Fill all 50 rows x 180 cols with realistic styled tr3x content
    let fill = build_tr3x_ansi_corpus(32 * 1024);
    term.write(&fill);

    let mut rs = RenderState::new().expect("render state");
    let overscan = Overscan { above: 1, below: 1 };

    // Warmup
    rs.begin_update(&term, overscan).unwrap();
    rs.end_update().unwrap();
    rs.clean().unwrap();

    const ITERS: usize = 500;

    // A) Clean frame skip (begin_update + end_update + DirtyState::Clean check)
    let mut clean_begin_ns = Vec::with_capacity(ITERS);
    let mut clean_total_ns = Vec::with_capacity(ITERS);
    for _ in 0..ITERS {
        let t0 = Instant::now();
        let snap = rs.begin_update(&term, overscan).unwrap();
        let t1 = Instant::now();
        rs.end_update().unwrap();
        assert_eq!(rs.dirty_state(), DirtyState::Clean);
        let t2 = Instant::now();
        black_box(snap);
        clean_begin_ns.push((t1 - t0).as_nanos() as u64);
        clean_total_ns.push((t2 - t0).as_nanos() as u64);
    }

    // B) 1-row partial update (mutate 1 line -> begin_update -> end_update -> for_each_dirty_row -> clean)
    let mut partial_begin_ns = Vec::with_capacity(ITERS);
    let mut partial_end_ns = Vec::with_capacity(ITERS);
    let mut partial_extract_ns = Vec::with_capacity(ITERS);
    let mut partial_total_ns = Vec::with_capacity(ITERS);
    let mut checksum: u64 = 0;

    for i in 0..ITERS {
        let tick = format!(
            "\x1b[25;1H\x1b[38;2;166;227;161m● working\x1b[0m frame={i:05}  streaming token output..."
        );
        term.write(tick.as_bytes());

        let t0 = Instant::now();
        let snap = rs.begin_update(&term, overscan).unwrap();
        let t1 = Instant::now();
        rs.end_update().unwrap();
        let t2 = Instant::now();
        let mut dirty_rows = 0usize;
        rs.for_each_dirty_row(|row| {
            dirty_rows += 1;
            for c in row.cells {
                if let CellContent::Codepoint(ch) = c.content {
                    checksum = checksum.wrapping_add(ch as u64);
                }
            }
        })
        .unwrap();
        rs.clean().unwrap();
        let t3 = Instant::now();
        debug_assert!(dirty_rows >= 1);
        black_box((snap, dirty_rows));

        partial_begin_ns.push((t1 - t0).as_nanos() as u64);
        partial_end_ns.push((t2 - t1).as_nanos() as u64);
        partial_extract_ns.push((t3 - t2).as_nanos() as u64);
        partial_total_ns.push((t3 - t0).as_nanos() as u64);
    }

    // C) Full 180x50 pane extraction (50 viewport + 2 overscan rows = 9,360 cells)
    let mut full_begin_ns = Vec::with_capacity(200);
    let mut full_extract_ns = Vec::with_capacity(200);
    let mut full_total_ns = Vec::with_capacity(200);
    for i in 0..200 {
        // Scroll by 1 line so all rows redraw
        let line = format!(
            "\x1b[38;2;137;180;250m[{i:04}]\x1b[0m \x1b[1mfull-pane-row\x1b[0m ───────────────────────── status: ok\r\n"
        );
        term.write(line.as_bytes());

        let t0 = Instant::now();
        let snap = rs.begin_update(&term, overscan).unwrap();
        let t1 = Instant::now();
        rs.end_update().unwrap();
        let mut cell_count = 0usize;
        rs.for_each_row(|row| {
            cell_count += row.cells.len();
            for c in row.cells {
                if let CellContent::Codepoint(ch) = c.content {
                    checksum = checksum.wrapping_add(ch as u64);
                }
            }
        })
        .unwrap();
        rs.clean().unwrap();
        let t2 = Instant::now();
        black_box((snap, cell_count));

        full_begin_ns.push((t1 - t0).as_nanos() as u64);
        full_extract_ns.push((t2 - t1).as_nanos() as u64);
        full_total_ns.push((t2 - t0).as_nanos() as u64);
    }
    black_box(checksum);

    let (cb50, cb_m, cb99) = summarize_us(&mut clean_begin_ns);
    let (ct50, ct_m, ct99) = summarize_us(&mut clean_total_ns);
    let (pb50, pb_m, pb99) = summarize_us(&mut partial_begin_ns);
    let (pe50, pe_m, pe99) = summarize_us(&mut partial_end_ns);
    let (px50, px_m, px99) = summarize_us(&mut partial_extract_ns);
    let (pt50, pt_m, pt99) = summarize_us(&mut partial_total_ns);
    let (fb50, fb_m, fb99) = summarize_us(&mut full_begin_ns);
    let (fx50, fx_m, fx99) = summarize_us(&mut full_extract_ns);
    let (ft50, ft_m, ft99) = summarize_us(&mut full_total_ns);

    println!("\n=== 2. Two-Phase RenderState Timings (180x50 + overscan 1/1) ===");
    println!(
        "  Clean frame begin_update (lock) : p50={cb50:6.2} us  mean={cb_m:6.2} us  p99={cb99:6.2} us  (target <50 us)"
    );
    println!(
        "  Clean frame total skip          : p50={ct50:6.2} us  mean={ct_m:6.2} us  p99={ct99:6.2} us"
    );
    println!(
        "  1-row partial begin_update(lock): p50={pb50:6.2} us  mean={pb_m:6.2} us  p99={pb99:6.2} us  (target <50 us)"
    );
    println!(
        "  1-row partial end_update        : p50={pe50:6.2} us  mean={pe_m:6.2} us  p99={pe99:6.2} us"
    );
    println!(
        "  1-row partial dirty extract     : p50={px50:6.2} us  mean={px_m:6.2} us  p99={px99:6.2} us"
    );
    println!(
        "  1-row partial total             : p50={pt50:6.2} us  mean={pt_m:6.2} us  p99={pt99:6.2} us"
    );
    println!(
        "  Full 180x50 begin_update (lock) : p50={fb50:6.2} us  mean={fb_m:6.2} us  p99={fb99:6.2} us  (target <50 us)"
    );
    println!(
        "  Full 180x50 end+extract (9360c) : p50={fx50:6.2} us  mean={fx_m:6.2} us  p99={fx99:6.2} us"
    );
    println!(
        "  Full 180x50 total               : p50={ft50:6.2} us  mean={ft_m:6.2} us  p99={ft99:6.2} us"
    );
}

fn bench_snapshot_ready() {
    // Build a deep 2,000-line scrollback terminal snapshot (180x50)
    let src = Terminal::new(
        180,
        50,
        TerminalConfig {
            scrollback_max_bytes: Some(16 * 1024 * 1024),
            ..TerminalConfig::default()
        },
    )
    .expect("src terminal");

    for i in 0..2000 {
        let row = format!(
            "\x1b[38;2;137;180;250m[{i:04}]\x1b[0m \x1b[1;38;2;166;227;161m✓\x1b[0m \x1b[38;2;205;214;244mtr3x thread log line with realistic ANSI colors & box chars ──▶ crates/omarex/src/app.rs:{i}\x1b[0m\r\n"
        );
        src.write(row.as_bytes());
    }
    let sb = src.scrollbar().unwrap();
    let snap_bytes = src.encode_snapshot().expect("encode 2000-line snapshot");

    const RUNS: usize = 25;
    let mut ready_ns = Vec::with_capacity(RUNS);
    let mut total_bg_ns = Vec::with_capacity(RUNS);
    let mut bg_pages = 0usize;

    for _ in 0..RUNS {
        let mut dec = SnapshotDecoder::new(TerminalConfig::default()).unwrap();
        let t0 = Instant::now();
        let mut ready_time = None;
        let mut term_opt = None;

        for chunk in snap_bytes.chunks(65_536) {
            if let SnapshotFeedStatus::Ready(term) = dec.feed(chunk).unwrap() {
                ready_time = Some(t0.elapsed());
                term_opt = Some(term);
            }
        }
        let rt = ready_time.expect("snapshot must reach READY");
        let _term = term_opt.expect("term");

        let t_bg = Instant::now();
        let mut pages = 0usize;
        loop {
            match dec.step_background(Duration::from_millis(50)).unwrap() {
                BackgroundStep::Progress { pages_decoded, .. } => pages += pages_decoded,
                BackgroundStep::Finished { pages_decoded, .. } => {
                    pages += pages_decoded;
                    break;
                }
                BackgroundStep::NeedMoreData => panic!("unexpected NeedMoreData"),
            }
        }
        bg_pages = pages;
        ready_ns.push(rt.as_nanos() as u64);
        total_bg_ns.push(t_bg.elapsed().as_nanos() as u64);
    }

    let (r50, r_m, r99) = summarize_us(&mut ready_ns);
    let (b50, b_m, b99) = summarize_us(&mut total_bg_ns);

    println!(
        "\n=== 3. Streaming GHOSTSNP Decode (180x50, {} total rows, {:.1} KiB, COMPRESS_HISTORY=true) ===",
        sb.total,
        snap_bytes.len() as f64 / 1024.0
    );
    println!(
        "  Time-to-READY (first paint)     : p50={:.3} ms  mean={:.3} ms  p99={:.3} ms  (target <5 ms)",
        r50 / 1000.0,
        r_m / 1000.0,
        r99 / 1000.0
    );
    println!(
        "  Background history ({bg_pages} pages)   : p50={:.3} ms  mean={:.3} ms  p99={:.3} ms",
        b50 / 1000.0,
        b_m / 1000.0,
        b99 / 1000.0
    );
}

fn main() {
    let info = vt_engine::build_info().expect("build_info");
    println!(
        "vt-engine benchmark | libghostty-vt={} simd={} kitty_graphics={}\n",
        info.version, info.simd, info.kitty_graphics
    );
    bench_vt_throughput();
    bench_render_state();
    bench_snapshot_ready();
}
