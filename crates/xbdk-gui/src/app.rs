//! The window: connection bar, tabs (Console, Files, Screen, Debug) and status bar.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use eframe::egui::{self, Color32, RichText};
use xbdm::{Console, Context, DirEntry, Screenshot};

use crate::worker::{human_size, parent_of, ConsoleInfo, Job, Reply, ThreadRow, Worker};

const MAX_EVENTS: usize = 5000;

/// Something to do to the app after the file list has been drawn.
type Action = Box<dyn FnOnce(&mut XbdkApp)>;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Console,
    Files,
    Screen,
    Debug,
}

impl Tab {
    pub fn from_name(name: &str) -> Option<Tab> {
        match name.to_ascii_lowercase().as_str() {
            "console" => Some(Tab::Console),
            "files" => Some(Tab::Files),
            "screen" => Some(Tab::Screen),
            "debug" => Some(Tab::Debug),
            _ => None,
        }
    }
}

/// A modal dialog waiting for the user.
enum Dialog {
    NewFolder { name: String },
    Rename { from: String, name: String },
    Delete { path: String, is_dir: bool },
    Launch { path: String },
    Reboot { cold: bool },
}

pub struct XbdkApp {
    worker: Worker,
    pending: usize,
    status: String,
    error: Option<String>,

    console_input: String,
    connected: Option<(SocketAddr, String)>,
    discovered: Vec<Console>,
    tab: Tab,
    dialog: Option<Dialog>,

    info: Option<Box<ConsoleInfo>>,
    launch_input: String,

    cwd: String,
    entries: Vec<DirEntry>,
    selected: Option<String>,
    download_dir: String,

    screen: Option<egui::TextureHandle>,
    last_shot: Option<Screenshot>,
    shot_pending: bool,
    auto_refresh: bool,
    refresh_ms: u64,
    last_shot_at: Instant,

    threads: Vec<ThreadRow>,
    thread_sel: Option<u32>,
    registers: Option<(u32, Context, Option<String>)>,
    bp_input: String,
    breakpoints: BTreeSet<u32>,
    mem_addr: String,
    mem_len: String,
    memory: Option<(u32, Vec<Option<u8>>)>,
    mem_write: String,
    events_on: bool,
    events: Vec<(SystemTime, String)>,
}

impl XbdkApp {
    pub fn new(cc: &eframe::CreationContext<'_>, tab: Tab) -> XbdkApp {
        let worker = Worker::spawn(cc.egui_ctx.clone());
        let console_input = std::env::var("XBDK_CONSOLE").unwrap_or_default();
        let download_dir = std::env::var("HOME")
            .map(|h| format!("{h}/Downloads"))
            .unwrap_or_else(|_| ".".into());
        let mut app = XbdkApp {
            worker,
            pending: 0,
            status: String::new(),
            error: None,
            console_input,
            connected: None,
            discovered: Vec::new(),
            tab,
            dialog: None,
            info: None,
            launch_input: String::new(),
            cwd: "HDD:\\".into(),
            entries: Vec::new(),
            selected: None,
            download_dir,
            screen: None,
            last_shot: None,
            shot_pending: false,
            auto_refresh: false,
            refresh_ms: 1000,
            last_shot_at: Instant::now(),
            threads: Vec::new(),
            thread_sel: None,
            registers: None,
            bp_input: String::new(),
            breakpoints: BTreeSet::new(),
            mem_addr: "82000000".into(),
            mem_len: "100".into(),
            memory: None,
            mem_write: String::new(),
            events_on: false,
            events: Vec::new(),
        };
        // Connect straight away: to XBDK_CONSOLE if set, otherwise to the only console found.
        app.job(Job::Connect(app.console_input.clone()));
        app.job(Job::Discover);
        app
    }

    fn job(&mut self, job: Job) {
        self.pending += 1;
        self.worker.send(job);
    }

