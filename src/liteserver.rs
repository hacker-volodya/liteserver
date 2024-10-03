use std::{sync::Arc, task::Poll, time::Duration};

use anyhow::{anyhow, Result};
use base64::Engine as _;
use broxus_util::now;
use futures_util::future::BoxFuture;
use ton_block::{AccountIdPrefixFull, Block, CommonMsgInfo, ConfigParams, CryptoSignaturePair, Deserializable, GetRepresentationHash, HashmapAugType, InRefValue, McShardRecord, MerkleProof, Message, MsgAddrStd, Serializable, ShardIdent, ShardStateUnsplit, Transaction, TraverseNextStep, SHARD_FULL};
use ton_indexer::{utils::{BlockProofStuff, BlockProofStuffAug, ShardStateStuff}, Engine, GlobalConfig};
use ton_liteapi::{
    layers::{UnwrapMessagesLayer, WrapErrorLayer},
    server::serve,
    tl::{
        common::{AccountId, BlockIdExt, BlockLink, Int256, LibraryEntry, Signature, SignatureSet, ZeroStateIdExt},
        request::{
            GetAccountState, GetAllShardsInfo, GetBlock, GetBlockHeader, GetBlockProof, GetConfigAll, GetConfigParams, GetLibraries, GetMasterchainInfoExt, GetTransactions, ListBlockTransactions, LookupBlock, Request, RunSmcMethod, SendMessage, WaitMasterchainSeqno, WrappedRequest
        },
        response::{AccountState, AllShardsInfo, BlockData, BlockHeader, BlockTransactions, ConfigInfo, LibraryResult, MasterchainInfo, MasterchainInfoExt, PartialBlockProof, Response, RunMethodResult, SendMsgStatus, TransactionId, TransactionList},
    },
    types::LiteError,
};
use ton_types::{deserialize_tree_of_cells, serialize_toc, BagOfCells, Cell, HashmapType, SliceData, UInt256, UsageTree};
use tower::{make::Shared, Service, ServiceBuilder};
use x25519_dalek::StaticSecret;

use crate::{tvm::EmulatorBuilder, utils::HashmapAugIterator};

#[derive(Clone)]
pub struct LiteServer {
    engine: Arc<Engine>,
    config: GlobalConfig,
}

impl LiteServer {
    pub async fn get_masterchain_info(&self) -> Result<MasterchainInfo> {
        let last = self.engine.load_shards_client_mc_block_id()?;
        let root_hash = self.engine.load_state(&last).await?.root_cell().repr_hash();
        Ok(MasterchainInfo {
            last: last.as_liteapi(),
            state_root_hash: Int256(root_hash.into()),
            init: ZeroStateIdExt {
                workchain: self.config.zero_state.shard_id.workchain_id(),
                root_hash: Int256(self.config.zero_state.root_hash.into()),
                file_hash: Int256(self.config.zero_state.file_hash.into()),
            },
        })
    }

    pub async fn get_masterchain_info_ext(&self, req: GetMasterchainInfoExt) -> Result<MasterchainInfoExt> {
        if req.mode != 0 {
            return Err(anyhow!("Unsupported mode"));
        }
        let last = self.engine.load_shards_client_mc_block_id()?;
        let state = self.engine.load_state(&last).await?;
        let root_hash = state.root_cell().repr_hash();
        Ok(MasterchainInfoExt {
            last: last.as_liteapi(),
            state_root_hash: Int256(root_hash.into()),
            init: ZeroStateIdExt {
                workchain: self.config.zero_state.shard_id.workchain_id(),
                root_hash: Int256(self.config.zero_state.root_hash.into()),
                file_hash: Int256(self.config.zero_state.file_hash.into()),
            },
            mode: (),
            version: 0x101,
            capabilities: 7,
            last_utime: state.state().gen_time(),
            now: now(),
        })
    }

    fn make_block_proof(
        block_root: Cell,
        with_state_update: bool,
        with_value_flow: bool,
        with_extra: bool,
        with_shard_hashes: bool,
        with_config: bool,
        config_params: Option<&[i32]>
    ) -> Result<MerkleProof> {
        let usage_tree = UsageTree::with_root(block_root.clone());
        let block = Block::construct_from_cell(usage_tree.root_cell())?;

        // add data to proof
        let info = block.read_info()?;
        let _prev_ref = info.read_prev_ref()?;
        let _prev_vert_ref = info.read_prev_vert_ref()?;
        let _master_ref = info.read_master_ref()?;
        if with_state_update {
            let _state_update = block.read_state_update()?;
        }
        if with_value_flow {
            block.read_value_flow()?.read_in_full_depth()?;
        }
        if with_extra {
            let mc_block_extra = block.read_extra()?.read_custom()?;
            if let Some(extra) = mc_block_extra {
                if with_shard_hashes {
                    extra.shards().root().ok_or(anyhow!("no shard hashes"))?.preload_with_depth_hint::<128>()?;
                }
                if with_config {
                    Self::touch_config(extra.config().ok_or(anyhow!("no config"))?, config_params)?;
                }
            }
        }
        MerkleProof::create_by_usage_tree(&block_root, usage_tree)
    }

    async fn load_block(&self, block_id: &BlockIdExt) -> Result<Option<Cell>> {
        self.load_block_by_tonlabs_id(&block_id.as_tonlabs()?).await
    }

    async fn load_block_by_tonlabs_id(&self, tonlabs_block_id: &ton_block::BlockIdExt) -> Result<Option<Cell>> {
        let block_handle_storage = self.engine.storage().block_handle_storage();
        let block_storage = self.engine.storage().block_storage();
        if let Some(handle) = block_handle_storage.load_handle(&tonlabs_block_id)? {
            if handle.meta().has_data() {
                let block = block_storage.load_block_data_raw_ref(&handle).await?;
                let block_root = ton_types::deserialize_tree_of_cells(&mut block.as_ref())?;
                return Ok(Some(block_root))
            }
        }
        Ok(None)
    }

