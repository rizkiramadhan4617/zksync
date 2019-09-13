use ethabi::{decode, ParamType};
use failure::format_err;
use futures::Future;
use std::collections::HashMap;
use std::convert::{TryFrom, TryInto};
use std::env;
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use web3::contract::Contract;
use web3::types::{Address, BlockNumber, Filter, FilterBuilder, Log, H160, U256};
use web3::{Transport, Web3};

use bigdecimal::BigDecimal;
use models::node::{AccountAddress, AccountId, TokenId};
use models::params::{FR_ADDRESS_LEN, PRIORITY_EXPIRATION};
use num_traits::FromPrimitive;
use std::sync::mpsc::sync_channel;
use storage::ConnectionPool;

// From enum OpType in Franklin.sol
const DEPOSIT_OPTYPE_ID: u8 = 1u8;
const FULLEXIT_OPTYPE_ID: u8 = 6u8;

pub struct EthWatch<T: Transport> {
    main_contract: (ethabi::Contract, Contract<T>),
    gov_contract: (ethabi::Contract, Contract<T>),
    processed_block: u64,
    eth_state: Arc<RwLock<ETHState>>,
    web3: Web3<T>,
    db_pool: ConnectionPool,
}

#[derive(Debug)]
pub struct ETHState {
    pub tokens: HashMap<TokenId, Address>,
    pub priority_queue: Vec<PriorityOp>,
}

#[derive(Debug)]
pub enum PriorityOpData {
    Deposit {
        sender: Address,
        token: TokenId,
        amount: BigDecimal,
        account: AccountAddress,
    },
    FullExit {
        account_id: AccountId,
        eth_address: Address,
        token: TokenId,
        signature: Vec<u8>,
    },
}

impl PriorityOpData {
    fn parse_pubdata(pub_data: &[u8], op_type_id: u8) -> Self {
        match op_type_id {
            DEPOSIT_OPTYPE_ID => {
                let sender = Address::from_slice(&pub_data[0..20]);
                let token = u16::from_be_bytes(pub_data[20..(20 + 2)].try_into().unwrap());
                // TODO: bigdecimal -> u128 conversion is buggy.
                let amount = BigDecimal::from_u128(u128::from_be_bytes(
                    pub_data[22..(22 + 16)].try_into().unwrap(),
                ))
                .unwrap();
                let account =
                    AccountAddress::from_bytes(&pub_data[38..(38 + FR_ADDRESS_LEN)]).unwrap();
                PriorityOpData::Deposit {
                    sender,
                    token,
                    amount,
                    account,
                }
            }
            FULLEXIT_OPTYPE_ID => {
                let account_id = {
                    let mut account_id_bytes = [0u8; 4];
                    account_id_bytes[1..4].copy_from_slice(&pub_data[0..3]);
                    u32::from_be_bytes(account_id_bytes)
                };
                let eth_address = Address::from_slice(&pub_data[2..(20 + 2)]);
                let token = u16::from_be_bytes(pub_data[22..(22 + 2)].try_into().unwrap());
                let signature = pub_data[24..(24 + 64)].to_vec();
                PriorityOpData::FullExit {
                    account_id,
                    eth_address,
                    token,
                    signature,
                }
            }
            _ => {
                panic!("Unsupported priority queue op type.");
            }
        }
    }
}

#[derive(Debug)]
pub struct PriorityOp {
    data: PriorityOpData,
    deadline_block: u64,
    eth_fee: BigDecimal,
}

impl TryFrom<Log> for PriorityOp {
    type Error = failure::Error;

    fn try_from(event: Log) -> Result<PriorityOp, failure::Error> {
        let mut dec_ev = decode(
            &[
                ParamType::Uint(8),   // OpType
                ParamType::Bytes,     // Pubdata
                ParamType::Uint(256), // expir. block
                ParamType::Uint(256), // fee
            ],
            &event.data.0,
        )
        .map_err(|e| format_err!("Event data decode: {:?}", e))?;

        Ok(PriorityOp {
            data: {
                let op_type = dec_ev
                    .remove(0)
                    .to_uint()
                    .as_ref()
                    .map(|ui| U256::as_u32(ui) as u8)
                    .unwrap();
                let op_pubdata = dec_ev.remove(0).to_bytes().unwrap();
                PriorityOpData::parse_pubdata(&op_pubdata, op_type)
            },
            deadline_block: dec_ev
                .remove(0)
                .to_uint()
                .as_ref()
                .map(U256::as_u64)
                .unwrap(),
            eth_fee: {
                let amount_uint = dec_ev.remove(0).to_uint().unwrap();
                BigDecimal::from_str(&format!("{}", amount_uint)).unwrap()
            },
        })
    }
}

