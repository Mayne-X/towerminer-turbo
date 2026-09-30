// No console window on Windows.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod config;
mod fmt;
mod miner;
mod protocol;

use config::{Config, Policy};
use eframe::egui::{self, Color32, RichText};
use miner::{Miner, Msg};
use protocol::{Block, Event, Profile, Status};
use std::collections::VecDeque;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const LOG_LINES: usize = 500;
const MAX_BLOCKS: usize = 1000;
/// A running miner reports every 5 s; past this, say how old the numbers are.
const STALE_SECS: u64 = 15;

const GREEN: Color32 = Color32::from_rgb(96, 200, 120);
const RED: Color32 = Color32::from_rgb(235, 96, 96);
const AMBER: Color32 = Color32::from_rgb(230, 180, 70);
const DIM: Color32 = Color32::from_gray(150);
const BRIGHT: Color32 = Color32::from_gray(235);

/// Logical CPUs of the machine. On Windows, across all processor groups:
/// `available_parallelism` only counts the current group (at most 64).
fn logical_cpus() -> usize {
    #[cfg(windows)]
    {
        // ALL_PROCESSOR_GROUPS = 0xFFFF.
        let n = unsafe { windows_sys::Win32::System::Threading::GetActiveProcessorCount(0xFFFF) } as usize;
        if n > 0 {
            return n;
        }
    }
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

fn main() {
    let max_threads = logical_cpus();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(format!("Towerminer {VERSION} - Jetsam CPU miner"))
            .with_inner_size([1000.0, 720.0])
            .with_min_inner_size([820.0, 560.0]),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    let res = eframe::run_native(
        "towerminer-gui",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, max_threads)))),
    );
    if let Err(e) = res {
        fatal(&format!(
            "The window could not be opened:\n{e}\n\nTowerminer GUI needs OpenGL 2.0 or newer. \
             Install or update the graphics driver of this PC.\n\
             The command-line towerminer.exe does not need it."
        ));
    }
}

#[cfg(windows)]
fn fatal(msg: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
    let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let (text, caption) = (wide(msg), wide("Towerminer"));
    unsafe {
        MessageBoxW(std::ptr::null_mut(), text.as_ptr(), caption.as_ptr(), MB_OK | MB_ICONERROR);
    }
}

