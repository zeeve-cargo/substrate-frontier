//! Service and ServiceFactory implementation. Specialized wrapper over substrate service.
use std::{
	collections::BTreeMap,
	sync::{Arc, Mutex},
	time::Duration,
};
use futures::{future, StreamExt};
use sc_client_api::BlockchainEvents;
use node_template_runtime::{self, opaque::Block, RuntimeApi};
use sc_cli::SubstrateCli;
use sc_client_api::{BlockBackend, ExecutorProvider};
pub use sc_executor::NativeElseWasmExecutor;
use sc_service::{error::Error as ServiceError, Configuration, TaskManager, RpcHandlers, BasePath};
use sc_telemetry::{Telemetry, TelemetryWorker};
use sc_rpc_api::DenyUnsafe;
use crate::rpc::{FullDeps, BabeDeps, GrandpaDeps, create_full};
use sp_runtime::traits::Block as BlockT;
use sc_network::NetworkService;
use sc_consensus_babe::SlotProportion;
// Frontier
use fc_consensus::FrontierBlockImport;
use fc_db::DatabaseSource;
use fc_mapping_sync::{MappingSyncWorker, SyncStrategy};
use fc_rpc::{EthTask, OverrideHandle};
use fc_rpc_core::types::{FeeHistoryCache, FeeHistoryCacheLimit, FilterPool};

use crate::cli::Cli;
// #[cfg(feature = "manual-seal")]
// use crate::cli::Sealing;

// Our native executor instance.
pub struct ExecutorDispatch;

impl sc_executor::NativeExecutionDispatch for ExecutorDispatch {
	/// Only enable the benchmarking host functions when we actually want to benchmark.
	#[cfg(feature = "runtime-benchmarks")]
	type ExtendHostFunctions = frame_benchmarking::benchmarking::HostFunctions;
	/// Otherwise we only use the default Substrate host functions.
	#[cfg(not(feature = "runtime-benchmarks"))]
	type ExtendHostFunctions = ();

	fn dispatch(method: &str, data: &[u8]) -> Option<Vec<u8>> {
		node_template_runtime::api::dispatch(method, data)
	}

	fn native_version() -> sc_executor::NativeVersion {
		node_template_runtime::native_version()
	}
}

type FullClient =
	sc_service::TFullClient<Block, RuntimeApi, NativeElseWasmExecutor<ExecutorDispatch>>;
type FullBackend = sc_service::TFullBackend<Block>;
type FullSelectChain = sc_consensus::LongestChain<FullBackend, Block>;
/// The transaction pool type defintion.
pub type TransactionPool = sc_transaction_pool::FullPool<Block, FullClient>;
/// A IO handler that uses all Full RPC extensions.
pub type IoHandler = jsonrpc_core::IoHandler<sc_rpc::Metadata>;
type FullGrandpaBlockImport =
	sc_finality_grandpa::GrandpaBlockImport<FullBackend, Block, FullClient, FullSelectChain>;

// FrontierFrontier
// #[cfg(feature = "aura")]
pub type ConsensusResult = (
	FrontierBlockImport<
		Block,
		sc_finality_grandpa::GrandpaBlockImport<FullBackend, Block, FullClient, FullSelectChain>,
		FullClient,
	>,
	sc_finality_grandpa::LinkHalf<Block, FullClient, FullSelectChain>,
);

// #[cfg(feature = "manual-seal")]
// pub type ConsensusResult = (
// 	FrontierBlockImport<Block, Arc<FullClient>, FullClient>,
// 	Sealing,
// );

pub fn frontier_database_dir(config: &Configuration, path: &str) -> std::path::PathBuf {
	let config_dir = config
		.base_path
		.as_ref()
		.map(|base_path| base_path.config_dir(config.chain_spec.id()))
		.unwrap_or_else(|| {
			BasePath::from_project("", "", &crate::cli::Cli::executable_name())
				.config_dir(config.chain_spec.id())
		});
	config_dir.join("frontier").join(path)
}