    async fn load_block_proof(&self, block_id: &ton_block::BlockIdExt) -> Result<BlockProofStuff> {
        let block_handle_storage = self.engine.storage().block_handle_storage();
        let block_storage = self.engine.storage().block_storage();
        if let Some(handle) = block_handle_storage.load_handle(&block_id)? {
            if handle.meta().has_data() {
                return Ok(block_storage.load_block_proof(&handle, false).await?);
            }
        }
        Err(anyhow!("no such proof in db"))
    }

    async fn load_state(&self, block_id: &BlockIdExt) -> Result<Arc<ShardStateStuff>> {
        self.load_state_by_tonlabs_id(&block_id.as_tonlabs()?).await
    }

    async fn load_state_by_tonlabs_id(&self, block_id: &ton_block::BlockIdExt) -> Result<Arc<ShardStateStuff>> {
        self.engine.load_state(block_id).await
    }

    pub async fn get_block(&self, request: GetBlock) -> Result<BlockData> {
        let block = self.load_block(&request.id).await?.ok_or(anyhow!("no such block in db"))?;
        Ok(BlockData { id: request.id, data: serialize_toc(&block)? })
    }

    pub async fn get_block_header(&self, request: GetBlockHeader) -> Result<BlockHeader> {
        let block_root = self.load_block(&request.id).await?.ok_or(anyhow!("no such block in db"))?;
        let merkle_proof = Self::make_block_proof(
            block_root,
            request.with_state_update.is_some(),
            request.with_value_flow.is_some(),
            request.with_extra.is_some(),
            false,
            false,
            None,
        )?;

        Ok(BlockHeader {
            id: request.id,
            mode: (),
            header_proof: merkle_proof.write_to_bytes()?,
            with_state_update: request.with_state_update,
            with_value_flow: request.with_value_flow,
            with_extra: request.with_extra,
            with_shard_hashes: request.with_shard_hashes,
            with_prev_blk_signatures: request.with_prev_blk_signatures,
        })
    }

    fn get_shard_with_proof(mc_state: Cell, for_account: &AccountIdPrefixFull) -> Result<(McShardRecord, MerkleProof)> {
        let usage_tree = UsageTree::with_root(mc_state.clone());
        let state = ShardStateUnsplit::construct_from_cell(usage_tree.root_cell())?;
        let extra = state
            .read_custom()?
            .ok_or_else(|| anyhow!("block must contain McStateExtra"))?;
        let shard = extra.shards().find_shard_by_prefix(for_account)?.ok_or(anyhow!("no such shard"))?;
        let proof = MerkleProof::create_by_usage_tree(&mc_state, usage_tree)?;
        Ok((shard, proof))
    }

    pub async fn get_all_shards_info(&self, request: GetAllShardsInfo) -> Result<AllShardsInfo> {
        let block_root = self.load_block(&request.id).await?.ok_or(anyhow!("no such block in db"))?;

        // proof1: from block to shardstate update
        let proof1 = Self::make_block_proof(block_root, true, false, false, false, false, None)?;

        // proof2: from shardstate root to shard_hashes
        let state_stuff = self.load_state(&request.id).await?;
        let usage_tree = UsageTree::with_root(state_stuff.root_cell().clone());
        let state = ShardStateUnsplit::construct_from_cell(usage_tree.root_cell())?;
        let extra = state
            .read_custom()?
            .ok_or_else(|| anyhow!("block must contain McStateExtra"))?;
        let shards = extra.shards();
        let proof2 = MerkleProof::create_by_usage_tree(state_stuff.root_cell(), usage_tree)?;

        let mut proof = Vec::new();
        BagOfCells::with_roots(&[proof1.serialize()?, proof2.serialize()?])
            .write_to(&mut proof, false)?;

        Ok(AllShardsInfo {
            id: request.id,
            proof,
            data: shards.write_to_bytes()?,
        })
    }

    pub async fn list_block_transactions(
        &self,
        req: ListBlockTransactions,
    ) -> Result<BlockTransactions> {
        let block_root = self.load_block(&req.id).await?.ok_or(anyhow!("no such block in db"))?;
        let usage_tree = UsageTree::with_root(block_root.clone());
        let block = Block::construct_from_cell(usage_tree.root_cell())?;
        let account_blocks = block.read_extra()?.read_account_blocks()?;
        let reverse = req.reverse_order.is_some();
        let after = req.after.as_ref().map(|txid| (UInt256::from_slice(&txid.account.0), txid.lt as u64));

        let mut ids = Vec::new();

        let mut add_tx = |_, InRefValue::<Transaction>(tx)| {
            ids.push(TransactionId {
                mode: (),
                account: Some(Int256(tx.account_addr.to_owned().get_next_bytes(32)?.try_into().unwrap())),
                lt: Some(tx.lt),
                hash: Some(Int256(*tx.hash()?.as_array()))
            });
            let should_continue = ids.len() < req.count as usize;
            Ok(should_continue)
        };
        
        let mut is_complete = true;
        if let Some((after_account, after_lt)) = after {
            if let Some(account) = account_blocks.get(&after_account)? {
                is_complete = account.transactions().iterate_ext(reverse, Some(after_lt), &mut add_tx)?;
            }
        }
        if is_complete {
            is_complete = account_blocks.iterate_ext(reverse, after.map(|x| x.0), |_, account_block| {
                let is_complete = account_block.transactions().iterate_ext(reverse, None, &mut add_tx)?;
                Ok(is_complete)
            })?;
        }

        Ok(BlockTransactions {
            id: req.id,
            req_count: req.count,
            incomplete: !is_complete,
            ids,
            proof: if req.want_proof.is_some() {
                MerkleProof::create_by_usage_tree(&block_root, usage_tree)?.serialize()?.write_to_bytes()?
            } else {
                Vec::new()
            },
        })
    }

