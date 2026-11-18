//! Read settings through the same backend as the GUI. No demo fallback.
use devhub_core::{Change, DeviceKind, DriverKind, HubError, HubResult, SettingValue};

fn main() -> HubResult<()> {
    let test = std::env::args()
        .skip(1)
        .any(|a| a == "--brightness-roundtrip");
    let hub = devhub_hid::connect()?;
    let mut checked = false;
    let mut keyboard = false;
    let mut mouse = false;
    for device in hub
        .scan()?
        .into_iter()
        .filter(|d| d.driver == DriverKind::Logitech)
    {
        let result = (|| -> HubResult<()> {
            let driver = hub.driver(&device.id)?;
            let info = driver.info();
            let state = driver.read()?;
            println!(
                "{:04x} {} {:?}, battery {:?}",
                info.pid, info.name, state.settings, info.battery
            );
            if info.kind == DeviceKind::Keyboard && test {
                let read = || {
                    driver
                        .read()?
                        .settings
                        .get("brightness")
                        .cloned()
                        .ok_or(HubError::Unsupported("brightness"))
                };
                let old = read()?;
                let trial = SettingValue::Choice(if old.as_choice() == 25 { 50 } else { 25 });
                let set = |v| {
                    driver.apply(&Change::Setting {
                        id: "brightness".into(),
                        value: v,
                    })
                };
                let check = |v| {
                    if read()? == v {
                        Ok(())
                    } else {
                        Err(HubError::Protocol("brightness readback mismatch".into()))
                    }
                };
                let trial_result = set(trial.clone()).and_then(|_| check(trial));
                set(old.clone())?;
                check(old)?;
                trial_result?;
                checked = true;
                println!("Keyboard brightness changed, read back and restored.");
            }
            match info.kind {
                DeviceKind::Keyboard => keyboard = true,
                DeviceKind::Mouse if state.mouse.is_some() => mouse = true,
                _ => {}
            }
            Ok(())
        })();
        if let Err(error) = result {
            eprintln!("{:04x}: {error}", device.pid);
        }
    }
    if !keyboard || !mouse || (test && !checked) {
        return Err(HubError::Protocol(format!("incomplete device check: keyboard={keyboard}, mouse={mouse}, brightness_roundtrip={checked}")));
    }
    Ok(())
}
