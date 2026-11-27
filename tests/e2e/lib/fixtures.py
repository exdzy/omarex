"""Deterministic fixture generator for opaque-box omarex E2E tests."""

from __future__ import annotations

import math
import shutil
import struct
import subprocess
import wave
import zlib
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

DARK_COLORS_TOML = """mode = "dark"

accent = "#89b4fa"
selection = "#45475a"
muted = "#585b70"

background = "#1a0b2e"
dark_background = "#130822"
darker_background = "#0d0518"
lighter_background = "#2a1647"

foreground = "#cdd6f4"
dark_foreground = "#6c7086"
light_foreground = "#bac2de"
bright_foreground = "#cdd6f4"

red = "#f38ba8"
yellow = "#f9e2af"
orange = "#fab387"
green = "#a6e3a1"
cyan = "#94e2d5"
blue = "#89b4fa"
magenta = "#cba6f7"
brown = "#7b5b55"

bright_red = "#f38ba8"
bright_yellow = "#f9e2af"
bright_green = "#a6e3a1"
bright_cyan = "#94e2d5"
bright_blue = "#89b4fa"
bright_magenta = "#f5c2e7"
"""

LIGHT_COLORS_TOML = """mode = "light"

accent = "#1e66f5"
selection = "#ccd0da"
muted = "#acb0be"

background = "#eff1f5"
dark_background = "#e3e4e8"
darker_background = "#d7d8dc"
lighter_background = "#dce0e8"

foreground = "#4c4f69"
dark_foreground = "#9ca0b0"
light_foreground = "#5c5f77"
bright_foreground = "#4c4f69"

red = "#d20f39"
yellow = "#df8e1d"
orange = "#fe640b"
green = "#40a02b"
cyan = "#179299"
blue = "#1e66f5"
magenta = "#8839ef"
brown = "#6c2715"

bright_red = "#d20f39"
bright_yellow = "#df8e1d"
bright_green = "#40a02b"
bright_cyan = "#179299"
bright_blue = "#1e66f5"
bright_magenta = "#ea76cb"
"""


def tool_capabilities() -> dict[str, bool]:
    """Report availability of external CLI tools used by fixtures & E2E tests."""
    tools = (
        "rex",
        "ffmpeg",
        "ffprobe",
        "pdftoppm",
        "pdftotext",
        "pw-play",
        "pw-cat",
        "git",
        "bash",
        "jq",
    )
    return {name: shutil.which(name) is not None for name in tools}


def _png_chunk(chunk_type: bytes, data: bytes) -> bytes:
    crc = zlib.crc32(chunk_type + data) & 0xFFFFFFFF
    return struct.pack(">I", len(data)) + chunk_type + data + struct.pack(">I", crc)


def write_sample_png(path: Path, width: int = 64, height: int = 48) -> None:
    """Write a deterministic 4-quadrant RGBA8 PNG using Python stdlib only."""
    raw = bytearray()
    for y in range(height):
        raw.append(0)  # Filter type 0 (None)
        top = y < height // 2
        for x in range(width):
            left = x < width // 2
            if top and left:
                r, g, b, a = (235, 64, 52, 255)
            elif top and not left:
                r, g, b, a = (52, 168, 83, 255)
            elif not top and left:
                r, g, b, a = (66, 133, 244, 255)
            else:
                r, g, b, a = (251, 188, 5, 255)
            raw.extend((r, g, b, a))

    ihdr = struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0)
    idat = zlib.compress(bytes(raw), level=6)
    png_bytes = (
        b"\x89PNG\r\n\x1a\n"
        + _png_chunk(b"IHDR", ihdr)
        + _png_chunk(b"IDAT", idat)
        + _png_chunk(b"IEND", b"")
    )
    path.write_bytes(png_bytes)


def write_sample_svg(path: Path) -> None:
    svg = """<svg xmlns="http://www.w3.org/2000/svg" width="64" height="32" viewBox="0 0 64 32">
  <rect x="0" y="0" width="64" height="32" fill="#1e1e2e"/>
  <rect x="4" y="4" width="24" height="24" rx="4" fill="#89b4fa"/>
  <circle cx="46" cy="16" r="10" fill="#a6e3a1"/>
</svg>
"""
    path.write_text(svg, encoding="utf-8")


