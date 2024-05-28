# TON Liteserver in Rust

## Method support
| Status | Method | Description |
|--------|--------|-------------|
| | **impemented**
| ✅ | WaitMasterchainSeqno
| ✅ | GetMasterchainInfo
| ✅ | GetBlockHeader
| ✅ | GetAllShardsInfo
| ✅ | ListBlockTransactions
| ✅ | GetBlock
| ✅ | GetAccountState | no shard_proof for now
| ⚠️ | GetTransactions | lt block search in progress
| ✅ | LookupBlock | utime search is not working for now
| | **high priority** (toncenter)
| ⚠️ | SendMessage | TVM required
| 🔜 | GetConfigParams
| ⚠️ | RunSmcMethod | TVM required
| ⚠️ | GetBlockProof | block proof research in progress
| ⚠️ | LookupBlockWithProof | block proof research in progress
| ⚠️ | GetShardBlockProof | block proof research in progress
| | **medium priority**
| 🔜 | GetConfigAll
| 🔜 | GetMasterchainInfoExt
| 🔜 | GetOneTransaction
| 🔜 | GetShardInfo
| 🔜 | GetAccountStatePrunned
| 🔜 | ListBlockTransactionsExt
| 🔜 | GetLibraries
| 🔜 | GetLibrariesWithProof
| | **low priority**
| 🔜 | GetTime
| 🔜 | GetVersion
| 🔜 | GetValidatorStats
| 🔜 | GetState
| 🔜 | GetOutMsgQueueSizes
| 🔜 | GetValidatorGroups
| 🔜 | GetCandidate