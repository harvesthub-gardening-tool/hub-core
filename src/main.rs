#![no_std]
#![no_main]
extern crate alloc;

mod ble;
mod config;

use alloc::vec::Vec;
use core::cell::RefCell;

use esp_backtrace as _;
use esp_println::{print, println};

use esp_hal::clock::CpuClock;
use esp_hal::timer::timg::TimerGroup;

use embassy_futures::select::{select, Either};
use embassy_time::{Duration, Timer};
use static_cell::StaticCell;

use bt_hci::controller::ExternalController;
use bt_hci::param::{LeAdvEventKind, LeAdvReportsIter, LeExtAdvReportsIter};
use esp_radio::ble::controller::BleConnector;

use trouble_host::connection::{ConnectConfig, ConnectParams, PhySet, ScanConfig};
use trouble_host::prelude::{DefaultPacketPool, EventHandler, HostResources};
use trouble_host::scan::Scanner;
use trouble_host::Host;

use crate::ble::adv;
use config::{
    ENVIRONMENTAL_SENSING_SERVICE_UUID16, HUM_CHAR_UUID16, SCAN_WAIT_SECONDS, TEMP_CHAR_UUID16,
};

type MyController = ExternalController<BleConnector<'static>, 4>;
type MyPacketPool = DefaultPacketPool;
const MAX_GATT_SERVICES: usize = 8;
type MyGattClient<'a> =
    trouble_host::gatt::GattClient<'a, MyController, MyPacketPool, MAX_GATT_SERVICES>;

const RESCAN_SECONDS: u64 = 8;
const RESCAN_SLICE_MS: u64 = 250;
const RESCAN_ATTEMPTS: usize = ((RESCAN_SECONDS * 1000) / RESCAN_SLICE_MS) as usize;

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

fn adv_from_legacy_report(report: bt_hci::param::LeAdvReport<'_>) -> adv::AdvData {
    let (connectable, scannable, scan_response, legacy) = match report.event_kind {
        LeAdvEventKind::AdvInd => (true, true, false, true),
        LeAdvEventKind::AdvDirectInd => (true, false, false, true),
        LeAdvEventKind::AdvScanInd => (false, true, false, true),
        LeAdvEventKind::AdvNonconnInd => (false, false, false, true),
        LeAdvEventKind::ScanRsp => (false, false, true, true),
    };

    let mut adv = adv::AdvData::blank_adv_data(
        report.addr_kind,
        report.addr,
        report.rssi,
        connectable,
        scannable,
        scan_response,
        legacy,
    );

    adv.parse_ad_payload(report.data, scan_response);
    adv
}

fn adv_from_ext_report(report: bt_hci::param::LeExtAdvReport<'_>) -> adv::AdvData {
    let event_kind = report.event_kind;

    let mut adv = adv::AdvData::blank_adv_data(
        report.addr_kind,
        report.addr,
        report.rssi,
        event_kind.connectable(),
        event_kind.scannable(),
        event_kind.scan_response(),
        event_kind.legacy(),
    );

    adv.parse_ad_payload(report.data, event_kind.scan_response());
    adv
}

struct ProbeInventory {
    devices: RefCell<Vec<adv::AdvData>>,
}

impl ProbeInventory {
    fn new() -> Self {
        Self {
            devices: RefCell::new(Vec::new()),
        }
    }

    fn upsert(&self, incoming: adv::AdvData) {
        let mut devices = self.devices.borrow_mut();

        if let Some(existing) = devices.iter_mut().find(|d| d.addr == incoming.addr) {
            let was_probe = existing.is_probe();
            existing.merge_from(&incoming);

            if !was_probe && existing.is_probe() {
                println!("[SCAN] New probe discovered:");
                existing.print();
                println!();
            }

            return;
        }

        let is_probe = incoming.is_probe();

        if is_probe {
            println!("[SCAN] New probe discovered:");
            incoming.print();
            println!();
        }

        devices.push(incoming);
    }

    fn probes_snapshot(&self) -> Vec<adv::AdvData> {
        self.devices
            .borrow()
            .iter()
            .copied()
            .filter(|d| d.is_probe())
            .collect()
    }
}

impl EventHandler for ProbeInventory {
    fn on_adv_reports(&self, reports: LeAdvReportsIter<'_>) {
        for report in reports {
            let Ok(report) = report else {
                continue;
            };

            self.upsert(adv_from_legacy_report(report));
        }
    }

    fn on_ext_adv_reports(&self, reports: LeExtAdvReportsIter<'_>) {
        for report in reports {
            let Ok(report) = report else {
                continue;
            };

            self.upsert(adv_from_ext_report(report));
        }
    }
}

