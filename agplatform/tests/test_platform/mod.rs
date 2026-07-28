use agplatform::Platform;
use agplatform::test_platform::test_platform;

#[test]
fn test_platform_() {
    let platform = test_platform();
    let _env = platform.env();
}
