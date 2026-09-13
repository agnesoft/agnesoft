use agplatform::Platform;
use agplatform::test_platform::test_platform;

#[test]
fn test_platform_api() {
    agplatform::TestFs::new();
    agplatform::TestEnv::new();

    let mut platform = test_platform();
    platform.env();
    platform.env_mut();
    platform.fs();
    platform.fs_mut();
}
