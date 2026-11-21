//! Redline: draw on your screen, talk, and send it to your coding agent.
//!
//! A see-through window sits over every display. The pen hotkey (Ctrl+Alt+D) turns drawing on;
//! with it off, clicks pass through to the apps underneath. While the pen is on the microphone
//! records (sox), and a note can be typed (Ctrl+Alt+N). Sending (Ctrl+Alt+S) writes a turn to
//! ~/.redline/turns/<id>/: a picture of the window the marks sit on with the ink on it, the
//! marks numbered in the order drawn, the speech transcribed locally (whisper.cpp), and a
//! timeline placing each mark among the words. Ctrl+Alt+C clears.
//!
//! A turn reaches an agent in one of two ways: Redline starts the user's own agent in their
//! project folder (see `agent.rs`), or an agent already running picks it up over MCP
//! (`mcp/server.mjs`). Settings live in ~/.redline/config.json (`config.rs`).
//! The controls live in the menu bar (macOS); the settings window opens from there.

mod agent;
mod config;
#[cfg(target_os = "macos")]
mod menubar;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use agent::tool;
use config::Config;
use rune::tok::type_;
use rune::{App, AppHandle, AppSpec, Color, Cx, El, FileDialogOptions, LineCap, Phase, Shape, TextEdit, Tx, Waker, WindowHandle, WindowOptions, WindowSpec, canvas, col, el, row, text, text_input};

const RED: Color = Color { r: 0.93, g: 0.16, b: 0.2, a: 1.0 };
const PANEL: Color = Color { r: 0.07, g: 0.07, b: 0.09, a: 0.92 };
const DIM: Color = Color { r: 1.0, g: 1.0, b: 1.0, a: 0.6 };

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

/// One pen stroke: when it was drawn (epoch ms) and its points in the overlay's points.
#[derive(Clone)]
struct Stroke {
    start: u128,
    end: u128,
    points: Vec<[f32; 2]>,
}

/// A spoken word and when it started (epoch ms).
struct Word {
    at: u128,
    text: String,
}

/// Everything the overlays, the settings window, the menu and the worker threads share.
#[derive(Default)]
struct Shared {
    drawing: AtomicBool,
    mic_on: AtomicBool,
    agent_busy: AtomicBool,
    sent: AtomicU64,
    /// Bumped on every change, so views know to redraw.
    rev: AtomicU64,
    config: Mutex<Config>,
    /// Strokes per overlay (one overlay per display), in that overlay's points.
    ink: Mutex<Vec<Vec<Stroke>>>,
    /// Notes typed for the next turn, and when (epoch ms).
    typed: Mutex<Vec<(u128, String)>>,
    /// The last thing the agent said, or its current state.
    reply: Mutex<String>,
    overlays: Mutex<Vec<WindowHandle>>,
    /// The overlays' native window ids, so the capture helper can skip them and still draw the ink.
    overlay_ids: Mutex<Vec<String>>,
    wakers: Mutex<Vec<Waker>>,
    /// The settings window's app, for opening it and for running things on the main thread.
    controls: Mutex<Option<AppHandle<State>>>,
    /// The running microphone recorder, the file it writes and when it began (epoch ms).
    recorder: Mutex<Option<(std::process::Child, PathBuf, u128)>>,
    /// Finished recordings (and when each began) waiting for the next turn.
    audio: Mutex<Vec<(PathBuf, u128)>>,
}

fn model() -> PathBuf {
    // The model stays in ~/.redline even when $REDLINE_HOME points a test elsewhere.
    let shared_home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(".redline");
    std::env::var_os("REDLINE_MODEL").map(PathBuf::from).unwrap_or_else(|| shared_home.join("models").join("ggml-base.bin"))
}

fn json_string(text: &str) -> String {
    serde_json::Value::String(text.to_string()).to_string()
}

/// The loudest sample in a recording, 0 (silence) to 1 (clipping); 0 when it can't be read.
fn loudest(wav: &Path) -> f64 {
    let Some(sox) = tool("sox") else { return 0.0 };
    let Ok(out) = Command::new(sox).arg(wav).args(["-n", "stat"]).output() else { return 0.0 };
    String::from_utf8_lossy(&out.stderr).lines()
        .find_map(|line| line.strip_prefix("Maximum amplitude:").and_then(|v| v.trim().parse().ok()))
        .unwrap_or(0.0)
}

