use crate::synchronizer::{BLOCK_FETCH_TOKEN, SEND_GET_HEADERS_TOKEN, TIMEOUT_EVICTION_TOKEN};
use crate::tests::TestNode;
use crate::{Config, NetworkProtocol, SyncSharedState, Synchronizer};
use ckb_chain::chain::ChainBuilder;
use ckb_chain_spec::consensus::Consensus;
use ckb_core::block::BlockBuilder;
use ckb_core::header::HeaderBuilder;
use ckb_core::transaction::{CellInput, CellOutput, TransactionBuilder};
use ckb_db::memorydb::MemoryKeyValueDB;
use ckb_notify::NotifyService;
use ckb_protocol::SyncMessage;
use ckb_shared::shared::{Shared, SharedBuilder};
use ckb_store::ChainKVStore;
use ckb_traits::ChainProvider;
use ckb_util::RwLock;
use faketime::{self, unix_time_as_millis};
use flatbuffers::get_root;
use numext_fixed_uint::U256;
use std::sync::mpsc::channel;
use std::sync::Arc;
use std::thread;

#[test]
fn basic_sync() {
    let faketime_file = faketime::millis_tempfile(0).expect("create faketime file");
    faketime::enable(&faketime_file);
    let thread_name = format!("FAKETIME={}", faketime_file.display());

    let (mut node1, shared1) = setup_node(&thread_name, 1);
    let (mut node2, shared2) = setup_node(&thread_name, 3);

    node1.connect(&mut node2, NetworkProtocol::SYNC.into());

    let (signal_tx1, signal_rx1) = channel();
    thread::Builder::new()
        .name(thread_name.clone())
        .spawn(move || {
            node1.start(&signal_tx1, |data| {
                let msg = get_root::<SyncMessage>(data);
                // terminate thread after 3 blocks
                msg.payload_as_block()
                    .map(|block| block.header().unwrap().number() == 3)
                    .unwrap_or(false)
            });
        })
        .expect("thread spawn");

    let (signal_tx2, _) = channel();
    thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            node2.start(&signal_tx2, |_| false);
        })
        .expect("thread spawn");

    // Wait node1 receive block from node2
    let _ = signal_rx1.recv();

    assert_eq!(shared1.chain_state().lock().tip_number(), 3);
    assert_eq!(
        shared1.chain_state().lock().tip_number(),
        shared2.chain_state().lock().tip_number()
    );
}

fn setup_node(
    thread_name: &str,
    height: u64,
) -> (TestNode, Shared<ChainKVStore<MemoryKeyValueDB>>) {
    let mut block = BlockBuilder::default()
        .header_builder(
            HeaderBuilder::default()
                .timestamp(unix_time_as_millis())
                .difficulty(U256::from(1000u64)),
        )
        .build();

    let consensus = Consensus::default().set_genesis_block(block.clone());
    let shared = SharedBuilder::<MemoryKeyValueDB>::new()
        .consensus(consensus)
        .build()
        .unwrap();
    let notify = NotifyService::default().start(Some(thread_name));

    let chain_service = ChainBuilder::new(shared.clone(), notify)
        .verification(false)
        .build();
    let chain_controller = chain_service.start::<&str>(None);

    for _i in 0..height {
        let number = block.header().number() + 1;
        let timestamp = block.header().timestamp() + 1;

        let last_epoch = shared.get_epoch_ext(&block.header().hash()).unwrap();
        let epoch = shared
            .next_epoch_ext(&last_epoch, block.header())
            .unwrap_or(last_epoch);

        let cellbase = TransactionBuilder::default()
            .input(CellInput::new_cellbase_input(number))
            .output(CellOutput::default())
            .build();

        let header_builder = HeaderBuilder::default()
            .parent_hash(block.header().hash().to_owned())
            .number(number)
            .epoch(epoch.number())
            .timestamp(timestamp)
            .difficulty(epoch.difficulty().clone());

        block = BlockBuilder::default()
            .transaction(cellbase)
            .header_builder(header_builder)
            .build();

        chain_controller
            .process_block(Arc::new(block.clone()))
            .expect("process block should be OK");
    }

    let sync_shared_state = Arc::new(SyncSharedState::new(shared.clone()));
    let synchronizer = Synchronizer::new(chain_controller, sync_shared_state, Config::default());
    let mut node = TestNode::default();
    let protocol = Arc::new(RwLock::new(synchronizer)) as Arc<_>;
    node.add_protocol(
        NetworkProtocol::SYNC.into(),
        &protocol,
        &[
            SEND_GET_HEADERS_TOKEN,
            BLOCK_FETCH_TOKEN,
            TIMEOUT_EVICTION_TOKEN,
        ],
    );
    (node, shared)
}
