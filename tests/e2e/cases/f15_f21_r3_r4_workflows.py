"""Tier 1 & Tier 2 opaque-box E2E tests for F15–F21 (R3 Power-User Workflows & R4 Rex Rev 24 / Telemetry)."""

from __future__ import annotations

import base64
import json
import os
import shutil
import subprocess
import tempfile
import time
from pathlib import Path
from typing import Any, Optional

from lib.fixtures import (
    DARK_COLORS_TOML,
    LIGHT_COLORS_TOML,
    create_fixtures,
    install_theme_into_xdg,
    tool_capabilities,
)
from lib.harness import SkipTest, TestCase, TestContext, TestRegistry, parse_headless_report
from lib.png_check import (
    assert_color_near,
    assert_not_solid_color,
    assert_valid_png,
    mean_color_in_rect,
    pixel_diff_stats,
    relative_luminance,
    sample_pixel,
)
from lib.rex_helper import (
    IsolatedRexServer,
    live_socket_present,
    probe_bad_endpoint,
    probe_live_server,
    rex_available,
)


def _require_rex() -> None:
    if not rex_available():
        raise SkipTest("rex binary not installed on PATH")


def _poll_until(predicate, timeout: float = 4.0, interval: float = 0.05) -> Any:
    deadline = time.monotonic() + timeout
    last_val = None
    while time.monotonic() < deadline:
        last_val = predicate()
        if last_val:
            return last_val
        time.sleep(interval)
    return last_val


