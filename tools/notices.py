"""Build the third-party notices from cargo-about output plus the Slint licence text.

Usage:
  python tools/notices.py <cargo-about-output.html> <out.html>         HTML for the zip
  python tools/notices.py --text <cargo-about-output.txt> <out.txt>    text embedded in WinPods.exe
"""
import html
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
SLINT = (ROOT / "licenses" / "LicenseRef-Slint-Royalty-free-2.0.md").read_text(encoding="utf-8")
SLINT_TITLE = "Slint Royalty-free Desktop, Mobile, and Web Applications License 2.0"


def build_html(src: pathlib.Path, out: pathlib.Path) -> None:
    section = (
        f'<div class="license" id="slint">\n  <h2>{SLINT_TITLE}</h2>\n'
        '  <p class="crates">Used by: slint and the i-slint-* crates (WinPods uses Slint under this licence).</p>\n'
        f"  <pre>{html.escape(SLINT)}</pre>\n</div>"
    )
    text = src.read_text(encoding="utf-8")
    if "<!-- SLINT_LICENSE -->" not in text:
        sys.exit("template marker missing")
    out.write_text(text.replace("<!-- SLINT_LICENSE -->", section), encoding="utf-8")


def build_text(src: pathlib.Path, out: pathlib.Path) -> None:
    body = src.read_text(encoding="utf-8").strip()
    if not body:
        sys.exit("empty cargo-about output")
    text = (
        "WinPods third-party license notices\n\n"
        "WinPods is MIT licensed. It includes the following open-source components.\n\n"
        f"== {SLINT_TITLE} ==\nUsed by: slint and the i-slint-* crates "
        "(WinPods uses Slint under this licence)\n\n"
        f"{SLINT.strip()}\n\n{body}\n"
    )
    out.write_text(text, encoding="utf-8", newline="\n")


if __name__ == "__main__":
    args = sys.argv[1:]
    if args[:1] == ["--text"] and len(args) == 3:
        build_text(pathlib.Path(args[1]), pathlib.Path(args[2]))
    elif len(args) == 2:
        build_html(pathlib.Path(args[0]), pathlib.Path(args[1]))
    else:
        sys.exit(__doc__)
    print(f"wrote {args[-1]}")