pub fn open_frontier_backend(config: &Configuration) -> Result<Arc<fc_db::Backend<Block>>, String> {
	Ok(Arc::new(fc_db::Backend::<Block>::new(
		&fc_db::DatabaseSettings {
			source: match config.database {
				DatabaseSource::RocksDb { .. } => DatabaseSource::RocksDb {
					path: frontier_database_dir(config, "db"),
					cache_size: 0,
				},
				DatabaseSource::ParityDb { .. } => DatabaseSource::ParityDb {
					path: frontier_database_dir(config, "paritydb"),
				},
				DatabaseSource::Auto { .. } => DatabaseSource::Auto {
					rocksdb_path: frontier_database_dir(config, "db"),
					paritydb_path: frontier_database_dir(config, "paritydb"),
					cache_size: 0,
				},
				_ => {
					return Err("Supported db sources: `rocksdb` | `paritydb` | `auto`".to_string())
				}
			},
		},
	)?))
}

/// Builds a new service for a full client.
pub fn new_full(mut config: Configuration, cli: &Cli) -> Result<TaskManager, ServiceError> {
	// new_full_base(config, |_, _| (), cli)
	// .map(|NewFullBase { task_manager, .. }| task_manager)
	let sc_service::PartialComponents {
		client,
		backend,
		mut task_manager,
		import_queue,
		keystore_container,
		select_chain,
		transaction_pool,
		other: (
			rpc_extensions_builder, 
			import_setup, 
			rpc_setup, 
			mut telemetry,
			frontier_backend,
			filter_pool,
			(fee_history_cache, fee_history_cache_limit)
		),
	} = new_partial(&config, &cli)?;

	let shared_voter_state = rpc_setup;
	let grandpa_protocol_name = sc_finality_grandpa::protocol_standard_name(
		&client.block_hash(0).ok().flatten().expect("Genesis block exists; qed"),
		&config.chain_spec,
	);

	config
		.network
		.extra_sets
		.push(sc_finality_grandpa::grandpa_peers_set_config(grandpa_protocol_name.clone()));
	let warp_sync = Arc::new(sc_finality_grandpa::warp_proof::NetworkProvider::new(
		backend.clone(),
		import_setup.1.shared_authority_set().clone(),
		Vec::default(),
	));

	let (network, system_rpc_tx, network_starter) =
		sc_service::build_network(sc_service::BuildNetworkParams {
			config: &config,
			client: client.clone(),
			transaction_pool: transaction_pool.clone(),
			spawn_handle: task_manager.spawn_handle(),
			import_queue,
			block_announce_validator_builder: None,
			warp_sync: Some(warp_sync),
		})?;

	if config.offchain_worker.enabled {
		sc_service::build_offchain_workers(
			&config,
			task_manager.spawn_handle(),
			client.clone(),
			network.clone(),
		);
	}

	let role = config.role.clone();
	let force_authoring = config.force_authoring;
	let backoff_authoring_blocks =
		Some(sc_consensus_slots::BackoffAuthoringOnFinalizedHeadLagging::default());
	let name = config.network.node_name.clone();
	let enable_grandpa = !config.disable_grandpa;
	let prometheus_registry = config.prometheus_registry().cloned();
	let overrides = crate::rpc::overrides_handle(client.clone());

	let rpc_handlers = sc_service::spawn_tasks(sc_service::SpawnTasksParams {
		config,
		backend: backend.clone(),
		client: client.clone(),
		keystore: keystore_container.sync_keystore(),
		network: network.clone(),
		rpc_extensions_builder: Box::new(rpc_extensions_builder),
		transaction_pool: transaction_pool.clone(),
		task_manager: &mut task_manager,
		system_rpc_tx,
		telemetry: telemetry.as_mut(),
	})?;

	spawn_frontier_tasks(
		&task_manager,
		client.clone(),
		backend,
		frontier_backend,
		filter_pool,
		overrides,
		fee_history_cache,
		fee_history_cache_limit,
	);

	let (block_import, grandpa_link, babe_link) = import_setup;

	// let with_startup_data: (
	// 	&sc_consensus_babe::BabeBlockImport<Block, FullClient, FullGrandpaBlockImport>,
	// 	&sc_consensus_babe::BabeLink<Block>,
	// ) = ();
	// (with_startup_data)(&block_import, &babe_link);

	if let sc_service::config::Role::Authority { .. } = &role {
		let proposer = sc_basic_authorship::ProposerFactory::new(
			task_manager.spawn_handle(),
			client.clone(),
			transaction_pool.clone(),
			prometheus_registry.as_ref(),
			telemetry.as_ref().map(|x| x.handle()),
		);

		let can_author_with =
			sp_consensus::CanAuthorWithNativeVersion::new(client.executor().clone());

		let client_clone = client.clone();
		let slot_duration = babe_link.config().slot_duration();
		let babe_config = sc_consensus_babe::BabeParams {
			keystore: keystore_container.sync_keystore(),
			client: client.clone(),
			select_chain,
			env: proposer,
			block_import,
			sync_oracle: network.clone(),
			justification_sync_link: network.clone(),
			create_inherent_data_providers: move |parent, ()| {
				let client_clone = client_clone.clone();
				async move {
					let uncles = sc_consensus_uncles::create_uncles_inherent_data_provider(
						&*client_clone,
						parent,
					)?;

					let timestamp = sp_timestamp::InherentDataProvider::from_system_time();

					let slot =
						sp_consensus_babe::inherents::InherentDataProvider::from_timestamp_and_slot_duration(
							*timestamp,
							slot_duration,
						);

					let storage_proof =
						sp_transaction_storage_proof::registration::new_data_provider(
							&*client_clone,
							&parent,
						)?;

					Ok((timestamp, slot, uncles, storage_proof))
				}
			},
			force_authoring,
			backoff_authoring_blocks,
			babe_link,
			can_author_with,
			block_proposal_slot_portion: SlotProportion::new(0.5),
			max_block_proposal_slot_portion: None,
			telemetry: telemetry.as_ref().map(|x| x.handle()),
		};

		let babe = sc_consensus_babe::start_babe(babe_config)?;
		task_manager.spawn_essential_handle().spawn_blocking(
			"babe-proposer",
			Some("block-authoring"),
			babe,
		);
	}

	// if the node isn't actively participating in consensus then it doesn't
	// need a keystore, regardless of which protocol we use below.
	let keystore =
		if role.is_authority() { Some(keystore_container.sync_keystore()) } else { None };

	let config = sc_finality_grandpa::Config {
		// FIXME #1578 make this available through chainspec
		gossip_duration: std::time::Duration::from_millis(333),
		justification_period: 512,
		name: Some(name),
		observer_enabled: false,
		keystore,
		local_role: role,
		telemetry: telemetry.as_ref().map(|x| x.handle()),
		protocol_name: grandpa_protocol_name,
	};

	if enable_grandpa {
		// start the full GRANDPA voter
		// NOTE: non-authorities could run the GRANDPA observer protocol, but at
		// this point the full voter should provide better guarantees of block
		// and vote data availability than the observer. The observer has not
		// been tested extensively yet and having most nodes in a network run it
		// could lead to finality stalls.
		let grandpa_config = sc_finality_grandpa::GrandpaParams {
			config,
			link: grandpa_link,
			network: network.clone(),
			telemetry: telemetry.as_ref().map(|x| x.handle()),
			voting_rule: sc_finality_grandpa::VotingRulesBuilder::default().build(),
			prometheus_registry,
			shared_voter_state,
		};

		// the GRANDPA voter task is considered infallible, i.e.
		// if it fails we take down the service with it.
		task_manager.spawn_essential_handle().spawn_blocking(
			"grandpa-voter",
			None,
			sc_finality_grandpa::run_grandpa_voter(grandpa_config)?,
		);
	}

	network_starter.start_network();
	// NewFullBase { task_manager, client, network, transaction_pool, rpc_handlers };
	Ok(task_manager)
}

