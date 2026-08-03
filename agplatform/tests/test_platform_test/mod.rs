use agplatform::Platform;
use agplatform::test_platform::test_platform;

#[test]
fn test_platform_api() {
    let mut platform = test_platform();
    let _ = platform.env();
    let _ = platform.env_mut();
}
