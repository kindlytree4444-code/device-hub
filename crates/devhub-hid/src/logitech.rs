//! Logicool / Logitech G マウス — HID++ 2.0 プロトコル (G HUB の置き換え)。
//!
//! 対象: PRO Wireless (有線 046d:c088 / LIGHTSPEED 受信機 046d:c539) ほか、
//! オンボードプロファイル (feature 0x8100) を持つ HID++ 2.0 マウス。
//! プロトコルの整理は `docs/LOGITECH_HIDPP.md`。
//!
//! # 経路
//!
//! - 設定チャネル: vendor collection `FF00:0002` (long report 0x11, 20B)。
//!   Windows では short (`FF00:0001`, report 0x10, 7B) と別の HID デバイスとして見える。
//! - 送信 `[0x11, dev, featIdx, func<<4 | swId, params(16)]`。応答は同じヘッダ + 16B。
//!   `dev` は有線 0xFF、LIGHTSPEED 受信機経由は 1 (ペア枠 1)。
//! - エラー: HID++ 2.0 `[0x11, dev, 0xFF, featIdx, func|sw, code]`、
//!   受信機からの HID++ 1.0 `[0x10, dev, 0x8F, …]` (short collection に届く)。
//! - feature は ID (0x2201 等) → index を Root (index 0) の getFeature で引く。
//!
//! # 設定の保存先
//!
//! G HUB の「オンボードメモリ」と同じく、マウス内のプロファイル (0x8100) を
//! 書き換える。DPI・ポーリング・ボタン・LED はマウス本体に残るので、
//! PC 側のソフトが落ちていても / 別の PC でも同じ設定で動く。
//! セクタ書き込みは CRC-CCITT 付き・読み戻し検証付き。

use devhub_core::backend::*;
use devhub_core::model::*;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ====================================================================== 定数

pub mod g913;

pub const REPORT_SHORT: u8 = 0x10;
pub const REPORT_LONG: u8 = 0x11;
pub const LONG_LEN: usize = 20;
/// 自分の要求を識別する software id (1..15、0 はデバイス発の通知)。
static SW_ID: std::sync::OnceLock<u8> = std::sync::OnceLock::new();

/// Use a distinct HID++ client ID when running the diagnostic alongside Device HUB.
pub fn use_diagnostic_software_id() -> HubResult<()> {
    if *SW_ID.get_or_init(|| 0x0C) != 0x0C {
        return Err(HubError::Invalid("HID++ client already initialized".into()));
    }
    Ok(())
}

fn software_id() -> u8 {
    *SW_ID.get_or_init(|| 0x0B)
}

/// 有線直結の device index。
pub const DEV_WIRED: u8 = 0xFF;
/// LIGHTSPEED 受信機のペア枠 1。
pub const DEV_RECEIVER_SLOT1: u8 = 0x01;

/// HID++ 2.0 feature ID。
pub mod feat {
    pub const ROOT: u16 = 0x0000;
    pub const FEATURE_SET: u16 = 0x0001;
    pub const FW_VERSION: u16 = 0x0003;
    pub const DEVICE_NAME: u16 = 0x0005;
    pub const BATTERY_STATUS: u16 = 0x1000;
    pub const BATTERY_VOLTAGE: u16 = 0x1001;
    pub const UNIFIED_BATTERY: u16 = 0x1004;
    pub const ADJUSTABLE_DPI: u16 = 0x2201;
    pub const REPORT_RATE: u16 = 0x8060;
    pub const ONBOARD_PROFILES: u16 = 0x8100;
    pub const RGB_EFFECTS: u16 = 0x8071;
}

// ====================================================================== リンク

/// 20B long report を送受信する口。実機は `hidapi::HidDevice`、テストはモック。
pub trait Link: Send {
    fn write(&mut self, data: &[u8]) -> HubResult<()>;
    /// 1 レポート読む (report ID 込み)。タイムアウトなら Ok(0)。
    fn read(&mut self, buf: &mut [u8], timeout_ms: i32) -> HubResult<usize>;
}

impl Link for hidapi::HidDevice {
    fn write(&mut self, data: &[u8]) -> HubResult<()> {
        hidapi::HidDevice::write(self, data)
            .map(|_| ())
            .map_err(|e| HubError::Io(format!("hid++ write: {e}")))
    }
    fn read(&mut self, buf: &mut [u8], timeout_ms: i32) -> HubResult<usize> {
        self.read_timeout(buf, timeout_ms)
            .map_err(|e| HubError::Io(format!("hid++ read: {e}")))
    }
}

// ====================================================================== HID++ 2.0 会話層

/// 要求 1 件の失敗理由 (ドライバ内部用 — 外へは HubError で出す)。
#[derive(Debug, Clone, PartialEq)]
pub enum PpError {
    /// 応答が来ない (無線ならマウスがスリープ / 圏外)。
    Timeout,
    /// HID++ 2.0 エラー応答 (code)。
    Feature(u8),
    /// HID++ 1.0 エラー (受信機: マウス未接続など)。
    Receiver(u8),
    /// 通信そのものの失敗。
    Io(String),
}

impl PpError {
    pub fn to_hub(&self, what: &str) -> HubError {
        match self {
            PpError::Timeout => {
                HubError::Protocol(format!("{what}: 応答がありません (スリープ中?)"))
            }
            PpError::Feature(c) => {
                HubError::Protocol(format!("{what}: HID++ エラー {c:#04x} ({})", err_name(*c)))
            }
            PpError::Receiver(c) => {
                HubError::Protocol(format!("{what}: 受信機エラー {c:#04x} (マウス未接続?)"))
            }
            PpError::Io(e) => HubError::Io(e.clone()),
        }
    }
}

fn err_name(c: u8) -> &'static str {
    match c {
        1 => "Unknown",
        2 => "InvalidArgument",
        3 => "OutOfRange",
        4 => "HWError",
        5 => "LogitechInternal",
        6 => "InvalidFeatureIndex",
        7 => "InvalidFunctionId",
        8 => "Busy",
        9 => "Unsupported",
        _ => "?",
    }
}

/// 受信 1 レポートの分類。
#[derive(Debug, PartialEq)]
pub enum Frame {
    Reply([u8; 16]),
    Error(PpError),
    /// 自分の要求と無関係 (通知など)。
    Other,
}

/// 受信レポートを、送った要求 (dev, feat_idx, func) と照合する。
pub fn classify(buf: &[u8], dev: u8, feat_idx: u8, func: u8) -> Frame {
    if buf.len() < 7 || buf[1] != dev {
        return Frame::Other;
    }
    let fs = (func << 4) | software_id();
    match buf[0] {
        REPORT_LONG | REPORT_SHORT => {
            if buf[2] == 0xFF && buf[3] == feat_idx && buf[4] == fs {
                return Frame::Error(PpError::Feature(buf[5]));
            }
            if buf[2] == 0x8F && buf[3] == feat_idx && buf[4] == fs {
                return Frame::Error(PpError::Receiver(buf[5]));
            }
            if buf[0] == REPORT_LONG && buf[2] == feat_idx && buf[3] == fs {
                let mut p = [0u8; 16];
                let n = (buf.len() - 4).min(16);
                p[..n].copy_from_slice(&buf[4..4 + n]);
                return Frame::Reply(p);
            }
            Frame::Other
        }
        _ => Frame::Other,
    }
}

/// HID++ 2.0 の会話 (feature index のキャッシュ付き)。
pub struct Hidpp<L: Link> {
    link: L,
    pub dev: u8,
    pub timeout_ms: u64,
    feats: HashMap<u16, Option<u8>>,
    /// 応答待ちの間に届いた、swId=0 のデバイス発通知 (featIdx, func)。
    pub events: Vec<(u8, u8)>,
    /// short collection の監視スレッドが「リンク切断」を見たら立つ。
    /// 立っている間は要求を送らず即 Timeout (スリープ中に毎回待たない)。
    pub link_down: Arc<AtomicBool>,
}

impl<L: Link> Hidpp<L> {
    pub fn new(link: L, dev: u8) -> Self {
        Self {
            link,
            dev,
            timeout_ms: if dev == DEV_WIRED { 600 } else { 1200 },
            feats: HashMap::new(),
            events: Vec::new(),
            link_down: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 要求を 1 件送り、対応する応答 16B を返す。
    pub fn request(&mut self, feat_idx: u8, func: u8, params: &[u8]) -> Result<[u8; 16], PpError> {
        if self.link_down.load(Ordering::Relaxed) {
            return Err(PpError::Timeout);
        }
        // 前の要求の取り残し (遅れて来た応答・通知) を捨てる。
        let mut buf = [0u8; 32];
        for _ in 0..16 {
            match self.link.read(&mut buf, 0) {
                Ok(n) if n > 0 => self.note_event(&buf[..n]),
                _ => break,
            }
        }
        let mut msg = [0u8; LONG_LEN];
        msg[0] = REPORT_LONG;
        msg[1] = self.dev;
        msg[2] = feat_idx;
        msg[3] = (func << 4) | software_id();
        let n = params.len().min(16);
        msg[4..4 + n].copy_from_slice(&params[..n]);
        self.link
            .write(&msg)
            .map_err(|e| PpError::Io(e.to_string()))?;
        let deadline = Instant::now() + Duration::from_millis(self.timeout_ms);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(PpError::Timeout);
            }
            let n = self
                .link
                .read(&mut buf, left.as_millis().clamp(1, i32::MAX as u128) as i32)
                .map_err(|e| PpError::Io(e.to_string()))?;
            if n == 0 {
                continue;
            }
            match classify(&buf[..n], self.dev, feat_idx, func) {
                Frame::Reply(p) => return Ok(p),
                Frame::Error(e) => return Err(e),
                Frame::Other => self.note_event(&buf[..n]),
            }
        }
    }

    fn note_event(&mut self, b: &[u8]) {
        // デバイス発の通知は swId = 0。
        if b.len() >= 4
            && b[0] == REPORT_LONG
            && b[1] == self.dev
            && b[3] & 0x0F == 0
            && self.events.len() < 64
        {
            self.events.push((b[2], b[3] >> 4));
        }
    }

    /// feature ID → index (無ければ None)。結果はキャッシュする。
    pub fn feature(&mut self, id: u16) -> Result<Option<u8>, PpError> {
        if id == feat::ROOT {
            return Ok(Some(0));
        }
        if let Some(v) = self.feats.get(&id) {
            return Ok(*v);
        }
        let r = self.request(0, 0, &id.to_be_bytes())?;
        let idx = (r[0] != 0).then_some(r[0]);
        self.feats.insert(id, idx);
        Ok(idx)
    }

