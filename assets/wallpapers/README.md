# Wallpapers

`scuba-divers.png` — the maintainer's own photograph (1920×1440, 8-bit RGB, non-interlaced),
added 2026-09-02 and shipped as the session's default wallpaper.

**Committed as supplied, and cropped by the build.** The screen is 16:10 and this is 4:3, so
fitting it whole leaves 107 pixels of desktop colour down each side. `xtask` centre-crops it to
1920×1200 when it stages the image (`wallpaper_png`), which fills the screen edge to edge.
Keeping the original here rather than a pre-cropped file means nothing is lost and the crop is a
dozen lines somebody can read, rather than a decision baked into a binary nobody can review.

It is the one binary asset in this repository that is **content** rather than a dependency — the
faces under `assets/fonts` are redistributed and carry their licence beside them; this is the
maintainer's own photograph and needs none.
