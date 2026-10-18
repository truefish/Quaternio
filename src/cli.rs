//! Headless rendering for reproducible, project-based export measurements.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::export::{
    export_mp4_with_diagnostics, export_range, EncoderPreference, ExportProgress, ExportSettings, RenderSpeed };
use crate::project::{FrameRate, Project};
use crate::export::diagnostics::ExportDiagnostics;

const HELP: &str = "Quaternio headless render\n\
Usage: quaternio --render PROJECT --output OUTPUT.mp4 [options]\n\
  --encoder auto|hardware|amd|cpu  Auto fallback, require GPU encoder, or CPU (default auto)\n\
  --route auto|composite   Allow eligible direct export, or require compositor\n\
  --speed balanced|fast|compact  Encoding speed/compression choice (default balanced)\n\
  --report REPORT.json    Save timing, planned frame count, and progress metadata\n\
  --width N --height N    Even output dimensions (default sequence dimensions)\n\
  --fps N[/D]             Output rate (default sequence rate)\n\
  --crf N                 Quality parameter, 1..40 (default 21; backend scales differ)\n\
  --in-out                Render the sequence's marked range\n\
  --cancel-after-ms N     Request cancellation after N ms for diagnostics\n\
  --render-help           Show this help without opening the editor\n";

struct Request {
    project: PathBuf,
    output: PathBuf,
    report: Option<PathBuf>,
    encoder: EncoderPreference,
    force_composite: bool,
    speed: RenderSpeed,
    width: Option<u32>,
    height: Option<u32>,
    frame_rate: Option<FrameRate>,
    crf: u8,
    use_in_out: bool,
    cancel_after_ms: Option<u64> }

#[derive(Default)]
struct ProgressSummary {
    messages: Vec<String> }

#[derive(Serialize)]
struct RenderReport {
    schema_version: u32,
    project: PathBuf,
    output: PathBuf,
    requested_encoder: &'static str,
    requested_route: &'static str,
    requested_speed: &'static str,
    encoder: Option<String>,
    route: Option<String>,
    width: u32,
    height: u32,
    frame_rate: FrameRate,
    quality_parameter: u8,
    frames_expected: i64,
    seconds_expected: f64,
    wall_seconds: f64,
    realtime_multiplier: Option<f64>,
    status: &'static str,
    error: Option<String>,
    progress_messages: Vec<String>,
    diagnostics: ExportDiagnostics }

/// Return false for a normal editor launch; a requested render finishes without UI setup.
pub fn run_if_requested() -> Result<bool, String> {
    let arguments: Vec<OsString> = std::env::args_os().skip(1).collect();
    if arguments.iter().any(|argument| argument == "--render-help") {
        println!("{HELP}");
        return Ok(true);
    }
    if !arguments.iter().any(|argument| argument == "--render") {
        return Ok(false);
    }
    let request = parse(arguments)?;
    let project = Project::load_from(&request.project);
    if let (Ok(project), Some(report)) = (&project, &request.report) {
        if project
            .media
            .iter()
            .any(|media| same_path(&media.path, report))
        {
            return Err("Render report must differ from every source media file".into());
        }
    }
    let requested_encoder = match request.encoder {
        EncoderPreference::Cpu => "cpu",
        EncoderPreference::Hardware => "hardware",
        EncoderPreference::Amd(_) => "amd",
        EncoderPreference::Auto => "auto" };
    let mut report = RenderReport {
        schema_version: 2,
        project: request.project.clone(),
        output: request.output.clone(),
        requested_encoder,
        requested_speed: match request.speed {
            RenderSpeed::Fast => "fast",
            RenderSpeed::Compact => "compact",
            RenderSpeed::Balanced => "balanced" },
        requested_route: if request.force_composite {
            "composite"
        } else {
            "auto"
        },
        encoder: None,
        route: None,
        width: 0,
        height: 0,
        frame_rate: FrameRate::default(),
        quality_parameter: request.crf,
        frames_expected: 0,
        seconds_expected: 0.0,
        wall_seconds: 0.0,
        realtime_multiplier: None,
        status: "error",
        error: None,
        progress_messages: Vec::new(),
        diagnostics: ExportDiagnostics::default() };
    let result = match project {
        Ok(project) => render(&request, &project, &mut report),
        Err(error) => Err(error) };
    if let Err(error) = &result {
        report.error = Some(error.clone());
        if error.to_lowercase().contains("cancelled") {
            report.status = "cancelled";
        }
    }
    let document = serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?;
    if let Some(path) = &request.report {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        std::fs::write(path, &document)
            .map_err(|error| format!("Could not write render report: {error}"))?;
    }
    println!("{document}");
    result.map(|()| true)
}

