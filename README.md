# hub-core

### Commands

#### Build and Flash ESP32s3
```
cargo run --release --bin hub-core-fw --target xtensa-esp32s3-none-elf -Z build-std=core,alloc,compiler_builtins
```

#### Erase ESP32s3
```
espflash erase-flash --chip esp32s3
```

#### Connect to ESP32s3 (COM)
```
espflash monitor --baud 115200
```

#### Format code
```
cargo fmt
```

### BLE

Advertising data > Manufacturer data :

- Company ID = 0x1234
- Data = ``48 48 2D 50 52 4F 42 45 01 02 07 50 72 6F 62 65 2D 41``
```
48 48 2D 50 52 4F 42 45     01 02   07          50 72 6F 62 65 2D 41
H  H  -  P  R  O  B  E      v1.2    name_len    P  r  o  b  e  -  A (name)
```