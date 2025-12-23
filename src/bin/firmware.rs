#![cfg_attr(target_arch = "xtensa", no_std)]
#![cfg_attr(target_arch = "xtensa", no_main)]

#[cfg(target_arch = "xtensa")]
use core::panic::PanicInfo;

// --- Panic handler seulement côté ESP (xtensa) ---
#[cfg(target_arch = "xtensa")]
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {}
}

// --- Firmware réel : seulement pour la cible xtensa-esp32-none-elf ---
#[cfg(target_arch = "xtensa")]
mod fw {
    use embassy_executor::Spawner;
    use embassy_time::{Duration, Timer};

    use esp_hal::{clock::CpuClock, timer::timg::TimerGroup};

    use log::info;

    #[esp_rtos::main]
    async fn main(_spawner: Spawner) -> ! {
        // Config CPU + init HAL
        let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
        let peripherals = esp_hal::init(config);

        // Timer matériel pour esp-rtos / embassy-time
        let timg0 = TimerGroup::new(peripherals.TIMG0);
        esp_rtos::start(timg0.timer0);

        // Logger via esp-println
        esp_println::logger::init_logger_from_env();

        loop {
            let value = hub_core::core_logic();
            info!("Hello from firmware (Embassy)! core_logic() = {value}");

            // Sleep async 500 ms
            Timer::after(Duration::from_millis(500)).await;
        }
    }
}

#[cfg(not(target_arch = "xtensa"))]
fn main() {
    // Ce binaire n’a pas vocation à tourner sur PC.
}
