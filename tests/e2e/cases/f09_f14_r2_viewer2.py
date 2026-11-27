"""Tier 1 and Tier 2 opaque-box E2E tests for R2 Agent-Companion 2.0 viewer features F9 through F14."""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import time
from pathlib import Path

from lib.fixtures import DARK_COLORS_TOML, LIGHT_COLORS_TOML, create_fixtures, tool_capabilities
from lib.harness import HeadlessReport, TestCase, TestContext, TestRegistry
from lib.png_check import (
    PngImage,
    assert_not_solid_color,
    assert_valid_png,
    mean_color_in_rect,
    pixel_diff_stats,
    relative_luminance,
    unique_color_count_sample,
)
from lib.rex_helper import IsolatedRexServer, rex_available

# Viewer body geometry at 1440x900 with sidebar_behavior="always", sidebar_width_px=248.0
VIEWER_BODY = (910, 72, 510, 810)
VIEWER_TABBAR = (905, 6, 520, 38)
VIEWER_GUTTER = (905, 72, 60, 400)
VIEWER_DIFF_GUTTER = (905, 72, 120, 420)


def _setup_dark_sidebar(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    ctx.write_settings({"sidebar_behavior": "always", "sidebar_width_px": 248.0})


def _start_blank_session(srv: IsolatedRexServer, label: str = "viewer_bg") -> None:
    srv.new_session(label=label, command=["sh", "-c", "printf '\\033[2J\\033[H'; sleep 25"])
    time.sleep(0.2)


def _render_viewer(
    ctx: TestContext,
    out_name: str,
    viewer_paths: list[str | Path] | None = None,
    *,
    srv: IsolatedRexServer | None = None,
    session_label: str = "viewer_bg",
    width: int = 1440,
    height: int = 900,
    scale: float | None = None,
    light: bool = False,
    scenario: str | None = None,
    extra_env: dict[str, str] | None = None,
) -> tuple[PngImage, HeadlessReport]:
    if srv is not None:
        rex_mode = "custom_socket"
        custom_socket = srv.endpoint
        extra_args = [session_label]
    else:
        rex_mode = "isolated_offline"
        custom_socket = None
        extra_args = None

    if extra_env is not None:
        out_path = ctx.out_dir / out_name
        out_path.parent.mkdir(parents=True, exist_ok=True)
        args: list[str] = ["--headless-render", str(out_path), "--width", str(width), "--height", str(height)]
        if scale is not None:
            args.extend(["--scale", str(scale)])
        if light:
            args.append("--light")
        if viewer_paths:
            for vp in viewer_paths:
                args.extend(["--headless-viewer", str(vp)])
        if scenario is not None:
            args.extend(["--headless-scenario", scenario])
        if extra_args:
            args.extend(extra_args)
        res = ctx.run_omarex(
            args,
            rex_mode=rex_mode,
            custom_socket=custom_socket,
            extra_env=extra_env,
            rust_log="off",
            timeout=15.0,
        )
        assert res.returncode == 0, f"omarex failed (rc={res.returncode}): {res.clean_stderr}"
        img = assert_valid_png(out_path, expected_width=width, expected_height=height, min_bytes=2048)
        from lib.harness import parse_headless_report

        rep = parse_headless_report(res.stdout)
        return img, rep

    res, png_path, rep = ctx.headless_render(
        out_name=out_name,
        extra_args=extra_args,
        width=width,
        height=height,
        scale=scale,
        light=light,
        viewer_paths=viewer_paths,
        scenario=scenario,
        rex_mode=rex_mode,
        custom_socket=custom_socket,
        rust_log="off",
        timeout=15.0,
    )
    assert res.returncode == 0, (
        f"omarex viewer render failed (rc={res.returncode}): {res.clean_stderr}\n{res.clean_stdout}"
    )
    assert not res.crashed, f"omarex crashed with signal {res.signal_num}"
    assert rep is not None, f"Missing HeadlessReport in stdout:\n{res.clean_stdout}"
    img = assert_valid_png(png_path, expected_width=width, expected_height=height, min_bytes=2048)
    return img, rep


# ---------------------------------------------------------------------------
# F9: Multi-token syntax highlighting + theme mapping
# ---------------------------------------------------------------------------


def _test_f09_t1_01_viewer_rust_offline(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=False)
    _setup_dark_sidebar(ctx)
    img_base, _ = _render_viewer(ctx, "f09_t1_01_base.png")
    img_rs, rep_rs = _render_viewer(ctx, "f09_t1_01_rs.png", [bundle.sample_rs])
    assert rep_rs.glyphs > 800, f"Expected > 800 glyphs for sample.rs, got {rep_rs.glyphs}"
    diff = pixel_diff_stats(img_base, img_rs)
    assert diff["diff_fraction"] > 0.25, f"Expected viewer sample.rs diff > 0.25: {diff}"


def _test_f09_t1_02_viewer_python(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=False)
    _setup_dark_sidebar(ctx)
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_base, _ = _render_viewer(ctx, "f09_t1_02_base.png", srv=srv)
        img_py, _ = _render_viewer(ctx, "f09_t1_02_py.png", [bundle.sample_py], srv=srv)
    assert_not_solid_color(img_py, min_unique_colors=6, step=2)
    diff = pixel_diff_stats(img_base, img_py, rect=VIEWER_BODY)
    assert diff["diff_fraction"] > 0.02, f"Expected sample.py VIEWER_BODY diff > 0.02 vs baseline: {diff}"


def _test_f09_t1_03_viewer_javascript(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=False)
    _setup_dark_sidebar(ctx)
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_base, _ = _render_viewer(ctx, "f09_t1_03_base.png", srv=srv)
        img_js, _ = _render_viewer(ctx, "f09_t1_03_js.png", [bundle.sample_js], srv=srv)
    assert_not_solid_color(img_js, min_unique_colors=6, step=2)
    diff = pixel_diff_stats(img_base, img_js, rect=VIEWER_BODY)
    assert diff["diff_fraction"] > 0.01, f"Expected sample.js VIEWER_BODY diff > 0.01 vs baseline: {diff}"


def _test_f09_t1_04_viewer_json(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=False)
    _setup_dark_sidebar(ctx)
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_base, _ = _render_viewer(ctx, "f09_t1_04_base.png", srv=srv)
        img_json, _ = _render_viewer(ctx, "f09_t1_04_json.png", [bundle.sample_json], srv=srv)
    assert_not_solid_color(img_json, min_unique_colors=5, step=2)
    diff = pixel_diff_stats(img_base, img_json, rect=VIEWER_BODY)
    assert diff["diff_fraction"] > 0.008, f"Expected sample.json VIEWER_BODY diff > 0.008 vs baseline: {diff}"


def _test_f09_t1_05_syntax_highlighting_vs_plain_txt(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=False)
    plain_txt = ctx.work_dir / "plain.txt"
    plain_txt.write_text(bundle.sample_rs.read_text(encoding="utf-8"), encoding="utf-8")
    _setup_dark_sidebar(ctx)
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_rs, _ = _render_viewer(ctx, "f09_t1_05_rs.png", [bundle.sample_rs], srv=srv)
        img_txt, _ = _render_viewer(ctx, "f09_t1_05_txt.png", [plain_txt], srv=srv)
    diff = pixel_diff_stats(img_txt, img_rs, rect=VIEWER_BODY)
    assert diff["diff_fraction"] > 0.01, (
        f"Expected language-aware .rs syntax highlighting to differ from plain .txt: {diff}"
    )
    assert unique_color_count_sample(img_rs, step=2) >= 6


def _test_f09_t2_01_syntax_viewer_scenario(ctx: TestContext) -> None:
    _setup_dark_sidebar(ctx)
    img, rep = _render_viewer(ctx, "f09_t2_01.png", scenario="syntax-viewer")
    assert rep.glyphs > 400, f"Expected > 400 glyphs in syntax-viewer scenario, got {rep.glyphs}"
    assert_not_solid_color(img, min_unique_colors=8, step=2)


def _test_f09_t2_02_syntax_theme_dark_vs_light(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=False)
    _setup_dark_sidebar(ctx)
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_d, _ = _render_viewer(ctx, "f09_t2_02_d.png", [bundle.sample_rs], srv=srv, light=False)
        img_l, _ = _render_viewer(ctx, "f09_t2_02_l.png", [bundle.sample_rs], srv=srv, light=True)
    diff = pixel_diff_stats(img_d, img_l, rect=VIEWER_BODY)
    assert diff["diff_fraction"] > 0.30, f"Expected dark vs --light viewer diff > 0.30: {diff}"
    lum_d = relative_luminance(mean_color_in_rect(img_d, *VIEWER_BODY)[:3])
    lum_l = relative_luminance(mean_color_in_rect(img_l, *VIEWER_BODY)[:3])
    assert lum_l > lum_d + 0.35, f"Expected light viewer lum ({lum_l:.3f}) >> dark ({lum_d:.3f})"


def _test_f09_t2_03_viewer_line_jump_scroll_diff(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=False)
    _setup_dark_sidebar(ctx)
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_1, _ = _render_viewer(ctx, "f09_t2_03_1.png", [f"{bundle.sample_rs}:1"], srv=srv)
        img_42, _ = _render_viewer(ctx, "f09_t2_03_42.png", [f"{bundle.sample_rs}:42"], srv=srv)
    diff = pixel_diff_stats(img_1, img_42, rect=VIEWER_BODY)
    assert diff["diff_fraction"] > 0.03, f"Expected :1 vs :42 scroll diff > 0.03: {diff}"


def _test_f09_t2_04_unknown_extension_fallback(ctx: TestContext) -> None:
    _setup_dark_sidebar(ctx)
    unknown = ctx.work_dir / "unknown.xyz123"
    unknown.write_text("alpha = 1\nbeta = 2\ngamma = 3\n", encoding="utf-8")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_base, _ = _render_viewer(ctx, "f09_t2_04_base.png", srv=srv)
        img_unk, _ = _render_viewer(ctx, "f09_t2_04_unk.png", [unknown], srv=srv)
    diff = pixel_diff_stats(img_base, img_unk, rect=VIEWER_BODY)
    assert diff["diff_fraction"] > 0.002 and diff["changed_pixels"] >= 500, (
        f"Expected unknown extension to render cleanly: {diff}"
    )


def _test_f09_t2_05_large_rust_file_no_truncation(ctx: TestContext) -> None:
    _setup_dark_sidebar(ctx)
    big_rs = ctx.work_dir / "large_160.rs"
    big_lines = [f"pub fn item_{i:03}(x: i32) -> i32 {{ x + {i} }}" for i in range(1, 165)]
    big_rs.write_text("\n".join(big_lines) + "\n", encoding="utf-8")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_top, rep_top = _render_viewer(ctx, "f09_t2_05_top.png", [f"{big_rs}:1"], srv=srv)
        img_bot, _ = _render_viewer(ctx, "f09_t2_05_bot.png", [f"{big_rs}:140"], srv=srv)
    assert rep_top.glyphs > 800, f"Expected > 800 glyphs for large Rust file, got {rep_top.glyphs}"
    diff = pixel_diff_stats(img_top, img_bot, rect=VIEWER_BODY)
    assert diff["diff_fraction"] > 0.03, f"Expected :1 vs :140 diff > 0.03: {diff}"


# ---------------------------------------------------------------------------
# F10: inotify live auto-reload in viewer
# ---------------------------------------------------------------------------


def _test_f10_t1_01_local_watched_file_open(ctx: TestContext) -> None:
    _setup_dark_sidebar(ctx)
    watched = ctx.work_dir / "watched.rs"
    watched.write_text("pub fn initial_fn() -> i32 { 1 }\n", encoding="utf-8")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_base, _ = _render_viewer(ctx, "f10_t1_01_base.png", srv=srv)
        img_w, _ = _render_viewer(ctx, "f10_t1_01_w.png", [watched], srv=srv)
    diff = pixel_diff_stats(img_base, img_w, rect=VIEWER_BODY)
    assert diff["diff_fraction"] > 0.002 and diff["changed_pixels"] >= 500, (
        f"Expected watched.rs VIEWER_BODY diff > 0.002: {diff}"
    )


def _test_f10_t1_02_atomic_replace_updates_viewer(ctx: TestContext) -> None:
    _setup_dark_sidebar(ctx)
    watched = ctx.work_dir / "atomic.rs"
    watched.write_text("pub fn version_one() -> i32 { 101 }\n", encoding="utf-8")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_before, _ = _render_viewer(ctx, "f10_t1_02_a.png", [watched], srv=srv)
        tmp = ctx.work_dir / "atomic.rs.tmp"
        tmp.write_text(
            "\n".join(f"pub fn updated_fn_{i}(x: i32) -> i32 {{ x * {i} }}" for i in range(1, 25)) + "\n",
            encoding="utf-8",
        )
        os.replace(tmp, watched)
        img_after, _ = _render_viewer(ctx, "f10_t1_02_b.png", [watched], srv=srv)
    diff = pixel_diff_stats(img_before, img_after, rect=VIEWER_BODY)
    assert diff["diff_fraction"] > 0.02, f"Expected atomic replace to update rendered content: {diff}"


def _test_f10_t1_03_delete_and_recreate_resilience(ctx: TestContext) -> None:
    _setup_dark_sidebar(ctx)
    watched = ctx.work_dir / "ephemeral.rs"
    watched.write_text("pub fn temp() -> bool { true }\n", encoding="utf-8")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        watched.unlink()
        img_deleted, _ = _render_viewer(ctx, "f10_t1_03_del.png", [watched], srv=srv)
        watched.write_text(
            "\n".join(f"pub fn recreated_{i}() -> usize {{ {i} }}" for i in range(1, 20)) + "\n",
            encoding="utf-8",
        )
        img_recreated, _ = _render_viewer(ctx, "f10_t1_03_rec.png", [watched], srv=srv)
    diff = pixel_diff_stats(img_deleted, img_recreated, rect=VIEWER_BODY)
    assert diff["diff_fraction"] > 0.02, f"Expected recreated file to differ from deleted-file state: {diff}"


def _test_f10_t1_04_nonexistent_path_graceful(ctx: TestContext) -> None:
    _setup_dark_sidebar(ctx)
    missing = ctx.work_dir / "does_not_exist_12345.rs"
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img, _ = _render_viewer(ctx, "f10_t1_04.png", [missing], srv=srv)
    assert_not_solid_color(img, min_unique_colors=4)


def _test_f10_t1_05_symlink_viewer_open(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=False)
    _setup_dark_sidebar(ctx)
    link_rs = ctx.work_dir / "symlink_sample.rs"
    link_rs.symlink_to(bundle.sample_rs)
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_orig, _ = _render_viewer(ctx, "f10_t1_05_orig.png", [bundle.sample_rs], srv=srv)
        img_link, _ = _render_viewer(ctx, "f10_t1_05_link.png", [link_rs], srv=srv)
    diff = pixel_diff_stats(img_orig, img_link, rect=VIEWER_BODY)
    assert diff["diff_fraction"] < 0.02, f"Expected symlinked file body to match original: {diff}"


def _test_f10_t2_01_live_reload_syntax_viewer_with_edit(ctx: TestContext) -> None:
    _setup_dark_sidebar(ctx)
    watched = ctx.work_dir / "live_watch.rs"
    watched.write_text("pub fn v1() -> i32 { 1 }\n", encoding="utf-8")
    img_before, rep_before = _render_viewer(
        ctx, "f10_t2_01_a.png", [watched], scenario="syntax-viewer"
    )
    watched.write_text(
        "\n".join(f"pub fn v2_line_{i}() -> i32 {{ {i} }}" for i in range(1, 30)) + "\n",
        encoding="utf-8",
    )
    img_after, rep_after = _render_viewer(
        ctx, "f10_t2_01_b.png", [watched], scenario="syntax-viewer"
    )
    assert rep_before.glyphs > 300 and rep_after.glyphs > 300, (
        f"Expected syntax-viewer glyphs > 300: before={rep_before.glyphs}, after={rep_after.glyphs}"
    )
    assert_not_solid_color(img_before, min_unique_colors=8, step=2)
    assert_not_solid_color(img_after, min_unique_colors=8, step=2)


def _test_f10_t2_02_atomic_sed_style_rename_syntax_viewer(ctx: TestContext) -> None:
    _setup_dark_sidebar(ctx)
    target = ctx.work_dir / "sed_target.rs"
    target.write_text("pub fn before_sed() -> &'static str { \"old\" }\n", encoding="utf-8")
    tmp = ctx.work_dir / ".sed_target.tmp"
    tmp.write_text("pub fn after_sed() -> &'static str { \"new\" }\n", encoding="utf-8")
    os.replace(tmp, target)
    img, rep = _render_viewer(ctx, "f10_t2_02.png", [target], scenario="syntax-viewer")
    assert rep.glyphs > 300, f"Expected syntax-viewer glyphs > 300, got {rep.glyphs}"
    assert_not_solid_color(img, min_unique_colors=8, step=2)


def _test_f10_t2_03_rapid_consecutive_writes_stabilize(ctx: TestContext) -> None:
    _setup_dark_sidebar(ctx)
    rapid = ctx.work_dir / "rapid.rs"
    rapid.write_text("pub fn step_0() -> i32 { 0 }\n", encoding="utf-8")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_init, _ = _render_viewer(ctx, "f10_t2_03_init.png", [rapid], srv=srv)
        for i in range(1, 21):
            tmp = ctx.work_dir / f".rapid_{i}.tmp"
            tmp.write_text(
                "\n".join(f"pub fn burst_{i}_{j}() -> i32 {{ {i + j} }}" for j in range(1, 20)) + "\n",
                encoding="utf-8",
            )
            os.replace(tmp, rapid)
        img_final, _ = _render_viewer(ctx, "f10_t2_03_final.png", [rapid], srv=srv)
    diff = pixel_diff_stats(img_init, img_final, rect=VIEWER_BODY)
    assert diff["diff_fraction"] > 0.02, f"Expected rapid writes final state to differ from initial: {diff}"


def _test_f10_t2_04_syntax_updates_on_code_rewrite(ctx: TestContext) -> None:
    _setup_dark_sidebar(ctx)
    rs_file = ctx.work_dir / "rewrite.rs"
    txt_file = ctx.work_dir / "rewrite.txt"
    rs_file.write_text("// initial comment only\n", encoding="utf-8")
    txt_file.write_text("// initial comment only\n", encoding="utf-8")
    code_body = (
        'pub fn compute(flag: bool) -> Result<&\'static str, usize> {\n'
        '    if flag { Ok("enabled") } else { Err(42) }\n'
        '}\n'
    ) * 8
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_comment, _ = _render_viewer(ctx, "f10_t2_04_comment.png", [rs_file], srv=srv)
        rs_file.write_text(code_body, encoding="utf-8")
        txt_file.write_text(code_body, encoding="utf-8")
        img_rs, _ = _render_viewer(ctx, "f10_t2_04_rs.png", [rs_file], srv=srv)
        img_txt, _ = _render_viewer(ctx, "f10_t2_04_txt.png", [txt_file], srv=srv)
    reload_diff = pixel_diff_stats(img_comment, img_rs, rect=VIEWER_BODY)
    assert reload_diff["diff_fraction"] > 0.03, (
        f"Expected rewritten .rs to reload and differ from comment-only render: {reload_diff}"
    )
    syntax_diff = pixel_diff_stats(img_rs, img_txt, rect=VIEWER_BODY)
    assert syntax_diff["diff_fraction"] > 0.01, (
        f"Expected rewritten .rs syntax highlighting to differ from .txt: {syntax_diff}"
    )


def _test_f10_t2_05_multi_file_clean_exit(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=False)
    _setup_dark_sidebar(ctx)
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        res, png_path, _ = ctx.headless_render(
            out_name="f10_t2_05.png",
            extra_args=["viewer_bg"],
            viewer_paths=[f"{bundle.sample_rs},{bundle.sample_py},{bundle.sample_js}"],
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="off",
            timeout=10.0,
        )
    assert res.returncode == 0 and res.elapsed_s < 10.0, f"Unexpected exit/hang: rc={res.returncode} t={res.elapsed_s}"
    assert_valid_png(png_path, 1440, 900, min_bytes=4096)


# ---------------------------------------------------------------------------
# F11: Git-diff view in viewer
# ---------------------------------------------------------------------------


def _test_f11_t1_01_git_repo_fixture_opens(ctx: TestContext) -> None:
    caps = tool_capabilities()
    assert caps["git"], "git CLI required for F11 tests"
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=True)
    gs = subprocess.run(
        ["git", "-C", str(bundle.git_repo), "status", "--porcelain"],
        capture_output=True,
        text=True,
        timeout=5.0,
        check=False,
    )
    assert gs.returncode == 0 and " M src/lib.rs" in gs.stdout, f"Unexpected git status: {gs.stdout!r}"
    _setup_dark_sidebar(ctx)
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img, _ = _render_viewer(ctx, "f11_t1_01.png", [bundle.git_tracked_file], srv=srv)
    assert_not_solid_color(img, min_unique_colors=6, step=2)


