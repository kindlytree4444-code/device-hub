//! Read settings through the same backend as the GUI. No demo fallback.
use devhub_core::{Change, DeviceKind, DriverKind, HubError, HubResult, SettingValue};

fn main() -> HubResult<()> {
    let mut test = false;
    let mut wanted = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--brightness-roundtrip" => test = true,
            "--mouse" => wanted.push(
                args.next()
                    .filter(|s| !s.is_empty() && !s.starts_with("--"))
                    .ok_or_else(|| HubError::Invalid("--mouse needs a model name".into()))?,
            ),
            _ => return Err(HubError::Invalid(arg)),
        }
    }
    devhub_hid::logitech::use_diagnostic_software_id()?;
    let hub = devhub_hid::connect()?;
    let mut checked = false;
    let mut keyboard = false;
    let mut mice = Vec::new();
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
                "{} {:04x} {} {:?}, battery {:?}, active_profile {}, lighting {:?}",
                info.id.0,
                info.pid,
                info.name,
                state.settings,
                info.battery,
                state.profile.active,
                driver
                    .caps()
                    .lighting
                    .iter()
                    .map(|a| (a.id, a.effects.iter().map(|e| e.id).collect::<Vec<_>>()))
                    .collect::<Vec<_>>()
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
                DeviceKind::Mouse if state.mouse.is_some() => mice.push(info.name),
                _ => {}
            }
            Ok(())
        })();
        if let Err(error) = result {
            eprintln!("{:04x}: {error}", device.pid);
        }
    }
    let mouse = !mice.is_empty()
        && wanted.iter().all(|name| {
            mice.iter()
                .any(|m| m.to_lowercase().contains(&name.to_lowercase()))
        });
    if !keyboard || !mouse || (test && !checked) {
        return Err(HubError::Protocol(format!(
            "incomplete device check: keyboard={keyboard}, mouse={mouse}, responding_mice={mice:?}, required_mice={wanted:?}, brightness_roundtrip={checked}"
        )));
    }
    Ok(())
}