/// Result of [`new_full_base`].
pub struct NewFullBase {
	/// The task manager of the node.
	pub task_manager: TaskManager,
	/// The client instance of the node.
	pub client: Arc<FullClient>,
	/// The networking service of the node.
	pub network: Arc<NetworkService<Block, <Block as BlockT>::Hash>>,
	/// The transaction pool of the node.
	pub transaction_pool: Arc<TransactionPool>,
	/// The rpc handlers of the node.
	pub rpc_handlers: RpcHandlers,
}

/// Creates a full service from the configuration.
// pub fn new_full_base(
// 	mut config: Configuration,
// 	with_startup_data: impl FnOnce(
// 		&sc_consensus_babe::BabeBlockImport<Block, FullClient, FullGrandpaBlockImport>,
// 		&sc_consensus_babe::BabeLink<Block>,
// 	),
// 	cli: &Cli
// ) -> Result<NewFullBase, ServiceError> {
	
// }

/// Creates a new partial node.
pub fn new_partial(
	config: &Configuration,
	cli: &Cli,
) -> Result<
	sc_service::PartialComponents<
		FullClient,
		FullBackend,
		FullSelectChain,
		sc_consensus::DefaultImportQueue<Block, FullClient>,
		sc_transaction_pool::FullPool<Block, FullClient>,
		(
			impl Fn(
				DenyUnsafe,
				sc_rpc::SubscriptionTaskExecutor,
			) -> Result<IoHandler, sc_service::Error>,
			(
				sc_consensus_babe::BabeBlockImport<Block, FullClient, FullGrandpaBlockImport>,
				sc_finality_grandpa::LinkHalf<Block, FullClient, FullSelectChain>,
				sc_consensus_babe::BabeLink<Block>,
			),
			sc_finality_grandpa::SharedVoterState,
			Option<Telemetry>,
			// ConsensusResult,
			Arc<fc_db::Backend<Block>>,
			Option<FilterPool>,
			(FeeHistoryCache, FeeHistoryCacheLimit),
		),
	>,
	ServiceError,