    /// feature を持っていれば呼ぶ。持っていなければ `Feature(9)` (Unsupported)。
    pub fn call(&mut self, id: u16, func: u8, params: &[u8]) -> Result<[u8; 16], PpError> {
        match self.feature(id)? {
            Some(idx) => self.request(idx, func, params),
            None => Err(PpError::Feature(9)),
        }
    }

    /// Root ping — プロトコルバージョン (major, minor)。
    pub fn ping(&mut self) -> Result<(u8, u8), PpError> {
        let r = self.request(0, 1, &[0, 0, 0x5A])?;
        Ok((r[0], r[1]))
    }

    /// feature キャッシュを捨てる (再接続後はマウス側の表が変わりうる)。
    pub fn forget(&mut self) {
        self.feats.clear();
    }
}

// ====================================================================== 0x0003 / 0x0005

/// 0x0003 getFwInfo の応答 → "RQM 12.01.B0019" 形式。
pub fn fw_string(p: &[u8; 16]) -> String {
    let prefix: String = p[1..4]
        .iter()
        .filter(|c| c.is_ascii_graphic())
        .map(|&c| c as char)
        .collect();
    let build = u16::from_be_bytes([p[6], p[7]]);
    format!("{prefix} {:02X}.{:02X}.B{build:04X}", p[4], p[5])
        .trim()
        .to_string()
}

// ====================================================================== 電池

/// 0x1001 BatteryVoltage の mV → 残量 % (Solaar と同じ近似表を線形補間)。
pub fn voltage_to_percent(mv: u16) -> u8 {
    const T: [(i32, i32); 13] = [
        (4186, 100),
        (4067, 90),
        (3989, 80),
        (3922, 70),
        (3859, 60),
        (3811, 50),
        (3778, 40),
        (3751, 30),
        (3717, 20),
        (3671, 10),
        (3646, 5),
        (3579, 2),
        (3500, 0),
    ];
    let v = mv as i32;
    if v >= T[0].0 {
        return 100;
    }
    for w in T.windows(2) {
        let (hv, hp) = w[0];
        let (lv, lp) = w[1];
        if v >= lv {
            return (lp + (v - lv) * (hp - lp) / (hv - lv)) as u8;
        }
    }
    0
}

/// 0x1001 応答 `[mV hi, mV lo, flags]` → Battery。flags bit7 = 外部電源 (充電中)。
pub fn parse_battery_voltage(p: &[u8; 16]) -> Battery {
    let mv = u16::from_be_bytes([p[0], p[1]]);
    let flags = p[2];
    let charging = flags & 0x80 != 0;
    // 充電完了 (sub status 2 = full) は 100%。
    let full = charging && flags & 0x07 == 2;
    Battery {
        percent: Some(if full { 100 } else { voltage_to_percent(mv) }),
        charging: charging && !full,
    }
}

/// 0x1000 応答 `[level%, next%, status]`。status 1..4 = 充電系。
pub fn parse_battery_status(p: &[u8; 16]) -> Battery {
    Battery {
        percent: (p[0] > 0).then_some(p[0].min(100)),
        charging: matches!(p[2], 1 | 2 | 4),
    }
}

/// 0x1004 getStatus 応答 `[soc%, level, charging, external]`。
pub fn parse_unified_battery(p: &[u8; 16]) -> Battery {
    Battery {
        percent: Some(p[0].min(100)),
        charging: matches!(p[2], 1 | 2),
    }
}

// ====================================================================== 0x2201 DPI 範囲

/// getSensorDpiList の値列 (0 終端前まで) → (min, max, step)。
/// `0xE000 | step` は「前後の値の間を step 刻み」の意味。
pub fn dpi_range(words: &[u16]) -> Option<(u32, u32, u32)> {
    let vals: Vec<u16> = words.iter().copied().take_while(|&w| w != 0).collect();
    let plain: Vec<u32> = vals
        .iter()
        .filter(|&&w| w < 0xE000)
        .map(|&w| w as u32)
        .collect();
    let (&min, &max) = (plain.iter().min()?, plain.iter().max()?);
    let step = vals
        .iter()
        .find(|&&w| w >= 0xE000)
        .map(|&w| (w & 0x1FFF) as u32)
        .filter(|&s| s > 0)
        .unwrap_or_else(|| {
            plain
                .windows(2)
                .map(|w| w[1].abs_diff(w[0]))
                .filter(|&d| d > 0)
                .min()
                .unwrap_or(50)
        });
    Some((min, max, step))
}

// ====================================================================== 0x8100 オンボードプロファイル

/// getOnboardProfilesDescription。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OnboardDesc {
    pub memory_model: u8,
    pub profile_format: u8,
    pub macro_format: u8,
    pub profile_count: u8,
    pub profile_count_oob: u8,
    pub button_count: u8,
    pub sector_count: u8,
    pub sector_size: u16,
}

impl OnboardDesc {
    pub fn parse(p: &[u8; 16]) -> Self {
        Self {
            memory_model: p[0],
            profile_format: p[1],
            macro_format: p[2],
            profile_count: p[3],
            profile_count_oob: p[4],
            button_count: p[5],
            sector_count: p[6],
            sector_size: u16::from_be_bytes([p[7], p[8]]),
        }
    }
    /// このドライバが解釈できる形式か (libratbag と同じ G402 系レイアウト)。
    pub fn layout_known(&self) -> bool {
        (1..=5).contains(&self.profile_format)
            && self.sector_size as usize >= prof::MIN_SECTOR
            && self.sector_size <= 0x1000
    }
}

/// ユーザープロファイルのセクタ (ディレクトリ = 0x0000、プロファイル i = i+1)。
pub const SECTOR_DIRECTORY: u16 = 0x0000;
/// ROM (工場出荷) プロファイル i のセクタ。
pub const fn rom_sector(i: u8) -> u16 {
    0x0100 + i as u16 + 1
}
pub const fn user_sector(i: u8) -> u16 {
    i as u16 + 1
}

/// CRC-CCITT (init 0xFFFF, poly 0x1021)。セクタ末尾 2B に BE で入る。
pub fn crc_ccitt(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// セクタ末尾の CRC が正しいか。
pub fn sector_crc_ok(data: &[u8]) -> bool {
    if data.len() < 3 {
        return false;
    }
    let n = data.len() - 2;
    crc_ccitt(&data[..n]) == u16::from_be_bytes([data[n], data[n + 1]])
}

/// セクタ末尾に CRC を書く。
pub fn seal_sector(data: &mut [u8]) {
    let n = data.len() - 2;
    let c = crc_ccitt(&data[..n]).to_be_bytes();
    data[n] = c[0];
    data[n + 1] = c[1];
}

/// ディレクトリ (セクタ 0) のエントリ `[addr hi, addr lo, enabled, 0]`、0xFFFF 終端。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DirEntry {
    pub sector: u16,
    pub enabled: bool,
}

pub fn parse_directory(data: &[u8], max: usize) -> Vec<DirEntry> {
    let mut out = vec![];
    for c in data.chunks_exact(4).take(max) {
        let sector = u16::from_be_bytes([c[0], c[1]]);
        if sector == 0xFFFF {
            break;
        }
        out.push(DirEntry {
            sector,
            enabled: c[2] != 0,
        });
    }
    out
}

pub fn build_directory(entries: &[DirEntry], sector_size: usize) -> Vec<u8> {
    let mut d = vec![0xFFu8; sector_size];
    for (i, e) in entries.iter().enumerate() {
        let s = e.sector.to_be_bytes();
        d[4 * i..4 * i + 4].copy_from_slice(&[s[0], s[1], e.enabled as u8, 0]);
    }
    seal_sector(&mut d);
    d
}

/// プロファイルセクタのオフセット (libratbag `hidpp20_internal_profile` と同じ)。
pub mod prof {
    pub const REPORT_RATE: usize = 0;
    pub const DEFAULT_DPI: usize = 1;
    pub const SWITCHED_DPI: usize = 2;
    pub const DPI: usize = 3; // u16 LE × 5
    pub const DPI_COUNT: usize = 5;
    pub const BUTTONS: usize = 32; // 4B × 16
    pub const MAX_BUTTONS: usize = 16;
    pub const NAME: usize = 160; // UTF-16LE 24 文字
    pub const LEDS: usize = 208; // 11B × 2
    pub const LED_LEN: usize = 11;
    /// 上記がすべて収まる最小セクタ長 (+CRC)。
    pub const MIN_SECTOR: usize = LEDS + 2 * LED_LEN + 2;
}

/// ボタン割当の型 (`binding[0]`)。
pub mod bind {
    pub const MACRO: u8 = 0x00;
    pub const HID: u8 = 0x80;
    pub const SPECIAL: u8 = 0x90;
    pub const DISABLED: u8 = 0xFF;
    pub const HID_NONE: u8 = 0x00;
    pub const HID_MOUSE: u8 = 0x01;
    pub const HID_KEY: u8 = 0x02;
    pub const HID_CONSUMER: u8 = 0x03;
    pub const SP_WHEEL_LEFT: u8 = 0x01;
    pub const SP_WHEEL_RIGHT: u8 = 0x02;
    pub const SP_DPI_UP: u8 = 0x03;
    pub const SP_DPI_DOWN: u8 = 0x04;
    pub const SP_DPI_CYCLE: u8 = 0x05;
    pub const SP_DPI_SHIFT: u8 = 0x07;
    pub const SP_PROFILE_CYCLE: u8 = 0x0A;
}

/// LED モード (`led[0]`)。
pub mod led {
    pub const OFF: u8 = 0x00;
    pub const FIXED: u8 = 0x01;
    pub const CYCLE: u8 = 0x03;
    pub const BREATHING: u8 = 0x0A;
}

/// プロファイル 1 セクタ分の生データと、その解釈。
#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    pub raw: Vec<u8>,
    /// マウスボタンのビットマスクがリトルエンディアンか (左ボタンの割当で自動判定)。
    pub bits_le: bool,
}

impl Profile {
    pub fn new(raw: Vec<u8>) -> Self {
        let mut p = Self {
            raw,
            bits_le: false,
        };
        // Left click can be moved to any base button. Use all five standard mouse masks.
        if let Some(le) = p
            .raw
            .get(prof::BUTTONS..prof::BUTTONS + 4 * prof::MAX_BUTTONS)
            .and_then(|raw| {
                raw.chunks_exact(4).find_map(|b| match b {
                    [bind::HID, bind::HID_MOUSE, 1 | 2 | 4 | 8 | 16, 0] => Some(true),
                    [bind::HID, bind::HID_MOUSE, 0, 1 | 2 | 4 | 8 | 16] => Some(false),
                    _ => None,
                })
            })
        {
            p.bits_le = le;
        }
        p
    }

