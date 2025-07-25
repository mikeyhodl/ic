use crate::{
    catch_up_package_provider::CatchUpPackageProvider,
    error::{OrchestratorError, OrchestratorResult},
    metrics::OrchestratorMetrics,
    process_manager::{Process, ProcessManager},
    registry_helper::RegistryHelper,
};
use async_trait::async_trait;
use ic_consensus_dkg::get_vetkey_public_keys;
use ic_crypto::get_master_public_key_from_transcript;
use ic_http_utils::file_downloader::FileDownloader;
use ic_image_upgrader::{
    error::{UpgradeError, UpgradeResult},
    ImageUpgrader,
};
use ic_interfaces_registry::RegistryClient;
use ic_logger::{error, info, warn, ReplicaLogger};
use ic_management_canister_types_private::MasterPublicKeyId;
use ic_protobuf::proxy::try_from_option_field;
use ic_registry_client_helpers::{node::NodeRegistry, subnet::SubnetRegistry};
use ic_registry_local_store::LocalStoreImpl;
use ic_registry_replicator::RegistryReplicator;
use ic_types::{
    consensus::{CatchUpPackage, HasHeight},
    crypto::{
        canister_threshold_sig::MasterPublicKey,
        threshold_sig::ni_dkg::{NiDkgId, NiDkgTargetSubnet},
    },
    Height, NodeId, RegistryVersion, ReplicaVersion, SubnetId,
};
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Instant,
};

const KEY_CHANGES_FILENAME: &str = "key_changed_metric.cbor";

pub struct ReplicaProcess {
    version: ReplicaVersion,
    binary: String,
    args: Vec<String>,
}

impl Process for ReplicaProcess {
    const NAME: &'static str = "Replica";

    type Version = ReplicaVersion;

    fn get_version(&self) -> &Self::Version {
        &self.version
    }

    fn get_binary(&self) -> &str {
        &self.binary
    }

    fn get_args(&self) -> &[String] {
        &self.args
    }

    fn get_env(&self) -> HashMap<String, String> {
        HashMap::new()
    }
}

/// Provides function to continuously check the Registry to determine if this
/// node should upgrade to a new release package, and if so, downloads and
/// extracts this release package and exec's the orchestrator binary contained
/// within.
pub(crate) struct Upgrade {
    pub registry: Arc<RegistryHelper>,
    pub metrics: Arc<OrchestratorMetrics>,
    replica_process: Arc<Mutex<ProcessManager<ReplicaProcess>>>,
    cup_provider: Arc<CatchUpPackageProvider>,
    replica_version: ReplicaVersion,
    replica_config_file: PathBuf,
    pub ic_binary_dir: PathBuf,
    pub image_path: PathBuf,
    registry_replicator: Arc<RegistryReplicator>,
    pub logger: ReplicaLogger,
    node_id: NodeId,
    /// The replica version that is prepared by 'prepare_upgrade' to upgrade to.
    pub prepared_upgrade_version: Option<ReplicaVersion>,
    pub orchestrator_data_directory: PathBuf,
}

impl Upgrade {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn new(
        registry: Arc<RegistryHelper>,
        metrics: Arc<OrchestratorMetrics>,
        replica_process: Arc<Mutex<ProcessManager<ReplicaProcess>>>,
        cup_provider: Arc<CatchUpPackageProvider>,
        replica_version: ReplicaVersion,
        replica_config_file: PathBuf,
        node_id: NodeId,
        ic_binary_dir: PathBuf,
        registry_replicator: Arc<RegistryReplicator>,
        release_content_dir: PathBuf,
        logger: ReplicaLogger,
        orchestrator_data_directory: PathBuf,
    ) -> Self {
        let value = Self {
            registry,
            metrics,
            replica_process,
            cup_provider,
            node_id,
            replica_version,
            replica_config_file,
            ic_binary_dir,
            image_path: release_content_dir.join("image.bin"),
            registry_replicator,
            logger: logger.clone(),
            prepared_upgrade_version: None,
            orchestrator_data_directory,
        };
        if let Err(e) = value.report_reboot_time() {
            warn!(logger, "Cannot report the reboot time: {}", e);
        }
        if let Err(e) = report_master_public_key_changed_metric(
            value.orchestrator_data_directory.join(KEY_CHANGES_FILENAME),
            &value.metrics,
        ) {
            warn!(
                logger,
                "Cannot report master public key changed metric: {}", e
            );
        }
        value.confirm_boot().await;
        value
    }

    fn report_reboot_time(&self) -> OrchestratorResult<()> {
        let elapsed_time = self.get_time_since_last_reboot_trigger()?;
        self.metrics
            .reboot_duration
            .set(elapsed_time.as_secs() as i64);
        Ok(())
    }