def _start_events_listener(endpoint: str, session_id: str) -> subprocess.Popen[str]:
    return subprocess.Popen(
        [
            "rex",
            "--autostart=false",
            "-S",
            endpoint,
            "events",
            "-s",
            session_id,
            "--json",
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        errors="replace",
    )


def _stop_events_listener(proc: subprocess.Popen[str]) -> str:
    if proc.poll() is None:
        proc.terminate()
        try:
            out, _ = proc.communicate(timeout=2.0)
            return out
        except subprocess.TimeoutExpired:
            proc.kill()
            out, _ = proc.communicate(timeout=1.0)
            return out
    out, _ = proc.communicate(timeout=1.0)
    return out


# ---------------------------------------------------------------------------
# F15: Vim Copy Mode (Ctrl+Shift+X / EnterCopyMode)
# ---------------------------------------------------------------------------


def _f15_t1_01_copy_mode_scenario_default(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    res, png_path, report = ctx.headless_render(
        out_name="f15_cm.png",
        scenario="copy-mode",
        rust_log="warn",
    )
    assert res.returncode == 0, f"copy-mode scenario failed ({res.returncode}): {res.clean_stderr}"
    assert report is not None, f"Missing headless report: {res.clean_stdout}"
    assert report.width == 1440 and report.height == 900
    assert report.draws > 0 and report.glyphs > 0
    img = assert_valid_png(png_path, expected_width=1440, expected_height=900, min_bytes=4096)
    assert_not_solid_color(img, min_unique_colors=6)


def _f15_t1_02_copy_mode_isolated_rex_scrollback(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = srv.new_session(
            label="cm-sess",
            command=["bash", "--norc", "--noprofile", "-c", "seq 1 120; sleep 5"],
        )
        sid = created["session_id"]
        bid = created["initial_windows"][0]["block_ids"][0]
        _poll_until(
            lambda: "120"
            in srv.api_call(
                "com.superlogical.terminal.format",
                {"session_id": sid, "block_id": bid, "args": {"format": "text"}},
            ).get("content", ""),
            timeout=3.0,
        )
        res, png_path, report = ctx.headless_render(
            out_name="f15_cm_rex.png",
            scenario="copy-mode",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        assert res.returncode == 0, f"copy-mode with Rex failed: {res.clean_stderr}"
        assert report is not None
        assert report.session == "cm-sess", f"Expected session cm-sess, got {report}"
        assert report.windows >= 1 and report.ready_panes >= 1
        assert_valid_png(png_path, expected_width=1440, expected_height=900, min_bytes=4096)


def _f15_t1_03_copy_mode_offline_fallback(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    res, png_path, report = ctx.headless_render(
        out_name="f15_cm_offline.png",
        scenario="copy-mode",
        rex_mode="isolated_offline",
        rust_log="warn",
    )
    assert res.returncode == 0, f"copy-mode offline fallback failed: {res.clean_stderr}"
    assert not res.crashed
    assert report is not None
    img = assert_valid_png(png_path, expected_width=1440, expected_height=900, min_bytes=2048)
    assert_not_solid_color(img, min_unique_colors=4)


def _f15_t1_04_copy_mode_scaled_geometry(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    res, png_path, report = ctx.headless_render(
        out_name="f15_cm_scaled.png",
        width=800,
        height=600,
        scale=2.0,
        scenario="copy-mode",
        rust_log="off",
    )
    assert res.returncode == 0, f"copy-mode scaled render failed: {res.clean_stderr}"
    assert report is not None
    assert report.width == 800 and report.height == 600
    assert abs(report.scale - 2.0) < 1e-3
    assert_valid_png(png_path, expected_width=800, expected_height=600, expected_color_type=6)


def _f15_t1_05_copy_mode_dark_vs_light(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    res_d, png_d, _ = ctx.headless_render(
        out_name="f15_cm_dark.png",
        width=800,
        height=600,
        scenario="copy-mode",
        light=False,
        rust_log="off",
    )
    assert res_d.returncode == 0, f"copy-mode dark failed: {res_d.clean_stderr}"

    res_l, png_l, _ = ctx.headless_render(
        out_name="f15_cm_light.png",
        width=800,
        height=600,
        scenario="copy-mode",
        light=True,
        rust_log="off",
    )
    assert res_l.returncode == 0, f"copy-mode light failed: {res_l.clean_stderr}"

    img_d = assert_valid_png(png_d, expected_width=800, expected_height=600)
    img_l = assert_valid_png(png_l, expected_width=800, expected_height=600)
    stats = pixel_diff_stats(img_d, img_l)
    assert stats["diff_fraction"] > 0.25, f"Expected dark vs --light diff > 0.25: {stats}"
    lum_d = relative_luminance(sample_pixel(img_d, 2, 2)[:3])
    lum_l = relative_luminance(sample_pixel(img_l, 2, 2)[:3])
    assert lum_l > lum_d + 0.30, f"Expected light luminance ({lum_l:.3f}) > dark ({lum_d:.3f}) + 0.30"


def _f15_t2_01_copy_mode_diff_vs_sidebar_dark(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    res_base, png_base, _ = ctx.headless_render(
        out_name="f15_base.png",
        scenario="sidebar-dark",
        rust_log="off",
    )
    assert res_base.returncode == 0, f"sidebar-dark failed: {res_base.clean_stderr}"

    res_cm, png_cm, _ = ctx.headless_render(
        out_name="f15_cm.png",
        scenario="copy-mode",
        rust_log="off",
    )
    assert res_cm.returncode == 0, f"copy-mode failed: {res_cm.clean_stderr}"

    img_base = assert_valid_png(png_base, expected_width=1440, expected_height=900)
    img_cm = assert_valid_png(png_cm, expected_width=1440, expected_height=900)
    stats = pixel_diff_stats(img_base, img_cm)
    assert stats["diff_fraction"] > 0.005, (
        f"Expected copy-mode to visibly differ from sidebar-dark: {stats}"
    )


def _f15_t2_02_copy_mode_populated_scrollback_diff(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    res_off, png_off, _ = ctx.headless_render(
        out_name="f15_off.png",
        scenario="copy-mode",
        rex_mode="isolated_offline",
        rust_log="off",
    )
    assert res_off.returncode == 0, f"copy-mode offline failed: {res_off.clean_stderr}"

    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = srv.new_session(
            label="cm-pop",
            command=[
                "bash",
                "--norc",
                "--noprofile",
                "-c",
                "for i in $(seq 1 150); do echo \"line_$i SEARCH_TARGET_ALPHA\"; done; sleep 5",
            ],
        )
        sid = created["session_id"]
        bid = created["initial_windows"][0]["block_ids"][0]
        _poll_until(
            lambda: "SEARCH_TARGET_ALPHA"
            in srv.api_call(
                "com.superlogical.terminal.format",
                {"session_id": sid, "block_id": bid, "args": {"format": "text"}},
            ).get("content", ""),
            timeout=3.0,
        )
        res_conn, png_conn, rep_conn = ctx.headless_render(
            out_name="f15_conn.png",
            scenario="copy-mode",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="off",
        )
        assert res_conn.returncode == 0, f"copy-mode connected failed: {res_conn.clean_stderr}"
        assert rep_conn is not None and rep_conn.windows >= 1 and rep_conn.ready_panes >= 1

    img_off = assert_valid_png(png_off, expected_width=1440, expected_height=900)
    img_conn = assert_valid_png(png_conn, expected_width=1440, expected_height=900)
    stats = pixel_diff_stats(img_off, img_conn)
    assert stats["diff_fraction"] > 0.01, f"Expected populated scrollback diff > 0.01: {stats}"


def _f15_t2_03_copy_mode_selection_uses_theme_color(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    res_a, png_a, _ = ctx.headless_render(
        out_name="f15_sel_a.png",
        scenario="copy-mode",
        rust_log="off",
    )
    assert res_a.returncode == 0, f"copy-mode theme A failed: {res_a.clean_stderr}"

    modified_toml = DARK_COLORS_TOML.replace('selection = "#45475a"', 'selection = "#ff2a85"')
    ctx.write_omarchy_theme(modified_toml, theme_name="mock-pink-sel")
    res_b, png_b, _ = ctx.headless_render(
        out_name="f15_sel_b.png",
        scenario="copy-mode",
        rust_log="off",
    )
    assert res_b.returncode == 0, f"copy-mode theme B failed: {res_b.clean_stderr}"

    img_a = assert_valid_png(png_a, expected_width=1440, expected_height=900)
    img_b = assert_valid_png(png_b, expected_width=1440, expected_height=900)
    stats = pixel_diff_stats(img_a, img_b)
    assert stats["changed_pixels"] >= 50, (
        f"Expected copy-mode selection color change to affect >= 50 pixels: {stats}"
    )


def _f15_t2_04_copy_mode_narrow_viewport_no_clip_crash(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    res, png_path, report = ctx.headless_render(
        out_name="f15_narrow.png",
        width=480,
        height=320,
        scenario="copy-mode",
        rust_log="warn",
    )
    assert res.returncode == 0, f"copy-mode narrow failed: {res.clean_stderr}"
    assert not res.has_error, f"Unexpected stderr/stdout error: {res.clean_stderr}"
    assert report is not None
    img = assert_valid_png(png_path, expected_width=480, expected_height=320)
    assert_not_solid_color(img, min_unique_colors=4)


def _f15_t2_05_copy_mode_preserves_rex_session(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = srv.new_session(label="cm-clean", command=["sleep", "10"])
        sid = created["session_id"]
        res, _, report = ctx.headless_render(
            out_name="f15_clean.png",
            scenario="copy-mode",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        assert res.returncode == 0, f"copy-mode failed: {res.clean_stderr}"
        assert report is not None and report.windows >= 1 and report.ready_panes >= 1
        sessions = srv.ls_sessions()
        assert any(s.get("session_id") == sid for s in sessions), f"Session lost: {sessions}"
        view = srv.api_call("session.view", {"session_id": sid})
        assert view.get("windows"), f"Expected valid windows in session.view: {view}"


# ---------------------------------------------------------------------------
# F16: Remote-Capable File/Dir-Picker (InsertPath, GoToDirectory)
# ---------------------------------------------------------------------------


def _make_f16_fixture_dir(base: Path) -> Path:
    td = base / "picker_root"
    td.mkdir(parents=True, exist_ok=True)
    (td / "sub_a").mkdir(exist_ok=True)
    (td / ".hidden_dir").mkdir(exist_ok=True)
    (td / "empty_d").mkdir(exist_ok=True)
    (td / "alpha.txt").write_text("alpha\n", encoding="utf-8")
    (td / ".dotfile").write_text("dot\n", encoding="utf-8")
    link_sub = td / "link_sub"
    if not link_sub.exists():
        link_sub.symlink_to(td / "sub_a")
    return td


def _f16_t1_01_list_dir_dirs_only(ctx: TestContext) -> None:
    _require_rex()
    td = _make_f16_fixture_dir(ctx.work_dir)
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = srv.new_session(label="f16-dirs", command=["sleep", "10"], cwd=td)
        sid = created["session_id"]
        bid = created["initial_windows"][0]["block_ids"][0]
        res = srv.api_call(
            "com.superlogical.terminal.list_dir",
            {"session_id": sid, "block_id": bid, "args": {"path": str(td), "files": False}},
        )
        assert res["status"] == "ok", res
        assert res["truncated"] is False, res
        by_name = {e["name"]: e for e in res["entries"]}
        assert "sub_a" in by_name and ".hidden_dir" in by_name
        assert "link_sub" in by_name and by_name["link_sub"].get("symlink") is True
        assert "alpha.txt" not in by_name and ".dotfile" not in by_name


def _f16_t1_02_list_dir_with_files(ctx: TestContext) -> None:
    _require_rex()
    td = _make_f16_fixture_dir(ctx.work_dir)
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = srv.new_session(label="f16-files", command=["sleep", "10"], cwd=td)
        sid = created["session_id"]
        bid = created["initial_windows"][0]["block_ids"][0]
        res = srv.api_call(
            "com.superlogical.terminal.list_dir",
            {"session_id": sid, "block_id": bid, "args": {"path": str(td), "files": True}},
        )
        assert res["status"] == "ok", res
        by_name = {e["name"]: e for e in res["entries"]}
        assert by_name["alpha.txt"].get("file") is True
        assert by_name[".dotfile"].get("file") is True
        assert by_name["sub_a"].get("file") is not True


def _f16_t1_03_list_dir_empty_dir(ctx: TestContext) -> None:
    _require_rex()
    td = _make_f16_fixture_dir(ctx.work_dir)
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = srv.new_session(label="f16-empty", command=["sleep", "10"], cwd=td)
        sid = created["session_id"]
        bid = created["initial_windows"][0]["block_ids"][0]
        res = srv.api_call(
            "com.superlogical.terminal.list_dir",
            {"session_id": sid, "block_id": bid, "args": {"path": str(td / "empty_d"), "files": True}},
        )
        assert res["status"] == "ok", res
        assert res["entries"] == [], res
        assert res["truncated"] is False, res


def _f16_t1_04_list_dir_missing_and_not_dir(ctx: TestContext) -> None:
    _require_rex()
    td = _make_f16_fixture_dir(ctx.work_dir)
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = srv.new_session(label="f16-err", command=["sleep", "10"], cwd=td)
        sid = created["session_id"]
        bid = created["initial_windows"][0]["block_ids"][0]
        missing = srv.api_call(
            "com.superlogical.terminal.list_dir",
            {"session_id": sid, "block_id": bid, "args": {"path": str(td / "no_such")}},
        )
        assert missing["status"] == "missing", missing
        assert missing["entries"] == [], missing

        not_dir = srv.api_call(
            "com.superlogical.terminal.list_dir",
            {"session_id": sid, "block_id": bid, "args": {"path": str(td / "alpha.txt")}},
        )
        assert not_dir["status"] == "not_dir", not_dir
        assert not_dir["entries"] == [], not_dir


def _f16_t1_05_list_dir_limit_and_prefix(ctx: TestContext) -> None:
    _require_rex()
    td = ctx.work_dir / "prefix_dir"
    td.mkdir(parents=True, exist_ok=True)
    for i in range(1, 7):
        (td / f"aa_{i:02d}.txt").write_text("a\n", encoding="utf-8")
        (td / f"bb_{i:02d}.txt").write_text("b\n", encoding="utf-8")

    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = srv.new_session(label="f16-limit", command=["sleep", "10"], cwd=td)
        sid = created["session_id"]
        bid = created["initial_windows"][0]["block_ids"][0]
        limited = srv.api_call(
            "com.superlogical.terminal.list_dir",
            {"session_id": sid, "block_id": bid, "args": {"path": str(td), "files": True, "limit": 4}},
        )
        assert limited["status"] == "ok" and len(limited["entries"]) == 4
        assert limited["truncated"] is True, limited

        prefixed = srv.api_call(
            "com.superlogical.terminal.list_dir",
            {"session_id": sid, "block_id": bid, "args": {"path": str(td), "files": True, "prefix": "AA_"}},
        )
        assert prefixed["status"] == "ok" and prefixed["truncated"] is False
        names = {e["name"] for e in prefixed["entries"]}
        assert names == {f"aa_{i:02d}.txt" for i in range(1, 7)}, names


def _f16_t2_01_list_dir_empty_path_cwd_and_tilde(ctx: TestContext) -> None:
    _require_rex()
    td = _make_f16_fixture_dir(ctx.work_dir)
    sub_a = td / "sub_a"
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = srv.new_session(label="f16-cwd", command=["sleep", "10"], cwd=sub_a)
        sid = created["session_id"]
        bid = created["initial_windows"][0]["block_ids"][0]
        res_empty = srv.api_call(
            "com.superlogical.terminal.list_dir",
            {"session_id": sid, "block_id": bid, "args": {"path": ""}},
        )
        assert res_empty["status"] == "ok", res_empty
        assert Path(res_empty["path"]).resolve() == sub_a.resolve(), res_empty

        res_tilde = srv.api_call(
            "com.superlogical.terminal.list_dir",
            {"session_id": sid, "block_id": bid, "args": {"path": "~"}},
        )
        assert res_tilde["status"] == "ok", res_tilde
        assert res_tilde["path"] == res_tilde["home"], res_tilde


def _f16_t2_02_go_to_directory_sync_and_headless_dir(ctx: TestContext) -> None:
    _require_rex()
    caps = tool_capabilities()
    bundle = create_fixtures(ctx.work_dir / "fx", generate_mp3=False, init_git=caps["git"])
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")

    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = srv.new_session(label="f16-goto", command=["bash", "--norc", "--noprofile"])
        sid = created["session_id"]
        bid = created["initial_windows"][0]["block_ids"][0]
        srv.api_call(
            "com.superlogical.terminal.write",
            {
                "session_id": sid,
                "block_id": bid,
                "args": {"data": f"cd '{bundle.git_repo}' && printf '\\033]7;file://localhost{bundle.git_repo}\\007'\n"},
            },
        )
        pwd_res = _poll_until(
            lambda: (
                r
                if str(bundle.git_repo)
                in str(
                    (
                        r := srv.api_call(
                            "com.superlogical.terminal.pwd",
                            {"session_id": sid, "block_id": bid, "args": {}},
                        )
                    ).get("pwd", "")
                )
                else None
            ),
            timeout=3.0,
        )
        assert pwd_res is not None, "terminal.pwd did not reflect cd target"
        ld = srv.api_call(
            "com.superlogical.terminal.list_dir",
            {"session_id": sid, "block_id": bid, "args": {"path": "", "files": True}},
        )
        assert ld["status"] == "ok" and any(e["name"] == "src" for e in ld["entries"]), ld

        res, png_path, report = ctx.headless_render(
            out_name="f16_goto.png",
            extra_args=["--working-directory", str(bundle.git_repo)],
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        assert res.returncode == 0, res.clean_stderr
        assert report is not None and report.windows >= 1 and report.ready_panes >= 1
        assert_valid_png(png_path, expected_width=1440, expected_height=900, min_bytes=4096)


def _f16_t2_03_insert_path_with_spaces_parity(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    spaced_dir = ctx.work_dir / "dir with spaces"
    spaced_dir.mkdir(parents=True, exist_ok=True)
    target = spaced_dir / "file name.rs"
    target.write_text("fn main() {}\n", encoding="utf-8")

    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = srv.new_session(label="f16-insert", command=["bash", "--norc", "--noprofile"])
        sid = created["session_id"]
        bid = created["initial_windows"][0]["block_ids"][0]

        res_before, png_before, rep_before = ctx.headless_render(
            out_name="f16_insert_before.png",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="off",
        )
        assert res_before.returncode == 0, res_before.clean_stderr
        assert rep_before is not None and rep_before.windows >= 1 and rep_before.ready_panes >= 1
        img_before = assert_valid_png(png_before, expected_width=1440, expected_height=900)

        srv.api_call(
            "com.superlogical.terminal.write",
            {
                "session_id": sid,
                "block_id": bid,
                "args": {"data": f"printf 'INSERTED:%s\\n' '{target}'\n"},
            },
        )
        fmt = _poll_until(
            lambda: (
                r
                if f"INSERTED:{target}"
                in (
                    r := srv.api_call(
                        "com.superlogical.terminal.format",
                        {"session_id": sid, "block_id": bid, "args": {"format": "text", "unwrap": True}},
                    )
                ).get("content", "")
                else None
            ),
            timeout=3.0,
        )
        assert fmt is not None and f"INSERTED:{target}" in fmt.get("content", "")

        res, png_path, report = ctx.headless_render(
            out_name="f16_insert.png",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        assert res.returncode == 0, res.clean_stderr
        assert report is not None and report.windows >= 1 and report.ready_panes >= 1
        img_after = assert_valid_png(png_path, expected_width=1440, expected_height=900)
        assert_not_solid_color(img_after, min_unique_colors=4)
        diff = pixel_diff_stats(img_before, img_after)
        assert diff["diff_fraction"] > 0.001, f"Expected INSERTED:<path> to change rendered pixels: {diff}"


def _f16_t2_04_broken_symlink_and_denied_resilience(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    td = ctx.work_dir / "edge_dir"
    td.mkdir(parents=True, exist_ok=True)
    (td / "dangling").symlink_to("/nonexistent/target/omarex_e2e")
    (td / "valid_sub").mkdir()
    locked = td / "locked_dir"
    locked.mkdir()
    os.chmod(locked, 0o000)
    try:
        with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
            created = srv.new_session(label="f16-edge", command=["sleep", "10"], cwd=td)
            sid = created["session_id"]
            bid = created["initial_windows"][0]["block_ids"][0]
            parent_res = srv.api_call(
                "com.superlogical.terminal.list_dir",
                {"session_id": sid, "block_id": bid, "args": {"path": str(td), "files": True}},
            )
            assert parent_res["status"] == "ok", parent_res
            names = {e["name"] for e in parent_res["entries"]}
            assert "dangling" not in names and "valid_sub" in names, names

            if os.geteuid() != 0:
                denied_res = srv.api_call(
                    "com.superlogical.terminal.list_dir",
                    {"session_id": sid, "block_id": bid, "args": {"path": str(locked)}},
                )
                assert denied_res["status"] == "denied", denied_res

            res, png_path, report = ctx.headless_render(
                out_name="f16_edge.png",
                rex_mode="custom_socket",
                custom_socket=srv.endpoint,
                rust_log="warn",
            )
            assert res.returncode == 0, res.clean_stderr
            assert report is not None and report.windows >= 1 and report.ready_panes >= 1
            assert_valid_png(png_path, expected_width=1440, expected_height=900)
    finally:
        os.chmod(locked, 0o755)


def _f16_t2_05_large_list_dir_and_settings_render(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    ctx.write_settings(
        {
            "sidebar_behavior": "always",
            "density": "comfortable",
            "pane_header_mode": "always",
            "font_size_pt": 11.0,
        }
    )
    big_dir = ctx.work_dir / "big_300"
    big_dir.mkdir(parents=True, exist_ok=True)
    for i in range(300):
        (big_dir / f"item_{i:03d}.txt").write_text("x\n", encoding="utf-8")

    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = srv.new_session(label="f16-big", command=["sleep", "10"], cwd=big_dir)
        sid = created["session_id"]
        bid = created["initial_windows"][0]["block_ids"][0]
        ld = srv.api_call(
            "com.superlogical.terminal.list_dir",
            {"session_id": sid, "block_id": bid, "args": {"path": str(big_dir), "files": True, "limit": 500}},
        )
        assert ld["status"] == "ok" and ld["truncated"] is False
        assert len(ld["entries"]) >= 300, len(ld["entries"])

        res, png_path, report = ctx.headless_render(
            out_name="f16_big.png",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        assert res.returncode == 0, res.clean_stderr
        assert not res.has_error and not res.has_warning, f"Warnings/errors: {res.clean_stdout} {res.clean_stderr}"
        assert report is not None and report.windows >= 1 and report.ready_panes >= 1
        assert_valid_png(png_path, expected_width=1440, expected_height=900)


# ---------------------------------------------------------------------------
# F17: Live-Preview Theme-Switcher (ChangeTheme)
# ---------------------------------------------------------------------------


def _f17_t1_01_theme_switcher_scenario_renders(ctx: TestContext) -> None:
    install_theme_into_xdg(ctx.xdg_config, ctx.xdg_state, theme="dark")
    res, png_path, report = ctx.headless_render(
        out_name="f17_ts.png",
        scenario="theme-switcher",
        rust_log="warn",
    )
    assert res.returncode == 0, f"theme-switcher scenario failed: {res.clean_stderr}"
    assert report is not None
    assert report.width == 1440 and report.height == 900 and report.glyphs > 0
    img = assert_valid_png(png_path, expected_width=1440, expected_height=900, min_bytes=4096)
    assert_not_solid_color(img, min_unique_colors=6)


def _f17_t1_02_xdg_dark_vs_light_colors_toml(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    res_d, png_d, _ = ctx.headless_render(out_name="f17_d.png", width=640, height=480, rust_log="off")
    assert res_d.returncode == 0

    ctx.write_omarchy_theme(LIGHT_COLORS_TOML, theme_name="mock-light")
    res_l, png_l, _ = ctx.headless_render(out_name="f17_l.png", width=640, height=480, rust_log="off")
    assert res_l.returncode == 0

    img_d = assert_valid_png(png_d, expected_width=640, expected_height=480)
    img_l = assert_valid_png(png_l, expected_width=640, expected_height=480)
    assert_color_near(sample_pixel(img_d, 2, 2), (20, 9, 36, 255), tolerance=10, label="dark corner")
    lum_l = relative_luminance(sample_pixel(img_l, 2, 2)[:3])
    assert lum_l > 0.55, f"Expected light corner luminance > 0.55, got {lum_l:.3f}"
    stats = pixel_diff_stats(img_d, img_l)
    assert stats["diff_fraction"] > 0.25, stats


def _f17_t1_03_theme_discovery_with_broken_dir(ctx: TestContext) -> None:
    install_theme_into_xdg(ctx.xdg_config, ctx.xdg_state, theme="dark", use_symlink=True)
    res, png_path, report = ctx.headless_render(
        out_name="f17_broken_catalog.png",
        scenario="theme-switcher",
        rust_log="warn",
    )
    assert res.returncode == 0, f"theme-switcher failed with broken-no-colors present: {res.clean_stderr}"
    assert not res.crashed and report is not None
    assert_valid_png(png_path, expected_width=1440, expected_height=900)


def _f17_t1_04_missing_colors_toml_osaka_jade_pattern(ctx: TestContext) -> None:
    themes_root = ctx.xdg_config / "omarchy" / "themes"
    themes_root.mkdir(parents=True, exist_ok=True)
    host_osaka = Path("/usr/share/omarchy/themes/osaka-jade")
    host_cat = Path("/usr/share/omarchy/themes/catppuccin")

    broken_dir = themes_root / "osaka-jade"
    if host_osaka.is_dir() and not (host_osaka / "colors.toml").exists():
        broken_dir.symlink_to(host_osaka)
    else:
        broken_dir.mkdir(parents=True, exist_ok=True)
        (broken_dir / "README.md").write_text("No colors.toml\n", encoding="utf-8")

    cat_dir = themes_root / "catppuccin"
    if host_cat.is_dir() and (host_cat / "colors.toml").is_file():
        cat_dir.symlink_to(host_cat)
    else:
        cat_dir.mkdir(parents=True, exist_ok=True)
        (cat_dir / "colors.toml").write_text(DARK_COLORS_TOML, encoding="utf-8")

    cur = ctx.xdg_state / "omarchy" / "current"
    cur.mkdir(parents=True, exist_ok=True)
    (cur / "theme.name").write_text("osaka-jade\n", encoding="utf-8")
    (cur / "theme").symlink_to(broken_dir)

    res, png_path, _ = ctx.headless_render(
        out_name="f17_osaka.png",
        width=480,
        height=320,
        rust_log="warn",
    )
    assert res.returncode == 0, f"omarex failed on osaka-jade missing colors.toml: {res.clean_stderr}"
    assert not res.crashed
    assert_valid_png(png_path, expected_width=480, expected_height=320)


def _f17_t1_05_light_flag_overrides_dark_xdg(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    res, png_path, _ = ctx.headless_render(
        out_name="f17_light_override.png",
        width=640,
        height=480,
        light=True,
        rust_log="off",
    )
    assert res.returncode == 0
    img = assert_valid_png(png_path, expected_width=640, expected_height=480)
    lum = relative_luminance(sample_pixel(img, 2, 2)[:3])
    assert lum > 0.50, f"Expected --light override corner luminance > 0.50, got {lum:.3f}"


def _f17_t2_01_theme_switcher_diff_vs_sidebar_dark(ctx: TestContext) -> None:
    install_theme_into_xdg(ctx.xdg_config, ctx.xdg_state, theme="dark")
    res_sb, png_sb, _ = ctx.headless_render(out_name="f17_sb.png", scenario="sidebar-dark", rust_log="off")
    assert res_sb.returncode == 0, res_sb.clean_stderr
    res_ts, png_ts, _ = ctx.headless_render(out_name="f17_ts.png", scenario="theme-switcher", rust_log="off")
    assert res_ts.returncode == 0, res_ts.clean_stderr

    img_sb = assert_valid_png(png_sb, expected_width=1440, expected_height=900)
    img_ts = assert_valid_png(png_ts, expected_width=1440, expected_height=900)
    full_stats = pixel_diff_stats(img_sb, img_ts)
    assert full_stats["diff_fraction"] > 0.03, full_stats
    modal_stats = pixel_diff_stats(img_sb, img_ts, rect=(400, 140, 640, 420))
    assert modal_stats["diff_fraction"] > 0.10, modal_stats


def _f17_t2_02_theme_switcher_lists_multiple_custom_themes(ctx: TestContext) -> None:
    install_theme_into_xdg(ctx.xdg_config, ctx.xdg_state, theme="dark")
    themes_root = ctx.xdg_config / "omarchy" / "themes"
    for idx, name in enumerate(("t-alpha", "t-beta", "t-gamma", "t-delta")):
        tdir = themes_root / name
        tdir.mkdir(parents=True, exist_ok=True)
        accent = f"#{30 + idx * 40:02x}a0f0"
        (tdir / "colors.toml").write_text(
            DARK_COLORS_TOML.replace("#89b4fa", accent),
            encoding="utf-8",
        )
    res, png_path, report = ctx.headless_render(
        out_name="f17_multi_themes.png",
        scenario="theme-switcher",
        rust_log="warn",
    )
    assert res.returncode == 0, res.clean_stderr
    assert report is not None and report.glyphs > 20
    img = assert_valid_png(png_path, expected_width=1440, expected_height=900)
    assert_not_solid_color(img, min_unique_colors=8)


def _f17_t2_03_atomic_symlink_swap_updates_render(ctx: TestContext) -> None:
    theme_link = install_theme_into_xdg(ctx.xdg_config, ctx.xdg_state, theme="dark", use_symlink=True)
    res_d, png_d, _ = ctx.headless_render(out_name="f17_swap_d.png", width=640, height=480, rust_log="off")
    assert res_d.returncode == 0

    light_dir = ctx.xdg_config / "omarchy" / "themes" / "mock-light"
    tmp_link = theme_link.parent / "theme.tmp"
    if tmp_link.is_symlink() or tmp_link.exists():
        tmp_link.unlink()
    tmp_link.symlink_to(light_dir)
    tmp_link.replace(theme_link)
    (theme_link.parent / "theme.name").write_text("mock-light\n", encoding="utf-8")

    res_l, png_l, _ = ctx.headless_render(out_name="f17_swap_l.png", width=640, height=480, rust_log="off")
    assert res_l.returncode == 0

    img_d = assert_valid_png(png_d, expected_width=640, expected_height=480)
    img_l = assert_valid_png(png_l, expected_width=640, expected_height=480)
    assert_color_near(sample_pixel(img_d, 2, 2), (20, 9, 36, 255), tolerance=10, label="swap dark")
    assert relative_luminance(sample_pixel(img_l, 2, 2)[:3]) > 0.55
    assert pixel_diff_stats(img_d, img_l)["diff_fraction"] > 0.25


def _f17_t2_04_theme_switcher_modal_polarity_dark_vs_light(ctx: TestContext) -> None:
    install_theme_into_xdg(ctx.xdg_config, ctx.xdg_state, theme="dark")
    res_d, png_d, _ = ctx.headless_render(
        out_name="f17_mod_d.png",
        scenario="theme-switcher",
        light=False,
        rust_log="off",
    )
    assert res_d.returncode == 0, res_d.clean_stderr
    res_l, png_l, _ = ctx.headless_render(
        out_name="f17_mod_l.png",
        scenario="theme-switcher",
        light=True,
        rust_log="off",
    )
    assert res_l.returncode == 0, res_l.clean_stderr

    img_d = assert_valid_png(png_d, expected_width=1440, expected_height=900)
    img_l = assert_valid_png(png_l, expected_width=1440, expected_height=900)
    stats = pixel_diff_stats(img_d, img_l)
    assert stats["diff_fraction"] > 0.25, stats
    mean_d = mean_color_in_rect(img_d, 400, 180, 640, 480)
    mean_l = mean_color_in_rect(img_l, 400, 180, 640, 480)
    assert relative_luminance(mean_l[:3]) > relative_luminance(mean_d[:3]) + 0.25


def _f17_t2_05_theme_applied_to_isolated_rex_pane(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        srv.new_session(label="theme-pane", command=["sleep", "10"])
        res, png_path, report = ctx.headless_render(
            out_name="f17_rex_pane.png",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        assert res.returncode == 0, res.clean_stderr
        assert report is not None and report.windows >= 1 and report.ready_panes >= 1
        img = assert_valid_png(png_path, expected_width=1440, expected_height=900)
        assert_color_near(sample_pixel(img, 2, 2), (20, 9, 36, 255), tolerance=10, label="rex dark corner")


# ---------------------------------------------------------------------------
# F18: OSC 52 Remote Clipboard (clipboard_written / clipboard_read)
# ---------------------------------------------------------------------------


def _emit_osc52_and_wait(
    srv: IsolatedRexServer,
    label: str,
    payload: str,
) -> tuple[str, str, dict[str, Any]]:
    b64 = base64.b64encode(payload.encode("utf-8")).decode("ascii")
    created = srv.new_session(
        label=label,
        command=["bash", "--norc", "--noprofile", "-c", f"printf '\\033]52;c;{b64}\\007'; sleep 8"],
    )
    sid = created["session_id"]
    bid = created["initial_windows"][0]["block_ids"][0]

    def _try_read() -> Optional[dict[str, Any]]:
        try:
            return srv.api_call(
                "com.superlogical.terminal.clipboard_read",
                {"session_id": sid, "block_id": bid, "args": {}},
            )
        except RuntimeError:
            return None

    res = _poll_until(_try_read, timeout=3.0)
    assert res is not None, f"clipboard_read never became available for {label}"
    return sid, bid, res


def _f18_t1_01_osc52_ascii_roundtrip(ctx: TestContext) -> None:
    _require_rex()
    payload = "omarex-osc52-ascii-12345"
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _, _, res = _emit_osc52_and_wait(srv, "osc52-ascii", payload)
        assert res["data"] == payload, res
        assert res["total"] == len(payload), res
        assert res["eof"] is True, res
        assert res["id"] >= 1, res


def _f18_t1_02_osc52_utf8_roundtrip(ctx: TestContext) -> None:
    _require_rex()
    payload = "Grüße-omarex-🚀-日本語"
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        _, _, res = _emit_osc52_and_wait(srv, "osc52-utf8", payload)
        assert res["data"] == payload, res
        assert res["total"] == len(payload.encode("utf-8")), res
        assert res["eof"] is True, res


def _f18_t1_03_clipboard_read_id_lookup_and_missing(ctx: TestContext) -> None:
    _require_rex()
    payload = "osc52-id-check"
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        sid, bid, res = _emit_osc52_and_wait(srv, "osc52-id", payload)
        cid = res["id"]
        by_id = srv.api_call(
            "com.superlogical.terminal.clipboard_read",
            {"session_id": sid, "block_id": bid, "args": {"id": cid}},
        )
        assert by_id["data"] == payload, by_id
        try:
            srv.api_call(
                "com.superlogical.terminal.clipboard_read",
                {"session_id": sid, "block_id": bid, "args": {"id": 999999}},
            )
        except RuntimeError as exc:
            assert "not_found" in str(exc), str(exc)
        else:
            raise AssertionError("Expected not_found error for clipboard_read id=999999")


def _f18_t1_04_headless_clipboard_disabled_with_osc52(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    ctx.write_settings({"warm_session_cache_limit": 2})
    payload = "disabled-clip-test"
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        sid, bid, before = _emit_osc52_and_wait(srv, "osc52-dis", payload)
        assert before["data"] == payload, before
        out_png = ctx.out_dir / "f18_dis.png"
        res = ctx.run_omarex(
            ["--headless-render", str(out_png)],
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            extra_env={"WAYLAND_DISPLAY": ""},
            rust_log="warn",
        )
        assert res.returncode == 0, res.clean_stderr
        assert not res.crashed
        report = parse_headless_report(res.stdout)
        assert report.windows >= 1 and report.ready_panes >= 1
        assert_valid_png(out_png, expected_width=1440, expected_height=900)
        after = srv.api_call(
            "com.superlogical.terminal.clipboard_read",
            {"session_id": sid, "block_id": bid, "args": {"id": before["id"]}},
        )
        assert after["data"] == payload and after["id"] == before["id"], after


def _f18_t1_05_clipboard_read_empty_block_not_found(ctx: TestContext) -> None:
    _require_rex()
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = srv.new_session(label="osc52-none", command=["sleep", "10"])
        sid = created["session_id"]
        bid = created["initial_windows"][0]["block_ids"][0]
        try:
            srv.api_call(
                "com.superlogical.terminal.clipboard_read",
                {"session_id": sid, "block_id": bid, "args": {}},
            )
        except RuntimeError as exc:
            assert "not_found" in str(exc), str(exc)
        else:
            raise AssertionError("Expected not_found on fresh terminal block without OSC 52")
        status = srv.api_call("server.status")
        assert status["api_revision"] == 24


def _f18_t2_01_chunked_clipboard_read_large_utf8(ctx: TestContext) -> None:
    _require_rex()
    payload = "omx-äöü-" * 800
    expected_bytes = len(payload.encode("utf-8"))
    assert expected_bytes > 6000
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        sid, bid, first = _emit_osc52_and_wait(srv, "osc52-big", payload)
        cid = first["id"]
        assert first["total"] == expected_bytes, first

        chunks: list[str] = []
        offset = 0
        loops = 0
        while True:
            part = srv.api_call(
                "com.superlogical.terminal.clipboard_read",
                {
                    "session_id": sid,
                    "block_id": bid,
                    "args": {"id": cid, "offset": offset, "max_bytes": 1024},
                },
            )
            chunks.append(part["data"])
            offset += len(part["data"].encode("utf-8"))
            loops += 1
            if part["eof"]:
                break
            assert loops < 64, "Too many chunked clipboard_read iterations"

        assert loops >= 6, f"Expected >= 6 chunks, got {loops}"
        assert "".join(chunks) == payload


def _f18_t2_02_clipboard_written_event_and_omarex_render(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    payload = "live-event-osc52-payload"
    b64 = base64.b64encode(payload.encode("utf-8")).decode("ascii")

    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = srv.new_session(label="osc52-ev", command=["bash", "--norc", "--noprofile"])
        sid = created["session_id"]
        bid = created["initial_windows"][0]["block_ids"][0]
        ev_proc = _start_events_listener(srv.endpoint, sid)
        try:
            time.sleep(0.25)
            srv.api_call(
                "com.superlogical.terminal.write",
                {"session_id": sid, "block_id": bid, "args": {"data": f"printf '\\033]52;c;{b64}\\007'\n"}},
            )
            def _try_clip_read():
                try:
                    return srv.api_call(
                        "com.superlogical.terminal.clipboard_read",
                        {"session_id": sid, "block_id": bid, "args": {}},
                    )
                except RuntimeError:
                    return None

            _poll_until(_try_clip_read, timeout=2.5)
            time.sleep(0.25)
        finally:
            ev_out = _stop_events_listener(ev_proc)

        assert "clipboard_written" in ev_out, f"Expected clipboard_written in events: {ev_out!r}"
        res, png_path, report = ctx.headless_render(
            out_name="f18_ev.png",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        assert res.returncode == 0, res.clean_stderr
        assert report is not None and report.windows >= 1 and report.ready_panes >= 1
        assert_valid_png(png_path, expected_width=1440, expected_height=900)


def _f18_t2_03_rapid_osc52_retains_latest(ctx: TestContext) -> None:
    _require_rex()
    b1 = base64.b64encode(b"copy-1").decode("ascii")
    b2 = base64.b64encode(b"copy-2").decode("ascii")
    b3 = base64.b64encode(b"copy-3").decode("ascii")
    cmd = f"printf '\\033]52;c;{b1}\\007\\033]52;c;{b2}\\007\\033]52;c;{b3}\\007'; sleep 8"
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = srv.new_session(label="osc52-seq", command=["bash", "--norc", "--noprofile", "-c", cmd])
        sid = created["session_id"]
        bid = created["initial_windows"][0]["block_ids"][0]
        def _poll_copy3():
            try:
                r = srv.api_call(
                    "com.superlogical.terminal.clipboard_read",
                    {"session_id": sid, "block_id": bid, "args": {"id": 0}},
                )
            except RuntimeError:
                return None
            return r if r.get("data") == "copy-3" else None

        latest = _poll_until(_poll_copy3, timeout=3.0)
        assert latest is not None and latest["data"] == "copy-3" and latest["eof"] is True


def _f18_t2_04_malformed_and_empty_osc52_resilience(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        srv.new_session(
            label="osc52-bad",
            command=[
                "bash",
                "--norc",
                "--noprofile",
                "-c",
                "printf '\\033]52;c;!!!bad_b64!!!\\007\\033]52;c;\\007READY\\n'; sleep 8",
            ],
        )
        time.sleep(0.3)
        res, png_path, report = ctx.headless_render(
            out_name="f18_bad.png",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        assert res.returncode == 0, res.clean_stderr
        assert not res.crashed
        assert report is not None and report.windows >= 1 and report.ready_panes >= 1
        assert_valid_png(png_path, expected_width=1440, expected_height=900)


def _f18_t2_05_clipboard_enabled_without_wayland_display(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    payload = "no-wayland-display-safe"
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        sid, bid, before = _emit_osc52_and_wait(srv, "osc52-noway", payload)
        assert before["data"] == payload, before
        out_png = ctx.out_dir / "f18_noway.png"
        res = ctx.run_omarex(
            ["--headless-render", str(out_png)],
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            extra_env={"WAYLAND_DISPLAY": "", "DISPLAY": ""},
            rust_log="warn",
        )
        assert res.returncode == 0, res.clean_stderr
        assert not res.crashed
        report = parse_headless_report(res.stdout)
        assert report.windows >= 1 and report.ready_panes >= 1
        assert_valid_png(out_png, expected_width=1440, expected_height=900)
        after = srv.api_call(
            "com.superlogical.terminal.clipboard_read",
            {"session_id": sid, "block_id": bid, "args": {"id": before["id"]}},
        )
        assert after["data"] == payload and after["id"] == before["id"], after


# ---------------------------------------------------------------------------
# F19: Live Listening Ports & 1-Click Port-Forwarding (● :<port>)
# ---------------------------------------------------------------------------


def _f19_t1_01_ports_pill_scenario_renders(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    res, png_path, report = ctx.headless_render(
        out_name="f19_ports_pill.png",
        width=1280,
        height=720,
        scenario="ports-pill",
        rust_log="warn",
    )
    assert res.returncode == 0, f"ports-pill scenario failed: {res.clean_stderr}"
    assert report is not None and report.draws > 0 and report.glyphs > 0
    img = assert_valid_png(png_path, expected_width=1280, expected_height=720, min_bytes=4096)
    assert_not_solid_color(img, min_unique_colors=6)


def _f19_t1_02_ports_rpc_empty_on_idle_pane(ctx: TestContext) -> None:
    _require_rex()
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = srv.new_session(label="f19-idle", command=["bash", "--norc", "--noprofile", "-c", "sleep 8"])
        sid = created["session_id"]
        bid = created["initial_windows"][0]["block_ids"][0]
        res = srv.api_call("com.superlogical.terminal.ports", {"session_id": sid, "block_id": bid, "args": {}})
        assert res["status"] == "ok", res
        assert res["ports"] == [], res
        assert isinstance(res["processes"], list), res


def _f19_t1_03_tunnel_info_dial_and_listen_flavors(ctx: TestContext) -> None:
    _require_rex()
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = srv.new_session(label="f19-tun", command=["sleep", "10"])
        sid = created["session_id"]

        dial_blk = srv.api_call(
            "session.new_block",
            {
                "session_id": sid,
                "flavor": "com.superlogical.tunnel.dial",
                "label": "dial-test",
                "options": {"host": "127.0.0.1", "port": 18080, "client_address": "127.0.0.1:28080"},
            },
        )
        dial_info = srv.api_call(
            "com.superlogical.tunnel.info",
            {"session_id": sid, "block_id": dial_blk["block_id"], "args": {}},
        )
        assert dial_info["flavor"] == "dial", dial_info
        assert dial_info["target"] == "127.0.0.1:18080", dial_info
        assert dial_info["connections"] == 0, dial_info

        listen_blk = srv.api_call(
            "session.new_block",
            {
                "session_id": sid,
                "flavor": "com.superlogical.tunnel.listen",
                "label": "listen-test",
                "options": {"port": 0, "client_target": "127.0.0.1:9000"},
            },
        )
        listen_info = srv.api_call(
            "com.superlogical.tunnel.info",
            {"session_id": sid, "block_id": listen_blk["block_id"], "args": {}},
        )
        assert listen_info["flavor"] == "listen", listen_info
        assert listen_info["address"].startswith("127.0.0.1:"), listen_info
        assert int(listen_info["address"].split(":")[1]) > 0, listen_info
        assert listen_info["connections"] == 0, listen_info


def _f19_t1_04_live_tcp_listener_detected_by_ports_rpc(ctx: TestContext) -> None:
    _require_rex()
    port_file = ctx.work_dir / "bound_port.txt"
    py_script = (
        "import socket, time; "
        "s = socket.socket(); "
        "s.bind(('127.0.0.1', 0)); "
        "s.listen(1); "
        f"open({str(port_file)!r}, 'w').write(str(s.getsockname()[1])); "
        "time.sleep(10)"
    )
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = srv.new_session(label="f19-tcp", command=["python3", "-c", py_script])
        sid = created["session_id"]
        bid = created["initial_windows"][0]["block_ids"][0]
        port_str = _poll_until(lambda: port_file.read_text().strip() if port_file.is_file() else "", timeout=3.0)
        assert port_str and port_str.isdigit(), f"TCP listener did not write port file: {port_str!r}"
        bound_port = int(port_str)

        def _check_bound_port():
            r = srv.api_call(
                "com.superlogical.terminal.ports",
                {"session_id": sid, "block_id": bid, "args": {}},
            )
            return r if any(p.get("port") == bound_port for p in r.get("ports", [])) else None

        ports_res = _poll_until(_check_bound_port, timeout=4.0)
        assert ports_res is not None, f"Port {bound_port} not reported by terminal.ports"
        entry = [p for p in ports_res["ports"] if p["port"] == bound_port][0]
        assert entry["address"] == "127.0.0.1", entry
        assert entry["protocol"] == "tcp", entry
        pids = {pr["pid"] for pr in ports_res["processes"]}
        assert entry["pid"] in pids, f"Listener pid {entry.get('pid')} missing in processes {pids}"


def _f19_t1_05_bad_socket_autostart_false_exit_4(ctx: TestContext) -> None:
    _require_rex()
    res = probe_bad_endpoint(ctx.xdg_runtime / "no-such-rex.sock")
    assert res.returncode == 4, f"Expected exit 4, got {res.returncode}: {res.stderr!r}"
    assert "autostart is disabled" in res.stderr, res.stderr


def _f19_t2_01_ports_pill_differs_from_baseline_header(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    res_base, png_base, _ = ctx.headless_render(
        out_name="f19_base.png",
        width=1280,
        height=720,
        scenario="sidebar-dark",
        rust_log="off",
    )
    assert res_base.returncode == 0, res_base.clean_stderr
    res_ports, png_ports, _ = ctx.headless_render(
        out_name="f19_ports.png",
        width=1280,
        height=720,
        scenario="ports-pill",
        rust_log="off",
    )
    assert res_ports.returncode == 0, res_ports.clean_stderr

    img_base = assert_valid_png(png_base, expected_width=1280, expected_height=720)
    img_ports = assert_valid_png(png_ports, expected_width=1280, expected_height=720)
    full = pixel_diff_stats(img_base, img_ports)
    assert full["diff_fraction"] > 0.002, full
    hdr = pixel_diff_stats(img_base, img_ports, rect=(0, 0, 1280, 80))
    assert hdr["changed_pixels"] > 20, hdr


def _f19_t2_02_ports_changed_event_and_omarex_render(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    port_file = ctx.work_dir / "ev_port.txt"
    py_script = (
        "import socket, time; "
        "s = socket.socket(); "
        "s.bind(('127.0.0.1', 0)); "
        "s.listen(1); "
        f"open({str(port_file)!r}, 'w').write(str(s.getsockname()[1])); "
        "time.sleep(10)"
    )
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = srv.new_session(label="f19-ev", command=["bash", "--norc", "--noprofile"])
        sid = created["session_id"]
        bid = created["initial_windows"][0]["block_ids"][0]
        ev_proc = _start_events_listener(srv.endpoint, sid)
        try:
            time.sleep(0.25)
            srv.api_call(
                "com.superlogical.terminal.write",
                {
                    "session_id": sid,
                    "block_id": bid,
                    "args": {"data": f"python3 -c {json.dumps(py_script)} &\n"},
                },
            )
            port_str = _poll_until(lambda: port_file.read_text().strip() if port_file.is_file() else "", timeout=3.0)
            assert port_str and port_str.isdigit(), "Listener did not start"
            bound_port = int(port_str)
            def _check_ev_port():
                r = srv.api_call(
                    "com.superlogical.terminal.ports",
                    {"session_id": sid, "block_id": bid, "args": {}},
                )
                return r if any(p.get("port") == bound_port for p in r.get("ports", [])) else None

            ports_res = _poll_until(_check_ev_port, timeout=4.0)
            assert ports_res is not None, f"Port {bound_port} not in terminal.ports"
            time.sleep(0.3)
        finally:
            ev_out = _stop_events_listener(ev_proc)

        assert "ports_changed" in ev_out, f"Expected ports_changed event, got: {ev_out!r}"
        res, png_path, report = ctx.headless_render(
            out_name="f19_ev_render.png",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        assert res.returncode == 0, res.clean_stderr
        assert report is not None and report.windows >= 1 and report.ready_panes >= 1
        assert_valid_png(png_path, expected_width=1440, expected_height=900)


def _f19_t2_03_tunnel_validation_and_close_lifecycle(ctx: TestContext) -> None:
    _require_rex()
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = srv.new_session(label="f19-tun-life", command=["sleep", "10"])
        sid = created["session_id"]

        try:
            srv.api_call(
                "session.new_block",
                {
                    "session_id": sid,
                    "flavor": "com.superlogical.tunnel.dial",
                    "options": {"target": "127.0.0.1:80"},
                },
            )
        except RuntimeError as exc:
            assert "unknown field" in str(exc) or "bad_request" in str(exc) or "500" in str(exc), str(exc)
        else:
            raise AssertionError("Expected invalid dial options {'target': ...} to fail")

        listen_blk = srv.api_call(
            "session.new_block",
            {
                "session_id": sid,
                "flavor": "com.superlogical.tunnel.listen",
                "options": {"port": 0},
            },
        )
        bid = listen_blk["block_id"]
        info = srv.api_call("com.superlogical.tunnel.info", {"session_id": sid, "block_id": bid, "args": {}})
        assert info["flavor"] == "listen" and int(info["address"].split(":")[1]) > 0

        srv.api_call("block.close", {"session_id": sid, "block_id": bid})
        try:
            srv.api_call("com.superlogical.tunnel.info", {"session_id": sid, "block_id": bid, "args": {}})
        except RuntimeError as exc:
            assert "not_found" in str(exc), str(exc)
        else:
            raise AssertionError("Expected tunnel.info after block.close to fail with not_found")


def _f19_t2_04_multiple_ports_sorted_deterministically(ctx: TestContext) -> None:
    _require_rex()
    ports_file = ctx.work_dir / "two_ports.txt"
    py_script = (
        "import socket, time; "
        "s1 = socket.socket(); s1.bind(('127.0.0.1', 0)); s1.listen(1); "
        "s2 = socket.socket(); s2.bind(('127.0.0.1', 0)); s2.listen(1); "
        f"open({str(ports_file)!r}, 'w').write(f'{{s1.getsockname()[1]}},{{s2.getsockname()[1]}}'); "
        "time.sleep(10)"
    )
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = srv.new_session(label="f19-multi-p", command=["python3", "-c", py_script])
        sid = created["session_id"]
        bid = created["initial_windows"][0]["block_ids"][0]
        raw = _poll_until(lambda: ports_file.read_text().strip() if ports_file.is_file() else "", timeout=3.0)
        assert raw and "," in raw
        expected = {int(x) for x in raw.split(",")}

        def _check_multi_ports():
            r = srv.api_call(
                "com.superlogical.terminal.ports",
                {"session_id": sid, "block_id": bid, "args": {}},
            )
            seen = {p.get("port") for p in r.get("ports", [])}
            return r if expected.issubset(seen) else None

        ports_res = _poll_until(_check_multi_ports, timeout=4.0)
        assert ports_res is not None
        found_ports = [p["port"] for p in ports_res["ports"] if p["port"] in expected]
        assert found_ports == sorted(found_ports), f"Ports not sorted ascending: {found_ports}"
        for p in ports_res["ports"]:
            if p["port"] in expected:
                assert p["protocol"] == "tcp" and "T" in p["first_seen_at"], p


def _f19_t2_05_closing_listener_removes_port(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    port_file = ctx.work_dir / "ephemeral_port.txt"
    stop_file = ctx.work_dir / "stop_listener.flag"
    py_script = "\n".join(
        [
            "import pathlib, socket, time",
            "s = socket.socket()",
            "s.bind(('127.0.0.1', 0))",
            "s.listen(1)",
            f"pathlib.Path({str(port_file)!r}).write_text(str(s.getsockname()[1]))",
            f"stop = pathlib.Path({str(stop_file)!r})",
            "deadline = time.monotonic() + 15.0",
            "while not stop.exists() and time.monotonic() < deadline:",
            "    time.sleep(0.05)",
            "s.close()",
            "time.sleep(8)",
        ]
    )
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = srv.new_session(label="f19-close-p", command=["python3", "-c", py_script])
        sid = created["session_id"]
        bid = created["initial_windows"][0]["block_ids"][0]
        try:
            port_str = _poll_until(lambda: port_file.read_text().strip() if port_file.is_file() else "", timeout=3.0)
            assert port_str and port_str.isdigit(), f"TCP listener did not write port file: {port_str!r}"
            bound_port = int(port_str)

            def _port_present():
                r = srv.api_call(
                    "com.superlogical.terminal.ports",
                    {"session_id": sid, "block_id": bid, "args": {}},
                )
                return r if any(p.get("port") == bound_port for p in r.get("ports", [])) else None

            detected = _poll_until(_port_present, timeout=4.0)
            assert detected is not None, f"Port {bound_port} never appeared in terminal.ports before close"

            stop_file.write_text("stop\n", encoding="utf-8")

            def _ports_cleared():
                r = srv.api_call(
                    "com.superlogical.terminal.ports",
                    {"session_id": sid, "block_id": bid, "args": {}},
                )
                return r if r.get("ports") == [] else None

            cleared = _poll_until(_ports_cleared, timeout=4.0)
            assert cleared is not None and cleared["ports"] == [], f"Port {bound_port} remained after close: {cleared}"
        finally:
            if not stop_file.exists():
                stop_file.write_text("stop\n", encoding="utf-8")

        res, png_path, report = ctx.headless_render(
            out_name="f19_closed.png",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        assert res.returncode == 0, res.clean_stderr
        assert report is not None and report.windows >= 1 and report.ready_panes >= 1
        assert_valid_png(png_path, expected_width=1440, expected_height=900)


# ---------------------------------------------------------------------------
# F20: Rich Diagnostics & Telemetry HUD (Ctrl+Alt+,)
# ---------------------------------------------------------------------------


def _f20_t1_01_diagnostics_hud_scenario_renders(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    res, png_path, report = ctx.headless_render(
        out_name="f20_hud.png",
        scenario="diagnostics-hud",
        rust_log="warn",
    )
    assert res.returncode == 0, f"diagnostics-hud scenario failed: {res.clean_stderr}"
    assert report is not None and report.draws > 0 and report.glyphs > 0
    img = assert_valid_png(png_path, expected_width=1440, expected_height=900, min_bytes=4096)
    assert_not_solid_color(img, min_unique_colors=6)


def _f20_t1_02_diagnostics_hud_overlays_workspace(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    res_sb, png_sb, _ = ctx.headless_render(out_name="f20_sb.png", scenario="sidebar-dark", rust_log="off")
    assert res_sb.returncode == 0, res_sb.clean_stderr
    res_hud, png_hud, _ = ctx.headless_render(out_name="f20_hud.png", scenario="diagnostics-hud", rust_log="off")
    assert res_hud.returncode == 0, res_hud.clean_stderr

    img_sb = assert_valid_png(png_sb, expected_width=1440, expected_height=900)
    img_hud = assert_valid_png(png_hud, expected_width=1440, expected_height=900)
    assert pixel_diff_stats(img_sb, img_hud)["diff_fraction"] > 0.05
    modal_stats = pixel_diff_stats(img_sb, img_hud, rect=(360, 150, 720, 600))
    assert modal_stats["changed_pixels"] > 5000, modal_stats


def _f20_t1_03_headless_stdout_telemetry_fields(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    res, png_path, report = ctx.headless_render(
        out_name="f20_telem.png",
        rex_mode="isolated_offline",
        rust_log="warn",
    )
    assert res.returncode == 0
    assert report is not None
    assert report.gpu != ""
    assert report.frame_us > 0
    assert report.draws >= 1 and report.quads >= 1 and report.glyphs >= 0
    assert_valid_png(png_path, expected_width=1440, expected_height=900)


def _f20_t1_04_server_status_schema_and_tailscale(ctx: TestContext) -> None:
    _require_rex()
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        st = srv.api_call("server.status")
        for key in ("pid", "instance_id", "started_at", "exit_empty", "session_count", "config", "listeners"):
            assert key in st, f"Missing {key} in server.status: {st}"
        assert st["api_revision"] == 24, st
        assert st["config"]["generation"] >= 1 and isinstance(st["config"]["hash"], str)

    if live_socket_present():
        live = probe_live_server()
        if live.get("available"):
            assert live["status"]["api_revision"] == 24, live["status"]
            if "tailscale" in live["status"]:
                assert "state" in live["status"]["tailscale"], live["status"]["tailscale"]


def _spawn_rex_with_config(cfg_path: Path, keep_tmp: bool = False):
    sock_dir = Path(tempfile.mkdtemp(prefix=f"omx-cfg-{os.getpid()}-", dir="/tmp"))
    sock_path = sock_dir / "s.sock"
    state_dir = sock_dir / "st"
    state_dir.mkdir(parents=True, exist_ok=True)
    endpoint = f"unix://{sock_path}"
    proc = subprocess.Popen(
        [
            "rex",
            "server",
            "run",
            "--listen",
            endpoint,
            "--state-dir",
            str(state_dir),
            "--restore=false",
            "--no-fs",
            "--config",
            str(cfg_path),
            "--log-level",
            "warn",
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        errors="replace",
    )
    deadline = time.monotonic() + 5.0
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            out, err = proc.communicate(timeout=1.0)
            shutil.rmtree(sock_dir, ignore_errors=True)
            raise RuntimeError(f"rex server --config exited early: {out} {err}")
        if sock_path.exists():
            probe = subprocess.run(
                ["rex", "--autostart=false", "-S", endpoint, "api", "call", "server.status"],
                capture_output=True,
                text=True,
                timeout=1.5,
                check=False,
            )
            if probe.returncode == 0:
                return proc, sock_dir, endpoint
        time.sleep(0.05)
    proc.kill()
    shutil.rmtree(sock_dir, ignore_errors=True)
    raise TimeoutError("rex server --config timed out")


def _cleanup_rex_with_config(proc: subprocess.Popen[str], sock_dir: Path, endpoint: str, keep_tmp: bool) -> None:
    try:
        subprocess.run(["rex", "--autostart=false", "-S", endpoint, "server", "stop"], capture_output=True, timeout=2.0)
        proc.wait(timeout=2.0)
    except Exception:
        proc.kill()
        proc.wait(timeout=1.0)
    finally:
        for pipe in (proc.stdout, proc.stderr):
            if pipe is not None:
                try:
                    pipe.close()
                except OSError:
                    pass
        if not keep_tmp:
            shutil.rmtree(sock_dir, ignore_errors=True)


def _f20_t1_05_broken_lua_keymap_diagnostics(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    cfg_path = ctx.work_dir / "broken_init.lua"
    cfg_path.write_text('rex.bind("ctrl+k", "nonexistent_action_xyz")\n', encoding="utf-8")

    proc, sock_dir, endpoint = _spawn_rex_with_config(cfg_path, keep_tmp=ctx.keep_tmp)
    try:
        km_res = subprocess.run(
            [
                "rex",
                "--autostart=false",
                "-S",
                endpoint,
                "api",
                "call",
                "client.keymap",
                json.dumps({"actions": [{"name": "pane.split", "kind": "client"}]}),
            ],
            capture_output=True,
            text=True,
            timeout=5.0,
            check=False,
        )
        assert km_res.returncode == 0, km_res.stderr
        km = json.loads(km_res.stdout)
        assert km.get("diagnostics"), f"Expected non-empty keymap diagnostics: {km}"

        subprocess.run(
            ["rex", "--autostart=false", "-S", endpoint, "new", "--json", "km-sess", "--", "sleep", "8"],
            capture_output=True,
            text=True,
            timeout=5.0,
            check=True,
        )
        res, png_path, report = ctx.headless_render(
            out_name="f20_km.png",
            rex_mode="custom_socket",
            custom_socket=endpoint,
            rust_log="warn",
        )
        assert res.returncode == 0, res.clean_stderr
        assert not res.crashed and report is not None and report.windows >= 1 and report.ready_panes >= 1
        assert_valid_png(png_path, expected_width=1440, expected_height=900)
    finally:
        _cleanup_rex_with_config(proc, sock_dir, endpoint, ctx.keep_tmp)


def _f20_t2_01_diagnostics_hud_dark_vs_light(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    res_d, png_d, _ = ctx.headless_render(out_name="f20_hud_d.png", scenario="diagnostics-hud", light=False, rust_log="off")
    assert res_d.returncode == 0, res_d.clean_stderr
    res_l, png_l, _ = ctx.headless_render(out_name="f20_hud_l.png", scenario="diagnostics-hud", light=True, rust_log="off")
    assert res_l.returncode == 0, res_l.clean_stderr

    img_d = assert_valid_png(png_d, expected_width=1440, expected_height=900)
    img_l = assert_valid_png(png_l, expected_width=1440, expected_height=900)
    assert pixel_diff_stats(img_d, img_l)["diff_fraction"] > 0.25


def _f20_t2_02_diagnostics_hud_offline_vs_connected_diff(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")

    res_plain, png_plain, rep_plain = ctx.headless_render(
        out_name="f20_plain_off.png",
        scenario=None,
        rex_mode="isolated_offline",
        rust_log="off",
    )
    assert res_plain.returncode == 0, res_plain.clean_stderr
    assert (
        rep_plain is not None
        and rep_plain.session is None
        and rep_plain.windows == 0
        and rep_plain.ready_panes == 0
    ), f"Expected plain offline fallback session=None windows=0 ready_panes=0, got {rep_plain}"

    res_off, png_off, rep_off = ctx.headless_render(
        out_name="f20_off.png",
        scenario="diagnostics-hud",
        rex_mode="isolated_offline",
        rust_log="off",
    )
    assert res_off.returncode == 0, res_off.clean_stderr
    assert (
        rep_off is not None
        and rep_off.session == "Work"
        and rep_off.session != "hud-live"
        and rep_off.windows >= 1
        and rep_off.ready_panes >= 1
    ), f"Expected offline diagnostics-hud QA seed session='Work' windows>=1 ready_panes>=1, got {rep_off}"

    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        srv.new_session(label="hud-live", command=["sleep", "10"])
        res_conn, png_conn, rep_conn = ctx.headless_render(
            out_name="f20_conn.png",
            scenario="diagnostics-hud",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="off",
        )
        assert res_conn.returncode == 0, res_conn.clean_stderr
        assert (
            rep_conn is not None
            and rep_conn.session == "hud-live"
            and rep_conn.windows >= 1
            and rep_conn.ready_panes >= 1
        ), f"Expected connected diagnostics-hud session='hud-live' windows>=1 ready_panes>=1, got {rep_conn}"

    img_plain = assert_valid_png(png_plain, expected_width=1440, expected_height=900)
    img_off = assert_valid_png(png_off, expected_width=1440, expected_height=900)
    img_conn = assert_valid_png(png_conn, expected_width=1440, expected_height=900)
    assert pixel_diff_stats(img_plain, img_off)["diff_fraction"] > 0.05
    assert pixel_diff_stats(img_off, img_conn)["diff_fraction"] > 0.01
    assert pixel_diff_stats(img_off, img_conn, rect=(350, 170, 740, 560))["changed_pixels"] > 200


def _f20_t2_03_diagnostics_hud_broken_init_lua_no_panic(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    cfg_path = ctx.work_dir / "warn_init.lua"
    cfg_path.write_text('rex.bind("ctrl+alt+z", "missing_action_hud_test")\n', encoding="utf-8")

    proc, sock_dir, endpoint = _spawn_rex_with_config(cfg_path, keep_tmp=ctx.keep_tmp)
    try:
        subprocess.run(
            ["rex", "--autostart=false", "-S", endpoint, "new", "--json", "hud-warn", "--", "sleep", "8"],
            capture_output=True,
            text=True,
            timeout=5.0,
            check=True,
        )
        res, png_path, report = ctx.headless_render(
            out_name="f20_hud_warn.png",
            scenario="diagnostics-hud",
            rex_mode="custom_socket",
            custom_socket=endpoint,
            rust_log="warn",
        )
        assert res.returncode == 0, res.clean_stderr
        assert not res.crashed and report is not None and report.windows >= 1 and report.ready_panes >= 1
        assert_valid_png(png_path, expected_width=1440, expected_height=900)
    finally:
        _cleanup_rex_with_config(proc, sock_dir, endpoint, ctx.keep_tmp)


def _f20_t2_04_diagnostics_hud_compact_viewport_and_scale(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    for sc, fname in ((1.0, "f20_800_1x.png"), (2.0, "f20_800_2x.png")):
        res, png_path, report = ctx.headless_render(
            out_name=fname,
            width=800,
            height=600,
            scale=sc,
            scenario="diagnostics-hud",
            rust_log="off",
        )
        assert res.returncode == 0, res.clean_stderr
        assert report is not None and report.quads > 0 and report.glyphs > 0
        img = assert_valid_png(png_path, expected_width=800, expected_height=600)
        assert_not_solid_color(img, min_unique_colors=4)


def _f20_t2_05_diagnostics_hud_toggle_render_consistency(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    r_a, p_a, _ = ctx.headless_render(out_name="f20_a.png", scenario="sidebar-dark", rust_log="off")
    assert r_a.returncode == 0, r_a.clean_stderr
    r_b, p_b, _ = ctx.headless_render(out_name="f20_b.png", scenario="diagnostics-hud", rust_log="off")
    assert r_b.returncode == 0, r_b.clean_stderr
    r_c, p_c, _ = ctx.headless_render(out_name="f20_c.png", scenario="sidebar-dark", rust_log="off")
    assert r_c.returncode == 0, r_c.clean_stderr

    img_a = assert_valid_png(p_a, expected_width=1440, expected_height=900)
    img_b = assert_valid_png(p_b, expected_width=1440, expected_height=900)
    img_c = assert_valid_png(p_c, expected_width=1440, expected_height=900)
    assert pixel_diff_stats(img_a, img_c)["diff_fraction"] < 0.01
    assert pixel_diff_stats(img_a, img_b)["diff_fraction"] > 0.05


# ---------------------------------------------------------------------------
# F21: Categorized Settings Modal & PipeWire OSC 7501 Micro-Chimes
# ---------------------------------------------------------------------------


def _f21_t1_01_settings_modal_scenario_renders(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    res, png_path, report = ctx.headless_render(
        out_name="f21_settings.png",
        scenario="settings-modal",
        rust_log="warn",
    )
    assert res.returncode == 0, f"settings-modal scenario failed: {res.clean_stderr}"
    assert report is not None and report.draws > 0 and report.glyphs > 0
    img = assert_valid_png(png_path, expected_width=1440, expected_height=900, min_bytes=4096)
    assert_not_solid_color(img, min_unique_colors=6)


def _f21_t1_02_settings_sidebar_width_clamping_and_behavior(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    ctx.write_settings({"sidebar_behavior": "always", "sidebar_width_px": 999.0})
    r_999, p_999, _ = ctx.headless_render(out_name="f21_w999.png", width=1024, height=640, rust_log="off")
    assert r_999.returncode == 0

    ctx.write_settings({"sidebar_behavior": "always", "sidebar_width_px": 380.0})
    r_380, p_380, _ = ctx.headless_render(out_name="f21_w380.png", width=1024, height=640, rust_log="off")
    assert r_380.returncode == 0

    ctx.write_settings({"sidebar_behavior": "always", "sidebar_width_px": 180.0})
    r_180, p_180, _ = ctx.headless_render(out_name="f21_w180.png", width=1024, height=640, rust_log="off")
    assert r_180.returncode == 0

    ctx.write_settings({"sidebar_behavior": "hidden", "sidebar_width_px": 380.0})
    r_hid, p_hid, _ = ctx.headless_render(out_name="f21_whid.png", width=1024, height=640, rust_log="off")
    assert r_hid.returncode == 0

    img_999 = assert_valid_png(p_999, expected_width=1024, expected_height=640)
    img_380 = assert_valid_png(p_380, expected_width=1024, expected_height=640)
    img_180 = assert_valid_png(p_180, expected_width=1024, expected_height=640)
    img_hid = assert_valid_png(p_hid, expected_width=1024, expected_height=640)
    assert pixel_diff_stats(img_999, img_380)["diff_fraction"] < 0.01
    assert pixel_diff_stats(img_180, img_380)["diff_fraction"] > 0.01
    assert pixel_diff_stats(img_380, img_hid)["diff_fraction"] > 0.05


def _f21_t1_03_settings_pane_chrome_options(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        srv.new_session(label="f21-chrome", command=["sleep", "10"])

        ctx.write_settings(
            {
                "pane_header_mode": "always",
                "highlight_active_border": True,
                "fade_inactive_panes": False,
                "adaptive_rounding": True,
            }
        )
        r_a, p_a, rep_a = ctx.headless_render(
            out_name="f21_chr_a.png",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="off",
        )
        assert r_a.returncode == 0 and rep_a is not None and rep_a.windows >= 1 and rep_a.ready_panes >= 1

        ctx.write_settings(
            {
                "pane_header_mode": "hidden",
                "highlight_active_border": False,
                "fade_inactive_panes": True,
                "adaptive_rounding": False,
            }
        )
        r_b, p_b, rep_b = ctx.headless_render(
            out_name="f21_chr_b.png",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="off",
        )
        assert r_b.returncode == 0 and rep_b is not None and rep_b.windows >= 1 and rep_b.ready_panes >= 1

    img_a = assert_valid_png(p_a, expected_width=1440, expected_height=900)
    img_b = assert_valid_png(p_b, expected_width=1440, expected_height=900)
    assert pixel_diff_stats(img_a, img_b)["diff_fraction"] > 0.002


def _f21_t1_04_settings_mute_and_collapsed_fields_accepted(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    ctx.write_settings(
        {
            "muted_blocks": ["block:test"],
            "muted_sessions": ["session:test"],
            "collapsed_sessions": ["local/session:test"],
        }
    )
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        srv.new_session(label="f21-mute-cfg", command=["sleep", "10"])
        res, png_path, report = ctx.headless_render(
            out_name="f21_muted_cfg.png",
            width=640,
            height=480,
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        assert res.returncode == 0, res.clean_stderr
        assert not res.has_warning and not res.has_error, (
            f"Unexpected warning/error with settings fields: {res.clean_stdout} {res.clean_stderr}"
        )
        assert report is not None and report.windows >= 1 and report.ready_panes >= 1
        assert_valid_png(png_path, expected_width=640, expected_height=480)


def _f21_t1_05_pipewire_tools_and_settings_render(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    caps = tool_capabilities()
    if caps["pw-play"]:
        pw = subprocess.run(["pw-play", "--help"], capture_output=True, text=True, timeout=3.0, check=False)
        assert pw.returncode == 0, pw.stderr
    res, png_path, report = ctx.headless_render(
        out_name="f21_pw_settings.png",
        scenario="settings-modal",
        rust_log="warn",
    )
    assert res.returncode == 0, res.clean_stderr
    assert report is not None
    assert_valid_png(png_path, expected_width=1440, expected_height=900)


def _f21_t2_01_settings_modal_distinct_region(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    r_sb, p_sb, _ = ctx.headless_render(out_name="f21_sb.png", scenario="sidebar-dark", rust_log="off")
    assert r_sb.returncode == 0, r_sb.clean_stderr
    r_st, p_st, _ = ctx.headless_render(out_name="f21_st.png", scenario="settings-modal", rust_log="off")
    assert r_st.returncode == 0, r_st.clean_stderr

    img_sb = assert_valid_png(p_sb, expected_width=1440, expected_height=900)
    img_st = assert_valid_png(p_st, expected_width=1440, expected_height=900)
    assert pixel_diff_stats(img_sb, img_st)["diff_fraction"] > 0.05
    modal_stats = pixel_diff_stats(img_sb, img_st, rect=(300, 120, 840, 660))
    assert modal_stats["changed_pixels"] > 5000, modal_stats


def _f21_t2_02_corrupt_and_partial_settings_fallback(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")

    cfg_dir = ctx.xdg_config / "omarex"
    cfg_dir.mkdir(parents=True, exist_ok=True)
    (cfg_dir / "settings.json").write_text("{corrupt json!!!", encoding="utf-8")
    r_bad, p_bad, _ = ctx.headless_render(out_name="f21_corrupt.png", width=800, height=600, rust_log="warn")
    assert r_bad.returncode == 0 and not r_bad.crashed
    assert_valid_png(p_bad, expected_width=800, expected_height=600)

    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = srv.new_session(
            label="f21-font",
            command=["bash", "--norc", "--noprofile", "-c", "for i in $(seq 1 25); do echo FONT_SIZE_ROW_$i; done; sleep 10"],
        )
        sid = created["session_id"]
        bid = created["initial_windows"][0]["block_ids"][0]
        _poll_until(
            lambda: "FONT_SIZE_ROW_25"
            in srv.api_call(
                "com.superlogical.terminal.format",
                {"session_id": sid, "block_id": bid, "args": {"format": "text"}},
            ).get("content", ""),
            timeout=3.0,
        )

        ctx.write_settings({"font_size_pt": 20.0})
        r_20, p_20, rep_20 = ctx.headless_render(
            out_name="f21_f20.png",
            width=1024,
            height=640,
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="off",
        )
        assert r_20.returncode == 0 and rep_20 is not None and rep_20.windows >= 1 and rep_20.ready_panes >= 1

        ctx.write_settings({"font_size_pt": 8.0})
        r_8, p_8, rep_8 = ctx.headless_render(
            out_name="f21_f8.png",
            width=1024,
            height=640,
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="off",
        )
        assert r_8.returncode == 0 and rep_8 is not None and rep_8.windows >= 1 and rep_8.ready_panes >= 1

    img_20 = assert_valid_png(p_20, expected_width=1024, expected_height=640)
    img_8 = assert_valid_png(p_8, expected_width=1024, expected_height=640)
    assert pixel_diff_stats(img_20, img_8)["diff_fraction"] > 0.01


def _f21_t2_03_material_and_density_affect_render(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    ctx.write_settings({"material": {"kind": "classic"}, "density": "comfortable"})
    r_a, p_a, _ = ctx.headless_render(out_name="f21_mat_a.png", scenario="split-panes", rust_log="off")
    assert r_a.returncode == 0, r_a.clean_stderr

    ctx.write_settings({"material": {"kind": "translucent"}, "density": "compact"})
    r_b, p_b, _ = ctx.headless_render(out_name="f21_mat_b.png", scenario="split-panes", rust_log="off")
    assert r_b.returncode == 0, r_b.clean_stderr

    img_a = assert_valid_png(p_a, expected_width=1440, expected_height=900)
    img_b = assert_valid_png(p_b, expected_width=1440, expected_height=900)
    assert pixel_diff_stats(img_a, img_b)["diff_fraction"] > 0.01


def _f21_t2_04_osc7501_program_status_and_muted_blocks(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = srv.new_session(label="f21-osc7501", command=["bash", "--norc", "--noprofile"])
        sid = created["session_id"]
        bid = created["initial_windows"][0]["block_ids"][0]

        srv.api_call(
            "com.superlogical.terminal.write",
            {
                "session_id": sid,
                "block_id": bid,
                "args": {"data": "printf '\\033]7501;state=blocked:kind=permission:app=claude\\033\\\\'\n"},
            },
        )
        def _check_state(want: str):
            r = srv.api_call(
                "com.superlogical.terminal.program_status",
                {"session_id": sid, "block_id": bid, "args": {}},
            )
            return r if any(rec.get("state") == want for rec in r.get("records", [])) else None

        blocked = _poll_until(lambda: _check_state("blocked"), timeout=3.0)
        assert blocked is not None, "Expected blocked record in program_status"

        r_unmuted, p_unmuted, rep_u = ctx.headless_render(
            out_name="f21_unmuted.png",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        assert r_unmuted.returncode == 0 and rep_u is not None and rep_u.windows >= 1 and rep_u.ready_panes >= 1
        assert_valid_png(p_unmuted, expected_width=1440, expected_height=900)

        srv.api_call(
            "com.superlogical.terminal.write",
            {
                "session_id": sid,
                "block_id": bid,
                "args": {"data": "printf '\\033]7501;state=done:app=claude\\033\\\\'\n"},
            },
        )
        done = _poll_until(lambda: _check_state("done"), timeout=3.0)
        assert done is not None, "Expected done record in program_status"

        ctx.write_settings({"muted_blocks": [bid]})
        r_muted, p_muted, rep_m = ctx.headless_render(
            out_name="f21_muted.png",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        assert r_muted.returncode == 0 and rep_m is not None and rep_m.windows >= 1 and rep_m.ready_panes >= 1
        assert_valid_png(p_muted, expected_width=1440, expected_height=900)


def _f21_t2_05_settings_modal_dark_vs_light_contrast(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    r_d, p_d, _ = ctx.headless_render(out_name="f21_mod_d.png", scenario="settings-modal", light=False, rust_log="off")
    assert r_d.returncode == 0, r_d.clean_stderr
    r_l, p_l, _ = ctx.headless_render(out_name="f21_mod_l.png", scenario="settings-modal", light=True, rust_log="off")
    assert r_l.returncode == 0, r_l.clean_stderr

    img_d = assert_valid_png(p_d, expected_width=1440, expected_height=900)
    img_l = assert_valid_png(p_l, expected_width=1440, expected_height=900)
    assert pixel_diff_stats(img_d, img_l)["diff_fraction"] > 0.25
    lum_d = relative_luminance(mean_color_in_rect(img_d, 300, 120, 840, 660)[:3])
    lum_l = relative_luminance(mean_color_in_rect(img_l, 300, 120, 840, 660)[:3])
    assert lum_l > lum_d + 0.30, f"Expected modal luminance light ({lum_l:.3f}) > dark ({lum_d:.3f}) + 0.30"


# ---------------------------------------------------------------------------
# Registration (70 tests: 35 Tier 1 + 35 Tier 2)
# ---------------------------------------------------------------------------


def register_tests(registry: TestRegistry) -> None:
    cases = [
        # F15
        ("F15-T1-01", 1, "F15", "copy-mode headless scenario renders valid 1440x900 PNG", _f15_t1_01_copy_mode_scenario_default),
        ("F15-T1-02", 1, "F15", "copy-mode connects to isolated Rex session with scrollback", _f15_t1_02_copy_mode_isolated_rex_scrollback),
        ("F15-T1-03", 1, "F15", "copy-mode offline fallback renders without crash", _f15_t1_03_copy_mode_offline_fallback),
        ("F15-T1-04", 1, "F15", "copy-mode honors --width 800 --height 600 --scale 2.0", _f15_t1_04_copy_mode_scaled_geometry),
        ("F15-T1-05", 1, "F15", "copy-mode dark vs --light palette contrast", _f15_t1_05_copy_mode_dark_vs_light),
        ("F15-T2-01", 2, "F15", "copy-mode visual diff vs sidebar-dark baseline", _f15_t2_01_copy_mode_diff_vs_sidebar_dark),
        ("F15-T2-02", 2, "F15", "copy-mode over populated IsolatedRexServer scrollback differs from offline", _f15_t2_02_copy_mode_populated_scrollback_diff),
        ("F15-T2-03", 2, "F15", "copy-mode selection highlight reflects Omarchy selection color", _f15_t2_03_copy_mode_selection_uses_theme_color),
        ("F15-T2-04", 2, "F15", "copy-mode at narrow 480x320 viewport does not clip or crash", _f15_t2_04_copy_mode_narrow_viewport_no_clip_crash),
        ("F15-T2-05", 2, "F15", "copy-mode leaves IsolatedRexServer session uncorrupted", _f15_t2_05_copy_mode_preserves_rex_session),
        # F16
        ("F16-T1-01", 1, "F16", "list_dir files=false returns dirs and symlinks only", _f16_t1_01_list_dir_dirs_only),
        ("F16-T1-02", 1, "F16", "list_dir files=true includes files and dotfiles", _f16_t1_02_list_dir_with_files),
        ("F16-T1-03", 1, "F16", "list_dir on empty directory returns empty entries", _f16_t1_03_list_dir_empty_dir),
        ("F16-T1-04", 1, "F16", "list_dir returns missing and not_dir status cleanly", _f16_t1_04_list_dir_missing_and_not_dir),
        ("F16-T1-05", 1, "F16", "list_dir limit truncation and case-insensitive prefix", _f16_t1_05_list_dir_limit_and_prefix),
        ("F16-T2-01", 2, "F16", "list_dir empty path uses cwd and tilde resolves home", _f16_t2_01_list_dir_empty_path_cwd_and_tilde),
        ("F16-T2-02", 2, "F16", "GoToDirectory cwd sync and omarex --working-directory render", _f16_t2_02_go_to_directory_sync_and_headless_dir),
        ("F16-T2-03", 2, "F16", "InsertPath spaced path write and screen capture parity", _f16_t2_03_insert_path_with_spaces_parity),
        ("F16-T2-04", 2, "F16", "list_dir omits dangling symlinks and reports denied on chmod 000", _f16_t2_04_broken_symlink_and_denied_resilience),
        ("F16-T2-05", 2, "F16", "large 300-entry list_dir and picker settings render", _f16_t2_05_large_list_dir_and_settings_render),
        # F17
        ("F17-T1-01", 1, "F17", "theme-switcher headless scenario renders valid 1440x900 PNG", _f17_t1_01_theme_switcher_scenario_renders),
        ("F17-T1-02", 1, "F17", "XDG_STATE_HOME dark vs light colors.toml switches render", _f17_t1_02_xdg_dark_vs_light_colors_toml),
        ("F17-T1-03", 1, "F17", "theme-switcher tolerates broken-no-colors directory", _f17_t1_03_theme_discovery_with_broken_dir),
        ("F17-T1-04", 1, "F17", "osaka-jade missing colors.toml falls back without panic", _f17_t1_04_missing_colors_toml_osaka_jade_pattern),
        ("F17-T1-05", 1, "F17", "--light flag overrides dark XDG theme", _f17_t1_05_light_flag_overrides_dark_xdg),
        ("F17-T2-01", 2, "F17", "theme-switcher modal differs visually from sidebar-dark", _f17_t2_01_theme_switcher_diff_vs_sidebar_dark),
        ("F17-T2-02", 2, "F17", "theme-switcher renders catalog with multiple custom themes", _f17_t2_02_theme_switcher_lists_multiple_custom_themes),
        ("F17-T2-03", 2, "F17", "atomic symlink swap on omarchy/current/theme updates render", _f17_t2_03_atomic_symlink_swap_updates_render),
        ("F17-T2-04", 2, "F17", "theme-switcher modal flips polarity between dark and --light", _f17_t2_04_theme_switcher_modal_polarity_dark_vs_light),
        ("F17-T2-05", 2, "F17", "active Omarchy theme applied to IsolatedRexServer pane render", _f17_t2_05_theme_applied_to_isolated_rex_pane),
        # F18
        ("F18-T1-01", 1, "F18", "OSC 52 ASCII payload roundtrip via clipboard_read", _f18_t1_01_osc52_ascii_roundtrip),
        ("F18-T1-02", 1, "F18", "OSC 52 multi-byte UTF-8 payload roundtrip via clipboard_read", _f18_t1_02_osc52_utf8_roundtrip),
        ("F18-T1-03", 1, "F18", "clipboard_read id lookup and invalid id not_found error", _f18_t1_03_clipboard_read_id_lookup_and_missing),
        ("F18-T1-04", 1, "F18", "headless read-only omarex render preserves OSC 52 clipboard_read", _f18_t1_04_headless_clipboard_disabled_with_osc52),
        ("F18-T1-05", 1, "F18", "clipboard_read on fresh block without OSC 52 returns not_found", _f18_t1_05_clipboard_read_empty_block_not_found),
        ("F18-T2-01", 2, "F18", "chunked clipboard_read over >6 KiB UTF-8 payload", _f18_t2_01_chunked_clipboard_read_large_utf8),
        ("F18-T2-02", 2, "F18", "clipboard_written event emitted on OSC 52 plus omarex render", _f18_t2_02_clipboard_written_event_and_omarex_render),
        ("F18-T2-03", 2, "F18", "rapid sequential OSC 52 writes retain latest payload for id=0", _f18_t2_03_rapid_osc52_retains_latest),
        ("F18-T2-04", 2, "F18", "malformed base64 and empty OSC 52 resilience", _f18_t2_04_malformed_and_empty_osc52_resilience),
        ("F18-T2-05", 2, "F18", "OSC 52 Rex session renders safely without WAYLAND_DISPLAY or DISPLAY", _f18_t2_05_clipboard_enabled_without_wayland_display),
        # F19
        ("F19-T1-01", 1, "F19", "ports-pill headless scenario renders valid non-solid PNG", _f19_t1_01_ports_pill_scenario_renders),
        ("F19-T1-02", 1, "F19", "com.superlogical.terminal.ports returns empty list on idle pane", _f19_t1_02_ports_rpc_empty_on_idle_pane),
        ("F19-T1-03", 1, "F19", "com.superlogical.tunnel.info reports dial and listen flavors", _f19_t1_03_tunnel_info_dial_and_listen_flavors),
        ("F19-T1-04", 1, "F19", "live local TCP listener detected by terminal.ports RPC", _f19_t1_04_live_tcp_listener_detected_by_ports_rpc),
        ("F19-T1-05", 1, "F19", "rex --autostart=false bad-socket exits 4", _f19_t1_05_bad_socket_autostart_false_exit_4),
        ("F19-T2-01", 2, "F19", "ports-pill scenario differs visibly in pane header band", _f19_t2_01_ports_pill_differs_from_baseline_header),
        ("F19-T2-02", 2, "F19", "ports_changed event + terminal.ports + omarex connected render", _f19_t2_02_ports_changed_event_and_omarex_render),
        ("F19-T2-03", 2, "F19", "tunnel dial option validation and block.close lifecycle", _f19_t2_03_tunnel_validation_and_close_lifecycle),
        ("F19-T2-04", 2, "F19", "multiple simultaneous listening ports sorted ascending", _f19_t2_04_multiple_ports_sorted_deterministically),
        ("F19-T2-05", 2, "F19", "closing TCP listener clears terminal.ports cleanly", _f19_t2_05_closing_listener_removes_port),
        # F20
        ("F20-T1-01", 1, "F20", "diagnostics-hud headless scenario renders valid PNG", _f20_t1_01_diagnostics_hud_scenario_renders),
        ("F20-T1-02", 1, "F20", "diagnostics-hud overlays modal pixels over sidebar-dark", _f20_t1_02_diagnostics_hud_overlays_workspace),
        ("F20-T1-03", 1, "F20", "headless stdout telemetry fields present and bounded", _f20_t1_03_headless_stdout_telemetry_fields),
        ("F20-T1-04", 1, "F20", "server.status config and Tailscale schema verified", _f20_t1_04_server_status_schema_and_tailscale),
        ("F20-T1-05", 1, "F20", "broken init.lua produces client.keymap diagnostics without crash", _f20_t1_05_broken_lua_keymap_diagnostics),
        ("F20-T2-01", 2, "F20", "diagnostics-hud renders telemetry in dark vs --light", _f20_t2_01_diagnostics_hud_dark_vs_light),
        ("F20-T2-02", 2, "F20", "diagnostics-hud offline vs connected IsolatedRexServer diff", _f20_t2_02_diagnostics_hud_offline_vs_connected_diff),
        ("F20-T2-03", 2, "F20", "diagnostics-hud with broken init.lua surfaces cleanly without panic", _f20_t2_03_diagnostics_hud_broken_init_lua_no_panic),
        ("F20-T2-04", 2, "F20", "diagnostics-hud stays bounded at 800x600 scale 1.0 and 2.0", _f20_t2_04_diagnostics_hud_compact_viewport_and_scale),
        ("F20-T2-05", 2, "F20", "diagnostics-hud toggle consistency against sidebar-dark", _f20_t2_05_diagnostics_hud_toggle_render_consistency),
        # F21
        ("F21-T1-01", 1, "F21", "settings-modal headless scenario renders valid PNG", _f21_t1_01_settings_modal_scenario_renders),
        ("F21-T1-02", 1, "F21", "settings.json clamps sidebar_width_px and toggles sidebar_behavior", _f21_t1_02_settings_sidebar_width_clamping_and_behavior),
        ("F21-T1-03", 1, "F21", "settings.json applies pane_header_mode and border/fade flags", _f21_t1_03_settings_pane_chrome_options),
        ("F21-T1-04", 1, "F21", "settings.json muted_blocks/sessions and collapsed_sessions accepted", _f21_t1_04_settings_mute_and_collapsed_fields_accepted),
        ("F21-T1-05", 1, "F21", "PipeWire CLI probe and settings-modal render", _f21_t1_05_pipewire_tools_and_settings_render),
        ("F21-T2-01", 2, "F21", "settings-modal renders distinct modal region over workspace", _f21_t2_01_settings_modal_distinct_region),
        ("F21-T2-02", 2, "F21", "corrupt and partial settings.json fall back cleanly", _f21_t2_02_corrupt_and_partial_settings_fallback),
        ("F21-T2-03", 2, "F21", "material.kind and density settings alter split-panes render", _f21_t2_03_material_and_density_affect_render),
        ("F21-T2-04", 2, "F21", "OSC 7501 blocked/done recorded and handled with muted_blocks", _f21_t2_04_osc7501_program_status_and_muted_blocks),
        ("F21-T2-05", 2, "F21", "settings-modal dark vs --light contrast inversion", _f21_t2_05_settings_modal_dark_vs_light_contrast),
    ]
    for cid, tier, feat, name, fn in cases:
        registry.add(TestCase(id=cid, tier=tier, feature=feat, name=name, fn=fn))
