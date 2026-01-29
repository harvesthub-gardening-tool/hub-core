# hub-core

### Flash
```
cargo run --release --bin hub-core-fw -Z build-std=core,compiler_builtins
```

### Erase flash
```
espflash erase-flash --chip esp32s3
```

### Connect to COM
```
espflash monitor -p COM8 --baud 115200
```
