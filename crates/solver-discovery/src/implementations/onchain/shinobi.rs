//! Shinobi Cash on-chain intent discovery implementation.
//!
//! This module monitors blockchain events for Shinobi Cash cross-chain intents
//! and converts them into intents for the solver to process.

use crate::{DiscoveryError, DiscoveryInterface};
use alloy_primitives::Address as AlloyAddress;
use alloy_provider::{Provider, ProviderBuilder, RootProvider};
use alloy_pubsub::PubSubFrontend;
use alloy_rpc_types::{Filter, Log};
use alloy_sol_types::{SolEvent, SolValue};
use alloy_transport_http::Http;
use alloy_transport_ws::WsConnect;
use async_trait::async_trait;
use futures::StreamExt;
use solver_types::current_timestamp;
use solver_types::{
	with_0x_prefix, ConfigSchema, IShinobiInputSettler, Intent, IntentMetadata, NetworksConfig,
	ShinobiIntent, ShinobiIntentSol,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, Mutex};
use tokio::task::JoinHandle;

const DEFAULT_POLLING_INTERVAL_SECS: u64 = 3;

/// Provider types for different transport modes.
enum ProviderType {
	/// HTTP provider for polling mode.
	Http(RootProvider<Http<reqwest::Client>>),
	/// WebSocket provider for subscription mode.
	WebSocket(RootProvider<PubSubFrontend>),
}

/// Shinobi Cash on-chain discovery implementation.
///
/// Monitors blockchain events for new Shinobi Cash intents (both withdrawals and deposits)
/// and converts them into intents for the solver to process.
/// Supports monitoring multiple chains concurrently using either HTTP polling
/// or WebSocket subscriptions (when polling_interval_secs = 0).
pub struct ShinobiDiscovery {
	/// RPC providers for each monitored network.
	providers: HashMap<u64, ProviderType>,
	/// The chain IDs being monitored.
	network_ids: Vec<u64>,
	/// Networks configuration.
	networks: NetworksConfig,
	/// Input settler addresses by chain ID.
	input_settlers: HashMap<u64, AlloyAddress>,
	/// The last processed block number for each chain (HTTP mode only).
	last_blocks: Arc<Mutex<HashMap<u64, u64>>>,
	/// Flag indicating if monitoring is active.
	is_monitoring: Arc<AtomicBool>,
	/// Handles for monitoring tasks.
	monitoring_handles: Arc<Mutex<Vec<JoinHandle<()>>>>,
	/// Channel for signaling monitoring shutdown.
	stop_signal: Arc<Mutex<Option<broadcast::Sender<()>>>>,
	/// Polling interval for monitoring loop in seconds (0 = WebSocket mode).
	polling_interval_secs: u64,
	/// Lock type for discovered intents.
	lock_type: String,
}

