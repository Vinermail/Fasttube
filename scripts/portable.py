"""Builds the portable Fasttube folder: exe + api sources + a Python runtime.

Variant A of the release plan: python-build-standalone plus site-packages
installed with `uv pip install --target`, started as `python.exe -m
ytapi.server` with PYTHONPATH pointing at `api/src`. No interpreter, no uv and
no venv of the development machine is involved at run time.

    python scripts/portable.py            # build dist/portable + the zip
    python scripts/portable.py --no-zip   # only the folder

The download is cached in dist/python.tar.gz; dist/ is git-ignored.
"""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
import tarfile
import tomllib
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DIST = ROOT / "dist"
CACHE = DIST / "python.tar.gz"
PYDIR = DIST / "python"
RUNTIME = DIST / "portable"
REQUIREMENTS = DIST / "runtime-requirements.txt"

# Pinned so a rebuild in a year still produces what was tested. Bump both
# numbers together, then run this script and the sandbox check again.
PYTHON_RELEASE = "20261026"
PYTHON_ASSET = (
    f"cpython-3.12.16%2B{PYTHON_RELEASE}-x86_64-pc-windows-msvc-install_only_stripped.tar.gz"
)
PYTHON_URL = (
    f"https://github.com/astral-sh/python-build-standalone/releases/download/"
    f"{PYTHON_RELEASE}/{PYTHON_ASSET}"
)

# Only what the sidecar imports at run time. grpcio-tools is a code generator
# for the protoc step and pip is a package installer: neither belongs in a
# shipped runtime.
EXCLUDED = ("grpcio-tools", "setuptools", "pip")


def run(command: list[str], **kwargs) -> None:
    printable = " ".join(command)
    print(f"  $ {printable}", flush=True)
    subprocess.run(command, check=True, **kwargs)


def rm(path: Path) -> None:
    if path.is_dir():
        shutil.rmtree(path, ignore_errors=True)
    elif path.exists():
        path.unlink()


def download_runtime() -> None:
    if PYDIR.joinpath("python.exe").exists():
        print("[python] already unpacked")
        return
    if not CACHE.exists():
        print(f"[python] downloading {PYTHON_URL}")
        run(
            [
                "curl",
                "-L",
                "--fail",
                "--max-time",
                "900",
                "-o",
                str(CACHE),
                PYTHON_URL,
            ]
        )
    print("[python] unpacking")
    with tarfile.open(CACHE) as archive:
        archive.extractall(DIST, filter="data")


def project_version() -> str:
    with (ROOT / "Cargo.toml").open("rb") as handle:
        return tomllib.load(handle)["package"]["version"]


def runtime_requirements() -> list[str]:
    """The lock file's exact versions, minus what is only used to build."""
    print("[deps] exporting api/uv.lock")
    run(
        [
            "uv",
            "export",
            "--project",
            "api",
            "--format",
            "requirements-txt",
            "--no-hashes",
            "--no-dev",
            "--no-emit-project",
            "-o",
            str(REQUIREMENTS),
        ],
        cwd=ROOT,
    )
    kept: list[str] = []
    for line in REQUIREMENTS.read_text(encoding="utf-8").splitlines():
        if not line or line.startswith("#") or line.startswith(" "):
            continue
        name = line.split("==")[0].split(">")[0].strip().lower()
        if name in EXCLUDED:
            continue
        kept.append(line)
    REQUIREMENTS.write_text("\n".join(kept) + "\n", encoding="utf-8")
    return kept


def install_deps() -> None:
    site_packages = PYDIR / "Lib" / "site-packages"
    # Always exported: the file is also the record of what a build shipped with.
    runtime_requirements()
    stamp = site_packages / "yt_dlp"
    if stamp.exists():
        print("[deps] already installed")
    else:
        print("[deps] installing into the bundled runtime")
    run(
        [
            "uv",
            "pip",
            "install",
            "--target",
            str(site_packages),
            "--python",
            str(PYDIR / "python.exe"),
            "-r",
            str(REQUIREMENTS),
        ],
        cwd=ROOT,
    )
    trim(site_packages)


