use crate::config::BlockAssemblerConfig;
use crate::error::Error;
use ckb_core::block::Block;
use ckb_core::extras::EpochExt;
use ckb_core::header::Header;
use ckb_core::script::Script;
use ckb_core::service::{Request, DEFAULT_CHANNEL_SIZE, SIGNAL_CHANNEL_SIZE};
use ckb_core::transaction::{
    Capacity, CellInput, CellOutput, OutPoint, ProposalShortId, Transaction, TransactionBuilder,
};
use ckb_core::uncle::UncleBlock;
use ckb_core::{Bytes, Cycle, Version};
use ckb_notify::NotifyController;
use ckb_shared::{shared::Shared, tx_pool::PoolEntry};
use ckb_store::ChainStore;
use ckb_traits::ChainProvider;
use ckb_util::Mutex;
use crossbeam_channel::{self, select, Receiver, Sender};
use failure::Error as FailureError;
use faketime::unix_time_as_millis;
use fnv::FnvHashMap;
use fnv::FnvHashSet;
use jsonrpc_types::{
    BlockTemplate, CellbaseTemplate, JsonBytes, TransactionTemplate, UncleTemplate,
};
use log::error;
use lru_cache::LruCache;
use numext_fixed_hash::H256;
use std::cmp;
use std::sync::{atomic::AtomicU64, atomic::AtomicUsize, atomic::Ordering, Arc};
use std::thread;
use stop_handler::{SignalSender, StopHandler};

const MAX_CANDIDATE_UNCLES: usize = 42;
type BlockTemplateParams = (Option<u64>, Option<u64>, Option<Version>);
type BlockTemplateResult = Result<BlockTemplate, FailureError>;
const BLOCK_ASSEMBLER_SUBSCRIBER: &str = "block_assembler";
const BLOCK_TEMPLATE_TIMEOUT: u64 = 3000;
const TEMPLATE_CACHE_SIZE: usize = 10;

struct TemplateCache {
    pub time: u64,
    pub uncles_updated_at: u64,
    pub txs_updated_at: u64,
    pub template: BlockTemplate,
}

impl TemplateCache {
    fn is_outdate(
        &self,
        last_uncles_updated_at: u64,
        last_txs_updated_at: u64,
        current_time: u64,
        number: String,
    ) -> bool {
        last_uncles_updated_at != self.uncles_updated_at
            || (last_txs_updated_at != self.txs_updated_at
                && current_time.saturating_sub(self.time) > BLOCK_TEMPLATE_TIMEOUT)
            || number != self.template.number
    }
}

struct FeeCalculator<'a> {
    txs: &'a [PoolEntry],
    provider: &'a dyn ChainProvider,
    txs_map: FnvHashMap<&'a H256, usize>,
}

impl<'a> FeeCalculator<'a> {
    fn new(txs: &'a [PoolEntry], provider: &'a dyn ChainProvider) -> Self {
        let mut txs_map = FnvHashMap::with_capacity_and_hasher(txs.len(), Default::default());
        for (index, tx) in txs.iter().enumerate() {
            txs_map.insert(tx.transaction.hash(), index);
        }
        Self {
            txs,
            provider,
            txs_map,
        }
    }

    fn get_capacity(&self, out_point: &OutPoint) -> Option<Capacity> {
        let cell_out_point = out_point.cell.as_ref()?;
        self.txs_map.get(&cell_out_point.tx_hash).map_or_else(
            || {
                self.provider
                    .get_transaction(&cell_out_point.tx_hash)
                    .and_then(|(tx, _block_hash)| {
                        tx.outputs()
                            .get(cell_out_point.index as usize)
                            .map(|output| output.capacity)
                    })
            },
            |index| {
                self.txs[*index]
                    .transaction
                    .outputs()
                    .get(cell_out_point.index as usize)
                    .map(|output| output.capacity)
            },
        )
    }

    fn calculate_transaction_fee(
        &self,
        transaction: &Transaction,
    ) -> Result<Capacity, FailureError> {
        let mut fee = Capacity::zero();
        for input in transaction.inputs() {
            if let Some(capacity) = self.get_capacity(&input.previous_output) {
                fee = fee.safe_add(capacity)?;
            } else {
                Err(Error::InvalidInput)?;
            }
        }
        let spent_capacity: Capacity = transaction
            .outputs()
            .iter()
            .map(|output| output.capacity)
            .try_fold(Capacity::zero(), Capacity::safe_add)?;
        if spent_capacity > fee {
            Err(Error::InvalidOutput)?;
        }
        fee = fee.safe_sub(spent_capacity)?;
        Ok(fee)
    }
}