    /// Checks for a new release package, and if found, upgrades to this release
    /// package
    pub(crate) async fn check(&mut self) -> OrchestratorResult<Option<SubnetId>> {
        let latest_registry_version = self.registry.get_latest_version();
        // Determine the subnet_id using the local CUP.
        let (subnet_id, local_cup_proto, local_cup) = {
            let maybe_proto = self.cup_provider.get_local_cup_proto();
            let maybe_cup = maybe_proto.as_ref().and_then(|proto| {
                CatchUpPackage::try_from(proto)
                    .inspect_err(|err| {
                        error!(self.logger, "Failed to deserialize CatchUpPackage: {}", err);
                    })
                    .ok()
            });

            match (&maybe_cup, &maybe_proto) {
                (Some(cup), _) => {
                    let subnet_id =
                        get_subnet_id(&*self.registry.registry_client, cup).map_err(|err| {
                            OrchestratorError::UpgradeError(format!(
                                "Couldn't determine the subnet id: {:?}",
                                err
                            ))
                        })?;
                    (subnet_id, maybe_proto, maybe_cup)
                }
                (None, Some(proto)) => {
                    // We found a local CUP proto that we can't deserialize. This may only happen
                    // if this is the first CUP we are reading on a new replica version after an
                    // upgrade. This means we have to be an assigned node, otherwise we would have
                    // left the subnet and deleted the CUP before upgrading to this version.
                    // The only way to leave this branch is via subnet recovery.
                    self.metrics.critical_error_cup_deserialization_failed.inc();

                    // Try to find the subnet ID by deserializing only the NiDkgId. If it fails
                    // we will have to recover using failover nodes.
                    let nidkg_id: NiDkgId = try_from_option_field(proto.signer.clone(), "NiDkgId")
                        .map_err(|err| {
                            OrchestratorError::UpgradeError(format!(
                                "Couldn't deserialize NiDkgId to determine the subnet id: {:?}",
                                err
                            ))
                        })?;

                    let subnet_id = match nidkg_id.target_subnet {
                        NiDkgTargetSubnet::Local => nidkg_id.dealer_subnet,
                        NiDkgTargetSubnet::Remote(_) => {
                            // If this CUP was created by a remote subnet, then it is a genesis/recovery
                            // CUP. This is the only case in the branch where we can trust the subnet ID
                            // of the latest registry version, as switching to a registry CUP "resets" the
                            // "oldest registry version in use" which is responsible for subnet membership.
                            match self.registry.get_subnet_id(latest_registry_version) {
                                Ok(subnet_id) => subnet_id,
                                Err(OrchestratorError::NodeUnassignedError(_, _)) => {
                                    // If the registry says that we are unassigned, this unassignment
                                    // must have happened after the registry CUP triggering the upgrade.
                                    // Otherwise we would have left the subnet before upgrading. This means
                                    // we will trust the registry and go ahead with removing the node's state
                                    // including the broken local CUP.
                                    self.remove_state().await?;
                                    return Ok(None);
                                }
                                Err(other) => return Err(other),
                            }
                        }
                    };
                    (subnet_id, maybe_proto, None)
                }
                (None, None) => match self.registry.get_subnet_id(latest_registry_version) {
                    Ok(subnet_id) => {
                        info!(self.logger, "Assignment to subnet {} detected", subnet_id);
                        (subnet_id, None, None)
                    }
                    // If no subnet is assigned to the node id, we're unassigned.
                    _ => {
                        self.check_for_upgrade_as_unassigned().await?;
                        return Ok(None);
                    }
                },
            }
        };

        // When we arrived here, we are an assigned node.
        let old_cup_height = local_cup.as_ref().map(HasHeight::height);

        // Get the latest available CUP from the disk, peers or registry and
        // persist it if necessary.
        let latest_cup = self
            .cup_provider
            .get_latest_cup(local_cup_proto, subnet_id)
            .await?;

        // If we replaced the previous local CUP, compare potential threshold master public keys with
        // the ones in the new CUP, to make sure they haven't changed. Raise an alert if they did.
        if let Some(old_cup) = local_cup {
            if old_cup.height() < latest_cup.height() {
                compare_master_public_keys(
                    &old_cup,
                    &latest_cup,
                    self.metrics.as_ref(),
                    self.orchestrator_data_directory.join(KEY_CHANGES_FILENAME),
                    &self.logger,
                );
            }
        }

        // If the CUP is unsigned, it's a registry CUP and we're in a genesis or subnet
        // recovery scenario. Check if we're in an NNS subnet recovery case and download
        // the new registry if needed.
        if !latest_cup.is_signed() {
            info!(
                self.logger,
                "The latest CUP (registry version={}, height={}) is unsigned: \
                a subnet genesis/recovery is in progress",
                latest_cup.content.registry_version(),
                latest_cup.height(),
            );

            self.download_registry_and_restart_if_nns_subnet_recovery(
                subnet_id,
                latest_registry_version,
            )
            .await?;
        }

        // Now when we have the most recent CUP, we check if we're still assigned.
        // If not, go into unassigned state.
        if should_node_become_unassigned(
            &*self.registry.registry_client,
            self.node_id,
            subnet_id,
            &latest_cup,
        ) {
            self.stop_replica()?;
            return match self.remove_state().await {
                Ok(()) => Ok(None),
                Err(err) => {
                    warn!(
                        self.logger,
                        "Removal of the node state failed with error {}", err
                    );
                    self.metrics.critical_error_state_removal_failed.inc();
                    Err(err)
                }
            };
        }

        // If we arrived here, we have the newest CUP and we're still assigned.
        // Now we check if this CUP requires a new replica version.
        let cup_registry_version = latest_cup.content.registry_version();
        let new_replica_version = self
            .registry
            .get_replica_version(subnet_id, cup_registry_version)?;
        if new_replica_version != self.replica_version {
            info!(
                self.logger,
                "Starting version upgrade at CUP registry version {}: {} -> {}",
                cup_registry_version,
                self.replica_version,
                new_replica_version
            );
            // Only downloads the new image if it doesn't already exists locally, i.e. it
            // was previously downloaded by `prepare_upgrade_if_scheduled()`, see
            // below.
            self.execute_upgrade(&new_replica_version).await?;
            return Ok(Some(subnet_id));
        }

        // If we arrive here, we are on the newest replica version.
        // Now we check if a subnet recovery is in progress.
        // If it is, we restart to pass the unsigned CUP to consensus.
        self.stop_replica_if_new_recovery_cup(&latest_cup, old_cup_height);

        // This will start a new replica process if none is running.
        self.ensure_replica_is_running(&self.replica_version, subnet_id)?;

        // This will trigger an image download if one is already scheduled but we did
        // not arrive at the corresponding CUP yet.
        self.prepare_upgrade_if_scheduled(subnet_id).await?;

        Ok(Some(subnet_id))
    }

