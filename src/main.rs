//! ModScope — macOS 用 Modbus Master 測試工具(操作方式參考 Radzio! Modbus Master Simulator)

mod format;
mod link;

use eframe::egui::{self, Color32, RichText, Sense};
use format::{format_values, parse_for_write, Ascii, Display, Func};
use link::{Cmd, ConnSettings, Dir, Job, LinkStatus, Mark, Parity, Proto, ScanReq, SharedRef, WriteOp};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

const BAUDS: [u32; 8] = [1200, 2400, 4800, 9600, 19200, 38400, 57600, 115200];
const HIGHLIGHT_SECS: f32 = 1.5;
const RED: Color32 = Color32::from_rgb(210, 40, 40);
const ORANGE: Color32 = Color32::from_rgb(220, 120, 0);
const GREEN: Color32 = Color32::from_rgb(20, 150, 60);
const BLUE: Color32 = Color32::from_rgb(40, 100, 210);

/// 右鍵選單的格子背景色
const CELL_COLORS: [(&str, Option<[u8; 3]>); 6] = [
    ("無", None),
    ("綠", Some([150, 230, 150])),
    ("黃", Some([255, 232, 120])),
    ("紅", Some([255, 165, 165])),
    ("藍", Some([165, 205, 255])),
    ("灰", Some([205, 205, 205])),
];

fn yes() -> bool {
    true
}
fn five() -> u32 {
    5
}

#[derive(Clone, Serialize, Deserialize)]
struct DeviceWin {
    id: u64,
    title: String,
    open: bool,
    unit: u8,
    func: Func,
    /// 畫面上的位址(依 one_based 設定可能從 1 起算)
    addr: u32,
    len: u16,
    display: Display,
    ascii: Ascii,
    aliases: BTreeMap<u16, String>,
    #[serde(default)]
    cell_colors: BTreeMap<u16, [u8; 3]>,
    #[serde(default = "yes")]
    highlight: bool,
    #[serde(default = "five")]
    log_interval: u32,
    #[serde(skip)]
    write: Option<WriteDlg>,
}

#[derive(Clone)]
struct WriteDlg {
    offset: u16,
    text: String,
    err: Option<String>,
}

impl DeviceWin {
    fn new(id: u64) -> Self {
        Self {
            id,
            title: format!("NONAME{id}"),
            open: true,
            unit: 1,
            func: Func::HoldingRegisters,
            addr: 0,
            len: 10,
            display: Display::Decimal,
            ascii: Ascii::Off,
            aliases: BTreeMap::new(),
            cell_colors: BTreeMap::new(),
            highlight: true,
            log_interval: 5,
            write: None,
        }
    }

    fn from_template(id: u64, t: &Template) -> Self {
        let mut w = Self::new(id);
        w.title = t.name.clone();
        w.unit = t.unit;
        w.func = t.func;
        w.addr = t.addr;
        w.len = t.len;
        w.display = t.display;
        w.ascii = t.ascii;
        w.aliases = t.aliases.clone();
        w.cell_colors = t.cell_colors.clone();
        w
    }
}

/// 設備範本: 一個裝置視窗的設定(功能碼、位址、格式、Alias、顏色)
#[derive(Clone, Serialize, Deserialize)]
struct Template {
    name: String,
    unit: u8,
    func: Func,
    addr: u32,
    len: u16,
    display: Display,
    ascii: Ascii,
    aliases: BTreeMap<u16, String>,
    #[serde(default)]
    cell_colors: BTreeMap<u16, [u8; 3]>,
}

impl Template {
    fn from_window(name: String, w: &DeviceWin) -> Self {
        Self {
            name,
            unit: w.unit,
            func: w.func,
            addr: w.addr,
            len: w.len,
            display: w.display,
            ascii: w.ascii,
            aliases: w.aliases.clone(),
            cell_colors: w.cell_colors.clone(),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Persist {
    conn: ConnSettings,
    windows: Vec<DeviceWin>,
    next_id: u64,
    #[serde(default)]
    dark: bool,
    #[serde(default = "yes")]
    confirm_writes: bool,
    #[serde(default)]
    templates: Vec<Template>,
}

struct ScanUi {
    open: bool,
    func: Func,
    addr: u32,
    from: u8,
    to: u8,
}

struct TrafficUi {
    open: bool,
    errors_only: bool,
    follow: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum WFn {
    Fc05,
    Fc06,
    Fc15,
    Fc16,
}

impl WFn {
    fn label(self) -> &'static str {
        match self {
            WFn::Fc05 => "FC05 Write single coil",
            WFn::Fc06 => "FC06 Write single register",
            WFn::Fc15 => "FC15 Write multiple coils",
            WFn::Fc16 => "FC16 Write multiple registers",
        }
    }
}

struct WritePanel {
    open: bool,
    unit: u8,
    wfn: WFn,
    addr: u32,
    display: Display,
    text: String,
}

struct CsvLog {
    path: PathBuf,
    file: std::fs::File,
    last: Option<Instant>,
    rows: u64,
}

/// 每個裝置視窗: 上一輪的值，以及每格最後變動的時間
#[derive(Default)]
struct Changes {
    prev: Vec<u16>,
    at: Vec<Option<Instant>>,
}

struct Rate {
    last: Instant,
    tx: u64,
    rx: u64,
    tx_kbs: f64,
    rx_kbs: f64,
}

/// 裝置視窗內觸發、需要 App 處理的動作
enum DevAction {
    Write(u8, WriteOp),
    SaveTemplate(u64),
    StartLog(u64),
    StopLog(u64),
    OpenFolder(PathBuf),
}

struct App {
    shared: SharedRef,
    tx: Sender<Cmd>,
    conn: ConnSettings,
    conn_draft: Option<ConnSettings>,
    ports: Vec<String>,
    windows: Vec<DeviceWin>,
    next_id: u64,
    scan: ScanUi,
    traffic: TrafficUi,
    write_panel: WritePanel,
    confirm_writes: bool,
    confirm_dont_ask: bool,
    pending_write: Option<(u8, WriteOp)>,
    templates: Vec<Template>,
    tmpl_dlg: Option<(u64, String)>,
    logs: HashMap<u64, CsvLog>,
    changes: HashMap<u64, Changes>,
    rate: Rate,
    toast: Option<(String, Instant)>,
    connected_wanted: bool,
    dark: bool,
    work_area: egui::Rect,
}

/// 高對比淺色主題(預設)與深色主題，並放大字體方便現場閱讀
fn apply_theme(ctx: &egui::Context, dark: bool) {
    use egui::{Stroke, TextStyle, Theme};
    let gray = Color32::from_gray;
    let blue = Color32::from_rgb(30, 100, 200);
    let mut l = egui::Visuals::light();
    l.panel_fill = gray(232);
    l.window_fill = gray(250);
    l.extreme_bg_color = Color32::WHITE;
    l.faint_bg_color = Color32::from_rgb(226, 234, 246);
    l.window_stroke = Stroke::new(1.0_f32, gray(110));
    l.selection.bg_fill = Color32::from_rgb(170, 200, 245);
    l.selection.stroke = Stroke::new(1.0_f32, gray(10));
    l.hyperlink_color = blue;
    let w = &mut l.widgets;
    w.noninteractive.fg_stroke = Stroke::new(1.0_f32, gray(15));
    w.noninteractive.bg_stroke = Stroke::new(1.0_f32, gray(150));
    for st in [&mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
        st.fg_stroke = Stroke::new(1.0_f32, gray(10));
    }
    w.inactive.bg_fill = gray(218);
    w.inactive.weak_bg_fill = gray(218);
    w.inactive.bg_stroke = Stroke::new(1.0_f32, gray(140));
    w.hovered.bg_fill = gray(205);
    w.hovered.weak_bg_fill = gray(205);
    w.hovered.bg_stroke = Stroke::new(1.0_f32, blue);
    w.active.bg_fill = gray(190);
    w.active.weak_bg_fill = gray(190);
    w.active.bg_stroke = Stroke::new(1.5_f32, blue);
    ctx.set_visuals_of(Theme::Light, l);

    let mut d = egui::Visuals::dark();
    d.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, gray(225));
    d.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, gray(235));
    d.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, gray(95));
    ctx.set_visuals_of(Theme::Dark, d);