def trim(site_packages: Path) -> None:
    """Removes what only the build needs: uv seeds pip into a fresh target, and
    a shipped runtime installs nothing. Always runs, also on a cached install."""
    dropped = []
    for pattern in ("pip", "pip-*", "setuptools", "setuptools-*", "pkg_resources"):
        for path in site_packages.glob(pattern):
            rm(path)
            dropped.append(path.name)
    for path in (PYDIR / "Scripts").glob("pip*.exe"):
        rm(path)
        dropped.append(path.name)
    if dropped:
        print(f"[deps] removed from the runtime: {', '.join(sorted(set(dropped)))}")


def copy_api(target: Path) -> None:
    """Sources only: no tests, no lock file, no caches, no venv."""
    package = target / "api" / "src" / "ytapi"
    rm(target / "api")
    package.parent.mkdir(parents=True, exist_ok=True)
    shutil.copytree(
        ROOT / "api" / "src" / "ytapi",
        package,
        ignore=shutil.ignore_patterns("__pycache__", "*.pyc", ".pytest_cache"),
    )


def assemble() -> Path:
    print("[layout] assembling dist/portable")
    rm(RUNTIME)
    RUNTIME.mkdir(parents=True)
    copy_api(RUNTIME)
    print("[layout] copying the runtime")
    shutil.copytree(PYDIR, RUNTIME / "python")
    exe = ROOT / "target" / "release" / "fasttube.exe"
    if not exe.exists():
        raise SystemExit("target/release/fasttube.exe is missing: run cargo build --release")
    shutil.copy2(exe, RUNTIME / "fasttube.exe")
    return RUNTIME


def smoke(folder: Path) -> None:
    """Starts the sidecar exactly as the app does: bundled python, api on the
    import path, nothing from this machine."""
    print("[check] importing the sidecar with the bundled runtime")
    env = {
        "PATH": r"C:\Windows\System32",
        "PYTHONPATH": str(folder / "api" / "src"),
        "PYTHONNOUSERSITE": "1",
        "PYTHONIOENCODING": "utf-8",
        # A smoke run must not leave bytecode in the tree that is about to be
        # packed: the api sources ship without __pycache__.
        "PYTHONDONTWRITEBYTECODE": "1",
        "SYSTEMROOT": os.environ.get("SYSTEMROOT", r"C:\Windows"),
        "LOCALAPPDATA": os.environ.get("LOCALAPPDATA", ""),
        "TEMP": os.environ.get("TEMP", ""),
        "TMP": os.environ.get("TMP", ""),
    }
    run(
        [
            str(folder / "python" / "python.exe"),
            "-c",
            "import ytapi.server, grpc, yt_dlp, ytmusicapi, sys;"
            "print('sidecar imports:', sys.version.split()[0], 'prefix:', sys.prefix)",
        ],
        env=env,
        cwd=folder,
    )


def drop_bytecode(folder: Path) -> None:
    """The api tree is source: no __pycache__ from the smoke run or a stray run."""
    removed = 0
    for cache in (folder / "api").rglob("__pycache__"):
        shutil.rmtree(cache, ignore_errors=True)
        removed += 1
    if removed:
        print(f"[layout] dropped {removed} __pycache__ from api")


def zip_folder(folder: Path, version: str) -> Path:
    archive = DIST / f"Fasttube-{version}-windows-x64-portable.zip"
    print(f"[zip] {archive.name}")
    rm(archive)
    with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED, compresslevel=6) as bundle:
        for path in sorted(folder.rglob("*")):
            if path.is_file():
                bundle.write(path, Path(f"Fasttube-{version}") / path.relative_to(folder))
    return archive


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--no-zip", action="store_true", help="skip the archive")
    args = parser.parse_args()

    version = project_version()
    download_runtime()
    install_deps()
    folder = assemble()
    smoke(folder)
    drop_bytecode(folder)
    if args.no_zip:
        print(f"[done] {folder}")
        return 0
    archive = zip_folder(folder, version)
    size = archive.stat().st_size / (1024 * 1024)
    print(f"[done] {folder} and {archive.name} ({size:.1f} MiB)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