    // Special case for when we are doing bootstrap subnet recovery for
    // nns and replacing the local registry store. Because we replace the
    // contents of the local registry store in the process of doing this, we
    // will not perpetually hit this case, and thus it is not important to
    // check the height.
    async fn download_registry_and_restart_if_nns_subnet_recovery(
        &self,
        subnet_id: SubnetId,
        registry_version: RegistryVersion,
    ) -> OrchestratorResult<()> {
        if let Some(registry_contents) = self
            .registry
            .registry_client
            .get_cup_contents(subnet_id, registry_version)
            .ok()
            .and_then(|record| record.value)
        {
            if let Some(registry_store_uri) = registry_contents.registry_store_uri {
                warn!(
                    self.logger,
                    "Downloading registry data from {} with hash {} for subnet recovery",
                    registry_store_uri.uri,
                    registry_store_uri.hash,
                );
                let downloader = FileDownloader::new(Some(self.logger.clone()));
                let local_store_location = tempfile::tempdir()
                    .expect("temporary location for local store download could not be created")
                    .keep();
                downloader
                    .download_and_extract_tar(
                        &registry_store_uri.uri,
                        &local_store_location,
                        Some(registry_store_uri.hash),
                    )
                    .await
                    .map_err(OrchestratorError::FileDownloadError)?;
                if let Err(e) = self.stop_replica() {
                    // Even though we fail to stop the replica, we should still
                    // replace the registry local store, so we simply issue a warning.
                    warn!(self.logger, "Failed to stop replica with error {:?}", e);
                }
                let new_local_store = LocalStoreImpl::new(local_store_location);
                self.registry_replicator
                    .stop_polling_and_set_local_registry_data(&new_local_store);
                reexec_current_process(&self.logger);
            }
        }
        Ok(())
    }

    async fn remove_state(&self) -> OrchestratorResult<()> {
        // Reset the key changed errors counter to not raise alerts in other subnets
        self.metrics.master_public_key_changed_errors.reset();
        remove_node_state(
            self.replica_config_file.clone(),
            self.cup_provider.get_cup_path(),
            self.orchestrator_data_directory.clone(),
        )
        .map_err(OrchestratorError::UpgradeError)?;
        info!(self.logger, "Subnet state removed");

        let instant = Instant::now();
        sync_and_trim_fs(&self.logger)
            .await
            .map_err(OrchestratorError::UpgradeError)?;
        let elapsed = instant.elapsed().as_millis();
        self.metrics.fstrim_duration.set(elapsed as i64);
        info!(
            self.logger,
            "Filesystem synced and trimmed in {}ms", elapsed
        );

        Ok(())
    }

    // Checks if the subnet record for the given subnet_id contains a different
    // replica version. If it is the case, the image will be downloaded. This
    // allows us to decrease the upgrade downtime.
    async fn prepare_upgrade_if_scheduled(
        &mut self,
        subnet_id: SubnetId,
    ) -> OrchestratorResult<()> {
        let (expected_replica_version, registry_version) =
            self.registry.get_expected_replica_version(subnet_id)?;
        if expected_replica_version != self.replica_version {
            info!(
                self.logger,
                "Replica version upgrade detected at registry version {}: {} -> {}",
                registry_version,
                self.replica_version,
                expected_replica_version
            );
            self.prepare_upgrade(&expected_replica_version).await?
        }
        Ok(())
    }

    async fn check_for_upgrade_as_unassigned(&mut self) -> OrchestratorResult<()> {
        let registry_version = self.registry.get_latest_version();

        // If the node is a boundary node, we upgrade to that version, otherwise we upgrade to the unassigned version
        let replica_version = self
            .registry
            .get_api_boundary_node_version(self.node_id, registry_version)
            .or_else(|err| match err {
                OrchestratorError::ApiBoundaryNodeMissingError(_, _) => self
                    .registry
                    .get_unassigned_replica_version(registry_version),
                err => Err(err),
            })?;

        if self.replica_version == replica_version {
            return Ok(());
        }
        info!(
            self.logger,
            "Replica upgrade on unassigned node detected: old version {}, new version {}",
            self.replica_version,
            replica_version
        );
        self.execute_upgrade(&replica_version)
            .await
            .map_err(OrchestratorError::from)
    }

    /// Stop the current replica process.
    pub fn stop_replica(&self) -> OrchestratorResult<()> {
        self.replica_process.lock().unwrap().stop().map_err(|e| {
            OrchestratorError::IoError(
                "Error when attempting to stop replica during upgrade".into(),
                e,
            )
        })
    }

    // Stop the replica if the given CUP is unsigned and higher than the given height.
    // Without restart, consensus would reject the unsigned artifact.
    // If stopping the replica fails, restart the current process instead.
    fn stop_replica_if_new_recovery_cup(
        &self,
        cup: &CatchUpPackage,
        old_cup_height: Option<Height>,
    ) {
        let new_height = cup.content.height();
        if !cup.is_signed() && old_cup_height.is_some() && Some(new_height) > old_cup_height {
            info!(
                self.logger,
                "Found higher unsigned CUP, restarting replica for subnet recovery..."
            );
            // Restarting the replica is enough to pass the unsigned CUP forward.
            // If we fail, restart the current process instead.
            if let Err(e) = self.stop_replica() {
                warn!(self.logger, "Failed to stop replica with error {:?}", e);
                reexec_current_process(&self.logger);
            }
        }
    }

    // Start the replica process if not running already
    fn ensure_replica_is_running(
        &self,
        replica_version: &ReplicaVersion,
        subnet_id: SubnetId,
    ) -> OrchestratorResult<()> {
        if self.replica_process.lock().unwrap().is_running() {
            return Ok(());
        }
        info!(self.logger, "Starting new replica process");
        let cup_path = self.cup_provider.get_cup_path();
        let replica_binary = self
            .ic_binary_dir
            .join("replica")
            .as_path()
            .display()
            .to_string();
        let cmd = vec![
            format!("--replica-version={}", replica_version.as_ref()),
            format!(
                "--config-file={}",
                self.replica_config_file.as_path().display()
            ),
            format!("--catch-up-package={}", cup_path.as_path().display()),
            format!("--force-subnet={}", subnet_id),
        ];

        self.replica_process
            .lock()
            .unwrap()
            .start(ReplicaProcess {
                version: replica_version.clone(),
                binary: replica_binary,
                args: cmd,
            })
            .map_err(|e| {
                OrchestratorError::IoError("Error when attempting to start new replica".into(), e)
            })
    }
}

