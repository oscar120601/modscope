//! 背景通訊執行緒: 持有唯一一條 Modbus 連線，輪流輪詢各裝置視窗、處理寫入與站號掃描。
//! 連線外面包了一層 Tap，記錄所有進出的原始位元組，供 Traffic monitor 與 Tx/Rx 統計使用。

use crate::format::Func;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskCx, Poll};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};
use tokio_modbus::client::{rtu, tcp, Context, Reader, Writer};
use tokio_modbus::prelude::{Slave, SlaveContext};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Proto {
    Rtu,
    Tcp,
    RtuOverTcp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Parity {
    None,
    Even,
    Odd,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConnSettings {
    pub proto: Proto,
    pub serial_port: String,
    pub baud: u32,
    pub data_bits: u8,
    pub parity: Parity,
    pub stop_bits: u8,
    pub host: String,
    pub tcp_port: u16,
    pub timeout_ms: u64,
    pub poll_delay_ms: u64,
    /// true = Register number(從 1 起算)；false = Register address(從 0 起算)
    pub one_based: bool,
}

impl Default for ConnSettings {
    fn default() -> Self {
        Self {
            proto: Proto::Tcp,
            serial_port: String::new(),
            baud: 9600,
            data_bits: 8,
            parity: Parity::None,
            stop_bits: 1,
            host: "192.168.1.1".into(),
            tcp_port: 502,
            timeout_ms: 1000,
            poll_delay_ms: 10,
            one_based: false,
        }
    }
}

impl ConnSettings {
    pub fn summary(&self) -> String {
        let p = match self.parity {
            Parity::None => 'N',
            Parity::Even => 'E',
            Parity::Odd => 'O',
        };
        match self.proto {
            Proto::Rtu => format!("RTU {} {} {}{}{}", self.serial_port, self.baud, self.data_bits, p, self.stop_bits),
            Proto::Tcp => format!("TCP {}:{}", self.host, self.tcp_port),
            Proto::RtuOverTcp => format!("RTU over TCP {}:{}", self.host, self.tcp_port),
        }
    }
}

/// 一個裝置視窗對應一個輪詢工作(位址已是 0 起算的協定位址)
#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    pub id: u64,
    pub unit: u8,
    pub func: Func,
    pub addr: u16,
    pub len: u16,
}

#[derive(Default, Clone)]
pub struct JobResult {
    pub values: Vec<u16>,
    pub polls: u64,
    pub ok: u64,
    pub errors: u64,
    pub last_err: Option<String>,
    /// 最近一次回應時間(ms)
    pub last_ms: f64,
    /// 平均回應時間(指數平滑, ms)
    pub avg_ms: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum LinkStatus {
    Disconnected,
    Connecting,
    Connected,
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct ScanReq {
    pub func: Func,
    pub addr: u16,
    pub from: u8,
    pub to: u8,
}

#[derive(Default)]
pub struct ScanState {
    pub running: bool,
    pub current: u8,
    pub from: u8,
    pub to: u8,
    /// (站號, 回應說明)
    pub found: Vec<(u8, String)>,
}

#[derive(Clone, Debug)]
pub enum WriteOp {
    /// FC05
    Coil(u16, bool),
    /// FC06
    Register(u16, u16),
    /// FC15
    Coils(u16, Vec<bool>),
    /// FC16
    Registers(u16, Vec<u16>),
}

impl WriteOp {
    /// `shown_offset`: 畫面位址與協定位址的差(Register number 模式為 1)
    pub fn describe(&self, shown_offset: u32) -> String {
        let s = |a: &u16| *a as u32 + shown_offset;
        match self {
            WriteOp::Coil(a, v) => format!("FC05  coil {} = {}", s(a), if *v { "ON" } else { "OFF" }),
            WriteOp::Register(a, v) => format!("FC06  reg {} = {v} (0x{v:04X})", s(a)),
            WriteOp::Coils(a, v) => format!(
                "FC15  coil {}..{} = [{}]",
                s(a),
                s(a) + v.len() as u32 - 1,
                v.iter().map(|b| if *b { "1" } else { "0" }).collect::<Vec<_>>().join(" ")
            ),
            WriteOp::Registers(a, v) => format!(
                "FC16  reg {}..{} = [{}]",
                s(a),
                s(a) + v.len() as u32 - 1,
                v.iter().map(|x| format!("0x{x:04X}")).collect::<Vec<_>>().join(" ")
            ),
        }
    }
}

pub enum Cmd {
    Connect(ConnSettings),
    Disconnect,
    Write { unit: u8, op: WriteOp },
    Scan(ScanReq),
    StopScan,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Tx,
    Rx,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    Normal,
    Exception,
    Error,
}

/// Traffic monitor 的一行
#[derive(Clone, Debug)]
pub struct Frame {
    pub time: String,
    pub dir: Dir,
    pub bytes: Vec<u8>,
    pub note: String,
    pub mark: Mark,
}

#[derive(Clone, Debug)]
pub struct WriteLog {
    pub time: String,
    pub unit: u8,
    pub desc: String,
    pub ok: bool,
    pub result: String,
}

pub const TRAFFIC_CAP: usize = 5000;

pub struct Shared {
    pub jobs: Vec<Job>,
    pub results: HashMap<u64, JobResult>,
    pub status: LinkStatus,
    pub requests: u64,
    pub responses: u64,
    pub started: Instant,
    pub write_msg: Option<String>,
    pub scan: ScanState,
    pub tx_bytes: u64,
    pub rx_bytes: u64,
    pub traffic: VecDeque<Frame>,
    pub traffic_paused: bool,
    pub write_log: Vec<WriteLog>,
    /// 協定位址 → 畫面位址的差，寫入紀錄顯示用
    pub shown_offset: u32,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            jobs: vec![],
            results: HashMap::new(),
            status: LinkStatus::Disconnected,
            requests: 0,
            responses: 0,
            started: Instant::now(),
            write_msg: None,
            scan: ScanState::default(),
            tx_bytes: 0,
            rx_bytes: 0,
            traffic: VecDeque::new(),
            traffic_paused: false,
            write_log: vec![],
            shown_offset: 0,
        }
    }
}

pub type SharedRef = Arc<Mutex<Shared>>;

pub fn now_str() -> String {
    chrono::Local::now().format("%H:%M:%S%.3f").to_string()
}

// ---------------------------------------------------------------------------
// Tap: 包在 TCP / 序列埠外面，記錄每次讀寫的原始位元組
// ---------------------------------------------------------------------------

#[derive(Default, Debug)]
struct TapBuf {
    tx: Vec<u8>,
    rx: Vec<u8>,
}

type TapRef = Arc<Mutex<TapBuf>>;

#[derive(Debug)]
struct Tap<T> {
    inner: T,
    buf: TapRef,
}

impl<T: AsyncRead + Unpin> AsyncRead for Tap<T> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut TaskCx<'_>, out: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        let before = out.filled().len();
        let r = Pin::new(&mut self.inner).poll_read(cx, out);
        if let Poll::Ready(Ok(())) = &r {
            let new = &out.filled()[before..];
            if !new.is_empty() {
                self.buf.lock().unwrap().rx.extend_from_slice(new);
            }
        }
        r
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Tap<T> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut TaskCx<'_>, data: &[u8]) -> Poll<std::io::Result<usize>> {
        let r = Pin::new(&mut self.inner).poll_write(cx, data);
        if let Poll::Ready(Ok(n)) = &r {
            self.buf.lock().unwrap().tx.extend_from_slice(&data[..*n]);
        }
        r
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskCx<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut TaskCx<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

struct Conn {
    ctx: Context,
    tap: TapRef,
}

/// 一次請求結束後，把攔截到的 TX / RX 位元組寫進 Traffic 紀錄
fn flush_traffic(conn: &Conn, shared: &SharedRef, label: &str, elapsed: Duration, mark: Mark, note: &str) {
    let (tx, rx) = {
        let mut b = conn.tap.lock().unwrap();
        (std::mem::take(&mut b.tx), std::mem::take(&mut b.rx))
    };
    let mut sh = shared.lock().unwrap();
    sh.tx_bytes += tx.len() as u64;
    sh.rx_bytes += rx.len() as u64;
    if sh.traffic_paused {
        return;
    }
    let time = now_str();
    let ms = elapsed.as_secs_f64() * 1000.0;
    sh.traffic.push_back(Frame { time: time.clone(), dir: Dir::Tx, bytes: tx, note: label.to_string(), mark: Mark::Normal });
    let rx_note = match mark {
        Mark::Normal => format!("{ms:.0} ms"),
        _ if rx.is_empty() => format!("{note} (無回應, {ms:.0} ms)"),
        _ => format!("{note} ({ms:.0} ms)"),
    };
    sh.traffic.push_back(Frame { time, dir: Dir::Rx, bytes: rx, note: rx_note, mark });
    while sh.traffic.len() > TRAFFIC_CAP {
        sh.traffic.pop_front();
    }
}

pub fn spawn(shared: SharedRef, rx: Receiver<Cmd>, repaint: impl Fn() + Send + 'static) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(run(shared, rx, repaint));
    });
}

