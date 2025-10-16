//! Shinobi Intent type definition.
//!
//! This module defines the `ShinobiIntent` struct, which extends the OIF StandardOrder
//! with bidirectional oracles and custom refund logic for Shinobi Cash's privacy-preserving
//! cross-chain operations.

use crate::standards::eip7683::MandateOutput;
use crate::{AvailableInput, InteropAddress, OrderParsable, RequestedOutput};
use alloy_primitives::{keccak256, Address, Bytes, U256};
use alloy_sol_types::SolValue;
use serde::{Deserialize, Serialize};

/// Shinobi Intent - extended StandardOrder with bidirectional oracles.
///
/// This structure supports both cross-chain withdrawals and deposits for Shinobi Cash,
/// a privacy-preserving protocol. It extends the OIF StandardOrder with:
/// - `intent_oracle`: Validates intent creation (origin → destination)
/// - `refund_calldata`: Enables custom refund logic (e.g., return to privacy pool)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShinobiIntent {
	// === Base StandardOrder Fields ===
	/// Intent creator (verified via msg.sender on origin chain)
	pub user: Address,

	/// User nonce for uniqueness
	pub nonce: U256,

	/// Chain where intent was created
	pub origin_chain_id: u64,

	/// Expiry timestamp for refunds
	pub expires: u32,

	/// Deadline for filling the intent
	pub fill_deadline: u32,

	/// Oracle for fill proof validation (destination → origin)
	/// Proves that outputs were filled on destination chain
	pub fill_oracle: Address,

	/// Input tokens to be escrowed [tokenId, amount][]
	pub inputs: Vec<[U256; 2]>,

	/// Outputs to be filled on destination chain
	pub outputs: Vec<MandateOutput>,

	// === Shinobi Extensions ===
	/// Oracle for intent proof validation (origin → destination)
	/// Proves that intent was created by verified user on origin chain
	/// Critical for deposits to prevent depositor address spoofing
	pub intent_oracle: Address,

	/// Custom refund calldata for protocol-specific refund logic
	/// Empty (0x) = simple ETH transfer to intent.user
	/// Present = execute custom refund (e.g., return to privacy pool as commitment)
	pub refund_calldata: Bytes,
}

impl ShinobiIntent {
	/// Generate canonical order identifier from ShinobiIntent.
	///
	/// This MUST match the Solidity implementation in ShinobiIntentLib.sol exactly:
	/// ```solidity
	/// keccak256(abi.encode(
	///     intent.user,
	///     intent.nonce,
	///     intent.originChainId,
	///     intent.expires,
	///     intent.fillDeadline,
	///     intent.fillOracle,
	///     keccak256(abi.encodePacked(intent.inputs)),
	///     keccak256(abi.encode(intent.outputs)),
	///     intent.intentOracle,
	///     keccak256(intent.refundCalldata)
	/// ))
	/// ```
	///
	/// # Returns
	/// The unique 32-byte order identifier
	pub fn order_identifier(&self) -> [u8; 32] {
		use super::settler::MandateOutput as SolMandateOutput;

		// Hash inputs using abi.encodePacked (concatenation)
		let inputs_bytes: Vec<u8> = self
			.inputs
			.iter()
			.flat_map(|input| {
				let mut bytes = Vec::new();
				bytes.extend_from_slice(&input[0].to_be_bytes::<32>());
				bytes.extend_from_slice(&input[1].to_be_bytes::<32>());
				bytes
			})
			.collect();
		let inputs_hash = keccak256(&inputs_bytes);

		// Convert outputs to Sol version for ABI encoding
		let sol_outputs: Vec<SolMandateOutput> = self
			.outputs
			.iter()
			.cloned()
			.map(|o| o.into())
			.collect();

		// Hash outputs using abi.encode
		let outputs_hash = keccak256(&sol_outputs.abi_encode());

		// Hash refund calldata
		let refund_hash = keccak256(&self.refund_calldata);

		// Encode all fields using abi.encode and hash
		let encoded = (
			self.user,
			self.nonce,
			U256::from(self.origin_chain_id),
			self.expires,
			self.fill_deadline,
			self.fill_oracle,
			inputs_hash,
			outputs_hash,
			self.intent_oracle,
			refund_hash,
		)
			.abi_encode();

		keccak256(&encoded).into()
	}

