use agplatform::CommandExt;
use agplatform::CommandOutput;
use agplatform::Platform;
use agplatform::test_platform::test_platform;

mod test_platform {
    use super::*;

    #[test]
    fn test_platform_should_expose_public_api() {
        agplatform::TestFs::new();
        agplatform::TestEnv::new();
        agplatform::TestProcess::new();
        agplatform::TestRequest::new();

        let mut platform = test_platform()
            .with_process(agplatform::TestProcess::new())
            .with_request(agplatform::TestRequest::new());
        platform.env();
        platform.env_mut();
        platform.fs();
        platform.fs_mut();
        platform.process();
        platform.process_mut();
        platform.request();
    }

    #[test]
    fn test_platform_should_run_commands() {
        let platform = test_platform();
        platform.process.set_output(
            "git",
            CommandOutput {
                status: Some(0),
                ..Default::default()
            },
        );

        std::process::Command::new("git")
            .run(platform.process())
            .unwrap();

        assert_eq!(platform.process.calls(), ["git"]);
    }
}
