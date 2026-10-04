#!/usr/bin/env python3
"""Render the silent project introduction. Requires Pillow and ffmpeg."""
from pathlib import Path
import math
import subprocess
from PIL import Image, ImageDraw, ImageFont

ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / 'site/media'
FONT = '/usr/share/fonts/TTF/JetBrainsMono-Regular.ttf'
SCENES = [
    ('YOUR FORGE. YOUR AGENTS.', 'forge-bot', 'From mention to momentum.', 'Forgejo  /  GitHub  /  GitLab'),
    ('01 / MENTION', 'Start in a comment', '@agent investigate this test failure', 'An issue or pull request is the starting point.'),
    ('02 / ROUTE', 'A small gateway', 'Verify  >  Authorize  >  Route', 'Pass the location URL and message to your agent.'),
    ('03 / ACT', 'Your agent takes over', 'Inspect  >  Edit  >  Test  >  Reply', 'The agent reads the context and does the work.'),
    ('04 / CONTINUE', 'Keep the conversation going', 'Follow up in the same thread.', 'Sessions carry context forward. Status shows progress.'),
    ('GET STARTED', 'Give your next issue an agent.', 'Build. Configure. Validate.', 'Read the deployment guide on the project page.'),
]
FPS, SECONDS = 12, 4


def frame(index, progress):
    im = Image.new('RGB', (1280, 720), '#111b19')
    d = ImageDraw.Draw(im)
    def write(x, y, value, size, color):
        d.text((x, y), value, font=ImageFont.truetype(FONT, size), fill=color)
    d.rounded_rectangle((64, 64, 1216, 656), radius=24, outline='#355249', width=2)
    write(100, 100, 'f. / forge-bot', 24, '#99b9af')
    label, title, body, footer = SCENES[index]
    # Gentle entrance; all meaningful text remains still for most of each scene.
    offset = round(16 * (1 - min(progress * 5, 1)))
    write(100, 216 + offset, label, 22, '#80e1b5')
    write(100, 280 + offset, title, 44, '#f1f5ed')
    write(100, 380 + offset, body, 28, '#f1f5ed')
    write(100, 448 + offset, footer, 20, '#99b9af')
    for n in range(len(SCENES)):
        x = 100 + n * 180
        d.rounded_rectangle((x, 588, x + 156, 594), radius=3, fill='#355249')
        if n <= index:
            width = 156 if n < index else max(1, round(156 * progress))
            d.rounded_rectangle((x, 588, x + width, 594), radius=3, fill='#80e1b5')
    return im


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    frame(0, 1).save(OUT / 'startup-poster.webp')
    command = ['ffmpeg', '-y', '-loglevel', 'error', '-f', 'rawvideo', '-pixel_format',
               'rgb24', '-video_size', '1280x720', '-framerate', str(FPS), '-i', '-',
               '-an', '-c:v', 'libx264', '-preset', 'medium', '-crf', '23', '-pix_fmt',
               'yuv420p', '-movflags', '+faststart', str(OUT / 'startup.mp4')]
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
