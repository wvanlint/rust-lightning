//! Defines anchor channel reserve requirements.
//!
//! The Lightning protocol advances the state of the channel based on commitment and HTLC
//! transactions, which allow each participant to unilaterally close the channel with the correct
//! state and resolve pending HTLCs on-chain. Originally, these transactions are signed by both
//! counterparties over the entire transaction and therefore contain a fixed fee, which can be
//! updated with the `update_fee` message by the funder. However, these fees can lead to
//! disagreements and can diverge from the prevailing fee rate if a party is disconnected.
//!
//! To address these issues, fees are provided exogenously for anchor output channels.
//! Anchor outputs are negotiated on channel opening to add outputs to each commitment transaction.
//! These outputs can be spent in a child transaction with additional fees to incentivize the
//! mining of the parent transaction, this technique is called Child Pays For Parent (CPFP).
//! Similarly, HTLC transactions will be signed with `SIGHASH_SINGLE|SIGHASH_ANYONECANPAY` so
//! additional inputs and outputs can be added to pay for fees.
//!
//! UTXO reserves will therefore be required to supply commitment transactions and HTLC
//! transactions with fees to be confirmed in a timely manner. If HTLCs are not resolved
//! appropriately, it can lead to loss of funds of the in-flight HLTCs as mentioned above. Only
//! partially satisfying UTXO requirements incurs the risk of not being able to resolve a subset of
//! HTLCs.
use crate::chain::chaininterface::BroadcasterInterface;
use crate::chain::chaininterface::FeeEstimator;
use crate::chain::chainmonitor::ChainMonitor;
use crate::chain::chainmonitor::Persist;
use crate::chain::Filter;
use crate::events::bump_transaction::Utxo;
use crate::ln::channelmanager::AChannelManager;
use crate::prelude::new_hash_set;
use crate::sign::ecdsa::EcdsaChannelSigner;
use crate::util::logger::Logger;
use bitcoin::constants::WITNESS_SCALE_FACTOR;
use bitcoin::Amount;
use bitcoin::FeeRate;
use bitcoin::Weight;
use core::ops::Deref;

// Transaction weights based on:
// https://github.com/lightning/bolts/blob/master/03-transactions.md#appendix-a-expected-weights
const COMMITMENT_TRANSACTION_BASE_WEIGHT: u64 = 900 + 224;
const COMMITMENT_TRANSACTION_PER_HTLC_WEIGHT: u64 = 172;
const PER_HTLC_TIMEOUT_WEIGHT: u64 = 666;
const PER_HTLC_SUCCESS_WEIGHT: u64 = 706;

// The transaction at least contains:
// - 4 bytes for the version
// - 4 bytes for the locktime
// - 1 byte for the number of inputs
// - 1 byte for the number of outputs
// - 2 bytes for the witness header
//   - 1 byte for the flag
//   - 1 byte for the marker
const TRANSACTION_BASE_WEIGHT: u64 = (4 + 4 + 1 + 1) * WITNESS_SCALE_FACTOR as u64 + 2;

// A P2WPKH input consists of:
// - 36 bytes for the previous outpoint:
//   - 32 bytes transaction hash
//   - 4 bytes index
// - 4 bytes for the sequence
// - 1 byte for the script sig length
// - the witness:
//   - 1 byte for witness items count
//   - 1 byte for the signature length
//   - 72 bytes for the signature
//   - 1 byte for the public key length
//   - 33 bytes for the public key
const P2WPKH_INPUT_WEIGHT: u64 = (36 + 4 + 1) * WITNESS_SCALE_FACTOR as u64 + (1 + 1 + 72 + 1 + 33);

// A P2WPKH output consists of:
// - 8 bytes for the output amount
// - 1 byte for the script length
// - 22 bytes for the script (OP_0 OP_PUSH20 20 byte public key hash)
const P2WPKH_OUTPUT_WEIGHT: u64 = (8 + 1 + 22) * WITNESS_SCALE_FACTOR as u64;

// A P2TR input consists of:
// - 36 bytes for the previous outpoint:
//   - 32 bytes transaction hash
//   - 4 bytes index
// - 4 bytes for the sequence
// - 1 byte for the script sig length
// - the witness:
//   - 1 byte for witness items count
//   - 1 byte for the signature length
//   - 64 bytes for the Schnorr signature
const P2TR_INPUT_WEIGHT: u64 = (36 + 4 + 1) * WITNESS_SCALE_FACTOR as u64 + (1 + 1 + 64);
// A P2TR output consists of:
// - 8 bytes for the output amount
// - 1 byte for the script length
// - 34 bytes for the script (OP_1 OP_PUSH32 32 byte Schnorr public key)
const P2TR_OUTPUT_WEIGHT: u64 = (8 + 1 + 34) * WITNESS_SCALE_FACTOR as u64;

