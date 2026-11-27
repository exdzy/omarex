"""Tier 1 baseline smoke tests verifying the E2E harness, CLI, headless Vulkan PNGs, fixtures, and Rex isolation."""

from __future__ import annotations

import os
import re
import subprocess
from pathlib import Path

from lib.fixtures import (
    DARK_COLORS_TOML,
    LIGHT_COLORS_TOML,
    create_fixtures,
    install_theme_into_xdg,
    tool_capabilities,
)
from lib.harness import SkipTest, TestCase, TestContext, TestRegistry
from lib.png_check import (
    assert_color_near,
    assert_not_solid_color,
    assert_valid_png,
    pixel_diff_stats,
    relative_luminance,
    sample_pixel,
)
from lib.rex_helper import (
    LIVE_ENDPOINT,
    LIVE_SOCKET_PATH,
    IsolatedRexServer,
    live_socket_present,
    probe_bad_endpoint,
    probe_live_server,
    rex_available,
    run_rex_readonly,
)


def _test_cli_help(ctx: TestContext) -> None:
    for flag in ("--help", "-h"):
        res = ctx.run_omarex([flag], timeout=3.0)
        assert res.returncode == 0, f"{flag} exited {res.returncode}: {res.clean_stderr}"
        assert res.clean_stderr == "", f"{flag} wrote unexpected stderr: {res.clean_stderr!r}"
        for expected in (
            "--headless-render",
            "--headless-viewer",
            "--width",
            "--height",
            "--scale",
            "--light",
            "--server",
            "--version",
        ):
            assert expected in res.clean_stdout, f"{flag} help missing {expected}"


def _test_cli_version(ctx: TestContext) -> None:
    pattern = re.compile(r"^omarex \d+\.\d+\.\d+ \(Rex API rev \d+, libghostty-vt .+\)$")
    for flag in ("--version", "-V"):
        res = ctx.run_omarex([flag], timeout=3.0)
        assert res.returncode == 0, f"{flag} exited {res.returncode}: {res.clean_stderr}"
        line = res.clean_stdout.strip()
        assert pattern.match(line), f"{flag} output unexpected: {line!r}"


def _test_cli_negative_args(ctx: TestContext) -> None:
    bad_invocations = [
        (["--bogus-flag-xyz"], "unknown option"),
        (["--headless-render"], "missing"),
        (["--width", "abc"], "invalid --width"),
        (["sess_one", "sess_two"], "unexpected extra argument"),
    ]
    for args, expected_err in bad_invocations:
        res = ctx.run_omarex(args, timeout=3.0)
        assert res.returncode == 1, f"{args} expected exit 1, got {res.returncode}"
        assert "Error:" in res.clean_stderr, f"{args} missing Error: on stderr: {res.clean_stderr!r}"
        assert expected_err in res.clean_stderr, (
            f"{args} expected {expected_err!r} in stderr, got {res.clean_stderr!r}"
        )


def _test_binary_resolution_override(ctx: TestContext) -> None:
    assert ctx.omarex_bin.is_file() and os.access(ctx.omarex_bin, os.X_OK)
    wrapper = ctx.work_dir / "omarex-wrapper.sh"
    wrapper.write_text(f'#!/usr/bin/env bash\nexec "{ctx.omarex_bin}" "$@"\n', encoding="utf-8")
    os.chmod(wrapper, 0o755)

    e2e_root = Path(__file__).resolve().parent.parent
    env = os.environ.copy()
    env["OMAREX_BIN"] = str(wrapper)
    proc = subprocess.run(
        [str(e2e_root / "run_e2e.sh"), "--list", "--filter", "M1-SMOKE-01"],
        env=env,
        capture_output=True,
        text=True,
        timeout=5.0,
        check=False,
    )
    assert proc.returncode == 0, f"run_e2e.sh --list failed: {proc.stderr}"
    assert "M1-SMOKE-01" in proc.stdout


