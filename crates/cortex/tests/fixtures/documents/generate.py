#!/usr/bin/env python3
"""Regenerate the document-conversion fixtures.

Every fixture is a real file in its format — a minimal but specification-valid
container — so the conversion tests exercise the actual parsers rather than a
stub. Each one carries the same recognizable strings ("Quarterly Report",
"Widgets", "4200") so a test can assert that content survived the conversion.

Run from anywhere:  python3 generate.py
"""

import os
import zipfile

HERE = os.path.dirname(os.path.abspath(__file__))

# Deterministic timestamp so regenerating produces byte-identical archives.
ZIP_DATE = (2026, 1, 1, 0, 0, 0)


def write_zip(name, entries, mimetype=None):
    """Write a zip container. `mimetype`, when given, is stored uncompressed
    as the first entry, which ODF and EPUB both require."""
    path = os.path.join(HERE, name)
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as z:
        if mimetype is not None:
            info = zipfile.ZipInfo("mimetype", ZIP_DATE)
            info.compress_type = zipfile.ZIP_STORED
            z.writestr(info, mimetype)
        for entry_name, content in entries:
            info = zipfile.ZipInfo(entry_name, ZIP_DATE)
            info.compress_type = zipfile.ZIP_DEFLATED
            z.writestr(info, content)
    print("wrote", name)


def write_text(name, content, newline="\n"):
    path = os.path.join(HERE, name)
    with open(path, "w", newline=newline, encoding="utf-8") as f:
        f.write(content)
    print("wrote", name)


def write_bytes(name, content):
    path = os.path.join(HERE, name)
    with open(path, "wb") as f:
        f.write(content)
    print("wrote", name)


# ── OOXML ───────────────────────────────────────────────────────────────

DOCX_RELS = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"""

DOCX_CONTENT_TYPES = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
<Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
<Override PartName="/word/numbering.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.numbering+xml"/>
</Types>"""

DOCX_DOCUMENT_RELS = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering" Target="numbering.xml"/>
</Relationships>"""

# A bulleted list is the commonest structure in a real Word document, and it
# only renders as one when the numbering definition it points at exists.
DOCX_NUMBERING = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:abstractNum w:abstractNumId="0">
<w:lvl w:ilvl="0"><w:numFmt w:val="bullet"/><w:lvlText w:val="&#8226;"/></w:lvl>
</w:abstractNum>
<w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>
</w:numbering>"""

# Heading levels come from the style's *name*, exactly as Word writes it —
# the style id alone carries no outline semantics.
DOCX_STYLES = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="heading 1"/></w:style>
<w:style w:type="paragraph" w:styleId="Heading2"><w:name w:val="heading 2"/></w:style>
</w:styles>"""

DOCX_DOCUMENT = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body>
<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Quarterly Report</w:t></w:r></w:p>
<w:p><w:r><w:t>Widgets shipped: 4200 units across three regions.</w:t></w:r></w:p>
<w:p><w:pPr><w:pStyle w:val="Heading2"/></w:pPr><w:r><w:t>Outlook</w:t></w:r></w:p>
<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>Expand the Bristol line</w:t></w:r></w:p>
<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>Retire the legacy SKU</w:t></w:r></w:p>
<w:p><w:r><w:rPr><w:b/></w:rPr><w:t>Bold conclusion</w:t></w:r><w:r><w:t xml:space="preserve"> follows here.</w:t></w:r></w:p>
</w:body>
</w:document>"""

write_zip(
    "sample.docx",
    [
        ("[Content_Types].xml", DOCX_CONTENT_TYPES),
        ("_rels/.rels", DOCX_RELS),
        ("word/_rels/document.xml.rels", DOCX_DOCUMENT_RELS),
        ("word/styles.xml", DOCX_STYLES),
        ("word/numbering.xml", DOCX_NUMBERING),
        ("word/document.xml", DOCX_DOCUMENT),
    ],
)

XLSX_CONTENT_TYPES = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
<Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
<Override PartName="/xl/sharedStrings.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml"/>
</Types>"""

XLSX_RELS = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"""

XLSX_WORKBOOK = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<sheets><sheet name="Inventory" sheetId="1" r:id="rId1"/></sheets>
</workbook>"""

XLSX_WORKBOOK_RELS = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.xml"/>
</Relationships>"""