    pub fn report_rate_hz(&self) -> u32 {
        1000 / (self.raw[prof::REPORT_RATE].max(1) as u32)
    }
    pub fn set_report_rate_hz(&mut self, hz: u32) {
        self.raw[prof::REPORT_RATE] = (1000 / hz.clamp(125, 1000)) as u8;
    }

    pub fn dpi_raw(&self, i: usize) -> u16 {
        let o = prof::DPI + 2 * i;
        u16::from_le_bytes([self.raw[o], self.raw[o + 1]])
    }
    fn set_dpi_raw(&mut self, i: usize, v: u16) {
        let o = prof::DPI + 2 * i;
        self.raw[o..o + 2].copy_from_slice(&v.to_le_bytes());
    }

    /// 有効な DPI 枠 (raw index, dpi)。0 / 0xFFFF は未使用枠。
    pub fn stages(&self) -> Vec<(usize, u32)> {
        (0..prof::DPI_COUNT)
            .map(|i| (i, self.dpi_raw(i)))
            .filter(|&(_, v)| v != 0 && v != 0xFFFF)
            .map(|(i, v)| (i, v as u32))
            .collect()
    }

    /// 既定 (起動時) ステージの、`stages()` 内での位置。
    pub fn active_stage(&self) -> usize {
        let d = self.raw[prof::DEFAULT_DPI] as usize;
        self.stages().iter().position(|&(i, _)| i == d).unwrap_or(0)
    }

    /// DPI シフト (スナイパー) で使う DPI。
    pub fn shift_dpi(&self) -> Option<u32> {
        let s = self.raw[prof::SWITCHED_DPI] as usize;
        self.stages()
            .iter()
            .find(|&&(i, _)| i == s)
            .map(|&(_, v)| v)
    }

    /// ステージを詰めて書く (未使用枠は 0)。
    pub fn set_stages(&mut self, dpis: &[u32], active: usize) {
        let shift = self.shift_dpi();
        let n = dpis.len().clamp(1, prof::DPI_COUNT);
        for i in 0..prof::DPI_COUNT {
            let v = dpis.get(i).filter(|_| i < n).copied().unwrap_or(0);
            self.set_dpi_raw(i, v.min(0xFFFE) as u16);
        }
        self.raw[prof::DEFAULT_DPI] = active.min(n - 1) as u8;
        // シフト先は同じ DPI のステージを追従 (無ければ範囲内へ丸める)。
        let sw = shift
            .and_then(|d| dpis.iter().take(n).position(|&x| x == d))
            .unwrap_or((self.raw[prof::SWITCHED_DPI] as usize).min(n - 1));
        self.raw[prof::SWITCHED_DPI] = sw as u8;
    }

    pub fn button_raw(&self, i: usize) -> [u8; 4] {
        let o = prof::BUTTONS + 4 * i;
        [
            self.raw[o],
            self.raw[o + 1],
            self.raw[o + 2],
            self.raw[o + 3],
        ]
    }
    pub fn set_button_raw(&mut self, i: usize, b: [u8; 4]) {
        let o = prof::BUTTONS + 4 * i;
        self.raw[o..o + 4].copy_from_slice(&b);
    }

    pub fn button(&self, i: usize) -> ButtonAction {
        decode_button(self.button_raw(i), self.bits_le, self.shift_dpi())
    }

    pub fn set_button(&mut self, i: usize, a: &ButtonAction) -> HubResult<()> {
        if i >= 2 * prof::MAX_BUTTONS {
            return Err(HubError::Invalid("ボタン番号".into()));
        }
        let b = match a {
            ButtonAction::Dpi(DpiFn::Shift(d)) => {
                // シフト先 DPI のステージを指す。無ければエラー (ステージに追加してもらう)。
                let st = self.stages();
                let (ri, _) = st.iter().find(|&&(_, v)| v == *d).ok_or_else(|| {
                    HubError::Invalid(format!(
                        "DPI シフト {d} はステージに無い値です (先に DPI ステージへ追加してください)"
                    ))
                })?;
                self.raw[prof::SWITCHED_DPI] = *ri as u8;
                [bind::SPECIAL, bind::SP_DPI_SHIFT, 0, 0]
            }
            other => encode_button(other, self.bits_le)?,
        };
        self.set_button_raw(i, b);
        Ok(())
    }

    /// プロファイル名 (UTF-16LE、0 / 0xFFFF 終端)。
    pub fn name(&self) -> String {
        let units: Vec<u16> = self.raw[prof::NAME..prof::NAME + 48]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&u| u != 0 && u != 0xFFFF)
            .collect();
        String::from_utf16_lossy(&units)
    }
    pub fn set_name(&mut self, name: &str) {
        let mut units: Vec<u16> = name.encode_utf16().take(23).collect();
        units.resize(24, 0);
        for (i, u) in units.iter().enumerate() {
            let o = prof::NAME + 2 * i;
            self.raw[o..o + 2].copy_from_slice(&u.to_le_bytes());
        }
    }

    pub fn led(&self, i: usize) -> [u8; prof::LED_LEN] {
        let o = prof::LEDS + prof::LED_LEN * i;
        let mut l = [0u8; prof::LED_LEN];
        l.copy_from_slice(&self.raw[o..o + prof::LED_LEN]);
        l
    }
    pub fn set_led(&mut self, i: usize, l: [u8; prof::LED_LEN]) {
        let o = prof::LEDS + prof::LED_LEN * i;
        self.raw[o..o + prof::LED_LEN].copy_from_slice(&l);
    }

    pub fn crc_ok(&self) -> bool {
        sector_crc_ok(&self.raw)
    }
    pub fn seal(&mut self) {
        seal_sector(&mut self.raw);
    }
}

fn media_usage(m: MediaFn) -> u16 {
    match m {
        MediaFn::PlayPause => 0x00CD,
        MediaFn::Stop => 0x00B7,
        MediaFn::Next => 0x00B5,
        MediaFn::Prev => 0x00B6,
        MediaFn::VolumeUp => 0x00E9,
        MediaFn::VolumeDown => 0x00EA,
        MediaFn::Mute => 0x00E2,
        MediaFn::Calculator => 0x0192,
        MediaFn::MyComputer => 0x0194,
        MediaFn::Browser => 0x0196,
        MediaFn::Mail => 0x018A,
        MediaFn::BrightnessUp => 0x006F,
        MediaFn::BrightnessDown => 0x0070,
        MediaFn::Refresh => 0x0227,
    }
}

fn media_from_usage(u: u16) -> Option<MediaFn> {
    use MediaFn::*;
    [
        PlayPause,
        Stop,
        Next,
        Prev,
        VolumeUp,
        VolumeDown,
        Mute,
        Calculator,
        MyComputer,
        Browser,
        Mail,
        BrightnessUp,
        BrightnessDown,
        Refresh,
    ]
    .into_iter()
    .find(|&m| media_usage(m) == u)
}

/// 4B 割当 → ButtonAction。
pub fn decode_button(b: [u8; 4], bits_le: bool, shift_dpi: Option<u32>) -> ButtonAction {
    let raw = || ButtonAction::Unsupported {
        ty: b[0],
        params: b[1..].to_vec(),
    };
    match b[0] {
        bind::HID => match b[1] {
            bind::HID_NONE => ButtonAction::Disabled,
            bind::HID_MOUSE => {
                let bits = if bits_le {
                    u16::from_le_bytes([b[2], b[3]])
                } else {
                    u16::from_be_bytes([b[2], b[3]])
                };
                if bits.count_ones() != 1 {
                    return raw();
                }
                match bits.trailing_zeros() {
                    0 => ButtonAction::Mouse(MouseFn::Left),
                    1 => ButtonAction::Mouse(MouseFn::Right),
                    2 => ButtonAction::Mouse(MouseFn::Middle),
                    3 => ButtonAction::Mouse(MouseFn::Back),
                    4 => ButtonAction::Mouse(MouseFn::Forward),
                    _ => raw(),
                }
            }
            bind::HID_KEY => ButtonAction::Key(KeyStroke {
                mods: b[2],
                usage: b[3] as u16,
            }),
            bind::HID_CONSUMER => media_from_usage(u16::from_be_bytes([b[2], b[3]]))
                .map(ButtonAction::Media)
                .unwrap_or_else(raw),
            _ => raw(),
        },
        bind::SPECIAL => match b[1] {
            bind::SP_WHEEL_LEFT => ButtonAction::Mouse(MouseFn::ScrollLeft),
            bind::SP_WHEEL_RIGHT => ButtonAction::Mouse(MouseFn::ScrollRight),
            bind::SP_DPI_UP => ButtonAction::Dpi(DpiFn::Up),
            bind::SP_DPI_DOWN => ButtonAction::Dpi(DpiFn::Down),
            bind::SP_DPI_CYCLE => ButtonAction::Dpi(DpiFn::Cycle),
            bind::SP_DPI_SHIFT => match shift_dpi {
                Some(d) => ButtonAction::Dpi(DpiFn::Shift(d)),
                None => raw(),
            },
            bind::SP_PROFILE_CYCLE => ButtonAction::ProfileCycle,
            _ => raw(),
        },
        bind::DISABLED => ButtonAction::Disabled,
        _ => raw(),
    }
}