    for theme in [Theme::Light, Theme::Dark] {
        ctx.style_mut_of(theme, |st| {
            for (ts, f) in st.text_styles.iter_mut() {
                f.size = match ts {
                    TextStyle::Small => 12.5,
                    TextStyle::Heading => 19.0,
                    _ => 15.0,
                };
            }
            st.spacing.item_spacing = egui::vec2(8.0, 5.0);
            st.spacing.button_padding = egui::vec2(8.0, 3.0);
            st.spacing.interact_size.y = 22.0;
        });
    }
    ctx.set_theme(if dark { Theme::Dark } else { Theme::Light });
}

fn load_cjk_font(ctx: &egui::Context) {
    // 讓 Alias 欄可以輸入中文(電壓、電流…)
    let candidates = [
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
        "/System/Library/Fonts/STHeiti Medium.ttc",
        "/Library/Fonts/Arial Unicode.ttf",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "C:\\Windows\\Fonts\\msjh.ttc",
    ];
    if let Some(bytes) = candidates.iter().find_map(|p| std::fs::read(p).ok()) {
        let mut fonts = egui::FontDefinitions::default();
        fonts
            .font_data
            .insert("cjk".into(), std::sync::Arc::new(egui::FontData::from_owned(bytes)));
        for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts.families.entry(fam).or_default().push("cjk".into());
        }
        ctx.set_fonts(fonts);
    }
}

/// 記錄檔、匯出檔的存放位置: ~/Documents/ModScope
fn data_dir() -> PathBuf {
    let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."));
    let docs = home.join("Documents");
    let base = if docs.is_dir() { docs } else { home };
    base.join("ModScope")
}

fn open_folder(p: &std::path::Path) {
    let cmd = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let _ = std::process::Command::new(cmd).arg(p).spawn();
}

