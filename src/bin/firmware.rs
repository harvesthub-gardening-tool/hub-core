#![no_std]
#![no_main]

use esp_backtrace as _;
use esp_println::{print, println};

use esp_hal::clock::CpuClock;
use esp_hal::timer::timg::TimerGroup;

use embassy_time::{Duration, Timer};
use static_cell::StaticCell;

use bt_hci::controller::ExternalController;

use esp_radio::ble::controller::BleConnector;

use trouble_host::connection::{ConnectConfig, ConnectParams, PhySet, ScanConfig};
use trouble_host::prelude::{HostResources, Runner};

type MyController = ExternalController<BleConnector<'static>, 4>;
type MyPacketPool = trouble_host::prelude::DefaultPacketPool;
type MyRunner = Runner<'static, MyController, MyPacketPool>;

#[embassy_executor::task]
async fn ble_runner_task(mut runner: MyRunner) {
    if let Err(e) = runner.run().await {
        println!("BLE runner stopped: {:?}", e);
    }
}

fn init_heap_and_desc() {
    esp_bootloader_esp_idf::esp_app_desc!();
    // BLE needs real heap. 256k is a safe starting point.
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
    println!("BOOT OK");

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    // esp-radio needs esp-rtos + a timer
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0);

    // radio must be 'static for BleConnector<'static>
    static RADIO: StaticCell<esp_radio::Controller<'static>> = StaticCell::new();
    let radio: &'static esp_radio::Controller<'static> =
        RADIO.init(esp_radio::init().expect("esp_radio::init() failed"));

    // Create connector (we do raw scan first using HCI)
    let ble_cfg = esp_radio::ble::Config::default();
    let mut connector =
        BleConnector::new(radio, peripherals.BT, ble_cfg).expect("BleConnector::new() failed");

    // -------- Phase 1: scan & pick strongest (prints everything) --------
    let picked = match hub_core::hci_scan_pick_strongest(&mut connector, Duration::from_secs(6)).await
    {
        Some(p) => p,
        None => {
            println!("No devices seen.");
            loop {
                Timer::after(Duration::from_secs(2)).await;
            }
        }
    };

    println!("Picked strongest device:");
    println!("  addr={:?}", picked.addr);
    println!("  kind={:?}", picked.kind);
    println!("  rssi={}", picked.rssi);

    if let Some(flags) = picked.flags {
        println!("  flags=0x{:02X}", flags);
    }
    if let Some(name) = picked.name_str() {
        println!("  name={}", name);
    }
    if let Some(cid) = picked.mfg_company_id {
        println!("  mfg_company_id=0x{:04X}", cid);
        if !picked.mfg_bytes().is_empty() {
            hexdump("  mfg_data:", picked.mfg_bytes());
        }
    }
    if !picked.raw_adv_bytes().is_empty() {
        hexdump("  adv_raw:", picked.raw_adv_bytes());
    }

    // -------- Phase 2: move connector into Trouble and connect --------
    let controller: MyController = ExternalController::new(connector);

    static RES: StaticCell<HostResources<MyPacketPool, 1, 3, 1>> = StaticCell::new();
    let resources = RES.init(HostResources::new());

    static STACK: StaticCell<trouble_host::Stack<'static, MyController, MyPacketPool>> =
        StaticCell::new();
    let stack = STACK.init(trouble_host::new(controller, resources));

    let trouble_host::Host { mut central, runner, .. } = stack.build();
    spawner.spawn(ble_runner_task(runner)).ok();

    // Filter accept list: only the picked device
    let fal = [(picked.kind, &picked.addr)];

    loop {
        println!(
            "Connecting… addr={:?} kind={:?} rssi={}",
            picked.addr, picked.kind, picked.rssi
        );

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
                println!("Connected! handle={:?}", conn.handle());
                Timer::after(Duration::from_secs(5)).await;
                println!("Disconnecting…");
                conn.disconnect();
                Timer::after(Duration::from_secs(2)).await;
            }
            Err(e) => {
                println!("Connect failed: {:?}", e);
                Timer::after(Duration::from_secs(2)).await;
            }
        }
    }
}