impl ShinobiDiscovery {
	/// Creates a new Shinobi discovery instance.
	///
	/// # Arguments
	/// * `input_settlers` - Map of chain_id -> InputSettler address
	/// * `networks` - Network configuration
	/// * `polling_interval_secs` - Polling interval (0 = WebSocket mode)
	/// * `lock_type` - Lock type for intents (typically "native_escrow")
	pub async fn new(
		input_settlers: HashMap<u64, AlloyAddress>,
		networks: NetworksConfig,
		polling_interval_secs: Option<u64>,
		lock_type: String,
	) -> Result<Self, DiscoveryError> {
		// Validate at least one settler configured
		if input_settlers.is_empty() {
			return Err(DiscoveryError::ValidationError(
				"At least one input settler must be configured".to_string(),
			));
		}

		let network_ids: Vec<u64> = input_settlers.keys().copied().collect();
		let interval = polling_interval_secs.unwrap_or(DEFAULT_POLLING_INTERVAL_SECS);
		let use_websocket = interval == 0;

		// Create providers and get initial blocks
		let mut providers = HashMap::new();
		let mut last_blocks = HashMap::new();

		for network_id in &network_ids {
			// Validate network exists
			let network = networks.get(network_id).ok_or_else(|| {
				DiscoveryError::ValidationError(format!(
					"Network {} not found in configuration",
					network_id
				))
			})?;

			if use_websocket {
				// WebSocket mode
				let ws_url = network.get_ws_url().ok_or_else(|| {
					DiscoveryError::Connection(format!(
						"No WebSocket RPC URL configured for network {}",
						network_id
					))
				})?;

				tracing::info!(
					"Creating WebSocket provider for Shinobi discovery on network {}: {}",
					network_id,
					ws_url
				);

				let ws_connect = WsConnect::new(ws_url.to_string());
				let provider = ProviderBuilder::new()
					.with_recommended_fillers()
					.on_ws(ws_connect)
					.await
					.map_err(|e| {
						DiscoveryError::Connection(format!(
							"Failed to connect to WebSocket for network {}: {}",
							network_id, e
						))
					})?;

				let root_provider = provider.root().clone();
				providers.insert(*network_id, ProviderType::WebSocket(root_provider));
			} else {
				// HTTP polling mode
				let http_url = network.get_http_url().ok_or_else(|| {
					DiscoveryError::Connection(format!(
						"No HTTP RPC URL configured for network {}",
						network_id
					))
				})?;

				tracing::info!(
					"Creating HTTP provider for Shinobi discovery on network {}: {}",
					network_id,
					http_url
				);

				let provider = RootProvider::new_http(http_url.parse().map_err(|e| {
					DiscoveryError::Connection(format!(
						"Invalid RPC URL for network {}: {}",
						network_id, e
					))
				})?);

				// Get current block number
				let current_block = provider.get_block_number().await.map_err(|e| {
					DiscoveryError::Connection(format!(
						"Failed to get block number for network {}: {}",
						network_id, e
					))
				})?;

				providers.insert(*network_id, ProviderType::Http(provider));
				last_blocks.insert(*network_id, current_block);

				tracing::info!(
					"Initialized Shinobi discovery for network {} at block {}",
					network_id,
					current_block
				);
			}
		}

		Ok(Self {
			providers,
			network_ids,
			networks,
			input_settlers,
			last_blocks: Arc::new(Mutex::new(last_blocks)),
			is_monitoring: Arc::new(AtomicBool::new(false)),
			monitoring_handles: Arc::new(Mutex::new(Vec::new())),
			stop_signal: Arc::new(Mutex::new(None)),
			polling_interval_secs: interval,
			lock_type,
		})
	}

	/// Convert Solidity ShinobiIntent event data to solver Intent type.
	fn convert_to_intent(
		&self,
		order_id: [u8; 32],
		sol_intent: ShinobiIntentSol,
		block_timestamp: u64,
	) -> Result<Intent, DiscoveryError> {
		// Convert ShinobiIntentSol to Rust ShinobiIntent
		let shinobi_intent: ShinobiIntent = sol_intent.clone().into();

		// ABI-encode the intent for order_bytes
		let order_bytes = sol_intent.abi_encode().into();

		// Create Intent
		Ok(Intent {
			id: with_0x_prefix(&hex::encode(order_id)),
			source: "on-chain".to_string(),
			standard: "shinobi".to_string(),
			metadata: IntentMetadata {
				requires_auction: false,
				exclusive_until: None,
				discovered_at: block_timestamp,
			},
			data: serde_json::to_value(&shinobi_intent).map_err(|e| {
				DiscoveryError::ParseError(format!("Failed to serialize intent data: {}", e))
			})?,
			order_bytes,
			quote_id: None,
			lock_type: self.lock_type.clone(),
		})
	}