def _test_f11_t1_02_modified_tracked_shows_diff_vs_nongit(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=True)
    non_git = ctx.work_dir / "non_git_lib.rs"
    non_git.write_text(bundle.git_tracked_file.read_text(encoding="utf-8"), encoding="utf-8")
    _setup_dark_sidebar(ctx)
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_git, _ = _render_viewer(ctx, "f11_t1_02_git.png", [bundle.git_tracked_file], srv=srv)
        img_plain, _ = _render_viewer(ctx, "f11_t1_02_plain.png", [non_git], srv=srv)
    diff = pixel_diff_stats(img_git, img_plain, rect=VIEWER_DIFF_GUTTER)
    assert diff["changed_pixels"] >= 60, (
        f"Expected modified git file to render diff markers in gutter vs identical non-git file: {diff}"
    )


def _test_f11_t1_03_staged_file_shows_diff_vs_nongit(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=True)
    non_git = ctx.work_dir / "non_git_staged.rs"
    non_git.write_text(bundle.git_staged_file.read_text(encoding="utf-8"), encoding="utf-8")
    _setup_dark_sidebar(ctx)
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_staged, _ = _render_viewer(ctx, "f11_t1_03_stg.png", [bundle.git_staged_file], srv=srv)
        img_plain, _ = _render_viewer(ctx, "f11_t1_03_plain.png", [non_git], srv=srv)
    diff = pixel_diff_stats(img_staged, img_plain, rect=VIEWER_DIFF_GUTTER)
    assert diff["changed_pixels"] >= 35, (
        f"Expected staged git file to render added-file diff markers in gutter vs non-git file: {diff}"
    )


