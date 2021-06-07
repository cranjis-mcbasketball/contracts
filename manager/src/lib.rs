use near_sdk::{
    AccountId,
    Balance,
    PanicOnDefault,
    Promise,
    PublicKey,
    borsh::{self, BorshDeserialize, BorshSerialize},
    collections::{LookupMap, TreeMap},
    env,
    json_types::{ValidAccountId, Base64VecU8, Base58PublicKey, U128},
    log,
    near_bindgen,
    serde::{Deserialize, Serialize},
    serde_json::json,
    Gas,
    PromiseResult,
    base64,
    BorshStorageKey
};
use cron_schedule::Schedule;
use std::str::FromStr;

near_sdk::setup_alloc!();

// Balance & Fee Definitions
pub const ONE_NEAR: u128 = 1_000_000_000_000_000_000_000_000;
pub const GAS_BASE_FEE: Gas = 3_000_000_000_000;
// TODO: investigate how much this should be, currently
// http post https://rpc.mainnet.near.org jsonrpc=2.0 id=dontcare method=EXPERIMENTAL_genesis_config
// > mainnet-config.json

pub const GAS_FOR_CALLBACK: Gas = 75_000_000_000_000;
pub const STAKE_BALANCE_MIN: u128 = 10 * ONE_NEAR;

// Boundary Definitions
pub const MAX_BLOCK_RANGE: u64 = 1_000_000_000_000_000;
pub const MAX_EPOCH_RANGE: u32 = 10_000;
pub const MAX_SECOND_RANGE: u32 = 600_000_000;
pub const SLOT_GRANULARITY: u64 = 60; // NOTE: Connection drain.. might be required if slot granularity changes
pub const NANO: u64 = 1_000_000_000;
pub const BPS_DENOMINATOR: u64 = 1_000;

#[derive(BorshStorageKey, BorshSerialize)]
pub enum StorageKeys {
    Tasks,
    Agents,
    Slots,
}

/// Allows tasks to be executed in async env
#[derive(BorshDeserialize, BorshSerialize, Debug, Serialize, Deserialize, PartialEq)]
#[serde(crate = "near_sdk::serde")]
pub enum TaskStatus {
    /// Shows a task is not currently active, ready for an agent to take
    Ready,

    /// Shows a task is currently being processed/called
    Active,

    /// Tasks marked as complete are ready for deletion from state. 
    Complete,

    /// Tasks that were known to fail by checking a promise callback.
    Failed
}

#[derive(BorshDeserialize, BorshSerialize, Debug, Serialize, Deserialize, PartialEq)]
#[serde(crate = "near_sdk::serde")]
pub struct Task {
    /// Entity responsible for this task, can change task details
    pub owner_id: AccountId,

    /// Account to direct all execution calls against
    pub contract_id: AccountId,

    /// Contract method this task will be executing
    pub function_id: String,

    /// Crontab Spec String
    /// Defines the interval spacing of execution
    pub cadence: String,

    /// Defines if this task can continue until balance runs out
    pub recurring: bool,

    /// Tasks status forces single executions per interval
    pub status: TaskStatus,

    /// Total balance of NEAR available for current and future executions
    pub total_deposit: Balance,

    /// Configuration of NEAR balance to send to each function call. This is the "amount" for a function call.
    pub deposit: Balance,

    /// Configuration of NEAR balance to attach to each function call. This is the "gas" for a function call.
    pub gas: Gas,

    // NOTE: Only allow static pre-defined bytes
    pub arguments: Vec<u8>
}

#[derive(BorshDeserialize, BorshSerialize, Debug, Serialize, Deserialize, PartialEq)]
#[serde(crate = "near_sdk::serde")]
pub struct Agent {
    pub payable_account_id: AccountId,
    pub balance: Balance,
    pub total_tasks_executed: u128
}

#[near_bindgen]
#[derive(BorshDeserialize, BorshSerialize, PanicOnDefault)]
pub struct CronManager {
    // Runtime
    // TODO: Setup DAO based management & ownership
    paused: bool,
    owner_id: AccountId,
    owner_pk: PublicKey,
    bps_block: u64,
    bps_timestamp: u64,

    // Basic management
    agents: LookupMap<AccountId, Agent>,
    slots: TreeMap<u128, Vec<Vec<u8>>>,
    tasks: LookupMap<Vec<u8>, Task>,

    // Economics
    available_balance: Balance,
    staked_balance: Balance,
    agent_fee: Balance,
    slot_granularity: u64
}

#[near_bindgen]
impl CronManager {
    /// ```bash
    /// near call cron.testnet new --accountId cron.testnet
    /// ```
    #[init]
    pub fn new() -> Self {
        assert!(!env::state_exists(), "The contract is already initialized");
        CronManager {
            paused: false,
            owner_id: env::signer_account_id(),
            owner_pk: env::signer_account_pk(),
            bps_block: env::block_index(),
            bps_timestamp: env::block_timestamp(),
            tasks: LookupMap::new(StorageKeys::Tasks),
            agents: LookupMap::new(StorageKeys::Agents),
            slots: TreeMap::new(StorageKeys::Slots),
            available_balance: 0,
            staked_balance: 0,
            agent_fee: u128::from(GAS_BASE_FEE),
            slot_granularity: SLOT_GRANULARITY
        }
    }

    // TODO:
    /// Migrate State
    /// Safely upgrade contract storage
    ///
    /// ```bash
    /// near call cron.testnet migrate --accountId cron.testnet
    /// ```
    // #[init(ignore_state)]
    // pub fn migrate_state(new_data: String) -> Self {
    //     // Deserialize the state using the old contract structure.
    //     let old_contract: CronManager = env::state_read().expect("Old state doesn't exist");
    //     // Verify that the migration can only be done by the owner.
    //     // This is not necessary, if the upgrade is done internally.
    //     assert_eq!(
    //         &env::predecessor_account_id(),
    //         &old_contract.owner_id,
    //         "Can only be called by the owner"
    //     );

    //     // Create the new contract using the data from the old contract.
    //     // CronManager { owner_id: old_contract.owner_id, data: old_contract.data, new_data }
    //     CronManager {
    //         paused: true,
    //         owner_id: old_contract.owner_id,
    //         owner_pk: old_contract.owner_pk,
    //         bps_block: env::block_index(),
    //         bps_timestamp: env::block_timestamp(),
    //         tasks: LookupMap::new(StorageKeys::Tasks),
    //         agents: LookupMap::new(StorageKeys::Agents),
    //         slots: TreeMap::new(StorageKeys::Slots),
    //         available_balance: 0,
    //         staked_balance: old_contract.staked_balance,
    //         agent_fee: u128::from(GAS_BASE_FEE),
    //         slot_granularity: SLOT_GRANULARITY
    //     }
    // }

