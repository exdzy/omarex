"""Tier 1 and Tier 2 opaque-box E2E tests for R1 features F3 through F8."""

from __future__ import annotations

import base64
import json
import time
from typing import Any

from lib.fixtures import DARK_COLORS_TOML, LIGHT_COLORS_TOML, create_fixtures
from lib.harness import HeadlessReport, TestCase, TestContext, TestRegistry
from lib.png_check import (
    PngImage,
    assert_color_near,
    assert_not_solid_color,
    assert_valid_png,
    mean_color_in_rect,
    pixel_diff_stats,
    relative_luminance,
    sample_pixel,
    unique_color_count_sample,
)
from lib.rex_helper import IsolatedRexServer, rex_available


def _require_rex() -> None:
    assert rex_available(), "rex CLI binary is required on PATH for isolated Rex tests"


def _first_block_id(created: dict[str, Any]) -> str:
    wins = created.get("initial_windows") or []
    assert wins, f"Missing initial_windows in session creation payload: {created}"
    bids = wins[0].get("block_ids") or []
    assert bids, f"Missing block_ids in initial_windows[0]: {created}"
    return str(bids[0])


def _api_call_or_fail(
    srv: IsolatedRexServer,
    method: str,
    args_dict: dict[str, Any] | None = None,
    timeout: float = 5.0,
) -> Any:
    try:
        return srv.api_call(method, args_dict=args_dict, timeout=timeout)
    except RuntimeError as exc:
        raise AssertionError(f"Rex API call {method} failed: {exc}") from exc


def _new_session_or_fail(
    srv: IsolatedRexServer,
    label: str,
    command: list[str],
    timeout: float = 5.0,
) -> tuple[str, str]:
    try:
        created = srv.new_session(label=label, command=command, timeout=timeout)
    except RuntimeError as exc:
        raise AssertionError(f"Rex new_session({label!r}) failed: {exc}") from exc
    sid = str(created.get("session_id", ""))
    assert sid.startswith("session:"), f"Unexpected session_id in {created}"
    return sid, _first_block_id(created)


def _render_session(
    ctx: TestContext,
    srv: IsolatedRexServer,
    session_label: str,
    out_name: str,
    width: int = 800,
    height: int = 300,
    scale: float | None = None,
    light: bool = False,
    scenario: str | None = None,
    viewer_paths: list[str] | None = None,
) -> tuple[PngImage, HeadlessReport]:
    res, png_path, report = ctx.headless_render(
        out_name=out_name,
        extra_args=[session_label],
        width=width,
        height=height,
        scale=scale,
        light=light,
        viewer_paths=viewer_paths,
        scenario=scenario,
        rex_mode="custom_socket",
        custom_socket=srv.endpoint,
        rust_log="off",
        timeout=15.0,
    )
    assert res.returncode == 0, (
        f"omarex --headless-render failed (rc={res.returncode}): {res.clean_stderr}\n{res.clean_stdout}"
    )
    assert not res.crashed, f"omarex crashed: signal={res.signal_num}"
    assert report is not None, f"Missing headless render report in stdout:\n{res.clean_stdout}"
    assert report.windows >= 1 and report.ready_panes >= 1, (
        f"Expected active Rex window/pane, got {report}"
    )
    img = assert_valid_png(png_path, expected_width=width, expected_height=height, min_bytes=1024)
    return img, report


def _wait_format_contains(
    srv: IsolatedRexServer,
    sid: str,
    bid: str,
    needle: str,
    timeout_s: float = 4.0,
) -> str:
    deadline = time.monotonic() + timeout_s
    last_text = ""
    while time.monotonic() < deadline:
        res = _api_call_or_fail(
            srv,
            "block.method",
            {"session_id": sid, "block_id": bid, "method": "format", "args": {"format": "text"}},
        )
        last_text = str((res or {}).get("content", ""))
        if needle in last_text:
            return last_text
        time.sleep(0.08)
    raise AssertionError(f"Timed out waiting for {needle!r} in block format output; got {last_text[-300:]!r}")


def _green_x_bounds_in_band(img: PngImage, y0: int, y1: int) -> tuple[int, int, int]:
    """Return (min_x, max_x, count) of green-dominant pixels in [y0, y1)."""
    min_x = img.width
    max_x = -1
    count = 0
    px = img.pixels
    w = img.width
    for y in range(max(0, y0), min(img.height, y1)):
        base = y * w * 4
        for x in range(w):
            idx = base + x * 4
            r, g, b = px[idx], px[idx + 1], px[idx + 2]
            if g >= 160 and g > r + 60 and g > b + 60:
                if x < min_x:
                    min_x = x
                if x > max_x:
                    max_x = x
                count += 1
    return min_x, max_x, count


# ---------------------------------------------------------------------------
# F3: Run-shaping & ligatures (->, =>, !==, ===, ::, |>)
# ---------------------------------------------------------------------------