XLSX_SHARED_STRINGS = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="6" uniqueCount="6">
<si><t>Part</t></si><si><t>Region</t></si><si><t>Units</t></si>
<si><t>Widgets</t></si><si><t>Bristol</t></si><si><t>Sprockets</t></si>
</sst>"""

XLSX_SHEET = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<sheetData>
<row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c><c r="C1" t="s"><v>2</v></c></row>
<row r="2"><c r="A2" t="s"><v>3</v></c><c r="B2" t="s"><v>4</v></c><c r="C2"><v>4200</v></c></row>
<row r="3"><c r="A3" t="s"><v>5</v></c><c r="B3" t="s"><v>4</v></c><c r="C3"><v>875</v></c></row>
</sheetData>
</worksheet>"""

write_zip(
    "sample.xlsx",
    [
        ("[Content_Types].xml", XLSX_CONTENT_TYPES),
        ("_rels/.rels", XLSX_RELS),
        ("xl/workbook.xml", XLSX_WORKBOOK),
        ("xl/_rels/workbook.xml.rels", XLSX_WORKBOOK_RELS),
        ("xl/sharedStrings.xml", XLSX_SHARED_STRINGS),
        ("xl/worksheets/sheet1.xml", XLSX_SHEET),
    ],
)

PPTX_CONTENT_TYPES = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>
<Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>
<Override PartName="/ppt/slides/slide2.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>
</Types>"""

PPTX_RELS = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/>
</Relationships>"""

PPTX_PRESENTATION = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentation xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
<p:sldIdLst><p:sldId id="256" r:id="rId1"/><p:sldId id="257" r:id="rId2"/></p:sldIdLst>
</p:presentation>"""

PPTX_PRESENTATION_RELS = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/>
<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide2.xml"/>
</Relationships>"""


def pptx_slide(title, body):
    return f"""<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
<p:cSld><p:spTree>
<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
<p:grpSpPr/>
<p:sp>
<p:nvSpPr><p:cNvPr id="2" name="Title 1"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr>
<p:spPr/>
<p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>{title}</a:t></a:r></a:p></p:txBody>
</p:sp>
<p:sp>
<p:nvSpPr><p:cNvPr id="3" name="Content 2"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr><p:nvPr><p:ph idx="1"/></p:nvPr></p:nvSpPr>
<p:spPr/>
<p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>{body}</a:t></a:r></a:p></p:txBody>
</p:sp>
</p:spTree></p:cSld>
</p:sld>"""


write_zip(
    "sample.pptx",
    [
        ("[Content_Types].xml", PPTX_CONTENT_TYPES),
        ("_rels/.rels", PPTX_RELS),
        ("ppt/presentation.xml", PPTX_PRESENTATION),
        ("ppt/_rels/presentation.xml.rels", PPTX_PRESENTATION_RELS),
        ("ppt/slides/slide1.xml", pptx_slide("Quarterly Report", "Widgets shipped: 4200 units")),
        ("ppt/slides/slide2.xml", pptx_slide("Outlook", "Expand the Bristol line")),
    ],
)

# ── OpenDocument ────────────────────────────────────────────────────────

ODT_CONTENT = """<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" office:version="1.3">
<office:body><office:text>
<text:h text:outline-level="1">Quarterly Report</text:h>
<text:p>Widgets shipped: 4200 units across three regions.</text:p>
<text:h text:outline-level="2">Outlook</text:h>
<text:list><text:list-item><text:p>Expand the Bristol line</text:p></text:list-item>
<text:list-item><text:p>Retire the legacy SKU</text:p></text:list-item></text:list>
</office:text></office:body>
</office:document-content>"""

write_zip(
    "sample.odt",
    [("content.xml", ODT_CONTENT)],
    mimetype="application/vnd.oasis.opendocument.text",
)

