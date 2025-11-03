#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_hal::{gpio::{Io, Level, Output, OutputConfig}, main};

#[esp_rtos::main] // provided by esp-rtos with feature "embassy"
async fn main(_spawner: Spawner) {
    // HAL 1.0 init
    let periph = esp_hal::init(esp_hal::Config::default());

    // Many ESP32 devkits have LED on GPIO2; adjust for your board if needed.
    let mut led = Output::new(periph.GPIO2, Level::High, OutputConfig::default());

    loop {
        led.toggle();
        Timer::after(Duration::from_millis(500)).await;
    }
}