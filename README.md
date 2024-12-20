# TON Liteserver in Rust

> :warning: **WARNING:** experimental software, not yet ready for production use

## Usage

### Docker CLI
```bash
# use mainnet config
docker run -d -it -P ghcr.io/hacker-volodya/liteserver

# use testnet config
docker run -d -it -P ghcr.io/hacker-volodya/liteserver --testnet

# use custom config from url
docker run -d -it -P ghcr.io/hacker-volodya/liteserver --global-config-url https://example.com/global.config.json

# use local config
docker run -d -it -P -v $PWD/global.config.json:/global.config.json ghcr.io/hacker-volodya/liteserver --global-config-path /global.config.json

# specify custom node config (examples below)
docker run -d -it -P -v $PWD/config.yml:/config.yml ghcr.io/hacker-volodya/liteserver --config /config.yml
```

### Docker Compose
```yaml
version: '2'
services:
  liteserver:
    image: ghcr.io/hacker-volodya/liteserver
    ports:
      - 30303:30303/udp  # nodes p2p (host port and container port must be equal, container port is specified in config.yml)
      - 3000:3000/tcp  # web interface
      - 3333:3333/tcp  # liteapi
    restart: always
```

## Method support
| Status | Method | Description |
|--------|--------|-------------|
| | **impemented**
| ✅ | WaitMasterchainSeqno
| ✅ | GetMasterchainInfo
| ✅ | GetMasterchainInfoExt
| ✅ | GetBlockHeader
| ✅ | GetAllShardsInfo
| ✅ | ListBlockTransactions
| ✅ | GetBlock
| ✅ | GetAccountState
| ✅ | GetTransactions
| ✅ | LookupBlock
| ✅ | GetConfigAll
| ✅ | GetMasterchainInfoExt
| ✅ | GetConfigParams
| ✅ | GetBlockProof
| ✅ | SendMessage
| ✅ | RunSmcMethod | no proofs
| ✅ | GetLibraries
| | **high priority** (toncenter)
| ⚠️ | GetShardBlockProof | block proof research in progress
| | **medium priority**
| 🔜 | GetOneTransaction
| 🔜 | GetShardInfo
| 🔜 | GetAccountStatePrunned
| 🔜 | ListBlockTransactionsExt
| 🔜 | GetLibrariesWithProof
| ⚠️ | LookupBlockWithProof | block proof research in progress
| | **low priority**
| 🔜 | GetTime
| 🔜 | GetVersion
| 🔜 | GetValidatorStats
| 🔜 | GetState
| 🔜 | GetOutMsgQueueSizes
| 🔜 | GetValidatorGroups
| 🔜 | GetCandidate

