# hub-core

### 1. Commands

````powershell
$env:WIFI_SSID = "YOUR_WIFI"
$env:WIFI_PASSWORD = "YOUR_WIFI_PASSWORD"
````
````
cargo +esp check
````
````
cargo +esp run
````

During `cargo +esp check` / `cargo +esp run`, the build script prints the hub
identity and a ready-to-open setup URI:

```text
warning: probe-grpc@0.1.0: HarvestHub setup hub_uuid=...
warning: probe-grpc@0.1.0: HarvestHub setup hub_secret=...
warning: probe-grpc@0.1.0: HarvestHub setup uri=harvesthub://hub-setup?hub_uuid=...&hub_secret=...&hub_name=HarvestHub-Dev
```

The generated identity is stored at `target/harvesthub-dev-identity.env` and is
embedded into the flashed firmware, then seeded into NVS on first boot. Delete
that file to generate a new identity, or set `HUB_DEVICE_ID`, `HUB_SECRET`, and
optionally `HUB_NAME` before building to force specific values.

- ESP
```
espflash erase-flash --chip esp32s3
```
```
espflash monitor --baud 115200
```

- Format code
```
cargo fmt
```

### 2. BLE

![image](assets/img/schema_ble.png)

- [GATT Services specifications](https://www.bluetooth.com/wp-content/uploads/Files/Specification/HTML/Assigned_Numbers/out/en/Assigned_Numbers.pdf)
- [GATT Characteristics specifications](https://btprodspecificationrefs.blob.core.windows.net/gatt-specification-supplement/GATT_Specification_Supplement.pdf)

#### HarvestHub ADV Manufacturer data (filter):

- ``<MARKER>``: HH-PROBE = 48 48 2D 50 52 4F 42 45, legacy TEST = 54 45 53 54
- ``<VERSION>``: ``<major>``.``<minor>``: 1.2 = 01 02
- ``<NAME_LEN>``: 7 = 07
- ``<NAME>``: Probe-A = 50 72 6F 62 65 2D 41

HEX: ``54 45 53 54 01 02 07 50 72 6F 62 65 2D 41``

#### Probe Environmental Sensing characteristics

All values are read from the Environmental Sensing service
(`0000181a-0000-1000-8000-00805f9b34fb`) and converted to `f64` before uplink:

| Metric | Characteristic UUID | Wire type | Application unit |
| --- | --- | --- | --- |
| Air temperature | `00002a6e-0000-1000-8000-00805f9b34fb` | `i16` big-endian centi-°C | °C |
| Air pressure | `00002a6d-0000-1000-8000-00805f9b34fb` | `u32` big-endian pascals | Pa |
| Air humidity | `00002a6f-0000-1000-8000-00805f9b34fb` | `u16` big-endian centi-% | % |
| Probe UUID | `12340002-0000-1000-8000-00805f9b34fb` | 36-byte ASCII UUID | probe node id |
| Soil temperature | `12340003-0000-1000-8000-00805f9b34fb` | `i16` big-endian centi-°C | °C |
| Soil humidity | `12340004-0000-1000-8000-00805f9b34fb` | `u16` big-endian centi-% | % |
