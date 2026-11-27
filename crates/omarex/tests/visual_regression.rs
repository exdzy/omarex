use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use gpu_renderer::{Color, DrawList, GpuConfig, GpuShared, Rect, layers};

struct TempTestDir {
    path: PathBuf,
}

impl TempTestDir {
    fn new(prefix: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "omarex-{prefix}-{}-{seq}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temp test dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempTestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[derive(Debug)]
struct SummaryReport {
    windows: usize,
    ready_panes: usize,
    draws: usize,
    quads: usize,
    glyphs: usize,
}

fn parse_summary_line(stdout: &str) -> SummaryReport {
    let line = stdout
        .lines()
        .find(|l| l.contains("omarex headless render complete:"))
        .unwrap_or_else(|| panic!("missing headless summary line in stdout:\n{stdout}"));

    let extract_usize = |key: &str| -> usize {
        let needle = format!("{key}=");
        let rest = line
            .split(&needle)
            .nth(1)
            .unwrap_or_else(|| panic!("missing {needle} in line: {line}"));
        let tok = rest.split_whitespace().next().unwrap_or("");
        tok.parse::<usize>()
            .unwrap_or_else(|e| panic!("failed to parse {key} from `{tok}`: {e}"))
    };

    SummaryReport {
        windows: extract_usize("windows"),
        ready_panes: extract_usize("ready_panes"),
        draws: extract_usize("draws"),
        quads: extract_usize("quads"),
        glyphs: extract_usize("glyphs"),
    }
}

fn run_headless_scenario(
    tmp: &TempTestDir,
    scenario: &str,
    out_name: &str,
    width: u32,
    height: u32,
) -> (PathBuf, SummaryReport, image::RgbaImage) {
    let bin = env!("CARGO_BIN_EXE_omarex");
    let out_path = tmp.path().join(out_name);
    let xdg_cfg = tmp.path().join("xdg_config");
    let xdg_data = tmp.path().join("xdg_data");
    let xdg_state = tmp.path().join("xdg_state");
    let xdg_cache = tmp.path().join("xdg_cache");
    let missing_sock = tmp.path().join("missing.sock");
    for dir in [&xdg_cfg, &xdg_data, &xdg_state, &xdg_cache] {
        std::fs::create_dir_all(dir).expect("create isolated xdg dir");
    }

    let output = Command::new(bin)
        .args([
            "--headless-render",
            out_path.to_str().unwrap(),
            "--headless-scenario",
            scenario,
            "--width",
            &width.to_string(),
            "--height",
            &height.to_string(),
        ])
        .env("HOME", tmp.path())
        .env("XDG_CONFIG_HOME", &xdg_cfg)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("XDG_STATE_HOME", &xdg_state)
        .env("XDG_CACHE_HOME", &xdg_cache)
        .env("REX_SERVER", format!("unix://{}", missing_sock.display()))
        .env("RUST_LOG", "off")
        .output()
        .expect("execute omarex headless scenario");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "scenario `{scenario}` failed (status={:?})\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status.code()
    );

    let report = parse_summary_line(&stdout);
    assert!(
        report.windows >= 1,
        "scenario `{scenario}` expected windows >= 1, got {}",
        report.windows
    );
    assert!(
        report.ready_panes >= 1,
        "scenario `{scenario}` expected ready_panes >= 1, got {}",
        report.ready_panes
    );
    assert!(
        report.draws > 0 && report.quads > 0 && report.glyphs > 0,
        "scenario `{scenario}` expected non-zero draws/quads/glyphs: {report:?}"
    );

    let dyn_img = image::open(&out_path).expect("decode rendered PNG");
    let rgba = dyn_img.to_rgba8();
    assert_eq!(rgba.dimensions(), (width, height));

    // Verify the image is not solid/blank (sampled unique RGB colors > 16).
    let raw = rgba.as_raw();
    let mut unique: HashSet<[u8; 3]> = HashSet::new();
    for px in raw.as_chunks::<4>().0.iter().step_by(17) {
        unique.insert([px[0], px[1], px[2]]);
        if unique.len() >= 32 {
            break;
        }
    }
    assert!(
        unique.len() >= 16,
        "scenario `{scenario}` produced nearly solid image ({} unique colors)",
        unique.len()
    );

    (out_path, report, rgba)
}

fn mean_luma(img: &image::RgbaImage) -> f64 {
    let mut sum = 0.0f64;
    let mut count = 0u64;
    for px in img.as_raw().as_chunks::<4>().0.iter().step_by(4) {
        sum += 0.2126 * f64::from(px[0]) + 0.7152 * f64::from(px[1]) + 0.0722 * f64::from(px[2]);
        count += 1;
    }
    sum / (count.max(1) as f64)
}

fn diff_fraction(a: &image::RgbaImage, b: &image::RgbaImage) -> f64 {
    assert_eq!(a.dimensions(), b.dimensions());
    let mut changed = 0u64;
    let mut total = 0u64;
    for (pa, pb) in a
        .as_raw()
        .as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_raw().as_chunks::<4>().0.iter())
    {
        total += 1;
        if pa[0].abs_diff(pb[0]) > 4 || pa[1].abs_diff(pb[1]) > 4 || pa[2].abs_diff(pb[2]) > 4 {
            changed += 1;
        }
    }
    (changed as f64) / (total.max(1) as f64)
}