    /// Tick: Cron Manager Heartbeat
    /// Used to aid computation of blocks per second, manage internal use of funds
    /// Ideally gets called every 6000 blocks
    ///
    /// near call cron.testnet tick '{}'
    pub fn tick(&mut self) {
        self.bps_block = env::block_index();
        self.bps_timestamp = env::block_timestamp();

        // TBD: Internal staking management
        log!("Balances: Available {}, Staked {}", self.available_balance, self.staked_balance);
    }

    /// Gets a set of tasks.
    /// Default: Returns the next executable set of tasks hashes.
    /// 
    /// Optional Parameters:
    /// "offset" - An unsigned integer specifying how far in the future to check for tasks that are slotted.
    /// 
    /// ```bash
    /// near view cron.testnet get_tasks
    /// ```
    pub fn get_tasks(&self, offset: Option<u64>) -> Vec<Base64VecU8> {
        let current_slot = self.get_slot_id(offset);

        // Get tasks based on current slot.
        // (Or closest past slot if there are leftovers.)
        let slot_ballpark = self.slots.floor_key(&current_slot);
        if let Some(k) = slot_ballpark {
            let mut ret: Vec<Base64VecU8> = Vec::new();
            let tasks = self.slots.get(&k).unwrap();
            for task in tasks.iter() {
                ret.push(Base64VecU8::from(task.to_vec()));
            }
            ret
        } else {
            vec![Base64VecU8::from(vec![])]
        }
    }

    /// Most useful for debugging at this point.
    pub fn get_all_tasks(&self, slot: Option<U128>) -> Vec<Task> {
        let mut ret: Vec<Task> = Vec::new();
        if let Some(slot_number) = slot {
            // User specified a slot number, only return tasks in there.
            let tasks_in_slot = self.slots.get(&slot_number.0).expect("Couldn't find tasks for given slot.");
            for task_hash in tasks_in_slot.iter() {
                let task = self.tasks.get(&task_hash)
                    .expect("No task found by hash");
                ret.push(task);
            }
        } else {
            // Return all slots
            for slot in self.slots.iter() {
                let tasks_in_slot = slot.1;
                for task_hash in tasks_in_slot.iter() {
                    let task = self.tasks.get(&task_hash)
                        .expect("No task found by hash");
                    ret.push(task);
                }
            }
        }
        ret
    }

    /// Gets the data payload of a single task by hash
    ///
    /// ```bash
    /// near view cron.testnet get_task '{"task_hash": "r2Jv…T4U4="}'
    /// ```
    pub fn get_task(&self, task_hash: Base64VecU8) -> Task {
        let task_hash = task_hash.0;
        let task = self.tasks.get(&task_hash)
            .expect("No task found by hash");
        task
    }

    /// Allows any user or contract to pay for future txns based on a specific schedule
    /// contract, function id & other settings. When the task runs out of balance
    /// the task is no longer executed, any additional funds will be returned to task owner.
    ///
    /// ```bash
    /// near call cron.testnet create_task '{"contract_id": "counter.in.testnet","function_id": "increment","cadence": "@daily","recurring": true,"deposit": 0,"gas": 2400000000000}' --accountId YOU.testnet
    /// ```
    #[payable]
    pub fn create_task(
        &mut self,
        contract_id: String,
        function_id: String,
        cadence: String,
        recurring: Option<bool>,
        deposit: Option<Balance>,
        gas: Option<Gas>,
        arguments: Option<Vec<u8>>
    ) -> Base64VecU8 {
        // No adding tasks while contract is paused
        assert_eq!(self.paused, false, "Create task paused");
        // check cadence can be parsed
        assert!(self.validate_cadence(cadence.clone()), "Cadence string invalid");
        log!("cadence {}", &cadence.clone());
        let item = Task {
            owner_id: env::signer_account_id(),
            contract_id,
            function_id,
            cadence,
            recurring: recurring.unwrap_or(false),
            status: TaskStatus::Ready,
            total_deposit: env::attached_deposit(),
            deposit: deposit.unwrap_or(0), // for Ⓝ only?
            gas: gas.unwrap_or(GAS_BASE_FEE),
            arguments: arguments.unwrap_or(b"".to_vec())
        };

        // Check that balance is sufficient for 1 execution minimum
        let call_balance_used = self.task_balance_uses(&item);
        let min_balance_needed: u128;
        if recurring.is_some() && recurring.unwrap() == true { min_balance_needed = call_balance_used * 2 }
        else { min_balance_needed = call_balance_used }
        assert!(min_balance_needed > u128::from(GAS_BASE_FEE), "Gas minimum has not been met");
        assert!(min_balance_needed <= item.total_deposit, "Not enough task balance to execute job, need at least {}", min_balance_needed);

        let hash = self.hash(&item);
        log!("Task Hash (as bytes) {:?}", &hash);
        // log!("Task data {:?}", &item.to_string());

        // Parse cadence into a future timestamp, then convert to a slot
        let next_slot = self.get_slot_from_cadence(item.cadence.clone());

        // Add task to catalog
        self.tasks.insert(&hash, &item);

        // Get previous task hashes in slot, add as needed
        let mut slot_slots = self.slots.get(&next_slot).unwrap_or(Vec::new());
        slot_slots.push(hash.clone());
        log!("Inserting into slot: {}", next_slot);
        self.slots.insert(&next_slot, &slot_slots);

        Base64VecU8::from(hash)
    }

