use anyhow::{Context, Result};
use serial_test::serial;

use wash_lib::cli::output::StartCommandOutput;

mod common;
use common::{TestWashInstance, HTTP_JSONIFY_OCI_REF};

use crate::common::wait_for_no_hosts;

/// Ensure that wash call works
#[tokio::test(flavor = "multi_thread")]
#[serial]
#[cfg_attr(not(can_reach_github_com), ignore = "github.com is not reachable")]
async fn integration_call() -> Result<()> {
    wait_for_no_hosts()
        .await
        .context("unexpected wasmcloud instance(s) running")?;

    let instance = TestWashInstance::create().await?;

    // Pre-emptively pull the OCI ref for the component to ensure we don't run into the
    // default testing timeout when attempting to start the component
    let _ = instance
        .pull(HTTP_JSONIFY_OCI_REF)
        .await
        .context("failed to pull component")?;

    // Start an echo component
    let StartCommandOutput { component_id, .. } = instance
        .start_component(HTTP_JSONIFY_OCI_REF, "http-jsonify")
        .await
        .context("failed to start component")?;
    let component_id = component_id.context("component ID not present after starting component")?;

    // Wait a bit for the component to initialize
    tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;

    // Build request payload to send to the echo component
    let request = serde_json::json!({
        "method": "GET",
        "path": "/",
        "body": "",
        "queryString": "",
        "header": {},
    });

    // Call the component
    // let cmd_output = instance
    //     .call_component(
    //         &component_id,
    //         "wasi:http/incoming-handler.handle",
    //         serde_json::to_string(&request).context("failed to convert wash call data")?,
    //     )
    //     .await
    //     .context("failed to call component")?;

    if let Err(e) = instance
        .call_component(
            &component_id,
            "wasi:http/incoming-handler.handle",
            serde_json::to_string(&request).context("failed to convert wash call data")?,
        )
        .await
        .context("failed to call component")
    {
        eprintln!("Printing log output!");

        eprintln!(
            "LOGS\n===\n{}",
            tokio::fs::read_to_string("/home/runner/.wash/downloads/wasmcloud.log").await?
        );

        anyhow::bail!("failed");
    }

    // assert!(cmd_output.success, "call command succeeded");
    // assert_eq!(cmd_output.response["status"], 200, "status code is 200");

    Ok(())
}