// An P2WSH anchor input consists of:
// - 36 bytes for the previous outpoint:
//   - 32 bytes transaction hash
//   - 4 bytes index
// - 4 bytes for the sequence
// - 1 byte for the script sig length
// - the witness:
//   - 1 byte for witness item count
//   - 1 byte for signature length
//   - 72 bytes signature
//   - 1 byte for script length
//   - 40 byte script
//     <pubkey> OP_CHECKSIG OP_IFDUP OP_NOTIF OP_16 OP_CHECKSEQUENCEVERIFY OP_ENDIF
//     - 33 byte pubkey with 1 byte OP_PUSHBYTES_33.
//     - 6 1-byte opcodes
const ANCHOR_INPUT_WEIGHT: u64 = (36 + 4 + 1) * WITNESS_SCALE_FACTOR as u64 + (1 + 1 + 72 + 1 + 40);

fn htlc_success_transaction_weight(context: &AnchorChannelReserveContext) -> u64 {
	PER_HTLC_SUCCESS_WEIGHT
		+ if context.taproot_wallet {
			P2TR_INPUT_WEIGHT + P2TR_OUTPUT_WEIGHT
		} else {
			P2WPKH_INPUT_WEIGHT + P2WPKH_OUTPUT_WEIGHT
		}
}

fn htlc_timeout_transaction_weight(context: &AnchorChannelReserveContext) -> u64 {
	PER_HTLC_TIMEOUT_WEIGHT
		+ if context.taproot_wallet {
			P2TR_INPUT_WEIGHT + P2TR_OUTPUT_WEIGHT
		} else {
			P2WPKH_INPUT_WEIGHT + P2WPKH_OUTPUT_WEIGHT
		}
}

fn anchor_output_spend_transaction_weight(context: &AnchorChannelReserveContext) -> u64 {
	TRANSACTION_BASE_WEIGHT
		+ ANCHOR_INPUT_WEIGHT
		+ if context.taproot_wallet {
			P2TR_INPUT_WEIGHT + P2TR_OUTPUT_WEIGHT
		} else {
			P2WPKH_INPUT_WEIGHT + P2WPKH_OUTPUT_WEIGHT
		}
}

/// Parameters defining the context around the anchor channel reserve requirement calculation.
pub struct AnchorChannelReserveContext {
	/// An upper bound fee rate estimate used to calculate the anchor channel reserve that is
	/// sufficient to provide fees for all required transactions.
	pub upper_bound_fee_rate: FeeRate,
	/// The expected number of accepted in-flight HTLCs per channel.
	///
	/// See [ChannelHandshakeConfig::our_max_accepted_htlcs] to restrict accepted in-flight HTLCs.
	///
	/// [ChannelHandshakeConfig::our_max_accepted_htlcs]: crate::util::config::ChannelHandshakeConfig::our_max_accepted_htlcs
	pub expected_accepted_htlcs: u16,
	/// Whether the wallet providing the anchor channel reserve uses Taproot P2TR outputs for its
	/// funds, or Segwit P2WPKH outputs otherwise.
	pub taproot_wallet: bool,
}

/// A default for the [AnchorChannelReserveContext] parameters is provided as follows:
/// - The upper bound fee rate is set to the 99th percentile of the median block fee rate since 2019:
///   ~50 sats/vbyte.
/// - The number of accepted in-flight HTLCs per channel is set to 10, providing additional margin
///   above the number seen for a large routing node over a month (average <1, maximum 10
///   accepted in-flight HTLCS aggregated across all channels).
/// - The wallet is assumed to be a Segwit wallet.
impl Default for AnchorChannelReserveContext {
	fn default() -> Self {
		AnchorChannelReserveContext {
			upper_bound_fee_rate: FeeRate::from_sat_per_kwu(50 * 250),
			expected_accepted_htlcs: 10,
			taproot_wallet: false,
		}
	}
}

