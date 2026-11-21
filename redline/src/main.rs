//! Redline overlay test: a see-through window over every display.
//! Ctrl+Alt+D toggles pen mode; outside pen mode, clicks pass through to the apps underneath.
//! Ctrl+Alt+C clears the ink. Ctrl+Alt+S sends a turn to the agent: a screenshot with the ink
//! on it plus where each mark sits, written to ~/.redline/turns/<id>/. While the pen is on the
//! microphone records (sox), and on send the recording is transcribed locally (whisper.cpp,
//! model at ~/.redline/models/ggml-base.bin or $REDLINE_MODEL) into the turn's note. Marks are
//! numbered in the order they were drawn, and the turn's timeline places each mark among the
//! spoken words, so "change this [mark 1] and this [mark 2]" says which is which.
//! Closing the small control window quits.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rune::{App, AppSpec, Color, Cx, El, LineCap, Phase, Shape, Tx, Waker, WindowHandle, WindowOptions, WindowSpec, canvas, col, el, text};

const RED: Color = Color { r: 0.93, g: 0.16, b: 0.2, a: 1.0 };
const PANEL: Color = Color { r: 0.07, g: 0.07, b: 0.09, a: 0.92 };

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

#[derive(Default)]
struct Shared {
    drawing: AtomicBool,
    clear: AtomicU64,
    sent: AtomicU64,
    /// Strokes per overlay (one overlay per display), in that overlay's points.
    ink: Mutex<Vec<Vec<Stroke>>>,
    overlays: Mutex<Vec<WindowHandle>>,
    wakers: Mutex<Vec<Waker>>,
    /// The running microphone recorder and the file it writes.
    recorder: Mutex<Option<(std::process::Child, PathBuf, u128)>>,
    /// Finished recordings (and when each began, epoch ms) waiting for the next turn.
    audio: Mutex<Vec<(PathBuf, u128)>>,
    mic_on: AtomicBool,
}

/// GUI apps don't inherit the shell's PATH, so look in Homebrew's folders too.
fn tool(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"].map(PathBuf::from))
        .map(|dir| dir.join(name))
        .find(|file| file.is_file())
}

fn model() -> PathBuf {
    std::env::var_os("REDLINE_MODEL").map(PathBuf::from).unwrap_or_else(|| {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        home.join(".redline").join("models").join("ggml-base.bin")
    })
}

fn json_string(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            '\t' => out.push(' '),
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `hh:mm:ss.mmm` to milliseconds.
fn clock_ms(clock: &str) -> Option<u128> {
    let mut parts = clock.trim().split(':');
    let (h, m, sec) = (parts.next()?.parse::<u128>().ok()?, parts.next()?.parse::<u128>().ok()?, parts.next()?.parse::<f64>().ok()?);
    Some((h * 3600 + m * 60) * 1000 + (sec * 1000.0).round() as u128)
}

/// The loudest sample in a recording, 0 (silence) to 1 (clipping); 0 when it can't be read.
fn loudest(wav: &Path) -> f64 {
    let Some(sox) = tool("sox") else { return 0.0 };
    let Ok(out) = Command::new(sox).arg(wav).args(["-n", "stat"]).output() else { return 0.0 };
    String::from_utf8_lossy(&out.stderr).lines()
        .find_map(|line| line.strip_prefix("Maximum amplitude:").and_then(|v| v.trim().parse().ok()))
        .unwrap_or(0.0)
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
    fn wake(&self) {
        for wake in self.wakers.lock().unwrap().iter() { wake(); }
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
    fn set_drawing(&self, on: bool) {
        if on { self.start_mic() } else { self.stop_mic() }
        self.drawing.store(on, Ordering::Relaxed);
        for window in self.overlays.lock().unwrap().iter() {
            if let Err(e) = window.set_click_through(!on) { eprintln!("click-through: {e}"); }
        }
        self.wake();
    }
    fn toggle(&self) { self.set_drawing(!self.drawing.load(Ordering::Relaxed)); }
    fn clear(&self) {
        // Clearing while the pen is still on keeps the current recording going.
        self.drop_audio();
        for strokes in self.ink.lock().unwrap().iter_mut() { strokes.clear(); }
        self.clear.fetch_add(1, Ordering::Relaxed);
        self.wake();
    }
    /// Pen off (so the hint and border are not in the picture), capture every display with the
    /// ink still showing, write the turn, then clear the ink.
    fn send(self: &Arc<Self>) {
        let shared = self.clone();
        shared.set_drawing(false);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(250));
            let audio: Vec<(PathBuf, u128)> = shared.audio.lock().unwrap().drain(..).collect();
            match write_turn(&shared, &audio) {
                Ok(dir) => { eprintln!("turn written: {}", dir.display()); shared.sent.fetch_add(1, Ordering::Relaxed); }
                Err(e) => eprintln!("turn failed: {e}"),
            }
            for (wav, _) in audio { std::fs::remove_file(wav).ok(); }
            shared.wake();
        });
    }
}

