/* tag::catalog[]
Title:: XNet messaging backward/forward compatibility with current mainnet version.

Goal:: Ensure IC upgrade doesn't break XNet communication

Runbook::
0. Deploy 1 root and 2 app subnets and install NNS canisters onto root,
   all running mainnet version.
1. Bless current version.
2. Deploy and start XNet test canisters for long running XNet test
3. Run XNet test between two app subnets (success criteria same as for SLO test).
4. Upgrade one app subnet to current version.
5. Run XNet test again.
6. Downgrade back to mainnet.
7. Run XNet test again.
8. Tear down XNet test canisters for long running XNet test and check success
   (success conditions for the long running test are more generous, as the main
    expected signal is that upgrade/downgrade with messages around will succeed
    and no messages are lost)

Success::
1. XNet test successfully completes for all version combinations

end::catalog[] */

use anyhow::Result;
use ic_consensus_system_test_utils::rw_message::install_nns_and_check_progress;
use ic_consensus_system_test_utils::upgrade::{
    assert_assigned_replica_version, bless_replica_version, deploy_guestos_to_all_subnet_nodes,
};
use ic_registry_subnet_type::SubnetType;
use ic_system_test_driver::driver::group::SystemTestGroup;
use ic_system_test_driver::driver::ic::{InternetComputer, Subnet};
use ic_system_test_driver::driver::pot_dsl::{PotSetupFn, SysTestFn};
use ic_system_test_driver::driver::prometheus_vm::{HasPrometheus, PrometheusVm};
use ic_system_test_driver::driver::test_env::TestEnv;
use ic_system_test_driver::driver::test_env_api::{
    get_guestos_img_version, get_guestos_update_img_sha256, get_guestos_update_img_url,
    get_guestos_update_img_version, HasPublicApiUrl, HasTopologySnapshot, IcNodeContainer,
    IcNodeSnapshot,
};
use ic_system_test_driver::systest;
use ic_system_test_driver::util::{block_on, runtime_from_url, MetricsFetcher};
use slog::{info, Logger};
use std::collections::BTreeMap;
use std::time::Duration;

const PER_TASK_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const OVERALL_TIMEOUT: Duration = Duration::from_secs(15 * 60);

const DKG_INTERVAL: u64 = 9;
const NODES_PER_SUBNET: usize = 1;

fn main() -> Result<()> {
    let config = Config::default();
    let test = config.clone().test();
    SystemTestGroup::new()
        .with_setup(config.build())
        .add_test(systest!(test))
        .with_timeout_per_test(PER_TASK_TIMEOUT) // each task (including the setup function) may take up to `per_task_timeout`.
        .with_overall_timeout(OVERALL_TIMEOUT) // the entire group may take up to `overall_timeout`.
        .execute_from_args()?;
    Ok(())
}

#[derive(Clone, Debug, Default)]
pub struct Config {
    with_prometheus: bool,
}

impl Config {
    pub fn with_prometheus(self) -> Self {
        Self {
            with_prometheus: true,
        }
    }

    /// Builds the IC instance.
    pub fn build(self) -> impl PotSetupFn {
        move |env: TestEnv| setup(env, self)
    }

    /// Returns a test function based on this configuration.
    pub fn test(self) -> impl SysTestFn {
        move |env: TestEnv| test(env)
    }
}

// Generic setup
fn setup(env: TestEnv, config: Config) {
    fn subnet(subnet_type: SubnetType, custom_dkg: Option<u64>) -> Subnet {
        let mut subnet = Subnet::new(subnet_type).add_nodes(NODES_PER_SUBNET);
        if let Some(dkg_interval) = custom_dkg {
            subnet = subnet.with_dkg_interval_length(ic_types::Height::from(dkg_interval));
        }
        subnet
    }
    if config.with_prometheus {
        PrometheusVm::default()
            .start(&env)
            .expect("failed to start prometheus VM");
    }
    let ic = InternetComputer::new();
    ic.add_subnet(subnet(SubnetType::System, None))
        .add_subnet(subnet(SubnetType::Application, Some(DKG_INTERVAL)))
        .add_subnet(subnet(SubnetType::Application, Some(DKG_INTERVAL)))
        .setup_and_start(&env)
        .expect("failed to setup IC under test");
    env.topology_snapshot().subnets().for_each(|subnet| {
        subnet
            .nodes()
            .for_each(|node| node.await_status_is_healthy().unwrap())
    });
    install_nns_and_check_progress(env.topology_snapshot());
    if config.with_prometheus {
        env.sync_with_prometheus();
    }
}

pub fn test(env: TestEnv) {
    block_on(test_async(env));
}

