# Whisker Emoji font

`WhiskerEmoji.ttf` is [Noto Color Emoji](https://github.com/googlefonts/noto-emoji)
(Copyright 2013 Google LLC, SIL Open Font License 1.1, see `LICENSE`), renamed
and trimmed by `trim_emoji_font.py`:

- ordinary text characters (space, digits, `#`, `*`, ©, ®, ™ and the rest of
  Latin-1) and a few symbols Whisker uses as icons are removed from its
  character map, so it can come first in the font list without taking over text;
- its line-height metrics are copied from DejaVu Sans, so lines don't get taller.

Whisker lists it before the system fonts, so emoji in messages show in colour
even where a text font has its own black-and-white emoji shapes.

To rebuild it:

    python3 trim_emoji_font.py /usr/share/fonts/truetype/noto/NotoColorEmoji.ttf \
        /usr/share/fonts/truetype/dejavu/DejaVuSans.ttf WhiskerEmoji.ttf