#[derive(Clone)]
pub struct BlockAssemblerController {
    get_block_template_sender: Sender<Request<BlockTemplateParams, BlockTemplateResult>>,
    stop: StopHandler<()>,
}

impl Drop for BlockAssemblerController {
    fn drop(&mut self) {
        self.stop.try_send();
    }
}

struct BlockAssemblerReceivers {
    get_block_template_receiver: Receiver<Request<BlockTemplateParams, BlockTemplateResult>>,
}

impl BlockAssemblerController {
    pub fn get_block_template(
        &self,
        bytes_limit: Option<u64>,
        proposals_limit: Option<u64>,
        max_version: Option<Version>,
    ) -> BlockTemplateResult {
        Request::call(
            &self.get_block_template_sender,
            (bytes_limit, proposals_limit, max_version),
        )
        .expect("get_block_template() failed")
    }
}

pub struct BlockAssembler<CS> {
    shared: Shared<CS>,
    candidate_uncles: LruCache<H256, Arc<Block>>,
    config: BlockAssemblerConfig,
    work_id: AtomicUsize,
    last_uncles_updated_at: AtomicU64,
    template_caches: Mutex<LruCache<(Cycle, u64, Version), TemplateCache>>,
    proof_size: usize,
}

impl<CS: ChainStore + 'static> BlockAssembler<CS> {
    pub fn new(shared: Shared<CS>, config: BlockAssemblerConfig) -> Self {
        Self {
            proof_size: shared.consensus().pow_engine().proof_size(),
            shared,
            config,
            candidate_uncles: LruCache::new(MAX_CANDIDATE_UNCLES),
            work_id: AtomicUsize::new(0),
            last_uncles_updated_at: AtomicU64::new(0),
            template_caches: Mutex::new(LruCache::new(TEMPLATE_CACHE_SIZE)),
        }
    }

    pub fn start<S: ToString>(
        mut self,
        thread_name: Option<S>,
        notify: &NotifyController,
    ) -> BlockAssemblerController {
        let (signal_sender, signal_receiver) =
            crossbeam_channel::bounded::<()>(SIGNAL_CHANNEL_SIZE);
        let (get_block_template_sender, get_block_template_receiver) =
            crossbeam_channel::bounded(DEFAULT_CHANNEL_SIZE);

        let mut thread_builder = thread::Builder::new();
        // Mainly for test: give a empty thread_name
        if let Some(name) = thread_name {
            thread_builder = thread_builder.name(name.to_string());
        }

        let receivers = BlockAssemblerReceivers {
            get_block_template_receiver,
        };

        let new_uncle_receiver = notify.subscribe_new_uncle(BLOCK_ASSEMBLER_SUBSCRIBER);
        let thread = thread_builder
            .spawn(move || loop {
                select! {
                    recv(signal_receiver) -> _ => {
                        break;
                    }
                    recv(new_uncle_receiver) -> msg => match msg {
                        Ok(uncle_block) => {
                            let hash = uncle_block.header().hash();
                            self.candidate_uncles.insert(hash.to_owned(), uncle_block);
                            self.last_uncles_updated_at
                                .store(unix_time_as_millis(), Ordering::SeqCst);
                        }
                        _ => {
                            error!(target: "miner", "new_uncle_receiver closed");
                            break;
                        }
                    },
                    recv(receivers.get_block_template_receiver) -> msg => match msg {
                        Ok(Request { responder, arguments: (bytes_limit, proposals_limit,  max_version) }) => {
                            let _ = responder.send(self.get_block_template(bytes_limit, proposals_limit, max_version));
                        },
                        _ => {
                            error!(target: "miner", "get_block_template_receiver closed");
                            break;
                        },
                    }
                }
            }).expect("Start MinerAgent failed");
        let stop = StopHandler::new(SignalSender::Crossbeam(signal_sender), thread);

        BlockAssemblerController {
            get_block_template_sender,
            stop,
        }
    }

    fn transform_params(
        &self,
        bytes_limit: Option<u64>,
        proposals_limit: Option<u64>,
        max_version: Option<Version>,
    ) -> (u64, u64, Version) {
        let consensus = self.shared.consensus();
        let bytes_limit = bytes_limit
            .min(Some(consensus.max_block_bytes()))
            .unwrap_or_else(|| consensus.max_block_bytes());
        let proposals_limit = proposals_limit
            .min(Some(consensus.max_block_proposals_limit()))
            .unwrap_or_else(|| consensus.max_block_proposals_limit());
        let version = max_version
            .min(Some(consensus.block_version()))
            .unwrap_or_else(|| consensus.block_version());

        (bytes_limit, proposals_limit, version)
    }

    fn transform_uncle(uncle: UncleBlock) -> UncleTemplate {
        let UncleBlock { header, proposals } = uncle;

        UncleTemplate {
            hash: header.hash().to_owned(),
            required: false,
            proposals: proposals.into_iter().map(Into::into).collect(),
            header: (&header).into(),
        }
    }

    fn transform_cellbase(tx: &Transaction, cycles: Option<Cycle>) -> CellbaseTemplate {
        CellbaseTemplate {
            hash: tx.hash().to_owned(),
            cycles: cycles.map(|c| c.to_string()),
            data: tx.into(),
        }
    }

    fn transform_tx(
        tx: &PoolEntry,
        required: bool,
        depends: Option<Vec<u32>>,
    ) -> TransactionTemplate {
        TransactionTemplate {
            hash: tx.transaction.hash().to_owned(),
            required,
            cycles: tx.cycles.map(|c| c.to_string()),
            depends,
            data: (&tx.transaction).into(),
        }
    }

    fn calculate_txs_size_limit(
        &self,
        bytes_limit: u64,
        uncles: &[UncleBlock],
        proposals: &[ProposalShortId],
    ) -> usize {
        let occupied = Header::serialized_size(self.proof_size)
            + uncles
                .iter()
                .map(|u| u.serialized_size(self.proof_size))
                .sum::<usize>()
            + proposals.len() * ProposalShortId::serialized_size();
        let bytes_limit = bytes_limit as usize;
        assert!(bytes_limit > occupied, "block size limit is too small");
        bytes_limit - occupied
    }

    fn get_block_template(
        &mut self,
        bytes_limit: Option<u64>,
        proposals_limit: Option<u64>,
        max_version: Option<Version>,
    ) -> Result<BlockTemplate, FailureError> {
        let cycles_limit = self.shared.consensus().max_block_cycles();
        let (bytes_limit, proposals_limit, version) =
            self.transform_params(bytes_limit, proposals_limit, max_version);
        let uncles_count_limit = self.shared.consensus().max_uncles_num() as u32;

        let last_uncles_updated_at = self.last_uncles_updated_at.load(Ordering::SeqCst);
        let chain_state = self.shared.chain_state().lock();
        let last_txs_updated_at = chain_state.get_last_txs_updated_at();

        let header = chain_state.tip_header().to_owned();
        let number = chain_state.tip_number() + 1;
        let current_time = cmp::max(unix_time_as_millis(), header.timestamp() + 1);

        let mut template_caches = self.template_caches.lock();

        if let Some(template_cache) = template_caches.get(&(cycles_limit, bytes_limit, version)) {
            if !template_cache.is_outdate(
                last_uncles_updated_at,
                last_txs_updated_at,
                current_time,
                number.to_string(),
            ) {
                return Ok(template_cache.template.clone());
            }
        }
        let last_epoch = chain_state.current_epoch_ext().clone();

        let next_epoch_ext = self.shared.next_epoch_ext(&last_epoch, &header);
        let current_epoch = next_epoch_ext.unwrap_or(last_epoch);

        let (uncles, bad_uncles) = self.prepare_uncles(&header, &current_epoch);
        if !bad_uncles.is_empty() {
            for bad in bad_uncles {
                self.candidate_uncles.remove(&bad);
            }
        }

        let proposals = chain_state.get_proposals(proposals_limit as usize);
        let txs_size_limit = self.calculate_txs_size_limit(bytes_limit, &uncles, &proposals);
        // It is assumed that cellbase transaction consumes 0 cycles, so it is not excluded when getting transactions from pool.
        let transactions = chain_state.get_staging_txs(txs_size_limit, cycles_limit);

        // Release the lock as soon as possible, let other services do their work
        drop(chain_state);

        let args = self
            .config
            .args
            .iter()
            .cloned()
            .map(JsonBytes::into_vec)
            .map(Bytes::from)
            .collect();

        // dummy cellbase
        let cellbase_lock = Script::new(args, self.config.code_hash.clone());
        let cellbase = self.create_cellbase_transaction(
            &header,
            &current_epoch,
            &transactions,
            cellbase_lock,
        )?;

        // Should recalculate current time after create cellbase (create cellbase may spend a lot of time)
        let current_time = cmp::max(unix_time_as_millis(), header.timestamp() + 1);
        let template = BlockTemplate {
            version,
            difficulty: current_epoch.difficulty().clone(),
            current_time: current_time.to_string(),
            number: number.to_string(),
            epoch: current_epoch.number().to_string(),
            parent_hash: header.hash().to_owned(),
            cycles_limit: cycles_limit.to_string(),
            bytes_limit: bytes_limit.to_string(),
            uncles_count_limit,
            uncles: uncles.into_iter().map(Self::transform_uncle).collect(),
            transactions: transactions
                .iter()
                .map(|tx| Self::transform_tx(tx, false, None))
                .collect(),
            proposals: proposals.into_iter().map(Into::into).collect(),
            cellbase: Self::transform_cellbase(&cellbase, None),
            work_id: format!("{}", self.work_id.fetch_add(1, Ordering::SeqCst)),
        };

        template_caches.insert(
            (cycles_limit, bytes_limit, version),
            TemplateCache {
                time: current_time,
                uncles_updated_at: last_uncles_updated_at,
                txs_updated_at: last_txs_updated_at,
                template: template.clone(),
            },
        );

        Ok(template)
    }

    fn create_cellbase_transaction(
        &self,
        tip: &Header,
        current_epoch: &EpochExt,
        pes: &[PoolEntry],
        lock: Script,
    ) -> Result<Transaction, FailureError> {
        // NOTE: To generate different cellbase txid, we put header number in the input script
        let input = CellInput::new_cellbase_input(tip.number() + 1);
        // NOTE: We could've just used byteorder to serialize u64 and hex string into bytes,
        // but the truth is we will modify this after we designed lock script anyway, so let's
        // stick to the simpler way and just convert everything to a single string, then to UTF8
        // bytes, they really serve the same purpose at the moment

        let block_reward = current_epoch.block_reward(tip.number() + 1)?;
        let mut fee = Capacity::zero();
        // depends cells may produced from previous tx
        let fee_calculator = FeeCalculator::new(&pes, &self.shared);
        for pe in pes {
            fee = fee.safe_add(fee_calculator.calculate_transaction_fee(&pe.transaction)?)?;
        }

        let output = CellOutput::new(block_reward.safe_add(fee)?, Bytes::new(), lock, None);

        Ok(TransactionBuilder::default()
            .input(input)
            .output(output)
            .build())
    }

    fn prepare_uncles(
        &self,
        tip: &Header,
        current_epoch_ext: &EpochExt,
    ) -> (Vec<UncleBlock>, Vec<H256>) {
        let max_uncles_age = self.shared.consensus().max_uncles_age();
        let mut excluded = FnvHashSet::default();

        // cB
        // tip      1 depth, valid uncle
        // tip.p^0  ---/  2
        // tip.p^1  -----/  3
        // tip.p^2  -------/  4
        // tip.p^3  ---------/  5
        // tip.p^4  -----------/  6
        // tip.p^5  -------------/
        // tip.p^6
        let mut block_hash = tip.hash().to_owned();
        excluded.insert(block_hash.clone());
        for _depth in 0..max_uncles_age {
            if let Some(block) = self.shared.block(&block_hash) {
                excluded.insert(block.header().parent_hash().to_owned());
                for uncle in block.uncles() {
                    excluded.insert(uncle.header.hash().to_owned());
                }

                block_hash = block.header().parent_hash().to_owned();
            } else {
                break;
            }
        }

        let current_number = tip.number() + 1;

        let max_uncles_num = self.shared.consensus().max_uncles_num();
        let mut included = FnvHashSet::default();
        let mut uncles = Vec::with_capacity(max_uncles_num);
        let mut bad_uncles = Vec::new();

        for (hash, block) in self.candidate_uncles.iter() {
            if uncles.len() == max_uncles_num {
                break;
            }

            let epoch_number = current_epoch_ext.number();

            // uncle must be same difficulty epoch with candidate
            if block.header().difficulty() != current_epoch_ext.difficulty()
                || block.header().epoch() != epoch_number
            {
                bad_uncles.push(hash.clone());
                continue;
            }

            let depth = current_number.saturating_sub(block.header().number());
            if depth > max_uncles_age as u64
                || depth < 1
                || included.contains(hash)
                || excluded.contains(hash)
            {
                bad_uncles.push(hash.clone());
            } else {
                let uncle = UncleBlock {
                    header: block.header().to_owned(),
                    proposals: block.proposals().to_vec(),
                };
                uncles.push(uncle);
                included.insert(hash.clone());
            }
        }
        (uncles, bad_uncles)
    }
}

