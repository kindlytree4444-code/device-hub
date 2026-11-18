//! Logicool HID++ 2.0 解析ツール (PRO Wireless 等)。
//!
//! ```text
//! cargo run -p devhub-hid --example logiprobe            # feature 一覧 + プロファイル dump
//! cargo run -p devhub-hid --example logiprobe -- sniff   # HID++ 通信を垂れ流し (G HUB の通信観察用)
//! cargo run -p devhub-hid --example logiprobe -- dump 0x0101   # 任意セクタの hex
//! ```
//!
//! 出力をそのまま docs/LOGITECH_HIDPP.md の「実機ログ」に貼れば、他環境でも解析を続けられる。
//! 書き込みは一切しない (読み取り専用)。

use devhub_hid::logitech::*;

const NAMES: &[(u16, &str)] = &[
    (0x0000, "Root"),
    (0x0001, "FeatureSet"),
    (0x0002, "FeatureInfo"),
    (0x0003, "DeviceFwVersion"),
    (0x0005, "DeviceName"),
    (0x0007, "DeviceFriendlyName"),
    (0x0020, "ConfigChange"),
    (0x00C2, "DfuControlSigned"),
    (0x00D0, "Dfu"),
    (0x1000, "BatteryStatus"),
    (0x1001, "BatteryVoltage"),
    (0x1004, "UnifiedBattery"),
    (0x1300, "LedControl"),
    (0x1800, "GenericTest"),
    (0x1801, "ManufacturingMode"),
    (0x1802, "DeviceReset"),
    (0x1803, "OOBState?"),
    (0x1805, "OobState"),
    (0x1806, "ConfigurableDeviceProperties"),
    (0x1811, "?"),
    (0x1813, "?"),
    (0x1830, "PowerModes"),
    (0x1875, "?"),
    (0x18A1, "?"),
    (0x1B04, "ReprogControlsV4"),
    (0x1D4B, "WirelessDeviceStatus"),
    (0x1DF3, "EquadDJDebugInfo"),
    (0x1E00, "EnableHiddenFeatures"),
    (0x1E02, "?"),
    (0x1E22, "?"),
    (0x1EB0, "TDE?"),
    (0x1F1F, "?"),
    (0x2201, "AdjustableDpi"),
    (0x2250, "?"),
    (0x8060, "AdjustableReportRate"),
    (0x8070, "ColorLedEffects"),
    (0x8071, "RgbEffects"),
    (0x8081, "PerKeyLighting"),
    (0x8090, "ModeStatus?"),
    (0x80A4, "AxisResponseCurve"),
    (0x8100, "OnboardProfiles"),
    (0x8110, "MouseButtonSpy"),
];

fn fname(id: u16) -> &'static str {
    NAMES
        .iter()
        .find(|(i, _)| *i == id)
        .map(|(_, n)| *n)
        .unwrap_or("?")
}