/// ButtonAction → 4B 割当 (DPI シフトは `Profile::set_button` 側で扱う)。
pub fn encode_button(a: &ButtonAction, bits_le: bool) -> HubResult<[u8; 4]> {
    let mouse = |bit: u16| {
        let v = 1u16 << bit;
        let b = if bits_le {
            v.to_le_bytes()
        } else {
            v.to_be_bytes()
        };
        Ok([bind::HID, bind::HID_MOUSE, b[0], b[1]])
    };
    match a {
        ButtonAction::Disabled => Ok([bind::HID, bind::HID_NONE, 0, 0]),
        ButtonAction::Mouse(MouseFn::Left) => mouse(0),
        ButtonAction::Mouse(MouseFn::Right) => mouse(1),
        ButtonAction::Mouse(MouseFn::Middle) => mouse(2),
        ButtonAction::Mouse(MouseFn::Back) => mouse(3),
        ButtonAction::Mouse(MouseFn::Forward) => mouse(4),
        ButtonAction::Mouse(MouseFn::ScrollLeft) => Ok([bind::SPECIAL, bind::SP_WHEEL_LEFT, 0, 0]),
        ButtonAction::Mouse(MouseFn::ScrollRight) => {
            Ok([bind::SPECIAL, bind::SP_WHEEL_RIGHT, 0, 0])
        }
        ButtonAction::Mouse(m) => Err(HubError::Invalid(format!(
            "{m:?} はオンボードプロファイルに割り当てられません"
        ))),
        ButtonAction::Dpi(DpiFn::Up) => Ok([bind::SPECIAL, bind::SP_DPI_UP, 0, 0]),
        ButtonAction::Dpi(DpiFn::Down) => Ok([bind::SPECIAL, bind::SP_DPI_DOWN, 0, 0]),
        ButtonAction::Dpi(DpiFn::Cycle) => Ok([bind::SPECIAL, bind::SP_DPI_CYCLE, 0, 0]),
        ButtonAction::Dpi(DpiFn::Shift(_)) => Err(HubError::Invalid(
            "DPI シフトはプロファイル経由で設定してください".into(),
        )),
        ButtonAction::Key(k) => {
            if k.usage > 0xFF {
                return Err(HubError::Invalid(format!(
                    "キーコード {:#x} は範囲外です",
                    k.usage
                )));
            }
            Ok([bind::HID, bind::HID_KEY, k.mods, k.usage as u8])
        }
        ButtonAction::Media(m) => {
            let u = media_usage(*m).to_be_bytes();
            Ok([bind::HID, bind::HID_CONSUMER, u[0], u[1]])
        }
        ButtonAction::ProfileCycle => Ok([bind::SPECIAL, bind::SP_PROFILE_CYCLE, 0, 0]),
        ButtonAction::Unsupported { ty, params } => {
            let mut b = [*ty, 0, 0, 0];
            for (i, v) in params.iter().take(3).enumerate() {
                b[1 + i] = *v;
            }
            Ok(b)
        }
        ButtonAction::Fire { .. }
        | ButtonAction::Macro { .. }
        | ButtonAction::PollingCycle
        | ButtonAction::ScrollModeToggle => Err(HubError::Unsupported("この割り当て")),
    }
}

/// 速度スライダー (1..=10) ↔ LED 周期 ms。
pub fn speed_to_period(s: u8) -> u16 {
    (11 - s.clamp(1, 10) as u16) * 1000
}
pub fn period_to_speed(ms: u16) -> u8 {
    (11 - (ms.clamp(1000, 10000) as u32 + 500) / 1000) as u8
}

/// LED 11B → LightingState。
pub fn decode_led(l: &[u8; prof::LED_LEN]) -> LightingState {
    let mut s = LightingState {
        on: l[0] != led::OFF,
        effect: led::FIXED,
        color: Rgb::new(l[1], l[2], l[3]),
        brightness: 100,
        speed: 5,
        direction: 0,
        per_key: BTreeMap::new(),
        zone_on: vec![],
        palette: vec![],
        palette_sel: 0,
        color_correction: None,
    };
    match l[0] {
        led::FIXED => s.effect = led::FIXED,
        led::CYCLE => {
            s.effect = led::CYCLE;
            s.speed = period_to_speed(u16::from_be_bytes([l[6], l[7]]));
            s.brightness = if l[8] == 0 { 100 } else { l[8].min(100) };
        }
        led::BREATHING => {
            s.effect = led::BREATHING;
            s.speed = period_to_speed(u16::from_be_bytes([l[4], l[5]]));
            s.brightness = if l[7] == 0 { 100 } else { l[7].min(100) };
        }
        _ => {}
    }
    s
}

/// LightingState → LED 11B。
pub fn encode_led(s: &LightingState) -> [u8; prof::LED_LEN] {
    let mut l = [0u8; prof::LED_LEN];
    if !s.on {
        return l;
    }
    let br = s.brightness.clamp(1, 100);
    match s.effect {
        led::CYCLE => {
            l[0] = led::CYCLE;
            l[6..8].copy_from_slice(&speed_to_period(s.speed).to_be_bytes());
            l[8] = br;
        }
        led::BREATHING => {
            l[0] = led::BREATHING;
            l[1..4].copy_from_slice(&[s.color.r, s.color.g, s.color.b]);
            l[4..6].copy_from_slice(&speed_to_period(s.speed).to_be_bytes());
            l[6] = 0; // waveform: 既定
            l[7] = br;
        }
        _ => {
            l[0] = led::FIXED;
            l[1..4].copy_from_slice(&[s.color.r, s.color.g, s.color.b]);
        }
    }
    l
}

// ====================================================================== メモリ操作

/// オンボードメモリ操作 (0x8100 の func 5..8)。
pub struct Onboard<'a, L: Link> {
    pub pp: &'a mut Hidpp<L>,
    pub idx: u8,
    pub sector_size: usize,
}

impl<L: Link> Onboard<'_, L> {
    /// セクタ全体を 16B ずつ読む (末尾が 16 の倍数でなければ最後は size-16 から)。
    pub fn read_sector(&mut self, sector: u16) -> Result<Vec<u8>, PpError> {
        let n = self.sector_size;
        let mut out = vec![0u8; n];
        let mut off = 0usize;
        while off < n {
            let at = off.min(n.saturating_sub(16));
            let s = sector.to_be_bytes();
            let o = (at as u16).to_be_bytes();
            let r = self.pp.request(self.idx, 5, &[s[0], s[1], o[0], o[1]])?;
            let take = (n - at).min(16);
            out[at..at + take].copy_from_slice(&r[..take]);
            off = at + 16;
        }
        Ok(out)
    }

    /// セクタ全体を書く (addrWrite → 16B × k → writeEnd)。
    pub fn write_sector(&mut self, sector: u16, data: &[u8]) -> Result<(), PpError> {
        let s = sector.to_be_bytes();
        let len = (data.len() as u16).to_be_bytes();
        self.pp
            .request(self.idx, 6, &[s[0], s[1], 0, 0, len[0], len[1]])?;
        for chunk in data.chunks(16) {
            let mut c = [0u8; 16];
            c[..chunk.len()].copy_from_slice(chunk);
            self.pp.request(self.idx, 7, &c)?;
        }
        self.pp.request(self.idx, 8, &[])?;
        Ok(())
    }
}

// ====================================================================== ドライバ

/// 機能の検出結果 (開いた時点)。
#[derive(Clone, Debug, Default)]
struct Feats {
    g502x: bool,
    lighting: bool,
    onboard: Option<OnboardDesc>,
    dpi: Option<(u32, u32, u32)>,
    battery: Option<u16>,
    report_rates: Vec<u32>,
}

pub struct LogiDriver {
    pp: Mutex<Hidpp<hidapi::HidDevice>>,
    info: Mutex<DeviceInfo>,
    feats: Mutex<Feats>,
    caps: Mutex<Capabilities>,
    events: Option<Arc<dyn Fn(HubEvent) + Send + Sync>>,
    /// RealHub と共有: DeviceId → (model, 実機名)。受信機経由でも一覧に実機名を出す。
    detected: Arc<Mutex<HashMap<String, (String, String)>>>,
    stop: Arc<AtomicBool>,
}

/// 0x8071 RgbEffects の LED 制御権 (func 5, Solaar の RGBControl と同じ)。
/// 応答 `[_, mode]`: 0 = 本体ファーム (オンボードプロファイルの LED が効く)、1 = ソフト。
/// G HUB はソフト制御にしたまま終わることがあり、そのままだとプロファイルの LED を
/// 書いても光り方が変わらない (G703 で LED が反映されなかった件の対策) — 本体へ戻す。
fn release_led_control<L: Link>(pp: &mut Hidpp<L>) -> Option<u8> {
    let idx = pp.feature(feat::RGB_EFFECTS).ok()??;
    let r = pp.request(idx, 5, &[]).ok()?;
    if r[1] != 0 {
        pp.request(idx, 5, &[1, 0]).ok()?;
    }
    Some(r[1])
}

pub fn open(
    dev: hidapi::HidDevice,
    info: DeviceInfo,
    events: Option<Arc<dyn Fn(HubEvent) + Send + Sync>>,
    detected: Arc<Mutex<HashMap<String, (String, String)>>>,
) -> HubResult<LogiDriver> {
    dev.set_blocking_mode(false).ok();
    let index = if info.transport == Transport::Wired {
        DEV_WIRED
    } else {
        DEV_RECEIVER_SLOT1
    };
    let pp = Hidpp::new(dev, index);
    let stop = Arc::new(AtomicBool::new(false));
    if info.transport != Transport::Wired {
        spawn_link_watch(&info, pp.link_down.clone(), events.clone(), stop.clone());
    }
    let d = LogiDriver {
        pp: Mutex::new(pp),
        info: Mutex::new(info),
        feats: Mutex::new(Feats::default()),
        caps: Mutex::new(Capabilities::empty()),
        events,
        detected,
        stop,
    };
    d.detect();
    Ok(d)
}

impl Drop for LogiDriver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// 受信機の short collection (`FF00:0001`) を読み、HID++ 1.0 の接続通知
/// `[0x10, 1, 0x41, proto, flags, …]` (flags bit6 = リンク無し) と 0x8F エラーで
/// マウスの在/不在を追う。
fn spawn_link_watch(
    info: &DeviceInfo,
    down: Arc<AtomicBool>,
    events: Option<Arc<dyn Fn(HubEvent) + Send + Sync>>,
    stop: Arc<AtomicBool>,
) {
    let (vid, pid, id) = (info.vid, info.pid, info.id.clone());
    std::thread::spawn(move || {
        let Ok(api) = hidapi::HidApi::new() else {
            return;
        };
        let Some(d) = api
            .device_list()
            .find(|d| {
                d.vendor_id() == vid
                    && d.product_id() == pid
                    && d.usage_page() == 0xFF00
                    && d.usage() == 0x0001
            })
            .and_then(|i| i.open_device(&api).ok())
        else {
            return;
        };
        let mut buf = [0u8; 32];
        while !stop.load(Ordering::Relaxed) {
            let Ok(n) = d.read_timeout(&mut buf, 250) else {
                return;
            };
            if n < 7 || buf[0] != REPORT_SHORT || buf[1] != DEV_RECEIVER_SLOT1 {
                continue;
            }
            if buf[2] == 0x41 {
                let lost = buf[4] & 0x40 != 0;
                let was = down.swap(lost, Ordering::Relaxed);
                if was != lost {
                    if let Some(ev) = &events {
                        // UI に再読み込みさせる (起きたら全設定・電池を取り直す)。
                        ev(HubEvent::StateChanged(id.clone()));
                    }
                }
            }
        }
    });
}

fn button_names(n: usize) -> Vec<ButtonSlot> {
    const NAMES: [&str; 6] = [
        "左ボタン",
        "右ボタン",
        "ホイールクリック",
        "サイド (後ろ)",
        "サイド (前)",
        "DPI ボタン",
    ];
    (0..n)
        .map(|i| ButtonSlot {
            name: NAMES
                .get(i)
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("ボタン {}", i + 1)),
        })
        .collect()
}