    /// ```bash
    /// near call cron.testnet update_task '{"task_hash": "","cadence": "@weekly","recurring": true,"deposit": 0,"gas": 2400000000000}' --accountId YOU.testnet
    /// ```
    #[payable]
    pub fn update_task(
        &mut self,
        task_hash: Base64VecU8,
        cadence: Option<String>,
        recurring: Option<bool>,
        deposit: Option<u128>,
        gas: Option<Gas>,
        arguments: Option<Vec<u8>>
    ) {
        assert_eq!(self.paused, false, "Update task paused");
        let hash = task_hash.0;
        let mut task = self.tasks.get(&hash)
            .expect("No task found by hash");
        
        assert_eq!(task.owner_id, env::predecessor_account_id(), "Only owner can update their task.");

        if cadence.is_some() {
            // check cadence can be parsed
            assert!(self.validate_cadence(cadence.clone().unwrap()), "Cadence string invalid");
            task.cadence = cadence.unwrap();
        }

        // Update args that exist
        if recurring.is_some() { task.recurring = recurring.unwrap(); }
        if deposit.is_some() { task.deposit = deposit.unwrap(); }
        if gas.is_some() { task.gas = gas.unwrap(); }
        if arguments.is_some() { task.arguments = arguments.unwrap(); }

        // Update task total available balance, if this function was given a deposit.
        if env::attached_deposit() > 0 {
            task.total_deposit += env::attached_deposit();
        }

        self.tasks.insert(&hash, &task);
    }

    /// Deletes a task in its entirety, returning any remaining balance to task owner.
    /// 
    /// ```bash
    /// near call cron.testnet remove_task '{"task_hash": ""}' --accountId YOU.testnet
    /// ```
    pub fn remove_task(&mut self, task_hash: Base64VecU8) {
        let hash = task_hash.0;
        let task = self.tasks.get(&hash)
            .expect("No task found by hash");

        assert_eq!(task.owner_id, env::predecessor_account_id(), "Only owner can remove their task.");

        // If owner, allow to remove task
        self.exit_task(hash);
    }

    /// Internal management of finishing a task.
    /// Responsible for cleaning up storage &
    /// returning any remaining balance to task owner.
    #[private]
    pub fn exit_task(&mut self, task_hash: Vec<u8>) {
        let task = self.tasks.get(&task_hash)
            .expect("No task found by hash");
        
        // return any balance
        if task.total_deposit > 0 {
            Promise::new(task.owner_id.to_string())
                .transfer(task.total_deposit);
        }

        // Remove task from map
        self.tasks.remove(&task_hash);

        // Remove task from schedule
        // Get previous task hashes in slot, find index of task hash, remove
        let next_slot = self.get_slot_from_cadence(task.cadence.clone());
        let mut slot_slots = self.slots.get(&next_slot).unwrap_or(Vec::new());
        let index = slot_slots.iter().position(|h| *h == task_hash).unwrap();
        slot_slots.remove(index);
        self.slots.insert(&next_slot, &slot_slots);
    }

    /// Executes a task based on the current task slot
    /// Computes whether a task should continue further or not
    /// Makes a cross-contract call with the task configuration
    /// Called directly by a registered agent
    ///
    /// ```bash
    /// near call cron.testnet proxy_call --accountId YOU.testnet
    /// ```
    // TODO: Change OOP, be careful on rewind execution!
    // TODO: How can this promise execute and allow panics?
    pub fn proxy_call(&mut self) {
        // No adding tasks while contract is paused
        assert_eq!(self.paused, false, "Task execution paused");

        // only registered agent signed, because micropayments will benefit long term
        let mut agent = self.agents.get(&env::signer_account_id())
            .expect("Agent not registered");

        // Get current slot based on block or timestamp
        // TODO: Adjust this to get slot from treemap floor?
        let current_slot = self.get_slot_id(None);
        log!("current slot {:?}", current_slot);

        // get task based on current slot
        let mut slot = self.slots.get(&current_slot)
            .expect("No tasks found in slot");
        log!("slot {:?}", &slot);

        // Get a single task hash, then retrieve task details
        let hash = slot.pop().expect("No tasks available");
        let mut task = self.tasks.get(&hash)
            .expect("No task found by hash");
        log!("Found Task {:?}", &task);

        // let hash = self.hash(&task);
        let call_balance_used = self.task_balance_uses(&task);

        assert!(call_balance_used <= task.total_deposit, "Not enough task balance to execute job");

        // Increment agent reward & task count
        agent.balance += self.agent_fee;
        agent.total_tasks_executed += 1;

        // TODO: Process task exit, if no future task can execute

        // only skip scheduling if user didnt intend
        if task.recurring != false {
            let next_slot = self.get_slot_from_cadence(task.cadence.clone());
            assert!(&current_slot < &next_slot, "Cannot schedule task in the past");

            // Get previous task hashes in slot, add as needed
            let mut slot_slots = self.slots.get(&next_slot).unwrap_or(Vec::new());
            slot_slots.push(hash.clone());
            self.slots.insert(&next_slot, &slot_slots);
        }

        // Update agent storage
        self.agents.insert(&env::signer_account_id(), &agent);


        // Decrease task balance
        // TODO: Change to real gas used
        task.total_deposit -= self.task_balance_used();

        task.status = TaskStatus::Active;

        // Update task storage
        self.tasks.insert(&hash, &task);

        // Call external contract with task variables
        let promise_first = env::promise_create(
            task.contract_id.clone(),
            &task.function_id.as_bytes(),
            task.arguments.as_slice(),
            // NOTE: Does this work if signer sends NO amount? Who pays??
            task.deposit,
            task.gas
        );
        let promise_second = env::promise_then(
            promise_first,
            env::current_account_id(),
            b"callback_for_proxy_call",
            json!({ "task_hash": hash }).to_string().as_bytes(),
            0,
            GAS_FOR_CALLBACK,
        );
        env::promise_return(promise_second);
    }

    #[private]
    pub fn callback_for_proxy_call(&mut self, task_hash: Vec<u8>) {
        assert_eq!(env::promise_results_count(), 1, "Expected 1 promise result.");
        let mut task = self.tasks.get(&task_hash.clone())
            .expect("No task found by hash");
        match env::promise_result(0) {
            PromiseResult::Successful(_) => {
                log!("Task {} completed successfully", base64::encode(task_hash.clone()));
                task.status = TaskStatus::Complete;
            },
            PromiseResult::Failed => {
                task.status = TaskStatus::Failed;
            },
            PromiseResult::NotReady => unreachable!()
        };
        self.tasks.insert(&task_hash, &task);
    }