async fn connect(s: &ConnSettings) -> Result<Conn, String> {
    let to = Duration::from_millis(s.timeout_ms.max(100) * 3);
    let tap: TapRef = Default::default();
    let ctx = match s.proto {
        Proto::Tcp | Proto::RtuOverTcp => {
            let target = format!("{}:{}", s.host.trim(), s.tcp_port);
            let stream = timeout(to, TcpStream::connect(&target))
                .await
                .map_err(|_| format!("連線 {target} 逾時"))?
                .map_err(|e| format!("連線 {target} 失敗: {e}"))?;
            let _ = stream.set_nodelay(true);
            let t = Tap { inner: stream, buf: tap.clone() };
            if s.proto == Proto::Tcp {
                tcp::attach(t)
            } else {
                rtu::attach(t)
            }
        }
        Proto::Rtu => {
            use tokio_serial::{DataBits, Parity as P, StopBits};
            let builder = tokio_serial::new(s.serial_port.trim(), s.baud)
                .data_bits(if s.data_bits == 7 { DataBits::Seven } else { DataBits::Eight })
                .parity(match s.parity {
                    Parity::None => P::None,
                    Parity::Even => P::Even,
                    Parity::Odd => P::Odd,
                })
                .stop_bits(if s.stop_bits == 2 { StopBits::Two } else { StopBits::One });
            let port = tokio_serial::SerialStream::open(&builder)
                .map_err(|e| format!("開啟 {} 失敗: {e}", s.serial_port))?;
            rtu::attach(Tap { inner: port, buf: tap.clone() })
        }
    };
    Ok(Conn { ctx, tap })
}