/// `hh:mm:ss.mmm` to milliseconds.
fn clock_ms(clock: &str) -> Option<u128> {
    let mut parts = clock.trim().split(':');
    let (h, m, sec) = (parts.next()?.parse::<u128>().ok()?, parts.next()?.parse::<u128>().ok()?, parts.next()?.parse::<f64>().ok()?);
    Some((h * 3600 + m * 60) * 1000 + (sec * 1000.0).round() as u128)
}

/// Transcribe one recording into words with their start times; Err says why there is no text.
fn transcribe(wav: &Path, began: u128) -> Result<Vec<Word>, String> {
    let whisper = tool("whisper-cli").ok_or("whisper-cli is not installed")?;
    let model = model();
    if !model.is_file() { return Err(format!("speech model not found at {}", model.display())); }
    // `-ml 1 -sow` prints one word per line: `[00:00:01.330 --> 00:00:01.860]   here,`
    let out = Command::new(whisper).arg("-m").arg(&model).arg("-f").arg(wav).args(["-l", "auto", "-np", "-ml", "1", "-sow"]).output().map_err(|e| e.to_string())?;
    if !out.status.success() { return Err(format!("whisper-cli failed: {}", String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or(""))); }
    let mut words = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Some((times, text)) = line.strip_prefix('[').and_then(|rest| rest.split_once(']')) else { continue };
        let Some(start) = times.split("-->").next().and_then(clock_ms) else { continue };
        let text = text.trim();
        // whisper prints placeholders like [BLANK_AUDIO] or (music) for non-speech.
        if text.is_empty() || text.starts_with('[') || text.starts_with('(') { continue; }
        words.push(Word { at: began + start, text: text.to_string() });
    }
    Ok(words)
}