impl LogiDriver {
    fn pp(&self) -> std::sync::MutexGuard<'_, Hidpp<hidapi::HidDevice>> {
        self.pp.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn set_asleep(&self, asleep: bool) {
        let mut i = self.info.lock().unwrap_or_else(|p| p.into_inner());
        if i.asleep != asleep {
            i.asleep = asleep;
            if let Some(ev) = &self.events {
                ev(HubEvent::Info(i.id.clone(), Box::new(i.clone())));
            }
        }
    }

    /// PpError を HubError にしつつ、無線の無応答はスリープ扱いにする。
    fn fail(&self, what: &str, e: PpError) -> HubError {
        let wireless = self
            .info
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .transport
            != Transport::Wired;
        if wireless && matches!(e, PpError::Timeout | PpError::Receiver(_)) {
            self.set_asleep(true);
        }
        e.to_hub(what)
    }

    /// 機能の検出 → 名前・FW・caps を確定する。マウスが寝ていたら後で再試行。
    fn detect(&self) {
        let mut pp = self.pp();
        pp.forget();
        if pp.ping().is_err() {
            drop(pp);
            self.set_asleep(true);
            return;
        }
        let mut f = Feats::default();
        if pp.call(feat::DEVICE_NAME, 2, &[]).map(|r| r[0]) != Ok(3) {
            return;
        }
        if let Ok(Some(idx)) = pp.feature(feat::ONBOARD_PROFILES) {
            if let Ok(p) = pp.request(idx, 0, &[]) {
                let desc = OnboardDesc::parse(&p);
                f.onboard = Some(desc);
            }
        }
        if let Ok(Some(idx)) = pp.feature(feat::ADJUSTABLE_DPI) {
            let mut words = vec![];
            if let Ok(p) = pp.request(idx, 1, &[0]) {
                words.extend(
                    p[1..]
                        .chunks_exact(2)
                        .map(|c| u16::from_be_bytes([c[0], c[1]])),
                );
                // 16B に収まらない一覧は続きがある (sensor, offset) — 2 回目まで読む。
                if !words.contains(&0) {
                    if let Ok(p2) = pp.request(idx, 1, &[0, 1]) {
                        words.extend(
                            p2[1..]
                                .chunks_exact(2)
                                .map(|c| u16::from_be_bytes([c[0], c[1]])),
                        );
                    }
                }
            }
            f.dpi = dpi_range(&words);
        }
        for b in [
            feat::UNIFIED_BATTERY,
            feat::BATTERY_STATUS,
            feat::BATTERY_VOLTAGE,
        ] {
            if let Ok(Some(_)) = pp.feature(b) {
                f.battery = Some(b);
                break;
            }
        }
        if let Ok(Some(idx)) = pp.feature(feat::REPORT_RATE) {
            if let Ok(p) = pp.request(idx, 0, &[]) {
                // bit n = (n+1) ms
                f.report_rates = (0..8)
                    .filter(|b| p[0] & (1 << b) != 0)
                    .map(|b| 1000 / (b + 1))
                    .filter(|hz| [125, 250, 500, 1000].contains(hz))
                    .collect();
            }
        }
        if f.report_rates.is_empty() {
            f.report_rates = vec![125, 250, 500, 1000];
        }
        f.report_rates.sort_unstable();

        // 名前 (0x0005) と FW (0x0003)。
        let name = read_name(&mut pp);
        f.g502x = name
            .as_deref()
            .is_some_and(|n| n.starts_with("G502 X") && !n.contains("PLUS"))
            && f.onboard.is_some_and(|d| {
                d.memory_model == 1 && d.profile_format == 3 && d.button_count == 11
            });
        f.lighting = !f.g502x
            && name
                .as_deref()
                .is_some_and(|n| !n.starts_with("G502 X") || n.contains("PLUS"))
            && (pp.feature(feat::RGB_EFFECTS).ok().flatten().is_some()
                || pp.feature(0x8070).ok().flatten().is_some());
        let fw = read_fw(&mut pp);
        drop(pp);

        let caps = build_caps(&f);
        *self.caps.lock().unwrap_or_else(|p| p.into_inner()) = caps;
        *self.feats.lock().unwrap_or_else(|p| p.into_inner()) = f.clone();
        let mut i = self.info.lock().unwrap_or_else(|p| p.into_inner());
        if f.g502x {
            i.model = "g502-x-lightspeed".into();
        }
        if let Some(n) = name.filter(|n| !n.is_empty()) {
            i.name = n;
        }
        if fw.is_some() {
            i.firmware = fw;
        }
        i.supported = f.onboard.is_some_and(|d| d.layout_known());
        i.asleep = false;
        self.detected
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(i.id.0.clone(), (i.model.clone(), i.name.clone()));
        let snapshot = i.clone();
        drop(i);
        if let Some(ev) = &self.events {
            ev(HubEvent::Info(snapshot.id.clone(), Box::new(snapshot)));
        }
    }

    fn onboard(&self) -> HubResult<OnboardDesc> {
        if self
            .feats
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .onboard
            .is_none()
        {
            self.detect();
        }
        let d = self
            .feats
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .onboard
            .ok_or(HubError::Unsupported("オンボードプロファイル (0x8100)"))?;
        if !d.layout_known() {
            return Err(HubError::Unsupported("このプロファイル形式"));
        }
        Ok(d)
    }

    fn require_onboard_mode(&self) -> HubResult<()> {
        self.onboard()?;
        let mode = self
            .pp()
            .call(feat::ONBOARD_PROFILES, 2, &[])
            .map_err(|e| e.to_hub("オンボードモード"))?;
        if mode[0] != 1 {
            return Err(HubError::Invalid(
                "一般設定でオンボードモードを有効にしてください".into(),
            ));
        }
        Ok(())
    }

