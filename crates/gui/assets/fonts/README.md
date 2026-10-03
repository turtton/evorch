# Bundled fonts

- `InterVariable.ttf` — [Inter](https://github.com/rsms/inter) 4.1 variable
  font (`opsz`, `wght` axes), SIL Open Font License 1.1 (`Inter-LICENSE.txt`).

  Subset to drop Inter's private-use glyphs (U+E000–U+F8FF), which would
  otherwise shadow the Phosphor icon font that follows Inter in the
  proportional family:

  ```sh
  uv run --with fonttools pyftsubset InterVariable.ttf \
    --unicodes="U+0000-DFFF,U+F900-10FFFF" --layout-features='*' \
    --glyph-names --notdef-outline --name-IDs='*' --name-languages='*' \
    --name-legacy --output-file=InterVariable.ttf
  ```
