# hub-core

### 1. Commands

- Build and Flash ESP32s3
```
cargo run --release --bin hub-core-fw --target xtensa-esp32s3-none-elf -Z build-std=core,alloc,compiler_builtins
```

- Erase ESP32s3
```
espflash erase-flash --chip esp32s3
```

- Connect to ESP32s3 (COM)
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

- ``<MARKER>``: TEST = 54 45 53 54
- ``<VERSION>``: ``<major>``.``<minor>``: 1.2 = 01 02
- ``<NAME_LEN>``: 7 = 07
- ``<NAME>``: Probe-A = 50 72 6F 62 65 2D 41

HEX: ``54 45 53 54 01 02 07 50 72 6F 62 65 2D 41``