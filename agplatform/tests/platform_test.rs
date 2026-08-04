use agplatform::Platform;

#[cfg(feature = "testing")]
mod test_platform_test;

#[test]
fn public_api() {
    let mut platform = agplatform::platform();
    let _ = platform.env();
    let _ = platform.env_mut();
    let _error = agplatform::Error::io("some error");
}
