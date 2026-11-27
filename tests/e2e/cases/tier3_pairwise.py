"""Tier 3 pairwise cross-feature E2E tests (T3-01..T3-22) for omarex."""

from __future__ import annotations

import base64
import json
import re
import subprocess
import time
from pathlib import Path
from typing import Any, Callable, Optional, TypeVar

from lib.fixtures import (
    DARK_COLORS_TOML,
    LIGHT_COLORS_TOML,
    create_fixtures,
    install_theme_into_xdg,
    write_sample_wav,
)
from lib.harness import HeadlessReport, TestCase, TestContext, TestRegistry
from lib.png_check import (
    PngImage,
    assert_not_solid_color,
    assert_valid_png,
    mean_color_in_rect,
    pixel_diff_stats,
    relative_luminance,
    sample_pixel,
    unique_color_count_sample,
)
from lib.rex_helper import IsolatedRexServer, rex_available

_T = TypeVar("_T")


def _require_rex() -> None:
    assert rex_available(), "rex CLI binary is required on PATH for isolated Rex E2E tests"


def _poll_until(
    fn: Callable[[], Optional[_T]],
    timeout_s: float = 5.0,
    interval_s: float = 0.05,
    desc: str = "condition",
) -> _T:
    deadline = time.monotonic() + timeout_s
    last_exc: Optional[BaseException] = None
    while time.monotonic() < deadline:
        try:
            val = fn()
            if val is not None and val is not False:
                return val
        except AssertionError:
            raise
        except Exception as exc:  # noqa: BLE001
            last_exc = exc
        time.sleep(interval_s)
    detail = f" (last error: {last_exc})" if last_exc is not None else ""
    raise AssertionError(f"Timed out after {timeout_s:.1f}s waiting for {desc}{detail}")


def _new_session_ok(
    srv: IsolatedRexServer,
    label: str,
    command: Optional[list[str]] = None,
    cwd: Optional[Path | str] = None,
    keep_open: bool = False,
) -> dict[str, Any]:
    try:
        created = srv.new_session(
            label=label,
            command=command,
            cwd=cwd,
            keep_open=keep_open,
        )
    except Exception as exc:  # noqa: BLE001
        raise AssertionError(f"IsolatedRexServer.new_session({label!r}) failed: {exc}") from exc
    sid = created.get("session_id")
    assert isinstance(sid, str) and sid.startswith("session:"), (
        f"Unexpected session_id from new_session({label!r}): {created}"
    )
    return created


def _api_call_ok(
    srv: IsolatedRexServer,
    method: str,
    args_dict: Optional[dict[str, Any]] = None,
    session: Optional[str] = None,
) -> Any:
    try:
        return srv.api_call(method, args_dict=args_dict, session=session)
    except Exception as exc:  # noqa: BLE001
        raise AssertionError(f"IsolatedRexServer.api_call({method!r}) failed: {exc}") from exc


def _block_call_ok(
    srv: IsolatedRexServer,
    session_id: str,
    block_id: str,
    method: str,
    args_dict: Optional[dict[str, Any]] = None,
) -> Any:
    return _api_call_ok(
        srv,
        method,
        {"session_id": session_id, "block_id": block_id, "args": args_dict or {}},
    )


def _session_view_ok(srv: IsolatedRexServer, session_id: str) -> dict[str, Any]:
    view = _api_call_ok(srv, "session.view", {"session_id": session_id})
    assert isinstance(view, dict), f"Expected dict from session.view, got {view!r}"
    return view


def _first_window_and_block(srv: IsolatedRexServer, session_id: str) -> tuple[str, str]:
    view = _session_view_ok(srv, session_id)
    windows = view.get("windows") or []
    assert windows, f"session.view returned no windows for {session_id}: {view}"
    win = windows[0]
    win_id = win.get("window_id")
    block_id = win.get("focused_block_id")
    if not block_id:
        layers = win.get("layers") or []
        for layer in layers:
            blocks = layer.get("blocks") or []
            if blocks:
                block_id = blocks[0].get("block_id")
                break
    assert isinstance(win_id, str) and isinstance(block_id, str), (
        f"Missing window_id/block_id in session.view: {view}"
    )
    return win_id, block_id


def _capture_text(srv: IsolatedRexServer, session_id: str, block_id: Optional[str] = None) -> str:
    args = ["capture", "-s", session_id]
    if block_id is not None:
        args.extend(["-b", block_id])
    res = srv.run_cli(args, timeout=5.0)
    assert res.returncode == 0, f"rex capture failed (rc={res.returncode}): {res.stderr.strip()}"
    return res.stdout


def _wait_capture_contains(
    srv: IsolatedRexServer,
    session_id: str,
    needles: tuple[str, ...],
    block_id: Optional[str] = None,
    timeout_s: float = 5.0,
) -> str:
    def _check() -> Optional[str]:
        txt = _capture_text(srv, session_id, block_id=block_id)
        if all(n in txt for n in needles):
            return txt
        return None

    return _poll_until(
        _check,
        timeout_s=timeout_s,
        desc=f"capture({session_id}) containing {needles!r}",
    )


def _assert_headless_ok(
    res: Any,
    png_path: Path,
    report: Optional[HeadlessReport],
    *,
    width: int = 1440,
    height: int = 900,
    min_bytes: int = 4096,
    min_colors: int = 4,
    require_rex: bool = False,
    expected_session: Optional[str] = None,
) -> PngImage:
    assert res.returncode == 0, (
        f"omarex {res.args} exited {res.returncode}: stderr={res.clean_stderr!r}\nstdout={res.clean_stdout!r}"
    )
    assert not res.crashed, f"omarex crashed with signal {res.signal_num}: {res.clean_stderr!r}"
    assert report is not None, f"Missing HeadlessReport line in stdout: {res.clean_stdout!r}"
    assert report.width == width and report.height == height, (
        f"Expected {width}x{height} report, got {report.width}x{report.height}"
    )
    if require_rex:
        assert report.windows >= 1 and report.ready_panes >= 1, (
            f"Expected active Rex panes (windows>=1, ready_panes>=1), got {report}"
        )
        assert report.draws > 0 and report.quads > 0 and report.glyphs > 0, (
            f"Expected rendered draws/quads/glyphs > 0, got {report}"
        )
    if expected_session is not None:
        assert report.session == expected_session, (
            f"Expected session={expected_session!r}, got {report.session!r}"
        )
    img = assert_valid_png(png_path, expected_width=width, expected_height=height, min_bytes=min_bytes)
    assert_not_solid_color(img, min_unique_colors=min_colors)
    return img


# ---------------------------------------------------------------------------
# T3-01 .. T3-22
# ---------------------------------------------------------------------------


