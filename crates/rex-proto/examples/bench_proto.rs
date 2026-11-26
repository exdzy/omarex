//! Protocol micro-benchmarks for `rex-proto` (`fast-anything` / `rust-anything` gate).
//!
//! Run with:
//! `cargo run --release -p rex-proto --example bench_proto`

use std::hint::black_box;
use std::time::Instant;

use rex_proto::data_stream::{FramedChunk, RexApcFramer};
use rex_proto::types::{
    IncomingControlFrame, ServerEvent, SessionEvent, SessionView, TerminalSetThemeArgs, ThemeScheme,
};

const CHANNEL_NONCE: u64 = 9_876_543_210_123_456;

fn build_mixed_vt_stream(target_bytes: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(target_bytes + 4096);
    let vt_block = b"\x1b[38;5;208m\x1b[1mClaude Code\x1b[0m \x1b[2m~/Work/omarex\x1b[0m  \
        \x1b[32m\xe2\x9c\x93\x1b[0m 12 passed; 0 failed in 0.00s \xe2\x80\x94 streaming output line with ANSI escapes\r\n";
    let kitty_block = b"\x1b_Ga=T,f=100,t=d,i=42,c=2,r=1;iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==\x1b\\";
    let valid_apc = format!(
        "\x1b_rex1;{CHANNEL_NONCE};{{\"type\":\"resize\",\"cols\":180,\"rows\":48,\"cell_width_px\":9,\"cell_height_px\":18,\"owner\":\"client:01a216bc-0000-7000-8000-000000000001\"}}\x1b\\"
    );
    let spoofed_apc = b"\x1b_rex1;42;{\"type\":\"child_exited\",\"exit_code\":137}\x1b\\";

    let mut iter = 0usize;
    while out.len() < target_bytes {
        for _ in 0..32 {
            out.extend_from_slice(vt_block);
        }
        out.extend_from_slice(kitty_block);
        if iter.is_multiple_of(4) {
            out.extend_from_slice(valid_apc.as_bytes());
        }
        if iter.is_multiple_of(8) {
            out.extend_from_slice(spoofed_apc);
        }
        iter += 1;
    }
    out
}

