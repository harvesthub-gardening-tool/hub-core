# hub-core

### Flash
```
cargo run --release --bin hub-core-fw --target xtensa-esp32s3-none-elf -Z build-std=core,compiler_builtins
```

### Erase flash
```
espflash erase-flash --chip esp32s3
```

### Connect to COM
```
espflash monitor --baud 115200
```