impl ETHState {
    fn add_new_token(&mut self, id: TokenId, address: Address) {
        self.tokens.insert(id, address);
    }
}

#[derive(Debug)]
struct TokenAddedEvent {
    address: Address,
    id: u32,
}

impl TryFrom<Log> for TokenAddedEvent {
    type Error = failure::Error;

    fn try_from(event: Log) -> Result<TokenAddedEvent, failure::Error> {
        let mut dec_ev = decode(&[ParamType::Address, ParamType::Uint(32)], &event.data.0)
            .map_err(|e| format_err!("Event data decode: {:?}", e))?;
        Ok(TokenAddedEvent {
            address: dec_ev.remove(0).to_address().unwrap(),
            id: dec_ev
                .remove(0)
                .to_uint()
                .as_ref()
                .map(U256::as_u32)
                .unwrap(),
        })
    }
}

impl<T: Transport> EthWatch<T> {
    pub fn new(web3: Web3<T>, db_pool: ConnectionPool) -> Self {
        let main_contract = {
            let abi_string = serde_json::Value::from_str(models::abi::FRANKLIN_CONTRACT)
                .unwrap()
                .get("abi")
                .unwrap()
                .to_string();
            let abi = ethabi::Contract::load(abi_string.as_bytes()).unwrap();
            let address = H160::from_str(
                &env::var("CONTRACT_ADDR")
                    .map(|s| s[2..].to_string())
                    .expect("CONTRACT_ADDR env var not found"),
            )
            .unwrap();

            (abi.clone(), Contract::new(web3.eth(), address, abi.clone()))
        };

        let gov_contract = {
            let abi_string = serde_json::Value::from_str(models::abi::GOVERNANCE_CONTRACT)
                .unwrap()
                .get("abi")
                .unwrap()
                .to_string();
            let abi = ethabi::Contract::load(abi_string.as_bytes()).unwrap();
            let address = H160::from_str(
                &env::var("GOVERNANCE_ADDR")
                    .map(|s| s[2..].to_string())
                    .expect("GOVERNANCE_ADDR env var not found"),
            )
            .unwrap();

            (abi.clone(), Contract::new(web3.eth(), address, abi.clone()))
        };

        Self {
            main_contract,
            gov_contract,
            processed_block: 0,
            eth_state: Arc::new(RwLock::new(ETHState {
                tokens: HashMap::new(),
                priority_queue: Vec::new(),
            })),
            web3,
            db_pool,
        }
    }

    fn get_new_token_event_filter(&self, from: BlockNumber, to: BlockNumber) -> Filter {
        let new_token_event_topic = self
            .gov_contract
            .0
            .event("TokenAdded")
            .expect("gov contract abi error")
            .signature();
        FilterBuilder::default()
            .address(vec![self.gov_contract.1.address()])
            .from_block(from)
            .to_block(to)
            .topics(Some(vec![new_token_event_topic]), None, None, None)
            .build()
    }

    fn get_new_token_events(
        &self,
        from: BlockNumber,
        to: BlockNumber,
    ) -> Result<Vec<TokenAddedEvent>, failure::Error> {
        let filter = self.get_new_token_event_filter(from, to);

        self.web3
            .eth()
            .logs(filter)
            .wait()?
            .into_iter()
            .map(|event| {
                TokenAddedEvent::try_from(event).map_err(|e| {
                    format_err!("Failed to parse TokenAdded event log from ETH: {}", e)
                })
            })
            .collect()
    }

    fn get_priority_op_event_filter(&self, from: BlockNumber, to: BlockNumber) -> Filter {
        let priority_op_event_topic = self
            .main_contract
            .0
            .event("NewPriorityRequest")
            .expect("main contract abi error")
            .signature();
        FilterBuilder::default()
            .address(vec![self.main_contract.1.address()])
            .from_block(from)
            .to_block(to)
            .topics(Some(vec![priority_op_event_topic]), None, None, None)
            .build()
    }

