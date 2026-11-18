use super::{parse_battery_voltage, read_fw, read_name, Hidpp, DEV_RECEIVER_SLOT1};
use devhub_core::{backend::*, model::*};
use std::collections::BTreeMap;
use std::sync::Mutex;

const BRIGHTNESS: u16 = 0x8040;
const RATE: u16 = 0x8060;
const RGB: u16 = 0x8071;
const LEVELS: [i64; 6] = [0, 6, 12, 25, 50, 100];

pub struct Keyboard {
    pp: Mutex<Hidpp<hidapi::HidDevice>>,
    info: DeviceInfo,
    caps: Capabilities,
    last_light: Mutex<BTreeMap<u8, LightingState>>,
}

pub fn open(dev: hidapi::HidDevice, mut info: DeviceInfo) -> HubResult<Keyboard> {
    let mut pp = Hidpp::new(dev, DEV_RECEIVER_SLOT1);
    // The receiver can take a second to answer the first request after idle.
    pp.timeout_ms = 1200;
    let name = read_name(&mut pp)
        .or_else(|| read_name(&mut pp))
        .ok_or(HubError::Disconnected)?;
    if !name.starts_with("G913 TKL ") && !name.starts_with("G915 TKL ") {
        return Err(HubError::Unsupported("受信機の接続先"));
    }
    info.name = name;
    info.firmware = read_fw(&mut pp);
    let mut caps = Capabilities::empty();
    let range = pp.call(BRIGHTNESS, 0, &[]).map_err(|e| e.to_hub("輝度"))?;
    if range[..6] == [0, 100, 5, 3, 0, 0] {
        caps.settings.push(SettingSpec::choice(
            "brightness",
            section::LIGHTING,
            "全体の輝度",
            LEVELS
                .iter()
                .map(|v| ChoiceOpt::new(*v, format!("{v}%")))
                .collect(),
        ));
    }
    if let Ok(r) = pp.call(RATE, 0, &[]) {
        let rates = [8, 4, 2, 1]
            .into_iter()
            .filter(|ms| r[0] & (1 << (ms - 1)) != 0)
            .map(|ms| ChoiceOpt::new(1000 / ms, format!("{} Hz", 1000 / ms)))
            .collect();
        caps.settings.push(SettingSpec::choice(
            "polling",
            section::PERFORMANCE,
            "ポーリングレート",
            rates,
        ));
    }
    if let Ok(r) = pp.call(RGB, 0, &[255, 255, 0]) {
        for id in 0..r[2].min(2) {
            // The TKL advertises off and fixed at indices 0 and 1 in each cluster.
            let fixed = pp
                .call(RGB, 0, &[id, 1, 0])
                .map_err(|e| e.to_hub("照明領域"))?;
            let off = pp
                .call(RGB, 0, &[id, 0, 0])
                .map_err(|e| e.to_hub("照明領域"))?;
            if fixed[..4] == [id, 1, 0, 1] && off[..4] == [id, 0, 0, 0] {
                caps.lighting.push(LightArea {
                    id,
                    name: if id == 0 {
                        "ロゴ・指定色"
                    } else {
                        "キー・指定色"
                    }
                    .into(),
                    effects: vec![LightEffect::new(1, "固定色").color().still()],
                    brightness_max: 0,
                    speed_max: 0,
                    directions: vec![],
                    switchable: true,
                    zones: vec![],
                    per_key: false,
                    direct_drive: false,
                    live_readback: false,
                });
            }
        }
    }
    caps.battery = pp
        .feature(super::feat::BATTERY_VOLTAGE)
        .ok()
        .flatten()
        .is_some();
    Ok(Keyboard {
        pp: Mutex::new(pp),
        info,
        caps,
        last_light: Mutex::new(BTreeMap::new()),
    })
}

impl Keyboard {
    fn call(&self, feature: u16, function: u8, args: &[u8]) -> HubResult<[u8; 16]> {
        self.pp
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .call(feature, function, args)
            .map_err(|e| e.to_hub("G913 TKL"))
    }
}