    async fn get_block_for_account(&self, account: &AccountId, reference_block: &ton_block::BlockIdExt) -> Result<ton_block::BlockIdExt> {
        if !reference_block.shard().is_masterchain() || account.workchain == -1 {
            return Ok(reference_block.clone())
        }
        let mc_block_root = self.load_block_by_tonlabs_id(&reference_block).await?.ok_or(anyhow!("no such block in db"))?;
        let mc_block = Block::construct_from_cell(mc_block_root)?;
        let mc_shard_record = mc_block
            .read_extra()?
            .read_custom()?.ok_or(anyhow!("bug: mc block must contain McBlockExtra"))?
            .hashes().find_shard_by_prefix(&account.prefix_full()?)?.ok_or(anyhow!("no such shard"))?;
        let shard_block = mc_shard_record.blk_id();
        Ok(shard_block.clone())
    }

    async fn get_block_for_account_with_proof(&self, account: &AccountId, reference_block: &ton_block::BlockIdExt) -> Result<(ton_block::BlockIdExt, Option<MerkleProof>)> {
        if !reference_block.shard().is_masterchain() || account.workchain == -1 {
            return Ok((reference_block.clone(), None))
        }
        let mc_state = self.load_state_by_tonlabs_id(&reference_block).await?;
        let (shard, proof) = Self::get_shard_with_proof(mc_state.root_cell().clone(), &account.prefix_full()?)?;
        Ok((shard.block_id, Some(proof)))
    }

    pub async fn get_account_state(&self, req: GetAccountState) -> Result<AccountState> {
        let reference_id = if req.id.seqno != 0xffff_ffff {
            req.id.as_tonlabs()?
        } else {
            self.engine.load_shards_client_mc_block_id()?
        };
        let (shard_id, shard_proof_) = self.get_block_for_account_with_proof(&req.account, &reference_id).await?;
        let shard_block = self.load_block_by_tonlabs_id(&shard_id).await?.ok_or(anyhow!("no such block in db"))?;
        let state_stuff = self.engine.load_state(&shard_id).await?;
        let usage_tree_p2 = UsageTree::with_root(state_stuff.root_cell().clone());
        let state = ShardStateUnsplit::construct_from_cell(usage_tree_p2.root_cell())?;
        let account = state
            .read_accounts()?
            .account(&req.account.id())?
            .ok_or(anyhow!("no such account"))?;
        let proof1 = Self::make_block_proof(shard_block, true, false, false, false, false, None)?;
        let proof2 = MerkleProof::create_by_usage_tree(state_stuff.root_cell(), usage_tree_p2)?;
        let mut proof = Vec::new();
        BagOfCells::with_roots(&[proof1.serialize()?, proof2.serialize()?]).write_to(&mut proof, false)?;
        let mut shard_proof = Vec::new();
        if let Some(proof4) = shard_proof_ {
            let mc_block = self.load_block_by_tonlabs_id(&reference_id).await?.ok_or(anyhow!("no such block in db"))?;
            let proof3 = Self::make_block_proof(mc_block, true, false, false, false, false, None)?;
            BagOfCells::with_roots(&[proof3.serialize()?, proof4.serialize()?]).write_to(&mut shard_proof, false)?;
        }
        Ok(AccountState { id: reference_id.as_liteapi(), shardblk: shard_id.as_liteapi(), shard_proof, proof, state: serialize_toc(&account.account_cell())? })
    }

    async fn search_mc_block_by_lt(&self, ltime: u64) -> Result<Option<ton_block::BlockIdExt>> {
        let last = self.engine.load_last_applied_mc_block_id()?;
        let mc_state = self.engine.load_state(&last).await?;
        let extra = mc_state.shard_state_extra()?;
        let result = extra.prev_blocks.traverse(|_, _, aug, value_opt| {
            if aug.max_end_lt < ltime {
                return Ok(TraverseNextStep::Stop)
            }
            if let Some(block_id) = value_opt {
                println!("found {block_id:?}");
                return Ok(TraverseNextStep::End(block_id))
            }
            Ok(TraverseNextStep::VisitZeroOne)
        })?;
        Ok(result.map(|id| id.master_block_id().1))
    }

    async fn search_mc_block_by_seqno(&self, seqno: u32) -> Result<Option<ton_block::BlockIdExt>> {
        let last = self.engine.load_last_applied_mc_block_id()?;
        if seqno == last.seq_no {
            return Ok(Some(last))
        }
        let mc_state = self.engine.load_state(&last).await?;
        let extra = mc_state.shard_state_extra()?;
        let result = extra.prev_blocks.get(&seqno)?;
        Ok(result.map(|id| id.master_block_id().1))
    }

    #[tracing::instrument(skip(self), level = "debug", err)]
    async fn search_shard_block_by_lt(&self, prefix: &AccountIdPrefixFull, ltime: u64) -> Result<Option<Vec<u8>>> {
        let full_shard = prefix.shard_ident()?;
        let raw_prefix = full_shard.shard_prefix_with_tag();
        // iterate through all possible shard prefixes (max_split = 4 for TON)
        for prefix_len in 0..5 {
            let shard = ShardIdent::with_prefix_len(prefix_len, prefix.workchain_id(), raw_prefix)?;
            if let Some(block) = self.engine.storage().block_storage().search_block_by_lt(&shard, ltime)? {
                return Ok(Some(block))
            }
        }
        Ok(None)
    }