	/// Process Open event and convert to Intent.
	async fn process_open_event(
		&self,
		log: &Log,
		_chain_id: u64,
	) -> Result<Intent, DiscoveryError> {
		// Decode the Open event
		let open_event = IShinobiInputSettler::Open::decode_log_data(&log.inner.data, true)
			.map_err(|e| {
				DiscoveryError::ParseError(format!("Failed to decode Open event: {}", e))
			})?;

		// Get block timestamp
		let block_timestamp = log
			.block_timestamp
			.map(|ts| ts as u64)
			.unwrap_or_else(current_timestamp);

		// Convert to Intent
		self.convert_to_intent(
			*open_event.orderId,
			open_event.intent.clone(),
			block_timestamp,
		)
	}

	/// Monitor events on a specific chain (HTTP polling mode).
	async fn monitor_chain_http(
		&self,
		chain_id: u64,
		provider: RootProvider<Http<reqwest::Client>>,
		settler_address: AlloyAddress,
		sender: mpsc::UnboundedSender<Intent>,
		mut stop_rx: broadcast::Receiver<()>,
	) {
		let mut interval =
			tokio::time::interval(tokio::time::Duration::from_secs(self.polling_interval_secs));

		loop {
			tokio::select! {
				_ = interval.tick() => {
					// Get current block
					let current_block = match provider.get_block_number().await {
						Ok(block) => block,
						Err(e) => {
							tracing::error!("Failed to get block number for chain {}: {}", chain_id, e);
							continue;
						}
					};

					// Get last processed block
					let mut last_blocks = self.last_blocks.lock().await;
					let last_block = last_blocks.get(&chain_id).copied().unwrap_or(current_block);

					if current_block <= last_block {
						continue;
					}

					// Create filter for Open events
					let filter = Filter::new()
						.address(settler_address)
						.event_signature(IShinobiInputSettler::Open::SIGNATURE_HASH)
						.from_block(last_block + 1)
						.to_block(current_block);

					// Get logs
					let logs = match provider.get_logs(&filter).await {
						Ok(logs) => logs,
						Err(e) => {
							tracing::error!("Failed to get logs for chain {}: {}", chain_id, e);
							continue;
						}
					};

					tracing::debug!(
						"Found {} Shinobi Open events on chain {} (blocks {}-{})",
						logs.len(),
						chain_id,
						last_block + 1,
						current_block
					);

					// Process each log
					for log in logs {
						match self.process_open_event(&log, chain_id).await {
							Ok(intent) => {
								tracing::info!(
									"Discovered Shinobi intent {} on chain {}",
									intent.id,
									chain_id
								);
								if sender.send(intent).is_err() {
									tracing::error!("Failed to send intent: channel closed");
									return;
								}
							}
							Err(e) => {
								tracing::error!("Failed to process Open event on chain {}: {}", chain_id, e);
							}
						}
					}

					// Update last processed block
					last_blocks.insert(chain_id, current_block);
				}
				_ = stop_rx.recv() => {
					tracing::info!("Stopping Shinobi HTTP monitoring for chain {}", chain_id);
					return;
				}
			}
		}
	}

	/// Monitor events on a specific chain (WebSocket subscription mode).
	async fn monitor_chain_ws(
		&self,
		chain_id: u64,
		provider: RootProvider<PubSubFrontend>,
		settler_address: AlloyAddress,
		sender: mpsc::UnboundedSender<Intent>,
		mut stop_rx: broadcast::Receiver<()>,
	) {
		// Create filter for Open events
		let filter = Filter::new()
			.address(settler_address)
			.event_signature(IShinobiInputSettler::Open::SIGNATURE_HASH);

		// Subscribe to logs
		let sub = match provider.subscribe_logs(&filter).await {
			Ok(sub) => sub,
			Err(e) => {
				tracing::error!(
					"Failed to subscribe to logs for chain {}: {}",
					chain_id,
					e
				);
				return;
			}
		};

		let mut stream = sub.into_stream();

		tracing::info!("Started WebSocket monitoring for Shinobi on chain {}", chain_id);

		loop {
			tokio::select! {
				Some(log) = stream.next() => {
					match self.process_open_event(&log, chain_id).await {
						Ok(intent) => {
							tracing::info!(
								"Discovered Shinobi intent {} on chain {}",
								intent.id,
								chain_id
							);
							if sender.send(intent).is_err() {
								tracing::error!("Failed to send intent: channel closed");
								return;
							}
						}
						Err(e) => {
							tracing::error!("Failed to process Open event on chain {}: {}", chain_id, e);
						}
					}
				}
				_ = stop_rx.recv() => {
					tracing::info!("Stopping Shinobi WebSocket monitoring for chain {}", chain_id);
					return;
				}
			}
		}
	}
}

