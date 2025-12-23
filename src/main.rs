fn main() {
    println!("Hello from host (PC)!");
    let value = hub_core::core_logic();
    println!("core_logic() returned {value}");
}