fn build_23_window_t3_session_view_json() -> String {
    let titles = [
        "Run and Optimize Inkling-2-Mini on Omarchy",
        "Omarchy Sound-Widget erweitern",
        "Rust SteamOS Anti-Cheat Compatibility",
        "Bluetooth Pairing Across Dual Boot",
        "Update T3 Code, Disable Telemetry",
        "Omarchy PC-Start wie beim Mac optimieren",
        "PC nach Steam-Downloads ausschalten",
        "DPI bei dir und CS2-Profis",
        "Categorize Open T3 Threads in HTML",
        "Disable T3 Code Telemetry",
        "Gesundheitsvorteile von Pferdefleisch",
        "Find Fast T-Shirt Resale Deals",
        "Ego Browser für Linux",
        "Kosten für ein Schaf in Deutschland",
        "Monitor-Grafik stark verbessern",
        "Tastaturbeleuchtung einschalten",
        "Steamkonto ohne Telefonnummer wiederherstellen",
        "Steam-Oberfläche zu stark vergrößert",
        "Werbung in Chromium blockieren",
        "Bedeutung des Gebots im Symbol",
        "Review and Integrate Omarchy Plugin",
        "Omarchy WLAN optimieren",
        "Run T3 Code with Rex on Omarchy",
    ];

    let mut windows = Vec::with_capacity(titles.len());
    for (idx, title) in titles.iter().enumerate() {
        let win_id = format!("window:01a216bc-53{:02x}-74b7-8d08-26ca9f808eb6", idx);
        let blk_id = format!("block:01a216bc-53{:02x}-781a-84b4-fc98fdf64fb4", idx);
        let layer_id = format!("layer:01a216bc-53{:02x}-74bd-a876-500219635eee", idx);

        let mut layers = vec![serde_json::json!({
            "layer_id": layer_id,
            "kind": "tiled",
            "bounds": [0, 0, 1, 1],
            "layout": {
                "rect": [0, 0, 1, 1],
                "block_id": blk_id,
            },
            "blocks": [{
                "block_id": blk_id,
                "label": format!("t3:1baa1939/f68600ef-cf19-4e5a-94d6-a3f88abf{:04x}", idx),
                "creator_name": "com.superlogical.terminal",
                "flavor": "com.superlogical.terminal.shell",
                "host_id": "d686093a-e969-4803-86a8-6a6dd1b4e79d",
            }]
        })];

        // Include a realistic floating /pr drawer on the active window.
        if idx == 18 {
            let pr_blk = "block:01a216bc-ffff-781a-84b4-fc98fdf64fb4";
            layers.push(serde_json::json!({
                "layer_id": "layer:01a216bc-ffff-74bd-a876-500219635eee",
                "kind": "floating",
                "state": "normal",
                "bounds": [0.45, 0.0, 0.55, 1.0],
                "layout": {
                    "rect": [0, 0, 1, 1],
                    "block_id": pr_blk,
                },
                "blocks": [{
                    "block_id": pr_blk,
                    "label": "t3:pr-drawer",
                    "creator_name": "com.superlogical.terminal",
                    "flavor": "com.superlogical.terminal.shell",
                    "host_id": "d686093a-e969-4803-86a8-6a6dd1b4e79d",
                }]
            }));
        }

        windows.push(serde_json::json!({
            "window_id": win_id,
            "label": title,
            "focused_block_id": blk_id,
            "layers": layers,
        }));
    }

    serde_json::json!({
        "revision": 492,
        "session_id": "session:01a216bc-52f2-72c7-aa9a-e94e1d60b77a",
        "label": "T3 Code",
        "active_window_id": "window:01a216bc-5312-74b7-8d08-26ca9f808eb6",
        "windows": windows,
    })
    .to_string()
}

fn bench_framer(stream: &[u8], chunk_size: usize, rounds: usize) -> (f64, usize, usize, u64) {
    // Warmup
    let mut warm = RexApcFramer::new(CHANNEL_NONCE);
    for c in stream.chunks(chunk_size) {
        black_box(warm.feed(c));
    }

    let mut total_vt_bytes = 0usize;
    let mut total_controls = 0usize;
    let mut total_spoofs = 0u64;

    let start = Instant::now();
    for _ in 0..rounds {
        let mut framer = RexApcFramer::new(CHANNEL_NONCE);
        for chunk in stream.chunks(chunk_size) {
            for item in framer.feed(black_box(chunk)) {
                match item {
                    FramedChunk::VtBytes(b) => total_vt_bytes += b.len(),
                    FramedChunk::Control(ctrl) => {
                        black_box(ctrl);
                        total_controls += 1;
                    }
                }
            }
        }
        for item in framer.finish() {
            if let FramedChunk::VtBytes(b) = item {
                total_vt_bytes += b.len();
            }
        }
        total_spoofs += framer.spoofed_apc_count();
    }
    let elapsed = start.elapsed().as_secs_f64();
    let total_input_bytes = (stream.len() * rounds) as f64;
    let mb_per_sec = (total_input_bytes / (1024.0 * 1024.0)) / elapsed;
    (
        mb_per_sec,
        total_vt_bytes / rounds,
        total_controls / rounds,
        total_spoofs / rounds as u64,
    )
}

fn bench_latency_us<F: FnMut()>(iters: usize, mut f: F) -> (f64, f64, f64) {
    // Warmup
    for _ in 0..200 {
        f();
    }

    let mut samples_ns = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t0 = Instant::now();
        f();
        samples_ns.push(t0.elapsed().as_nanos() as f64);
    }
    samples_ns.sort_by(|a, b| a.total_cmp(b));
    let mean_us = (samples_ns.iter().sum::<f64>() / samples_ns.len() as f64) / 1_000.0;
    let p50_us = samples_ns[samples_ns.len() / 2] / 1_000.0;
    let p99_us = samples_ns[(samples_ns.len() * 99) / 100] / 1_000.0;
    (p50_us, mean_us, p99_us)
}

