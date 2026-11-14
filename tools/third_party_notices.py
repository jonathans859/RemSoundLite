"""Write THIRD_PARTY_NOTICES.txt: the licences of everything built into remsoundlite.exe.

    python tools/third_party_notices.py [output] [--wx-dir DIR]

Run after `cargo build --release` (wxWidgets' source is downloaded into target/release by the
build). Needs cargo-about 0.9 on PATH (`cargo install cargo-about --locked --features cli`).

cargo-about covers the Rust crates (see about.toml). It can't see the C and C++ libraries some
crates compile in, so those licences are read from their own source trees here:
wxWidgets and the libraries it bundles (wxdragon-sys), libopus (audiopus_sys) and prism with
its bundled code (prism-sys). Every file listed must exist, so a dependency update that moves
one fails the build instead of silently dropping a notice.
"""

import argparse
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RULE = "-" * 78

INTRO = """\
Third-party notices for RemSoundLite
====================================

RemSoundLite is MIT licensed (see LICENSE). It is built with the software listed below, each
under its own licence, reproduced here as those licences require.

The complete source code of RemSoundLite, and with it the means to rebuild it against modified
versions of the libraries below (as the LGPL and MPL parts allow), is available at
https://github.com/jonathans859/RemSoundLite

"""

# (component, description, files relative to the wxWidgets source tree)
WX = [
    ("wxWidgets", "wxWindows Library Licence 3.1, which builds on the LGPL 2.0 below",
     ["docs/licence.txt", "docs/lgpl.txt"]),
    ("zlib (bundled with wxWidgets)", "zlib licence", ["src/zlib/LICENSE"]),
    ("libpng (bundled with wxWidgets)", "libpng licence", ["src/png/LICENSE"]),
    ("libtiff (bundled with wxWidgets)", "libtiff licence", ["src/tiff/LICENSE.md"]),
    ("Expat (bundled with wxWidgets)", "MIT", ["src/expat/expat/COPYING"]),
    ("PCRE2 (bundled with wxWidgets)", "BSD-3-Clause", ["3rdparty/pcre/LICENCE.md"]),
    ("NanoSVG (bundled with wxWidgets)", "zlib licence", ["3rdparty/nanosvg/LICENSE.txt"]),
    ("LunaSVG and PlutoVG (bundled with wxWidgets)", "MIT",
     ["3rdparty/lunasvg/LICENSE", "3rdparty/lunasvg/plutovg/LICENSE"]),
    ("libwebp (bundled with wxWidgets)", "BSD-3-Clause with a patent grant",
     ["3rdparty/libwebp/COPYING", "3rdparty/libwebp/PATENTS"]),
]


def crate_dirs():
    """Source directory of every package in the build, by crate name."""
    out = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked"],
        cwd=ROOT, check=True, capture_output=True, text=True, encoding="utf-8",
    ).stdout
    return {p["name"]: Path(p["manifest_path"]).parent for p in json.loads(out)["packages"]}


def read(path):
    if not path.is_file():
        sys.exit(f"Licence file not found: {path}")
    return path.read_text(encoding="utf-8", errors="replace").strip()


def section(name, description, texts):
    parts = [RULE, f"{name} ({description})", ""]
    for text in texts:
        parts += [text, ""]
    return "\n".join(parts) + "\n"


def libjpeg_legal(wx):
    """libjpeg keeps its licence in the LEGAL ISSUES part of its README."""
    lines = read(wx / "src/jpeg/README").splitlines()
    try:
        start = lines.index("LEGAL ISSUES")
        end = lines.index("REFERENCES")
    except ValueError:
        sys.exit("libjpeg's README no longer has its LEGAL ISSUES section where expected")
    return "\n".join(lines[start:end]).strip()


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("output", nargs="?", default="THIRD_PARTY_NOTICES.txt")
    parser.add_argument("--wx-dir", default=os.environ.get("WXWIDGETS_DIR"),
                        help="wxWidgets source tree (default: target/release/wxWidgets)")
    args = parser.parse_args()

    wx = Path(args.wx_dir) if args.wx_dir else ROOT / "target" / "release" / "wxWidgets"
    if not (wx / "docs" / "licence.txt").is_file():
        sys.exit(f"No wxWidgets source at {wx}; run `cargo build --release` first")

    # Through a file: cargo-about refuses to write to stdout when started from PowerShell, which
    # is the default shell of GitHub's Windows runners.
    with tempfile.TemporaryDirectory() as tmp:
        crates_file = Path(tmp) / "crates.txt"
        result = subprocess.run(
            ["cargo", "about", "generate", "--locked", "--fail", "about.hbs",
             "-o", str(crates_file)],
            cwd=ROOT, capture_output=True, text=True, encoding="utf-8", errors="replace",
        )
        if result.returncode != 0:
            sys.exit(f"cargo about failed:\n{result.stderr}")
        crates = crates_file.read_text(encoding="utf-8").strip()

    dirs = crate_dirs()
    native = []
    for name, description, files in WX:
        native.append(section(name, description, [read(wx / f) for f in files]))
    native.append(section("libjpeg (bundled with wxWidgets)", "IJG licence", [libjpeg_legal(wx)]))
    native.append(section("Opus (built by audiopus_sys)", "BSD-3-Clause",
                          [read(dirs["audiopus_sys"] / "opus" / "COPYING")]))
    # prism ships one folder per component it builds in or draws on, each with its licence.
    prism = dirs["prism-sys"] / "prism"
    native.append(section("prism (built by prism-sys)", "MPL-2.0; source at "
                          "https://github.com/ethindp/prism", [read(prism / "LICENSE")]))
    for folder in sorted(p for p in (prism / "LICENSES").iterdir() if p.is_dir()):
        if folder.name == "prism":
            continue  # the same MPL text as above
        files = sorted(f for f in folder.iterdir() if f.is_file())
        if not files:
            sys.exit(f"No licence file in {folder}")
        native.append(section(folder.name, "bundled with prism", [read(f) for f in files]))

    text = (INTRO + "Native libraries\n================\n\n" + "".join(native) + "\n"
            + crates + "\n")
    Path(args.output).write_text(text, encoding="utf-8", newline="\r\n")
    print(f"Wrote {args.output} ({len(text.splitlines())} lines)")


if __name__ == "__main__":
    main()