/// Every stroke on every display in the order drawn: (display, index within it), numbered from 1.
fn mark_order(ink: &[Vec<Stroke>]) -> Vec<(usize, usize)> {
    let mut order: Vec<(u128, usize, usize)> = ink.iter().enumerate()
        .flat_map(|(d, strokes)| strokes.iter().enumerate().filter(|(_, s)| !s.points.is_empty()).map(move |(i, s)| (s.start, d, i)))
        .collect();
    order.sort();
    order.into_iter().map(|(_, d, i)| (d, i)).collect()
}

fn write_turn(shared: &Shared, audio: &[(PathBuf, u128)]) -> std::io::Result<PathBuf> {
    let ink = shared.ink.lock().unwrap().clone();
    let id = now_ms();
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let dir = home.join(".redline").join("turns").join(id.to_string());
    std::fs::create_dir_all(&dir)?;
    let files: Vec<PathBuf> = (0..ink.len().max(1)).map(|i| dir.join(format!("screen-{i}.png"))).collect();
    // macOS only for now: one file per display. Linux will use the screenshot portal.
    let status = Command::new("screencapture").arg("-x").args(&files).status()?;
    if !status.success() { return Err(std::io::Error::other("screencapture failed (Screen Recording permission?)")); }
    // The picture is taken, so the ink can go while the speech is transcribed.
    for strokes in shared.ink.lock().unwrap().iter_mut() { strokes.clear(); }
    shared.clear.fetch_add(1, Ordering::Relaxed);
    shared.wake();

    let (mut words, mut problem, mut seconds, mut peak) = (Vec::new(), None, 0.0f64, 0.0f64);
    for (wav, began) in audio {
        // 16 kHz mono 16-bit is 32000 bytes a second after the 44-byte header.
        let length = std::fs::metadata(wav).map(|m| m.len().saturating_sub(44) as f64 / 32000.0).unwrap_or(0.0);
        seconds += length;
        // Loudest sample (0 to 1), to tell a silent recording from speech that wasn't recognised.
        let level = loudest(wav);
        peak = peak.max(level);
        match transcribe(wav, *began) {
            Ok(heard) => { eprintln!("mic: {length:.1}s recorded, peak {level:.3}, heard {} words", heard.len()); words.extend(heard) }
            Err(e) => { eprintln!("mic: {length:.1}s recorded, {e}"); problem = Some(e) }
        }
    }
    if audio.is_empty() { eprintln!("mic: nothing was recorded for this turn"); }

    // One clock for words and marks: seconds since the turn's first word or stroke.
    let order = mark_order(&ink);
    let zero = audio.iter().map(|(_, began)| *began).chain(order.iter().map(|&(d, i)| ink[d][i].start)).min().unwrap_or(id);
    let secs = |at: u128| at.saturating_sub(zero) as f64 / 1000.0;
    // The timeline drops each mark in among the words at the moment its stroke began.
    let mut events: Vec<(u128, u8, String)> = words.iter().map(|w| (w.at, 1, w.text.clone())).collect();
    events.extend(order.iter().enumerate().map(|(n, &(d, i))| (ink[d][i].start, 0, format!("[mark {}]", n + 1))));
    events.sort();
    let timeline = events.iter().map(|(_, _, text)| text.as_str()).collect::<Vec<_>>().join(" ");
    let note = if words.is_empty() { "null".to_string() } else { json_string(&words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ")) };
    let note_error = problem.map(|e| format!(",\"note_error\":{}", json_string(&e))).unwrap_or_default();
    let word_list = words.iter().map(|w| format!("{{\"t\":{:.1},\"w\":{}}}", secs(w.at), json_string(&w.text))).collect::<Vec<_>>().join(",");

    let mut displays = Vec::new();
    for (d, file) in files.iter().enumerate() {
        if !file.exists() { continue; }
        // A smaller JPEG is what the agent looks at; the PNG stays for detail.
        let small = dir.join(format!("screen-{d}.jpg"));
        Command::new("sips").args(["-s", "format", "jpeg", "-s", "formatOptions", "80", "-Z", "1800"]).arg(file).arg("--out").arg(&small).output()?;
        let marks: Vec<String> = order.iter().enumerate().filter(|(_, (on, _))| *on == d).map(|(n, &(_, i))| {
            let stroke = &ink[d][i];
            let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
            for [x, y] in &stroke.points { x0 = x0.min(*x); y0 = y0.min(*y); x1 = x1.max(*x); y1 = y1.max(*y); }
            format!("{{\"n\":{},\"t_start\":{:.1},\"t_end\":{:.1},\"bbox_points\":[{x0:.0},{y0:.0},{x1:.0},{y1:.0}]}}", n + 1, secs(stroke.start), secs(stroke.end))
        }).collect();
        displays.push(format!("{{\"display\":{d},\"image\":\"screen-{d}.jpg\",\"full_image\":\"screen-{d}.png\",\"marks\":[{}]}}", marks.join(",")));
    }
    // turn.json is written last: its presence means the turn is complete.
    std::fs::write(dir.join("turn.json"), format!(
        "{{\"id\":\"{id}\",\"note\":{note}{note_error},\"timeline\":{},\"recorded_seconds\":{seconds:.1},\"audio_peak\":{peak:.3},\"words\":[{word_list}],\"displays\":[{}]}}\n",
        json_string(&timeline), displays.join(",")))?;
    Ok(dir)
}

struct State {
    overlay: bool,
    /// Which overlay this is (its slot in `Shared::ink`).
    index: usize,
    shared: Arc<Shared>,
    drawing: bool,
    seen_clear: u64,
    seen_sent: u64,
    mic: bool,
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
        State { overlay, index, shared, drawing: false, seen_clear: 0, seen_sent: 0, mic: false }
    }
}

impl Drop for State {
    fn drop(&mut self) {
        if !self.overlay {
            self.shared.stop_mic();
            self.shared.drop_audio();
            for window in self.shared.overlays.lock().unwrap().iter() { window.close(); }
        }
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

fn overlay_view(s: &State) -> El {
    let mut paper = canvas(ink(s)).key("ink").w_full().h_full();
    if s.drawing {
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
    if s.drawing {
        root = root.child(el().key("hint").pin(None, None, Some(24.0), Some(24.0)).padding(12.0).radius(10.0).fill(PANEL)
            .child(text(if s.mic { "Pen on · ● mic recording · Ctrl+Alt+S send to agent · Ctrl+Alt+D pen off · Ctrl+Alt+C clear" }
                else { "Pen on · mic off (sox not found) · Ctrl+Alt+S send to agent · Ctrl+Alt+D pen off · Ctrl+Alt+C clear" }).color(Color::WHITE)));
    }
    root
}

fn button(label: &'static str, on_press: fn(&mut State)) -> El {
    el().padding(12.0).radius(8.0).fill(RED.alpha(0.18)).child(text(label).color(Color::WHITE)).on_press(on_press)
}

fn view(s: &State, _: &mut Cx<State>) -> El {
    if s.overlay { return overlay_view(s); }
    col().w_full().h_full().padding(24.0).gap(12.0).fill(Color { r: 0.07, g: 0.07, b: 0.09, a: 1.0 })
        .child(text(if s.drawing { "Redline: pen ON" } else { "Redline: pen off (clicks pass through)" }).color(Color::WHITE))
        .child(text("Ctrl+Alt+D pen · Ctrl+Alt+S send · Ctrl+Alt+C clear").color(Color::WHITE.alpha(0.6)))
        .child(text(format!("Turns sent to the agent: {}", s.seen_sent)).color(Color::WHITE.alpha(0.6)))
        .child(text(speech_status(s.mic)).color(Color::WHITE.alpha(0.6)))
        .child(button("Toggle pen", |s| s.shared.toggle()))
        .child(button("Send to agent", |s| s.shared.send()))
        .child(button("Clear ink", |s| s.shared.clear()))
}

fn speech_status(recording: bool) -> &'static str {
    if recording { "● Mic recording (pen is on)" }
    else if tool("sox").is_none() { "Dictation off: sox is not installed" }
    else if tool("whisper-cli").is_none() { "Dictation off: whisper-cli is not installed" }
    else if !model().is_file() { "Dictation off: speech model missing" }
    else { "Dictation ready: the mic records while the pen is on" }
}

fn poll(s: &mut State, _: f64, _: &mut Tx) -> bool {
    let drawing = s.shared.drawing.load(Ordering::Relaxed);
    let clear = s.shared.clear.load(Ordering::Relaxed);
    let sent = s.shared.sent.load(Ordering::Relaxed);
    let mic = s.shared.mic_on.load(Ordering::Relaxed);
    let changed = drawing != s.drawing || clear != s.seen_clear || sent != s.seen_sent || mic != s.mic;
    s.mic = mic;
    s.drawing = drawing;
    s.seen_clear = clear;
    s.seen_sent = sent;
    changed
}

fn setup(s: &mut State, waker: Waker) {
    s.shared.wakers.lock().unwrap().push(waker);
    // REDLINE_AUTOSEND=1 sends one turn two seconds after launch, to test the bridge unattended.
    if !s.overlay && std::env::var_os("REDLINE_AUTOSEND").is_some() {
        let shared = s.shared.clone();
        std::thread::spawn(move || { std::thread::sleep(std::time::Duration::from_secs(2)); shared.send(); });
    }
}

fn spec() -> AppSpec<State> {
    // Every window is given its real state with `WindowSpec::state`; this is only the fallback.
    AppSpec { setup: Some(setup), poll: Some(poll), ..AppSpec::new("Redline", (380.0, 370.0), || State::new(false, 0, Arc::new(Shared::default())), view) }
}

fn main() {
    rune::run_windows(|displays| {
        let shared = Arc::new(Shared::default());
        let controls = App::new(spec())
            .global_shortcut("ctrl+alt+d", |s: &mut State, _| s.shared.toggle())
            .global_shortcut("ctrl+alt+c", |s: &mut State, _| s.shared.clear())
            .global_shortcut("ctrl+alt+s", |s: &mut State, _| s.shared.send());
        let mut windows = vec![WindowSpec::from_app("controls", controls).state(State::new(false, 0, shared.clone()))];
        for (i, display) in displays.iter().enumerate() {
            let handles = shared.clone();
            windows.push(WindowSpec::new(format!("overlay-{i}"), spec())
                .state(State::new(true, i, shared.clone()))
                .options(WindowOptions::overlay(display))
                .on_created(move |window| {
                    eprintln!("overlay {i} window number: {:?}", window.window_number());
                    handles.overlays.lock().unwrap().push(window);
                }));
        }
        windows
    });
}