def _test_f11_t1_04_untracked_file_no_spurious_error(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=True)
    _setup_dark_sidebar(ctx)
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img, _ = _render_viewer(ctx, "f11_t1_04.png", [bundle.git_untracked_file], srv=srv)
    assert_not_solid_color(img, min_unique_colors=4, step=2)


def _test_f11_t1_05_nongit_file_no_spurious_diff(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=False)
    _setup_dark_sidebar(ctx)
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img, _ = _render_viewer(ctx, "f11_t1_05.png", [bundle.sample_rs], srv=srv)
    assert_not_solid_color(img, min_unique_colors=6, step=2)


def _test_f11_t2_01_clean_commit_vs_unstaged_mod_gutter(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=True)
    _setup_dark_sidebar(ctx)
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_dirty, _ = _render_viewer(ctx, "f11_t2_01_dirty.png", [bundle.git_tracked_file], srv=srv)
        subprocess.run(
            ["git", "-C", str(bundle.git_repo), "checkout", "--", "src/lib.rs"],
            check=True,
            timeout=5.0,
        )
        img_clean, _ = _render_viewer(ctx, "f11_t2_01_clean.png", [bundle.git_tracked_file], srv=srv)
    diff = pixel_diff_stats(img_clean, img_dirty, rect=VIEWER_DIFF_GUTTER)
    assert diff["changed_pixels"] >= 60, (
        f"Expected dirty vs clean commit diff gutter markers: {diff}"
    )


def _test_f11_t2_02_staged_and_unstaged_diff_distinction(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=True)
    plain_tracked = ctx.work_dir / "plain_tracked.rs"
    plain_staged = ctx.work_dir / "plain_staged.rs"
    plain_tracked.write_text(bundle.git_tracked_file.read_text(encoding="utf-8"), encoding="utf-8")
    plain_staged.write_text(bundle.git_staged_file.read_text(encoding="utf-8"), encoding="utf-8")
    _setup_dark_sidebar(ctx)
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_u, _ = _render_viewer(ctx, "f11_t2_02_u.png", [bundle.git_tracked_file], srv=srv)
        img_pu, _ = _render_viewer(ctx, "f11_t2_02_pu.png", [plain_tracked], srv=srv)
        img_s, _ = _render_viewer(ctx, "f11_t2_02_s.png", [bundle.git_staged_file], srv=srv)
        img_ps, _ = _render_viewer(ctx, "f11_t2_02_ps.png", [plain_staged], srv=srv)
    diff_u = pixel_diff_stats(img_u, img_pu, rect=VIEWER_DIFF_GUTTER)
    diff_s = pixel_diff_stats(img_s, img_ps, rect=VIEWER_DIFF_GUTTER)
    assert diff_u["changed_pixels"] >= 60 and diff_s["changed_pixels"] >= 35, (
        f"Expected both unstaged ({diff_u}) and staged ({diff_s}) git diff indicators in gutter"
    )