impl Shared {
    /// Something changed: redraw the overlays and refresh the settings window and menu title.
    fn touch(self: &Arc<Self>) {
        self.rev.fetch_add(1, Ordering::Relaxed);
        for wake in self.wakers.lock().unwrap().iter() { wake(); }
        if let Some(controls) = self.controls.lock().unwrap().as_ref() {
            let shared = self.clone();
            // Runs on the main thread, where the menu bar may be touched.
            controls.update(move |s, _| {
                s.seen_rev = shared.rev.load(Ordering::Relaxed);
                #[cfg(target_os = "macos")]
                menubar::set_title(if shared.drawing.load(Ordering::Relaxed) { "● Redline" }
                    else if shared.agent_busy.load(Ordering::Relaxed) { "Redline …" }
                    else { "Redline" });
            });
        }
    }
    fn start_mic(&self) {
        let mut recorder = self.recorder.lock().unwrap();
        if recorder.is_some() { return; }
        let Some(sox) = tool("sox") else { return };
        let id = now_ms();
        let wav = std::env::temp_dir().join(format!("redline-{id}.wav"));
        // 16 kHz mono 16-bit is what whisper wants.
        match Command::new(sox).args(["-q", "-d", "-r", "16000", "-c", "1", "-b", "16"]).arg(&wav).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
            Ok(child) => { *recorder = Some((child, wav, id)); self.mic_on.store(true, Ordering::Relaxed); }
            Err(e) => eprintln!("mic: {e}"),
        }
    }
    /// Stop recording and keep the file for the next turn.
    fn stop_mic(&self) {
        let Some((mut child, wav, began)) = self.recorder.lock().unwrap().take() else { return };
        // SIGINT lets sox finish the WAV header; kill() would leave it wrong.
        Command::new("kill").args(["-INT", &child.id().to_string()]).status().ok();
        child.wait().ok();
        self.mic_on.store(false, Ordering::Relaxed);
        if wav.is_file() { self.audio.lock().unwrap().push((wav, began)); }
    }
    fn drop_audio(&self) {
        for (wav, _) in self.audio.lock().unwrap().drain(..) { std::fs::remove_file(wav).ok(); }
    }
    fn set_drawing(self: &Arc<Self>, on: bool) {
        if on {
            self.start_mic();
            // A new turn is starting: the last reply has been read.
            if !self.agent_busy.load(Ordering::Relaxed) { self.reply.lock().unwrap().clear(); }
        } else {
            self.stop_mic();
        }
        self.drawing.store(on, Ordering::Relaxed);
        for window in self.overlays.lock().unwrap().iter() {
            if let Err(e) = window.set_click_through(!on) { eprintln!("click-through: {e}"); }
        }
        self.touch();
    }
    fn toggle(self: &Arc<Self>) { self.set_drawing(!self.drawing.load(Ordering::Relaxed)); }
    fn clear(self: &Arc<Self>) {
        // Clearing while the pen is still on keeps the current recording going.
        self.drop_audio();
        self.typed.lock().unwrap().clear();
        for strokes in self.ink.lock().unwrap().iter_mut() { strokes.clear(); }
        self.touch();
    }
    fn add_note(self: &Arc<Self>, note: &str) {
        let note = note.trim();
        if note.is_empty() { return; }
        self.typed.lock().unwrap().push((now_ms(), note.to_string()));
        self.touch();
    }
    /// Show the settings window, where notes are typed.
    fn open_controls(&self) {
        if let Some(controls) = self.controls.lock().unwrap().as_ref() { controls.open(); }
    }
    fn say(self: &Arc<Self>, reply: impl Into<String>) {
        *self.reply.lock().unwrap() = reply.into();
        self.touch();
    }
    /// Pen off (so the hint and border are not in the picture), take the picture with the ink
    /// still showing, write the turn, clear the ink, then hand the turn to the agent.
    fn send(self: &Arc<Self>, whole_screen: bool) {
        let shared = self.clone();
        shared.set_drawing(false);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(250));
            let audio: Vec<(PathBuf, u128)> = shared.audio.lock().unwrap().drain(..).collect();
            let turn = write_turn(&shared, &audio, whole_screen);
            for (wav, _) in audio { std::fs::remove_file(wav).ok(); }
            match turn {
                Ok(turn) => {
                    eprintln!("turn written: {}", turn.dir.display());
                    shared.sent.fetch_add(1, Ordering::Relaxed);
                    shared.touch();
                    shared.hand_to_agent(&turn);
                }
                Err(e) => { eprintln!("turn failed: {e}"); shared.say(format!("Could not send the turn: {e}")); }
            }
        });
    }
    /// Start the user's agent on the turn, if one is set up. Otherwise the turn waits for an
    /// agent to fetch it over MCP.
    fn hand_to_agent(self: &Arc<Self>, turn: &Turn) {
        let config = self.config.lock().unwrap().clone();
        if let Some(why) = agent::unavailable(&config) {
            if config.agent != "off" { self.say(format!("Turn saved, but the agent did not start: {why}.")); }
            return;
        }
        if self.agent_busy.swap(true, Ordering::Relaxed) {
            self.say("The agent is still working on the last turn; this one was saved but not started.");
            return;
        }
        // This turn is spoken for, so an MCP listener doesn't pick it up as well.
        let _ = std::fs::write(turn.dir.join("delivered"), "started by redline\n");
        self.say("Agent working…");
        let prompt = agent::prompt(&turn.timeline, &turn.typed, &turn.pictured, &turn.image);
        let reply = match agent::run(&config, &turn.dir, &turn.image, &prompt) {
            Ok(text) if text.is_empty() => "The agent finished without saying anything.".to_string(),
            Ok(text) => text,
            Err(e) => format!("Agent problem: {e}"),
        };
        eprintln!("agent: {reply}");
        self.agent_busy.store(false, Ordering::Relaxed);
        self.say(reply);
    }
}

/// The window a turn pictured: its corner in screen points, and what it is.
struct Pictured {
    x: f32,
    y: f32,
    app: String,
    title: String,
}

/// What a written turn holds, for handing to the agent.
struct Turn {
    dir: PathBuf,
    image: PathBuf,
    timeline: String,
    typed: Vec<String>,
    pictured: String,
}

/// Every stroke on every display in the order drawn: (display, index within it), numbered from 1.
fn mark_order(ink: &[Vec<Stroke>]) -> Vec<(usize, usize)> {
    let mut order: Vec<(u128, usize, usize)> = ink.iter().enumerate()
        .flat_map(|(d, strokes)| strokes.iter().enumerate().filter(|(_, s)| !s.points.is_empty()).map(move |(i, s)| (s.start, d, i)))
        .collect();
    order.sort();
    order.into_iter().map(|(_, d, i)| (d, i)).collect()
}

