#!/usr/bin/env python3
"""SVG から expos.ico を作り直す。ImageMagick の convert が必要。"""
import re
import struct
import subprocess
import tempfile
from pathlib import Path

here = Path(__file__).resolve().parent
sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256]
sources = {16: "expos-16.svg", 24: "expos-24.svg"}
images = []
with tempfile.TemporaryDirectory() as work:
    for size in sizes:
        svg = (here / sources.get(size, "expos.svg")).read_text()
        # 大きく描いてから縮小し、輪郭をなめらかにする。
        large = Path(work, "large.svg")
        large.write_text(re.sub(r'width="\d+" height="\d+" xmlns',
                                f'width="{size * 8}" height="{size * 8}" xmlns', svg, count=1))
        png = Path(work, f"{size}.png")
        subprocess.run(["convert", "-background", "none", str(large), "-filter", "Box",
                        "-resize", f"{size}x{size}", "-strip", f"PNG32:{png}"], check=True)
        images.append(png.read_bytes())

offset = 6 + 16 * len(sizes)
header = struct.pack("<HHH", 0, 1, len(sizes))
for size, data in zip(sizes, images):
    header += struct.pack("<BBBBHHII", size % 256, size % 256, 0, 0, 1, 32, len(data), offset)
    offset += len(data)
(here / "expos.ico").write_bytes(header + b"".join(images))