#[test]
fn headless_scenarios_all_13_render_valid_distinct_pngs() {
    let tmp = TempTestDir::new("visual-qa");
    let scenarios = [
        "sidebar-dark",
        "sidebar-light",
        "split-panes",
        "floating-pr",
        "copy-mode",
        "syntax-viewer",
        "diagnostics-hud",
        "ports-pill",
        "markdown-viewer",
        "theme-switcher",
        "settings-modal",
        "dnd-split",
        "multi-window",
    ];

    let mut rendered: Vec<(&str, image::RgbaImage)> = Vec::new();
    for name in scenarios {
        let (_, report, img) = run_headless_scenario(&tmp, name, &format!("{name}.png"), 960, 600);
        if name == "multi-window" {
            assert!(
                report.windows >= 2,
                "multi-window expected windows >= 2, got {}",
                report.windows
            );
            assert!(
                report.ready_panes >= 2,
                "multi-window expected ready_panes >= 2, got {}",
                report.ready_panes
            );
        }
        rendered.push((name, img));
    }

    let dark = &rendered
        .iter()
        .find(|(n, _)| *n == "sidebar-dark")
        .unwrap()
        .1;
    let light = &rendered
        .iter()
        .find(|(n, _)| *n == "sidebar-light")
        .unwrap()
        .1;
    let dark_luma = mean_luma(dark);
    let light_luma = mean_luma(light);
    assert!(
        light_luma > dark_luma + 40.0,
        "expected sidebar-light luma ({light_luma:.1}) > sidebar-dark luma ({dark_luma:.1}) + 40"
    );

    for (name, img) in &rendered {
        if *name == "sidebar-dark" {
            continue;
        }
        let frac = diff_fraction(dark, img);
        assert!(
            frac > 0.001,
            "scenario `{name}` had negligible visual diff vs sidebar-dark ({frac:.6})"
        );
    }
}

#[test]
fn headless_unknown_scenario_exits_with_error_code_1() {
    let tmp = TempTestDir::new("visual-qa-err");
    let bin = env!("CARGO_BIN_EXE_omarex");
    let out_path = tmp.path().join("bogus.png");
    let output = Command::new(bin)
        .args([
            "--headless-render",
            out_path.to_str().unwrap(),
            "--headless-scenario",
            "does-not-exist",
        ])
        .env("HOME", tmp.path())
        .env("REX_SERVER", "unix:///tmp/nonexistent-omarex-test.sock")
        .output()
        .expect("run omarex with unknown scenario");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Error:"),
        "expected `Error:` on stderr, got: {stderr}"
    );
    assert!(!out_path.exists());
}

