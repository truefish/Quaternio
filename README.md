<p align="center">
  <img src="brand/quaternio.svg" alt="Quaternio" width="96" height="96"/>
</p>

# Quaternio

A free, open-source video editor written in Rust. Beta builds available for Windows, Linux, and macOS.


## Versions and project files

App version (the release you download, e.g. `0.1.0-beta.1`) is independent of the `.quaternio` project schema (`PROJECT_VERSION` in the file). Updating Quaternio does not require re-saving projects unless a release notes a schema change. Older projects keep loading; projects saved with a newer schema need a matching or newer editor.

In Preferences → **Versions**, you can browse published GitHub releases and install or switch builds. When a newer release exists, a quiet toolbar badge appears — no modal on launch.

## Quickstart

Download a build from [Releases](../../releases). Each archive already includes `ffmpeg` and `ffprobe` next to the Quaternio binary.

### Windows (x64)

**Installer**

1. Download `Quaternio-Setup-v….exe` (example: `Quaternio-Setup-v0.1.0-beta.1.exe`).
2. Run the installer.
3. Open **Quaternio** from the Start menu.

Quaternio installs under `%LOCALAPPDATA%\Quaternio` (no admin required). Windows may show a SmartScreen warning on this unsigned beta — choose **More info** → **Run anyway**.

**Portable zip**

1. Download `Quaternio-Windows-Portable-v….zip`.
2. Extract the zip to a folder.
3. Open that folder and double-click `quaternio.exe`.

Keep `quaternio.exe`, `ffmpeg.exe`, and `ffprobe.exe` in the same folder.

### Linux (x86_64)

Release builds are **x86_64 only** (not ARM). You need a desktop session (X11 or Wayland).

1. Download `Quaternio-Linux-x86_64-v….tar.gz`.
2. Extract and enter the folder:

```bash
tar -xzf Quaternio-Linux-x86_64-v0.1.0-beta.1.tar.gz
cd Quaternio-Linux-x86_64-v0.1.0-beta.1
```

(Use the exact file and folder names from the release.)

3. Run:

```bash
./quaternio
```

Linux builds default to the OpenGL (FemtoVG) renderer so button presses keep redrawing under Wayland. Drag-and-drop from the file manager works on X11, Windows, and macOS; native Wayland does not deliver file-drop events yet (winit), so use **File → Import** there, or:

```bash
WINIT_UNIX_BACKEND=x11 ./quaternio
```

If OpenGL is unavailable and the UI looks frozen while clicks still “work”, force software rendering or X11:

```bash
SLINT_BACKEND=winit-software ./quaternio
# or:
WINIT_UNIX_BACKEND=x11 ./quaternio
```

If the app fails to start because a library is missing, install common desktop libraries (Debian/Ubuntu):

```bash
sudo apt install libxcb-shape0 libxcb-xfixes0 libxkbcommon0 libxkbcommon-x11-0 \
  libfontconfig1 libx11-xcb1 libxcursor1 libwayland-client0 libegl1 libgl1 \
  libasound2t64 || \
sudo apt install libxcb-shape0 libxcb-xfixes0 libxkbcommon0 libxkbcommon-x11-0 \
  libfontconfig1 libx11-xcb1 libxcursor1 libwayland-client0 libegl1 libgl1 \
  libasound2
```

### macOS

This beta ships **Apple Silicon (`arm64`)** builds (`Quaternio-macOS-arm64-v….tar.gz`). Intel macOS downloads are deferred while GitHub’s older Intel runners are unavailable.

1. Download the arm64 archive.
2. Extract and enter the folder:

```bash
tar -xzf Quaternio-macOS-arm64-v0.1.0-beta.1.tar.gz
cd Quaternio-macOS-arm64-v0.1.0-beta.1
```

(Use the exact names from the release.)

3. Clear Gatekeeper quarantine (needed for this unsigned beta):

```bash
xattr -dr com.apple.quarantine .
```

4. Run:

```bash
./quaternio
```

If macOS still blocks it: System Settings → Privacy & Security → allow Quaternio, or in Finder right-click `quaternio` → **Open**.

## Build from source

Use this to compile Quaternio yourself.

You need:

- [Rust](https://rustup.rs/) (stable)
- **FFmpeg** and **ffprobe** on your `PATH`, or copied next to the built binary before you run it

### Windows (x64)

1. Install [Rust](https://rustup.rs/) (default MSVC toolchain).
2. Install [Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/) with the **Desktop development with C++** workload (required by the MSVC linker).
3. Install FFmpeg so `ffmpeg.exe` and `ffprobe.exe` are on your `PATH`, or plan to copy them next to the built exe.
4. In this repo folder:

```bat
cargo build --release
```

5. Run:

```bat
target\release\quaternio.exe
```

If FFmpeg is not on `PATH`, copy `ffmpeg.exe` and `ffprobe.exe` into `target\release\` first.

### Linux (x86_64)

1. Install [Rust](https://rustup.rs/).
2. Install build packages (Debian/Ubuntu):

```bash
sudo apt update
sudo apt install build-essential pkg-config \
  libxcb-shape0-dev libxcb-xfixes0-dev libxkbcommon-dev \
  libfontconfig-dev libx11-xcb-dev libxcursor-dev \
  libxkbcommon-x11-dev libx11-dev libwayland-dev libasound2-dev \
  libegl1-mesa-dev libgl1-mesa-dev
```

3. Install FFmpeg (`ffmpeg` and `ffprobe` on your `PATH`), or plan to copy them next to the binary.
4. In this repo folder:

```bash
cargo build --release
```

5. Run:

```bash
./target/release/quaternio
```

If FFmpeg is not on `PATH`, copy `ffmpeg` and `ffprobe` into `target/release/` first.

### macOS (Apple Silicon or Intel)

1. Install [Rust](https://rustup.rs/).
2. Install Xcode Command Line Tools:

```bash
xcode-select --install
```

3. Install FFmpeg (`ffmpeg` and `ffprobe` on your `PATH`), for example with Homebrew:

```bash
brew install ffmpeg
```

4. In this repo folder:

```bash
cargo build --release
```

5. Run:

```bash
./target/release/quaternio
```

## License

MIT ([LICENSE](LICENSE)). Release builds also include FFmpeg (its own license) and the Inter font ([OFL](ui/fonts/LICENSE.txt)).