fn hex(bytes: &[u8], max: usize) -> String {
    let mut s: String = bytes.iter().take(max).map(|b| format!("{b:02X} ")).collect();
    if bytes.len() > max {
        s.push_str(&format!("… (共 {} bytes)", bytes.len()));
    }
    s.trim_end().to_string()
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn parse_bit(t: &str) -> Result<bool, String> {
    match t.trim().to_ascii_lowercase().as_str() {
        "1" | "on" | "true" => Ok(true),
        "0" | "off" | "false" => Ok(false),
        x => Err(format!("「{x}」不是 0/1 (或 on/off)")),
    }
}

fn split_values(t: &str) -> Vec<&str> {
    // 有逗號或分號就用它分隔(保留 binary 裡的空白)，否則用空白分隔
    let v: Vec<&str> = if t.contains([',', ';']) {
        t.split([',', ';']).collect()
    } else {
        t.split_whitespace().collect()
    };
    v.into_iter().map(str::trim).filter(|x| !x.is_empty()).collect()
}

impl App {
    fn new(cc: &eframe::CreationContext) -> Self {
        load_cjk_font(&cc.egui_ctx);
        let shared: SharedRef = Default::default();
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = cc.egui_ctx.clone();
        link::spawn(shared.clone(), rx, move || ctx.request_repaint());

        let p: Option<Persist> = cc.storage.and_then(|s| eframe::get_value(s, eframe::APP_KEY));
        let dark = p.as_ref().map(|p| p.dark).unwrap_or(false);
        let confirm_writes = p.as_ref().map(|p| p.confirm_writes).unwrap_or(true);
        let templates = p.as_ref().map(|p| p.templates.clone()).unwrap_or_default();
        apply_theme(&cc.egui_ctx, dark);
        let (conn, windows, next_id) = match p {
            Some(p) if !p.windows.is_empty() => (p.conn, p.windows, p.next_id),
            Some(p) => (p.conn, vec![DeviceWin::new(1)], 2),
            None => (ConnSettings::default(), vec![DeviceWin::new(1)], 2),
        };
        let mut app = Self {
            shared,
            tx,
            conn,
            conn_draft: None,
            ports: link::list_serial_ports(),
            windows,
            next_id,
            scan: ScanUi { open: false, func: Func::HoldingRegisters, addr: 0, from: 1, to: 247 },
            traffic: TrafficUi { open: false, errors_only: false, follow: true },
            write_panel: WritePanel {
                open: false,
                unit: 1,
                wfn: WFn::Fc06,
                addr: 0,
                display: Display::Decimal,
                text: String::new(),
            },
            confirm_writes,
            confirm_dont_ask: false,
            pending_write: None,
            templates,
            tmpl_dlg: None,
            logs: HashMap::new(),
            changes: HashMap::new(),
            rate: Rate { last: Instant::now(), tx: 0, rx: 0, tx_kbs: 0.0, rx_kbs: 0.0 },
            toast: None,
            connected_wanted: false,
            dark,
            work_area: egui::Rect::EVERYTHING,
        };
        // 測試用: MODSCOPE_TCP=127.0.0.1:5020 啟動時自動以 TCP 連線
        if let Ok(t) = std::env::var("MODSCOPE_TCP") {
            if let Some((h, p)) = t.rsplit_once(':') {
                app.conn.proto = Proto::Tcp;
                app.conn.host = h.into();
                app.conn.tcp_port = p.parse().unwrap_or(502);
                app.connect();
            }
        }
        // 測試用: MODSCOPE_SHOW=traffic,write 啟動時打開指定視窗
        if let Ok(v) = std::env::var("MODSCOPE_SHOW") {
            app.traffic.open = v.contains("traffic");
            app.write_panel.open = v.contains("write");
            if v.contains("confirm") {
                app.pending_write = Some((1, WriteOp::Registers(10, vec![0x41A0, 0x8F5C])));
            }
        }
        app
    }

    fn shown_offset(&self) -> u32 {
        self.conn.one_based as u32
    }

    fn to_proto_addr(&self, shown: u32) -> u16 {
        if self.conn.one_based {
            shown.saturating_sub(1).min(65535) as u16
        } else {
            shown.min(65535) as u16
        }
    }

    fn notify(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    fn sync_jobs(&self) {
        let jobs: Vec<Job> = self
            .windows
            .iter()
            .filter(|w| w.open && w.len > 0)
            .map(|w| Job {
                id: w.id,
                unit: w.unit,
                func: w.func,
                addr: self.to_proto_addr(w.addr),
                len: w.len,
            })
            .collect();
        let mut sh = self.shared.lock().unwrap();
        if sh.jobs != jobs {
            sh.jobs = jobs;
        }
    }

    fn connect(&mut self) {
        self.connected_wanted = true;
        let _ = self.tx.send(Cmd::Connect(self.conn.clone()));
    }

    fn disconnect(&mut self) {
        self.connected_wanted = false;
        let _ = self.tx.send(Cmd::Disconnect);
    }

    fn new_window(&mut self) {
        self.windows.push(DeviceWin::new(self.next_id));
        self.next_id += 1;
    }

    fn open_settings(&mut self) {
        self.ports = link::list_serial_ports();
        self.conn_draft = Some(self.conn.clone());
    }

    /// 所有寫入都從這裡走: 需要確認就先放進 pending
    fn request_write(&mut self, unit: u8, op: WriteOp) {
        if !self.connected_wanted {
            self.notify("尚未連線，無法寫入");
            return;
        }
        if self.confirm_writes {
            self.confirm_dont_ask = false;
            self.pending_write = Some((unit, op));
        } else {
            let _ = self.tx.send(Cmd::Write { unit, op });
        }
    }

    fn templates_file() -> PathBuf {
        data_dir().join("templates.json")
    }

    fn export_templates(&mut self) {
        let path = Self::templates_file();
        let res = std::fs::create_dir_all(data_dir())
            .and_then(|_| std::fs::write(&path, serde_json::to_string_pretty(&self.templates).unwrap_or_default()));
        match res {
            Ok(()) => self.notify(format!("已匯出 {} 個範本到 {}", self.templates.len(), path.display())),
            Err(e) => self.notify(format!("匯出失敗: {e}")),
        }
    }

    fn import_templates(&mut self) {
        let path = Self::templates_file();
        let parsed = std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|s| serde_json::from_str::<Vec<Template>>(&s).map_err(|e| e.to_string()));
        match parsed {
            Ok(list) => {
                let n = list.len();
                for t in list {
                    self.templates.retain(|x| x.name != t.name);
                    self.templates.push(t);
                }
                self.templates.sort_by(|a, b| a.name.cmp(&b.name));
                self.notify(format!("已從 {} 匯入 {n} 個範本", path.display()));
            }
            Err(e) => self.notify(format!("匯入失敗 ({}): {e}", path.display())),
        }
    }

    fn start_log(&mut self, id: u64) {
        let Some(w) = self.windows.iter().find(|w| w.id == id) else { return };
        let dir = data_dir().join("logs");
        let safe: String = w.title.chars().map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' }).collect();
        let path = dir.join(format!("{}_{}.csv", safe, chrono::Local::now().format("%Y%m%d_%H%M%S")));
        let mut header = vec!["Time".to_string()];
        for i in 0..w.len as u32 {
            let a = w.addr + i;
            header.push(match w.aliases.get(&(i as u16)) {
                Some(al) => csv_field(&format!("{a} {al}")),
                None => a.to_string(),
            });
        }
        let res = std::fs::create_dir_all(&dir).and_then(|_| {
            let mut f = std::fs::File::create(&path)?;
            // UTF-8 BOM，讓 Excel 正確顯示中文 Alias
            f.write_all(b"\xEF\xBB\xBF")?;
            writeln!(f, "{}", header.join(","))?;
            Ok(f)
        });
        match res {
            Ok(file) => {
                self.notify(format!("開始記錄: {}", path.display()));
                self.logs.insert(id, CsvLog { path, file, last: None, rows: 0 });
            }
            Err(e) => self.notify(format!("無法建立記錄檔: {e}")),
        }
    }

    fn write_logs(&mut self) {
        let sh = self.shared.lock().unwrap();
        for w in &self.windows {
            let Some(log) = self.logs.get_mut(&w.id) else { continue };
            let Some(r) = sh.results.get(&w.id) else { continue };
            if r.values.len() != w.len as usize || r.last_err.is_some() {
                continue;
            }
            if log.last.is_some_and(|t| t.elapsed() < Duration::from_secs(w.log_interval.max(1) as u64)) {
                continue;
            }
            let vals = format_values(&r.values, w.func, w.display, Ascii::Off);
            let mut row = vec![chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()];
            row.extend(vals.iter().map(|v| csv_field(v)));
            if writeln!(log.file, "{}", row.join(",")).is_ok() {
                let _ = log.file.flush();
                log.rows += 1;
            }
            log.last = Some(Instant::now());
        }
    }

    fn menu_and_toolbar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("menu").show(ctx, |ui| {
            #[allow(deprecated)]
            egui::menu::bar(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("New device window").clicked() {
                        self.new_window();
                        ui.close_menu();
                    }
                    ui.menu_button("從範本開新視窗", |ui| {
                        if self.templates.is_empty() {
                            ui.label(RichText::new("(尚無範本，可在裝置視窗按「存成範本」)").weak());
                        }
                        let mut pick = None;
                        for (i, t) in self.templates.iter().enumerate() {
                            if ui.button(&t.name).clicked() {
                                pick = Some(i);
                            }
                        }
                        if let Some(i) = pick {
                            let w = DeviceWin::from_template(self.next_id, &self.templates[i]);
                            self.next_id += 1;
                            self.windows.push(w);
                            ui.close_menu();
                        }
                    });
                    ui.menu_button("刪除範本", |ui| {
                        let mut del = None;
                        for (i, t) in self.templates.iter().enumerate() {
                            if ui.button(format!("🗑 {}", t.name)).clicked() {
                                del = Some(i);
                            }
                        }
                        if let Some(i) = del {
                            let t = self.templates.remove(i);
                            self.notify(format!("已刪除範本「{}」", t.name));
                            ui.close_menu();
                        }
                    });
                    ui.separator();
                    if ui.button("匯出範本 (templates.json)").clicked() {
                        self.export_templates();
                        ui.close_menu();
                    }
                    if ui.button("匯入範本 (templates.json)").clicked() {
                        self.import_templates();
                        ui.close_menu();
                    }
                    if ui.button("開啟資料夾 (記錄/匯出)").clicked() {
                        let _ = std::fs::create_dir_all(data_dir());
                        open_folder(&data_dir());
                        ui.close_menu();
                    }
                });
                ui.menu_button("Connection", |ui| {
                    if ui.add_enabled(!self.connected_wanted, egui::Button::new("Connect")).clicked() {
                        self.connect();
                        ui.close_menu();
                    }
                    if ui.add_enabled(self.connected_wanted, egui::Button::new("Disconnect")).clicked() {
                        self.disconnect();
                        ui.close_menu();
                    }
                    if ui.button("Settings…").clicked() {
                        self.open_settings();
                        ui.close_menu();
                    }
                });
                ui.menu_button("Tools", |ui| {
                    if ui.button("Slave ID scanner…").clicked() {
                        self.scan.open = true;
                        ui.close_menu();
                    }
                    if ui.button("Traffic monitor (Tx/Rx)…").clicked() {
                        self.traffic.open = true;
                        ui.close_menu();
                    }
                    if ui.button("Write panel…").clicked() {
                        self.write_panel.open = true;
                        ui.close_menu();
                    }
                    ui.separator();
                    ui.checkbox(&mut self.confirm_writes, "寫入前確認");
                });
                ui.menu_button("View", |ui| {
                    let before = self.dark;
                    ui.radio_value(&mut self.dark, false, "Light (高對比)");
                    ui.radio_value(&mut self.dark, true, "Dark");
                    if before != self.dark {
                        apply_theme(ui.ctx(), self.dark);
                        ui.close_menu();
                    }
                });
                ui.menu_button("Window", |ui| {
                    for w in &mut self.windows {
                        let label = if w.open { format!("✔ {}", w.title) } else { w.title.clone() };
                        if ui.button(label).clicked() {
                            w.open = true;
                            ui.close_menu();
                        }
                    }
                });
            });
            ui.horizontal(|ui| {
                if ui.button("🗋 New").clicked() {
                    self.new_window();
                }
                ui.separator();
                if ui.button("⚙ Settings").clicked() {
                    self.open_settings();
                }
                if self.connected_wanted {
                    if ui.button("⏹ Disconnect").clicked() {
                        self.disconnect();
                    }
                } else if ui.button("▶ Connect").clicked() {
                    self.connect();
                }
                ui.separator();
                if ui.button("🔍 Scan").clicked() {
                    self.scan.open = true;
                }
                if ui.button("📡 Traffic").clicked() {
                    self.traffic.open = !self.traffic.open;
                }
                if ui.button("✏ Write").clicked() {
                    self.write_panel.open = !self.write_panel.open;
                }
                ui.separator();
                ui.label(RichText::new(self.conn.summary()).weak());
            });
        });
    }

    fn status_bar(&mut self, ctx: &egui::Context) {
        let sh = self.shared.lock().unwrap();
        // 每秒更新一次 Tx/Rx 速率
        let dt = self.rate.last.elapsed().as_secs_f64();
        if dt >= 1.0 {
            self.rate.tx_kbs = (sh.tx_bytes - self.rate.tx) as f64 / dt / 1024.0;
            self.rate.rx_kbs = (sh.rx_bytes - self.rate.rx) as f64 / dt / 1024.0;
            self.rate.tx = sh.tx_bytes;
            self.rate.rx = sh.rx_bytes;
            self.rate.last = Instant::now();
        }
        let secs = sh.started.elapsed().as_secs_f64().max(1.0);
        let (status, color) = match &sh.status {
            LinkStatus::Disconnected => ("未連線".to_string(), Color32::GRAY),
            LinkStatus::Connecting => ("連線中…".to_string(), ORANGE),
            LinkStatus::Connected => ("已連線".to_string(), GREEN),
            LinkStatus::Failed(e) => (e.clone(), RED),
        };
        let txrx = format!(
            "Tx: {:.2} KB  {:.3} KB/s    Rx: {:.2} KB  {:.3} KB/s",
            sh.tx_bytes as f64 / 1024.0,
            self.rate.tx_kbs,
            sh.rx_bytes as f64 / 1024.0,
            self.rate.rx_kbs
        );
        let reqs = format!(
            "Requests: {}  Responses: {}  ({:.1} req/s)",
            sh.requests,
            sh.responses,
            sh.requests as f64 / secs
        );
        let write_msg = sh.write_msg.clone();
        drop(sh);
        if self.connected_wanted {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
        let toast = self.toast.as_ref().filter(|(_, t)| t.elapsed() < Duration::from_secs(8)).map(|(m, _)| m.clone());
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.colored_label(color, "●");
                ui.label(status);
                ui.separator();
                ui.label(RichText::new(txrx).monospace());
                ui.separator();
                ui.label(reqs);
                if let Some(m) = toast {
                    ui.separator();
                    ui.colored_label(BLUE, m);
                } else if let Some(m) = write_msg {
                    ui.separator();
                    ui.label(m);
                }
            });
        });
    }

    fn connection_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut d) = self.conn_draft.take() else { return };
        let mut keep = true;
        let mut apply = false;
        let mut cancel = false;
        let modal = egui::Modal::new(egui::Id::new("conn_modal")).show(ctx, |ui| {
            ui.set_width(540.0);
            ui.heading("Connection settings");
            ui.separator();
            ui.horizontal(|ui| {
                ui.group(|ui| {
                    ui.vertical(|ui| {
                        ui.strong("Protocol");
                        ui.radio_value(&mut d.proto, Proto::Rtu, "Modbus RTU");
                        ui.radio_value(&mut d.proto, Proto::Tcp, "Modbus TCP");
                        ui.radio_value(&mut d.proto, Proto::RtuOverTcp, "Modbus RTU over TCP/IP");
                    });
                });
                ui.group(|ui| {
                    ui.vertical(|ui| {
                        ui.strong("Addressing convention");
                        ui.radio_value(&mut d.one_based, false, "Register address (starting from 0)");
                        ui.radio_value(&mut d.one_based, true, "Register number (starting from 1)");
                    });
                });
            });
            ui.add_enabled_ui(d.proto == Proto::Rtu, |ui| {
                ui.group(|ui| {
                    ui.strong("Modbus RTU");
                    egui::Grid::new("rtu").num_columns(2).show(ui, |ui| {
                        ui.label("Port");
                        ui.horizontal(|ui| {
                            egui::ComboBox::from_id_salt("port")
                                .width(230.0)
                                .selected_text(if d.serial_port.is_empty() { "(請選擇)" } else { &d.serial_port })
                                .show_ui(ui, |ui| {
                                    for p in &self.ports {
                                        ui.selectable_value(&mut d.serial_port, p.clone(), p);
                                    }
                                });
                            if ui.button("⟳").on_hover_text("重新掃描序列埠").clicked() {
                                self.ports = link::list_serial_ports();
                            }
                        });
                        ui.end_row();
                        ui.label("");
                        ui.add(egui::TextEdit::singleline(&mut d.serial_port).hint_text("/dev/cu.usbserial-XXXX"));
                        ui.end_row();
                        ui.label("Bitrate");
                        egui::ComboBox::from_id_salt("baud").selected_text(d.baud.to_string()).show_ui(ui, |ui| {
                            for b in BAUDS {
                                ui.selectable_value(&mut d.baud, b, b.to_string());
                            }
                        });
                        ui.end_row();
                        ui.label("Data bits");
                        ui.horizontal(|ui| {
                            ui.radio_value(&mut d.data_bits, 8, "8");
                            ui.radio_value(&mut d.data_bits, 7, "7");
                        });
                        ui.end_row();
                        ui.label("Parity");
                        ui.horizontal(|ui| {
                            ui.radio_value(&mut d.parity, Parity::None, "None");
                            ui.radio_value(&mut d.parity, Parity::Even, "Even");
                            ui.radio_value(&mut d.parity, Parity::Odd, "Odd");
                        });
                        ui.end_row();
                        ui.label("Stop bits");
                        ui.horizontal(|ui| {
                            ui.radio_value(&mut d.stop_bits, 1, "1");
                            ui.radio_value(&mut d.stop_bits, 2, "2");
                        });
                        ui.end_row();
                    });
                });
            });
            ui.add_enabled_ui(d.proto != Proto::Rtu, |ui| {
                ui.group(|ui| {
                    ui.strong("Modbus TCP");
                    egui::Grid::new("tcp").num_columns(2).show(ui, |ui| {
                        ui.label("IP address");
                        ui.text_edit_singleline(&mut d.host);
                        ui.end_row();
                        ui.label("TCP port");
                        ui.add(egui::DragValue::new(&mut d.tcp_port));
                        ui.end_row();
                    });
                });
            });
            ui.group(|ui| {
                ui.strong("General");
                egui::Grid::new("gen").num_columns(2).show(ui, |ui| {
                    ui.label("Timeout (ms)");
                    ui.add(egui::DragValue::new(&mut d.timeout_ms).range(50..=10000));
                    ui.end_row();
                    ui.label("Delay between polls (ms)");
                    ui.add(egui::DragValue::new(&mut d.poll_delay_ms).range(0..=10000));
                    ui.end_row();
                });
            });
            ui.horizontal(|ui| {
                if ui.button("  OK  ").clicked() {
                    apply = true;
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
        });
        if modal.should_close() && !apply {
            keep = false;
        }
        if apply {
            self.conn = d;
            if self.connected_wanted {
                self.connect(); // 套用新設定重新連線
            }
        } else if keep && !cancel {
            self.conn_draft = Some(d);
        }
    }

    fn scanner_window(&mut self, ctx: &egui::Context) {
        if !self.scan.open {
            return;
        }
        let mut open = true;
        let (running, current, from, to, found) = {
            let sh = self.shared.lock().unwrap();
            (sh.scan.running, sh.scan.current, sh.scan.from, sh.scan.to, sh.scan.found.clone())
        };
        egui::Window::new("Slave ID scanner")
            .default_pos([760.0, 120.0])
            .default_width(420.0)
            .constrain_to(self.work_area)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.add_enabled_ui(!running, |ui| {
                    egui::Grid::new("scan").num_columns(2).show(ui, |ui| {
                        ui.label("Function");
                        egui::ComboBox::from_id_salt("scanf").selected_text(self.scan.func.label()).show_ui(ui, |ui| {
                            for f in Func::ALL {
                                ui.selectable_value(&mut self.scan.func, f, f.label());
                            }
                        });
                        ui.end_row();
                        ui.label("Address");
                        ui.add(egui::DragValue::new(&mut self.scan.addr).range(0..=65535));
                        ui.end_row();
                        ui.label("Slave ID 範圍");
                        ui.horizontal(|ui| {
                            ui.add(egui::DragValue::new(&mut self.scan.from).range(0..=247));
                            ui.label("–");
                            ui.add(egui::DragValue::new(&mut self.scan.to).range(0..=247));
                        });
                        ui.end_row();
                    });
                });
                ui.label("提示: 掃描前可把 Timeout 調到 200–300 ms 加快速度。設備回 Exception 也代表該站號存在。");
                ui.horizontal(|ui| {
                    if running {
                        if ui.button("⏹ Stop").clicked() {
                            let _ = self.tx.send(Cmd::StopScan);
                        }
                        let total = (to.saturating_sub(from) as f32 + 1.0).max(1.0);
                        ui.add(
                            egui::ProgressBar::new((current.saturating_sub(from) as f32 + 1.0) / total)
                                .text(format!("ID {current}")),
                        );
                    } else if ui.add_enabled(self.connected_wanted, egui::Button::new("▶ Start")).clicked() {
                        let addr = self.to_proto_addr(self.scan.addr);
                        let (a, b) = (self.scan.from.min(self.scan.to), self.scan.from.max(self.scan.to));
                        let _ = self.tx.send(Cmd::Scan(ScanReq { func: self.scan.func, addr, from: a, to: b }));
                    }
                    if !self.connected_wanted {
                        ui.label("(請先連線)");
                    }
                });
                ui.separator();
                ui.label(format!("找到 {} 台設備 (雙擊開啟裝置視窗)", found.len()));
                egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                    for (unit, msg) in &found {
                        let r = ui.add(egui::Label::new(format!("ID {unit:>3}   {msg}")).sense(Sense::click()));
                        if r.double_clicked() {
                            let mut w = DeviceWin::new(self.next_id);
                            self.next_id += 1;
                            w.unit = *unit;
                            w.func = self.scan.func;
                            w.addr = self.scan.addr;
                            w.title = format!("ID {unit}");
                            self.windows.push(w);
                        }
                    }
                });
            });
        self.scan.open = open;
    }

    // ------------------------------------------------------------------
    // Traffic monitor
    // ------------------------------------------------------------------
    fn traffic_window(&mut self, ctx: &egui::Context) {
        if !self.traffic.open {
            return;
        }
        let mut open = true;
        let mut notice: Option<String> = None;
        let shared = self.shared.clone();
        let t = &mut self.traffic;
        // 固定在視窗下方、可拉動高度，不會蓋住裝置視窗
        egui::TopBottomPanel::bottom("traffic")
            .resizable(true)
            .default_height(230.0)
            .height_range(120.0..=700.0)
            .show(ctx, |ui| {
                let mut sh = shared.lock().unwrap();
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.strong("📡 Traffic monitor");
                    ui.separator();
                    ui.checkbox(&mut sh.traffic_paused, "⏸ 暫停");
                    ui.checkbox(&mut t.errors_only, "只看錯誤");
                    ui.checkbox(&mut t.follow, "自動捲到最新");
                    if ui.button("清除").clicked() {
                        sh.traffic.clear();
                    }
                    if ui.button("📋 複製").clicked() {
                        ui.ctx().copy_text(dump_traffic(&sh.traffic));
                        notice = Some(format!("已複製 {} 筆封包", sh.traffic.len()));
                    }
                    if ui.button("💾 匯出 txt").clicked() {
                        let path = data_dir().join(format!("traffic_{}.txt", chrono::Local::now().format("%Y%m%d_%H%M%S")));
                        let res = std::fs::create_dir_all(data_dir()).and_then(|_| std::fs::write(&path, dump_traffic(&sh.traffic)));
                        notice = Some(match res {
                            Ok(()) => format!("已匯出到 {}", path.display()),
                            Err(e) => format!("匯出失敗: {e}"),
                        });
                    }
                    ui.label(RichText::new(format!("{} 筆 (最多保留 {})", sh.traffic.len(), link::TRAFFIC_CAP)).weak());
                    ui.separator();
                    ui.colored_label(BLUE, "■ TX");
                    ui.colored_label(GREEN, "■ RX");
                    ui.colored_label(ORANGE, "■ Exception");
                    ui.colored_label(RED, "■ Timeout/錯誤");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("關閉").clicked() {
                            open = false;
                        }
                    });
                });
                ui.separator();
                let n = sh.traffic.len();
                let bad = |i: usize| sh.traffic.get(i).is_some_and(|f| f.mark != Mark::Normal);
                let idx: Vec<usize> = if t.errors_only {
                    (0..n).filter(|&i| bad(i) || (sh.traffic[i].dir == Dir::Tx && bad(i + 1))).collect()
                } else {
                    (0..n).collect()
                };
                let row_h = ui.text_style_height(&egui::TextStyle::Monospace) + ui.spacing().item_spacing.y;
                egui::ScrollArea::both()
                    .auto_shrink(false)
                    .stick_to_bottom(t.follow)
                    .show_rows(ui, row_h, idx.len(), |ui, range| {
                        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                        for k in range {
                            let f = &sh.traffic[idx[k]];
                            let (tag, c) = match (f.dir, f.mark) {
                                (Dir::Tx, _) => ("TX", BLUE),
                                (Dir::Rx, Mark::Normal) => ("RX", GREEN),
                                (Dir::Rx, Mark::Exception) => ("RX", ORANGE),
                                (Dir::Rx, Mark::Error) => ("RX", RED),
                            };
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(&f.time).monospace().weak());
                                ui.label(RichText::new(tag).monospace().strong().color(c));
                                // 說明欄固定寬度放在 hex 前面，長封包也看得到回應時間與錯誤
                                let note = RichText::new(&f.note);
                                let note = if f.mark == Mark::Normal { note.weak() } else { note.color(c) };
                                ui.allocate_ui_with_layout(
                                    egui::vec2(230.0, row_h),
                                    egui::Layout::left_to_right(egui::Align::Center),
                                    |ui| {
                                        ui.set_min_width(230.0);
                                        ui.add(egui::Label::new(note).truncate()).on_hover_text(&f.note);
                                    },
                                );
                                let body = if f.bytes.is_empty() { "—".to_string() } else { hex(&f.bytes, 64) };
                                ui.label(RichText::new(body).monospace());
                            });
                        }
                    });
            });
        t.open = open;
        if let Some(m) = notice {
            self.notify(m);
        }
    }

    // ------------------------------------------------------------------
    // Write panel
    // ------------------------------------------------------------------
    fn build_write_op(&self) -> Result<WriteOp, String> {
        let p = &self.write_panel;
        let a = self.to_proto_addr(p.addr);
        let vals = split_values(&p.text);
        if vals.is_empty() {
            return Err("請輸入要寫入的值".into());
        }
        match p.wfn {
            WFn::Fc05 => {
                if vals.len() != 1 {
                    return Err("FC05 一次只能寫一個 coil，多個請改用 FC15".into());
                }
                Ok(WriteOp::Coil(a, parse_bit(vals[0])?))
            }
            WFn::Fc15 => {
                let bits = vals.iter().map(|v| parse_bit(v)).collect::<Result<Vec<_>, _>>()?;
                if bits.len() > 1968 {
                    return Err("FC15 最多 1968 個 coil".into());
                }
                Ok(WriteOp::Coils(a, bits))
            }
            WFn::Fc06 => {
                if vals.len() != 1 {
                    return Err("FC06 一次只能寫一個暫存器，多個請改用 FC16".into());
                }
                let w = parse_for_write(vals[0], p.display)?;
                if w.len() != 1 {
                    return Err("32/64 位元格式會佔用多個暫存器，請改用 FC16".into());
                }
                Ok(WriteOp::Register(a, w[0]))
            }
            WFn::Fc16 => {
                let mut words = vec![];
                for v in vals {
                    words.extend(parse_for_write(v, p.display)?);
                }
                if words.len() > 123 {
                    return Err(format!("FC16 最多 123 個暫存器 (目前 {})", words.len()));
                }
                Ok(WriteOp::Registers(a, words))
            }
        }
    }

    fn write_panel_window(&mut self, ctx: &egui::Context) {
        if !self.write_panel.open {
            return;
        }
        let mut open = true;
        let op = self.build_write_op();
        let offset = self.shown_offset();
        let mut send: Option<(u8, WriteOp)> = None;
        let log: Vec<link::WriteLog> = self.shared.lock().unwrap().write_log.iter().rev().take(200).cloned().collect();
        let mut clear_log = false;
        let connected = self.connected_wanted;
        let area = self.work_area;
        let p = &mut self.write_panel;
        let confirm = &mut self.confirm_writes;
        egui::Window::new("Write panel")
            .default_pos([ctx.screen_rect().right() - 480.0, 90.0])
            .default_width(460.0)
            .constrain_to(area)
            .open(&mut open)
            .show(ctx, |ui| {
                egui::Grid::new("wp").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    ui.label("Device ID");
                    ui.add(egui::DragValue::new(&mut p.unit).range(0..=247));
                    ui.end_row();
                    ui.label("Function");
                    egui::ComboBox::from_id_salt("wfn").width(260.0).selected_text(p.wfn.label()).show_ui(ui, |ui| {
                        for f in [WFn::Fc05, WFn::Fc06, WFn::Fc15, WFn::Fc16] {
                            ui.selectable_value(&mut p.wfn, f, f.label());
                        }
                    });
                    ui.end_row();
                    ui.label(if offset == 1 { "Register number" } else { "Address" });
                    ui.add(egui::DragValue::new(&mut p.addr).range(0..=65535));
                    ui.end_row();
                    if matches!(p.wfn, WFn::Fc06 | WFn::Fc16) {
                        ui.label("Format");
                        egui::ComboBox::from_id_salt("wfmt").width(260.0).selected_text(p.display.label()).show_ui(ui, |ui| {
                            use Display::*;
                            for d in [Decimal, Integer, Hex, Binary, U32, U32Sw, I32, I32Sw, F32, F32Sw, F64, F64Sw] {
                                ui.selectable_value(&mut p.display, d, d.label());
                            }
                        });
                        ui.end_row();
                    }
                    ui.label("Values");
                    let hint = match p.wfn {
                        WFn::Fc05 => "1 / 0 (或 on / off)",
                        WFn::Fc15 => "例: 1 0 1 1",
                        WFn::Fc06 => "例: 100 或 0x0064",
                        WFn::Fc16 => "例: 100, 200, 300",
                    };
                    ui.add(egui::TextEdit::singleline(&mut p.text).hint_text(hint).desired_width(260.0));
                    ui.end_row();
                });
                ui.add_space(4.0);
                match &op {
                    Ok(o) => ui.label(RichText::new(format!("將送出: {}", o.describe(offset))).monospace()),
                    Err(e) => ui.colored_label(RED, e),
                };
                ui.horizontal(|ui| {
                    let can = op.is_ok() && connected;
                    if ui.add_enabled(can, egui::Button::new(RichText::new("✏ 送出").strong())).clicked() {
                        if let Ok(o) = &op {
                            send = Some((p.unit, o.clone()));
                        }
                    }
                    if !connected {
                        ui.label("(請先連線)");
                    }
                    ui.checkbox(confirm, "寫入前確認");
                });
                ui.separator();
                ui.horizontal(|ui| {
                    ui.strong("寫入紀錄");
                    if ui.small_button("清除").clicked() {
                        clear_log = true;
                    }
                });
                egui::ScrollArea::vertical().max_height(180.0).auto_shrink([false, true]).show(ui, |ui| {
                    if log.is_empty() {
                        ui.label(RichText::new("(尚無寫入)").weak());
                    }
                    for l in &log {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&l.time).monospace().weak());
                            ui.colored_label(if l.ok { GREEN } else { RED }, if l.ok { "✓" } else { "✗" });
                            ui.label(format!("ID {}  {}", l.unit, l.desc));
                            if !l.ok {
                                ui.colored_label(RED, &l.result);
                            }
                        });
                    }
                });
            });
        self.write_panel.open = open;
        if clear_log {
            self.shared.lock().unwrap().write_log.clear();
        }
        if let Some((u, o)) = send {
            self.request_write(u, o);
        }
    }

    fn confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some((unit, op)) = self.pending_write.clone() else { return };
        let offset = self.shown_offset();
        let summary = self.conn.summary();
        let mut decided: Option<bool> = None;
        let dont_ask = &mut self.confirm_dont_ask;
        let modal = egui::Modal::new(egui::Id::new("confirm_write")).show(ctx, |ui| {
            ui.set_min_width(440.0);
            ui.heading("確認寫入");
            ui.label("即將對設備寫入以下內容，送出前建議再核對一次站號與位址:");
            ui.add_space(4.0);
            egui::Grid::new("cw").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                ui.label("連線");
                ui.label(summary);
                ui.end_row();
                ui.label("站號");
                ui.label(RichText::new(unit.to_string()).strong());
                ui.end_row();
                ui.label("內容");
                ui.label(RichText::new(op.describe(offset)).monospace().strong());
                ui.end_row();
            });
            ui.add_space(6.0);
            ui.checkbox(dont_ask, "之後不再詢問 (可在 Tools 選單重新開啟)");
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                // 刻意不接受 Enter: 避免寫入對話框按下的 Enter 直接把確認也送出
                if ui.button(RichText::new("✏ 送出").strong()).clicked() {
                    decided = Some(true);
                }
                if ui.button("取消").clicked() {
                    decided = Some(false);
                }
            });
        });
        if decided.is_none() && modal.should_close() {
            decided = Some(false);
        }
        match decided {
            Some(true) => {
                if self.confirm_dont_ask {
                    self.confirm_writes = false;
                }
                let _ = self.tx.send(Cmd::Write { unit, op });
                self.pending_write = None;
            }
            Some(false) => self.pending_write = None,
            None => {}
        }
    }

    fn template_dialog(&mut self, ctx: &egui::Context) {
        let Some((id, mut name)) = self.tmpl_dlg.take() else { return };
        let mut done: Option<bool> = None;
        let exists = self.templates.iter().any(|t| t.name == name.trim());
        let modal = egui::Modal::new(egui::Id::new("tmpl")).show(ctx, |ui| {
            ui.set_min_width(360.0);
            ui.heading("存成範本");
            ui.label("範本會保存功能碼、位址、長度、顯示格式、Alias 與格子顏色。");
            let r = ui.add(egui::TextEdit::singleline(&mut name).hint_text("例: 台達電表 DPM").desired_width(320.0));
            r.request_focus();
            if exists {
                ui.colored_label(ORANGE, "已有同名範本，儲存後會覆蓋。");
            }
            ui.horizontal(|ui| {
                if ui.add_enabled(!name.trim().is_empty(), egui::Button::new("儲存")).clicked()
                    || (ui.input(|i| i.key_pressed(egui::Key::Enter)) && !name.trim().is_empty())
                {
                    done = Some(true);
                }
                if ui.button("取消").clicked() {
                    done = Some(false);
                }
            });
        });
        if done.is_none() && modal.should_close() {
            done = Some(false);
        }
        match done {
            Some(true) => {
                if let Some(w) = self.windows.iter().find(|w| w.id == id) {
                    let n = name.trim().to_string();
                    self.templates.retain(|t| t.name != n);
                    self.templates.push(Template::from_window(n.clone(), w));
                    self.templates.sort_by(|a, b| a.name.cmp(&b.name));
                    self.notify(format!("已存成範本「{n}」 (File → 從範本開新視窗)"));
                }
            }
            Some(false) => {}
            None => self.tmpl_dlg = Some((id, name)),
        }
    }

    // ------------------------------------------------------------------
    // 裝置視窗
    // ------------------------------------------------------------------
    fn device_windows(&mut self, ctx: &egui::Context) {
        let one_based = self.conn.one_based;
        let area = self.work_area;
        let mut actions: Vec<DevAction> = vec![];
        let mut any_highlight = false;
        for w in &mut self.windows {
            if !w.open {
                continue;
            }
            let result = self.shared.lock().unwrap().results.get(&w.id).cloned().unwrap_or_default();

            // 比對上一輪的值，記錄變動時間
            let ch = self.changes.entry(w.id).or_default();
            if result.values.len() != ch.prev.len() {
                ch.at = vec![None; result.values.len()];
            } else {
                for (i, (a, b)) in result.values.iter().zip(&ch.prev).enumerate() {
                    if a != b {
                        ch.at[i] = Some(Instant::now());
                    }
                }
            }
            ch.prev = result.values.clone();
            if !w.highlight {
                ch.at.iter_mut().for_each(|x| *x = None);
            }
            any_highlight |= ch.at.iter().flatten().any(|t| t.elapsed().as_secs_f32() < HIGHLIGHT_SECS);
            let changed = ch.at.clone();
            let log_info = self.logs.get(&w.id).map(|l| (l.path.clone(), l.rows));

            let mut open = true;
            egui::Window::new(&w.title)
                .id(egui::Id::new(("dev", w.id)))
                .open(&mut open)
                .default_size([980.0, 470.0])
                .constrain_to(area)
                .default_pos([12.0 + (w.id % 6) as f32 * 30.0, 80.0 + (w.id % 6) as f32 * 30.0])
                .show(ctx, |ui| {
                    device_header(ui, w, &result, &self.shared);
                    device_tools(ui, w, log_info, &mut actions);
                    ui.separator();
                    device_table(ui, w, &result, one_based, &changed);
                    if let Some(op) = write_dialog(ctx, w, one_based) {
                        actions.push(DevAction::Write(w.unit, op));
                    }
                });
            if !open {
                actions.push(DevAction::StopLog(w.id));
            }
            w.open = open;
        }
        if any_highlight {
            ctx.request_repaint_after(Duration::from_millis(60));
        }
        for a in actions {
            match a {
                DevAction::Write(u, op) => self.request_write(u, op),
                DevAction::SaveTemplate(id) => {
                    let name = self.windows.iter().find(|w| w.id == id).map(|w| w.title.clone()).unwrap_or_default();
                    self.tmpl_dlg = Some((id, name));
                }
                DevAction::StartLog(id) => self.start_log(id),
                DevAction::StopLog(id) => {
                    if let Some(l) = self.logs.remove(&id) {
                        self.notify(format!("已停止記錄 ({} 筆): {}", l.rows, l.path.display()));
                    }
                }
                DevAction::OpenFolder(p) => open_folder(&p),
            }
        }
    }
}