    #[tracing::instrument(skip(self), level = "debug", err)]
    async fn search_transactions(&self, account: &AccountId, mut lt: u64, count: Option<usize>) -> Result<(Vec<BlockIdExt>, Vec<Transaction>)> {
        let mut transactions = Vec::new();
        let mut block_ids = Vec::new();
        while let Some(block_raw) = self.search_shard_block_by_lt(&account.prefix_full()?, lt).await? {
            let block_root = ton_types::deserialize_tree_of_cells(&mut block_raw.as_slice())?;
            let block = Block::construct_from_cell(block_root.clone())?;
            let block_info = block.read_info()?;
            let block_id = BlockIdExt {
                workchain: block_info.shard().workchain_id(),
                shard: block_info.shard().shard_prefix_with_tag(),
                seqno: block_info.seq_no(),
                root_hash: Int256(*block_root.repr_hash().as_array()),
                file_hash: Int256(*UInt256::calc_file_hash(&block_raw).as_array()),
            };
            if let Some(account_block) = block.read_extra()?.read_account_blocks()?.get(&account.raw_id())? {
                block_ids.push(block_id);
                println!("we want lt {lt}");
                let complete = account_block.transactions().iterate_ext(true, None, |_, InRefValue(tx)| {
                    if tx.lt != lt {
                        return Ok(true)
                    }
                    println!("we got lt {} (wanted {}), next lt we want is {}", tx.lt, lt, tx.prev_trans_lt);
                    lt = tx.prev_trans_lt;
                    transactions.push(tx);
                    Ok(if let Some(count) = count { transactions.len() < count } else { true })
                })?;
                if !complete {
                    break
                }
            } else {
                break
            }
        }
        Ok((block_ids, transactions))
    }

    pub async fn get_transactions(&self, req: GetTransactions) -> Result<TransactionList> {
        let (ids, transactions) = self.search_transactions(&req.account, req.lt, Some(req.count as usize)).await?;
        let mut boc = Vec::new();
        BagOfCells::with_roots(transactions.iter().map(|tx| tx.serialize()).collect::<Result<Vec<_>>>()?.as_slice()).write_to(&mut boc, false)?;
        Ok(TransactionList {
            ids,
            transactions: boc,
        })
    }

    pub async fn lookup_block(&self, req: LookupBlock) -> Result<BlockHeader> {
        let shard = ShardIdent::with_tagged_prefix(req.id.workchain, req.id.shard)?;
        let block = if let Some(utime) = req.utime {
            self.engine.storage().block_storage().search_block_by_utime(&shard, utime)?
        } else if let Some(lt) = req.lt {
            self.engine.storage().block_storage().search_block_by_lt(&shard, lt)?
        } else if req.seqno.is_some() {
            self.engine.storage().block_storage().search_block_by_seqno(&shard, req.id.seqno)?
        } else {
            return Err(anyhow!("exactly one of utime, lt or seqno must be specified"))
        }.ok_or(anyhow!("no such block in db"))?;
        
        let block_root = ton_types::deserialize_tree_of_cells(&mut block.as_ref())?;
        let merkle_proof = Self::make_block_proof(
            block_root.clone(),
            req.with_state_update.is_some(),
            req.with_value_flow.is_some(),
            req.with_extra.is_some(),
            false,
            false,
            None,
        )?;

        let seqno = Block::construct_from_cell(block_root.clone())?.read_info()?.seq_no();

        Ok(BlockHeader {
            id: BlockIdExt {
                workchain: shard.workchain_id(),
                shard: shard.shard_prefix_with_tag(),
                seqno,
                root_hash: Int256(*block_root.repr_hash().as_array()),
                file_hash: Int256(*UInt256::calc_file_hash(&block).as_array()),
            },
            mode: (),
            header_proof: merkle_proof.write_to_bytes()?,
            with_state_update: req.with_state_update,
            with_value_flow: req.with_value_flow,
            with_extra: req.with_extra,
            with_shard_hashes: req.with_shard_hashes,
            with_prev_blk_signatures: req.with_prev_blk_signatures,
        })  
    }

    fn update_config_params(params: &mut Vec<i32>, with_validator_set: bool, with_special_smc: bool, with_workchain_info: bool, with_capabilities: bool) -> Result<()> {
        if with_validator_set {
            params.push(34);
        }
        if with_special_smc {
            params.push(31);
        }
        if with_workchain_info {
            params.push(12);
        }
        if with_capabilities {
            params.push(8);
        }
        Ok(())
    }

    fn touch_config(config: &ConfigParams, with_params: Option<&[i32]>) -> Result<()> {
        fn preload_param(config: &ConfigParams, param: i32) -> Result<()> {
            let key = SliceData::load_builder(param.write_to_new_cell()?)?;
            let slice = config.config_params.get(key)?.ok_or(anyhow!("no such param {param}"))?; 
            let cell = slice.reference_opt(0).ok_or(anyhow!("no such reference in param {param}"))?;
            cell.preload_with_depth_hint::<32>()?;
            Ok(())
        }
        if let Some(with_params) = with_params {
            for i in with_params {
                preload_param(config, *i)?;
            }
        } else {
            config.config_params.data().ok_or(anyhow!("no config"))?.preload_with_depth_hint::<128>()?;
        }
        Ok(())
    }

    fn make_state_proof(state_root: Cell, params: Option<&[i32]>, with_accounts: bool, with_prev_blocks: bool, with_shard_hashes: bool) -> Result<MerkleProof> {
        let usage_tree = UsageTree::with_root(state_root.clone());
        let state = ShardStateUnsplit::construct_from_cell(usage_tree.root_cell())?;
        if with_accounts {
            state.read_accounts()?;
        }
        let custom = state.read_custom()?.ok_or(anyhow!("no custom data in state"))?;
        if with_prev_blocks {
            let mut counter = 0;
            let _ = custom.prev_blocks.iterate_ext(true, None, |_, _| { counter += 1; Ok(counter < 16) })?;
        }
        if with_shard_hashes {
            let _ = custom.shards.root().ok_or(anyhow!("no shards in state"))?.preload_with_depth_hint::<32>()?;
        }
        Self::touch_config(custom.config(), params)?;
        Ok(MerkleProof::create_by_usage_tree(&state_root, usage_tree)?)
    }