/// Picture only the window under the marks (display 0's overlay starts at the screen's origin,
/// so its points are screen points). None when no single window fits or the helper is missing.
fn capture_window(shared: &Shared, ink: &[Vec<Stroke>], order: &[(usize, usize)], out: &Path) -> Option<Pictured> {
    if order.iter().any(|&(d, _)| d != 0) { return None; }
    let helper = std::env::current_exe().ok()?.parent()?.join("redline-capture");
    if !helper.is_file() { return None; }
    let centres = order.iter().map(|&(d, i)| {
        let points = &ink[d][i].points;
        let (x0, x1) = points.iter().fold((f32::MAX, f32::MIN), |(lo, hi), p| (lo.min(p[0]), hi.max(p[0])));
        let (y0, y1) = points.iter().fold((f32::MAX, f32::MIN), |(lo, hi), p| (lo.min(p[1]), hi.max(p[1])));
        format!("{:.0},{:.0}", (x0 + x1) / 2.0, (y0 + y1) / 2.0)
    }).collect::<Vec<_>>().join(";");
    let mut command = Command::new(helper);
    command.arg("--out").arg(out).arg("--overlay").arg(shared.overlay_ids.lock().unwrap().join(","));
    if !centres.is_empty() { command.arg("--points").arg(centres); }
    let result = command.output().ok()?;
    if !result.status.success() {
        eprintln!("capture: whole screen ({})", String::from_utf8_lossy(&result.stderr).trim());
        return None;
    }
    // `window <x> <y> <w> <h> <app> <title>`, tab-separated.
    let line = String::from_utf8_lossy(&result.stdout);
    let mut fields = line.trim_end_matches('\n').split('\t');
    if fields.next()? != "window" { return None; }
    let (x, y) = (fields.next()?.parse().ok()?, fields.next()?.parse().ok()?);
    let (_w, _h) = (fields.next()?, fields.next()?);
    Some(Pictured { x, y, app: fields.next().unwrap_or("").to_string(), title: fields.next().unwrap_or("").to_string() })
}