#[async_trait]
impl DiscoveryInterface for ShinobiDiscovery {
	fn config_schema(&self) -> Box<dyn ConfigSchema> {
		Box::new(ShinobiDiscoveryConfigSchema)
	}

	async fn start_monitoring(
		&self,
		sender: mpsc::UnboundedSender<Intent>,
	) -> Result<(), DiscoveryError> {
		if self
			.is_monitoring
			.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
			.is_err()
		{
			return Err(DiscoveryError::AlreadyMonitoring);
		}

		tracing::info!("Starting Shinobi intent discovery monitoring");

		// Create broadcast channel for stop signal
		let (stop_tx, _) = broadcast::channel(1);
		*self.stop_signal.lock().await = Some(stop_tx.clone());

		let mut handles = Vec::new();

		// Start monitoring task for each network
		for network_id in &self.network_ids {
			let settler_address = self
				.input_settlers
				.get(network_id)
				.ok_or_else(|| {
					DiscoveryError::ValidationError(format!(
						"No settler address for network {}",
						network_id
					))
				})?
				.clone();

			let provider = self.providers.get(network_id).ok_or_else(|| {
				DiscoveryError::Connection(format!("No provider for network {}", network_id))
			})?;

			let chain_id = *network_id;
			let sender_clone = sender.clone();
			let stop_rx = stop_tx.subscribe();

			let handle = match provider {
				ProviderType::Http(provider) => {
					let provider = provider.clone();
					let self_clone = Arc::new(self.clone());
					tokio::spawn(async move {
						self_clone
							.monitor_chain_http(
								chain_id,
								provider,
								settler_address,
								sender_clone,
								stop_rx,
							)
							.await;
					})
				}
				ProviderType::WebSocket(provider) => {
					let provider = provider.clone();
					let self_clone = Arc::new(self.clone());
					tokio::spawn(async move {
						self_clone
							.monitor_chain_ws(chain_id, provider, settler_address, sender_clone, stop_rx)
							.await;
					})
				}
			};

			handles.push(handle);
		}

		*self.monitoring_handles.lock().await = handles;

		tracing::info!(
			"Shinobi discovery monitoring started for {} networks",
			self.network_ids.len()
		);

		Ok(())
	}

	async fn stop_monitoring(&self) -> Result<(), DiscoveryError> {
		if !self.is_monitoring.swap(false, Ordering::SeqCst) {
			return Ok(());
		}

		tracing::info!("Stopping Shinobi intent discovery monitoring");

		// Send stop signal
		if let Some(stop_tx) = self.stop_signal.lock().await.take() {
			let _ = stop_tx.send(());
		}

		// Wait for all tasks to complete
		let handles = std::mem::take(&mut *self.monitoring_handles.lock().await);
		for handle in handles {
			let _ = handle.await;
		}

		tracing::info!("Shinobi discovery monitoring stopped");

		Ok(())
	}
}