#[test]
fn gpu_shared_renders_multiple_offscreen_targets_with_shared_atlas() {
    let shared = GpuShared::new_headless(&GpuConfig::default()).expect("create GpuShared");
    let target_a = shared.create_offscreen_target(320, 200).expect("target A");
    let target_b = shared.create_offscreen_target(320, 200).expect("target B");

    let mut dl_a = DrawList::new(320, 200, 1.0);
    dl_a.set_clear_color(Color::from_hex(0x10141c));
    dl_a.with_layer(layers::CANVAS, |dl| {
        dl.push_rect(
            Rect::new(20.0, 20.0, 120.0, 80.0),
            Color::from_hex(0xe28203),
        );
    });
    let frame_a = dl_a.finish();

    let mut dl_b = DrawList::new(320, 200, 1.0);
    dl_b.set_clear_color(Color::from_hex(0xf4f4f6));
    dl_b.with_layer(layers::CANVAS, |dl| {
        dl.push_rect(
            Rect::new(140.0, 60.0, 120.0, 80.0),
            Color::from_hex(0x2b6cb0),
        );
    });
    let frame_b = dl_b.finish();

    shared
        .render_to_offscreen(&frame_a, &target_a)
        .expect("render A");
    shared
        .render_to_offscreen(&frame_b, &target_b)
        .expect("render B");

    let rgba_a = shared.readback_offscreen(&target_a).expect("readback A");
    let rgba_b = shared.readback_offscreen(&target_b).expect("readback B");
    assert_eq!(rgba_a.len(), 320 * 200 * 4);
    assert_eq!(rgba_b.len(), 320 * 200 * 4);
    assert_ne!(rgba_a, rgba_b);
}

fn crop_diff_fraction(
    a: &image::RgbaImage,
    b: &image::RgbaImage,
    x0: u32,
    y0: u32,
    w: u32,
    h: u32,
) -> (f64, u64) {
    assert_eq!(a.dimensions(), b.dimensions());
    let mut changed = 0u64;
    let mut total = 0u64;
    for y in y0..(y0 + h).min(a.height()) {
        for x in x0..(x0 + w).min(a.width()) {
            let pa = a.get_pixel(x, y).0;
            let pb = b.get_pixel(x, y).0;
            total += 1;
            if pa[0].abs_diff(pb[0]) > 4 || pa[1].abs_diff(pb[1]) > 4 || pa[2].abs_diff(pb[2]) > 4 {
                changed += 1;
            }
        }
    }
    ((changed as f64) / (total.max(1) as f64), changed)
}