fn dump_traffic(frames: &std::collections::VecDeque<link::Frame>) -> String {
    let mut s = String::new();
    for f in frames {
        let dir = if f.dir == Dir::Tx { "TX" } else { "RX" };
        s.push_str(&format!("{}  {}  {:<60}  {}\n", f.time, dir, hex(&f.bytes, usize::MAX), f.note));
    }
    s
}

fn device_header(ui: &mut egui::Ui, w: &mut DeviceWin, r: &link::JobResult, shared: &SharedRef) {
    ui.horizontal_top(|ui| {
        ui.group(|ui| {
            ui.vertical(|ui| {
                ui.strong("Device Settings");
                ui.horizontal(|ui| {
                    ui.label("Device ID");
                    ui.add(egui::DragValue::new(&mut w.unit).range(0..=247));
                });
                egui::ComboBox::from_id_salt(("func", w.id)).selected_text(w.func.label()).show_ui(ui, |ui| {
                    for f in Func::ALL {
                        ui.selectable_value(&mut w.func, f, f.label());
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Address");
                    ui.add(egui::DragValue::new(&mut w.addr).range(0..=65535));
                    ui.label("Length");
                    ui.add(egui::DragValue::new(&mut w.len).range(1..=if w.func.is_bit() { 2000 } else { 125 }));
                });
                ui.horizontal(|ui| {
                    ui.label("Title");
                    ui.add(egui::TextEdit::singleline(&mut w.title).desired_width(130.0));
                });
            });
        });
        ui.add_enabled_ui(!w.func.is_bit(), |ui| {
            ui.group(|ui| {
                ui.vertical(|ui| {
                    ui.strong("Display options");
                    egui::Grid::new(("disp", w.id)).num_columns(3).show(ui, |ui| {
                        use Display::*;
                        let rows = [[Decimal, U32, F32], [Integer, U32Sw, F32Sw], [Hex, I32, F64], [Binary, I32Sw, F64Sw]];
                        for row in rows {
                            for d in row {
                                ui.radio_value(&mut w.display, d, d.label());
                            }
                            ui.end_row();
                        }
                    });
                });
            });
            ui.group(|ui| {
                ui.vertical(|ui| {
                    ui.strong("ASCII display");
                    ui.radio_value(&mut w.ascii, Ascii::Off, "Off");
                    ui.radio_value(&mut w.ascii, Ascii::OneChar, "1 char/reg");
                    ui.radio_value(&mut w.ascii, Ascii::TwoChar, "2 char/reg");
                    ui.radio_value(&mut w.ascii, Ascii::TwoCharSw, "2 char/reg sw");
                });
            });
        });
        ui.group(|ui| {
            ui.set_min_width(190.0);
            ui.vertical(|ui| {
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                ui.strong("Frame counters");
                ui.label(format!("Number of polls : {}", r.polls));
                ui.label(format!("Valid responses : {}", r.ok));
                ui.label(format!("Errors : {}", r.errors));
                if r.ok > 0 {
                    ui.label(format!("Response : {:.0} ms (avg {:.0})", r.last_ms, r.avg_ms));
                } else {
                    ui.label("Response : —");
                }
                if ui.button("Reset").clicked() {
                    if let Some(x) = shared.lock().unwrap().results.get_mut(&w.id) {
                        x.polls = 0;
                        x.ok = 0;
                        x.errors = 0;
                        x.avg_ms = 0.0;
                    }
                }
            });
        });
    });
    if let Some(e) = &r.last_err {
        ui.colored_label(RED, format!("⚠ {e}"));
    }
}

/// 標題下方的工具列: 變動高亮、CSV 記錄、存成範本
fn device_tools(ui: &mut egui::Ui, w: &mut DeviceWin, log: Option<(PathBuf, u64)>, actions: &mut Vec<DevAction>) {
    ui.horizontal(|ui| {
        ui.checkbox(&mut w.highlight, "變動高亮");
        ui.separator();
        let mut on = log.is_some();
        if ui.checkbox(&mut on, "CSV 記錄").changed() {
            actions.push(if on { DevAction::StartLog(w.id) } else { DevAction::StopLog(w.id) });
        }
        ui.label("每");
        ui.add(egui::DragValue::new(&mut w.log_interval).range(1..=3600).suffix(" 秒"));
        if let Some((path, rows)) = log {
            ui.colored_label(GREEN, format!("● 記錄中 {rows} 筆"));
            if ui.small_button("📂").on_hover_text(path.display().to_string()).clicked() {
                if let Some(dir) = path.parent() {
                    actions.push(DevAction::OpenFolder(dir.to_path_buf()));
                }
            }
        }
        ui.separator();
        if ui.button("💾 存成範本").clicked() {
            actions.push(DevAction::SaveTemplate(w.id));
        }
        ui.label(RichText::new("右鍵數值可設定顏色、寫入").weak());
    });
}

fn device_table(ui: &mut egui::Ui, w: &mut DeviceWin, r: &link::JobResult, one_based: bool, changed: &[Option<Instant>]) {
    // 視窗設定改了但還沒輪詢到新資料時，先不要拿舊資料亂顯示
    let fresh = r.values.len() == w.len as usize;
    let texts = if fresh { format_values(&r.values, w.func, w.display, w.ascii) } else { vec![] };
    let blocks = (w.len as usize).div_ceil(10);
    let base = w.addr as usize;
    let width = if w.func.is_bit() { 1 } else { w.display.width() };
    egui::ScrollArea::both().show(ui, |ui| {
        egui::Grid::new(("tbl", w.id)).striped(true).min_col_width(30.0).spacing([12.0, 4.0]).show(ui, |ui| {
            ui.label("");
            for b in 0..blocks {
                ui.strong("Alias");
                value_cell(ui, egui::Label::new(RichText::new(format!("{}", base + b * 10)).strong()), None);
            }
            ui.end_row();
            for row in 0..10usize {
                ui.label(RichText::new(format!("+{row}")).weak());
                for b in 0..blocks {
                    let idx = b * 10 + row;
                    if idx >= w.len as usize {
                        ui.label("");
                        value_cell(ui, egui::Label::new(""), None);
                        continue;
                    }
                    let key = idx as u16;
                    let mut alias = w.aliases.get(&key).cloned().unwrap_or_default();
                    if ui.add_sized([120.0, ui.spacing().interact_size.y], egui::TextEdit::singleline(&mut alias)).changed() {
                        if alias.is_empty() {
                            w.aliases.remove(&key);
                        } else {
                            w.aliases.insert(key, alias);
                        }
                    }
                    let text = texts.get(idx).cloned().unwrap_or_else(|| "—".into());

                    // 背景色: 剛變動的高亮優先，其次是使用者設定的顏色
                    let group = idx - idx % width; // 多暫存器數值看整組有沒有變
                    let since = (group..(group + width).min(changed.len()))
                        .filter_map(|i| changed.get(i).copied().flatten())
                        .map(|t| t.elapsed().as_secs_f32())
                        .fold(f32::MAX, f32::min);
                    let user = w.cell_colors.get(&key).map(|c| Color32::from_rgb(c[0], c[1], c[2]));
                    let bg = if since < HIGHLIGHT_SECS && !text.is_empty() {
                        let a = ((1.0 - since / HIGHLIGHT_SECS) * 230.0) as u8;
                        Some(Color32::from_rgba_unmultiplied(255, 190, 40, a))
                    } else {
                        user
                    };
                    let mut rt = RichText::new(&text).monospace();
                    if bg.is_some() && user.is_some() {
                        rt = rt.color(Color32::BLACK);
                    }
                    let resp = value_cell(ui, egui::Label::new(rt).sense(Sense::click()), bg).on_hover_text(format!(
                        "{} {}{}",
                        if one_based { "Register number" } else { "Address" },
                        base + idx,
                        if w.func.writable() { "  (雙擊寫入，右鍵更多)" } else { "  (右鍵設定顏色)" }
                    ));
                    let writable = w.func.writable() && !text.is_empty() && fresh;
                    let start_write = |w: &mut DeviceWin| {
                        let cur = if w.func.is_bit() { String::new() } else { text.split(' ').next().unwrap_or("").to_string() };
                        w.write = Some(WriteDlg { offset: key, text: cur, err: None });
                    };
                    if resp.double_clicked() && writable {
                        start_write(w);
                    }
                    resp.context_menu(|ui| {
                        ui.label(RichText::new(format!("位址 {}", base + idx)).weak());
                        if ui.add_enabled(writable, egui::Button::new("✏ 寫入…")).clicked() {
                            start_write(w);
                            ui.close();
                        }
                        if ui.button("📋 複製數值").clicked() {
                            ui.ctx().copy_text(text.clone());
                            ui.close();
                        }
                        ui.separator();
                        ui.label("背景色");
                        ui.horizontal(|ui| {
                            for (name, c) in CELL_COLORS {
                                let fill = c.map(|c| Color32::from_rgb(c[0], c[1], c[2])).unwrap_or(Color32::TRANSPARENT);
                                let btn = egui::Button::new(RichText::new(name).color(if c.is_some() { Color32::BLACK } else { ui.visuals().text_color() })).fill(fill);
                                if ui.add(btn).clicked() {
                                    match c {
                                        Some(c) => {
                                            w.cell_colors.insert(key, c);
                                        }
                                        None => {
                                            w.cell_colors.remove(&key);
                                        }
                                    }
                                    ui.close();
                                }
                            }
                        });
                    });
                }
                ui.end_row();
            }
        });
    });
}

/// 數值欄固定寬度、靠左對齊，避免 float / binary 被擠壓；可帶背景色
fn value_cell(ui: &mut egui::Ui, label: egui::Label, bg: Option<Color32>) -> egui::Response {
    ui.allocate_ui_with_layout(
        egui::vec2(170.0, ui.spacing().interact_size.y),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_min_width(170.0);
            if let Some(bg) = bg {
                let rect = ui.max_rect().expand2(egui::vec2(4.0, 1.0));
                ui.painter().rect_filled(rect, 3.0, bg);
            }
            ui.add(label)
        },
    )
    .inner
}

