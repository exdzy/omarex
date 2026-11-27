"""Shared opaque-box E2E test library for omarex (lazy-loaded to keep `python3 -m lib.harness` clean)."""

from __future__ import annotations

import importlib
from typing import Any

_EXPORTS: dict[str, tuple[str, str]] = {
    "CommandResult": (".harness", "CommandResult"),
    "HeadlessReport": (".harness", "HeadlessReport"),
    "SkipTest": (".harness", "SkipTest"),
    "TestCase": (".harness", "TestCase"),
    "TestContext": (".harness", "TestContext"),
    "TestRegistry": (".harness", "TestRegistry"),
    "discover_cases": (".harness", "discover_cases"),
    "parse_headless_report": (".harness", "parse_headless_report"),
    "strip_ansi": (".harness", "strip_ansi"),
    "DARK_COLORS_TOML": (".fixtures", "DARK_COLORS_TOML"),
    "LIGHT_COLORS_TOML": (".fixtures", "LIGHT_COLORS_TOML"),
    "FixtureBundle": (".fixtures", "FixtureBundle"),
    "create_fixtures": (".fixtures", "create_fixtures"),
    "install_theme_into_xdg": (".fixtures", "install_theme_into_xdg"),
    "tool_capabilities": (".fixtures", "tool_capabilities"),
    "PngImage": (".png_check", "PngImage"),
    "assert_color_near": (".png_check", "assert_color_near"),
    "assert_not_solid_color": (".png_check", "assert_not_solid_color"),
    "assert_valid_png": (".png_check", "assert_valid_png"),
    "mean_color_in_rect": (".png_check", "mean_color_in_rect"),
    "pixel_diff_stats": (".png_check", "pixel_diff_stats"),
    "read_png": (".png_check", "read_png"),
    "relative_luminance": (".png_check", "relative_luminance"),
    "sample_pixel": (".png_check", "sample_pixel"),
    "unique_color_count_sample": (".png_check", "unique_color_count_sample"),
    "LIVE_ENDPOINT": (".rex_helper", "LIVE_ENDPOINT"),
    "LIVE_SOCKET_PATH": (".rex_helper", "LIVE_SOCKET_PATH"),
    "IsolatedRexServer": (".rex_helper", "IsolatedRexServer"),
    "live_socket_present": (".rex_helper", "live_socket_present"),
    "probe_bad_endpoint": (".rex_helper", "probe_bad_endpoint"),
    "probe_live_server": (".rex_helper", "probe_live_server"),
    "rex_available": (".rex_helper", "rex_available"),
    "run_rex_isolated": (".rex_helper", "run_rex_isolated"),
    "run_rex_readonly": (".rex_helper", "run_rex_readonly"),
}

__all__ = list(_EXPORTS.keys())


def __getattr__(name: str) -> Any:
    if name in _EXPORTS:
        mod_rel, attr = _EXPORTS[name]
        mod = importlib.import_module(mod_rel, __name__)
        val = getattr(mod, attr)
        globals()[name] = val
        return val
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")