#[cfg(test)]
mod tests {
    use crate::{block_assembler::BlockAssembler, config::BlockAssemblerConfig};
    use ckb_chain::chain::ChainBuilder;
    use ckb_chain::chain::ChainController;
    use ckb_chain_spec::consensus::Consensus;
    use ckb_core::block::Block;
    use ckb_core::block::BlockBuilder;
    use ckb_core::extras::EpochExt;
    use ckb_core::header::{Header, HeaderBuilder};
    use ckb_core::script::Script;
    use ckb_core::transaction::{
        CellInput, CellOutput, ProposalShortId, Transaction, TransactionBuilder,
    };
    use ckb_core::{BlockNumber, Bytes, EpochNumber};
    use ckb_db::memorydb::MemoryKeyValueDB;
    use ckb_notify::{NotifyController, NotifyService};
    use ckb_pow::Pow;
    use ckb_shared::shared::Shared;
    use ckb_shared::shared::SharedBuilder;
    use ckb_store::{ChainKVStore, ChainStore};
    use ckb_traits::ChainProvider;
    use ckb_verification::{BlockVerifier, HeaderResolverWrapper, HeaderVerifier, Verifier};
    use jsonrpc_types::{BlockTemplate, CellbaseTemplate};
    use numext_fixed_hash::H256;
    use std::convert::TryInto;
    use std::sync::Arc;

