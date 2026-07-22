use std::env;

fn main() {
    println!("cargo:rerun-if-env-changed=STELLA_BUILD_GIT_SHA");

    let package_version =
        env::var("CARGO_PKG_VERSION").expect("Cargo must provide CARGO_PKG_VERSION");
    let build_version = match env::var("STELLA_BUILD_GIT_SHA") {
        Ok(sha) if !sha.is_empty() => {
            assert!(
                sha.is_ascii() && !sha.bytes().any(|byte| matches!(byte, b'\r' | b'\n')),
                "STELLA_BUILD_GIT_SHA must be ASCII and must not contain a newline"
            );
            format!("{package_version}-dev.{sha}")
        }
        _ => package_version,
    };

    println!("cargo:rustc-env=STELLA_BUILD_VERSION={build_version}");
}