    pub async fn get_config_params(&self, mut req: GetConfigParams) -> Result<ConfigInfo> {
        if req.id.workchain != -1 {
            return Err(anyhow!("requested block is not in masterchain"))
        }
        Self::update_config_params(&mut req.param_list, req.with_validator_set.is_some(), req.with_special_smc.is_some(), req.with_workchain_info.is_some(), req.with_capabilities.is_some())?;
        let block_root = self.load_block_by_tonlabs_id(&req.id.as_tonlabs()?).await?.ok_or(anyhow!("no such block in db"))?;
        let block = Block::construct_from_cell(block_root.clone())?;
        if req.extract_from_key_block.is_some() {
            let key_seqno = block.read_info()?.prev_key_block_seqno();
            let key_id = self.search_mc_block_by_seqno(key_seqno).await?.ok_or(anyhow!("no such key block in masterchain state"))?;
            let key_root = self.load_block_by_tonlabs_id(&key_id).await?.ok_or(anyhow!("no such key block in db"))?;
            Ok(ConfigInfo {
                mode: (),
                id: key_id.as_liteapi(),
                state_proof: Vec::new(),
                config_proof: Self::make_block_proof(
                    key_root,
                    false,
                    false,
                    true,
                    req.with_shard_hashes.is_some(),
                    true,
                    Some(req.param_list.as_slice()),
                )?.write_to_bytes()?,
                with_state_root: req.with_state_root,
                with_libraries: req.with_libraries,
                with_state_extra_root: req.with_state_extra_root,
                with_shard_hashes: req.with_shard_hashes,
                with_accounts_root: req.with_accounts_root,
                with_prev_blocks: req.with_prev_blocks,
                extract_from_key_block: req.extract_from_key_block,
                with_validator_set: req.with_validator_set,
                with_special_smc: req.with_special_smc,
                with_workchain_info: req.with_workchain_info,
                with_capabilities: req.with_capabilities,
            })
        } else {
            let state_root = self.load_state(&req.id).await?.root_cell().clone();
            Ok(ConfigInfo {
                mode: (),
                id: req.id,
                state_proof: Self::make_block_proof(block_root, true, false, false, false, false, None)?.write_to_bytes()?,
                config_proof: Self::make_state_proof(
                    state_root,
                    Some(req.param_list.as_slice()),
                    req.with_accounts_root.is_some(), 
                    req.with_prev_blocks.is_some(), 
                    req.with_shard_hashes.is_some()
                )?.write_to_bytes()?,
                with_state_root: req.with_state_root,
                with_libraries: req.with_libraries,
                with_state_extra_root: req.with_state_extra_root,
                with_shard_hashes: req.with_shard_hashes,
                with_accounts_root: req.with_accounts_root,
                with_prev_blocks: req.with_accounts_root,
                extract_from_key_block: req.extract_from_key_block,
                with_validator_set: req.with_validator_set,
                with_special_smc: req.with_special_smc,
                with_workchain_info: req.with_workchain_info,
                with_capabilities: req.with_capabilities,
            })
        }
        
    }

    pub async fn get_config_all(&self, req: GetConfigAll) -> Result<ConfigInfo> {
        if req.id.workchain != -1 {
            return Err(anyhow!("requested block is not in masterchain"))
        }
        let block_root = self.load_block_by_tonlabs_id(&req.id.as_tonlabs()?).await?.ok_or(anyhow!("no such block in db"))?;
        let block = Block::construct_from_cell(block_root.clone())?;
        if req.extract_from_key_block.is_some() {
            let key_seqno = block.read_info()?.prev_key_block_seqno();
            let key_id = self.search_mc_block_by_seqno(key_seqno).await?.ok_or(anyhow!("no such key block in masterchain state"))?;
            let key_root = self.load_block_by_tonlabs_id(&key_id).await?.ok_or(anyhow!("no such key block in db"))?;
            Ok(ConfigInfo {
                mode: (),
                id: key_id.as_liteapi(),
                state_proof: Vec::new(),
                config_proof: Self::make_block_proof(
                    key_root,
                    false,
                    false,
                    true,
                    req.with_shard_hashes.is_some(),
                    true,
                    None,
                )?.write_to_bytes()?,
                with_state_root: req.with_state_root,
                with_libraries: req.with_libraries,
                with_state_extra_root: req.with_state_extra_root,
                with_shard_hashes: req.with_shard_hashes,
                with_accounts_root: req.with_accounts_root,
                with_prev_blocks: req.with_prev_blocks,
                extract_from_key_block: req.extract_from_key_block,
                with_validator_set: req.with_validator_set,
                with_special_smc: req.with_special_smc,
                with_workchain_info: req.with_workchain_info,
                with_capabilities: req.with_capabilities,
            })
        } else {
            let state_root = self.load_state(&req.id).await?.root_cell().clone();
            Ok(ConfigInfo {
                mode: (),
                id: req.id,
                state_proof: Self::make_block_proof(block_root, true, false, false, false, false, None)?.write_to_bytes()?,
                config_proof: Self::make_state_proof(
                    state_root,
                    None,
                    req.with_accounts_root.is_some(), 
                    req.with_prev_blocks.is_some(), 
                    req.with_shard_hashes.is_some()
                )?.write_to_bytes()?,
                with_state_root: req.with_state_root,
                with_libraries: req.with_libraries,
                with_state_extra_root: req.with_state_extra_root,
                with_shard_hashes: req.with_shard_hashes,
                with_accounts_root: req.with_accounts_root,
                with_prev_blocks: req.with_accounts_root,
                extract_from_key_block: req.extract_from_key_block,
                with_validator_set: req.with_validator_set,
                with_special_smc: req.with_special_smc,
                with_workchain_info: req.with_workchain_info,
                with_capabilities: req.with_capabilities,
            })
        }
    }

    fn construct_proof<T, F>(root: &Cell, mut visitor: F) -> Result<MerkleProof> where T: Deserializable, F: FnMut(&T) -> Result<()> {
        let usage_tree = UsageTree::with_root(root.clone());
        visitor(&T::construct_from_cell(usage_tree.root_cell())?)?;
        Ok(MerkleProof::create_by_usage_tree(root, usage_tree)?)
    }