fn run_headless_custom(
    tmp: &TempTestDir,
    xdg_state: &Path,
    extra_args: &[&str],
    out_name: &str,
    server_ep: Option<&str>,
) -> (PathBuf, image::RgbaImage) {
    let bin = env!("CARGO_BIN_EXE_omarex");
    let out_path = tmp.path().join(out_name);
    let xdg_cfg = tmp.path().join("xdg_config");
    let xdg_data = tmp.path().join("xdg_data");
    let xdg_cache = tmp.path().join("xdg_cache");
    let missing_sock = tmp.path().join("missing.sock");
    for dir in [&xdg_cfg, &xdg_data, xdg_cache.as_path(), xdg_state] {
        std::fs::create_dir_all(dir).expect("create isolated xdg dir");
    }

    let mut args = vec![
        "--headless-render",
        out_path.to_str().unwrap(),
        "--width",
        "960",
        "--height",
        "600",
    ];
    args.extend_from_slice(extra_args);

    let endpoint = server_ep
        .map(str::to_owned)
        .unwrap_or_else(|| format!("unix://{}", missing_sock.display()));

    let output = Command::new(bin)
        .args(&args)
        .env("HOME", tmp.path())
        .env("XDG_CONFIG_HOME", &xdg_cfg)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("XDG_STATE_HOME", xdg_state)
        .env("XDG_CACHE_HOME", &xdg_cache)
        .env("REX_SERVER", &endpoint)
        .env("RUST_LOG", "off")
        .output()
        .expect("execute omarex headless render");

    assert!(
        output.status.success(),
        "headless render `{out_name}` failed (status={:?})\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let rgba = image::open(&out_path).expect("decode PNG").to_rgba8();
    assert_eq!(rgba.dimensions(), (960, 600));
    (out_path, rgba)
}

#[test]
fn gap03_malformed_utf8_colors_toml_falls_back_cleanly() {
    let tmp = TempTestDir::new("gap03-utf8");
    let xdg_state = tmp.path().join("xdg_state");
    let theme_dir = xdg_state.join("omarchy/current/theme");
    std::fs::create_dir_all(&theme_dir).unwrap();
    std::fs::write(theme_dir.join("colors.toml"), "background = \"€€\"\n").unwrap();

    let (_, img) = run_headless_custom(&tmp, &xdg_state, &[], "utf8_fallback.png", None);
    assert!(mean_luma(&img) > 5.0);
}

#[test]
fn gap02_repeated_headless_viewer_pdf_page_jump_renders_page_2() {
    if Command::new("pdfinfo").arg("-v").output().is_err()
        || Command::new("pdftoppm").arg("-v").output().is_err()
    {
        return;
    }

    let tmp = TempTestDir::new("gap02-pdf");
    let xdg_state = tmp.path().join("xdg_state");
    let pdf_path = tmp.path().join("two_page.pdf");

    let mut pdf = Vec::new();
    pdf.extend_from_slice(b"%PDF-1.4\n");
    let mut offsets = Vec::new();
    let obj1 = b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n";
    let obj2 = b"2 0 obj\n<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 2 >>\nendobj\n";
    let obj3 =
        b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R >>\nendobj\n";
    let stream1 = b"0 0 1 rg 0 0 200 200 re f\n";
    let obj4 = format!(
        "4 0 obj\n<< /Length {} >>\nstream\n{}endstream\nendobj\n",
        stream1.len(),
        std::str::from_utf8(stream1).unwrap()
    );
    let obj5 =
        b"5 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 6 0 R >>\nendobj\n";
    let stream2 = b"1 0 0 rg 0 0 200 200 re f\n";
    let obj6 = format!(
        "6 0 obj\n<< /Length {} >>\nstream\n{}endstream\nendobj\n",
        stream2.len(),
        std::str::from_utf8(stream2).unwrap()
    );
    let objects: [&[u8]; 6] = [
        obj1.as_slice(),
        obj2.as_slice(),
        obj3.as_slice(),
        obj4.as_bytes(),
        obj5.as_slice(),
        obj6.as_bytes(),
    ];
    for o in objects {
        offsets.push(pdf.len());
        pdf.extend_from_slice(o);
    }
    let xref_pos = pdf.len();
    pdf.extend_from_slice(format!("xref\n0 {}\n", offsets.len() + 1).as_bytes());
    pdf.extend_from_slice(b"0000000000 65535 f \n");
    for off in offsets {
        pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_pos}\n%%EOF\n",
            7
        )
        .as_bytes(),
    );
    std::fs::write(&pdf_path, &pdf).unwrap();

    let spec1 = format!("{}:1", pdf_path.display());
    let spec2 = format!("{}:2", pdf_path.display());

    let (_, p1) = run_headless_custom(
        &tmp,
        &xdg_state,
        &["--headless-viewer", &spec1],
        "pdf_p1.png",
        None,
    );
    let (_, p2) = run_headless_custom(
        &tmp,
        &xdg_state,
        &["--headless-viewer", &spec2],
        "pdf_p2.png",
        None,
    );
    let (_, p1_then_2) = run_headless_custom(
        &tmp,
        &xdg_state,
        &["--headless-viewer", &spec1, "--headless-viewer", &spec2],
        "pdf_p1_then_2.png",
        None,
    );

    let (diff_vs_p2, changed_vs_p2) = crop_diff_fraction(&p2, &p1_then_2, 480, 60, 460, 500);
    let (diff_vs_p1, _) = crop_diff_fraction(&p1, &p1_then_2, 480, 60, 460, 500);
    assert!(
        diff_vs_p2 < 0.01,
        "expected repeated PDF jump to render page 2 (diff_vs_p2={diff_vs_p2:.4}, changed={changed_vs_p2})"
    );
    assert!(
        diff_vs_p1 > 0.05,
        "expected repeated PDF jump to differ from page 1 (diff_vs_p1={diff_vs_p1:.4})"
    );
}

#[test]
fn gap06_offline_syntax_viewer_with_explicit_viewer_path_has_no_extra_tab() {
    let tmp = TempTestDir::new("gap06-tabs");
    let xdg_state = tmp.path().join("xdg_state");
    let py_path = tmp.path().join("sample.py");
    std::fs::write(&py_path, "def hello():\n    return 42\n").unwrap();

    let py_str = py_path.to_str().unwrap();
    let (_, baseline_viewer) = run_headless_custom(
        &tmp,
        &xdg_state,
        &[
            "--headless-scenario",
            "sidebar-dark",
            "--headless-viewer",
            py_str,
        ],
        "baseline_viewer.png",
        None,
    );
    let (_, scenario_viewer) = run_headless_custom(
        &tmp,
        &xdg_state,
        &[
            "--headless-scenario",
            "syntax-viewer",
            "--headless-viewer",
            py_str,
        ],
        "scenario_viewer.png",
        None,
    );

    let (tab_diff, changed) =
        crop_diff_fraction(&baseline_viewer, &scenario_viewer, 480, 0, 480, 48);
    assert!(
        tab_diff < 0.001,
        "expected no extra synthetic viewer tab when --headless-viewer is passed (tab_diff={tab_diff:.4}, changed={changed})"
    );
}

