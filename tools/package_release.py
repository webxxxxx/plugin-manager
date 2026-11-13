#!/usr/bin/env python3
"""Package one release target.

The core ships as a single multicall binary: `svcd` plus the `svc`, `plugin`
and `mcp` entry names dispatch on argv[0]. A release archive therefore holds
the same executable under all four names, together with the licence and the
config example, under a single top-level directory.

Archives are deterministic (fixed entry metadata, no build timestamp) so the
same binary produces the same bytes on every run. Unix targets are tar.gz with
the aliases stored as hardlinks; the Windows target is a zip with plain copies,
because zip cannot express hardlinks.

Exit codes: 0 = ok, 2 = bad input or I/O error.
"""
import argparse
import gzip
import hashlib
import os
import re
import shutil
import sys
import tarfile
import tempfile
import zipfile

EPOCH = 315532800  # 1980-01-01T00:00:00Z; zip cannot store an earlier mtime.
TARGET_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.-]*$")
VERSION_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.+-]*$")
ALIASES = ("svc", "plugin", "mcp")


def fail(message):
    print("error: " + message, file=sys.stderr)
    return 2


def chmod(path, mode):
    try:
        os.chmod(path, mode)
    except OSError:
        pass  # Windows only tracks the read-only bit; the archive carries the mode.


def stage(root, binary, extras, exe):
    """Lay the archive tree out and return the executable entry names."""
    entry = "svcd" + exe
    staged = os.path.join(root, entry)
    shutil.copyfile(binary, staged)
    chmod(staged, 0o755)
    for alias in ALIASES:
        target = os.path.join(root, alias + exe)
        try:
            os.link(staged, target)
        except OSError:
            shutil.copyfile(staged, target)
        chmod(target, 0o755)
    for path in extras:
        copy = os.path.join(root, os.path.basename(path))
        shutil.copyfile(path, copy)
        chmod(copy, 0o644)
    return {entry, *(alias + exe for alias in ALIASES)}


def ordered_names(root, exe):
    """The canonical `svcd` entry first, then the aliases and extras sorted."""
    canonical = "svcd" + exe
    return [canonical] + [name for name in sorted(os.listdir(root)) if name != canonical]


def write_tar(root, archive, top, exec_names, exe):
    def normalize(info):
        info.uid = 0
        info.gid = 0
        info.uname = ""
        info.gname = ""
        info.mtime = EPOCH
        info.mode = 0o755 if os.path.basename(info.name) in exec_names else 0o644
        return info

    with open(archive, "wb") as raw:
        with gzip.GzipFile(fileobj=raw, mode="wb", compresslevel=9, mtime=0, filename="") as gz:
            with tarfile.open(fileobj=gz, mode="w", format=tarfile.GNU_FORMAT) as tar:
                head = tarfile.TarInfo(top)
                head.type = tarfile.DIRTYPE
                head.mode = 0o755
                head.mtime = EPOCH
                head.uid = head.gid = 0
                head.uname = head.gname = ""
                tar.addfile(head)
                for name in ordered_names(root, exe):
                    tar.add(
                        os.path.join(root, name),
                        arcname=top + "/" + name,
                        recursive=False,
                        filter=normalize,
                    )


def write_zip(root, archive, top, exec_names, exe):
    stamp = (1980, 1, 1, 0, 0, 0)
    with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as zf:
        head = zipfile.ZipInfo(top + "/", date_time=stamp)
        head.create_system = 3
        head.external_attr = 0o040755 << 16
        zf.writestr(head, b"")
        for name in ordered_names(root, exe):
            info = zipfile.ZipInfo(top + "/" + name, date_time=stamp)
            info.compress_type = zipfile.ZIP_DEFLATED
            info.create_system = 3
            info.external_attr = (0o100755 if name in exec_names else 0o100644) << 16
            with open(os.path.join(root, name), "rb") as handle:
                zf.writestr(info, handle.read())


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def main(argv=None):
    parser = argparse.ArgumentParser(description="Package one svcd release target.")
    parser.add_argument("--binary", required=True, help="path to the stripped release binary")
    parser.add_argument("--target", required=True, help="target triple or label, e.g. x86_64-unknown-linux-musl")
    parser.add_argument("--version", required=True, help="crate version without the v prefix")
    parser.add_argument("--outdir", default="dist", help="archive output directory (default: dist)")
    parser.add_argument("--extra", action="append", default=[], help="additional file to put at the archive root; repeatable")
    args = parser.parse_args(argv)

    if not TARGET_RE.match(args.target):
        return fail("invalid --target: " + args.target)
    if not VERSION_RE.match(args.version):
        return fail("invalid --version: " + args.version)
    if not os.path.isfile(args.binary):
        return fail("binary not found: " + args.binary)

    windows = "windows" in args.target
    exe = ".exe" if windows else ""
    reserved = {"svcd" + exe, "SHA256SUMS", *(alias + exe for alias in ALIASES)}
    extra_paths = []
    seen = set()
    for path in args.extra:
        name = os.path.basename(path)
        if not os.path.isfile(path):
            return fail("extra file not found: " + path)
        if name in reserved:
            return fail("extra file collides with a release entry: " + name)
        if name in seen:
            return fail("duplicate extra file: " + name)
        seen.add(name)
        extra_paths.append(path)

    stem = "svcd-{}-{}".format(args.version, args.target)
    suffix = ".zip" if windows else ".tar.gz"
    os.makedirs(args.outdir, exist_ok=True)
    archive = os.path.join(args.outdir, stem + suffix)
    if os.path.exists(archive):
        os.remove(archive)

    with tempfile.TemporaryDirectory(prefix="svcd-package-") as staging:
        root = os.path.join(staging, stem)
        os.makedirs(root)
        exec_names = stage(root, args.binary, extra_paths, exe)
        if windows:
            write_zip(root, archive, stem, exec_names, exe)
        else:
            write_tar(root, archive, stem, exec_names, exe)

    print("archive: " + archive)
    entries = sorted(exec_names | {os.path.basename(p) for p in extra_paths})
    print("entries: " + ", ".join(entries))
    print("size:    {:,} B".format(os.path.getsize(archive)))
    print("sha256:  " + sha256(archive))
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except OSError as error:
        print("error: " + str(error), file=sys.stderr)
        sys.exit(2)
