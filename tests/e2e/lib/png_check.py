"""Pure-Python PNG decoder and pixel-level assertions for opaque-box E2E tests."""

from __future__ import annotations

import math
import struct
import zlib
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

PNG_SIGNATURE = b"\x89PNG\r\n\x1a\n"


@dataclass(frozen=True)
class PngImage:
    """Decoded 8-bit RGBA PNG image."""

    width: int
    height: int
    bit_depth: int
    color_type: int
    pixels: bytes  # Row-major RGBA8 (len == width * height * 4)
    file_size: int
    chunks: tuple[str, ...]
    first_row_filter: int


def _paeth_predictor(a: int, b: int, c: int) -> int:
    p = a + b - c
    pa = abs(p - a)
    pb = abs(p - b)
    pc = abs(p - c)
    if pa <= pb and pa <= pc:
        return a
    if pb <= pc:
        return b
    return c


def _unfilter_scanlines(raw: bytes, width: int, height: int, bpp: int) -> tuple[bytes, int]:
    stride = width * bpp
    expected_len = height * (1 + stride)
    if len(raw) != expected_len:
        raise ValueError(
            f"Decompressed PNG data size mismatch: got {len(raw)} bytes, expected {expected_len}"
        )

    out = bytearray(height * stride)
    prev_row = bytearray(stride)
    first_filter = raw[0] if height > 0 else 0

    src_off = 0
    dst_off = 0
    for _row in range(height):
        ftype = raw[src_off]
        src_off += 1
        scan = raw[src_off : src_off + stride]
        src_off += stride

        cur = bytearray(stride)
        if ftype == 0:  # None
            cur[:] = scan
        elif ftype == 1:  # Sub
            for i in range(stride):
                left = cur[i - bpp] if i >= bpp else 0
                cur[i] = (scan[i] + left) & 0xFF
        elif ftype == 2:  # Up
            for i in range(stride):
                cur[i] = (scan[i] + prev_row[i]) & 0xFF
        elif ftype == 3:  # Average
            for i in range(stride):
                left = cur[i - bpp] if i >= bpp else 0
                up = prev_row[i]
                cur[i] = (scan[i] + ((left + up) >> 1)) & 0xFF
        elif ftype == 4:  # Paeth
            for i in range(stride):
                left = cur[i - bpp] if i >= bpp else 0
                up = prev_row[i]
                up_left = prev_row[i - bpp] if i >= bpp else 0
                cur[i] = (scan[i] + _paeth_predictor(left, up, up_left)) & 0xFF
        else:
            raise ValueError(f"Unsupported PNG scanline filter type: {ftype}")

        out[dst_off : dst_off + stride] = cur
        dst_off += stride
        prev_row = cur

    return bytes(out), first_filter


