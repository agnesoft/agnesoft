use agplatform::Platform;
use agplatform::test_platform::test_platform;

#[test]
fn test_platform_should_expose_public_api() {
    agplatform::TestFs::new();
    agplatform::TestEnv::new();

    let mut platform = test_platform();
    platform.env();
    platform.env_mut();
    platform.fs();
    platform.fs_mut();
}