// Generic test
pub async fn test_async(env: TestEnv) {
    let logger = env.logger();

    let nns_node = env
        .topology_snapshot()
        .root_subnet()
        .nodes()
        .next()
        .unwrap();

    let app_subnets: Vec<_> = env
        .topology_snapshot()
        .subnets()
        .filter(|s| s.subnet_id != env.topology_snapshot().root_subnet_id())
        .map(|s| (s.subnet_id, s.clone(), s.nodes().next().unwrap()))
        .collect();

    let app_subnet_runtimes = app_subnets
        .clone()
        .into_iter()
        .map(|(_, _, node)| node)
        .map(|node| runtime_from_url(node.get_public_url(), node.effective_canister_id()));

    let xnet_config = xnet_slo_test_lib::Config::new(2, 1, Duration::from_secs(30), 10);
    let long_xnet_config = xnet_slo_test_lib::Config::new_with_custom_thresholds(
        2,
        1,
        // Given that we use `deploy_and_start` and `tear_down` directly
        // the runtime parameter will be ignored for the main test run
        // and only used when checking the success of the test. We set
        // it conservatively low so that the success evaluation is more
        // generous.
        Duration::from_secs(90),
        10,
        0.3,
        // Given that there are a couple of subnet upgrades happening
        // while the long running test is running we are generous
        // with error thresholds.
        75.0,
        120,
    );

    // NOTE: For these tests, it is intended they run _from_ the mainnet
    // version, _to_ the branch version, and back.
    //
    // The test definition should specify `uses_guestos_mainnet_img` to choose
    // the mainnet image as the initial image, and `uses_guestos_update` to
    // choose the branch image as the upgrade target.
    let mainnet_version = get_guestos_img_version().expect("initial IC version");
    let branch_version = get_guestos_update_img_version().expect("target IC version");

    let (upgrade_subnet_id, _, upgrade_node) = app_subnets.first().unwrap();

    info!(&logger, "Blessing upgrade version.");

    let sha256 = get_guestos_update_img_sha256().unwrap();
    let upgrade_url = get_guestos_update_img_url().unwrap();
    bless_replica_version(
        &nns_node,
        &branch_version,
        &logger,
        &sha256,
        vec![upgrade_url.to_string()],
    )
    .await;

    info!(&logger, "Blessed all versions.");

    info!(&logger, "Starting long running XNet load");
    let runtimes = app_subnet_runtimes.clone().collect::<Vec<_>>();
    let long_running_canisters =
        xnet_slo_test_lib::deploy_and_start(env.clone(), &runtimes, &long_xnet_config, &logger)
            .await;

    info!(&logger, "Starting XNet test between 2 app subnets.");

    xnet_slo_test_lib::test_async_impl(
        env.clone(),
        app_subnet_runtimes.clone(),
        xnet_config.clone(),
        &logger,
    )
    .await;

    assert_no_critical_errors(&env, &logger).await;

    info!(&logger, "Upgrading 1 app subnet.");

    upgrade_to(
        &nns_node,
        *upgrade_subnet_id,
        upgrade_node,
        &branch_version,
        &logger,
    )
    .await;

    info!(&logger, "Starting XNet test between 2 app subnets.");

    xnet_slo_test_lib::test_async_impl(
        env.clone(),
        app_subnet_runtimes.clone(),
        xnet_config.clone(),
        &logger,
    )
    .await;

    assert_no_critical_errors(&env, &logger).await;

    info!(&logger, "Downgrading app subnet back to initial version.");

    upgrade_to(
        &nns_node,
        *upgrade_subnet_id,
        upgrade_node,
        &mainnet_version,
        &logger,
    )
    .await;

    info!(&logger, "Starting XNet test between 2 app subnets.");

    xnet_slo_test_lib::test_async_impl(
        env.clone(),
        app_subnet_runtimes,
        xnet_config.clone(),
        &logger,
    )
    .await;

    info!(&logger, "Tearing down long running canisters.");

    let metrics = xnet_slo_test_lib::tear_down(&long_running_canisters, &logger).await;
    assert!(
        xnet_slo_test_lib::check_success(metrics, &long_xnet_config, &logger),
        "Long running canisters didn't meet success conditions."
    );

    assert_no_critical_errors(&env, &logger).await;
}

async fn upgrade_to(
    nns_node: &IcNodeSnapshot,
    subnet_id: ic_types::SubnetId,
    subnet_node: &IcNodeSnapshot,
    target_version: &str,
    logger: &Logger,
) {
    deploy_guestos_to_all_subnet_nodes(
        nns_node,
        &ic_types::ReplicaVersion::try_from(target_version).unwrap(),
        subnet_id,
    )
    .await;
    assert_assigned_replica_version(subnet_node, target_version, logger.clone());
}

async fn assert_no_critical_errors(env: &TestEnv, log: &slog::Logger) {
    let nodes = env.topology_snapshot().subnets().flat_map(|s| s.nodes());
    const NUM_RETRIES: u32 = 10;
    const BACKOFF_TIME_MILLIS: u64 = 500;

    let metrics = MetricsFetcher::new(nodes, vec!["critical_errors".to_string()]);
    for i in 0..NUM_RETRIES {
        match metrics.fetch::<u64>().await {
            Ok(result) => {
                assert!(!result.is_empty());
                let filtered_results = result
                    .iter()
                    .filter(|(_, v)| v.iter().any(|x| *x > 0))
                    .collect::<BTreeMap<_, _>>();
                assert!(
                    filtered_results.is_empty(),
                    "Critical error detected: {:?}",
                    filtered_results
                );
                return;
            }
            Err(e) => {
                info!(log, "Could not scrape metrics: {e}, attempt {i}.");
            }
        }
        tokio::time::sleep(Duration::from_millis(BACKOFF_TIME_MILLIS)).await;
    }
    panic!("Couldn't obtain metrics after {NUM_RETRIES} attempts.");
}