#[cfg(not(windows))]
fn fatal(msg: &str) {
    eprintln!("{msg}");
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Phase {
    Idle,
    Running,
    Stopped,
    Exited,
}

/// What one run of the miner has reported.
struct Run {
    started: Instant,
    profile: Option<Profile>,
    status: Option<Status>,
    status_at: Option<Instant>,
    /// Oldest first.
    blocks: VecDeque<Block>,
    /// Last `{"type":"error"}` message.
    error: Option<String>,
    /// Last stderr line that looks like an error (else the last line).
    last_log: Option<String>,
    exit_code: Option<i32>,
}

impl Run {
    fn new() -> Run {
        Run {
            started: Instant::now(),
            profile: None,
            status: None,
            status_at: None,
            blocks: VecDeque::new(),
            error: None,
            last_log: None,
            exit_code: None,
        }
    }
}

struct App {
    cfg: Config,
    max_threads: usize,
    miner: Option<Miner>,
    rx: Option<Receiver<Msg>>,
    phase: Phase,
    run: Option<Run>,
    /// A problem found before the miner could start.
    notice: Option<String>,
    log: VecDeque<String>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, max_threads: usize) -> App {
        let ctx = &cc.egui_ctx;
        ctx.set_theme(egui::Theme::Dark);
        ctx.all_styles_mut(|style| {
            use egui::{FontId, TextStyle};
            style.text_styles = [
                (TextStyle::Heading, FontId::proportional(20.0)),
                (TextStyle::Body, FontId::proportional(14.5)),
                (TextStyle::Button, FontId::proportional(14.5)),
                (TextStyle::Small, FontId::proportional(11.5)),
                (TextStyle::Monospace, FontId::monospace(12.5)),
            ]
            .into();
            style.spacing.item_spacing = egui::vec2(8.0, 6.0);
            style.spacing.button_padding = egui::vec2(10.0, 5.0);
        });
        let mut app = App {
            cfg: config::load(),
            max_threads: max_threads.max(1),
            miner: None,
            rx: None,
            phase: Phase::Idle,
            run: None,
            notice: None,
            log: VecDeque::new(),
        };
        app.gui_log(format!("towerminer-gui {VERSION}; settings: {}", config::path().display()));
        app
    }

    fn threads(&self) -> usize {
        match self.cfg.threads {
            0 => self.max_threads,
            t => t.min(self.max_threads),
        }
    }

    fn push_log(&mut self, line: String) {
        if self.log.len() >= LOG_LINES {
            self.log.pop_front();
        }
        self.log.push_back(line);
    }

    fn gui_log(&mut self, line: String) {
        let t = fmt::utc(fmt::now_unix());
        self.push_log(format!("[gui {} UTC] {line}", &t[11..]));
    }

    fn save_cfg(&mut self) {
        if let Err(e) = config::save(&self.cfg) {
            self.gui_log(format!("could not save the settings to {}: {e}", config::path().display()));
        }
    }

    fn start(&mut self, ctx: &egui::Context) {
        self.notice = None;
        let rpc = self.cfg.rpc.trim().to_string();
        if !(rpc.starts_with("http://") || rpc.starts_with("https://")) {
            self.notice = Some(
                "The node RPC URL must start with http:// or https:// (for example http://127.0.0.1:9701).".into(),
            );
            return;
        }
        let exe = match miner::miner_path() {
            Some(p) if p.is_file() => p,
            p => {
                let at = p.map(|p| format!("\nExpected here: {}", p.display())).unwrap_or_default();
                self.notice = Some(format!(
                    "towerminer.exe was not found next to this program.{at}\n\
                     Extract the whole zip into one folder and start towerminer-gui.exe from there."
                ));
                return;
            }
        };
        self.cfg.rpc = rpc.clone();
        self.cfg.coinbase = self.cfg.coinbase.trim().to_string();
        self.cfg.key = self.cfg.key.trim().to_string();
        self.save_cfg();

        let threads = self.threads();
        let mut args = vec!["--rpc".to_string(), rpc];
        if !self.cfg.coinbase.is_empty() {
            args.push("--coinbase".into());
            args.push(self.cfg.coinbase.clone());
        }
        args.extend([
            "--threads".to_string(),
            threads.to_string(),
            "--policy".to_string(),
            self.cfg.policy.arg().to_string(),
            "--status-json".to_string(),
        ]);
        let key_note = if self.cfg.key.is_empty() { "no key" } else { "key set (TOWERMINER_KEY)" };
        let wake_ctx = ctx.clone();
        match Miner::start(&exe, &args, &self.cfg.key, move || wake_ctx.request_repaint()) {
            Ok((m, rx, notes)) => {
                self.push_log(String::new());
                self.gui_log(format!("started {} (pid {}) {} [{key_note}]", miner::MINER_EXE, m.pid, args.join(" ")));
                for n in notes {
                    self.gui_log(n);
                }
                self.miner = Some(m);
                self.rx = Some(rx);
                self.run = Some(Run::new());
                self.phase = Phase::Running;
            }
            Err(e) => {
                self.gui_log(e.clone());
                self.notice = Some(e);
            }
        }
    }

    fn stop(&mut self) {
        if let Some(mut m) = self.miner.take() {
            m.stop();
            self.gui_log(format!("stopped {} (pid {})", miner::MINER_EXE, m.pid));
            self.phase = Phase::Stopped;
        }
    }

    /// Apply everything the reader threads sent since the last frame.
    fn drain(&mut self) {
        let Some(rx) = &self.rx else { return };
        let msgs: Vec<Msg> = rx.try_iter().collect();
        for m in msgs {
            match m {
                Msg::Event(ev) => self.on_event(ev),
                Msg::Log(line) => {
                    if let Some(run) = &mut self.run {
                        let l = line.to_ascii_lowercase();
                        let errorish = l.contains("error") || l.contains("fatal") || l.contains("failed");
                        if errorish || run.last_log.is_none() || !is_errorish(run.last_log.as_deref()) {
                            run.last_log = Some(line.clone());
                        }
                    }
                    self.push_log(line);
                }
                Msg::Closed => {}
            }
        }
    }

    fn on_event(&mut self, ev: Event) {
        let Some(run) = &mut self.run else { return };
        match ev {
            Event::Profile(p) => run.profile = Some(p),
            Event::Status(s) => {
                run.status = Some(s);
                run.status_at = Some(Instant::now());
            }
            Event::Block(b) => {
                if run.blocks.len() >= MAX_BLOCKS {
                    run.blocks.pop_front();
                }
                run.blocks.push_back(b);
            }
            Event::Error(e) => {
                run.error = Some(e.clone());
                self.gui_log(format!("miner error: {e}"));
            }
        }
    }

    /// Notice a miner that ended on its own.
    fn poll_exit(&mut self) {
        let Some(m) = &mut self.miner else { return };
        let Some(st) = m.try_wait() else { return };
        let pid = m.pid;
        self.miner = None;
        self.phase = Phase::Exited;
        if let Some(run) = &mut self.run {
            run.exit_code = st.code();
        }
        let code = st.code().map(|c| c.to_string()).unwrap_or_else(|| "?".into());
        self.gui_log(format!("{} (pid {pid}) exited with code {code}", miner::MINER_EXE));
    }

    /// Red (error) or amber banner above the status.
    fn banner(&self) -> Option<(Color32, String)> {
        if let Some(n) = &self.notice {
            return Some((RED, n.clone()));
        }
        let run = self.run.as_ref()?;
        match self.phase {
            Phase::Running => run.error.as_ref().map(|e| (RED, format!("The miner reported an error: {e}"))),
            Phase::Exited => {
                let code = run.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "unknown".into());
                Some(match (&run.error, run.exit_code) {
                    (Some(e), _) => (RED, format!("The miner stopped: {e}\n(exit code {code})")),
                    (None, Some(0)) => (AMBER, "The miner exited (exit code 0).".into()),
                    (None, _) => {
                        let last = run
                            .last_log
                            .as_ref()
                            .map(|l| format!("\nLast message: {l}"))
                            .unwrap_or_default();
                        (RED, format!("The miner stopped unexpectedly (exit code {code}). See the log below.{last}"))
                    }
                })
            }
            Phase::Idle | Phase::Stopped => None,
        }
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let running = self.miner.is_some();
        ui.add_space(8.0);
        ui.heading("Settings");
        ui.add_space(8.0);
        let mut remember_changed = false;
        ui.add_enabled_ui(!running, |ui| {
            ui.label("Node RPC URL");
            ui.add(egui::TextEdit::singleline(&mut self.cfg.rpc).desired_width(f32::INFINITY));
            ui.add_space(6.0);
            ui.label("Mining key");
            ui.add(
                egui::TextEdit::singleline(&mut self.cfg.key)
                    .password(true)
                    .hint_text("pool or node key")
                    .desired_width(f32::INFINITY),
            );
            remember_changed = ui.checkbox(&mut self.cfg.remember_key, "Remember key").changed();
            ui.add_space(6.0);
            ui.label("Coinbase address");
            ui.add(
                egui::TextEdit::singleline(&mut self.cfg.coinbase)
                    .hint_text("optional")
                    .desired_width(f32::INFINITY),
            );
            ui.add_space(6.0);
            ui.label(format!("Threads (this PC has {} logical CPUs)", self.max_threads));
            let mut t = self.threads();
            if ui.add(egui::Slider::new(&mut t, 1..=self.max_threads)).changed() {
                self.cfg.threads = t;
            }
            ui.add_space(6.0);
            ui.label("Policy");
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.cfg.policy, Policy::Hashrate, "Hashrate")
                    .on_hover_text("The fastest profile.");
                ui.radio_value(&mut self.cfg.policy, Policy::Efficiency, "Efficiency")
                    .on_hover_text("The most hashes per watt: cooler and quieter.");
            });
        });
        if remember_changed {
            self.save_cfg();
        }
        ui.add_space(16.0);
        ui.horizontal(|ui| {
            let size = egui::vec2(128.0, 36.0);
            let start = egui::Button::new(RichText::new("Start").size(17.0).strong()).min_size(size);
            if ui.add_enabled(!running, start).clicked() {
                self.start(ctx);
            }
            let stop = egui::Button::new(RichText::new("Stop").size(17.0).strong()).min_size(size);
            if ui.add_enabled(running, stop).clicked() {
                self.stop();
            }
        });
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            ui.add_space(6.0);
            ui.label(RichText::new(format!("Settings: {}", config::path().display())).small().color(DIM));
        });
    }

    fn status_ui(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        if let Some((color, text)) = self.banner() {
            egui::Frame::group(ui.style())
                .fill(color.gamma_multiply(0.15))
                .stroke(egui::Stroke::new(1.0, color))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new(text).color(color).strong());
                });
            ui.add_space(8.0);
        }

        let run = self.run.as_ref();
        let status = run.and_then(|r| r.status.as_ref());

        // State line.
        let (state, color) = match self.phase {
            Phase::Idle => ("Idle".to_string(), DIM),
            Phase::Stopped => ("Stopped".to_string(), DIM),
            Phase::Exited => ("Exited".to_string(), RED),
            Phase::Running => match status.and_then(|s| s.state.as_deref()) {
                Some("mining") => ("Mining".to_string(), GREEN),
                Some("waiting") => ("Waiting".to_string(), AMBER),
                Some("error") => ("Error".to_string(), RED),
                Some(other) => (other.to_string(), AMBER),
                None => ("Starting".to_string(), AMBER),
            },
        };
        ui.horizontal(|ui| {
            ui.label(RichText::new("State").color(DIM));
            ui.label(RichText::new(state).size(18.0).strong().color(color));
            if self.phase == Phase::Running {
                if let Some(msg) = status.and_then(|s| s.message.as_deref()).filter(|m| !m.trim().is_empty()) {
                    ui.label(RichText::new(msg).color(if color == RED { RED } else { BRIGHT }));
                }
                match run.and_then(|r| r.status_at) {
                    Some(at) if at.elapsed().as_secs() >= STALE_SECS => {
                        ui.label(RichText::new(format!("(no update for {} s)", at.elapsed().as_secs())).color(AMBER));
                    }
                    None => {
                        let s = run.map(|r| r.started.elapsed().as_secs()).unwrap_or(0);
                        ui.label(RichText::new(format!("waiting for the first report ({s} s)")).color(DIM));
                    }
                    _ => {}
                }
            }
        });
        ui.add_space(6.0);

        // Main numbers.
        let dash = || "-".to_string();
        let hps = status.and_then(|s| s.hps).map(fmt::hashrate).unwrap_or_else(dash);
        let height = status.and_then(|s| s.height).map(|h| h.to_string()).unwrap_or_else(dash);
        let uptime = match (status.and_then(|s| s.uptime_s), run) {
            (Some(u), _) => fmt::duration(u),
            (None, Some(r)) if self.phase == Phase::Running => fmt::duration(r.started.elapsed().as_secs()),
            _ => dash(),
        };
        let count = |f: fn(&Status) -> Option<u64>| status.and_then(f);
        let n = |v: Option<u64>| v.map(|x| x.to_string()).unwrap_or_else(dash);
        let (found, acc, refu, unk) = (
            count(|s| s.found),
            count(|s| s.accepted),
            count(|s| s.refused),
            count(|s| s.unknown),
        );
        let w3 = ((ui.available_width() - 2.0 * 12.0) / 3.0 - 14.0).max(80.0);
        let w4 = ((ui.available_width() - 3.0 * 12.0) / 4.0 - 14.0).max(60.0);
        // Last known numbers stay visible after a stop, greyed out.
        let live = if self.phase == Phase::Running { BRIGHT } else { DIM };
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            tile(ui, w3, "Hashrate", hps, live, 26.0);
            tile(ui, w3, "Height", height, live, 26.0);
            tile(ui, w3, "Uptime", uptime, live, 26.0);
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            let warn = |v: Option<u64>, c: Color32| if v.unwrap_or(0) > 0 && live == BRIGHT { c } else { live };
            tile(ui, w4, "Found", n(found), live, 20.0);
            tile(ui, w4, "Accepted", n(acc), warn(acc, GREEN), 20.0);
            tile(ui, w4, "Refused", n(refu), warn(refu, RED), 20.0);
            tile(ui, w4, "Unknown", n(unk), warn(unk, AMBER), 20.0)
                .on_hover_text("Submitted, but the node gave no answer in time.");
        });
        ui.add_space(10.0);

        // Profile.
        ui.label(RichText::new("Profile").strong());
        match run.and_then(|r| r.profile.as_ref()) {
            None => {
                ui.label(RichText::new("Shown once the miner has started.").color(DIM));
            }
            Some(p) => {
                let s = |v: &Option<String>| v.clone().unwrap_or_else(dash);
                let tpc = p.tpc.map(|t| format!(" ({t} per core)")).unwrap_or_default();
                let prefetch = match p.prefetch {
                    Some(true) => ", prefetch on",
                    Some(false) => ", prefetch off",
                    None => "",
                };
                egui::Grid::new("profile").num_columns(4).spacing([14.0, 4.0]).show(ui, |ui| {
                    ui.label(RichText::new("CPU").color(DIM));
                    ui.label(s(&p.cpu));
                    ui.label(RichText::new("Backend").color(DIM));
                    ui.label(s(&p.backend));
                    ui.end_row();
                    ui.label(RichText::new("Threads").color(DIM));
                    ui.label(format!("{}{tpc}", n(p.threads)));
                    ui.label(RichText::new("Kernel").color(DIM));
                    ui.label(s(&p.kernel));
                    ui.end_row();
                    ui.label(RichText::new("Pads").color(DIM));
                    ui.label(format!("{}{prefetch}", n(p.pads)));
                    ui.label(RichText::new("Pages").color(DIM));
                    let pc = match p.large_pages() {
                        Some(true) => GREEN,
                        Some(false) => AMBER,
                        None => BRIGHT,
                    };
                    ui.label(RichText::new(s(&p.pages)).color(pc));
                    ui.end_row();
                    ui.label(RichText::new("Miner").color(DIM));
                    ui.label(format!("towerminer {}", s(&p.version)));
                    ui.end_row();
                });
                if p.large_pages() == Some(false) {
                    ui.add_space(2.0);
                    ui.label(
                        RichText::new(
                            "Tip: large pages are off. Give your Windows account the \"Lock pages in memory\" \
                             right, then sign out and back in, for about +25 % hashrate (see README-GUI.txt).",
                        )
                        .color(AMBER),
                    );
                }
            }
        }
        ui.add_space(10.0);

        // Blocks found, newest first.
        let blocks = run.map(|r| &r.blocks);
        let nb = blocks.map(|b| b.len()).unwrap_or(0);
        ui.label(RichText::new(format!("Blocks found ({nb})")).strong());
        match blocks.filter(|b| !b.is_empty()) {
            None => {
                ui.label(RichText::new("None yet.").color(DIM));
            }
            Some(blocks) => {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    egui::Grid::new("blocks").num_columns(4).striped(true).spacing([18.0, 4.0]).show(ui, |ui| {
                        for h in ["Time (UTC)", "Height", "Result", "Hash"] {
                            ui.label(RichText::new(h).color(DIM));
                        }
                        ui.end_row();
                        for b in blocks.iter().rev() {
                            ui.label(b.ts.map(fmt::utc).unwrap_or_else(dash));
                            ui.label(n(b.height));
                            let r = b.result.clone().unwrap_or_else(|| "?".into());
                            let c = match r.as_str() {
                                "accepted" => GREEN,
                                "refused" => RED,
                                _ => AMBER,
                            };
                            ui.label(RichText::new(r).color(c));
                            match &b.hash {
                                Some(h) => {
                                    ui.label(RichText::new(fmt::short_hash(h)).monospace()).on_hover_text(h);
                                }
                                None => {
                                    ui.label(RichText::new("-").color(DIM));
                                }
                            }
                            ui.end_row();
                        }
                    });
                });
            }
        }
    }

    fn log_ui(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Log").strong());
            ui.label(RichText::new(format!("(miner output, last {LOG_LINES} lines)")).small().color(DIM));
        });
        let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show_rows(ui, row_h, self.log.len(), |ui, range| {
                for line in self.log.range(range) {
                    let c = if line.starts_with("[gui") { DIM } else { BRIGHT };
                    ui.add(egui::Label::new(RichText::new(line).monospace().color(c)).truncate());
                }
            });
    }
}

