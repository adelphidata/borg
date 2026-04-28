---
name: pdf
description: "Read, extract, split, and merge PDF files"
requires:
  bins: ["pdftotext"]
---

# PDF Skill

Read and manipulate PDF files using poppler-utils (`pdftotext`, `pdftoppm`, `pdfinfo`, `pdfunite`) and `qpdf`. Install on macOS with `brew install poppler qpdf`; on Debian/Ubuntu with `apt install poppler-utils qpdf`.

## Read Text Content

```bash
# Extract all text to stdout
pdftotext input.pdf -

# Preserve layout (better for tables/columns)
pdftotext -layout input.pdf -

# Specific page range
pdftotext -f 3 -l 5 input.pdf -

# Write to a file
pdftotext input.pdf out.txt
```

## Inspect Metadata

```bash
# Page count, size, author, title
pdfinfo input.pdf

# Just the page count (scriptable)
pdfinfo input.pdf | awk '/^Pages:/ {print $2}'
```

## Render Pages to Images

```bash
# Render every page as PNG at 150 DPI: page-1.png, page-2.png, ...
pdftoppm -png -r 150 input.pdf page

# Single page
pdftoppm -png -r 200 -f 1 -l 1 input.pdf cover

# JPEG output
pdftoppm -jpeg -r 100 input.pdf page
```

## Split / Extract Pages

```bash
# Extract pages 3-7 into a new PDF
qpdf input.pdf --pages input.pdf 3-7 -- out.pdf

# Split every page into its own file
qpdf --split-pages input.pdf out.pdf
# Produces out-01.pdf, out-02.pdf, ...
```

## Merge PDFs

```bash
# Concatenate
pdfunite a.pdf b.pdf c.pdf merged.pdf

# Or with qpdf (more flexible — pick page ranges per input)
qpdf --empty --pages a.pdf 1-z b.pdf 1-z -- merged.pdf
```

## Decrypt / Reduce Size

```bash
# Remove a known password
qpdf --password=PASS --decrypt locked.pdf unlocked.pdf

# Linearize (web-optimize) and clean up
qpdf --linearize input.pdf out.pdf
```

## Notes

- `pdftotext -` writes to stdout — pipe it into `grep`, `head`, `wc -l`, etc.
- For OCR on scanned PDFs use `ocrmypdf` (separate tool, not bundled here).
- Always confirm the output path before overwriting.
