//! Writing to Arkiv: signed bodies and chunks become create ops (flags 3, design 2.1), sent
//! through the paced pipeline (design 4.6); per-repo addresses are topped up from the wallet
//! per window of in-flight txs (design 4.2 step 6).

use alloy_signer_local::PrivateKeySigner;
use charter_core::{
    AttrValue, Attribute, Body, Envelope, ErrorCode, Hash32, Rid, chunk_attributes, content_hash,
    entity_attributes,
};
use charter_crypto::SecretKey;
use charter_store::pipeline::Landed;
use charter_store::{
    Address, Arkiv, AttributeValue, Operation, PipelineConfig, TxPipeline, U256, create_op,
    random_salt,
};

use crate::{Result, SyncError};

/// A signed entity ready to send: its envelope bytes and content hash.
#[derive(Debug, Clone)]
pub struct Signed {
    pub bytes: Vec<u8>,
    pub h: Hash32,
}

pub fn sign<B: Body>(sk: &SecretKey, body: &B) -> Result<Signed> {
    let env = Envelope::sign(sk, B::KIND, body.encode())?;
    let bytes = env.encode();
    Ok(Signed {
        h: content_hash(B::KIND, &bytes),
        bytes,
    })
}

pub(crate) fn wire(attrs: &[Attribute]) -> Vec<(&str, AttributeValue, bool)> {
    attrs
        .iter()
        .map(|a| {
            let v = match &a.value {
                AttrValue::Str(s) => AttributeValue::Str(s.clone()),
                AttrValue::U64(n) => AttributeValue::U64(*n),
                AttrValue::Bytes(b) => AttributeValue::Bytes(b.clone()),
                AttrValue::Bytes32(h) => AttributeValue::Bytes32(*h),
            };
            (a.name.as_str(), v, a.indexed())
        })
        .collect()
}

/// The create op for already-signed envelope bytes of kind `B`.
pub fn entity_op<B: Body>(bytes: &[u8], lifetime: u64) -> Result<Operation> {
    let env = Envelope::decode(bytes)?;
    let body = B::decode(&env.body)?;
    let attrs = entity_attributes(&env, bytes, &body);
    Ok(create_op(&wire(&attrs), lifetime, random_salt())?)
}

pub fn chunk_op(
    rid: &Rid,
    pack_id: &Hash32,
    index: u64,
    bytes: &[u8],
    lifetime: u64,
) -> Result<Operation> {
    let attrs = chunk_attributes(rid, pack_id, index, bytes);
    Ok(create_op(&wire(&attrs), lifetime, random_salt())?)
}

/// Sends every job (one tx each) from `signer` and waits for all receipts.
pub async fn send<A: Arkiv>(
    a: &A,
    signer: &PrivateKeySigner,
    cfg: &PipelineConfig,
    jobs: Vec<Vec<Operation>>,
) -> Result<Vec<Landed>> {
    if jobs.is_empty() {
        return Ok(vec![]);
    }
    let chain_id = a.chain_id().await?;
    let mut p = TxPipeline::new(a, signer.clone(), chain_id, cfg.clone());
    Ok(p.run(jobs).await?)
}

/// Gas `execute(ops)` would use from `from`.
pub async fn estimate<A: Arkiv>(a: &A, from: Address, ops: &[Operation]) -> Result<u64> {
    Ok(a.estimate(from, ops).await?)
}

/// What `gas` costs at the pipeline's fee cap: `gas x 1.1 x 2 x base fee`.
pub fn price(gas: u64, base_fee: u128) -> U256 {
    U256::from(gas) * U256::from(11) / U256::from(10) * U256::from(2 * base_fee)
}

/// Sends every job from `signer` like [`send`], topping `signer` up from `wallet` before
/// each window its balance cannot cover (design 4.2 step 6).
pub async fn send_funded<A: Arkiv>(
    a: &A,
    signer: &PrivateKeySigner,
    wallet: &PrivateKeySigner,
    cfg: &PipelineConfig,
    jobs: Vec<Vec<Operation>>,
) -> Result<Vec<Landed>> {
    if jobs.is_empty() {
        return Ok(vec![]);
    }
    let chain_id = a.chain_id().await?;
    let mut p = TxPipeline::new(a, signer.clone(), chain_id, cfg.clone()).funded_by(wallet.clone());
    Ok(p.run(jobs).await?)
}

/// The one up-front check of a push: `wallet` plus what `spenders` already hold must cover
/// `need` (predicted spend plus one window's reserve), else `E_INSUFFICIENT_FUNDS`.
pub async fn check_funds<A: Arkiv>(
    a: &A,
    wallet: Address,
    spenders: &[Address],
    need: U256,
) -> Result<()> {
    let mut have = a.balance(wallet).await?;
    for s in spenders {
        have += a.balance(*s).await?;
    }
    if have >= need {
        return Ok(());
    }
    Err(SyncError::coded(
        ErrorCode::InsufficientFunds,
        format!(
            "this push needs {need} wei (predicted spend plus one window of in-flight max fees); \
             wallet {wallet} and the repo addresses hold {have} (run `chart fund`)"
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use charter_core::{PeerBody, open};
    use charter_crypto::MasterSeed;

    #[test]
    fn signed_entity_ops_carry_the_schema_attributes() {
        let sk = MasterSeed::from_bytes([4; 32]).signing_key();
        let pk = sk.public_key();
        let body = PeerBody {
            alg: pk.alg().id(),
            pk: pk.as_bytes().to_vec(),
            alias: "a".into(),
        };
        let s = sign(&sk, &body).expect("sign");
        let op = entity_op::<PeerBody>(&s.bytes, 10).expect("op");
        assert!(!op.operationData.is_empty());
        let env = Envelope::decode(&s.bytes).expect("env");
        let attrs = entity_attributes(&env, &s.bytes, &body);
        assert_eq!(wire(&attrs).len(), attrs.len());
        assert!(wire(&attrs).iter().any(|(n, _, idx)| *n == "ag.v" && !idx));
        let obs = charter_core::Observed {
            key: [0; 32],
            creator: [0; 20],
            block: 1,
            tx: 0,
            expires_at: 2,
            updated_at: 1,
            attributes: attrs,
        };
        assert_eq!(open::<PeerBody>(&obs).expect("opens").h, s.h);
        assert!(entity_op::<PeerBody>(b"junk", 1).is_err());
        assert!(chunk_op(&[1; 32], &[2; 32], 0, &[1, 2, 3], 5).is_ok());
        assert_eq!(price(10, 1), U256::from(22));
    }
}