    fn construct_signatures(block_root: Cell, proof: &BlockProofStuff) -> Result<SignatureSet> {
        let block = Block::construct_from_cell(block_root)?;
        let info = block.read_info()?;
        let mut signatures = SignatureSet {
            validator_set_hash: info.gen_validator_list_hash_short(),
            catchain_seqno: info.gen_catchain_seqno(),
            signatures: Vec::new(),
        };
        proof.proof().clone().signatures.ok_or(anyhow!("no signatures"))?.pure_signatures.signatures().iterate_slices(|ref mut _key, ref mut slice| {
            let sign = CryptoSignaturePair::construct_from(slice)?;
            signatures.signatures.push(Signature {
                signature: sign.sign.signature().to_bytes().to_vec(),
                node_id_short: Int256(*sign.node_id_short.as_array()),
            });
            Ok(true)
        })?;
        Ok(signatures)
    }

    pub async fn get_block_proof(&self, req: GetBlockProof) -> Result<PartialBlockProof> {
        if req.known_block.workchain != -1 || req.known_block.shard != SHARD_FULL {
            return Err(anyhow!("known_block must be in masterchain"))
        }
        let target_block = if let Some(target_block) = &req.target_block {
            if target_block.workchain != -1 || target_block.shard != SHARD_FULL {
                return Err(anyhow!("target_block must be in masterchain"))
            }
            target_block.as_tonlabs()?
        } else if req.allow_weak_target.is_some() {
            self.engine.load_last_applied_mc_block_id()?
        } else {
            self.engine.load_shards_client_mc_block_id()?
        };
        let base_block = if req.target_block.is_some() && req.base_block_from_request.is_some() || req.target_block.is_none() && req.allow_weak_target.is_none() {
            if req.known_block.seqno > target_block.seq_no {
                req.known_block.as_tonlabs()?
            } else {
                target_block.clone()
            }
        } else {
            self.engine.load_last_applied_mc_block_id()?
        };
        let known_block = req.known_block.as_tonlabs()?;
        if target_block.seq_no < known_block.seq_no {
            let known_block_root = self.load_block_by_tonlabs_id(&known_block).await?.ok_or(anyhow!("no such known_block"))?;
            let known_state = self.load_state(&req.known_block).await?;
            let known_state_root = known_state.root_cell();
            let mut to_key_block = false;
            let proof = Self::construct_proof::<ShardStateUnsplit, _>(known_state_root, |state| {
                let target_ref = state.read_custom()?.ok_or(anyhow!("no custom in mc state"))?.prev_blocks.get(&target_block.seq_no)?.ok_or(anyhow!("no such target block"))?;
                to_key_block = target_ref.key;
                Ok(())
            })?;
            Ok(PartialBlockProof {
                complete: true,
                from: req.known_block.clone(),
                to: target_block.as_liteapi(),
                steps: [
                    BlockLink::BlockLinkBack {
                        to_key_block,
                        from: req.known_block,
                        to: target_block.as_liteapi(),
                        dest_proof: Vec::new(),
                        proof: proof.write_to_bytes()?,
                        state_proof: Self::make_block_proof(known_block_root, true, false, false, false, false, None)?.write_to_bytes()?,
                    }
                ].to_vec(),
            })
        } else if target_block.seq_no > known_block.seq_no {
            let mut steps = Vec::new();
            let base_state = self.load_state(&base_block.as_liteapi()).await?;
            let extra = base_state.shard_state_extra()?;
            let prev_blocks = &extra.prev_blocks;
            let mut current = known_block;
            let mut current_root = self.load_block_by_tonlabs_id(&current).await?.ok_or(anyhow!("no such known_block"))?;
            loop {
                if let Some(next_key_block) = prev_blocks.get_next_key_block(current.seq_no + 1)? {
                    if next_key_block.seq_no <= target_block.seq_no {
                        let next = next_key_block.clone().master_block_id().1;
                        let next_root = self.load_block_by_tonlabs_id(&next).await?.ok_or(anyhow!("no such block in db"))?;
                        let proof = self.load_block_proof(&next).await?;
                        steps.push(BlockLink::BlockLinkForward {
                            to_key_block: true,
                            from: current.as_liteapi(),
                            to: next.as_liteapi(),
                            dest_proof: Self::make_block_proof(next_root.clone(), false, false, false, false, false, None)?.write_to_bytes()?,
                            config_proof: Self::make_block_proof(current_root, false, false, true, false, true, Some(&[28, 34]))?.write_to_bytes()?,
                            signatures: Self::construct_signatures(next_root.clone(), &proof)?,
                        });
                        if next_key_block.seq_no == target_block.seq_no {
                            break
                        }
                        current = next;
                        current_root = next_root;
                        continue
                    }
                }
                let next_root = self.load_block_by_tonlabs_id(&target_block).await?.ok_or(anyhow!("no such block in db"))?;
                let proof = self.load_block_proof(&target_block).await?;
                steps.push(BlockLink::BlockLinkForward {
                    to_key_block: false,
                    from: current.as_liteapi(),
                    to: target_block.as_liteapi(),
                    dest_proof: Self::make_block_proof(next_root.clone(), false, false, false, false, false, None)?.write_to_bytes()?,
                    config_proof: Self::make_block_proof(current_root, false, false, true, false, true, Some(&[28, 34]))?.write_to_bytes()?,
                    signatures: Self::construct_signatures(next_root, &proof)?,
                });
                break
            }
            Ok(PartialBlockProof {
                complete: true,
                from: req.known_block,
                to: target_block.as_liteapi(),
                steps,
            })
        } else {
            Ok(PartialBlockProof {
                complete: true,
                from: req.known_block,
                to: target_block.as_liteapi(),
                steps: Vec::new(),
            })
        }
    }