    /// Add any account as an agent that will be able to execute tasks.
    /// Registering allows for rewards accruing with micro-payments which will accumulate to more long-term.
    /// 
    /// Optional Parameters:
    /// "payable_account_id" - Allows a different account id to be specified, so a user can receive funds at a different account than the agent account.
    ///
    /// ```bash
    /// near call cron.testnet register_agent '{"payable_account_id": "YOU.testnet"}' --accountId YOUR_AGENT.testnet
    /// ```
    #[payable]
    pub fn register_agent(
        &mut self,
        payable_account_id: Option<ValidAccountId>
    ) {
        assert_eq!(self.paused, false, "Register agent paused");

        // TODO: assert that attached deposit is enough to cover storage cost. This is to protect from storage attack.
        // check that account isn't already added
        let account = env::signer_account_id();
        if let Some(a) = self.agents.get(&account) {
            panic!("Agent {} already exists", a.payable_account_id);
        };

        let payable_id;
        if payable_account_id.is_some() {
            payable_id = payable_account_id.unwrap().to_string();
        } else {
            payable_id = env::signer_account_id();
        }

        let agent = Agent {
            payable_account_id: payable_id,
            balance: 0,
            total_tasks_executed: 0
        };

        self.agents.insert(&account, &agent);
    }

    /// Update agent details, specifically the payable account id for an agent.
    ///
    /// ```bash
    /// near call cron.testnet update_agent '{"payable_account_id": "YOU.testnet"}' --accountId YOUR_AGENT.testnet
    /// ```
    pub fn update_agent(
        &mut self,
        payable_account_id: Option<ValidAccountId>
    ) {
        assert_eq!(self.paused, false, "Update agent paused");

        let account = env::signer_account_id();
        println!("account {:?}", account);

        // check that signer agent exists
        if let Some(mut agent) = self.agents.get(&account) {
            // match payable_account_id.clone() {
            //     Some(_id) => {
            //         agent.payable_account_id = payable_account_id.unwrap().to_string();
            //     }
            //     None => ()
            // }

            if payable_account_id.is_some() {
                agent.payable_account_id = payable_account_id.unwrap().to_string();
                self.agents.insert(&account, &agent);
            }
        } else {
            panic!("Agent must register");
        };
    }

    /// Removes the agent from the active set of agents.
    /// Withdraws all reward balances to the agent payable account id.
    ///
    /// ```bash
    /// near call cron.testnet unregister_agent --accountId YOUR_AGENT.testnet
    /// ```
    pub fn unregister_agent(&mut self) {
        let account = env::signer_account_id();

        // check that signer agent exists
        if let Some(_acct) = self.agents.get(&account) {
            // Check if there is balance, if any pay rewards to payable account id.
            if _acct.balance > 0 {
                Promise::new(_acct.payable_account_id.to_string())
                    .transfer(_acct.balance);
            }

            // Remove from active agents
            self.agents.remove(&account);
        } else {
            panic!("No Agent");
        };
    }

    /// Allows an agent to withdraw all rewards, paid to the specified payable account id.
    ///
    /// ```bash
    /// near call cron.testnet withdraw_task_balance --accountId YOUR_AGENT.testnet
    /// ```
    pub fn withdraw_task_balance(&mut self) -> Promise {
        let account = env::signer_account_id();

        // check that signer agent exists
        if let Some(agent) = self.agents.get(&account) {
            assert!(agent.balance > 0, "No Agent balance");
            Promise::new(agent.payable_account_id.to_string())
                .transfer(agent.balance)
        } else {
            panic!("No Agent");
        }
    }

    /// Gets the agent data stats
    ///
    /// ```bash
    /// near view cron.testnet get_agent '{"account": "YOUR_AGENT.testnet"}'
    /// ```
    pub fn get_agent(&self, account: AccountId) -> Option<Agent> {
        self.agents.get(&account)
    }

    fn hash(&self, item: &Task) -> Vec<u8> {
        // Generate hash
        let input = format!(
                "{:?}{:?}{:?}",
                item.contract_id,
                item.function_id,
                item.cadence
            );
        env::keccak256(input.as_bytes())
    }

    /// Returns the base amount required to execute 1 task
    /// Fee breakdown:
    /// - Used Gas: Task Txn Fee Cost
    /// - Agent Fee: Incentivize Execution SLA
    /// 
    /// Task Fee Example:
    /// Gas: 50 Tgas
    /// Agent: 100 Tgas
    /// Total: 150 Tgas
    ///
    /// NOTE: Gas cost includes the cross-contract call & internal logic of this contract.
    /// Direct contract gas fee will be lower than task execution costs.
    fn task_balance_used(&self) -> u128 {
        u128::from(env::used_gas()) + self.agent_fee
    }

    /// Returns the base amount required to execute 1 task
    fn task_balance_uses(&self, task: &Task) -> u128 {
        task.deposit + u128::from(task.gas) + self.agent_fee
    }

    /// Check if a cadence string is valid by attempting to parse it
    fn validate_cadence(&self, cadence: String) -> bool {
        let s = Schedule::from_str(&cadence);
        if s.is_ok() { true }
        else { false }
    }

    /// If no offset, Returns current slot based on current block height
    /// If offset, Returns next slot based on current block height & integer offset
    /// rounded to nearest granularity (~every 1.6 block per sec)
    fn get_slot_id(&self, offset: Option<u64>) -> u128 {
        let block = env::block_index() * BPS_DENOMINATOR;
        let id;

        if let Some(o) = offset {
            // NOTE: Assumption here is that the offset will be in seconds. (blocks per second)
            //       Slot granularity will be in minutes (60 blocks per slot)
            // let offset_denom = o * BPS_DENOMINATOR;
            let offset_denom = o;
            let granularity = self.slot_granularity * BPS_DENOMINATOR;
            let slot_rem = core::cmp::max(offset_denom % granularity, 1);
            let slot_round = core::cmp::max(offset_denom.saturating_sub(slot_rem), granularity);
            let next = block + slot_round;

            if next - block > block + MAX_BLOCK_RANGE {
                // Protect against extreme future block schedules
                id = u64::min(next, block + MAX_BLOCK_RANGE);
            } else {
                id = next;
            }
        } else {
            id = block;
        }

        // Compute the slot id by truncating to granularity
        let block_id = id / BPS_DENOMINATOR;
        let rem = block_id % self.slot_granularity;
        let id_round = block_id.saturating_sub(rem);

        u128::from(id_round)
    }

