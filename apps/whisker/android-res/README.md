# Android launcher icon

An adaptive icon, so the launcher's own shape is filled edge to edge rather
than the round artwork sitting inside it. CI (`.github/workflows/build.yml`,
"Adaptive icon") compiles this folder with aapt2 and puts it in the APK in
place of the one xbuild makes from `assets/icon.png`; `ids.txt` keeps
`mipmap/icon` at the id xbuild's manifest points to.

- `res/values/colors.xml`: the background layer, the colour of the artwork's disc.
- `res/mipmap-*/icon_foreground.png`: `assets/icon.png` at 76 of the 108 dp
  canvas, made with

      ffmpeg -i ../../assets/icon.png -vf "scale=S:S:flags=lanczos,format=rgba,pad=C:C:(C-S)/2:(C-S)/2:color=0x00000000" out.png

  for C = 108, 162, 216, 324, 432 and S = C × 76 / 108.