    pub async fn run_smc_method(&self, req: RunSmcMethod) -> Result<RunMethodResult> {
        let reference_id = if req.id.seqno != 0xffff_ffff {
            req.id.as_tonlabs()?
        } else {
            self.engine.load_shards_client_mc_block_id()?
        };
        let shard_id = self.get_block_for_account(&req.account, &reference_id).await?;
        let state_stuff = self.engine.load_state(&shard_id).await?;
        let usage_tree_p2 = UsageTree::with_root(state_stuff.root_cell().clone());
        let state = ShardStateUnsplit::construct_from_cell(usage_tree_p2.root_cell())?;
        let account = state
            .read_accounts()?
            .account(&req.account.id())?
            .ok_or(anyhow!("no such account"))?
            .read_account()?;
        let state_init = if let ton_block::AccountState::AccountActive { state_init } = account.state().ok_or(anyhow!("no state for account"))? {
            state_init
        } else {
            return Err(anyhow!("account is not active"))
        };
        let code = serialize_toc(state_init.code().ok_or(anyhow!("no code in state"))?)?;
        let data = serialize_toc(state_init.data().ok_or(anyhow!("no data in state"))?)?;
        let libs = state_stuff.state().libraries().write_to_bytes()?;
        let mc_state = self.engine.load_state(&reference_id).await?;
        let config = mc_state.shard_state_extra()?.config.config_params.write_to_bytes()?;
        let balance = account.balance().and_then(|x| x.grams.as_u64()).unwrap_or(0);
        let empty_stack = vec![181, 238, 156, 114, 65, 1, 1, 1, 0, 5, 0, 0, 6, 0, 0, 0, 208, 9, 95, 69];
        let stack = if req.params.len() > 0 {
            &req.params
        } else {
            &empty_stack
        };
        let result = EmulatorBuilder::new(&code, &data)
            .with_gas_limit(1000000)
            .with_c7(&req.account, now(), balance, &[0u8; 32], &config)
            .with_libs(&libs)
            .run_get_method(req.method_id as i32, stack);
        let result = match result {
            crate::tvm::TvmEmulatorRunGetMethodResult::Error(e) => return Err(anyhow!("tvm error: {:?}", e)),
            crate::tvm::TvmEmulatorRunGetMethodResult::Success(r) => r,
        };
        Ok(RunMethodResult {
            mode: (),
            id: reference_id.as_liteapi(),
            shardblk: shard_id.as_liteapi(),
            shard_proof: None,
            proof: None,
            state_proof: None,
            init_c7: None,
            lib_extras: None,
            exit_code: result.vm_exit_code,
            result: Some(base64::engine::general_purpose::STANDARD.decode(result.stack)?),
        })
    }

    pub async fn send_message(&self, req: SendMessage) -> Result<SendMsgStatus> {
        let message = Message::construct_from_bytes(req.body.as_slice())?;
        let message_info = if let CommonMsgInfo::ExtInMsgInfo(info) = message.header() {
            info
        } else {
            return Err(anyhow!("message is not inbound external"))
        };
        let reference_id = self.engine.load_shards_client_mc_block_id()?;
        let (wc, account_id) = message_info.dst.extract_std_address(true)?;
        let acc_id = AccountId {
            workchain: wc,
            id: Int256(account_id.get_bytestring(0).try_into().unwrap()),
        };
        let shard_id = self.get_block_for_account(&acc_id, &reference_id).await?;
        let state_stuff = self.engine.load_state(&shard_id).await?;
        let acc = state_stuff.state().read_accounts()?.account(&account_id)?.ok_or(anyhow!("no such account"))?.read_account()?;
        let state_init = if let ton_block::AccountState::AccountActive { state_init } = acc.state().ok_or(anyhow!("no state for account"))? {
            state_init
        } else {
            return Err(anyhow!("account is not active"))
        };
        let code = serialize_toc(state_init.code().ok_or(anyhow!("no code in state"))?)?;
        let data = serialize_toc(state_init.data().ok_or(anyhow!("no data in state"))?)?;
        let libs = state_stuff.state().libraries().write_to_bytes()?;
        let mc_state = self.engine.load_state(&reference_id).await?;
        let config = mc_state.shard_state_extra()?.config.config_params.write_to_bytes()?;
        let balance = acc.balance().and_then(|x| x.grams.as_u64()).unwrap_or(0);
        let result = EmulatorBuilder::new(&code, &data)
            .with_gas_limit(1000000)
            .with_c7(&acc_id, now(), balance, &[0u8; 32], &config)
            .with_libs(&libs)
            .run_external(&req.body);
        let result = match result {
            crate::tvm::TvmEmulatorSendExternalMessageResult::Error(e) => return Err(anyhow!("tvm error: {:?}", e)),
            crate::tvm::TvmEmulatorSendExternalMessageResult::Success(r) => r,
        };
        if !result.accepted {
            return Err(anyhow!("message was not accepted"))
        }
        self.engine.broadcast_external_message(-1, &req.body)?;
        Ok(SendMsgStatus { status: 1 })
    }

    pub async fn get_libraries(&self, req: GetLibraries) -> Result<LibraryResult> {
        let mut result = Vec::new();
        let state = self.load_state_by_tonlabs_id(&self.engine.load_last_applied_mc_block_id()?).await?;
        for hash in req.library_list {
            let library = state.state().libraries().get(&UInt256::from_slice(&hash.0))?.ok_or(anyhow!("no such library: {:?}", hash))?;
            result.push(LibraryEntry { hash, data: library.lib().write_to_bytes()? });
        }
        Ok(LibraryResult { result })
    }

    pub async fn wait_masterchain_seqno(&self, req: WaitMasterchainSeqno) -> Result<()> {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(req.timeout_ms as u64)) => Err(anyhow!("Timeout")),
            r = async {
                loop {
                    // load_last_applied_mc_block_id uses sync mutex, may be very slow
                    let last_seqno = self.engine.load_last_applied_mc_block_id()?.seq_no;
                    if req.seqno <= last_seqno {
                        return Ok(())
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            } => r
        }
    }