def _test_f11_t2_03_diff_clears_after_git_commit(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=True)
    _setup_dark_sidebar(ctx)
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_dirty, _ = _render_viewer(ctx, "f11_t2_03_dirty.png", [bundle.git_tracked_file], srv=srv)
        subprocess.run(["git", "-C", str(bundle.git_repo), "add", "src/lib.rs"], check=True, timeout=5.0)
        subprocess.run(
            ["git", "-C", str(bundle.git_repo), "commit", "-q", "-m", "commit edit"],
            check=True,
            timeout=5.0,
        )
        img_committed, _ = _render_viewer(ctx, "f11_t2_03_clean.png", [bundle.git_tracked_file], srv=srv)
    diff = pixel_diff_stats(img_dirty, img_committed, rect=VIEWER_DIFF_GUTTER)
    assert diff["changed_pixels"] >= 60, (
        f"Expected diff markers to clear in gutter after committing same file text: {diff}"
    )


def _test_f11_t2_04_git_diff_dark_vs_light(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=True)
    non_git = ctx.work_dir / "non_git_cmp.rs"
    non_git.write_text(bundle.git_tracked_file.read_text(encoding="utf-8"), encoding="utf-8")
    _setup_dark_sidebar(ctx)
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_gd, _ = _render_viewer(ctx, "f11_t2_04_gd.png", [bundle.git_tracked_file], srv=srv, light=False)
        img_gl, _ = _render_viewer(ctx, "f11_t2_04_gl.png", [bundle.git_tracked_file], srv=srv, light=True)
        img_pd, _ = _render_viewer(ctx, "f11_t2_04_pd.png", [non_git], srv=srv, light=False)
        img_pl, _ = _render_viewer(ctx, "f11_t2_04_pl.png", [non_git], srv=srv, light=True)
    assert pixel_diff_stats(img_gd, img_gl, rect=VIEWER_BODY)["diff_fraction"] > 0.30
    diff_d = pixel_diff_stats(img_gd, img_pd, rect=VIEWER_DIFF_GUTTER)
    diff_l = pixel_diff_stats(img_gl, img_pl, rect=VIEWER_DIFF_GUTTER)
    assert diff_d["changed_pixels"] >= 60, f"Missing dark git diff gutter markers: {diff_d}"
    assert diff_l["changed_pixels"] >= 60, f"Missing light git diff gutter markers: {diff_l}"


def _test_f11_t2_05_large_multihunk_diff_scroll(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=True)
    big = bundle.git_repo / "src" / "multi_hunk.rs"
    lines = [f"pub fn h_{i:03}() -> i32 {{ {i} }}" for i in range(1, 91)]
    big.write_text("\n".join(lines) + "\n", encoding="utf-8")
    subprocess.run(["git", "-C", str(bundle.git_repo), "add", "src/multi_hunk.rs"], check=True, timeout=5.0)
    subprocess.run(["git", "-C", str(bundle.git_repo), "commit", "-q", "-m", "add multi"], check=True, timeout=5.0)
    lines[4] = "pub fn h_005() -> i32 { 5555 } // hunk 1"
    lines[69] = "pub fn h_070() -> i32 { 7070 } // hunk 2"
    big.write_text("\n".join(lines) + "\n", encoding="utf-8")

    non_git = ctx.work_dir / "multi_hunk_plain.rs"
    non_git.write_text(big.read_text(encoding="utf-8"), encoding="utf-8")
    _setup_dark_sidebar(ctx)
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_1, _ = _render_viewer(ctx, "f11_t2_05_1.png", [f"{big}:1"], srv=srv)
        img_70, _ = _render_viewer(ctx, "f11_t2_05_70.png", [f"{big}:70"], srv=srv)
        img_plain70, _ = _render_viewer(ctx, "f11_t2_05_p70.png", [f"{non_git}:70"], srv=srv)
    assert pixel_diff_stats(img_1, img_70, rect=VIEWER_BODY)["diff_fraction"] > 0.03
    diff_hunk = pixel_diff_stats(img_70, img_plain70, rect=VIEWER_DIFF_GUTTER)
    assert diff_hunk["changed_pixels"] >= 35, (
        f"Expected hunk 2 at line 70 to show git-diff gutter markers vs non-git copy: {diff_hunk}"
    )


# ---------------------------------------------------------------------------
# F12: Viewer text selection, path:line copy & Ctrl+F search
# ---------------------------------------------------------------------------


def _test_f12_t1_01_path_line_jump_cli(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=False)
    _setup_dark_sidebar(ctx)
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_base, _ = _render_viewer(ctx, "f12_t1_01_base.png", srv=srv)
        img, rep = _render_viewer(ctx, "f12_t1_01.png", [f"{bundle.sample_rs}:2"], srv=srv)
    assert rep.glyphs > 800, f"Expected > 800 glyphs for sample.rs:2, got {rep.glyphs}"
    assert pixel_diff_stats(img_base, img, rect=VIEWER_BODY)["diff_fraction"] > 0.05


def _test_f12_t1_02_line_col_jump_and_rex_actions(ctx: TestContext) -> None:
    assert rex_available(), "rex CLI binary required"
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=False)
    _setup_dark_sidebar(ctx)
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        act_res = srv.run_cli(["actions", "--json"])
        assert act_res.returncode == 0
        assert len(json.loads(act_res.stdout).get("actions", [])) > 0
        _start_blank_session(srv)
        img_1, _ = _render_viewer(ctx, "f12_t1_02_1.png", [f"{bundle.sample_rs}:1"], srv=srv)
        img_12, _ = _render_viewer(ctx, "f12_t1_02_12.png", [f"{bundle.sample_rs}:12:4"], srv=srv)
    diff = pixel_diff_stats(img_1, img_12, rect=VIEWER_BODY)
    assert diff["diff_fraction"] > 0.02, f"Expected :12:4 line:col jump diff > 0.02: {diff}"


def _test_f12_t1_03_viewer_find_scenario_contract(ctx: TestContext) -> None:
    _setup_dark_sidebar(ctx)
    img, rep = _render_viewer(ctx, "f12_t1_03.png", scenario="syntax-viewer")
    assert rep.glyphs > 400
    assert_not_solid_color(img, min_unique_colors=8, step=2)


def _test_f12_t1_04_multi_file_comma_tabs(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=False)
    _setup_dark_sidebar(ctx)
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_one, _ = _render_viewer(ctx, "f12_t1_04_one.png", [bundle.sample_rs], srv=srv)
        img_multi, _ = _render_viewer(
            ctx,
            "f12_t1_04_multi.png",
            [f"{bundle.sample_rs},{bundle.sample_py},{bundle.sample_js}"],
            srv=srv,
        )
    diff = pixel_diff_stats(img_one, img_multi, rect=VIEWER_TABBAR)
    assert diff["diff_fraction"] > 0.05, f"Expected multi-file viewer tabbar diff > 0.05: {diff}"


def _test_f12_t1_05_line_number_gutter_render(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=False)
    _setup_dark_sidebar(ctx)
    empty = ctx.work_dir / "empty.txt"
    empty.write_text("", encoding="utf-8")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_rs, _ = _render_viewer(ctx, "f12_t1_05_rs.png", [bundle.sample_rs], srv=srv)
        img_emp, _ = _render_viewer(ctx, "f12_t1_05_emp.png", [empty], srv=srv)
    diff = pixel_diff_stats(img_rs, img_emp, rect=VIEWER_GUTTER)
    assert diff["diff_fraction"] > 0.03, f"Expected line-number gutter diff > 0.03: {diff}"


def _test_f12_t2_01_scroll_jump_line_1_vs_60(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=False)
    _setup_dark_sidebar(ctx)
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_1, _ = _render_viewer(ctx, "f12_t2_01_1.png", [f"{bundle.sample_rs}:1"], srv=srv)
        img_60, _ = _render_viewer(ctx, "f12_t2_01_60.png", [f"{bundle.sample_rs}:60"], srv=srv)
    diff = pixel_diff_stats(img_1, img_60, rect=VIEWER_BODY)
    assert diff["diff_fraction"] > 0.03, f"Expected :1 vs :60 diff > 0.03: {diff}"