def _test_f03_t1_01_ascii_fastpath_render(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    ctx.write_settings({"sidebar_behavior": "hidden", "pane_header_mode": "hidden"})
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _new_session_or_fail(
            srv,
            "ascii",
            [
                "sh",
                "-c",
                "printf '\\033[2J\\033[H0123456789 ABCDEFGHIJKLMNOPQRSTUVWXYZ abcdefghijklmnopqrstuvwxyz\\n'; sleep 6",
            ],
        )
        time.sleep(0.3)
        img, rep = _render_session(ctx, srv, "ascii", "f03_t1_01.png", width=800, height=300)
    assert rep.glyphs >= 55, f"Expected >= 55 ASCII glyphs rendered, got {rep.glyphs}"
    assert_not_solid_color(img, min_unique_colors=3, step=2)


def _test_f03_t1_02_ligature_two_char_operators(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    ctx.write_settings(
        {
            "sidebar_behavior": "hidden",
            "pane_header_mode": "hidden",
            "font_family": "JetBrainsMono Nerd Font",
            "font_size_pt": 14.0,
        }
    )
    lig_str = "-> => " * 12
    sep_str = "- > = > " * 12
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _new_session_or_fail(
            srv, "lig2", ["sh", "-c", f"printf '\\033[2J\\033[H{lig_str}'; sleep 6"]
        )
        _new_session_or_fail(
            srv, "sep2", ["sh", "-c", f"printf '\\033[2J\\033[H{sep_str}'; sleep 6"]
        )
        time.sleep(0.3)
        _, rep_lig = _render_session(ctx, srv, "lig2", "f03_t1_02_lig.png", width=800, height=300)
        _, rep_sep = _render_session(ctx, srv, "sep2", "f03_t1_02_sep.png", width=800, height=300)
    assert rep_lig.glyphs <= rep_sep.glyphs - 16, (
        f"Expected ->/=> ligature run shaping to reduce glyph count by >=16: "
        f"lig={rep_lig.glyphs} vs sep={rep_sep.glyphs}"
    )


def _test_f03_t1_03_ligature_triple_char_operators(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    ctx.write_settings(
        {
            "sidebar_behavior": "hidden",
            "pane_header_mode": "hidden",
            "font_family": "JetBrainsMono Nerd Font",
            "font_size_pt": 14.0,
        }
    )
    lig_str = "!== === " * 8
    sep_str = "! = = = = = " * 8
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _new_session_or_fail(
            srv, "lig3", ["sh", "-c", f"printf '\\033[2J\\033[H{lig_str}'; sleep 6"]
        )
        _new_session_or_fail(
            srv, "sep3", ["sh", "-c", f"printf '\\033[2J\\033[H{sep_str}'; sleep 6"]
        )
        time.sleep(0.3)
        _, rep_lig = _render_session(ctx, srv, "lig3", "f03_t1_03_lig.png", width=800, height=300)
        _, rep_sep = _render_session(ctx, srv, "sep3", "f03_t1_03_sep.png", width=800, height=300)
    assert rep_lig.glyphs <= rep_sep.glyphs - 20, (
        f"Expected !==/=== triple ligatures to reduce glyph count by >=20: "
        f"lig={rep_lig.glyphs} vs sep={rep_sep.glyphs}"
    )


def _test_f03_t1_04_ligature_scope_and_pipe_operators(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    ctx.write_settings(
        {
            "sidebar_behavior": "hidden",
            "pane_header_mode": "hidden",
            "font_family": "JetBrainsMono Nerd Font",
            "font_size_pt": 14.0,
        }
    )
    lig_str = ":: |> " * 10
    sep_str = ": : | > " * 10
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _new_session_or_fail(
            srv, "ligp", ["sh", "-c", f"printf '\\033[2J\\033[H{lig_str}'; sleep 6"]
        )
        _new_session_or_fail(
            srv, "sepp", ["sh", "-c", f"printf '\\033[2J\\033[H{sep_str}'; sleep 6"]
        )
        time.sleep(0.3)
        _, rep_lig = _render_session(ctx, srv, "ligp", "f03_t1_04_lig.png", width=800, height=300)
        _, rep_sep = _render_session(ctx, srv, "sepp", "f03_t1_04_sep.png", width=800, height=300)
    assert rep_lig.glyphs <= rep_sep.glyphs - 12, (
        f"Expected ::/|> ligature run shaping to reduce glyph count by >=12: "
        f"lig={rep_lig.glyphs} vs sep={rep_sep.glyphs}"
    )


def _test_f03_t1_05_font_size_clamping(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    imgs: dict[float, PngImage] = {}
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _new_session_or_fail(
            srv,
            "fsize",
            ["sh", "-c", "printf '\\033[2J\\033[Hfn clamp(x: i32) -> i32 { x + 1 }'; sleep 8"],
        )
        time.sleep(0.3)
        for pt in (2.0, 7.0, 11.0, 24.0, 36.0, 99.0):
            ctx.write_settings(
                {"sidebar_behavior": "hidden", "pane_header_mode": "hidden", "font_size_pt": pt}
            )
            imgs[pt], _ = _render_session(
                ctx, srv, "fsize", f"f03_t1_05_{int(pt)}.png", width=640, height=400
            )
    d_low = pixel_diff_stats(imgs[2.0], imgs[7.0])
    d_high = pixel_diff_stats(imgs[36.0], imgs[99.0])
    d_mid = pixel_diff_stats(imgs[11.0], imgs[24.0])
    assert d_low["diff_fraction"] == 0.0, f"Expected font_size_pt 2.0 to clamp to 7.0: {d_low}"
    assert d_high["diff_fraction"] == 0.0, f"Expected font_size_pt 99.0 to clamp to 36.0: {d_high}"
    assert d_mid["diff_fraction"] > 0.01, f"Expected 11pt vs 24pt to differ visibly: {d_mid}"


def _test_f03_t2_01_ligature_vs_non_ligature_font(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    ops = "-> => !== === :: |> -> =>"
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _new_session_or_fail(
            srv,
            "fcomp",
            ["sh", "-c", f"printf '\\033[2J\\033[H\\033[38;2;0;255;0m{ops}\\033[0m'; sleep 6"],
        )
        time.sleep(0.3)
        ctx.write_settings(
            {
                "sidebar_behavior": "hidden",
                "pane_header_mode": "hidden",
                "font_family": "JetBrainsMono Nerd Font",
                "font_size_pt": 18.0,
            }
        )
        img_jb, rep_jb = _render_session(ctx, srv, "fcomp", "f03_t2_01_jb.png", width=800, height=300)
        ctx.write_settings(
            {
                "sidebar_behavior": "hidden",
                "pane_header_mode": "hidden",
                "font_family": "Liberation Mono",
                "font_size_pt": 18.0,
            }
        )
        img_lib, rep_lib = _render_session(ctx, srv, "fcomp", "f03_t2_01_lib.png", width=800, height=300)
    diff = pixel_diff_stats(img_jb, img_lib, rect=(16, 16, 500, 50))
    assert rep_jb.glyphs <= rep_lib.glyphs - 6, (
        f"Expected ligature font glyph count ({rep_jb.glyphs}) <= non-ligature ({rep_lib.glyphs}) - 6"
    )
    assert diff["diff_fraction"] > 0.005, f"Expected visible ligature glyph difference: {diff}"


def _test_f03_t2_02_hidpi_scale_ligatures(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    ctx.write_settings(
        {
            "sidebar_behavior": "hidden",
            "pane_header_mode": "hidden",
            "font_family": "JetBrainsMono Nerd Font",
            "font_size_pt": 14.0,
        }
    )
    lig_str = "-> => !== === :: |> -> =>"
    sep_str = "- > = > ! = = = = = : : | > - > = >"
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _new_session_or_fail(srv, "hlig", ["sh", "-c", f"printf '\\033[2J\\033[H{lig_str}'; sleep 6"])
        _new_session_or_fail(srv, "hsep", ["sh", "-c", f"printf '\\033[2J\\033[H{sep_str}'; sleep 6"])
        time.sleep(0.3)
        img_1x, _ = _render_session(ctx, srv, "hlig", "f03_t2_02_1x.png", width=640, height=320, scale=1.0)
        img_2x, rep_2x = _render_session(
            ctx, srv, "hlig", "f03_t2_02_2x.png", width=1280, height=640, scale=2.0
        )
        _, rep_sep_2x = _render_session(
            ctx, srv, "hsep", "f03_t2_02_sep2x.png", width=1280, height=640, scale=2.0
        )
    assert_not_solid_color(img_1x, min_unique_colors=3, step=2)
    assert_not_solid_color(img_2x, min_unique_colors=3, step=2)
    assert rep_2x.glyphs <= rep_sep_2x.glyphs - 6, (
        f"Expected 2.0x HiDPI run-shaping to preserve ligatures: {rep_2x.glyphs} vs sep {rep_sep_2x.glyphs}"
    )


def _test_f03_t2_03_mixed_ascii_operator_alignment(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    ctx.write_settings(
        {
            "sidebar_behavior": "hidden",
            "pane_header_mode": "hidden",
            "font_family": "JetBrainsMono Nerd Font",
            "font_size_pt": 14.0,
        }
    )
    row1 = "fn f(x: i32) -> i32 { x }  \\033[38;2;0;255;0m# END\\033[0m"
    row2 = "fn g(x: i32) - > i32 { x } \\033[38;2;0;255;0m# END\\033[0m"
    # Baseline separated session with identical char count for glyph count comparison
    row1_sep = "fn f(x: i32) - > i32 { x } \\033[38;2;0;255;0m# END\\033[0m"
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _new_session_or_fail(
            srv, "align", ["sh", "-c", f"printf '\\033[2J\\033[H{row1}\\n{row2}'; sleep 6"]
        )
        _new_session_or_fail(
            srv, "align_sep", ["sh", "-c", f"printf '\\033[2J\\033[H{row1_sep}\\n{row2}'; sleep 6"]
        )
        time.sleep(0.3)
        img, rep_mix = _render_session(ctx, srv, "align", "f03_t2_03.png", width=800, height=300)
        _, rep_sep = _render_session(ctx, srv, "align_sep", "f03_t2_03_sep.png", width=800, height=300)
    r1_min, r1_max, r1_cnt = _green_x_bounds_in_band(img, 18, 43)
    r2_min, r2_max, r2_cnt = _green_x_bounds_in_band(img, 43, 72)
    assert r1_cnt >= 10 and r2_cnt >= 10, (
        f"Expected green # END markers on both rows, counts=({r1_cnt}, {r2_cnt})"
    )
    assert abs(r1_min - r2_min) <= 4 and abs(r1_max - r2_max) <= 4, (
        f"Expected ligature row # END x-span ({r1_min}..{r1_max}) to stay aligned with row 2 ({r2_min}..{r2_max})"
    )
    assert rep_mix.glyphs < rep_sep.glyphs, (
        f"Expected ligature shaping in mixed row: mix={rep_mix.glyphs} vs sep={rep_sep.glyphs}"
    )


def _test_f03_t2_04_viewer_vs_terminal_ligature_parity(ctx: TestContext) -> None:
    _require_rex()
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=False)
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    ctx.write_settings(
        {
            "sidebar_behavior": "hidden",
            "pane_header_mode": "hidden",
            "font_family": "JetBrainsMono Nerd Font",
            "font_size_pt": 13.0,
        }
    )
    ops_lig = "-> => !== === :: |> -> => !== ==="
    ops_sep = "- > = > ! = = = = = : : | > - > = > ! = = = = ="
    lig_rs = ctx.work_dir / "viewer_lig.rs"
    sep_rs = ctx.work_dir / "viewer_sep.rs"
    lig_rs.write_text(
        "\n".join(f'let _op_{i} = "-> => !== === :: |>"; // -> => :: |>' for i in range(12)) + "\n",
        encoding="utf-8",
    )
    sep_rs.write_text(
        "\n".join(f'let _op_{i} = "- > = > ! = = = = = : : | >"; // - > = > : : | >' for i in range(12)) + "\n",
        encoding="utf-8",
    )
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _new_session_or_fail(srv, "vblank", ["sh", "-c", "printf '\\033[2J\\033[H'; sleep 8"])
        _new_session_or_fail(srv, "tonly", ["sh", "-c", f"printf '\\033[2J\\033[H{ops_lig}'; sleep 8"])
        _new_session_or_fail(srv, "sonly", ["sh", "-c", f"printf '\\033[2J\\033[H{ops_sep}'; sleep 8"])
        time.sleep(0.3)
        img_sample, rep_sample = _render_session(
            ctx,
            srv,
            "vblank",
            "f03_t2_04_sample.png",
            width=1200,
            height=700,
            viewer_paths=[f"{bundle.sample_rs}:16"],
        )
        img_vlig, rep_vlig = _render_session(
            ctx,
            srv,
            "vblank",
            "f03_t2_04_vlig.png",
            width=1200,
            height=700,
            viewer_paths=[str(lig_rs)],
        )
        img_vsep, rep_vsep = _render_session(
            ctx,
            srv,
            "vblank",
            "f03_t2_04_vsep.png",
            width=1200,
            height=700,
            viewer_paths=[str(sep_rs)],
        )
        _, rep_t = _render_session(ctx, srv, "tonly", "f03_t2_04_tonly.png", width=800, height=300)
        _, rep_s = _render_session(ctx, srv, "sonly", "f03_t2_04_sonly.png", width=800, height=300)
    assert_not_solid_color(img_sample, min_unique_colors=6, step=2)
    assert rep_sample.glyphs >= 400, f"Expected sample.rs viewer glyphs >= 400, got {rep_sample.glyphs}"
    viewer_diff = pixel_diff_stats(img_vlig, img_vsep, rect=(740, 60, 440, 620))
    assert viewer_diff["diff_fraction"] > 0.002, (
        f"Expected viewer lig_rs vs sep_rs operators to differ visibly: {viewer_diff}"
    )
    assert rep_vlig.glyphs <= rep_vsep.glyphs, (
        f"Expected viewer ligature glyph count ({rep_vlig.glyphs}) <= sep ({rep_vsep.glyphs})"
    )
    assert rep_t.glyphs <= rep_s.glyphs - 6, (
        f"Expected terminal ligatures active alongside viewer parity: {rep_t.glyphs} vs sep {rep_s.glyphs}"
    )


def _test_f03_t2_05_invalid_font_family_fallback(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    ctx.write_settings(
        {
            "sidebar_behavior": "hidden",
            "pane_header_mode": "hidden",
            "font_family": "NonExistentBogusFontXYZ999",
            "font_size_pt": 12.0,
        }
    )
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _new_session_or_fail(
            srv, "bogusfont", ["sh", "-c", "printf '\\033[2J\\033[Hfallback -> ok'; sleep 6"]
        )
        time.sleep(0.3)
        img, rep = _render_session(ctx, srv, "bogusfont", "f03_t2_05.png", width=640, height=400)
    assert rep.glyphs >= 10, f"Expected fallback font glyphs >= 10, got {rep.glyphs}"
    assert_not_solid_color(img, min_unique_colors=3, step=2)


# ---------------------------------------------------------------------------
# F4: Pixel-accurate Powerline (U+E0B0..=U+E0D7) & extra symbols
# ---------------------------------------------------------------------------


def _test_f04_t1_01_powerline_fixture_and_viewer(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=False)
    text = bundle.powerline_txt.read_text(encoding="utf-8")
    for ch in ("\ue0b0", "\ue0b2", "\ue0b4", "\ue0b6", "\ue0b8", "\ue0d6", "\u250c", "\u2518"):
        assert ch in text, f"Missing expected glyph {ch!r} in powerline_sample.txt"
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _new_session_or_fail(srv, "pwl_v", ["sh", "-c", "sleep 6"])
        img, _ = _render_session(
            ctx,
            srv,
            "pwl_v",
            "f04_t1_01.png",
            width=1000,
            height=700,
            viewer_paths=[str(bundle.powerline_txt)],
        )
    assert_not_solid_color(img, min_unique_colors=4, step=2)


def _test_f04_t1_02_powerline_triangles_render(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    ctx.write_settings({"sidebar_behavior": "hidden", "pane_header_mode": "hidden", "font_size_pt": 16.0})
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _new_session_or_fail(srv, "blank", ["sh", "-c", "printf '\\033[2J\\033[H'; sleep 6"])
        _new_session_or_fail(
            srv,
            "tri",
            [
                "sh",
                "-c",
                "printf '\\033[2J\\033[H\\033[38;2;0;255;128m\\ue0b0\\ue0b2\\ue0b0\\ue0b2\\033[0m'; sleep 6",
            ],
        )
        time.sleep(0.3)
        img_blank, _ = _render_session(ctx, srv, "blank", "f04_t1_02_b.png", width=640, height=300)
        img_tri, rep_tri = _render_session(ctx, srv, "tri", "f04_t1_02_t.png", width=640, height=300)
    assert rep_tri.glyphs >= 4, f"Expected >= 4 Powerline triangle glyphs, got {rep_tri.glyphs}"
    diff = pixel_diff_stats(img_blank, img_tri, rect=(18, 18, 160, 60))
    assert diff["changed_pixels"] >= 100, f"Expected Powerline triangle pixels rendered: {diff}"
    # Verify vertical span covers full cell height (>= 18px at 16pt) and horizontal continuity
    # across adjacent \ue0b0\ue0b2\ue0b0\ue0b2 cells with zero dark vertical seam.
    min_y, max_y = img_tri.height, -1
    for y in range(18, 70):
        for x in range(18, 140):
            r, g, b, _ = sample_pixel(img_tri, x, y)
            if g > 150 and g > r + 40:
                min_y = min(min_y, y)
                max_y = max(max_y, y)
    assert max_y - min_y >= 18, f"Expected Powerline full cell-height vertical span >= 18px, got {min_y}..{max_y}"
    mid_y = (min_y + max_y) // 2
    mid_xs = [
        x
        for x in range(18, 140)
        if (px := sample_pixel(img_tri, x, mid_y))[1] > 100 and px[1] > px[0] + 30
    ]
    assert len(mid_xs) >= 40, f"Expected >= 40 continuous green px across 4 Powerline cells, got {len(mid_xs)}"
    gaps = [mid_xs[i + 1] - mid_xs[i] for i in range(len(mid_xs) - 1) if mid_xs[i + 1] - mid_xs[i] > 1]
    assert not gaps, f"Unexpected dark vertical seam(s) between adjacent Powerline cells at y={mid_y}: {gaps}"


def _test_f04_t1_03_powerline_rounded_separators(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    ctx.write_settings({"sidebar_behavior": "hidden", "pane_header_mode": "hidden", "font_size_pt": 16.0})
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _new_session_or_fail(srv, "blank", ["sh", "-c", "printf '\\033[2J\\033[H'; sleep 6"])
        _new_session_or_fail(
            srv,
            "rnd",
            [
                "sh",
                "-c",
                "printf '\\033[2J\\033[H\\033[38;2;255;180;0m\\ue0b4\\ue0b6\\ue0b4\\ue0b6\\033[0m'; sleep 6",
            ],
        )
        time.sleep(0.3)
        img_blank, _ = _render_session(ctx, srv, "blank", "f04_t1_03_b.png", width=640, height=300)
        img_rnd, rep_rnd = _render_session(ctx, srv, "rnd", "f04_t1_03_r.png", width=640, height=300)
    assert rep_rnd.glyphs >= 4, f"Expected >= 4 rounded Powerline glyphs, got {rep_rnd.glyphs}"
    diff = pixel_diff_stats(img_blank, img_rnd, rect=(18, 18, 160, 60))
    assert diff["changed_pixels"] >= 80, f"Expected rounded Powerline pixels: {diff}"
    amber = sum(
        1
        for y in range(18, 70)
        for x in range(18, 140)
        if (px := sample_pixel(img_rnd, x, y))[0] > 180 and px[1] > 120 and px[2] < 80
    )
    assert amber >= 40, f"Expected amber rounded Powerline pixels >= 40, got {amber}"


def _test_f04_t1_04_box_drawing_grid(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    ctx.write_settings({"sidebar_behavior": "hidden", "pane_header_mode": "hidden", "font_size_pt": 14.0})
    box_seq = "\\u250c\\u2500\\u2500\\u252c\\u2500\\u2500\\u2510\\n\\u2502A \\u2502B \\u2502\\n\\u2514\\u2500\\u2500\\u2534\\u2500\\u2500\\u2518"
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _new_session_or_fail(srv, "blank", ["sh", "-c", "printf '\\033[2J\\033[H'; sleep 6"])
        _new_session_or_fail(
            srv,
            "box",
            ["sh", "-c", f"printf '\\033[2J\\033[H\\033[38;2;120;200;255m{box_seq}\\033[0m'; sleep 6"],
        )
        time.sleep(0.3)
        img_blank, _ = _render_session(ctx, srv, "blank", "f04_t1_04_b.png", width=640, height=300)
        img_box, _ = _render_session(ctx, srv, "box", "f04_t1_04_x.png", width=640, height=300)
    diff = pixel_diff_stats(img_blank, img_box, rect=(18, 18, 180, 120))
    assert diff["changed_pixels"] >= 100, f"Expected box-drawing grid pixels >= 100: {diff}"


def _test_f04_t1_05_powerline_prompt_with_sidebar(ctx: TestContext) -> None:
    _require_rex()
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=False)
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    ctx.write_settings({"sidebar_behavior": "always", "pane_header_mode": "always"})
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _new_session_or_fail(
            srv,
            "pwl_cli",
            ["sh", "-c", f"printf '\\033[2J\\033[H'; cat '{bundle.powerline_txt}'; sleep 6"],
        )
        time.sleep(0.3)
        img, rep = _render_session(ctx, srv, "pwl_cli", "f04_t1_05.png", width=1000, height=700)
    assert rep.glyphs >= 30, f"Expected >= 30 glyphs for Powerline prompt output, got {rep.glyphs}"
    assert_not_solid_color(img, min_unique_colors=6, step=2)


def _test_f04_t2_01_extra_powerline_symbols_coverage(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    ctx.write_settings({"sidebar_behavior": "hidden", "pane_header_mode": "hidden", "font_size_pt": 15.0})
    syms = "".join(chr(cp) for cp in (0xE0B8, 0xE0BA, 0xE0BC, 0xE0BE, 0xE0C0, 0xE0C4, 0xE0C6, 0xE0C8, 0xE0CC, 0xE0D0, 0xE0D2, 0xE0D4, 0xE0D6))
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _new_session_or_fail(srv, "blank", ["sh", "-c", "printf '\\033[2J\\033[H'; sleep 6"])
        _new_session_or_fail(
            srv,
            "ext",
            ["sh", "-c", f"printf '\\033[2J\\033[H\\033[38;2;0;255;0m{syms}\\033[0m'; sleep 6"],
        )
        time.sleep(0.3)
        img_b, _ = _render_session(ctx, srv, "blank", "f04_t2_01_b.png", width=640, height=300)
        img_e, rep_e = _render_session(ctx, srv, "ext", "f04_t2_01_e.png", width=640, height=300)
    assert rep_e.glyphs >= 13, f"Expected >= 13 extra Powerline glyphs rendered, got {rep_e.glyphs}"
    diff = pixel_diff_stats(img_b, img_e, rect=(18, 18, 240, 60))
    assert diff["changed_pixels"] >= 150, f"Expected extra Powerline symbols rendered: {diff}"
    # Verify non-blank green columns across the horizontal span (>= 70 distinct x columns)
    green_xs = {
        x
        for x in range(18, 220)
        if any(
            (px := sample_pixel(img_e, x, y))[1] > 150 and px[1] > px[0] + 50
            for y in range(20, 52)
        )
    }
    assert len(green_xs) >= 70, f"Expected continuous Powerline coverage across >= 70 x cols, got {len(green_xs)}"


def _test_f04_t2_02_powerline_hidpi_scaling(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    ctx.write_settings({"sidebar_behavior": "hidden", "pane_header_mode": "hidden", "font_size_pt": 14.0})
    syms = "\ue0b0\ue0b2\ue0b4\ue0b6\ue0b8\ue0ba\ue0bc\ue0be\ue0c0\ue0c4\ue0c8\ue0cc\ue0d0\ue0d6"
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _new_session_or_fail(srv, "p_hi", ["sh", "-c", f"printf '\\033[2J\\033[H{syms}'; sleep 6"])
        time.sleep(0.3)
        img_1x, rep_1x = _render_session(ctx, srv, "p_hi", "f04_t2_02_1x.png", width=640, height=300, scale=1.0)
        img_2x, rep_2x = _render_session(ctx, srv, "p_hi", "f04_t2_02_2x.png", width=1280, height=600, scale=2.0)
    assert rep_1x.glyphs >= 13 and rep_2x.glyphs >= 13, (
        f"Expected >= 13 Powerline glyphs at 1x ({rep_1x.glyphs}) and 2x ({rep_2x.glyphs})"
    )
    assert unique_color_count_sample(img_2x, step=1) >= 4, "Expected crisp multi-tone Powerline pixels at 2.0x"
    assert abs(rep_2x.scale - 2.0) < 1e-3
    assert_not_solid_color(img_1x, min_unique_colors=3, step=2)


def _test_f04_t2_03_powerline_dark_vs_light(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    ctx.write_settings({"sidebar_behavior": "hidden", "pane_header_mode": "hidden"})
    prompt = "\\ue0b6\\ue0b0 main \\ue0b2 100% \\ue0b4"
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _new_session_or_fail(srv, "pl_dl", ["sh", "-c", f"printf '\\033[2J\\033[H{prompt}'; sleep 6"])
        time.sleep(0.3)
        img_d, _ = _render_session(ctx, srv, "pl_dl", "f04_t2_03_d.png", width=800, height=400, light=False)
        img_l, _ = _render_session(ctx, srv, "pl_dl", "f04_t2_03_l.png", width=800, height=400, light=True)
    diff = pixel_diff_stats(img_d, img_l)
    assert diff["diff_fraction"] > 0.25, f"Expected dark vs --light Powerline diff > 0.25: {diff}"
    lum_d = relative_luminance(mean_color_in_rect(img_d, 300, 200, 200, 100)[:3])
    lum_l = relative_luminance(mean_color_in_rect(img_l, 300, 200, 200, 100)[:3])
    assert lum_l > lum_d + 0.3, f"Expected light pane luminance ({lum_l:.3f}) >> dark ({lum_d:.3f})"


def _test_f04_t2_04_narrow_width_no_clipping_crash(ctx: TestContext) -> None:
    _require_rex()
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=False)
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    ctx.write_settings({"sidebar_behavior": "hidden", "pane_header_mode": "hidden"})
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _new_session_or_fail(
            srv, "narrow", ["sh", "-c", f"printf '\\033[2J\\033[H'; cat '{bundle.powerline_txt}'; sleep 6"]
        )
        time.sleep(0.3)
        img, _ = _render_session(ctx, srv, "narrow", "f04_t2_04.png", width=320, height=240)
    assert_not_solid_color(img, min_unique_colors=3, step=2)


def _test_f04_t2_05_powerline_colored_segment_contrast(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    ctx.write_settings({"sidebar_behavior": "hidden", "pane_header_mode": "hidden", "font_size_pt": 15.0})
    seg = "\\033[?25l\\033[48;2;40;80;180m\\033[38;2;255;255;255m main \\033[38;2;40;80;180m\\033[49m\\ue0b0\\033[0m"
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _new_session_or_fail(srv, "seg", ["sh", "-c", f"printf '\\033[2J\\033[H{seg}'; sleep 6"])
        time.sleep(0.3)
        img, _ = _render_session(ctx, srv, "seg", "f04_t2_05.png", width=800, height=300)
    mean = mean_color_in_rect(img, 24, 24, 52, 18)
    assert mean[2] > mean[0] + 40, f"Expected blue-dominant prompt segment bg, got RGBA={mean}"

    def _blue_span(col_x: int) -> int:
        return sum(
            1
            for y in range(15, 55)
            if (px := sample_pixel(img, col_x, y))[2] > 120 and px[0] < 80
        )

    solid_cols = [x for x in range(80, 125) if _blue_span(x) >= 18]
    assert solid_cols, "Expected solid blue segment reaching right edge before \\ue0b0 separator"
    edge_x = solid_cols[-1]
    base_span = _blue_span(edge_x)
    mid_span = _blue_span(edge_x + 4)
    assert 1 <= mid_span < base_span, (
        f"Expected \\ue0b0 separator at x={edge_x + 1}.. to taper from {base_span}px to {mid_span}px"
    )
    assert _blue_span(edge_x + 20) == 0, (
        f"Expected dark background beyond \\ue0b0 tip at x={edge_x + 20}"
    )
    bg_px = sample_pixel(img, edge_x + 20, 34)
    assert bg_px[2] < 80, f"Expected dark bg beyond \\ue0b0 separator, got {bg_px}"


# ---------------------------------------------------------------------------
# F5: Copy Shine (220 ms GPU diagonal shine)
# ---------------------------------------------------------------------------


def _test_f05_t1_01_copy_mode_scenario_renders(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    res, png_path, _ = ctx.headless_render(
        out_name="f05_t1_01.png",
        scenario="copy-mode",
        width=1440,
        height=900,
        rex_mode="isolated_offline",
        rust_log="off",
    )
    assert res.returncode == 0, f"--headless-scenario copy-mode failed: {res.clean_stderr}"
    img = assert_valid_png(png_path, expected_width=1440, expected_height=900, min_bytes=4096)
    assert_not_solid_color(img, min_unique_colors=6, step=2)


def _test_f05_t1_02_copy_mode_settle_vs_idle(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    ctx.write_settings({"sidebar_behavior": "hidden", "pane_header_mode": "hidden"})
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _new_session_or_fail(srv, "csettle", ["sh", "-c", "printf 'copy settle line\\n'; sleep 6"])
        time.sleep(0.25)
        img_idle, _ = _render_session(ctx, srv, "csettle", "f05_t1_02_idle.png", width=800, height=600)
        img_cm, rep_cm = _render_session(
            ctx, srv, "csettle", "f05_t1_02_cm.png", width=800, height=600, scenario="copy-mode"
        )
    assert rep_cm.frame_us < 100000, f"Frame time too slow for copy-mode: {rep_cm.frame_us}us"
    assert rep_cm.draws >= 3 and rep_cm.quads >= 10, f"Unexpected copy-mode telemetry: {rep_cm}"
    assert_not_solid_color(img_cm, min_unique_colors=6, step=2)
    diff = pixel_diff_stats(img_idle, img_cm, rect=(10, 10, 780, 580))
    assert diff["diff_fraction"] > 0.01, f"Expected copy-mode to visibly differ from idle pane: {diff}"


def _test_f05_t1_03_osc52_clipboard_and_copy_mode_shine(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    ctx.write_settings({"sidebar_behavior": "always"})
    payload_b64 = base64.b64encode(b"copy-shine-test").decode("ascii")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        sid, bid = _new_session_or_fail(
            srv,
            "cshine",
            ["sh", "-c", f"printf '\\033]52;c;{payload_b64}\\007copy-shine-ready\\n'; sleep 6"],
        )
        time.sleep(0.35)
        clip = _api_call_or_fail(
            srv, "block.method", {"session_id": sid, "block_id": bid, "method": "clipboard_read"}
        )
        assert (clip or {}).get("data") == "copy-shine-test", f"Unexpected clipboard_read: {clip}"
        res, png_path, rep = ctx.headless_render(
            out_name="f05_t1_03.png",
            scenario="copy-mode",
            width=1000,
            height=650,
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="off",
        )
    assert res.returncode == 0, f"copy-mode scenario with OSC 52 failed: {res.clean_stderr}"
    assert rep is not None and rep.ready_panes >= 1, f"Expected active pane in copy-mode: {rep}"
    img = assert_valid_png(png_path, expected_width=1000, expected_height=650, min_bytes=2048)
    assert_not_solid_color(img, min_unique_colors=6, step=2)


def _test_f05_t1_04_empty_clipboard_and_copy_mode_safe(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        sid, bid = _new_session_or_fail(srv, "cempty", ["sh", "-c", "sleep 6"])
        time.sleep(0.2)
        clip_res = srv.run_cli(
            [
                "api",
                "call",
                "block.method",
                json.dumps({"session_id": sid, "block_id": bid, "method": "clipboard_read"}),
            ]
        )
        assert clip_res.returncode != 0 and (
            "not_found" in clip_res.stderr or "no clipboard write" in clip_res.stderr
        ), f"Expected clean not_found on empty clipboard_read: rc={clip_res.returncode} err={clip_res.stderr}"
        img, rep = _render_session(
            ctx, srv, "cempty", "f05_t1_04.png", width=640, height=480, scenario="copy-mode"
        )
    assert rep.ready_panes >= 1
    assert_not_solid_color(img, min_unique_colors=5, step=2)


def _test_f05_t1_05_osc52_utf8_roundtrip_and_copy_mode(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    utf8_text = "shine-utf8-✓-αβ"
    payload_b64 = base64.b64encode(utf8_text.encode("utf-8")).decode("ascii")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        sid, bid = _new_session_or_fail(
            srv,
            "cutf8",
            ["sh", "-c", f"printf '\\033]52;c;{payload_b64}\\007utf8-ready\\n'; sleep 6"],
        )
        time.sleep(0.35)
        clip = _api_call_or_fail(
            srv, "block.method", {"session_id": sid, "block_id": bid, "method": "clipboard_read"}
        )
        assert (clip or {}).get("data") == utf8_text, f"Expected UTF-8 clipboard roundtrip, got {clip}"
        img, rep = _render_session(
            ctx, srv, "cutf8", "f05_t1_05.png", width=800, height=600, scenario="copy-mode"
        )
    assert rep.ready_panes >= 1
    assert_not_solid_color(img, min_unique_colors=6, step=2)


def _test_f05_t2_01_copy_mode_differs_from_idle(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    res_idle, p_idle, _ = ctx.headless_render(
        out_name="f05_t2_01_idle.png", width=1440, height=900, rex_mode="isolated_offline", rust_log="off"
    )
    assert res_idle.returncode == 0
    res_cm, p_cm, _ = ctx.headless_render(
        out_name="f05_t2_01_cm.png",
        scenario="copy-mode",
        width=1440,
        height=900,
        rex_mode="isolated_offline",
        rust_log="off",
    )
    assert res_cm.returncode == 0, f"copy-mode scenario failed: {res_cm.clean_stderr}"
    diff = pixel_diff_stats(assert_valid_png(p_idle, 1440, 900), assert_valid_png(p_cm, 1440, 900))
    assert diff["diff_fraction"] > 0.01, f"Expected copy-mode to visibly differ from idle: {diff}"


def _test_f05_t2_02_copy_mode_dark_vs_light(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    res_d, p_d, _ = ctx.headless_render(
        out_name="f05_t2_02_d.png",
        scenario="copy-mode",
        width=1440,
        height=900,
        rex_mode="isolated_offline",
        rust_log="off",
    )
    assert res_d.returncode == 0, f"copy-mode dark failed: {res_d.clean_stderr}"
    res_l, p_l, _ = ctx.headless_render(
        out_name="f05_t2_02_l.png",
        scenario="copy-mode",
        width=1440,
        height=900,
        light=True,
        rex_mode="isolated_offline",
        rust_log="off",
    )
    assert res_l.returncode == 0, f"copy-mode light failed: {res_l.clean_stderr}"
    img_d = assert_valid_png(p_d, 1440, 900)
    img_l = assert_valid_png(p_l, 1440, 900)
    assert unique_color_count_sample(img_d, step=2) >= 8
    diff = pixel_diff_stats(img_d, img_l)
    assert diff["diff_fraction"] > 0.25, f"Expected dark vs --light copy-mode diff > 0.25: {diff}"


def _test_f05_t2_03_copy_mode_vs_idle_and_light_palette(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    res_idle, p_idle, _ = ctx.headless_render(
        out_name="f05_t2_03_idle.png",
        width=1440,
        height=900,
        rex_mode="isolated_offline",
        rust_log="off",
    )
    assert res_idle.returncode == 0
    res_cm_d, p_cm_d, _ = ctx.headless_render(
        out_name="f05_t2_03_dark.png",
        scenario="copy-mode",
        width=1440,
        height=900,
        rex_mode="isolated_offline",
        rust_log="off",
    )
    assert res_cm_d.returncode == 0, f"copy-mode dark failed: {res_cm_d.clean_stderr}"
    res_cm_l, p_cm_l, _ = ctx.headless_render(
        out_name="f05_t2_03_light.png",
        scenario="copy-mode",
        width=1440,
        height=900,
        light=True,
        rex_mode="isolated_offline",
        rust_log="off",
    )
    assert res_cm_l.returncode == 0, f"copy-mode light failed: {res_cm_l.clean_stderr}"
    img_idle = assert_valid_png(p_idle, 1440, 900, min_bytes=4096)
    img_dark = assert_valid_png(p_cm_d, 1440, 900, min_bytes=4096)
    img_light = assert_valid_png(p_cm_l, 1440, 900, min_bytes=4096)
    assert_not_solid_color(img_dark, min_unique_colors=6, step=2)
    diff_idle = pixel_diff_stats(img_idle, img_dark, rect=(10, 10, 1420, 880))
    assert diff_idle["diff_fraction"] > 0.01, f"Expected copy-mode to differ from idle pane: {diff_idle}"
    diff_theme = pixel_diff_stats(img_dark, img_light)
    assert diff_theme["diff_fraction"] > 0.25, f"Expected copy-mode dark vs --light diff > 0.25: {diff_theme}"


def _test_f05_t2_04_copy_shine_clipped_inside_pane(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    ctx.write_settings({"sidebar_behavior": "always", "sidebar_width_px": 248.0})
    res, png_path, _ = ctx.headless_render(
        out_name="f05_t2_04.png",
        scenario="copy-mode",
        width=1440,
        height=900,
        rex_mode="isolated_offline",
        rust_log="off",
    )
    assert res.returncode == 0, f"copy-mode with sidebar failed: {res.clean_stderr}"
    img = assert_valid_png(png_path, 1440, 900)
    sidebar_mean = mean_color_in_rect(img, 4, 500, 100, 100)
    assert_color_near(sidebar_mean, (19, 8, 34, 255), tolerance=12, label="sidebar unbleed")


def _test_f05_t2_05_copy_mode_multiscale_natural_anim(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    ctx.write_settings({"sidebar_behavior": "always"})
    for tag, w, h, sc in (("1x", 800, 600, 1.0), ("2x", 1600, 1200, 2.0)):
        res, png_path, rep = ctx.headless_render(
            out_name=f"f05_t2_05_{tag}.png",
            scenario="copy-mode",
            width=w,
            height=h,
            scale=sc,
            rex_mode="isolated_offline",
            rust_log="off",
        )
        assert res.returncode == 0, f"copy-mode {tag} failed: {res.clean_stderr}"
        assert rep is not None and rep.draws >= 3, f"Unexpected report for {tag}: {rep}"
        assert abs(rep.scale - sc) < 1e-3, f"Scale mismatch for {tag}: {rep}"
        img = assert_valid_png(png_path, w, h, min_bytes=2048)
        assert_not_solid_color(img, min_unique_colors=6, step=2)


# ---------------------------------------------------------------------------
# F6: Spring border & slide/fade transitions
# ---------------------------------------------------------------------------


def _test_f06_t1_01_highlight_active_border_toggle(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _new_session_or_fail(srv, "bdr", ["sh", "-c", "sleep 6"])
        ctx.write_settings({"sidebar_behavior": "always", "highlight_active_border": True})
        img_on, _ = _render_session(ctx, srv, "bdr", "f06_t1_01_on.png", width=800, height=600)
        ctx.write_settings({"sidebar_behavior": "always", "highlight_active_border": False})
        img_off, _ = _render_session(ctx, srv, "bdr", "f06_t1_01_off.png", width=800, height=600)
    px_on = sample_pixel(img_on, 250, 300)
    px_off = sample_pixel(img_off, 250, 300)
    assert_color_near(px_on, (128, 168, 233, 255), tolerance=12, label="active border on")
    max_diff = max(abs(px_on[i] - px_off[i]) for i in range(3))
    assert max_diff > 30, f"Expected highlight_active_border off ({px_off}) to differ from on ({px_on})"


def _test_f06_t1_02_sidebar_always_vs_hidden(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _new_session_or_fail(
            srv, "sbar", ["sh", "-c", "printf '\\033[48;2;20;140;60m\\033[2J'; sleep 6"]
        )
        time.sleep(0.25)
        ctx.write_settings({"sidebar_behavior": "always", "sidebar_width_px": 248.0})
        img_a, _ = _render_session(ctx, srv, "sbar", "f06_t1_02_a.png", width=800, height=600)
        ctx.write_settings({"sidebar_behavior": "hidden"})
        img_h, _ = _render_session(ctx, srv, "sbar", "f06_t1_02_h.png", width=800, height=600)
    diff = pixel_diff_stats(img_a, img_h, rect=(0, 0, 248, 600))
    assert diff["diff_fraction"] > 0.05, f"Expected sidebar always vs hidden diff > 0.05: {diff}"


def _test_f06_t1_03_fade_inactive_panes_toggle(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        sid, _ = _new_session_or_fail(
            srv,
            "fade",
            ["sh", "-c", "printf '\\033[2J\\033[H\\033[38;2;255;255;255mLEFT_PANE'; sleep 6"],
        )
        sp = _api_call_or_fail(
            srv,
            "session.new_split",
            {
                "session_id": sid,
                "direction": "horizontal",
                "side": "after",
                "layout": {"block": {"flavor": "com.superlogical.terminal.shell"}},
            },
        )
        bid2 = sp["block_ids"][0]
        time.sleep(0.25)
        _api_call_or_fail(
            srv,
            "block.method",
            {
                "session_id": sid,
                "block_id": bid2,
                "method": "write",
                "args": {"data": "printf '\\033[2J\\033[H\\033[38;2;255;255;255mRIGHT_PANE'\n"},
            },
        )
        time.sleep(0.25)
        ctx.write_settings(
            {"sidebar_behavior": "always", "pane_header_mode": "always", "fade_inactive_panes": True}
        )
        img_on, _ = _render_session(ctx, srv, "fade", "f06_t1_03_on.png", width=1000, height=600)
        ctx.write_settings(
            {"sidebar_behavior": "always", "pane_header_mode": "always", "fade_inactive_panes": False}
        )
        img_off, _ = _render_session(ctx, srv, "fade", "f06_t1_03_off.png", width=1000, height=600)
    diff = pixel_diff_stats(img_on, img_off, rect=(250, 10, 740, 580))
    assert diff["changed_pixels"] >= 50 and diff["max_diff"] >= 10, (
        f"Expected fade_inactive_panes toggle to affect inactive pane rendering: {diff}"
    )


def _test_f06_t1_04_sidebar_width_clamping(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    imgs: dict[float, PngImage] = {}
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _new_session_or_fail(srv, "sbw", ["sh", "-c", "sleep 8"])
        for w_px in (50.0, 180.0, 380.0, 500.0):
            ctx.write_settings({"sidebar_behavior": "always", "sidebar_width_px": w_px})
            imgs[w_px], _ = _render_session(
                ctx, srv, "sbw", f"f06_t1_04_{int(w_px)}.png", width=1440, height=900
            )
    assert pixel_diff_stats(imgs[50.0], imgs[180.0])["diff_fraction"] == 0.0, "Expected 50px to clamp to 180px"
    assert pixel_diff_stats(imgs[380.0], imgs[500.0])["diff_fraction"] == 0.0, "Expected 500px to clamp to 380px"
    diff_mid = pixel_diff_stats(imgs[180.0], imgs[380.0])
    assert diff_mid["diff_fraction"] > 0.01, f"Expected 180px vs 380px sidebar width diff: {diff_mid}"


def _test_f06_t1_05_adaptive_rounding_config(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        sid, _ = _new_session_or_fail(
            srv,
            "rnd_cfg",
            ["sh", "-c", "printf '\\033[2J\\033[H\\033[38;2;255;255;255mLEFT_PANE'; sleep 6"],
        )
        sp = _api_call_or_fail(
            srv,
            "session.new_split",
            {
                "session_id": sid,
                "direction": "horizontal",
                "side": "after",
                "layout": {"block": {"flavor": "com.superlogical.terminal.shell"}},
            },
        )
        bid2 = sp["block_ids"][0]
        time.sleep(0.25)
        _api_call_or_fail(
            srv,
            "block.method",
            {
                "session_id": sid,
                "block_id": bid2,
                "method": "write",
                "args": {"data": "printf '\\033[2J\\033[H\\033[38;2;255;255;255mRIGHT_PANE'\n"},
            },
        )
        time.sleep(0.25)
        ctx.write_settings(
            {"sidebar_behavior": "hidden", "pane_header_mode": "hidden", "adaptive_rounding": True}
        )
        img_on, rep_on = _render_session(ctx, srv, "rnd_cfg", "f06_t1_05_on.png", width=800, height=600)
        ctx.write_settings(
            {"sidebar_behavior": "hidden", "pane_header_mode": "hidden", "adaptive_rounding": False}
        )
        img_off, rep_off = _render_session(ctx, srv, "rnd_cfg", "f06_t1_05_off.png", width=800, height=600)
    assert rep_on.ready_panes >= 2 and rep_off.ready_panes >= 2, (
        f"Expected 2 ready panes in split layout: on={rep_on}, off={rep_off}"
    )
    assert_not_solid_color(img_on, min_unique_colors=4, step=2)
    assert_not_solid_color(img_off, min_unique_colors=4, step=2)
    diff = pixel_diff_stats(img_on, img_off)
    assert diff["diff_fraction"] < 0.05, (
        f"Expected adaptive_rounding toggle on 2-pane split to preserve layout consistency: {diff}"
    )


def _test_f06_t2_01_split_panes_scenario_border(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    ctx.write_settings({"highlight_active_border": True})
    res, png_path, _ = ctx.headless_render(
        out_name="f06_t2_01.png",
        scenario="split-panes",
        width=1440,
        height=900,
        rex_mode="isolated_offline",
        rust_log="off",
    )
    assert res.returncode == 0, f"--headless-scenario split-panes failed: {res.clean_stderr}"
    img = assert_valid_png(png_path, expected_width=1440, expected_height=900, min_bytes=4096)
    assert_not_solid_color(img, min_unique_colors=6, step=2)


def _test_f06_t2_02_split_panes_active_border_diff(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        sid, _ = _new_session_or_fail(srv, "sp_bdr", ["sh", "-c", "sleep 6"])
        _api_call_or_fail(
            srv,
            "session.new_split",
            {
                "session_id": sid,
                "direction": "horizontal",
                "side": "after",
                "layout": {"block": {"flavor": "com.superlogical.terminal.shell"}},
            },
        )
        time.sleep(0.25)
        ctx.write_settings({"sidebar_behavior": "hidden", "highlight_active_border": True})
        img_on, _ = _render_session(ctx, srv, "sp_bdr", "f06_t2_02_on.png", width=1000, height=600)
        ctx.write_settings({"sidebar_behavior": "hidden", "highlight_active_border": False})
        img_off, _ = _render_session(ctx, srv, "sp_bdr", "f06_t2_02_off.png", width=1000, height=600)
    diff = pixel_diff_stats(img_on, img_off)
    assert diff["diff_fraction"] > 0.001 and diff["max_diff"] > 20, (
        f"Expected split active border diff: {diff}"
    )


def _test_f06_t2_03_split_panes_animation_settle(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    imgs: list[PngImage] = []
    for idx in (1, 2):
        ctx.write_settings({"sidebar_behavior": "always", "highlight_active_border": True})
        res, png_path, rep = ctx.headless_render(
            out_name=f"f06_t2_03_{idx}.png",
            scenario="split-panes",
            width=1440,
            height=900,
            rex_mode="isolated_offline",
            rust_log="off",
        )
        assert res.returncode == 0, f"split-panes run {idx} failed: {res.clean_stderr}"
        assert rep is not None and rep.frame_us < 100000, f"Expected fast settled split-panes frame: {rep}"
        imgs.append(assert_valid_png(png_path, expected_width=1440, expected_height=900))
    diff = pixel_diff_stats(imgs[0], imgs[1])
    assert diff["diff_fraction"] < 0.05, f"Expected settled split-panes states to match closely: {diff}"


def _test_f06_t2_04_split_panes_dark_vs_light(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    res_d, p_d, _ = ctx.headless_render(
        out_name="f06_t2_04_d.png",
        scenario="split-panes",
        width=1440,
        height=900,
        rex_mode="isolated_offline",
        rust_log="off",
    )
    assert res_d.returncode == 0, f"split-panes dark failed: {res_d.clean_stderr}"
    res_l, p_l, _ = ctx.headless_render(
        out_name="f06_t2_04_l.png",
        scenario="split-panes",
        width=1440,
        height=900,
        light=True,
        rex_mode="isolated_offline",
        rust_log="off",
    )
    assert res_l.returncode == 0, f"split-panes light failed: {res_l.clean_stderr}"
    img_d = assert_valid_png(p_d, 1440, 900)
    img_l = assert_valid_png(p_l, 1440, 900)
    diff = pixel_diff_stats(img_d, img_l)
    assert diff["diff_fraction"] > 0.25, f"Expected dark vs --light split-panes diff > 0.25: {diff}"


def _test_f06_t2_05_viewer_transition_settle_telemetry(ctx: TestContext) -> None:
    _require_rex()
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=False)
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    ctx.write_settings({"sidebar_behavior": "always", "highlight_active_border": True})
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _new_session_or_fail(srv, "vsettle", ["sh", "-c", "sleep 6"])
        _, rep = _render_session(
            ctx,
            srv,
            "vsettle",
            "f06_t2_05.png",
            width=1440,
            height=900,
            viewer_paths=[str(bundle.sample_rs)],
        )
    assert rep.frame_us < 100000 and rep.draws >= 3, f"Unexpected frame telemetry: {rep}"


# ---------------------------------------------------------------------------
# F7: Inertia smooth scroll + scrollback resilience
# ---------------------------------------------------------------------------


def _test_f07_t1_01_large_scrollback_generation(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        sid, bid = _new_session_or_fail(
            srv, "sb250", ["sh", "-c", "seq 1 250; echo SB250_END; sleep 6"]
        )
        txt = _wait_format_contains(srv, sid, bid, "SB250_END")
        assert "1\n" in txt and "250\n" in txt, "Expected lines 1..250 in Rex scrollback"
        img, _ = _render_session(ctx, srv, "sb250", "f07_t1_01.png", width=800, height=600)
    assert_not_solid_color(img, min_unique_colors=4, step=2)


def _test_f07_t1_02_scrollback_tail_rendering(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        sid, bid = _new_session_or_fail(
            srv, "sb300", ["sh", "-c", "seq 1 300; echo TAIL_300_DONE; sleep 6"]
        )
        txt = _wait_format_contains(srv, sid, bid, "TAIL_300_DONE")
        assert "300\n" in txt and "1\n" in txt
        img, rep = _render_session(ctx, srv, "sb300", "f07_t1_02.png", width=800, height=600)
    assert rep.glyphs >= 30, f"Expected >= 30 tail glyphs rendered, got {rep.glyphs}"
    assert_not_solid_color(img, min_unique_colors=4, step=2)


def _test_f07_t1_03_scrollback_font_reflow_no_warnings(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    imgs: dict[float, PngImage] = {}
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        sid, bid = _new_session_or_fail(
            srv, "sb_reflow", ["sh", "-c", "seq 1 250; echo REFLOW_READY; sleep 6"]
        )
        _wait_format_contains(srv, sid, bid, "REFLOW_READY")
        for pt in (10.0, 18.0):
            ctx.write_settings(
                {"sidebar_behavior": "hidden", "pane_header_mode": "hidden", "font_size_pt": pt}
            )
            res, png_path, rep = ctx.headless_render(
                out_name=f"f07_t1_03_{int(pt)}.png",
                extra_args=["sb_reflow"],
                width=800,
                height=600,
                rex_mode="custom_socket",
                custom_socket=srv.endpoint,
                rust_log="warn",
            )
            assert res.returncode == 0 and not res.has_error and not res.has_warning, (
                f"Scrollback render at {pt}pt produced unexpected warning/error: {res.clean_stdout}\n{res.clean_stderr}"
            )
            assert rep is not None and rep.ready_panes >= 1
            imgs[pt] = assert_valid_png(png_path, 800, 600, min_bytes=2048)
    diff = pixel_diff_stats(imgs[10.0], imgs[18.0], rect=(20, 20, 760, 560))
    assert diff["diff_fraction"] > 0.01, f"Expected scrollback viewport reflow diff 10pt vs 18pt: {diff}"


def _test_f07_t1_04_wheel_event_accepted(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        sid, bid = _new_session_or_fail(
            srv, "wheel", ["sh", "-c", "seq 1 120; echo WHEEL_OK; sleep 6"]
        )
        _wait_format_contains(srv, sid, bid, "WHEEL_OK")
        ev_res = _api_call_or_fail(
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
        assert isinstance(ev_res, dict), f"Expected dict response from wheel block.event, got {ev_res}"
        img, _ = _render_session(ctx, srv, "wheel", "f07_t1_04.png", width=800, height=600)
    assert_not_solid_color(img, min_unique_colors=4)


def _test_f07_t1_05_rapid_2000_line_burst(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        sid, bid = _new_session_or_fail(
            srv, "burst2k", ["sh", "-c", "seq 1 2000; echo BURST_DONE; sleep 6"]
        )
        txt = _wait_format_contains(srv, sid, bid, "BURST_DONE", timeout_s=5.0)
        assert "2000\n" in txt, "Expected 2000 in burst output"
        img, _ = _render_session(ctx, srv, "burst2k", "f07_t1_05.png", width=800, height=600)
    assert_not_solid_color(img, min_unique_colors=4, step=2)


def _test_f07_t2_01_tail_updates_on_new_output(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    ctx.write_settings({"sidebar_behavior": "hidden", "pane_header_mode": "hidden"})
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        sid, bid = _new_session_or_fail(srv, "tailupd", ["sh"])
        _api_call_or_fail(
            srv,
            "block.method",
            {
                "session_id": sid,
                "block_id": bid,
                "method": "write",
                "args": {"data": "seq 1 120; echo PHASE1_READY\n"},
            },
        )
        _wait_format_contains(srv, sid, bid, "PHASE1_READY")
        img_a, _ = _render_session(ctx, srv, "tailupd", "f07_t2_01_a.png", width=800, height=600)
        _api_call_or_fail(
            srv,
            "block.method",
            {
                "session_id": sid,
                "block_id": bid,
                "method": "write",
                "args": {"data": "seq 121 240; echo PHASE2_MARKER\n"},
            },
        )
        txt = _wait_format_contains(srv, sid, bid, "PHASE2_MARKER")
        assert "1\n" in txt and "240\n" in txt
        img_b, _ = _render_session(ctx, srv, "tailupd", "f07_t2_01_b.png", width=800, height=600)
    diff = pixel_diff_stats(img_a, img_b, rect=(20, 20, 760, 560))
    assert diff["diff_fraction"] > 0.003 and diff["changed_pixels"] >= 1000, (
        f"Expected rendered viewport tail to update: {diff}"
    )


def _test_f07_t2_02_scrollback_burst_settle_telemetry(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    ctx.write_settings({"sidebar_behavior": "hidden", "pane_header_mode": "hidden"})
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        sid, bid = _new_session_or_fail(srv, "burst_sc", ["sh"])
        _api_call_or_fail(
            srv,
            "block.method",
            {
                "session_id": sid,
                "block_id": bid,
                "method": "write",
                "args": {"data": "seq 1 180; echo BURST_1\n"},
            },
        )
        _wait_format_contains(srv, sid, bid, "BURST_1")
        img_a, rep_a = _render_session(ctx, srv, "burst_sc", "f07_t2_02_a.png", width=800, height=600)
        _api_call_or_fail(
            srv,
            "block.method",
            {
                "session_id": sid,
                "block_id": bid,
                "method": "write",
                "args": {"data": "seq 181 360; echo BURST_2\n"},
            },
        )
        _wait_format_contains(srv, sid, bid, "BURST_2")
        img_b, rep_b = _render_session(ctx, srv, "burst_sc", "f07_t2_02_b.png", width=800, height=600)
    assert rep_a.frame_us < 100000 and rep_b.frame_us < 100000, (
        f"Unexpected high frame_us after bursts: a={rep_a.frame_us}, b={rep_b.frame_us}"
    )
    assert_not_solid_color(img_b, min_unique_colors=4)
    diff = pixel_diff_stats(img_a, img_b, rect=(20, 20, 760, 560))
    assert diff["diff_fraction"] > 0.003, f"Expected second scrollback burst to update viewport: {diff}"


def _test_f07_t2_03_rex_terminal_actions_contract(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        act_res = srv.run_cli(["actions", "--json"])
        assert act_res.returncode == 0, f"rex actions failed: {act_res.stderr}"
        names = {a.get("name") for a in json.loads(act_res.stdout).get("actions", [])}
        for req_action in (
            "com.superlogical.terminal.format",
            "com.superlogical.terminal.clear",
            "com.superlogical.terminal.resize",
        ):
            assert req_action in names, f"Missing expected Rex terminal action {req_action!r}"
        sid, bid = _new_session_or_fail(srv, "act_s", ["sh", "-c", "seq 1 150; echo OK; sleep 6"])
        _wait_format_contains(srv, sid, bid, "OK")
        img, _ = _render_session(ctx, srv, "act_s", "f07_t2_03.png", width=800, height=600)
    assert_not_solid_color(img, min_unique_colors=4)


def _test_f07_t2_04_continuous_500_line_flood_no_corruption(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _new_session_or_fail(
            srv,
            "flood500",
            ["sh", "-c", "i=0; while [ $i -lt 500 ]; do echo LINE_$i; i=$((i+1)); done; sleep 6"],
        )
        img, _ = _render_session(ctx, srv, "flood500", "f07_t2_04.png", width=800, height=600)
    assert_not_solid_color(img, min_unique_colors=3)


def _test_f07_t2_05_scrollback_preserved_across_resize(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        sid, bid = _new_session_or_fail(
            srv, "rsz", ["sh", "-c", "seq 1 220; echo RSZ_READY; sleep 8"]
        )
        _wait_format_contains(srv, sid, bid, "RSZ_READY")
        _api_call_or_fail(
            srv,
            "block.method",
            {"session_id": sid, "block_id": bid, "method": "resize", "args": {"columns": 60, "rows": 20}},
        )
        _api_call_or_fail(
            srv,
            "block.method",
            {"session_id": sid, "block_id": bid, "method": "resize", "args": {"columns": 100, "rows": 30}},
        )
        fmt = _api_call_or_fail(
            srv,
            "block.method",
            {"session_id": sid, "block_id": bid, "method": "format", "args": {"format": "text"}},
        )
        content = str((fmt or {}).get("content", ""))
        assert "1\n" in content and "220\n" in content, "Scrollback lost after resize"
        img, _ = _render_session(ctx, srv, "rsz", "f07_t2_05.png", width=800, height=600)
    assert_not_solid_color(img, min_unique_colors=4)


# ---------------------------------------------------------------------------
# F8: Program-color padding fill (program_bg)
# ---------------------------------------------------------------------------


def _test_f08_t1_01_default_padding_baseline(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    ctx.write_settings({"sidebar_behavior": "hidden", "pane_header_mode": "hidden"})
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _new_session_or_fail(srv, "idle8", ["sh", "-c", "printf '\\033[2J\\033[H'; sleep 6"])
        time.sleep(0.25)
        img, _ = _render_session(ctx, srv, "idle8", "f08_t1_01.png", width=800, height=600)
    assert_color_near(sample_pixel(img, 2, 2), (19, 8, 34, 255), tolerance=10, label="outer frame")
    assert_color_near(sample_pixel(img, 15, 15), (26, 11, 46, 255), tolerance=10, label="pane padding")
    assert_color_near(sample_pixel(img, 200, 200), (26, 11, 46, 255), tolerance=10, label="terminal grid")


def _test_f08_t1_02_fullscreen_tui_extends_into_padding(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    ctx.write_settings({"sidebar_behavior": "hidden", "pane_header_mode": "hidden"})
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _new_session_or_fail(
            srv,
            "tui_bg",
            ["sh", "-c", "printf '\\033[48;2;20;140;60m\\033[2J\\033[HFULLSCREEN_TUI'; sleep 6"],
        )
        time.sleep(0.3)
        img, _ = _render_session(ctx, srv, "tui_bg", "f08_t1_02.png", width=800, height=600)
    assert_color_near(sample_pixel(img, 200, 200), (20, 140, 60, 255), tolerance=10, label="grid center")
    assert_color_near(sample_pixel(img, 15, 15), (20, 140, 60, 255), tolerance=12, label="program_bg padding")


def _test_f08_t1_03_alt_screen_enter_exit_modes(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        sid, bid = _new_session_or_fail(srv, "alt", ["sh"])
        _api_call_or_fail(
            srv,
            "block.method",
            {
                "session_id": sid,
                "block_id": bid,
                "method": "write",
                "args": {"data": "printf '\\033[?1049h\\033[48;2;180;50;50m\\033[2JALT_ON'\n"},
            },
        )
        _wait_format_contains(srv, sid, bid, "ALT_ON")
        m_alt = _api_call_or_fail(
            srv, "block.method", {"session_id": sid, "block_id": bid, "method": "modes"}
        )
        assert (m_alt or {}).get("screen") == "alternate", f"Expected alternate screen, got {m_alt}"
        _api_call_or_fail(
            srv,
            "block.method",
            {
                "session_id": sid,
                "block_id": bid,
                "method": "write",
                "args": {"data": "printf '\\033[?1049lALT_OFF\\n'\n"},
            },
        )
        _wait_format_contains(srv, sid, bid, "ALT_OFF")
        m_pri = _api_call_or_fail(
            srv, "block.method", {"session_id": sid, "block_id": bid, "method": "modes"}
        )
        assert (m_pri or {}).get("screen") == "primary", f"Expected primary screen after exit, got {m_pri}"
        img, _ = _render_session(ctx, srv, "alt", "f08_t1_03.png", width=800, height=600)
    assert_not_solid_color(img, min_unique_colors=3)


def _test_f08_t1_04_pane_header_mode_variants(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    imgs: dict[str, PngImage] = {}
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _new_session_or_fail(srv, "phm", ["sh", "-c", "sleep 6"])
        for mode in ("always", "on_hover", "hidden"):
            ctx.write_settings({"sidebar_behavior": "hidden", "pane_header_mode": mode})
            imgs[mode], _ = _render_session(
                ctx, srv, "phm", f"f08_t1_04_{mode}.png", width=800, height=600
            )
    diff = pixel_diff_stats(imgs["always"], imgs["hidden"], rect=(20, 12, 760, 34))
    assert diff["diff_fraction"] > 0.02, f"Expected pane_header_mode always vs hidden diff > 0.02: {diff}"


def _test_f08_t1_05_dark_vs_light_default_padding(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    ctx.write_settings({"sidebar_behavior": "hidden", "pane_header_mode": "hidden"})
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _new_session_or_fail(srv, "pad_dl", ["sh", "-c", "printf '\\033[2J\\033[H'; sleep 6"])
        time.sleep(0.25)
        img_d, _ = _render_session(ctx, srv, "pad_dl", "f08_t1_05_d.png", width=800, height=600, light=False)
        img_l, _ = _render_session(ctx, srv, "pad_dl", "f08_t1_05_l.png", width=800, height=600, light=True)
    assert_color_near(sample_pixel(img_d, 15, 15), (26, 11, 46, 255), tolerance=10, label="dark padding")
    assert_color_near(sample_pixel(img_l, 15, 15), (239, 241, 245, 255), tolerance=12, label="light padding")


def _test_f08_t2_01_program_bg_all_four_margins(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    ctx.write_settings({"sidebar_behavior": "hidden", "pane_header_mode": "hidden"})
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _new_session_or_fail(
            srv, "margins", ["sh", "-c", "printf '\\033[48;2;20;140;60m\\033[2J'; sleep 6"]
        )
        time.sleep(0.3)
        img, _ = _render_session(ctx, srv, "margins", "f08_t2_01.png", width=800, height=600)
    for name, pt in (
        ("top", (400, 15)),
        ("bottom", (400, 584)),
        ("left", (15, 300)),
        ("right", (784, 300)),
    ):
        assert_color_near(sample_pixel(img, *pt), (20, 140, 60, 255), tolerance=12, label=f"{name} margin")


def _test_f08_t2_02_alt_screen_exit_restores_padding(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    ctx.write_settings({"sidebar_behavior": "hidden", "pane_header_mode": "hidden"})
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        sid, bid = _new_session_or_fail(srv, "alt_rst", ["sh"])
        _api_call_or_fail(
            srv,
            "block.method",
            {
                "session_id": sid,
                "block_id": bid,
                "method": "write",
                "args": {"data": "printf '\\033[?1049h\\033[48;2;20;140;60m\\033[2J'\n"},
            },
        )
        time.sleep(0.3)
        img_a, _ = _render_session(ctx, srv, "alt_rst", "f08_t2_02_a.png", width=800, height=600)
        _api_call_or_fail(
            srv,
            "block.method",
            {
                "session_id": sid,
                "block_id": bid,
                "method": "write",
                "args": {"data": "printf '\\033[?1049l\\033[0m\\033[2JRESTORED\\n'\n"},
            },
        )
        _wait_format_contains(srv, sid, bid, "RESTORED")
        img_b, _ = _render_session(ctx, srv, "alt_rst", "f08_t2_02_b.png", width=800, height=600)
    assert_color_near(sample_pixel(img_a, 15, 15), (20, 140, 60, 255), tolerance=12, label="alt TUI padding")
    assert_color_near(sample_pixel(img_b, 15, 15), (26, 11, 46, 255), tolerance=12, label="restored dark padding")


def _test_f08_t2_03_split_panes_isolated_program_bg(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    ctx.write_settings({"sidebar_behavior": "hidden", "pane_header_mode": "hidden"})
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        sid, _ = _new_session_or_fail(
            srv, "spl_bg", ["sh", "-c", "printf '\\033[48;2;20;140;60m\\033[2J'; sleep 8"]
        )
        sp = _api_call_or_fail(
            srv,
            "session.new_split",
            {
                "session_id": sid,
                "direction": "horizontal",
                "side": "after",
                "layout": {"block": {"flavor": "com.superlogical.terminal.shell"}},
            },
        )
        bid2 = sp["block_ids"][0]
        time.sleep(0.25)
        _api_call_or_fail(
            srv,
            "block.method",
            {
                "session_id": sid,
                "block_id": bid2,
                "method": "write",
                "args": {"data": "printf '\\033[48;2;30;80;200m\\033[2J'\n"},
            },
        )
        time.sleep(0.3)
        img, _ = _render_session(ctx, srv, "spl_bg", "f08_t2_03.png", width=1000, height=600)
    assert_color_near(sample_pixel(img, 15, 300), (20, 140, 60, 255), tolerance=12, label="left pane padding")
    assert_color_near(sample_pixel(img, 984, 300), (30, 80, 200, 255), tolerance=12, label="right pane padding")


def _test_f08_t2_04_light_theme_with_dark_program_bg(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(LIGHT_COLORS_TOML, theme_name="mock-light")
    ctx.write_settings({"sidebar_behavior": "hidden", "pane_header_mode": "hidden"})
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _new_session_or_fail(
            srv, "lt_tui", ["sh", "-c", "printf '\\033[48;2;15;25;45m\\033[2J'; sleep 6"]
        )
        time.sleep(0.3)
        img, _ = _render_session(ctx, srv, "lt_tui", "f08_t2_04.png", width=800, height=600, light=True)
    frame_lum = relative_luminance(sample_pixel(img, 2, 2)[:3])
    assert frame_lum > 0.5, f"Expected outer frame to remain light (lum={frame_lum:.3f})"
    assert_color_near(sample_pixel(img, 15, 15), (15, 25, 45, 255), tolerance=12, label="dark TUI padding")


def _test_f08_t2_05_pane_header_tints_with_program_bg(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    ctx.write_settings({"sidebar_behavior": "hidden", "pane_header_mode": "always"})
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _new_session_or_fail(srv, "hdr_idle", ["sh", "-c", "printf '\\033[2J\\033[H'; sleep 6"])
        _new_session_or_fail(
            srv, "hdr_tui", ["sh", "-c", "printf '\\033[48;2;20;140;60m\\033[2J'; sleep 6"]
        )
        time.sleep(0.3)
        img_idle, _ = _render_session(ctx, srv, "hdr_idle", "f08_t2_05_idle.png", width=800, height=600)
        img_tui, _ = _render_session(ctx, srv, "hdr_tui", "f08_t2_05_tui.png", width=800, height=600)
    hdr_rect = (200, 18, 400, 16)
    diff = pixel_diff_stats(img_idle, img_tui, rect=hdr_rect)
    mean_idle = mean_color_in_rect(img_idle, *hdr_rect)
    mean_tui = mean_color_in_rect(img_tui, *hdr_rect)
    assert diff["diff_fraction"] > 0.05 or mean_tui[1] > mean_idle[1] + 10.0, (
        f"Expected pane header strip to tint toward program_bg: diff={diff}, "
        f"idle_mean={mean_idle}, tui_mean={mean_tui}"
    )


def register_tests(registry: TestRegistry) -> None:
    cases = [
        # F3
        TestCase("F03-T1-01", 1, "F3", "ASCII fastpath terminal render with printable range", _test_f03_t1_01_ascii_fastpath_render),
        TestCase("F03-T1-02", 1, "F3", "Run-shaping merges -> and => two-char ligatures", _test_f03_t1_02_ligature_two_char_operators),
        TestCase("F03-T1-03", 1, "F3", "Run-shaping merges !== and === triple-char ligatures", _test_f03_t1_03_ligature_triple_char_operators),
        TestCase("F03-T1-04", 1, "F3", "Run-shaping merges :: and |> operator ligatures", _test_f03_t1_04_ligature_scope_and_pipe_operators),
        TestCase("F03-T1-05", 1, "F3", "font_size_pt clamps cleanly to 7..36 range", _test_f03_t1_05_font_size_clamping),
        TestCase("F03-T2-01", 2, "F3", "Ligature font vs non-ligature font glyph and pixel diff", _test_f03_t2_01_ligature_vs_non_ligature_font),
        TestCase("F03-T2-02", 2, "F3", "HiDPI scale 1.0 vs 2.0 preserves ligature run-shaping", _test_f03_t2_02_hidpi_scale_ligatures),
        TestCase("F03-T2-03", 2, "F3", "Mixed ASCII and operator run preserves terminal column alignment", _test_f03_t2_03_mixed_ascii_operator_alignment),
        TestCase("F03-T2-04", 2, "F3", "Viewer and terminal ligature parity on Rust operators", _test_f03_t2_04_viewer_vs_terminal_ligature_parity),
        TestCase("F03-T2-05", 2, "F3", "Invalid font_family falls back gracefully without crash", _test_f03_t2_05_invalid_font_family_fallback),
        # F4
        TestCase("F04-T1-01", 1, "F4", "Powerline sample fixture integrity and viewer render", _test_f04_t1_01_powerline_fixture_and_viewer),
        TestCase("F04-T1-02", 1, "F4", "Powerline U+E0B0/U+E0B2 triangles render full cell height", _test_f04_t1_02_powerline_triangles_render),
        TestCase("F04-T1-03", 1, "F4", "Powerline U+E0B4/U+E0B6 rounded separators render cleanly", _test_f04_t1_03_powerline_rounded_separators),
        TestCase("F04-T1-04", 1, "F4", "Box-drawing U+2500..U+257F grid renders without gaps", _test_f04_t1_04_box_drawing_grid),
        TestCase("F04-T1-05", 1, "F4", "Powerline prompt renders inside Rex terminal with sidebar", _test_f04_t1_05_powerline_prompt_with_sidebar),
        TestCase("F04-T2-01", 2, "F4", "Extra Powerline glyphs U+E0B8..U+E0D6 render continuously", _test_f04_t2_01_extra_powerline_symbols_coverage),
        TestCase("F04-T2-02", 2, "F4", "Powerline glyphs render crisply at scale 1.0 and 2.0", _test_f04_t2_02_powerline_hidpi_scaling),
        TestCase("F04-T2-03", 2, "F4", "Powerline prompt adapts between dark and --light palettes", _test_f04_t2_03_powerline_dark_vs_light),
        TestCase("F04-T2-04", 2, "F4", "Powerline output at minimum 320x240 width does not crash", _test_f04_t2_04_narrow_width_no_clipping_crash),
        TestCase("F04-T2-05", 2, "F4", "Powerline colored background segment preserves contrast", _test_f04_t2_05_powerline_colored_segment_contrast),
        # F5
        TestCase("F05-T1-01", 1, "F5", "copy-mode headless scenario renders Copy Shine frame", _test_f05_t1_01_copy_mode_scenario_renders),
        TestCase("F05-T1-02", 1, "F5", "copy-mode settles cleanly and differs visibly from idle pane", _test_f05_t1_02_copy_mode_settle_vs_idle),
        TestCase("F05-T1-03", 1, "F5", "OSC 52 clipboard_read roundtrip paired with copy-mode render", _test_f05_t1_03_osc52_clipboard_and_copy_mode_shine),
        TestCase("F05-T1-04", 1, "F5", "Empty clipboard_read returns not_found and copy-mode renders safely", _test_f05_t1_04_empty_clipboard_and_copy_mode_safe),
        TestCase("F05-T1-05", 1, "F5", "OSC 52 UTF-8 clipboard roundtrip paired with copy-mode render", _test_f05_t1_05_osc52_utf8_roundtrip_and_copy_mode),
        TestCase("F05-T2-01", 2, "F5", "copy-mode scenario differs visibly from idle workspace", _test_f05_t2_01_copy_mode_differs_from_idle),
        TestCase("F05-T2-02", 2, "F5", "copy-mode scenario adapts cleanly between dark and --light", _test_f05_t2_02_copy_mode_dark_vs_light),
        TestCase("F05-T2-03", 2, "F5", "copy-mode compares against idle pane and dark vs --light theme", _test_f05_t2_03_copy_mode_vs_idle_and_light_palette),
        TestCase("F05-T2-04", 2, "F5", "copy-mode Copy Shine stays clipped inside pane bounds", _test_f05_t2_04_copy_shine_clipped_inside_pane),
        TestCase("F05-T2-05", 2, "F5", "copy-mode renders across 1x and 2x scales cleanly", _test_f05_t2_05_copy_mode_multiscale_natural_anim),
        # F6
        TestCase("F06-T1-01", 1, "F6", "highlight_active_border toggles accent border pixel color", _test_f06_t1_01_highlight_active_border_toggle),
        TestCase("F06-T1-02", 1, "F6", "sidebar_behavior always vs hidden shifts workspace layout", _test_f06_t1_02_sidebar_always_vs_hidden),
        TestCase("F06-T1-03", 1, "F6", "fade_inactive_panes dims inactive split pane rendering", _test_f06_t1_03_fade_inactive_panes_toggle),
        TestCase("F06-T1-04", 1, "F6", "sidebar_width_px clamps cleanly to 180..380 range", _test_f06_t1_04_sidebar_width_clamping),
        TestCase("F06-T1-05", 1, "F6", "adaptive_rounding config preserves 2-pane split layout consistency", _test_f06_t1_05_adaptive_rounding_config),
        TestCase("F06-T2-01", 2, "F6", "split-panes scenario renders active border and distinct panes", _test_f06_t2_01_split_panes_scenario_border),
        TestCase("F06-T2-02", 2, "F6", "Split panes highlight_active_border toggle produces border diff", _test_f06_t2_02_split_panes_active_border_diff),
        TestCase("F06-T2-03", 2, "F6", "split-panes settles deterministically with fast frame time", _test_f06_t2_03_split_panes_animation_settle),
        TestCase("F06-T2-04", 2, "F6", "split-panes renders dark vs --light theme contrast", _test_f06_t2_04_split_panes_dark_vs_light),
        TestCase("F06-T2-05", 2, "F6", "Viewer transition settle finishes with healthy frame telemetry", _test_f06_t2_05_viewer_transition_settle_telemetry),
        # F7
        TestCase("F07-T1-01", 1, "F7", "250-line output populates Rex scrollback and renders cleanly", _test_f07_t1_01_large_scrollback_generation),
        TestCase("F07-T1-02", 1, "F7", "300-line scrollback integrity and tail glyph rendering", _test_f07_t1_02_scrollback_tail_rendering),
        TestCase("F07-T1-03", 1, "F7", "250-line scrollback viewport reflows across font sizes without warnings", _test_f07_t1_03_scrollback_font_reflow_no_warnings),
        TestCase("F07-T1-04", 1, "F7", "Rex block.event wheel scroll accepted cleanly before render", _test_f07_t1_04_wheel_event_accepted),
        TestCase("F07-T1-05", 1, "F7", "Rapid 2000-line output burst renders without crash", _test_f07_t1_05_rapid_2000_line_burst),
        TestCase("F07-T2-01", 2, "F7", "Incremental output updates rendered terminal viewport tail", _test_f07_t2_01_tail_updates_on_new_output),
        TestCase("F07-T2-02", 2, "F7", "Repeated scrollback bursts settle fast with updated tail pixels", _test_f07_t2_02_scrollback_burst_settle_telemetry),
        TestCase("F07-T2-03", 2, "F7", "Rex terminal format/clear/resize actions contract available", _test_f07_t2_03_rex_terminal_actions_contract),
        TestCase("F07-T2-04", 2, "F7", "Continuous 500-line output flood produces valid CRC-verified PNG", _test_f07_t2_04_continuous_500_line_flood_no_corruption),
        TestCase("F07-T2-05", 2, "F7", "Scrollback preserved across block.method resize transitions", _test_f07_t2_05_scrollback_preserved_across_resize),
        # F8
        TestCase("F08-T1-01", 1, "F8", "Default idle pane frame, padding, and grid theme colors", _test_f08_t1_01_default_padding_baseline),
        TestCase("F08-T1-02", 1, "F8", "Fullscreen TUI background extends into pane padding", _test_f08_t1_02_fullscreen_tui_extends_into_padding),
        TestCase("F08-T1-03", 1, "F8", "Alternate screen buffer entry and exit via block.method modes", _test_f08_t1_03_alt_screen_enter_exit_modes),
        TestCase("F08-T1-04", 1, "F8", "pane_header_mode always vs hidden changes top header strip", _test_f08_t1_04_pane_header_mode_variants),
        TestCase("F08-T1-05", 1, "F8", "Dark vs --light default terminal padding colors", _test_f08_t1_05_dark_vs_light_default_padding),
        TestCase("F08-T2-01", 2, "F8", "Fullscreen truecolor TUI fills all four pane padding margins", _test_f08_t2_01_program_bg_all_four_margins),
        TestCase("F08-T2-02", 2, "F8", "Exiting alt-screen TUI restores theme background padding", _test_f08_t2_02_alt_screen_exit_restores_padding),
        TestCase("F08-T2-03", 2, "F8", "Split panes isolate distinct program_bg padding per pane", _test_f08_t2_03_split_panes_isolated_program_bg),
        TestCase("F08-T2-04", 2, "F8", "Light theme outer frame coexists with dark TUI program_bg padding", _test_f08_t2_04_light_theme_with_dark_program_bg),
        TestCase("F08-T2-05", 2, "F8", "Pane header strip tints toward fullscreen TUI program_bg", _test_f08_t2_05_pane_header_tints_with_program_bg),
    ]
    for case in cases:
        registry.add(case)