impl DeviceDriver for Keyboard {
    fn info(&self) -> DeviceInfo {
        let mut i = self.info.clone();
        i.battery = self.battery().ok();
        i
    }
    fn caps(&self) -> Capabilities {
        self.caps.clone()
    }
    fn read(&self) -> HubResult<DeviceState> {
        let mut settings = BTreeMap::new();
        if self.caps.setting("brightness").is_some() {
            let p = self.call(BRIGHTNESS, 1, &[])?;
            settings.insert(
                "brightness".into(),
                SettingValue::Choice(u16::from_be_bytes([p[0], p[1]]) as i64),
            );
        }
        if self.caps.setting("polling").is_some() {
            let ms = self.call(RATE, 1, &[])?[0];
            if ms > 0 {
                settings.insert("polling".into(), SettingValue::Choice(1000 / i64::from(ms)));
            }
        }
        Ok(DeviceState {
            profile: ProfileState::default(),
            settings,
            mouse: None,
            keyboard: None,
            lighting: self
                .last_light
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone(),
            peq: None,
        })
    }
    fn apply(&self, change: &Change) -> HubResult<()> {
        match change {
            Change::Setting {
                id,
                value: SettingValue::Choice(v),
            } => {
                let spec = self.caps.setting(id).ok_or(HubError::Unsupported("設定"))?;
                if !matches!(&spec.kind,SettingKind::Choice{options} if options.iter().any(|o|o.value==*v))
                {
                    return Err(HubError::Invalid(id.clone()));
                }
                match id.as_str() {
                    "brightness" => self.call(BRIGHTNESS, 2, &(*v as u16).to_be_bytes())?,
                    "polling" => self.call(RATE, 2, &[(1000 / v) as u8])?,
                    _ => return Err(HubError::Unsupported("設定")),
                };
            }
            Change::Lighting { area, state }
                if self.caps.lighting.iter().any(|a| a.id == *area) =>
            {
                let mode = self.call(super::feat::ONBOARD_PROFILES, 2, &[])?[0];
                let control = self.call(RGB, 5, &[0])?;
                let power = self.call(RGB, 8, &[0])?[1];
                let change_mode = mode != 2;
                let change_control = control[1..3] != [3, 7];
                let change_power = power != 1;
                let mut restore = Vec::new();
                let result = (|| -> HubResult<()> {
                    if change_mode {
                        restore.push((super::feat::ONBOARD_PROFILES, 1, vec![mode]));
                        self.call(super::feat::ONBOARD_PROFILES, 1, &[2])?;
                    }
                    if change_control {
                        restore.push((RGB, 5, vec![1, control[1], control[2]]));
                        self.call(RGB, 5, &[1, 3, 7])?;
                    }
                    if change_power {
                        restore.push((RGB, 8, vec![1, power]));
                        self.call(RGB, 8, &[1, 1])?;
                    }
                    let mut p = [0u8; 13];
                    p[0] = *area;
                    if state.on {
                        p[1..6].copy_from_slice(&[
                            1,
                            state.color.r,
                            state.color.g,
                            state.color.b,
                            2,
                        ]);
                    }
                    p[12] = 1;
                    self.call(RGB, 1, &p)?;
                    Ok(())
                })();
                if let Err(error) = result {
                    let mut failures = Vec::new();
                    for (feature, function, args) in restore.into_iter().rev() {
                        if let Err(e) = self.call(feature, function, &args) {
                            failures.push(e.to_string());
                        }
                    }
                    if failures.is_empty() {
                        return Err(error);
                    }
                    return Err(HubError::Protocol(format!(
                        "{error} / 復元エラー: {}",
                        failures.join(", ")
                    )));
                }
                self.last_light
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(*area, state.clone());
            }
            _ => return Err(HubError::Unsupported("この変更")),
        }
        Ok(())
    }
    fn battery(&self) -> HubResult<Battery> {
        Ok(parse_battery_voltage(&self.call(
            super::feat::BATTERY_VOLTAGE,
            0,
            &[],
        )?))
    }
}