> {
	use sc_network::warp_request_handler::WarpSyncProvider;

	if config.keystore_remote.is_some() {
		return Err(ServiceError::Other(
			"Remote Keystores are not supported.".to_string(),
		));
	}

	let telemetry = config
		.telemetry_endpoints
		.clone()
		.filter(|x| !x.is_empty())
		.map(|endpoints| -> Result<_, sc_telemetry::Error> {
			let worker = TelemetryWorker::new(16)?;
			let telemetry = worker.handle().new_telemetry(endpoints);
			Ok((worker, telemetry))
		})
		.transpose()?;

	let executor = NativeElseWasmExecutor::<ExecutorDispatch>::new(
		config.wasm_method,
		config.default_heap_pages,
		config.max_runtime_instances,
		config.runtime_cache_size,
	);

	let (client, backend, keystore_container, task_manager) =
		sc_service::new_full_parts::<Block, RuntimeApi, _>(
			config,
			telemetry.as_ref().map(|(_, telemetry)| telemetry.handle()),
			executor,
		)?;
	let client = Arc::new(client);

	let telemetry = telemetry.map(|(worker, telemetry)| {
		task_manager.spawn_handle().spawn("telemetry", None, worker.run());
		telemetry
	});

	let select_chain = sc_consensus::LongestChain::new(backend.clone());

	let transaction_pool = sc_transaction_pool::BasicPool::new_full(
		config.transaction_pool.clone(),
		config.role.is_authority().into(),
		config.prometheus_registry(),
		task_manager.spawn_essential_handle(),
		client.clone(),
	);
	let frontier_backend = open_frontier_backend(config)?;
	let filter_pool: Option<FilterPool> = Some(Arc::new(Mutex::new(BTreeMap::new())));
	let fee_history_cache: FeeHistoryCache = Arc::new(Mutex::new(BTreeMap::new()));
	let fee_history_cache_limit: FeeHistoryCacheLimit = cli.run.fee_history_limit;

	let (grandpa_block_import, grandpa_link) = sc_finality_grandpa::block_import(
		client.clone(),
		&(client.clone() as Arc<_>),
		select_chain.clone(),
		telemetry.as_ref().map(|x| x.handle()),
	)?;

	let frontier_block_import = FrontierBlockImport::new(
		grandpa_block_import.clone(),
		client.clone(),
		frontier_backend.clone(),
	);

	let target_gas_price = cli.run.target_gas_price;

	let justification_import = grandpa_block_import.clone();

	let (block_import, babe_link) = sc_consensus_babe::block_import(
		sc_consensus_babe::Config::get(&*client)?,
		grandpa_block_import.clone(),
		client.clone(),
	)?;

	let slot_duration = babe_link.config().slot_duration();
	let import_queue = sc_consensus_babe::import_queue(
		babe_link.clone(),
		block_import.clone(),
		Some(Box::new(justification_import.clone())),
		client.clone(),
		select_chain.clone(),
		move |_, ()| async move {
			let timestamp = sp_timestamp::InherentDataProvider::from_system_time();

			let slot =
				sp_consensus_babe::inherents::InherentDataProvider::from_timestamp_and_slot_duration(
					*timestamp,
					slot_duration,
				);

			let uncles =
				sp_authorship::InherentDataProvider::<<Block as BlockT>::Header>::check_inherents();

			Ok((timestamp, slot, uncles))
		},
		&task_manager.spawn_essential_handle(),
		config.prometheus_registry(),
		sp_consensus::CanAuthorWithNativeVersion::new(client.executor().clone()),
		telemetry.as_ref().map(|x| x.handle()),
	)?;
	let import_queue_ = sc_consensus_babe::import_queue(
		babe_link.clone(),
		block_import.clone(),
		Some(Box::new(justification_import)),
		client.clone(),
		select_chain.clone(),
		move |_, ()| async move {
			let timestamp = sp_timestamp::InherentDataProvider::from_system_time();

			let slot =
				sp_consensus_babe::inherents::InherentDataProvider::from_timestamp_and_slot_duration(
					*timestamp,
					slot_duration,
				);

			let uncles =
				sp_authorship::InherentDataProvider::<<Block as BlockT>::Header>::check_inherents();

			Ok((timestamp, slot, uncles))
		},
		&task_manager.spawn_essential_handle(),
		config.prometheus_registry(),
		sp_consensus::CanAuthorWithNativeVersion::new(client.executor().clone()),
		telemetry.as_ref().map(|x| x.handle()),
	)?;
	let warp_sync: Option<Arc<dyn WarpSyncProvider<Block>>> = Some(Arc::new(
		sc_finality_grandpa::warp_proof::NetworkProvider::new(
			backend.clone(),
			grandpa_link.shared_authority_set().clone(),
			Vec::default(),
		),
	));
	let (network, system_rpc_tx, network_starter) =
		sc_service::build_network(sc_service::BuildNetworkParams {
			config: &config,
			client: client.clone(),
			transaction_pool: transaction_pool.clone(),
			spawn_handle: task_manager.spawn_handle(),
			import_queue: import_queue_,
			block_announce_validator_builder: None,
			warp_sync,
		})?;

	let import_setup = (block_import, grandpa_link, babe_link);

	let (rpc_extensions_builder, rpc_setup) = {
		let (_, grandpa_link, babe_link) = &import_setup;

		let justification_stream = grandpa_link.justification_stream();
		let shared_authority_set = grandpa_link.shared_authority_set().clone();
		let shared_voter_state = sc_finality_grandpa::SharedVoterState::empty();
		let rpc_setup = shared_voter_state.clone();

		let finality_proof_provider = sc_finality_grandpa::FinalityProofProvider::new_for_service(
			backend.clone(),
			Some(shared_authority_set.clone()),
		);

		let babe_config = babe_link.config().clone();
		let shared_epoch_changes = babe_link.epoch_changes().clone();

		let client = client.clone();
		let pool = transaction_pool.clone();
		let select_chain = select_chain.clone();
		let keystore = keystore_container.sync_keystore();
		let chain_spec = config.chain_spec.cloned_box();
		let role = config.role.clone();
		let is_authority = role.is_authority();
		let enable_dev_signer = cli.run.enable_dev_signer;
		let network = network.clone();
		let filter_pool = filter_pool.clone();
		let frontier_backend = frontier_backend.clone();
		let max_past_logs = cli.run.max_past_logs;
		let fee_history_cache = fee_history_cache.clone();
		let prometheus_registry = config.prometheus_registry().cloned();
		let overrides = crate::rpc::overrides_handle(client.clone());
		let block_data_cache = Arc::new(fc_rpc::EthBlockDataCacheTask::new(
			task_manager.spawn_handle(),
			overrides.clone(),
			50,
			50,
			prometheus_registry.clone(),
		));

		let rpc_extensions_builder = move |deny_unsafe, subscription_executor| {
			let deps = FullDeps {
				client: client.clone(),
				pool: pool.clone(),
				graph: pool.pool().clone(),
				select_chain: select_chain.clone(),
				chain_spec: chain_spec.cloned_box(),
				deny_unsafe,
				is_authority,
				babe: BabeDeps {
					babe_config: babe_config.clone(),
					shared_epoch_changes: shared_epoch_changes.clone(),
					keystore: keystore.clone(),
				},
				grandpa: GrandpaDeps {
					shared_voter_state: shared_voter_state.clone(),
					shared_authority_set: shared_authority_set.clone(),
					justification_stream: justification_stream.clone(),
					subscription_executor,
					finality_provider: finality_proof_provider.clone(),
				},
				enable_dev_signer,
				network: network.clone(),
				filter_pool: filter_pool.clone(),
				backend: frontier_backend.clone(),
				max_past_logs,
				fee_history_cache: fee_history_cache.clone(),
				fee_history_cache_limit,
				overrides: overrides.clone(),
				block_data_cache: block_data_cache.clone(),
			};

			create_full(deps).map_err(Into::into)
		};

		(rpc_extensions_builder, rpc_setup)
	};

	Ok(sc_service::PartialComponents {
		client,
		backend,
		task_manager,
		keystore_container,
		select_chain,
		import_queue,
		transaction_pool,
		other: (
			rpc_extensions_builder, 
			import_setup, 
			rpc_setup, 
			telemetry,
			frontier_backend,
			filter_pool,
			(fee_history_cache, fee_history_cache_limit),
		),
	})
}