#[async_trait]
impl ImageUpgrader<ReplicaVersion, Option<SubnetId>> for Upgrade {
    fn get_prepared_version(&self) -> Option<&ReplicaVersion> {
        self.prepared_upgrade_version.as_ref()
    }

    fn set_prepared_version(&mut self, version: Option<ReplicaVersion>) {
        self.prepared_upgrade_version = version
    }

    fn binary_dir(&self) -> &PathBuf {
        &self.ic_binary_dir
    }

    fn image_path(&self) -> &PathBuf {
        &self.image_path
    }

    fn data_dir(&self) -> Option<&PathBuf> {
        Some(&self.orchestrator_data_directory)
    }

    fn get_release_package_urls_and_hash(
        &self,
        version: &ReplicaVersion,
    ) -> UpgradeResult<(Vec<String>, Option<String>)> {
        let record = self
            .registry
            .get_replica_version_record(version.clone(), self.registry.get_latest_version())
            .map_err(UpgradeError::from)?;

        Ok((
            record.release_package_urls,
            Some(record.release_package_sha256_hex),
        ))
    }

    fn log(&self) -> &ReplicaLogger {
        &self.logger
    }

    fn get_load_balance_number(&self) -> usize {
        // XOR all the u8 in node_id:
        let principal = self.node_id.get().0;
        principal.as_slice().iter().fold(0, |acc, x| (acc ^ x)) as usize
    }

    async fn check_for_upgrade(&mut self) -> UpgradeResult<Option<SubnetId>> {
        self.check().await.map_err(UpgradeError::from)
    }
}

// Returns the subnet id for the given CUP.
fn get_subnet_id(registry: &dyn RegistryClient, cup: &CatchUpPackage) -> Result<SubnetId, String> {
    let dkg_summary = &cup
        .content
        .block
        .get_value()
        .payload
        .as_ref()
        .as_summary()
        .dkg;
    // Note that although sometimes CUPs have no signatures (e.g. genesis and
    // recovery CUPs) they always have the signer id (the DKG id), which is taken
    // from the high-threshold transcript when we build a genesis/recovery CUP.
    let dkg_id = &cup.signature.signer;
    // If the DKG key material was signed by the subnet itself — use it, if not, get
    // the subnet id from the registry.
    match dkg_id.target_subnet {
        NiDkgTargetSubnet::Local => Ok(dkg_id.dealer_subnet),
        // If we hit this case, then the local CUP is a genesis or recovery CUP of an application
        // subnet or of the NNS subnet recovered on failover nodes. We cannot derive the subnet id
        // from it, so we use the registry version of that CUP and the node id of one of the
        // high-threshold committee members, to find out to which subnet this node belongs to.
        NiDkgTargetSubnet::Remote(_) => {
            let node_id = dkg_summary
                .current_transcripts()
                .values()
                .next()
                .ok_or("No current transcript found")?
                .committee
                .get()
                .iter()
                .next()
                .ok_or("No nodes in current transcript committee found")?;
            match registry.get_subnet_id_from_node_id(*node_id, dkg_summary.registry_version) {
                Ok(Some(subnet_id)) => Ok(subnet_id),
                other => Err(format!(
                    "Couldn't get the subnet id from the registry for node {:?} at registry version {}: {:?}",
                    node_id, dkg_summary.registry_version, other
                )),
            }
        }
    }
}

// Checks if the node still belongs to the subnet it was assigned the last time.
// We decide this by checking the subnet membership starting from the oldest
// relevant version of the local CUP and ending with the latest registry
// version.
fn should_node_become_unassigned(
    registry: &dyn RegistryClient,
    node_id: NodeId,
    subnet_id: SubnetId,
    cup: &CatchUpPackage,
) -> bool {
    let oldest_relevant_version = cup.get_oldest_registry_version_in_use().get();
    let latest_registry_version = registry.get_latest_version().get();
    // Make sure that if the latest registry version is for some reason violating
    // the assumption that it's higher/equal than any other version used in the
    // system, we still do not remove the subnet state by a mistake.
    if latest_registry_version < oldest_relevant_version {
        return false;
    }
    for version in oldest_relevant_version..=latest_registry_version {
        if let Ok(Some(members)) =
            registry.get_node_ids_on_subnet(subnet_id, RegistryVersion::from(version))
        {
            if members.iter().any(|id| id == &node_id) {
                return false;
            }
        }
    }
    true
}

// Call `sync` and `fstrim` on the data partition
async fn sync_and_trim_fs(logger: &ReplicaLogger) -> Result<(), String> {
    let mut fstrim_script = tokio::process::Command::new("/opt/ic/bin/sync_fstrim.sh");
    info!(logger, "Running command '{:?}'...", fstrim_script);
    match fstrim_script.status().await {
        Ok(status) => {
            if status.success() {
                Ok(())
            } else {
                Err(format!(
                    "Failed to run command '{:?}', return value: {}",
                    fstrim_script, status
                ))
            }
        }
        Err(err) => Err(format!(
            "Failed to run command '{:?}', error: {}",
            fstrim_script, err
        )),
    }
}

