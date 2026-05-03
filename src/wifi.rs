use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sys::EspError;
use esp_idf_svc::wifi::{BlockingWifi, ClientConfiguration, Configuration, EspWifi};
use log::{error, info};
use std::thread;
use std::time::Duration;

// NVS partition is taken once in `main` and shared across subsystems
// (Wi-Fi calibration storage + hub-token persistence). Passing it in
// avoids the one-shot `EspDefaultNvsPartition::take()` failing on the
// second caller.
pub fn init(
    nvs_partition: EspDefaultNvsPartition,
) -> Result<BlockingWifi<EspWifi<'static>>, EspError> {
    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;

    let wifi = BlockingWifi::wrap(
        EspWifi::new(peripherals.modem, sys_loop.clone(), Some(nvs_partition))?,
        sys_loop,
    )?;

    Ok(wifi)
}

pub fn connect(
    ssid: &str,
    password: &str,
    wifi: &mut BlockingWifi<EspWifi<'static>>,
) -> anyhow::Result<()> {
    let wifi_configuration = Configuration::Client(ClientConfiguration {
        ssid: ssid.try_into()?,
        password: password.try_into()?,
        ..Default::default()
    });

    wifi.set_configuration(&wifi_configuration)?;
    wifi.start()?;
    info!("Wi-Fi started");

    let ap_infos = wifi.scan()?;
    info!("Found {} APs", ap_infos.len());

    for ap in &ap_infos {
        info!(
            "AP ssid='{}' channel={} rssi={} auth={:?}",
            ap.ssid, ap.channel, ap.signal_strength, ap.auth_method
        );
    }

    let found = ap_infos.iter().any(|ap| ap.ssid.as_str() == ssid);
    if !found {
        anyhow::bail!("Configured SSID '{}' not found in scan", ssid);
    }

    let mut last_err = None;

    for attempt in 1..=5 {
        info!("Connecting to SSID '{}' (attempt {})", ssid, attempt);

        match wifi.connect() {
            Ok(()) => {
                info!("Wi-Fi connected");
                wifi.wait_netif_up()?;
                info!("Wi-Fi netif up");
                info!(
                    "Wi-Fi DHCP info: {:?}",
                    wifi.wifi().sta_netif().get_ip_info()?
                );
                return Ok(());
            }
            Err(e) => {
                error!("Wi-Fi connect attempt {} failed: {:?}", attempt, e);
                last_err = Some(e);
                thread::sleep(Duration::from_secs(3));
            }
        }
    }

    Err(last_err.unwrap().into())
}