def _test_headless_render_dark_isolated_xdg(ctx: TestContext) -> None:
    # Dark purple theme (#1a0b2e -> 22% darkened frame ~ (20, 9, 36))
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    ctx.write_settings({"sidebar_behavior": "always", "font_size_pt": 11.0})

    res, png_path, report = ctx.headless_render(
        out_name="dark_1440x900.png",
        rex_mode="isolated_offline",
        rust_log="warn",
        timeout=15.0,
    )
    assert res.returncode == 0, f"--headless-render failed: {res.clean_stderr}\n{res.clean_stdout}"
    assert report is not None
    assert report.width == 1440 and report.height == 900
    assert abs(report.scale - 1.0) < 1e-3
    assert report.session is None and report.windows == 0 and report.ready_panes == 0

    img = assert_valid_png(png_path, expected_width=1440, expected_height=900, min_bytes=4096)
    assert_not_solid_color(img, min_unique_colors=4)

    # Top-left frame pixel reflects darkened #1a0b2e (26, 11, 46) * 0.78 ~= (20, 9, 36)
    corner = sample_pixel(img, 2, 2)
    assert_color_near(corner, (20, 9, 36, 255), tolerance=10, label="dark theme corner")


def _test_headless_render_geometry_and_scale(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    res, png_path, report = ctx.headless_render(
        out_name="scaled_800x600.png",
        width=800,
        height=600,
        scale=2.0,
        rex_mode="isolated_offline",
        rust_log="off",
        timeout=15.0,
    )
    assert res.returncode == 0, f"scaled headless render failed: {res.clean_stderr}"
    assert report is not None
    assert report.width == 800 and report.height == 600
    assert abs(report.scale - 2.0) < 1e-3

    img = assert_valid_png(png_path, expected_width=800, expected_height=600, min_bytes=2048)
    assert_not_solid_color(img, min_unique_colors=2)


def _test_headless_render_light_vs_dark_diff(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")

    res_dark, dark_png, _ = ctx.headless_render(
        out_name="cmp_dark.png",
        width=640,
        height=480,
        rex_mode="isolated_offline",
        rust_log="off",
        timeout=15.0,
    )
    assert res_dark.returncode == 0

    res_light, light_png, _ = ctx.headless_render(
        out_name="cmp_light.png",
        width=640,
        height=480,
        light=True,
        rex_mode="isolated_offline",
        rust_log="off",
        timeout=15.0,
    )
    assert res_light.returncode == 0

    img_dark = assert_valid_png(dark_png, expected_width=640, expected_height=480)
    img_light = assert_valid_png(light_png, expected_width=640, expected_height=480)
    stats = pixel_diff_stats(img_dark, img_light)
    assert stats["diff_fraction"] > 0.25, (
        f"Expected --light to differ substantially from dark render: {stats}"
    )

    lum_dark = relative_luminance(sample_pixel(img_dark, 2, 2)[:3])
    lum_light = relative_luminance(sample_pixel(img_light, 2, 2)[:3])
    assert lum_light > lum_dark + 0.3, (
        f"Expected --light border luminance ({lum_light:.3f}) >> dark ({lum_dark:.3f})"
    )


def _test_headless_viewer_opens_sample_rust(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fixtures", generate_mp3=False, init_git=False)
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")

    res_base, base_png, rep_base = ctx.headless_render(
        out_name="no_viewer.png",
        rex_mode="isolated_offline",
        rust_log="off",
        timeout=15.0,
    )
    assert res_base.returncode == 0 and rep_base is not None

    res_view, view_png, rep_view = ctx.headless_render(
        out_name="with_viewer.png",
        viewer_paths=[f"{bundle.sample_rs}:2"],
        rex_mode="isolated_offline",
        rust_log="off",
        timeout=15.0,
    )
    assert res_view.returncode == 0 and rep_view is not None

    img_base = assert_valid_png(base_png, expected_width=1440, expected_height=900)
    img_view = assert_valid_png(view_png, expected_width=1440, expected_height=900)
    assert_not_solid_color(img_view, min_unique_colors=6)

    stats = pixel_diff_stats(img_base, img_view)
    assert stats["diff_fraction"] > 0.03, (
        f"Expected --headless-viewer to visibly change output PNG: {stats}"
    )
    assert rep_view.quads >= rep_base.quads, (
        f"Expected viewer render quads ({rep_view.quads}) >= baseline ({rep_base.quads})"
    )


def _test_fixtures_integrity_and_missing_colors_theme(ctx: TestContext) -> None:
    caps = tool_capabilities()
    bundle = create_fixtures(ctx.work_dir / "fixtures", generate_mp3=caps["ffmpeg"], init_git=caps["git"])

    assert bundle.sample_rs.read_text(encoding="utf-8").count("\n") >= 80
    for tok in ("->", "=>", "!==", "===", "::", "|>"):
        assert tok in bundle.sample_rs.read_text(encoding="utf-8")

    assert_valid_png(bundle.sample_png, expected_width=64, expected_height=48, min_bytes=120)
    assert bundle.anim_gif.read_bytes()[:6] == b"GIF89a"
    assert bundle.tone_wav.stat().st_size > 16000

    if caps["ffprobe"] and bundle.tone_mp3 is not None:
        probe = subprocess.run(
            ["ffprobe", "-v", "error", "-show_entries", "format=duration", "-of", "default=nw=1", str(bundle.tone_mp3)],
            capture_output=True,
            text=True,
            timeout=5.0,
            check=False,
        )
        assert probe.returncode == 0, f"ffprobe failed on tone.mp3: {probe.stderr}"

    if caps["pdftotext"]:
        txt = subprocess.run(
            ["pdftotext", str(bundle.sample_pdf), "-"],
            capture_output=True,
            text=True,
            timeout=5.0,
            check=False,
        )
        assert txt.returncode == 0, f"pdftotext failed: {txt.stderr}"
        assert "Omarex E2E Page 1" in txt.stdout and "Omarex E2E Page 2" in txt.stdout

    if caps["pdftoppm"]:
        ppm_prefix = ctx.out_dir / "pdf_page"
        ppm = subprocess.run(
            ["pdftoppm", "-png", "-r", "72", str(bundle.sample_pdf), str(ppm_prefix)],
            capture_output=True,
            text=True,
            timeout=5.0,
            check=False,
        )
        assert ppm.returncode == 0, f"pdftoppm failed: {ppm.stderr}"
        assert (ctx.out_dir / "pdf_page-1.png").is_file()
        assert (ctx.out_dir / "pdf_page-2.png").is_file()

    if caps["git"]:
        gs = subprocess.run(
            ["git", "-C", str(bundle.git_repo), "status", "--porcelain"],
            capture_output=True,
            text=True,
            timeout=5.0,
            check=False,
        )
        assert gs.returncode == 0
        assert " M src/lib.rs" in gs.stdout
        assert "A  staged.rs" in gs.stdout
        assert "?? untracked.txt" in gs.stdout

    # Theme dir resilience check: install themes + broken dir missing colors.toml (mirrors osaka-jade)
    install_theme_into_xdg(ctx.xdg_config, ctx.xdg_state, theme="light", use_symlink=True)
    broken_dir = ctx.xdg_config / "omarchy" / "themes" / "broken-no-colors"
    assert broken_dir.is_dir() and not (broken_dir / "colors.toml").exists()

    # Point current theme symlink at broken dir -> omarex must NOT crash
    theme_link = ctx.xdg_state / "omarchy" / "current" / "theme"
    theme_link.unlink()
    theme_link.symlink_to(broken_dir)
    res, png_path, _ = ctx.headless_render(
        out_name="broken_theme_fallback.png",
        width=480,
        height=320,
        rex_mode="isolated_offline",
        rust_log="warn",
        timeout=15.0,
    )
    assert res.returncode == 0, f"omarex crashed on missing colors.toml: {res.clean_stderr}"
    assert_valid_png(png_path, expected_width=480, expected_height=320, min_bytes=1024)


def _test_rex_helper_isolation_and_live_guard(ctx: TestContext) -> None:
    if not rex_available():
        raise SkipTest("rex binary not installed on PATH")

    # 1. Missing socket must exit 4 with --autostart=false
    bad_sock = ctx.xdg_runtime / "does-not-exist.sock"
    bad_res = probe_bad_endpoint(bad_sock)
    assert bad_res.returncode == 4, (
        f"Expected exit 4 on missing socket, got {bad_res.returncode}: {bad_res.stderr!r}"
    )
    assert "autostart is disabled" in bad_res.stderr

    # 2. Live-socket read-only guard rejects mutating commands
    try:
        run_rex_readonly(["new", "should-be-blocked"], endpoint=LIVE_ENDPOINT)
    except RuntimeError as exc:
        assert "Refusing" in str(exc)
    else:
        raise AssertionError("run_rex_readonly allowed mutating 'new' command")

    if live_socket_present():
        live_info = probe_live_server()
        assert live_info["available"], f"Live server probe failed: {live_info}"
        assert live_info["status"]["api_revision"] == 24

    # 3. IsolatedRexServer lifecycle + omarex connecting to isolated server
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        assert srv.socket_path is not None
        assert len(str(srv.socket_path).encode("utf-8")) < 108
        status = srv.api_call("server.status")
        assert status["api_revision"] == 24

        created = srv.new_session(label="e2e-smoke-sess", command=["sleep", "10"])
        assert str(created.get("session_id", "")).startswith("session:"), (
            f"Unexpected rex new --json output: {created}"
        )
        assert created.get("initial_windows"), f"Missing initial_windows in {created}"

        sessions = srv.ls_sessions()
        assert any(
            s.get("session_id") == created["session_id"] and s.get("label") == "e2e-smoke-sess"
            for s in sessions
        ), f"Created session missing in ls_sessions: {sessions}"

        # Render headless against the ISOLATED Rex server (never touching live socket)
        res, png_path, report = ctx.headless_render(
            out_name="isolated_rex_live.png",
            width=800,
            height=600,
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
            timeout=15.0,
        )
        assert res.returncode == 0
        assert report is not None
        assert report.session == "e2e-smoke-sess", f"Expected isolated session, got {report}"
        assert report.windows >= 1 and report.ready_panes >= 1
        assert_valid_png(png_path, expected_width=800, expected_height=600, min_bytes=4096)

    assert not srv.socket_path.exists(), "IsolatedRexServer left socket behind after __exit__"


def register_tests(registry: TestRegistry) -> None:
    registry.add(
        TestCase(
            id="M1-SMOKE-01",
            tier=1,
            feature="F24",
            name="CLI --help and -h exit 0 with documented options",
            fn=_test_cli_help,
        )
    )
    registry.add(
        TestCase(
            id="M1-SMOKE-02",
            tier=1,
            feature="F24",
            name="CLI --version and -V print valid version and Rex rev",
            fn=_test_cli_version,
        )
    )
    registry.add(
        TestCase(
            id="M1-SMOKE-03",
            tier=1,
            feature="F24",
            name="CLI invalid flags, missing values, and bad --width exit 1",
            fn=_test_cli_negative_args,
        )
    )
    registry.add(
        TestCase(
            id="M1-SMOKE-04",
            tier=1,
            feature="F24",
            name="run_e2e.sh honors OMAREX_BIN override",
            fn=_test_binary_resolution_override,
        )
    )
    registry.add(
        TestCase(
            id="M1-SMOKE-05",
            tier=1,
            feature="F24",
            name="--headless-render in isolated XDG applies dark colors.toml",
            fn=_test_headless_render_dark_isolated_xdg,
        )
    )
    registry.add(
        TestCase(
            id="M1-SMOKE-06",
            tier=1,
            feature="F24",
            name="--headless-render --width 800 --height 600 --scale 2.0 matches IHDR",
            fn=_test_headless_render_geometry_and_scale,
        )
    )
    registry.add(
        TestCase(
            id="M1-SMOKE-07",
            tier=1,
            feature="F24",
            name="--headless-render --light differs markedly from dark render",
            fn=_test_headless_render_light_vs_dark_diff,
        )
    )
    registry.add(
        TestCase(
            id="M1-SMOKE-08",
            tier=1,
            feature="F24",
            name="--headless-viewer sample.rs:2 renders valid distinct 1440x900 PNG",
            fn=_test_headless_viewer_opens_sample_rust,
        )
    )
    registry.add(
        TestCase(
            id="M1-SMOKE-09",
            tier=1,
            feature="F14",
            name="fixtures.py media, PDF, git repo, and missing colors.toml resilience",
            fn=_test_fixtures_integrity_and_missing_colors_theme,
        )
    )
    registry.add(
        TestCase(
            id="M1-SMOKE-10",
            tier=1,
            feature="F19",
            name="rex_helper isolation, live-socket guard, and IsolatedRexServer render",
            fn=_test_rex_helper_isolation_and_live_guard,
        )
    )
