#!/usr/bin/env python3
"""Render the silent project introduction. Requires Pillow and ffmpeg.

The output is a 1920x1080 H.264 MP4 at 30 fps. The renderer draws every
frame with Pillow and streams raw RGB frames to ffmpeg. The storyboard follows
one concrete run end to end -- a review comment, the gateway, the agent's
terminal session, and the reply in the thread -- instead of showing static
title cards. The MP4 is published to the Cloudflare CDN and intentionally not
committed to Git (see contrib/cloudflare-media/); only the poster is tracked.
No audio or external footage is used.
"""
from pathlib import Path
import subprocess
from PIL import Image, ImageDraw, ImageFont

ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / 'site/media'
FONT = '/usr/share/fonts/TTF/JetBrainsMono-Regular.ttf'
FONT_BOLD = '/usr/share/fonts/TTF/JetBrainsMono-Bold.ttf'

WIDTH, HEIGHT = 1920, 1080
FPS, SECONDS = 30, 4

INK = '#f2f6f0'
MUTED = '#9ab4aa'
FAINT = '#5f7d72'
ACCENT = '#80e1b5'
ACCENT_DIM = '#37614f'
PANEL = '#0f1c18'
PANEL_ALT = '#162924'
BORDER = '#2f4f44'
CODE = '#cdd8cd'
AMBER = '#e5c07b'
BLUE = '#6cb6ff'
RED = '#e06c75'

_font_cache = {}


def _font(path, size):
    key = (path, size)
    font = _font_cache.get(key)
    if font is None:
        font = ImageFont.truetype(path, size)
        _font_cache[key] = font
    return font


def _background():
    """Vertical gradient from deep green to a slightly lighter panel tone."""
    top = (10, 20, 18)
    bottom = (21, 41, 36)
    im = Image.new('RGB', (WIDTH, HEIGHT), top)
    d = ImageDraw.Draw(im)
    for y in range(HEIGHT):
        t = y / (HEIGHT - 1)
        color = tuple(round(top[i] + (bottom[i] - top[i]) * t) for i in range(3))
        d.line([(0, y), (WIDTH, y)], fill=color)
    return im


_BASE = _background()


def _clamp01(value):
    return max(0.0, min(1.0, value))


def _phase(progress, start, end):
    if end <= start:
        return 1.0 if progress >= end else 0.0
    return _clamp01((progress - start) / (end - start))


def _ease(value):
    value = _clamp01(value)
    return 1 - (1 - value) ** 3


def _typed(value, progress, start, end):
    return value[:round(len(value) * _ease(_phase(progress, start, end)))]


def _write(d, xy, value, size, color, bold=False, anchor=None):
    d.text(xy, value, font=_font(FONT_BOLD if bold else FONT, size), fill=color, anchor=anchor)


def _window(d, x0, y0, x1, y1, title):
    d.rounded_rectangle((x0, y0, x1, y1), radius=24, fill=PANEL, outline=BORDER, width=3)
    d.line((x0 + 2, y0 + 66, x1 - 2, y0 + 66), fill=BORDER, width=3)
    for index, color in enumerate((RED, AMBER, ACCENT)):
        cx = x0 + 40 + index * 36
        d.ellipse((cx - 9, y0 + 33 - 9, cx + 9, y0 + 33 + 9), fill=color)
    _write(d, (x0 + 168, y0 + 33), title, 24, MUTED, anchor='lm')


def _avatar(d, cx, cy, label, fill=ACCENT_DIM):
    radius = 34
    d.ellipse((cx - radius, cy - radius, cx + radius, cy + radius), fill=fill)
    _write(d, (cx, cy), label, 26, INK, bold=True, anchor='mm')


def _bubble(d, x0, y0, x1, y1, fill=PANEL_ALT):
    d.rounded_rectangle((x0, y0, x1, y1), radius=22, fill=fill, outline=BORDER, width=2)