    fn with_onboard<T>(
        &self,
        what: &str,
        f: impl FnOnce(&mut Onboard<'_, hidapi::HidDevice>, &OnboardDesc) -> Result<T, PpError>,
    ) -> HubResult<T> {
        let desc = self.onboard()?;
        let mut pp = self.pp();
        let idx = match pp.feature(feat::ONBOARD_PROFILES) {
            Ok(Some(i)) => i,
            Ok(None) => return Err(HubError::Unsupported("オンボードプロファイル (0x8100)")),
            Err(e) => {
                drop(pp);
                return Err(self.fail(what, e));
            }
        };
        let mut ob = Onboard {
            pp: &mut *pp,
            idx,
            sector_size: desc.sector_size as usize,
        };
        let r = f(&mut ob, &desc);
        drop(pp);
        match r {
            Ok(v) => {
                self.set_asleep(false);
                Ok(v)
            }
            Err(e) => Err(self.fail(what, e)),
        }
    }

    /// 現在のプロファイル番号 (0 始まり) と、ディレクトリ・現在プロファイルの中身。
    fn load(&self) -> HubResult<Loaded> {
        self.with_onboard("プロファイル読み込み", load_profiles)
    }

    /// 現在プロファイルを書き換えて保存する。
    fn edit(&self, f: impl FnOnce(&mut Profile) -> HubResult<()>) -> HubResult<()> {
        let mut l = self.load()?;
        let before = l.current.raw.clone();
        f(&mut l.current)?;
        if l.current.raw == before && l.in_user {
            return Ok(());
        }
        self.with_onboard("プロファイル書き込み", move |ob, desc| {
            commit(ob, desc, &l)
        })
    }
}

/// 読み込んだプロファイル状態。
#[derive(Clone, Debug)]
pub struct Loaded {
    /// 現在のプロファイル (0 始まり)。
    pub active: u8,
    /// ディレクトリ (無効なら ROM 既定から作った仮のもの)。
    pub dir: Vec<DirEntry>,
    pub dir_valid: bool,
    /// 現在プロファイルの中身。
    pub current: Profile,
    /// current がユーザー領域 (書き換え可能) から来たか。
    pub in_user: bool,
    /// 全プロファイルの名前。
    pub names: Vec<String>,
}

fn read_profile<L: Link>(
    ob: &mut Onboard<'_, L>,
    dir: &[DirEntry],
    i: u8,
) -> Result<(Profile, bool), PpError> {
    // ユーザー領域が読めない場合は工場設定を編集元にしない。
    if let Some(e) = dir.get(i as usize).filter(|e| e.sector < 0x0100) {
        for _ in 0..2 {
            let raw = ob.read_sector(e.sector)?;
            if sector_crc_ok(&raw) && (1..=8).contains(&raw[prof::REPORT_RATE]) {
                return Ok((Profile::new(raw), true));
            }
        }
        return Err(PpError::Io("ユーザープロファイルを正しく読み取れません".into()));
    }
    let raw = ob.read_sector(rom_sector(i))?;
    if !(1..=8).contains(&raw[prof::REPORT_RATE]) {
        return Err(PpError::Io("ROM プロファイルのレートが範囲外です".into()));
    }
    Ok((Profile::new(raw), false))
}

pub fn load_profiles<L: Link>(
    ob: &mut Onboard<'_, L>,
    desc: &OnboardDesc,
) -> Result<Loaded, PpError> {
    let count = desc.profile_count.max(1);
    let raw_dir = ob.read_sector(SECTOR_DIRECTORY)?;
    let dir_valid = sector_crc_ok(&raw_dir);
    let mut dir = if dir_valid {
        parse_directory(&raw_dir, count as usize)
    } else {
        vec![]
    };
    if dir.is_empty() {
        dir = (0..count)
            .map(|i| DirEntry {
                sector: rom_sector(i),
                enabled: i == 0,
            })
            .collect();
    }
    // getCurrentProfile → [hi, lo] (ユーザー i+1 / ROM 0x0100+i+1)。
    let cur = ob.pp.request(ob.idx, 4, &[])?;
    let active = (cur[1].max(1) - 1).min(count - 1);
    let (current, in_user) = read_profile(ob, &dir, active)?;
    let mut names = vec![String::new(); count as usize];
    names[active as usize] = current.name();
    Ok(Loaded {
        active,
        dir,
        dir_valid,
        current,
        in_user,
        names,
    })
}

/// 現在プロファイルをユーザー領域へ書き、ディレクトリを更新して再読込させる。
///
/// ディレクトリが ROM を指していたプロファイルは、ユーザー領域へ ROM の中身を
/// 写してから付け替える (指し先が空セクタになるのを防ぐ)。
pub fn commit<L: Link>(
    ob: &mut Onboard<'_, L>,
    desc: &OnboardDesc,
    l: &Loaded,
) -> Result<(), PpError> {
    let size = desc.sector_size as usize;
    let mut dir = l.dir.clone();
    for (i, e) in dir.iter_mut().enumerate() {
        let i = i as u8;
        let target = user_sector(i);
        if i == l.active {
            let mut p = l.current.clone();
            p.raw.resize(size, 0xFF);
            p.seal();
            ob.write_sector(target, &p.raw)?;
            // 読み戻し検証 (フラッシュ書き込みの取りこぼし対策)。
            let back = ob.read_sector(target)?;
            if back != p.raw {
                return Err(PpError::Io("書き込み後の読み戻しが一致しません".into()));
            }
            e.sector = target;
            e.enabled = true;
        } else if e.sector >= 0x0100 {
            let mut raw = ob.read_sector(e.sector)?;
            if !sector_crc_ok(&raw) {
                continue;
            }
            raw.resize(size, 0xFF);
            ob.write_sector(target, &raw)?;
            e.sector = target;
        }
    }
    let new_dir = build_directory(&dir, size);
    let need_dir = !l.dir_valid || dir != l.dir;
    if need_dir {
        ob.write_sector(SECTOR_DIRECTORY, &new_dir)?;
    }
    // 再読込: 現在プロファイルを選び直すとマウスが新しい中身で動く。
    ob.pp.request(ob.idx, 3, &[0, l.active + 1])?;
    Ok(())
}

fn read_name<L: Link>(pp: &mut Hidpp<L>) -> Option<String> {
    let idx = pp.feature(feat::DEVICE_NAME).ok()??;
    let len = pp.request(idx, 0, &[]).ok()?[0] as usize;
    let mut s = Vec::new();
    while s.len() < len.min(64) {
        let r = pp.request(idx, 1, &[s.len() as u8]).ok()?;
        let take = (len - s.len()).min(16);
        s.extend_from_slice(&r[..take]);
    }
    let end = s.iter().position(|&b| b == 0).unwrap_or(s.len());
    Some(String::from_utf8_lossy(&s[..end]).trim().to_string())
}

fn read_fw<L: Link>(pp: &mut Hidpp<L>) -> Option<String> {
    let idx = pp.feature(feat::FW_VERSION).ok()??;
    let n = pp.request(idx, 0, &[]).ok()?[0];
    for e in 0..n.min(8) {
        let r = pp.request(idx, 1, &[e]).ok()?;
        // type 0 = メインアプリケーション。
        if r[0] == 0 {
            return Some(fw_string(&r));
        }
    }
    None
}

const G502X_BUTTONS: [&str; 11] = [
    "左クリック",
    "右クリック",
    "中央クリック",
    "戻る (G4)",
    "DPI シフト (G6)",
    "進む (G5)",
    "左チルト (G10)",
    "右チルト (G11)",
    "プロファイル (G9)",
    "DPI 上 (G8)",
    "DPI 下 (G7)",
];

fn physical_slot(slot: usize, base: usize, shifted: bool) -> HubResult<usize> {
    if slot < base {
        return Ok(slot);
    }
    if shifted && slot - base < base {
        return Ok(prof::MAX_BUTTONS + slot - base);
    }
    Err(HubError::Invalid("ボタン番号".into()))
}

fn build_caps(f: &Feats) -> Capabilities {
    let mut c = Capabilities::empty();
    let Some(desc) = f.onboard.filter(|d| d.layout_known()) else {
        return c;
    };
    let (dmin, dmax, dstep) = f.dpi.unwrap_or((100, 25600, 50));
    let nbtn = (desc.button_count as usize).clamp(1, prof::MAX_BUTTONS);
    c.settings.push(
        SettingSpec::toggle("onboard_mode", section::SYSTEM, "オンボードモード")
            .help("本体に保存した割り当てを使います。オフにするとホスト側の制御に戻ります。"),
    );
    c.mouse = Some(MouseCaps {
        dpi_min: dmin,
        dpi_max: dmax,
        dpi_step: dstep,
        min_stages: 1,
        max_stages: prof::DPI_COUNT,
        xy_split: false,
        stage_colors: false,
        stage_toggle: false,
        buttons: if f.g502x {
            G502X_BUTTONS
                .iter()
                .map(|n| ButtonSlot { name: (*n).into() })
                .chain(G502X_BUTTONS.iter().map(|n| ButtonSlot {
                    name: format!("G-Shift · {n}"),
                }))
                .collect()
        } else {
            button_names(nbtn)
        },
        button_kinds: vec![
            ButtonActionKind::Mouse,
            ButtonActionKind::Dpi,
            ButtonActionKind::Key,
            ButtonActionKind::Media,
            ButtonActionKind::ProfileCycle,
            ButtonActionKind::Disabled,
        ],
        require_left_click: true,
        macros: None,
        live_buttons: vec![],
        adaptive_dpi: None,
    });
    c.settings.push(
        SettingSpec::choice(
            "polling",
            section::PERFORMANCE,
            "ポーリングレート",
            f.report_rates
                .iter()
                .map(|&hz| ChoiceOpt::new(hz as i64, format!("{hz} Hz")))
                .collect(),
        )
        .help("オンボードプロファイルに保存されます"),
    );
    if desc.profile_count > 1 {
        c.profiles = Some(ProfileCaps {
            count: desc.profile_count,
            renamable: true,
        });
    }
    if f.lighting {
        c.lighting.push(LightArea {
            id: 0,
            name: "LED".into(),
            effects: vec![
                LightEffect::new(led::FIXED, "固定").color().still(),
                LightEffect::new(led::BREATHING, "呼吸").color(),
                LightEffect::new(led::CYCLE, "カラーサイクル"),
            ],
            brightness_max: 100,
            speed_max: 10,
            directions: vec![],
            switchable: true,
            zones: vec![],
            per_key: false,
            direct_drive: false,
            live_readback: false,
        });
    }
    c.actions.push(
        ActionSpec::new(
            "restore_profile",
            section::SYSTEM,
            "このプロファイルを出荷時に戻す",
        )
        .danger("現在のオンボードプロファイルを工場出荷時の内容で上書きします。よろしいですか?"),
    );
    c.actions.push(
        ActionSpec::new("redetect", section::SYSTEM, "マウスを再検出")
            .help("受信機の先のマウスを入れ替えたときなど"),
    );
    c.battery = f.battery.is_some();
    c
}

impl DeviceDriver for LogiDriver {
    fn info(&self) -> DeviceInfo {
        if self
            .feats
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .onboard
            .is_none()
        {
            self.detect();
        }
        let mut i = self.info.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if let Ok(b) = self.battery() {
            i.battery = Some(b);
        }
        i
    }

    fn caps(&self) -> Capabilities {
        self.caps.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn read(&self) -> HubResult<DeviceState> {
        let l = self.load()?;
        let p = &l.current;
        // いま使っている DPI 枠 (本体の DPI ボタンで変わる)。保存値 (起動時の枠) とは別なので、
        // 読めればこちらを表示する (G703 実機で default=1 / current=2 を確認)。
        let live_stage = self
            .with_onboard("DPI 枠", |ob, _| ob.pp.request(ob.idx, 0x0B, &[]))
            .ok()
            .and_then(|r| p.stages().iter().position(|&(i, _)| i == r[0] as usize));
        let mut settings = BTreeMap::new();
        if let Ok(p) = self.pp().call(feat::ONBOARD_PROFILES, 2, &[]) {
            settings.insert("onboard_mode".into(), SettingValue::Bool(p[0] == 1));
        }
        settings.insert(
            "polling".into(),
            SettingValue::Choice(p.report_rate_hz() as i64),
        );
        let base = (self.onboard()?.button_count as usize).clamp(1, prof::MAX_BUTTONS);
        let shifted = self.feats.lock().unwrap_or_else(|p| p.into_inner()).g502x;
        let nbtn = if shifted { base * 2 } else { base };
        let stages: Vec<DpiStage> = p
            .stages()
            .into_iter()
            .map(|(_, x)| DpiStage {
                x,
                y: None,
                color: Rgb::default(),
                enabled: true,
            })
            .collect();
        let mut lighting = BTreeMap::new();
        if !self.caps().lighting.is_empty() {
            lighting.insert(0u8, decode_led(&p.led(0)));
        }
        Ok(DeviceState {
            profile: ProfileState {
                active: l.active,
                names: l.names.clone(),
            },
            settings,
            mouse: Some(MouseState {
                active_stage: live_stage.unwrap_or_else(|| p.active_stage()),
                stages,
                buttons: (0..nbtn)
                    .map(|i| physical_slot(i, base, shifted).map(|slot| p.button(slot)))
                    .collect::<HubResult<_>>()?,
                macros: vec![],
                adaptive: None,
            }),
            keyboard: None,
            lighting,
            peq: None,
        })
    }

    fn apply(&self, change: &Change) -> HubResult<()> {
        if let Change::Setting {
            id,
            value: SettingValue::Bool(on),
        } = change
        {
            if id == "onboard_mode" {
                self.onboard()?;
                self.pp()
                    .call(feat::ONBOARD_PROFILES, 1, &[if *on { 1 } else { 2 }])
                    .map_err(|e| e.to_hub("オンボードモード"))?;
                return Ok(());
            }
        }
        self.require_onboard_mode()?;
        let (dmin, dmax) = self
            .caps()
            .mouse
            .as_ref()
            .map(|m| (m.dpi_min, m.dpi_max))
            .unwrap_or((100, 25600));
        match change {
            Change::Setting { id, value } if id == "polling" => {
                let hz = value.as_choice() as u32;
                if ![125, 250, 500, 1000].contains(&hz) {
                    return Err(HubError::Invalid(format!("{hz} Hz")));
                }
                self.edit(|p| {
                    p.set_report_rate_hz(hz);
                    Ok(())
                })
            }
            Change::DpiStages { stages, active } => {
                if stages.is_empty() || stages.len() > prof::DPI_COUNT {
                    return Err(HubError::Invalid("DPI ステージは 1〜5 個".into()));
                }
                let dpis: Vec<u32> = stages.iter().map(|s| s.x.clamp(dmin, dmax)).collect();
                self.edit(|p| {
                    p.set_stages(&dpis, *active);
                    Ok(())
                })
            }
            Change::ActiveStage(a) => self.edit(|p| {
                let n = p.stages().len().max(1);
                let dpis: Vec<u32> = p.stages().iter().map(|&(_, v)| v).collect();
                p.set_stages(&dpis, (*a).min(n - 1));
                Ok(())
            }),
            Change::Button { slot, action } => {
                let base = (self.onboard()?.button_count as usize).clamp(1, prof::MAX_BUTTONS);
                let shifted = self.feats.lock().unwrap_or_else(|p| p.into_inner()).g502x;
                let index = physical_slot(*slot, base, shifted)?;
                self.edit(|p| {
                    if index < base
                        && p.button(index) == ButtonAction::Mouse(MouseFn::Left)
                        && *action != ButtonAction::Mouse(MouseFn::Left)
                        && !(0..base).any(|i| {
                            i != index && p.button(i) == ButtonAction::Mouse(MouseFn::Left)
                        })
                    {
                        return Err(HubError::Invalid(
                            "通常レイヤーに左クリックが必要です".into(),
                        ));
                    }
                    p.set_button(index, action)
                })
            }
            Change::Lighting { area: 0, state } if !self.caps().lighting.is_empty() => {
                // G HUB は 2 ゾーンに同じ値を書く (G703 実機) — 両方へ書く。
                let l = encode_led(state);
                self.edit(|p| {
                    p.set_led(0, l);
                    p.set_led(1, l);
                    Ok(())
                })?;
                release_led_control(&mut self.pp());
                Ok(())
            }
            Change::Profile(n) => {
                let desc = self.onboard()?;
                if *n >= desc.profile_count {
                    return Err(HubError::Invalid(format!("プロファイル {}", n + 1)));
                }
                self.with_onboard("プロファイル切替", |ob, _| {
                    ob.pp.request(ob.idx, 3, &[0, n + 1]).map(|_| ())
                })
            }
            Change::ProfileName { index, name } => {
                let l = self.load()?;
                if *index != l.active {
                    return Err(HubError::Invalid(
                        "名前は選択中のプロファイルだけ変更できます".into(),
                    ));
                }
                self.edit(|p| {
                    p.set_name(name);
                    Ok(())
                })
            }
            _ => Err(HubError::Unsupported("この変更")),
        }
    }

    fn run_action(&self, id: &str, _file: Option<&std::path::Path>) -> HubResult<ActionOutcome> {
        match id {
            "restore_profile" => {
                self.require_onboard_mode()?;
                let l = self.load()?;
                self.with_onboard("出荷時に戻す", |ob, desc| {
                    let raw = ob.read_sector(rom_sector(l.active))?;
                    if !sector_crc_ok(&raw) {
                        return Err(PpError::Io("ROM プロファイルが読めません".into()));
                    }
                    let mut l2 = l.clone();
                    l2.current = Profile::new(raw);
                    commit(ob, desc, &l2)
                })?;
                Ok(ActionOutcome {
                    message: Some("プロファイルを出荷時の内容に戻しました".into()),
                    reload: true,
                })
            }
            "redetect" => {
                self.feats.lock().unwrap_or_else(|p| p.into_inner()).onboard = None;
                self.detect();
                Ok(ActionOutcome {
                    message: Some("再検出しました".into()),
                    reload: true,
                })
            }
            _ => Err(HubError::Unsupported("action")),
        }
    }

    fn battery(&self) -> HubResult<Battery> {
        let which = self
            .feats
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .battery
            .ok_or(HubError::Unsupported("battery"))?;
        let func = if which == feat::UNIFIED_BATTERY { 1 } else { 0 };
        let r = self.pp().call(which, func, &[]);
        let p = r.map_err(|e| self.fail("電池", e))?;
        self.set_asleep(false);
        Ok(match which {
            feat::UNIFIED_BATTERY => parse_unified_battery(&p),
            feat::BATTERY_STATUS => parse_battery_status(&p),
            _ => parse_battery_voltage(&p),
        })
    }
}

// ====================================================================== テスト

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// 送信を記録し、セクタを持つ疑似マウス。
    struct FakeMouse {
        dev: u8,
        sent: Vec<Vec<u8>>,
        out: VecDeque<Vec<u8>>,
        sectors: HashMap<u16, Vec<u8>>,
        sector_size: usize,
        current: u16,
        writing: Option<(u16, Vec<u8>, usize)>,
        /// 応答の前に割り込ませる通知。
        noise: bool,
    }

