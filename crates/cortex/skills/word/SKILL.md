---
name: word
description: "Create and edit Word .docx files: reports, letters, memos, and documents filled from a template with {{placeholders}}, plus export to PDF. Reading a .docx needs no skill (read_file converts it); use this to write a new one or change an existing one."
category: utilities
triggers:
  - "make a word document"
  - "create a docx"
  - "edit this word file"
  - "fill in this template"
  - "write this up as a word doc"
requires:
  bins: ["python3"]
install:
  python3:
    brew: python
    apt: python3-venv
    url: "https://www.python.org/downloads/"
---

# Word (.docx)

To read a .docx, call `read_file`; it returns Markdown. To write or edit one,
use `python-docx` in Borg's own Python environment (system Python often
refuses `pip install`). Set it up once:

```bash
[ -x ~/.borg/venv/bin/python3 ] || python3 -m venv ~/.borg/venv
~/.borg/venv/bin/python3 -c "import docx" 2>/dev/null || ~/.borg/venv/bin/python3 -m pip install -q python-docx
```

Save where the user asks (default `~/Documents/`) and give the full path.
Never overwrite the user's original: write `<name>-edited.docx` unless told
otherwise. Never edit the XML inside a .docx with sed; it corrupts the file.

## Create

```bash
~/.borg/venv/bin/python3 - <<'EOF'
from docx import Document
from docx.shared import Mm, Pt

doc = Document()
sec = doc.sections[0]
sec.page_width, sec.page_height = Mm(210), Mm(297)        # A4; US Letter is Mm(215.9), Mm(279.4)
doc.styles["Normal"].font.name = "Calibri"
doc.styles["Normal"].font.size = Pt(11)

doc.add_heading("Quarterly Report", level=0)              # level 0 is the title style
doc.add_paragraph("Prepared for the board, October 2026.")
doc.add_heading("Summary", level=1)
p = doc.add_paragraph("Revenue grew ")
p.add_run("12%").bold = True
p.add_run(" over last quarter.")
for item in ["Hired two engineers", "Closed the Berlin office"]:
    doc.add_paragraph(item, style="List Bullet")           # "List Number" for 1, 2, 3

table = doc.add_table(rows=1, cols=3)
table.style = "Table Grid"
for cell, text in zip(table.rows[0].cells, ["Region", "Q2", "Q3"]):
    cell.text = text
    cell.paragraphs[0].runs[0].bold = True
for row in [("EU", "1.2M", "1.4M"), ("US", "2.0M", "2.2M")]:
    for cell, text in zip(table.add_row().cells, row):
        cell.text = text

# doc.add_picture("chart.png", width=Mm(150))             # PNG or JPEG that exists on disk
doc.add_page_break()
sec.footer.paragraphs[0].text = "Confidential"
doc.save("/Users/me/Documents/report.docx")
EOF
```

Styles that always exist: `Title`, `Heading 1` to `Heading 9`, `List Bullet`,
`List Number`, `Quote`, `Table Grid`, `Light Grid Accent 1`. Any other style
name raises `KeyError` until you add it.

## Edit

Open with `doc = Document(path)` and list paragraphs first, so you edit the
right index:

```python
for i, p in enumerate(doc.paragraphs): print(i, p.style.name, p.text[:60])
```

Replace text everywhere (body, tables, headers, footers). Word splits text
into runs at random points; when a phrase spans runs, this keeps the first
run's formatting:

```python
def replace_everywhere(doc, old, new):
    def fix(p):
        if old not in p.text: return
        want = p.text.replace(old, new)
        for r in p.runs: r.text = r.text.replace(old, new)
        if p.text != want:                     # the phrase was split across runs
            for r in p.runs[1:]: r.text = ""
            p.runs[0].text = want
    for part in [doc] + [s.header for s in doc.sections] + [s.footer for s in doc.sections]:
        for p in part.paragraphs: fix(p)
        for t in part.tables:
            for row in t.rows:
                for cell in row.cells:
                    for p in cell.paragraphs: fix(p)
```

- **Template**: put `{{client}}`-style tokens in the file, call
  `replace_everywhere` once per value, then search the result for `{{` to
  catch tokens nobody filled.
- **Table cell**: `doc.tables[0].cell(1, 2).text = "1.5M"` (resets that cell's formatting).
- **Insert**: `doc.paragraphs[5].insert_paragraph_before("New text", style="Normal")`.
- **Delete**: `p = doc.paragraphs[5]; p._element.getparent().remove(p._element)`.
- **Restyle**: `doc.paragraphs[0].style = "Heading 1"`.
- **Comment**: `doc.add_comment(runs=p.runs, text="Check this date", author="Borg")`.

python-docx cannot accept or reject tracked changes. To see if a file has any:
`unzip -p file.docx word/document.xml | grep -c '<w:ins '`. Tell the user to
resolve them in Word.

## PDF and old .doc files

LibreOffice converts without opening a window:

```bash
soffice --headless --convert-to pdf --outdir ~/Documents ~/Documents/report.docx
soffice --headless --convert-to docx --outdir ~/Documents ~/Documents/old.doc
```

On macOS it may live at `/Applications/LibreOffice.app/Contents/MacOS/soffice`.
If it is not installed (`brew install --cask libreoffice`, or
`apt install libreoffice-writer`), say so; python-docx cannot render a PDF.

## Check your work

Open the result with `read_file`: the new text must be there and replaced text
gone. A table of contents or page-number field fills in when Word opens the
file.