/// Returns the amount that needs to be maintained as a reserve per anchor channel.
///
/// This reserve currently needs to be allocated as a disjoint set of UTXOs per channel,
/// as claims are not yet aggregated across channels.
pub fn get_reserve_per_channel(context: &AnchorChannelReserveContext) -> Amount {
	let weight = Weight::from_wu(
		COMMITMENT_TRANSACTION_BASE_WEIGHT +
		// Reserves are calculated assuming each accepted HTLC is forwarded as the upper bound.
		// - Inbound payments would require less reserves, but confirmations are still required when
		// making the preimage public through the mempool.
		// - Outbound payments don't require reserves to avoid loss of funds.
		2 * (context.expected_accepted_htlcs as u64) * COMMITMENT_TRANSACTION_PER_HTLC_WEIGHT +
		anchor_output_spend_transaction_weight(context) +
		// To calculate an upper bound on required reserves, it is assumed that each HTLC is resolved in a
		// separate transaction. However, they might be aggregated when possible depending on timelocks and
		// expiries.
		htlc_success_transaction_weight(context) * (context.expected_accepted_htlcs as u64) +
		htlc_timeout_transaction_weight(context) * (context.expected_accepted_htlcs as u64),
	);
	context.upper_bound_fee_rate.fee_wu(weight).unwrap_or(Amount::MAX)
}

/// Calculates the number of anchor channels that can be supported by the reserve provided
/// by `utxos`.
pub fn get_supportable_anchor_channels(
	context: &AnchorChannelReserveContext, utxos: &[Utxo],
) -> u64 {
	let reserve_per_channel = get_reserve_per_channel(context);
	let mut total_fractional_amount = Amount::from_sat(0);
	let mut num_whole_utxos = 0;
	for utxo in utxos {
		if utxo.output.value >= reserve_per_channel {
			num_whole_utxos += 1;
		} else {
			total_fractional_amount =
				total_fractional_amount.checked_add(utxo.output.value).unwrap_or(Amount::MAX);
			let satisfaction_fee = context
				.upper_bound_fee_rate
				.fee_wu(Weight::from_wu(utxo.satisfaction_weight))
				.unwrap_or(Amount::MAX);
			total_fractional_amount =
				total_fractional_amount.checked_sub(satisfaction_fee).unwrap_or(Amount::MIN);
		}
	}
	// We require disjoint sets of UTXOs for the reserve of each channel,
	// as claims are only aggregated per channel currently.
	//
	// UTXOs larger than the required reserve are a singleton disjoint set.
	// A disjoint set of fractional UTXOs could overcontribute by any amount less than the
	// required reserve, approaching double the reserve.
	//
	// Note that for the fractional UTXOs, this is an approximation as we can't efficiently calculate
	// a worst-case coin selection as an NP-complete problem.
	num_whole_utxos + total_fractional_amount.to_sat() / reserve_per_channel.to_sat() / 2
}

/// Verifies whether the anchor channel reserve provided by `utxos` is sufficient to support
/// an additional anchor channel.
///
/// This should be verified:
/// - Before opening a new outbound anchor channel with [ChannelManager::create_channel].
/// - Before accepting a new inbound anchor channel while handling [Event::OpenChannelRequest].
///
/// [ChannelManager::create_channel]: crate::ln::channelmanager::ChannelManager::create_channel
/// [Event::OpenChannelRequest]: crate::events::Event::OpenChannelRequest
pub fn can_support_additional_anchor_channel<
	AChannelManagerRef: Deref,
	ChannelSigner: EcdsaChannelSigner,
	FilterRef: Deref,
	BroadcasterRef: Deref,
	EstimatorRef: Deref,
	LoggerRef: Deref,
	PersistRef: Deref,
	ChainMonitorRef: Deref<
		Target = ChainMonitor<
			ChannelSigner,
			FilterRef,
			BroadcasterRef,
			EstimatorRef,
			LoggerRef,
			PersistRef,
		>,
	>,
>(
	context: &AnchorChannelReserveContext, utxos: &[Utxo], a_channel_manager: &AChannelManagerRef,
	chain_monitor: &ChainMonitorRef,
) -> bool
where
	AChannelManagerRef::Target: AChannelManager,
	FilterRef::Target: Filter,
	BroadcasterRef::Target: BroadcasterInterface,
	EstimatorRef::Target: FeeEstimator,
	LoggerRef::Target: Logger,
	PersistRef::Target: Persist<ChannelSigner>,
{
	let mut anchor_channels_with_balance = new_hash_set();
	// Calculate the number of in-progress anchor channels by inspecting ChannelMonitors with balance.
	// This includes channels that are in the process of being resolved on-chain.
	for (outpoint, channel_id) in chain_monitor.list_monitors() {
		let channel_monitor = if let Ok(channel_monitor) = chain_monitor.get_monitor(outpoint) {
			channel_monitor
		} else {
			continue;
		};
		if channel_monitor.channel_type_features().supports_anchors_zero_fee_htlc_tx()
			&& !channel_monitor.get_claimable_balances().is_empty()
		{
			anchor_channels_with_balance.insert(channel_id);
		}
	}
	// Count channels that are in the middle of negotiation as well.
	let num_anchor_channels = anchor_channels_with_balance.len()
		+ a_channel_manager
			.get_cm()
			.list_channels()
			.into_iter()
			.filter(|c| c.channel_type.is_none())
			.count();
	get_supportable_anchor_channels(context, utxos) > num_anchor_channels as u64
}

