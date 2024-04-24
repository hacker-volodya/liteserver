use anyhow::Result;
use ton_block::{Augmentable, Augmentation, Deserializable, HashmapAugType, Serializable, TraverseNextStep};
use ton_types::BuilderData;

pub trait HashmapAugIterator<K, X, Y> {
    fn iterate_ext<F>(&self, reverse_order: bool, start_after: Option<K>, p: F) -> Result<bool> where F: FnMut(K, X) -> Result<bool>;
}

impl<
    K: Deserializable + Serializable, 
    X: Deserializable + Serializable + Augmentation<Y>, 
    Y: Augmentable,
    T: HashmapAugType<K, X, Y>
> HashmapAugIterator<K, X, Y> for T {
    fn iterate_ext<F>(&self, reverse_order: bool, start_after: Option<K>, mut p: F) -> Result<bool> where F: FnMut(K, X) -> Result<bool> {
        let after_cell_opt = start_after.map(|start| start.write_to_new_cell()).transpose()?;
        let is_complete = self.traverse(|prefix, prefix_len, _, value_opt| {
            let prefix = BuilderData::with_raw(prefix.into(), prefix_len)?;
            if let Some(after_cell) = &after_cell_opt {
                match prefix.compare_data(after_cell)? {
                    (Some(_), None) => unreachable!("prefix can't include full key"),
                    // do not visit branches that are smaller than `after` (for direct order) or bigger than `after` (for reverse order)
                    (None, Some(next_bit)) => if !reverse_order && next_bit == 1 {
                        return Ok(TraverseNextStep::VisitOne)
                    } else if reverse_order && next_bit == 0 {
                        return Ok(TraverseNextStep::VisitZero)
                    },
                    // in previous match arm we are still going to `after` value, but it can be not present,
                    // so we can accidentally run into unneeded branches, stop it here
                    (Some(n1), Some(n2)) => if !reverse_order && n1 < n2 || reverse_order && n1 > n2 {
                        return Ok(TraverseNextStep::Stop)
                    },
                    // stop if we found `after`
                    (None, None) => return Ok(TraverseNextStep::Stop),
                }
            }
            if let Some(value) = value_opt {
                let should_continue = p(K::construct_from_cell(prefix.into_cell()?)?, value)?;
                if !should_continue {
                    return Ok(TraverseNextStep::End(()));
                }
            }
            Ok(if reverse_order {
                TraverseNextStep::VisitOneZero
            } else {
                TraverseNextStep::VisitZeroOne
            })
        })?.is_some();
        Ok(is_complete)
    }
}

#[test]
fn test_hashmap_iterator() -> Result<()> {
    use ton_block::*;
    use ton_types::*;
    let mut transactions = Transactions::new();
    let acc = Account::with_address(ton_block::MsgAddressInt::AddrStd(MsgAddrStd::with_address(None, 0, AccountId::from_string("0000000000000000000000000000000000000000000000000000000000000000")?)));
    let msg = Message::with_ext_in_header(ExternalInboundMessageHeader::default());
    for i in 0..100 {
        transactions.insert(&Transaction::with_account_and_message(&acc, &msg, i)?)?;
    }
    let test_data: Vec<(bool, Option<u64>, Vec<u64>)> = vec![
        (true, Some(40), (0..40).rev().collect()),
        (true, Some(70), (0..70).rev().collect()),
        (false, Some(30), (31..100).collect()),
        (false, Some(0), (1..100).collect()),
        (false, Some(99), vec![]),
        (false, Some(100), vec![]),
        (false, Some(101), vec![]),
        (false, Some(1000), vec![]),
        (false, Some(98), vec![99]),
        (false, Some(97), vec![98, 99]),
        (true, Some(1), vec![0]),
        (true, Some(2), vec![1, 0]),
        (true, Some(0), vec![]),
        (true, Some(1000), (0..100).rev().collect()),
        (true, Some(100), (0..100).rev().collect()),
        (true, Some(99), (0..99).rev().collect()),
        (true, None, (0..100).rev().collect()),
        (false, None, (0..100).collect()),
    ];
    for (reverse_order, start_after, expected_keys) in test_data {
        let mut result = Vec::new();
        transactions.iterate_ext(reverse_order, start_after, |_, InRefValue(value)| {
            result.push(value.lt);
            Ok(true)
        })?;
        assert_eq!(result, expected_keys, "test case is rev={reverse_order}, after={start_after:?}");
    }
    Ok(())
}