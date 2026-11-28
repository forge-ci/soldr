//! Compile-only consumer: an application using a kernal-api runtime
//! capability while its build script embeds Windows resources.

use kernal_api::platform::window_icon::{icon_support, IconScope};

fn main() {
    let _ = icon_support(IconScope::Host);
}
