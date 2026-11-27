"""Tier 4 real-world end-to-end scenario tests (T4-01..T4-11) for omarex."""

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
    create_fixtures,
    install_theme_into_xdg,
    tool_capabilities,
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
from lib.rex_helper import (
    IsolatedRexServer,
    live_socket_present,
    probe_bad_endpoint,
    probe_live_server,
    rex_available,
)

_T = TypeVar("_T")


def _require_rex() -> None:
    assert rex_available(), "rex CLI binary is required on PATH for isolated Rex E2E tests"


def _poll_until(
    fn: Callable[[], Optional[_T]],
    timeout_s: float = 6.0,
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
        for layer in win.get("layers") or []:
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
    min_ready_panes: int = 1,
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
        assert report.windows >= 1 and report.ready_panes >= min_ready_panes, (
            f"Expected windows>=1, ready_panes>={min_ready_panes}, got {report}"
        )
        assert report.draws > 0 and report.quads > 0 and report.glyphs > 0, (
            f"Expected draws/quads/glyphs > 0, got {report}"
        )
    if expected_session is not None:
        assert report.session == expected_session, (
            f"Expected session={expected_session!r}, got {report.session!r}"
        )
    img = assert_valid_png(png_path, expected_width=width, expected_height=height, min_bytes=min_bytes)
    assert_not_solid_color(img, min_unique_colors=min_colors)
    return img


# ---------------------------------------------------------------------------
# T4-01 .. T4-11
# ---------------------------------------------------------------------------


def _test_t4_01_rust_code_review_and_diff_audit(ctx: TestContext) -> None:
    _require_rex()
    bundle = create_fixtures(ctx.work_dir / "fx", generate_mp3=False, init_git=True)
    ctx.write_omarchy_theme(DARK_COLORS_TOML, "mock-dark")
    ctx.write_settings({"font_size_pt": 11.5})

    lines = [
        f"pub fn stage_{i:02d}(x: i32) -> i32 {{ match x {{ 0 => 1, v => v + {i} }} }} // :: !== === |>"
        for i in range(1, 90)
    ]
    bundle.git_tracked_file.write_text("\n".join(lines) + "\n", encoding="utf-8")
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
            "Baseline 89-line Rust file",
        ],
    ):
        proc = subprocess.run(args, capture_output=True, text=True, timeout=5.0, check=False)
        assert proc.returncode == 0, f"git setup failed ({args}): {proc.stderr}"

    lines[7] = "pub fn stage_08_modified(x: i32) -> Result<i32, &'static str> { Ok(x * 2) }"
    lines[67] = "pub fn stage_68_modified(x: i32) -> i32 { x ^ 0x55 }"
    modified_step1 = "\n".join(lines) + "\n"
    bundle.git_tracked_file.write_text(modified_step1, encoding="utf-8")

    plain_audit = ctx.work_dir / "plain_audit.txt"
    plain_audit.write_text(modified_step1, encoding="utf-8")

    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = _new_session_ok(
            srv,
            "t4-01",
            command=[
                "bash",
                "-c",
                "git diff --stat; printf 'fn review(a: i32) -> i32 { a |> f } :: => !== ===\\n'; sleep 20",
            ],
            cwd=bundle.git_repo,
        )
        _wait_capture_contains(srv, created["session_id"], ("lib.rs", "|>"))

        res_plain, p_plain, rep_plain = ctx.headless_render(
            out_name="t4_01_plain_line8.png",
            extra_args=["t4-01"],
            viewer_paths=[f"{plain_audit}:8"],
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        res_s1, p_s1, rep_s1 = ctx.headless_render(
            out_name="t4_01_step1_line8.png",
            extra_args=["t4-01"],
            viewer_paths=[f"{bundle.git_tracked_file}:8"],
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )

        # Simulate external edit before step 2
        lines[67] = "pub fn audit_check(v: i32) -> Result<i32, &'static str> { if v != 0 { Ok(v) } else { Err(\"zero\") } }"
        bundle.git_tracked_file.write_text("\n".join(lines) + "\n", encoding="utf-8")

        res_s2, p_s2, rep_s2 = ctx.headless_render(
            out_name="t4_01_step2_line68.png",
            extra_args=["t4-01"],
            viewer_paths=[f"{bundle.git_tracked_file}:68"],
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        res_s3, p_s3, rep_s3 = ctx.headless_render(
            out_name="t4_01_step3_syntax.png",
            extra_args=["t4-01"],
            scenario="syntax-viewer",
            viewer_paths=[f"{bundle.git_tracked_file}:68"],
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )

    img_plain = _assert_headless_ok(
        res_plain, p_plain, rep_plain, require_rex=True, expected_session="t4-01", min_colors=4
    )
    img_s1 = _assert_headless_ok(
        res_s1, p_s1, rep_s1, require_rex=True, expected_session="t4-01", min_colors=8
    )
    img_s2 = _assert_headless_ok(
        res_s2, p_s2, rep_s2, require_rex=True, expected_session="t4-01", min_colors=8
    )

    f9_f11_diff = pixel_diff_stats(img_s1, img_plain, rect=(720, 40, 700, 820))
    assert f9_f11_diff["diff_fraction"] > 0.005, (
        f"Expected F9 syntax + F11 git-diff on git_tracked_file:8 to differ from plain_audit.txt:8: {f9_f11_diff}"
    )

    diff = pixel_diff_stats(img_s1, img_s2)
    assert diff["diff_fraction"] > 0.01, (
        f"Expected line jump + external edit diff between line 8 and line 68: {diff}"
    )

    _assert_headless_ok(res_s3, p_s3, rep_s3, require_rex=True, min_colors=8)


def _test_t4_02_agent_documentation_and_media_inspection(ctx: TestContext) -> None:
    caps = tool_capabilities()
    bundle = create_fixtures(ctx.work_dir / "fx", generate_mp3=caps["ffmpeg"], init_git=False)
    ctx.write_omarchy_theme(DARK_COLORS_TOML, "mock-dark")

    res_md, p_md, rep_md = ctx.headless_render(
        out_name="t4_02_md.png",
        scenario="markdown-viewer",
        viewer_paths=[str(bundle.sample_md)],
        rex_mode="isolated_offline",
        rust_log="warn",
    )
    res_p1, p_p1, rep_p1 = ctx.headless_render(
        out_name="t4_02_pdf_p1.png",
        viewer_paths=[f"{bundle.sample_pdf}:1"],
        rex_mode="isolated_offline",
        rust_log="warn",
    )
    res_p2, p_p2, rep_p2 = ctx.headless_render(
        out_name="t4_02_pdf_p2.png",
        viewer_paths=[f"{bundle.sample_pdf}:2"],
        rex_mode="isolated_offline",
        rust_log="warn",
    )
    res_wav, p_wav, rep_wav = ctx.headless_render(
        out_name="t4_02_wav.png",
        viewer_paths=[str(bundle.tone_wav)],
        rex_mode="isolated_offline",
        rust_log="warn",
    )

    img_md = _assert_headless_ok(res_md, p_md, rep_md, min_colors=8)
    img_p1 = _assert_headless_ok(res_p1, p_p1, rep_p1, min_colors=4)
    img_p2 = _assert_headless_ok(res_p2, p_p2, rep_p2, min_colors=4)
    img_wav = _assert_headless_ok(res_wav, p_wav, rep_wav, min_colors=4)

    if bundle.tone_mp3 is not None:
        res_mp3, p_mp3, rep_mp3 = ctx.headless_render(
            out_name="t4_02_mp3.png",
            viewer_paths=[str(bundle.tone_mp3)],
            rex_mode="isolated_offline",
            rust_log="warn",
        )
        _assert_headless_ok(res_mp3, p_mp3, rep_mp3, min_colors=4)

    assert unique_color_count_sample(img_md, step=6) >= 8, (
        "Expected Markdown 2.0 (table + fenced Rust + inline PNG) to have >= 8 colors"
    )
    pdf_diff = pixel_diff_stats(img_p1, img_p2)
    assert pdf_diff["diff_fraction"] > 0.002, (
        f"Expected PDF Page 1 vs Page 2 to render distinct pages: {pdf_diff}"
    )
    assert pixel_diff_stats(img_md, img_wav)["diff_fraction"] > 0.02
    assert pixel_diff_stats(img_p1, img_wav)["diff_fraction"] > 0.01


def _test_t4_03_long_build_output_and_copy_mode_yank(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, "mock-dark")
    ctx.write_settings({"smooth_scroll": True, "animation_style": "natural"})
    osc_payload = "BUILD_ERR_LINE_0195"
    b64 = base64.b64encode(osc_payload.encode("utf-8")).decode("ascii")

    build_script = (
        f"for i in $(seq -w 1 220); do printf 'BUILD_LINE_%s\\n' \"$i\"; done; "
        f"printf '\\033]52;c;{b64}\\a'; "
        f"for j in 1 2 3 4 5; do printf 'STREAM_TAIL_0%s\\n' \"$j\"; done; "
        f"sleep 20"
    )

    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = _new_session_ok(srv, "t4-03", command=["bash", "-c", build_script])
        sid = created["session_id"]
        _, bid = _first_window_and_block(srv, sid)
        captured = _wait_capture_contains(srv, sid, ("BUILD_LINE_220", "STREAM_TAIL_05"))
        assert "BUILD_LINE_001" in captured and "BUILD_LINE_100" in captured

        ev_res = _api_call_ok(
            srv,
            "block.event",
            {
                "session_id": sid,
                "block_id": bid,
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
            out_name="t4_03_bottom.png",
            extra_args=["t4-03"],
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )

        clip = _poll_until(
            lambda: (
                lambda r: r if isinstance(r, dict) and r.get("data") == osc_payload else None
            )(_block_call_ok(srv, sid, bid, "com.superlogical.terminal.clipboard_read", {})),
            timeout_s=5.0,
            desc="OSC 52 clipboard_read",
        )
        assert clip.get("eof") is True, f"Expected eof=True: {clip}"

        res_copy, p_copy, rep_copy = ctx.headless_render(
            out_name="t4_03_copy_mode.png",
            extra_args=["t4-03"],
            scenario="copy-mode",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )

    assert not res_bot.has_warning and not res_bot.has_error, (
        f"smooth_scroll config produced warning/error: {res_bot.clean_stdout}\n{res_bot.clean_stderr}"
    )
    img_bot = _assert_headless_ok(
        res_bot, p_bot, rep_bot, require_rex=True, expected_session="t4-03"
    )
    img_copy = _assert_headless_ok(
        res_copy, p_copy, rep_copy, require_rex=True, min_colors=6
    )

    assert rep_bot is not None and rep_copy is not None
    assert rep_copy.quads > rep_bot.quads, (
        f"Expected copy-mode + Copy Shine quads ({rep_copy.quads}) > baseline ({rep_bot.quads})"
    )
    diff = pixel_diff_stats(img_bot, img_copy)
    assert diff["diff_fraction"] > 0.01, (
        f"Expected copy-mode scenario after build output to differ visibly from bottom state: {diff}"
    )


def _test_t4_04_fullscreen_tui_and_powerline_splits(ctx: TestContext) -> None:
    _require_rex()
    create_fixtures(ctx.work_dir / "fx", generate_mp3=False, init_git=False)
    ctx.write_omarchy_theme(DARK_COLORS_TOML, "mock-dark")
    ctx.write_settings(
        {
            "highlight_active_border": True,
            "fade_inactive_panes": True,
            "density": "comfortable",
        }
    )

    tui_py = (
        "import sys, time; "
        "sys.stdout.write('\\x1b[?1049h\\x1b[48;2;18;82;58m\\x1b[2J\\x1b[2;1H"
        "\\ue0b6\\ue0b0 TUI \\ue0b0 fn(x) -> y => z \\ue0b4\\n"
        "\\u250c\\u2500\\u2500\\u2500\\u2510'); "
        "sys.stdout.flush(); time.sleep(20)"
    )

    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = _new_session_ok(srv, "t4-04", command=["python3", "-c", tui_py])
        sid = created["session_id"]
        split_res = srv.run_cli(
            [
                "split",
                "-s",
                sid,
                "--split=right",
                "--",
                "bash",
                "-c",
                "printf 'normal pane -> :: |>\\n'; sleep 20",
            ]
        )
        assert split_res.returncode == 0, f"rex split failed: {split_res.stderr}"

        def _get_two_blocks() -> Optional[list[str]]:
            v = _session_view_ok(srv, sid)
            wins = v.get("windows") or []
            if not wins:
                return None
            blocks = (wins[0].get("layers") or [{}])[0].get("blocks") or []
            if len(blocks) >= 2:
                return [blocks[0]["block_id"], blocks[1]["block_id"]]
            return None

        b1, b2 = _poll_until(_get_two_blocks, timeout_s=5.0, desc="2 split blocks in t4-04")
        _wait_capture_contains(srv, sid, ("fn(x)",), block_id=b1)
        _wait_capture_contains(srv, sid, ("normal pane",), block_id=b2)

        res_live, p_live, rep_live = ctx.headless_render(
            out_name="t4_04_live_split.png",
            extra_args=["t4-04"],
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        res_scen, p_scen, rep_scen = ctx.headless_render(
            out_name="t4_04_scenario_split.png",
            extra_args=["t4-04"],
            scenario="split-panes",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )

    img_live = _assert_headless_ok(
        res_live,
        p_live,
        rep_live,
        require_rex=True,
        min_ready_panes=2,
        expected_session="t4-04",
        min_colors=8,
    )
    _assert_headless_ok(res_scen, p_scen, rep_scen, require_rex=True, min_colors=8)

    left_mean = mean_color_in_rect(img_live, 340, 120, 120, 120)
    right_mean = mean_color_in_rect(img_live, 1080, 120, 120, 120)
    assert left_mean[1] > right_mean[1] + 20.0, (
        f"Expected emerald program_bg (18,82,58) in left split pane vs dark theme in right: "
        f"left={left_mean}, right={right_mean}"
    )


def _test_t4_05_live_omarchy_theme_and_settings_customization(ctx: TestContext) -> None:
    install_theme_into_xdg(ctx.xdg_config, ctx.xdg_state, theme="dark", use_symlink=True)
    ctx.write_settings(
        {
            "sidebar_behavior": "always",
            "sidebar_width_px": 240.0,
            "density": "comfortable",
            "highlight_active_border": True,
        }
    )

    res_set, p_set, rep_set = ctx.headless_render(
        out_name="t4_05_settings.png",
        scenario="settings-modal",
        rex_mode="isolated_offline",
        rust_log="warn",
    )
    res_dark, p_dark, rep_dark = ctx.headless_render(
        out_name="t4_05_switcher_dark.png",
        scenario="theme-switcher",
        rex_mode="isolated_offline",
        rust_log="warn",
    )

    install_theme_into_xdg(ctx.xdg_config, ctx.xdg_state, theme="light", use_symlink=True)
    ctx.write_settings(
        {
            "sidebar_behavior": "always",
            "sidebar_width_px": 220.0,
            "density": "compact",
            "material": {"kind": "translucent", "alpha": 0.88},
        }
    )
    res_light, p_light, rep_light = ctx.headless_render(
        out_name="t4_05_switcher_light.png",
        scenario="theme-switcher",
        rex_mode="isolated_offline",
        rust_log="warn",
    )

    broken_dir = ctx.xdg_config / "omarchy" / "themes" / "broken-no-colors"
    theme_link = ctx.xdg_state / "omarchy" / "current" / "theme"
    if theme_link.exists() or theme_link.is_symlink():
        theme_link.unlink()
    theme_link.symlink_to(broken_dir)
    res_brk, p_brk, rep_brk = ctx.headless_render(
        out_name="t4_05_broken_theme.png",
        scenario="sidebar-dark",
        rex_mode="isolated_offline",
        rust_log="warn",
    )

    _assert_headless_ok(res_set, p_set, rep_set, min_colors=6)
    img_dark = _assert_headless_ok(res_dark, p_dark, rep_dark, min_colors=6)
    img_light = _assert_headless_ok(res_light, p_light, rep_light, min_colors=6)
    _assert_headless_ok(res_brk, p_brk, rep_brk, min_colors=4)

    diff = pixel_diff_stats(img_dark, img_light)
    assert diff["diff_fraction"] > 0.25, (
        f"Expected dark vs light theme-switcher diff > 0.25: {diff}"
    )
    lum_dark = relative_luminance(sample_pixel(img_dark, 2, 2)[:3])
    lum_light = relative_luminance(sample_pixel(img_light, 2, 2)[:3])
    assert lum_light > lum_dark + 0.30, (
        f"Expected light corner luminance ({lum_light:.3f}) > dark ({lum_dark:.3f}) + 0.30"
    )


def _test_t4_06_local_dev_server_and_port_forwarding_ops(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, "mock-dark")
    ctx.write_settings({"sidebar_behavior": "always"})

    dev_proj = ctx.work_dir / "dev_proj"
    (dev_proj / "src").mkdir(parents=True)
    (dev_proj / "index.html").write_text("<h1>omarex dev</h1>\n", encoding="utf-8")
    (dev_proj / "src" / "app.py").write_text("print('ok')\n", encoding="utf-8")
    (dev_proj / ".env").write_text("PORT=0\n", encoding="utf-8")

    srv_py = (
        "import http.server, socketserver, threading, time; "
        "httpd = socketserver.TCPServer(('127.0.0.1', 0), http.server.SimpleHTTPRequestHandler); "
        "port = httpd.server_address[1]; "
        "threading.Thread(target=httpd.serve_forever, daemon=True).start(); "
        "print(f'DEV_PORT={port}', flush=True); "
        "time.sleep(20)"
    )

    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = _new_session_ok(srv, "t4-06", command=["python3", "-c", srv_py], cwd=dev_proj)
        sid = created["session_id"]
        _, bid = _first_window_and_block(srv, sid)
        captured = _wait_capture_contains(srv, sid, ("DEV_PORT=",))
        port_m = re.search(r"DEV_PORT=(\d+)", captured)
        assert port_m is not None, f"Missing DEV_PORT= in capture: {captured!r}"
        dev_port = int(port_m.group(1))

        dir_res = _block_call_ok(
            srv,
            sid,
            bid,
            "com.superlogical.terminal.list_dir",
            {"path": ".", "files": True},
        )
        assert dir_res.get("status") == "ok", f"list_dir failed: {dir_res}"
        names = {e.get("name") for e in (dir_res.get("entries") or [])}
        assert {"index.html", "src", ".env"}.issubset(names), f"Unexpected entries: {names}"

        _poll_until(
            lambda: any(
                p.get("port") == dev_port and p.get("protocol") == "tcp"
                for p in (_block_call_ok(srv, sid, bid, "com.superlogical.terminal.ports", {}).get("ports") or [])
            ),
            timeout_s=6.0,
            desc=f"ports RPC detecting DEV_PORT {dev_port}",
        )

        # Verify tunnel creator capability / info RPC if registered on server
        creators_res = _api_call_ok(srv, "block_creator.list")
        creators = {c.get("name"): c for c in (creators_res.get("creators") or [])}
        if "com.superlogical.tunnel" in creators:
            flavors = {f.get("flavor"): f for f in (creators["com.superlogical.tunnel"].get("flavors") or [])}
            if "com.superlogical.tunnel.listen" in flavors:
                opts = {
                    "address": "127.0.0.1",
                    "port": 0,
                    "client_target": f"127.0.0.1:{dev_port}",
                }
                tblk = _api_call_ok(
                    srv,
                    "session.new_block",
                    {
                        "session_id": sid,
                        "flavor": "com.superlogical.tunnel.listen",
                        "label": "dev-fwd",
                        "options": opts,
                    },
                )
                tinfo = _block_call_ok(srv, sid, tblk["block_id"], "com.superlogical.tunnel.info", {})
                assert tinfo.get("flavor") == "listen" and "connections" in tinfo, (
                    f"Unexpected tunnel.info: {tinfo}"
                )

        res_ports, p_ports, rep_ports = ctx.headless_render(
            out_name="t4_06_ports_pill.png",
            extra_args=["t4-06"],
            scenario="ports-pill",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        res_hud, p_hud, rep_hud = ctx.headless_render(
            out_name="t4_06_hud.png",
            extra_args=["t4-06"],
            scenario="diagnostics-hud",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )

    img_ports = _assert_headless_ok(res_ports, p_ports, rep_ports, require_rex=True, min_colors=6)
    img_hud = _assert_headless_ok(res_hud, p_hud, rep_hud, require_rex=True, min_colors=6)
    diff = pixel_diff_stats(img_ports, img_hud)
    assert diff["diff_fraction"] > 0.05, f"Expected ports-pill vs diagnostics-hud diff > 0.05: {diff}"


def _test_t4_07_workspace_split_float_and_multiwindow_tearoff(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, "mock-dark")
    ctx.write_settings({"highlight_active_border": True})

    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        c_main = _new_session_ok(srv, "t4-07-main", command=["sleep", "20"])
        c_dest = _new_session_ok(srv, "t4-07-dest", command=["sleep", "20"])
        sid_main = c_main["session_id"]
        sid_dest = c_dest["session_id"]

        _, b1 = _first_window_and_block(srv, sid_main)
        sp1 = _api_call_ok(
            srv,
            "session.new_split",
            {
                "session_id": sid_main,
                "anchor_id": b1,
                "direction": "horizontal",
                "side": "after",
                "layout": {"block": {"flavor": "com.superlogical.terminal.shell", "label": "pane2"}},
                "focus": True,
            },
        )
        b2 = sp1["block_ids"][0]
        _api_call_ok(
            srv,
            "session.swap_blocks",
            {"session_id": sid_main, "block_id": b1, "other_block_id": b2},
        )

        win2_res = _api_call_ok(
            srv,
            "session.new_window",
            {
                "session_id": sid_main,
                "window_label": "torn-win",
                "layout": {"block": {"flavor": "com.superlogical.terminal.shell", "label": "torn"}},
            },
        )
        _api_call_ok(
            srv,
            "session.transfer_window",
            {"session_id": sid_main, "window_id": win2_res["window_id"], "to_session_id": sid_dest},
        )

        # Render split-panes and dnd-split WHILE t4-07-main still has active tiled splits
        res_sp, p_sp, rep_sp = ctx.headless_render(
            out_name="t4_07_splits.png",
            extra_args=["t4-07-main"],
            scenario="split-panes",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        res_dn, p_dn, rep_dn = ctx.headless_render(
            out_name="t4_07_dnd.png",
            extra_args=["t4-07-main"],
            scenario="dnd-split",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )

        # Now float both tiled blocks in main window -> tiled layer empty (FloatOrDock fix)
        _api_call_ok(
            srv,
            "session.float_block",
            {"session_id": sid_main, "block_id": b1, "bounds": [0.1, 0.1, 0.4, 0.7]},
        )
        _api_call_ok(
            srv,
            "session.float_block",
            {"session_id": sid_main, "block_id": b2, "bounds": [0.52, 0.1, 0.4, 0.7]},
        )
        view_after_float = _session_view_ok(srv, sid_main)
        tiled_after = (view_after_float["windows"][0]["layers"] or [])[0]
        assert tiled_after.get("layout") is None, (
            f"Expected empty tiled layout after floating all blocks: {tiled_after}"
        )

        res_fl, p_fl, rep_fl = ctx.headless_render(
            out_name="t4_07_float.png",
            extra_args=["t4-07-main"],
            scenario="floating-pr",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        res_mw, p_mw, rep_mw = ctx.headless_render(
            out_name="t4_07_multiwin.png",
            extra_args=["t4-07-dest"],
            scenario="multi-window",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )

    img_sp = _assert_headless_ok(res_sp, p_sp, rep_sp, require_rex=True, min_colors=6)
    img_dn = _assert_headless_ok(res_dn, p_dn, rep_dn, require_rex=True, min_colors=6)
    img_fl = _assert_headless_ok(res_fl, p_fl, rep_fl, require_rex=True, min_colors=6)
    img_mw = _assert_headless_ok(res_mw, p_mw, rep_mw, require_rex=True, min_colors=6)

    assert pixel_diff_stats(img_sp, img_dn)["diff_fraction"] > 0.01
    assert pixel_diff_stats(img_sp, img_fl)["diff_fraction"] > 0.01
    assert pixel_diff_stats(img_sp, img_mw)["diff_fraction"] > 0.01 or (
        rep_mw is not None and rep_mw.windows >= 2
    ), "Expected multi-window render to differ from split-panes render"


def _test_t4_08_headless_visual_regression_all_13_scenarios(ctx: TestContext) -> None:
    _require_rex()
    bundle = create_fixtures(ctx.work_dir / "fx", generate_mp3=False, init_git=True)
    install_theme_into_xdg(ctx.xdg_config, ctx.xdg_state, theme="dark")
    ctx.write_settings({"sidebar_behavior": "always", "highlight_active_border": True})

    scenarios: list[tuple[str, bool, Optional[list[str | Path]]]] = [
        ("sidebar-dark", False, None),
        ("sidebar-light", True, None),
        ("split-panes", False, None),
        ("floating-pr", False, None),
        ("copy-mode", False, None),
        ("syntax-viewer", False, [f"{bundle.sample_rs}:1"]),
        ("diagnostics-hud", False, None),
        ("ports-pill", False, None),
        ("markdown-viewer", False, [str(bundle.sample_md)]),
        ("theme-switcher", False, None),
        ("settings-modal", False, None),
        ("dnd-split", False, None),
        ("multi-window", False, None),
    ]

    imgs: dict[str, PngImage] = {}
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _new_session_ok(srv, "t4-08", command=["sleep", "45"])
        for name, is_light, viewers in scenarios:
            res, png_path, report = ctx.headless_render(
                out_name=f"t4_08_{name}.png",
                extra_args=["t4-08"],
                light=is_light,
                viewer_paths=viewers,
                scenario=name,
                rex_mode="custom_socket",
                custom_socket=srv.endpoint,
                rust_log="warn",
            )
            imgs[name] = _assert_headless_ok(
                res,
                png_path,
                report,
                require_rex=True,
                min_bytes=4096,
                min_colors=4,
            )

    assert pixel_diff_stats(imgs["sidebar-dark"], imgs["sidebar-light"])["diff_fraction"] > 0.25
    lum_dark = relative_luminance(sample_pixel(imgs["sidebar-dark"], 2, 2)[:3])
    lum_light = relative_luminance(sample_pixel(imgs["sidebar-light"], 2, 2)[:3])
    assert lum_light > lum_dark + 0.30, (
        f"Expected sidebar-light lum ({lum_light:.3f}) > sidebar-dark ({lum_dark:.3f}) + 0.30"
    )
    for overlay in (
        "diagnostics-hud",
        "settings-modal",
        "theme-switcher",
        "floating-pr",
        "dnd-split",
        "copy-mode",
        "syntax-viewer",
    ):
        diff = pixel_diff_stats(imgs["sidebar-dark"], imgs[overlay])
        assert diff["diff_fraction"] > 0.01, (
            f"Scenario {overlay!r} did not visibly differ from sidebar-dark: {diff}"
        )


def _test_t4_09_offline_degraded_rex_resilience(ctx: TestContext) -> None:
    _require_rex()
    live_before = probe_live_server() if live_socket_present() else None

    settings_dir = ctx.xdg_config / "omarex"
    settings_dir.mkdir(parents=True, exist_ok=True)
    (settings_dir / "settings.json").write_text("{not valid json: [1, 2", encoding="utf-8")

    broken_dir = ctx.work_dir / "missing-colors-theme"
    broken_dir.mkdir(parents=True, exist_ok=True)
    state_theme_dir = ctx.xdg_state / "omarchy" / "current"
    state_theme_dir.mkdir(parents=True, exist_ok=True)
    (state_theme_dir / "theme.name").write_text("broken-theme\n", encoding="utf-8")
    (state_theme_dir / "theme").symlink_to(broken_dir)

    bad_res = probe_bad_endpoint(ctx.xdg_runtime / "dead-rex.sock")
    assert bad_res.returncode == 4 and "autostart is disabled" in bad_res.stderr, (
        f"Expected exit 4 + 'autostart is disabled', got rc={bad_res.returncode}: {bad_res.stderr!r}"
    )

    res_off, p_off, rep_off = ctx.headless_render(
        out_name="t4_09_offline.png",
        width=800,
        height=600,
        rex_mode="isolated_offline",
        rust_log="warn",
    )
    res_hud, p_hud, rep_hud = ctx.headless_render(
        out_name="t4_09_hud_offline.png",
        width=800,
        height=600,
        scenario="diagnostics-hud",
        rex_mode="isolated_offline",
        rust_log="warn",
    )

    img_off = _assert_headless_ok(res_off, p_off, rep_off, width=800, height=600, min_bytes=2048)
    assert rep_off is not None
    assert rep_off.session is None and rep_off.windows == 0 and rep_off.ready_panes == 0
    combined_log = res_off.clean_stdout + "\n" + res_off.clean_stderr
    assert res_off.has_warning and "settings.json" in combined_log, (
        f"Expected WARN log referencing corrupt settings.json: stdout={res_off.clean_stdout!r} stderr={res_off.clean_stderr!r}"
    )

    img_hud = _assert_headless_ok(res_hud, p_hud, rep_hud, width=800, height=600, min_bytes=2048)
    diff = pixel_diff_stats(img_off, img_hud)
    assert diff["diff_fraction"] > 0.03, (
        f"Expected offline diagnostics-hud to render visibly over offline fallback: {diff}"
    )

    if live_before is not None and live_socket_present():
        live_after = probe_live_server()
        before_ids = {s.get("session_id") for s in live_before.get("sessions", [])}
        after_ids = {s.get("session_id") for s in live_after.get("sessions", [])}
        assert before_ids == after_ids, (
            f"Live Rex socket sessions mutated during offline test: before={before_ids}, after={after_ids}"
        )


def _assert_rect_non_collapsed(img: PngImage, rect: tuple[int, int, int, int], label: str) -> None:
    x0, y0, rw, rh = rect
    x1 = min(img.width, x0 + rw)
    y1 = min(img.height, y0 + rh)
    colors: set[tuple[int, int, int]] = set()
    for y in range(max(0, y0), y1, 4):
        for x in range(max(0, x0), x1, 4):
            r, g, b, _ = sample_pixel(img, x, y)
            colors.add((r, g, b))
            if len(colors) >= 3:
                return
    raise AssertionError(
        f"Expected non-collapsed rendered content (>=3 colors) in {label} rect={rect}, found {len(colors)}: {colors}"
    )


def _test_t4_10_hidpi_multiscale_workstation_session(ctx: TestContext) -> None:
    _require_rex()
    bundle = create_fixtures(ctx.work_dir / "fx", generate_mp3=False, init_git=False)
    ctx.write_omarchy_theme(DARK_COLORS_TOML, "mock-dark")
    ctx.write_settings({"font_size_pt": 11.0, "sidebar_behavior": "always"})

    plain_rs = ctx.work_dir / "plain_rs.txt"
    plain_rs.write_text(bundle.sample_rs.read_text(encoding="utf-8"), encoding="utf-8")
    plain_md = ctx.work_dir / "plain_md.txt"
    plain_md.write_text(bundle.sample_md.read_text(encoding="utf-8"), encoding="utf-8")

    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = _new_session_ok(
            srv,
            "t4-10",
            command=[
                "bash",
                "-c",
                "printf '\\xee\\x82\\xb0 main \\xee\\x82\\xb0 fn(x) -> y => z :: !== === |>\\n'; sleep 20",
            ],
        )
        _wait_capture_contains(srv, created["session_id"], ("fn(x)", "|>"))

        res_1x, p_1x, rep_1x = ctx.headless_render(
            out_name="t4_10_1x.png",
            width=1280,
            height=800,
            scale=1.0,
            extra_args=["t4-10"],
            viewer_paths=[f"{bundle.sample_rs}:1"],
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        res_15x, p_15x, rep_15x = ctx.headless_render(
            out_name="t4_10_15x.png",
            width=1920,
            height=1200,
            scale=1.5,
            extra_args=["t4-10"],
            viewer_paths=[f"{bundle.sample_rs}:1"],
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        res_15x_txt, p_15x_txt, rep_15x_txt = ctx.headless_render(
            out_name="t4_10_15x_txt.png",
            width=1920,
            height=1200,
            scale=1.5,
            extra_args=["t4-10"],
            viewer_paths=[f"{plain_rs}:1"],
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        res_2x, p_2x, rep_2x = ctx.headless_render(
            out_name="t4_10_2x.png",
            width=2560,
            height=1600,
            scale=2.0,
            extra_args=["t4-10"],
            scenario="markdown-viewer",
            viewer_paths=[str(bundle.sample_md)],
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        res_2x_txt, p_2x_txt, rep_2x_txt = ctx.headless_render(
            out_name="t4_10_2x_txt.png",
            width=2560,
            height=1600,
            scale=2.0,
            extra_args=["t4-10"],
            viewer_paths=[str(plain_md)],
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        res_cmp2x, p_cmp2x, rep_cmp2x = ctx.headless_render(
            out_name="t4_10_1280_2x.png",
            width=1280,
            height=800,
            scale=2.0,
            extra_args=["t4-10"],
            scenario="syntax-viewer",
            viewer_paths=[f"{bundle.sample_rs}:1"],
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )

    img_1x = _assert_headless_ok(res_1x, p_1x, rep_1x, width=1280, height=800, require_rex=True, min_colors=6)
    img_15x = _assert_headless_ok(res_15x, p_15x, rep_15x, width=1920, height=1200, require_rex=True, min_colors=6)
    img_15x_txt = _assert_headless_ok(res_15x_txt, p_15x_txt, rep_15x_txt, width=1920, height=1200, require_rex=True, min_colors=4)
    img_2x_txt = _assert_headless_ok(res_2x_txt, p_2x_txt, rep_2x_txt, width=2560, height=1600, require_rex=True, min_colors=4)

    assert rep_1x is not None and abs(rep_1x.scale - 1.0) < 1e-3
    assert rep_15x is not None and abs(rep_15x.scale - 1.5) < 1e-3

    f9_hidpi_diff = pixel_diff_stats(img_15x, img_15x_txt, rect=(1100, 80, 780, 1080))
    assert f9_hidpi_diff["diff_fraction"] > 0.01, (
        f"Expected F9 syntax highlighting on sample.rs vs plain_rs.txt at 1.5x HiDPI: {f9_hidpi_diff}"
    )

    img_2x = _assert_headless_ok(res_2x, p_2x, rep_2x, width=2560, height=1600, require_rex=True, min_colors=8)
    img_cmp2x = _assert_headless_ok(
        res_cmp2x, p_cmp2x, rep_cmp2x, width=1280, height=800, require_rex=True, min_colors=6
    )

    assert rep_2x is not None and rep_2x_txt is not None and abs(rep_2x.scale - 2.0) < 1e-3

    assert rep_2x.quads > rep_2x_txt.quads, (
        f"Expected F13 Markdown 2.0 quads ({rep_2x.quads}) > plain_md.txt ({rep_2x_txt.quads}) at 2.0x"
    )
    f13_hidpi_diff = pixel_diff_stats(img_2x, img_2x_txt, rect=(1400, 100, 1100, 1400))
    assert f13_hidpi_diff["diff_fraction"] > 0.02, (
        f"Expected F13 markdown-viewer vs plain_md.txt at 2.0x HiDPI: {f13_hidpi_diff}"
    )

    diff = pixel_diff_stats(img_1x, img_cmp2x)
    assert diff["diff_fraction"] > 0.05, (
        f"Expected 1.0x vs 2.0x scale at 1280x800 to rescale UI/glyphs visibly: {diff}"
    )

    # Non-clipping check at 1280x800 @2.0x (640 logical px auto-collapses sidebar): left-pane, terminal, and viewer remain visible
    _assert_rect_non_collapsed(img_cmp2x, (10, 60, 200, 680), "left-pane-margin @2x (auto-collapsed sidebar)")
    _assert_rect_non_collapsed(img_cmp2x, (280, 60, 400, 680), "terminal @2x")
    _assert_rect_non_collapsed(img_cmp2x, (740, 60, 500, 680), "viewer @2x")


def _test_t4_11_isolated_rex_multisession_lifecycle_and_telemetry(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, "mock-dark")
    ctx.write_settings({"warm_session_cache_limit": 4})

    clip_str = "t4-11-clip"
    clip_b64 = base64.b64encode(clip_str.encode("utf-8")).decode("ascii")
    alpha_py = (
        "import socket, time; "
        "s = socket.socket(); s.bind(('127.0.0.1', 0)); s.listen(2); "
        "print(f'PORT={s.getsockname()[1]}', flush=True); "
        "time.sleep(25)"
    )
    beta_cmd = (
        "printf '\\033]7501;state=blocked:app=t4agent:msg=Need approval\\a'; "
        "sleep 0.2; "
        "printf '\\033]7501;state=done:app=t4agent:msg=Finished\\a'; "
        f"printf '\\033]52;c;{clip_b64}\\a'; "
        "echo BETA_READY; sleep 25"
    )

    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        sock_path = srv.socket_path
        assert sock_path is not None
        st0 = _api_call_ok(srv, "server.status")
        assert st0.get("api_revision") == 24 and st0.get("session_count") == 0, (
            f"Unexpected initial server.status: {st0}"
        )

        ca = _new_session_ok(srv, "alpha", command=["python3", "-c", alpha_py], cwd=ctx.work_dir)
        cb = _new_session_ok(srv, "beta", command=["bash", "-c", beta_cmd])
        _new_session_ok(srv, "gamma", command=["sleep", "25"])

        sid_a = ca["session_id"]
        sid_b = cb["session_id"]
        _, bid_a = _first_window_and_block(srv, sid_a)
        _, bid_b = _first_window_and_block(srv, sid_b)

        try:
            sessions = srv.ls_sessions()
        except Exception as exc:  # noqa: BLE001
            raise AssertionError(f"srv.ls_sessions() failed: {exc}") from exc
        labels = {s.get("label") for s in sessions}
        assert {"alpha", "beta", "gamma"}.issubset(labels), f"Missing sessions: {labels}"

        st1 = _api_call_ok(srv, "server.status")
        assert st1.get("session_count") == 3, f"Expected session_count==3, got {st1}"

        cap_a = _wait_capture_contains(srv, sid_a, ("PORT=",))
        alpha_port = int(re.search(r"PORT=(\d+)", cap_a).group(1))
        _wait_capture_contains(srv, sid_b, ("BETA_READY",))

        dir_a = _block_call_ok(
            srv, sid_a, bid_a, "com.superlogical.terminal.list_dir", {"path": ".", "files": True}
        )
        assert dir_a.get("status") == "ok", f"alpha list_dir failed: {dir_a}"

        _poll_until(
            lambda: any(
                p.get("port") == alpha_port
                for p in (_block_call_ok(srv, sid_a, bid_a, "com.superlogical.terminal.ports", {}).get("ports") or [])
            ),
            timeout_s=6.0,
            desc=f"alpha port {alpha_port}",
        )

        pstat = _poll_until(
            lambda: (
                lambda r: r if (r.get("records") or []) else None
            )(_block_call_ok(srv, sid_b, bid_b, "com.superlogical.terminal.program_status", {})),
            timeout_s=5.0,
            desc="beta OSC 7501 program_status",
        )
        assert pstat["records"][0].get("state") in ("blocked", "done", "idle", "working", "error"), (
            f"Unexpected program_status record: {pstat}"
        )

        clip_b = _poll_until(
            lambda: (
                lambda r: r if isinstance(r, dict) and r.get("data") == clip_str else None
            )(_block_call_ok(srv, sid_b, bid_b, "com.superlogical.terminal.clipboard_read", {})),
            timeout_s=5.0,
            desc="beta OSC 52 clipboard_read",
        )
        assert clip_b.get("eof") is True

        res_a, p_a, rep_a = ctx.headless_render(
            out_name="t4_11_alpha.png",
            extra_args=["alpha"],
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        res_hud, p_hud, rep_hud = ctx.headless_render(
            out_name="t4_11_hud.png",
            extra_args=["beta"],
            scenario="diagnostics-hud",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )

    assert not sock_path.exists(), f"IsolatedRexServer left socket behind: {sock_path}"
    dead = probe_bad_endpoint(sock_path)
    assert dead.returncode == 4 and "autostart is disabled" in dead.stderr

    _assert_headless_ok(res_a, p_a, rep_a, require_rex=True, expected_session="alpha")
    _assert_headless_ok(res_hud, p_hud, rep_hud, require_rex=True, min_colors=6)


def register_tests(registry: TestRegistry) -> None:
    specs = [
        ("T4-01", "F3,F9,F10,F11,F12", "Rust Code Review & Diff Audit", _test_t4_01_rust_code_review_and_diff_audit),
        ("T4-02", "F9,F13,F14,F24", "Agent Documentation & Media Inspection", _test_t4_02_agent_documentation_and_media_inspection),
        ("T4-03", "F5,F7,F15,F18", "Long-Running Build Output & Copy Mode Yank", _test_t4_03_long_build_output_and_copy_mode_yank),
        ("T4-04", "F3,F4,F6,F8,F24", "Fullscreen TUI & Powerline Prompt Session", _test_t4_04_fullscreen_tui_and_powerline_splits),
        ("T4-05", "F6,F17,F21,F24", "Live Omarchy Theme & Settings Customization", _test_t4_05_live_omarchy_theme_and_settings_customization),
        ("T4-06", "F16,F19,F20,F21", "Local Dev Server & Port-Forwarding Ops", _test_t4_06_local_dev_server_and_port_forwarding_ops),
        ("T4-07", "F6,F22,F23,F24", "Workspace Split, Float & Multi-Window Tear-Off", _test_t4_07_workspace_split_float_and_multiwindow_tearoff),
        ("T4-08", "F24", "Headless Visual Regression Gate (All 13 Scenarios)", _test_t4_08_headless_visual_regression_all_13_scenarios),
        ("T4-09", "F18,F19,F20,F24", "Offline / Degraded Rex Resilience", _test_t4_09_offline_degraded_rex_resilience),
        ("T4-10", "F3,F4,F9,F13,F24", "HiDPI Multi-Scale Workstation Session", _test_t4_10_hidpi_multiscale_workstation_session),
        ("T4-11", "F16,F18,F19,F20,F21", "Isolated Rex Multi-Session Lifecycle & Telemetry", _test_t4_11_isolated_rex_multisession_lifecycle_and_telemetry),
    ]
    for case_id, feature, name, fn in specs:
        registry.add(TestCase(id=case_id, tier=4, feature=feature, name=name, fn=fn))
