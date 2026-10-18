#!/usr/bin/env python3
"""Opt-in, real-project export measurements; Python standard library only."""

import argparse
import datetime
import json
import math
import os
import platform
import hashlib
from pathlib import Path
import re
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import time


ROOT = Path(__file__).resolve().parents[1]
SIZES = {"1080p": (1920, 1080), "4k": (3840, 2160)}


def positive(value):
    number = float(value)
    if not math.isfinite(number) or number <= 0:
        raise argparse.ArgumentTypeError("must be finite and positive")
    return number


def arguments():
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=r'''Manifest (JSON, supplied by you; no generated media or fixtures):
  {"fixtures": [{"name": "my-edit", "project": "projects/my-edit.quaternio",
                 "references": {"1080p": "references/my-edit-1080.mp4"}}]}
Project/reference paths resolve relative to the manifest. Names must be unique.
Media must already be available to the project. Renders run from the repo root.

Examples (run explicitly; never part of build/release):
  python scripts/benchmark_exports.py projects.json --resolution 1080p 4k
  python scripts/benchmark_exports.py projects.json --mode cold --resolution 4k
  python scripts/benchmark_exports.py projects.json --compare --encoder cpu

Warm mode: one warmup followed by five measured exports for EACH fixture/size.
Cold mode: one separately labeled export per fixture/size, no warmup or median.
Cold means first invocation here, NOT verified cold OS/GPU/disk caches. For true
cold measurements, reset caches/reboot externally and select one fixture/size.
Each export uses a fresh process. Warmup/cold never enter warm medians.

Output: unique directory under ignored target/benchmarks (never release/dist).
Keeps all MP4s, CLI reports, logs, probe results and incremental summary.json.
Medians include CLI render seconds, process seconds, bytes and decoded frames.
Probing/comparison are outside render timing. --compare requires a real reference
for every selected size; checks dimensions/frame count/rate and computes FFmpeg
SSIM on the first measured (or cold) output. No scaling or frame-rate conversion;
SSIM is video-only, not an audio test or a claim of perceptual equivalence.
No competitor timings are inferred. Actual encoder/route are retained per run.
Timeout/failure stops immediately, preserving diagnostics; Ctrl+C exits 130.
Interrupted/failed samples are excluded from medians; summary marks incompleteness.
All subprocesses have timeouts; termination also targets render child processes.

Opt-in routes (set environment variables before running):
  QUATERNIO_EXPERIMENTAL_TIMELINE=1  Single-session cuts/gaps filtergraph
  QUATERNIO_EXPERIMENTAL_GPU=1       Probe additional native-frame routes;
                                   with timeline flag, promote continuous cuts
  QUATERNIO_EXPORT_WORKER=1          Isolate export in a child process
  QUATERNIO_DISABLE_GPU_RESIDENT=1   Disable native GPU picture paths
These flags do not bypass compatibility probes or enable unsupported effects.
''')
    parser.add_argument("manifest", type=Path)
    parser.add_argument("--machine-label", default="unspecified", help="hardware/driver description for comparing machines")
    parser.add_argument("--binary", default=str(ROOT / "target/release/quaternio.exe" if os.name == "nt" else ROOT / "target/release/quaternio"), help="existing CLI executable; never builds it")
    parser.add_argument("--resolution", nargs="+", choices=SIZES, default=["1080p"], help="default: 1080p")
    parser.add_argument("--fixture", action="append", help="select a manifest name; repeatable")
    parser.add_argument("--mode", choices=["warm", "cold"], default="warm")
    parser.add_argument("--encoder", choices=["auto", "hardware", "cpu"], default="auto")
    parser.add_argument("--route", choices=["auto", "composite"], default="auto")
    parser.add_argument("--speed", choices=["balanced", "fast", "compact"], default="balanced")
    parser.add_argument("--crf", type=int, choices=range(1, 41), default=21)
    parser.add_argument("--fps", help="CLI output rate N or N/D; otherwise project rate")
    parser.add_argument("--in-out", action="store_true", help="render marked sequence range")
    parser.add_argument("--timeout", type=positive, default=3600, help="seconds per render (default 3600)")
    parser.add_argument("--probe-timeout", type=positive, default=300, help="seconds per probe/comparison (default 300)")
    parser.add_argument("--ffprobe", default="ffprobe")
    parser.add_argument("--ffmpeg", default="ffmpeg", help="used only with --compare")
    parser.add_argument("--compare", action="store_true")
    return parser.parse_args()


def executable(value):
    found = shutil.which(value)
    if not found:
        raise ValueError(f"Executable not found: {value}")
    return str(Path(found).resolve())


