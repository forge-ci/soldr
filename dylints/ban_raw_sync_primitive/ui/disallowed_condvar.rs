use std::sync::{Condvar, Mutex};

fn wait(pair: &(Mutex<bool>, Condvar)) {
    let (mutex, condvar) = pair;
    let guard = mutex.lock().unwrap();
    let _guard = condvar.wait(guard);
}

fn main() {
    let _ = wait;
}
