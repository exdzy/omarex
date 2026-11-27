"""Tier 1 & Tier 2 opaque-box E2E tests for F22–F24 (DnD Split, FloatOrDock, Multi-Window, Visual QA)."""

from __future__ import annotations

import time
from typing import Any

from lib.fixtures import DARK_COLORS_TOML, LIGHT_COLORS_TOML, create_fixtures
from lib.harness import SkipTest, TestCase, TestContext, TestRegistry
from lib.png_check import (
    assert_not_solid_color,
    assert_valid_png,
    pixel_diff_stats,
    relative_luminance,
    sample_pixel,
)
from lib.rex_helper import IsolatedRexServer, rex_available

ALL_13_SCENARIOS = (
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


def _tiled_layer(window_obj: dict[str, Any]) -> dict[str, Any]:
    for layer in window_obj.get("layers", []):
        if layer.get("kind") == "tiled":
            return layer
    raise AssertionError(f"Missing tiled layer in window: {window_obj}")


def _floating_layers(window_obj: dict[str, Any]) -> list[dict[str, Any]]:
    return [layer for layer in window_obj.get("layers", []) if layer.get("kind") == "floating"]


# ---------------------------------------------------------------------------
# F22: 5-Zone DnD Split Preview, Tab Drag-Reorder & FloatOrDock Fix
# ---------------------------------------------------------------------------


def _f22_t1_01_dnd_split_scenario_renders(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    res, png_path, report = ctx.headless_render(
        out_name="f22_dnd.png",
        scenario="dnd-split",
        rust_log="warn",
    )
    assert res.returncode == 0, f"dnd-split scenario failed: {res.clean_stderr}"
    assert report is not None and report.draws > 0 and report.glyphs > 0
    img = assert_valid_png(png_path, expected_width=1440, expected_height=900, min_bytes=4096)
    assert_not_solid_color(img, min_unique_colors=6)


def _f22_t1_02_floating_pr_scenario_renders(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    res, png_path, report = ctx.headless_render(
        out_name="f22_fpr.png",
        scenario="floating-pr",
        rust_log="warn",
    )
    assert res.returncode == 0, f"floating-pr scenario failed: {res.clean_stderr}"
    assert report is not None and report.draws > 0 and report.glyphs > 0
    img = assert_valid_png(png_path, expected_width=1440, expected_height=900, min_bytes=4096)
    assert_not_solid_color(img, min_unique_colors=6)


def _f22_t1_03_swap_blocks_rpc_and_negative(ctx: TestContext) -> None:
    _require_rex()
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = srv.new_session(label="f22-swap", command=["sleep", "10"])
        sid = created["session_id"]
        b1 = created["initial_windows"][0]["block_ids"][0]
        sp = srv.api_call(
            "session.new_split",
            {
                "session_id": sid,
                "anchor_id": b1,
                "direction": "horizontal",
                "side": "after",
                "layout": {"block": {"flavor": "com.superlogical.terminal.shell", "label": "p2"}},
            },
        )
        b2 = sp["block_ids"][0]
        view_before = srv.api_call("session.view", {"session_id": sid})
        rev_before = view_before["revision"]
        split_before = _tiled_layer(view_before["windows"][0])["layout"]["split"]
        assert split_before["before"]["block_id"] == b1
        assert split_before["after"]["block_id"] == b2

        sw = srv.api_call("session.swap_blocks", {"session_id": sid, "block_id": b1, "other_block_id": b2})
        assert sw["revision"] > rev_before
        view_after = srv.api_call("session.view", {"session_id": sid})
        split_after = _tiled_layer(view_after["windows"][0])["layout"]["split"]
        assert split_after["before"]["block_id"] == b2
        assert split_after["after"]["block_id"] == b1

        try:
            srv.api_call("session.swap_blocks", {"session_id": sid, "block_id": b1, "other_block_id": b1})
        except RuntimeError as exc:
            assert "bad_request" in str(exc), str(exc)
        else:
            raise AssertionError("Expected bad_request when swapping block with itself")


def _f22_t1_04_move_block_reparents_to_vertical_split(ctx: TestContext) -> None:
    _require_rex()
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = srv.new_session(label="f22-move", command=["sleep", "10"])
        sid = created["session_id"]
        b1 = created["initial_windows"][0]["block_ids"][0]
        sp1 = srv.api_call(
            "session.new_split",
            {
                "session_id": sid,
                "anchor_id": b1,
                "direction": "horizontal",
                "side": "after",
                "layout": {"block": {"flavor": "com.superlogical.terminal.shell", "label": "p2"}},
            },
        )
        b2 = sp1["block_ids"][0]
        sp2 = srv.api_call(
            "session.new_split",
            {
                "session_id": sid,
                "anchor_id": b2,
                "direction": "horizontal",
                "side": "after",
                "layout": {"block": {"flavor": "com.superlogical.terminal.shell", "label": "p3"}},
            },
        )
        _b3 = sp2["block_ids"][0]

        mv = srv.api_call(
            "session.move_block",
            {
                "session_id": sid,
                "block_id": b2,
                "anchor_block_id": b1,
                "direction": "vertical",
                "side": "before",
                "ratio": 0.4,
            },
        )
        assert mv.get("created_split_id"), mv
        view = srv.api_call("session.view", {"session_id": sid})
        root_split = _tiled_layer(view["windows"][0])["layout"]["split"]
        vsplit = root_split["before"]["split"]
        assert vsplit["direction"] == "vertical", vsplit
        assert vsplit["before"]["block_id"] == b2 and vsplit["after"]["block_id"] == b1


def _f22_t1_05_move_window_reorders_tabs(ctx: TestContext) -> None:
    _require_rex()
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = srv.new_session(label="f22-tabs", command=["sleep", "10"])
        sid = created["session_id"]
        w1 = created["initial_windows"][0]["window_id"]
        w2 = srv.api_call(
            "session.new_window",
            {"session_id": sid, "window_label": "w2", "layout": {"block": {"flavor": "com.superlogical.terminal.shell"}}},
        )["window_id"]
        w3 = srv.api_call(
            "session.new_window",
            {"session_id": sid, "window_label": "w3", "layout": {"block": {"flavor": "com.superlogical.terminal.shell"}}},
        )["window_id"]

        srv.api_call("session.move_window", {"session_id": sid, "window_id": w3, "before_window_id": w1})
        view = srv.api_call("session.view", {"session_id": sid})
        order = [w["window_id"] for w in view["windows"]]
        assert order == [w3, w1, w2], f"Expected [w3, w1, w2], got {order}"


def _f22_t2_01_dnd_split_differs_from_split_panes(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    r_sp, p_sp, _ = ctx.headless_render(out_name="f22_sp.png", scenario="split-panes", rust_log="off")
    assert r_sp.returncode == 0, r_sp.clean_stderr
    r_dnd, p_dnd, _ = ctx.headless_render(out_name="f22_dnd.png", scenario="dnd-split", rust_log="off")
    assert r_dnd.returncode == 0, r_dnd.clean_stderr

    img_sp = assert_valid_png(p_sp, expected_width=1440, expected_height=900)
    img_dnd = assert_valid_png(p_dnd, expected_width=1440, expected_height=900)
    assert pixel_diff_stats(img_sp, img_dnd)["diff_fraction"] > 0.02


def _f22_t2_02_floating_pr_renders_elevated_layer(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    r_sb, p_sb, _ = ctx.headless_render(out_name="f22_sb.png", scenario="sidebar-dark", rust_log="off")
    assert r_sb.returncode == 0, r_sb.clean_stderr
    r_fpr, p_fpr, _ = ctx.headless_render(out_name="f22_fpr.png", scenario="floating-pr", rust_log="off")
    assert r_fpr.returncode == 0, r_fpr.clean_stderr

    img_sb = assert_valid_png(p_sb, expected_width=1440, expected_height=900)
    img_fpr = assert_valid_png(p_fpr, expected_width=1440, expected_height=900)
    drawer_stats = pixel_diff_stats(img_sb, img_fpr, rect=(650, 40, 750, 820))
    assert drawer_stats["diff_fraction"] > 0.05, drawer_stats


def _f22_t2_03_float_last_tiled_block_empty_tiled_layer(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = srv.new_session(label="f22-empty-tiled", command=["sleep", "10"])
        sid = created["session_id"]
        b1 = created["initial_windows"][0]["block_ids"][0]
        srv.api_call(
            "session.float_block",
            {"session_id": sid, "block_id": b1, "bounds": [0.1, 0.1, 0.8, 0.8]},
        )
        view = srv.api_call("session.view", {"session_id": sid})
        win = view["windows"][0]
        assert _tiled_layer(win)["layout"] is None, f"Expected empty tiled layout: {win}"
        assert len(_floating_layers(win)) == 1, win

        res, png_path, report = ctx.headless_render(
            out_name="f22_empty_tiled.png",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        assert res.returncode == 0, res.clean_stderr
        assert report is not None and report.windows >= 1 and report.ready_panes >= 1
        img = assert_valid_png(png_path, expected_width=1440, expected_height=900)
        assert_not_solid_color(img, min_unique_colors=4)


def _f22_t2_04_minimized_floating_layer_state(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = srv.new_session(label="f22-min", command=["sleep", "10"])
        sid = created["session_id"]
        nl = srv.api_call(
            "session.new_layer",
            {
                "session_id": sid,
                "bounds": [0.15, 0.15, 0.7, 0.7],
                "focus": True,
                "layout": {
                    "block": {
                        "flavor": "com.superlogical.terminal.shell",
                        "label": "flt",
                        "options": {
                            "command": [
                                "bash",
                                "--norc",
                                "--noprofile",
                                "-c",
                                "for i in $(seq 1 20); do echo FLOAT_LINE_$i; done; sleep 10",
                            ]
                        },
                    }
                },
            },
        )
        lid = nl["layer_id"]
        fbid = nl["block_ids"][0]
        _poll_until(
            lambda: "FLOAT_LINE_20"
            in srv.api_call(
                "com.superlogical.terminal.format",
                {"session_id": sid, "block_id": fbid, "args": {"format": "text"}},
            ).get("content", ""),
            timeout=3.0,
        )

        srv.api_call("session.set_layer_state", {"session_id": sid, "layer_id": lid, "state": "minimized"})
        view_min = srv.api_call("session.view", {"session_id": sid})
        assert _floating_layers(view_min["windows"][0])[0].get("state") == "minimized"
        r_min, p_min, rep_min = ctx.headless_render(
            out_name="f22_min.png",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="off",
        )
        assert r_min.returncode == 0 and rep_min is not None and rep_min.windows >= 1 and rep_min.ready_panes >= 1

        srv.api_call("session.set_layer_state", {"session_id": sid, "layer_id": lid, "state": "normal", "focus": True})
        r_norm, p_norm, rep_norm = ctx.headless_render(
            out_name="f22_norm.png",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="off",
        )
        assert r_norm.returncode == 0 and rep_norm is not None and rep_norm.windows >= 1 and rep_norm.ready_panes >= 1

    img_min = assert_valid_png(p_min, expected_width=1440, expected_height=900)
    img_norm = assert_valid_png(p_norm, expected_width=1440, expected_height=900)
    assert pixel_diff_stats(img_min, img_norm)["diff_fraction"] > 0.02


def _f22_t2_05_dock_floating_block_back_into_split(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        created = srv.new_session(label="f22-dock", command=["sleep", "10"])
        sid = created["session_id"]
        b1 = created["initial_windows"][0]["block_ids"][0]
        sp = srv.api_call(
            "session.new_split",
            {
                "session_id": sid,
                "anchor_id": b1,
                "direction": "horizontal",
                "side": "after",
                "layout": {"block": {"flavor": "com.superlogical.terminal.shell", "label": "p2"}},
            },
        )
        b2 = sp["block_ids"][0]
        srv.api_call("session.float_block", {"session_id": sid, "block_id": b2, "bounds": [0.2, 0.2, 0.6, 0.6]})
        srv.api_call(
            "session.move_block",
            {"session_id": sid, "block_id": b2, "anchor_block_id": b1, "direction": "horizontal", "side": "after"},
        )
        view = srv.api_call("session.view", {"session_id": sid})
        win = view["windows"][0]
        assert _floating_layers(win) == [], f"Expected floating layer removed after dock: {win}"
        assert len(_tiled_layer(win)["blocks"]) == 2

        res, png_path, report = ctx.headless_render(
            out_name="f22_docked.png",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        assert res.returncode == 0, res.clean_stderr
        assert report is not None and report.windows >= 1 and report.ready_panes >= 2
        assert_valid_png(png_path, expected_width=1440, expected_height=900)


# ---------------------------------------------------------------------------
# F23: True Multi-Window & Tab Tear-Off (MoveTabToNewWindow)
# ---------------------------------------------------------------------------


def _f23_t1_01_multi_window_scenario_renders(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    res, png_path, report = ctx.headless_render(
        out_name="f23_mw.png",
        scenario="multi-window",
        rust_log="warn",
    )
    assert res.returncode == 0, f"multi-window scenario failed: {res.clean_stderr}"
    assert report is not None and report.draws > 0 and report.glyphs > 0
    img = assert_valid_png(png_path, expected_width=1440, expected_height=900, min_bytes=4096)
    assert_not_solid_color(img, min_unique_colors=6)


def _f23_t1_02_cli_class_and_title_flags_accepted(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    res, png_path, report = ctx.headless_render(
        out_name="f23_flags.png",
        extra_args=["--class", "omarex-test-class", "--title", "Custom Test Title"],
        rust_log="warn",
    )
    assert res.returncode == 0, res.clean_stderr
    assert not res.has_error and report is not None
    assert_valid_png(png_path, expected_width=1440, expected_height=900)


def _f23_t1_03_transfer_window_rpc_between_sessions(ctx: TestContext) -> None:
    _require_rex()
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        s1 = srv.new_session(label="f23-src", command=["sleep", "10"])
        sid1 = s1["session_id"]
        w1 = s1["initial_windows"][0]["window_id"]
        nw = srv.api_call(
            "session.new_window",
            {"session_id": sid1, "window_label": "tearoff", "layout": {"block": {"flavor": "com.superlogical.terminal.shell"}}},
        )
        w2 = nw["window_id"]
        w2_blocks = nw["block_ids"]

        s2 = srv.new_session(label="f23-dst", command=["sleep", "10"])
        sid2 = s2["session_id"]
        w3 = s2["initial_windows"][0]["window_id"]

        tr = srv.api_call(
            "session.transfer_window",
            {"session_id": sid1, "window_id": w2, "to_session_id": sid2, "focus": True},
        )
        assert tr["revision"] >= 1 and tr["to_revision"] >= 1
        v1 = srv.api_call("session.view", {"session_id": sid1})
        v2 = srv.api_call("session.view", {"session_id": sid2})
        assert [w["window_id"] for w in v1["windows"]] == [w1]
        assert [w["window_id"] for w in v2["windows"]] == [w3, w2]
        moved_win = [w for w in v2["windows"] if w["window_id"] == w2][0]
        moved_blocks = [b["block_id"] for b in _tiled_layer(moved_win)["blocks"]]
        assert moved_blocks == w2_blocks


def _f23_t1_04_transfer_window_negative_errors(ctx: TestContext) -> None:
    _require_rex()
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        s1 = srv.new_session(label="f23-neg1", command=["sleep", "10"])
        sid1 = s1["session_id"]
        w1 = s1["initial_windows"][0]["window_id"]
        s2 = srv.new_session(label="f23-neg2", command=["sleep", "10"])
        sid2 = s2["session_id"]

        try:
            srv.api_call(
                "session.transfer_window",
                {"session_id": sid1, "window_id": w1, "to_session_id": sid1},
            )
        except RuntimeError as exc:
            assert "bad_request" in str(exc), str(exc)
        else:
            raise AssertionError("Expected bad_request on same-session transfer_window")

        try:
            srv.api_call(
                "session.transfer_window",
                {"session_id": sid1, "window_id": "window:11111111-1111-4111-8111-111111111111", "to_session_id": sid2},
            )
        except RuntimeError as exc:
            assert "not_found" in str(exc), str(exc)
        else:
            raise AssertionError("Expected not_found on nonexistent window_id")


def _f23_t1_05_multi_window_scenario_and_isolated_report(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    res_sc, png_sc, rep_sc = ctx.headless_render(
        out_name="f23_sc_mw.png",
        scenario="multi-window",
        rust_log="warn",
    )
    assert res_sc.returncode == 0, res_sc.clean_stderr
    assert rep_sc is not None and rep_sc.draws > 0 and rep_sc.quads > 0 and rep_sc.glyphs > 0
    img_sc = assert_valid_png(png_sc, expected_width=1440, expected_height=900, min_bytes=4096)
    assert_not_solid_color(img_sc, min_unique_colors=6)

    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        s1 = srv.new_session(label="f23-mw-rex", command=["sleep", "10"])
        sid = s1["session_id"]
        srv.api_call(
            "session.new_window",
            {"session_id": sid, "window_label": "win-2", "layout": {"block": {"flavor": "com.superlogical.terminal.shell"}}},
        )
        res_rex, png_rex, rep_rex = ctx.headless_render(
            out_name="f23_rex_mw.png",
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        assert res_rex.returncode == 0, res_rex.clean_stderr
        assert rep_rex is not None and rep_rex.windows >= 2 and rep_rex.ready_panes >= 1
        assert_valid_png(png_rex, expected_width=1440, expected_height=900)


def _f23_t2_01_multi_window_differs_from_sidebar_dark(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    r_sb, p_sb, _ = ctx.headless_render(out_name="f23_sb.png", scenario="sidebar-dark", rust_log="off")
    assert r_sb.returncode == 0, r_sb.clean_stderr
    r_mw, p_mw, _ = ctx.headless_render(out_name="f23_mw.png", scenario="multi-window", rust_log="off")
    assert r_mw.returncode == 0, r_mw.clean_stderr

    img_sb = assert_valid_png(p_sb, expected_width=1440, expected_height=900)
    img_mw = assert_valid_png(p_mw, expected_width=1440, expected_height=900)
    assert_not_solid_color(img_mw, min_unique_colors=6)
    assert pixel_diff_stats(img_sb, img_mw)["diff_fraction"] > 0.02


def _f23_t2_02_transfer_window_preserves_scrollback_and_renders(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        s1 = srv.new_session(label="tearoff-src", command=["sleep", "10"])
        sid1 = s1["session_id"]
        nw = srv.api_call(
            "session.new_window",
            {
                "session_id": sid1,
                "window_label": "marker-win",
                "layout": {
                    "block": {
                        "flavor": "com.superlogical.terminal.shell",
                        "options": {"command": ["bash", "--norc", "--noprofile", "-c", "echo TEAROFF_MARKER_42; sleep 10"]},
                    }
                },
            },
        )
        w2 = nw["window_id"]
        b2 = nw["block_ids"][0]
        _poll_until(
            lambda: "TEAROFF_MARKER_42"
            in srv.api_call(
                "com.superlogical.terminal.format",
                {"session_id": sid1, "block_id": b2, "args": {"format": "text"}},
            ).get("content", ""),
            timeout=3.0,
        )

        s2 = srv.new_session(label="tearoff-dst", command=["sleep", "10"])
        sid2 = s2["session_id"]
        srv.api_call("session.transfer_window", {"session_id": sid1, "window_id": w2, "to_session_id": sid2, "focus": True})

        fmt = srv.api_call(
            "com.superlogical.terminal.format",
            {"session_id": sid2, "block_id": b2, "args": {"format": "text"}},
        )
        assert "TEAROFF_MARKER_42" in fmt["content"], fmt

        res, png_path, report = ctx.headless_render(
            out_name="f23_tearoff_dst.png",
            extra_args=["tearoff-dst"],
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        assert res.returncode == 0, res.clean_stderr
        assert report is not None and report.windows >= 1 and report.ready_panes >= 1
        assert report.session == "tearoff-dst", f"Expected session 'tearoff-dst', got {report}"
        assert_valid_png(png_path, expected_width=1440, expected_height=900)


def _f23_t2_03_closing_torn_off_window_keeps_source_intact(ctx: TestContext) -> None:
    _require_rex()
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    with IsolatedRexServer(keep_tmp=ctx.keep_tmp) as srv:
        s1 = srv.new_session(label="tearoff-src", command=["sleep", "10"])
        sid1 = s1["session_id"]
        w1 = s1["initial_windows"][0]["window_id"]
        w2 = srv.api_call(
            "session.new_window",
            {"session_id": sid1, "window_label": "sec", "layout": {"block": {"flavor": "com.superlogical.terminal.shell"}}},
        )["window_id"]

        s2 = srv.new_session(label="tearoff-dst", command=["sleep", "10"])
        sid2 = s2["session_id"]
        srv.api_call("session.transfer_window", {"session_id": sid1, "window_id": w2, "to_session_id": sid2})
        srv.api_call("session.close_window", {"session_id": sid2, "window_id": w2})

        view1 = srv.api_call("session.view", {"session_id": sid1})
        assert [w["window_id"] for w in view1["windows"]] == [w1]

        res, png_path, report = ctx.headless_render(
            out_name="f23_prim_ok.png",
            extra_args=["tearoff-src"],
            rex_mode="custom_socket",
            custom_socket=srv.endpoint,
            rust_log="warn",
        )
        assert res.returncode == 0, res.clean_stderr
        assert report is not None and report.windows >= 1 and report.ready_panes >= 1
        assert report.session == "tearoff-src", f"Expected session 'tearoff-src', got {report}"
        assert_valid_png(png_path, expected_width=1440, expected_height=900)


def _f23_t2_04_multi_window_shared_atlas_scales(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    res_1, png_1, rep_1 = ctx.headless_render(
        out_name="f23_1x.png",
        width=1280,
        height=720,
        scale=1.0,
        scenario="multi-window",
        rust_log="off",
    )
    assert res_1.returncode == 0, res_1.clean_stderr
    assert rep_1 is not None and rep_1.draws > 0 and rep_1.glyphs > 0
    assert abs(rep_1.scale - 1.0) < 1e-3, f"Expected scale 1.0, got {rep_1.scale}"
    img_1 = assert_valid_png(png_1, expected_width=1280, expected_height=720, expected_color_type=6)
    assert_not_solid_color(img_1, min_unique_colors=6)

    res_2, png_2, rep_2 = ctx.headless_render(
        out_name="f23_2x.png",
        width=1280,
        height=720,
        scale=2.0,
        scenario="multi-window",
        rust_log="off",
    )
    assert res_2.returncode == 0, res_2.clean_stderr
    assert rep_2 is not None and rep_2.draws > 0 and rep_2.glyphs > 0
    assert abs(rep_2.scale - 2.0) < 1e-3, f"Expected scale 2.0, got {rep_2.scale}"
    img_2 = assert_valid_png(png_2, expected_width=1280, expected_height=720, expected_color_type=6)
    assert_not_solid_color(img_2, min_unique_colors=6)

    stats = pixel_diff_stats(img_1, img_2)
    assert stats["diff_fraction"] > 0.10, f"Expected 1.0x vs 2.0x multi-window scale diff > 0.10: {stats}"


def _f23_t2_05_multi_window_honors_isolated_xdg_theme_and_settings(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(LIGHT_COLORS_TOML, theme_name="mock-light")
    ctx.write_settings({"sidebar_behavior": "hidden"})
    res, png_path, report = ctx.headless_render(
        out_name="f23_xdg_light.png",
        scenario="multi-window",
        rust_log="off",
    )
    assert res.returncode == 0, res.clean_stderr
    assert report is not None
    img = assert_valid_png(png_path, expected_width=1440, expected_height=900)
    lum = relative_luminance(sample_pixel(img, 2, 2)[:3])
    assert lum > 0.50, f"Expected isolated light theme luminance > 0.50, got {lum:.3f}"


# ---------------------------------------------------------------------------
# F24: Automated --headless-render & --headless-scenario QA Suite
# ---------------------------------------------------------------------------


def _f24_t1_01_headless_default_geometry_and_report(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    res, png_path, report = ctx.headless_render(out_name="f24_default.png", rust_log="warn")
    assert res.returncode == 0, res.clean_stderr
    assert report is not None
    assert report.width == 1440 and report.height == 900
    assert abs(report.scale - 1.0) < 1e-3
    assert report.draws > 0 and report.quads > 0 and report.frame_us > 0
    assert_valid_png(png_path, expected_width=1440, expected_height=900, expected_color_type=6)


def _f24_t1_02_headless_geometry_variants(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    for w, h, sc, fname in ((320, 240, 0.5, "f24_small.png"), (1920, 1080, 1.5, "f24_fhd.png")):
        res, png_path, report = ctx.headless_render(
            out_name=fname,
            width=w,
            height=h,
            scale=sc,
            rust_log="off",
        )
        assert res.returncode == 0, res.clean_stderr
        assert report is not None
        assert report.width == w and report.height == h and abs(report.scale - sc) < 1e-3
        img = assert_valid_png(png_path, expected_width=w, expected_height=h, min_bytes=1024)
        assert_not_solid_color(img, min_unique_colors=2)


def _f24_t1_03_headless_light_flag_forces_light_palette(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    res, png_path, _ = ctx.headless_render(
        out_name="f24_light.png",
        width=640,
        height=480,
        light=True,
        rust_log="off",
    )
    assert res.returncode == 0
    img = assert_valid_png(png_path, expected_width=640, expected_height=480)
    assert relative_luminance(sample_pixel(img, 2, 2)[:3]) > 0.50
    assert_not_solid_color(img, min_unique_colors=4)


def _f24_t1_04_headless_viewer_repeatable_paths(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    bundle = create_fixtures(ctx.work_dir / "fx", generate_mp3=False, init_git=False)

    r_base, p_base, rep_base = ctx.headless_render(out_name="f24_vbase.png", rust_log="off")
    assert r_base.returncode == 0 and rep_base is not None

    r_view, p_view, rep_view = ctx.headless_render(
        out_name="f24_vopen.png",
        viewer_paths=[f"{bundle.sample_rs}:10", bundle.sample_md],
        rust_log="off",
    )
    assert r_view.returncode == 0 and rep_view is not None

    img_base = assert_valid_png(p_base, expected_width=1440, expected_height=900)
    img_view = assert_valid_png(p_view, expected_width=1440, expected_height=900)
    assert pixel_diff_stats(img_base, img_view)["diff_fraction"] > 0.03
    assert rep_view.quads >= rep_base.quads


def _f24_t1_05_help_and_all_13_scenarios_accepted(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    help_res = ctx.run_omarex(["--help"], timeout=3.0)
    assert help_res.returncode == 0, help_res.clean_stderr
    assert "--headless-scenario" in help_res.clean_stdout, (
        "omarex --help missing --headless-scenario option"
    )
    for idx, sc in enumerate(ALL_13_SCENARIOS):
        res, png_path, report = ctx.headless_render(
            out_name=f"f24_t1_{idx:02d}_{sc}.png",
            width=640,
            height=480,
            scenario=sc,
            rust_log="off",
        )
        assert res.returncode == 0, f"Scenario {sc!r} failed: {res.clean_stderr}"
        assert report is not None, f"Scenario {sc!r} missing HeadlessReport"
        assert_valid_png(png_path, expected_width=640, expected_height=480)


def _f24_t2_01_all_13_scenarios_non_solid_rgba8(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    create_fixtures(ctx.work_dir / "fx", generate_mp3=False, init_git=False)
    for idx, sc in enumerate(ALL_13_SCENARIOS):
        res, png_path, report = ctx.headless_render(
            out_name=f"f24_full_{idx:02d}_{sc}.png",
            scenario=sc,
            rust_log="warn",
        )
        assert res.returncode == 0, f"Scenario {sc!r} failed: {res.clean_stderr}"
        assert report is not None
        assert report.draws > 0 and report.quads > 0 and report.glyphs > 0, f"{sc}: {report}"
        img = assert_valid_png(
            png_path,
            expected_width=1440,
            expected_height=900,
            min_bytes=4096,
            expected_color_type=6,
        )
        assert_not_solid_color(img, min_unique_colors=6)


def _f24_t2_02_sidebar_dark_vs_light_luminance_inversion(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    r_d, p_d, _ = ctx.headless_render(out_name="f24_sb_d.png", scenario="sidebar-dark", rust_log="off")
    assert r_d.returncode == 0, r_d.clean_stderr
    r_l, p_l, _ = ctx.headless_render(out_name="f24_sb_l.png", scenario="sidebar-light", rust_log="off")
    assert r_l.returncode == 0, r_l.clean_stderr

    img_d = assert_valid_png(p_d, expected_width=1440, expected_height=900)
    img_l = assert_valid_png(p_l, expected_width=1440, expected_height=900)
    assert pixel_diff_stats(img_d, img_l)["diff_fraction"] > 0.25
    lum_d = relative_luminance(sample_pixel(img_d, 2, 2)[:3])
    lum_l = relative_luminance(sample_pixel(img_l, 2, 2)[:3])
    assert lum_l > lum_d + 0.30, f"Expected sidebar-light lum ({lum_l:.3f}) > dark ({lum_d:.3f}) + 0.30"


def _f24_t2_03_invalid_headless_scenario_fails_cleanly(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    res, _, _ = ctx.headless_render(
        out_name="f24_invalid_sc.png",
        scenario="unknown-bogus-scenario",
        rust_log="warn",
    )
    assert res.returncode != 0, "Expected non-zero exit for unknown scenario"
    assert not res.crashed, "Process crashed with signal"
    assert "Error:" in res.clean_stderr, f"Expected Error: in stderr, got {res.clean_stderr!r}"
    assert "unknown option" not in res.clean_stderr.lower(), (
        f"--headless-scenario flag itself is unimplemented (got 'unknown option'): {res.clean_stderr!r}"
    )


def _f24_t2_04_pairwise_scenario_visual_distinctness(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    pairs = (
        ("syntax-viewer", "markdown-viewer"),
        ("diagnostics-hud", "settings-modal"),
        ("split-panes", "floating-pr"),
        ("ports-pill", "dnd-split"),
        ("multi-window", "sidebar-dark"),
    )
    rendered: dict[str, Any] = {}
    for a, b in pairs:
        for sc in (a, b):
            if sc not in rendered:
                res, png_path, _ = ctx.headless_render(out_name=f"f24_pair_{sc}.png", scenario=sc, rust_log="off")
                assert res.returncode == 0, f"Scenario {sc!r} failed: {res.clean_stderr}"
                rendered[sc] = assert_valid_png(png_path, expected_width=1440, expected_height=900)
        stats = pixel_diff_stats(rendered[a], rendered[b])
        assert stats["diff_fraction"] > 0.01, f"Scenarios {a!r} and {b!r} appear aliased: {stats}"


def _f24_t2_05_scale_density_and_repeat_determinism(ctx: TestContext) -> None:
    ctx.write_omarchy_theme(DARK_COLORS_TOML, theme_name="mock-dark")
    r1, p1, _ = ctx.headless_render(out_name="f24_det1.png", width=800, height=600, scale=1.0, scenario="sidebar-dark", rust_log="off")
    assert r1.returncode == 0, r1.clean_stderr
    r2, p2, _ = ctx.headless_render(out_name="f24_det2.png", width=800, height=600, scale=1.0, scenario="sidebar-dark", rust_log="off")
    assert r2.returncode == 0, r2.clean_stderr
    r_hi, p_hi, _ = ctx.headless_render(out_name="f24_hi.png", width=800, height=600, scale=2.0, scenario="sidebar-dark", rust_log="off")
    assert r_hi.returncode == 0, r_hi.clean_stderr

    img1 = assert_valid_png(p1, expected_width=800, expected_height=600)
    img2 = assert_valid_png(p2, expected_width=800, expected_height=600)
    img_hi = assert_valid_png(p_hi, expected_width=800, expected_height=600)
    assert pixel_diff_stats(img1, img2)["diff_fraction"] < 0.01
    assert pixel_diff_stats(img1, img_hi)["diff_fraction"] > 0.10


# ---------------------------------------------------------------------------
# Registration (30 tests: 15 Tier 1 + 15 Tier 2)
# ---------------------------------------------------------------------------


def register_tests(registry: TestRegistry) -> None:
    cases = [
        # F22
        ("F22-T1-01", 1, "F22", "dnd-split headless scenario renders valid non-solid PNG", _f22_t1_01_dnd_split_scenario_renders),
        ("F22-T1-02", 1, "F22", "floating-pr headless scenario renders valid non-solid PNG", _f22_t1_02_floating_pr_scenario_renders),
        ("F22-T1-03", 1, "F22", "session.swap_blocks swaps split blocks and rejects self-swap", _f22_t1_03_swap_blocks_rpc_and_negative),
        ("F22-T1-04", 1, "F22", "session.move_block reparents block to vertical split", _f22_t1_04_move_block_reparents_to_vertical_split),
        ("F22-T1-05", 1, "F22", "session.move_window reorders tabs deterministically", _f22_t1_05_move_window_reorders_tabs),
        ("F22-T2-01", 2, "F22", "dnd-split renders 5-zone drop overlay distinct from split-panes", _f22_t2_01_dnd_split_differs_from_split_panes),
        ("F22-T2-02", 2, "F22", "floating-pr renders elevated floating layer over tiled pane", _f22_t2_02_floating_pr_renders_elevated_layer),
        ("F22-T2-03", 2, "F22", "FloatOrDock with empty tiled layer renders cleanly without panic", _f22_t2_03_float_last_tiled_block_empty_tiled_layer),
        ("F22-T2-04", 2, "F22", "minimized vs normal floating layer renders cleanly and differs", _f22_t2_04_minimized_floating_layer_state),
        ("F22-T2-05", 2, "F22", "docking floating block back into split tree updates layout and panes", _f22_t2_05_dock_floating_block_back_into_split),
        # F23
        ("F23-T1-01", 1, "F23", "multi-window headless scenario renders valid non-solid PNG", _f23_t1_01_multi_window_scenario_renders),
        ("F23-T1-02", 1, "F23", "CLI --class and --title flags accepted in headless render", _f23_t1_02_cli_class_and_title_flags_accepted),
        ("F23-T1-03", 1, "F23", "session.transfer_window moves window cleanly between sessions", _f23_t1_03_transfer_window_rpc_between_sessions),
        ("F23-T1-04", 1, "F23", "session.transfer_window rejects same_session and unknown window", _f23_t1_04_transfer_window_negative_errors),
        ("F23-T1-05", 1, "F23", "multi-window scenario and 2-window IsolatedRexServer report", _f23_t1_05_multi_window_scenario_and_isolated_report),
        ("F23-T2-01", 2, "F23", "multi-window scenario differs visibly from sidebar-dark", _f23_t2_01_multi_window_differs_from_sidebar_dark),
        ("F23-T2-02", 2, "F23", "session.transfer_window preserves scrollback and renders attached", _f23_t2_02_transfer_window_preserves_scrollback_and_renders),
        ("F23-T2-03", 2, "F23", "closing torn-off window leaves source session intact", _f23_t2_03_closing_torn_off_window_keeps_source_intact),
        ("F23-T2-04", 2, "F23", "multi-window shared GPU/atlas renders across scale 1.0 and 2.0", _f23_t2_04_multi_window_shared_atlas_scales),
        ("F23-T2-05", 2, "F23", "multi-window honors isolated XDG light colors.toml and settings", _f23_t2_05_multi_window_honors_isolated_xdg_theme_and_settings),
        # F24
        ("F24-T1-01", 1, "F24", "--headless-render default 1440x900 @1.00x PNG and HeadlessReport", _f24_t1_01_headless_default_geometry_and_report),
        ("F24-T1-02", 1, "F24", "--width --height --scale geometry variants match PNG IHDR", _f24_t1_02_headless_geometry_variants),
        ("F24-T1-03", 1, "F24", "--light forces light palette over dark XDG theme", _f24_t1_03_headless_light_flag_forces_light_palette),
        ("F24-T1-04", 1, "F24", "--headless-viewer repeatable paths alter render and quads", _f24_t1_04_headless_viewer_repeatable_paths),
        ("F24-T1-05", 1, "F24", "--help documents --headless-scenario and all 13 scenarios accepted", _f24_t1_05_help_and_all_13_scenarios_accepted),
        ("F24-T2-01", 2, "F24", "all 13 coordinated --headless-scenario renders produce non-solid RGBA8 PNGs", _f24_t2_01_all_13_scenarios_non_solid_rgba8),
        ("F24-T2-02", 2, "F24", "sidebar-dark vs sidebar-light pixel diff and luminance inversion", _f24_t2_02_sidebar_dark_vs_light_luminance_inversion),
        ("F24-T2-03", 2, "F24", "invalid --headless-scenario fails cleanly without unknown-option", _f24_t2_03_invalid_headless_scenario_fails_cleanly),
        ("F24-T2-04", 2, "F24", "pairwise visual distinctness across UI scenario pairs", _f24_t2_04_pairwise_scenario_visual_distinctness),
        ("F24-T2-05", 2, "F24", "--scale 2.0 vs 1.0 layout scaling and repeat-render determinism", _f24_t2_05_scale_density_and_repeat_determinism),
    ]
    for cid, tier, feat, name, fn in cases:
        registry.add(TestCase(id=cid, tier=tier, feature=feat, name=name, fn=fn))
