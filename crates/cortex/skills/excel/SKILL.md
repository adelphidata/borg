---
name: excel
description: "Create and edit Excel .xlsx workbooks: styled tables, live formulas, number formats, charts, dropdowns, and CSV conversion. Reading a workbook needs no skill (read_file converts it); use this to build a spreadsheet, change cells, or turn a CSV into one."
category: utilities
triggers:
  - "make a spreadsheet"
  - "create an excel file"
  - "update this xlsx"
  - "convert this csv to excel"
  - "add a chart to the spreadsheet"
requires:
  bins: ["python3"]
install:
  python3:
    brew: python
    apt: python3-venv
    url: "https://www.python.org/downloads/"
---

# Excel (.xlsx)

To read a workbook, call `read_file`; it returns each sheet as a table. For
exact cells or formulas, or to write, use `openpyxl` in Borg's own Python
environment. Set it up once:

```bash
[ -x ~/.borg/venv/bin/python3 ] || python3 -m venv ~/.borg/venv
~/.borg/venv/bin/python3 -c "import openpyxl" 2>/dev/null || ~/.borg/venv/bin/python3 -m pip install -q openpyxl
```

Save where the user asks (default `~/Documents/`) and give the full path.
Write edits to a new file unless told otherwise.

## Rules

- **Formulas, not pasted results.** Write `"=SUM(D2:D4)"`, never a number
  computed in Python, so the sheet updates when an input changes. Hard-code
  only raw data and the inputs the user will change.
- Colour inputs blue (`Font(color="0000FF")`) and leave formulas black, so a
  reader sees what is an assumption.
- openpyxl never calculates. Set `fullCalcOnLoad` (below) so Excel computes on
  open. A file only holds computed values after Excel or LibreOffice saved it.

## Create

```bash
~/.borg/venv/bin/python3 - <<'EOF'
from openpyxl import Workbook
from openpyxl.chart import BarChart, Reference
from openpyxl.formatting.rule import CellIsRule
from openpyxl.styles import Font, PatternFill
from openpyxl.workbook.properties import CalcProperties
from openpyxl.worksheet.datavalidation import DataValidation
from openpyxl.worksheet.table import Table, TableStyleInfo

wb = Workbook(); ws = wb.active; ws.title = "Sales"
ws.append(["Month", "Units", "Price", "Revenue", "Status"])
for r, (month, units, price) in enumerate([("Jan", 120, 9.5), ("Feb", 135, 9.5), ("Mar", 160, 9.0)], start=2):
    ws.append([month, units, price, f"=B{r}*C{r}", "Open"])
    for col in "BC": ws[f"{col}{r}"].font = Font(color="0000FF")      # inputs
ws["A5"], ws["D5"] = "Total", "=SUM(D2:D4)"

for c in ws[1]:
    c.font = Font(bold=True, color="FFFFFF"); c.fill = PatternFill("solid", fgColor="1F4E79")
for r in range(2, 6):
    for col in "CD": ws[f"{col}{r}"].number_format = '"$"#,##0.00'     # or '0.0%', 'yyyy-mm-dd'
ws.column_dimensions["A"].width = 12
ws.freeze_panes = "A2"

ws.add_table(Table(displayName="SalesTable", ref="A1:E4",
                   tableStyleInfo=TableStyleInfo(name="TableStyleMedium2", showRowStripes=True)))
ws.conditional_formatting.add("D2:D4", CellIsRule(operator="lessThan", formula=["1200"],
                              fill=PatternFill("solid", fgColor="FFC7CE")))
dv = DataValidation(type="list", formula1='"Open,Won,Lost"')            # dropdown
ws.add_data_validation(dv); dv.add("E2:E4")

chart = BarChart(); chart.title = "Revenue by month"                    # also LineChart, PieChart
chart.add_data(Reference(ws, min_col=4, min_row=1, max_row=4), titles_from_data=True)
chart.set_categories(Reference(ws, min_col=1, min_row=2, max_row=4))
ws.add_chart(chart, "G2")

wb.calculation = CalcProperties(fullCalcOnLoad=True)
wb.save("/Users/me/Documents/sales.xlsx")
EOF
```

Dates: write `datetime.date(2026, 1, 31)` and give the cell a date format.
More sheets: `wb.create_sheet("Inputs")`; refer across with `='Inputs'!B2`.

## Read and edit

```python
from openpyxl import load_workbook
wb = load_workbook(path)                         # formulas as text
cached = load_workbook(path, data_only=True)     # last computed values; None if never opened in Excel
ws = wb["Sales"]; print(wb.sheetnames, ws.dimensions)
for row in ws.iter_rows(min_row=1, max_row=10, values_only=True): print(row)
ws["C3"] = 9.75
ws.append(["Apr", 150, 9.0, "=B6*C6", "Open"])
wb.save(new_path)
```

## CSV

```python
import csv
def val(v):
    for cast in (int, float):
        try: return cast(v)
        except ValueError: pass
    return v
with open("in.csv", newline="", encoding="utf-8-sig") as f:      # delimiter=";" for many European files
    for i, row in enumerate(csv.reader(f)):
        ws.append(row if i == 0 else [val(v) for v in row])
```

Keep ID-like columns (ZIP codes, phone numbers, "007") as text, or the
leading zeros vanish. To export, open with `data_only=True` and write
`ws.iter_rows(values_only=True)` with `csv.writer`.

## Pitfalls

- Never save a workbook loaded with `data_only=True`: every formula becomes a
  fixed value.
- Re-saving an existing file with openpyxl can drop shapes, images, and some
  chart styling. Check the result, and tell the user if the file had them.
- `insert_rows` and `delete_cols` do not shift formula references or merged
  ranges. On a sheet with formulas, append at the end or rebuild the sheet.
- Sheet names: at most 31 characters, none of `[ ] : * ? / \`.
- Sheet protection is a "please do not edit" flag, not security.
- Old `.xls` files: convert first with
  `soffice --headless --convert-to xlsx --outdir <dir> old.xls` (LibreOffice).
  The same tool exports PDF: `--convert-to pdf`.

## Check your work

Reload the saved file and print the sheets, dimensions, and the cells you
touched; formulas must still start with `=`.
