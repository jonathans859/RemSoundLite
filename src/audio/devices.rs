//! Listing Windows audio endpoints with WASAPI.

use wasapi::{DeviceEnumerator, Direction};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceList {
    pub inputs: Vec<DeviceInfo>,
    pub outputs: Vec<DeviceInfo>,
}

fn collect(enumerator: &DeviceEnumerator, direction: &Direction) -> Vec<DeviceInfo> {
    let mut out = Vec::new();
    let Ok(collection) = enumerator.get_device_collection(direction) else { return out };
    for device in &collection {
        let Ok(device) = device else { continue };
        let (Ok(id), Ok(name)) = (device.get_id(), device.get_friendlyname()) else { continue };
        out.push(DeviceInfo { id, name });
    }
    out.sort_by_key(|a| a.name.to_lowercase());
    out
}

/// Active capture and render endpoints. Runs on a short-lived MTA thread so it never cares
/// which COM apartment the caller is in (the wx GUI thread is an STA).
pub fn list() -> DeviceList {
    std::thread::spawn(|| {
        let _ = wasapi::initialize_mta();
        let Ok(enumerator) = DeviceEnumerator::new() else { return DeviceList::default() };
        DeviceList {
            inputs: collect(&enumerator, &Direction::Capture),
            outputs: collect(&enumerator, &Direction::Render),
        }
    })
    .join()
    .unwrap_or_default()
}