fn hex(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let api = hidapi::HidApi::new().expect("hidapi");
    let targets: Vec<_> = api
        .device_list()
        .filter(|d| d.vendor_id() == 0x046D && d.usage_page() == 0xFF00)
        .collect();
    if targets.is_empty() {
        eprintln!("Logicool の HID++ インターフェース (046d / FF00) が見つかりません");
        return;
    }
    for d in &targets {
        println!(
            "# {:04x}:{:04x} if={} up={:04x} u={:04x} {:?}",
            d.vendor_id(),
            d.product_id(),
            d.interface_number(),
            d.usage_page(),
            d.usage(),
            d.product_string()
        );
    }

    if args.first().map(String::as_str) == Some("sniff") {
        sniff(&api, &targets);
        return;
    }

    let Some(long) = targets.iter().find(|d| d.usage() == 0x0002) else {
        eprintln!("long collection (FF00:0002) がありません");
        return;
    };
    let dev = long.open_device(&api).expect("open");
    // 有線 (c088 等) は 0xFF、受信機は 1。
    let wired = !matches!(
        long.product_id(),
        0xC539 | 0xC53F | 0xC547 | 0xC54D | 0xC548
    );
    let index = if wired { DEV_WIRED } else { DEV_RECEIVER_SLOT1 };
    let mut pp = Hidpp::new(dev, index);
    match pp.ping() {
        Ok((maj, min)) => println!("\nping: HID++ {maj}.{min} (device index {index:#04x})"),
        Err(e) => {
            println!("\nping 失敗: {e:?} — 無線ならマウスを動かして起こしてから再実行");
            return;
        }
    }

    if let Some(i) = args.iter().position(|a| a == "dump") {
        let sector = args
            .get(i + 1)
            .and_then(|s| u16::from_str_radix(s.trim_start_matches("0x"), 16).ok())
            .unwrap_or(0);
        dump_sector(&mut pp, sector);
        return;
    }

    // ---- feature 一覧 (0x0001 FeatureSet)
    println!("\n## features");
    match pp.feature(0x0001) {
        Ok(Some(fs)) => {
            let n = pp.request(fs, 0, &[]).map(|r| r[0]).unwrap_or(0);
            for i in 0..=n {
                if let Ok(r) = pp.request(fs, 1, &[i]) {
                    let id = u16::from_be_bytes([r[0], r[1]]);
                    println!(
                        "  [{i:2}] {id:#06x} v{} type={:#04x} {}",
                        r[3],
                        r[2],
                        fname(id)
                    );
                }
            }
        }
        _ => println!("  FeatureSet なし"),
    }

    // ---- 名前・FW
    if let Ok(Some(idx)) = pp.feature(0x0003) {
        let n = pp.request(idx, 0, &[]).map(|r| r[0]).unwrap_or(0);
        println!("\n## firmware ({n} entities)");
        for e in 0..n {
            if let Ok(r) = pp.request(idx, 1, &[e]) {
                println!("  type {} : {}   raw {}", r[0], fw_string(&r), hex(&r));
            }
        }
    }

    // ---- 電池
    for (id, func) in [(0x1000u16, 0u8), (0x1001, 0), (0x1004, 1)] {
        if let Ok(r) = pp.call(id, func, &[]) {
            println!("\n## battery {id:#06x}: {}", hex(&r[..4]));
            let b = match id {
                0x1001 => parse_battery_voltage(&r),
                0x1004 => parse_unified_battery(&r),
                _ => parse_battery_status(&r),
            };
            println!("  -> {:?}%, charging={}", b.percent, b.charging);
        }
    }

    // ---- DPI
    if let Ok(Some(idx)) = pp.feature(0x2201) {
        println!("\n## 0x2201 AdjustableDpi");
        if let Ok(r) = pp.request(idx, 1, &[0]) {
            let w: Vec<u16> = r[1..]
                .chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect();
            println!("  list raw {} -> {:?}", hex(&r), dpi_range(&w));
        }
        if let Ok(r) = pp.request(idx, 2, &[0]) {
            println!(
                "  current {} default {}",
                u16::from_be_bytes([r[1], r[2]]),
                u16::from_be_bytes([r[3], r[4]])
            );
        }
    }
    if let Ok(Some(idx)) = pp.feature(0x8060) {
        let list = pp.request(idx, 0, &[]).map(|r| r[0]).unwrap_or(0);
        let cur = pp.request(idx, 1, &[]).map(|r| r[0]).unwrap_or(0);
        println!("\n## 0x8060 ReportRate: supported mask {list:#010b}, current {cur} ms");
    }

    if let Ok(Some(idx)) = pp.feature(0x8071) {
        match pp.request(idx, 5, &[]) {
            Ok(r) => println!(
                "\n## 0x8071 RgbEffects: LED 制御 = {} (0=本体 1=ソフト) raw {}",
                r[1],
                hex(&r[..4])
            ),
            Err(e) => println!("\n## 0x8071 RgbEffects: func5 失敗 {e:?}"),
        }
    }

    // ---- オンボードプロファイル
    let Ok(Some(ob)) = pp.feature(0x8100) else {
        println!("\n0x8100 OnboardProfiles なし");
        return;
    };
    let desc = match pp.request(ob, 0, &[]) {
        Ok(r) => {
            println!("\n## 0x8100 OnboardProfiles\n  desc raw {}", hex(&r));
            OnboardDesc::parse(&r)
        }
        Err(e) => {
            println!("desc 失敗 {e:?}");
            return;
        }
    };
    println!("  {desc:?}  layout_known={}", desc.layout_known());
    if let Ok(r) = pp.request(ob, 2, &[]) {
        println!("  mode = {} (1=onboard 2=host)", r[0]);
    }
    if let Ok(r) = pp.request(ob, 4, &[]) {
        println!(
            "  current profile sector = {:#06x}",
            u16::from_be_bytes([r[0], r[1]])
        );
    }
    if let Ok(r) = pp.request(ob, 0x0B, &[]) {
        println!("  current dpi index = {}", r[0]);
    }
    let mut o = Onboard {
        pp: &mut pp,
        idx: ob,
        sector_size: desc.sector_size as usize,
    };
    match load_profiles(&mut o, &desc) {
        Ok(l) => {
            println!("  directory valid={} entries={:?}", l.dir_valid, l.dir);
            let p = &l.current;
            println!(
                "  active={} in_user={} crc_ok={} name={:?}",
                l.active,
                l.in_user,
                p.crc_ok(),
                p.name()
            );
            println!(
                "  rate={}Hz stages={:?} active_stage={} shift={:?} bits_le={}",
                p.report_rate_hz(),
                p.stages(),
                p.active_stage(),
                p.shift_dpi(),
                p.bits_le
            );
            for i in 0..(desc.button_count as usize).min(16) {
                println!(
                    "  button {i:2}: {}  {:?}",
                    hex(&p.button_raw(i)),
                    p.button(i)
                );
            }
            println!("  led0 {}  led1 {}", hex(&p.led(0)), hex(&p.led(1)));
            println!("\n  raw:");
            for (n, c) in p.raw.chunks(16).enumerate() {
                println!("  {:04x}: {}", n * 16, hex(c));
            }
        }
        Err(e) => println!("  load 失敗 {e:?}"),
    }
}