fn write_turn(shared: &Arc<Shared>, audio: &[(PathBuf, u128)], whole_screen: bool) -> std::io::Result<Turn> {
    let ink = shared.ink.lock().unwrap().clone();
    let typed: Vec<(u128, String)> = shared.typed.lock().unwrap().drain(..).collect();
    let id = now_ms();
    let dir = config::home().join("turns").join(id.to_string());
    std::fs::create_dir_all(&dir)?;
    let order = mark_order(&ink);
    let mut files: Vec<PathBuf> = (0..ink.len().max(1)).map(|i| dir.join(format!("screen-{i}.png"))).collect();
    // REDLINE_TEST_IMAGE stands in for the screen, so tests never picture what is really on it.
    let pictured = if let Some(image) = std::env::var_os("REDLINE_TEST_IMAGE") {
        std::fs::copy(image, &files[0])?;
        Some(Pictured { x: 0.0, y: 0.0, app: "Browser".into(), title: "test picture".into() })
    } else if whole_screen { None } else { capture_window(shared, &ink, &order, &files[0]) };
    if pictured.is_some() {
        files.truncate(1);
    } else {
        // macOS only for now: one file per display. Linux will use the screenshot portal.
        let status = Command::new("screencapture").arg("-x").args(&files).status()?;
        if !status.success() { return Err(std::io::Error::other("screencapture failed (Screen Recording permission?)")); }
    }
    // Marks are reported relative to the picture: the window's corner when only it was pictured.
    let (off_x, off_y) = pictured.as_ref().map(|p| (p.x, p.y)).unwrap_or((0.0, 0.0));
    let (capture, pictured_as) = match &pictured {
        Some(p) => (format!("{{\"mode\":\"window\",\"app\":{},\"title\":{}}}", json_string(&p.app), json_string(&p.title)),
            format!("the {} window \"{}\"", p.app, p.title)),
        None => ("{\"mode\":\"screen\"}".to_string(), "the whole screen".to_string()),
    };
    // The picture is taken, so the ink can go while the speech is transcribed.
    for strokes in shared.ink.lock().unwrap().iter_mut() { strokes.clear(); }
    shared.touch();

    let (mut words, mut problem, mut seconds, mut peak) = (Vec::new(), None, 0.0f64, 0.0f64);
    for (wav, began) in audio {
        // 16 kHz mono 16-bit is 32000 bytes a second after the 44-byte header.
        let length = std::fs::metadata(wav).map(|m| m.len().saturating_sub(44) as f64 / 32000.0).unwrap_or(0.0);
        seconds += length;
        // Loudness tells a silent recording from speech that wasn't recognised.
        let level = loudest(wav);
        peak = peak.max(level);
        match transcribe(wav, *began) {
            Ok(heard) => { eprintln!("mic: {length:.1}s recorded, peak {level:.3}, heard {} words", heard.len()); words.extend(heard) }
            Err(e) => { eprintln!("mic: {length:.1}s recorded, {e}"); problem = Some(e) }
        }
    }

    // One clock for words, notes and marks: seconds since the turn's first of any of them.
    let zero = audio.iter().map(|(_, began)| *began)
        .chain(order.iter().map(|&(d, i)| ink[d][i].start))
        .chain(typed.iter().map(|(at, _)| *at))
        .min().unwrap_or(id);
    let secs = |at: u128| at.saturating_sub(zero) as f64 / 1000.0;
    // The timeline drops each mark in among the words at the moment its stroke began.
    let mut events: Vec<(u128, u8, String)> = words.iter().map(|w| (w.at, 1, w.text.clone())).collect();
    events.extend(order.iter().enumerate().map(|(n, &(d, i))| (ink[d][i].start, 0, format!("[mark {}]", n + 1))));
    events.extend(typed.iter().map(|(at, note)| (*at, 2, format!("(typed: \"{note}\")"))));
    events.sort();
    let timeline = events.iter().map(|(_, _, text)| text.as_str()).collect::<Vec<_>>().join(" ");
    let spoken = words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ");
    let note = if spoken.is_empty() { "null".to_string() } else { json_string(&spoken) };
    let note_error = problem.map(|e| format!(",\"note_error\":{}", json_string(&e))).unwrap_or_default();
    let word_list = words.iter().map(|w| format!("{{\"t\":{:.1},\"w\":{}}}", secs(w.at), json_string(&w.text))).collect::<Vec<_>>().join(",");
    let typed_list = typed.iter().map(|(at, note)| format!("{{\"t\":{:.1},\"text\":{}}}", secs(*at), json_string(note))).collect::<Vec<_>>().join(",");

    let mut displays = Vec::new();
    for (d, file) in files.iter().enumerate() {
        if !file.exists() { continue; }
        // A smaller JPEG is what the agent looks at; the PNG stays for detail.
        let small = dir.join(format!("screen-{d}.jpg"));
        Command::new("sips").args(["-s", "format", "jpeg", "-s", "formatOptions", "80", "-Z", "1800"]).arg(file).arg("--out").arg(&small).output()?;
        let marks: Vec<String> = order.iter().enumerate().filter(|(_, (on, _))| *on == d).map(|(n, &(_, i))| {
            let stroke = &ink[d][i];
            let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
            for [x, y] in &stroke.points { x0 = x0.min(*x - off_x); y0 = y0.min(*y - off_y); x1 = x1.max(*x - off_x); y1 = y1.max(*y - off_y); }
            format!("{{\"n\":{},\"t_start\":{:.1},\"t_end\":{:.1},\"bbox_points\":[{x0:.0},{y0:.0},{x1:.0},{y1:.0}]}}", n + 1, secs(stroke.start), secs(stroke.end))
        }).collect();
        displays.push(format!("{{\"display\":{d},\"image\":\"screen-{d}.jpg\",\"full_image\":\"screen-{d}.png\",\"marks\":[{}]}}", marks.join(",")));
    }
    // turn.json is written last: its presence means the turn is complete.
    std::fs::write(dir.join("turn.json"), format!(
        "{{\"id\":\"{id}\",\"note\":{note}{note_error},\"typed\":[{typed_list}],\"timeline\":{},\"capture\":{capture},\"recorded_seconds\":{seconds:.1},\"audio_peak\":{peak:.3},\"words\":[{word_list}],\"displays\":[{}]}}\n",
        json_string(&timeline), displays.join(",")))?;
    Ok(Turn { image: dir.join("screen-0.jpg"), dir, timeline, typed: typed.into_iter().map(|(_, note)| note).collect(), pictured: pictured_as })
}

struct State {
    overlay: bool,
    /// Which overlay this is (its slot in `Shared::ink`).
    index: usize,
    shared: Arc<Shared>,
    seen_rev: u64,
    /// The note being typed in the settings window.
    note: TextEdit,
}

