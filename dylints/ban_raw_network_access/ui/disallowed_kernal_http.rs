// aux-build:kernal_api.rs

extern crate kernal_api;

fn main() {
    let _ = kernal_api::http::Client::new();
}