// Deletes the subnet state consisting of the consensus pool, execution state,
// the local CUP and the persisted error metric of threshold key changes.
fn remove_node_state(
    replica_config_file: PathBuf,
    cup_path: PathBuf,
    orchestrator_data_directory: PathBuf,
) -> Result<(), String> {
    use ic_config::{Config, ConfigSource};
    use std::fs::{remove_dir_all, remove_file};
    let tmpdir = tempfile::Builder::new()
        .prefix("ic_config")
        .tempdir()
        .map_err(|err| format!("Couldn't create a temporary directory: {:?}", err))?;
    let config = Config::load_with_tmpdir(
        ConfigSource::File(replica_config_file),
        tmpdir.path().to_path_buf(),
    );

    let consensus_pool_path = config.artifact_pool.consensus_pool_path;
    remove_dir_all(&consensus_pool_path).map_err(|err| {
        format!(
            "Couldn't delete the consensus pool at {:?}: {:?}",
            consensus_pool_path, err
        )
    })?;

    let state_path = config.state_manager.state_root();

    // We have to explicitly delete child sub-directories and files from the state_root,
    // instead of calling remove_dir_all(state_path) because
    // deleting the "page_deltas" directory results in a SELinux issue: upon deletion of
    // a directory/file, its SELinux class is not persisted if it's recreated. Upon
    // re-creation, the SELinux rights of the creator are applied, not the "old" ones.
    // Deleting the page_deltas directory would thus remove the sandbox capacity to
    // do IO in the page delta files.
    for entry in std::fs::read_dir(state_path.as_path()).map_err(|err| {
        format!(
            "Error iterating through dir {:?}, because {:?}",
            state_path.as_path(),
            err
        )
    })? {
        let en = entry
            .as_ref()
            .expect("Getting reference of dir entry failed.");
        // If this isn't the page deltas directory, it's safe to delete.
        if en
            .file_name()
            .into_string()
            .expect("Converting file name to string failed.")
            != config.state_manager.page_deltas_dirname()
        {
            if en
                .file_type()
                .expect("IO error fetching file type.")
                .is_dir()
            {
                remove_dir_all(en.path())
            } else {
                std::fs::remove_file(en.path())
            }
            .map_err(|err| {
                format!(
                    "Couldn't delete the path {:?}, because {:?}",
                    en.path(),
                    err
                )
            })?;
        } else {
            // Look into the page_deltas/ directory and delete any possible leftover files.
            for entry in std::fs::read_dir(
                state_path
                    .as_path()
                    .join(config.state_manager.page_deltas_dirname()),
            )
            .map_err(|err| {
                format!(
                    "Error iterating through dir {:?}, because {:?}",
                    state_path.as_path(),
                    err
                )
            })? {
                std::fs::remove_file(entry.expect("Error getting file under page_delta/.").path())
                    .map_err(|err| {
                        format!(
                            "Couldn't delete the file {:?}, because {:?}",
                            en.path(),
                            err
                        )
                    })?;
            }
        }
    }

    remove_file(&cup_path)
        .map_err(|err| format!("Couldn't delete the CUP at {:?}: {:?}", cup_path, err))?;

    let key_changed_metric = orchestrator_data_directory.join(KEY_CHANGES_FILENAME);
    if key_changed_metric.try_exists().map_err(|err| {
        format!(
            "Failed to check if {:?} exists, because {:?}",
            key_changed_metric, err
        )
    })? {
        remove_file(&key_changed_metric).map_err(|err| {
            format!(
                "Couldn't delete the key changes metric at {:?}: {:?}",
                key_changed_metric, err
            )
        })?;
    }

    Ok(())
}

// Re-execute the current process, exactly as it was originally called.
fn reexec_current_process(logger: &ReplicaLogger) -> OrchestratorError {
    let args: Vec<String> = std::env::args().collect();
    info!(
        logger,
        "Restarting the current process with the same arguments it was originally executed with: {:?}",
        &args[..]
    );
    let error = exec::Command::new(&args[0]).args(&args[1..]).exec();
    OrchestratorError::ExecError(PathBuf::new(), error)
}

/// Return the threshold master public key of the given CUP, if it exists.
fn get_master_public_keys(
    cup: &CatchUpPackage,
    log: &ReplicaLogger,
) -> BTreeMap<MasterPublicKeyId, MasterPublicKey> {
    let payload = cup.content.block.get_value().payload.as_ref();

    let (mut public_keys, _) = get_vetkey_public_keys(&payload.as_summary().dkg, log);

    let Some(idkg) = payload.as_idkg() else {
        return public_keys;
    };

    for (key_id, key_transcript) in &idkg.key_transcripts {
        let Some(transcript) = key_transcript
            .current
            .as_ref()
            .and_then(|transcript_ref| idkg.idkg_transcripts.get(&transcript_ref.transcript_id()))
        else {
            continue;
        };

        match get_master_public_key_from_transcript(transcript) {
            Ok(public_key) => {
                public_keys.insert(key_id.clone().into(), public_key);
            }
            Err(err) => {
                warn!(
                    log,
                    "Failed to get the master public key for key id {}: {:?}", key_id, err,
                );
            }
        };
    }

    public_keys
}

/// Get threshold master public keys of both CUPs and make sure previous keys weren't changed
/// or deleted. Raise an alert if they were.
fn compare_master_public_keys(
    old_cup: &CatchUpPackage,
    new_cup: &CatchUpPackage,
    metrics: &OrchestratorMetrics,
    path: PathBuf,
    log: &ReplicaLogger,
) {
    let old_public_keys = get_master_public_keys(old_cup, log);
    if old_public_keys.is_empty() {
        return;
    }

    let new_public_keys = get_master_public_keys(new_cup, log);
    let mut changes = BTreeMap::new();

    for (key_id, old_public_key) in old_public_keys {
        let key_id_label = key_id.to_string();

        // Get the metric here already, which will initialize it with zero
        // even if keys haven't changed.
        let metric = metrics
            .master_public_key_changed_errors
            .get_metric_with_label_values(&[&key_id_label])
            .expect("Failed to get master public key changed metric");

        if let Some(new_public_key) = new_public_keys.get(&key_id) {
            if old_public_key != *new_public_key {
                error!(
                    log,
                    "Threshold master public key for {} has changed! Old: {:?}, New: {:?}",
                    key_id,
                    old_public_key,
                    new_public_key,
                );
                metric.inc();
                changes.insert(key_id_label.clone(), metric.get());
            }
        } else {
            error!(
                log,
                "Threshold master public key for {} has been deleted!", key_id,
            );
            metric.inc();
            changes.insert(key_id_label, metric.get());
        }
    }

    // We persist the latest value of the changed metrics, such that we can re-apply them
    // after the restart. As any increase in the value is enough to trigger the alert, it
    // is fine to reset the metric of keys that haven't changed.
    if let Err(e) = persist_master_public_key_changed_metric(path, changes) {
        warn!(
            log,
            "Failed to persist master public key changed metric: {}", e
        )
    }
}