    fn handle_replies(&mut self, ctx: &egui::Context) {
        while let Ok(reply) = self.worker.replies.try_recv() {
            match reply {
                Reply::Connected { addr, name } => {
                    self.status = format!("connected to {name} ({addr})");
                    self.error = None;
                    self.console_input = addr.ip().to_string();
                    self.connected = Some((addr, name));
                    self.job(Job::Info);
                    self.job(Job::List(self.cwd.clone()));
                    self.on_tab_opened();
                }
                Reply::Discovered(list) => self.discovered = list,
                Reply::Info(info) => self.info = Some(info),
                Reply::Listing { path, entries } => {
                    self.cwd = path;
                    self.entries = entries;
                    self.selected = None;
                }
                Reply::Changed(dir) => {
                    if dir.trim_end_matches('\\').eq_ignore_ascii_case(self.cwd.trim_end_matches('\\')) {
                        self.job(Job::List(self.cwd.clone()));
                    }
                }
                Reply::Screenshot(shot) => {
                    let image = egui::ColorImage::from_rgb([shot.width as usize, shot.height as usize], &shot.rgb);
                    match &mut self.screen {
                        Some(tex) => tex.set(image, egui::TextureOptions::LINEAR),
                        None => self.screen = Some(ctx.load_texture("screen", image, egui::TextureOptions::LINEAR)),
                    }
                    self.last_shot = Some(shot);
                    self.shot_pending = false;
                    self.last_shot_at = Instant::now();
                }
                Reply::Threads(rows) => self.threads = rows,
                Reply::Registers { thread, context, stopped } => self.registers = Some((thread, context, stopped)),
                Reply::Memory { addr, bytes } => self.memory = Some((addr, bytes)),
                Reply::Breakpoint { addr, set } => {
                    if set {
                        self.breakpoints.insert(addr);
                    } else {
                        self.breakpoints.remove(&addr);
                    }
                }
                Reply::Event(event) => {
                    self.events.push((SystemTime::now(), event.to_string()));
                    if self.events.len() > MAX_EVENTS {
                        self.events.drain(..self.events.len() - MAX_EVENTS);
                    }
                }
                Reply::EventsStopped => self.events_on = false,
                Reply::Status(s) => self.status = s,
                Reply::Error(e) => {
                    self.error = Some(e);
                    self.shot_pending = false;
                }
                Reply::JobDone => self.pending = self.pending.saturating_sub(1),
            }
        }
    }