def _test_f12_t2_02_syntax_viewer_dark_vs_light_highlights(ctx: TestContext) -> None:
    _setup_dark_sidebar(ctx)
    img_d, _ = _render_viewer(ctx, "f12_t2_02_d.png", scenario="syntax-viewer", light=False)
    img_l, _ = _render_viewer(ctx, "f12_t2_02_l.png", scenario="syntax-viewer", light=True)
    assert unique_color_count_sample(img_d, step=2) >= 8
    diff = pixel_diff_stats(img_d, img_l, rect=VIEWER_BODY)
    assert diff["diff_fraction"] > 0.25, f"Expected syntax-viewer dark vs light diff > 0.25: {diff}"


def _test_f12_t2_03_search_target_jump_and_scenario(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=False)
    assert "SEARCH_TARGET_ALPHA" in bundle.sample_py.read_text(encoding="utf-8")
    _setup_dark_sidebar(ctx)
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_1, _ = _render_viewer(ctx, "f12_t2_03_1.png", [f"{bundle.sample_py}:1"], srv=srv)
        img_14, _ = _render_viewer(ctx, "f12_t2_03_14.png", [f"{bundle.sample_py}:14"], srv=srv)
    assert pixel_diff_stats(img_1, img_14, rect=VIEWER_BODY)["diff_fraction"] > 0.02
    img_sc, _ = _render_viewer(ctx, "f12_t2_03_sc.png", scenario="syntax-viewer")
    assert_not_solid_color(img_sc, min_unique_colors=8, step=2)


def _test_f12_t2_04_path_line_col_hidpi_and_scenario(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=False)
    _setup_dark_sidebar(ctx)
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_loc, _ = _render_viewer(ctx, "f12_t2_04_loc.png", [f"{bundle.sample_rs}:42:5"], srv=srv)
    assert_not_solid_color(img_loc, min_unique_colors=6, step=2)
    img_2x, _ = _render_viewer(
        ctx, "f12_t2_04_2x.png", width=1600, height=1000, scale=2.0, scenario="syntax-viewer"
    )
    assert_not_solid_color(img_2x, min_unique_colors=8, step=2)


def _test_f12_t2_05_out_of_range_line_and_empty_file(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=False)
    _setup_dark_sidebar(ctx)
    empty = ctx.work_dir / "zero.txt"
    empty.write_text("", encoding="utf-8")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_oob, _ = _render_viewer(ctx, "f12_t2_05_oob.png", [f"{bundle.sample_rs}:99999"], srv=srv)
        img_emp, _ = _render_viewer(ctx, "f12_t2_05_emp.png", [empty], srv=srv)
    assert_not_solid_color(img_oob, min_unique_colors=5, step=2)
    assert_not_solid_color(img_emp, min_unique_colors=4, step=2)


# ---------------------------------------------------------------------------
# F13: Markdown rendering 2.0
# ---------------------------------------------------------------------------


def _test_f13_t1_01_markdown_open(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=False)
    _setup_dark_sidebar(ctx)
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_base, _ = _render_viewer(ctx, "f13_t1_01_base.png", srv=srv)
        img_md, rep_md = _render_viewer(ctx, "f13_t1_01_md.png", [bundle.sample_md], srv=srv)
    assert rep_md.glyphs > 400, f"Expected > 400 glyphs for sample.md, got {rep_md.glyphs}"
    assert pixel_diff_stats(img_base, img_md, rect=VIEWER_BODY)["diff_fraction"] > 0.10


def _test_f13_t1_02_heading_styling_vs_plain_text(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=False)
    plain = ctx.work_dir / "sample_md.txt"
    plain.write_text(bundle.sample_md.read_text(encoding="utf-8"), encoding="utf-8")
    _setup_dark_sidebar(ctx)
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_md, _ = _render_viewer(ctx, "f13_t1_02_md.png", [bundle.sample_md], srv=srv)
        img_txt, _ = _render_viewer(ctx, "f13_t1_02_txt.png", [plain], srv=srv)
    diff = pixel_diff_stats(img_md, img_txt, rect=VIEWER_BODY)
    assert diff["diff_fraction"] > 0.03, f"Expected formatted Markdown diff > 0.03 vs raw txt: {diff}"


def _test_f13_t1_03_gfm_pipe_table_quads(ctx: TestContext) -> None:
    _setup_dark_sidebar(ctx)
    table_src = "| ColA | ColB | ColC |\n|---|---|---|\n| 1 | 2 | 3 |\n| 4 | 5 | 6 |\n"
    md_path = ctx.work_dir / "table_only.md"
    txt_path = ctx.work_dir / "raw_table.txt"
    md_path.write_text(table_src, encoding="utf-8")
    txt_path.write_text(table_src, encoding="utf-8")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        _, rep_md = _render_viewer(ctx, "f13_t1_03_md.png", [md_path], srv=srv)
        _, rep_txt = _render_viewer(ctx, "f13_t1_03_txt.png", [txt_path], srv=srv)
    assert rep_md.quads > rep_txt.quads + 2, (
        f"Expected GFM table borders/cells to emit extra quads: md={rep_md.quads} vs txt={rep_txt.quads}"
    )


def _test_f13_t1_04_fenced_code_block_multitoken_highlight(ctx: TestContext) -> None:
    _setup_dark_sidebar(ctx)
    fenced_rust = ctx.work_dir / "fenced_rust.md"
    fenced_text = ctx.work_dir / "fenced_text.md"
    code = (
        "// Varied Rust syntax for multi-token highlighting\n"
        "pub struct RouteCheck {\n"
        "    pub port: u16,\n"
        "    pub active: bool,\n"
        "}\n"
        "impl RouteCheck {\n"
        '    pub fn evaluate(&self, limit: usize) -> Result<&\'static str, u16> {\n'
        "        let mut count: usize = 0;\n"
        "        for idx in 0..limit {\n"
        "            count += idx;\n"
        "        }\n"
        '        if self.active && count > 42 { Ok("ready") } else { Err(self.port) }\n'
        "    }\n"
        "}\n"
    )
    fenced_rust.write_text(f"```rust\n{code}```\n", encoding="utf-8")
    fenced_text.write_text(f"```text\n{code}```\n", encoding="utf-8")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_rust, _ = _render_viewer(ctx, "f13_t1_04_rust.png", [fenced_rust], srv=srv)
        img_text, _ = _render_viewer(ctx, "f13_t1_04_text.png", [fenced_text], srv=srv)
    assert unique_color_count_sample(img_rust, step=2) >= 6, (
        "Expected multi-token color diversity (>= 6 unique colors) in fenced Rust block"
    )
    diff = pixel_diff_stats(img_rust, img_text, rect=VIEWER_BODY)
    assert diff["diff_fraction"] > 0.002 and diff["changed_pixels"] >= 800, (
        f"Expected ```rust fenced block highlighting to differ from ```text: {diff}"
    )


def _test_f13_t1_05_inline_local_image_quads(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=False)
    _setup_dark_sidebar(ctx)
    no_img_md = bundle.docs_dir / "no_img.md"
    no_img_md.write_text(
        bundle.sample_md.read_text(encoding="utf-8").replace(
            "![Sample Quadrant Image](../media/sample.png)", ""
        ),
        encoding="utf-8",
    )
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_with, rep_with = _render_viewer(ctx, "f13_t1_05_with.png", [bundle.sample_md], srv=srv)
        img_without, rep_without = _render_viewer(ctx, "f13_t1_05_without.png", [no_img_md], srv=srv)
    diff = pixel_diff_stats(img_with, img_without, rect=VIEWER_BODY)
    assert (
        (rep_with.draws > rep_without.draws or rep_with.quads > rep_without.quads)
        and diff["changed_pixels"] >= 500
        and diff["diff_fraction"] > 0.001
    ), (
        f"Expected inline PNG in sample.md to add image draws/quads and pixels: "
        f"draws {rep_with.draws} vs {rep_without.draws}, "
        f"quads {rep_with.quads} vs {rep_without.quads}, diff={diff}"
    )


def _test_f13_t2_01_markdown_viewer_scenario(ctx: TestContext) -> None:
    _setup_dark_sidebar(ctx)
    img, _ = _render_viewer(ctx, "f13_t2_01.png", scenario="markdown-viewer")
    assert_not_solid_color(img, min_unique_colors=8, step=2)