def read_png(path: Path | str) -> PngImage:
    """Decode and CRC-verify an 8-bit RGB or RGBA PNG file into RGBA8 pixel bytes."""
    p = Path(path)
    data = p.read_bytes()
    file_size = len(data)
    if file_size < 33:
        raise ValueError(f"PNG file too small ({file_size} bytes): {p}")
    if data[:8] != PNG_SIGNATURE:
        raise ValueError(f"Invalid PNG signature in {p}: {data[:8]!r}")

    offset = 8
    chunks: list[str] = []
    ihdr: Optional[tuple[int, int, int, int, int, int, int]] = None
    idat_parts: list[bytes] = []
    saw_iend = False

    while offset < file_size:
        if offset + 12 > file_size:
            raise ValueError(f"Truncated PNG chunk header at offset {offset} in {p}")
        length, ctype_bytes = struct.unpack(">I4s", data[offset : offset + 8])
        offset += 8
        if offset + length + 4 > file_size:
            raise ValueError(f"Truncated PNG chunk {ctype_bytes!r} at offset {offset} in {p}")
        payload = data[offset : offset + length]
        offset += length
        (expected_crc,) = struct.unpack(">I", data[offset : offset + 4])
        offset += 4

        actual_crc = zlib.crc32(ctype_bytes + payload) & 0xFFFFFFFF
        if actual_crc != expected_crc:
            raise ValueError(
                f"CRC mismatch in chunk {ctype_bytes!r} of {p}: "
                f"got 0x{actual_crc:08x}, expected 0x{expected_crc:08x}"
            )

        ctype = ctype_bytes.decode("ascii", errors="replace")
        chunks.append(ctype)

        if len(chunks) == 1:
            if ctype != "IHDR" or length != 13:
                raise ValueError(f"First chunk must be 13-byte IHDR, got {ctype} ({length}B)")
            ihdr = struct.unpack(">IIBBBBB", payload)
        elif ctype == "IDAT":
            idat_parts.append(payload)
        elif ctype == "IEND":
            if length != 0:
                raise ValueError(f"IEND chunk must be empty, got {length} bytes")
            saw_iend = True
            break

    if ihdr is None or not saw_iend:
        raise ValueError(f"Missing IHDR or IEND chunk in {p}")
    if not idat_parts:
        raise ValueError(f"Missing IDAT chunk in {p}")

    width, height, bit_depth, color_type, compression, filt, interlace = ihdr
    if width <= 0 or height <= 0:
        raise ValueError(f"Invalid PNG dimensions {width}x{height} in {p}")
    if compression != 0 or filt != 0 or interlace != 0:
        raise ValueError(
            f"Unsupported PNG header params (comp={compression}, filt={filt}, interlace={interlace}) in {p}"
        )
    if bit_depth != 8 or color_type not in (2, 6):
        raise ValueError(
            f"Unsupported PNG format (bit_depth={bit_depth}, color_type={color_type}) in {p}"
        )

    bpp = 4 if color_type == 6 else 3
    raw = zlib.decompress(b"".join(idat_parts))
    unfiltered, first_filter = _unfilter_scanlines(raw, width, height, bpp)

    if color_type == 2:
        rgba = bytearray(width * height * 4)
        for i in range(width * height):
            s = i * 3
            d = i * 4
            rgba[d] = unfiltered[s]
            rgba[d + 1] = unfiltered[s + 1]
            rgba[d + 2] = unfiltered[s + 2]
            rgba[d + 3] = 255
        pixels = bytes(rgba)
    else:
        pixels = unfiltered

    return PngImage(
        width=width,
        height=height,
        bit_depth=bit_depth,
        color_type=color_type,
        pixels=pixels,
        file_size=file_size,
        chunks=tuple(chunks),
        first_row_filter=first_filter,
    )


def assert_valid_png(
    path: Path | str,
    expected_width: Optional[int] = None,
    expected_height: Optional[int] = None,
    min_bytes: int = 1024,
    expected_color_type: Optional[int] = None,
) -> PngImage:
    """Assert PNG exists, meets byte/dimension/format constraints, and decodes cleanly."""
    p = Path(path)
    if not p.is_file():
        raise AssertionError(f"Expected PNG file does not exist: {p}")
    size = p.stat().st_size
    if size < min_bytes:
        raise AssertionError(f"PNG file {p} too small: {size} bytes < {min_bytes} min_bytes")
    img = read_png(p)
    if expected_width is not None and img.width != expected_width:
        raise AssertionError(
            f"PNG width mismatch for {p}: got {img.width}, expected {expected_width}"
        )
    if expected_height is not None and img.height != expected_height:
        raise AssertionError(
            f"PNG height mismatch for {p}: got {img.height}, expected {expected_height}"
        )
    if expected_color_type is not None and img.color_type != expected_color_type:
        raise AssertionError(
            f"PNG color_type mismatch for {p}: got {img.color_type}, expected {expected_color_type}"
        )
    return img


def sample_pixel(img: PngImage, x: int, y: int) -> tuple[int, int, int, int]:
    """Return (r, g, b, a) at integer pixel coordinate (x, y)."""
    if not (0 <= x < img.width and 0 <= y < img.height):
        raise ValueError(f"Pixel ({x}, {y}) out of bounds for {img.width}x{img.height}")
    idx = (y * img.width + x) * 4
    px = img.pixels
    return (px[idx], px[idx + 1], px[idx + 2], px[idx + 3])


