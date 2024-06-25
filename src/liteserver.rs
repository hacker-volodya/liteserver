use std::{sync::Arc, task::Poll, time::Duration};

use anyhow::{anyhow, Result};
use broxus_util::now;
use futures_util::future::BoxFuture;
use ton_block::{AccountIdPrefixFull, Block, Deserializable, GetRepresentationHash, HashmapAugType, InRefValue, MerkleProof, MsgAddrStd, Serializable, ShardIdent, ShardStateUnsplit, Transaction, TraverseNextStep};
use ton_indexer::{utils::ShardStateStuff, Engine, GlobalConfig};
use ton_liteapi::{
    layers::{UnwrapMessagesLayer, WrapErrorLayer},
    server::serve,
    tl::{
        common::{AccountId, BlockIdExt, Int256, ZeroStateIdExt},
        request::{
            GetAccountState, GetAllShardsInfo, GetBlock, GetBlockHeader, GetConfigAll, GetConfigParams, GetMasterchainInfoExt, GetTransactions, ListBlockTransactions, LookupBlock, Request, WaitMasterchainSeqno, WrappedRequest
        },
        response::{AccountState, AllShardsInfo, BlockData, BlockHeader, BlockTransactions, ConfigInfo, MasterchainInfo, MasterchainInfoExt, Response, TransactionId, TransactionList},
    },
    types::LiteError,
};
use ton_types::{serialize_toc, BagOfCells, Cell, UInt256, UsageTree};
use tower::{make::Shared, Service, ServiceBuilder};
use x25519_dalek::StaticSecret;

use crate::utils::HashmapAugIterator;

#[derive(Clone)]
pub struct LiteServer {
    engine: Arc<Engine>,
    config: GlobalConfig,
}

impl LiteServer {
    pub async fn get_masterchain_info(&self) -> Result<MasterchainInfo> {
        let last = self.engine.load_last_applied_mc_block_id()?;
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
        let last = self.engine.load_last_applied_mc_block_id()?;
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
            let _mc_block_extra = block.read_extra()?.read_custom()?;
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

    async fn load_state(&self, block_id: &BlockIdExt) -> Result<Arc<ShardStateStuff>> {
        self.engine.load_state(&block_id.as_tonlabs()?).await
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

    pub async fn get_all_shards_info(&self, request: GetAllShardsInfo) -> Result<AllShardsInfo> {
        let block_root = self.load_block(&request.id).await?.ok_or(anyhow!("no such block in db"))?;

        // proof1: from block to shardstate update
        let proof1 = Self::make_block_proof(block_root, true, false, false)?;

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

    pub async fn get_account_state(&self, req: GetAccountState) -> Result<AccountState> {
        let reference_id = if req.id.seqno != 0xffff_ffff {
            req.id.as_tonlabs()?
        } else {
            self.engine.load_last_applied_mc_block_id()?
        };
        let shard_id = self.get_block_for_account(&req.account, &reference_id).await?;
        let shard_block = self.load_block_by_tonlabs_id(&shard_id).await?.ok_or(anyhow!("no such block in db"))?;
        let state_stuff = self.engine.load_state(&shard_id).await?;
        let usage_tree_p2 = UsageTree::with_root(state_stuff.root_cell().clone());
        let state = ShardStateUnsplit::construct_from_cell(usage_tree_p2.root_cell())?;
        let account = state
            .read_accounts()?
            .account(&req.account.id())?
            .ok_or(anyhow!("no such account"))?;
        let proof1 = Self::make_block_proof(shard_block, true, false, false)?;
        let proof2 = MerkleProof::create_by_usage_tree(state_stuff.root_cell(), usage_tree_p2)?;
        let mut proof = Vec::new();
        BagOfCells::with_roots(&[proof1.serialize()?, proof2.serialize()?]).write_to(&mut proof, false)?;
        Ok(AccountState { id: reference_id.as_liteapi(), shardblk: shard_id.as_liteapi(), shard_proof: Vec::new(), proof, state: serialize_toc(&account.account_cell())? })
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

    fn make_state_proof(state_root: Cell, with_accounts: bool, with_prev_blocks: bool) -> Result<MerkleProof> {
        let usage_tree = UsageTree::with_root(state_root.clone());
        let state = ShardStateUnsplit::construct_from_cell(usage_tree.root_cell())?;
        if with_accounts {
            state.read_accounts()?;
        }
        let custom = state.read_custom()?.ok_or(anyhow!("no custom data in state"))?;
        let mut counter = 0;
        let _ = custom.prev_blocks.iterate_ext(true, None, |_, _| { counter += 1; Ok(counter < 16) })?;
        Ok(MerkleProof::create_by_usage_tree(&state_root, usage_tree)?)
    }

    pub async fn get_config_params(&self, req: GetConfigParams) -> Result<ConfigInfo> {
        if req.id.workchain != -1 {
            return Err(anyhow!("requested block is not in masterchain"))
        }
        let block_root = self.load_block_by_tonlabs_id(&req.id.as_tonlabs()?).await?.ok_or(anyhow!("no such block in db"))?;
        let block = Block::construct_from_cell(block_root.clone())?;
        if req.extract_from_key_block.is_some() {
            let key_seqno = block.read_info()?.prev_key_block_seqno();
            let key_id = self.search_mc_block_by_seqno(key_seqno).await?.ok_or(anyhow!("no such key block in masterchain state"))?;
            let key_root = self.load_block_by_tonlabs_id(&key_id).await?.ok_or(anyhow!("no such key block in db"))?;
            let usage_tree = UsageTree::with_root(key_root.clone());
            let key_block = Block::construct_from_cell(usage_tree.root_cell())?;
            let config = key_block.read_extra()?.read_custom()?.ok_or(anyhow!("no custom data in key block"))?.config().ok_or(anyhow!("no config in key block"))?;
            Ok(ConfigInfo {
                mode: (),
                id: key_id.as_liteapi(),
                state_proof: Vec::new(),
                config_proof: todo!(),
                with_state_root: req.with_state_root,
                with_libraries: req.with_libraries,
                with_state_extra_root: req.with_state_extra_root,
                with_shard_hashes: req.with_shard_hashes,
                with_accounts_root: req.with_accounts_root,
                with_prev_blocks: req.with_prev_blocks,
                extract_from_key_block: req.extract_from_key_block,
            })
        } else {
            let state_root = self.load_state(&req.id).await?.root_cell().clone();
            let usage_tree = UsageTree::with_root(state_root.clone());
            let state = ShardStateUnsplit::construct_from_cell(usage_tree.root_cell())?;
            let config = state.read_custom()?.ok_or(anyhow!("no custom data in state"))?.config();
            Ok(ConfigInfo {
                mode: (),
                id: req.id,
                state_proof: Self::make_block_proof(block_root, true, false, false)?.write_to_bytes()?,
                config_proof: Self::make_state_proof(state_root, req.with_accounts_root.is_some(), req.with_prev_blocks.is_some())?.write_to_bytes()?,
                with_state_root: req.with_state_root,
                with_libraries: req.with_libraries,
                with_state_extra_root: req.with_state_extra_root,
                with_shard_hashes: req.with_shard_hashes,
                with_accounts_root: req.with_accounts_root,
                with_prev_blocks: req.with_accounts_root,
                extract_from_key_block: req.extract_from_key_block,
            })
        }
        
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