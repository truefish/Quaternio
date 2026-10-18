#!/usr/bin/env python3
"""Fetch verified sidecars and record their provenance. Python 3.11+, stdlib only."""
import argparse
import hashlib
import json
from pathlib import Path, PurePosixPath
import platform
import shutil
import subprocess
import tarfile
import tempfile
import time
import urllib.request
import zipfile

ROOT = Path(__file__).resolve().parent


def sha256(path):
    with path.open('rb') as source:
        return hashlib.file_digest(source, 'sha256').hexdigest()


def fetch(asset, cache):
    path = cache / asset['file']
    if not path.exists():
        # Never cache a partial or unchecked download under the final name.
        with tempfile.NamedTemporaryFile(dir=cache, delete=False) as temporary:
            partial = Path(temporary.name)
        try:
            for attempt in range(3):
                try:
                    with urllib.request.urlopen(asset['url'], timeout=60) as source, partial.open('wb') as dest:
                        shutil.copyfileobj(source, dest)
                    break
                except OSError:
                    if attempt == 2:
                        raise
                    time.sleep(2 ** attempt)
            verify(partial, asset['sha256'])
            partial.replace(path)
        finally:
            partial.unlink(missing_ok=True)
    verify(path, asset['sha256'])
    return path


def verify(path, expected):
    actual = sha256(path)
    if actual != expected:
        raise RuntimeError(f'SHA256 mismatch for {path}: expected {expected}, got {actual}')


def unpack(path, output, names):
    # Read selected regular files only; archive paths never become output paths.
    found = set()
    with (zipfile.ZipFile(path) if zipfile.is_zipfile(path) else tarfile.open(path)) as archive:
        entries = archive.infolist() if isinstance(archive, zipfile.ZipFile) else archive.getmembers()
        for entry in entries:
            member = entry.filename if isinstance(archive, zipfile.ZipFile) else entry.name
            name = PurePosixPath(member).name
            if name not in names or '__MACOSX' in PurePosixPath(member).parts:
                continue
            if name in found:
                raise RuntimeError(f'Ambiguous {name} in {path}')
            if isinstance(archive, tarfile.TarFile):
                if not entry.isfile():
                    raise RuntimeError(f'Not a regular file: {member}')
                source = archive.extractfile(entry)
            else:
                if entry.is_dir() or (entry.external_attr >> 16) & 0o170000 == 0o120000:
                    raise RuntimeError(f'Not a regular file: {member}')
                source = archive.open(entry)
            with source, (output / name).open('wb') as destination:
                shutil.copyfileobj(source, destination)
            found.add(name)
    return found


def prepare(target, cache, output):
    manifest = json.loads((ROOT / 'ffmpeg-lock.json').read_text())
    spec = manifest['targets'][target]
    expected_system, expected_machine = spec['host']
    machine = platform.machine().lower()
    if platform.system() != expected_system or machine not in expected_machine:
        raise RuntimeError(f'{target} must run on {spec["host"]}; got {platform.system()} {machine}')
    cache.mkdir(parents=True, exist_ok=True)
    output.mkdir(parents=True, exist_ok=True)
    notices = output / 'FFmpeg-notices'
    notices.mkdir(exist_ok=True)
    suffix = '.exe' if expected_system == 'Windows' else ''
    binaries = {'ffmpeg' + suffix, 'ffprobe' + suffix}
    found = set()
    for asset in spec['assets']:
        archive = fetch(asset, cache)
        selected = unpack(archive, output, binaries)
        if found & selected:
            raise RuntimeError('Duplicate binary across archives')
        found |= selected
        unpack(archive, notices, {'LICENSE.txt'})
    if found != binaries:
        raise RuntimeError(f'Missing sidecars: {binaries - found}')
    for binary in sorted(binaries):
        path = (output / binary).resolve()
        path.chmod(0o755)
        if expected_system == 'Darwin':
            subprocess.run(['codesign', '--force', '-s', '-', str(path)], check=True)
        for flag, label in [('-version', 'version'), ('-buildconf', 'configuration'), ('-L', 'license')]:
            result = subprocess.run([str(path), flag], check=True, stdout=subprocess.PIPE,
                                    stderr=subprocess.STDOUT, timeout=60)
            (notices / f'{binary}-{label}.txt').write_bytes(result.stdout)
    shutil.copytree(ROOT / 'licenses', notices / 'licenses', dirs_exist_ok=True)
    shutil.copyfile(ROOT / 'FFmpeg-NOTICE.txt', notices / 'README.txt')
    record = {'target': target, **spec, 'verification': manifest['verification'],
              'packaged_binary_sha256': {name: sha256(output / name) for name in sorted(binaries)}}
    (notices / 'provenance.json').write_text(json.dumps(record, indent=2) + '\n', encoding='utf-8')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('target', choices=['windows-x86_64', 'linux-x86_64', 'macos-arm64', 'macos-x86_64'])
    parser.add_argument('--cache', type=Path, default=ROOT / 'cache')
    parser.add_argument('--output', type=Path, default=ROOT / 'cache' / 'prepared')
    args = parser.parse_args()
    prepare(args.target, args.cache, args.output)