def stop(process):
    if os.name == "nt":
        # /T includes FFmpeg children; never kill unrelated processes by name.
        result = subprocess.run(
            ["taskkill", "/PID", str(process.pid), "/T", "/F"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=15)
        if result.returncode and process.poll() is None:
            process.kill()
    else:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    process.wait(timeout=15)


def run(command, prefix, timeout):
    options = ({"creationflags": subprocess.CREATE_NEW_PROCESS_GROUP}
               if os.name == "nt" else {"start_new_session": True})
    with prefix.with_suffix(".stdout").open("wb") as out, prefix.with_suffix(".stderr").open("wb") as err:
        started = time.perf_counter()
        process = subprocess.Popen(command, cwd=ROOT, stdin=subprocess.DEVNULL,
                                   stdout=out, stderr=err, **options)
        try:
            code = process.wait(timeout=timeout)
        except (subprocess.TimeoutExpired, KeyboardInterrupt):
            try:
                stop(process)
            except (OSError, subprocess.SubprocessError) as error:
                print(f"Process cleanup failed for PID {process.pid}: {error}", file=sys.stderr)
                if process.poll() is None:
                    process.kill()
            raise
        elapsed = time.perf_counter() - started
    if code:
        raise RuntimeError(f"Exit {code}: {command[0]}; see {prefix}.stderr")
    return elapsed


def read_json(path):
    with path.open(encoding="utf-8-sig") as stream:
        return json.load(stream)


def save(path, data):
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(data, indent=2, allow_nan=False) + "\n", encoding="utf-8")
    temporary.replace(path)


def fixtures(args):
    manifest = args.manifest.resolve()
    entries = read_json(manifest)["fixtures"]
    if not isinstance(entries, list) or not entries:
        raise ValueError("Manifest fixtures must be a nonempty list")
    names = set()
    selected = []
    for entry in entries:
        name = entry["name"]
        if not isinstance(name, str) or not name.strip() or name in names:
            raise ValueError("Fixture names must be nonempty, unique strings")
        names.add(name)
        if args.fixture and name not in args.fixture:
            continue
        project = (manifest.parent / entry["project"]).resolve()
        if not project.is_file():
            raise ValueError(f"Project missing: {project}")
        references = {}
        if args.compare:
            for size in args.resolution:
                reference = (manifest.parent / entry["references"][size]).resolve()
                if not reference.is_file():
                    raise ValueError(f"Reference missing: {reference}")
                references[size] = str(reference)
        selected.append({"name": name, "project": str(project), "references": references})
    if args.fixture and set(args.fixture) - names:
        raise ValueError(f"Unknown fixtures: {sorted(set(args.fixture) - names)}")
    return selected


def probe(args, video, prefix):
    run([args.ffprobe, "-v", "error", "-select_streams", "v:0", "-count_frames",
         "-show_entries", "stream=width,height,nb_read_frames,avg_frame_rate",
         "-of", "json", str(video)], prefix, args.probe_timeout)
    data = read_json(prefix.with_suffix(".stdout"))
    stream = data["streams"][0]
    stream["nb_read_frames"] = int(stream["nb_read_frames"])
    if stream["nb_read_frames"] <= 0:
        raise ValueError(f"No decoded video frames: {video}")
    return stream


