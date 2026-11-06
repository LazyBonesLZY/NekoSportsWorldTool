#!/usr/bin/env python3
"""把 `run --dry-run --out <json>` 生成的 GCJ 轨迹叠到卫星图上，并输出距离指标。

只读本地文件，不联网。轨迹 JSON 的 crs 必须是 gcj02（上传体系）。

    python3 scripts/render_track_overlay.py /tmp/track_school.json \
        --output docs/track_overlay_gcj.png
"""
import argparse
import json
import math
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

ROOT = Path(__file__).resolve().parents[1]
PI = math.pi


def pixel(p, ref):
    lat, lng = p
    size = 256 * 2 ** ref["tile_zoom"]
    return (
        (lng + 180) / 360 * size - ref["tile_origin"][0] * 256,
        (1 - math.asinh(math.tan(math.radians(lat))) / PI) / 2 * size
        - ref["tile_origin"][1] * 256,
    )


def plane(p, center):
    scale = 111320.0 * math.cos(math.radians(center[0]))
    return ((p[1] - center[1]) * scale, (p[0] - center[0]) * 111132.0)


def seg_dist(p, a, b):
    dx, dy = b[0] - a[0], b[1] - a[1]
    n = dx * dx + dy * dy
    t = max(0.0, min(1.0, ((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / n)) if n else 0.0
    return math.hypot(p[0] - a[0] - t * dx, p[1] - a[1] - t * dy)


def dist_to_polyline(p, line):
    if len(line) < 2:
        return float("inf")
    return min(seg_dist(p, line[i], line[i + 1]) for i in range(len(line) - 1))


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("track", type=Path)
    ap.add_argument("--satellite", type=Path, default=ROOT / "docs/stadium_satellite_source.png")
    ap.add_argument("--output", type=Path, default=ROOT / "docs/track_overlay_gcj.png")
    opt = ap.parse_args()

    ref = json.loads((ROOT / "data" / "jiangmen_stadium.json").read_text())
    data = json.loads(opt.track.read_text())
    assert data["crs"] == "gcj02", f"轨迹必须是 gcj02，实际 {data['crs']}"

    track = [tuple(p) for p in data["track"]]
    checkpoints = [tuple(p) for p in data.get("checkpoints", [])]
    ring = [tuple(p) for p in ref["centerline_samples"]]

    center = [sum(p[i] for p in ring) / len(ring) for i in range(2)]
    ring_plane = [plane(p, center) for p in ring]
    track_plane = [plane(p, center) for p in track]

    track_to_ring = [dist_to_polyline(p, ring_plane) for p in track_plane]
    cp_to_track = [dist_to_polyline(plane(c, center), track_plane) for c in checkpoints]
    length = sum(math.dist(a, b) for a, b in zip(track_plane, track_plane[1:]))

    img = Image.open(opt.satellite).convert("RGB")
    draw = ImageDraw.Draw(img)
    try:
        font = ImageFont.truetype(
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", 18
        )
    except OSError:
        font = ImageFont.load_default()

    draw.line([pixel(p, ref) for p in ring] + [pixel(ring[0], ref)], fill=(255, 215, 0), width=2)
    draw.line([pixel(p, ref) for p in track], fill=(0, 255, 90), width=3)
    for c in checkpoints:
        x, y = pixel(c, ref)
        draw.ellipse([x - 6, y - 6, x + 6, y + 6], outline=(0, 200, 255), width=3)
    x0, y0 = pixel(track[0], ref)
    draw.ellipse([x0 - 9, y0 - 9, x0 + 9, y0 + 9], outline=(255, 0, 0), width=3)

    lines = [
        f"track {len(track)} pts  {length:.1f} m",
        f"declared {data['record']['totalDistance']:.0f} m / {data['record']['totalTime']} s",
        f"track->ring  p95 {sorted(track_to_ring)[int(0.95*(len(track_to_ring)-1))]:.1f} m"
        f"  max {max(track_to_ring):.1f} m",
        f"checkpoint->track max {max(cp_to_track):.1f} m" if cp_to_track else "no checkpoints",
        "yellow = traced track, green = generated, red = start",
    ]
    y = 8
    for text in lines:
        draw.text((10, y), text, fill=(255, 255, 255), font=font, stroke_width=3, stroke_fill=(0, 0, 0))
        y += 24

    opt.output.parent.mkdir(parents=True, exist_ok=True)
    img.save(opt.output)
    metrics = {
        "reference_id": ref["id"],
        "runMode": data.get("runMode"),
        "points": len(track),
        "polyline_length_m": round(length, 2),
        "declared_distance_m": data["record"]["totalDistance"],
        "track_to_ring_m": {
            "p50": round(sorted(track_to_ring)[len(track_to_ring) // 2], 2),
            "p95": round(sorted(track_to_ring)[int(0.95 * (len(track_to_ring) - 1))], 2),
            "max": round(max(track_to_ring), 2),
        },
        "checkpoint_to_track_m": [round(d, 2) for d in cp_to_track],
        "output": str(opt.output),
    }
    opt.output.with_suffix(".metrics.json").write_text(json.dumps(metrics, ensure_ascii=False, indent=2))
    print(json.dumps(metrics, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()