    const ONBOARD_IDX: u8 = 0x0E;

    impl FakeMouse {
        fn new(dev: u8, sector_size: usize) -> Self {
            Self {
                dev,
                sent: vec![],
                out: VecDeque::new(),
                sectors: HashMap::new(),
                sector_size,
                current: rom_sector(0),
                writing: None,
                noise: false,
            }
        }
        fn reply(&mut self, feat: u8, fs: u8, p: &[u8]) {
            let mut r = vec![0u8; 20];
            r[0] = REPORT_LONG;
            r[1] = self.dev;
            r[2] = feat;
            r[3] = fs;
            r[4..4 + p.len()].copy_from_slice(p);
            self.out.push_back(r);
        }
    }

    impl Link for FakeMouse {
        fn write(&mut self, d: &[u8]) -> HubResult<()> {
            self.sent.push(d.to_vec());
            assert_eq!(d.len(), LONG_LEN);
            assert_eq!(d[0], REPORT_LONG);
            let (feat, fs) = (d[2], d[3]);
            let func = fs >> 4;
            let p = &d[4..];
            if self.noise {
                // 無関係な通知 (swId 0) を先に流す。
                let mut n = vec![0u8; 20];
                n[0] = REPORT_LONG;
                n[1] = self.dev;
                n[2] = 0x04;
                n[3] = 0x00;
                self.out.push_back(n);
            }
            match (feat, func) {
                (0, 0) => {
                    let id = u16::from_be_bytes([p[0], p[1]]);
                    let idx = if id == feat::ONBOARD_PROFILES {
                        ONBOARD_IDX
                    } else {
                        0
                    };
                    self.reply(feat, fs, &[idx, 0, 0]);
                }
                (0, 1) => self.reply(feat, fs, &[4, 2, p[2]]),
                (ONBOARD_IDX, 4) => {
                    let c = self.current.to_be_bytes();
                    self.reply(feat, fs, &c);
                }
                (ONBOARD_IDX, 3) => {
                    self.current = u16::from_be_bytes([p[0], p[1]]);
                    self.reply(feat, fs, &[]);
                }
                (ONBOARD_IDX, 5) => {
                    let s = u16::from_be_bytes([p[0], p[1]]);
                    let o = u16::from_be_bytes([p[2], p[3]]) as usize;
                    let data = self
                        .sectors
                        .get(&s)
                        .cloned()
                        .unwrap_or_else(|| vec![0xFF; self.sector_size]);
                    assert!(o + 16 <= self.sector_size, "read past sector end");
                    let chunk = data[o..o + 16].to_vec();
                    self.reply(feat, fs, &chunk);
                }
                (ONBOARD_IDX, 6) => {
                    let s = u16::from_be_bytes([p[0], p[1]]);
                    let len = u16::from_be_bytes([p[4], p[5]]) as usize;
                    self.writing = Some((s, vec![], len));
                    self.reply(feat, fs, &[]);
                }
                (ONBOARD_IDX, 7) => {
                    let w = self.writing.as_mut().expect("addrWrite first");
                    w.1.extend_from_slice(&p[..16]);
                    self.reply(feat, fs, &[]);
                }
                (ONBOARD_IDX, 8) => {
                    let (s, mut data, len) = self.writing.take().unwrap();
                    data.truncate(len);
                    self.sectors.insert(s, data);
                    self.reply(feat, fs, &[]);
                }
                _ => {
                    // HID++ 2.0 エラー (InvalidFunctionId)。
                    let mut r = vec![0u8; 20];
                    r[0] = REPORT_LONG;
                    r[1] = self.dev;
                    r[2] = 0xFF;
                    r[3] = feat;
                    r[4] = fs;
                    r[5] = 7;
                    self.out.push_back(r);
                }
            }
            Ok(())
        }
        fn read(&mut self, buf: &mut [u8], _t: i32) -> HubResult<usize> {
            match self.out.pop_front() {
                Some(r) => {
                    buf[..r.len()].copy_from_slice(&r);
                    Ok(r.len())
                }
                None => Ok(0),
            }
        }
    }

    fn sample_profile(size: usize) -> Vec<u8> {
        let mut raw = vec![0xFFu8; size];
        raw[0] = 1; // 1000 Hz
        raw[1] = 1; // 既定ステージ 2
        raw[2] = 0; // シフト = ステージ 1
        for (i, d) in [400u16, 800, 1600, 3200, 0].iter().enumerate() {
            raw[3 + 2 * i..5 + 2 * i].copy_from_slice(&d.to_le_bytes());
        }
        let btns: [[u8; 4]; 5] = [
            [0x80, 0x01, 0x00, 0x01],
            [0x80, 0x01, 0x00, 0x02],
            [0x80, 0x01, 0x00, 0x04],
            [0x80, 0x01, 0x00, 0x08],
            [0x90, 0x05, 0x00, 0x00],
        ];
        for (i, b) in btns.iter().enumerate() {
            raw[32 + 4 * i..36 + 4 * i].copy_from_slice(b);
        }
        let mut p = Profile::new(raw);
        p.set_name("Default");
        p.set_led(0, [led::FIXED, 0, 0xAA, 0xFF, 0, 0, 0, 0, 0, 0, 0]);
        p.seal();
        p.raw
    }

    fn desc(size: u16) -> OnboardDesc {
        OnboardDesc {
            memory_model: 1,
            profile_format: 3,
            macro_format: 1,
            profile_count: 3,
            profile_count_oob: 0,
            button_count: 8,
            sector_count: 16,
            sector_size: size,
        }
    }

    #[test]
    fn crc_ccitt_known_vector() {
        // CRC-16/CCITT-FALSE("123456789") = 0x29B1
        assert_eq!(crc_ccitt(b"123456789"), 0x29B1);
    }

    #[test]
    fn little_endian_mouse_with_left_click_moved() {
        let mut raw = vec![0xff; 255];
        raw[prof::BUTTONS + 4..prof::BUTTONS + 8].copy_from_slice(&[
            bind::HID,
            bind::HID_MOUSE,
            1,
            0,
        ]);
        let p = Profile::new(raw);
        assert!(p.bits_le);
        assert_eq!(p.button(1), ButtonAction::Mouse(MouseFn::Left));
    }

    #[test]
    fn g502x_banks_leave_wheel_and_reserved_slots_alone() {
        let slots: Vec<_> = (0..22)
            .map(|i| physical_slot(i, 11, true).unwrap())
            .collect();
        assert_eq!(slots, (0..11).chain(16..27).collect::<Vec<_>>());
        assert!(physical_slot(22, 11, true).is_err());
        assert!(physical_slot(11, 11, false).is_err());
    }