    fn get_priority_op_events(
        &self,
        from: BlockNumber,
        to: BlockNumber,
    ) -> Result<Vec<PriorityOp>, failure::Error> {
        let filter = self.get_priority_op_event_filter(from, to);
        self.web3
            .eth()
            .logs(filter)
            .wait()?
            .into_iter()
            .map(|event| {
                PriorityOp::try_from(event).map_err(|e| {
                    format_err!("Failed to parse priority queue event log from ETH: {:?}", e)
                })
            })
            .collect()
    }

    fn restore_state_from_eth(&mut self, block: u64) {
        let mut eth_state = self.eth_state.write().expect("ETH state lock");

        // restore priority queue
        let prior_queue_events = self
            .get_priority_op_events(
                BlockNumber::Number(block.saturating_sub(PRIORITY_EXPIRATION)),
                BlockNumber::Number(block),
            )
            .expect("Failed to restore priority queue events from ETH");
        eth_state
            .priority_queue
            .extend(prior_queue_events.into_iter());

        // restore token list from governance contract
        let new_tokens = self
            .get_new_token_events(BlockNumber::Earliest, BlockNumber::Number(block))
            .expect("Failed to restore token list from ETH");
        for token in new_tokens.into_iter() {
            eth_state.add_new_token(token.id as TokenId, token.address)
        }

        debug!("ETH state: {:#?}", *eth_state);
    }

    fn process_new_blocks(&mut self, last_block: u64) -> Result<(), failure::Error> {
        debug_assert!(self.processed_block < last_block);

        let new_tokens = self.get_new_token_events(
            BlockNumber::Number(self.processed_block + 1),
            BlockNumber::Number(last_block),
        )?;
        let priority_op_events = self.get_priority_op_events(
            BlockNumber::Number(self.processed_block + 1),
            BlockNumber::Number(last_block),
        )?;

        let mut eth_state = self.eth_state.write().expect("ETH state lock");
        for priority_op in priority_op_events.into_iter() {
            debug!("New priority op: {:?}", priority_op);
            eth_state.priority_queue.push(priority_op);
        }
        for token in new_tokens.into_iter() {
            debug!("New token added: {:?}", token);
            eth_state.add_new_token(token.id as TokenId, token.address);
        }

        // TODO: check if op was executed. decide best way.
        Ok(())
    }

    fn commit_state(&self) {
        let eth_state = self.eth_state.read().expect("eth state read lock");
        self.db_pool
            .access_storage()
            .map(|storage| {
                for (id, address) in &eth_state.tokens {
                    if let Err(e) = storage.store_token(*id, &format!("0x{:x}", address), None) {
                        warn!("Failed to add token to db: {:?}", e);
                    }
                }
            })
            .unwrap_or_default();
    }

    pub fn get_shared_eth_state(&self) -> Arc<RwLock<ETHState>> {
        self.eth_state.clone()
    }

    pub fn run(mut self) {
        let block = self
            .web3
            .eth()
            .block_number()
            .wait()
            .expect("Block number")
            .as_u64();
        self.processed_block = block;
        self.restore_state_from_eth(block);

        loop {
            std::thread::sleep(Duration::from_secs(1));
            let last_block_number = self.web3.eth().block_number().wait();
            let block = if let Ok(block) = last_block_number {
                block.as_u64()
            } else {
                continue;
            };

            if block > self.processed_block {
                self.process_new_blocks(block);
                self.commit_state();
            }
        }
    }
}

pub fn start_eth_watch(pool: ConnectionPool) -> Arc<RwLock<ETHState>> {
    let (sender, receiver) = sync_channel(1);

    std::thread::Builder::new()
        .name("eth_watch".to_string())
        .spawn(move || {
            let web3_url = env::var("WEB3_URL").expect("WEB3_URL env var not found");
            let (_eloop, transport) = web3::transports::Http::new(&web3_url).unwrap();
            let web3 = web3::Web3::new(transport);
            let eth_watch = EthWatch::new(web3, pool);
            sender.send(eth_watch.get_shared_eth_state()).unwrap();
            eth_watch.run();
        })
        .expect("Eth watcher thread");

    receiver.recv().unwrap()
}