// Implement Clone for ShinobiDiscovery (needed for spawning tasks)
impl Clone for ShinobiDiscovery {
	fn clone(&self) -> Self {
		Self {
			providers: HashMap::new(), // Providers are not cloneable, will be unused in clone
			network_ids: self.network_ids.clone(),
			networks: self.networks.clone(),
			input_settlers: self.input_settlers.clone(),
			last_blocks: Arc::clone(&self.last_blocks),
			is_monitoring: Arc::clone(&self.is_monitoring),
			monitoring_handles: Arc::clone(&self.monitoring_handles),
			stop_signal: Arc::clone(&self.stop_signal),
			polling_interval_secs: self.polling_interval_secs,
			lock_type: self.lock_type.clone(),
		}
	}
}

/// Configuration schema for Shinobi discovery.
struct ShinobiDiscoveryConfigSchema;

impl ConfigSchema for ShinobiDiscoveryConfigSchema {
	fn validate(&self, _config: &toml::Value) -> Result<(), solver_types::ValidationError> {
		// Basic validation is done in the factory function
		Ok(())
	}
}

/// Configuration structure for Shinobi discovery (from TOML).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ShinobiDiscoveryConfig {
	/// Map of chain_id -> InputSettler address (as hex string)
	pub input_settlers: HashMap<u64, String>,
	/// Lock type (typically "native_escrow")
	pub lock_type: String,
	/// Polling interval in seconds (0 = WebSocket)
	#[serde(default)]
	pub polling_interval_secs: Option<u64>,
}

/// Registry implementation for Shinobi discovery.
pub struct Registry;

impl crate::DiscoveryRegistry for Registry {}

impl solver_types::ImplementationRegistry for Registry {
	type Factory = crate::DiscoveryFactory;

	const NAME: &'static str = "shinobi";

