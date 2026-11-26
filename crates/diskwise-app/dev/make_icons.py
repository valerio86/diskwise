"""Draw the app icon (a tiny treemap in the app's palette) and write every size
Tauri's bundler wants. Needs Pillow; run from crates/diskwise-app."""
import os, subprocess, tempfile
from PIL import Image, ImageDraw

def draw(size):
    s = size / 1024
    img = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    # macOS icon grid: 824px rounded square centred in 1024.
    pad, r = 100 * s, 185 * s
    d.rounded_rectangle([pad, pad, size - pad, size - pad], r, fill=(29, 30, 36))
    g = 16 * s  # gap between tiles
    x0, y0, x1, y1 = 170 * s, 170 * s, size - 170 * s, size - 170 * s
    tiles = [  # (x0, y0, x1, y1) in 0..1 of the inner square, colour
        ((0.00, 0.00, 0.58, 1.00), (78, 82, 112)),    # slate: untouched space
        ((0.58, 0.00, 1.00, 0.52), (51, 140, 237)),   # blue: safe to clean
        ((0.58, 0.52, 1.00, 0.80), (210, 142, 5)),    # amber: caution
        ((0.58, 0.80, 0.80, 1.00), (226, 77, 139)),   # pink: review
        ((0.80, 0.80, 1.00, 1.00), (78, 82, 112)),
    ]
    w, h = x1 - x0, y1 - y0
    for (a, b, c, e), col in tiles:
        d.rounded_rectangle([x0 + a * w + g / 2, y0 + b * h + g / 2, x0 + c * w - g / 2, y0 + e * h - g / 2], 22 * s, fill=col)
    return img

os.makedirs("icons", exist_ok=True)
big = draw(1024)
big.save("icons/icon.png")
for name, px in [("32x32.png", 32), ("128x128.png", 128), ("128x128@2x.png", 256)]:
    draw(px).save(f"icons/{name}")
with tempfile.TemporaryDirectory() as t:
    iconset = os.path.join(t, "icon.iconset")
    os.makedirs(iconset)
    for px in [16, 32, 128, 256, 512]:
        draw(px).save(f"{iconset}/icon_{px}x{px}.png")
        draw(px * 2).save(f"{iconset}/icon_{px}x{px}@2x.png")
    subprocess.run(["iconutil", "-c", "icns", iconset, "-o", "icons/icon.icns"], check=True)
print("icons written")
