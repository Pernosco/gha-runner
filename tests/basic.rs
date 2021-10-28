use tokio::runtime;

use gha_runner::*;

/// `cargo test` exits with code 101 when tests fail
const CARGO_TEST_FAILURE_EXIT_CODE: i32 = 101;

#[test]
fn basic() {
    env_logger::init();

    // "Medium sized" images
    let images: DockerImageMapping = DockerImageMapping {
        ubuntu_18_04: "ghcr.io/catthehacker/ubuntu:act-18.04".into(),
        ubuntu_20_04: "ghcr.io/catthehacker/ubuntu:act-20.04".into(),
    };

    let runtime = runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async move {
        match run_workflow_with_local_backend(
            "Pernosco",
            "github-actions-test",
            "6475d0f048a72996e3bd559cdd3763f53fe3d072",
            ".github/workflows/build.yml",
            "Build+test (stable, ubuntu-18.04)",
            &images,
            LocalDockerOptions::default(),
        )
        .await
        {
            WorkflowResult::AllStepsPassed => panic!("Expected test failure"),
            WorkflowResult::StepFailed {
                step_name,
                exit_code,
            } => {
                assert_eq!(
                    step_name,
                    Some("cargo test (debug; default features)".to_string())
                );
                assert_eq!(exit_code, CARGO_TEST_FAILURE_EXIT_CODE);
            }
        }
    });
}