    #[tracing::instrument(skip(self), level = "debug", fields(prefix=req.wait_masterchain_seqno.is_some(), method=format!("{:?}", req.request).split('(').next().unwrap()), err(level = tracing::Level::DEBUG))]
    async fn call_impl(&self, req: WrappedRequest) -> Result<Response> {
        if let Some(wait_req) = req.wait_masterchain_seqno {
            self.wait_masterchain_seqno(wait_req).await?;
        }
        match req.request {
            Request::GetMasterchainInfo => Ok(Response::MasterchainInfo(
                self.get_masterchain_info().await?,
            )),
            Request::GetMasterchainInfoExt(req) => Ok(Response::MasterchainInfoExt(
                self.get_masterchain_info_ext(req).await?,
            )),
            Request::GetBlockHeader(req) => {
                Ok(Response::BlockHeader(self.get_block_header(req).await?))
            }
            Request::GetAllShardsInfo(req) => Ok(Response::AllShardsInfo(
                self.get_all_shards_info(req).await?,
            )),
            Request::ListBlockTransactions(req) => Ok(Response::BlockTransactions(
                self.list_block_transactions(req).await?,
            )),
            Request::GetAccountState(req) => Ok(Response::AccountState(
                self.get_account_state(req).await?,
            )),
            Request::GetBlock(req) => Ok(Response::BlockData(
                self.get_block(req).await?,
            )),
            Request::GetTransactions(req) => Ok(Response::TransactionList(
                self.get_transactions(req).await?,
            )),
            Request::LookupBlock(req) => Ok(Response::BlockHeader(
                self.lookup_block(req).await?,
            )),
            Request::GetConfigParams(req) => Ok(Response::ConfigInfo(
                self.get_config_params(req).await?,
            )),
            Request::GetConfigAll(req) => Ok(Response::ConfigInfo(
                self.get_config_all(req).await?,
            )),
            Request::GetBlockProof(req) => Ok(Response::PartialBlockProof(
                self.get_block_proof(req).await?,
            )),
            Request::RunSmcMethod(req) => Ok(Response::RunMethodResult(
                self.run_smc_method(req).await?,
            )),
            Request::SendMessage(req) => Ok(Response::SendMsgStatus(
                self.send_message(req).await?,
            )),
            Request::GetLibraries(req) => Ok(Response::LibraryResult(
                self.get_libraries(req).await?,
            )),
            _ => Err(anyhow!("unimplemented method: {:?}", req.request)),
        }
    }
}

impl Service<WrappedRequest> for LiteServer {
    type Response = Response;
    type Error = LiteError;
    type Future = BoxFuture<'static, Result<Response, LiteError>>;

    fn poll_ready(&mut self, _cx: &mut std::task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: WrappedRequest) -> Self::Future {
        let ls = self.clone();
        Box::pin(async move {
            ls.call_impl(req)
                .await
                .map_err(|e| LiteError::UnknownError(e.into()))
        })
    }
}

pub fn run(engine: Arc<Engine>, config: GlobalConfig) {
    let ls = LiteServer { engine, config };
    tokio::spawn(async move {
        // TODO: key from environment variables
        let key: [u8; 32] =
            hex::decode("f0971651aec4bb0d65ec3861c597687fda9c1e7d2ee8a93acb9a131aa9f3aee7")
                .unwrap()
                .try_into()
                .unwrap();
        let key = StaticSecret::from(key);

        // TODO: configurable layers, rate limiting by ip/adnl
        let service = ServiceBuilder::new()
            .buffer(100)
            .layer(UnwrapMessagesLayer)
            .layer(WrapErrorLayer)
            .service(ls);
        serve(&("0.0.0.0", 3333), key, Shared::new(service))
            .await
            .expect("liteserver error");
    });
}

trait BlockIdExtAsTonlabs {
    fn as_tonlabs(&self) -> Result<ton_block::BlockIdExt>;
}

trait BlockIdExtAsLiteapi {
    fn as_liteapi(&self) -> ton_liteapi::tl::common::BlockIdExt;
}

impl BlockIdExtAsTonlabs for ton_liteapi::tl::common::BlockIdExt {
    fn as_tonlabs(&self) -> Result<ton_block::BlockIdExt> {
        Ok(ton_block::BlockIdExt {
            shard_id: ShardIdent::with_tagged_prefix(self.workchain, self.shard)?,
            seq_no: self.seqno,
            root_hash: UInt256::from_slice(&self.root_hash.0),
            file_hash: UInt256::from_slice(&self.file_hash.0),
        })
    }
}

impl BlockIdExtAsLiteapi for ton_block::BlockIdExt {
    fn as_liteapi(&self) -> ton_liteapi::tl::common::BlockIdExt {
        ton_liteapi::tl::common::BlockIdExt {
            workchain: self.shard().workchain_id(),
            shard: self.shard().shard_prefix_with_tag(),
            seqno: self.seq_no,
            root_hash: Int256(*self.root_hash.as_array()),
            file_hash: Int256(*self.file_hash.as_array()),
        }
    }
}

trait AccountIdAsTonlabs {
    fn prefix_full(&self) -> Result<ton_block::AccountIdPrefixFull>;
    fn std(&self) -> ton_block::MsgAddrStd;
    fn raw_id(&self) -> UInt256;
    fn id(&self) -> ton_types::AccountId;
}

impl AccountIdAsTonlabs for ton_liteapi::tl::common::AccountId {
    fn std(&self) -> ton_block::MsgAddrStd {
        MsgAddrStd::with_address(None, self.workchain as i8, ton_types::AccountId::from_raw(self.id.0.to_vec(),256))
    }

    fn prefix_full(&self) -> Result<ton_block::AccountIdPrefixFull> {
        ton_block::AccountIdPrefixFull::prefix(&ton_block::MsgAddressInt::AddrStd(self.std()))
    }

    fn raw_id(&self) -> UInt256 {
        UInt256::from_slice(&self.id.0)
    }

    fn id(&self) -> ton_types::AccountId {
        ton_types::AccountId::from_raw(self.id.0.to_vec(), 256)
    }
}