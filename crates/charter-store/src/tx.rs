//! Signed transactions, receipts, calldata sizing and revert decoding.

use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, Bytes, TxHash, TxKind, U256, address};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, SolInterface};
use arkiv_bindings::{IEntityRegistry, Operation};

use crate::StoreError;

/// The entity registry every Arkiv write goes to.
pub const ARKIV_ADDRESS: Address = address!("4400000000000000000000000000000000000044");
/// Rabdi testnet.
pub const RABDI_CHAIN_ID: u64 = 7_794_095;
/// Public RPC endpoint of Rabdi.
pub const RABDI_RPC: &str = "https://rpc.rabdi.db-chain.testnet.arkiv.network";
/// Largest raw signed tx the public RPC accepts (MEASURED).
pub const MAX_RAW_TX_BYTES: usize = 131_030;
/// Per-tx gas cap enforced by the executor (EIP-7825).
pub const TX_GAS_CAP: u64 = 16_777_216;
/// Block gas limit observed on Rabdi.
pub const BLOCK_GAS_LIMIT: u64 = 30_000_000;

/// The fields a caller chooses; chain id and signature are added by [`SignedTx::sign`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxParams {
    pub nonce: u64,
    pub to: Address,
    pub value: U256,
    pub input: Bytes,
    pub gas_limit: u64,
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
}

/// An EIP-1559 tx, signed and 2718-encoded, plus the fields the pipeline tracks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedTx {
    raw: Bytes,
    hash: TxHash,
    from: Address,
    chain_id: u64,
    nonce: u64,
    gas_limit: u64,
    max_cost: U256,
}

impl SignedTx {
    pub fn sign(signer: &PrivateKeySigner, chain_id: u64, p: TxParams) -> Result<Self, StoreError> {
        let max_cost = U256::from(p.gas_limit) * U256::from(p.max_fee_per_gas) + p.value;
        let tx = TxEip1559 {
            chain_id,
            nonce: p.nonce,
            gas_limit: p.gas_limit,
            max_fee_per_gas: p.max_fee_per_gas,
            max_priority_fee_per_gas: p.max_priority_fee_per_gas,
            to: TxKind::Call(p.to),
            value: p.value,
            access_list: Default::default(),
            input: p.input,
        };
        let sig = signer
            .sign_hash_sync(&tx.signature_hash())
            .map_err(|e| StoreError::Rpc(format!("signing failed: {e}")))?;
        let env = TxEnvelope::from(tx.into_signed(sig));
        Ok(Self {
            raw: env.encoded_2718().into(),
            hash: *env.tx_hash(),
            from: signer.address(),
            chain_id,
            nonce: p.nonce,
            gas_limit: p.gas_limit,
            max_cost,
        })
    }

    pub fn raw(&self) -> &Bytes {
        &self.raw
    }
    pub fn hash(&self) -> TxHash {
        self.hash
    }
    pub fn from(&self) -> Address {
        self.from
    }
    pub fn chain_id(&self) -> u64 {
        self.chain_id
    }
    pub fn nonce(&self) -> u64 {
        self.nonce
    }
    pub fn gas_limit(&self) -> u64 {
        self.gas_limit
    }
    /// `gas_limit x max_fee_per_gas + value`: what the sender must hold.
    pub fn max_cost(&self) -> U256 {
        self.max_cost
    }
}

/// What the pipeline needs from a receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Receipt {
    pub tx_hash: TxHash,
    pub block_number: u64,
    pub success: bool,
    pub gas_used: u64,
    pub effective_gas_price: u128,
    /// Position in its block (`transactionIndex`).
    pub index: u32,
}

/// What a tx moved: sender, recipient and value (`eth_getTransactionByHash`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxInfo {
    pub hash: TxHash,
    pub from: Address,
    /// `None` for a contract creation.
    pub to: Option<Address>,
    pub value: U256,
    /// `None` while pending.
    pub block_number: Option<u64>,
}

/// `execute(ops)` calldata.
pub fn execute_calldata(ops: &[Operation]) -> Bytes {
    IEntityRegistry::executeCall { ops: ops.to_vec() }
        .abi_encode()
        .into()
}

/// Calldata bytes of `execute` with no ops: selector, array offset, array length.
pub const EXECUTE_BASE_BYTES: usize = 4 + 32 + 32;

/// Calldata bytes one op adds to `execute`: array slot, tuple head (tag, offset), length, padded data.
pub fn op_calldata_bytes(op: &Operation) -> usize {
    32 + 32 + 32 + 32 + op.operationData.len().div_ceil(32) * 32
}

/// Decode revert data as an `IEntityRegistry` error; returns its signature, e.g.
/// `NotOwner(bytes32,address,address)`.
pub fn decode_revert(data: &[u8]) -> Option<String> {
    use IEntityRegistry::IEntityRegistryErrors as E;
    let e = E::abi_decode(data).ok()?;
    E::name_by_selector(e.selector()).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Transaction;
    use alloy_consensus::transaction::SignerRecoverable;
    use alloy_eips::eip2718::Decodable2718;
    use alloy_primitives::B256;
    use alloy_sol_types::SolError;

    #[test]
    fn signed_tx_round_trips_and_recovers_the_sender() {
        let key = PrivateKeySigner::random();
        let p = TxParams {
            nonce: 7,
            to: ARKIV_ADDRESS,
            value: U256::ZERO,
            input: execute_calldata(&[Operation::delete(B256::ZERO)]),
            gas_limit: 100_000,
            max_fee_per_gas: 2_000_000_000,
            max_priority_fee_per_gas: 0,
        };
        let tx = SignedTx::sign(&key, RABDI_CHAIN_ID, p).expect("sign");
        let env = TxEnvelope::decode_2718(&mut tx.raw().as_ref()).expect("decode");
        assert_eq!(env.recover_signer().expect("recover"), key.address());
        assert_eq!((env.nonce(), env.chain_id()), (7, Some(RABDI_CHAIN_ID)));
        assert_eq!(*env.tx_hash(), tx.hash());
        assert_eq!(
            (tx.from(), tx.nonce(), tx.gas_limit(), tx.chain_id()),
            (key.address(), 7, 100_000, RABDI_CHAIN_ID)
        );
    }

    #[test]
    fn op_sizes_add_up_to_the_encoded_calldata() {
        let ops = vec![
            Operation::delete(B256::ZERO),
            Operation::create(1, 0, 10, 3, vec![]),
            Operation::extend_expiry(B256::ZERO, 0, 5),
        ];
        let predicted: usize =
            EXECUTE_BASE_BYTES + ops.iter().map(op_calldata_bytes).sum::<usize>();
        assert_eq!(predicted, execute_calldata(&ops).len());
    }

    #[test]
    fn decodes_registry_errors() {
        let data = IEntityRegistry::ReadOnlyEntity {
            entityKey: B256::ZERO,
        }
        .abi_encode();
        assert!(
            decode_revert(&data)
                .expect("decodes")
                .contains("ReadOnlyEntity")
        );
        assert_eq!(decode_revert(&[1, 2, 3]), None);
    }
}