    /// Parse cadence into a schedule
    /// Get next approximate block from a schedule
    /// return slot from the difference of upcoming block and current block
    fn get_slot_from_cadence(&mut self, cadence: String) -> u128 {
        let current_block = env::block_index();
        let current_block_ts = env::block_timestamp();

        // Schedule params
        // NOTE: eventually use TryFrom
        let schedule = Schedule::from_str(&cadence).unwrap();
        let next_ts = schedule.next_after(&current_block_ts).unwrap();
        let next_diff = next_ts - current_block_ts;

        // calculate the average blocks, to get predicted future block
        let blocks_total = core::cmp::max(current_block - self.bps_block, 1);
        let mut bps = (blocks_total * NANO * BPS_DENOMINATOR) / std::cmp::max(current_block_ts - self.bps_timestamp, 1);

        // Protect against bps being 0
        if bps < 1 { bps = 1; }

        // return upcoming slot
        // Get the next block based on time it takes until next timestamp
        // let offset = next_diff / bps / NANO;
        let offset = next_diff / bps;
        let current = self.get_slot_id(None);
        let next_slot = self.get_slot_id(Some(offset));

        if current == next_slot {
            // Add slot granularity to make sure the minimum next slot is a block within next slot granularity range
            current + u128::from(self.slot_granularity)
        } else {
            next_slot
        }
    }

    pub fn update_settings(
        &mut self,
        owner_id: Option<AccountId>,
        owner_pk: Option<Base58PublicKey>,
        slot_granularity: Option<u64>,
        paused: Option<bool>,
        agent_fee: Option<Balance>,
    ) {
        assert_eq!(self.owner_id, env::signer_account_id(), "Must be owner");

        if owner_id.is_some() { self.owner_id = owner_id.unwrap(); }
        if owner_pk.is_some() { self.owner_pk = owner_pk.unwrap().into(); }

        if slot_granularity.is_some() { self.slot_granularity = slot_granularity.unwrap(); }
        if paused.is_some() { self.paused = paused.unwrap(); }
        if agent_fee.is_some() { self.agent_fee = agent_fee.unwrap(); }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use near_sdk::test_utils::{accounts, VMContextBuilder};
    use near_sdk::json_types::{ValidAccountId};
    use near_sdk::{testing_env, MockedBlockchain};

    use chrono::*;
    use cron_schedule::{Schedule};
    use std::str::FromStr;
    use chrono::prelude::DateTime;
    use chrono::Utc;

    pub fn get_sample_task() -> Task {
        Task {
            owner_id: String::from("bob"),
            contract_id: String::from("contract.testnet"),
            function_id: String::from("increment"),
            cadence: String::from("@daily"),
            recurring: false,
            status: TaskStatus::Ready,
            total_deposit: 3000000000300,
            deposit: 100,
            gas: 200,
            arguments: vec![]
        }
    }

    // from https://stackoverflow.com/a/50072164/711863
    pub fn human_readable_time(time_nano: u64) -> String {
        let timestamp = (time_nano / 1_000_000_000).to_string().parse::<i64>().unwrap();
        let naive = NaiveDateTime::from_timestamp(timestamp, 0);
        let datetime: DateTime<Utc> = DateTime::from_utc(naive, Utc);
        let newdate = datetime.format("%Y-%m-%d %H:%M:%S");
        // Print the newly formatted date and time
        // println!("{}", newdate);
        newdate.to_string()
    }

    fn get_context(predecessor_account_id: ValidAccountId) -> VMContextBuilder {
        let mut builder = VMContextBuilder::new();
        builder
            .current_account_id(accounts(0))
            .signer_account_id(predecessor_account_id.clone())
            .signer_account_pk(b"ed25519:4ZhGmuKTfQn9ZpHCQVRwEr4JnutL8Uu3kArfxEqksfVM".to_vec())
            .predecessor_account_id(predecessor_account_id)
            .block_index(1234)
            .block_timestamp(1_600_000_000_000_000_000);
        builder
    }

    #[test]
    fn test_contract_new() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let contract = CronManager::new();
        testing_env!(context.is_view(true).build());
        assert!(contract.get_all_tasks(None).is_empty());
    }

    #[test]
    fn test_task_create() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let mut contract = CronManager::new();
        testing_env!(context.is_view(true).build());
        assert!(contract.get_all_tasks(None).is_empty());
        testing_env!(context.is_view(false).attached_deposit(3000000000300).build());
        let task_id = contract.create_task(
            "contract.testnet".to_string(),
            "increment".to_string(),
            "@daily".to_string(),
            Some(false),
            Some(100),
            Some(200),
            None,
        );

        testing_env!(context.is_view(true).build());
        assert_eq!(contract.get_all_tasks(None).len(), 1);