    fn start_chain(
        consensus: Option<Consensus>,
        notify: Option<NotifyController>,
    ) -> (
        ChainController,
        Shared<ChainKVStore<MemoryKeyValueDB>>,
        NotifyController,
    ) {
        let mut builder = SharedBuilder::<MemoryKeyValueDB>::new();
        if let Some(consensus) = consensus {
            builder = builder.consensus(consensus);
        }
        let shared = builder.build().unwrap();

        let notify = notify.unwrap_or_else(|| NotifyService::default().start::<&str>(None));
        let chain_service = ChainBuilder::new(shared.clone(), notify.clone())
            .verification(false)
            .build();
        let chain_controller = chain_service.start::<&str>(None);
        (chain_controller, shared, notify)
    }

    fn setup_block_assembler<CS: ChainStore + 'static>(
        shared: Shared<CS>,
        config: BlockAssemblerConfig,
    ) -> BlockAssembler<CS> {
        BlockAssembler::new(shared, config)
    }

    #[test]
    fn test_get_block_template() {
        let (_chain_controller, shared, _notify) = start_chain(None, None);
        let config = BlockAssemblerConfig {
            code_hash: H256::zero(),
            args: vec![],
        };
        let mut block_assembler = setup_block_assembler(shared.clone(), config);

        let block_template = block_assembler
            .get_block_template(None, None, None)
            .unwrap();

        let BlockTemplate {
            version,
            difficulty,
            current_time,
            number,
            epoch,
            parent_hash,
            uncles, // Vec<UncleTemplate>
            transactions, // Vec<TransactionTemplate>
            proposals, // Vec<ProposalShortId>
            cellbase, // CellbaseTemplate
            ..
            // cycles_limit,
            // bytes_limit,
            // uncles_count_limit,
        } = block_template;

        let cellbase = {
            let CellbaseTemplate { data, .. } = cellbase;
            data
        };

        let header_builder = HeaderBuilder::default()
            .version(version)
            .number(number.parse::<BlockNumber>().unwrap())
            .epoch(epoch.parse::<EpochNumber>().unwrap())
            .difficulty(difficulty)
            .timestamp(current_time.parse::<u64>().unwrap())
            .parent_hash(parent_hash);

        let block = BlockBuilder::default()
            .uncles(
                uncles
                    .into_iter()
                    .map(TryInto::try_into)
                    .collect::<Result<_, _>>()
                    .unwrap(),
            )
            .transaction(cellbase.try_into().unwrap())
            .transactions(
                transactions
                    .into_iter()
                    .map(TryInto::try_into)
                    .collect::<Result<_, _>>()
                    .unwrap(),
            )
            .proposals(
                proposals
                    .into_iter()
                    .map(TryInto::try_into)
                    .collect::<Result<_, _>>()
                    .unwrap(),
            )
            .header_builder(header_builder)
            .build();

        let resolver = HeaderResolverWrapper::new(block.header(), shared.clone());
        let header_verify_result = {
            let chain_state = shared.chain_state().lock();
            let header_verifier =
                HeaderVerifier::new(&*chain_state, Pow::Dummy(Default::default()).engine());
            header_verifier.verify(&resolver)
        };
        assert!(header_verify_result.is_ok());

        let block_verify = BlockVerifier::new(shared.clone());
        assert!(block_verify.verify(&block).is_ok());
    }

    fn gen_block(parent_header: &Header, nonce: u64, epoch: &EpochExt) -> Block {
        let number = parent_header.number() + 1;
        let cellbase = create_cellbase(number, epoch);
        let header = HeaderBuilder::default()
            .parent_hash(parent_header.hash().to_owned())
            .timestamp(parent_header.timestamp() + 10)
            .number(number)
            .epoch(epoch.number())
            .difficulty(epoch.difficulty().clone())
            .nonce(nonce)
            .build();

        BlockBuilder::default()
            .header(header)
            .transaction(cellbase)
            .proposal(ProposalShortId::from_slice(&[1; 10]).unwrap())
            .unsafe_build()
    }

    fn create_cellbase(number: BlockNumber, epoch: &EpochExt) -> Transaction {
        TransactionBuilder::default()
            .input(CellInput::new_cellbase_input(number))
            .output(CellOutput::new(
                epoch.block_reward(number).unwrap(),
                Bytes::new(),
                Script::default(),
                None,
            ))
            .build()
    }

    #[test]
    fn test_prepare_uncles() {
        let mut consensus = Consensus::default();
        consensus.genesis_epoch_ext.set_length(4);
        let epoch = consensus.genesis_epoch_ext().clone();

        let (chain_controller, shared, notify) = start_chain(Some(consensus), None);
        let config = BlockAssemblerConfig {
            code_hash: H256::zero(),
            args: vec![],
        };
        let block_assembler = setup_block_assembler(shared.clone(), config);
        let new_uncle_receiver = notify.subscribe_new_uncle("test_prepare_uncles");
        let block_assembler_controller = block_assembler.start(Some("test"), &notify.clone());

        let genesis = shared.block_header(&shared.block_hash(0).unwrap()).unwrap();

        let block0_0 = gen_block(&genesis, 11, &epoch);
        let block0_1 = gen_block(&genesis, 10, &epoch);

        let last_epoch = epoch.clone();
        let epoch = shared
            .next_epoch_ext(&last_epoch, block0_1.header())
            .unwrap_or(last_epoch);

        let block1_1 = gen_block(block0_1.header(), 10, &epoch);

        chain_controller
            .process_block(Arc::new(block0_1.clone()))
            .unwrap();
        chain_controller
            .process_block(Arc::new(block0_0.clone()))
            .unwrap();
        chain_controller
            .process_block(Arc::new(block1_1.clone()))
            .unwrap();

        // block number 3, epoch 0
        let _ = new_uncle_receiver.recv();
        let block_template = block_assembler_controller
            .get_block_template(None, None, None)
            .unwrap();
        assert_eq!(&block_template.uncles[0].hash, block0_0.header().hash());

        let last_epoch = epoch.clone();
        let epoch = shared
            .next_epoch_ext(&last_epoch, block1_1.header())
            .unwrap_or(last_epoch);

        let block2_1 = gen_block(block1_1.header(), 10, &epoch);
        chain_controller
            .process_block(Arc::new(block2_1.clone()))
            .unwrap();

        let block_template = block_assembler_controller
            .get_block_template(None, None, None)
            .unwrap();
        // block number 4, epoch 1, block_template should not include last epoch uncles
        assert!(block_template.uncles.is_empty());
    }
}
