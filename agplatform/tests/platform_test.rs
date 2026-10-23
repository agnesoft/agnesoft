use agplatform::Platform;

#[cfg(feature = "testing")]
mod test_platform_test;

mod platform {
    use super::*;

    #[test]
    fn platform_should_expose_public_api() {
        agplatform::Error::fs("fs error");
        agplatform::Error::env("env error");
        agplatform::Error::process("process error");
        agplatform::Error::request("request error");

        let mut platform = agplatform::platform();
        platform.env();
        platform.env_mut();
        platform.fs();
        platform.fs_mut();
        platform.process();
        platform.process_mut();
        platform.request();
    }
}