def _test_t3_01_ligatures_and_syntax_viewer(ctx: TestContext) -> None:
    _require_rex()
    bundle = create_fixtures(ctx.work_dir / "fx", generate_mp3=False, init_git=False)
    ctx.write_omarchy_theme(DARK_COLORS_TOML, "mock-dark")

    plain_txt = ctx.work_dir / "plain.txt"
    plain_txt.write_text(bundle.sample_rs.read_text(encoding="utf-8"), encoding="utf-8")

    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = _new_session_ok(
            srv,
            "t3-01",
            command=[
                "bash",
                "-c",
                "printf 'fn map(x: i32) -> i32 { match x { 0 => 1, v => v } } // :: !== === |>\\n'; sleep 15",
            ],
        )
        _wait_capture_contains(srv, created["session_id"], ("fn map", "|>"))

        # Compare ligature font (JetBrainsMono) vs non-ligature font (Liberation Mono) on identical text
        ctx.write_settings({"font_family": "Liberation Mono", "font_size_pt": 14.0})
        res_lib, p_lib, rep_lib = ctx.headless_render(
            out_name="t3_01_term_nolig.png",
            extra_args=["t3-01"],
            width=1440,
            height=900,
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )

        ctx.write_settings({"font_family": "JetBrainsMono Nerd Font", "font_size_pt": 14.0})
        res_term, p_term, rep_term = ctx.headless_render(
            out_name="t3_01_term_only.png",
            extra_args=["t3-01"],
            width=1440,
            height=900,
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        res_rs, p_rs, rep_rs = ctx.headless_render(
            out_name="t3_01_term_plus_viewer.png",
            extra_args=["t3-01"],
            width=1440,
            height=900,
            viewer_paths=[f"{bundle.sample_rs}:16"],
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        res_txt, p_txt, rep_txt = ctx.headless_render(
            out_name="t3_01_plain_viewer.png",
            extra_args=["t3-01"],
            width=1440,
            height=900,
            viewer_paths=[f"{plain_txt}:16"],
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )

    img_lib = _assert_headless_ok(
        res_lib, p_lib, rep_lib, require_rex=True, expected_session="t3-01"
    )
    img_term = _assert_headless_ok(
        res_term, p_term, rep_term, require_rex=True, expected_session="t3-01"
    )
    img_rs = _assert_headless_ok(
        res_rs, p_rs, rep_rs, require_rex=True, expected_session="t3-01", min_colors=6
    )
    img_txt = _assert_headless_ok(
        res_txt, p_txt, rep_txt, require_rex=True, expected_session="t3-01"
    )

    assert rep_term is not None and rep_lib is not None
    lig_px_diff = pixel_diff_stats(img_term, img_lib, rect=(250, 40, 900, 120))
    assert rep_term.glyphs <= rep_lib.glyphs - 5 and lig_px_diff["diff_fraction"] > 0.001, (
        f"Expected F3 operator ligatures (->, =>, ::, !==, ===, |>) with JetBrainsMono to merge glyphs "
        f"vs Liberation Mono: lig={rep_term.glyphs} vs nonlig={rep_lib.glyphs}, diff={lig_px_diff}"
    )

    open_diff = pixel_diff_stats(img_term, img_rs)
    assert open_diff["diff_fraction"] > 0.05, (
        f"Opening sample.rs viewer should visibly alter frame: {open_diff}"
    )

    rs_colors = unique_color_count_sample(img_rs, step=6)
    txt_colors = unique_color_count_sample(img_txt, step=6)
    viewer_diff = pixel_diff_stats(img_rs, img_txt, rect=(720, 40, 700, 820))
    assert rs_colors > txt_colors or viewer_diff["diff_fraction"] > 0.01, (
        f"Expected multi-token Rust syntax highlighting to differ from plain .txt "
        f"(rs_colors={rs_colors}, txt_colors={txt_colors}, viewer_diff={viewer_diff})"
    )


def _test_t3_02_powerline_over_program_bg(ctx: TestContext) -> None:
    _require_rex()
    bundle = create_fixtures(ctx.work_dir / "fx", generate_mp3=False, init_git=False)
    ctx.write_omarchy_theme(DARK_COLORS_TOML, "mock-dark")
    ctx.write_settings({"density": "comfortable", "pane_header_mode": "always"})

    tui_py = (
        "import sys, time; "
        "sys.stdout.write('\\x1b[?1049h\\x1b[48;2;14;77;110m\\x1b[2J\\x1b[2;1H"
        "\\ue0b6\\ue0b0 main \\ue0b0 src/lib.rs \\ue0b2 100% \\ue0b4'); "
        "sys.stdout.flush(); time.sleep(15)"
    )

    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        c_base = _new_session_ok(
            srv,
            "t3-02-base",
            command=["bash", "-c", f"cat '{bundle.powerline_txt}'; sleep 15"],
        )
        c_tui = _new_session_ok(
            srv,
            "t3-02-tui",
            command=["python3", "-c", tui_py],
        )
        _wait_capture_contains(srv, c_base["session_id"], ("src/lib.rs", "100%"))
        _wait_capture_contains(srv, c_tui["session_id"], ("src/lib.rs",))

        res_base, p_base, rep_base = ctx.headless_render(
            out_name="t3_02_base.png",
            extra_args=["t3-02-base"],
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        res_tui, p_tui, rep_tui = ctx.headless_render(
            out_name="t3_02_tui.png",
            extra_args=["t3-02-tui"],
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )

    img_base = _assert_headless_ok(
        res_base, p_base, rep_base, require_rex=True, expected_session="t3-02-base"
    )
    img_tui = _assert_headless_ok(
        res_tui, p_tui, rep_tui, require_rex=True, expected_session="t3-02-tui"
    )

    full_diff = pixel_diff_stats(img_base, img_tui)
    assert full_diff["diff_fraction"] > 0.20, (
        f"Expected fullscreen teal program_bg TUI to change >20% of frame: {full_diff}"
    )

    pad_base = mean_color_in_rect(img_base, 1412, 60, 16, 40)
    pad_tui = mean_color_in_rect(img_tui, 1412, 60, 16, 40)
    assert pad_tui[1] > pad_base[1] + 20.0 and pad_tui[2] > pad_base[2] + 20.0, (
        f"Expected program_bg teal fill (14,77,110) in pane padding; "
        f"base_pad={pad_base}, tui_pad={pad_tui}"
    )


def _test_t3_03_copy_mode_triggers_copy_shine(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, "mock-dark")
    ctx.write_settings({"highlight_active_border": True})

    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = _new_session_ok(
            srv,
            "t3-03",
            command=[
                "bash",
                "-c",
                "printf 'ALPHA_COPY_TARGET_LINE\\nBETA_LINE\\nGAMMA_LINE\\n'; sleep 15",
            ],
        )
        _wait_capture_contains(srv, created["session_id"], ("ALPHA_COPY_TARGET_LINE", "GAMMA_LINE"))

        res_norm, p_norm, rep_norm = ctx.headless_render(
            out_name="t3_03_normal.png",
            extra_args=["t3-03"],
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        res_copy, p_copy, rep_copy = ctx.headless_render(
            out_name="t3_03_copy_mode.png",
            extra_args=["t3-03"],
            scenario="copy-mode",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )

    img_norm = _assert_headless_ok(
        res_norm, p_norm, rep_norm, require_rex=True, expected_session="t3-03"
    )
    img_copy = _assert_headless_ok(
        res_copy, p_copy, rep_copy, require_rex=True, min_colors=6
    )

    diff = pixel_diff_stats(img_norm, img_copy)
    assert diff["diff_fraction"] > 0.01, (
        f"Expected copy-mode + Copy Shine scenario to visually differ from normal pane: {diff}"
    )


def _test_t3_04_spring_transitions_with_diagnostics_hud(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, "mock-dark")
    ctx.write_settings(
        {
            "highlight_active_border": True,
            "fade_inactive_panes": True,
            "sidebar_behavior": "always",
        }
    )

    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _new_session_ok(srv, "t3-04", command=["sleep", "15"])
        res_base, p_base, rep_base = ctx.headless_render(
            out_name="t3_04_base.png",
            extra_args=["t3-04"],
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        res_hud, p_hud, rep_hud = ctx.headless_render(
            out_name="t3_04_hud.png",
            extra_args=["t3-04"],
            scenario="diagnostics-hud",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )

    img_base = _assert_headless_ok(
        res_base, p_base, rep_base, require_rex=True, expected_session="t3-04"
    )
    img_hud = _assert_headless_ok(res_hud, p_hud, rep_hud, require_rex=True, min_colors=6)
    assert rep_base is not None and rep_hud is not None
    assert rep_hud.frame_us > 0, f"Expected frame_us > 0 in HUD report: {rep_hud}"
    assert rep_hud.quads > rep_base.quads, (
        f"Expected Diagnostics HUD graph/panel quads ({rep_hud.quads}) > base ({rep_base.quads})"
    )

    diff = pixel_diff_stats(img_base, img_hud)
    assert diff["diff_fraction"] > 0.05, (
        f"Expected diagnostics-hud overlay to change >5% of pixels: {diff}"
    )


def _test_t3_05_inertia_scroll_and_program_bg(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, "mock-dark")
    ctx.write_settings({"density": "comfortable", "smooth_scroll": True, "animation_style": "natural"})

    base_script = "seq 1 250; echo BASE_SCROLL_END; sleep 15"
    tui_py = (
        "import sys, time; "
        "sys.stdout.write('\\n'.join(str(i) for i in range(1, 251)) + '\\n'); "
        "sys.stdout.write('\\x1b[?1049h\\x1b[48;2;20;90;50m\\x1b[2J\\x1b[2;1HTUI_PROGRAM_BG_ACTIVE'); "
        "sys.stdout.flush(); time.sleep(15)"
    )

    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        c_base = _new_session_ok(srv, "t3-05-base", command=["bash", "-c", base_script])
        c_tui = _new_session_ok(srv, "t3-05-tui", command=["python3", "-c", tui_py])
        sid_base = c_base["session_id"]
        _, bid_base = _first_window_and_block(srv, sid_base)

        captured = _wait_capture_contains(srv, sid_base, ("1", "250", "BASE_SCROLL_END"))
        assert "100" in captured and "200" in captured, "Scrollback missing intermediate lines"
        _wait_capture_contains(srv, c_tui["session_id"], ("TUI_PROGRAM_BG_ACTIVE",))

        # Verify Rex wheel event contract & scrollback integrity (Note: interactive client-side
        # ↓ New output pill requires a live scrolled client session, so headless tests verify
        # scrollback preservation, smooth_scroll config, wheel event contract, and F8 program_bg).
        ev_res = _api_call_ok(
            srv,
            "block.event",
            {
                "session_id": sid_base,
                "block_id": bid_base,
                "event": {
                    "type": "wheel",
                    "payload": {
                        "pixel": {"x": 100, "y": 100},
                        "delta_x": 0,
                        "delta_y": 5,
                        "delta_z": 0,
                        "delta_mode": "line",
                        "mods": "",
                    },
                },
            },
        )
        assert isinstance(ev_res, dict), f"Expected dict response from wheel block.event: {ev_res}"

        res_bot, p_bot, rep_bot = ctx.headless_render(
            out_name="t3_05_bottom.png",
            extra_args=["t3-05-base"],
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        res_tui, p_tui, rep_tui = ctx.headless_render(
            out_name="t3_05_tui_program_bg.png",
            extra_args=["t3-05-tui"],
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )

    assert not res_bot.has_warning and not res_bot.has_error, (
        f"smooth_scroll config produced warning/error: {res_bot.clean_stdout}\n{res_bot.clean_stderr}"
    )
    img_bot = _assert_headless_ok(
        res_bot, p_bot, rep_bot, require_rex=True, expected_session="t3-05-base"
    )
    img_tui = _assert_headless_ok(
        res_tui, p_tui, rep_tui, require_rex=True, expected_session="t3-05-tui"
    )

    full_diff = pixel_diff_stats(img_bot, img_tui)
    assert full_diff["diff_fraction"] > 0.20, (
        f"Expected alt-screen program_bg TUI to change >20% of frame: {full_diff}"
    )

    pad_base = mean_color_in_rect(img_bot, 1412, 60, 16, 40)
    pad_tui = mean_color_in_rect(img_tui, 1412, 60, 16, 40)
    assert pad_tui[1] > pad_base[1] + 25.0 and pad_tui[1] > pad_tui[0] + 25.0, (
        f"Expected program_bg green fill (20,90,50) in pane padding vs dark baseline (26,11,46); "
        f"base_pad={pad_base}, tui_pad={pad_tui}"
    )


def _test_t3_06_syntax_viewer_with_inotify_reload(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fx", generate_mp3=False, init_git=False)
    ctx.write_omarchy_theme(DARK_COLORS_TOML, "mock-dark")
    ctx.write_settings({"density": "comfortable"})

    watched = ctx.work_dir / "watched.rs"
    watched.write_text(bundle.sample_rs.read_text(encoding="utf-8"), encoding="utf-8")

    # Note: Single-shot headless invocations open watched.rs fresh from disk per run;
    # live inotify re-arming requires an active open viewer process. Here we verify
    # pre/post atomic-replace rendering, syntax-viewer scenario, and F9 syntax vs .txt parity.
    res_before, p_before, rep_before = ctx.headless_render(
        out_name="t3_06_before.png",
        viewer_paths=[f"{watched}:1"],
        rex_mode="isolated_offline",
        rust_log="warn",
    )

    new_body = "\n".join(
        [
            "//! Reloaded via atomic rename for inotify verification.",
            "#[inline(always)]",
            "pub fn reloaded_pipeline(input: &[i32]) -> Vec<i32> {",
            '    let banner: &str = "RELOADED_INOTIFY_BANNER_2026";',
            "    let _len = banner.len();",
            "    input.iter().map(|v| v * 3 + 7).collect()",
            "}",
        ]
        + [f"pub const MARKER_{i:02d}: &str = \"TOKEN_{i:02d}\";" for i in range(1, 25)]
    ) + "\n"
    tmp_swap = ctx.work_dir / "watched.rs.tmp"
    tmp_swap.write_text(new_body, encoding="utf-8")
    tmp_swap.replace(watched)

    watched_plain = ctx.work_dir / "watched_plain.txt"
    watched_plain.write_text(new_body, encoding="utf-8")

    res_after, p_after, rep_after = ctx.headless_render(
        out_name="t3_06_after.png",
        viewer_paths=[f"{watched}:1"],
        rex_mode="isolated_offline",
        rust_log="warn",
    )
    res_plain, p_plain, rep_plain = ctx.headless_render(
        out_name="t3_06_plain.png",
        viewer_paths=[f"{watched_plain}:1"],
        rex_mode="isolated_offline",
        rust_log="warn",
    )
    res_scen, p_scen, rep_scen = ctx.headless_render(
        out_name="t3_06_syntax_scenario.png",
        scenario="syntax-viewer",
        viewer_paths=[f"{watched}:1"],
        rex_mode="isolated_offline",
        rust_log="warn",
    )

    img_before = _assert_headless_ok(res_before, p_before, rep_before, min_colors=6)
    img_after = _assert_headless_ok(res_after, p_after, rep_after, min_colors=6)
    img_plain = _assert_headless_ok(res_plain, p_plain, rep_plain, min_colors=4)
    _assert_headless_ok(res_scen, p_scen, rep_scen, min_colors=6)
    assert not res_after.has_error, f"Unexpected error during reload render: {res_after.clean_stderr}"

    diff = pixel_diff_stats(img_before, img_after)
    assert diff["diff_fraction"] > 0.02, (
        f"Expected reloaded watched.rs syntax view to change >2% of pixels: {diff}"
    )
    syntax_diff = pixel_diff_stats(img_after, img_plain, rect=(720, 40, 700, 820))
    assert syntax_diff["diff_fraction"] > 0.01, (
        f"Expected reloaded watched.rs syntax highlighting to differ from identical .txt: {syntax_diff}"
    )


def _test_t3_07_syntax_highlighting_with_git_diff(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fx", generate_mp3=False, init_git=True)
    ctx.write_omarchy_theme(DARK_COLORS_TOML, "mock-dark")

    clean_copy = ctx.work_dir / "clean_lib.rs"
    clean_copy.write_text(bundle.git_tracked_file.read_text(encoding="utf-8"), encoding="utf-8")

    res_clean, p_clean, rep_clean = ctx.headless_render(
        out_name="t3_07_non_git.png",
        viewer_paths=[f"{clean_copy}:1"],
        rex_mode="isolated_offline",
        rust_log="warn",
    )
    res_git, p_git, rep_git = ctx.headless_render(
        out_name="t3_07_git_diff.png",
        viewer_paths=[f"{bundle.git_tracked_file}:1"],
        rex_mode="isolated_offline",
        rust_log="warn",
    )

    img_clean = _assert_headless_ok(res_clean, p_clean, rep_clean, min_colors=6)
    img_git = _assert_headless_ok(res_git, p_git, rep_git, min_colors=6)

    diff = pixel_diff_stats(img_clean, img_git)
    assert diff["diff_fraction"] > 0.005, (
        f"Expected git-diff view of modified tracked file to differ from non-git copy: {diff}"
    )


def _test_t3_08_inotify_reload_with_search_anchor(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, "mock-dark")
    lines = [f"// filler line {i}" for i in range(1, 45)]
    lines[4] = "pub fn find_alpha() -> &'static str { \"NEEDLE_TOKEN\" }"
    lines[24] = "pub fn find_beta() -> &'static str { \"NEEDLE_TOKEN\" }"
    search_rs = ctx.work_dir / "search_reload.rs"
    search_rs.write_text("\n".join(lines) + "\n", encoding="utf-8")

    # Note: Single-shot --headless-render cannot keep a viewer open across live inotify
    # events or interactive Ctrl+F keystrokes; here we verify in-place edit update, :5 vs :25
    # line-jump (F12), syntax-viewer scenario (which exercises F12 search/selection), and .rs vs .txt.
    res_v1, p_v1, rep_v1 = ctx.headless_render(
        out_name="t3_08_v1.png",
        viewer_paths=[f"{search_rs}:5"],
        rex_mode="isolated_offline",
        rust_log="warn",
    )

    lines[4] = "pub fn find_alpha_updated() -> &'static str { \"UPDATED_NEEDLE_TOKEN_LONG_STRING\" }"
    lines[9] = "pub fn find_gamma() -> &'static str { \"NEEDLE_TOKEN\" }"
    updated_src = "\n".join(lines) + "\n"
    search_rs.write_text(updated_src, encoding="utf-8")

    plain_search = ctx.work_dir / "plain_search.txt"
    plain_search.write_text(updated_src, encoding="utf-8")

    res_v2, p_v2, rep_v2 = ctx.headless_render(
        out_name="t3_08_v2.png",
        viewer_paths=[f"{search_rs}:5"],
        rex_mode="isolated_offline",
        rust_log="warn",
    )
    res_l25, p_l25, rep_l25 = ctx.headless_render(
        out_name="t3_08_line25.png",
        viewer_paths=[f"{search_rs}:25"],
        rex_mode="isolated_offline",
        rust_log="warn",
    )
    res_plain, p_plain, rep_plain = ctx.headless_render(
        out_name="t3_08_plain.png",
        viewer_paths=[f"{plain_search}:5"],
        rex_mode="isolated_offline",
        rust_log="warn",
    )
    res_scen, p_scen, rep_scen = ctx.headless_render(
        out_name="t3_08_scenario.png",
        scenario="syntax-viewer",
        viewer_paths=[f"{search_rs}:5"],
        rex_mode="isolated_offline",
        rust_log="warn",
    )

    img_v1 = _assert_headless_ok(res_v1, p_v1, rep_v1, min_colors=6)
    img_v2 = _assert_headless_ok(res_v2, p_v2, rep_v2, min_colors=6)
    img_l25 = _assert_headless_ok(res_l25, p_l25, rep_l25, min_colors=6)
    img_plain = _assert_headless_ok(res_plain, p_plain, rep_plain, min_colors=4)
    _assert_headless_ok(res_scen, p_scen, rep_scen, min_colors=6)
    assert not res_v2.has_error, f"Unexpected error on reload: {res_v2.clean_stderr}"

    diff = pixel_diff_stats(img_v1, img_v2)
    assert diff["diff_fraction"] > 0.003, (
        f"Expected in-place edit at line 5 to update rendered viewer pixels: {diff}"
    )
    jump_diff = pixel_diff_stats(img_v2, img_l25)
    assert jump_diff["diff_fraction"] > 0.01, (
        f"Expected :5 vs :25 line jump to scroll viewer: {jump_diff}"
    )
    syntax_diff = pixel_diff_stats(img_v2, img_plain, rect=(720, 40, 700, 820))
    assert syntax_diff["diff_fraction"] > 0.01, (
        f"Expected search_reload.rs syntax colors to differ from identical .txt: {syntax_diff}"
    )


def _test_t3_09_git_diff_with_line_jump(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fx", generate_mp3=False, init_git=True)
    ctx.write_omarchy_theme(DARK_COLORS_TOML, "mock-dark")

    base_lines = [f"pub fn helper_{i:02d}(v: i32) -> i32 {{ v + {i} }}" for i in range(1, 92)]
    bundle.git_tracked_file.write_text("\n".join(base_lines) + "\n", encoding="utf-8")
    for args in (
        ["git", "-C", str(bundle.git_repo), "add", "src/lib.rs"],
        [
            "git",
            "-C",
            str(bundle.git_repo),
            "-c",
            "user.name=Omarex E2E",
            "-c",
            "user.email=e2e@example.com",
            "commit",
            "-m",
            "Expand lib.rs to 90+ lines",
        ],
    ):
        proc = subprocess.run(args, capture_output=True, text=True, timeout=5.0, check=False)
        assert proc.returncode == 0, f"git setup failed ({args}): {proc.stderr}"

    base_lines[4] = "pub fn helper_05_modified_top(v: i32) -> i32 { v * 100 }"
    base_lines[71] = "pub fn helper_72_modified_bottom(v: i32) -> i32 { v * 999 }"
    modified_text = "\n".join(base_lines) + "\n"
    bundle.git_tracked_file.write_text(modified_text, encoding="utf-8")

    non_git = ctx.work_dir / "non_git.rs"
    non_git.write_text(modified_text, encoding="utf-8")

    res_nongit, p_nongit, rep_nongit = ctx.headless_render(
        out_name="t3_09_nongit5.png",
        viewer_paths=[f"{non_git}:5"],
        rex_mode="isolated_offline",
        rust_log="warn",
    )
    res_top, p_top, rep_top = ctx.headless_render(
        out_name="t3_09_line5.png",
        viewer_paths=[f"{bundle.git_tracked_file}:5"],
        rex_mode="isolated_offline",
        rust_log="warn",
    )
    res_bot, p_bot, rep_bot = ctx.headless_render(
        out_name="t3_09_line72.png",
        viewer_paths=[f"{bundle.git_tracked_file}:72"],
        rex_mode="isolated_offline",
        rust_log="warn",
    )

    img_nongit = _assert_headless_ok(res_nongit, p_nongit, rep_nongit, min_colors=6)
    img_top = _assert_headless_ok(res_top, p_top, rep_top, min_colors=6)
    img_bot = _assert_headless_ok(res_bot, p_bot, rep_bot, min_colors=6)

    git_diff = pixel_diff_stats(img_nongit, img_top)
    assert git_diff["diff_fraction"] > 0.005, (
        f"Expected F11 git-diff view of modified tracked file at :5 to differ from non_git.rs:5: {git_diff}"
    )

    diff = pixel_diff_stats(img_top, img_bot)
    assert diff["diff_fraction"] > 0.01, (
        f"Expected git-diff viewer at :5 vs :72 to scroll to distinct hunks: {diff}"
    )


def _test_t3_10_markdown_fenced_rust_and_ligatures(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fx", generate_mp3=False, init_git=False)
    ctx.write_omarchy_theme(DARK_COLORS_TOML, "mock-dark")

    plain_md = ctx.work_dir / "plain_md.txt"
    plain_md.write_text(bundle.sample_md.read_text(encoding="utf-8"), encoding="utf-8")

    res_scen, p_scen, rep_scen = ctx.headless_render(
        out_name="t3_10_md_scenario.png",
        scenario="markdown-viewer",
        viewer_paths=[str(bundle.sample_md)],
        rex_mode="isolated_offline",
        rust_log="warn",
    )
    res_dir, p_dir, rep_dir = ctx.headless_render(
        out_name="t3_10_md_direct.png",
        viewer_paths=[str(bundle.sample_md)],
        rex_mode="isolated_offline",
        rust_log="warn",
    )
    res_plain, p_plain, rep_plain = ctx.headless_render(
        out_name="t3_10_md_plain.png",
        viewer_paths=[str(plain_md)],
        rex_mode="isolated_offline",
        rust_log="warn",
    )

    img_scen = _assert_headless_ok(res_scen, p_scen, rep_scen, min_colors=8)
    img_dir = _assert_headless_ok(res_dir, p_dir, rep_dir, min_colors=8)
    img_plain = _assert_headless_ok(res_plain, p_plain, rep_plain, min_colors=4)

    assert rep_dir is not None and rep_plain is not None
    assert rep_dir.quads > rep_plain.quads, (
        f"Expected Markdown 2.0 (tables, fenced code bg, inline image) to emit more quads than plain .txt: "
        f"md={rep_dir.quads} vs txt={rep_plain.quads}"
    )
    diff_dir = pixel_diff_stats(img_dir, img_plain, rect=(720, 40, 700, 820))
    assert diff_dir["diff_fraction"] > 0.02, (
        f"Expected Markdown 2.0 sample.md to differ from identical-text plain_md.txt in viewer: {diff_dir}"
    )
    diff_scen = pixel_diff_stats(img_scen, img_plain, rect=(720, 40, 700, 820))
    assert diff_scen["diff_fraction"] > 0.02, (
        f"Expected markdown-viewer scenario to differ from identical-text plain_md.txt: {diff_scen}"
    )


def _test_t3_11_viewer_switching_md_pdf_wav(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fx", generate_mp3=False, init_git=False)
    ctx.write_omarchy_theme(DARK_COLORS_TOML, "mock-dark")
    ctx.write_settings({"sidebar_behavior": "always", "sidebar_width_px": 248.0})

    plain_md = ctx.work_dir / "plain_md.txt"
    plain_md.write_text(bundle.sample_md.read_text(encoding="utf-8"), encoding="utf-8")
    silent_wav = ctx.work_dir / "silent.wav"
    write_sample_wav(silent_wav, duration_s=1.0, freq_hz=0.0)

    viewer_body = (910, 72, 510, 810)
    viewer_tabbar = (905, 6, 520, 38)

    res_multi, p_multi, rep_multi = ctx.headless_render(
        out_name="t3_11_multi.png",
        viewer_paths=[str(bundle.sample_md), str(bundle.sample_pdf), str(bundle.tone_wav)],
        rex_mode="isolated_offline",
        rust_log="warn",
    )
    res_md_scen, p_md_scen, rep_md_scen = ctx.headless_render(
        out_name="t3_11_md_scen.png",
        scenario="markdown-viewer",
        viewer_paths=[str(bundle.sample_md)],
        rex_mode="isolated_offline",
        rust_log="warn",
    )
    res_md, p_md, rep_md = ctx.headless_render(
        out_name="t3_11_md.png",
        viewer_paths=[str(bundle.sample_md)],
        rex_mode="isolated_offline",
        rust_log="warn",
    )
    res_plain_md, p_plain_md, rep_plain_md = ctx.headless_render(
        out_name="t3_11_plain_md.png",
        viewer_paths=[str(plain_md)],
        rex_mode="isolated_offline",
        rust_log="warn",
    )
    res_pdf1, p_pdf1, rep_pdf1 = ctx.headless_render(
        out_name="t3_11_pdf1.png",
        viewer_paths=[f"{bundle.sample_pdf}:1"],
        rex_mode="isolated_offline",
        rust_log="warn",
    )
    res_pdf2, p_pdf2, rep_pdf2 = ctx.headless_render(
        out_name="t3_11_pdf2.png",
        viewer_paths=[f"{bundle.sample_pdf}:2"],
        rex_mode="isolated_offline",
        rust_log="warn",
    )
    res_wav, p_wav, rep_wav = ctx.headless_render(
        out_name="t3_11_wav.png",
        viewer_paths=[str(bundle.tone_wav)],
        rex_mode="isolated_offline",
        rust_log="warn",
    )
    res_sil, p_sil, rep_sil = ctx.headless_render(
        out_name="t3_11_silent_wav.png",
        viewer_paths=[str(silent_wav)],
        rex_mode="isolated_offline",
        rust_log="warn",
    )

    img_multi = _assert_headless_ok(res_multi, p_multi, rep_multi, min_colors=6)
    img_md = _assert_headless_ok(res_md, p_md, rep_md, min_colors=6)
    img_plain_md = _assert_headless_ok(res_plain_md, p_plain_md, rep_plain_md, min_colors=4)
    img_pdf1 = _assert_headless_ok(res_pdf1, p_pdf1, rep_pdf1, min_colors=4)
    img_pdf2 = _assert_headless_ok(res_pdf2, p_pdf2, rep_pdf2, min_colors=4)
    img_wav = _assert_headless_ok(res_wav, p_wav, rep_wav, min_colors=4)
    img_sil = _assert_headless_ok(res_sil, p_sil, rep_sil, min_colors=4)

    assert rep_md is not None and rep_plain_md is not None
    assert rep_md.quads > rep_plain_md.quads, (
        f"Expected F13 Markdown 2.0 quads > plain_md.txt: md={rep_md.quads} vs txt={rep_plain_md.quads}"
    )
    md_vs_txt = pixel_diff_stats(img_md, img_plain_md, rect=viewer_body)
    assert md_vs_txt["diff_fraction"] > 0.02, (
        f"Expected F13 Markdown 2.0 render to differ from identical plain_md.txt: {md_vs_txt}"
    )

    assert rep_wav is not None
    assert rep_wav.quads >= 40 and rep_wav.glyphs < 1200, (
        f"Expected F14 audio waveform + scrub bar (quads>=40, glyphs<1200), got "
        f"quads={rep_wav.quads}, glyphs={rep_wav.glyphs}"
    )
    wav_vs_sil = pixel_diff_stats(img_wav, img_sil, rect=viewer_body)
    assert wav_vs_sil["diff_fraction"] > 0.005, (
        f"Expected 440Hz tone.wav waveform to differ from silent.wav inside viewer: {wav_vs_sil}"
    )

    pdf_pages_diff = pixel_diff_stats(img_pdf1, img_pdf2, rect=viewer_body)
    assert pdf_pages_diff["diff_fraction"] > 0.01, (
        f"Expected sample.pdf:1 vs :2 to differ inside viewer: {pdf_pages_diff}"
    )
    page_lum = relative_luminance(mean_color_in_rect(img_pdf1, 950, 140, 400, 500)[:3])
    assert page_lum > 0.45, f"Expected rendered PDF page luminance > 0.45, got {page_lum:.3f}"

    tabbar_diff = pixel_diff_stats(img_multi, img_md, rect=viewer_tabbar)
    assert tabbar_diff["diff_fraction"] > 0.02, (
        f"Expected 3-file viewer tabbar to differ from single-file sample.md: {tabbar_diff}"
    )

    for label, a, b in (
        ("md vs pdf1", img_md, img_pdf1),
        ("pdf1 vs wav", img_pdf1, img_wav),
        ("md vs wav", img_md, img_wav),
    ):
        diff = pixel_diff_stats(a, b, rect=viewer_body)
        assert diff["diff_fraction"] > 0.01, (
            f"Expected distinct viewer renders for {label}: {diff}"
        )

    _assert_headless_ok(res_md_scen, p_md_scen, rep_md_scen, min_colors=8)


def _test_t3_12_pdf_and_audio_dark_vs_light(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fx", generate_mp3=False, init_git=False)
    ctx.write_settings({"sidebar_behavior": "always", "sidebar_width_px": 248.0})
    install_theme_into_xdg(ctx.xdg_config, ctx.xdg_state, theme="dark")

    silent_wav = ctx.work_dir / "silent.wav"
    write_sample_wav(silent_wav, duration_s=1.0, freq_hz=0.0)
    viewer_body = (910, 72, 510, 810)

    res_pd, p_pd, rep_pd = ctx.headless_render(
        out_name="t3_12_pdf_dark.png",
        width=1440,
        height=900,
        viewer_paths=[str(bundle.sample_pdf)],
        rex_mode="isolated_offline",
        rust_log="warn",
    )
    res_wd, p_wd, rep_wd = ctx.headless_render(
        out_name="t3_12_wav_dark.png",
        width=1440,
        height=900,
        viewer_paths=[str(bundle.tone_wav)],
        rex_mode="isolated_offline",
        rust_log="warn",
    )
    res_sd, p_sd, rep_sd = ctx.headless_render(
        out_name="t3_12_silent_dark.png",
        width=1440,
        height=900,
        viewer_paths=[str(silent_wav)],
        rex_mode="isolated_offline",
        rust_log="warn",
    )

    # Switch XDG theme to light WITHOUT passing --light (verifies F17 XDG theme loading)
    install_theme_into_xdg(ctx.xdg_config, ctx.xdg_state, theme="light")
    res_pl_xdg, p_pl_xdg, rep_pl_xdg = ctx.headless_render(
        out_name="t3_12_pdf_light_xdg.png",
        width=1440,
        height=900,
        light=False,
        viewer_paths=[str(bundle.sample_pdf)],
        rex_mode="isolated_offline",
        rust_log="warn",
    )
    res_wl_xdg, p_wl_xdg, rep_wl_xdg = ctx.headless_render(
        out_name="t3_12_wav_light_xdg.png",
        width=1440,
        height=900,
        light=False,
        viewer_paths=[str(bundle.tone_wav)],
        rex_mode="isolated_offline",
        rust_log="warn",
    )
    res_ts, p_ts, rep_ts = ctx.headless_render(
        out_name="t3_12_theme_switcher.png",
        width=1440,
        height=900,
        scenario="theme-switcher",
        viewer_paths=[str(bundle.tone_wav)],
        rex_mode="isolated_offline",
        rust_log="warn",
    )

    img_pd = _assert_headless_ok(res_pd, p_pd, rep_pd, width=1440, height=900)
    img_wd = _assert_headless_ok(res_wd, p_wd, rep_wd, width=1440, height=900)
    img_sd = _assert_headless_ok(res_sd, p_sd, rep_sd, width=1440, height=900)
    img_pl_xdg = _assert_headless_ok(res_pl_xdg, p_pl_xdg, rep_pl_xdg, width=1440, height=900)
    img_wl_xdg = _assert_headless_ok(res_wl_xdg, p_wl_xdg, rep_wl_xdg, width=1440, height=900)

    assert rep_wd is not None
    assert rep_wd.quads >= 40 and rep_wd.glyphs < 1200, (
        f"Expected F14 audio waveform + scrub bar quads>=40, glyphs<1200; got {rep_wd}"
    )
    wav_vs_sil = pixel_diff_stats(img_wd, img_sd, rect=viewer_body)
    assert wav_vs_sil["diff_fraction"] > 0.005, (
        f"Expected 440Hz tone.wav waveform to differ from silent.wav: {wav_vs_sil}"
    )

    page_lum = relative_luminance(mean_color_in_rect(img_pd, 950, 140, 400, 500)[:3])
    assert page_lum > 0.45, f"Expected rendered PDF page luminance > 0.45, got {page_lum:.3f}"

    pdf_diff = pixel_diff_stats(img_pd, img_pl_xdg)
    wav_diff = pixel_diff_stats(img_wd, img_wl_xdg)
    assert pdf_diff["diff_fraction"] > 0.20, f"PDF dark vs XDG light diff too small: {pdf_diff}"
    assert wav_diff["diff_fraction"] > 0.20, f"Audio dark vs XDG light diff too small: {wav_diff}"

    lum_viewer_d = relative_luminance(mean_color_in_rect(img_wd, *viewer_body)[:3])
    lum_viewer_l = relative_luminance(mean_color_in_rect(img_wl_xdg, *viewer_body)[:3])
    assert lum_viewer_l > lum_viewer_d + 0.30, (
        f"Light viewer luminance ({lum_viewer_l:.3f}) !>> dark ({lum_viewer_d:.3f}) + 0.30"
    )

    _assert_headless_ok(res_ts, p_ts, rep_ts, width=1440, height=900, min_colors=6)


def _test_t3_13_copy_mode_osc52_clipboard_roundtrip(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, "mock-dark")
    payload = "omarex-t3-13-osc52-copy-payload"
    b64 = base64.b64encode(payload.encode("utf-8")).decode("ascii")

    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = _new_session_ok(
            srv,
            "t3-13",
            command=["bash", "-c", f"printf '\\033]52;c;{b64}\\a'; sleep 15"],
        )
        sid = created["session_id"]
        _, bid = _first_window_and_block(srv, sid)

        def _poll_clip() -> Optional[dict[str, Any]]:
            try:
                res = _block_call_ok(srv, sid, bid, "com.superlogical.terminal.clipboard_read", {})
            except AssertionError:
                return None
            if isinstance(res, dict) and res.get("data") == payload:
                return res
            return None

        clip = _poll_until(_poll_clip, timeout_s=5.0, desc="OSC 52 clipboard_read")
        assert clip.get("eof") is True and clip.get("total") == len(payload.encode("utf-8")), (
            f"Unexpected clipboard_read result: {clip}"
        )

        res_base, p_base, rep_base = ctx.headless_render(
            out_name="t3_13_base.png",
            extra_args=["t3-13"],
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        res, png_path, report = ctx.headless_render(
            out_name="t3_13_copy_mode.png",
            extra_args=["t3-13"],
            scenario="copy-mode",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )

    img_base = _assert_headless_ok(
        res_base, p_base, rep_base, require_rex=True, expected_session="t3-13"
    )
    img_copy = _assert_headless_ok(res, png_path, report, require_rex=True, min_colors=6)
    assert rep_base is not None and report is not None
    assert report.quads > rep_base.quads, (
        f"Expected copy-mode + Copy Shine quads ({report.quads}) > baseline ({rep_base.quads})"
    )
    diff = pixel_diff_stats(img_base, img_copy)
    assert diff["diff_fraction"] > 0.01, (
        f"Expected copy-mode to visibly differ from baseline session render: {diff}"
    )


def _test_t3_14_list_dir_and_live_ports_pill(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, "mock-dark")

    picker_root = ctx.work_dir / "picker_root"
    (picker_root / "sub_a").mkdir(parents=True)
    (picker_root / "sub_b").mkdir(parents=True)
    (picker_root / ".hidden_dir").mkdir(parents=True)
    (picker_root / "dev_server.py").write_text("# listener\n", encoding="utf-8")

    srv_py = (
        "import socket, time; "
        "s = socket.socket(); s.bind(('127.0.0.1', 0)); s.listen(4); "
        "print(f'BOUND_PORT={s.getsockname()[1]}', flush=True); "
        "time.sleep(15)"
    )

    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = _new_session_ok(
            srv,
            "t3-14",
            command=["python3", "-c", srv_py],
            cwd=picker_root,
        )
        sid = created["session_id"]
        _, bid = _first_window_and_block(srv, sid)
        captured = _wait_capture_contains(srv, sid, ("BOUND_PORT=",))
        match = re.search(r"BOUND_PORT=(\d+)", captured)
        assert match is not None, f"Could not parse BOUND_PORT from capture: {captured!r}"
        bound_port = int(match.group(1))

        dir_res = _block_call_ok(
            srv,
            sid,
            bid,
            "com.superlogical.terminal.list_dir",
            {"path": ".", "files": True},
        )
        assert dir_res.get("status") == "ok", f"list_dir failed: {dir_res}"
        names = {e.get("name") for e in (dir_res.get("entries") or [])}
        for expected in ("sub_a", "sub_b", ".hidden_dir", "dev_server.py"):
            assert expected in names, f"list_dir missing {expected!r} in {names}"

        def _poll_port() -> Optional[dict[str, Any]]:
            ports_res = _block_call_ok(srv, sid, bid, "com.superlogical.terminal.ports", {})
            for p in ports_res.get("ports") or []:
                if p.get("port") == bound_port and p.get("protocol") == "tcp":
                    return p
            return None

        _poll_until(_poll_port, timeout_s=6.0, desc=f"listening TCP port {bound_port}")

        res, png_path, report = ctx.headless_render(
            out_name="t3_14_ports.png",
            extra_args=["t3-14"],
            scenario="ports-pill",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )

    _assert_headless_ok(res, png_path, report, require_rex=True, min_colors=6)


def _test_t3_15_theme_switcher_with_spring_sidebar(ctx: TestContext) -> None:
    _require_rex()
    install_theme_into_xdg(ctx.xdg_config, ctx.xdg_state, theme="dark", use_symlink=True)
    ctx.write_settings(
        {
            "sidebar_behavior": "always",
            "highlight_active_border": True,
            "fade_inactive_panes": True,
        }
    )

    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _new_session_ok(srv, "t3-15", command=["sleep", "15"])
        res_dark, p_dark, rep_dark = ctx.headless_render(
            out_name="t3_15_switcher_dark.png",
            extra_args=["t3-15"],
            scenario="theme-switcher",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        install_theme_into_xdg(ctx.xdg_config, ctx.xdg_state, theme="light", use_symlink=True)
        res_light, p_light, rep_light = ctx.headless_render(
            out_name="t3_15_switcher_light.png",
            extra_args=["t3-15"],
            scenario="theme-switcher",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )

    img_dark = _assert_headless_ok(res_dark, p_dark, rep_dark, require_rex=True, min_colors=6)
    img_light = _assert_headless_ok(res_light, p_light, rep_light, require_rex=True, min_colors=6)

    diff = pixel_diff_stats(img_dark, img_light)
    assert diff["diff_fraction"] > 0.20, (
        f"Expected dark vs light theme-switcher symlink swap diff > 0.20: {diff}"
    )


def _test_t3_16_light_and_custom_xdg_across_scenarios(ctx: TestContext) -> None:
    install_theme_into_xdg(ctx.xdg_config, ctx.xdg_state, theme="dark")

    # Polarity-neutral scenario (settings-modal) verifies XDG_STATE_HOME colors.toml without --light
    res_xdg_dark, p_xdg_dark, rep_xdg_dark = ctx.headless_render(
        out_name="t3_16_modal_xdg_dark.png",
        width=960,
        height=640,
        light=False,
        scenario="settings-modal",
        rex_mode="isolated_offline",
        rust_log="warn",
    )
    # Switch XDG_STATE_HOME to light theme WITHOUT --light flag
    install_theme_into_xdg(ctx.xdg_config, ctx.xdg_state, theme="light")
    res_xdg_light, p_xdg_light, rep_xdg_light = ctx.headless_render(
        out_name="t3_16_modal_xdg_light.png",
        width=960,
        height=640,
        light=False,
        scenario="settings-modal",
        rex_mode="isolated_offline",
        rust_log="warn",
    )
    # Restore dark XDG theme and verify --light + sidebar-light override against sidebar-dark
    install_theme_into_xdg(ctx.xdg_config, ctx.xdg_state, theme="dark")
    res_dark, p_dark, rep_dark = ctx.headless_render(
        out_name="t3_16_sidebar_dark.png",
        width=960,
        height=640,
        light=False,
        scenario="sidebar-dark",
        rex_mode="isolated_offline",
        rust_log="warn",
    )
    res_light, p_light, rep_light = ctx.headless_render(
        out_name="t3_16_sidebar_light.png",
        width=960,
        height=640,
        light=True,
        scenario="sidebar-light",
        rex_mode="isolated_offline",
        rust_log="warn",
    )

    img_xdg_dark = _assert_headless_ok(
        res_xdg_dark, p_xdg_dark, rep_xdg_dark, width=960, height=640, min_colors=6
    )
    img_xdg_light = _assert_headless_ok(
        res_xdg_light, p_xdg_light, rep_xdg_light, width=960, height=640, min_colors=6
    )
    img_dark = _assert_headless_ok(res_dark, p_dark, rep_dark, width=960, height=640, min_colors=6)
    img_light = _assert_headless_ok(res_light, p_light, rep_light, width=960, height=640, min_colors=6)

    xdg_diff = pixel_diff_stats(img_xdg_dark, img_xdg_light)
    assert xdg_diff["diff_fraction"] > 0.25, (
        f"Custom XDG light theme (without --light) vs dark diff too small: {xdg_diff}"
    )
    diff = pixel_diff_stats(img_dark, img_light)
    assert diff["diff_fraction"] > 0.25, f"sidebar-dark vs sidebar-light diff too small: {diff}"

    # Outer scrimmed right-pane & corner luminance checks (outside settings-modal card at 960x640)
    lum_xdg_pane_dark = relative_luminance(mean_color_in_rect(img_xdg_dark, 780, 100, 150, 440)[:3])
    lum_xdg_pane_light = relative_luminance(mean_color_in_rect(img_xdg_light, 780, 100, 150, 440)[:3])
    assert lum_xdg_pane_light > lum_xdg_pane_dark + 0.30, (
        f"Expected XDG light outer pane lum ({lum_xdg_pane_light:.3f}) > dark ({lum_xdg_pane_dark:.3f}) + 0.30"
    )
    lum_xdg_corner_dark = relative_luminance(sample_pixel(img_xdg_dark, 2, 2)[:3])
    lum_xdg_corner_light = relative_luminance(sample_pixel(img_xdg_light, 2, 2)[:3])
    assert lum_xdg_corner_light > lum_xdg_corner_dark + 0.30, (
        f"Expected XDG light corner lum ({lum_xdg_corner_light:.3f}) > dark ({lum_xdg_corner_dark:.3f}) + 0.30"
    )

    lum_dark = relative_luminance(sample_pixel(img_dark, 2, 2)[:3])
    lum_light = relative_luminance(sample_pixel(img_light, 2, 2)[:3])
    assert lum_light > lum_dark + 0.30, (
        f"Expected sidebar-light corner lum ({lum_light:.3f}) > dark ({lum_dark:.3f}) + 0.30"
    )


def _test_t3_17_chunked_osc52_with_copy_shine(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, "mock-dark")
    payload = "omarex-shine-osc52-" * 270
    b64 = base64.b64encode(payload.encode("utf-8")).decode("ascii")

    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = _new_session_ok(
            srv,
            "t3-17",
            command=["bash", "-c", f"printf '\\033]52;c;{b64}\\a'; sleep 15"],
        )
        sid = created["session_id"]
        _, bid = _first_window_and_block(srv, sid)

        def _poll_first_chunk() -> Optional[dict[str, Any]]:
            try:
                res = _block_call_ok(
                    srv,
                    sid,
                    bid,
                    "com.superlogical.terminal.clipboard_read",
                    {"offset": 0, "max_bytes": 2048},
                )
            except AssertionError:
                return None
            if isinstance(res, dict) and res.get("total", 0) >= len(payload.encode("utf-8")):
                return res
            return None

        first = _poll_until(_poll_first_chunk, timeout_s=5.0, desc="chunked clipboard_read start")
        clip_id = first["id"]
        chunks = [first["data"]]
        offset = len(first["data"].encode("utf-8"))
        eof = bool(first.get("eof"))
        while not eof:
            nxt = _block_call_ok(
                srv,
                sid,
                bid,
                "com.superlogical.terminal.clipboard_read",
                {"id": clip_id, "offset": offset, "max_bytes": 2048},
            )
            chunks.append(nxt["data"])
            offset += len(nxt["data"].encode("utf-8"))
            eof = bool(nxt.get("eof"))

        reassembled = "".join(chunks)
        assert reassembled == payload, (
            f"Chunked clipboard_read mismatch: got len={len(reassembled)}, expected={len(payload)}"
        )

        res_base, p_base, rep_base = ctx.headless_render(
            out_name="t3_17_base.png",
            extra_args=["t3-17"],
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        res, png_path, report = ctx.headless_render(
            out_name="t3_17_copy_shine.png",
            extra_args=["t3-17"],
            scenario="copy-mode",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )

    img_base = _assert_headless_ok(
        res_base, p_base, rep_base, require_rex=True, expected_session="t3-17"
    )
    img_copy = _assert_headless_ok(res, png_path, report, require_rex=True, min_colors=6)
    assert rep_base is not None and report is not None
    assert report.quads > rep_base.quads, (
        f"Expected copy-mode + Copy Shine quads ({report.quads}) > baseline ({rep_base.quads})"
    )
    diff = pixel_diff_stats(img_base, img_copy)
    assert diff["diff_fraction"] > 0.01, (
        f"Expected copy-mode + Copy Shine to visibly differ from baseline render: {diff}"
    )


def _test_t3_18_live_ports_and_diagnostics_hud(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, "mock-dark")

    srv_py = (
        "import socket, time; "
        "s = socket.socket(); s.bind(('127.0.0.1', 0)); s.listen(2); "
        "print(f'PORT={s.getsockname()[1]}', flush=True); "
        "time.sleep(15)"
    )

    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = _new_session_ok(srv, "t3-18", command=["python3", "-c", srv_py])
        sid = created["session_id"]
        _, bid = _first_window_and_block(srv, sid)
        captured = _wait_capture_contains(srv, sid, ("PORT=",))
        port_m = re.search(r"PORT=(\d+)", captured)
        assert port_m is not None, f"Missing PORT= in capture: {captured!r}"
        port = int(port_m.group(1))

        _poll_until(
            lambda: any(
                p.get("port") == port
                for p in (_block_call_ok(srv, sid, bid, "com.superlogical.terminal.ports", {}).get("ports") or [])
            ),
            timeout_s=6.0,
            desc=f"ports RPC seeing {port}",
        )

        status = _api_call_ok(srv, "server.status")
        assert status.get("api_revision") == 24 and status.get("session_count", 0) >= 1, (
            f"Unexpected server.status: {status}"
        )

        res_ports, p_ports, rep_ports = ctx.headless_render(
            out_name="t3_18_ports.png",
            extra_args=["t3-18"],
            scenario="ports-pill",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        res_hud, p_hud, rep_hud = ctx.headless_render(
            out_name="t3_18_hud.png",
            extra_args=["t3-18"],
            scenario="diagnostics-hud",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )

    img_ports = _assert_headless_ok(res_ports, p_ports, rep_ports, require_rex=True, min_colors=6)
    img_hud = _assert_headless_ok(res_hud, p_hud, rep_hud, require_rex=True, min_colors=6)
    diff = pixel_diff_stats(img_ports, img_hud)
    assert diff["diff_fraction"] > 0.05, (
        f"Expected Diagnostics HUD over live port session to differ from ports-pill: {diff}"
    )


def _test_t3_19_diagnostics_hud_and_settings_modal_coexistence(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, "mock-dark")
    ctx.write_settings(
        {
            "sidebar_behavior": "always",
            "density": "comfortable",
            "material": {"kind": "classic"},
        }
    )

    res_base, p_base, rep_base = ctx.headless_render(
        out_name="t3_19_base.png", rex_mode="isolated_offline", rust_log="warn"
    )
    res_hud, p_hud, rep_hud = ctx.headless_render(
        out_name="t3_19_hud.png",
        scenario="diagnostics-hud",
        rex_mode="isolated_offline",
        rust_log="warn",
    )
    res_set, p_set, rep_set = ctx.headless_render(
        out_name="t3_19_settings.png",
        scenario="settings-modal",
        rex_mode="isolated_offline",
        rust_log="warn",
    )
    res_rest, p_rest, rep_rest = ctx.headless_render(
        out_name="t3_19_restored.png", rex_mode="isolated_offline", rust_log="warn"
    )

    img_base = _assert_headless_ok(res_base, p_base, rep_base)
    img_hud = _assert_headless_ok(res_hud, p_hud, rep_hud, min_colors=6)
    img_set = _assert_headless_ok(res_set, p_set, rep_set, min_colors=6)
    img_rest = _assert_headless_ok(res_rest, p_rest, rep_rest)

    assert pixel_diff_stats(img_base, img_hud)["diff_fraction"] > 0.05, (
        "diagnostics-hud should differ visibly from baseline"
    )
    assert pixel_diff_stats(img_base, img_set)["diff_fraction"] > 0.05, (
        "settings-modal should differ visibly from baseline"
    )
    assert pixel_diff_stats(img_hud, img_set)["diff_fraction"] > 0.02, (
        "diagnostics-hud and settings-modal should render distinct modal content"
    )
    assert pixel_diff_stats(img_base, img_rest)["diff_fraction"] < 0.01, (
        "baseline re-render should remain clean after modals"
    )


def _test_t3_20_settings_material_density_with_theme_switch(ctx: TestContext) -> None:
    install_theme_into_xdg(ctx.xdg_config, ctx.xdg_state, theme="dark")
    ctx.write_settings(
        {
            "density": "comfortable",
            "material": {"kind": "classic"},
            "sidebar_width_px": 260.0,
        }
    )
    res_a, p_a, rep_a = ctx.headless_render(
        out_name="t3_20_a.png",
        scenario="settings-modal",
        rex_mode="isolated_offline",
        rust_log="warn",
    )

    install_theme_into_xdg(ctx.xdg_config, ctx.xdg_state, theme="light")
    settings_b = {
        "density": "compact",
        "material": {"kind": "translucent", "alpha": 0.85},
        "sidebar_width_px": 200.0,
    }
    settings_path = ctx.write_settings(settings_b)
    res_b, p_b, rep_b = ctx.headless_render(
        out_name="t3_20_b.png",
        scenario="settings-modal",
        rex_mode="isolated_offline",
        rust_log="warn",
    )

    img_a = _assert_headless_ok(res_a, p_a, rep_a, min_colors=6)
    img_b = _assert_headless_ok(res_b, p_b, rep_b, min_colors=6)
    assert not res_b.has_error, f"Unexpected error with translucent compact config: {res_b.clean_stderr}"

    diff = pixel_diff_stats(img_a, img_b)
    assert diff["diff_fraction"] > 0.25, (
        f"Expected dark+comfortable+classic vs light+compact+translucent diff > 0.25: {diff}"
    )

    persisted = json.loads(settings_path.read_text(encoding="utf-8"))
    assert persisted == settings_b, f"settings.json altered unexpectedly: {persisted}"


def _test_t3_21_dnd_split_float_and_transfer_window(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, "mock-dark")

    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        ca = _new_session_ok(srv, "t3-21-a", command=["sleep", "20"])
        cb = _new_session_ok(srv, "t3-21-b", command=["sleep", "20"])
        sid_a = ca["session_id"]
        sid_b = cb["session_id"]

        split_res = srv.run_cli(["split", "-s", sid_a, "--split=right", "--", "sleep", "20"])
        assert split_res.returncode == 0, f"rex split failed: {split_res.stderr}"

        def _two_blocks() -> Optional[tuple[str, str, int]]:
            v = _session_view_ok(srv, sid_a)
            wins = v.get("windows") or []
            if not wins:
                return None
            tiled = (wins[0].get("layers") or [{}])[0]
            blocks = tiled.get("blocks") or []
            if len(blocks) >= 2:
                return blocks[0]["block_id"], blocks[1]["block_id"], int(v["revision"])
            return None

        b1, b2, rev0 = _poll_until(_two_blocks, timeout_s=5.0, desc="2 tiled blocks after split")
        swap_res = _api_call_ok(
            srv,
            "session.swap_blocks",
            {"session_id": sid_a, "block_id": b1, "other_block_id": b2},
        )
        assert int(swap_res["revision"]) > rev0, f"swap_blocks did not advance revision: {swap_res}"

        nw_res = _api_call_ok(
            srv,
            "session.new_window",
            {
                "session_id": sid_a,
                "window_label": "tearoff",
                "layout": {"block": {"flavor": "com.superlogical.terminal.shell", "label": "torn"}},
            },
        )
        win2 = nw_res["window_id"]
        xfer_res = _api_call_ok(
            srv,
            "session.transfer_window",
            {"session_id": sid_a, "window_id": win2, "to_session_id": sid_b},
        )
        assert int(xfer_res["to_revision"]) >= 1, f"transfer_window unexpected: {xfer_res}"

        # Render dnd-split while t3-21-a still has active tiled splits (matching T4-07)
        res_dnd, p_dnd, rep_dnd = ctx.headless_render(
            out_name="t3_21_dnd.png",
            extra_args=["t3-21-a"],
            scenario="dnd-split",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )

        # Float both remaining tiled blocks in t3-21-a so tiled layer becomes empty (FloatOrDock edge)
        _api_call_ok(
            srv,
            "session.float_block",
            {"session_id": sid_a, "block_id": b1, "bounds": [0.05, 0.05, 0.45, 0.8]},
        )
        _api_call_ok(
            srv,
            "session.float_block",
            {"session_id": sid_a, "block_id": b2, "bounds": [0.52, 0.05, 0.43, 0.8]},
        )
        view_after_float = _session_view_ok(srv, sid_a)
        tiled_after = (view_after_float["windows"][0]["layers"] or [])[0]
        assert tiled_after.get("layout") is None, (
            f"Expected empty tiled layout after floating all blocks: {tiled_after}"
        )

        res_flt, p_flt, rep_flt = ctx.headless_render(
            out_name="t3_21_floating.png",
            extra_args=["t3-21-a"],
            scenario="floating-pr",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        res_mw, p_mw, rep_mw = ctx.headless_render(
            out_name="t3_21_multiwin.png",
            extra_args=["t3-21-b"],
            scenario="multi-window",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )

    img_dnd = _assert_headless_ok(res_dnd, p_dnd, rep_dnd, require_rex=True, min_colors=6)
    img_flt = _assert_headless_ok(res_flt, p_flt, rep_flt, require_rex=True, min_colors=6)
    _assert_headless_ok(res_mw, p_mw, rep_mw, require_rex=True, min_colors=6)

    diff = pixel_diff_stats(img_dnd, img_flt)
    assert diff["diff_fraction"] > 0.01, (
        f"Expected dnd-split and floating-pr scenarios to differ visually: {diff}"
    )


def _test_t3_22_multi_window_shared_gpu_and_scale(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, "mock-dark")
    ctx.write_settings({"sidebar_behavior": "always"})

    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _new_session_ok(srv, "t3-22-w1", command=["sleep", "15"])
        _new_session_ok(srv, "t3-22-w2", command=["sleep", "15"])

        res_single, p_single, rep_single = ctx.headless_render(
            out_name="t3_22_single.png",
            width=1280,
            height=720,
            scale=1.0,
            scenario="sidebar-dark",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        res_1x, p_1x, rep_1x = ctx.headless_render(
            out_name="t3_22_1x.png",
            width=1280,
            height=720,
            scale=1.0,
            scenario="multi-window",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        res_2x, p_2x, rep_2x = ctx.headless_render(
            out_name="t3_22_2x.png",
            width=1280,
            height=720,
            scale=2.0,
            scenario="multi-window",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )

    img_single = _assert_headless_ok(
        res_single, p_single, rep_single, width=1280, height=720, require_rex=True, min_colors=6
    )
    img_1x = _assert_headless_ok(
        res_1x, p_1x, rep_1x, width=1280, height=720, require_rex=True, min_colors=6
    )
    img_2x = _assert_headless_ok(
        res_2x, p_2x, rep_2x, width=1280, height=720, require_rex=True, min_colors=6
    )
    assert rep_1x is not None and rep_2x is not None
    assert rep_1x.gpu != "" and rep_2x.gpu != "", "Expected non-empty GPU adapter name"
    assert abs(rep_1x.scale - 1.0) < 1e-3 and abs(rep_2x.scale - 2.0) < 1e-3

    mw_vs_single = pixel_diff_stats(img_single, img_1x)
    assert rep_1x.windows >= 2 or mw_vs_single["diff_fraction"] > 0.02, (
        f"Expected multi-window scenario to differ from single-window sidebar-dark "
        f"(windows={rep_1x.windows}, diff={mw_vs_single})"
    )

    diff = pixel_diff_stats(img_1x, img_2x)
    assert diff["diff_fraction"] > 0.05, (
        f"Expected multi-window 1.0x vs 2.0x scale to differ visibly: {diff}"
    )


def register_tests(registry: TestRegistry) -> None:
    specs = [
        ("T3-01", "F3+F9", "Terminal ligatures (->, =>, ::) + viewer multi-token syntax highlighting on sample.rs", _test_t3_01_ligatures_and_syntax_viewer),
        ("T3-02", "F4+F8", "Powerline glyphs (U+E0B0..=U+E0D7) over fullscreen program_bg TUI padding fill", _test_t3_02_powerline_over_program_bg),
        ("T3-03", "F5+F15", "Vim Copy Mode (v + y) triggering 220 ms GPU Copy Shine", _test_t3_03_copy_mode_triggers_copy_shine),
        ("T3-04", "F6+F20", "Animated spring/fade transitions paired with Rich Diagnostics HUD frame-time graph", _test_t3_04_spring_transitions_with_diagnostics_hud),
        ("T3-05", "F7+F8", "Inertia scroll + New output pill inside colored program_bg pane", _test_t3_05_inertia_scroll_and_program_bg),
        ("T3-06", "F9+F10", "Syntax-highlighted file in viewer auto-reloaded via inotify after external edit", _test_t3_06_syntax_viewer_with_inotify_reload),
        ("T3-07", "F9+F11", "Multi-token syntax highlighting combined with git-diff +/- view in git_repo", _test_t3_07_syntax_highlighting_with_git_diff),
        ("T3-08", "F10+F12", "inotify auto-reload while Ctrl+F search match is active in viewer", _test_t3_08_inotify_reload_with_search_anchor),
        ("T3-09", "F11+F12", "Git-diff viewer combined with path:line jump and selection copy", _test_t3_09_git_diff_with_line_jump),
        ("T3-10", "F13+F3", "Markdown 2.0 fenced rust code blocks rendered with operator ligatures", _test_t3_10_markdown_fenced_rust_and_ligatures),
        ("T3-11", "F13+F14", "Switching viewer tabs between sample.md, sample.pdf, and tone.wav", _test_t3_11_viewer_switching_md_pdf_wav),
        ("T3-12", "F14+F17", "PDF / Audio waveform viewer contrast under dark vs --light Omarchy themes", _test_t3_12_pdf_and_audio_dark_vs_light),
        ("T3-13", "F15+F18", "Copy Mode yank propagating through Rex OSC 52 clipboard_written/clipboard_read", _test_t3_13_copy_mode_osc52_clipboard_roundtrip),
        ("T3-14", "F16+F19", "Remote-capable File/Dir-Picker (list_dir) alongside live :<port> detection in isolated Rex", _test_t3_14_list_dir_and_live_ports_pill),
        ("T3-15", "F17+F6", "Live-preview Theme-Switcher paired with spring border & sidebar slide/fade", _test_t3_15_theme_switcher_with_spring_sidebar),
        ("T3-16", "F17+F24", "--light and custom XDG_STATE_HOME themes across --headless-scenario outputs", _test_t3_16_light_and_custom_xdg_across_scenarios),
        ("T3-17", "F18+F5", "OSC 52 remote clipboard event paired with Copy Shine feedback", _test_t3_17_chunked_osc52_with_copy_shine),
        ("T3-18", "F19+F20", "Live :<port> listener telemetry reflected in Diagnostics HUD", _test_t3_18_live_ports_and_diagnostics_hud),
        ("T3-19", "F20+F21", "Diagnostics HUD + Categorized Settings modal coexistence & clean dismissal", _test_t3_19_diagnostics_hud_and_settings_modal_coexistence),
        ("T3-20", "F21+F17", "Settings modal material/density changes combined with Omarchy theme switch", _test_t3_20_settings_material_density_with_theme_switch),
        ("T3-21", "F22+F23", "5-zone DnD split / FloatOrDock paired with MoveTabToNewWindow multi-window", _test_t3_21_dnd_split_float_and_transfer_window),
        ("T3-22", "F23+F24", "Multi-window shared GPU/atlas verified through --headless-scenario multi-window", _test_t3_22_multi_window_shared_gpu_and_scale),
    ]
    for case_id, feature, name, fn in specs:
        registry.add(TestCase(id=case_id, tier=3, feature=feature, name=name, fn=fn))