/// 讀取結果: Ok(值) / Exception(裝置有回應但拒絕)/ Transport(逾時、斷線等，需重建連線)
enum ReadOutcome {
    Ok(Vec<u16>),
    Exception(String),
    Transport(String),
}

fn func_code(f: Func) -> u8 {
    match f {
        Func::Coils => 1,
        Func::DiscreteInputs => 2,
        Func::HoldingRegisters => 3,
        Func::InputRegisters => 4,
    }
}

async fn read(
    conn: &mut Conn,
    shared: &SharedRef,
    offset: u32,
    unit: u8,
    func: Func,
    addr: u16,
    len: u16,
    to: Duration,
) -> (ReadOutcome, Duration) {
    // 清掉前一次殘留的位元組，讓紀錄對得上
    *conn.tap.lock().unwrap() = TapBuf::default();
    conn.ctx.set_slave(Slave(unit));
    let ctx = &mut conn.ctx;
    let start = Instant::now();
    let fut = async {
        let bits = |v: Vec<bool>| v.into_iter().map(u16::from).collect::<Vec<u16>>();
        match func {
            Func::Coils => ctx.read_coils(addr, len).await.map(|r| r.map(bits)),
            Func::DiscreteInputs => ctx.read_discrete_inputs(addr, len).await.map(|r| r.map(bits)),
            Func::HoldingRegisters => ctx.read_holding_registers(addr, len).await,
            Func::InputRegisters => ctx.read_input_registers(addr, len).await,
        }
    };
    let outcome = match timeout(to, fut).await {
        Err(_) => ReadOutcome::Transport("Timeout".into()),
        Ok(Err(e)) => ReadOutcome::Transport(e.to_string()),
        Ok(Ok(Err(ex))) => ReadOutcome::Exception(format!("Exception: {ex}")),
        Ok(Ok(Ok(v))) => ReadOutcome::Ok(v),
    };
    let elapsed = start.elapsed();
    let label = format!("ID {unit} FC{:02} @{} ×{len}", func_code(func), addr as u32 + offset);
    let (mark, note) = match &outcome {
        ReadOutcome::Ok(_) => (Mark::Normal, String::new()),
        ReadOutcome::Exception(e) => (Mark::Exception, e.clone()),
        ReadOutcome::Transport(e) => (Mark::Error, e.clone()),
    };
    flush_traffic(conn, shared, &label, elapsed, mark, &note);
    (outcome, elapsed)
}