#[test]
fn gap07_sidebar_dark_preserves_custom_dark_xdg_theme() {
    let tmp = TempTestDir::new("gap07-dark-theme");
    let xdg_state = tmp.path().join("xdg_state");
    let theme_dir = xdg_state.join("omarchy/current/theme");
    std::fs::create_dir_all(&theme_dir).unwrap();
    std::fs::write(
        theme_dir.join("colors.toml"),
        r##"
mode = "dark"
accent = "#ff5500"
selection = "#4a0000"
muted = "#aa6666"
background = "#3a0000"
dark_background = "#2d0000"
darker_background = "#1a0000"
lighter_background = "#4d0000"
foreground = "#ffffff"
dark_foreground = "#cccccc"
light_foreground = "#ffffff"
bright_foreground = "#ffffff"
red = "#ff3333"
yellow = "#ffcc00"
green = "#33cc33"
cyan = "#33cccc"
blue = "#3366ff"
magenta = "#cc33ff"
bright_red = "#ff6666"
bright_yellow = "#ffdd55"
bright_green = "#66dd66"
bright_cyan = "#66dddd"
bright_blue = "#6699ff"
bright_magenta = "#dd66ff"
"##,
    )
    .unwrap();

    let (_, img) = run_headless_custom(
        &tmp,
        &xdg_state,
        &["--headless-scenario", "sidebar-dark"],
        "sidebar_custom_dark.png",
        None,
    );
    let px = img.get_pixel(30, 140).0;
    assert!(
        px[0] > px[2].saturating_add(20),
        "expected custom dark red sidebar pixel, got {px:?}"
    );
}

