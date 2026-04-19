use std::env;

fn main() {
    let profile = env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    if profile == "release" {
        println!("cargo:rustc-env=LOG_LEVEL=INFO");
    }else {
        println!("cargo:rustc-env=LOG_LEVEL=DEBUG");
    }
}
