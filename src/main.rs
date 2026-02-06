#![no_std]
#![no_main]
extern crate alloc;

mod ble;
mod config;
use crate::config::SCAN_WAIT_SECONDS;

use alloc::vec::Vec;

use esp_backtrace as _;
use esp_println::{print, println};

use esp_hal::clock::CpuClock;
use esp_hal::timer::timg::TimerGroup;

use embassy_time::{Duration, Timer};
use static_cell::StaticCell;

use esp_radio::ble::controller::BleConnector;

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
async fn main(_spawner: embassy_executor::Spawner) -> ! {
    init_heap_and_desc();

    esp_println::logger::init_logger_from_env();
    println!("[BOOT] OK");

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

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

    loop {
        println!("[HUB] Start scan");

        println!("[SCAN] Scanning probes...");
        let probes: Vec<ble::adv::AdvData> =
            ble::hci::hci_scan_probes(&mut connector, Duration::from_secs(8)).await;

        println!("[SCAN] Found {} probe(s):", probes.len());
        for (i, p) in probes.iter().enumerate() {
            println!(">> ({}):", i + 1);
            p.print();
            println!();
        }

        println!("[HUB] Waiting {}s before next scan...", SCAN_WAIT_SECONDS);
        Timer::after(Duration::from_secs(SCAN_WAIT_SECONDS)).await;
    }
}