#[test]
fn gap01_plain_headless_render_queries_live_ports_pill() {
    let rex_help = Command::new("rex")
        .arg("--help")
        .output()
        .expect("execute rex --help");
    assert!(
        rex_help.status.success(),
        "rex CLI binary must be available for gap01 test (status={:?}, stderr={})",
        rex_help.status.code(),
        String::from_utf8_lossy(&rex_help.stderr)
    );

    struct ShortSockDir {
        path: PathBuf,
    }
    impl ShortSockDir {
        fn new() -> Self {
            use std::os::unix::fs::PermissionsExt;
            static SEQ: AtomicU64 = AtomicU64::new(1);
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let path = PathBuf::from(format!("/tmp/omx-vr-{}-{seq}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create short socket dir");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                .expect("set 0o700 permissions on short socket dir");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }
    impl Drop for ShortSockDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    struct RexChildGuard(std::process::Child);
    impl Drop for RexChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let tmp = TempTestDir::new("gap01");
    let sock_dir = ShortSockDir::new();
    let sock_path = sock_dir.path().join("s.sock");
    assert!(
        sock_path.to_string_lossy().len() <= 95,
        "unix socket path `{}` exceeds 95 bytes ({})",
        sock_path.display(),
        sock_path.to_string_lossy().len()
    );
    let endpoint = format!("unix://{}", sock_path.display());
    let state_dir = tmp.path().join("state");
    std::fs::create_dir_all(&state_dir).unwrap();

    let child = Command::new("rex")
        .args([
            "server",
            "run",
            "--listen",
            &endpoint,
            "--state-dir",
            state_dir.to_str().unwrap(),
            "--restore=false",
            "--no-fs",
            "--log-level",
            "warn",
        ])
        .env_remove("REX_SERVER")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn isolated rex server");
    let _guard = RexChildGuard(child);

    let ready_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut ready = false;
    while std::time::Instant::now() < ready_deadline {
        if sock_path.exists()
            && Command::new("rex")
                .args([
                    "--autostart=false",
                    "-S",
                    &endpoint,
                    "api",
                    "call",
                    "server.status",
                ])
                .env("REX_SERVER", &endpoint)
                .output()
                .is_ok_and(|o| o.status.success())
        {
            ready = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(40));
    }
    assert!(ready, "isolated rex server did not become ready");

    // Create a session with an open pane and no listening port for baseline plain headless render.
    let create_base_json = serde_json::json!({
        "label": "Gap01Baseline",
        "initial_windows": [{
            "window_label": "main",
            "layout": {
                "block": {
                    "flavor": "com.superlogical.terminal.shell",
                    "label": "term",
                    "options": {
                        "command": ["sh", "-lc", "sleep 15"]
                    }
                }
            }
        }]
    })
    .to_string();
    let create_out = Command::new("rex")
        .args([
            "--autostart=false",
            "-S",
            &endpoint,
            "api",
            "call",
            "session.create",
            &create_base_json,
        ])
        .env("REX_SERVER", &endpoint)
        .output()
        .expect("create baseline session");
    assert!(
        create_out.status.success(),
        "session.create Gap01Baseline failed (status={:?})\nstdout:\n{}\nstderr:\n{}",
        create_out.status.code(),
        String::from_utf8_lossy(&create_out.stdout),
        String::from_utf8_lossy(&create_out.stderr)
    );

    let xdg_state = tmp.path().join("xdg_state");
    let (_, baseline_img) = run_headless_custom(
        &tmp,
        &xdg_state,
        &["--", "Gap01Baseline"],
        "plain_no_port.png",
        Some(&endpoint),
    );

    // Now create a second session running a Python TCP listener and wait for rex ports API to see it.
    let listener_cmd = "python3 -c \"import socket,time; s=socket.socket(); s.bind(('127.0.0.1',0)); s.listen(1); time.sleep(15)\"";
    let create_port_json = serde_json::json!({
        "label": "Gap01Port",
        "initial_windows": [{
            "window_label": "main",
            "layout": {
                "block": {
                    "flavor": "com.superlogical.terminal.shell",
                    "label": "term",
                    "options": {
                        "command": ["sh", "-lc", listener_cmd]
                    }
                }
            }
        }]
    })
    .to_string();
    let port_sess_out = Command::new("rex")
        .args([
            "--autostart=false",
            "-S",
            &endpoint,
            "api",
            "call",
            "session.create",
            &create_port_json,
        ])
        .env("REX_SERVER", &endpoint)
        .output()
        .expect("create port session");
    assert!(
        port_sess_out.status.success(),
        "session.create Gap01Port failed (status={:?})\nstdout:\n{}\nstderr:\n{}",
        port_sess_out.status.code(),
        String::from_utf8_lossy(&port_sess_out.stdout),
        String::from_utf8_lossy(&port_sess_out.stderr)
    );
    let port_sess_val: serde_json::Value =
        serde_json::from_slice(&port_sess_out.stdout).expect("parse session.create json");
    let sess_id = port_sess_val["session_id"]
        .as_str()
        .expect("session_id in session.create response")
        .to_owned();
    let block_id = port_sess_val["initial_windows"][0]["block_ids"][0]
        .as_str()
        .expect("initial_windows[0].block_ids[0] in session.create response")
        .to_owned();

    let port_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut port_seen = false;
    while std::time::Instant::now() < port_deadline {
        let probe_json = serde_json::json!({
            "session_id": sess_id,
            "block_id": block_id,
        })
        .to_string();
        if let Ok(out) = Command::new("rex")
            .args([
                "--autostart=false",
                "-S",
                &endpoint,
                "api",
                "call",
                "com.superlogical.terminal.ports",
                &probe_json,
            ])
            .env("REX_SERVER", &endpoint)
            .output()
            && out.status.success()
            && let Ok(val) = serde_json::from_slice::<serde_json::Value>(&out.stdout)
            && val["ports"].as_array().is_some_and(|a| !a.is_empty())
        {
            port_seen = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(60));
    }
    assert!(
        port_seen,
        "isolated rex server never reported listening port for python listener"
    );

    let (_, live_port_img) = run_headless_custom(
        &tmp,
        &xdg_state,
        &["--", "Gap01Port"],
        "plain_with_port.png",
        Some(&endpoint),
    );
    let (_, changed_header_px) = crop_diff_fraction(&baseline_img, &live_port_img, 240, 8, 700, 44);
    assert!(
        changed_header_px > 100,
        "expected plain --headless-render to query ListPorts and render port pill in header (changed_header_px={changed_header_px})"
    );
}

#[test]
fn new_gap01_headless_viewer_markdown_escaped_multibyte_utf8_succeeds() {
    let tmp = TempTestDir::new("new-gap01-md");
    let xdg_state = tmp.path().join("xdg_state");
    let md_path = tmp.path().join("escaped_utf8.md");
    std::fs::write(
        &md_path,
        "# Hello\n\nEscaped euro: \\€ and umlaut: \\ä and crab: \\🦀\n\n![alt\\€](missing\\🦀.png)\n",
    )
    .expect("write escaped utf8 markdown");

    let (_, img) = run_headless_custom(
        &tmp,
        &xdg_state,
        &["--headless-viewer", md_path.to_str().unwrap()],
        "escaped_utf8_md.png",
        None,
    );
    assert!(mean_luma(&img) > 5.0);
}

#[test]
fn new_gap02_headless_viewer_short_viewport_scrollbar_does_not_panic() {
    let tmp = TempTestDir::new("new-gap02-short-viewport");
    let xdg_cfg = tmp.path().join("xdg_config");
    let xdg_data = tmp.path().join("xdg_data");
    let xdg_state = tmp.path().join("xdg_state");
    let xdg_cache = tmp.path().join("xdg_cache");
    let missing_sock = tmp.path().join("missing.sock");
    for dir in [&xdg_cfg, &xdg_data, &xdg_state, &xdg_cache] {
        std::fs::create_dir_all(dir).expect("create isolated xdg dir");
    }

    let long_path = tmp.path().join("long.txt");
    let long_txt: String = (1..=100).map(|n| format!("line {n}\n")).collect();
    std::fs::write(&long_path, long_txt).expect("write 100-line file");

    let out_path = tmp.path().join("short_viewport.png");
    let bin = env!("CARGO_BIN_EXE_omarex");
    let output = Command::new(bin)
        .args([
            "--headless-render",
            out_path.to_str().unwrap(),
            "--headless-viewer",
            long_path.to_str().unwrap(),
            "--width",
            "1440",
            "--height",
            "420",
            "--scale",
            "4.0",
        ])
        .env("HOME", tmp.path())
        .env("XDG_CONFIG_HOME", &xdg_cfg)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("XDG_STATE_HOME", &xdg_state)
        .env("XDG_CACHE_HOME", &xdg_cache)
        .env("REX_SERVER", format!("unix://{}", missing_sock.display()))
        .env("RUST_LOG", "off")
        .output()
        .expect("execute omarex headless short viewport render");

    assert!(
        output.status.success(),
        "short viewport headless viewer failed (status={:?})\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let rgba = image::open(&out_path).expect("decode PNG").to_rgba8();
    assert_eq!(rgba.dimensions(), (1440, 420));
}

#[test]
fn settings_ios_qr_headless_scenario_renders_scannable_qr_code() {
    let tmp = TempTestDir::new("settings-ios-qr");
    let (dark_path, _, dark_img) =
        run_headless_scenario(&tmp, "settings-ios-qr", "ios_qr_dark.png", 1280, 760);
    let (_, _, settings_img) =
        run_headless_scenario(&tmp, "settings-modal", "settings_default.png", 1280, 760);

    let frac = diff_fraction(&dark_img, &settings_img);
    assert!(
        frac > 0.01,
        "settings-ios-qr must visibly differ from default settings-modal (diff={frac:.4})"
    );

    if let Ok(out) = Command::new("zbarimg")
        .args(["--raw", "-q", dark_path.to_str().unwrap()])
        .output()
        && out.status.success()
    {
        let decoded = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        assert_eq!(
            decoded, "https://omarchy-workstation.tail000000.ts.net",
            "GPU-rendered Connect iOS QR code must decode cleanly to the Tailscale HTTPS URL"
        );
    }
}

#[test]
fn settings_server_and_program_resume_headless_scenarios_render_cleanly() {
    let tmp = TempTestDir::new("settings-server-resume");
    let (_, _, server_img) =
        run_headless_scenario(&tmp, "settings-server", "settings_server.png", 1280, 760);
    let (_, _, settings_img) =
        run_headless_scenario(&tmp, "settings-modal", "settings_default.png", 1280, 760);
    let server_diff = diff_fraction(&server_img, &settings_img);
    assert!(
        server_diff > 0.01,
        "settings-server must visibly differ from default settings-modal (diff={server_diff:.4})"
    );

    let (_, _, resume_img) =
        run_headless_scenario(&tmp, "program-resume", "program_resume.png", 1280, 760);
    let (_, _, split_img) =
        run_headless_scenario(&tmp, "split-panes", "split_panes.png", 1280, 760);
    let resume_diff = diff_fraction(&resume_img, &split_img);
    assert!(
        resume_diff > 0.005,
        "program-resume must visibly differ from baseline split-panes (diff={resume_diff:.4})"
    );
}