	fn factory() -> Self::Factory {
		|config, networks| {
			let cfg: ShinobiDiscoveryConfig = config.clone().try_into().map_err(|e| {
				DiscoveryError::ValidationError(format!("Invalid Shinobi discovery config: {}", e))
			})?;

			// Parse addresses
			let mut input_settlers = HashMap::new();
			for (chain_id, addr_str) in cfg.input_settlers {
				let addr = addr_str
					.parse::<AlloyAddress>()
					.map_err(|e| {
						DiscoveryError::ValidationError(format!(
							"Invalid settler address for chain {}: {}",
							chain_id, e
						))
					})?;
				input_settlers.insert(chain_id, addr);
			}

			// Create discovery instance
			let discovery = tokio::runtime::Handle::current().block_on(async {
				ShinobiDiscovery::new(
					input_settlers,
					networks.clone(),
					cfg.polling_interval_secs,
					cfg.lock_type.clone(),
				)
				.await
			})?;

			Ok(Box::new(discovery))
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use alloy_primitives::{Address as AlloyAddress, Bytes, U256};
	use solver_types::standards::eip7683::MandateOutput;

	/// Helper to create a test ShinobiIntent
	fn create_test_intent() -> ShinobiIntent {
		ShinobiIntent {
			user: AlloyAddress::from([1u8; 20]),
			nonce: U256::from(123),
			origin_chain_id: 1,
			expires: 2000000000,
			fill_deadline: 1900000000,
			fill_oracle: AlloyAddress::from([2u8; 20]),
			inputs: vec![[U256::from(0), U256::from(1000000000000000000u64)]],
			outputs: vec![MandateOutput {
				oracle: [3u8; 32],
				settler: [4u8; 32],
				chain_id: U256::from(42161),
				token: [0u8; 32], // Native ETH
				amount: U256::from(900000000000000000u64),
				recipient: [5u8; 32],
				call: Vec::new(),
				context: Vec::new(),
			}],
			intent_oracle: AlloyAddress::from([6u8; 20]),
			refund_calldata: Bytes::new(),
		}
	}

	#[test]
	fn test_intent_conversion() {
		let intent = create_test_intent();
		let order_id = intent.order_identifier();

		// Convert to Solidity format
		let sol_intent: ShinobiIntentSol = intent.clone().into();

		// Create mock discovery config
		let discovery = ShinobiDiscovery {
			providers: HashMap::new(),
			network_ids: vec![1],
			networks: NetworksConfig::default(),
			input_settlers: HashMap::new(),
			last_blocks: Arc::new(Mutex::new(HashMap::new())),
			is_monitoring: Arc::new(AtomicBool::new(false)),
			monitoring_handles: Arc::new(Mutex::new(Vec::new())),
			stop_signal: Arc::new(Mutex::new(None)),
			polling_interval_secs: 3,
			lock_type: "native_escrow".to_string(),
		};

		// Test conversion to Intent
		let result = discovery.convert_to_intent(order_id, sol_intent, 1000);

		assert!(result.is_ok());
		let solver_intent = result.unwrap();

		// Verify Intent fields
		assert_eq!(solver_intent.id, with_0x_prefix(&hex::encode(order_id)));
		assert_eq!(solver_intent.source, "on-chain");
		assert_eq!(solver_intent.standard, "shinobi");
		assert_eq!(solver_intent.lock_type, "native_escrow");
		assert_eq!(solver_intent.metadata.requires_auction, false);
		assert_eq!(solver_intent.metadata.discovered_at, 1000);

		// Verify order_bytes can be decoded back
		let decoded: ShinobiIntentSol =
			ShinobiIntentSol::abi_decode(&solver_intent.order_bytes, true).unwrap();
		assert_eq!(decoded.user, intent.user);
		assert_eq!(decoded.nonce, intent.nonce);
	}

	#[test]
	fn test_native_eth_detection() {
		let intent = create_test_intent();

		// Verify this is a native ETH intent
		assert!(intent.is_native_eth_output());
		assert_eq!(intent.output_amount(), Some(U256::from(900000000000000000u64)));
	}

	#[test]
	fn test_withdrawal_vs_deposit() {
		let mut withdrawal_intent = create_test_intent();
		withdrawal_intent.origin_chain_id = 1; // Ethereum

		let mut deposit_intent = create_test_intent();
		deposit_intent.origin_chain_id = 42161; // Arbitrum

		assert!(withdrawal_intent.is_withdrawal());
		assert!(!withdrawal_intent.is_deposit());

		assert!(!deposit_intent.is_withdrawal());
		assert!(deposit_intent.is_deposit());
	}

	#[test]
	fn test_config_structure() {
		// Test that the config struct has the right fields
		let mut input_settlers = HashMap::new();
		input_settlers.insert(1, "0x1111111111111111111111111111111111111111".to_string());
		input_settlers.insert(42161, "0x2222222222222222222222222222222222222222".to_string());

		let config = ShinobiDiscoveryConfig {
			lock_type: "native_escrow".to_string(),
			polling_interval_secs: Some(5),
			input_settlers,
		};

		assert_eq!(config.lock_type, "native_escrow");
		assert_eq!(config.polling_interval_secs, Some(5));
		assert_eq!(config.input_settlers.len(), 2);
		assert_eq!(
			config.input_settlers.get(&1).unwrap(),
			"0x1111111111111111111111111111111111111111"
		);
		assert_eq!(
			config.input_settlers.get(&42161).unwrap(),
			"0x2222222222222222222222222222222222222222"
		);
	}

	#[test]
	fn test_order_id_deterministic() {
		let intent1 = create_test_intent();
		let intent2 = create_test_intent();

		let id1 = intent1.order_identifier();
		let id2 = intent2.order_identifier();

		// Same intent should produce same order ID
		assert_eq!(id1, id2);

		// Different nonce should produce different order ID
		let mut intent3 = create_test_intent();
		intent3.nonce = U256::from(456);
		let id3 = intent3.order_identifier();

		assert_ne!(id1, id3);
	}

	#[test]
	fn test_config_schema_validation() {
		let schema = ShinobiDiscoveryConfigSchema;

		// Empty config should pass (validation done in factory)
		let empty_config = toml::Value::Table(Default::default());
		assert!(schema.validate(&empty_config).is_ok());
	}
}