## Node config example
```yaml
indexer:
  ip_address: 0.0.0.0:30303
  rocks_db_path: /data/rocksdb
  file_db_path: /data/file
  state_gc_options: null
  blocks_gc_options:
    # - `before_previous_key_block` - on each new key block delete all blocks before the previous one
    # - `before_previous_persistent_state` - on each new key block delete all blocks before the previous key block with persistent state
    kind: before_previous_key_block
    enable_for_sync: true  # Whether to enable blocks GC during sync.
    max_blocks_per_batch: 100000  # Max `WriteBatch` entries before apply
  shard_state_cache_options:
    ttl_sec: 120  # LRU cache item duration.
  db_options:
    rocksdb_lru_capacity: 1 GB
    cells_cache_size: 2 GB
    low_thread_pool_size: 8
    high_thread_pool_size: 8
    max_subcompactions: 8
  archive_options:
    gc_interval:
      # - `manual` - Do not perform archives GC
      # - `persistent_states` - Archives GC triggers on each persistent state
      type: persistent_states
      offset_sec: 300  # Remove archives after this interval after the new persistent state
  sync_options:
    old_blocks_policy:  # Whether to sync very old blocks
      type: ignore
      # type: sync
      # from_seqno: 100
    parallel_archive_downloads: 16
    save_to_disk_threshold: 1073741824
    max_block_applier_depth: 32
    force_use_get_next_block: false  # Ignore archives
  persistent_state_options:
    prepare_persistent_states: false
    persistent_state_parallelism: 1
    remove_old_states: true
  adnl_options:
    query_min_timeout_ms: 500  # Minimal ADNL query timeout. Will override the used timeout if it is less.
    query_default_timeout_ms: 5000  # Default ADNL query timeout. Will be used if no timeout is specified.
    transfer_timeout_sec: 3  # ADNL multipart transfer timeout. It will drop the transfer if it is not completed within this timeout.
    clock_tolerance_sec: 60  # Permissible time difference between remote and local clocks.
    channel_reset_timeout_sec: 30  # Drop channels which had no response for this amount of time.
    address_list_timeout_sec: 1000  # How much time address lists from packets should be valid.
    packet_history_enabled: false  # Whether to add additional duplicated packets check.
    packet_signature_required: true  # Whether handshake packets signature is mandatory.
    force_use_priority_channels: false  # Whether to use priority channels for queries.
    use_loopback_for_neighbours: false  # Whether to use loopback ip to communicate with nodes on the same ip
    version: null  # ADNL protocol version
  rldp_options:
    max_answer_size: 10485760  # Max allowed RLDP answer size in bytes. Query will be rejected if answer is bigger.
    max_peer_queries: 16  # Max parallel RLDP queries per peer.
    query_min_timeout_ms: 500  # Min RLDP query timeout.
    query_max_timeout_ms: 10000  # Max RLDP query timeout
    query_wave_len: 10  # Number of FEC messages to send in group. There will be a short delay between them.
    query_wave_interval_ms: 10  # Interval between FEC broadcast waves.
    force_compression: false  # Whether requests will be compressed.
  dht_options:
    value_ttl_sec: 3600  # Default stored value timeout used for [`Node::store_overlay_node`] and [`Node::store_address`]
    query_timeout_ms: 1000  # ADNL query timeout
    default_value_batch_len: 5  # Amount of DHT peers, used for values search
    bad_peer_threshold: 5  # Max peer penalty points. On each unsuccessful query every peer gains 2 points, and then they are reduced by one on each good action.
    max_allowed_k: 20  # Max allowed `k` value for DHT `FindValue` query.
    max_key_name_len: 127  # Max allowed key name length (in bytes). See [`everscale_network::proto::dht::Key`]
    max_key_index: 15  # Max allowed key index
    storage_gc_interval_ms: 10000  # Storage GC interval. Will remove all outdated entries
  overlay_shard_options:
    max_neighbours: 200  # More persistent list of peers. Used to distribute broadcasts.
    max_broadcast_log: 1000  # Max simultaneous broadcasts.
    broadcast_gc_interval_ms: 1000  # Broadcasts GC interval. Will leave at most `max_broadcast_log` each iteration.
    overlay_peers_timeout_ms: 60000  # Neighbours or random peers update interval.
    max_ordinary_broadcast_len: 768  # Packets with length bigger than this will be sent using FEC broadcast.
    broadcast_target_count: 5  # Max number of peers to distribute broadcast to.
    secondary_broadcast_target_count: 3  # Max number of peers to redistribute ordinary broadcast to.
    secondary_fec_broadcast_target_count: 3  # Max number of peers to redistribute FEC broadcast to.
    fec_broadcast_wave_len: 20  # Number of FEC messages to send in group. There will be a short delay between them.
    fec_broadcast_wave_interval_ms: 10  # Interval between FEC broadcast waves.
    broadcast_timeout_sec: 60  # Overlay broadcast timeout. It will be forcefully dropped if not received in this time.
    force_compression: false  # Whether requests will be compressed.
  neighbours_options:
    max_neighbours: 16
    reloading_min_interval_sec: 10
    reloading_max_interval_sec: 30
    ping_interval_ms: 500
    search_interval_ms: 1000
    ping_min_timeout_ms: 10
    ping_max_timeout_ms: 1000
    default_rldp_roundtrip_ms: 2000
    max_ping_tasks: 6
    max_exchange_tasks: 6
```