fn write_dialog(ctx: &egui::Context, w: &mut DeviceWin, one_based: bool) -> Option<WriteOp> {
    let mut dlg = w.write.take()?;
    let shown = w.addr + dlg.offset as u32;
    let proto = if one_based { shown.saturating_sub(1) } else { shown } as u16;
    let mut keep = true;
    let mut op = None;
    let modal = egui::Modal::new(egui::Id::new(("wr", w.id))).show(ctx, |ui| {
        ui.set_min_width(320.0);
        ui.heading(format!("Write — ID {} @ {}", w.unit, shown));
        ui.separator();
        if w.func == Func::Coils {
            ui.label("FC05 Write single coil");
            ui.horizontal(|ui| {
                if ui.button("ON").clicked() {
                    op = Some(WriteOp::Coil(proto, true));
                }
                if ui.button("OFF").clicked() {
                    op = Some(WriteOp::Coil(proto, false));
                }
            });
        } else {
            ui.label(format!("格式: {}", w.display.label()));
            let r = ui.text_edit_singleline(&mut dlg.text);
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui.button("Send").clicked() || enter {
                match parse_for_write(&dlg.text, w.display) {
                    Ok(words) if words.len() == 1 => op = Some(WriteOp::Register(proto, words[0])),
                    Ok(words) => op = Some(WriteOp::Registers(proto, words)),
                    Err(e) => dlg.err = Some(e),
                }
            }
            if let Some(e) = &dlg.err {
                ui.colored_label(RED, e);
            }
            ui.label(RichText::new("單一暫存器用 FC06, 32/64 位元格式用 FC16").weak());
        }
        if ui.button("Cancel").clicked() {
            ui.close();
        }
    });
    if modal.should_close() {
        keep = false;
    }
    if keep && op.is_none() {
        w.write = Some(dlg);
    }
    op
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.sync_jobs();
        self.write_logs();
        self.menu_and_toolbar(ctx);
        self.status_bar(ctx);
        self.traffic_window(ctx);
        // 浮動視窗都限制在中間工作區，不會蓋到上下方的面板
        self.work_area = egui::CentralPanel::default().show(ctx, |ui| ui.max_rect()).inner;
        self.device_windows(ctx);
        self.write_panel_window(ctx);
        self.scanner_window(ctx);
        self.connection_dialog(ctx);
        self.template_dialog(ctx);
        self.confirm_dialog(ctx);
        // 已關閉且沒有別名、顏色的空視窗就直接丟掉
        let changes = &mut self.changes;
        self.windows.retain(|w| {
            let keep = w.open || !w.aliases.is_empty() || !w.cell_colors.is_empty();
            if !keep {
                changes.remove(&w.id);
            }
            keep
        });
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let p = Persist {
            conn: self.conn.clone(),
            windows: self.windows.clone(),
            next_id: self.next_id,
            dark: self.dark,
            confirm_writes: self.confirm_writes,
            templates: self.templates.clone(),
        };
        eframe::set_value(storage, eframe::APP_KEY, &p);
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 780.0])
            .with_title("ModScope — Modbus Master"),
        ..Default::default()
    };
    eframe::run_native("ModScope", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}
