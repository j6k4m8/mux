# website

Hand-edited static site, published to GitHub Pages by
`.github/workflows/static-site.yml` (it uploads this directory as-is).

- `index.html` — the page
- `privacy/index.html` — privacy policy, linked from the OAuth consent screen
- `images/` — screenshots, generated from the full-resolution captures in
  `docs/screenshots/`

Both pages carry their own copy of the stylesheet. If you change one, change the
other, or they drift.

To regenerate an image after replacing a capture in `docs/screenshots/`:

```sh
sips -s format jpeg -s formatOptions 80 --resampleWidth 1500 \
  docs/screenshots/mux-desktop.png --out website/images/mux-desktop.jpg
```

Use `--resampleWidth 620` for the narrow shot.