impl State {
    fn new(overlay: bool, index: usize, shared: Arc<Shared>) -> Self {
        if overlay {
            let mut ink = shared.ink.lock().unwrap();
            if ink.len() <= index { ink.resize(index + 1, Vec::new()); }
            // REDLINE_DEMO=1 starts each overlay with one stroke, to check it shows without a mouse.
            if std::env::var_os("REDLINE_DEMO").is_some() {
                let points = (0..60).map(|i| { let t = i as f32 / 59.0; [300.0 + 400.0 * t, 300.0 + 60.0 * (t * 12.0).sin()] }).collect();
                ink[index].push(Stroke { start: now_ms(), end: now_ms(), points });
            }
        }
        State { overlay, index, shared, seen_rev: 0, note: TextEdit::new("") }
    }
}

fn ink(s: &State) -> Shape {
    let ink = s.shared.ink.lock().unwrap();
    let Some(strokes) = ink.get(s.index) else { return Shape::new() };
    strokes.iter().fold(Shape::new(), |shape, stroke| match stroke.points.as_slice() {
        [p] => shape.line(*p, *p, 4.0, LineCap::Round, RED),
        points => shape.stroke(points.to_vec(), 4.0, RED),
    })
}

/// A dark rounded label on the overlay.
fn pill(key: &'static str, message: String) -> El {
    el().key(key).padding(12.0).radius(10.0).fill(PANEL).max_w(520.0).child(text(message).color(Color::WHITE).wrap())
}

fn shorten(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit { return text.to_string(); }
    text.chars().take(limit).collect::<String>() + "…"
}

fn overlay_view(s: &State) -> El {
    let drawing = s.shared.drawing.load(Ordering::Relaxed);
    let mut paper = canvas(ink(s)).key("ink").w_full().h_full();
    if drawing {
        paper = paper.border(3.0, RED.alpha(0.7)).on_drag(|s: &mut State, d, _| {
            let mut ink = s.shared.ink.lock().unwrap();
            let Some(strokes) = ink.get_mut(s.index) else { return };
            match d.phase {
                Phase::Down => strokes.push(Stroke { start: now_ms(), end: now_ms(), points: vec![[d.x, d.y]] }),
                _ => if let Some(stroke) = strokes.last_mut() { stroke.points.push([d.x, d.y]); stroke.end = now_ms(); },
            }
        });
    }
    let mut root = el().w_full().h_full().child(paper);
    // A numbered tag where each stroke began, so the picture and the timeline agree.
    {
        let ink = s.shared.ink.lock().unwrap();
        for (n, &(d, i)) in mark_order(&ink).iter().enumerate() {
            if d != s.index { continue; }
            let [x, y] = ink[d][i].points[0];
            root = root.child(col().key(format!("mark-{}", n + 1)).pin(Some(x - 11.0), Some(y - 26.0), None, None).size(22.0, 22.0).radius(11.0).fill(RED).center()
                .child(text(format!("{}", n + 1)).color(Color::WHITE)));
        }
    }
    if s.index != 0 { return root; }
    // Typed notes and the agent's word sit bottom-left on the main display.
    let notes: Vec<String> = s.shared.typed.lock().unwrap().iter().map(|(_, note)| format!("Note: {}", shorten(note, 160))).collect();
    let reply = s.shared.reply.lock().unwrap().clone();
    let mut stack = col().key("messages").pin(Some(24.0), None, None, Some(24.0)).gap(8.0);
    for note in notes { stack = stack.child(pill("typed", note)); }
    if !reply.is_empty() && !drawing { stack = stack.child(pill("reply", shorten(&reply, 420))); }
    root = root.child(stack);
    if drawing {
        let keys = s.shared.config.lock().unwrap().clone();
        let mic = if s.shared.mic_on.load(Ordering::Relaxed) { "● mic recording" } else { "mic off (sox not found)" };
        root = root.child(pill("hint", format!("Pen on · {mic} · send {} · note {} · pen off {} · clear {}", keys.send_key, keys.note_key, keys.pen_key, keys.clear_key))
            .pin(None, None, Some(24.0), Some(24.0)));
    }
    root
}

fn button(label: impl Into<String>, lit: bool, on_press: fn(&mut State)) -> El {
    el().padding(10.0).radius(8.0).fill(RED.alpha(if lit { 0.55 } else { 0.18 })).child(text(label.into()).color(Color::WHITE)).on_press(on_press)
}

fn set_agent(s: &mut State, agent: &str) {
    let mut config = s.shared.config.lock().unwrap();
    config.agent = agent.to_string();
    config.save();
    drop(config);
    s.shared.touch();
}

