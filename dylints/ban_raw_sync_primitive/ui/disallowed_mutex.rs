use std::sync::Mutex;

fn main() {
    let mutex = Mutex::new(0);
    let _guard = mutex.lock();
}