fn dump_sector(pp: &mut Hidpp<hidapi::HidDevice>, sector: u16) {
    let Ok(Some(ob)) = pp.feature(0x8100) else {
        return;
    };
    let Ok(r) = pp.request(ob, 0, &[]) else {
        return;
    };
    let desc = OnboardDesc::parse(&r);
    let mut o = Onboard {
        pp,
        idx: ob,
        sector_size: desc.sector_size as usize,
    };
    match o.read_sector(sector) {
        Ok(data) => {
            println!(
                "sector {sector:#06x} ({} B) crc_ok={}",
                data.len(),
                sector_crc_ok(&data)
            );
            for (n, c) in data.chunks(16).enumerate() {
                println!("{:04x}: {}", n * 16, hex(c));
            }
        }
        Err(e) => println!("read 失敗 {e:?}"),
    }
}

/// 全 FF00 collection を開いて受信をそのまま表示する。
/// G HUB を起動したまま実行すると、G HUB が送った要求への応答・通知が見える
/// (Windows は他プロセスの送信は見えないが、応答から要求内容はほぼ復元できる)。
fn sniff(api: &hidapi::HidApi, targets: &[&hidapi::DeviceInfo]) {
    let devs: Vec<_> = targets
        .iter()
        .filter_map(|d| d.open_device(api).ok().map(|h| (d.usage(), h)))
        .collect();
    for (_, h) in &devs {
        h.set_blocking_mode(false).ok();
    }
    println!("\nsniff 中 (Ctrl+C で終了)…");
    let t0 = std::time::Instant::now();
    let mut buf = [0u8; 64];
    loop {
        let mut any = false;
        for (u, h) in &devs {
            if let Ok(n) = h.read(&mut buf) {
                if n > 0 {
                    any = true;
                    let b = &buf[..n];
                    let note = if b.len() >= 4 && b[0] == REPORT_LONG && b[2] != 0xFF {
                        format!(" feat#{} fn{} sw{}", b[2], b[3] >> 4, b[3] & 15)
                    } else {
                        String::new()
                    };
                    println!(
                        "{:9.3} u{u:04x} {}{note}",
                        t0.elapsed().as_secs_f32(),
                        hex(b)
                    );
                }
            }
        }
        if !any {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
}
