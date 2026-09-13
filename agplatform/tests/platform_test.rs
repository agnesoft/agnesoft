use agplatform::Platform;

#[cfg(feature = "testing")]
mod test_platform_test;

#[test]
fn public_api() {
    agplatform::Error::fs("fs error");
    agplatform::Error::env("env error");

    let mut platform = agplatform::platform();
    platform.env();
    platform.env_mut();
    platform.fs();
    platform.fs_mut();
}
