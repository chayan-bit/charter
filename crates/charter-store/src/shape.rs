//! Reusing one `eth_estimateGas` for txs of the same shape (docs/p5/speed-and-cost.md 3.13,
//! "local fee formula"). A create's gas is calldata plus compute plus storage: storage and
//! compute follow the attribute names, types, value lengths and the lifetime, which the
//! shape pins; calldata gas also depends on how many bytes are zero, which is corrected
//! for here. Anything else (extends, patches, transfers) is always estimated, because its
//! gas depends on chain state the op does not carry.

use std::collections::HashMap;

use alloy_primitives::B256;
use alloy_primitives::Bytes;
use arkiv_bindings::{Create, OP_CREATE, Operation};

/// Extra floor gas of a nonzero calldata byte over a zero one (EIP-7623: 40 vs 10).
const NONZERO_EXTRA: u64 = 30;

/// Everything about a create job that its gas depends on, except the bytes themselves.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Shape {
    calldata_len: usize,
    ops: Vec<CreateShape>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CreateShape {
    expires_at: u64,
    min_lifetime: u64,
    flags: u8,
    attrs: Vec<(B256, u8, usize)>,
}

impl Shape {
    /// The shape of `execute(ops)`, or `None` unless every op is a create.
    pub(crate) fn of(ops: &[Operation], calldata: &Bytes) -> Option<Self> {
        let ops = ops
            .iter()
            .map(|op| {
                if op.operation != OP_CREATE {
                    return None;
                }
                let c: Create = op.payload_of().ok()?;
                Some(CreateShape {
                    expires_at: c.expiresAt,
                    min_lifetime: c.minLifetime,
                    flags: c.creationFlags,
                    attrs: c
                        .attributes
                        .into_iter()
                        .map(|a| (a.name, a.typeId, a.value.len()))
                        .collect(),
                })
            })
            .collect::<Option<Vec<_>>>()?;
        (!ops.is_empty()).then_some(Self {
            calldata_len: calldata.len(),
            ops,
        })
    }
}

fn zeros(b: &[u8]) -> usize {
    b.iter().filter(|x| **x == 0).count()
}

/// Estimates seen per shape: the gas and the zero-byte count of the calldata it was for.
#[derive(Debug, Default)]
pub(crate) struct Estimates(HashMap<Shape, (u64, usize)>);

impl Estimates {
    /// Gas for `calldata` of shape `s` from an earlier estimate: that estimate plus the
    /// floor gas of every zero byte it had that this one lacks.
    pub(crate) fn get(&self, s: &Shape, calldata: &[u8]) -> Option<u64> {
        let (gas, ref_zeros) = self.0.get(s)?;
        let fewer = ref_zeros.saturating_sub(zeros(calldata)) as u64;
        Some(gas + NONZERO_EXTRA * fewer)
    }

    pub(crate) fn put(&mut self, s: Shape, calldata: &[u8], gas: u64) {
        self.0.insert(s, (gas, zeros(calldata)));
    }

    pub(crate) fn forget(&mut self, s: &Shape) {
        self.0.remove(s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AttributeValue, create_op, execute_calldata, extend_op};

    fn chunk(bytes: Vec<u8>, lifetime: u64) -> Operation {
        let attrs = [("ag.body", AttributeValue::Bytes(bytes), false)];
        create_op(&attrs, lifetime, 7).expect("op")
    }

    fn shape(ops: &[Operation]) -> Option<Shape> {
        Shape::of(ops, &execute_calldata(ops))
    }

    #[test]
    fn same_lengths_and_lifetime_share_a_shape() {
        let a = shape(&[chunk(vec![1; 100], 10)]).expect("create");
        assert_eq!(shape(&[chunk(vec![9; 100], 10)]), Some(a.clone()));
        assert_ne!(shape(&[chunk(vec![1; 101], 10)]), Some(a.clone()));
        assert_ne!(shape(&[chunk(vec![1; 100], 11)]), Some(a.clone()));
        assert_ne!(
            shape(&[chunk(vec![1; 100], 10), chunk(vec![1; 100], 10)]),
            Some(a)
        );
    }

    #[test]
    fn extends_and_mixed_jobs_have_no_shape() {
        let ext = extend_op(B256::ZERO, 5);
        assert_eq!(shape(std::slice::from_ref(&ext)), None);
        assert_eq!(shape(&[chunk(vec![1; 4], 1), ext]), None);
        assert_eq!(shape(&[]), None);
    }

    #[test]
    fn fewer_zero_bytes_add_their_floor_gas() {
        let zeros_op = [chunk(vec![0; 100], 10)];
        let ones_op = [chunk(vec![1; 100], 10)];
        let (z, o) = (execute_calldata(&zeros_op), execute_calldata(&ones_op));
        let s = shape(&zeros_op).expect("shape");
        let mut e = Estimates::default();
        assert_eq!(e.get(&s, &z), None);
        e.put(s.clone(), &z, 50_000);
        assert_eq!(e.get(&s, &z), Some(50_000));
        assert_eq!(e.get(&s, &o), Some(50_000 + 30 * 100));
        e.put(s.clone(), &o, 60_000);
        assert_eq!(
            e.get(&s, &z),
            Some(60_000),
            "more zeros never lowers the reuse"
        );
        e.forget(&s);
        assert_eq!(e.get(&s, &z), None);
    }
}