    // ---- top and bottom bars -------------------------------------------------

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Console:");
            let edit = ui.add(egui::TextEdit::singleline(&mut self.console_input).hint_text("IP or name").desired_width(160.0));
            let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui.button("Connect").clicked() || enter {
                self.job(Job::Connect(self.console_input.clone()));
            }
            let found = self.discovered.clone();
            egui::ComboBox::from_id_salt("found")
                .selected_text(format!("{} found", found.len()))
                .show_ui(ui, |ui| {
                    for c in &found {
                        if ui.selectable_label(false, format!("{}  {}", c.name, c.ip)).clicked() {
                            self.console_input = c.ip.to_string();
                            self.job(Job::Connect(self.console_input.clone()));
                        }
                    }
                    if ui.button("Search again").clicked() {
                        self.job(Job::Discover);
                    }
                });
            ui.separator();
            for (tab, label) in [(Tab::Console, "Console"), (Tab::Files, "Files"), (Tab::Screen, "Screen"), (Tab::Debug, "Debug")] {
                if ui.selectable_label(self.tab == tab, label).clicked() && self.tab != tab {
                    self.tab = tab;
                    self.on_tab_opened();
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| match &self.connected {
                Some((addr, name)) => {
                    ui.label(RichText::new(format!("{name}  {}", addr.ip())).color(Color32::from_rgb(80, 200, 120)));
                    status_dot(ui, Color32::from_rgb(80, 200, 120));
                }
                None => {
                    ui.label(RichText::new("not connected").color(Color32::GRAY));
                    status_dot(ui, Color32::GRAY);
                }
            });
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if self.pending > 0 {
                ui.spinner();
            }
            if let Some(err) = &self.error {
                ui.label(RichText::new(err).color(Color32::from_rgb(230, 90, 80)));
                if ui.small_button("✕").clicked() {
                    self.error = None;
                }
            } else {
                ui.label(&self.status);
            }
        });
    }

    // ---- Console tab ---------------------------------------------------------

    fn console_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Console");
            if ui.button("⟳ Refresh").clicked() {
                self.job(Job::Info);
            }
        });
        let Some(info) = &self.info else {
            ui.label("Not connected yet.");
            return;
        };
        egui::Grid::new("sysinfo").num_columns(2).spacing([24.0, 4.0]).show(ui, |ui| {
            ui.label("Name");
            ui.strong(&info.name);
            ui.end_row();
            ui.label("Kit");
            ui.label(&info.kit);
            ui.end_row();
            for (k, v) in &info.system {
                ui.label(k);
                ui.label(v);
                ui.end_row();
            }
        });

        ui.add_space(12.0);
        ui.heading("Running title");
        match &info.title {
            Some(t) => {
                egui::Grid::new("title").num_columns(2).spacing([24.0, 4.0]).show(ui, |ui| {
                    if let Some(name) = &t.name {
                        ui.label("Name");
                        ui.strong(name);
                        ui.end_row();
                    }
                    if let Some(i) = &t.info {
                        ui.label("Title ID");
                        ui.monospace(format!("{:08X}", i.title_id));
                        ui.end_row();
                        ui.label("Media ID");
                        ui.monospace(format!("{:08X}", i.media_id));
                        ui.end_row();
                        ui.label("Version");
                        ui.label(format!("{} (base {})", i.version, i.base_version));
                        ui.end_row();
                    }
                    ui.label("Executable");
                    ui.monospace(&t.executable);
                    ui.end_row();
                });
            }
            None => {
                ui.label("Unknown");
            }
        }

        ui.add_space(12.0);
        ui.heading("Drives");
        egui::Grid::new("drives").num_columns(3).spacing([16.0, 6.0]).show(ui, |ui| {
            for (drive, space) in &info.drives {
                ui.label(format!("{drive}:"));
                match space {
                    Some(s) => {
                        let used = s.total.saturating_sub(s.free);
                        ui.add(egui::ProgressBar::new(used as f32 / s.total as f32).desired_width(240.0));
                        ui.label(format!("{} free of {}", human_size(s.available), human_size(s.total)));
                    }
                    None => {
                        ui.label("");
                        ui.label("");
                    }
                }
                ui.end_row();
            }
        });

        ui.add_space(12.0);
        ui.heading("Power and titles");
        ui.horizontal(|ui| {
            if ui.button("Reboot").clicked() {
                self.dialog = Some(Dialog::Reboot { cold: false });
            }
            if ui.button("Cold reboot").clicked() {
                self.dialog = Some(Dialog::Reboot { cold: true });
            }
            ui.separator();
            ui.add(egui::TextEdit::singleline(&mut self.launch_input).hint_text("HDD:\\Games\\x\\default.xex").desired_width(280.0));
            if ui.button("Launch").clicked() && !self.launch_input.trim().is_empty() {
                self.dialog = Some(Dialog::Launch { path: self.launch_input.trim().to_string() });
            }
        });
    }

    // ---- Files tab -----------------------------------------------------------

    fn files_tab(&mut self, ui: &mut egui::Ui) {
        let drives: Vec<String> = self.info.as_ref().map(|i| i.drives.iter().map(|(d, _)| d.clone()).collect()).unwrap_or_default();
        ui.horizontal_wrapped(|ui| {
            for d in drives {
                let root = format!("{d}:\\");
                if ui.selectable_label(self.cwd.eq_ignore_ascii_case(&root), &d).clicked() {
                    self.job(Job::List(root));
                }
            }
        });
        ui.horizontal(|ui| {
            if ui.button("⬆ Up").clicked() {
                self.job(Job::List(parent_of(&self.cwd)));
            }
            if ui.button("⟳").on_hover_text("Refresh").clicked() {
                self.job(Job::List(self.cwd.clone()));
            }
            if ui.button("New folder").clicked() {
                self.dialog = Some(Dialog::NewFolder { name: String::new() });
            }
            let edit = ui.add(egui::TextEdit::singleline(&mut self.cwd).desired_width(ui.available_width() - 8.0));
            if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                self.job(Job::List(self.cwd.clone()));
            }
        });
        ui.horizontal(|ui| {
            ui.label("Download to:");
            ui.add(egui::TextEdit::singleline(&mut self.download_dir).desired_width(320.0));
            ui.label(RichText::new("Drop files or folders on the window to upload here.").weak());
        });
        ui.separator();

        let mut action: Option<Action> = None;
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            egui::Grid::new("files").num_columns(3).striped(true).spacing([24.0, 2.0]).min_col_width(80.0).show(ui, |ui| {
                ui.strong("Name");
                ui.strong("Size");
                ui.strong("Modified");
                ui.end_row();
                for e in &self.entries {
                    let full = xbdm::path::join(&self.cwd, &e.name);
                    let label = if e.is_dir { format!("📁 {}", e.name) } else { format!("📄 {}", e.name) };
                    let selected = self.selected.as_deref() == Some(full.as_str());
                    let resp = ui.selectable_label(selected, label);
                    if resp.clicked() {
                        self.selected = Some(full.clone());
                    }
                    let is_xex = e.name.to_ascii_lowercase().ends_with(".xex");
                    if resp.double_clicked() {
                        let full = full.clone();
                        if e.is_dir {
                            action = Some(Box::new(move |app| app.job(Job::List(full))));
                        } else if is_xex {
                            action = Some(Box::new(move |app| app.dialog = Some(Dialog::Launch { path: full })));
                        }
                    }
                    let is_dir = e.is_dir;
                    let name = e.name.clone();
                    resp.context_menu(|ui| {
                        if ui.button("Download").clicked() {
                            let full = full.clone();
                            action = Some(Box::new(move |app| {
                                let local_dir = PathBuf::from(&app.download_dir);
                                let _ = std::fs::create_dir_all(&local_dir);
                                app.job(Job::Download { remote: full, is_dir, local_dir });
                            }));
                            ui.close();
                        }
                        if is_xex && ui.button("Launch").clicked() {
                            let full = full.clone();
                            action = Some(Box::new(move |app| app.dialog = Some(Dialog::Launch { path: full })));
                            ui.close();
                        }
                        if ui.button("Rename…").clicked() {
                            let (full, name) = (full.clone(), name.clone());
                            action = Some(Box::new(move |app| app.dialog = Some(Dialog::Rename { from: full, name })));
                            ui.close();
                        }
                        if ui.button("Delete…").clicked() {
                            let full = full.clone();
                            action = Some(Box::new(move |app| app.dialog = Some(Dialog::Delete { path: full, is_dir })));
                            ui.close();
                        }
                    });
                    ui.label(if e.is_dir { String::new() } else { human_size(e.size) });
                    ui.label(format_time(e.modified));
                    ui.end_row();
                }
            });
        });
        if let Some(action) = action {
            action(self);
        }
    }

    fn handle_drops(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        if dropped.is_empty() || self.connected.is_none() {
            return;
        }
        self.tab = Tab::Files;
        for local in dropped {
            self.job(Job::Upload { local, remote_dir: self.cwd.clone() });
        }
    }

    // ---- Screen tab ----------------------------------------------------------

    fn screen_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("📷 Capture").clicked() && !self.shot_pending {
                self.request_screenshot();
            }
            ui.checkbox(&mut self.auto_refresh, "Live");
            ui.add(egui::Slider::new(&mut self.refresh_ms, 250..=5000).suffix(" ms").text("interval"));
            if let Some(shot) = &self.last_shot {
                if ui.button("Save PNG").clicked() {
                    match save_png(shot, &self.download_dir) {
                        Ok(path) => self.status = format!("saved {}", path.display()),
                        Err(e) => self.error = Some(format!("saving screenshot: {e}")),
                    }
                }
                ui.label(format!("{}×{}", shot.width, shot.height));
            }
        });
        if self.auto_refresh && !self.shot_pending && self.last_shot_at.elapsed() >= Duration::from_millis(self.refresh_ms) {
            self.request_screenshot();
        }
        if self.auto_refresh {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
        match &self.screen {
            Some(tex) => {
                ui.centered_and_justified(|ui| {
                    ui.add(egui::Image::new(egui::load::SizedTexture::from_handle(tex)).max_size(ui.available_size()).maintain_aspect_ratio(true));
                });
            }
            None => {
                ui.label("No screenshot yet.");
            }
        }
    }

    /// Loads what a tab shows the first time it is opened.
    fn on_tab_opened(&mut self) {
        if self.connected.is_none() {
            return;
        }
        match self.tab {
            Tab::Screen if self.screen.is_none() && !self.shot_pending => self.request_screenshot(),
            Tab::Debug if self.threads.is_empty() => self.job(Job::Threads),
            _ => {}
        }
    }

    fn request_screenshot(&mut self) {
        if self.connected.is_some() {
            self.shot_pending = true;
            self.job(Job::Screenshot);
        }
    }

    // ---- Debug tab -----------------------------------------------------------

    fn debug_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("⏸ Stop").on_hover_text("Stop all title threads").clicked() {
                self.job(Job::Stop);
                self.job(Job::Threads);
            }
            if ui.button("▶ Go").on_hover_text("Resume execution").clicked() {
                self.job(Job::Go);
                self.job(Job::Threads);
            }
            let thread = self.thread_sel;
            ui.add_enabled_ui(thread.is_some(), |ui| {
                if ui.button("Step into").on_hover_text("Execute one instruction of the selected (stopped) thread").clicked() {
                    self.job(Job::Step { thread: thread.expect("enabled"), over: false });
                }
                if ui.button("Step over").on_hover_text("Like Step into, but runs calls to completion").clicked() {
                    self.job(Job::Step { thread: thread.expect("enabled"), over: true });
                }
            });
            ui.separator();
            let listening = self.events_on;
            if ui.selectable_label(listening, "📡 Events").on_hover_text("Stream debug output, breaks and exceptions").clicked() {
                if listening {
                    self.worker.stop_events();
                    self.events_on = false;
                } else if let Some((addr, _)) = self.connected {
                    self.worker.start_events(addr);
                    self.events_on = true;
                }
            }
        });
        ui.separator();

        egui::Panel::left("threads").resizable(true).default_size(420.0).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.strong("Threads");
                if ui.small_button("⟳").clicked() {
                    self.job(Job::Threads);
                }
            });
            egui::ScrollArea::vertical().id_salt("threads_scroll").max_height(220.0).show(ui, |ui| {
                egui::Grid::new("threads_grid").num_columns(5).striped(true).show(ui, |ui| {
                    for h in ["Thread", "Pri", "CPU", "Start", "State"] {
                        ui.strong(h);
                    }
                    ui.end_row();
                    let mut clicked = None;
                    for row in &self.threads {
                        let id = row.info.id;
                        if ui.selectable_label(self.thread_sel == Some(id), RichText::new(format!("{id:08X}")).monospace()).clicked() {
                            clicked = Some(id);
                        }
                        ui.label(row.info.priority.to_string());
                        ui.label(row.info.processor.to_string());
                        ui.monospace(format!("{:08X}", row.info.start));
                        ui.label(row.stopped.as_deref().map_or("running".to_string(), |r| format!("stopped: {r}")));
                        ui.end_row();
                    }
                    if let Some(id) = clicked {
                        self.thread_sel = Some(id);
                        self.job(Job::Registers(id));
                    }
                });
            });
            ui.separator();
            ui.strong("Registers");
            if let Some((thread, ctx, stopped)) = &self.registers {
                ui.label(format!("thread {thread:08X} — {}", stopped.as_deref().unwrap_or("running (volatile registers unavailable)")));
                egui::ScrollArea::vertical().id_salt("regs_scroll").show(ui, |ui| {
                    egui::Grid::new("regs").num_columns(4).show(ui, |ui| {
                        for (i, (name, value)) in ctx.registers.iter().enumerate() {
                            ui.monospace(format!("{name:>5} {value:016X}"));
                            if i % 2 == 1 {
                                ui.end_row();
                            }
                        }
                    });
                });
            } else {
                ui.label(RichText::new("Select a thread.").weak());
            }
        });

        ui.horizontal_wrapped(|ui| {
            ui.strong("Breakpoints");
            ui.add(egui::TextEdit::singleline(&mut self.bp_input).hint_text("address").desired_width(100.0));
            if ui.button("Add").clicked() {
                match parse_hex(&self.bp_input) {
                    Some(addr) => self.job(Job::Breakpoint { addr, set: true }),
                    None => self.error = Some(format!("not a hex address: {}", self.bp_input)),
                }
            }
            let mut remove = None;
            for &addr in &self.breakpoints {
                if ui.button(format!("{addr:08X} ✕")).clicked() {
                    remove = Some(addr);
                }
            }
            if let Some(addr) = remove {
                self.job(Job::Breakpoint { addr, set: false });
            }
        });
        ui.separator();

        ui.horizontal_wrapped(|ui| {
            ui.strong("Memory");
            ui.add(egui::TextEdit::singleline(&mut self.mem_addr).desired_width(90.0));
            ui.label("length");
            ui.add(egui::TextEdit::singleline(&mut self.mem_len).desired_width(50.0));
            if ui.button("Read").clicked() {
                match (parse_hex(&self.mem_addr), parse_hex(&self.mem_len)) {
                    (Some(addr), Some(len)) if len > 0 && len <= 0x10000 => self.job(Job::ReadMemory { addr, len }),
                    _ => self.error = Some("enter a hex address and a length up to 10000".into()),
                }
            }
            ui.separator();
            ui.add(egui::TextEdit::singleline(&mut self.mem_write).hint_text("hex bytes to write").desired_width(160.0));
            if ui.button("Write").clicked() {
                match (parse_hex(&self.mem_addr), parse_bytes(&self.mem_write)) {
                    (Some(addr), Some(data)) if !data.is_empty() => {
                        self.job(Job::WriteMemory { addr, data });
                        if let Some(len) = parse_hex(&self.mem_len) {
                            self.job(Job::ReadMemory { addr, len });
                        }
                    }
                    _ => self.error = Some("enter an address and an even number of hex digits".into()),
                }
            }
        });
        let available = ui.available_height();
        egui::ScrollArea::vertical().id_salt("mem_scroll").max_height(available * 0.5).show(ui, |ui| {
            if let Some((addr, bytes)) = &self.memory {
                ui.monospace(hexdump(*addr, bytes));
            }
        });
        ui.separator();

        ui.horizontal(|ui| {
            ui.strong("Events");
            if ui.small_button("Clear").clicked() {
                self.events.clear();
            }
            if !self.events_on {
                ui.label(RichText::new("(off — press 📡 Events)").weak());
            }
        });
        egui::ScrollArea::vertical().id_salt("events_scroll").stick_to_bottom(true).auto_shrink(false).show(ui, |ui| {
            for (when, text) in &self.events {
                ui.monospace(format!("{}  {text}", clock(*when)));
            }
        });
    }

    // ---- dialogs -------------------------------------------------------------

    fn dialogs(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &mut self.dialog else { return };
        let mut close = false;
        let mut job = None;
        egui::Modal::new(egui::Id::new("dialog")).show(ctx, |ui| {
            ui.set_width(380.0);
            match dialog {
                Dialog::NewFolder { name } => {
                    ui.heading("New folder");
                    ui.label(format!("in {}", self.cwd));
                    let edit = ui.text_edit_singleline(name);
                    edit.request_focus();
                    let ok = ui.input(|i| i.key_pressed(egui::Key::Enter));
                    ui.horizontal(|ui| {
                        if (ui.button("Create").clicked() || ok) && !name.trim().is_empty() {
                            job = Some(Job::Mkdir(xbdm::path::join(&self.cwd, name.trim())));
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                }
                Dialog::Rename { from, name } => {
                    ui.heading("Rename");
                    ui.monospace(from.as_str());
                    ui.text_edit_singleline(name);
                    ui.horizontal(|ui| {
                        if ui.button("Rename").clicked() && !name.trim().is_empty() {
                            let to = xbdm::path::join(&parent_of(from), name.trim());
                            job = Some(Job::Rename { from: from.clone(), to });
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                }
                Dialog::Delete { path, is_dir } => {
                    ui.heading("Delete");
                    ui.label(format!("Delete {}{}?", path, if *is_dir { " (must be empty)" } else { "" }));
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new("Delete").color(Color32::from_rgb(230, 90, 80))).clicked() {
                            job = Some(Job::Delete { path: path.clone(), is_dir: *is_dir });
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                }
                Dialog::Launch { path } => {
                    ui.heading("Launch");
                    ui.label(format!("Launch {path}? The current title will be closed."));
                    ui.horizontal(|ui| {
                        if ui.button("Launch").clicked() {
                            job = Some(Job::Launch(path.clone()));
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                }
                Dialog::Reboot { cold } => {
                    ui.heading(if *cold { "Cold reboot" } else { "Reboot" });
                    ui.label(if *cold {
                        "Fully restart the console? xbdm must be loaded again afterwards (e.g. by re-running the exploit)."
                    } else {
                        "Reboot to the dashboard? The current title will be closed."
                    });
                    ui.horizontal(|ui| {
                        if ui.button("Reboot").clicked() {
                            job = Some(Job::Reboot { cold: *cold });
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                }
            }
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                close = true;
            }
        });
        if let Some(job) = job {
            self.job(job);
        }
        if close {
            self.dialog = None;
        }
    }
}

impl eframe::App for XbdkApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.handle_replies(&ctx);
        self.handle_drops(&ctx);

        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(4.0);
            self.top_bar(ui);
            ui.add_space(2.0);
        });
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        egui::CentralPanel::default().show(ui, |ui| match self.tab {
            Tab::Console => {
                egui::ScrollArea::vertical().show(ui, |ui| self.console_tab(ui));
            }
            Tab::Files => self.files_tab(ui),
            Tab::Screen => self.screen_tab(ui),
            Tab::Debug => self.debug_tab(ui),
        });

        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            egui::Area::new(egui::Id::new("drop_hint")).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(&ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.heading(format!("Drop to upload into {}", self.cwd));
                });
            });
        }
        self.dialogs(&ctx);
    }

    fn on_exit(&mut self) {
        self.worker.stop_events();
    }
}

fn status_dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
}

fn parse_hex(s: &str) -> Option<u32> {
    let s = s.trim();
    let digits = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    u32::from_str_radix(digits, 16).ok()
}

fn parse_bytes(s: &str) -> Option<Vec<u8>> {
    let hex: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok()).collect()
}