def main():
    args = arguments()
    summary = None
    destination = None
    try:
        args.binary = executable(args.binary)
        args.ffprobe = executable(args.ffprobe)
        if args.compare:
            args.ffmpeg = executable(args.ffmpeg)
        entries = fixtures(args)
        args.resolution = list(dict.fromkeys(args.resolution))
        base = ROOT / "target" / "benchmarks"
        base.mkdir(parents=True, exist_ok=True)
        destination = Path(tempfile.mkdtemp(prefix=datetime.datetime.now().strftime("%Y%m%d-%H%M%S-"), dir=base))
        summary = {"schema_version": 1, "status": "running", "settings": {
            key: str(value) if isinstance(value, Path) else value for key, value in vars(args).items()},
            "fixtures": entries, "cases": []}
        with Path(args.binary).open("rb") as binary:
            binary_hash = hashlib.file_digest(binary, "sha256").hexdigest()
        summary["environment"] = {
            "os": platform.platform(), "architecture": platform.machine(),
            "logical_cpus": os.cpu_count(), "binary_sha256": binary_hash,
            "overrides": {key: os.environ.get(key) for key in (
                "QUATERNIO_EXPERIMENTAL_TIMELINE", "QUATERNIO_EXPERIMENTAL_GPU",
                "QUATERNIO_DISABLE_GPU_RESIDENT", "QUATERNIO_EXPORT_WORKER",
                "QUATERNIO_EXPORT_FFMPEG", "QUATERNIO_VAAPI_DEVICE")},
        }
        save(destination / "summary.json", summary)
        print(f"Results: {destination}", flush=True)
        for index, fixture in enumerate(entries):
            for size in args.resolution:
                width, height = SIZES[size]
                folder = destination / f"{index:03d}-{size}"
                folder.mkdir()
                case = {"fixture": fixture["name"], "resolution": size, "runs": [], "status": "running"}
                summary["cases"].append(case)
                phases = ["cold"] if args.mode == "cold" else ["warmup"] + ["measured"] * 5
                for number, phase in enumerate(phases):
                    stem = folder / f"{number}-{phase}"
                    video, report = stem.with_suffix(".mp4"), stem.with_suffix(".json")
                    command = [args.binary, "--render", fixture["project"], "--output", str(video),
                               "--report", str(report), "--width", str(width), "--height", str(height),
                               "--encoder", args.encoder, "--route", args.route, "--speed", args.speed,
                               "--crf", str(args.crf)]
                    if args.fps:
                        command.extend(["--fps", args.fps])
                    if args.in_out:
                        command.append("--in-out")
                    sample = {"phase": phase, "status": "running", "command": command}
                    case["runs"].append(sample)
                    save(destination / "summary.json", summary)
                    print(f"{fixture['name']} {size}: {phase} {number + 1}/{len(phases)}", flush=True)
                    sample["process_seconds"] = run(command, stem, args.timeout)
                    data = read_json(report)
                    if data.get("status") != "complete":
                        raise RuntimeError(f"Render {data.get('status')}: {data.get('error')}")
                    seconds = float(data["wall_seconds"])
                    if not math.isfinite(seconds) or seconds < 0:
                        raise ValueError(f"Invalid render timing: {report}")
                    decoded = probe(args, video, folder / f"{number}-probe")
                    if (decoded["width"], decoded["height"], decoded["nb_read_frames"]) != (width, height, data["frames_expected"]):
                        raise ValueError(f"Output dimensions/frame count mismatch: {video}")
                    sample.update(status="complete", render_seconds=seconds, bytes=video.stat().st_size,
                                  frames=decoded["nb_read_frames"], probe=decoded, report=data)
                    save(destination / "summary.json", summary)
                measured = [item for item in case["runs"] if item["phase"] == "measured"]
                if measured:
                    case["medians"] = {key: statistics.median(item[key] for item in measured)
                                       for key in ("render_seconds", "process_seconds", "bytes", "frames")}
                if args.compare:
                    sample = measured[0] if measured else case["runs"][0]
                    reference = fixture["references"][size]
                    reference_probe = probe(args, reference, folder / "reference-probe")
                    if reference_probe != sample["probe"]:
                        raise ValueError(f"Reference dimensions, frame count or rate differ: {reference}")
                    prefix = folder / "comparison"
                    run([args.ffmpeg, "-nostdin", "-v", "info", "-i", sample["report"]["output"],
                         "-i", reference, "-filter_complex",
                         "[0:v:0]setpts=PTS-STARTPTS[a];[1:v:0]setpts=PTS-STARTPTS[b];[a][b]ssim=shortest=1[v]",
                         "-map", "[v]", "-an", "-f", "null", "-"], prefix, args.probe_timeout)
                    log = prefix.with_suffix(".stderr").read_text(encoding="utf-8", errors="replace")
                    scores = re.findall(r"SSIM .*All:([0-9.]+)", log)
                    if not scores:
                        raise ValueError(f"No SSIM result; see {prefix}.stderr")
                    case["comparison"] = {"reference": reference, "probe": reference_probe,
                                          "ssim": float(scores[-1]), "sample_phase": sample["phase"]}
                case["status"] = "complete"
                save(destination / "summary.json", summary)
        summary["status"] = "complete"
        save(destination / "summary.json", summary)
        print(f"Complete: {destination / 'summary.json'}")
        return 0
    except (KeyboardInterrupt, Exception) as error:
        cancelled = isinstance(error, KeyboardInterrupt)
        status = "cancelled" if cancelled else "failed"
        message = "Interrupted by user" if cancelled else f"{type(error).__name__}: {error}"
        if summary is not None:
            summary.update(status=status, error=message)
            for case in summary["cases"]:
                if case["status"] == "running":
                    case.update(status=status, error=message)
                    for sample in case["runs"]:
                        if sample["status"] == "running":
                            sample.update(status=status, error=message)
            save(destination / "summary.json", summary)
        print(f"Benchmark {status}: {message}", file=sys.stderr)
        return 130 if cancelled else 1


if __name__ == "__main__":
    sys.exit(main())