#[cfg(test)]
mod test {
	use super::*;
	use bitcoin::{OutPoint, ScriptBuf, TxOut, Txid};
	use std::str::FromStr;

	#[test]
	fn test_get_reserve_per_channel() {
		// At a 1000 sats/kw, with 4 expected transactions at ~1kw (commitment transaction, anchor
		// output spend transaction, 2 HTLC transactions), we expect the reserve to be around 4k sats.
		assert_eq!(
			get_reserve_per_channel(&AnchorChannelReserveContext {
				upper_bound_fee_rate: FeeRate::from_sat_per_kwu(1000),
				expected_accepted_htlcs: 1,
				taproot_wallet: false,
			}),
			Amount::from_sat(4349)
		);
	}

	fn make_p2wpkh_utxo(amount: Amount) -> Utxo {
		Utxo {
			outpoint: OutPoint {
				txid: Txid::from_str(
					"4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b",
				)
				.unwrap(),
				vout: 0,
			},
			output: TxOut { value: amount, script_pubkey: ScriptBuf::new() },
			satisfaction_weight: 1 * 4 + (1 + 1 + 72 + 1 + 33),
		}
	}

	#[test]
	fn test_get_supportable_anchor_channels() {
		let context = AnchorChannelReserveContext::default();
		let reserve_per_channel = get_reserve_per_channel(&context);
		// Only 3 disjoint sets with a value greater than the required reserve can be created.
		let utxos = vec![
			make_p2wpkh_utxo(reserve_per_channel * 3 / 2),
			make_p2wpkh_utxo(reserve_per_channel),
			make_p2wpkh_utxo(reserve_per_channel * 99 / 100),
			make_p2wpkh_utxo(reserve_per_channel * 99 / 100),
			make_p2wpkh_utxo(reserve_per_channel * 20 / 100),
		];
		assert_eq!(get_supportable_anchor_channels(&context, utxos.as_slice()), 3);
	}

	#[test]
	fn test_anchor_output_spend_transaction_weight() {
		// Example with smaller signatures:
		// https://mempool.space/tx/188b0f9f26999a48611dba4e2a88507251eba31f3695d005023de3514cba34bd
		// DER-encoded ECDSA signatures vary in size and can be 71-73 bytes.
		assert_eq!(
			anchor_output_spend_transaction_weight(&AnchorChannelReserveContext {
				taproot_wallet: false,
				..Default::default()
			}),
			717
		);

		// Example:
		// https://mempool.space/tx/9c493177e395ec77d9e725e1cfd465c5f06d4a5816dd0274c3a8c2442d854a85
		assert_eq!(
			anchor_output_spend_transaction_weight(&AnchorChannelReserveContext {
				taproot_wallet: true,
				..Default::default()
			}),
			723
		);
	}

	#[test]
	fn test_htlc_success_transaction_weight() {
		assert_eq!(
			htlc_success_transaction_weight(&AnchorChannelReserveContext {
				taproot_wallet: false,
				..Default::default()
			}),
			1102
		);

		assert_eq!(
			htlc_success_transaction_weight(&AnchorChannelReserveContext {
				taproot_wallet: true,
				..Default::default()
			}),
			1108
		);
	}

	#[test]
	fn test_htlc_timeout_transaction_weight() {
		// Example with smaller signatures:
		// https://mempool.space/tx/37185342f9f088bd12376599b245dbc02eb0bb6c4b99568b75a8cd775ddfd1f4
		assert_eq!(
			htlc_timeout_transaction_weight(&AnchorChannelReserveContext {
				taproot_wallet: false,
				..Default::default()
			}),
			1062
		);

		assert_eq!(
			htlc_timeout_transaction_weight(&AnchorChannelReserveContext {
				taproot_wallet: true,
				..Default::default()
			}),
			1068
		);
	}
}