    #[test]
    fn classify_frames() {
        let mut ok = [0u8; 20];
        ok[..4].copy_from_slice(&[0x11, 0xFF, 0x0E, 0x5B]);
        ok[4] = 0x42;
        assert!(matches!(classify(&ok, 0xFF, 0x0E, 5), Frame::Reply(p) if p[0] == 0x42));
        // 別 func の応答は無関係。
        assert_eq!(classify(&ok, 0xFF, 0x0E, 4), Frame::Other);
        // 別 device index。
        assert_eq!(classify(&ok, 0x01, 0x0E, 5), Frame::Other);
        let err = [0x11, 0xFF, 0xFF, 0x0E, 0x5B, 0x02, 0, 0];
        assert_eq!(
            classify(&err, 0xFF, 0x0E, 5),
            Frame::Error(PpError::Feature(2))
        );
        let rerr = [0x10, 0x01, 0x8F, 0x0E, 0x5B, 0x09, 0];
        assert_eq!(
            classify(&rerr, 0x01, 0x0E, 5),
            Frame::Error(PpError::Receiver(9))
        );
    }

    #[test]
    fn request_skips_notifications() {
        let mut m = FakeMouse::new(DEV_WIRED, 256);
        m.noise = true;
        let mut pp = Hidpp::new(m, DEV_WIRED);
        assert_eq!(pp.ping().unwrap(), (4, 2));
        assert_eq!(
            pp.feature(feat::ONBOARD_PROFILES).unwrap(),
            Some(ONBOARD_IDX)
        );
        assert_eq!(pp.feature(feat::ADJUSTABLE_DPI).unwrap(), None);
        assert!(!pp.events.is_empty());
        // 未対応関数は HID++ エラーで返る。
        assert_eq!(pp.request(ONBOARD_IDX, 0x0F, &[]), Err(PpError::Feature(7)));
    }

    #[test]
    fn dpi_list_with_step() {
        assert_eq!(dpi_range(&[100, 0xE032, 25600, 0]), Some((100, 25600, 50)));
        assert_eq!(dpi_range(&[400, 800, 1600, 0, 9]), Some((400, 1600, 400)));
        assert_eq!(dpi_range(&[0]), None);
    }

    #[test]
    fn battery_voltage_curve() {
        assert_eq!(voltage_to_percent(4200), 100);
        assert_eq!(voltage_to_percent(3811), 50);
        assert_eq!(voltage_to_percent(3400), 0);
        let mut p = [0u8; 16];
        p[..3].copy_from_slice(&[0x0F, 0x1B, 0x00]); // 3867mV, 放電中
        let b = parse_battery_voltage(&p);
        assert_eq!(b.percent, Some(61));
        assert!(!b.charging);
        p[2] = 0x80; // 充電中
        assert!(parse_battery_voltage(&p).charging);
        p[2] = 0x82; // 充電完了
        assert_eq!(parse_battery_voltage(&p).percent, Some(100));
    }

    #[test]
    fn profile_fields_roundtrip() {
        let mut p = Profile::new(sample_profile(256));
        assert!(p.crc_ok());
        assert!(!p.bits_le);
        assert_eq!(p.report_rate_hz(), 1000);
        assert_eq!(p.stages().len(), 4);
        assert_eq!(p.active_stage(), 1);
        assert_eq!(p.shift_dpi(), Some(400));
        assert_eq!(p.button(0), ButtonAction::Mouse(MouseFn::Left));
        assert_eq!(p.button(3), ButtonAction::Mouse(MouseFn::Back));
        assert_eq!(p.button(4), ButtonAction::Dpi(DpiFn::Cycle));
        assert_eq!(p.name(), "Default");

        p.set_report_rate_hz(500);
        assert_eq!(p.raw[0], 2);
        p.set_stages(&[400, 1200], 1);
        assert_eq!(p.stages(), vec![(0, 400), (1, 1200)]);
        assert_eq!(p.dpi_raw(2), 0);
        assert_eq!(p.shift_dpi(), Some(400));
        p.set_button(4, &ButtonAction::Dpi(DpiFn::Shift(1200)))
            .unwrap();
        assert_eq!(p.button(4), ButtonAction::Dpi(DpiFn::Shift(1200)));
        assert!(p
            .set_button(4, &ButtonAction::Dpi(DpiFn::Shift(999)))
            .is_err());
        p.set_button(
            5,
            &ButtonAction::Key(KeyStroke {
                mods: mods::LCTRL,
                usage: 0x06,
            }),
        )
        .unwrap();
        assert_eq!(p.button_raw(5), [0x80, 0x02, 0x01, 0x06]);
        p.set_button(6, &ButtonAction::Media(MediaFn::VolumeUp))
            .unwrap();
        assert_eq!(p.button_raw(6), [0x80, 0x03, 0x00, 0xE9]);
        assert_eq!(p.button(6), ButtonAction::Media(MediaFn::VolumeUp));
        p.seal();
        assert!(p.crc_ok());
    }

    #[test]
    fn mouse_bits_endianness_autodetect() {
        let mut raw = sample_profile(256);
        raw[32..36].copy_from_slice(&[0x80, 0x01, 0x01, 0x00]);
        raw[36..40].copy_from_slice(&[0x80, 0x01, 0x02, 0x00]);
        let p = Profile::new(raw);
        assert!(p.bits_le);
        assert_eq!(p.button(1), ButtonAction::Mouse(MouseFn::Right));
        assert_eq!(
            encode_button(&ButtonAction::Mouse(MouseFn::Forward), true).unwrap(),
            [0x80, 0x01, 0x10, 0x00]
        );
    }

    #[test]
    fn unknown_binding_roundtrips() {
        let b = [0x00, 0x01, 0x23, 0x00]; // マクロ (page/offset)
        let a = decode_button(b, false, None);
        assert!(matches!(a, ButtonAction::Unsupported { ty: 0, .. }));
        assert_eq!(encode_button(&a, false).unwrap(), b);
    }

    #[test]
    fn led_roundtrip() {
        let mut s = decode_led(&[led::BREATHING, 1, 2, 3, 0x0F, 0xA0, 0, 80, 0, 0, 0]);
        assert_eq!(s.effect, led::BREATHING);
        assert_eq!(s.color, Rgb::new(1, 2, 3));
        assert_eq!(s.speed, 7); // 4000ms
        assert_eq!(s.brightness, 80);
        assert_eq!(
            encode_led(&s)[..8],
            [led::BREATHING, 1, 2, 3, 0x0F, 0xA0, 0, 80]
        );
        s.on = false;
        assert_eq!(encode_led(&s), [0; 11]);
    }

    #[test]
    fn directory_codec() {
        let d = build_directory(
            &[
                DirEntry {
                    sector: 1,
                    enabled: true,
                },
                DirEntry {
                    sector: 0x0102,
                    enabled: false,
                },
            ],
            256,
        );
        assert!(sector_crc_ok(&d));
        assert_eq!(&d[..9], &[0, 1, 1, 0, 1, 2, 0, 0, 0xFF]);
        assert_eq!(parse_directory(&d, 5).len(), 2);
    }

    #[test]
    fn corrupt_user_slot_does_not_fall_back_to_factory_buttons() {
        let size = 255;
        let mut mouse = FakeMouse::new(DEV_RECEIVER_SLOT1, size);
        mouse.sectors.insert(rom_sector(0), sample_profile(size));
        let mut bad = sample_profile(size);
        bad[prof::REPORT_RATE] = 255;
        seal_sector(&mut bad);
        mouse.sectors.insert(user_sector(0), bad);
        let mut pp = Hidpp::new(mouse, DEV_RECEIVER_SLOT1);
        let mut ob = Onboard {
            pp: &mut pp,
            idx: ONBOARD_IDX,
            sector_size: size,
        };
        let dir = [DirEntry {
            sector: user_sector(0),
            enabled: true,
        }];
        assert!(read_profile(&mut ob, &dir, 0).is_err());
    }

    /// ROM 既定で動いているマウスに初めて書く: ユーザー領域へ写し、
    /// ディレクトリを作り、現在プロファイルを選び直す。
    #[test]
    fn first_commit_from_rom() {
        for size in [255usize, 256] {
            let mut m = FakeMouse::new(DEV_RECEIVER_SLOT1, size);
            for i in 0..3 {
                m.sectors.insert(rom_sector(i), sample_profile(size));
            }
            let mut pp = Hidpp::new(m, DEV_RECEIVER_SLOT1);
            let d = desc(size as u16);
            assert!(d.layout_known());
            let mut ob = Onboard {
                pp: &mut pp,
                idx: ONBOARD_IDX,
                sector_size: size,
            };
            let mut l = load_profiles(&mut ob, &d).unwrap();
            assert_eq!(l.active, 0);
            assert!(!l.in_user);
            assert!(!l.dir_valid);
            l.current.set_report_rate_hz(250);
            commit(&mut ob, &d, &l).unwrap();

            // もう一度読むとユーザー領域・新しい値。
            let l2 = load_profiles(&mut ob, &d).unwrap();
            assert!(l2.dir_valid);
            assert!(l2.in_user);
            assert_eq!(l2.current.report_rate_hz(), 250);
            assert_eq!(
                l2.dir.iter().map(|e| e.sector).collect::<Vec<_>>(),
                vec![1, 2, 3]
            );
            let m = &pp.link;
            assert_eq!(m.current, 1);
            for s in 1..=3u16 {
                assert!(sector_crc_ok(&m.sectors[&s]), "sector {s} sealed");
                assert_eq!(m.sectors[&s].len(), size);
            }
        }
    }
    /// G703 実機 (c090 有線) のプロファイル 1 をそのまま解釈する。
    #[test]
    fn g703_real_profile() {
        let txt = include_str!("../fixtures/logitech/g703-wired-profile1.txt");
        let raw: Vec<u8> = txt
            .lines()
            .filter(|l| !l.starts_with('#'))
            .filter_map(|l| l.split_once(": "))
            .flat_map(|(_, h)| {
                h.split_whitespace()
                    .map(|b| u8::from_str_radix(b, 16).unwrap())
            })
            .collect();
        assert_eq!(raw.len(), 255);
        let d = OnboardDesc::parse(&[1, 4, 1, 5, 1, 6, 0x10, 0, 0xFF, 0x0A, 4, 0, 0, 0, 0, 0]);
        assert_eq!(d.sector_size, 255);
        assert!(d.layout_known());
        let mut p = Profile::new(raw);
        assert!(p.crc_ok());
        assert!(!p.bits_le);
        assert_eq!(p.report_rate_hz(), 1000);
        assert_eq!(p.stages(), vec![(0, 400), (1, 800), (2, 1600), (3, 3200)]);
        assert_eq!(p.active_stage(), 1);
        assert_eq!(p.button(5), ButtonAction::Dpi(DpiFn::Cycle));
        assert_eq!(p.name(), "");
        let led = decode_led(&p.led(0));
        assert_eq!((led.effect, led.brightness), (led::CYCLE, 100));
        assert_eq!(encode_led(&led)[..9], p.led(0)[..9]);
        // 書き換えて封をしても CRC は合う (255B セクタ)。
        p.set_report_rate_hz(500);
        p.seal();
        assert!(p.crc_ok());
    }
}
