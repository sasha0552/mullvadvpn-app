mod access_methods;
mod account;
mod audits;
pub mod config;
mod daita;
mod dns;
mod helpers;
mod install;
mod relay_ip_overrides;
mod settings;
mod software;
mod split_tunnel;
mod test_metadata;
mod tunnel;
mod tunnel_state;
mod ui;

use itertools::Itertools;
pub use test_metadata::TestMetadata;

use anyhow::Context;
use futures::future::BoxFuture;
use std::time::Duration;

use crate::{
    mullvad_daemon::{MullvadClientArgument, RpcClientProvider},
    package::get_version_from_path,
};
use config::TEST_CONFIG;
use helpers::{get_app_env, install_app};
pub use install::test_upgrade_app;
use mullvad_management_interface::MullvadProxyClient;
use test_rpc::{meta::Os, ServiceClient};

const WAIT_FOR_TUNNEL_STATE_TIMEOUT: Duration = Duration::from_secs(40);

#[derive(Clone)]
pub struct TestContext {
    pub rpc_provider: RpcClientProvider,
}

pub type TestWrapperFunction =
    fn(TestContext, ServiceClient, MullvadClientArgument) -> BoxFuture<'static, anyhow::Result<()>>;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("RPC call failed")]
    Rpc(#[from] test_rpc::Error),

    #[error("geoip lookup failed")]
    GeoipLookup(#[source] test_rpc::Error),

    #[error("Found running daemon unexpectedly")]
    DaemonRunning,

    #[error("Daemon unexpectedly not running")]
    DaemonNotRunning,

    #[error("The daemon returned an error: {0}")]
    Daemon(String),

    #[error("The daemon ended up in the the wrong tunnel-state: {0:?}")]
    UnexpectedTunnelState(Box<mullvad_types::states::TunnelState>),

    #[error("The daemon ended up in the error state: {0:?}")]
    UnexpectedErrorState(talpid_types::tunnel::ErrorState),

    #[error("The gRPC client ran into an error: {0}")]
    ManagementInterface(#[from] mullvad_management_interface::Error),

    #[error("GUI test binary missing")]
    MissingGuiTest,

    #[cfg(target_os = "macos")]
    #[error("An error occurred: {0}")]
    Other(String),
}

#[derive(Clone)]
/// An abbreviated version of [`TestMetadata`]
pub struct TestDescription {
    pub name: &'static str,
    pub targets: &'static [Os],
    pub priority: Option<i32>,
}

pub fn should_run_on_os(targets: &[Os], os: Os) -> bool {
    targets.is_empty() || targets.contains(&os)
}

/// Get a list of all tests, sorted by priority.
pub fn get_test_descriptions() -> Vec<TestDescription> {
    let tests: Vec<_> = inventory::iter::<TestMetadata>()
        .map(|test| TestDescription {
            priority: test.priority,
            name: test.name,
            targets: test.targets,
        })
        .sorted_by_key(|test| test.priority)
        .collect_vec();

    // Since `test_upgrade_app` is not registered with inventory, we need to add it manually
    let test_upgrade_app = TestDescription {
        priority: None,
        name: "test_upgrade_app",
        targets: &[],
    };
    [vec![test_upgrade_app], tests].concat()
}

/// Return all tests with names matching the input argument. Filters out tests that are skipped for
/// the target platform and `test_upgrade_app`, which is run separately.
pub fn get_filtered_tests(specified_tests: &[String]) -> Result<Vec<TestMetadata>, anyhow::Error> {
    let mut tests: Vec<_> = inventory::iter::<TestMetadata>().cloned().collect();
    tests.sort_by_key(|test| test.priority.unwrap_or(0));

    let mut tests = if specified_tests.is_empty() {
        // Keep all tests
        tests
    } else {
        specified_tests
            .iter()
            .map(|f| {
                tests
                    .iter()
                    .find(|t| t.name.eq_ignore_ascii_case(f))
                    .cloned()
                    .ok_or(anyhow::anyhow!("Test '{f}' not found"))
            })
            .collect::<Result<_, anyhow::Error>>()?
    };
    tests.retain(|test| should_run_on_os(test.targets, TEST_CONFIG.os));
    Ok(tests)
}

/// Make sure the daemon is installed and logged in and restore settings to the defaults.
pub async fn prepare_daemon(
    rpc: &ServiceClient,
    rpc_provider: &RpcClientProvider,
) -> anyhow::Result<()> {
    // Check if daemon should be restarted
    let mut mullvad_client = ensure_daemon_version(rpc, rpc_provider)
        .await
        .context("Failed to restart daemon")?;

    log::debug!("Resetting daemon settings before test");
    mullvad_client
        .reset_settings()
        .await
        .context("Failed to reset settings")?;
    helpers::disconnect_and_wait(&mut mullvad_client)
        .await
        .context("Failed to disconnect daemon after test")?;
    helpers::ensure_logged_in(&mut mullvad_client).await?;
    helpers::custom_lists::add_default_lists(&mut mullvad_client).await?;
    helpers::custom_lists::set_default_location(&mut mullvad_client).await?;

    Ok(())
}

/// Reset the daemons environment.
///
/// Will and restart or reinstall it if necessary.
async fn ensure_daemon_version(
    rpc: &ServiceClient,
    rpc_provider: &RpcClientProvider,
) -> anyhow::Result<MullvadProxyClient> {
    let app_package_filename = &TEST_CONFIG.app_package_filename;

    let mullvad_client = if correct_daemon_version_is_running(rpc_provider.new_client().await).await
    {
        ensure_daemon_environment(rpc)
            .await
            .context("Failed to reset daemon environment")?;
        rpc_provider.new_client().await
    } else {
        // NOTE: Reinstalling the app resets the daemon environment
        install_app(rpc, app_package_filename, rpc_provider)
            .await
            .with_context(|| format!("Failed to install app '{app_package_filename}'"))?
    };
    Ok(mullvad_client)
}

/// Conditionally restart the running daemon
///
/// If the daemon was started with non-standard environment variables, subsequent tests may break
/// due to assuming a default configuration. In that case, reset the environment variables and
/// restart.
pub async fn ensure_daemon_environment(rpc: &ServiceClient) -> Result<(), anyhow::Error> {
    let current_env = rpc
        .get_daemon_environment()
        .await
        .context("Failed to get daemon env variables")?;
    let default_env = get_app_env()
        .await
        .context("Failed to get daemon default env variables")?;
    if current_env != default_env {
        log::debug!("Restarting daemon due changed environment variables. Values since last test {current_env:?}");
        rpc.set_daemon_environment(default_env)
            .await
            .context("Failed to restart daemon")?;
    };
    Ok(())
}

/// Checks if daemon is installed with the version specified by `TEST_CONFIG.app_package_filename`
async fn correct_daemon_version_is_running(mut mullvad_client: MullvadProxyClient) -> bool {
    let app_package_filename = &TEST_CONFIG.app_package_filename;
    let expected_version = get_version_from_path(std::path::Path::new(app_package_filename))
        .unwrap_or_else(|_| panic!("Invalid app version: {app_package_filename}"));

    use mullvad_management_interface::Error::*;
    match mullvad_client.get_current_version().await {
        // Failing to reach the daemon is a sign that it is not installed
        Err(Rpc(..)) => {
            log::debug!("Could not reach active daemon before test, it is not running");
            false
        }
        Err(e) => panic!("Failed to get app version: {e}"),
        Ok(version) if version == expected_version => true,
        _ => {
            log::debug!("Daemon version mismatch");
            false
        }
    }
}
