"""Render a TUI screen dump to a PNG, approximating a dark or light terminal.

Used to check colours, which text renders can't show. Needs Pillow and the
Menlo font (macOS). Typical use:

    DISKWISE_SNAPSHOT=disk.snapshot.json DISKWISE_RENDER_DIR=/tmp/r DISKWISE_COLOR=256 \
      cargo test -p diskwise-cli render_real -- --ignored
    python3 dev/cells2png.py /tmp/r/browse.json /tmp/r/browse.png dark   # or light
"""
import json, sys, re
from PIL import Image, ImageDraw, ImageFont

def xterm(n):
    base = [(0,0,0),(205,0,0),(0,205,0),(205,205,0),(0,0,238),(205,0,205),(0,205,205),(229,229,229),
            (127,127,127),(255,0,0),(0,255,0),(255,255,0),(92,92,255),(255,0,255),(0,255,255),(255,255,255)]
    if n < 16: return base[n]
    if n < 232:
        n -= 16; lv = [0,95,135,175,215,255]
        return (lv[n//36], lv[(n//6)%6], lv[n%6])
    g = 8 + (n-232)*10; return (g,g,g)

NAMED = {"Black":0,"Red":1,"Green":2,"Yellow":3,"Blue":4,"Magenta":5,"Cyan":6,"Gray":7,"DarkGray":8,
         "LightRed":9,"LightGreen":10,"LightYellow":11,"LightBlue":12,"LightMagenta":13,"LightCyan":14,"White":15}

def color(spec, default):
    if spec == "Reset": return default
    m = re.match(r"Indexed\((\d+)\)", spec)
    if m: return xterm(int(m.group(1)))
    m = re.match(r"Rgb\((\d+), (\d+), (\d+)\)", spec)
    if m: return tuple(int(x) for x in m.groups())
    return xterm(NAMED[spec])

src, out, theme = sys.argv[1], sys.argv[2], sys.argv[3] if len(sys.argv) > 3 else "dark"
fg0, bg0 = ((220,220,220),(30,30,30)) if theme == "dark" else ((30,30,30),(250,250,250))
rows = json.load(open(src))
font = ImageFont.truetype("/System/Library/Fonts/Menlo.ttc", 15, index=0)
bold = ImageFont.truetype("/System/Library/Fonts/Menlo.ttc", 15, index=1)
cw, ch = 9, 19
img = Image.new("RGB", (cw*len(rows[0]), ch*len(rows)), bg0)
d = ImageDraw.Draw(img)
for y, row in enumerate(rows):
    for x, cell in enumerate(row):
        sym, fg, bg, b = cell[:4]
        rev = cell[4] if len(cell) > 4 else False
        bgc = color(bg, bg0)
        fgc = color(fg, fg0)
        if rev: fgc, bgc = bgc, fgc
        if bgc != bg0: d.rectangle([x*cw, y*ch, (x+1)*cw-1, (y+1)*ch-1], fill=bgc)
        if sym.strip():
            d.text((x*cw, y*ch+1), sym, font=bold if b else font, fill=fgc)
img.save(out)