fn render(request: &Request, project: &Project, report: &mut RenderReport) -> Result<(), String> {
    let settings = ExportSettings {
        speed: request.speed.clone(),
        width: request.width.unwrap_or(project.sequence.width).max(16) & !1,
        height: request.height.unwrap_or(project.sequence.height).max(16) & !1,
        frame_rate: request.frame_rate.unwrap_or(project.sequence.frame_rate),
        crf: request.crf,
        encoder: request.encoder.clone(),
        force_composite: request.force_composite,
        use_in_out: request.use_in_out,
        output: request.output.clone() };
    if settings.frame_rate.num == 0 || settings.frame_rate.den == 0 {
        return Err("Frame rate numerator and denominator must be positive".into());
    }
    if project
        .media
        .iter()
        .any(|media| same_path(&media.path, &request.output))
    {
        return Err("Render output must differ from every source media file".into());
    }
    let (start, end) = export_range(project, &settings)?;
    report.width = settings.width;
    report.height = settings.height;
    report.frame_rate = settings.frame_rate;
    report.seconds_expected = end - start;
    report.frames_expected = ((end - start) * settings.frame_rate.fps()).round().max(1.0) as i64;

    let cancel = Arc::new(AtomicBool::new(false));
    let (sender, receiver) = mpsc::channel::<ExportProgress>();
    let progress_worker = std::thread::spawn(move || {
        let mut summary = ProgressSummary::default();
        for progress in receiver {
            if (progress.fraction == 0.0 || progress.done || progress.error.is_some())
                && !summary.messages.contains(&progress.message)
            {
                if summary.messages.len() == 32 {
                    summary.messages.remove(0);
                }
                summary.messages.push(progress.message);
            }
        }
        summary
    });
    let started = Instant::now();
    if let Some(milliseconds) = request.cancel_after_ms {
        let timeout_cancel = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(milliseconds));
            timeout_cancel.store(true, Ordering::Relaxed);
        });
    }
    let result = export_mp4_with_diagnostics(
        project, &settings, &cancel, sender, &mut report.diagnostics,
    );
    report.encoder = report.diagnostics.backend.clone();
    report.route = report.diagnostics.route.map(|route| route.as_str().to_owned());
    report.wall_seconds = started.elapsed().as_secs_f64();
    let summary = progress_worker
        .join()
        .map_err(|_| "Render progress worker stopped unexpectedly")?;
    report.progress_messages = summary.messages;
    if result.is_ok() {
        report.status = "complete";
        report.realtime_multiplier =
            Some(report.seconds_expected / report.wall_seconds.max(f64::EPSILON));
    }
    result
}

