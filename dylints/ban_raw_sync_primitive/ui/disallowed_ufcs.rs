use std::sync::Mutex;

fn main() {
    let mutex = Mutex::new(0);
    let _guard = Mutex::lock(&mutex);
    let lock_fn = Mutex::<i32>::try_lock;
    let _ = lock_fn(&mutex);
}