fn speech_status(recording: bool) -> &'static str {
    if recording { "● Mic recording (pen is on)" }
    else if tool("sox").is_none() { "Dictation off: sox is not installed" }
    else if tool("whisper-cli").is_none() { "Dictation off: whisper-cli is not installed" }
    else if !model().is_file() { "Dictation off: speech model missing" }
    else { "Dictation ready: the mic records while the pen is on" }
}

/// The settings window: type a note, choose the project and the agent, see what it last said.
fn controls_view(s: &State) -> El {
    let config = s.shared.config.lock().unwrap().clone();
    let drawing = s.shared.drawing.load(Ordering::Relaxed);
    // The folder's name and its parent are enough to recognise it; the full path rarely fits.
    let project = config.project.as_ref().map(|p| {
        let tail: Vec<String> = p.components().rev().take(2).map(|c| c.as_os_str().to_string_lossy().to_string()).collect();
        format!("…/{}", tail.into_iter().rev().collect::<Vec<_>>().join("/"))
    }).unwrap_or_else(|| "No project folder chosen".into());
    let agent_line = match agent::unavailable(&config) {
        None => "Each turn starts this agent in the project folder.".to_string(),
        Some(_) if config.agent == "off" => "Agent off: turns wait for an agent to fetch them over MCP.".to_string(),
        Some(why) => format!("The agent can't start: {why}."),
    };
    let reply = s.shared.reply.lock().unwrap().clone();
    let pending = s.shared.typed.lock().unwrap().len();
    col().w_full().h_full().padding(24.0).gap(10.0).fill(Color { r: 0.07, g: 0.07, b: 0.09, a: 1.0 })
        .child(text(if drawing { "Redline: pen ON" } else { "Redline: pen off (clicks pass through)" }).font(type_::TITLE).color(Color::WHITE))
        .child(text(format!("Pen {} · send {} · note {} · clear {}", config.pen_key, config.send_key, config.note_key, config.clear_key)).color(DIM).wrap())
        .child(text(speech_status(s.shared.mic_on.load(Ordering::Relaxed))).color(DIM))
        .child(text(if pending == 0 { "Type a note for the next turn, then press Enter:".to_string() } else { format!("{pending} note(s) attached. Add another:") }).color(Color::WHITE))
        .child(el().w_full().padding(10.0).radius(8.0).fill(Color::WHITE.alpha(0.08))
            .child(text_input(&s.note, type_::BODY).key("note").single_line().autofocus().w_full().color(Color::WHITE)
                .on_edit(|s: &mut State| &mut s.note)
                .on_submit(|s: &mut State, _| {
                    let note = s.note.text.clone();
                    s.note.clear();
                    s.shared.add_note(&note);
                })))
        .child(text("Project").font(type_::TITLE).color(Color::WHITE))
        .child(text(project).color(DIM).wrap())
        .child(el().padding(10.0).radius(8.0).fill(RED.alpha(0.18)).child(text("Choose project folder…").color(Color::WHITE))
            .on_press_tx(|_: &mut State, tx: &mut Tx| {
                let options = FileDialogOptions { title: "Choose the project your agent should work in".into(), ..Default::default() };
                tx.open_folder(options, |s: &mut State, result, _| {
                    if let Ok(Some(folder)) = result {
                        let mut config = s.shared.config.lock().unwrap();
                        config.project = Some(folder);
                        config.save();
                        drop(config);
                        s.shared.touch();
                    }
                });
            }))
        .child(text("Agent").font(type_::TITLE).color(Color::WHITE))
        .child(row().gap(8.0)
            .child(button("Off", config.agent == "off", |s| set_agent(s, "off")))
            .child(button("Codex", config.agent == "codex", |s| set_agent(s, "codex")))
            .child(button("Claude Code", config.agent == "claude", |s| set_agent(s, "claude"))))
        .child(text(agent_line).color(DIM).wrap())
        .child(text(if reply.is_empty() { format!("Turns sent: {}", s.shared.sent.load(Ordering::Relaxed)) } else { shorten(&reply, 300) }).color(Color::WHITE).wrap())
        .child(row().gap(8.0)
            .child(button("Pen on/off", drawing, |s| s.shared.toggle()))
            .child(button("Send", false, |s| s.shared.send(false)))
            .child(button("Clear", false, |s| s.shared.clear())))
}