        let daily_task = get_sample_task();
        assert_eq!(contract.get_task(task_id), daily_task);
    }

    #[test]
    #[should_panic(expected = "Create task paused")]
    fn test_task_create_paused() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let mut contract = CronManager::new();
        testing_env!(context.is_view(false).build());
        contract.update_settings(None, None, None, Some(true), None);
        testing_env!(context.is_view(false).attached_deposit(3000000000300).build());
        contract.create_task(
            "contract.testnet".to_string(),
            "increment".to_string(),
            "@daily".to_string(),
            Some(true),
            Some(100),
            Some(200),
            None,
        );
    }

    #[test]
    #[should_panic(expected = "Cadence string invalid")]
    fn test_task_create_bad_cadence() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let mut contract = CronManager::new();
        testing_env!(context.is_view(false).attached_deposit(3000000000300).build());
        contract.create_task(
            "contract.testnet".to_string(),
            "increment".to_string(),
            "raspberry_oat_milk".to_string(),
            Some(true),
            Some(100),
            Some(200),
            None,
        );
    }

    #[test]
    #[should_panic(expected = "Not enough task balance to execute job, need at least 3000000100200")]
    fn test_task_create_deposit_not_enuf() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let mut contract = CronManager::new();
        testing_env!(context.is_view(false).attached_deposit(0).build());
        contract.create_task(
            "contract.testnet".to_string(),
            "increment".to_string(),
            "@daily".to_string(),
            Some(false),
            Some(100000),
            Some(200),
            None,
        );
    }

    #[test]
    #[should_panic(expected = "Not enough task balance to execute job, need at least 6000000200400")]
    fn test_task_create_deposit_not_enuf_recurring() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let mut contract = CronManager::new();
        testing_env!(context.is_view(false).attached_deposit(0).build());
        contract.create_task(
            "contract.testnet".to_string(),
            "increment".to_string(),
            "@daily".to_string(),
            Some(true),
            Some(100000),
            Some(200),
            None,
        );
    }

    #[test]
    #[should_panic(expected = "Gas minimum has not been met")]
    fn test_task_create_gas_min() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let mut contract = CronManager::new();
        testing_env!(context.is_view(false).attached_deposit(0).build());
        contract.create_task(
            "contract.testnet".to_string(),
            "increment".to_string(),
            "@daily".to_string(),
            Some(false),
            Some(0),
            Some(0),
            None,
        );
    }

    #[test]
    fn test_task_create_slot_schedule() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let mut contract = CronManager::new();
        testing_env!(context.is_view(false).attached_deposit(3000000000200).build());
        contract.create_task(
            "contract.testnet".to_string(),
            "increment".to_string(),
            "*/10 * * * * *".to_string(),
            Some(false),
            Some(0),
            Some(200),
            None,
        );
        testing_env!(context.is_view(true).build());
        let slot = contract.slots.get(&1260).expect("Should have something here");
        assert_eq!(slot[0], [233, 217, 1, 85, 174, 36, 220, 148, 248, 181, 105, 12, 71, 127, 52, 183, 172, 171, 193, 186, 212, 162, 3, 139, 78, 84, 11, 30, 30, 194, 160, 130]);
    }

    // TODO: Finish
    // #[test]
    // fn test_task_proxy() {
    //     let mut context = get_context(accounts(1));
    //     testing_env!(context.build());
    //     let mut contract = CronManager::new();
    //     testing_env!(context.is_view(false).attached_deposit(6000000000000).build());
    //     contract.create_task(
    //         "contract.testnet".to_string(),
    //         "increment".to_string(),
    //         "*/10 * * * * *".to_string(),
    //         Some(false),
    //         None,
    //         None,
    //         None,
    //     );
    //     testing_env!(context.is_view(false).build());
    //     contract.register_agent(None);

    //     testing_env!(context.is_view(true).block_index(1260).build());
    //     assert!(contract.get_all_tasks(None).len() > 0);
    //     testing_env!(context.is_view(false).build());
    //     contract.proxy_call();
    //     assert!(contract.get_all_tasks(None).is_empty());
    // }

    #[test]
    #[should_panic(expected = "Expected 1 promise result.")]
    fn test_task_proxy_callback() {
        let context = get_context(accounts(1));
        testing_env!(context.build());
        let mut contract = CronManager::new();
        contract.callback_for_proxy_call(vec!(0,1,2,3));
    }

    #[test]
    #[should_panic(expected = "Agent not registered")]
    fn test_task_proxy_agent_not_registered() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let mut contract = CronManager::new();
        testing_env!(context.is_view(false).attached_deposit(3000000000300).build());
        contract.create_task(
            "contract.testnet".to_string(),
            "increment".to_string(),
            "@daily".to_string(),
            Some(false),
            Some(100),
            Some(200),
            None,
        );
        testing_env!(context.is_view(false).block_index(1260).attached_deposit(3000000000000).prepaid_gas(300000000000).build());
        contract.proxy_call();
    }

    #[test]
    #[should_panic(expected = "Task execution paused")]
    fn test_task_proxy_paused() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let mut contract = CronManager::new();
        testing_env!(context.is_view(false).attached_deposit(3000000000300).build());
        contract.create_task(
            "contract.testnet".to_string(),
            "increment".to_string(),
            "@daily".to_string(),
            Some(false),
            Some(100),
            Some(200),
            None,
        );
        contract.update_settings(None, None, None, Some(true), None);
        testing_env!(context.is_view(false).block_index(1260).build());
        contract.proxy_call();
    }

    #[test]
    #[should_panic(expected = "No tasks found in slot")]
    fn test_task_proxy_no_tasks() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let mut contract = CronManager::new();
        contract.register_agent(None);
        testing_env!(context.is_view(false).block_index(1260).build());
        contract.proxy_call();
    }

    #[test]
    fn test_task_update() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let mut contract = CronManager::new();
        testing_env!(context.is_view(true).build());
        assert!(contract.get_all_tasks(None).is_empty());
        testing_env!(context.is_view(false).attached_deposit(3000000000300).build());
        let task_hash = contract.create_task(
            "contract.testnet".to_string(),
            "increment".to_string(),
            "@daily".to_string(),
            Some(false),
            Some(100),
            Some(200),
            None,
        );

        testing_env!(context.is_view(false).attached_deposit(10000000000000).build());
        contract.update_task(
            task_hash.clone(),
            Some("* * */12 * * *".to_string()),
            Some(true),
            Some(10000000000000),
            None,
            None
        );

        testing_env!(context.is_view(true).build());
        let task = contract.get_task(task_hash);
        assert_eq!(task.cadence, "* * */12 * * *");
        assert_eq!(task.recurring, true);
        assert_eq!(task.deposit, 10000000000000);
        assert_eq!(task.total_deposit, 13000000000300);
    }

    #[test]
    #[should_panic(expected = "Only owner can update their task.")]
    fn test_task_update_not_owner() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let mut contract = CronManager::new();
        testing_env!(context.is_view(true).build());
        assert!(contract.get_all_tasks(None).is_empty());
        testing_env!(context.is_view(false).attached_deposit(3000000000300).build());
        let task_hash = contract.create_task(
            "contract.testnet".to_string(),
            "increment".to_string(),
            "@daily".to_string(),
            Some(false),
            Some(100),
            Some(200),
            None,
        );

        testing_env!(context.is_view(false).attached_deposit(10000000000000).signer_account_id(accounts(4)).predecessor_account_id(accounts(4)).build());
        contract.update_task(
            task_hash.clone(),
            Some("* * */12 * * *".to_string()),
            Some(true),
            Some(10000000000000),
            None,
            None
        );
    }

    #[test]
    #[should_panic(expected = "No task found by hash")]
    fn test_task_update_no_task() {
        let context = get_context(accounts(1));
        testing_env!(context.build());
        let mut contract = CronManager::new();
        contract.update_task(
            Base64VecU8::from(vec!(0,1,2,3)),
            Some("* * */12 * * *".to_string()),
            Some(true),
            Some(10000000000000),
            None,
            None
        );
    }

    #[test]
    #[should_panic(expected = "Cadence string invalid")]
    fn test_task_update_bad_cadence() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let mut contract = CronManager::new();
        testing_env!(context.is_view(true).build());
        assert!(contract.get_all_tasks(None).is_empty());
        testing_env!(context.is_view(false).attached_deposit(3000000000300).build());
        let task_hash = contract.create_task(
            "contract.testnet".to_string(),
            "increment".to_string(),
            "@daily".to_string(),
            Some(false),
            Some(100),
            Some(200),
            None,
        );

        testing_env!(context.is_view(false).attached_deposit(10000000000000).build());
        contract.update_task(
            task_hash.clone(),
            Some("dunder_mifflin".to_string()),
            Some(true),
            Some(10000000000000),
            None,
            None
        );
    }

    #[test]
    fn test_task_remove() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let mut contract = CronManager::new();
        testing_env!(context.is_view(true).build());
        assert!(contract.get_all_tasks(None).is_empty());
        testing_env!(context.is_view(false).attached_deposit(3000000000300).build());
        let task_hash = contract.create_task(
            "contract.testnet".to_string(),
            "increment".to_string(),
            "@daily".to_string(),
            Some(false),
            Some(100),
            Some(200),
            None,
        );

        testing_env!(context.is_view(true).build());
        assert_eq!(contract.get_all_tasks(None).len(), 1);

        testing_env!(context.is_view(false).build());
        contract.remove_task(task_hash);

        testing_env!(context.is_view(true).build());
        assert_eq!(contract.get_all_tasks(None).len(), 0);
    }

    #[test]
    #[should_panic(expected = "Only owner can remove their task.")]
    fn test_task_remove_not_owner() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let mut contract = CronManager::new();
        testing_env!(context.is_view(true).build());
        assert!(contract.get_all_tasks(None).is_empty());
        testing_env!(context.is_view(false).attached_deposit(3000000000300).build());
        let task_hash = contract.create_task(
            "contract.testnet".to_string(),
            "increment".to_string(),
            "@daily".to_string(),
            Some(false),
            Some(100),
            Some(200),
            None,
        );

        testing_env!(context.is_view(true).build());
        assert_eq!(contract.get_all_tasks(None).len(), 1);

        testing_env!(context.is_view(false).signer_account_id(accounts(4)).predecessor_account_id(accounts(4)).build());
        contract.remove_task(task_hash);
    }

    #[test]
    #[should_panic(expected = "No task found by hash")]
    fn test_task_remove_no_task() {
        let context = get_context(accounts(1));
        testing_env!(context.build());
        let mut contract = CronManager::new();
        contract.remove_task(Base64VecU8::from(vec!(0,1,2,3)));
    }

    #[test]
    fn test_get_slot_id_current_block() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let contract = CronManager::new();
        testing_env!(context.is_view(true).build());
        let slot = contract.get_slot_id(None);

        assert_eq!(slot, 1200);
    }

    #[test]
    fn test_get_slot_id_offset_block() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let contract = CronManager::new();
        testing_env!(context.is_view(true).build());
        let slot = contract.get_slot_id(Some(1_000));

        assert_eq!(slot, 1260);
    }

    #[test]
    fn test_get_slot_id_max_block() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let contract = CronManager::new();
        testing_env!(context.is_view(true).build());
        let slot = contract.get_slot_id(Some(1_000_000_000_000));

        // ensure even if we pass in a HUGE number, it can only be scheduled UP to the max pre-defined block settings
        assert_eq!(slot, 1_000_001_160);
    }

    #[test]
    fn test_get_slot_id_change_granularity() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let mut contract = CronManager::new();
        testing_env!(context.is_view(true).build());
        let slot = contract.get_slot_id(None);
        assert_eq!(slot, 1200);

        testing_env!(context.is_view(false).build());
        contract.update_settings(None, None, Some(10), None, None);
        testing_env!(context.is_view(true).build());
        let slot = contract.get_slot_id(None);
        assert_eq!(slot, 1230);

        testing_env!(context.is_view(false).build());
        contract.update_settings(None, None, Some(1), None, None);
        testing_env!(context.is_view(true).build());
        let slot = contract.get_slot_id(None);
        assert_eq!(slot, 1234);
    }

    #[test]
    #[should_panic(expected = "Must be owner")]
    fn test_update_settings_fail() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let mut contract = CronManager::new();
        testing_env!(context.is_view(true).build());
        assert_eq!(contract.slot_granularity, SLOT_GRANULARITY);

        testing_env!(context.is_view(false).signer_account_id(accounts(3)).predecessor_account_id(accounts(3)).build());
        contract.update_settings(None, None, Some(10), None, None);
    }

    #[test]
    fn test_update_settings() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let mut contract = CronManager::new();
        testing_env!(context.is_view(true).build());
        assert_eq!(contract.slot_granularity, SLOT_GRANULARITY);

        testing_env!(context.is_view(false).build());
        contract.update_settings(None, None, Some(10), Some(true), None);
        testing_env!(context.is_view(true).build());
        assert_eq!(contract.slot_granularity, 10);
        assert_eq!(contract.paused, true);
    }

    #[test]
    fn test_get_slot_from_cadence_ts_check() {
        let context = get_context(accounts(1));
        testing_env!(context.build());
        let current_block_ts = env::block_timestamp();
        let schedule1 = Schedule::from_str(&"*/5 * * * * *".to_string()).unwrap();
        let next_ts1 = schedule1.next_after(&current_block_ts).unwrap();
        println!("TS 1: {} {}", next_ts1, human_readable_time(next_ts1));
        assert_eq!(next_ts1, 1600000005000000000);

        let schedule2 = Schedule::from_str(&"* */5 * * * *".to_string()).unwrap();
        let next_ts2 = schedule2.next_after(&current_block_ts).unwrap();
        println!("TS 2: {} {}", next_ts2, human_readable_time(next_ts2));
        assert_eq!(next_ts2, 1600000200000000000);

        let schedule3 = Schedule::from_str(&"* * */5 * * *".to_string()).unwrap();
        let next_ts3 = schedule3.next_after(&current_block_ts).unwrap();
        println!("TS 3: {} {}", next_ts3, human_readable_time(next_ts3));
        assert_eq!(next_ts3, 1600009200000000000);

        let schedule4 = Schedule::from_str(&"* * * */5 * *".to_string()).unwrap();
        let next_ts4 = schedule4.next_after(&current_block_ts).unwrap();
        println!("TS 4: {} {}", next_ts4, human_readable_time(next_ts4));
        assert_eq!(next_ts4, 1600214400000000000);

        let schedule5 = Schedule::from_str(&"* * * * */5 *".to_string()).unwrap();
        let next_ts5 = schedule5.next_after(&current_block_ts).unwrap();
        println!("TS 5: {} {}", next_ts5, human_readable_time(next_ts5));
        assert_eq!(next_ts5, 1604188800000000000);

        let schedule6 = Schedule::from_str(&"* * * * * * 2025".to_string()).unwrap();
        let next_ts6 = schedule6.next_after(&current_block_ts).unwrap();
        println!("TS 6: {} {}", next_ts6, human_readable_time(next_ts6));
        assert_eq!(next_ts6, 1757766401000000000);
    }

    #[test]
    fn test_get_slot_from_cadence() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let mut contract = CronManager::new();
        testing_env!(context.is_view(false).block_index(1334).block_timestamp(1_600_000_000_300_000_000).build());
        testing_env!(context.is_view(true).build());
        let slot1 = contract.get_slot_from_cadence("*/5 * * * * *".to_string()); // Immediately next slot (since every 5 seconds)
        println!("SLOT 1 {}",slot1);
        assert_eq!(slot1, 1380);
        let slot2 = contract.get_slot_from_cadence("* */5 * * * *".to_string()); // Every 5 mins
        println!("SLOT 2 {}",slot2);
        assert_eq!(slot2, 1860);
        let slot3 = contract.get_slot_from_cadence("* * */5 * * *".to_string()); // Every 5th hour
        println!("SLOT 3 {}",slot3);
        assert_eq!(slot3, 28860);
        let slot4 = contract.get_slot_from_cadence("* * * */5 * *".to_string()); // Every 5th day
        println!("SLOT 4 {}",slot4);
        assert_eq!(slot4, 644460);
        let slot5 = contract.get_slot_from_cadence("* * * * */5 *".to_string()); // Every 5th Month
        println!("SLOT 5 {}",slot5);
        assert_eq!(slot5, 12567720);
        let slot6 = contract.get_slot_from_cadence("* * * * * * 2025".to_string());
        println!("SLOT 6 {}",slot6);
        assert_eq!(slot6, 473300940);
    }

    #[test]
    fn test_agent_register_check() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let contract = CronManager::new();
        testing_env!(context.is_view(true).build());
        assert!(contract.get_agent(accounts(1).to_string()).is_none());
    }

    #[test]
    fn test_agent_register_new() {
        let mut context = get_context(accounts(1));
        testing_env!(context.is_view(false).build());
        let mut contract = CronManager::new();
        contract.register_agent(Some(accounts(1)));

        testing_env!(context.is_view(true).build());
        let _agent = contract.get_agent(accounts(1).to_string());
        assert_eq!(contract.get_agent(accounts(1).to_string()), Some(Agent {
            payable_account_id: accounts(1).to_string(),
            balance: 0,
            total_tasks_executed: 0
        }));
    }

    #[test]
    #[should_panic(expected = "Agent must register")]
    fn test_agent_update_check() {
        let context = get_context(accounts(1));
        testing_env!(context.build());
        let mut contract = CronManager::new();
        contract.update_agent(None);
        contract.update_agent(Some(accounts(2)));
    }

    #[test]
    fn test_agent_update() {
        let mut context = get_context(accounts(1));
        testing_env!(context.is_view(false).build());
        let mut contract = CronManager::new();
        contract.register_agent(Some(accounts(1)));
        contract.update_agent(Some(accounts(2)));

        testing_env!(context.is_view(true).build());
        let _agent = contract.get_agent(accounts(1).to_string());
        assert_eq!(contract.get_agent(accounts(1).to_string()), Some(Agent {
            payable_account_id: accounts(2).to_string(),
            balance: 0,
            total_tasks_executed: 0
        }));
    }

    #[test]
    #[should_panic(expected = "No Agent")]
    fn test_agent_unregister_check() {
        let context = get_context(accounts(3));
        testing_env!(context.build());
        let mut contract = CronManager::new();
        contract.unregister_agent();
    }

    #[test]
    fn test_agent_unregister_no_balance() {
        let mut context = get_context(accounts(1));
        testing_env!(context.is_view(false).build());
        let mut contract = CronManager::new();
        contract.register_agent(Some(accounts(1)));
        contract.unregister_agent();

        testing_env!(context.is_view(true).build());
        let _agent = contract.get_agent(accounts(1).to_string());
        assert_eq!(contract.get_agent(accounts(1).to_string()), None);
    }

    #[test]
    #[should_panic(expected = "No Agent")]
    fn test_agent_withdraw_check() {
        let context = get_context(accounts(3));
        testing_env!(context.build());
        let mut contract = CronManager::new();
        contract.withdraw_task_balance();
    }

    #[test]
    fn test_agent_withdraw_balance() {
        let mut context = get_context(accounts(1));
        testing_env!(context.is_view(false).build());
        let mut contract = CronManager::new();
        contract.update_settings(None, None, Some(1), None, None);
        contract.register_agent(Some(accounts(1)));
        // enough for 2 runs
        testing_env!(context.is_view(false).attached_deposit(6000000000600).build());
        contract.create_task(
            "contract.testnet".to_string(),
            "increment".to_string(),
            "* * * * * *".to_string(),
            Some(true),
            None,
            Some(200),
            None,
        );

        testing_env!(context.is_view(false).block_index(1235).build());
        contract.proxy_call();
        contract.withdraw_task_balance();

        testing_env!(context.is_view(true).build());
        let agent = contract.get_agent(accounts(1).to_string());
        assert_eq!(agent.unwrap().balance, 3000000000000);
    }

    #[test]
    fn test_hash_compute() {
        let context = get_context(accounts(3));
        testing_env!(context.build());
        let contract = CronManager::new();
        let task = get_sample_task();
        let hash = contract.hash(&task);
        assert_eq!(hash, [239, 129, 115, 87, 45, 53, 242, 8, 151, 179, 26, 143, 84, 131, 173, 197, 248, 228, 81, 103, 58, 131, 238, 15, 9, 201, 157, 197, 202, 113, 69, 139], "Hash is not equivalent")
    }

    #[test]
    fn test_tick() {
        let mut context = get_context(accounts(1));
        testing_env!(context.is_view(false).build());
        let mut contract = CronManager::new();
        testing_env!(context.is_view(true).build());
        assert_eq!(contract.bps_block, 1234);
        testing_env!(context.is_view(false).block_index(1275).build());
        contract.tick();
        testing_env!(context.is_view(true).block_index(1375).build());
        assert_eq!(contract.bps_block, 1275);
    }
}