use std::sync::Arc;

use garage_util::background::*;
use garage_util::config::*;

use garage_rpc::membership::System;
use garage_rpc::rpc_client::RpcHttpClient;
use garage_rpc::rpc_server::RpcServer;

use garage_table::table_fullcopy::*;
use garage_table::table_sharded::*;
use garage_table::*;

use crate::block::*;
use crate::block_ref_table::*;
use crate::bucket_table::*;
use crate::key_table::*;
use crate::object_table::*;
use crate::version_table::*;

pub struct Garage {
	pub config: Config,

	pub db: sled::Db,
	pub background: Arc<BackgroundRunner>,
	pub system: Arc<System>,
	pub block_manager: Arc<BlockManager>,

	pub bucket_table: Arc<Table<BucketTable, TableFullReplication>>,
	pub key_table: Arc<Table<KeyTable, TableFullReplication>>,

	pub object_table: Arc<Table<ObjectTable, TableShardedReplication>>,
	pub version_table: Arc<Table<VersionTable, TableShardedReplication>>,
	pub block_ref_table: Arc<Table<BlockRefTable, TableShardedReplication>>,
}

impl Garage {
	pub async fn new(
		config: Config,
		db: sled::Db,
		background: Arc<BackgroundRunner>,
		rpc_server: &mut RpcServer,
	) -> Arc<Self> {
		info!("Initialize membership management system...");
		let rpc_http_client = Arc::new(
			RpcHttpClient::new(config.max_concurrent_rpc_requests, &config.rpc_tls)
				.expect("Could not create RPC client"),
		);
		let system = System::new(
			config.metadata_dir.clone(),
			rpc_http_client,
			background.clone(),
			rpc_server,
		);

		let data_rep_param = TableShardedReplication {
			replication_factor: config.data_replication_factor,
			write_quorum: (config.data_replication_factor + 1) / 2,
			read_quorum: 1,
		};

		let meta_rep_param = TableShardedReplication {
			replication_factor: config.meta_replication_factor,
			write_quorum: (config.meta_replication_factor + 1) / 2,
			read_quorum: (config.meta_replication_factor + 1) / 2,
		};

		let control_rep_param = TableFullReplication::new(
			config.meta_epidemic_fanout,
			(config.meta_epidemic_fanout + 1) / 2,
		);

		info!("Initialize block manager...");
		let block_manager = BlockManager::new(
			&db,
			config.data_dir.clone(),
			data_rep_param.clone(),
			system.clone(),
			rpc_server,
		);

		info!("Initialize block_ref_table...");
		let block_ref_table = Table::new(
			BlockRefTable {
				background: background.clone(),
				block_manager: block_manager.clone(),
			},
			data_rep_param.clone(),
			system.clone(),
			&db,
			"block_ref".to_string(),
			rpc_server,
		)
		.await;

		info!("Initialize version_table...");
		let version_table = Table::new(
			VersionTable {
				background: background.clone(),
				block_ref_table: block_ref_table.clone(),
			},
			meta_rep_param.clone(),
			system.clone(),
			&db,
			"version".to_string(),
			rpc_server,
		)
		.await;

		info!("Initialize object_table...");
		let object_table = Table::new(
			ObjectTable {
				background: background.clone(),
				version_table: version_table.clone(),
			},
			meta_rep_param.clone(),
			system.clone(),
			&db,
			"object".to_string(),
			rpc_server,
		)
		.await;

		info!("Initialize bucket_table...");
		let bucket_table = Table::new(
			BucketTable,
			control_rep_param.clone(),
			system.clone(),
			&db,
			"bucket".to_string(),
			rpc_server,
		)
		.await;

		info!("Initialize key_table_table...");
		let key_table = Table::new(
			KeyTable,
			control_rep_param.clone(),
			system.clone(),
			&db,
			"key".to_string(),
			rpc_server,
		)
		.await;

		info!("Initialize Garage...");
		let garage = Arc::new(Self {
			config,
			db,
			system: system.clone(),
			block_manager,
			background,
			bucket_table,
			key_table,
			object_table,
			version_table,
			block_ref_table,
		});

		info!("Start block manager background thread...");
		garage.block_manager.garage.swap(Some(garage.clone()));
		garage.block_manager.clone().spawn_background_worker().await;

		garage
	}
}
