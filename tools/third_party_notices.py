"""Write THIRD_PARTY_NOTICES.txt: the licences of everything built into remsoundlite.exe.

    python tools/third_party_notices.py [output] [--wx-dir DIR]

wxWidgets' licences come from its source tree: the one the build downloads into target/release,
or, when that isn't there, the same archive fetched here and checked against the SHA-256
wxdragon-sys pins. (A CI run whose build came from the cache never runs wxdragon-sys's build
script, and rust-cache deletes that folder before saving, so it is often missing.) Needs
cargo-about 0.9 on PATH (`cargo install cargo-about --locked --features cli`).

cargo-about covers the Rust crates (see about.toml). It can't see the C and C++ libraries some
crates compile in, so those licences are read from their own source trees here:
wxWidgets and the libraries it bundles (wxdragon-sys), libopus (audiopus_sys) and prism with
its bundled code (prism-sys). Every file listed must exist, so a dependency update that moves
one fails the build instead of silently dropping a notice.
"""

import argparse
import hashlib
import io
import json
import os
import re
import subprocess
import sys
import tempfile
import urllib.request
import zipfile
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


def wx_licences_from_archive(wxdragon_sys, dest):
    """Fetches the wxWidgets source archive wxdragon-sys builds from and unpacks just the
    licence files into `dest`, laid out like the source tree. Returns `dest`."""
    build_rs = (wxdragon_sys / "build.rs").read_text(encoding="utf-8")
    url = re.search(r'const WX_SRC_URL: &str = "([^"]+)"', build_rs)
    sha = re.search(r'const WX_SRC_URL_SHA256: &str = "([0-9a-f]{64})"', build_rs)
    if not url or not sha:
        sys.exit(f"Can't find WX_SRC_URL and WX_SRC_URL_SHA256 in {wxdragon_sys / 'build.rs'}")
    url, sha = url[1], sha[1]
    print(f"No wxWidgets source from the build; fetching {url}")
    with urllib.request.urlopen(url, timeout=300) as response:
        data = response.read()
    if hashlib.sha256(data).hexdigest() != sha:
        sys.exit(f"{url} doesn't match the SHA-256 wxdragon-sys expects")
    wanted = [f for _, _, files in WX for f in files] + ["src/jpeg/README"]
    with zipfile.ZipFile(io.BytesIO(data)) as archive:
        names = set(archive.namelist())
        # The archive may or may not put everything under one top-level folder.
        marker = next((n for n in names if n.endswith("docs/licence.txt")), None)
        if marker is None:
            sys.exit(f"{url} has no docs/licence.txt")
        prefix = marker[: -len("docs/licence.txt")]
        for relative in wanted:
            if prefix + relative not in names:
                sys.exit(f"Licence file not found in the wxWidgets archive: {relative}")
            target = dest / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(archive.read(prefix + relative))
    return dest


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

    dirs = crate_dirs()
    with tempfile.TemporaryDirectory() as tmp:
        wx = Path(args.wx_dir) if args.wx_dir else ROOT / "target" / "release" / "wxWidgets"
        if not (wx / "docs" / "licence.txt").is_file():
            if args.wx_dir:
                sys.exit(f"No wxWidgets source at {wx}")
            wx = wx_licences_from_archive(dirs["wxdragon-sys"], Path(tmp) / "wxWidgets")
        write_notices(args.output, wx, dirs, Path(tmp))


def write_notices(output, wx, dirs, tmp):
    # Through a file: cargo-about refuses to write to stdout when started from PowerShell, which
    # is the default shell of GitHub's Windows runners.
    crates_file = tmp / "crates.txt"
    result = subprocess.run(
        ["cargo", "about", "generate", "--locked", "--fail", "about.hbs",
         "-o", str(crates_file)],
        cwd=ROOT, capture_output=True, text=True, encoding="utf-8", errors="replace",
    )
    if result.returncode != 0:
        sys.exit(f"cargo about failed:\n{result.stderr}")
    crates = crates_file.read_text(encoding="utf-8").strip()

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
    Path(output).write_text(text, encoding="utf-8", newline="\r\n")
    print(f"Wrote {output} ({len(text.splitlines())} lines)")


if __name__ == "__main__":
    main()