def mean_color_in_rect(
    img: PngImage, x: int, y: int, w: int, h: int
) -> tuple[float, float, float, float]:
    """Return mean (r, g, b, a) across the clamped rectangle [x, x+w) x [y, y+h)."""
    x0 = max(0, x)
    y0 = max(0, y)
    x1 = min(img.width, x + w)
    y1 = min(img.height, y + h)
    if x1 <= x0 or y1 <= y0:
        raise ValueError(f"Empty rectangle ({x}, {y}, {w}, {h}) for {img.width}x{img.height}")

    sr = sg = sb = sa = 0
    count = 0
    px = img.pixels
    width = img.width
    for row in range(y0, y1):
        base = row * width * 4
        for col in range(x0, x1):
            idx = base + col * 4
            sr += px[idx]
            sg += px[idx + 1]
            sb += px[idx + 2]
            sa += px[idx + 3]
            count += 1
    return (sr / count, sg / count, sb / count, sa / count)


def unique_color_count_sample(img: PngImage, step: int = 8) -> int:
    """Count distinct RGBA colors sampled on a strided grid across the image."""
    if step < 1:
        raise ValueError("step must be >= 1")
    colors: set[tuple[int, int, int, int]] = set()
    px = img.pixels
    w = img.width
    for y in range(0, img.height, step):
        row_base = y * w * 4
        for x in range(0, w, step):
            idx = row_base + x * 4
            colors.add((px[idx], px[idx + 1], px[idx + 2], px[idx + 3]))
    return len(colors)


def assert_not_solid_color(img: PngImage, min_unique_colors: int = 2, step: int = 8) -> None:
    """Assert the decoded image contains visual content rather than a uniform fill."""
    count = unique_color_count_sample(img, step=step)
    if count < min_unique_colors:
        raise AssertionError(
            f"PNG appears solid/blank: only {count} unique sampled colors (< {min_unique_colors})"
        )


def pixel_diff_stats(
    img_a: PngImage,
    img_b: PngImage,
    rect: Optional[tuple[int, int, int, int]] = None,
    channel_tolerance: int = 4,
) -> dict[str, float]:
    """Compute diff_fraction, rms, and max_diff between two same-sized PNG images."""
    if img_a.width != img_b.width or img_a.height != img_b.height:
        raise ValueError(
            f"PNG dimension mismatch: {img_a.width}x{img_a.height} vs {img_b.width}x{img_b.height}"
        )

    if rect is None:
        x0, y0, x1, y1 = 0, 0, img_a.width, img_a.height
    else:
        rx, ry, rw, rh = rect
        x0 = max(0, rx)
        y0 = max(0, ry)
        x1 = min(img_a.width, rx + rw)
        y1 = min(img_a.height, ry + rh)
        if x1 <= x0 or y1 <= y0:
            raise ValueError(f"Empty comparison rect: {rect}")

    pa = img_a.pixels
    pb = img_b.pixels
    w = img_a.width
    total_pixels = (x1 - x0) * (y1 - y0)
    changed_pixels = 0
    sq_sum = 0
    max_diff = 0

    for y in range(y0, y1):
        row_base = y * w * 4
        for x in range(x0, x1):
            idx = row_base + x * 4
            dr = abs(pa[idx] - pb[idx])
            dg = abs(pa[idx + 1] - pb[idx + 1])
            db = abs(pa[idx + 2] - pb[idx + 2])
            da = abs(pa[idx + 3] - pb[idx + 3])
            px_max = max(dr, dg, db, da)
            if px_max > max_diff:
                max_diff = px_max
            if px_max > channel_tolerance:
                changed_pixels += 1
            sq_sum += dr * dr + dg * dg + db * db

    rms = math.sqrt(sq_sum / (total_pixels * 3))
    return {
        "diff_fraction": changed_pixels / total_pixels,
        "rms": rms,
        "max_diff": float(max_diff),
        "total_pixels": float(total_pixels),
        "changed_pixels": float(changed_pixels),
    }


def assert_color_near(
    actual: tuple[float, ...],
    expected: tuple[float, ...],
    tolerance: int = 8,
    label: str = "pixel",
) -> None:
    """Assert RGB(A) color matches expected values within per-channel tolerance."""
    channels = min(len(actual), len(expected))
    for i in range(channels):
        if abs(actual[i] - expected[i]) > tolerance:
            raise AssertionError(
                f"{label} color mismatch: actual={actual[:channels]}, "
                f"expected={expected[:channels]}, tolerance={tolerance}"
            )


def relative_luminance(rgb: tuple[float, float, float]) -> float:
    """Compute approximate sRGB relative luminance in [0.0, 1.0]."""
    r, g, b = (c / 255.0 for c in rgb[:3])
    return 0.2126 * r + 0.7152 * g + 0.0722 * b
