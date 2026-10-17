use std::sync::RwLock;

fn main() {
    let lock = RwLock::new(0);
    let _read = lock.read();
    let _write = lock.try_write();
}
