use agplatform::Platform;

#[cfg(feature = "testing")]
mod test_platform;

#[test]
fn platform() {
    let platform = agplatform::platform();
    let _env = platform.env();
}