	/// Validate intent structure.
	///
	/// Checks:
	/// - User address is not zero
	/// - At least one input exists
	/// - At least one output exists
	/// - Intent oracle is not zero (except for crosschain intents where it can be zero)
	/// - Fill oracle is not zero
	/// - Refund calldata can be empty (valid for simple refunds)
	///
	/// Note: Time-based validation (fill_deadline, expires) should be done
	/// separately when current block timestamp is available.
	///
	/// # Returns
	/// `Ok(())` if valid, `Err(String)` with error message otherwise
	pub fn validate(&self) -> Result<(), String> {
		if self.user == Address::ZERO {
			return Err("Invalid user: zero address".into());
		}
		if self.inputs.is_empty() {
			return Err("Invalid inputs: must have at least one input".into());
		}
		if self.outputs.is_empty() {
			return Err("Invalid outputs: must have at least one output".into());
		}

		// Intent oracle can be zero for crosschain withdrawals (optimistic settlement).
		// For crosschain deposits, intent_oracle != 0 and must be validated by the solver.
		// The OutputSettler contract handles this distinction based on intent_oracle value.

		if self.fill_oracle == Address::ZERO {
			return Err("Invalid fill oracle: zero address".into());
		}

		// Note: refundCalldata can be empty (for simple ETH refunds)

		Ok(())
	}

	/// Check if this is a withdrawal intent (origin is Ethereum).
	///
	/// Withdrawals: Ethereum → User Chain (direct settlement)
	/// Deposits: User Chain → Ethereum (hyperlane settlement)
	///
	/// # Returns
	/// `true` if origin_chain_id is 1 (Ethereum mainnet)
	pub fn is_withdrawal(&self) -> bool {
		self.origin_chain_id == 1
	}

	/// Check if this is a deposit intent (origin is a user chain).
	///
	/// # Returns
	/// `true` if origin_chain_id is not 1 (not Ethereum mainnet)
	pub fn is_deposit(&self) -> bool {
		!self.is_withdrawal()
	}

	/// Get the destination chain ID from the first output.
	///
	/// Shinobi intents typically have a single output on the destination chain.
	///
	/// # Returns
	/// The destination chain ID, or `None` if no outputs exist
	pub fn destination_chain_id(&self) -> Option<u64> {
		self.outputs.first().map(|output| {
			// MandateOutput.chain_id is U256, convert to u64
			output.chain_id.to::<u64>()
		})
	}

	/// Check if the output token is native ETH.
	///
	/// Native ETH is represented as bytes32(0).
	///
	/// # Returns
	/// `true` if the first output token is bytes32(0)
	pub fn is_native_eth_output(&self) -> bool {
		self.outputs
			.first()
			.map(|output| output.token == [0u8; 32])
			.unwrap_or(false)
	}

