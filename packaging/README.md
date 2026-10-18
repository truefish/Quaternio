# Packaging

## Publish a release

Use the `main` branch. Install Git, Rust, and Python 3.11 or newer, and sign in
to GitHub for Git pushes. Stage the files you want to release, including new
source files and assets. Review them with `git diff --cached --stat`.
Unstaged changes and untracked files are not included.

From the repository folder, run in Git Bash, Linux, or macOS:

```sh
bash ./release.sh
bash ./release.sh --publish
```

The first command previews the version and staged files. The second updates the
Cargo version files, commits staged changes, creates a tag, and pushes `main` and
the tag together. The next beta number is selected automatically. To choose a
version, add `--version 0.2.0-beta.1` to both commands. Existing tags are not replaced.

In PowerShell, use `python scripts/release.py` and
`python scripts/release.py --publish` instead.

Watch the [Release workflow](https://github.com/truefish/Quaternio/actions/workflows/release.yml).
Once all builds succeed, the [GitHub release](https://github.com/truefish/Quaternio/releases)
contains the Windows installer and portable ZIP, Linux archive, and Apple Silicon
and Intel Mac archives. A successful push starts the builds; it does not mean
the packages are ready yet.

## Prepare FFmpeg

Run with Python 3.11+ on the matching native host:

```sh
python packaging/ffmpeg.py windows-x86_64
python packaging/ffmpeg.py linux-x86_64
python packaging/ffmpeg.py macos-arm64
python packaging/ffmpeg.py macos-x86_64
```

Choose one target. Archives go in `packaging/cache`; prepared binaries and
`FFmpeg-notices` go in `packaging/cache/prepared`. Override with `--cache` and
`--output`. Use fresh output/staging directories for manual packaging.

`ffmpeg-lock.json` pins archive URLs and SHA256 hashes. Downloads and cached
archives are verified before extraction. Investigate mismatches before changing
pins; review and verify replacement archives, and retain an authorized mirror
before upstream downloads expire.

`release.yml` packages Windows, Linux x86_64, and both macOS architectures.
Application artifacts, including NSIS, contain no FFmpeg executables. Runtime setup uses this lock manifest, verifies downloads and required capabilities, retains license/source notices and tool build information, and stores the engine outside application release folders. Existing compatible installations can be selected without downloading. The preparation script remains available for development and offline distribution.
`check.yml` runs `cargo check` and behavioral tests on those platforms; physical GPU/export validation remains separate.

## Limits

- These are checksum-pinned third-party binaries, not reproducible source builds.
  Managed media-engine setup downloads and inspects FFmpeg; application release
  jobs do not bundle or compile it.
- Exact dependency source revisions, patches, and a complete corresponding
  source bundle have not been established and are unavailable in this repository.
  Notices and builder links do not establish complete license compliance.
- Windows preparation and all archive hashes/extraction were checked locally.
  Ubuntu 22.04 under WSL2 passed optimized builds, X11 preview startup with both
  renderers, and CPU exports with installed FFmpeg 4.4.2. Physical Linux GPU/Wayland,
  macOS runtime, managed Linux engine download, and NSIS compilation still need validation.
  Intel binaries match the builder's published hashes and have x86_64 headers.
- macOS packages use ad-hoc signing, without Developer ID signing or notarization.
  FFmpeg versions differ by platform; codec parity is not established.

For reproducible builds, first retain verified FFmpeg and dependency sources,
patches and licenses; pin toolchains and configure flags; validate real exports
on each architecture; then compare independent clean rebuilds. That source-build
pipeline is future work, not implemented here.