fn hexdump(addr: u32, bytes: &[Option<u8>]) -> String {
    let mut out = String::new();
    for (row, chunk) in bytes.chunks(16).enumerate() {
        let hex: Vec<String> = chunk.iter().map(|b| b.map_or("??".into(), |b| format!("{b:02X}"))).collect();
        let ascii: String = chunk
            .iter()
            .map(|b| match b {
                Some(c) if c.is_ascii_graphic() || *c == b' ' => *c as char,
                _ => '.',
            })
            .collect();
        out.push_str(&format!("{:08X}  {:<47}  {ascii}\n", addr.wrapping_add(row as u32 * 16), hex.join(" ")));
    }
    out
}

fn save_png(shot: &Screenshot, dir: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    std::fs::create_dir_all(dir)?;
    let secs = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let path = PathBuf::from(dir).join(format!("xbox-screenshot-{secs}.png"));
    let file = std::io::BufWriter::new(std::fs::File::create(&path)?);
    let mut encoder = png::Encoder::new(file, shot.width, shot.height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(&shot.rgb)?;
    Ok(path)
}

/// `YYYY-MM-DD HH:MM` in UTC.
fn format_time(t: Option<SystemTime>) -> String {
    let Some(secs) = t.and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs()) else {
        return String::new();
    };
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil-from-days (Howard Hinnant).
    let z = days as i64 + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}", rem / 3600, rem % 3600 / 60)
}

/// `HH:MM:SS` (UTC) for the event log.
fn clock(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) % 86_400;
    format!("{:02}:{:02}:{:02}", secs / 3600, secs % 3600 / 60, secs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(parse_hex("0x82000000"), Some(0x82000000));
        assert_eq!(parse_bytes("60 00 00 00"), Some(vec![0x60, 0, 0, 0]));
        assert_eq!(parse_bytes("600"), None);
        assert!(hexdump(0x10, &[Some(b'A'), None]).starts_with("00000010  41 ??"));
    }
}