fn parse(arguments: Vec<OsString>) -> Result<Request, String> {
    let mut arguments = arguments.into_iter();
    let mut seen = HashSet::new();
    let mut project = None;
    let mut output = None;
    let mut report = None;
    let mut encoder = EncoderPreference::Auto;
    let mut amd_adapter = None;
    let mut force_composite = false;
    let mut width = None;
    let mut height = None;
    let mut frame_rate = None;
    let mut crf = 21;
    let mut speed = RenderSpeed::Balanced;
    let mut use_in_out = false;
    let mut cancel_after_ms = None;
    while let Some(option) = arguments.next() {
        let option = option.to_str().ok_or("Render option is not valid text")?;
        if !seen.insert(option.to_owned()) {
            return Err(format!("Render option specified more than once: {option}"));
        }
        if option == "--in-out" {
            use_in_out = true;
            continue;
        }
        let value = arguments
            .next()
            .ok_or_else(|| format!("Missing value for {option}"))?;
        match option {
            "--render" => project = Some(PathBuf::from(value)),
            "--output" => output = Some(PathBuf::from(value)),
            "--report" => report = Some(PathBuf::from(value)),
            "--encoder" => {
                encoder = match value.to_str() {
                    Some("auto") => EncoderPreference::Auto,
                    Some("cpu") => EncoderPreference::Cpu,
                    Some("hardware") => EncoderPreference::Hardware,
                    Some("amd") => EncoderPreference::Amd(None),
                    _ => return Err("--encoder must be auto, hardware, amd, or cpu".into()) }
            }
            "--route" => {
                force_composite = match value.to_str() {
                    Some("auto") => false,
                    Some("composite") => true,
                    _ => return Err("--route must be auto or composite".into()) }
            }
            "--speed" => {
                speed = match value.to_str() {
                    Some("balanced") => RenderSpeed::Balanced,
                    Some("fast") => RenderSpeed::Fast,
                    Some("compact") => RenderSpeed::Compact,
                    _ => return Err("--speed must be balanced, fast, or compact".into()) }
            }
            "--width" => width = Some(parse_dimension(&value, option)?),
            "--height" => height = Some(parse_dimension(&value, option)?),
            "--crf" => {
                crf = value
                    .to_str()
                    .and_then(|value| value.parse::<u8>().ok())
                    .filter(|value| (1..=40).contains(value))
                    .ok_or("--crf must be 1..40")?
            }
            "--amd-adapter" => {
                amd_adapter = Some(value.to_str().and_then(|v| v.parse::<u32>().ok()).filter(|v| *v < 32).ok_or("--amd-adapter must be 0..31")?);
            },
            "--fps" => frame_rate = Some(parse_rate(&value)?),
            "--cancel-after-ms" => {
                cancel_after_ms = Some(
                    value
                        .to_str()
                        .and_then(|value| value.parse::<u64>().ok())
                        .filter(|value| (1..=600_000).contains(value))
                        .ok_or("--cancel-after-ms must be 1..600000")?,
                )
            }
            _ => return Err(format!("Unknown render option: {option}\n{HELP}")) }
    }
    if let Some(adapter) = amd_adapter {
        if !cfg!(windows) || !matches!(encoder, EncoderPreference::Amd(_)) { return Err("--amd-adapter requires --encoder amd on Windows".into()); }
        encoder = EncoderPreference::Amd(Some(adapter));
    }
    let project = project.ok_or("--render requires a project path")?;
    let output = output.ok_or("--render requires --output OUTPUT.mp4")?;
    if same_path(&project, &output)
        || report
            .as_ref()
            .is_some_and(|report| same_path(report, &output) || same_path(report, &project))
    {
        return Err("Project, output, and report must have different paths".into());
    }
    Ok(Request {
        project,
        output,
        report,
        encoder,
        force_composite,
        speed,
        width,
        height,
        frame_rate,
        crf,
        use_in_out,
        cancel_after_ms })
}

fn parse_dimension(value: &OsString, option: &str) -> Result<u32, String> {
    value
        .to_str()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| (16..=7680).contains(value) && value % 2 == 0)
        .ok_or_else(|| format!("{option} must be an even integer from 16 to 7680"))
}

fn parse_rate(value: &OsString) -> Result<FrameRate, String> {
    let text = value
        .to_str()
        .ok_or("--fps must be an integer or rational N/D")?;
    let (numerator, denominator) = text.split_once('/').unwrap_or((text, "1"));
    let num = numerator
        .parse::<u32>()
        .map_err(|_| "Invalid --fps numerator")?;
    let den = denominator
        .parse::<u32>()
        .map_err(|_| "Invalid --fps denominator")?;
    let rate = FrameRate::rational(num, den);
    if num == 0 || den == 0 || !(1.0..=240.0).contains(&rate.fps()) {
        return Err("--fps must be positive and between 1 and 240 fps".into());
    }
    Ok(rate)
}

fn same_path(left: &Path, right: &Path) -> bool {
    fn normalize(path: &Path) -> PathBuf {
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir().unwrap_or_default().join(path)
        };
        let mut result = PathBuf::new();
        for component in path.components() {
            match component {
                Component::CurDir => (),
                Component::ParentDir => {
                    result.pop();
                }
                component => result.push(component.as_os_str()) }
        }
        result
    }
    let left = normalize(left);
    let right = normalize(right);
    if cfg!(windows) {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    } else {
        left == right
    }
}