def _test_f13_t2_02_inline_png_quadrant_pixels(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=False)
    _setup_dark_sidebar(ctx)
    img_only = bundle.docs_dir / "img_only.md"
    img_only.write_text("# Inline PNG\n\n![quad](../media/sample.png)\n", encoding="utf-8")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_present, _ = _render_viewer(ctx, "f13_t2_02_pres.png", [img_only], srv=srv)
        bundle.sample_png.unlink()
        img_missing, _ = _render_viewer(ctx, "f13_t2_02_miss.png", [img_only], srv=srv)
    diff = pixel_diff_stats(img_present, img_missing, rect=VIEWER_BODY)
    assert diff["changed_pixels"] >= 500 and diff["diff_fraction"] > 0.001, (
        f"Expected inline PNG pixels in VIEWER_BODY: {diff}"
    )
    vx, vy, vw, vh = VIEWER_BODY
    px = img_present.pixels
    w = img_present.width
    red_quad_px = 0
    blue_quad_px = 0
    for y in range(vy, vy + vh, 2):
        base = y * w * 4
        for x in range(vx, vx + vw, 2):
            idx = base + x * 4
            r, g, b = px[idx], px[idx + 1], px[idx + 2]
            if r > 200 and g < 90 and b < 90:
                red_quad_px += 1
            elif r < 90 and g < 160 and b > 200:
                blue_quad_px += 1
    assert red_quad_px >= 10 and blue_quad_px >= 10, (
        f"Expected sample.png red/blue quadrant pixels in viewer, got red={red_quad_px}, blue={blue_quad_px}"
    )


def _test_f13_t2_03_fenced_rust_vs_plain_fenced(ctx: TestContext) -> None:
    _setup_dark_sidebar(ctx)
    md_rust = ctx.work_dir / "f_rust.md"
    md_plain = ctx.work_dir / "f_plain.md"
    code = (
        "// Next port allocation pipeline\n"
        "pub struct PortPool {\n"
        "    pub base: u16,\n"
        "    pub limit: u16,\n"
        "}\n"
        "impl PortPool {\n"
        '    pub fn next_port(&self, cur: u16) -> Result<u16, &\'static str> {\n'
        "        let max_port: u16 = self.base + self.limit;\n"
        "        match cur {\n"
        '            0 => Err("invalid"),\n'
        "            p if p >= max_port => Ok(self.base),\n"
        "            p => Ok(p + 1),\n"
        "        }\n"
        "    }\n"
        "}\n"
    )
    md_rust.write_text(f"```rust\n{code}```\n", encoding="utf-8")
    md_plain.write_text(f"```text\n{code}```\n", encoding="utf-8")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_r, _ = _render_viewer(ctx, "f13_t2_03_r.png", [md_rust], srv=srv)
        img_p, _ = _render_viewer(ctx, "f13_t2_03_p.png", [md_plain], srv=srv)
    diff = pixel_diff_stats(img_r, img_p, rect=VIEWER_BODY)
    assert diff["diff_fraction"] > 0.002 and diff["changed_pixels"] >= 800, (
        f"Expected ```rust fenced block syntax highlighting to differ from ```text: {diff}"
    )


def _test_f13_t2_04_broken_image_and_malformed_link_resilience(ctx: TestContext) -> None:
    _setup_dark_sidebar(ctx)
    bad_md = ctx.work_dir / "broken.md"
    bad_md.write_text("# Broken\n\n![missing](./no_such_pic.png)\n\n[broken](\n", encoding="utf-8")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img, _ = _render_viewer(ctx, "f13_t2_04.png", [bad_md], srv=srv)
    assert_not_solid_color(img, min_unique_colors=4, step=2)


def _test_f13_t2_05_markdown_dark_vs_light(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=False)
    _setup_dark_sidebar(ctx)
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_d, _ = _render_viewer(ctx, "f13_t2_05_d.png", [bundle.sample_md], srv=srv, light=False)
        img_l, _ = _render_viewer(ctx, "f13_t2_05_l.png", [bundle.sample_md], srv=srv, light=True)
    diff = pixel_diff_stats(img_d, img_l, rect=VIEWER_BODY)
    assert diff["diff_fraction"] > 0.30, f"Expected Markdown dark vs --light diff > 0.30: {diff}"
    lum_d = relative_luminance(mean_color_in_rect(img_d, *VIEWER_BODY)[:3])
    lum_l = relative_luminance(mean_color_in_rect(img_l, *VIEWER_BODY)[:3])
    assert lum_l > lum_d + 0.35


# ---------------------------------------------------------------------------
# F14: Audio-only & PDF viewer previews
# ---------------------------------------------------------------------------


def _test_f14_t1_01_wav_waveform_vs_binary_hex(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=False)
    wav_bin = ctx.work_dir / "tone_wav.bin"
    wav_bin.write_bytes(bundle.tone_wav.read_bytes())
    _setup_dark_sidebar(ctx)
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_wav, _ = _render_viewer(ctx, "f14_t1_01_wav.png", [bundle.tone_wav], srv=srv)
        img_bin, _ = _render_viewer(ctx, "f14_t1_01_bin.png", [wav_bin], srv=srv)
    diff = pixel_diff_stats(img_wav, img_bin, rect=VIEWER_BODY)
    assert diff["diff_fraction"] > 0.10, (
        f"Expected tone.wav audio preview to differ from binary hex dump: {diff}"
    )


def _test_f14_t1_02_mp3_waveform_vs_binary_hex(ctx: TestContext) -> None:
    caps = tool_capabilities()
    assert caps["ffmpeg"], "ffmpeg required to generate tone.mp3 fixture"
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=True, init_git=False)
    assert bundle.tone_mp3 is not None and bundle.tone_mp3.is_file(), "tone.mp3 fixture missing"
    mp3_bin = ctx.work_dir / "tone_mp3.bin"
    mp3_bin.write_bytes(bundle.tone_mp3.read_bytes())
    _setup_dark_sidebar(ctx)
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_mp3, _ = _render_viewer(ctx, "f14_t1_02_mp3.png", [bundle.tone_mp3], srv=srv)
        img_bin, _ = _render_viewer(ctx, "f14_t1_02_bin.png", [mp3_bin], srv=srv)
    diff = pixel_diff_stats(img_mp3, img_bin, rect=VIEWER_BODY)
    assert diff["diff_fraction"] > 0.10, (
        f"Expected tone.mp3 audio preview to differ from binary hex dump: {diff}"
    )


def _test_f14_t1_03_pdf_rendered_vs_raw_text(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=False)
    pdf_txt = ctx.work_dir / "sample_pdf.txt"
    pdf_txt.write_bytes(bundle.sample_pdf.read_bytes())
    _setup_dark_sidebar(ctx)
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_pdf, _ = _render_viewer(ctx, "f14_t1_03_pdf.png", [bundle.sample_pdf], srv=srv)
        img_txt, _ = _render_viewer(ctx, "f14_t1_03_txt.png", [pdf_txt], srv=srv)
    diff = pixel_diff_stats(img_pdf, img_txt, rect=VIEWER_BODY)
    assert diff["diff_fraction"] > 0.25, (
        f"Expected rendered PDF preview to differ from raw %PDF-1.4 text: {diff}"
    )


def _test_f14_t1_04_png_and_svg_previews(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=False)
    _setup_dark_sidebar(ctx)
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_base, _ = _render_viewer(ctx, "f14_t1_04_base.png", srv=srv)
        img_png, rep_png = _render_viewer(ctx, "f14_t1_04_png.png", [bundle.sample_png], srv=srv)
        img_svg, rep_svg = _render_viewer(ctx, "f14_t1_04_svg.png", [bundle.sample_svg], srv=srv)
    assert rep_png.quads > 50 and rep_svg.quads > 50
    assert pixel_diff_stats(img_base, img_png, rect=VIEWER_BODY)["diff_fraction"] > 0.25
    assert pixel_diff_stats(img_png, img_svg, rect=VIEWER_BODY)["diff_fraction"] > 0.05


def _test_f14_t1_05_animated_gif_preview(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=False)
    _setup_dark_sidebar(ctx)
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_base, _ = _render_viewer(ctx, "f14_t1_05_base.png", srv=srv)
        img_gif, rep_gif = _render_viewer(ctx, "f14_t1_05_gif.png", [bundle.anim_gif], srv=srv)
    assert rep_gif.quads >= 20, f"Expected GIF preview quads >= 20, got {rep_gif.quads}"
    diff = pixel_diff_stats(img_base, img_gif, rect=VIEWER_BODY)
    assert diff["diff_fraction"] > 0.01 and diff["changed_pixels"] > 2000, (
        f"Expected anim.gif pixels rendered in VIEWER_BODY: {diff}"
    )


