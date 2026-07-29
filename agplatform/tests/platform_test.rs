use agplatform::Platform;

#[cfg(feature = "testing")]
mod test_platform_test;

#[test]
fn public_types() {
    let platform = agplatform::platform();
    let _env = platform.env();
    let _error = agplatform::Error::env("some error");
}