/// Persist the given map of master public key changed metrics in `path`.
fn persist_master_public_key_changed_metric(
    path: PathBuf,
    changes: BTreeMap<String, u64>,
) -> OrchestratorResult<()> {
    let file = std::fs::File::create(path).map_err(OrchestratorError::key_monitoring_error)?;
    serde_cbor::to_writer(file, &changes).map_err(OrchestratorError::key_monitoring_error)
}

/// Increment the `master_public_key_changed_errors` metric by the values persisted in the given file.
fn report_master_public_key_changed_metric(
    path: PathBuf,
    metrics: &OrchestratorMetrics,
) -> OrchestratorResult<()> {
    // If the file doesn't exist then there is nothing to report.
    if !path
        .try_exists()
        .map_err(OrchestratorError::key_monitoring_error)?
    {
        return Ok(());
    }
    let file = std::fs::File::open(path).map_err(OrchestratorError::key_monitoring_error)?;
    let key_changes: BTreeMap<String, u64> =
        serde_cbor::from_reader(file).map_err(OrchestratorError::key_monitoring_error)?;

    for (key, count) in key_changes {
        metrics
            .master_public_key_changed_errors
            .with_label_values(&[&key])
            .inc_by(count);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use ic_crypto_test_utils_canister_threshold_sigs::{
        generate_key_transcript, CanisterThresholdSigTestEnvironment, IDkgParticipants,
    };
    use ic_crypto_test_utils_ni_dkg::{
        run_ni_dkg_and_create_single_transcript, NiDkgTestEnvironment, RandomNiDkgConfig,
    };
    use ic_crypto_test_utils_reproducible_rng::{reproducible_rng, ReproducibleRng};
    use ic_management_canister_types_private::{
        EcdsaCurve, EcdsaKeyId, SchnorrAlgorithm, SchnorrKeyId, VetKdCurve, VetKdKeyId,
    };
    use ic_metrics::MetricsRegistry;
    use ic_test_utilities_consensus::fake::{Fake, FakeContent};
    use ic_test_utilities_logger::with_test_replica_logger;
    use ic_test_utilities_types::ids::subnet_test_id;
    use ic_types::{
        batch::ValidationContext,
        consensus::{
            dkg::DkgSummary,
            idkg::{self, MasterKeyTranscript, TranscriptAttributes},
            Block, BlockPayload, CatchUpContent, HashedBlock, HashedRandomBeacon, Payload,
            RandomBeacon, RandomBeaconContent, Rank, SummaryPayload,
        },
        crypto::{
            canister_threshold_sig::idkg::IDkgTranscript,
            threshold_sig::ni_dkg::{NiDkgMasterPublicKeyId, NiDkgTag, NiDkgTranscript},
            AlgorithmId, CryptoHash, CryptoHashOf,
        },
        signature::ThresholdSignature,
        time::UNIX_EPOCH,
    };
    use tempfile::{tempdir, TempDir};

    fn make_ecdsa_key_id() -> MasterPublicKeyId {
        MasterPublicKeyId::Ecdsa(EcdsaKeyId {
            curve: EcdsaCurve::Secp256k1,
            name: "some_ecdsa_key".to_string(),
        })
    }

    fn make_schnorr_key_id() -> MasterPublicKeyId {
        MasterPublicKeyId::Schnorr(SchnorrKeyId {
            algorithm: SchnorrAlgorithm::Ed25519,
            name: "some_eddsa_key".to_string(),
        })
    }

    fn make_vetkd_key_id() -> MasterPublicKeyId {
        MasterPublicKeyId::VetKd(VetKdKeyId {
            curve: VetKdCurve::Bls12_381_G2,
            name: "some_vetkd_key".to_string(),
        })
    }

    fn make_key_ids_for_all_schemes() -> Vec<MasterPublicKeyId> {
        vec![
            make_ecdsa_key_id(),
            make_schnorr_key_id(),
            make_vetkd_key_id(),
        ]
    }

    fn clone_key_id_with_name(key_id: &MasterPublicKeyId, name: &str) -> MasterPublicKeyId {
        let mut key_id = key_id.clone();
        match key_id {
            MasterPublicKeyId::Ecdsa(ref mut key_id) => key_id.name = name.into(),
            MasterPublicKeyId::Schnorr(ref mut key_id) => key_id.name = name.into(),
            MasterPublicKeyId::VetKd(ref mut key_id) => key_id.name = name.into(),
        }
        key_id
    }

    #[derive(Clone)]
    enum KeyTranscript {
        IDkg(IDkgTranscript),
        NiDkg(NiDkgTranscript),
    }

    fn make_cup(
        h: Height,
        key_transcript: Option<(MasterPublicKeyId, KeyTranscript)>,
    ) -> CatchUpPackage {
        let mut nidkg_transcripts = BTreeMap::new();
        let mut idkg_transcripts = BTreeMap::new();
        let mut idkg_key_transcripts = Vec::new();

        if let Some((key_id, transcript)) = key_transcript {
            match (&key_id, transcript) {
                (MasterPublicKeyId::VetKd(_), KeyTranscript::NiDkg(transcript)) => {
                    nidkg_transcripts.insert(transcript.dkg_id.dkg_tag.clone(), transcript);
                }
                (MasterPublicKeyId::Ecdsa(_), KeyTranscript::IDkg(transcript))
                | (MasterPublicKeyId::Schnorr(_), KeyTranscript::IDkg(transcript)) => {
                    idkg_transcripts.insert(transcript.transcript_id, transcript.clone());
                    let unmasked = idkg::UnmaskedTranscriptWithAttributes::new(
                        transcript.to_attributes(),
                        idkg::UnmaskedTranscript::try_from((h, &transcript)).unwrap(),
                    );
                    idkg_key_transcripts.push(MasterKeyTranscript {
                        current: Some(unmasked),
                        next_in_creation: idkg::KeyTranscriptCreation::Begin,
                        master_key_id: key_id.clone().try_into().unwrap(),
                    });
                }
                _ => panic!("Unexpected key ID, transcript combination"),
            }
        }

        let mut idkg = idkg::IDkgPayload::empty(h, subnet_test_id(0), idkg_key_transcripts);
        idkg.idkg_transcripts = idkg_transcripts;

        let block = Block::new(
            CryptoHashOf::from(CryptoHash(Vec::new())),
            Payload::new(
                ic_types::crypto::crypto_hash,
                BlockPayload::Summary(SummaryPayload {
                    dkg: DkgSummary::fake().with_current_transcripts(nidkg_transcripts),
                    idkg: Some(idkg),
                }),
            ),
            h,
            Rank(46),
            ValidationContext {
                registry_version: RegistryVersion::from(101),
                certified_height: Height::from(42),
                time: UNIX_EPOCH,
            },
        );

        CatchUpPackage {
            content: CatchUpContent::new(
                HashedBlock::new(ic_types::crypto::crypto_hash, block),
                HashedRandomBeacon::new(
                    ic_types::crypto::crypto_hash,
                    RandomBeacon::fake(RandomBeaconContent::new(
                        h,
                        CryptoHashOf::from(CryptoHash(Vec::new())),
                    )),
                ),
                CryptoHashOf::from(CryptoHash(Vec::new())),
                None,
            ),
            signature: ThresholdSignature::fake(),
        }
    }

    fn get_master_key_changed_metric(
        key: &MasterPublicKeyId,
        metrics: &OrchestratorMetrics,
    ) -> u64 {
        metrics
            .master_public_key_changed_errors
            .get_metric_with_label_values(&[&key.to_string()])
            .unwrap()
            .get()
    }

    struct Setup {
        rng: ReproducibleRng,
        tmp: TempDir,
    }

    impl Setup {
        fn new() -> Self {
            let tmp = tempdir().expect("Unable to create temp directory");
            let rng = reproducible_rng();
            Self { rng, tmp }
        }

        fn generate_key_transcript(
            &mut self,
            key_id: &MasterPublicKeyId,
        ) -> (MasterPublicKeyId, KeyTranscript) {
            let transcript = match key_id {
                MasterPublicKeyId::Ecdsa(ecdsa_key_id) => match ecdsa_key_id.curve {
                    EcdsaCurve::Secp256k1 => {
                        self.generate_idkg_key_transcript(AlgorithmId::ThresholdEcdsaSecp256k1)
                    }
                },
                MasterPublicKeyId::Schnorr(schnorr_key_id) => match schnorr_key_id.algorithm {
                    SchnorrAlgorithm::Bip340Secp256k1 => {
                        self.generate_idkg_key_transcript(AlgorithmId::ThresholdSchnorrBip340)
                    }

                    SchnorrAlgorithm::Ed25519 => {
                        self.generate_idkg_key_transcript(AlgorithmId::ThresholdEd25519)
                    }
                },
                MasterPublicKeyId::VetKd(_) => self.generate_nidkg_key_transcript(key_id),
            };
            (key_id.clone(), transcript)
        }

        fn generate_idkg_key_transcript(&mut self, alg: AlgorithmId) -> KeyTranscript {
            let env = CanisterThresholdSigTestEnvironment::new(1, &mut self.rng);
            let (dealers, receivers) = env.choose_dealers_and_receivers(
                &IDkgParticipants::AllNodesAsDealersAndReceivers,
                &mut self.rng,
            );
            KeyTranscript::IDkg(generate_key_transcript(
                &env,
                &dealers,
                &receivers,
                alg,
                &mut self.rng,
            ))
        }

        fn generate_nidkg_key_transcript(&mut self, key_id: &MasterPublicKeyId) -> KeyTranscript {
            let MasterPublicKeyId::VetKd(vetkd_key_id) = key_id.clone() else {
                panic!("Can't generate nidkg transcript for {}", key_id);
            };
            let config = RandomNiDkgConfig::builder()
                .dkg_tag(NiDkgTag::HighThresholdForKey(
                    NiDkgMasterPublicKeyId::VetKd(vetkd_key_id),
                ))
                .subnet_size(4)
                .build(&mut self.rng);
            let env =
                NiDkgTestEnvironment::new_for_config_with_remote_vault(config.get(), &mut self.rng);
            KeyTranscript::NiDkg(run_ni_dkg_and_create_single_transcript(
                config.get(),
                &env.crypto_components,
            ))
        }

        fn path(&self) -> PathBuf {
            self.tmp.path().join(KEY_CHANGES_FILENAME)
        }
    }

    #[test]
    fn test_key_deletion_raises_alert_all_schemes() {
        for key_id in make_key_ids_for_all_schemes() {
            test_key_deletion_raises_alert(key_id)
        }
    }

    fn test_key_deletion_raises_alert(key_id: MasterPublicKeyId) {
        with_test_replica_logger(|log| {
            let mut setup = Setup::new();
            let key = setup.generate_key_transcript(&key_id);

            let c1 = make_cup(Height::from(10), Some(key));
            let c2 = make_cup(Height::from(100), None);

            let metrics = OrchestratorMetrics::new(&MetricsRegistry::new());

            let before = get_master_key_changed_metric(&key_id, &metrics);
            compare_master_public_keys(&c1, &c2, &metrics, setup.path(), &log);
            let after = get_master_key_changed_metric(&key_id, &metrics);

            assert_eq!(before + 1, after);

            let metrics_new = OrchestratorMetrics::new(&MetricsRegistry::new());
            report_master_public_key_changed_metric(setup.path(), &metrics_new).unwrap();
            let after_restart = get_master_key_changed_metric(&key_id, &metrics_new);

            assert_eq!(after_restart, after);

            // If there are no persisted metrics we should not report anything
            let metrics_new = OrchestratorMetrics::new(&MetricsRegistry::new());
            let path = setup.path().parent().unwrap().join("test");
            report_master_public_key_changed_metric(path, &metrics_new).unwrap();
            let non_existent = get_master_key_changed_metric(&key_id, &metrics_new);

            assert_eq!(non_existent, 0);
        });
    }

    #[test]
    fn test_key_change_raises_alert_all_schemes() {
        for key_id in make_key_ids_for_all_schemes() {
            test_key_change_raises_alert(key_id)
        }
    }

    fn test_key_change_raises_alert(key_id: MasterPublicKeyId) {
        with_test_replica_logger(|log| {
            let mut setup = Setup::new();
            let key1 = setup.generate_key_transcript(&key_id);
            let key2 = setup.generate_key_transcript(&key_id);

            let c1 = make_cup(Height::from(10), Some(key1));
            let c2 = make_cup(Height::from(100), Some(key2));

            let metrics = OrchestratorMetrics::new(&MetricsRegistry::new());

            let before = get_master_key_changed_metric(&key_id, &metrics);
            compare_master_public_keys(&c1, &c2, &metrics, setup.path(), &log);
            let after = get_master_key_changed_metric(&key_id, &metrics);

            assert_eq!(before + 1, after);
        });
    }

    #[test]
    fn test_key_unchanged_does_not_raise_alert_all_schemes() {
        for key_id in make_key_ids_for_all_schemes() {
            test_key_unchanged_does_not_raise_alert(key_id)
        }
    }

    fn test_key_unchanged_does_not_raise_alert(key_id: MasterPublicKeyId) {
        with_test_replica_logger(|log| {
            let mut setup = Setup::new();
            let key = setup.generate_key_transcript(&key_id);

            let c1 = make_cup(Height::from(10), Some(key.clone()));
            let c2 = make_cup(Height::from(100), Some(key));

            let metrics = OrchestratorMetrics::new(&MetricsRegistry::new());

            let before = get_master_key_changed_metric(&key_id, &metrics);
            compare_master_public_keys(&c1, &c2, &metrics, setup.path(), &log);
            let after = get_master_key_changed_metric(&key_id, &metrics);

            assert_eq!(before, after);
        });
    }

    #[test]
    fn test_key_id_change_raises_alert_all_schemes() {
        for key_id in make_key_ids_for_all_schemes() {
            test_key_id_change_raises_alert(key_id)
        }
    }

    fn test_key_id_change_raises_alert(key_id1: MasterPublicKeyId) {
        with_test_replica_logger(|log| {
            let mut setup = Setup::new();
            let key = setup.generate_key_transcript(&key_id1);
            let c1 = make_cup(Height::from(10), Some(key.clone()));

            let key_id2 = clone_key_id_with_name(&key_id1, "other_key");
            let c2 = if let (
                MasterPublicKeyId::VetKd(ref key_id),
                KeyTranscript::NiDkg(ref transcript),
            ) = (&key_id2, &key.1)
            {
                let mut transcript2 = transcript.clone();
                transcript2.dkg_id.dkg_tag =
                    NiDkgTag::HighThresholdForKey(NiDkgMasterPublicKeyId::VetKd(key_id.clone()));
                make_cup(
                    Height::from(100),
                    Some((key_id2, KeyTranscript::NiDkg(transcript2))),
                )
            } else {
                make_cup(Height::from(100), Some((key_id2, key.1)))
            };

            let metrics = OrchestratorMetrics::new(&MetricsRegistry::new());

            let before = get_master_key_changed_metric(&key_id1, &metrics);
            compare_master_public_keys(&c1, &c2, &metrics, setup.path(), &log);
            let after = get_master_key_changed_metric(&key_id1, &metrics);

            assert_eq!(before + 1, after);
        });
    }

    #[test]
    fn test_key_created_does_not_raise_alert_all_schemes() {
        for key_id in make_key_ids_for_all_schemes() {
            test_key_created_does_not_raise_alert(key_id)
        }
    }

    fn test_key_created_does_not_raise_alert(key_id: MasterPublicKeyId) {
        with_test_replica_logger(|log| {
            let mut setup = Setup::new();
            let key = setup.generate_key_transcript(&key_id);

            let c1 = make_cup(Height::from(10), None);
            let c2 = make_cup(Height::from(100), Some(key));

            let metrics = OrchestratorMetrics::new(&MetricsRegistry::new());

            let before = get_master_key_changed_metric(&key_id, &metrics);
            compare_master_public_keys(&c1, &c2, &metrics, setup.path(), &log);
            let after = get_master_key_changed_metric(&key_id, &metrics);

            assert_eq!(before, after);
        });
    }

    #[test]
    fn test_no_keys_created_does_not_raise_alert() {
        with_test_replica_logger(|log| {
            let setup = Setup::new();
            let key_id = make_ecdsa_key_id();

            let c1 = make_cup(Height::from(10), None);
            let c2 = make_cup(Height::from(100), None);

            let metrics = OrchestratorMetrics::new(&MetricsRegistry::new());

            let before = get_master_key_changed_metric(&key_id, &metrics);
            compare_master_public_keys(&c1, &c2, &metrics, setup.path(), &log);
            let after = get_master_key_changed_metric(&key_id, &metrics);

            assert_eq!(before, after);
        });
    }
}
