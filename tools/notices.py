"""Build THIRD-PARTY-NOTICES.html: cargo-about output plus the Slint licence text.

Usage: python tools/notices.py <cargo-about-output.html> <out.html>
"""
import html
import pathlib
import sys

src, out = map(pathlib.Path, sys.argv[1:3])
root = pathlib.Path(__file__).resolve().parent.parent
slint = (root / "licenses" / "LicenseRef-Slint-Royalty-free-2.0.md").read_text(encoding="utf-8")
section = (
    '<div class="license" id="slint">\n  <h2>Slint Royalty-free Desktop, Mobile, and Web Applications License 2.0</h2>\n'
    '  <p class="crates">Used by: slint and the i-slint-* crates (WinPods uses Slint under this licence).</p>\n'
    f"  <pre>{html.escape(slint)}</pre>\n</div>"
)
text = src.read_text(encoding="utf-8")
if "<!-- SLINT_LICENSE -->" not in text:
    sys.exit("template marker missing")
out.write_text(text.replace("<!-- SLINT_LICENSE -->", section), encoding="utf-8")
print(f"wrote {out}")