def _test_f14_t2_01_audio_waveform_and_scrub_bar_geometry(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=False)
    _setup_dark_sidebar(ctx)
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_wav, rep_wav = _render_viewer(ctx, "f14_t2_01.png", [bundle.tone_wav], srv=srv)
    assert rep_wav.quads >= 40, f"Expected audio waveform + scrub bar quads >= 40, got {rep_wav.quads}"
    assert rep_wav.glyphs < 1200, (
        f"Expected audio metadata labels (< 1200 glyphs) rather than hex dump, got {rep_wav.glyphs}"
    )
    assert_not_solid_color(img_wav, min_unique_colors=5, step=2)


def _write_distinct_two_page_pdf(path: Path) -> None:
    """Write a valid 2-page PDF 1.4 with computed xref offsets and a large colored rect on Page 2."""
    objects: list[bytes] = [
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>",
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 6 0 R >>",
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 7 0 R >>",
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
    ]
    p1_stream = b"BT /F1 18 Tf 72 720 Td (Omarex Distinct PDF Page 1) Tj ET"
    p2_stream = (
        b"0.1 0.2 0.8 rg 100 200 400 400 re f\n"
        b"BT /F1 18 Tf 72 720 Td (Omarex Distinct PDF Page 2) Tj ET"
    )
    objects.append(
        f"<< /Length {len(p1_stream)} >>\nstream\n".encode("ascii")
        + p1_stream
        + b"\nendstream"
    )
    objects.append(
        f"<< /Length {len(p2_stream)} >>\nstream\n".encode("ascii")
        + p2_stream
        + b"\nendstream"
    )
    buf = bytearray(b"%PDF-1.4\n")
    offsets: list[int] = [0]
    for idx, body in enumerate(objects, start=1):
        offsets.append(len(buf))
        buf.extend(f"{idx} 0 obj\n".encode("ascii"))
        buf.extend(body)
        buf.extend(b"\nendobj\n")
    xref_offset = len(buf)
    buf.extend(f"xref\n0 {len(objects) + 1}\n".encode("ascii"))
    buf.extend(b"0000000000 65535 f \n")
    for off in offsets[1:]:
        buf.extend(f"{off:010d} 00000 n \n".encode("ascii"))
    buf.extend(
        f"trailer\n<< /Size {len(objects) + 1} /Root 1 0 R >>\n"
        f"startxref\n{xref_offset}\n%%EOF\n".encode("ascii")
    )
    path.write_bytes(bytes(buf))


def _test_f14_t2_02_pdf_page_1_vs_page_2(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=False)
    distinct_pdf = ctx.work_dir / "distinct_pages.pdf"
    _write_distinct_two_page_pdf(distinct_pdf)
    _setup_dark_sidebar(ctx)
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_p1, _ = _render_viewer(ctx, "f14_t2_02_p1.png", [bundle.sample_pdf], srv=srv)
        img_p2, _ = _render_viewer(ctx, "f14_t2_02_p2.png", [f"{bundle.sample_pdf}:2"], srv=srv)
        img_d1, _ = _render_viewer(ctx, "f14_t2_02_d1.png", [f"{distinct_pdf}:1"], srv=srv)
        img_d2, _ = _render_viewer(ctx, "f14_t2_02_d2.png", [f"{distinct_pdf}:2"], srv=srv)
    page_lum = relative_luminance(mean_color_in_rect(img_p1, 950, 140, 400, 500)[:3])
    assert page_lum > 0.45, f"Expected rendered PDF page luminance > 0.45, got {page_lum:.3f}"
    sample_diff = pixel_diff_stats(img_p1, img_p2, rect=VIEWER_BODY)
    assert sample_diff["changed_pixels"] >= 20, (
        f"Expected sample.pdf page 1 vs page 2 to differ (>= 20 px): {sample_diff}"
    )
    distinct_diff = pixel_diff_stats(img_d1, img_d2, rect=VIEWER_BODY)
    assert distinct_diff["diff_fraction"] > 0.02, (
        f"Expected distinct 2-page PDF page 1 vs page 2 diff > 0.02: {distinct_diff}"
    )


def _test_f14_t2_03_corrupt_and_empty_audio_pdf_resilience(ctx: TestContext) -> None:
    _setup_dark_sidebar(ctx)
    files = {
        "empty.pdf": b"",
        "corrupt.pdf": b"%PDF-1.4\nNOT_A_VALID_XREF_GARBAGE\n",
        "empty.wav": b"",
        "corrupt.wav": b"RIFF\x00\x00\x00\x00WAVEBAD",
    }
    paths = []
    for name, data in files.items():
        p = ctx.work_dir / name
        p.write_bytes(data)
        paths.append(p)
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        for p in paths:
            img, _ = _render_viewer(ctx, f"f14_t2_03_{p.name}.png", [p], srv=srv)
            assert_not_solid_color(img, min_unique_colors=3)


def _test_f14_t2_04_svg_jpg_png_pixel_colors(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=False)
    _setup_dark_sidebar(ctx)
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _start_blank_session(srv)
        img_png, _ = _render_viewer(ctx, "f14_t2_04_png.png", [bundle.sample_png], srv=srv)
        img_jpg, _ = _render_viewer(ctx, "f14_t2_04_jpg.png", [bundle.sample_jpg], srv=srv)
        img_svg, _ = _render_viewer(ctx, "f14_t2_04_svg.png", [bundle.sample_svg], srv=srv)
    assert unique_color_count_sample(img_png, step=4) >= 6
    assert pixel_diff_stats(img_png, img_svg, rect=VIEWER_BODY)["diff_fraction"] > 0.05
    assert_not_solid_color(img_jpg, min_unique_colors=4)


def _test_f14_t2_05_tool_missing_path_fallback_safety(ctx: TestContext) -> None:
    bundle = create_fixtures(ctx.work_dir / "fix", generate_mp3=False, init_git=False)
    _setup_dark_sidebar(ctx)
    empty_bin = ctx.work_dir / "empty_bin"
    empty_bin.mkdir(parents=True, exist_ok=True)
    for tag, target in (("pdf", bundle.sample_pdf), ("wav", bundle.tone_wav)):
        img, _ = _render_viewer(
            ctx,
            f"f14_t2_05_{tag}.png",
            [target],
            extra_env={"PATH": str(empty_bin)},
        )
        assert_not_solid_color(img, min_unique_colors=4)