ODS_CONTENT = """<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:office-value="urn:oasis:names:tc:opendocument:xmlns:office:1.0" office:version="1.3">
<office:body><office:spreadsheet>
<table:table table:name="Inventory">
<table:table-row>
<table:table-cell office:value-type="string"><text:p>Part</text:p></table:table-cell>
<table:table-cell office:value-type="string"><text:p>Region</text:p></table:table-cell>
<table:table-cell office:value-type="string"><text:p>Units</text:p></table:table-cell>
</table:table-row>
<table:table-row>
<table:table-cell office:value-type="string"><text:p>Widgets</text:p></table:table-cell>
<table:table-cell office:value-type="string"><text:p>Bristol</text:p></table:table-cell>
<table:table-cell office:value-type="float" office:value="4200"><text:p>4200</text:p></table:table-cell>
</table:table-row>
</table:table>
</office:spreadsheet></office:body>
</office:document-content>"""

write_zip(
    "sample.ods",
    [("content.xml", ODS_CONTENT)],
    mimetype="application/vnd.oasis.opendocument.spreadsheet",
)

ODP_CONTENT = """<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" office:version="1.3">
<office:body><office:presentation>
<draw:page draw:name="page1">
<draw:frame presentation:class="title" xmlns:presentation="urn:oasis:names:tc:opendocument:xmlns:presentation:1.0"><draw:text-box><text:p>Quarterly Report</text:p></draw:text-box></draw:frame>
<draw:frame><draw:text-box><text:p>Widgets shipped: 4200 units</text:p></draw:text-box></draw:frame>
</draw:page>
<draw:page draw:name="page2">
<draw:frame presentation:class="title" xmlns:presentation="urn:oasis:names:tc:opendocument:xmlns:presentation:1.0"><draw:text-box><text:p>Outlook</text:p></draw:text-box></draw:frame>
<draw:frame><draw:text-box><text:p>Expand the Bristol line</text:p></draw:text-box></draw:frame>
</draw:page>
</office:presentation></office:body>
</office:document-content>"""

write_zip(
    "sample.odp",
    [("content.xml", ODP_CONTENT)],
    mimetype="application/vnd.oasis.opendocument.presentation",
)

# ── EPUB ────────────────────────────────────────────────────────────────

EPUB_CONTAINER = """<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
<rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"""

EPUB_OPF = """<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="bookid">
<metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
<dc:identifier id="bookid">urn:uuid:borg-fixture-0001</dc:identifier>
<dc:title>Quarterly Report</dc:title>
<dc:language>en</dc:language>
</metadata>
<manifest>
<item id="ch1" href="chapter1.xhtml" media-type="application/xhtml+xml"/>
<item id="ch2" href="chapter2.xhtml" media-type="application/xhtml+xml"/>
</manifest>
<spine><itemref idref="ch1"/><itemref idref="ch2"/></spine>
</package>"""


def epub_chapter(heading, body):
    return f"""<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>{heading}</title></head>
<body><h1>{heading}</h1><p>{body}</p></body></html>"""


write_zip(
    "sample.epub",
    [
        ("META-INF/container.xml", EPUB_CONTAINER),
        ("OEBPS/content.opf", EPUB_OPF),
        ("OEBPS/chapter1.xhtml", epub_chapter("Quarterly Report", "Widgets shipped: 4200 units across three regions.")),
        ("OEBPS/chapter2.xhtml", epub_chapter("Outlook", "Expand the Bristol line.")),
    ],
    mimetype="application/epub+zip",
)

# ── RTF / CSV ───────────────────────────────────────────────────────────

write_text(
    "sample.rtf",
    r"""{\rtf1\ansi\deff0{\fonttbl{\f0 Helvetica;}}
\b\fs36 Quarterly Report\b0\fs24\par
Widgets shipped: 4200 units across three regions.\par
\b Outlook\b0\par
Expand the Bristol line.\par
}
""",
)

write_text(
    "sample.csv",
    "Part,Region,Units\nWidgets,Bristol,4200\nSprockets,Bristol,875\n",
)

# ── PDF ─────────────────────────────────────────────────────────────────
# Hand-assembled so the xref offsets are exact; a text-layer PDF is what
# distinguishes a real conversion from a fallback extraction.


