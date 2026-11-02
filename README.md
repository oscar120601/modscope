# ModScope — macOS 用 Modbus Master 測試工具

操作方式參考 Radzio! Modbus Master Simulator（行為重寫，未使用原程式碼），以 Rust + egui 撰寫。

## 功能（v0.1）
- **連線**：Modbus RTU（USB 轉 RS-485）、Modbus TCP、Modbus RTU over TCP/IP
- **位址慣例**：Register address（從 0）／Register number（從 1）
- **多個裝置視窗**：各自設定 Device ID、FC01/02/03/04、Address、Length，輪流輪詢
- **顯示格式**：Decimal、Integer、Hex、Binary、32bit unsigned/integer/float、64bit float，皆含 sw.（word swap, CDAB）
- **ASCII 顯示**：1 char/reg、2 char/reg、2 char/reg sw
- **Alias 欄**：可輸入中文（電壓、電流…）
- **寫入**：雙擊數值 → Coil 用 FC05；單一暫存器 FC06；32/64 位元依目前格式自動用 FC16
- **Frame counters**：polls / valid responses / errors + Reset，並顯示最後一次錯誤原因（Timeout 或 Exception 名稱）
- **站號掃描器**（Tools → Slave ID scanner）：掃 1–247，有回 Exception 也算找到；雙擊結果直接開視窗
- **外觀**: View 選單可切換 Light (高對比，預設) / Dark
- **Tx/Rx 通訊監看**: 狀態列顯示 Tx/Rx 位元組與 KB/s；下方 Traffic monitor 逐筆列出原始封包 (hex)、回應時間，
  Exception 以橘色、Timeout/錯誤以紅色標示；可暫停、只看錯誤、複製、匯出 txt
- **寫入面板** (✏ Write): FC05 / FC06 / FC15 / FC16，可一次輸入多筆值 (逗號或空白分隔)，依選定格式自動轉換 (含 float/word swap)，送出前先預覽封包內容
- **寫入前確認**: 列出連線、站號、位址與數值再送出 (Tools → 寫入前確認 可開關)；寫入紀錄保留本次所有寫入結果
- **變動高亮**: 數值有變化的格子會以橘色閃爍並淡出
- **格子背景色**: 右鍵數值 → 綠/黃/紅/藍/灰，右鍵選單也可直接寫入、複製數值
- **設備範本**: 裝置視窗按「存成範本」；File → 從範本開新視窗；可匯出/匯入 `~/Documents/ModScope/templates.json` 分享給同事
- **CSV 記錄**: 每個裝置視窗可勾選 CSV 記錄並設定間隔，檔案存在 `~/Documents/ModScope/logs/` (含 BOM，Excel 可直接開中文)
- **回應時間**: Frame counters 顯示最近一次與平均回應時間 (ms)
- **自動保存**：連線設定、視窗配置與 Alias 關閉時自動保存，下次開啟還原

## 在 Mac 上編譯
```bash
# 第一次：安裝 Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

cd modscope
cargo run --release            # 直接執行
./scripts/build-mac-app.sh     # 打包成 target/ModScope.app
```

## 現場使用提醒
- USB 轉 RS-485 建議用 FTDI 晶片，macOS 免驅動；序列埠選 `/dev/cu.usbserial-XXXX`（不要用 `/dev/tty.*`）
- 掃描站號時可先把 Timeout 調到 200–300 ms，速度會快很多
- 逾時後程式會自動重建連線，避免殘留半個封包影響下一筆

## 測試
```bash
cargo test                     # 格式轉換單元測試
cargo run --example smoke      # 對 127.0.0.1:5020 的模擬 slave 做通訊煙霧測試
MODSCOPE_TCP=127.0.0.1:5020 cargo run   # 啟動時自動以 TCP 連線 (測試用)
MODSCOPE_SHOW=traffic,write cargo run    # 啟動時打開指定面板 (測試用)
```

## 待辦（下一版）
- 內建台達電表、泓格閘道器範本 (需要確認型號與暫存器表)
- 鮑率／格式自動偵測