#[esp_rtos::main]
async fn main(_spawner: embassy_executor::Spawner) -> ! {
    init_heap_and_desc();

    esp_println::logger::init_logger_from_env();
    println!("[BOOT] OK");

    let config_hw = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config_hw);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0);

    static RADIO: StaticCell<esp_radio::Controller<'static>> = StaticCell::new();
    let radio: &'static esp_radio::Controller<'static> =
        RADIO.init(esp_radio::init().expect("esp_radio::init() failed"));

    let ble_cfg = esp_radio::ble::Config::default();
    let connector =
        BleConnector::new(radio, peripherals.BT, ble_cfg).expect("BleConnector::new() failed");

    let controller: MyController = ExternalController::new(connector);

    static RES: StaticCell<HostResources<MyPacketPool, 1, 3, 1>> = StaticCell::new();
    let resources = RES.init(HostResources::new());

    static STACK: StaticCell<trouble_host::Stack<'static, MyController, MyPacketPool>> =
        StaticCell::new();
    let stack = STACK.init(trouble_host::new(controller, resources));

    let Host {
        mut central,
        mut runner,
        ..
    } = stack.build();

    let inventory = ProbeInventory::new();

    match select(
        runner.run_with_handler(&inventory),
        async {
            loop {
                println!("[SCAN] Scanning probes...");

                let mut scanner = Scanner::new(central);

                let scan_cfg = ScanConfig {
                    active: true,
                    filter_accept_list: &[],
                    phys: PhySet::M1,
                    interval: Duration::from_millis(100),
                    window: Duration::from_millis(50),
                    timeout: Duration::from_secs(RESCAN_SECONDS),
                };

                for _ in 0..RESCAN_ATTEMPTS {
                    match scanner.scan_ext(&scan_cfg).await {
                        Ok(session) => {
                            // On garde la session un petit moment pour laisser remonter les reports
                            Timer::after(Duration::from_millis(RESCAN_SLICE_MS)).await;

                            // Ici le drop déclenche l’arrêt du scan
                            drop(session);

                            // Très important: on laisse la stack traiter réellement l’arrêt
                            Timer::after(Duration::from_millis(120)).await;
                        }
                        Err(e) => {
                            println!("[SCAN] scan_ext failed: {:?}", e);
                            Timer::after(Duration::from_millis(200)).await;
                        }
                    }
                }

                central = scanner.into_inner();

                let probes = inventory.probes_snapshot();
                println!("[SCAN] Known probes: {}", probes.len());

                if probes.is_empty() {
                    println!(
                        "[HUB] No probe known yet. Waiting {}s before next cycle...",
                        SCAN_WAIT_SECONDS
                    );
                    Timer::after(Duration::from_secs(SCAN_WAIT_SECONDS)).await;
                    continue;
                }

                // Petite marge supplémentaire avant tout connect
                Timer::after(Duration::from_millis(200)).await;
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

                    Timer::after(Duration::from_millis(150)).await;

                    match central.connect_ext(&cfg).await {
                        Ok(conn) => {
                            println!("[HUB] Connected handle={:?}", conn.handle());

                            println!("[GATT] Creating client...");
                            match MyGattClient::new(stack, &conn).await {
                                Ok(gatt) => {
                                    println!("[GATT] Client ready.");

                                    let do_reads = async {
                                        println!("[GATT] Reading values...");

                                        let svc = ENVIRONMENTAL_SENSING_SERVICE_UUID16;
                                        let temp_uuid = TEMP_CHAR_UUID16;
                                        let hum_uuid = HUM_CHAR_UUID16;

                                        match select(
                                            Timer::after(Duration::from_secs(3)),
                                            ble::gatt_client::read_probe_data(&gatt, svc, temp_uuid, hum_uuid),
                                        )
                                            .await
                                        {
                                            Either::First(_) => {
                                                println!("[GATT] Read timeout");
                                            }
                                            Either::Second(Some(r)) => {
                                                if let Some(t) = r.temperature_c_x100 {
                                                    println!("temp={:.2}C", t as f32 / 100.0);
                                                } else {
                                                    println!("temp=<missing>");
                                                }

                                                if let Some(h) = r.humidity_pct_x100 {
                                                    println!("hum={:.2}%", h as f32 / 100.0);
                                                } else {
                                                    println!("hum=<missing>");
                                                }
                                            }
                                            Either::Second(None) => {
                                                println!("[GATT] Service/characteristic missing or read failed");
                                            }
                                        }
                                    };

                                    match select(gatt.task(), do_reads).await {
                                        Either::First(res) => match res {
                                            Ok(()) => println!("[GATT] task ended"),
                                            Err(e) => println!("[GATT] task error: {:?}", e),
                                        },
                                        Either::Second(()) => {}
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

                println!(
                    "[HUB] Waiting {}s before next cycle...",
                    SCAN_WAIT_SECONDS
                );
                Timer::after(Duration::from_secs(SCAN_WAIT_SECONDS)).await;
            }
        },
    )
        .await
    {
        Either::First(res) => {
            if let Err(e) = res {
                println!("[BLE] runner stopped: {:?}", e);
            }

            loop {
                Timer::after(Duration::from_secs(60)).await;
            }
        }
        Either::Second(never) => match never {
            _ => todo!(),
        },
    }
}
