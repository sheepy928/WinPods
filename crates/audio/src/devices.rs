//! Audio endpoint description. CONTRACT.
//!
//! Besides the contract types this module holds the endpoint helpers shared by the
//! device-watch and capture threads: COM apartment setup, enumeration of active
//! endpoints into [`DeviceInfo`] values, the default render endpoint (Multimedia
//! role), and matching a configured device (id first, then display name, then
//! friendly name, case-insensitively) with [`find_device`].

use std::cmp::Ordering;
use std::marker::PhantomData;

use wasapi::{Device, DeviceEnumerator, DeviceState, Direction, Role, WasapiError};

/// Whether an endpoint is captured as loopback (a playback device) or as a recording
/// device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeviceKind {
    Loopback,
    Input,
}

/// One capturable endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// WASAPI endpoint id (stable across renames).
    pub id: String,
    /// Friendly name, e.g. "CABLE Input (VB-Audio Virtual Cable)".
    pub name: String,
    /// Name shown in the UI and stored in config: loopbacks get " [Loopback]"
    /// appended, matching the Python v0.1 config values.
    pub display_name: String,
    pub kind: DeviceKind,
    /// Mix-format sample rate and channel count.
    pub sample_rate: u32,
    pub channels: u16,
    /// True for the current default render device (Multimedia role).
    pub is_default: bool,
}

/// Suffix that marks a loopback endpoint in [`DeviceInfo::display_name`] (the same
/// spelling PyAudioWPatch used, so v0.1 config values keep matching).
pub const LOOPBACK_SUFFIX: &str = " [Loopback]";

/// The UI/config name of an endpoint: `name` plus [`LOOPBACK_SUFFIX`] for loopbacks.
pub fn display_name(name: &str, kind: DeviceKind) -> String {
    match kind {
        DeviceKind::Loopback => format!("{name}{LOOPBACK_SUFFIX}"),
        DeviceKind::Input => name.to_owned(),
    }
}

/// Find the endpoint a configured source refers to: an exact `id` match wins, then a
/// case-insensitive match on `display_name`, then on the friendly `name`. Within each
/// step the first device in `devices` order wins (loopbacks come first in
/// [`crate::DeviceWatch::list`] order).
pub fn find_device<'a>(
    devices: &'a [DeviceInfo],
    id: Option<&str>,
    name: &str,
) -> Option<&'a DeviceInfo> {
    let by_id = id
        .filter(|id| !id.is_empty())
        .and_then(|id| devices.iter().find(|d| d.id.eq_ignore_ascii_case(id)));
    by_id
        .or_else(|| {
            devices
                .iter()
                .find(|d| eq_ignore_case(&d.display_name, name))
        })
        .or_else(|| devices.iter().find(|d| eq_ignore_case(&d.name, name)))
}

/// Unicode-aware case-insensitive equality without allocating.
fn eq_ignore_case(a: &str, b: &str) -> bool {
    a.chars()
        .flat_map(char::to_lowercase)
        .eq(b.chars().flat_map(char::to_lowercase))
}

/// Unicode-aware case-insensitive ordering without allocating.
fn cmp_ignore_case(a: &str, b: &str) -> Ordering {
    a.chars()
        .flat_map(char::to_lowercase)
        .cmp(b.chars().flat_map(char::to_lowercase))
}

/// Listing order: loopbacks first, then inputs; each group by display name,
/// case-insensitively (the v0.1 order). Used with a stable sort.
fn device_order(a: &DeviceInfo, b: &DeviceInfo) -> Ordering {
    let group = |d: &DeviceInfo| d.kind != DeviceKind::Loopback;
    group(a)
        .cmp(&group(b))
        .then_with(|| cmp_ignore_case(&a.display_name, &b.display_name))
}

/// `HRESULT_FROM_WIN32(ERROR_NOT_FOUND)`: no endpoint for the requested role.
const E_NOTFOUND: i32 = 0x8007_0490_u32 as i32;
/// `E_FAIL`, reported for wasapi errors that carry no HRESULT.
const E_FAIL: i32 = 0x8000_4005_u32 as i32;

/// The HRESULT behind a wasapi error (`E_FAIL` when it has none).
pub(crate) fn hresult(error: &WasapiError) -> i32 {
    match error {
        WasapiError::Windows(e) | WasapiError::RegisterNotifications(e) => e.code().0,
        _ => E_FAIL,
    }
}

/// COM initialised as multi-threaded apartment on the current thread for the guard's
/// lifetime. Declare it before any COM object so it is dropped after them.
pub(crate) struct ComGuard {
    // CoUninitialize must run on the thread that initialised COM.
    _not_send: PhantomData<*const ()>,
}