	/// Get the output amount.
	///
	/// # Returns
	/// The amount from the first output, or `None` if no outputs exist
	pub fn output_amount(&self) -> Option<U256> {
		self.outputs.first().map(|output| output.amount)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_order_identifier_consistency() {
		let intent = ShinobiIntent {
			user: Address::from([1u8; 20]),
			nonce: U256::from(123),
			origin_chain_id: 1,
			expires: 2000000000,
			fill_deadline: 1900000000,
			fill_oracle: Address::from([2u8; 20]),
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
			intent_oracle: Address::from([6u8; 20]),
			refund_calldata: Bytes::new(),
		};

		let order_id = intent.order_identifier();
		// Order ID should be deterministic
		let order_id_2 = intent.order_identifier();
		assert_eq!(order_id, order_id_2);

		// Should be 32 bytes
		assert_eq!(order_id.len(), 32);
	}

	#[test]
	fn test_validation() {
		let valid_intent = ShinobiIntent {
			user: Address::from([1u8; 20]),
			nonce: U256::from(1),
			origin_chain_id: 1,
			expires: 2000000000,
			fill_deadline: 1900000000,
			fill_oracle: Address::from([2u8; 20]),
			inputs: vec![[U256::from(0), U256::from(1000)]],
			outputs: vec![MandateOutput {
				oracle: [0u8; 32],
				settler: [0u8; 32],
				chain_id: U256::from(42161),
				token: [0u8; 32],
				amount: U256::from(900),
				recipient: [0u8; 32],
				call: Vec::new(),
				context: Vec::new(),
			}],
			intent_oracle: Address::from([3u8; 20]),
			refund_calldata: Bytes::new(),
		};

		assert!(valid_intent.validate().is_ok());

		// Test invalid user
		let mut invalid = valid_intent.clone();
		invalid.user = Address::ZERO;
		assert!(invalid.validate().is_err());

		// Test no inputs
		let mut invalid = valid_intent.clone();
		invalid.inputs = vec![];
		assert!(invalid.validate().is_err());

		// Test no outputs
		let mut invalid = valid_intent.clone();
		invalid.outputs = vec![];
		assert!(invalid.validate().is_err());

		// Test invalid intent oracle
		let mut invalid = valid_intent.clone();
		invalid.intent_oracle = Address::ZERO;
		assert!(invalid.validate().is_err());

		// Test invalid fill oracle
		let mut invalid = valid_intent.clone();
		invalid.fill_oracle = Address::ZERO;
		assert!(invalid.validate().is_err());
	}

	#[test]
	fn test_withdrawal_deposit_detection() {
		let withdrawal = ShinobiIntent {
			user: Address::from([1u8; 20]),
			nonce: U256::from(1),
			origin_chain_id: 1, // Ethereum
			expires: 2000000000,
			fill_deadline: 1900000000,
			fill_oracle: Address::from([2u8; 20]),
			inputs: vec![[U256::from(0), U256::from(1000)]],
			outputs: vec![MandateOutput {
				oracle: [0u8; 32],
				settler: [0u8; 32],
				chain_id: U256::from(42161),
				token: [0u8; 32],
				amount: U256::from(900),
				recipient: [0u8; 32],
				call: Vec::new(),
				context: Vec::new(),
			}],
			intent_oracle: Address::from([3u8; 20]),
			refund_calldata: Bytes::new(),
		};

		assert!(withdrawal.is_withdrawal());
		assert!(!withdrawal.is_deposit());

		let deposit = ShinobiIntent {
			origin_chain_id: 42161, // Arbitrum
			..withdrawal.clone()
		};

		assert!(!deposit.is_withdrawal());
		assert!(deposit.is_deposit());
	}

	#[test]
	fn test_native_eth_detection() {
		let eth_intent = ShinobiIntent {
			user: Address::from([1u8; 20]),
			nonce: U256::from(1),
			origin_chain_id: 1,
			expires: 2000000000,
			fill_deadline: 1900000000,
			fill_oracle: Address::from([2u8; 20]),
			inputs: vec![[U256::from(0), U256::from(1000)]],
			outputs: vec![MandateOutput {
				oracle: [0u8; 32],
				settler: [0u8; 32],
				chain_id: U256::from(42161),
				token: [0u8; 32], // Native ETH
				amount: U256::from(900),
				recipient: [0u8; 32],
				call: Vec::new(),
				context: Vec::new(),
			}],
			intent_oracle: Address::from([3u8; 20]),
			refund_calldata: Bytes::new(),
		};

		assert!(eth_intent.is_native_eth_output());

		let erc20_intent = ShinobiIntent {
			outputs: vec![MandateOutput {
				token: [0xAAu8; 32], // ERC20 token
				..eth_intent.outputs[0].clone()
			}],
			..eth_intent.clone()
		};

		assert!(!erc20_intent.is_native_eth_output());
	}
}

// Implement OrderParsable trait for ShinobiIntent
impl OrderParsable for ShinobiIntent {
	fn parse_available_inputs(&self) -> Vec<AvailableInput> {
		use crate::{bytes32_to_address, parse_address, Address as SolverAddress};

		self.inputs
			.iter()
			.map(|input| {
				let token_id = input[0];
				let amount = input[1];

				// For Shinobi, token_id in inputs array represents the token address
				// token_id == 0 means native ETH
				let token_bytes = token_id.to_be_bytes::<32>();
				let token_address_hex = bytes32_to_address(&token_bytes);
				let token_addr =
					parse_address(&token_address_hex).unwrap_or(SolverAddress(vec![0u8; 20]));

				// Create interop addresses
				let asset = InteropAddress::from((self.origin_chain_id, token_addr));
				let user = InteropAddress::from((self.origin_chain_id, SolverAddress(self.user.0.to_vec())));

				AvailableInput {
					user,
					asset,
					amount,
					lock: None, // Shinobi uses escrow, not explicit locks
				}
			})
			.collect()
	}

	fn parse_requested_outputs(&self) -> Vec<RequestedOutput> {
		use crate::{bytes32_to_address, parse_address, Address as SolverAddress};

		self.outputs
			.iter()
			.map(|output| {
				let chain_id: u64 = output.chain_id.try_into().unwrap_or(0);

				// Convert bytes32 token address to Address
				let token_address_hex = bytes32_to_address(&output.token);
				let token_addr =
					parse_address(&token_address_hex).unwrap_or(SolverAddress(vec![0u8; 20]));

				// Convert bytes32 recipient address to Address
				let recipient_address_hex = bytes32_to_address(&output.recipient);
				let recipient_addr =
					parse_address(&recipient_address_hex).unwrap_or(SolverAddress(vec![0u8; 20]));

				// Create interop addresses
				let asset = InteropAddress::from((chain_id, token_addr));
				let receiver = InteropAddress::from((chain_id, recipient_addr));

				RequestedOutput {
					receiver,
					asset,
					amount: output.amount,
					calldata: if output.call.is_empty() {
						None
					} else {
						Some(hex::encode(&output.call))
					},
				}
			})
			.collect()
	}

	fn parse_lock_type(&self) -> Option<String> {
		// Shinobi always uses native escrow for cross-chain operations
		Some("native_escrow".to_string())
	}

	fn input_oracle(&self) -> String {
		// Return the fill oracle address (validates fills on destination chain)
		format!("0x{}", hex::encode(self.fill_oracle.as_slice()))
	}

	fn origin_chain_id(&self) -> u64 {
		self.origin_chain_id
	}

	fn destination_chain_ids(&self) -> Vec<u64> {
		self.outputs
			.iter()
			.map(|output| output.chain_id.try_into().unwrap_or(0u64))
			.collect()
	}
}