async fn run(shared: SharedRef, rx: Receiver<Cmd>, repaint: impl Fn()) {
    let mut settings: Option<ConnSettings> = None;
    let mut conn: Option<Conn> = None;
    let mut writes: VecDeque<(u8, WriteOp)> = VecDeque::new();
    let mut scan: Option<(ScanReq, u8)> = None;
    let mut job_idx = 0usize;

    loop {
        // 1. 處理 GUI 送來的指令
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                Cmd::Connect(s) => {
                    settings = Some(s);
                    conn = None;
                }
                Cmd::Disconnect => {
                    settings = None;
                    conn = None;
                    scan = None;
                    let mut sh = shared.lock().unwrap();
                    sh.status = LinkStatus::Disconnected;
                    sh.scan.running = false;
                }
                Cmd::Write { unit, op } => writes.push_back((unit, op)),
                Cmd::Scan(req) => {
                    let mut sh = shared.lock().unwrap();
                    sh.scan = ScanState { running: true, current: req.from, from: req.from, to: req.to, found: vec![] };
                    scan = Some((req.clone(), req.from));
                }
                Cmd::StopScan => {
                    scan = None;
                    shared.lock().unwrap().scan.running = false;
                }
            }
        }

        let Some(s) = settings.clone() else {
            sleep(Duration::from_millis(50)).await;
            continue;
        };
        let to = Duration::from_millis(s.timeout_ms.max(50));
        let offset = s.one_based as u32;
        shared.lock().unwrap().shown_offset = offset;

        // 2. 確保連線存在
        if conn.is_none() {
            shared.lock().unwrap().status = LinkStatus::Connecting;
            repaint();
            match connect(&s).await {
                Ok(c) => {
                    conn = Some(c);
                    shared.lock().unwrap().status = LinkStatus::Connected;
                }
                Err(e) => {
                    shared.lock().unwrap().status = LinkStatus::Failed(e);
                    repaint();
                    sleep(Duration::from_secs(1)).await;
                    continue;
                }
            }
        }
        let c = conn.as_mut().unwrap();

        // 3. 寫入優先
        if let Some((unit, op)) = writes.pop_front() {
            *c.tap.lock().unwrap() = TapBuf::default();
            c.ctx.set_slave(Slave(unit));
            let desc = op.describe(offset);
            let start = Instant::now();
            let res = match &op {
                WriteOp::Coil(a, v) => timeout(to, c.ctx.write_single_coil(*a, *v)).await,
                WriteOp::Register(a, v) => timeout(to, c.ctx.write_single_register(*a, *v)).await,
                WriteOp::Coils(a, v) => timeout(to, c.ctx.write_multiple_coils(*a, v)).await,
                WriteOp::Registers(a, v) => timeout(to, c.ctx.write_multiple_registers(*a, v)).await,
            };
            let elapsed = start.elapsed();
            let (ok, result, mark, broken) = match res {
                Err(_) => (false, "Timeout".to_string(), Mark::Error, true),
                Ok(Err(e)) => (false, e.to_string(), Mark::Error, true),
                Ok(Ok(Err(ex))) => (false, format!("Exception: {ex}"), Mark::Exception, false),
                Ok(Ok(Ok(()))) => (true, "OK".to_string(), Mark::Normal, false),
            };
            flush_traffic(c, &shared, &format!("ID {unit} WRITE {desc}"), elapsed, mark, &result);
            if broken {
                conn = None;
            }
            {
                let mut sh = shared.lock().unwrap();
                sh.requests += 1;
                if !broken {
                    sh.responses += 1;
                }
                sh.write_msg = Some(format!("{} 站號 {unit}: {desc} → {result}", if ok { "✓" } else { "✗" }));
                sh.write_log.push(WriteLog { time: now_str(), unit, desc, ok, result });
                if sh.write_log.len() > 500 {
                    sh.write_log.remove(0);
                }
            }
            repaint();
            continue;
        }

        // 4. 站號掃描(掃描中暫停一般輪詢)
        if let Some((req, unit)) = scan.as_mut() {
            let u = *unit;
            let (outcome, _) = read(c, &shared, offset, u, req.func, req.addr, 1, to).await;
            let mut sh = shared.lock().unwrap();
            sh.requests += 1;
            sh.scan.current = u;
            match outcome {
                ReadOutcome::Ok(v) => {
                    sh.responses += 1;
                    sh.scan.found.push((u, format!("OK, 值 = {}", v.first().copied().unwrap_or(0))));
                }
                // 有回 Exception 代表這個站號存在，只是該位址不可讀
                ReadOutcome::Exception(e) => {
                    sh.responses += 1;
                    sh.scan.found.push((u, e));
                }
                ReadOutcome::Transport(_) => conn = None,
            }
            if u >= req.to {
                sh.scan.running = false;
                scan = None;
            } else {
                *unit = u + 1;
            }
            drop(sh);
            repaint();
            sleep(Duration::from_millis(s.poll_delay_ms)).await;
            continue;
        }

        // 5. 一般輪詢: 每圈輪流讀一個視窗
        let jobs = shared.lock().unwrap().jobs.clone();
        if jobs.is_empty() {
            sleep(Duration::from_millis(50)).await;
            continue;
        }
        job_idx %= jobs.len();
        let job = jobs[job_idx].clone();
        job_idx += 1;

        let (outcome, elapsed) = read(c, &shared, offset, job.unit, job.func, job.addr, job.len, to).await;
        {
            let mut sh = shared.lock().unwrap();
            sh.requests += 1;
            let r = sh.results.entry(job.id).or_default();
            r.polls += 1;
            let mut responded = false;
            match outcome {
                ReadOutcome::Ok(v) => {
                    r.ok += 1;
                    r.values = v;
                    r.last_err = None;
                    responded = true;
                }
                ReadOutcome::Exception(e) => {
                    r.errors += 1;
                    r.last_err = Some(e);
                    responded = true;
                }
                ReadOutcome::Transport(e) => {
                    r.errors += 1;
                    r.last_err = Some(e);
                    // 逾時後串流可能殘留半個封包，重建連線最保險
                    conn = None;
                }
            }
            if responded {
                let ms = elapsed.as_secs_f64() * 1000.0;
                r.last_ms = ms;
                r.avg_ms = if r.avg_ms == 0.0 { ms } else { r.avg_ms * 0.8 + ms * 0.2 };
                sh.responses += 1;
            }
        }
        repaint();
        sleep(Duration::from_millis(s.poll_delay_ms)).await;
    }
}

pub fn list_serial_ports() -> Vec<String> {
    let mut v: Vec<String> = tokio_serial::available_ports()
        .map(|ps| ps.into_iter().map(|p| p.port_name).collect())
        .unwrap_or_default();
    // macOS 建議用 /dev/cu.*(tty.* 會等待 DCD)
    v.retain(|p| !p.starts_with("/dev/tty.") || cfg!(not(target_os = "macos")));
    v.sort();
    v
}