impl ComGuard {
    pub(crate) fn mta() -> windows::core::Result<Self> {
        // S_FALSE (already initialised as MTA) is success and is balanced as well.
        wasapi::initialize_mta().ok()?;
        Ok(Self {
            _not_send: PhantomData,
        })
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        wasapi::deinitialize();
    }
}

/// Id of the default render endpoint for the Multimedia role (`None` when there is
/// no playback device at all).
pub(crate) fn default_render_id(
    enumerator: &DeviceEnumerator,
) -> Result<Option<String>, WasapiError> {
    match enumerator.get_default_device_for_role(&Direction::Render, &Role::Multimedia) {
        Ok(device) => device.get_id().map(Some),
        Err(WasapiError::Windows(e)) if e.code().0 == E_NOTFOUND => Ok(None),
        Err(e) => Err(e),
    }
}

/// The default render endpoint (Multimedia role) with its description.
pub(crate) fn default_render(
    enumerator: &DeviceEnumerator,
) -> Result<Option<(Device, DeviceInfo)>, WasapiError> {
    let device = match enumerator.get_default_device_for_role(&Direction::Render, &Role::Multimedia)
    {
        Ok(device) => device,
        Err(WasapiError::Windows(e)) if e.code().0 == E_NOTFOUND => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut info = describe(&device, None)?;
    info.is_default = true;
    Ok(Some((device, info)))
}

/// Describe one endpoint. `default_id` marks the default render endpoint.
pub(crate) fn describe(
    device: &Device,
    default_id: Option<&str>,
) -> Result<DeviceInfo, WasapiError> {
    let id = device.get_id()?;
    let name = device.get_friendlyname()?;
    let kind = match device.get_direction() {
        Direction::Render => DeviceKind::Loopback,
        Direction::Capture => DeviceKind::Input,
    };
    let (sample_rate, channels) = stream_format(device)?;
    Ok(DeviceInfo {
        is_default: kind == DeviceKind::Loopback && default_id == Some(id.as_str()),
        display_name: display_name(&name, kind),
        id,
        name,
        kind,
        sample_rate,
        channels,
    })
}

/// Shared-mode mix format rate and channels; falls back to the stored device format
/// when the endpoint cannot be activated. Activating a client does not start any
/// stream.
fn stream_format(device: &Device) -> Result<(u32, u16), WasapiError> {
    let mix = device
        .get_iaudioclient()
        .and_then(|client| client.get_mixformat());
    match mix {
        Ok(format) => Ok((format.get_samplespersec(), format.get_nchannels())),
        Err(mix_error) => match device.get_device_format() {
            Ok(format) => Ok((format.get_samplespersec(), format.get_nchannels())),
            Err(_) => Err(mix_error),
        },
    }
}

/// All active render endpoints (as loopbacks) and capture endpoints (as inputs),
/// in [`device_order`], with the devices themselves. Endpoints that cannot be
/// described (for example because they vanished mid-enumeration) are skipped.
pub(crate) fn enumerate_with_devices(
    enumerator: &DeviceEnumerator,
) -> Result<Vec<(Device, DeviceInfo)>, WasapiError> {
    let default_id = default_render_id(enumerator)?;
    let mut found = Vec::new();
    for direction in [Direction::Render, Direction::Capture] {
        let collection = enumerator.get_device_collection(&direction)?;
        for index in 0..collection.get_nbr_devices()? {
            let described = collection
                .get_device_at_index(index)
                .and_then(|device| describe(&device, default_id.as_deref()).map(|i| (device, i)));
            match described {
                Ok(pair) => found.push(pair),
                Err(e) => tracing::debug!("skipping {direction} endpoint {index}: {e}"),
            }
        }
    }
    found.sort_by(|(_, a), (_, b)| device_order(a, b));
    Ok(found)
}

/// All active endpoints, described and sorted (see [`crate::DeviceWatch::list`]).
pub(crate) fn enumerate(enumerator: &DeviceEnumerator) -> Result<Vec<DeviceInfo>, WasapiError> {
    Ok(enumerate_with_devices(enumerator)?
        .into_iter()
        .map(|(_, info)| info)
        .collect())
}

/// Resolve a configured endpoint: by `id` (when it is still active), otherwise by
/// [`find_device`] over the active endpoints.
pub(crate) fn resolve(
    enumerator: &DeviceEnumerator,
    id: Option<&str>,
    name: &str,
) -> Result<Option<(Device, DeviceInfo)>, WasapiError> {
    if let Some(id) = id.filter(|id| !id.is_empty()) {
        match enumerator.get_device(id) {
            Ok(device) if matches!(device.get_state(), Ok(DeviceState::Active)) => {
                let default_id = default_render_id(enumerator).unwrap_or(None);
                let info = describe(&device, default_id.as_deref())?;
                return Ok(Some((device, info)));
            }
            Ok(_) => tracing::debug!("endpoint {id} is not active; matching by name"),
            Err(e) => tracing::debug!("endpoint {id} not found ({e}); matching by name"),
        }
    }
    let candidates = enumerate_with_devices(enumerator)?;
    let infos: Vec<DeviceInfo> = candidates.iter().map(|(_, info)| info.clone()).collect();
    let Some(wanted) = find_device(&infos, None, name).map(|info| info.id.clone()) else {
        return Ok(None);
    };
    Ok(candidates.into_iter().find(|(_, info)| info.id == wanted))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(id: &str, name: &str, kind: DeviceKind) -> DeviceInfo {
        DeviceInfo {
            id: id.to_owned(),
            name: name.to_owned(),
            display_name: display_name(name, kind),
            kind,
            sample_rate: 48_000,
            channels: 2,
            is_default: false,
        }
    }

    fn sample_devices() -> Vec<DeviceInfo> {
        vec![
            dev(
                "{r-cable}",
                "CABLE Input (VB-Audio Virtual Cable)",
                DeviceKind::Loopback,
            ),
            dev(
                "{r-spk}",
                "Speakers (Realtek(R) Audio)",
                DeviceKind::Loopback,
            ),
            dev(
                "{c-cable}",
                "CABLE Output (VB-Audio Virtual Cable)",
                DeviceKind::Input,
            ),
            dev("{c-same}", "Speakers (Realtek(R) Audio)", DeviceKind::Input),
        ]
    }

    #[test]
    fn loopbacks_get_the_suffix() {
        assert_eq!(
            display_name("CABLE Input (VB-Audio Virtual Cable)", DeviceKind::Loopback),
            "CABLE Input (VB-Audio Virtual Cable) [Loopback]"
        );
        assert_eq!(display_name("Mic", DeviceKind::Input), "Mic");
    }

    #[test]
    fn id_match_wins() {
        let devices = sample_devices();
        let found = find_device(&devices, Some("{c-cable}"), "Speakers (Realtek(R) Audio)");
        assert_eq!(found.map(|d| d.id.as_str()), Some("{c-cable}"));
        // Endpoint ids are GUID strings; their case is not significant.
        let found = find_device(&devices, Some("{C-CABLE}"), "");
        assert_eq!(found.map(|d| d.id.as_str()), Some("{c-cable}"));
    }

    #[test]
    fn falls_back_to_display_name_then_name() {
        let devices = sample_devices();
        // Stale id, v0.1 config value with the loopback suffix.
        let found = find_device(
            &devices,
            Some("{gone}"),
            "cable input (vb-audio virtual cable) [LOOPBACK]",
        );
        assert_eq!(found.map(|d| d.id.as_str()), Some("{r-cable}"));
        // Display name of the input with the same friendly name as a render device:
        // the display-name pass finds the exact input.
        let found = find_device(&devices, None, "CABLE OUTPUT (VB-Audio Virtual Cable)");
        assert_eq!(found.map(|d| d.id.as_str()), Some("{c-cable}"));
        // Bare friendly name: display names do not match the loopback, so the
        // display-name pass finds the input "Speakers (...)" (whose display name is
        // the bare name) before the name pass would find the loopback.
        let found = find_device(&devices, None, "speakers (realtek(r) audio)");
        assert_eq!(found.map(|d| d.id.as_str()), Some("{c-same}"));
    }

    #[test]
    fn name_pass_finds_loopbacks_by_bare_name() {
        let devices = vec![dev("{r}", "Lautsprecher (Ä-Gerät)", DeviceKind::Loopback)];
        let found = find_device(&devices, None, "LAUTSPRECHER (ä-GERÄT)");
        assert_eq!(found.map(|d| d.id.as_str()), Some("{r}"));
        assert!(find_device(&devices, None, "Lautsprecher").is_none());
        assert!(find_device(&devices, Some(""), "nothing").is_none());
        assert!(find_device(&[], Some("{r}"), "x").is_none());
    }

    #[test]
    fn sorted_loopbacks_first_then_by_name() {
        let mut devices = [
            dev("1", "zeta mic", DeviceKind::Input),
            dev("2", "Speakers", DeviceKind::Loopback),
            dev("3", "alpha mic", DeviceKind::Input),
            dev("4", "cable input", DeviceKind::Loopback),
            dev("5", "Beta", DeviceKind::Input),
        ];
        devices.sort_by(device_order);
        let ids: Vec<&str> = devices.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, ["4", "2", "3", "5", "1"]);
    }

    #[test]
    fn hresult_of_errors() {
        let e = WasapiError::Windows(windows::core::Error::from_hresult(windows::core::HRESULT(
            0x8889_0004_u32 as i32,
        )));
        assert_eq!(hresult(&e), 0x8889_0004_u32 as i32);
        assert_eq!(hresult(&WasapiError::ClientNotInit), E_FAIL);
    }
}
