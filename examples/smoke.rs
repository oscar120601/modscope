// 無 GUI 的通訊層煙霧測試：先啟動 127.0.0.1:5020 的模擬 slave，再 cargo run --example smoke
#[path = "../src/format.rs"] #[allow(dead_code)] mod format;
#[path = "../src/link.rs"] #[allow(dead_code)] mod link;
use link::*;
use std::time::Duration;
fn main() {
    let shared: SharedRef = Default::default();
    let (tx, rx) = std::sync::mpsc::channel();
    spawn(shared.clone(), rx, || {});
    let mut s = ConnSettings::default();
    s.host = "127.0.0.1".into(); s.tcp_port = 5020; s.timeout_ms = 300;
    tx.send(Cmd::Connect(s)).unwrap();
    shared.lock().unwrap().jobs = vec![Job { id: 1, unit: 1, func: format::Func::HoldingRegisters, addr: 0, len: 6 }];
    std::thread::sleep(Duration::from_millis(600));
    tx.send(Cmd::Write { unit: 1, op: WriteOp::Register(3, 999) }).unwrap();
    tx.send(Cmd::Write { unit: 1, op: WriteOp::Registers(10, vec![0x41A0, 0x8F5C]) }).unwrap();
    tx.send(Cmd::Write { unit: 1, op: WriteOp::Coils(0, vec![true, false, true]) }).unwrap();
    tx.send(Cmd::Write { unit: 9, op: WriteOp::Register(0, 1) }).unwrap(); // 不存在的站號 → Timeout
    std::thread::sleep(Duration::from_millis(1500));
    let sh = shared.lock().unwrap();
    let r = &sh.results[&1];
    println!("status={:?} polls={} ok={} err={} last={:.1}ms avg={:.1}ms", sh.status, r.polls, r.ok, r.errors, r.last_ms, r.avg_ms);
    println!("tx_bytes={} rx_bytes={} traffic_frames={}", sh.tx_bytes, sh.rx_bytes, sh.traffic.len());
    for w in &sh.write_log { println!("write: ID {} {} -> {} ({})", w.unit, w.desc, w.result, w.ok); }
    for f in sh.traffic.iter().take(2) { println!("frame: {} {:?} {:02X?} {}", f.time, f.dir, f.bytes, f.note); }
    for f in sh.traffic.iter().filter(|f| f.mark != Mark::Normal).take(2) { println!("bad: {:?} {:02X?} {}", f.dir, f.bytes, f.note); }
}
