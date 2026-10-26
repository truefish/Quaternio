<p align="center">
  <img src="brand/quaternio.svg" alt="Quaternio" width="96" height="96"/>
</p>

# Quaternio

A free, open-source video editor written in Rust, with beta builds for Windows, Linux, and macOS.

**[Download](https://github.com/truefish/Quaternio/releases)** · [Quickstart](#quickstart) · [Build from source](#build-from-source)

## Screenshots

**Edit:** browse media, arrange clips, and adjust the picture.

![Editing workspace with source and program previews, a multitrack timeline, and clip controls](docs/screenshots/editing-workspace.png)

**Export:** set the file name, folder, quality, and encoding method.

![Export workspace with sequence preview, destination fields, video settings, and export queue](docs/screenshots/export-workspace.png)

Sample imagery: [NASA](https://www.nasa.gov/image-article/sunrise-over-pacific-2/) and [NASA/Matthew Dominick](https://www.nasa.gov/image-article/sunrise-begins/).

## Features

- **Project manager:** searchable recent projects, pinned favorites, thumbnail cards, and a compact list view.
- **Autosave:** saved projects update automatically every 30 seconds; toggle it under **Preferences → General**.
- **Multitrack editing:** moves, trims, splits, inserts, ripple edits, transitions, and clip properties, with undo/redo.
- **Creator tools:** editable titles and lower thirds, SRT/WebVTT captions, keyframes, clip speed/reverse/freeze, and slip/rolling/slide tools.
- **Audio finishing:** loudness normalization, EQ, compression, voice cleanup, music ducking, and an optional sample-peak ceiling.
- **Sequences and multicam:** alternate versions, nesting, adjustment clips, camera cuts, and manual or waveform synchronization.
- **Picture finishing:** SDR Rec.709, PQ/HLG tone mapping, embedded 3D LUTs, scopes, high-precision composition, and optional GPU processing.
- **Local transcription:** an optional one-time speech-model download, editable captions, and transcript-assisted cuts.
- **Portability:** collect source files, archive a project, or export timeline XML with a compatibility report.
- **Review:** timecoded notes at the playhead, with search, resolve/reopen, and `.qreview` import/export.
- **Live peer editing:** collaborate in real time over direct P2P, with relay fallback.
- **Hardware export:** VideoToolbox, VAAPI, NVIDIA, Intel, and AMD, with automatic CPU fallback.
- **Activity panel:** imports, exports, preview preparation, and media-engine setup at a glance.
- **Track height:** Compact, Medium, or Large (a local preference, not saved in the project).

## Quickstart

Download a build from [Releases](https://github.com/truefish/Quaternio/releases). Archives don't bundle FFmpeg. Quaternio uses a compatible `ffmpeg`/`ffprobe` pair if it finds one. Otherwise, the first import or export offers a one-time setup, or lets you pick installed tools. The managed engine lives in your app-data folder and is shared across releases. You can reopen setup anytime via **Preferences → Media engine**.

### Windows (x64)

**Installer:** run `Quaternio-Setup-v….exe`, then launch **Quaternio** from the Start menu. It installs to `%LOCALAPPDATA%\Quaternio` and needs no admin rights.

**Portable:** extract `Quaternio-Windows-Portable-v….zip` and run `quaternio.exe`.

> This beta is unsigned. If SmartScreen warns you, choose **More info → Run anyway**.

### Linux (x86_64)

Requires X11 or Wayland. ARM is not supported.

```bash
tar -xzf Quaternio-Linux-x86_64-v0.1.0-beta.8.tar.gz
cd Quaternio-Linux-x86_64-v0.1.0-beta.8
./quaternio
```

Use the exact filenames from the release. Quaternio uses OpenGL and prefers X11/XWayland when `DISPLAY` is set, which enables file-manager drag-and-drop. On native Wayland, use **File → Import media** instead.

```bash
WINIT_UNIX_BACKEND=wayland ./quaternio                          # force native Wayland
WINIT_UNIX_BACKEND=x11 SLINT_BACKEND=winit-software ./quaternio # software rendering fallback
```

**Missing library on startup? (Debian/Ubuntu)**

```bash
sudo apt install libxcb-shape0 libxcb-xfixes0 libxkbcommon0 libxkbcommon-x11-0 \
  libfontconfig1 libx11-xcb1 libxcursor1 libwayland-client0 libegl1 libgl1 \
  libasound2t64 || \
sudo apt install libxcb-shape0 libxcb-xfixes0 libxkbcommon0 libxkbcommon-x11-0 \
  libfontconfig1 libx11-xcb1 libxcursor1 libwayland-client0 libegl1 libgl1 \
  libasound2
```

### macOS

Pick `arm64` for Apple Silicon or `x86_64` for Intel (older releases may be Apple Silicon only).

```bash
tar -xzf Quaternio-macOS-arm64-v0.1.0-beta.8.tar.gz
cd Quaternio-macOS-arm64-v0.1.0-beta.8
xattr -dr com.apple.quarantine .   # needed for this unsigned beta
./quaternio
```

If macOS still blocks it, allow it under **System Settings → Privacy & Security**, or right-click `quaternio` in Finder and choose **Open**.

## Projects and versions

Quaternio opens to the project manager. Create a project, open a `.quaternio` file, or return to a recent project. Use **File → Project manager** to come back anytime. Removing an entry from recent projects keeps the file on disk.

Save a new project once to choose its location. Autosave is on by default; interrupted untitled work is protected by the existing recovery checkpoints.

The app version (e.g. `0.1.0-beta.8`) is independent of the `.quaternio` project schema (`PROJECT_VERSION`). Check release notes for schema changes when switching versions. Supported older projects are migrated on load; projects saved with a newer schema may need a newer editor.

**Preferences → Versions** lets you browse GitHub releases and install or switch builds. A quiet toolbar badge appears when a newer release exists, with no launch-time modal.

## Review

Open **Review** in the toolbar to add notes at the playhead, jump between them, and resolve or reopen feedback. Notes are saved with the project and support undo.

1. Save the project, then export a `.qreview` file. Reviewers should start from that same saved project.
2. Importing adds new notes without overwriting your local resolution state.
3. Review files contain notes and project identity only, with no footage or media paths.

Older projects get an identity on load, so save once before distributing review copies.

## Live peer editing

1. **Host:** **Collaborate → Start a session → Create invitation**, then copy the invite code.
2. **Guest:** **Collaborate → Join with code**, then paste it.

The public service at `https://connect.quaternio.app` is configured automatically and is **optional**. Direct P2P is preferred, with relay fallback when needed, including behind CGNAT. Connections show **Direct** or **Relayed**. Disable the public service or choose your own under **Advanced**. Direct/LAN connections also support invitation/reply files.

Edits and review notes synchronize through the host, with viewer permissions, per-author undo and retained conflicting edits. Media stays on the host: guests stream previews and audio, browse thumbnails and waveforms, and upload their own contributions to host storage. Guests can request an export from the host. Preview resolution adapts to connection speed; exports use the original media.

Connections using the coordination service reconnect automatically after temporary interruptions, retaining pending edits and resuming uploads. When the host leaves, the session ends for everyone; there is no host transfer. Manual invitation/reply connections need a new exchange after a disconnection.

### Optional self-hosting

On a public Linux server with Python 3, Docker Compose, this checkout and DNS configured:

```sh
sudo python3 scripts/deploy_relay.py --domain relay.example.com --directory /opt/quaternio-relay
```

Open the printed firewall ports. If you already use a reverse proxy, route the hostname to the printed upstream with WebSocket upgrades; an nginx snippet is generated. Enter your server address and private host key in **Advanced**. Add `--public` to offer anonymous public sessions, or `--on-demand` to stop containers with Enter/Ctrl+C.

Established direct sessions survive service shutdown; relayed sessions need it throughout. ICE currently prepares fallback allocations during setup. Automated network testing covers relay fallback; real ISP CGNAT conditions can vary.

## Create

Open **Create** in the toolbar. Choose a focused tool section for text, animation, audio, precision editing, transcription, multicam, or picture finishing. New text is an editable timeline clip; save a text style to reuse it. Caption files support `.srt` and `.vtt`; video exports can include or omit burned-in captions. Safe-area guides appear only in the editor.

To animate a property, select a clip, set its first keyframe, move the playhead, then adjust that property in the inspector. Linear, hold, and ease interpolation are available. Speed edits also affect linked audio; freeze frames are silent. Slowing a clip requires room after it. Speed changes currently change audio pitch.

Audio mix effects are shared by preview and export. Normalization measures the selected source clip on the host, targeting −16 LUFS while avoiding peaks above −1 dBTP, within the clip gain limit. The optional mix ceiling limits samples to −1 dBFS; it is not a true-peak limiter.

Use the timeline sequence menu to switch versions; each collaborator keeps their own sequence view. **Create → Project and sequences** provides source destinations, sync locks, project collection and XML export. Multicam keeps camera 1 audio continuous while picture angles change.

**Transcription** downloads a 57 MiB model only when requested, then processes speech locally. Review the transcript before adding captions or removing a passage. Models can be cancelled or deleted. Voice cleanup (a high-pass filter and gentle gate) and ducking are reversible; mark dialogue and music tracks before enabling ducking.

**Picture finishing** delivers SDR Rec.709. Tagged PQ/HLG sources are tone mapped; log footage needs a conversion LUT. High precision decodes 16-bit source pixels and composites in floating point. Optional GPU processing supports ordinary layered footage; unsupported effects or adapters use the CPU.

Creator projects use schema **7**. Collaboration requires **0.1.0-beta.9 or newer** on every participant. Older project files migrate on load; save a separate copy when keeping an older editor in use.

## Exports

In **Export**, choose **MP4 video** (H.264 or HEVC when available) or **WAV audio** (48 kHz, 16-bit stereo). The encoder list comes from your FFmpeg installation; hardware support is checked at export. Choosing a specific encoder requires that encoder and does not silently switch to another one. **Automatic** uses the method below:


- **Auto:** hardware encoding with CPU fallback.
- **Software:** CPU encoding.
- **Hardware:** requires a working GPU encoder and fails with an error if none is available.
- **AMD:** requires AMD AMF on Windows or an AMD VAAPI device on Linux.

| Platform | Hardware encoders |
|---|---|
| macOS | VideoToolbox |
| Linux | VAAPI, NVIDIA |
| Windows | NVIDIA, Intel, AMD |

GPU export needs a compatible driver and FFmpeg encoder. Notes:

- AMD can be selected explicitly. Windows also offers an optional D3D11 adapter index.
- Linux VAAPI (including Radeon) needs the host `libva` libraries, a GPU driver, and access to `/dev/dri/renderD*`. Set `QUATERNIO_VAAPI_DEVICE` to choose a render node.
- The encoder and picture-processing route are reported separately, and picture effects may still run on the CPU.
- Click an export status message to view or copy the full details.

## Build from source

**Requirements:** [Rust](https://rustup.rs/) 1.92 or newer, CMake, LLVM/Clang and the platform prerequisites below. For import, playback and export, Quaternio detects a compatible FFmpeg/ffprobe pair or offers one-time Media engine setup. Debug and release builds share the managed engine.

If Clang is not discovered automatically, set `LIBCLANG_PATH` to its library directory (Windows: commonly `C:\Program Files\LLVM\bin`; macOS: `$(brew --prefix llvm)/lib`). These are build tools; users running a release do not need them.

To make this persistent for Windows Cargo commands, add the following to `%USERPROFILE%\.cargo\config.toml`, using your installed LLVM directory:

```toml
[env]
LIBCLANG_PATH = { value = 'C:\Program Files\LLVM\bin', force = false }
```

You can also point `QUATERNIO_FFMPEG_DIR` at a folder with a matching pair, or select a tools folder in setup. Setup validates versions and capabilities. For offline installs, put the pinned archives from `packaging/ffmpeg-lock.json` (using their `file` names) in one folder, or select an already-extracted pair.

| Platform | Extra prerequisites |
|---|---|
| **Windows** | MSVC toolchain plus [VS Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/) with *Desktop development with C++*, CMake and LLVM/Clang |
| **Linux** | See below |
| **macOS** | `xcode-select --install`; `brew install cmake llvm` (FFmpeg is installed separately) |

**Linux build packages (Debian/Ubuntu)**

```bash
sudo apt update
sudo apt install build-essential pkg-config cmake libclang-dev \
  libxcb-shape0-dev libxcb-xfixes0-dev libxkbcommon-dev \
  libfontconfig-dev libx11-xcb-dev libxcursor-dev \
  libxkbcommon-x11-dev libx11-dev libwayland-dev libasound2-dev \
  libegl1-mesa-dev libgl1-mesa-dev
```

Then build and run:

```bash
cargo build --release --locked
./target/release/quaternio        # Windows: target\release\quaternio.exe
```

Use **Preferences → Media engine** to install or select FFmpeg if it is not already available.

## License

Quaternio is licensed under [MIT](LICENSE). The bundled Inter font uses the [SIL Open Font License](ui/fonts/LICENSE.txt). FFmpeg is installed separately and remains subject to its own license; Media engine setup retains the managed build's license notices.