def _caption(d, value):
    _write(d, (WIDTH // 2, 936), value, 34, MUTED, anchor='mm')


def scene_brand(d, progress):
    intro = _ease(_phase(progress, 0.0, 0.5))
    rise = round(46 * (1 - intro))
    cx = WIDTH // 2
    d.rounded_rectangle((cx - 74, 322 + rise, cx + 74, 336 + rise), radius=7, fill=ACCENT)
    _write(d, (cx, 470 + rise), 'forge-bot', 176, INK, bold=True, anchor='mm')
    _write(d, (cx, 620 + rise), 'Turn a comment into a working agent.', 54, MUTED, anchor='mm')
    labels = ['Forgejo', 'GitHub', 'GitLab']
    widths = [round(d.textlength(label, font=_font(FONT, 30))) + 52 for label in labels]
    gap = 26
    total = sum(widths) + gap * (len(labels) - 1)
    x = round(cx - total / 2)
    for label, width in zip(labels, widths):
        d.rounded_rectangle((x, 730, x + width, 794), radius=32, fill=PANEL, outline=BORDER, width=2)
        _write(d, (x + width // 2, 762), label, 30, ACCENT, anchor='mm')
        x += width + gap
    _caption(d, 'One bot for Forgejo, GitHub, and GitLab.')


def scene_mention(d, progress):
    _window(d, 170, 214, 1750, 858, 'shylock/forge-bot · issue #187 · needs: agent')
    _write(d, (240, 322), 'Login test fails on expired sessions', 48, INK, bold=True)
    _write(d, (240, 392), 'maintainer · opened 2 minutes ago', 26, FAINT)

    _avatar(d, 284, 500, 'm')
    _bubble(d, 340, 452, 1160, 548)
    _write(d, (376, 478), 'The retry loop never expires the token.', 38, CODE)

    _avatar(d, 284, 686, 'you', fill='#244a5f')
    _bubble(d, 340, 620, 1340, 752, fill='#122b2a')
    typed = _typed('@agent fix the failing login test', progress, 0.3, 0.8)
    _write(d, (376, 662), typed, 42, ACCENT)
    if progress < 0.96:
        width = d.textlength(typed, font=_font(FONT, 42))
        d.rectangle((376 + width + 8, 654, 376 + width + 30, 712), fill=ACCENT)

    _caption(d, 'It starts where the work already happens: in a comment.')


def scene_gateway(d, progress):
    _write(d, (WIDTH // 2, 300), 'The gateway', 68, INK, bold=True, anchor='mm')
    _write(d, (WIDTH // 2, 386), 'Each mention is verified, authorized, and routed.', 38, MUTED, anchor='mm')
    steps = [('webhook', 'event received'), ('verify', 'signature valid'), ('authorize', 'policy checked'), ('route', 'agent selected')]
    box_w, box_h, gap = 320, 190, 56
    total = len(steps) * box_w + (len(steps) - 1) * gap
    left = (WIDTH - total) // 2
    top = 520
    for index, (name, sub) in enumerate(steps):
        active = _phase(progress, 0.08 + index * 0.19, 0.26 + index * 0.19) > 0.5
        x0 = left + index * (box_w + gap)
        d.rounded_rectangle((x0, top, x0 + box_w, top + box_h), radius=24,
                            fill=PANEL_ALT if active else PANEL,
                            outline=ACCENT if active else BORDER, width=4 if active else 3)
        _write(d, (x0 + box_w // 2, top + 66), name, 44, INK if active else MUTED, bold=True, anchor='mm')
        _write(d, (x0 + box_w // 2, top + 132), sub, 26, ACCENT if active else FAINT, anchor='mm')
        if index < len(steps) - 1:
            ax = x0 + box_w + 8
            line_color = '#3d6154' if active else BORDER
            d.line((ax, top + box_h // 2, ax + gap - 16, top + box_h // 2), fill=line_color, width=4)
            d.polygon([(ax + gap - 16, top + box_h // 2 - 10), (ax + gap - 16, top + box_h // 2 + 10),
                       (ax + gap, top + box_h // 2)], fill=line_color)
    _caption(d, 'Verify the event. Authorize the actor. Route to your agent.')


def scene_agent(d, progress):
    _window(d, 180, 214, 1740, 858, 'pi-rpc · agent session')
    _write(d, (240, 312), 'forge-bot', 30, ACCENT, bold=True)
    _write(d, (470, 312), 'working in shylock/forge-bot', 28, FAINT)

    lines = [
        ('$ cargo test --workspace', CODE, False),
        ('    running 1937 tests', MUTED, False),
        ('    test result: ok. 1937 passed; 0 failed', ACCENT, True),
        ('$ git diff --stat', CODE, False),
        (' src/session/queue.rs | 42 +++++++++++++++++---------', BLUE, False),
        (' 1 file changed, 33 insertions(+), 9 deletions(-)', AMBER, False),
    ]
    y = 392
    for index, (line, color, bold) in enumerate(lines):
        if _phase(progress, 0.06 + index * 0.11, 0.12 + index * 0.11) <= 0:
            break
        _write(d, (250, y), line, 38, color or CODE, bold=bold)
        y += 66

    if progress > 0.82:
        bx0, by0 = 1150, 300
        d.rounded_rectangle((bx0, by0, 1680, 440), radius=26, fill='#123326', outline=ACCENT, width=3)
        _write(d, (bx0 + 265, by0 + 52), '✓ tests passed', 46, ACCENT, bold=True, anchor='mm')
        _write(d, (bx0 + 265, by0 + 108), '33 insertions, 9 deletions', 28, MUTED, anchor='mm')

    _caption(d, 'Your agent inspects, edits, tests, and commits.')


def scene_reply(d, progress):
    _window(d, 170, 214, 1750, 858, 'shylock/forge-bot · issue #187')
    _write(d, (240, 322), 'Login test fails on expired sessions', 48, INK, bold=True)
    _write(d, (240, 392), 'maintainer · opened 2 minutes ago', 26, FAINT)

    _avatar(d, 284, 500, 'm')
    _bubble(d, 340, 452, 1160, 548, fill='#101a17')
    _write(d, (376, 478), 'The retry loop never expires the token.', 38, FAINT)

    _avatar(d, 284, 690, 'f', fill='#1f4a3a')
    _write(d, (340, 596), 'forge-bot', 30, ACCENT, bold=True)
    _bubble(d, 340, 632, 1620, 792, fill='#122b2a')
    reply = 'Fixed the expiry check in queue.rs and added a regression test.'
    typed = _typed(reply, progress, 0.25, 0.85)
    _write(d, (376, 664), typed, 36, CODE)
    if len(typed) == len(reply):
        _write(d, (376, 722), '✓ 1937 tests pass', 32, ACCENT, bold=True)
    _caption(d, 'It replies in the same thread, with the context it gathered.')


def scene_cta(d, progress):
    intro = _ease(_phase(progress, 0.0, 0.5))
    rise = round(40 * (1 - intro))
    _write(d, (WIDTH // 2, 330 + rise), 'Give your next issue an agent.', 78, INK, bold=True, anchor='mm')
    steps = [('1', 'Mention @agent in the thread'), ('2', 'Review the diff and the tests'), ('3', 'Continue in the same session')]
    y = 500
    for index, (step, label) in enumerate(steps):
        reveal = _phase(progress, 0.2 + index * 0.16, 0.36 + index * 0.16) > 0.5
        x = 470
        d.ellipse((x, y, x + 72, y + 72), fill=ACCENT if reveal else PANEL_ALT, outline=BORDER, width=2)
        _write(d, (x + 36, y + 36), step, 40, '#0d1b17' if reveal else MUTED, bold=True, anchor='mm')
        _write(d, (x + 120, y + 36), label, 44, INK if reveal else MUTED, anchor='lm')
        y += 118
    _caption(d, 'Read the deployment guide to get started.')


SCENES = [scene_brand, scene_mention, scene_gateway, scene_agent, scene_reply, scene_cta]


def _progress(d, index, progress):
    count = len(SCENES)
    left, right, gap = 140, WIDTH - 140, 20
    seg = (right - left - gap * (count - 1)) / count
    for n in range(count):
        x = left + n * (seg + gap)
        d.rounded_rectangle((x, 1000, x + seg, 1008), radius=4, fill='#2c4a40')
        if n < index:
            d.rounded_rectangle((x, 1000, x + seg, 1008), radius=4, fill=ACCENT)
        elif n == index:
            d.rounded_rectangle((x, 1000, x + max(3, seg * progress), 1008), radius=4, fill=ACCENT)


def frame(index, progress):
    im = _BASE.copy()
    d = ImageDraw.Draw(im)
    d.rounded_rectangle((70, 56, WIDTH - 70, HEIGHT - 56), radius=40, outline='#28443a', width=3)
    _write(d, (140, 130), 'f. / forge-bot', 34, MUTED, anchor='lm')
    _write(d, (WIDTH - 140, 130), 'silent · 24 s', 28, FAINT, anchor='rm')
    SCENES[index](d, progress)
    _progress(d, index, progress)
    return im


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    frame(0, 1).save(OUT / 'startup-poster.webp', quality=90, method=6)
    command = [
        'ffmpeg', '-y', '-loglevel', 'error',
        '-f', 'rawvideo', '-pixel_format', 'rgb24',
        '-video_size', f'{WIDTH}x{HEIGHT}', '-framerate', str(FPS), '-i', '-',
        '-an', '-c:v', 'libx264', '-preset', 'slow', '-crf', '18',
        '-profile:v', 'high', '-pix_fmt', 'yuv420p',
        '-g', str(FPS), '-keyint_min', str(FPS), '-sc_threshold', '0',
        '-movflags', '+faststart', str(OUT / 'startup.mp4'),
    ]
    proc = subprocess.Popen(command, stdin=subprocess.PIPE)
    try:
        for index in range(len(SCENES)):
            for tick in range(FPS * SECONDS):
                proc.stdin.write(frame(index, tick / (FPS * SECONDS - 1)).tobytes())
    finally:
        proc.stdin.close()
    if proc.wait() != 0:
        raise SystemExit('ffmpeg failed')


if __name__ == '__main__':
    main()