fn view(s: &State, _: &mut Cx<State>) -> El {
    if s.overlay { overlay_view(s) } else { controls_view(s) }
}

fn poll(s: &mut State, _: f64, _: &mut Tx) -> bool {
    let rev = s.shared.rev.load(Ordering::Relaxed);
    let changed = rev != s.seen_rev;
    s.seen_rev = rev;
    changed
}

fn setup(s: &mut State, waker: Waker) {
    s.shared.wakers.lock().unwrap().push(waker);
    // REDLINE_AUTOSEND=1 sends one turn two seconds after launch, to test unattended.
    // REDLINE_DEMO_NOTE attaches a typed note to it.
    if !s.overlay && std::env::var_os("REDLINE_AUTOSEND").is_some() {
        let shared = s.shared.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(2));
            if let Ok(note) = std::env::var("REDLINE_DEMO_NOTE") { shared.add_note(&note); }
            shared.send(false);
        });
    }
}

fn spec() -> AppSpec<State> {
    // Every window is given its real state with `WindowSpec::state`; this is only the fallback.
    AppSpec { setup: Some(setup), poll: Some(poll), ..AppSpec::new("Redline", (420.0, 620.0), || State::new(false, 0, Arc::new(Shared::default())), view) }
}

/// Menu-bar controls: the same actions as the hotkeys, plus the settings window and Quit.
#[cfg(target_os = "macos")]
fn install_menu(shared: &Arc<Shared>) {
    let keys = shared.config.lock().unwrap().clone();
    let entry = |label: String, action: fn(&Arc<Shared>)| {
        let shared = shared.clone();
        Some((label, Box::new(move || action(&shared)) as menubar::Action))
    };
    menubar::install("Redline", vec![
        entry(format!("Pen on/off  ({})", keys.pen_key), |s| s.toggle()),
        entry(format!("Send to agent  ({})", keys.send_key), |s| s.send(false)),
        entry("Send whole screen".into(), |s| s.send(true)),
        entry(format!("Type a note…  ({})", keys.note_key), |s| s.open_controls()),
        entry(format!("Clear  ({})", keys.clear_key), |s| s.clear()),
        None,
        entry("Settings…".into(), |s| s.open_controls()),
        None,
        entry("Quit Redline".into(), |s| {
            s.stop_mic();
            s.drop_audio();
            if let Some(controls) = s.controls.lock().unwrap().as_ref() { controls.quit(); }
        }),
    ]);
}

fn main() {
    let config = Config::load();
    // Rune's headless tooling (`--snap out.png`, `--tree`, ...) draws the settings window
    // without touching the desktop; any such flag hands the arguments to Rune.
    let tooling = std::env::args().skip(1).any(|a| a.starts_with("--"));
    let windows = move |displays: &[rune::Display]| {
        let shared = Arc::new(Shared { config: Mutex::new(config.clone()), ..Shared::default() });
        let controls = App::new(spec())
            .windowless()
            .global_shortcut(&config.pen_key, |s: &mut State, _| s.shared.toggle())
            .global_shortcut(&config.clear_key, |s: &mut State, _| s.shared.clear())
            .global_shortcut(&config.send_key, |s: &mut State, _| s.shared.send(false))
            .global_shortcut(&config.note_key, |s: &mut State, _| s.shared.open_controls());
        *shared.controls.lock().unwrap() = Some(controls.handle());
        let mut windows = vec![WindowSpec::from_app("controls", controls).state(State::new(false, 0, shared.clone()))];
        for (i, display) in displays.iter().enumerate() {
            let handles = shared.clone();
            windows.push(WindowSpec::new(format!("overlay-{i}"), spec())
                .state(State::new(true, i, shared.clone()))
                .options(WindowOptions::overlay(display))
                .on_created(move |window| {
                    if let Some(number) = window.window_number() { handles.overlay_ids.lock().unwrap().push(number.to_string()); }
                    // Overlays are created on the main thread once AppKit is up: the place to
                    // add the menu-bar item (it only installs once).
                    #[cfg(target_os = "macos")]
                    install_menu(&handles);
                    handles.overlays.lock().unwrap().push(window);
                }));
        }
        windows
    };
    if tooling {
        rune::run_windows(windows);
    } else {
        // An accessory app: no Dock icon, and the settings window stays closed until asked for.
        rune::run_windows_with_args(rune::RunOptions { accessory: true }, [], windows);
    }
}