def build_pdf():
    lines = [
        b"BT /F1 18 Tf 72 720 Td (Quarterly Report) Tj ET",
        b"BT /F1 12 Tf 72 690 Td (Widgets shipped: 4200 units across three regions.) Tj ET",
        b"BT /F1 12 Tf 72 670 Td (Outlook: expand the Bristol line.) Tj ET",
    ]
    stream = b"\n".join(lines)
    objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] "
        b"/Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>",
        b"<< /Length " + str(len(stream)).encode() + b" >>\nstream\n" + stream + b"\nendstream",
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
    ]

    out = bytearray(b"%PDF-1.4\n")
    offsets = []
    for i, body in enumerate(objects, start=1):
        offsets.append(len(out))
        out += f"{i} 0 obj\n".encode() + body + b"\nendobj\n"

    xref_at = len(out)
    out += f"xref\n0 {len(objects) + 1}\n".encode()
    out += b"0000000000 65535 f \n"
    for off in offsets:
        out += f"{off:010d} 00000 n \n".encode()
    out += f"trailer\n<< /Size {len(objects) + 1} /Root 1 0 R >>\nstartxref\n{xref_at}\n%%EOF\n".encode()
    return bytes(out)


write_bytes("sample.pdf", build_pdf())


def build_type3_pdf():
    """A PDF whose Type3 font declares LastChar 178 but supplies three widths,
    and whose content stream shows a character past the end of that array.

    `pdf-extract` panics on this rather than returning an error — the same way
    it does on real PDFs in the wild — so it pins the panic guard on the read
    path."""
    stream = b"BT /T1 12 Tf 72 720 Td (\xb3\xb4) Tj ET"
    charproc = b"1 0 0 0 0 0 d0\n"
    objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] "
        b"/Resources << /Font << /T1 5 0 R >> >> /Contents 4 0 R >>",
        b"<< /Length " + str(len(stream)).encode() + b" >>\nstream\n" + stream + b"\nendstream",
        b"<< /Type /Font /Subtype /Type3 /Name /T1 /FontBBox [-10 -10 10 10] "
        b"/FontMatrix [1 0 0 1 0 0] /CharProcs 6 0 R /Encoding 7 0 R "
        b"/FirstChar 0 /LastChar 178 /Widths [1 1 1] >>",
        b"<< /a 8 0 R >>",
        b"<< /Type /Encoding /Differences [179 /a] >>",
        b"<< /Length " + str(len(charproc)).encode() + b" >>\nstream\n" + charproc + b"\nendstream",
    ]

    out = bytearray(b"%PDF-1.4\n")
    offsets = []
    for i, body in enumerate(objects, start=1):
        offsets.append(len(out))
        out += f"{i} 0 obj\n".encode() + body + b"\nendobj\n"

    xref_at = len(out)
    out += f"xref\n0 {len(objects) + 1}\n".encode()
    out += b"0000000000 65535 f \n"
    for off in offsets:
        out += f"{off:010d} 00000 n \n".encode()
    out += f"trailer\n<< /Size {len(objects) + 1} /Root 1 0 R >>\nstartxref\n{xref_at}\n%%EOF\n".encode()
    return bytes(out)


write_bytes("type3_font.pdf", build_type3_pdf())

# A document whose extension lies: real .docx bytes named `.txt`. Reading it
# must still convert, because format detection reads the content signature.
#
# Deliberately padded well past any plausible header-prefix size. A ZIP
# package is identified through its central directory, which sits at the END
# of the container, so sniffing a fixed prefix would identify this fixture only
# if it were small. Keeping it large pins detection to the whole file.
padding = "\n".join(
    f"<w:p><w:r><w:t>Region {i} shipped {i * 7} units, invoiced at {i * 131 % 9973} "
    f"against forecast {i * 17 % 4441} in the period under review.</w:t></w:r></w:p>"
    for i in range(4000)
)
LARGE_DOCX_DOCUMENT = DOCX_DOCUMENT.replace("</w:body>", padding + "\n</w:body>")

write_zip(
    "mislabeled.txt",
    [
        ("[Content_Types].xml", DOCX_CONTENT_TYPES),
        ("_rels/.rels", DOCX_RELS),
        ("word/_rels/document.xml.rels", DOCX_DOCUMENT_RELS),
        ("word/styles.xml", DOCX_STYLES),
        ("word/numbering.xml", DOCX_NUMBERING),
        ("word/document.xml", LARGE_DOCX_DOCUMENT),
    ],
)

size = os.path.getsize(os.path.join(HERE, "mislabeled.txt"))
assert size > 16384, f"mislabeled.txt must stay large enough to pin whole-file detection, got {size}"
print(f"  mislabeled.txt is {size} bytes")