fn main() {
    println!("=== rex-proto Protocol Benchmarks ===");

    // Verify omarchy-theme RexSetThemeArgs JSON wire compatibility
    let omarchy_theme_json = serde_json::json!({
        "foreground": "#ffffff",
        "background": "#121212",
        "cursor": "#ffffff",
        "palette": [
            "0=#121212", "1=#ef3827", "2=#b79d62", "3=#bca674",
            "4=#e28203", "5=#db860b", "6=#b39456", "7=#ffffff",
            "8=#9e9e9e", "9=#ff5940", "10=#cbb176", "11=#d0ba89",
            "12=#ff980f", "13=#fd9d12", "14=#c8a96a", "15=#ffffff"
        ],
        "scheme": "dark"
    });
    let parsed_theme: TerminalSetThemeArgs =
        serde_json::from_value(omarchy_theme_json).expect("omarchy-theme JSON must match rev 24");
    assert_eq!(parsed_theme.scheme, Some(ThemeScheme::Dark));
    assert_eq!(parsed_theme.palette.as_ref().map(Vec::len), Some(16));
    println!("[check] omarchy-theme RexSetThemeArgs -> rex-proto TerminalSetThemeArgs: OK");

    // 1. RexApcFramer throughput on 4 MiB mixed VT + Kitty + rex1 APC stream
    let stream = build_mixed_vt_stream(4 * 1024 * 1024);
    println!(
        "\n[1] RexApcFramer throughput (stream = {:.2} MiB mixed VT + Kitty + rex1 APC + spoofs):",
        stream.len() as f64 / (1024.0 * 1024.0)
    );
    for (chunk_size, rounds) in [(256usize, 16usize), (65_536usize, 32usize)] {
        let (mb_s, vt_b, ctrls, spoofs) = bench_framer(&stream, chunk_size, rounds);
        println!(
            "  chunk={chunk_size:>5} B | {mb_s:>8.1} MiB/s | vt={vt_b} B | auth_apcs={ctrls} | spoofs_rejected={spoofs}"
        );
    }

    // 2. SessionView (23-window T3 Code payload) deserialization latency
    let view_json = build_23_window_t3_session_view_json();
    let parsed: SessionView =
        serde_json::from_str(&view_json).expect("23-window T3 Code SessionView must parse");
    assert_eq!(parsed.windows.len(), 23);
    let (v_p50, v_mean, v_p99) = bench_latency_us(2_000, || {
        let v: SessionView = serde_json::from_str(black_box(&view_json)).unwrap();
        black_box(v);
    });
    println!(
        "\n[2] SessionView deserialize (23-window T3 Code, {} bytes): p50={v_p50:.2} µs | mean={v_mean:.2} µs | p99={v_p99:.2} µs",
        view_json.len()
    );

    // 3. session_view_changed event envelope deserialization latency
    let ev_json = r#"{"type":"event","seq":1024,"payload":{"type":"session_event","payload":{"session_id":"session:01a216bc-52f2-72c7-aa9a-e94e1d60b77a","event":{"type":"session_view_changed","payload":{"revision":493}}}}}"#;
    let (e_p50, e_mean, e_p99) = bench_latency_us(10_000, || {
        let frame: IncomingControlFrame = serde_json::from_str(black_box(ev_json)).unwrap();
        if let IncomingControlFrame::Event(ev) = frame {
            let parsed = ServerEvent::from_raw(ev.payload).unwrap();
            assert!(matches!(
                parsed,
                ServerEvent::SessionEvent {
                    event: SessionEvent::SessionViewChanged { revision: 493 },
                    ..
                }
            ));
            black_box(parsed);
        }
    });
    println!(
        "[3] session_view_changed envelope parse ({} bytes):       p50={e_p50:.2} µs | mean={e_mean:.2} µs | p99={e_p99:.2} µs",
        ev_json.len()
    );
}
