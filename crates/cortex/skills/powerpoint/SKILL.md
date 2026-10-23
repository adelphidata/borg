---
name: powerpoint
description: "Create and edit PowerPoint .pptx decks: title and bullet slides, tables, charts, images, speaker notes, and decks built on a company template, plus export to PDF. Reading a deck needs no skill (read_file converts it); use this to build a deck or change one."
category: utilities
triggers:
  - "make a slide deck"
  - "create a powerpoint"
  - "build a presentation"
  - "edit these slides"
  - "add speaker notes"
requires:
  bins: ["python3"]
install:
  python3:
    brew: python
    apt: python3-venv
    url: "https://www.python.org/downloads/"
---

# PowerPoint (.pptx)

To read a deck, call `read_file`; it returns each slide as Markdown. To build
or edit one, use `python-pptx` in Borg's own Python environment. Set it up once:

```bash
[ -x ~/.borg/venv/bin/python3 ] || python3 -m venv ~/.borg/venv
~/.borg/venv/bin/python3 -c "import pptx" 2>/dev/null || ~/.borg/venv/bin/python3 -m pip install -q python-pptx
```

Plan the story first: one message per slide, at most six bullets of about
twelve words, details in the speaker notes. Save where the user asks (default
`~/Documents/`) and give the full path.

## Create

```bash
~/.borg/venv/bin/python3 - <<'EOF'
from pptx import Presentation
from pptx.chart.data import CategoryChartData
from pptx.dml.color import RGBColor
from pptx.enum.chart import XL_CHART_TYPE
from pptx.util import Inches, Pt

prs = Presentation()                    # or Presentation("brand-template.pptx"): keep its size, skip the next 4 lines
f = 13.333 / 10                         # the built-in template is 4:3; widen it and its layouts to 16:9
prs.slide_width = Inches(13.333)
for layout in prs.slide_layouts:
    for ph in layout.placeholders: ph.left, ph.width = int(ph.left * f), int(ph.width * f)
L = {layout.name: layout for layout in prs.slide_layouts}   # print(list(L)) to see a template's names

s = prs.slides.add_slide(L["Title Slide"])
s.shapes.title.text = "Q3 Review"
s.placeholders[1].text = "October 2026"

s = prs.slides.add_slide(L["Title and Content"])
s.shapes.title.text = "Highlights"
tf = s.placeholders[1].text_frame
tf.text = "Revenue up 12%"
for text, level in [("EU led growth", 1), ("Two new hires", 0)]:
    p = tf.add_paragraph(); p.text = text; p.level = level
s.notes_slide.notes_text_frame.text = "Mention the Berlin closure."

s = prs.slides.add_slide(L["Title Only"])
s.shapes.title.text = "Revenue by region"
data = CategoryChartData(); data.categories = ["Q1", "Q2", "Q3"]
data.add_series("EU", (1.1, 1.2, 1.4)); data.add_series("US", (1.9, 2.0, 2.2))
s.shapes.add_chart(XL_CHART_TYPE.COLUMN_CLUSTERED,          # also LINE, PIE, BAR_CLUSTERED
                   Inches(1), Inches(1.6), Inches(11.3), Inches(5.4), data)

s = prs.slides.add_slide(L["Title Only"])
s.shapes.title.text = "Headcount"
rows = [("Team", "Q2", "Q3"), ("Eng", "12", "14"), ("Sales", "8", "8")]
table = s.shapes.add_table(len(rows), 3, Inches(1), Inches(1.6), Inches(8), Inches(2)).table
for r, row in enumerate(rows):
    for c, text in enumerate(row): table.cell(r, c).text = text

s = prs.slides.add_slide(L["Blank"])
# s.shapes.add_picture("logo.png", Inches(1), Inches(1), height=Inches(1.5))
tf = s.shapes.add_textbox(Inches(1), Inches(3), Inches(11.3), Inches(1.5)).text_frame
tf.text = "Thank you"
run = tf.paragraphs[0].runs[0]
run.font.size = Pt(44); run.font.bold = True; run.font.color.rgb = RGBColor(0x1F, 0x4E, 0x79)
prs.save("/Users/me/Documents/q3-review.pptx")
EOF
```

A company template keeps its fonts, colours, and master. Its layout names
differ, so print them first. `s.shapes.title` is `None` on a blank layout.

## Edit

```python
prs = Presentation(path)
for i, s in enumerate(prs.slides):                     # inventory first
    print(i, s.slide_layout.name, [sh.name for sh in s.shapes])

def replace(prs, old, new):                            # slide text, tables, and notes
    for s in prs.slides:
        frames = [s.notes_slide.notes_text_frame] if s.has_notes_slide else []
        for sh in s.shapes:
            if sh.has_text_frame: frames.append(sh.text_frame)
            if sh.has_table: frames += [c.text_frame for row in sh.table.rows for c in row.cells]
        for tf in frames:
            for p in tf.paragraphs:
                for r in p.runs: r.text = r.text.replace(old, new)

ids = prs.slides._sldIdLst                             # no public API to delete or move slides
gone = ids[3]; prs.part.drop_rel(gone.rId); ids.remove(gone)         # delete slide 4
last = ids[-1]; ids.remove(last); ids.insert(1, last)                # move the last slide to 2nd
prs.save(new_path)
```

Text split across runs with different formatting will not match `replace`;
check those slides by hand. For a chart, rebuild it from new data with
`chart.replace_data(new_chart_data)`.

## PDF and slide images

```bash
soffice --headless --convert-to pdf --outdir ~/Documents ~/Documents/q3-review.pptx
mkdir -p ~/.borg/cache/slides     # optional: one PNG per slide
pdftoppm -png -r 80 ~/Documents/q3-review.pdf ~/.borg/cache/slides/q3
```

Then look at each PNG with `read_file` to catch overflowing text or overlaps.
This needs LibreOffice (`brew install --cask libreoffice`, or
`apt install libreoffice-impress`; on macOS it may live at
`/Applications/LibreOffice.app/Contents/MacOS/soffice`) and, for the PNGs,
poppler. Without them, say so and check the deck with `read_file` only.

## Check your work

Open the saved deck with `read_file`: slide count, titles, bullets, tables,
and notes must match the plan.