def write_sample_gif(path: Path) -> None:
    """Write a deterministic 2-frame animated GIF89a (16x16, 60ms / 120ms delays)."""
    # Global color table: 4 entries (red, green, blue, yellow)
    gct = bytes(
        [
            220, 40, 40,
            40, 200, 90,
            60, 120, 240,
            240, 210, 40,
        ]
    )
    header = b"GIF89a" + struct.pack("<HHBBB", 16, 16, 0x91, 0, 0) + gct
    netscape = (
        b"\x21\xff\x0bNETSCAPE2.0\x03\x01\x00\x00\x00"
    )

    def _frame(color_idx: int, delay_cs: int) -> bytes:
        gce = struct.pack("<BBBBHB", 0x21, 0xF9, 0x04, 0x04, delay_cs, 0) + b"\x00"
        img_desc = b"\x2c" + struct.pack("<HHHHB", 0, 0, 16, 16, 0)
        # 2-bit LZW minimum code size = 2; clear=4, end=5
        # Encode 256 pixels cleanly in small sub-blocks without compression tricks:
        # Using uncompressed LZW stream: emit clear code (4) frequently every few pixels
        # Pack 3-bit codes (clear=4, pixel=color_idx, end=5) LSB-first.
        bits = 0
        nbits = 0
        out = bytearray()

        def emit(code: int) -> None:
            nonlocal bits, nbits
            bits |= code << nbits
            nbits += 3
            while nbits >= 8:
                out.append(bits & 0xFF)
                bits >>= 8
                nbits -= 8

        emit(4)  # clear
        for i in range(256):
            if i > 0 and i % 3 == 0:
                emit(4)  # reset dictionary before code width grows past 3 bits
            emit(color_idx)
        emit(5)  # EOI
        if nbits > 0:
            out.append(bits & 0xFF)

        blocks = bytearray([2])  # LZW minimum code size = 2
        pos = 0
        while pos < len(out):
            chunk = out[pos : pos + 255]
            blocks.append(len(chunk))
            blocks.extend(chunk)
            pos += len(chunk)
        blocks.append(0)  # block terminator
        return gce + img_desc + bytes(blocks)

    gif_bytes = header + netscape + _frame(0, 6) + _frame(1, 12) + b"\x3b"
    path.write_bytes(gif_bytes)


def write_sample_wav(path: Path, duration_s: float = 1.0, freq_hz: float = 440.0) -> None:
    """Write a deterministic 16kHz 16-bit mono PCM WAV sine tone."""
    sample_rate = 16000
    total_frames = int(sample_rate * duration_s)
    amp = 14000
    samples = bytearray()
    for i in range(total_frames):
        val = int(amp * math.sin(2.0 * math.pi * freq_hz * i / sample_rate))
        samples.extend(struct.pack("<h", val))

    with wave.open(str(path), "wb") as wf:
        wf.setnchannels(1)
        wf.setsampwidth(2)
        wf.setframerate(sample_rate)
        wf.writeframes(bytes(samples))