fn is_errorish(l: Option<&str>) -> bool {
    l.map(|l| {
        let l = l.to_ascii_lowercase();
        l.contains("error") || l.contains("fatal") || l.contains("failed")
    })
    .unwrap_or(false)
}

fn tile(ui: &mut egui::Ui, width: f32, label: &str, value: String, color: Color32, size: f32) -> egui::Response {
    egui::Frame::group(ui.style())
        .show(ui, |ui| {
            ui.set_min_width(width);
            ui.vertical(|ui| {
                ui.label(RichText::new(label).small().color(DIM));
                ui.label(RichText::new(value).size(size).strong().color(color));
            });
        })
        .response
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain();
        self.poll_exit();
        if self.miner.is_some() {
            // Uptime and the "no update for" hint move without new events.
            ctx.request_repaint_after(Duration::from_secs(1));
        }
        egui::TopBottomPanel::bottom("log")
            .resizable(true)
            .default_height(190.0)
            .min_height(90.0)
            .show(ctx, |ui| self.log_ui(ui));
        egui::SidePanel::left("settings")
            .resizable(false)
            .exact_width(300.0)
            .show(ctx, |ui| self.settings_ui(ui, ctx));
        egui::CentralPanel::default().show(ctx, |ui| self.status_ui(ui));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.stop();
        self.save_cfg();
    }
}