def register_tests(registry: TestRegistry) -> None:
    cases = [
        # F9
        TestCase("F09-T1-01", 1, "F9", "Viewer renders sample.rs offline with >800 glyphs", _test_f09_t1_01_viewer_rust_offline),
        TestCase("F09-T1-02", 1, "F9", "Viewer renders sample.py with multi-color output", _test_f09_t1_02_viewer_python),
        TestCase("F09-T1-03", 1, "F9", "Viewer renders sample.js with multi-color output", _test_f09_t1_03_viewer_javascript),
        TestCase("F09-T1-04", 1, "F9", "Viewer renders sample.json with structured palette", _test_f09_t1_04_viewer_json),
        TestCase("F09-T1-05", 1, "F9", "Multi-token Rust syntax highlighting differs from plain .txt", _test_f09_t1_05_syntax_highlighting_vs_plain_txt),
        TestCase("F09-T2-01", 2, "F9", "syntax-viewer headless scenario renders multi-token palette", _test_f09_t2_01_syntax_viewer_scenario),
        TestCase("F09-T2-02", 2, "F9", "Syntax viewer remaps colors cleanly between dark and --light", _test_f09_t2_02_syntax_theme_dark_vs_light),
        TestCase("F09-T2-03", 2, "F9", "Viewer :1 vs :42 line jump scrolls viewport", _test_f09_t2_03_viewer_line_jump_scroll_diff),
        TestCase("F09-T2-04", 2, "F9", "Unknown file extension falls back gracefully in viewer", _test_f09_t2_04_unknown_extension_fallback),
        TestCase("F09-T2-05", 2, "F9", "Large >160-line Rust file renders and scrolls without truncation", _test_f09_t2_05_large_rust_file_no_truncation),
        # F10
        TestCase("F10-T1-01", 1, "F10", "Local watched Rust file opens cleanly in viewer", _test_f10_t1_01_local_watched_file_open),
        TestCase("F10-T1-02", 1, "F10", "Atomic os.replace updates viewer content on reload", _test_f10_t1_02_atomic_replace_updates_viewer),
        TestCase("F10-T1-03", 1, "F10", "Deleted and recreated watched file recovers cleanly", _test_f10_t1_03_delete_and_recreate_resilience),
        TestCase("F10-T1-04", 1, "F10", "Nonexistent viewer file path handles error without crash", _test_f10_t1_04_nonexistent_path_graceful),
        TestCase("F10-T1-05", 1, "F10", "Symlinked viewer file renders identically to target", _test_f10_t1_05_symlink_viewer_open),
        TestCase("F10-T2-01", 2, "F10", "syntax-viewer handles external file mutation without crash", _test_f10_t2_01_live_reload_syntax_viewer_with_edit),
        TestCase("F10-T2-02", 2, "F10", "syntax-viewer handles atomic sed-style rename re-arm", _test_f10_t2_02_atomic_sed_style_rename_syntax_viewer),
        TestCase("F10-T2-03", 2, "F10", "Rapid 20x consecutive file writes stabilize to final state", _test_f10_t2_03_rapid_consecutive_writes_stabilize),
        TestCase("F10-T2-04", 2, "F10", "Rewritten .rs syntax highlighting differs from .txt", _test_f10_t2_04_syntax_updates_on_code_rewrite),
        TestCase("F10-T2-05", 2, "F10", "Multi-file viewer exits cleanly without watch hang", _test_f10_t2_05_multi_file_clean_exit),
        # F11
        TestCase("F11-T1-01", 1, "F11", "Git fixture repo state validated and opened in viewer", _test_f11_t1_01_git_repo_fixture_opens),
        TestCase("F11-T1-02", 1, "F11", "Modified git-tracked file shows diff markers vs non-git copy", _test_f11_t1_02_modified_tracked_shows_diff_vs_nongit),
        TestCase("F11-T1-03", 1, "F11", "Staged git file shows added diff markers vs non-git copy", _test_f11_t1_03_staged_file_shows_diff_vs_nongit),
        TestCase("F11-T1-04", 1, "F11", "Untracked git file opens cleanly without spurious diff error", _test_f11_t1_04_untracked_file_no_spurious_error),
        TestCase("F11-T1-05", 1, "F11", "Non-git file renders without spurious diff failure", _test_f11_t1_05_nongit_file_no_spurious_diff),
        TestCase("F11-T2-01", 2, "F11", "Clean commit vs unstaged edit differs in viewer diff gutter", _test_f11_t2_01_clean_commit_vs_unstaged_mod_gutter),
        TestCase("F11-T2-02", 2, "F11", "Staged and unstaged files both display git-diff indicators", _test_f11_t2_02_staged_and_unstaged_diff_distinction),
        TestCase("F11-T2-03", 2, "F11", "Diff gutter markers clear after committing modified file", _test_f11_t2_03_diff_clears_after_git_commit),
        TestCase("F11-T2-04", 2, "F11", "Git-diff view renders under both dark and --light themes", _test_f11_t2_04_git_diff_dark_vs_light),
        TestCase("F11-T2-05", 2, "F11", "Large multi-hunk git file preserves diff markers at line 70", _test_f11_t2_05_large_multihunk_diff_scroll),
        # F12
        TestCase("F12-T1-01", 1, "F12", "CLI --headless-viewer path:line jumps cleanly", _test_f12_t1_01_path_line_jump_cli),
        TestCase("F12-T1-02", 1, "F12", "Viewer path:line:col jump and Rex actions contract", _test_f12_t1_02_line_col_jump_and_rex_actions),
        TestCase("F12-T1-03", 1, "F12", "syntax-viewer scenario exercises viewer search/selection mode", _test_f12_t1_03_viewer_find_scenario_contract),
        TestCase("F12-T1-04", 1, "F12", "Comma-separated multi-file viewer opens distinct tabbar", _test_f12_t1_04_multi_file_comma_tabs),
        TestCase("F12-T1-05", 1, "F12", "Viewer line-number gutter renders non-empty line indices", _test_f12_t1_05_line_number_gutter_render),
        TestCase("F12-T2-01", 2, "F12", "Viewer scroll jump sample.rs:1 vs :60 differs in body", _test_f12_t2_01_scroll_jump_line_1_vs_60),
        TestCase("F12-T2-02", 2, "F12", "syntax-viewer search/selection adapts between dark and --light", _test_f12_t2_02_syntax_viewer_dark_vs_light_highlights),
        TestCase("F12-T2-03", 2, "F12", "SEARCH_TARGET_ALPHA jump on sample.py:14 paired with syntax-viewer", _test_f12_t2_03_search_target_jump_and_scenario),
        TestCase("F12-T2-04", 2, "F12", "path:line:col jump and 2x HiDPI syntax-viewer render cleanly", _test_f12_t2_04_path_line_col_hidpi_and_scenario),
        TestCase("F12-T2-05", 2, "F12", "Out-of-range line jump and 0-byte empty file do not panic", _test_f12_t2_05_out_of_range_line_and_empty_file),
        # F13
        TestCase("F13-T1-01", 1, "F13", "Viewer opens sample.md with >400 rendered glyphs", _test_f13_t1_01_markdown_open),
        TestCase("F13-T1-02", 1, "F13", "Markdown headings/formatting differ visually from plain .txt", _test_f13_t1_02_heading_styling_vs_plain_text),
        TestCase("F13-T1-03", 1, "F13", "Markdown GFM pipe table renders structural border quads", _test_f13_t1_03_gfm_pipe_table_quads),
        TestCase("F13-T1-04", 1, "F13", "Markdown fenced Rust block renders multi-token syntax colors", _test_f13_t1_04_fenced_code_block_multitoken_highlight),
        TestCase("F13-T1-05", 1, "F13", "Markdown inline local image emits image quads and pixels", _test_f13_t1_05_inline_local_image_quads),
        TestCase("F13-T2-01", 2, "F13", "markdown-viewer headless scenario renders Markdown 2.0", _test_f13_t2_01_markdown_viewer_scenario),
        TestCase("F13-T2-02", 2, "F13", "Inline sample.png quadrant colors appear in Markdown viewer", _test_f13_t2_02_inline_png_quadrant_pixels),
        TestCase("F13-T2-03", 2, "F13", "Fenced rust block highlights differently from fenced text block", _test_f13_t2_03_fenced_rust_vs_plain_fenced),
        TestCase("F13-T2-04", 2, "F13", "Broken relative image path and malformed link do not crash", _test_f13_t2_04_broken_image_and_malformed_link_resilience),
        TestCase("F13-T2-05", 2, "F13", "Markdown viewer preserves dark vs --light legibility", _test_f13_t2_05_markdown_dark_vs_light),
        # F14
        TestCase("F14-T1-01", 1, "F14", "tone.wav audio preview differs visually from binary hex dump", _test_f14_t1_01_wav_waveform_vs_binary_hex),
        TestCase("F14-T1-02", 1, "F14", "tone.mp3 audio preview differs visually from binary hex dump", _test_f14_t1_02_mp3_waveform_vs_binary_hex),
        TestCase("F14-T1-03", 1, "F14", "sample.pdf rendered preview differs from raw %PDF-1.4 text", _test_f14_t1_03_pdf_rendered_vs_raw_text),
        TestCase("F14-T1-04", 1, "F14", "sample.png and sample.svg render distinct image previews", _test_f14_t1_04_png_and_svg_previews),
        TestCase("F14-T1-05", 1, "F14", "2-frame animated GIF opens cleanly in viewer", _test_f14_t1_05_animated_gif_preview),
        TestCase("F14-T2-01", 2, "F14", "Audio waveform + scrub bar emits >=40 quads without hex dump", _test_f14_t2_01_audio_waveform_and_scrub_bar_geometry),
        TestCase("F14-T2-02", 2, "F14", "PDF viewer renders Page 1 vs Page 2 distinctly with light page", _test_f14_t2_02_pdf_page_1_vs_page_2),
        TestCase("F14-T2-03", 2, "F14", "Empty and corrupt PDF/WAV files render safely without crash", _test_f14_t2_03_corrupt_and_empty_audio_pdf_resilience),
        TestCase("F14-T2-04", 2, "F14", "PNG, JPG, and SVG render accurate multi-color image pixels", _test_f14_t2_04_svg_jpg_png_pixel_colors),
        TestCase("F14-T2-05", 2, "F14", "Empty PATH without pdftoppm/ffmpeg degrades safely without crash", _test_f14_t2_05_tool_missing_path_fallback_safety),
    ]
    for case in cases:
        registry.add(case)
