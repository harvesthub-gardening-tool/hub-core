#![no_std]
#![no_main]

use esp_backtrace as _;
use esp_hal::{clock::CpuClock, delay::Delay};
use esp_println::println;

esp_bootloader_esp_idf::esp_app_desc!();

#[esp_hal::main]
fn main() -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let _peripherals = esp_hal::init(config);

    esp_println::logger::init_logger_from_env();
    println!("BOOT OK: running firmware!");

    let d = Delay::new();
    loop {
        let value = hub_core::core_logic();
        println!("Hello from ESP32-S3 firmware! core_logic() = {value}");
        d.delay_millis(500u32);
    }
}