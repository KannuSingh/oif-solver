//! Shinobi settler contract interface bindings.
//!
//! This module provides Rust bindings for the Shinobi InputSettler and OutputSettler
//! contracts using alloy-sol-types for ABI encoding/decoding.

use alloy_sol_types::sol;

sol! {
	/// Solidity representation of ShinobiIntent for ABI encoding
	#[derive(Debug, PartialEq, Eq)]
	struct ShinobiIntentSol {
		address user;
		uint256 nonce;
		uint256 originChainId;
		uint32 expires;
		uint32 fillDeadline;
		address fillOracle;
		uint256[2][] inputs;
		MandateOutput[] outputs;
		address intentOracle;
		bytes refundCalldata;
	}

	/// Mandate output structure (from OIF)
	#[derive(Debug, PartialEq, Eq)]
	struct MandateOutput {
		bytes32 oracle;
		bytes32 settler;
		uint256 chainId;
		bytes32 token;
		uint256 amount;
		bytes32 recipient;
		bytes call;
		bytes context;
	}

	/// Interface for Shinobi Input Settler (origin chain - escrow side)
	#[sol(rpc)]
	interface IShinobiInputSettler {
		/// Emitted when an intent is opened and funds escrowed
		event Open(bytes32 indexed orderId, ShinobiIntentSol intent);

		/// Emitted when an intent is finalized and funds released to solver
		event Finalised(bytes32 indexed orderId, bytes32 solver, bytes32 destination);

		/// Emitted when an intent is refunded
		event Refunded(bytes32 indexed orderId);

		/// Open an intent and escrow funds
		function open(ShinobiIntentSol calldata intent) external payable;

		/// Finalize an intent by validating fill proofs and releasing funds to solver
		function finalise(ShinobiIntentSol calldata intent, bytes[] calldata fillProofs) external;

		/// Refund an expired intent
		function refund(ShinobiIntentSol calldata intent) external;

		/// Generate order identifier for an intent
		function orderIdentifier(ShinobiIntentSol memory intent) external view returns (bytes32);
	}

	/// Interface for Shinobi Output Settler (destination chain - fill side)
	#[sol(rpc)]
	interface IShinobiOutputSettler {
		/// Emitted when an output is successfully filled
		event OutputFilled(
			bytes32 indexed orderId,
			bytes32 solver,
			uint32 timestamp,
			MandateOutput output,
			uint256 finalAmount
		);

		/// Fill an intent on destination chain
		function fill(ShinobiIntentSol calldata intent) external payable;

		/// Get the fill record for a specific intent
		function getFillRecord(bytes32 orderId, bytes32 outputHash) external view returns (bytes32 payloadHash);
	}
}

use super::ShinobiIntent;
use crate::standards::eip7683::MandateOutput as RustMandateOutput;
use alloy_primitives::U256;

/// Convert Rust ShinobiIntent to Solidity ShinobiIntentSol for ABI encoding
impl From<ShinobiIntent> for ShinobiIntentSol {
	fn from(intent: ShinobiIntent) -> Self {
		ShinobiIntentSol {
			user: intent.user,
			nonce: intent.nonce,
			originChainId: U256::from(intent.origin_chain_id),
			expires: intent.expires,
			fillDeadline: intent.fill_deadline,
			fillOracle: intent.fill_oracle,
			inputs: intent.inputs,
			outputs: intent.outputs.into_iter().map(|o| o.into()).collect(),
			intentOracle: intent.intent_oracle,
			refundCalldata: intent.refund_calldata,
		}
	}
}

/// Convert Solidity ShinobiIntentSol to Rust ShinobiIntent
impl From<ShinobiIntentSol> for ShinobiIntent {
	fn from(sol: ShinobiIntentSol) -> Self {
		ShinobiIntent {
			user: sol.user,
			nonce: sol.nonce,
			origin_chain_id: sol.originChainId.to::<u64>(),
			expires: sol.expires,
			fill_deadline: sol.fillDeadline,
			fill_oracle: sol.fillOracle,
			inputs: sol.inputs,
			outputs: sol.outputs.into_iter().map(|o| o.into()).collect(),
			intent_oracle: sol.intentOracle,
			refund_calldata: sol.refundCalldata,
		}
	}
}

/// Convert Rust MandateOutput to Solidity MandateOutput
impl From<RustMandateOutput> for MandateOutput {
	fn from(output: RustMandateOutput) -> Self {
		MandateOutput {
			oracle: output.oracle.into(),
			settler: output.settler.into(),
			chainId: output.chain_id,
			token: output.token.into(),
			amount: output.amount,
			recipient: output.recipient.into(),
			call: output.call.into(),
			context: output.context.into(),
		}
	}
}

/// Convert Solidity MandateOutput to Rust MandateOutput
impl From<MandateOutput> for RustMandateOutput {
	fn from(sol: MandateOutput) -> Self {
		RustMandateOutput {
			oracle: *sol.oracle,
			settler: *sol.settler,
			chain_id: sol.chainId,
			token: *sol.token,
			amount: sol.amount,
			recipient: *sol.recipient,
			call: sol.call.to_vec(),
			context: sol.context.to_vec(),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use alloy_primitives::{Address, Bytes};

	#[test]
	fn test_intent_conversion() {
		let rust_intent = ShinobiIntent {
			user: Address::from([1u8; 20]),
			nonce: U256::from(123),
			origin_chain_id: 1,
			expires: 2000000000,
			fill_deadline: 1900000000,
			fill_oracle: Address::from([2u8; 20]),
			inputs: vec![[U256::from(0), U256::from(1000000000000000000u64)]],
			outputs: vec![RustMandateOutput {
				oracle: [3u8; 32],
				settler: [4u8; 32],
				chain_id: U256::from(42161),
				token: [0u8; 32],
				amount: U256::from(900000000000000000u64),
				recipient: [5u8; 32],
				call: Vec::new(),
				context: Vec::new(),
			}],
			intent_oracle: Address::from([6u8; 20]),
			refund_calldata: Bytes::new(),
		};

		// Convert to Solidity
		let sol_intent: ShinobiIntentSol = rust_intent.clone().into();

		// Convert back to Rust
		let back_to_rust: ShinobiIntent = sol_intent.into();

		// Should be identical
		assert_eq!(rust_intent, back_to_rust);
	}

	#[test]
	fn test_mandate_output_conversion() {
		let rust_output = RustMandateOutput {
			oracle: [1u8; 32],
			settler: [2u8; 32],
			chain_id: U256::from(42161),
			token: [0u8; 32],
			amount: U256::from(1000),
			recipient: [3u8; 32],
			call: vec![0x12, 0x34],
			context: Vec::new(),
		};

		let sol_output: MandateOutput = rust_output.clone().into();
		let back_to_rust: RustMandateOutput = sol_output.into();

		assert_eq!(rust_output.oracle, back_to_rust.oracle);
		assert_eq!(rust_output.settler, back_to_rust.settler);
		assert_eq!(rust_output.chain_id, back_to_rust.chain_id);
		assert_eq!(rust_output.token, back_to_rust.token);
		assert_eq!(rust_output.amount, back_to_rust.amount);
		assert_eq!(rust_output.recipient, back_to_rust.recipient);
		assert_eq!(rust_output.call, back_to_rust.call);
	}
}