def write_sample_pdf(path: Path) -> None:
    """Write a deterministic 2-page PDF 1.4 with valid computed xref offsets."""
    objects: list[bytes] = [
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>",
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 6 0 R >>",
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 7 0 R >>",
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
    ]

    p1_stream = b"BT /F1 18 Tf 72 720 Td (Omarex E2E Page 1) Tj ET"
    p2_stream = (
        b"0.1 0.2 0.8 rg 100 200 400 400 re f\n"
        b"BT /F1 18 Tf 72 720 Td (Omarex E2E Page 2) Tj ET"
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


def _write_sample_rust(path: Path) -> None:
    lines = [
        "// Opaque-box Rust syntax & ligature fixture for omarex E2E tests",
        "use std::collections::HashMap;",
        "use std::fmt::{self, Display};",
        "",
        "#[derive(Debug, Clone, PartialEq, Eq)]",
        "pub enum PipelineState {",
        "    Idle,",
        "    Active { port: u16, label: String },",
        "    Completed(usize),",
        "}",
        "",
        "pub trait Transform<T> {",
        "    fn apply(&self, input: T) -> Result<T, String>;",
        "}",
        "",
        "pub fn evaluate_operators(lhs: i64, rhs: i64) -> Result<&'static str, String> {",
        '    // Ligature tokens: -> => !== === :: |>',
        "    let strict_neq = \"!==\";",
        "    let strict_eq = \"===\";",
        "    let pipe_tok = \"|>\";",
        "    let _markers = (strict_neq, strict_eq, pipe_tok);",
        "    match (lhs == rhs, lhs != rhs) {",
        '        (true, false) => Ok("equal"),',
        '        (false, true) => Ok("distinct"),',
        '        _ => Err("unreachable".to_string()),',
        "    }",
        "}",
        "",
        "impl Display for PipelineState {",
        "    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {",
        "        match self {",
        '            Self::Idle => write!(f, "idle"),',
        '            Self::Active { port, label } => write!(f, "{label}:{port}"),',
        '            Self::Completed(n) => write!(f, "done({n})"),',
        "        }",
        "    }",
        "}",
        "",
        "pub fn build_lookup_table(count: usize) -> HashMap<usize, String> {",
        "    let mut map = HashMap::with_capacity(count);",
        "    for idx in 0..count {",
        '        map.insert(idx, format!("entry_{idx:03}"));',
        "    }",
        "    map",
        "}",
    ]
    for i in range(46, 95):
        lines.append(f"pub fn helper_line_{i:02}(v: i32) -> i32 {{ v + {i} }}")
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")


def _write_sample_python(path: Path) -> None:
    content = '''"""Python fixture for syntax highlighting and search."""

from dataclasses import dataclass
from typing import Callable


@dataclass(frozen=True)
class TokenSpec:
    kind: str
    pattern: str


def transform(items: list[int], fn: Callable[[int], int]) -> list[int]:
    # SEARCH_TARGET_ALPHA
    return [fn(x) for x in items if x >= 0]


if __name__ == "__main__":
    print(transform([1, 2, 3], lambda n: n * 2))
'''
    path.write_text(content, encoding="utf-8")


def _write_sample_js(path: Path) -> None:
    content = """// JS fixture with => !== === operators
export const formatRoute = (host, port) => {
  if (port !== 0 && host === "localhost") {
    return `http://${host}:${port}/health`;
  }
  return "http://127.0.0.1:8080/health";
};
"""
    path.write_text(content, encoding="utf-8")


def _write_sample_json(path: Path) -> None:
    content = """{
  "client": "omarex",
  "api_revision": 24,
  "features": ["ligatures", "powerline", "viewer-2.0", "diagnostics-hud"],
  "thresholds": [1, 2, 4, 8, 16]
}
"""
    path.write_text(content, encoding="utf-8")


def _write_powerline_sample(path: Path) -> None:
    lines = [
        "\ue0b6\ue0b0 main \ue0b0 src/lib.rs \ue0b1 \ue0b2 100% \ue0b4",
        "\ue0b8\ue0ba\ue0bc\ue0be\ue0c0\ue0c4\ue0c6\ue0c8\ue0cc\ue0d0\ue0d2\ue0d4\ue0d6",
        "\u250c\u2500\u2500\u2500\u252c\u2500\u2500\u2500\u2510",
        "\u2502 A \u2502 B \u2502",
        "\u2514\u2500\u2500\u2500\u2534\u2500\u2500\u2500\u2518",
    ]
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")


def _write_sample_md(path: Path) -> None:
    content = """# Omarex Agent Companion 2.0

## Operator & Syntax Rendering

```rust
fn next_port(cur: u16) -> Result<u16, &'static str> {
    match cur {
        0 => Err("invalid"),
        p => Ok(p + 1),
    }
}
```

```python
def greet(name: str) -> str:
    return f"hello {name}"
```

## Capabilities Table

| Feature | Category | Status |
|---|---|---|
| Run-shaping | R1 | Active |
| Markdown 2.0 | R2 | Active |
| Copy Mode | R3 | Active |

## Inline Local Image & Links

![Sample Quadrant Image](../media/sample.png)

- External link: [Example](https://example.com)
- Relative link: [Rust Sample](../code/sample.rs)
"""
    path.write_text(content, encoding="utf-8")


@dataclass(frozen=True)
class FixtureBundle:
    """Paths to all generated fixtures inside a test workspace."""

    root: Path
    code_dir: Path
    sample_rs: Path
    sample_py: Path
    sample_js: Path
    sample_json: Path
    powerline_txt: Path
    docs_dir: Path
    sample_md: Path
    sample_pdf: Path
    media_dir: Path
    sample_png: Path
    sample_svg: Path
    sample_jpg: Path
    anim_gif: Path
    tone_wav: Path
    tone_mp3: Optional[Path]
    git_repo: Path
    git_tracked_file: Path
    git_staged_file: Path
    git_untracked_file: Path
    themes_dir: Path
    dark_theme_dir: Path
    light_theme_dir: Path
    broken_theme_dir: Path


def install_theme_into_xdg(
    xdg_config: Path,
    xdg_state: Path,
    theme: str = "dark",
    use_symlink: bool = False,
) -> Path:
    """Install mock Omarchy themes into isolated XDG config/state dirs and select one."""
    themes_root = xdg_config / "omarchy" / "themes"
    dark_dir = themes_root / "mock-dark"
    light_dir = themes_root / "mock-light"
    broken_dir = themes_root / "broken-no-colors"
    dark_dir.mkdir(parents=True, exist_ok=True)
    light_dir.mkdir(parents=True, exist_ok=True)
    broken_dir.mkdir(parents=True, exist_ok=True)

    (dark_dir / "colors.toml").write_text(DARK_COLORS_TOML, encoding="utf-8")
    (light_dir / "colors.toml").write_text(LIGHT_COLORS_TOML, encoding="utf-8")
    (broken_dir / "README.md").write_text("Missing colors.toml intentionally\n", encoding="utf-8")

    current_dir = xdg_state / "omarchy" / "current"
    current_dir.mkdir(parents=True, exist_ok=True)

    selected_name = "mock-light" if theme == "light" else "mock-dark"
    selected_src = light_dir if theme == "light" else dark_dir
    (current_dir / "theme.name").write_text(f"{selected_name}\n", encoding="utf-8")

    theme_link = current_dir / "theme"
    if theme_link.is_symlink() or theme_link.is_file():
        theme_link.unlink()
    elif theme_link.is_dir():
        shutil.rmtree(theme_link)

    if use_symlink:
        theme_link.symlink_to(selected_src)
    else:
        theme_link.mkdir(parents=True, exist_ok=True)
        (theme_link / "colors.toml").write_text(
            (selected_src / "colors.toml").read_text(encoding="utf-8"),
            encoding="utf-8",
        )
    return theme_link


def create_fixtures(
    root: Path,
    generate_mp3: bool = True,
    init_git: bool = True,
) -> FixtureBundle:
    """Generate a complete deterministic fixture bundle under `root`."""
    root = Path(root)
    code_dir = root / "code"
    docs_dir = root / "docs"
    media_dir = root / "media"
    git_repo = root / "git_repo"
    themes_dir = root / "themes"

    for d in (code_dir, docs_dir, media_dir, themes_dir):
        d.mkdir(parents=True, exist_ok=True)

    sample_rs = code_dir / "sample.rs"
    sample_py = code_dir / "sample.py"
    sample_js = code_dir / "sample.js"
    sample_json = code_dir / "sample.json"
    powerline_txt = code_dir / "powerline_sample.txt"
    _write_sample_rust(sample_rs)
    _write_sample_python(sample_py)
    _write_sample_js(sample_js)
    _write_sample_json(sample_json)
    _write_powerline_sample(powerline_txt)

    sample_png = media_dir / "sample.png"
    sample_svg = media_dir / "sample.svg"
    sample_jpg = media_dir / "sample.jpg"
    anim_gif = media_dir / "anim.gif"
    tone_wav = media_dir / "tone.wav"
    tone_mp3: Optional[Path] = None

    write_sample_png(sample_png)
    write_sample_svg(sample_svg)
    write_sample_gif(anim_gif)
    write_sample_wav(tone_wav)

    ffmpeg_bin = shutil.which("ffmpeg")
    if ffmpeg_bin is not None:
        res_jpg = subprocess.run(
            [ffmpeg_bin, "-y", "-loglevel", "error", "-i", str(sample_png), str(sample_jpg)],
            capture_output=True,
            timeout=10.0,
            check=False,
        )
        if res_jpg.returncode != 0 or not sample_jpg.is_file():
            sample_jpg.write_bytes(b"\xff\xd8\xff\xe0\x00\x10JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00\xff\xd9")

        if generate_mp3:
            mp3_candidate = media_dir / "tone.mp3"
            res_mp3 = subprocess.run(
                [
                    ffmpeg_bin,
                    "-y",
                    "-loglevel",
                    "error",
                    "-f",
                    "wav",
                    "-i",
                    str(tone_wav),
                    "-codec:a",
                    "libmp3lame",
                    "-q:a",
                    "6",
                    str(mp3_candidate),
                ],
                capture_output=True,
                timeout=10.0,
                check=False,
            )
            if res_mp3.returncode == 0 and mp3_candidate.is_file():
                tone_mp3 = mp3_candidate
    else:
        sample_jpg.write_bytes(b"\xff\xd8\xff\xe0\x00\x10JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00\xff\xd9")

    sample_md = docs_dir / "sample.md"
    sample_pdf = docs_dir / "sample.pdf"
    _write_sample_md(sample_md)
    write_sample_pdf(sample_pdf)

    git_tracked_file = git_repo / "src" / "lib.rs"
    git_staged_file = git_repo / "staged.rs"
    git_untracked_file = git_repo / "untracked.txt"
    (git_repo / "src").mkdir(parents=True, exist_ok=True)
    (git_repo / "README.md").write_text("# Fixture Repo\n", encoding="utf-8")
    git_tracked_file.write_text(
        "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
        encoding="utf-8",
    )

    if init_git and shutil.which("git") is not None:
        subprocess.run(["git", "-C", str(git_repo), "init", "-q"], check=True, timeout=5.0)
        subprocess.run(["git", "-C", str(git_repo), "config", "user.name", "Omarex E2E"], check=True, timeout=5.0)
        subprocess.run(["git", "-C", str(git_repo), "config", "user.email", "e2e@example.invalid"], check=True, timeout=5.0)
        subprocess.run(["git", "-C", str(git_repo), "add", "README.md", "src/lib.rs"], check=True, timeout=5.0)
        subprocess.run(["git", "-C", str(git_repo), "commit", "-q", "-m", "initial commit"], check=True, timeout=5.0)

        # Unstaged modification
        git_tracked_file.write_text(
            "pub fn add(a: i32, b: i32) -> i32 {\n    let sum = a + b;\n    sum\n}\n",
            encoding="utf-8",
        )
        # Staged addition
        git_staged_file.write_text("pub fn staged_fn() -> bool { true }\n", encoding="utf-8")
        subprocess.run(["git", "-C", str(git_repo), "add", "staged.rs"], check=True, timeout=5.0)
        # Untracked file
        git_untracked_file.write_text("untracked fixture file\n", encoding="utf-8")
    else:
        git_staged_file.write_text("pub fn staged_fn() -> bool { true }\n", encoding="utf-8")
        git_untracked_file.write_text("untracked fixture file\n", encoding="utf-8")

    dark_theme_dir = themes_dir / "mock-dark"
    light_theme_dir = themes_dir / "mock-light"
    broken_theme_dir = themes_dir / "broken-no-colors"
    for d in (dark_theme_dir, light_theme_dir, broken_theme_dir):
        d.mkdir(parents=True, exist_ok=True)
    (dark_theme_dir / "colors.toml").write_text(DARK_COLORS_TOML, encoding="utf-8")
    (light_theme_dir / "colors.toml").write_text(LIGHT_COLORS_TOML, encoding="utf-8")
    (broken_theme_dir / "README.md").write_text("No colors.toml\n", encoding="utf-8")

    return FixtureBundle(
        root=root,
        code_dir=code_dir,
        sample_rs=sample_rs,
        sample_py=sample_py,
        sample_js=sample_js,
        sample_json=sample_json,
        powerline_txt=powerline_txt,
        docs_dir=docs_dir,
        sample_md=sample_md,
        sample_pdf=sample_pdf,
        media_dir=media_dir,
        sample_png=sample_png,
        sample_svg=sample_svg,
        sample_jpg=sample_jpg,
        anim_gif=anim_gif,
        tone_wav=tone_wav,
        tone_mp3=tone_mp3,
        git_repo=git_repo,
        git_tracked_file=git_tracked_file,
        git_staged_file=git_staged_file,
        git_untracked_file=git_untracked_file,
        themes_dir=themes_dir,
        dark_theme_dir=dark_theme_dir,
        light_theme_dir=light_theme_dir,
        broken_theme_dir=broken_theme_dir,
    )
