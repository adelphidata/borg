---
name: pdf
description: "Read, extract, split, and merge PDF files"
triggers:
  - "read this pdf"
  - "extract text from pdf"
  - "split the pdf"
  - "merge pdfs"
requires:
  bins: ["pdftotext"]
---

# PDF

poppler-utils (`pdftotext`, `pdftoppm`, `pdfinfo`, `pdfunite`) + `qpdf`. For
OCR on scanned PDFs use `ocrmypdf` (separate).

## Text

```bash
pdftotext input.pdf -                    # stdout
pdftotext -layout input.pdf -            # preserve columns/tables
pdftotext -f 3 -l 5 input.pdf -          # page range
```

## Metadata

```bash
pdfinfo input.pdf
pdfinfo input.pdf | awk '/^Pages:/ {print $2}'
```

## Render to images

```bash
pdftoppm -png -r 150 input.pdf page              # page-1.png, page-2.png, ...
pdftoppm -png -r 200 -f 1 -l 1 input.pdf cover
pdftoppm -jpeg -r 100 input.pdf page
```

## Split / extract

```bash
qpdf input.pdf --pages input.pdf 3-7 -- out.pdf  # extract range
qpdf --split-pages input.pdf out.pdf             # one file per page
```

## Merge

```bash
pdfunite a.pdf b.pdf c.pdf merged.pdf
# qpdf if you need per-input page ranges:
qpdf --empty --pages a.pdf 1-z b.pdf 1-z -- merged.pdf
```

## Decrypt / linearize

```bash
qpdf --password=PASS --decrypt locked.pdf unlocked.pdf
qpdf --linearize input.pdf out.pdf
```
