# hub-core

## 1. Localhost

```
cargo run --bin hub-core-host
```

## 2. Firmware

- Build:
```
cargo build 
    --bin hub-core-fw 
    --target xtensa-esp32-none-elf 
    --no-default-features 
    --features firmware 
    --profile dev
```
- Run:
```
cargo run
    --bin hub-core-fw
    --target xtensa-esp32-none-elf
    --no-default-features
    --features firmware
    --profile dev
```