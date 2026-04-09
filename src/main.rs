#![no_std]
#![no_main]
extern crate alloc;

mod ble;
mod config;

use alloc::vec::Vec;

use esp_backtrace as _;
use esp_println::{print, println};

use esp_hal::clock::CpuClock;
use esp_hal::timer::timg::TimerGroup;

use embassy_futures::select::{select, Either};
use embassy_time::{Duration, Timer};
use static_cell::StaticCell;

use bt_hci::controller::ExternalController;
use esp_radio::ble::controller::BleConnector;

use trouble_host::connection::{ConnectConfig, ConnectParams, PhySet, ScanConfig};
use trouble_host::prelude::{DefaultPacketPool, HostResources, Runner};
use trouble_host::Host;

use crate::ble::helpers::uuid16_to_uuid128;
use config::{
    ENVIRONMENTAL_SENSING_SERVICE_UUID16, HUM_CHAR_UUID16, SCAN_WAIT_SECONDS, TEMP_CHAR_UUID16,
};

type MyController = ExternalController<BleConnector<'static>, 4>;
type MyPacketPool = DefaultPacketPool;
type MyRunner = Runner<'static, MyController, MyPacketPool>;

// choose something small but > number of services you’ll ever care about
const MAX_GATT_SERVICES: usize = 8;
type MyGattClient<'a> =
    trouble_host::gatt::GattClient<'a, MyController, MyPacketPool, MAX_GATT_SERVICES>;

#[embassy_executor::task]
async fn ble_runner_task(mut runner: MyRunner) {
    if let Err(e) = runner.run().await {
        println!("[BLE] runner stopped: {:?}", e);
    }
}

fn init_heap_and_desc() {
    esp_bootloader_esp_idf::esp_app_desc!();
    esp_alloc::heap_allocator!(size: 256 * 1024);
}

fn hexdump(prefix: &str, data: &[u8]) {
    print!("{prefix}");
    for b in data {
        print!(" {:02X}", b);
    }
    println!();
}

#[esp_rtos::main]
async fn main(spawner: embassy_executor::Spawner) -> ! {
    init_heap_and_desc();

    esp_println::logger::init_logger_from_env();
    println!("[BOOT] OK");

    let config_hw = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config_hw);

    // esp-radio needs esp-rtos + a timer
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0);

    // radio must be 'static for BleConnector<'static>
    static RADIO: StaticCell<esp_radio::Controller<'static>> = StaticCell::new();
    let radio: &'static esp_radio::Controller<'static> =
        RADIO.init(esp_radio::init().expect("esp_radio::init() failed"));

    // Create connector (raw HCI scan)
    let ble_cfg = esp_radio::ble::Config::default();
    let mut connector =
        BleConnector::new(radio, peripherals.BT, ble_cfg).expect("BleConnector::new() failed");

    // ------------------ Phase 1: RAW HCI scan (ONCE) ------------------
    println!("[SCAN] Raw HCI scanning probes...");
    let probes: Vec<ble::adv::AdvData> =
        ble::hci::hci_scan_probes(&mut connector, Duration::from_secs(8)).await;

    println!("[SCAN] Found {} probe(s)", probes.len());
    for (i, p) in probes.iter().enumerate() {
        println!(">> ({})", i + 1);
        p.print();
        println!();
    }

    if probes.is_empty() {
        println!("[HUB] No probes found at boot. Will just idle forever.");
        loop {
            Timer::after(Duration::from_secs(SCAN_WAIT_SECONDS)).await;
        }
    }

    // ------------------ Phase 2: Move controller into Trouble (ONCE) ------------------
    let controller: MyController = ExternalController::new(connector);

    static RES: StaticCell<HostResources<MyPacketPool, 1, 3, 1>> = StaticCell::new();
    let resources = RES.init(HostResources::new());

    static STACK: StaticCell<trouble_host::Stack<'static, MyController, MyPacketPool>> =
        StaticCell::new();
    let stack = STACK.init(trouble_host::new(controller, resources));

    let Host {
        mut central,
        runner,
        ..
    } = stack.build();
    spawner.spawn(ble_runner_task(runner)).ok();

    // ------------------ Phase 3: loop forever (reconnect + read) ------------------
    loop {
        println!("[HUB] Polling {} probe(s)...", probes.len());

        for p in probes.iter() {
            if !p.connectable {
                println!("[HUB] Skip non-connectable addr={:?}", p.addr);
                continue;
            }

            println!(
                "[HUB] Connecting addr={:?} kind={:?} rssi={}",
                p.addr, p.kind, p.rssi
            );

            // Filter accept list: only this device
            let fal = [(p.kind, &p.addr)];

            let scan_cfg = ScanConfig {
                active: true,
                filter_accept_list: &fal,
                phys: PhySet::M1,
                interval: Duration::from_millis(100),
                window: Duration::from_millis(50),
                timeout: Duration::from_secs(10),
            };

            let cfg = ConnectConfig {
                scan_config: scan_cfg,
                connect_params: ConnectParams::default(),
            };

            match central.connect(&cfg).await {
                Ok(conn) => {
                    println!("[HUB] Connected handle={:?}", conn.handle());

                    println!("[GATT] Creating client...");
                    match MyGattClient::new(stack, &conn).await {
                        Ok(gatt) => {
                            println!("[GATT] Client ready.");

                            // IMPORTANT:
                            // Drive gatt.task() concurrently, otherwise reads can time out
                            // because responses aren't being processed.
                            let do_reads = async {
                                println!("[GATT] Reading values...");

                                let svc = uuid16_to_uuid128(ENVIRONMENTAL_SENSING_SERVICE_UUID16);
                                let char_list = &[
                                    uuid16_to_uuid128(TEMP_CHAR_UUID16),
                                    uuid16_to_uuid128(HUM_CHAR_UUID16),
                                ];

                                if let Some(r) =
                                    ble::gatt_client::read_probe_data_list(&gatt, svc, char_list)
                                        .await
                                {
                                    if let Some(t) = r.temperature_c_x100 {
                                        println!("temp={:.2}C", t as f32 / 100.0);
                                    }
                                    if let Some(h) = r.humidity_pct_x100 {
                                        println!("hum={:.2}%", h as f32 / 100.0);
                                    }
                                } else {
                                    println!("[GATT] Read returned None");
                                }
                            };

                            match select(gatt.task(), do_reads).await {
                                Either::First(res) => {
                                    // gatt.task() ended first (disconnect or error)
                                    match res {
                                        Ok(()) => println!("[GATT] task ended"),
                                        Err(e) => println!("[GATT] task error: {:?}", e),
                                    }
                                }
                                Either::Second(()) => {
                                    // reads finished; gatt.task() is dropped here
                                }
                            }
                        }
                        Err(e) => println!("[GATT] GattClient::new failed: {:?}", e),
                    }

                    println!("[HUB] Disconnect");
                    conn.disconnect();
                    Timer::after(Duration::from_secs(1)).await;
                }
                Err(e) => {
                    println!("[HUB] Connect failed: {:?}", e);
                    Timer::after(Duration::from_secs(2)).await;
                }
            }
        }

        println!("[HUB] Waiting {}s before next poll...", SCAN_WAIT_SECONDS);
        Timer::after(Duration::from_secs(SCAN_WAIT_SECONDS)).await;
    }
}
