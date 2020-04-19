use std::collections::HashMap;
use std::hash::Hash as StdHash;
use std::hash::Hasher;
use std::io::Read;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::future::join_all;
use futures::select;
use futures_util::future::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::prelude::*;
use tokio::sync::watch;
use tokio::sync::Mutex;

use crate::background::BackgroundRunner;
use crate::data::*;
use crate::error::Error;
use crate::rpc_client::*;
use crate::rpc_server::*;
use crate::server::Config;

const PING_INTERVAL: Duration = Duration::from_secs(10);
const PING_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_FAILED_PINGS: usize = 3;

#[derive(Debug, Serialize, Deserialize)]
pub enum Message {
	Ok,
	Ping(PingMessage),
	PullStatus,
	PullConfig,
	AdvertiseNodesUp(Vec<AdvertisedNode>),
	AdvertiseConfig(NetworkConfig),
}

impl RpcMessage for Message {}

#[derive(Debug, Serialize, Deserialize)]
pub struct PingMessage {
	pub id: UUID,
	pub rpc_port: u16,

	pub status_hash: Hash,
	pub config_version: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AdvertisedNode {
	pub id: UUID,
	pub addr: SocketAddr,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NetworkConfig {
	pub members: HashMap<UUID, NetworkConfigEntry>,
	pub version: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NetworkConfigEntry {
	pub datacenter: String,
	pub n_tokens: u32,
}

pub struct System {
	pub config: Config,
	pub id: UUID,

	pub rpc_http_client: Arc<RpcHttpClient>,
	rpc_client: Arc<RpcClient<Message>>,

	pub status: watch::Receiver<Arc<Status>>,
	pub ring: watch::Receiver<Arc<Ring>>,

	update_lock: Mutex<(watch::Sender<Arc<Status>>, watch::Sender<Arc<Ring>>)>,

	pub background: Arc<BackgroundRunner>,
}

#[derive(Debug, Clone)]
pub struct Status {
	pub nodes: HashMap<UUID, NodeStatus>,
	pub hash: Hash,
}

#[derive(Debug, Clone)]
pub struct NodeStatus {
	pub addr: SocketAddr,
	pub remaining_ping_attempts: usize,
}

#[derive(Clone)]
pub struct Ring {
	pub config: NetworkConfig,
	pub ring: Vec<RingEntry>,
	pub n_datacenters: usize,
}

#[derive(Clone, Debug)]
pub struct RingEntry {
	pub location: Hash,
	pub node: UUID,
	pub datacenter: u64,
}

impl Status {
	fn handle_ping(&mut self, ip: IpAddr, info: &PingMessage) -> bool {
		let addr = SocketAddr::new(ip, info.rpc_port);
		let old_status = self.nodes.insert(
			info.id.clone(),
			NodeStatus {
				addr: addr.clone(),
				remaining_ping_attempts: MAX_FAILED_PINGS,
			},
		);
		match old_status {
			None => {
				eprintln!("Newly pingable node: {}", hex::encode(&info.id));
				true
			}
			Some(x) => x.addr != addr,
		}
	}

	fn recalculate_hash(&mut self) {
		let mut nodes = self.nodes.iter().collect::<Vec<_>>();
		nodes.sort_unstable_by_key(|(id, _status)| *id);

		let mut hasher = Sha256::new();
		eprintln!("Current set of pingable nodes: --");
		for (id, status) in nodes {
			eprintln!("{} {}", hex::encode(&id), status.addr);
			hasher.input(format!("{} {}\n", hex::encode(&id), status.addr));
		}
		eprintln!("END --");
		self.hash
			.as_slice_mut()
			.copy_from_slice(&hasher.result()[..]);
	}
}

impl Ring {
	fn rebuild_ring(&mut self) {
		let mut new_ring = vec![];
		let mut datacenters = vec![];

		for (id, config) in self.config.members.iter() {
			let mut dc_hasher = std::collections::hash_map::DefaultHasher::new();
			config.datacenter.hash(&mut dc_hasher);
			let datacenter = dc_hasher.finish();

			if !datacenters.contains(&datacenter) {
				datacenters.push(datacenter);
			}

			for i in 0..config.n_tokens {
				let location = hash(format!("{} {}", hex::encode(&id), i).as_bytes());

				new_ring.push(RingEntry {
					location: location.into(),
					node: id.clone(),
					datacenter,
				})
			}
		}

		new_ring.sort_unstable_by(|x, y| x.location.cmp(&y.location));
		self.ring = new_ring;
		self.n_datacenters = datacenters.len();

		// eprintln!("RING: --");
		// for e in self.ring.iter() {
		// 	eprintln!("{:?}", e);
		// }
		// eprintln!("END --");
	}

	pub fn walk_ring(&self, from: &Hash, n: usize) -> Vec<UUID> {
		if n >= self.config.members.len() {
			return self.config.members.keys().cloned().collect::<Vec<_>>();
		}

		let start = match self.ring.binary_search_by(|x| x.location.cmp(from)) {
			Ok(i) => i,
			Err(i) => {
				if i == 0 {
					self.ring.len() - 1
				} else {
					i - 1
				}
			}
		};

		self.walk_ring_from_pos(start, n)
	}

	fn walk_ring_from_pos(&self, start: usize, n: usize) -> Vec<UUID> {
		if n >= self.config.members.len() {
			return self.config.members.keys().cloned().collect::<Vec<_>>();
		}

		let mut ret = vec![];
		let mut datacenters = vec![];

		let mut delta = 0;
		while ret.len() < n {
			let i = (start + delta) % self.ring.len();
			delta += 1;

			if !datacenters.contains(&self.ring[i].datacenter) {
				ret.push(self.ring[i].node.clone());
				datacenters.push(self.ring[i].datacenter);
			} else if datacenters.len() == self.n_datacenters && !ret.contains(&self.ring[i].node) {
				ret.push(self.ring[i].node.clone());
			}
		}

		ret
	}
}

fn read_network_config(metadata_dir: &PathBuf) -> Result<NetworkConfig, Error> {
	let mut path = metadata_dir.clone();
	path.push("network_config");

	let mut file = std::fs::OpenOptions::new()
		.read(true)
		.open(path.as_path())?;

	let mut net_config_bytes = vec![];
	file.read_to_end(&mut net_config_bytes)?;

	let net_config = rmp_serde::decode::from_read_ref(&net_config_bytes[..])?;

	Ok(net_config)
}

impl System {
	pub fn new(
		config: Config,
		id: UUID,
		background: Arc<BackgroundRunner>,
		rpc_server: &mut RpcServer,
	) -> Arc<Self> {
		let net_config = match read_network_config(&config.metadata_dir) {
			Ok(x) => x,
			Err(e) => {
				println!(
					"No valid previous network configuration stored ({}), starting fresh.",
					e
				);
				NetworkConfig {
					members: HashMap::new(),
					version: 0,
				}
			}
		};
		let mut status = Status {
			nodes: HashMap::new(),
			hash: Hash::default(),
		};
		status.recalculate_hash();
		let (update_status, status) = watch::channel(Arc::new(status));

		let mut ring = Ring {
			config: net_config,
			ring: Vec::new(),
			n_datacenters: 0,
		};
		ring.rebuild_ring();
		let (update_ring, ring) = watch::channel(Arc::new(ring));

		let rpc_http_client =
			Arc::new(RpcHttpClient::new(&config.rpc_tls).expect("Could not create RPC client"));

		let rpc_path = "_membership";
		let rpc_client = RpcClient::new(
			RpcAddrClient::<Message>::new(rpc_http_client.clone(), rpc_path.into()),
			background.clone(),
			status.clone(),
		);

		let sys = Arc::new(System {
			config,
			id,
			rpc_http_client,
			rpc_client,
			status,
			ring,
			update_lock: Mutex::new((update_status, update_ring)),
			background,
		});
		sys.clone().register_handler(rpc_server, rpc_path.into());
		sys
	}

	fn register_handler(self: Arc<Self>, rpc_server: &mut RpcServer, path: String) {
		rpc_server.add_handler::<Message, _, _>(path, move |msg, addr| {
			let self2 = self.clone();
			async move {
				match msg {
					Message::Ping(ping) => self2.handle_ping(&addr, &ping).await,

					Message::PullStatus => self2.handle_pull_status(),
					Message::PullConfig => self2.handle_pull_config(),
					Message::AdvertiseNodesUp(adv) => self2.handle_advertise_nodes_up(&adv).await,
					Message::AdvertiseConfig(adv) => self2.handle_advertise_config(&adv).await,

					_ => Err(Error::Message(format!("Unexpected RPC message"))),
				}
			}
		});
	}

	pub fn rpc_client<M: RpcMessage + 'static>(self: &Arc<Self>, path: &str) -> Arc<RpcClient<M>> {
		RpcClient::new(
			RpcAddrClient::new(self.rpc_http_client.clone(), path.to_string()),
			self.background.clone(),
			self.status.clone(),
		)
	}

	async fn save_network_config(self: Arc<Self>) -> Result<(), Error> {
		let mut path = self.config.metadata_dir.clone();
		path.push("network_config");

		let ring = self.ring.borrow().clone();
		let data = rmp_to_vec_all_named(&ring.config)?;

		let mut f = tokio::fs::File::create(path.as_path()).await?;
		f.write_all(&data[..]).await?;
		Ok(())
	}

	pub fn make_ping(&self) -> Message {
		let status = self.status.borrow().clone();
		let ring = self.ring.borrow().clone();
		Message::Ping(PingMessage {
			id: self.id.clone(),
			rpc_port: self.config.rpc_port,
			status_hash: status.hash.clone(),
			config_version: ring.config.version,
		})
	}

	pub async fn broadcast(self: Arc<Self>, msg: Message, timeout: Duration) {
		let status = self.status.borrow().clone();
		let to = status
			.nodes
			.keys()
			.filter(|x| **x != self.id)
			.cloned()
			.collect::<Vec<_>>();
		self.rpc_client.call_many(&to[..], msg, timeout).await;
	}

	pub async fn bootstrap(self: Arc<Self>) {
		let bootstrap_peers = self
			.config
			.bootstrap_peers
			.iter()
			.map(|ip| (ip.clone(), None))
			.collect::<Vec<_>>();
		self.clone().ping_nodes(bootstrap_peers).await;

		self.clone()
			.background
			.spawn_worker(|stop_signal| self.ping_loop(stop_signal).map(Ok))
			.await;
	}

	async fn ping_nodes(self: Arc<Self>, peers: Vec<(SocketAddr, Option<UUID>)>) {
		let ping_msg = self.make_ping();
		let ping_resps = join_all(peers.iter().map(|(addr, id_option)| {
			let sys = self.clone();
			let ping_msg_ref = &ping_msg;
			async move {
				(
					id_option,
					addr.clone(),
					sys.rpc_client
						.by_addr()
						.call(&addr, ping_msg_ref, PING_TIMEOUT)
						.await,
				)
			}
		}))
		.await;

		let update_locked = self.update_lock.lock().await;
		let mut status: Status = self.status.borrow().as_ref().clone();
		let ring = self.ring.borrow().clone();

		let mut has_changes = false;
		let mut to_advertise = vec![];

		for (id_option, addr, ping_resp) in ping_resps {
			if let Ok(Message::Ping(info)) = ping_resp {
				let is_new = status.handle_ping(addr.ip(), &info);
				if is_new {
					has_changes = true;
					to_advertise.push(AdvertisedNode {
						id: info.id.clone(),
						addr: addr.clone(),
					});
				}
				if is_new || status.hash != info.status_hash {
					self.background
						.spawn_cancellable(self.clone().pull_status(info.id.clone()).map(Ok));
				}
				if is_new || ring.config.version < info.config_version {
					self.background
						.spawn_cancellable(self.clone().pull_config(info.id.clone()).map(Ok));
				}
			} else if let Some(id) = id_option {
				let remaining_attempts = status
					.nodes
					.get(id)
					.map(|x| x.remaining_ping_attempts)
					.unwrap_or(0);
				if remaining_attempts == 0 {
					eprintln!(
						"Removing node {} after too many failed pings",
						hex::encode(&id)
					);
					status.nodes.remove(&id);
					has_changes = true;
				} else {
					if let Some(st) = status.nodes.get_mut(id) {
						st.remaining_ping_attempts = remaining_attempts - 1;
					}
				}
			}
		}
		if has_changes {
			status.recalculate_hash();
		}
		if let Err(e) = update_locked.0.broadcast(Arc::new(status)) {
			eprintln!("In ping_nodes: could not save status update ({})", e);
		}
		drop(update_locked);

		if to_advertise.len() > 0 {
			self.broadcast(Message::AdvertiseNodesUp(to_advertise), PING_TIMEOUT)
				.await;
		}
	}

	pub async fn handle_ping(
		self: Arc<Self>,
		from: &SocketAddr,
		ping: &PingMessage,
	) -> Result<Message, Error> {
		let update_locked = self.update_lock.lock().await;
		let mut status: Status = self.status.borrow().as_ref().clone();

		let is_new = status.handle_ping(from.ip(), ping);
		if is_new {
			status.recalculate_hash();
		}
		let status_hash = status.hash.clone();
		let config_version = self.ring.borrow().config.version;

		update_locked.0.broadcast(Arc::new(status))?;
		drop(update_locked);

		if is_new || status_hash != ping.status_hash {
			self.background
				.spawn_cancellable(self.clone().pull_status(ping.id.clone()).map(Ok));
		}
		if is_new || config_version < ping.config_version {
			self.background
				.spawn_cancellable(self.clone().pull_config(ping.id.clone()).map(Ok));
		}

		Ok(self.make_ping())
	}

	pub fn handle_pull_status(&self) -> Result<Message, Error> {
		let status = self.status.borrow().clone();
		let mut mem = vec![];
		for (node, status) in status.nodes.iter() {
			mem.push(AdvertisedNode {
				id: node.clone(),
				addr: status.addr.clone(),
			});
		}
		Ok(Message::AdvertiseNodesUp(mem))
	}

	pub fn handle_pull_config(&self) -> Result<Message, Error> {
		let ring = self.ring.borrow().clone();
		Ok(Message::AdvertiseConfig(ring.config.clone()))
	}

	pub async fn handle_advertise_nodes_up(
		self: Arc<Self>,
		adv: &[AdvertisedNode],
	) -> Result<Message, Error> {
		let mut to_ping = vec![];

		let update_lock = self.update_lock.lock().await;
		let mut status: Status = self.status.borrow().as_ref().clone();
		let mut has_changed = false;

		for node in adv.iter() {
			if node.id == self.id {
				// learn our own ip address
				let self_addr = SocketAddr::new(node.addr.ip(), self.config.rpc_port);
				let old_self = status.nodes.insert(
					node.id.clone(),
					NodeStatus {
						addr: self_addr,
						remaining_ping_attempts: MAX_FAILED_PINGS,
					},
				);
				has_changed = match old_self {
					None => true,
					Some(x) => x.addr != self_addr,
				};
			} else if !status.nodes.contains_key(&node.id) {
				to_ping.push((node.addr.clone(), Some(node.id.clone())));
			}
		}
		if has_changed {
			status.recalculate_hash();
		}
		update_lock.0.broadcast(Arc::new(status))?;
		drop(update_lock);

		if to_ping.len() > 0 {
			self.background
				.spawn_cancellable(self.clone().ping_nodes(to_ping).map(Ok));
		}

		Ok(Message::Ok)
	}

	pub async fn handle_advertise_config(
		self: Arc<Self>,
		adv: &NetworkConfig,
	) -> Result<Message, Error> {
		let mut ring: Ring = self.ring.borrow().as_ref().clone();
		let update_lock = self.update_lock.lock().await;

		if adv.version > ring.config.version {
			ring.config = adv.clone();
			ring.rebuild_ring();
			update_lock.1.broadcast(Arc::new(ring))?;
			drop(update_lock);

			self.background.spawn_cancellable(
				self.clone()
					.broadcast(Message::AdvertiseConfig(adv.clone()), PING_TIMEOUT)
					.map(Ok),
			);
			self.background.spawn(self.clone().save_network_config());
		}

		Ok(Message::Ok)
	}

	pub async fn ping_loop(self: Arc<Self>, mut stop_signal: watch::Receiver<bool>) {
		loop {
			let restart_at = tokio::time::delay_for(PING_INTERVAL);

			let status = self.status.borrow().clone();
			let ping_addrs = status
				.nodes
				.iter()
				.filter(|(id, _)| **id != self.id)
				.map(|(id, status)| (status.addr.clone(), Some(id.clone())))
				.collect::<Vec<_>>();

			self.clone().ping_nodes(ping_addrs).await;

			select! {
				_ = restart_at.fuse() => (),
				must_exit = stop_signal.recv().fuse() => {
					match must_exit {
						None | Some(true) => return,
						_ => (),
					}
				}
			}
		}
	}

	pub fn pull_status(
		self: Arc<Self>,
		peer: UUID,
	) -> impl futures::future::Future<Output = ()> + Send + 'static {
		async move {
			let resp = self
				.rpc_client
				.call(&peer, Message::PullStatus, PING_TIMEOUT)
				.await;
			if let Ok(Message::AdvertiseNodesUp(nodes)) = resp {
				let _: Result<_, _> = self.handle_advertise_nodes_up(&nodes).await;
			}
		}
	}

	pub async fn pull_config(self: Arc<Self>, peer: UUID) {
		let resp = self
			.rpc_client
			.call(&peer, Message::PullConfig, PING_TIMEOUT)
			.await;
		if let Ok(Message::AdvertiseConfig(config)) = resp {
			let _: Result<_, _> = self.handle_advertise_config(&config).await;
		}
	}
}