fn spawn_frontier_tasks(
	task_manager: &TaskManager,
	client: Arc<FullClient>,
	backend: Arc<FullBackend>,
	frontier_backend: Arc<fc_db::Backend<Block>>,
	filter_pool: Option<FilterPool>,
	overrides: Arc<OverrideHandle<Block>>,
	fee_history_cache: FeeHistoryCache,
	fee_history_cache_limit: FeeHistoryCacheLimit,
) {
	task_manager.spawn_essential_handle().spawn(
		"frontier-mapping-sync-worker",
		None,
		MappingSyncWorker::new(
			client.import_notification_stream(),
			Duration::new(6, 0),
			client.clone(),
			backend,
			frontier_backend.clone(),
			3,
			0,
			SyncStrategy::Normal,
		)
		.for_each(|()| future::ready(())),
	);

	// Spawn Frontier EthFilterApi maintenance task.
	if let Some(filter_pool) = filter_pool {
		// Each filter is allowed to stay in the pool for 100 blocks.
		const FILTER_RETAIN_THRESHOLD: u64 = 100;
		task_manager.spawn_essential_handle().spawn(
			"frontier-filter-pool",
			None,
			EthTask::filter_pool_task(client.clone(), filter_pool, FILTER_RETAIN_THRESHOLD),
		);
	}

	// Spawn Frontier FeeHistory cache maintenance task.
	task_manager.spawn_essential_handle().spawn(
		"frontier-fee-history",
		None,
		EthTask::fee_history_task(
			client.clone(),
			overrides,
			fee_history_cache,
			fee_history_cache_limit,
		),
	);

	task_manager.spawn_essential_handle().spawn(
		"frontier-schema-cache-task",
		None,
		EthTask::ethereum_schema_cache_task(client, frontier_backend),
	);
}
